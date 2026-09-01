use crate::tunnel::{Tunnel, TunnelEvent};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tracing::{debug, error};
use crate::protocol::Message;

static CONN_COUNTER: AtomicU32 = AtomicU32::new(1);

pub fn next_conn_id() -> u32 {
    CONN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Bidirectional relay between a TCP stream and the tunnel.
/// Takes ownership of the stream and the event receiver.
pub async fn relay(
    stream: TcpStream,
    conn_id: u32,
    tunnel: Arc<Tunnel>,
    mut event_rx: mpsc::Receiver<TunnelEvent>,
) {
    let (mut tcp_read, mut tcp_write) = stream.into_split();
    let tunnel_clone = tunnel.clone();

    // Raised by the write half when the connection has failed. The read half
    // has to stop cooperatively rather than being aborted: aborting would drop
    // its socket half, and both halves are needed to abort the connection.
    let failed = Arc::new(Notify::new());
    let failed_read = failed.clone();

    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        let mut give_up = false;
        loop {
            let read = tokio::select! {
                // Cancel-safe: no bytes are consumed if the other branch wins.
                result = tcp_read.read(&mut buf) => result,
                _ = failed_read.notified() => {
                    debug!("[{}] connection failed; stopping the upload side", conn_id);
                    give_up = true;
                    break;
                }
            };
            match read {
                Ok(0) => {
                    debug!("[{}] client EOF", conn_id);
                    break;
                }
                Ok(n) => {
                    debug!("[{}] client -> tunnel {} bytes", conn_id, n);
                    let msg = Message::Data {
                        conn_id,
                        data: buf[..n].to_vec(),
                    };
                    if let Err(e) = tunnel_clone.send_message(&msg).await {
                        error!("[{}] tunnel send error: {}", conn_id, e);
                        break;
                    }
                }
                Err(e) => {
                    debug!("[{}] client read error: {}", conn_id, e);
                    break;
                }
            }
        }
        // Tell the server we are done sending. It drops the target's write
        // half, so the target sees a real FIN — the half-close signal some
        // protocols need to start replying. Deliberately *not* unregistering
        // the conn_id here: the target may still be streaming a response, and
        // dropping the dispatch channel now would discard it.
        //
        // Skipped when the connection failed: the target socket is already gone
        // and the server has released its writer, so there is nothing to close.
        if !give_up {
            let close_msg = Message::Close { conn_id };
            let _ = tunnel_clone.send_message(&close_msg).await;
        }
        tcp_read
    });

    let failed_notify = failed;
    let write_task = tokio::spawn(async move {
        // Whether this connection ended in a failure. A truncated stream must
        // not be handed to the app as a clean end-of-stream.
        let mut failed = false;
        while let Some(event) = event_rx.recv().await {
            match event {
                TunnelEvent::Data(data) => {
                    if data.is_empty() {
                        continue;
                    }
                    debug!("[{}] tunnel -> client {} bytes", conn_id, data.len());
                    if let Err(e) = tcp_write.write_all(&data).await {
                        error!("[{}] client write error: {}", conn_id, e);
                        break;
                    }
                }
                TunnelEvent::Close => {
                    debug!("[{}] tunnel closed", conn_id);
                    // Everything arrived: FIN, so the app reads a clean EOF.
                    let _ = tcp_write.shutdown().await;
                    break;
                }
                TunnelEvent::Error(msg) => {
                    debug!("[{}] tunnel error: {}", conn_id, msg);
                    failed = true;
                    // Release the read half so the connection can be aborted:
                    // the app is waiting on a response that will never arrive,
                    // so it will not close its side on its own.
                    failed_notify.notify_one();
                    break;
                }
                TunnelEvent::Exit(_) => {
                    // Only meaningful for remote exec; a TCP relay never sees it.
                    break;
                }
            }
        }
        (tcp_write, failed)
    });

    // Both directions run to completion independently: an upload that finishes
    // must not cut off a response still in flight, and a target that stops
    // replying must not cut off an upload still in progress.
    //
    // On failure the write half signals the read half to stop, so both return
    // their socket halves and the connection can be aborted below.
    let (read_half, write_half) = tokio::join!(read_task, write_task);
    let failed = matches!(&write_half, Ok((_, true)));

    tunnel.unregister_connection(conn_id).await;

    // A failed connection has to fail visibly. Dropping the socket sends FIN,
    // which the app reads as a successful end-of-stream on data that is in fact
    // truncated. Reuniting the halves and setting SO_LINGER to zero makes the
    // close a RST instead, so the app's read fails and it cannot mistake the
    // short stream for a complete one. Reunite consumes both halves, so no FIN
    // escapes on the way.
    if !failed {
        return;
    }
    let (Ok(read_half), Ok((write_half, _))) = (read_half, write_half) else {
        debug!("[{}] relay half did not return its socket; closing with FIN", conn_id);
        return;
    };
    match read_half.reunite(write_half) {
        Ok(stream) => {
            // Deprecated in tokio because a *non-zero* linger blocks the thread
            // on drop. A zero timeout is the opposite: it makes close() return
            // immediately and emit RST, which is exactly the signal wanted here.
            #[allow(deprecated)]
            let linger = stream.set_linger(Some(Duration::ZERO));
            if let Err(e) = linger {
                debug!("[{}] could not set SO_LINGER, closing with FIN: {}", conn_id, e);
            }
        }
        Err(e) => debug!("[{}] could not reunite socket halves: {}", conn_id, e),
    }
}
