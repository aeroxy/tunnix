use crate::tunnel::{Tunnel, TunnelEvent};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error};
use crate::protocol::Message;

static CONN_COUNTER: AtomicU32 = AtomicU32::new(1);

pub fn next_conn_id() -> u32 {
    CONN_COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// How the read half's loop ended.
enum Upload {
    /// The app closed its side; nothing more will be sent.
    Eof,
    /// The app's socket or the tunnel failed. The connection is dead.
    Failed,
    /// The write half stopped us because the connection failed.
    Stopped(Stop),
}

/// What the read half tells the write half once the upload side is done.
enum UploadEnd {
    /// Clean: the app is finished sending. No failure report can follow from
    /// this direction, so the write half may exit once the response is closed.
    Eof,
    /// The upload side itself broke. The write half must fail the connection
    /// now rather than wait for a terminal event that cannot arrive.
    Failed,
}

/// Why the write half is stopping the read half, so it knows whether the
/// server still needs to be told.
enum Stop {
    /// The server reported the failure itself, so it has already released the
    /// target.
    ServerReported,
    /// The failure is local to this client; the server has heard nothing.
    Local,
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

    // The two halves coordinate over two one-shot signals, each carrying its
    // reason, so neither side has to infer state the other already knows.
    //
    // Read half -> write half: how the upload side ended. After the app's EOF
    // the write half deliberately keeps going: the target closing its output
    // says nothing about the app's upload, and the reverse holds too, so this
    // relay ends only when *both* directions are done. An app that holds a
    // half-closed connection open forever keeps its conn_id registered here
    // and its target socket held on the server - correct for TCP, but note
    // that neither side applies an idle timeout, so "forever" is literal.
    let (upload_tx, mut upload_rx) = oneshot::channel::<UploadEnd>();
    // Write half -> read half: stop, the connection has failed. The read half
    // has to stop cooperatively rather than being aborted: aborting would drop
    // its socket half, and both halves are needed to abort the connection.
    let (stop_tx, mut stop_rx) = oneshot::channel::<Stop>();

    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        let outcome = loop {
            let read = tokio::select! {
                // Biased toward the stop signal: once the connection has
                // failed, every further chunk costs another send_message on
                // the tunnel that just broke, and an unbiased poll can keep
                // picking the read for as long as the app has bytes buffered.
                biased;
                stop = &mut stop_rx => {
                    debug!("[{}] connection failed; stopping the upload side", conn_id);
                    // Err means the write half went away without saying why
                    // (it panicked). The server has heard nothing either way.
                    break Upload::Stopped(stop.unwrap_or(Stop::Local));
                }
                // Cancel-safe: no bytes are consumed if the other branch wins.
                result = tcp_read.read(&mut buf) => result,
            };
            match read {
                Ok(0) => {
                    debug!("[{}] client EOF", conn_id);
                    break Upload::Eof;
                }
                Ok(n) => {
                    debug!("[{}] client -> tunnel {} bytes", conn_id, n);
                    let msg = Message::Data {
                        conn_id,
                        data: buf[..n].to_vec(),
                    };
                    if let Err(e) = tunnel_clone.send_message(&msg).await {
                        error!("[{}] tunnel send error: {}", conn_id, e);
                        break Upload::Failed;
                    }
                }
                Err(e) => {
                    debug!("[{}] client read error: {}", conn_id, e);
                    break Upload::Failed;
                }
            }
        };

        // Tell the server how this side ended. Deliberately *not*
        // unregistering the conn_id here: the target may still be streaming a
        // response, and dropping the dispatch channel now would discard it.
        match outcome {
            // The server reported the failure itself, so it has already
            // released the target. Another POST would be redundant, and on a
            // broken tunnel it costs a full reconnect wait before failing.
            Upload::Stopped(Stop::ServerReported) => {}
            // Close: we are done sending. The server drops the target's write
            // half, so the target sees a real FIN - the half-close signal some
            // protocols need to start replying - and keeps its read half, so
            // the response still flows.
            Upload::Eof => {
                let _ = tunnel_clone.send_message(&Message::Close { conn_id }).await;
            }
            // Abort: this connection is dead on our side. Close would only
            // half-close it, and the server would keep pulling the target's
            // response into a conn_id we are about to unregister - for an
            // abandoned download, all the way to the end. Abort releases the
            // target entirely.
            //
            // Sent on a detached task: the app is waiting on a response that
            // will never arrive and has to find that out now, but send_message
            // can spend a full RECONNECT_WAIT on the very tunnel whose failure
            // got us here, so it must not sit between the failure and the RST
            // below. The server still needs to hear it, so hand it off rather
            // than drop it.
            Upload::Failed | Upload::Stopped(Stop::Local) => {
                tokio::spawn(async move {
                    let _ = tunnel_clone.send_message(&Message::Abort { conn_id }).await;
                });
            }
        }

        // Let the write half stop waiting. Only a failure of the upload side
        // itself is reported as one: a stop came *from* the write half, which
        // is already gone.
        let _ = upload_tx.send(match outcome {
            Upload::Failed => UploadEnd::Failed,
            Upload::Eof | Upload::Stopped(_) => UploadEnd::Eof,
        });
        tcp_read
    });

    let write_task = tokio::spawn(async move {
        // A clean end-of-stream is only ever earned by an explicit terminal
        // event. Anything else that ends this loop leaves the connection
        // broken, and handing that to the app as a successful EOF is the
        // silent corruption this relay exists to avoid.
        let mut failed = false;
        // Whether that failure was the server's own report: it has then
        // already released the target, and the read half need not tell it.
        let mut server_reported = false;
        // Set once the response direction has closed cleanly. The upload can
        // still fail on the target after that, and the server reports it as a
        // late Error, so this half keeps draining until the upload side is
        // done rather than exiting on Close and losing that report.
        let mut response_closed = false;
        // Set once the read half has reported the upload's end. Its one-shot
        // is consumed at that point, so this also retires its select branch.
        let mut upload_ended = false;
        loop {
            let event = tokio::select! {
                // Biased so queued events are always drained before the exit
                // signal wins: a late Error must not be lost to the race.
                biased;
                received = event_rx.recv() => match received {
                    Some(event) => event,
                    None => {
                        // The server session was reset, or the dispatcher
                        // dropped this connection as stalled. Before Close
                        // that truncates the response. After Close the
                        // response is complete, but an upload still in
                        // progress now lands on a server that no longer knows
                        // this conn_id and discards it with a 200 - so the
                        // upload has to stop, even though the response
                        // direction was fine.
                        debug!("[{}] dispatch channel dropped before the connection finished", conn_id);
                        failed = true;
                        break;
                    }
                },
                upload = &mut upload_rx, if !upload_ended => {
                    upload_ended = true;
                    match upload {
                        Ok(UploadEnd::Eof) if response_closed => {
                            debug!("[{}] upload finished; nothing left to report", conn_id);
                            break;
                        }
                        // The response is still open: keep draining it. No
                        // failure report can follow from the upload side now.
                        Ok(UploadEnd::Eof) => continue,
                        // The upload side broke - or the read half is gone
                        // without a report, which is a panic. A broken tunnel
                        // produces no events at all, so waiting for a terminal
                        // one that cannot arrive would park this relay, and
                        // the app with it, indefinitely.
                        Ok(UploadEnd::Failed) | Err(_) => {
                            debug!("[{}] upload side failed; failing the connection", conn_id);
                            failed = true;
                            break;
                        }
                    }
                }
            };
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
                    response_closed = true;
                    // Everything arrived: FIN, so the app reads a clean EOF.
                    let _ = tcp_write.shutdown().await;
                    // Not done yet unless the upload side already is - it may
                    // still fail on the target, and the app has to hear about
                    // it.
                    if upload_ended {
                        break;
                    }
                }
                TunnelEvent::Error(msg) => {
                    debug!("[{}] tunnel error: {}", conn_id, msg);
                    failed = true;
                    server_reported = true;
                    break;
                }
                TunnelEvent::Exit(_) => {
                    // Only meaningful for remote exec; a TCP relay never sees
                    // it. If one arrives anyway the response direction has
                    // ended without a Close, so fail the connection rather
                    // than leave the read half parked with nothing to stop it.
                    debug!("[{}] unexpected exit status on a TCP relay", conn_id);
                    failed = true;
                    break;
                }
            }
        }

        if failed {
            // Release the read half so the connection can be aborted: the app
            // is waiting on a response that will never arrive, so it will not
            // close its side on its own.
            let _ = stop_tx.send(if server_reported { Stop::ServerReported } else { Stop::Local });
        }
        // Abort only when the response was *not* already delivered in full.
        // Once Close has been forwarded the app has every byte and a FIN;
        // resetting then makes its kernel discard whatever it has not read
        // yet, destroying a good response to report a failure on the other
        // direction. The upload failing after that is reported by the app's
        // own write failing, not by throwing away what it asked for.
        (tcp_write, failed && !response_closed)
    });

    // Both directions run to completion independently: an upload that finishes
    // must not cut off a response still in flight, and a target that stops
    // replying must not cut off an upload still in progress.
    //
    // On failure the write half signals the read half to stop, so both return
    // their socket halves and the connection can be aborted below.
    let (read_half, write_half) = tokio::join!(read_task, write_task);
    let abort = matches!(&write_half, Ok((_, true)));

    tunnel.unregister_connection(conn_id).await;

    // A truncated response has to fail visibly. Dropping the socket sends FIN,
    // which the app reads as a successful end-of-stream on data that is in fact
    // truncated. Reuniting the halves and setting SO_LINGER to zero makes the
    // close a RST instead, so the app's read fails and it cannot mistake the
    // short stream for a complete one. Reunite consumes both halves, so no FIN
    // escapes on the way. A connection whose response *did* arrive in full is
    // never aborted here - see the write half's return value.
    if !abort {
        return;
    }
    let (Ok(read_half), Ok((write_half, _))) = (read_half, write_half) else {
        debug!("[{}] relay half did not return its socket; closing with FIN", conn_id);
        return;
    };
    match read_half.reunite(write_half) {
        Ok(stream) => {
            // Zero linger makes close() return immediately and emit RST, which
            // is exactly the signal wanted here. (The general set_linger is
            // deprecated because a *non-zero* timeout blocks the thread on
            // drop; this dedicated call carries no such warning.)
            if let Err(e) = stream.set_zero_linger() {
                debug!("[{}] could not set SO_LINGER, closing with FIN: {}", conn_id, e);
            }
        }
        Err(e) => debug!("[{}] could not reunite socket halves: {}", conn_id, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use crate::tunnel::tests::test_tunnel;
    use std::io::Read as _;
    use std::net::TcpListener as StdListener;

    /// The dispatch channel can be dropped without any terminal event: a server
    /// session Reset clears every channel, and the SSE dispatcher drops a
    /// connection it considers stalled. The response is truncated either way,
    /// so the app must not be handed a clean end-of-stream.
    /// Real time, deliberately: the read half's teardown Close goes out on a
    /// detached task, so nothing here waits on the tunnel and the relay tears
    /// down in milliseconds. A paused clock would auto-advance past any timeout
    /// set here while that Close was doing real (non-timer) network I/O.
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

    /// The channel can also be dropped *after* a clean Close, while the app is
    /// still uploading. The server that reset no longer knows the conn_id, so
    /// every further chunk is discarded with a 200. The relay must not sit
    /// waiting for an app that has no reason to stop - it has to stop the
    /// upload side itself. The response direction already completed, so the app
    /// keeps the clean EOF it was given.
    #[tokio::test]
    async fn a_dropped_dispatch_channel_after_close_still_ends_the_relay() {
        const CONN_ID: u32 = 6;

        // Stand-in for the app: it holds its side open, never closing, the way
        // an upload in progress would.
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let app = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = release_rx.recv();
            drop(sock);
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let tunnel = Arc::new(test_tunnel("pw"));
        let (event_tx, event_rx) = mpsc::channel(8);

        let relay = tokio::spawn(relay(stream, CONN_ID, tunnel, event_rx));

        // Response completes cleanly, then the session is reset mid-upload.
        event_tx.send(TunnelEvent::Data(b"reply".to_vec())).await.unwrap();
        event_tx.send(TunnelEvent::Close).await.unwrap();
        drop(event_tx);

        // Without treating the drop as a failure, the read half keeps waiting
        // for the app's EOF and this never returns.
        let finished = tokio::time::timeout(Duration::from_secs(10), relay).await;
        let _ = release_tx.send(());
        tokio::task::spawn_blocking(move || app.join().unwrap()).await.unwrap();
        finished
            .expect("relay kept waiting on the app after its channel was dropped")
            .expect("relay panicked");
    }

    /// A broken upload side must fail the connection, not park it. Nothing is
    /// coming back down the response direction - the tunnel send failed, or the
    /// app's socket died - so a write half that waits for a terminal event
    /// waits forever, holding the relay and its conn_id registration open.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_upload_does_not_park_the_relay() {
        use std::os::fd::AsRawFd;
        const CONN_ID: u32 = 9;

        // App that aborts its side: SO_LINGER with a zero timeout turns close()
        // into a RST, so the relay's read half errors instead of seeing EOF.
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Held until the relay is running: a reset that lands during connect()
        // would fail the handshake instead of the relay's read.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let app = std::thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let _ = release_rx.recv();
            let linger = libc::linger { l_onoff: 1, l_linger: 0 };
            unsafe {
                libc::setsockopt(
                    sock.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    &linger as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::linger>() as libc::socklen_t,
                );
            }
            drop(sock);
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let tunnel = Arc::new(test_tunnel("pw"));
        // Sender held, so the write half gets no channel-drop rescue: only the
        // read half's failure can end it.
        let (_event_tx, event_rx) = mpsc::channel(8);

        let relay = tokio::spawn(relay(stream, CONN_ID, tunnel, event_rx));
        let _ = release_tx.send(());
        tokio::task::spawn_blocking(move || app.join().unwrap()).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay parked after the upload side failed")
            .expect("relay panicked");
    }

    /// A server `Error` before the response finished means the stream is
    /// truncated, so the app has to be failed rather than handed a short read
    /// that looks complete. This is also the only path that sets
    /// `server_reported`, which suppresses the read half's teardown Close.
    #[tokio::test]
    async fn a_server_error_before_close_fails_the_connection() {
        const CONN_ID: u32 = 7;

        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let app = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            sock.read_to_end(&mut got).is_err()
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let tunnel = Arc::new(test_tunnel("pw"));
        let (event_tx, event_rx) = mpsc::channel(8);

        let relay = tokio::spawn(relay(stream, CONN_ID, tunnel, event_rx));

        event_tx.send(TunnelEvent::Data(b"partial".to_vec())).await.unwrap();
        event_tx
            .send(TunnelEvent::Error("target read failed".to_string()))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay did not finish")
            .expect("relay panicked");

        let errored = tokio::task::spawn_blocking(move || app.join().unwrap())
            .await
            .unwrap();
        assert!(errored, "truncated response was handed to the app as a clean EOF");
    }

    /// The mirror case: the `Error` lands *after* the response completed, so it
    /// can only be about the upload direction. Resetting the socket then would
    /// make the app's kernel discard a response it already received in full, so
    /// the connection must close with the FIN the Close already sent.
    #[tokio::test]
    async fn a_server_error_after_close_does_not_destroy_the_response() {
        const CONN_ID: u32 = 8;

        let payload: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();

        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // The app does not read until released, so the response is still
        // sitting unread in its receive buffer when the relay tears down -
        // exactly the bytes a reset would make the kernel throw away.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let app = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = release_rx.recv();
            let mut got = Vec::new();
            let result = sock.read_to_end(&mut got);
            (result.is_ok(), got)
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let tunnel = Arc::new(test_tunnel("pw"));
        let (event_tx, event_rx) = mpsc::channel(8);

        let relay = tokio::spawn(relay(stream, CONN_ID, tunnel, event_rx));

        event_tx.send(TunnelEvent::Data(payload)).await.unwrap();
        event_tx.send(TunnelEvent::Close).await.unwrap();
        event_tx
            .send(TunnelEvent::Error("target write failed".to_string()))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay did not finish")
            .expect("relay panicked");

        let _ = release_tx.send(());
        let (clean, got) = tokio::task::spawn_blocking(move || app.join().unwrap())
            .await
            .unwrap();
        assert!(clean, "a complete response was destroyed by a reset");
        assert_eq!(got, expected, "response payload was lost");
    }
}
