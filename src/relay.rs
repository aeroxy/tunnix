use crate::protocol::Message;
use crate::tunnel::{Tunnel, TunnelEvent};
use rama::{
    telemetry::tracing::{debug, error},
    utils::octets::kib,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

static CONN_COUNTER: AtomicU32 = AtomicU32::new(1);
const FAILED_INPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

pub fn next_conn_id() -> u32 {
    CONN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Bidirectional relay between a TCP stream and the tunnel.
/// Takes ownership of the stream and the event receiver.
pub async fn relay<S>(
    stream: S,
    conn_id: u32,
    tunnel: Arc<Tunnel>,
    mut event_rx: mpsc::Receiver<TunnelEvent>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut tcp_read, mut tcp_write) = tokio::io::split(stream);
    let tunnel_clone = tunnel.clone();

    let read = async move {
        let mut buf = vec![0u8; kib(32)];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) => {
                    debug!(conn_id, "client reached EOF");
                    break;
                }
                Ok(n) => {
                    debug!(conn_id, bytes = n, "relaying client data to tunnel");
                    let msg = Message::Data {
                        conn_id,
                        data: buf[..n].to_vec(),
                    };
                    if let Err(e) = tunnel_clone.send_message(&msg).await {
                        error!(conn_id, error = %e, "tunnel send failed");
                        // Forwarding has failed, so remaining input cannot
                        // reach the target. Linger briefly to drain unread
                        // kernel bytes before dropping the socket; this keeps
                        // an already-delivered FIN from becoming an immediate
                        // RST without retaining a broken relay indefinitely.
                        let drain = async {
                            loop {
                                match tcp_read.read(&mut buf).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(_) => {}
                                }
                            }
                        };
                        if tokio::time::timeout(FAILED_INPUT_DRAIN_TIMEOUT, drain)
                            .await
                            .is_err()
                        {
                            debug!(
                                conn_id,
                                timeout_seconds = FAILED_INPUT_DRAIN_TIMEOUT.as_secs(),
                                "failed input linger timed out"
                            );
                        }
                        break;
                    }
                }
                Err(e) => {
                    debug!(conn_id, error = %e, "client read failed");
                    break;
                }
            }
        }
    };

    let write = async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                TunnelEvent::Data(data) => {
                    if data.is_empty() {
                        continue;
                    }
                    debug!(
                        conn_id,
                        bytes = data.len(),
                        "relaying tunnel data to client"
                    );
                    if let Err(e) = tcp_write.write_all(&data).await {
                        error!(conn_id, error = %e, "client write failed");
                        return false;
                    }
                }
                TunnelEvent::Close => {
                    debug!(conn_id, "tunnel closed");
                    if let Err(e) = tcp_write.shutdown().await {
                        debug!(conn_id, error = %e, "client write shutdown failed");
                        return false;
                    }
                    return true;
                }
                TunnelEvent::Error(msg) => {
                    debug!(conn_id, error = %msg, "tunnel error");
                    return false;
                }
                TunnelEvent::Exit(_) => {
                    // Only meaningful for remote exec; a TCP relay never sees it.
                    return false;
                }
            }
        }
        false
    };

    tokio::pin!(read);
    tokio::pin!(write);

    let (local_input_closed, graceful_remote_close) = tokio::select! {
        _ = &mut read => (true, false),
        graceful = &mut write => (false, graceful),
    };

    let close = Message::Close { conn_id };
    if local_input_closed {
        // Close is directional on the wire: it ends client -> target input.
        // Keep draining concurrently with the POST so a response burst cannot
        // fill this connection's channel and stall the shared SSE dispatcher.
        let (close_result, _) = tokio::join!(tunnel.send_message(&close), write.as_mut());
        let _ = close_result;
    } else if graceful_remote_close {
        // Target -> client reached EOF, but TCP remains writable in the other
        // direction. Stop tracking response events and keep forwarding local
        // input until the application reaches EOF, then close that direction.
        tunnel.unregister_connection(conn_id).await;
        read.await;
        let _ = tunnel.send_message(&close).await;
        return;
    } else {
        let _ = tunnel.send_message(&close).await;
    }
    tunnel.unregister_connection(conn_id).await;
}
