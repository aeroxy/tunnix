//! Client side of `push` / `pull`: stream a zstd-compressed tar archive over the
//! tunnel. The transport already encrypts every message, so the file bytes are
//! compressed first and encrypted by the channel (compress-then-encrypt).
//!
//! Structurally this mirrors `exec`: allocate a conn_id, register for events,
//! run the session, and always unregister.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::archive::{spawn_compress, spawn_decompress};
use crate::protocol::Message;
use crate::relay::next_conn_id;
use crate::tunnel::{Tunnel, TunnelEvent};

/// Upload `locals` (files or directories) to `remote` (a destination directory
/// on the server), compressing the combined tar stream with zstd at `level`.
pub async fn push(
    tunnel: Arc<Tunnel>,
    locals: Vec<PathBuf>,
    remote: String,
    level: i32,
) -> Result<()> {
    for local in &locals {
        if !local.exists() {
            bail!("local path does not exist: {}", local.display());
        }
    }
    let conn_id = next_conn_id();
    let mut event_rx = tunnel.register_connection(conn_id).await;
    let result = push_session(&tunnel, conn_id, &mut event_rx, locals, remote, level).await;
    tunnel.unregister_connection(conn_id).await;
    result
}

async fn push_session(
    tunnel: &Arc<Tunnel>,
    conn_id: u32,
    event_rx: &mut tokio::sync::mpsc::Receiver<TunnelEvent>,
    locals: Vec<PathBuf>,
    remote: String,
    level: i32,
) -> Result<()> {
    // Announce the upload; the server replies with an empty Data ACK on success
    // or an Error (e.g. transfers disabled) which we surface before streaming.
    let msg = Message::Push { conn_id, path: remote };
    if let Some(Message::Error { message, .. }) = tunnel.send_connect(&msg).await? {
        bail!("{}", message);
    }

    let (mut chunks, comp_handle) = spawn_compress(locals, level);

    let stream_result = async {
        loop {
            tokio::select! {
                maybe = chunks.recv() => match maybe {
                    Some(data) => {
                        tunnel.send_message(&Message::Data { conn_id, data }).await?;
                    }
                    None => break, // compressor finished (or failed — checked below)
                },
                event = event_rx.recv() => match event {
                    // The server stays silent until the transfer completes; any
                    // message here means it aborted early.
                    Some(TunnelEvent::Error(m)) => bail!("remote error: {}", m),
                    Some(TunnelEvent::Close) | None => {
                        bail!("connection closed before the transfer completed")
                    }
                    _ => {}
                },
            }
        }
        comp_handle.await.context("join compress task")??;
        anyhow::Ok(())
    }
    .await;

    // Always signal end-of-stream so the server tears down its decompressor,
    // even if compression failed mid-way (it will then report a decode error).
    let _ = tunnel.send_message(&Message::Close { conn_id }).await;
    stream_result?;

    // Wait for the server to confirm it finished writing to disk.
    loop {
        match event_rx.recv().await {
            Some(TunnelEvent::Exit(0)) => return Ok(()),
            Some(TunnelEvent::Exit(code)) => bail!("server reported transfer failure (exit {})", code),
            Some(TunnelEvent::Error(m)) => bail!("remote error: {}", m),
            Some(TunnelEvent::Close) | None => {
                bail!("connection closed before the server confirmed the transfer")
            }
            Some(TunnelEvent::Data(_)) => {} // nothing expected, ignore
        }
    }
}

/// Download `remotes` (files or directories on the server) into `local`,
/// unpacking the single zstd-compressed tar stream the server sends back.
pub async fn pull(
    tunnel: Arc<Tunnel>,
    remotes: Vec<String>,
    local: PathBuf,
    level: i32,
) -> Result<()> {
    let conn_id = next_conn_id();
    let mut event_rx = tunnel.register_connection(conn_id).await;
    let result = pull_session(&tunnel, conn_id, &mut event_rx, remotes, local, level).await;
    tunnel.unregister_connection(conn_id).await;
    result
}

async fn pull_session(
    tunnel: &Arc<Tunnel>,
    conn_id: u32,
    event_rx: &mut tokio::sync::mpsc::Receiver<TunnelEvent>,
    remotes: Vec<String>,
    local: PathBuf,
    level: i32,
) -> Result<()> {
    let msg = Message::Pull { conn_id, paths: remotes, level };
    if let Some(Message::Error { message, .. }) = tunnel.send_connect(&msg).await? {
        bail!("{}", message);
    }

    let (chunk_tx, unpack_handle) = spawn_decompress(local);

    let mut transfer_err: Option<String> = None;
    loop {
        match event_rx.recv().await {
            Some(TunnelEvent::Data(data)) => {
                // Backpressure: this blocks while the decompressor is busy. If
                // it ended early (error), stop feeding it.
                if chunk_tx.send(data).await.is_err() {
                    break;
                }
            }
            // The server confirms a complete transfer with ExitStatus(0) before
            // Close. A Close or stream-end without that confirmation means the
            // server aborted mid-stream — treat it as an error, not success, so
            // a truncated-but-parseable archive isn't reported as complete.
            Some(TunnelEvent::Exit(0)) => break,
            Some(TunnelEvent::Close) => {
                transfer_err = Some("server closed the stream unexpectedly".to_string());
                break;
            }
            None => {
                transfer_err = Some("event stream ended unexpectedly".to_string());
                break;
            }
            Some(TunnelEvent::Exit(code)) => {
                transfer_err = Some(format!("server reported transfer failure (exit {})", code));
                break;
            }
            Some(TunnelEvent::Error(m)) => {
                transfer_err = Some(format!("remote error: {}", m));
                break;
            }
        }
    }

    // EOF to the decompressor, then tell the server to stop streaming.
    drop(chunk_tx);
    let _ = tunnel.send_message(&Message::Close { conn_id }).await;

    if let Some(e) = transfer_err {
        let _ = unpack_handle.await;
        bail!("{}", e);
    }
    unpack_handle.await.context("join unpack task")??;
    Ok(())
}
