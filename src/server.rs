use anyhow::Result;
use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};
use crate::crypto::Crypto;
use crate::protocol::Message;
use crate::reload::{CliOverrides, HotServerConfig};

type BoxBody = http_body_util::Either<
    Full<Bytes>,
    StreamBody<futures::stream::BoxStream<'static, Result<Frame<Bytes>, std::convert::Infallible>>>,
>;

struct Session {
    tcp_writers: HashMap<u32, mpsc::Sender<Vec<u8>>>,
    /// SSE channel: encrypted messages queued for streaming to client
    sse_tx: mpsc::Sender<Vec<u8>>,
}

struct ServerState {
    hot: Arc<ArcSwap<HotServerConfig>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

pub async fn run_server(
    listen_addr: &str,
    initial_hot: HotServerConfig,
    config_path: Option<String>,
    cli_overrides: Arc<CliOverrides>,
) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!("HTTP server listening on {}", listen_addr);

    let hot = Arc::new(ArcSwap::from_pointee(initial_hot));

    if let Some(path) = config_path {
        let hot_clone = hot.clone();
        let overrides = cli_overrides.clone();
        tokio::spawn(async move {
            crate::reload::config_watcher_server(path, hot_clone, overrides).await;
        });
    }

    let state = Arc::new(ServerState {
        hot,
        sessions: Mutex::new(HashMap::new()),
    });

    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("Connection from {}", addr);
        let state = state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle_request(req, state).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await
            {
                debug!("HTTP error from {}: {}", addr, e);
            }
        });
    }
}

async fn handle_request(
    req: Request<hyper::body::Incoming>,
    state: Arc<ServerState>,
) -> Result<Response<BoxBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    debug!("{} {}", method, path);

    let hot = state.hot.load();

    // /health always returns plain text, regardless of prefix (load-balancer probes)
    if method == hyper::Method::GET && path == "/health" {
        info!("Health check");
        return Ok(ok_response(&format!("{}\n", hot.health_body)));
    }

    // Strip configured prefix before routing
    let effective_path: &str = if hot.path_prefix.is_empty() {
        &path
    } else {
        match path.strip_prefix(hot.path_prefix.as_str()) {
            Some(rest) => rest,
            None => return Ok(ok_response("not found")),
        }
    };

    let response = match (method, effective_path) {
        (hyper::Method::GET, "" | "/") => root_response(&hot).await,

        (hyper::Method::GET, "/health") => {
            info!("Health check");
            ok_response(&format!("{}\n", hot.health_body))
        }

        (hyper::Method::GET, p) if p.starts_with("/stream/") => {
            let session_id = p.trim_start_matches("/stream/").to_string();
            handle_stream(&session_id, &state).await
        }

        (hyper::Method::POST, p) if p.starts_with("/send/") => {
            let session_id = p.trim_start_matches("/send/").to_string();
            let body = match req.collect().await {
                Ok(b) => b.to_bytes(),
                Err(e) => {
                    error!("Body read error: {}", e);
                    return Ok(ok_response("bad request"));
                }
            };
            handle_send(&session_id, &body, &state).await
        }

        _ => ok_response("not found"),
    };

    Ok(response)
}

fn ok_response(msg: &str) -> Response<BoxBody> {
    Response::builder()
        .status(StatusCode::OK)
        .body(http_body_util::Either::Left(Full::new(Bytes::from(
            msg.to_string(),
        ))))
        .unwrap()
}

fn service_unavailable_response(msg: &str) -> Response<BoxBody> {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(http_body_util::Either::Left(Full::new(Bytes::from(
            msg.to_string(),
        ))))
        .unwrap()
}

async fn root_response(hot: &HotServerConfig) -> Response<BoxBody> {
    if let Some(url) = &hot.root_redirect {
        return Response::builder()
            .status(StatusCode::MOVED_PERMANENTLY)
            .header("Location", url.as_str())
            .body(http_body_util::Either::Left(Full::new(Bytes::new())))
            .unwrap();
    }
    if let Some(path) = &hot.root_html {
        match tokio::fs::read_to_string(path).await {
            Ok(content) => {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "text/html; charset=utf-8")
                    .body(http_body_util::Either::Left(Full::new(Bytes::from(content))))
                    .unwrap();
            }
            Err(e) => error!("Failed to read root_html '{}': {}", path, e),
        }
    }
    ok_response(&format!("{}\n", hot.health_body))
}

