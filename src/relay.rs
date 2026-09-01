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
    //
    // Only failures raise it. After a clean Close the read half deliberately
    // stays open: the target closing its output says nothing about the app's
    // upload, which the server still accepts, so this relay ends when the app
    // closes its own side. An app that holds a half-closed connection open
    // forever keeps its conn_id registered - correct for TCP, and bounded in
    // practice by the app being a local process rather than a remote peer.
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
        // A clean end-of-stream is only ever earned by an explicit terminal
        // event. Anything else that ends this loop leaves the response
        // truncated, and handing that to the app as a successful EOF is the
        // silent corruption this relay exists to avoid.
        let mut terminal_seen = false;
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
                        failed = true;
                        break;
                    }
                }
                TunnelEvent::Close => {
                    debug!("[{}] tunnel closed", conn_id);
                    terminal_seen = true;
                    // Everything arrived: FIN, so the app reads a clean EOF.
                    let _ = tcp_write.shutdown().await;
                    break;
                }
                TunnelEvent::Error(msg) => {
                    debug!("[{}] tunnel error: {}", conn_id, msg);
                    terminal_seen = true;
                    failed = true;
                    break;
                }
                TunnelEvent::Exit(_) => {
                    // Only meaningful for remote exec; a TCP relay never sees it.
                    terminal_seen = true;
                    break;
                }
            }
        }

        // The channel was dropped with no terminal event: the server session
        // was reset, or the dispatcher dropped this connection as stalled.
        // Either way the response stopped mid-stream.
        if !terminal_seen {
            debug!("[{}] dispatch channel dropped mid-stream", conn_id);
            failed = true;
        }
        if failed {
            // Release the read half so the connection can be aborted: the app
            // is waiting on a response that will never arrive, so it will not
            // close its side on its own.
            failed_notify.notify_one();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::tests::test_tunnel;
    use std::io::Read as _;
    use std::net::TcpListener as StdListener;

    /// The dispatch channel can be dropped without any terminal event: a server
    /// session Reset clears every channel, and the SSE dispatcher drops a
    /// connection it considers stalled. The response is truncated either way,
    /// so the app must not be handed a clean end-of-stream.
    #[tokio::test]
    async fn a_dropped_dispatch_channel_is_not_a_clean_eof() {
        const CONN_ID: u32 = 5;

        // Stand-in for the proxied app, on a blocking socket so we can assert
        // on exactly what a real client's read would see.
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let result = sock.read_to_end(&mut got);
            (result.is_err(), got)
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let tunnel = Arc::new(test_tunnel("pw"));
        let (event_tx, event_rx) = mpsc::channel(8);

        let relay = tokio::spawn(relay(stream, CONN_ID, tunnel, event_rx));

        // Part of a response arrives, then the channel goes away mid-stream
        // with no Close and no Error - exactly what Reset and the stall path do.
        event_tx.send(TunnelEvent::Data(b"partial".to_vec())).await.unwrap();
        drop(event_tx);

        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay did not finish")
            .expect("relay panicked");

        let (errored, got) = tokio::task::spawn_blocking(move || app.join().unwrap())
            .await
            .unwrap();
        assert!(
            errored,
            "truncated response was handed to the app as a clean EOF after {} byte(s)",
            got.len()
        );
    }
}
