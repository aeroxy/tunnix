use crate::tunnel::{Tunnel, TunnelEvent};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error};
use tunnix_common::protocol::Message;

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

    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            match tcp_read.read(&mut buf).await {
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
        let close_msg = Message::Close { conn_id };
        let _ = tunnel_clone.send_message(&close_msg).await;
        tunnel_clone.unregister_connection(conn_id).await;
    });

    let write_task = tokio::spawn(async move {
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
                    break;
                }
                TunnelEvent::Error(msg) => {
                    debug!("[{}] tunnel error: {}", conn_id, msg);
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = read_task => {},
        _ = write_task => {},
    }
}
