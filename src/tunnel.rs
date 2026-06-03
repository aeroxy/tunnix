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

const RECONNECT_WAIT: Duration = Duration::from_secs(10);

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
            // retry for the full RECONNECT_WAIT (10s) timeout.
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
        if tokio::time::timeout(Duration::from_secs(10), ready).await.is_err() {
            warn!("SSE stream not ready after 10s; proceeding anyway");
        }

        Ok(tunnel)
    }

    /// Read SSE events and dispatch to connection handlers
    async fn sse_read_loop(&self, is_reconnect: bool) -> Result<()> {
        let hot = self.hot.load();
        let sid = self.session_id.read().await;
        let url = format!("{}/stream/{}", hot.server_base_url, *sid);
        info!("Opening SSE stream for session {} at {}", sid, hot.server_base_url);
        drop(sid);
        let resp = hot.http_client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("SSE stream failed: {}", resp.status());
        }

        info!("SSE stream connected");

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
        // for the full RECONNECT_WAIT (10s) timeout.
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
                            // the full 10s RECONNECT_WAIT.
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
                debug!("[{}] SSE close", conn_id);
                let tx = {
                    let channels = self.response_channels.lock().await;
                    channels.get(&conn_id).cloned()
                };
                if let Some(tx) = tx {
                    let _ = tx.send(TunnelEvent::Close).await;
                }
            }
            Message::Error { conn_id, message } => {
                warn!("[{:?}] SSE error: {}", conn_id, message);
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
                debug!("[{}] SSE exit status {}", conn_id, code);
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
        let encrypted = hot.crypto.encrypt(&bytes)?;

        let initial_sid = self.session_id.read().await.clone();

        match self.try_post(&encrypted).await {
            Ok(v) => Ok(v),
            Err(first_err) => {
                let err_str = first_err.to_string();
                if err_str.contains("unknown session") {
                    let mut sid = self.session_id.write().await;
                    if *sid == initial_sid {
                        warn!("Server lost session; generating new session ID");
                        *sid = format!("{:016x}", rand::random::<u64>());
                        let mut channels = self.response_channels.lock().await;
                        channels.clear();
                    } else {
                        debug!("Session already updated by another thread, skipping generation");
                    }
                }

                warn!("send failed: {}; forcing SSE reconnect and retrying", first_err);
                let ready = self.sse_ready.notified();
                tokio::pin!(ready);
                self.reconnect_signal.notify_one();
                let _ = tokio::time::timeout(RECONNECT_WAIT, ready).await;
                self.try_post(&encrypted).await
            }
        }
    }

    async fn try_post(&self, encrypted: &[u8]) -> Result<Option<Vec<u8>>> {
        let hot = self.hot.load();
        let sid = self.session_id.read().await;
        let url = format!("{}/send/{}", hot.server_base_url, *sid);
        drop(sid);
        let resp = hot
            .http_client
            .post(&url)
            .body(encrypted.to_vec())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Tunnel without a live server. handle_sse_message only touches
    /// `hot.crypto` (to decrypt) and `response_channels` (to dispatch), so the
    /// http_client / server_base_url are placeholders.
    fn test_tunnel(password: &str) -> Tunnel {
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
}
