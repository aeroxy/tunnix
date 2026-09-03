use anyhow::Result;
use arc_swap::ArcSwap;
use base64ct::{Base64, Encoding};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};
use crate::crypto::Crypto;
use crate::protocol::Message;
use crate::reload::{build_http_client, HotClientConfig};

/// Bound on establishing the SSE GET (connect + response headers). The
/// http_client has no global timeout (it would kill the streaming body), and
/// a half-open pooled connection otherwise wedges `send().await` forever —
/// the reconnect task is the only one, so the whole tunnel dies silently.
const SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Time send_message waits for the SSE stream to become ready after forcing
/// a reconnect. Must exceed SSE_CONNECT_TIMEOUT (15s): the GET alone can take
/// that long under network congestion, and this also covers first-frame
/// processing before sse_ready fires.
const RECONNECT_WAIT: Duration = Duration::from_secs(20);
/// Per-connection event queue depth. Deep enough that a merely slow local app
/// keeps draining without stalling dispatch.
const CONN_CHANNEL_CAPACITY: usize = 256;
/// How long a single event may wait for one connection's consumer before that
/// connection is declared wedged and dropped. All connections share one
/// dispatch loop, so an unbounded wait here starves every other connection on
/// the tunnel.
///
/// Must stay well under the server's per-message delivery budget in
/// `send_to_client` (50 attempts x (500ms + 100ms) ~= 30s). Dispatch runs
/// inline in the SSE read loop, so this is also how long the client can go
/// without reading the SSE socket at all: at the same magnitude as that budget,
/// one stalled consumer would let the server exhaust its retries on unrelated
/// healthy connections and tear them down as unreachable. 5s is still ~20x what
/// a draining 256-slot queue needs.
const DISPATCH_STALL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stalled `Close` may wait off the dispatch loop before its
/// connection is given up on. Generous where `DISPATCH_STALL_TIMEOUT` is tight:
/// this wait blocks no other connection, and the alternative is resetting an
/// app whose response was in fact complete.
const CLOSE_HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);

/// Events received from server via SSE
#[derive(Debug)]
pub enum TunnelEvent {
    Data(Vec<u8>),
    Close,
    Error(String),
    /// Remote process exited with this code (remote exec).
    Exit(i32),
}

/// Tunnel handles communication with the server via SSE + HTTP POST
pub struct Tunnel {
    pub session_id: Arc<tokio::sync::RwLock<String>>,
    pub hot: Arc<ArcSwap<HotClientConfig>>,
    pub response_channels: Arc<Mutex<HashMap<u32, mpsc::Sender<TunnelEvent>>>>,
    pub reconnect_signal: Arc<Notify>,
    sse_ready: Arc<Notify>,
}