/// SSE endpoint: streams encrypted messages to client
async fn handle_stream(session_id: &str, state: &ServerState) -> Response<BoxBody> {
    info!("SSE stream opened for session {}", session_id);

    let (sse_tx, sse_rx) = mpsc::channel::<Vec<u8>>(1024);

    let was_new;
    let _session = {
        let mut sessions = state.sessions.lock().await;
        was_new = !sessions.contains_key(session_id);
        let s = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Session {
                    tcp_writers: HashMap::new(),
                    sse_tx: sse_tx.clone(),
                }))
            })
            .clone();

        let mut s_lock = s.lock().await;
        s_lock.sse_tx = sse_tx.clone();
        drop(s_lock);
        s
    };

    // If we just created this session (e.g. after a server restart while the
    // client kept its old session id), the client may still be holding orphan
    // conn_ids that we know nothing about. Tell it to clear them.
    if was_new {
        let hot = state.hot.load();
        match make_encrypted_response(&hot.crypto, &Message::Reset) {
            Ok(payload) => {
                if sse_tx.send(payload).await.is_err() {
                    warn!("Failed to push Reset to fresh session {}", session_id);
                }
            }
            Err(e) => error!("Failed to encrypt Reset for {}: {}", session_id, e),
        }
    }

    // Don't hold an extra sender here — the session keeps its own clone, and
    // we want the rx side to hang up cleanly when the session is dropped.
    drop(sse_tx);

    // Keepalive every 15s as an SSE comment line. The client parser ignores
    // lines without `data: `, but the byte read resets its 30s read timeout,
    // so idle-but-healthy tunnels don't churn through reconnects.
    let keepalive = tokio::time::interval(Duration::from_secs(15));
    let stream = futures::stream::unfold(
        (sse_rx, keepalive),
        |(mut rx, mut keepalive)| async move {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(data) => {
                        use base64ct::{Base64, Encoding};
                        let encoded = Base64::encode_string(&data);
                        let event = format!("data: {}\n\n", encoded);
                        let frame = Frame::data(Bytes::from(event));
                        Some((Ok::<_, std::convert::Infallible>(frame), (rx, keepalive)))
                    }
                    None => None,
                },
                _ = keepalive.tick() => {
                    let frame = Frame::data(Bytes::from(":\n\n"));
                    Some((Ok::<_, std::convert::Infallible>(frame), (rx, keepalive)))
                }
            }
        },
    );

    let body: BoxBody = http_body_util::Either::Right(StreamBody::new(
        Box::pin(stream) as futures::stream::BoxStream<'static, _>,
    ));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}

/// Handle encrypted message from client
async fn handle_send(
    session_id: &str,
    body: &Bytes,
    state: &ServerState,
) -> Response<BoxBody> {
    let hot = state.hot.load();

    let session = {
        let sessions = state.sessions.lock().await;
        match sessions.get(session_id) {
            Some(s) => s.clone(),
            None => {
                warn!("Unknown session: {}", session_id);
                return service_unavailable_response("unknown session");
            }
        }
    };

    let plaintext = match hot.crypto.decrypt(body) {
        Ok(p) => p,
        Err(e) => {
            error!("Decrypt failed: {}", e);
            return ok_response("decrypt error");
        }
    };

    let message = match Message::from_bytes(&plaintext) {
        Ok(m) => m,
        Err(e) => {
            error!("Deserialize failed: {}", e);
            return ok_response("deserialize error");
        }
    };

    match message {
        Message::Connect { conn_id, host, port } => {
            info!("[{}] CONNECT {}:{}", conn_id, host, port);
            let target = format!("{}:{}", host, port);

            match TcpStream::connect(&target).await {
                Err(e) => {
                    error!("[{}] Failed to connect to {}: {}", conn_id, target, e);
                    let err_msg = Message::Error {
                        conn_id: Some(conn_id),
                        message: format!("Connect failed: {}", e),
                    };
                    match make_encrypted_response(&hot.crypto, &err_msg) {
                        Ok(data) => Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/octet-stream")
                            .body(http_body_util::Either::Left(Full::new(Bytes::from(data))))
                            .unwrap(),
                        Err(e) => {
                            error!("Error encrypt: {}", e);
                            ok_response("error")
                        }
                    }
                }
                Ok(tcp_stream) => {
                    info!("[{}] Connected to {}", conn_id, target);
                    let (tcp_read, tcp_write) = tcp_stream.into_split();

                    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(256);
                    {
                        let mut sess = session.lock().await;
                        sess.tcp_writers.insert(conn_id, write_tx);
                    };

                    let crypto = hot.crypto.clone();
                    tokio::spawn(async move {
                        relay_tcp_connection(
                            conn_id, &host, port, tcp_read, tcp_write, write_rx,
                            session, crypto,
                        )
                        .await;
                    });

                    match make_encrypted_response(
                        &hot.crypto,
                        &Message::Data { conn_id, data: vec![] },
                    ) {
                        Ok(data) => Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "application/octet-stream")
                            .body(http_body_util::Either::Left(Full::new(Bytes::from(data))))
                            .unwrap(),
                        Err(e) => {
                            error!("ACK encrypt error: {}", e);
                            ok_response("error")
                        }
                    }
                }
            }
        }
        Message::Data { conn_id, data } => {
            debug!("[{}] DATA {} bytes from client", conn_id, data.len());
            let sess = session.lock().await;
            if let Some(tx) = sess.tcp_writers.get(&conn_id) {
                let _ = tx.send(data).await;
            }
            ok_response("")
        }
        Message::Close { conn_id } => {
            info!("[{}] CLOSE", conn_id);
            let mut sess = session.lock().await;
            sess.tcp_writers.remove(&conn_id);
            ok_response("")
        }
        Message::Ping => {
            match make_encrypted_response(&hot.crypto, &Message::Pong) {
                Ok(data) => Response::builder()
                    .status(StatusCode::OK)
                    .body(http_body_util::Either::Left(Full::new(Bytes::from(data))))
                    .unwrap(),
                Err(_) => ok_response(""),
            }
        }
        _ => ok_response(""),
    }
}

