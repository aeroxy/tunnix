use anyhow::Result;
use base64ct::{Base64, Encoding};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use tunnix_common::crypto::Crypto;
use tunnix_common::protocol::Message;

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
    session_id: String,
    crypto: Arc<Crypto>,
    http_client: reqwest::Client,
    pub response_channels: Mutex<HashMap<u32, mpsc::Sender<TunnelEvent>>>,
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
            session_id: session_id.clone(),
            crypto,
            http_client,
            response_channels: Mutex::new(HashMap::new()),
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
                info!("Opening SSE stream for session {}", tunnel_clone.session_id);
                if let Err(e) = tunnel_clone.sse_read_loop().await {
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
        let url = format!("{}/stream/{}", self.server_base_url, self.session_id);
        let resp = self.http_client.get(&url).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("SSE stream failed: {}", resp.status());
        }

        info!("SSE stream connected");

        use futures::StreamExt;
        let mut stream = resp.bytes_stream();

        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
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

    /// Send encrypted message to server via HTTP POST
    pub async fn send_message(&self, msg: &Message) -> Result<Option<Vec<u8>>> {
        let bytes = msg.to_bytes()?;
        let encrypted = self.crypto.encrypt(&bytes)?;

        let url = format!("{}/send/{}", self.server_base_url, self.session_id);
        let resp = self
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