impl Tunnel {
    /// Connect to the server: open SSE stream and start reading
    pub async fn connect(
        server_url: &str,
        crypto: Arc<Crypto>,
        headers: &HashMap<String, String>,
        health_expected: &str,
    ) -> Result<Arc<Self>> {
        let session_id = format!("{:016x}", rand::random::<u64>());

        let http_client = build_http_client(headers)?;

        let server_base_url = server_url.trim_end_matches('/').to_string();
        let hot = Arc::new(ArcSwap::from_pointee(HotClientConfig {
            crypto,
            http_client,
            server_base_url: server_base_url.clone(),
        }));

        let tunnel = Arc::new(Tunnel {
            session_id: Arc::new(tokio::sync::RwLock::new(session_id.clone())),
            hot,
            response_channels: Arc::new(Mutex::new(HashMap::new())),
            reconnect_signal: Arc::new(Notify::new()),
            sse_ready: Arc::new(Notify::new()),
        });

        // Test connection with health check
        let health_url = format!("{}/health", server_base_url);
        info!("Testing connection to {}", health_url);
        let hot_snap = tunnel.hot.load();
        let resp = hot_snap.http_client.get(&health_url).send().await?;
        let body = resp.text().await?;
        info!("Server health: {}", body.trim());
        if body.trim() != health_expected.trim() {
            anyhow::bail!(
                "Health check mismatch: expected {:?}, got {:?}",
                health_expected.trim(),
                body.trim()
            );
        }

        // Register interest in the first "SSE ready" notification BEFORE spawning
        // the reader (via enable()), so we can't miss it if the stream connects
        // fast. We hold a cloned Arc so the future doesn't borrow `tunnel`.
        let sse_ready = tunnel.sse_ready.clone();
        let ready = sse_ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();

        // Open SSE stream
        let tunnel_clone = tunnel.clone();
        tokio::spawn(async move {
            // First iteration is the initial connect — gate readiness on the
            // first `data:` frame so a fresh-session Reset is consumed before
            // `register_connection` lands. On reconnects, the server won't send
            // a Reset and the first event may be a keepalive comment; in that
            // case signal on any event to avoid stalling `send_message`'s
            // retry for the full RECONNECT_WAIT timeout.
            let mut is_reconnect = false;
            loop {
                let res = tunnel_clone.sse_read_loop(is_reconnect).await;
                is_reconnect = true;
                match res {
                    Err(e) if e.to_string().contains("forced reconnect") => continue,
                    Err(e) => {
                        error!("SSE stream error: {}, reconnecting in 3s...", e);
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    }
                    Ok(()) => {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        });

        // Wait for the stream to actually establish — the server creates the
        // session when it handles GET /stream, so the first POST /send must not
        // race ahead of it (otherwise: 503 "unknown session").
        if tokio::time::timeout(RECONNECT_WAIT, ready).await.is_err() {
            warn!("SSE stream not ready after {:?}; proceeding anyway", RECONNECT_WAIT);
        }

        Ok(tunnel)
    }

    /// Read SSE events and dispatch to connection handlers
    async fn sse_read_loop(&self, is_reconnect: bool) -> Result<()> {
        let hot = self.hot.load();
        let sid = self.session_id.read().await;
        let url = format!("{}/stream/{}", hot.server_base_url, *sid);
        info!("Opening SSE stream for session {} at {}", sid, hot.server_base_url);
        let captured_sid = sid.clone();
        drop(sid);
        // `send()` resolves at response headers, so this bounds only the
        // connection setup, not the long-lived streaming body.
        let resp = tokio::time::timeout(SSE_CONNECT_TIMEOUT, hot.http_client.get(&url).send())
            .await
            .map_err(|_| anyhow::anyhow!("SSE connect timed out after {:?}", SSE_CONNECT_TIMEOUT))??;

        if !resp.status().is_success() {
            anyhow::bail!("SSE stream failed: {}", resp.status());
        }

        info!("SSE stream connected");

        // Drain any reconnect permit buffered while we were connecting
        // (send_message fires notify_one on POST failure; if it fired during
        // the GET above, the permit would otherwise tear down this freshly
        // established stream on the first select! iteration).
        //
        // Only drain if the session_id hasn't changed: the config watcher
        // rotates the session_id *before* firing reconnect_signal, so a
        // changed sid means its permit is in-flight and must NOT be consumed
        // (otherwise the stream stays on the old session). Holding the read
        // lock during the drain prevents the config watcher from rotating
        // mid-check, since it needs the write lock first.
        let current_sid = self.session_id.read().await;
        if *current_sid == captured_sid {
            use futures::FutureExt;
            let _ = self.reconnect_signal.notified().now_or_never();
        } else {
            drop(current_sid);
            anyhow::bail!("forced reconnect");
        }
        drop(current_sid);

        use futures::StreamExt;
        let mut stream = resp.bytes_stream();

        let mut buffer = String::new();
        // On the initial connect, signal readiness only AFTER the first
        // `data:` frame is processed: a new session's first data frame is
        // `Reset` (which clears pending channels), and handling it before
        // `connect()` returns ensures the first registration + POST can't be
        // wiped by a late Reset. We can't just wait for the first chunk: the
        // server's keepalive `:\n\n` comment can be emitted ahead of the
        // queued Reset (its interval's first tick is immediate), so only a
        // real data frame guarantees the Reset is consumed.
        //
        // On reconnects the server won't queue a Reset, so the first event may
        // be a keepalive; signal on any event to avoid stalling send_message
        // for the full RECONNECT_WAIT timeout.
        let mut signaled_ready = false;

        loop {
            tokio::select! {
                chunk = tokio::time::timeout(Duration::from_secs(30), stream.next()) => {
                    let chunk = match chunk {
                        Ok(Some(c)) => c?,
                        Ok(None) => break,
                        Err(_) => {
                            warn!("SSE read timeout, reconnecting");
                            break;
                        }
                    };
                    let text = String::from_utf8_lossy(&chunk);
                    buffer.push_str(&text);

                    while let Some(pos) = buffer.find("\n\n") {
                        let event = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        let mut had_data = false;
                        for line in event.lines() {
                            if let Some(data_str) = line.strip_prefix("data: ") {
                                match Base64::decode_vec(data_str.trim()) {
                                    Ok(encrypted) => {
                                        self.handle_sse_message(&encrypted).await;
                                        had_data = true;
                                    }
                                    Err(e) => {
                                        warn!("Base64 decode error: {}", e);
                                    }
                                }
                            }
                        }
                        if had_data && !signaled_ready {
                            self.sse_ready.notify_waiters();
                            signaled_ready = true;
                        } else if is_reconnect && !signaled_ready {
                            // Keepalive comment — the stream is up and the
                            // server is not going to send a Reset. Notify
                            // send_message's retry path so it doesn't burn
                            // the full RECONNECT_WAIT.
                            self.sse_ready.notify_waiters();
                            signaled_ready = true;
                        }
                    }
                }
                _ = self.reconnect_signal.notified() => {
                    warn!("SSE forced reconnect (triggered by send failure)");
                    anyhow::bail!("forced reconnect");
                }
            }
        }

        warn!("SSE stream ended");
        Ok(())
    }

    /// Process a decrypted message from SSE
    async fn handle_sse_message(&self, encrypted: &[u8]) {
        let hot = self.hot.load();
        let plaintext = match hot.crypto.decrypt(encrypted) {
            Ok(p) => p,
            Err(e) => {
                error!("Decrypt failed: {}", e);
                return;
            }
        };

        let message = match Message::from_bytes(&plaintext) {
            Ok(m) => m,
            Err(e) => {
                error!("Deserialize failed: {}", e);
                return;
            }
        };

        match message {
            Message::Data { conn_id, data } => {
                debug!("[{}] SSE data {} bytes", conn_id, data.len());
                self.dispatch_event(conn_id, TunnelEvent::Data(data)).await;
            }
            Message::Close { conn_id } => {
                debug!("[{}] SSE close", conn_id);
                self.dispatch_event(conn_id, TunnelEvent::Close).await;
            }
            Message::Error { conn_id, message } => {
                warn!("[{:?}] SSE error: {}", conn_id, message);
                if let Some(cid) = conn_id {
                    self.dispatch_event(cid, TunnelEvent::Error(message)).await;
                }
            }
            Message::ExitStatus { conn_id, code } => {
                debug!("[{}] SSE exit status {}", conn_id, code);
                self.dispatch_event(conn_id, TunnelEvent::Exit(code)).await;
            }
            Message::Reset => {
                // Server signalled the session was freshly created (e.g. it
                // restarted). Drop every pending response channel so the
                // relay tasks exit and their SOCKS5/HTTP clients reconnect.
                let mut channels = self.response_channels.lock().await;
                let count = channels.len();
                channels.clear();
                if count > 0 {
                    warn!("Server session reset: tearing down {} pending connection(s)", count);
                } else {
                    debug!("Server session reset (no pending connections)");
                }
            }
            Message::Pong => debug!("PONG"),
            _ => {}
        }
    }

    pub async fn send_message(&self, msg: &Message) -> Result<Option<Vec<u8>>> {
        let hot = self.hot.load();
        let bytes = msg.to_bytes()?;
        let encrypted = bytes::Bytes::from(hot.crypto.encrypt(&bytes)?);

        let sid = self.session_id.read().await.clone();

        match self.try_post(&sid, encrypted.clone()).await {
            Ok(v) => Ok(v),
            Err(first_err) => {
                // Any failure — including 503 "unknown session" after a
                // server restart — is handled by forcing an SSE reconnect
                // and retrying once. The session ID is deliberately NOT
                // rotated: the server (re)creates the session on
                // GET /stream/{sid}, and a genuinely fresh session announces
                // itself with Reset, which clears stale conn state. Keeping
                // the ID stable lets concurrent failures converge on one
                // reconnect instead of racing to rotate (the old "death
                // spiral"), and preserves in-flight server relays when the
                // server didn't actually restart.
                warn!("send failed: {}; forcing SSE reconnect and retrying", first_err);
                // Register interest BEFORE signaling, so the SSE task can't
                // win the race and fire sse_ready between notify_one and our
                // first poll.
                let ready = self.sse_ready.notified();
                tokio::pin!(ready);
                ready.as_mut().enable();
                self.reconnect_signal.notify_one();
                let _ = tokio::time::timeout(RECONNECT_WAIT, ready).await;
                // Re-read: the hot-reload watcher may have rotated the sid
                // (password/header change) while we waited.
                let sid = self.session_id.read().await.clone();
                self.try_post(&sid, encrypted).await
            }
        }
    }

    async fn try_post(&self, sid: &str, encrypted: bytes::Bytes) -> Result<Option<Vec<u8>>> {
        let hot = self.hot.load();
        let url = format!("{}/send/{}", hot.server_base_url, sid);
        let resp = hot
            .http_client
            .post(&url)
            .body(encrypted)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.bytes().await?;

        if !status.is_success() {
            anyhow::bail!("Server error: {} {}", status, String::from_utf8_lossy(&body));
        }

        if body.is_empty() {
            Ok(None)
        } else {
            Ok(Some(body.to_vec()))
        }
    }

    /// Send a Connect message and decrypt the ACK from the HTTP response body
    pub async fn send_connect(&self, msg: &Message) -> Result<Option<Message>> {
        match self.send_message(msg).await? {
            Some(data) if !data.is_empty() => {
                let hot = self.hot.load();
                let plaintext = hot.crypto.decrypt(&data)?;
                let response = Message::from_bytes(&plaintext)?;
                Ok(Some(response))
            }
            _ => Ok(None),
        }
    }

    /// Hand one event to a registered connection. Unknown conn_ids are a silent
    /// no-op (the relay may already have torn down).
    ///
    /// Every connection is fed from a single dispatch loop, so this must never
    /// wait indefinitely: a consumer that has stopped reading fills its queue,
    /// and an unbounded send would then starve every other connection on the
    /// tunnel. A connection that makes no progress within
    /// `DISPATCH_STALL_TIMEOUT` is dropped instead — closing its channel ends
    /// its relay, which closes the local socket so the app sees the failure.
    /// Note this bounds the stall, not the latency: dispatch can still be held
    /// up for one timeout by a consumer that stops reading.
    ///
    /// `Close` is the exception, and is handed off instead of dropped — see
    /// below.
    async fn dispatch_event(&self, conn_id: u32, event: TunnelEvent) {
        // Clone the sender out of the guard before awaiting: holding the mutex
        // across send() would block session ops and every other dispatch too.
        let tx = {
            let channels = self.response_channels.lock().await;
            channels.get(&conn_id).cloned()
        };
        let Some(tx) = tx else { return };

        // reserve() rather than send(): a send that loses its race with the
        // timeout drops the event with it, and the stalled `Close` below has to
        // survive to be handed off.
        let stalled = match tokio::time::timeout(DISPATCH_STALL_TIMEOUT, tx.reserve()).await {
            Ok(Ok(permit)) => {
                permit.send(event);
                return;
            }
            // Receiver gone: the relay already tore down. Nothing to report.
            Ok(Err(_)) => return,
            Err(_) => event,
        };

        // Dropping the connection here is right for a stalled `Data`: the
        // response is truncated whatever we do, so the app has to be failed.
        // A stalled `Close` is not - everything before it is already queued, so
        // the app's response is complete and dropping the connection would
        // reset it over nothing but the end-of-stream marker. Wait that one out
        // off the dispatch loop, where it starves nobody. Ordering is safe: the
        // only event that can follow a `Close` is a late `Error`, and an
        // `Error` that overtakes it just fails the connection - which is what
        // that `Error` means anyway.
        if matches!(stalled, TunnelEvent::Close) {
            debug!("[{}] consumer stalled; delivering Close off the dispatch loop", conn_id);
            let channels = self.response_channels.clone();
            tokio::spawn(async move {
                if tokio::time::timeout(CLOSE_HANDOFF_TIMEOUT, tx.send(stalled))
                    .await
                    .is_err()
                {
                    warn!("[{}] consumer never took its Close; dropping the connection", conn_id);
                    channels.lock().await.remove(&conn_id);
                }
            });
            return;
        }

        warn!(
            "[{}] consumer stalled for {:?}; dropping the connection to keep the tunnel moving",
            conn_id, DISPATCH_STALL_TIMEOUT
        );
        self.unregister_connection(conn_id).await;
    }

    /// Register a connection and return event receiver
    pub async fn register_connection(&self, conn_id: u32) -> mpsc::Receiver<TunnelEvent> {
        let (tx, rx) = mpsc::channel(CONN_CHANNEL_CAPACITY);
        let mut channels = self.response_channels.lock().await;
        channels.insert(conn_id, tx);
        rx
    }

    /// Unregister a connection
    pub async fn unregister_connection(&self, conn_id: u32) {
        let mut channels = self.response_channels.lock().await;
        channels.remove(&conn_id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a Tunnel without a live server. handle_sse_message only touches
    /// `hot.crypto` (to decrypt) and `response_channels` (to dispatch), so the
    /// http_client / server_base_url are placeholders.
    pub(crate) fn test_tunnel(password: &str) -> Tunnel {
        let hot = HotClientConfig {
            crypto: Arc::new(Crypto::new(password).unwrap()),
            http_client: reqwest::Client::new(),
            server_base_url: "http://127.0.0.1:0".to_string(),
        };
        Tunnel {
            session_id: Arc::new(tokio::sync::RwLock::new("test-session".to_string())),
            hot: Arc::new(ArcSwap::from_pointee(hot)),
            response_channels: Arc::new(Mutex::new(HashMap::new())),
            reconnect_signal: Arc::new(Notify::new()),
            sse_ready: Arc::new(Notify::new()),
        }
    }

    /// Encrypt a message the way the server would before pushing it over SSE.
    fn sse_frame(tunnel: &Tunnel, msg: &Message) -> Vec<u8> {
        let bytes = msg.to_bytes().unwrap();
        tunnel.hot.load().crypto.encrypt(&bytes).unwrap()
    }

    #[tokio::test]
    async fn data_is_dispatched_to_the_registered_conn() {
        let tunnel = test_tunnel("pw");
        let mut rx = tunnel.register_connection(1).await;

        let frame = sse_frame(&tunnel, &Message::Data { conn_id: 1, data: b"hello".to_vec() });
        tunnel.handle_sse_message(&frame).await;

        match rx.try_recv() {
            Ok(TunnelEvent::Data(d)) => assert_eq!(d, b"hello"),
            other => panic!("expected Data event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn close_delivers_a_close_event() {
        let tunnel = test_tunnel("pw");
        let mut rx = tunnel.register_connection(7).await;

        let frame = sse_frame(&tunnel, &Message::Close { conn_id: 7 });
        tunnel.handle_sse_message(&frame).await;

        assert!(matches!(rx.try_recv(), Ok(TunnelEvent::Close)));
    }

    #[tokio::test]
    async fn exit_status_delivers_an_exit_event() {
        let tunnel = test_tunnel("pw");
        let mut rx = tunnel.register_connection(3).await;

        let frame = sse_frame(&tunnel, &Message::ExitStatus { conn_id: 3, code: 42 });
        tunnel.handle_sse_message(&frame).await;

        assert!(matches!(rx.try_recv(), Ok(TunnelEvent::Exit(42))));
    }

    #[tokio::test]
    async fn reset_clears_channels_and_closes_every_receiver() {
        let tunnel = test_tunnel("pw");
        let mut rx1 = tunnel.register_connection(1).await;
        let mut rx2 = tunnel.register_connection(2).await;
        assert_eq!(tunnel.response_channels.lock().await.len(), 2);

        let frame = sse_frame(&tunnel, &Message::Reset);
        tunnel.handle_sse_message(&frame).await;

        // The map is emptied...
        assert!(tunnel.response_channels.lock().await.is_empty());
        // ...and each dropped sender closes its receiver, so the relay tasks
        // waiting on event_rx observe None and exit.
        assert!(rx1.recv().await.is_none());
        assert!(rx2.recv().await.is_none());
    }

    #[tokio::test]
    async fn data_for_an_unknown_conn_is_a_silent_no_op() {
        let tunnel = test_tunnel("pw");
        // No registration for conn 99: must not panic and must not register one.
        let frame = sse_frame(&tunnel, &Message::Data { conn_id: 99, data: vec![1, 2, 3] });
        tunnel.handle_sse_message(&frame).await;
        assert!(tunnel.response_channels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn undecryptable_frame_is_dropped_without_dispatch() {
        let tunnel = test_tunnel("right-pw");
        let mut rx = tunnel.register_connection(1).await;

        // Frame encrypted under a different key fails to decrypt and is ignored.
        let wrong = Crypto::new("wrong-pw").unwrap();
        let bytes = Message::Data { conn_id: 1, data: b"x".to_vec() }.to_bytes().unwrap();
        let frame = wrong.encrypt(&bytes).unwrap();
        tunnel.handle_sse_message(&frame).await;

        assert!(rx.try_recv().is_err(), "nothing should have been delivered");
    }

    /// One connection whose consumer has stopped reading must not wedge the
    /// shared dispatch loop: its channel fills, and an unbounded send there
    /// parks forever, starving every other connection on the tunnel.
    /// Time is paused, so the stall timeout elapses without a real wait.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_consumer_cannot_wedge_the_dispatch_loop() {
        let tunnel = test_tunnel("pw");
        // Held but never drained: this is the stalled local app.
        let _stalled = tunnel.register_connection(1).await;
        let mut healthy = tunnel.register_connection(2).await;

        // Fill the stalled connection's channel to capacity.
        let stalled_frame = sse_frame(&tunnel, &Message::Data { conn_id: 1, data: vec![0u8; 8] });
        for _ in 0..CONN_CHANNEL_CAPACITY {
            tunnel.handle_sse_message(&stalled_frame).await;
        }

        // The next send has nowhere to go. It must give up rather than park.
        tokio::time::timeout(Duration::from_secs(600), tunnel.handle_sse_message(&stalled_frame))
            .await
            .expect("dispatch loop parked on a full channel");

        // A wedged connection gets dropped, not carried forever.
        assert!(
            !tunnel.response_channels.lock().await.contains_key(&1),
            "stalled connection should have been torn down"
        );

        // The whole point: other connections still get served.
        let healthy_frame = sse_frame(&tunnel, &Message::Data { conn_id: 2, data: b"fine".to_vec() });
        tokio::time::timeout(Duration::from_secs(600), tunnel.handle_sse_message(&healthy_frame))
            .await
            .expect("dispatch to a healthy connection was blocked");
        match healthy.try_recv() {
            Ok(TunnelEvent::Data(d)) => assert_eq!(d, b"fine"),
            other => panic!("expected Data on the healthy conn, got {:?}", other),
        }
    }

    /// A `Close` that arrives while the consumer is stalled must not tear the
    /// connection down: every byte before it is already queued, so the app's
    /// response is complete and dropping it would reset the app over nothing
    /// but the end-of-stream marker. Time is paused, so the stall timeout
    /// elapses without a real wait.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_close_is_handed_off_rather_than_dropped() {
        let tunnel = test_tunnel("pw");
        let mut rx = tunnel.register_connection(1).await;

        // Fill the queue: the app has stopped reading mid-response.
        let data = sse_frame(&tunnel, &Message::Data { conn_id: 1, data: vec![7u8; 8] });
        for _ in 0..CONN_CHANNEL_CAPACITY {
            tunnel.handle_sse_message(&data).await;
        }

        // The Close has nowhere to go. Dispatch must neither park on it nor
        // drop the connection because of it.
        let close = sse_frame(&tunnel, &Message::Close { conn_id: 1 });
        tokio::time::timeout(Duration::from_secs(600), tunnel.handle_sse_message(&close))
            .await
            .expect("dispatch loop parked on a stalled Close");
        assert!(
            tunnel.response_channels.lock().await.contains_key(&1),
            "a stalled Close should not tear the connection down"
        );

        // The app resumes reading: it gets its whole response, then the Close.
        for _ in 0..CONN_CHANNEL_CAPACITY {
            assert!(matches!(rx.recv().await, Some(TunnelEvent::Data(_))));
        }
        assert!(
            matches!(rx.recv().await, Some(TunnelEvent::Close)),
            "the handed-off Close never arrived"
        );
    }
}
