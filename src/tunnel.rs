use anyhow::Result;
use base64ct::{Base64, Encoding};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};
use crate::crypto::Crypto;
use crate::protocol::Message;

const RECONNECT_WAIT: Duration = Duration::from_secs(10);

/// Events received from server via SSE
#[derive(Debug)]
pub enum TunnelEvent {
    Data(Vec<u8>),
    Close,
    Error(String),
}

/// Tunnel handles communication with the server via SSE + HTTP POST
pub struct Tunnel {
    server_base_url: String,
    session_id: RwLock<String>,
    crypto: Arc<Crypto>,
    http_client: reqwest::Client,
    pub response_channels: Mutex<HashMap<u32, mpsc::Sender<TunnelEvent>>>,
    /// Fired by send_message when a POST fails; tells the SSE loop to drop
    /// its current stream and reconnect immediately instead of waiting for
    /// the underlying TCP read to error out (which may never happen if an
    /// upstream LB silently half-closes).
    reconnect_signal: Notify,
    /// Fired by the SSE loop each time the stream is freshly connected.
    /// send_message awaits this (with timeout) after triggering a reconnect.
    sse_ready: Notify,
}

impl Tunnel {
    /// Connect to the server: open SSE stream and start reading
    pub async fn connect(
        server_url: &str,
        crypto: Arc<Crypto>,
        headers: &HashMap<String, String>,
        health_expected: &str,
    ) -> Result<Arc<Self>> {
        // Generate session ID
        let session_id = format!("{:016x}", rand::random::<u64>());

        // Build HTTP client with custom headers
        let mut default_headers = reqwest::header::HeaderMap::new();
        for (key, value) in headers {
            default_headers.insert(
                reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                reqwest::header::HeaderValue::from_str(value)?,
            );
        }

        let http_client = reqwest::Client::builder()
            .default_headers(default_headers)
            .danger_accept_invalid_certs(true)
            .build()?;

        let tunnel = Arc::new(Tunnel {
            server_base_url: server_url.trim_end_matches('/').to_string(),
            session_id: RwLock::new(session_id.clone()),
            crypto,
            http_client,
            response_channels: Mutex::new(HashMap::new()),
            reconnect_signal: Notify::new(),
            sse_ready: Notify::new(),
        });

        // Test connection with health check
        let health_url = format!("{}/health", tunnel.server_base_url);
        info!("Testing connection to {}", health_url);
        let resp = tunnel.http_client.get(&health_url).send().await?;
        let body = resp.text().await?;
        info!("Server health: {}", body.trim());
        if body.trim() != health_expected.trim() {
            anyhow::bail!(
                "Health check mismatch: expected {:?}, got {:?}",
                health_expected.trim(),
                body.trim()
            );
        }

        // Open SSE stream
        let tunnel_clone = tunnel.clone();
        tokio::spawn(async move {
            loop {
                let sid = tunnel_clone.session_id.read().await.clone();
                info!("Opening SSE stream for session {}", sid);
                if let Err(e) = tunnel_clone.sse_read_loop().await {
                    if e.to_string().contains("forced reconnect") {
                        continue;
                    }
                    error!("SSE stream error: {}, reconnecting in 3s...", e);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        });

        // Wait a moment for SSE to establish
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok(tunnel)
    }

    /// Read SSE events and dispatch to connection handlers
    async fn sse_read_loop(&self) -> Result<()> {
        let sid = self.session_id.read().await;
        let url = format!("{}/stream/{}", self.server_base_url, *sid);
        drop(sid);
        let resp = self.http_client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("SSE stream failed: {}", resp.status());
        }

        info!("SSE stream connected");
        // Wake any send_message calls that are blocked waiting for a fresh stream.
        self.sse_ready.notify_waiters();

        use futures::StreamExt;
        let mut stream = resp.bytes_stream();

        let mut buffer = String::new();

        loop {
            tokio::select! {
                chunk = stream.next() => {
                    let chunk = match chunk {
                        Some(c) => c?,
                        None => break,
                    };
                    let text = String::from_utf8_lossy(&chunk);
                    buffer.push_str(&text);

                    // Process complete SSE events (end with \n\n)
                    while let Some(pos) = buffer.find("\n\n") {
                        let event = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        // Parse "data: <base64>" lines
                        for line in event.lines() {
                            if let Some(data_str) = line.strip_prefix("data: ") {
                                match Base64::decode_vec(data_str.trim()) {
                                    Ok(encrypted) => {
                                        self.handle_sse_message(&encrypted).await;
                                    }
                                    Err(e) => {
                                        warn!("Base64 decode error: {}", e);
                                    }
                                }
                            }
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
        let plaintext = match self.crypto.decrypt(encrypted) {
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
                let channels = self.response_channels.lock().await;
                if let Some(tx) = channels.get(&conn_id) {
                    let _ = tx.send(TunnelEvent::Data(data)).await;
                }
            }
            Message::Close { conn_id } => {
                debug!("[{}] SSE close", conn_id);
                let channels = self.response_channels.lock().await;
                if let Some(tx) = channels.get(&conn_id) {
                    let _ = tx.send(TunnelEvent::Close).await;
                }
            }
            Message::Error { conn_id, message } => {
                warn!("[{:?}] SSE error: {}", conn_id, message);
                if let Some(cid) = conn_id {
                    let channels = self.response_channels.lock().await;
                    if let Some(tx) = channels.get(&cid) {
                        let _ = tx.send(TunnelEvent::Error(message)).await;
                    }
                }
            }
            Message::Pong => debug!("PONG"),
            _ => {}
        }
    }

    /// Send encrypted message to server via HTTP POST.
    ///
    /// On first failure (HTTP error, transport error) we force the SSE loop
    /// to reconnect and retry once. This handles the common case of the
    /// server being restarted while the client's SSE is still half-open:
    /// without this, the client would log "Failed to send CONNECT or no ACK"
    /// indefinitely until manually restarted.
    pub async fn send_message(&self, msg: &Message) -> Result<Option<Vec<u8>>> {
        let bytes = msg.to_bytes()?;
        let encrypted = self.crypto.encrypt(&bytes)?;

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
                // Subscribe to sse_ready BEFORE firing the signal so we don't
                // miss a fast reconnection.
                let ready = self.sse_ready.notified();
                tokio::pin!(ready);
                self.reconnect_signal.notify_one();
                let _ = tokio::time::timeout(RECONNECT_WAIT, ready).await;
                self.try_post(&encrypted).await
            }
        }
    }

    async fn try_post(&self, encrypted: &[u8]) -> Result<Option<Vec<u8>>> {
        let sid = self.session_id.read().await;
        let url = format!("{}/send/{}", self.server_base_url, *sid);
        drop(sid);
        let resp = self
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
                let plaintext = self.crypto.decrypt(&data)?;
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