fn make_encrypted_response(crypto: &Crypto, msg: &Message) -> Result<Vec<u8>> {
    let bytes = msg.to_bytes()?;
    Ok(crypto.encrypt(&bytes)?)
}

/// Relay data between an already-connected TCP stream and the tunnel SSE/POST channels.
async fn relay_tcp_connection(
    conn_id: u32,
    host: &str,
    port: u16,
    mut tcp_read: tokio::net::tcp::OwnedReadHalf,
    mut tcp_write: tokio::net::tcp::OwnedWriteHalf,
    mut write_rx: mpsc::Receiver<Vec<u8>>,
    session: Arc<Mutex<Session>>,
    crypto: Arc<Crypto>,
) {
    let crypto_clone = crypto.clone();
    let session_clone = session.clone();
    let read_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 32768];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) => {
                    debug!("[{}] TCP EOF", conn_id);
                    break;
                }
                Ok(n) => {
                    debug!("[{}] TCP -> SSE {} bytes", conn_id, n);
                    let msg = Message::Data {
                        conn_id,
                        data: buf[..n].to_vec(),
                    };
                    let bytes = match msg.to_bytes() {
                        Ok(b) => b,
                        Err(e) => { error!("[{}] Serialize: {}", conn_id, e); break; }
                    };
                    let encrypted = match crypto_clone.encrypt(&bytes) {
                        Ok(e) => e,
                        Err(e) => { error!("[{}] Encrypt: {}", conn_id, e); break; }
                    };

                    let sse_tx = {
                        let sess = session_clone.lock().await;
                        sess.sse_tx.clone()
                    };
                    if sse_tx.send(encrypted).await.is_err() {
                        debug!("[{}] SSE stream replaced or closed, retrying in next read", conn_id);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                Err(e) => {
                    debug!("[{}] TCP read error: {}", conn_id, e);
                    break;
                }
            }
        }

        let close = Message::Close { conn_id };
        if let Ok(bytes) = close.to_bytes() {
            if let Ok(encrypted) = crypto_clone.encrypt(&bytes) {
                let sse_tx = {
                    let sess = session_clone.lock().await;
                    sess.sse_tx.clone()
                };
                let _ = sse_tx.send(encrypted).await;
            }
        }
        let mut sess = session_clone.lock().await;
        sess.tcp_writers.remove(&conn_id);
    });

    let write_task = tokio::spawn(async move {
        while let Some(data) = write_rx.recv().await {
            if data.is_empty() {
                continue;
            }
            debug!("[{}] Client -> TCP {} bytes", conn_id, data.len());
            if let Err(e) = tcp_write.write_all(&data).await {
                error!("[{}] TCP write error: {}", conn_id, e);
                break;
            }
        }
    });

    tokio::select! {
        _ = read_task => {},
        _ = write_task => {},
    }

    info!("[{}] Connection closed for {}:{}", conn_id, host, port);
}
