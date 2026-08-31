use crate::crypto::Crypto;
use crate::protocol::Message;
use crate::reload::{build_http_client, HotClientConfig};
use anyhow::Result;
use arc_swap::ArcSwap;
use base64ct::{Base64, Encoding};
use rama::{
    bytes::Bytes,
    futures::{FutureExt, StreamExt},
    http::{
        body::util::BodyExt,
        service::client::{HttpClientExt, RequestBuilder},
        BodyExtractExt, HeaderMap, Response,
    },
    net::uri::Uri,
    rt::Executor,
    telemetry::tracing::{debug, error, info, warn},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify};

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
        server_url: &Uri,
        crypto: Arc<Crypto>,
        headers: &HeaderMap,
        health_expected: &str,
        exec: Executor,
    ) -> Result<Arc<Self>> {
        let session_id = format!("{:016x}", rand::random::<u64>());

        let http_client = build_http_client(exec.clone());
        let http_headers = headers.clone();

        let server_base_url = server_url.clone();
        let hot = Arc::new(ArcSwap::from_pointee(HotClientConfig {
            crypto,
            http_client,
            http_headers,
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
        let health_url = endpoint(&server_base_url, ["health"]);
        info!(url = %health_url, "checking tunnel server health");
        let hot_snap = tunnel.hot.load();
        let resp = with_http_headers(hot_snap.http_client.get(health_url), &hot_snap.http_headers)
            .send()
            .await
            .map_err(http_error)?;
        let body = resp.try_into_string().await.map_err(http_error)?;
        info!(response = body.trim(), "tunnel server health check passed");
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
        exec.spawn_cancellable_task(async move {
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
                        error!(error = %e, retry_after_seconds = 3, "SSE stream failed");
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
            warn!(
                "SSE stream not ready after {:?}; proceeding anyway",
                RECONNECT_WAIT
            );
        }

        Ok(tunnel)
    }

    /// Read SSE events and dispatch to connection handlers
    async fn sse_read_loop(&self, is_reconnect: bool) -> Result<()> {
        let hot = self.hot.load();
        let sid = self.session_id.read().await;
        let url = endpoint(&hot.server_base_url, ["stream", sid.as_str()]);
        info!(session_id = %sid, base_url = %hot.server_base_url, "opening SSE stream");
        let captured_sid = sid.clone();
        drop(sid);
        // `send()` resolves at response headers, so this bounds only the
        // connection setup, not the long-lived streaming body.
        let resp = tokio::time::timeout(
            SSE_CONNECT_TIMEOUT,
            with_http_headers(hot.http_client.get(url), &hot.http_headers).send(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("SSE connect timed out after {:?}", SSE_CONNECT_TIMEOUT))?
        .map_err(http_error)?;

        if !resp.status().is_success() {
            anyhow::bail!("SSE stream failed: {}", resp.status());
        }

        info!(session_id = %captured_sid, "SSE stream connected");

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
            let _ = self.reconnect_signal.notified().now_or_never();
        } else {
            drop(current_sid);
            anyhow::bail!("forced reconnect");
        }
        drop(current_sid);

        let mut stream = resp.into_body().into_string_data_event_stream();
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
                    let event = match chunk {
                        Ok(Some(event)) => event.map_err(http_error)?,
                        Ok(None) => break,
                        Err(_) => {
                            warn!(timeout_seconds = 30, "SSE read timed out; reconnecting");
                            break;
                        }
                    };

                    if let Some(data) = event.into_data() {
                        match Base64::decode_vec(data.trim()) {
                            Ok(encrypted) => {
                                self.handle_sse_message(&encrypted).await;
                                if !signaled_ready {
                                    self.sse_ready.notify_waiters();
                                    signaled_ready = true;
                                }
                            }
                            Err(e) => warn!(error = %e, "SSE payload base64 decode failed"),
                        }
                    } else if is_reconnect && !signaled_ready {
                        // Rama exposes keepalive comments as data-less events.
                        // A reconnect does not carry Reset, so headers plus any
                        // parsed event are enough to release the retry path.
                        self.sse_ready.notify_waiters();
                        signaled_ready = true;
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
                error!(error = %e, "SSE payload decryption failed");
                return;
            }
        };

        let message = match Message::from_bytes(&plaintext) {
            Ok(m) => m,
            Err(e) => {
                error!(error = %e, "SSE payload deserialization failed");
                return;
            }
        };

        match message {
            Message::Data { conn_id, data } => {
                debug!(conn_id, bytes = data.len(), "received SSE tunnel data");
                // Clone the sender out of the guard before awaiting send().
                // Holding the mutex across `tx.send().await` lets one slow
                // consumer (full channel) stall dispatch for every connection.
                let tx = {
                    let channels = self.response_channels.lock().await;
                    channels.get(&conn_id).cloned()
                };
                if let Some(tx) = tx {
                    let _ = tx.send(TunnelEvent::Data(data)).await;
                }
            }
            Message::Close { conn_id } => {
                debug!(conn_id, "received SSE tunnel close");
                let tx = {
                    let channels = self.response_channels.lock().await;
                    channels.get(&conn_id).cloned()
                };
                if let Some(tx) = tx {
                    let _ = tx.send(TunnelEvent::Close).await;
                }
            }
            Message::Error { conn_id, message } => {
                warn!(?conn_id, error = %message, "received SSE tunnel error");
                if let Some(cid) = conn_id {
                    let tx = {
                        let channels = self.response_channels.lock().await;
                        channels.get(&cid).cloned()
                    };
                    if let Some(tx) = tx {
                        let _ = tx.send(TunnelEvent::Error(message)).await;
                    }
                }
            }
            Message::ExitStatus { conn_id, code } => {
                debug!(conn_id, code, "received SSE process exit status");
                let tx = {
                    let channels = self.response_channels.lock().await;
                    channels.get(&conn_id).cloned()
                };
                if let Some(tx) = tx {
                    let _ = tx.send(TunnelEvent::Exit(code)).await;
                }
            }
            Message::Reset => {
                // Server signalled the session was freshly created (e.g. it
                // restarted). Drop every pending response channel so the
                // relay tasks exit and their SOCKS5/HTTP clients reconnect.
                let mut channels = self.response_channels.lock().await;
                let count = channels.len();
                channels.clear();
                if count > 0 {
                    warn!(
                        "Server session reset: tearing down {} pending connection(s)",
                        count
                    );
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
        let encrypted = Bytes::from(hot.crypto.encrypt(&bytes)?);

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
                warn!(
                    "send failed: {}; forcing SSE reconnect and retrying",
                    first_err
                );
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

    async fn try_post(&self, sid: &str, encrypted: Bytes) -> Result<Option<Vec<u8>>> {
        let hot = self.hot.load();
        let url = endpoint(&hot.server_base_url, ["send", sid]);
        let resp = with_http_headers(hot.http_client.post(url), &hot.http_headers)
            .body(encrypted)
            .send()
            .await
            .map_err(http_error)?;

        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(http_error)?
            .to_bytes();

        if !status.is_success() {
            anyhow::bail!(
                "Server error: {} {}",
                status,
                String::from_utf8_lossy(&body)
            );
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

    /// Register a connection and return event receiver
    pub async fn register_connection(&self, conn_id: u32) -> mpsc::Receiver<TunnelEvent> {
        let (tx, rx) = mpsc::channel(256);
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

fn endpoint<const N: usize>(base: &Uri, segments: [&str; N]) -> Uri {
    let mut uri = base.clone();
    {
        let mut path = uri.path_mut();
        for segment in segments {
            path.push_segment(segment);
        }
    }
    uri
}

fn with_http_headers<'a, S, B, M>(
    mut request: RequestBuilder<'a, S, Response<B>, M>,
    headers: &HeaderMap,
) -> RequestBuilder<'a, S, Response<B>, M> {
    for (name, value) in headers.ordered_iter() {
        request = request.header(name.clone(), value.clone());
    }
    request
}

fn http_error(error: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Tunnel without a live server. handle_sse_message only touches
    /// `hot.crypto` (to decrypt) and `response_channels` (to dispatch), so the
    /// http_client / server_base_url are placeholders.
    fn test_tunnel(password: &str) -> Tunnel {
        let hot = HotClientConfig {
            crypto: Arc::new(Crypto::new(password).unwrap()),
            http_client: build_http_client(Executor::default()),
            http_headers: Default::default(),
            server_base_url: Uri::parse("http://127.0.0.1:0").unwrap(),
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

    #[test]
    fn endpoint_appends_and_encodes_typed_path_segments() {
        let base = Uri::parse("https://example.com/tunnix/").unwrap();
        let endpoint = endpoint(&base, ["stream", "id/with space"]);
        assert_eq!(
            endpoint.to_string(),
            "https://example.com/tunnix/stream/id%2Fwith%20space"
        );
    }

    #[tokio::test]
    async fn data_is_dispatched_to_the_registered_conn() {
        let tunnel = test_tunnel("pw");
        let mut rx = tunnel.register_connection(1).await;

        let frame = sse_frame(
            &tunnel,
            &Message::Data {
                conn_id: 1,
                data: b"hello".to_vec(),
            },
        );
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

        let frame = sse_frame(
            &tunnel,
            &Message::ExitStatus {
                conn_id: 3,
                code: 42,
            },
        );
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
        let frame = sse_frame(
            &tunnel,
            &Message::Data {
                conn_id: 99,
                data: vec![1, 2, 3],
            },
        );
        tunnel.handle_sse_message(&frame).await;
        assert!(tunnel.response_channels.lock().await.is_empty());
    }

    #[tokio::test]
    async fn undecryptable_frame_is_dropped_without_dispatch() {
        let tunnel = test_tunnel("right-pw");
        let mut rx = tunnel.register_connection(1).await;

        // Frame encrypted under a different key fails to decrypt and is ignored.
        let wrong = Crypto::new("wrong-pw").unwrap();
        let bytes = Message::Data {
            conn_id: 1,
            data: b"x".to_vec(),
        }
        .to_bytes()
        .unwrap();
        let frame = wrong.encrypt(&bytes).unwrap();
        tunnel.handle_sse_message(&frame).await;

        assert!(rx.try_recv().is_err(), "nothing should have been delivered");
    }
}
