use crate::protocol::Message;
use crate::tunnel::{Tunnel, TunnelEvent};
use rama::{
    telemetry::tracing::{debug, error},
    utils::octets::kib,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

static CONN_COUNTER: AtomicU32 = AtomicU32::new(1);

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
                        break;
                    }
                }
                TunnelEvent::Close => {
                    debug!(conn_id, "tunnel closed");
                    break;
                }
                TunnelEvent::Error(msg) => {
                    debug!(conn_id, error = %msg, "tunnel error");
                    break;
                }
                TunnelEvent::Exit(_) => {
                    // Only meaningful for remote exec; a TCP relay never sees it.
                    break;
                }
            }
        }
    };

    tokio::pin!(read);
    tokio::pin!(write);

    let local_input_closed = tokio::select! {
        _ = &mut read => true,
        _ = &mut write => false,
    };

    let close = Message::Close { conn_id };
    let _ = tunnel.send_message(&close).await;
    if local_input_closed {
        // Close is directional on the wire: it ends client -> target input.
        // Keep delivering target -> client data until the server sends its
        // own Close, preserving responses from protocols that reply after EOF.
        write.await;
    }
    tunnel.unregister_connection(conn_id).await;
}
