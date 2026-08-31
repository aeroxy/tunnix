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
const CLOSE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

pub fn next_conn_id() -> u32 {
    CONN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

async fn send_directional_close(tunnel: &Tunnel, conn_id: u32) -> bool {
    match tokio::time::timeout(
        CLOSE_DELIVERY_TIMEOUT,
        tunnel.send_message(&Message::Close { conn_id }),
    )
    .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(error)) => {
            debug!(conn_id, %error, "tunnel close delivery failed");
            false
        }
        Err(_) => {
            debug!(
                conn_id,
                timeout_seconds = CLOSE_DELIVERY_TIMEOUT.as_secs(),
                "tunnel close delivery timed out"
            );
            false
        }
    }
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
                    return true;
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
                        return false;
                    }
                }
                Err(e) => {
                    debug!(conn_id, error = %e, "client read failed");
                    return false;
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

    let (local_input_closed, clean_local_eof, graceful_remote_close) = tokio::select! {
        clean = &mut read => (true, clean, false),
        graceful = &mut write => (false, false, graceful),
    };

    if local_input_closed {
        if !clean_local_eof {
            // A local read or forwarding failure is not a TCP half-close. Stop
            // response dispatch before the best-effort Close so a dead tunnel
            // cannot leave this relay waiting forever for an event.
            tunnel.unregister_connection(conn_id).await;
            send_directional_close(&tunnel, conn_id).await;
            return;
        }
        // Close is directional on the wire: it ends client -> target input.
        // Keep draining concurrently with the POST so a response burst cannot
        // fill this connection's channel and stall the shared SSE dispatcher.
        let send_close = send_directional_close(&tunnel, conn_id);
        tokio::pin!(send_close);
        tokio::select! {
            close_delivered = &mut send_close => {
                // A successful directional Close keeps the reverse direction
                // alive; a failed POST cannot produce a later terminal event.
                if close_delivered {
                    write.await;
                }
            }
            _ = &mut write => {
                let _ = send_close.await;
            }
        }
    } else if graceful_remote_close {
        // Target -> client reached EOF, but TCP remains writable in the other
        // direction. Stop tracking response events and keep forwarding local
        // input until the application reaches EOF, then close that direction.
        tunnel.unregister_connection(conn_id).await;
        read.await;
        send_directional_close(&tunnel, conn_id).await;
        return;
    } else {
        send_directional_close(&tunnel, conn_id).await;
    }
    tunnel.unregister_connection(conn_id).await;
}
