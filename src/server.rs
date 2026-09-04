use anyhow::Result;
use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, warn};
use crate::archive::{spawn_compress, spawn_decompress};
use crate::crypto::Crypto;
use crate::protocol::Message;
use crate::reload::{CliOverrides, HotServerConfig};

type BoxBody = http_body_util::Either<
    Full<Bytes>,
    StreamBody<futures::stream::BoxStream<'static, Result<Frame<Bytes>, std::convert::Infallible>>>,
>;

struct Session {
    /// Per-conn_id sink for client→target bytes. Covers both TCP connections and
    /// PTYs (remote exec) — a PTY is just another duplex byte stream.
    tcp_writers: HashMap<u32, mpsc::Sender<Vec<u8>>>,
    /// Per-conn_id abort signal for TCP relays. `Message::Abort` fires it so
    /// the relay stops pulling the target: `Close` only half-closes the upload
    /// direction, and an abandoned download would otherwise be pulled to
    /// completion into a conn_id the client has already forgotten. Only TCP
    /// connections register one; PTYs and transfers have their own teardown.
    aborts: HashMap<u32, Arc<Notify>>,
    /// Per-conn_id resize request channel for remote-exec PTYs. Senders are
    /// kept in the session so Message::Resize can deliver a new PtySize
    /// without taking the master PTY out of `relay_pty_connection`.
    #[cfg(unix)]
    pty_resize: HashMap<u32, mpsc::Sender<PtySize>>,
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
                    aborts: HashMap::new(),
                    #[cfg(unix)]
                    pty_resize: HashMap::new(),
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
                    let abort = Arc::new(Notify::new());
                    {
                        let mut sess = session.lock().await;
                        sess.tcp_writers.insert(conn_id, write_tx);
                        sess.aborts.insert(conn_id, abort.clone());
                    };

                    let crypto = hot.crypto.clone();
                    tokio::spawn(async move {
                        relay_tcp_connection(
                            conn_id, &host, port, tcp_read, tcp_write, write_rx, abort,
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
            // Clone the writer out and drop the session lock before awaiting the
            // send. tx is a bounded channel, so holding the lock across
            // tx.send().await would freeze every other session op — and deadlock
            // the cleanup paths that need the lock to remove the writer —
            // whenever the writer's consumer stalls.
            let tx = {
                let sess = session.lock().await;
                sess.tcp_writers.get(&conn_id).cloned()
            };
            if let Some(tx) = tx {
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
        Message::Abort { conn_id } => {
            info!("[{}] ABORT", conn_id);
            // The connection is dead on the client's side. Unlike Close this
            // releases both directions: the writer so the upload side ends,
            // and the abort signal so the read task stops pulling the target
            // instead of streaming the rest of its response to nobody.
            let mut sess = session.lock().await;
            sess.tcp_writers.remove(&conn_id);
            if let Some(abort) = sess.aborts.remove(&conn_id) {
                abort.notify_one();
            }
            ok_response("")
        }
        #[cfg(unix)]
        Message::Exec { conn_id, cmd, cols, rows, term } => {
            if !hot.allow_exec {
                warn!("[{}] EXEC denied: remote exec disabled", conn_id);
                return encrypted_response(
                    &hot.crypto,
                    &Message::Error {
                        conn_id: Some(conn_id),
                        message: "remote exec is disabled on this server".to_string(),
                    },
                );
            }
            info!("[{}] EXEC {} ({}x{})", conn_id, if cmd.is_some() { "cmd" } else { "<shell>" }, cols, rows);
            // The command may contain secrets — keep it out of normal logs.
            debug!("[{}] EXEC command: {}", conn_id, cmd.as_deref().unwrap_or("<shell>"));

            let pty_system = native_pty_system();
            let pair = match pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
                Ok(p) => p,
                Err(e) => {
                    error!("[{}] openpty failed: {}", conn_id, e);
                    return encrypted_response(
                        &hot.crypto,
                        &Message::Error { conn_id: Some(conn_id), message: format!("openpty failed: {}", e) },
                    );
                }
            };

            let mut builder = match &cmd {
                Some(c) => {
                    let mut b = CommandBuilder::new("/bin/sh");
                    b.arg("-c");
                    b.arg(c);
                    b
                }
                None => {
                    // Fall back to /bin/sh when SHELL is unset OR set-but-empty;
                    // an empty program name would make the spawn fail with ENOENT.
                    let shell = std::env::var("SHELL").unwrap_or_default();
                    CommandBuilder::new(if shell.is_empty() { "/bin/sh".to_string() } else { shell })
                }
            };
            builder.env("TERM", if term.is_empty() { "xterm-256color".to_string() } else { term });

            let child = match pair.slave.spawn_command(builder) {
                Ok(c) => c,
                Err(e) => {
                    error!("[{}] spawn failed: {}", conn_id, e);
                    return encrypted_response(
                        &hot.crypto,
                        &Message::Error { conn_id: Some(conn_id), message: format!("spawn failed: {}", e) },
                    );
                }
            };
            // Drop the slave handle so the master reader sees EOF once the child exits.
            drop(pair.slave);

            let master = pair.master;
            let writer = match master.take_writer() {
                Ok(w) => w,
                Err(e) => {
                    error!("[{}] take writer failed: {}", conn_id, e);
                    return encrypted_response(
                        &hot.crypto,
                        &Message::Error { conn_id: Some(conn_id), message: format!("pty writer failed: {}", e) },
                    );
                }
            };

            let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(256);
            let (resize_tx, resize_rx) = mpsc::channel::<PtySize>(4);
            {
                let mut sess = session.lock().await;
                sess.tcp_writers.insert(conn_id, write_tx);
                sess.pty_resize.insert(conn_id, resize_tx);
            }

            let crypto = hot.crypto.clone();
            tokio::spawn(async move {
                relay_pty_connection(conn_id, master, writer, child, write_rx, resize_rx, session, crypto).await;
            });

            encrypted_response(&hot.crypto, &Message::Data { conn_id, data: vec![] })
        }
        #[cfg(not(unix))]
        Message::Exec { conn_id, .. } => {
            warn!("[{}] EXEC denied: remote exec is not supported on this platform", conn_id);
            encrypted_response(
                &hot.crypto,
                &Message::Error {
                    conn_id: Some(conn_id),
                    message: "remote exec is not supported on this platform".to_string(),
                },
            )
        }
        #[cfg(unix)]
        Message::Resize { conn_id, cols, rows } => {
            debug!("[{}] RESIZE {}x{}", conn_id, cols, rows);
            let sess = session.lock().await;
            if let Some(tx) = sess.pty_resize.get(&conn_id) {
                let _ = tx.try_send(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            }
            ok_response("")
        }
        #[cfg(not(unix))]
        Message::Resize { conn_id, .. } => {
            debug!("[{}] RESIZE ignored: remote exec is not supported on this platform", conn_id);
            ok_response("")
        }
        Message::Pull { conn_id, paths, level } => {
            if !hot.allow_transfer {
                warn!("[{}] PULL denied: file transfer disabled", conn_id);
                return encrypted_response(
                    &hot.crypto,
                    &Message::Error {
                        conn_id: Some(conn_id),
                        message: "file transfer is disabled on this server".to_string(),
                    },
                );
            }
            info!("[{}] PULL {}", conn_id, paths.join(", "));
            let srcs: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
            let crypto = hot.crypto.clone();
            tokio::spawn(async move {
                relay_pull(conn_id, srcs, level, session, crypto).await;
            });
            // ACK so the client starts unpacking; the archive follows over SSE.
            encrypted_response(&hot.crypto, &Message::Data { conn_id, data: vec![] })
        }
        Message::Push { conn_id, path } => {
            if !hot.allow_transfer {
                warn!("[{}] PUSH denied: file transfer disabled", conn_id);
                return encrypted_response(
                    &hot.crypto,
                    &Message::Error {
                        conn_id: Some(conn_id),
                        message: "file transfer is disabled on this server".to_string(),
                    },
                );
            }
            info!("[{}] PUSH -> {}", conn_id, path);
            // Register the decompressor's input as this conn's writer: the
            // existing Data handler forwards incoming chunks to it, and the
            // Close handler drops it (EOF) when the client finishes streaming.
            let (chunk_tx, unpack_handle) = spawn_decompress(PathBuf::from(path));
            {
                let mut sess = session.lock().await;
                sess.tcp_writers.insert(conn_id, chunk_tx);
            }
            let crypto = hot.crypto.clone();
            tokio::spawn(async move {
                relay_push(conn_id, unpack_handle, session, crypto).await;
            });
            encrypted_response(&hot.crypto, &Message::Data { conn_id, data: vec![] })
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

/// Build an HTTP 200 response whose body is the encrypted, serialized `msg`
/// (the same octet-stream ACK shape the Connect path uses).
fn encrypted_response(crypto: &Crypto, msg: &Message) -> Response<BoxBody> {
    match make_encrypted_response(crypto, msg) {
        Ok(data) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .body(http_body_util::Either::Left(Full::new(Bytes::from(data))))
            .unwrap(),
        Err(e) => {
            error!("Encrypt response error: {}", e);
            ok_response("error")
        }
    }
}

/// Relay data between an already-connected TCP stream and the tunnel SSE/POST channels.
async fn relay_tcp_connection(
    conn_id: u32,
    host: &str,
    port: u16,
    mut tcp_read: tokio::net::tcp::OwnedReadHalf,
    mut tcp_write: tokio::net::tcp::OwnedWriteHalf,
    mut write_rx: mpsc::Receiver<Vec<u8>>,
    abort: Arc<Notify>,
    session: Arc<Mutex<Session>>,
    crypto: Arc<Crypto>,
) {
    let crypto_clone = crypto.clone();
    let session_clone = session.clone();
    // Resolves once the read task has ended, however it ended: the sender is
    // moved into that task, so a panic drops it too. The write side uses it to
    // hold off starting its own watchdog - see there.
    let (read_done_tx, read_done_rx) = tokio::sync::oneshot::channel::<()>();
    let read_task = tokio::spawn(async move {
        let _read_done = read_done_tx;
        let mut buf = vec![0u8; 32768];
        // Set once a delivery has exhausted its retries: the client is then
        // known unreachable, so teardown shouldn't spend another full budget.
        let mut client_gone = false;
        // Set when the target itself failed, as opposed to closing cleanly.
        let mut read_failure: Option<String> = None;
        // Set when the client sent Abort: the connection is dead on its side
        // and it has already unregistered the conn_id, so there is nobody to
        // report to and nothing left to relay.
        let mut aborted = false;
        // A silent target gives this task nothing to react to, so a vanished
        // client would otherwise leave it blocked on read() forever, holding
        // the target socket. The delivery retries below only notice a dead
        // client when a chunk actually arrives. Pinned once: the counter resets
        // itself whenever the stream is live.
        let sse_dead = await_sse_dead(session_clone.clone());
        tokio::pin!(sse_dead);
        loop {
            let read = tokio::select! {
                // Cancel-safe: no bytes are consumed if the other branch wins.
                result = tcp_read.read(&mut buf) => result,
                _ = abort.notified() => {
                    debug!("[{}] client aborted; releasing the target", conn_id);
                    aborted = true;
                    break;
                }
                _ = &mut sse_dead => {
                    debug!("[{}] SSE closed continuously; abandoning target read", conn_id);
                    client_gone = true;
                    break;
                }
            };
            match read {
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
                    // Never drop a chunk: a gap would corrupt the proxied TCP
                    // byte stream. send_to_client retries across a client
                    // reconnect and bounds each attempt, so a half-open client
                    // (channel full but undrained) can't park us forever. If it
                    // stays unreachable, close the relay cleanly rather than
                    // skip bytes.
                    if !send_to_client(conn_id, &msg, &crypto_clone, &session_clone).await {
                        debug!("[{}] SSE unreachable; closing TCP relay", conn_id);
                        client_gone = true;
                        break;
                    }
                }
                Err(e) => {
                    debug!("[{}] TCP read error: {}", conn_id, e);
                    read_failure = Some(e.to_string());
                    break;
                }
            }
        }

        if aborted {
            // handle_send already removed the writer, so the write task is
            // ending on its own. Nothing to send: the client is not listening
            // for this conn_id any more. Idempotent in case of a race with the
            // final cleanup below.
            session_clone.lock().await.tcp_writers.remove(&conn_id);
        } else if client_gone {
            // No further uploads can arrive, so drop the writer to keep
            // teardown bounded: the write task ends once its sender is gone.
            session_clone.lock().await.tcp_writers.remove(&conn_id);
            // The client is known unreachable, so spend one attempt, not the
            // full budget (as the PTY teardown does). It still matters: a
            // client that has just reconnected would otherwise never hear
            // that this connection died, and its relay would sit until the
            // proxied app gave up on its own.
            let msg = Message::Error {
                conn_id: Some(conn_id),
                message: "client unreachable; target relay abandoned".to_string(),
            };
            let _ = send_to_client_with_attempts(conn_id, &msg, &crypto_clone, &session_clone, 1).await;
        } else {
            // A target that failed mid-response must not be reported the same
            // way as one that finished: Close means "everything arrived", and
            // the client turns it into a clean EOF for the proxied app. Error
            // says the stream is truncated, so the app is failed instead of
            // being handed a short read that looks complete.
            let terminal = match read_failure {
                Some(message) => {
                    // A failed target socket cannot accept an upload either, so
                    // release its writer instead of holding it for a Close that
                    // is no longer coming: the client aborts this connection
                    // rather than finishing its side.
                    session_clone.lock().await.tcp_writers.remove(&conn_id);
                    Message::Error {
                        conn_id: Some(conn_id),
                        message: format!("target read failed: {}", message),
                    }
                }
                None => Message::Close { conn_id },
            };
            // The client's relay ends its response direction on this message,
            // so it has to arrive: retry across a reconnect instead of making a
            // single attempt that a momentarily-full queue would defeat.
            if send_to_client(conn_id, &terminal, &crypto_clone, &session_clone).await {
                // Deliberately keeping the writer registered: the target
                // closing its output says nothing about the client's upload,
                // which may still be in flight. handle_send drops the writer
                // when the client's own Close arrives, and that is what shuts
                // the target's write side down.
            } else {
                // The client exhausted the delivery budget, so its Close is
                // not coming either. Release the writer now instead of leaving
                // the write task to wait on the SSE watchdog, which never
                // fires for a client whose stream is open but wedged.
                debug!("[{}] SSE unreachable after target EOF; releasing target writer", conn_id);
                session_clone.lock().await.tcp_writers.remove(&conn_id);
            }
        }
    });

    let session_watch = session.clone();
    let session_dead = session.clone();
    let write_task = tokio::spawn(async move {
        // The writer now outlives target-output EOF, so nothing else would wake
        // this task if the client vanished without sending Close. Bound the
        // wait on the client the same way the PTY relay does.
        //
        // Deliberately not started until the read task is gone. While both
        // halves are live, that task's own watchdog (and its failing
        // deliveries) already cover a vanished client: every one of those paths
        // removes this writer, which ends this task through `recv()`. Only once
        // the read task has finished - having kept the writer registered past
        // target-output EOF - is there nothing left to wake us. So one watchdog
        // timer runs per connection instead of two, and until then this future
        // is parked on a channel rather than ticking. Awaiting the sender's
        // *drop* rather than a value covers a read task that panicked.
        let sse_dead = async move {
            let _ = read_done_rx.await;
            // Pinned by the caller below: the counter resets itself whenever
            // the stream is live.
            await_sse_dead(session_dead).await;
        };
        tokio::pin!(sse_dead);
        loop {
            let data = tokio::select! {
                received = write_rx.recv() => match received {
                    Some(data) => data,
                    None => break,
                },
                _ = &mut sse_dead => {
                    debug!("[{}] SSE closed continuously; ending TCP write side", conn_id);
                    break;
                }
            };
            if data.is_empty() {
                continue;
            }
            debug!("[{}] Client -> TCP {} bytes", conn_id, data.len());
            if let Err(e) = tcp_write.write_all(&data).await {
                error!("[{}] TCP write error: {}", conn_id, e);
                // The upload direction is broken. Release the writer now rather
                // than at teardown: until it goes, handle_send keeps accepting
                // client bytes into a channel nobody drains and answering 200,
                // so the client uploads into nothing. Then tell it, so a failed
                // upload is visible instead of silently discarded.
                session_watch.lock().await.tcp_writers.remove(&conn_id);
                let msg = Message::Error {
                    conn_id: Some(conn_id),
                    message: format!("target write failed: {}", e),
                };
                let _ = send_to_client(conn_id, &msg, &crypto, &session_watch).await;
                break;
            }
        }
    });

    // Both directions run to completion independently: the target closing its
    // output must not cut off an upload still in progress, and a finished
    // upload must not cut off target output still arriving. The write task ends
    // when the client's Close drops its sender, or on a target write error.
    let (_, _) = tokio::join!(read_task, write_task);

    // Idempotent: the normal paths already removed the writer (client Close, or
    // the client-gone branch above). This catches a target write error, which
    // ends the write task with the entry still registered. The abort signal
    // goes too; nothing can fire it usefully once the relay is gone.
    {
        let mut sess = session.lock().await;
        sess.tcp_writers.remove(&conn_id);
        sess.aborts.remove(&conn_id);
    }

    info!("[{}] Connection closed for {}:{}", conn_id, host, port);
}

/// Relay between a PTY (remote exec) and the tunnel SSE/POST channels.
/// PTY readers/writers are blocking std::io, so the blocking halves run on
/// `spawn_blocking` threads and bridge to async via channels — but the SSE send
/// path mirrors `relay_tcp_connection` so it stays reconnection-aware.
#[cfg(unix)]
async fn relay_pty_connection(
    conn_id: u32,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    write_rx: mpsc::Receiver<Vec<u8>>,
    resize_rx: mpsc::Receiver<PtySize>,
    session: Arc<Mutex<Session>>,
    crypto: Arc<Crypto>,
) {
    // PTY <-> SSE. The master fd is driven with non-blocking I/O via tokio's
    // AsyncFd so both directions live on the async runtime, with no
    // spawn_blocking threads. This matters on teardown: a backgrounded process
    // can keep the slave open so the master never reaches EOF, and a blocking
    // reader thread parked in read() cannot be cancelled (abort() only marks
    // the JoinHandle; the OS thread lives on). An AsyncFd read task aborts
    // cleanly instead of leaking a thread for that background process's life.
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tokio::io::unix::AsyncFd;
    use tokio::io::Interest;

    // try_clone_reader()/take_writer() both dup() the master fd, so they all
    // share one open file description — and therefore the O_NONBLOCK flag. We
    // set it once on the master and hand each task its own dup'd AsyncFd.
    let setup = (|| -> std::io::Result<(AsyncFd<OwnedFd>, AsyncFd<OwnedFd>)> {
        let master_fd = master
            .as_raw_fd()
            .ok_or_else(|| std::io::Error::other("PTY master has no fd"))?;
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        let dup_afd = |interest| -> std::io::Result<AsyncFd<OwnedFd>> {
            let fd = unsafe { libc::dup(master_fd) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            AsyncFd::with_interest(unsafe { OwnedFd::from_raw_fd(fd) }, interest)
        };
        Ok((dup_afd(Interest::READABLE)?, dup_afd(Interest::WRITABLE)?))
    })();
    let (read_afd, write_afd) = match setup {
        Ok(pair) => pair,
        Err(e) => {
            error!("[{}] PTY async setup failed: {}", conn_id, e);
            {
                let mut sess = session.lock().await;
                sess.tcp_writers.remove(&conn_id);
                sess.pty_resize.remove(&conn_id);
            }
            // Terminal Error: without it the client waits on a PTY that will
            // never produce output, so retry across a reconnect rather than
            // losing it to a momentarily-full queue.
            let msg = Message::Error {
                conn_id: Some(conn_id),
                message: format!("PTY setup failed: {}", e),
            };
            let _ = send_to_client(conn_id, &msg, &crypto, &session).await;
            return;
        }
    };

    // PTY -> SSE: read on the runtime, then encrypt and forward each chunk.
    let mut read_task = {
        let crypto_fwd = crypto.clone();
        let session_fwd = session.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 32768];
            loop {
                let mut guard = match read_afd.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| {
                    let n = unsafe {
                        libc::read(
                            inner.get_ref().as_raw_fd(),
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(Ok(0)) => break, // EOF: slave fully closed
                    Ok(Ok(n)) => {
                        if !forward_pty_chunk(conn_id, buf[..n].to_vec(), &crypto_fwd, &session_fwd).await {
                            break;
                        }
                    }
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(Err(_)) => break,           // read error
                    Err(_would_block) => continue, // readiness was spurious
                }
            }
        })
    };

    // client -> PTY: drain the per-conn write channel onto the master. `writer`
    // is moved in only so its Drop sends EOF to the shell at teardown (parity
    // with the previous blocking writer); the bytes themselves go via write_afd.
    let mut write_task = {
        tokio::spawn(async move {
            let _writer = writer;
            let mut write_rx = write_rx;
            while let Some(data) = write_rx.recv().await {
                if data.is_empty() {
                    continue;
                }
                let mut pos = 0;
                while pos < data.len() {
                    let mut guard = match write_afd.writable().await {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    match guard.try_io(|inner| {
                        let n = unsafe {
                            libc::write(
                                inner.get_ref().as_raw_fd(),
                                data[pos..].as_ptr() as *const libc::c_void,
                                data.len() - pos,
                            )
                        };
                        if n < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(n as usize)
                        }
                    }) {
                        Ok(Ok(n)) => pos += n,
                        Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Ok(Err(_)) => return,
                        Err(_would_block) => continue,
                    }
                }
            }
        })
    };

    // Wait for the child; if the client side closes first, kill it.
    let mut killer = child.clone_killer();
    let mut wait_handle =
        tokio::task::spawn_blocking(move || {
            let mut child = child;
            child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1)
        });

    // Watchdog: an abrupt client disconnect (network drop, process killed)
    // leaves write_tx in tcp_writers, so write_blocking never finishes and the
    // child runs forever. Probe the session's sse_tx; only kill the child if
    // SSE has been continuously closed for at least 6s, so transient drops
    // (e.g., a client reconnect within forward_pty_chunk's 5s retry window)
    // don't kill an otherwise-healthy session.
    let sse_dead = await_sse_dead(session.clone());
    // Pin to the stack so we can re-poll across loop iterations; the async
    // block holds a !Unpin MutexGuard across .await.
    tokio::pin!(sse_dead);

    let mut resize_rx = resize_rx;
    let mut client_gone = false;
    let code = loop {
        tokio::select! {
            res = &mut wait_handle => break res.unwrap_or(-1),
            _ = &mut write_task => {
                // Client went away (Close removed the writer sender). Kill the child.
                let _ = killer.kill();
                client_gone = true;
                break (&mut wait_handle).await.unwrap_or(-1);
            }
            _ = &mut sse_dead => {
                debug!("[{}] SSE stream closed continuously; killing orphaned PTY child", conn_id);
                let _ = killer.kill();
                client_gone = true;
                break (&mut wait_handle).await.unwrap_or(-1);
            }
            new_size = resize_rx.recv() => {
                if let Some(s) = new_size {
                    if let Err(e) = master.resize(s) {
                        error!("[{}] PTY resize failed: {}", conn_id, e);
                    }
                }
            }
        }
    };
    drop(master);

    // Child has exited. Give the read task up to 2s to drain buffered PTY
    // output and observe EOF, then abort. With AsyncFd the read task is a plain
    // async task: if a backgrounded process keeps the slave open so EOF never
    // arrives, abort() cancels it immediately — no blocking thread is left
    // parked in read() as the old spawn_blocking reader would have been.
    if tokio::time::timeout(Duration::from_secs(2), &mut read_task).await.is_err() {
        debug!("[{}] PTY read loop still pending (background process holding the pty?); aborting", conn_id);
        read_task.abort();
        let _ = read_task.await;
    }

    // The write task ends on its own once the client closes the write channel;
    // abort it in case we're tearing down while it's parked waiting for the
    // master to become writable.
    write_task.abort();
    let _ = write_task.await;

    // Report exit code, then close the logical connection.
    for msg in [
        Message::ExitStatus { conn_id, code },
        Message::Close { conn_id },
    ] {
        // When the client is already known gone (writer closed, or SSE dead
        // ≥6s past the reconnect window) one best-effort attempt is enough:
        // spending the full budget only delays teardown to rediscover that.
        let attempts = if client_gone { 1 } else { SEND_ATTEMPTS };
        let sent = send_to_client_with_attempts(conn_id, &msg, &crypto, &session, attempts).await;
        if !sent {
            if !client_gone {
                error!("[{}] failed to deliver shutdown message", conn_id);
            }
            // Stop at the first failure, as relay_pull and relay_push do: a
            // false return means the client is unreachable, so spending a
            // second full budget on the Close only delays teardown to
            // rediscover that.
            break;
        }
    }

    {
        let mut sess = session.lock().await;
        sess.tcp_writers.remove(&conn_id);
        #[cfg(unix)]
        sess.pty_resize.remove(&conn_id);
    }

    info!("[{}] PTY session closed (exit {})", conn_id, code);
}

/// Encrypt one PTY chunk and push it to the (possibly-reconnected) SSE sender.
/// Retries briefly across a client reconnect so a transient SSE drop doesn't
/// lose output. Returns false if the chunk could not be delivered.
#[cfg(unix)]
async fn forward_pty_chunk(
    conn_id: u32,
    data: Vec<u8>,
    crypto: &Crypto,
    session: &Arc<Mutex<Session>>,
) -> bool {
    let msg = Message::Data { conn_id, data };
    if send_to_client(conn_id, &msg, crypto, session).await {
        return true;
    }
    error!("[{}] SSE reconnect timed out; dropping PTY output", conn_id);
    false
}

/// How long a session's SSE stream must stay continuously closed before its
/// relays treat the client as gone. Long enough that a client reconnecting
/// inside the send retry window doesn't count as a disconnect.
const SSE_DEAD_GRACE: Duration = Duration::from_secs(6);

/// Resolves once this session's SSE stream has been closed continuously for
/// `SSE_DEAD_GRACE`.
///
/// An abrupt client disconnect (network drop, killed process) leaves a writer
/// registered in `tcp_writers` with nothing left to drain it, so a relay's
/// write task would park on `recv()` forever, holding its target socket open.
/// Relays that can be left waiting on the client select on this to bound their
/// teardown. A reconnect replaces `sse_tx` with a live sender and resets the
/// count, so transient drops don't tear down a healthy session.
///
/// `try_lock`, never `lock().await`: callers hold this future inside a
/// `select!` that stops polling it the instant the other branch wins, and
/// tokio's mutex is fair - it hands the freed lock to the waiter at the head of
/// its queue whether or not anyone is still polling it. A watchdog that queued
/// there would strand the lock, and every later `send_to_client` on that
/// session would park on it forever. A tick that loses the race is simply
/// skipped: the counter is left alone rather than reset, so a contended lock
/// delays this watchdog but cannot defeat it.
async fn await_sse_dead(session: Arc<Mutex<Session>>) {
    let mut closed_secs: u32 = 0;
    let grace_secs = SSE_DEAD_GRACE.as_secs() as u32;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(sess) = session.try_lock() else { continue };
        let closed = sess.sse_tx.is_closed();
        drop(sess);
        if closed {
            closed_secs += 1;
            if closed_secs >= grace_secs {
                return;
            }
        } else {
            closed_secs = 0;
        }
    }
}

/// Encrypt `msg` and push it to the (possibly-reconnected) SSE sender, retrying
/// briefly across a client reconnect. Returns false if it could not be
/// delivered. Cross-platform sibling of `forward_pty_chunk`; used by TCP
/// relays, PTY teardown and transfers. Terminal messages (`Close`, `Error`,
/// `ExitStatus`) must go through here: a single send loses them whenever the
/// client's queue is momentarily full or its SSE stream is being replaced.
///
/// The budget here (50 x (500ms + 100ms) ~= 30s) is what declares a client
/// unreachable, so it must stay well above the client's `DISPATCH_STALL_TIMEOUT`
/// in tunnel.rs. That timeout is how long one stalled consumer can keep the
/// client from reading the SSE socket; if the two were the same magnitude, that
/// stall alone would exhaust this budget and tear down healthy connections.
async fn send_to_client(conn_id: u32, msg: &Message, crypto: &Crypto, session: &Arc<Mutex<Session>>) -> bool {
    send_to_client_with_attempts(conn_id, msg, crypto, session, SEND_ATTEMPTS).await
}

/// Delivery attempts `send_to_client` spends before declaring a client
/// unreachable. See the note on that function for why the resulting budget has
/// to stay well above the client's `DISPATCH_STALL_TIMEOUT`.
const SEND_ATTEMPTS: u32 = 50;

/// `send_to_client` with an explicit attempt budget. Only teardown paths that
/// already know the client is gone should pass anything but `SEND_ATTEMPTS`:
/// one best-effort attempt still gets the message out if the client happens to
/// be reachable, without spending the full budget discovering it is not.
async fn send_to_client_with_attempts(
    conn_id: u32,
    msg: &Message,
    crypto: &Crypto,
    session: &Arc<Mutex<Session>>,
    attempts: u32,
) -> bool {
    let bytes = match msg.to_bytes() {
        Ok(b) => b,
        Err(e) => { error!("[{}] serialize: {}", conn_id, e); return false; }
    };
    let encrypted = match crypto.encrypt(&bytes) {
        Ok(e) => e,
        Err(e) => { error!("[{}] encrypt: {}", conn_id, e); return false; }
    };
    for attempt in 0..attempts {
        let sse_tx = {
            let sess = session.lock().await;
            sess.sse_tx.clone()
        };
        // Bound each send: a half-open client keeps the old bounded channel's
        // receiver alive-but-undrained, so send() would park forever and defeat
        // the re-fetch above. On timeout, fall through to pick up a reconnect.
        if tokio::time::timeout(Duration::from_millis(500), sse_tx.send(encrypted.clone()))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return true;
        }
        // Space out retries only when there is another one coming: a caller
        // that already knows the client is gone spends one attempt, and should
        // not pay a backoff on its way to giving up.
        if attempt + 1 < attempts {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    false
}

/// `pull`: tar + zstd-compress `path` and stream it to the client over SSE,
/// then report completion (ExitStatus + Close) or an Error.
async fn relay_pull(
    conn_id: u32,
    paths: Vec<PathBuf>,
    level: i32,
    session: Arc<Mutex<Session>>,
    crypto: Arc<Crypto>,
) {
    let (mut chunks, comp_handle) = spawn_compress(paths, level);

    let mut client_gone = false;
    while let Some(data) = chunks.recv().await {
        if !send_to_client(conn_id, &Message::Data { conn_id, data }, &crypto, &session).await {
            client_gone = true;
            break;
        }
    }

    // If we bailed early, drop the receiver so the blocking compressor's next
    // `blocking_send` errors out instead of parking forever on the bounded
    // channel (which would hang `comp_handle.await` and leak the thread).
    if client_gone {
        drop(chunks);
    }

    let result = match comp_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{:#}", e)),
        Err(e) => Err(format!("compress task failed: {}", e)),
    };

    if client_gone {
        debug!("[{}] PULL aborted: client connection lost", conn_id);
    } else if let Err(message) = result {
        error!("[{}] PULL failed: {}", conn_id, message);
        let _ = send_to_client(
            conn_id,
            &Message::Error { conn_id: Some(conn_id), message },
            &crypto,
            &session,
        )
        .await;
    } else {
        // Only attempt Close if ExitStatus landed: a false return means the
        // client is permanently gone, so a second 5s retry round is wasted.
        if send_to_client(conn_id, &Message::ExitStatus { conn_id, code: 0 }, &crypto, &session).await {
            let _ = send_to_client(conn_id, &Message::Close { conn_id }, &crypto, &session).await;
        }
        info!("[{}] PULL complete", conn_id);
    }
}

/// `push`: await the decompressor draining the client's incoming archive (fed
/// via the conn's writer channel + Close), then report completion or an Error.
async fn relay_push(
    conn_id: u32,
    unpack_handle: tokio::task::JoinHandle<Result<()>>,
    session: Arc<Mutex<Session>>,
    crypto: Arc<Crypto>,
) {
    // Watchdog: a client that dies mid-push (network drop, killed process)
    // never sends Close, so its writer sender lingers in `tcp_writers` and the
    // decompressor parks forever on a truncated archive's blocking read. Mirror
    // the PTY watchdog — once SSE has been closed for SSE_DEAD_GRACE, drop the
    // writer to force EOF so the unpack fails cleanly instead of hanging.
    let watchdog = await_sse_dead(session.clone());

    tokio::pin!(unpack_handle);
    let join = tokio::select! {
        res = &mut unpack_handle => res,
        _ = watchdog => {
            debug!("[{}] PUSH watchdog: SSE gone; forcing EOF on stalled unpack", conn_id);
            // Drop the writer so the decompressor sees EOF and unblocks.
            session.lock().await.tcp_writers.remove(&conn_id);
            (&mut unpack_handle).await
        }
    };
    let result = match join {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{:#}", e)),
        Err(e) => Err(format!("unpack task failed: {}", e)),
    };

    // Close normally removes the writer; drop it here too in case the unpack
    // finished (tar end-marker) before the client's Close arrived.
    {
        let mut sess = session.lock().await;
        sess.tcp_writers.remove(&conn_id);
    }

    match result {
        Ok(()) => {
            // Skip Close if ExitStatus failed — the client is gone and a
            // second 5s retry round would just delay task cleanup.
            if send_to_client(conn_id, &Message::ExitStatus { conn_id, code: 0 }, &crypto, &session).await {
                let _ = send_to_client(conn_id, &Message::Close { conn_id }, &crypto, &session).await;
            }
            info!("[{}] PUSH complete", conn_id);
        }
        Err(message) => {
            error!("[{}] PUSH failed: {}", conn_id, message);
            let _ = send_to_client(
                conn_id,
                &Message::Error { conn_id: Some(conn_id), message },
                &crypto,
                &session,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal `Close` must survive an SSE queue that is momentarily full.
    /// The relay used to make a single 500ms attempt and give up, so a client
    /// that stalled (or was mid-reconnect) never learned the target closed and
    /// its relay was left waiting forever.
    #[tokio::test]
    async fn terminal_close_survives_full_sse_queue() {
        const CONN_ID: u32 = 42;

        // Target that accepts and immediately closes: the relay's read half
        // sees EOF straight away and heads for the terminal Close.
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((sock, _)) = target.accept().await {
                drop(sock);
            }
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        // Capacity-1 SSE channel, pre-filled so the queue is full and undrained.
        let (sse_tx, mut sse_rx) = mpsc::channel::<Vec<u8>>(1);
        sse_tx.send(b"already queued".to_vec()).await.unwrap();

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));

        // Keep the writer alive so only the read half drives teardown.
        let (_write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);

        tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            Arc::new(Notify::new()),
            session.clone(),
            crypto.clone(),
        ));

        // Stay full well past the old 500ms single-shot budget, then drain.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(sse_rx.recv().await.unwrap(), b"already queued".to_vec());

        let queued = tokio::time::timeout(Duration::from_secs(10), sse_rx.recv())
            .await
            .expect("terminal Close was dropped instead of retried")
            .expect("SSE channel closed before the Close arrived");

        let decrypted = crypto.decrypt(&queued).unwrap();
        match Message::from_bytes(&decrypted).unwrap() {
            Message::Close { conn_id } => assert_eq!(conn_id, CONN_ID),
            other => panic!("expected Close, got {:?}", other),
        }
    }

    /// A client that vanishes after the target closed its output must not leave
    /// the relay parked. The writer deliberately outlives target-output EOF so
    /// uploads can finish, which means nothing else will ever wake the write
    /// task — only the SSE watchdog can end it. Time is paused, so the grace
    /// period elapses without a real wait.
    #[tokio::test(start_paused = true)]
    async fn write_side_gives_up_when_the_client_vanishes() {
        const CONN_ID: u32 = 44;

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((sock, _)) = target.accept().await {
                drop(sock);
            }
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        // Client is gone: its SSE receiver is dropped, so sse_tx is closed.
        let (sse_tx, sse_rx) = mpsc::channel::<Vec<u8>>(16);
        drop(sse_rx);

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));

        // Registered writer, held open the way a live client's would be: only
        // the client's Close would normally remove it, and it never arrives.
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);
        session.lock().await.tcp_writers.insert(CONN_ID, write_tx);

        let relay = tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            Arc::new(Notify::new()),
            session.clone(),
            crypto,
        ));

        tokio::time::timeout(Duration::from_secs(600), relay)
            .await
            .expect("relay never finished: the write side parked on a dead client")
            .expect("relay task panicked");

        assert!(
            !session.lock().await.tcp_writers.contains_key(&CONN_ID),
            "writer should have been cleaned up after teardown"
        );
    }

    /// The write side's watchdog is deliberately not started until the read
    /// task has gone, so this covers the one case that actually depends on it:
    /// the target closes its output cleanly, the terminal `Close` is delivered,
    /// and the writer is kept registered so an upload could still finish. The
    /// read task is then finished and nothing else can wake this half, so only
    /// the watchdog can end it once the client vanishes. Time is paused, so the
    /// grace period elapses without a real wait.
    #[tokio::test(start_paused = true)]
    async fn write_side_watchdog_starts_after_the_read_task_keeps_the_writer() {
        const CONN_ID: u32 = 46;

        // Target that closes its output immediately: a clean EOF, not a failure,
        // so the read task sends Close and keeps the writer.
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((sock, _)) = target.accept().await {
                drop(sock);
            }
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        // Healthy client to begin with, so the terminal Close is delivered and
        // the writer is deliberately left registered.
        let (sse_tx, mut sse_rx) = mpsc::channel::<Vec<u8>>(16);

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));

        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);
        session.lock().await.tcp_writers.insert(CONN_ID, write_tx);

        let relay = tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            Arc::new(Notify::new()),
            session.clone(),
            crypto.clone(),
        ));

        // Wait for the Close: past this point the read task has finished and
        // has left the writer in place, so the write half is parked with
        // nothing but its watchdog left to end it.
        let queued = tokio::time::timeout(Duration::from_secs(60), sse_rx.recv())
            .await
            .expect("terminal Close never arrived")
            .expect("SSE channel closed before the Close arrived");
        let decrypted = crypto.decrypt(&queued).unwrap();
        match Message::from_bytes(&decrypted).unwrap() {
            Message::Close { conn_id } => assert_eq!(conn_id, CONN_ID),
            other => panic!("expected Close, got {:?}", other),
        }
        assert!(
            session.lock().await.tcp_writers.contains_key(&CONN_ID),
            "the writer should outlive target-output EOF so an upload can finish"
        );

        // Now the client vanishes.
        drop(sse_rx);

        tokio::time::timeout(Duration::from_secs(600), relay)
            .await
            .expect("write side parked: its watchdog never started after the read task ended")
            .expect("relay task panicked");

        assert!(
            !session.lock().await.tcp_writers.contains_key(&CONN_ID),
            "writer should have been cleaned up after teardown"
        );
    }

    /// The same teardown bound, but with an idle target: it never sends and
    /// never closes, so the read side has nothing to react to and the delivery
    /// retries never run. Without a watchdog there the task blocks on read()
    /// forever, leaking the task, the socket and the session entry.
    #[tokio::test(start_paused = true)]
    async fn read_side_gives_up_when_the_client_vanishes() {
        const CONN_ID: u32 = 45;

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        // Accept and hold: silent, but very much open.
        let held = tokio::spawn(async move {
            let accepted = target.accept().await;
            std::future::pending::<()>().await;
            drop(accepted);
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        let (sse_tx, sse_rx) = mpsc::channel::<Vec<u8>>(16);
        drop(sse_rx);

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));

        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);
        session.lock().await.tcp_writers.insert(CONN_ID, write_tx);

        let relay = tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            Arc::new(Notify::new()),
            session.clone(),
            crypto,
        ));

        tokio::time::timeout(Duration::from_secs(600), relay)
            .await
            .expect("relay never finished: the read side blocked on an idle target")
            .expect("relay task panicked");

        assert!(
            !session.lock().await.tcp_writers.contains_key(&CONN_ID),
            "writer should have been cleaned up after teardown"
        );
        held.abort();
    }

    /// A terminal `Close` must follow the client across an SSE reconnect. The
    /// relay used to snapshot `sse_tx` once, so a Close racing a reconnect went
    /// into the replaced channel and was never seen on the live one.
    #[tokio::test]
    async fn terminal_close_follows_replaced_sse_channel() {
        const CONN_ID: u32 = 43;

        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((sock, _)) = target.accept().await {
                drop(sock);
            }
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        // The "old" stream: full and undrained, as a stalled client leaves it.
        let (old_tx, _old_rx) = mpsc::channel::<Vec<u8>>(1);
        old_tx.send(b"stale".to_vec()).await.unwrap();

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx: old_tx,
        }));

        let (_write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);

        tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            Arc::new(Notify::new()),
            session.clone(),
            crypto.clone(),
        ));

        // Client reconnects: handle_stream swaps in a fresh channel while the
        // relay is still trying to deliver its Close to the old one.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let (new_tx, mut new_rx) = mpsc::channel::<Vec<u8>>(16);
        session.lock().await.sse_tx = new_tx;

        let queued = tokio::time::timeout(Duration::from_secs(10), new_rx.recv())
            .await
            .expect("terminal Close never reached the reconnected stream")
            .expect("SSE channel closed before the Close arrived");

        let decrypted = crypto.decrypt(&queued).unwrap();
        match Message::from_bytes(&decrypted).unwrap() {
            Message::Close { conn_id } => assert_eq!(conn_id, CONN_ID),
            other => panic!("expected Close, got {:?}", other),
        }
    }

    /// The watchdog lives inside a `select!` that stops polling it the moment
    /// the other branch wins. Tokio's mutex is fair, so a freed lock is handed
    /// to the waiter at the head of its queue whether or not anyone is still
    /// polling it: a watchdog that queued on `session.lock()` strands the lock,
    /// and every later `send_to_client` on that session parks on it forever -
    /// the relay goes silent mid-connection with no error anywhere.
    /// Time is paused, so the watchdog's tick elapses without a real wait.
    #[tokio::test(start_paused = true)]
    async fn the_sse_watchdog_never_strands_the_session_lock() {
        let (sse_tx, _sse_rx) = mpsc::channel::<Vec<u8>>(16);
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));

        // Someone else holds the lock, as `handle_send` briefly does for every
        // uploaded chunk.
        let held = session.clone().lock_owned().await;

        let watchdog = await_sse_dead(session.clone());
        tokio::pin!(watchdog);
        // Poll the watchdog past its first tick, into the point where it wants
        // the session lock, then abandon it - exactly what `select!` does each
        // time the relay's other branch wins.
        for _ in 0..5 {
            tokio::select! {
                biased;
                _ = &mut watchdog => unreachable!("the stream is live; this must not fire"),
                _ = tokio::time::sleep(Duration::from_millis(400)) => {}
            }
        }

        drop(held);
        let regained = tokio::time::timeout(Duration::from_secs(30), session.lock())
            .await
            .expect("watchdog stranded the session lock: every send_to_client would park here");
        drop(regained);
    }

    /// `Message::Abort` must release the target entirely, not just the upload
    /// direction. `Close` deliberately keeps the read task pulling the target's
    /// output - the half-close an app that finished sending relies on - so a
    /// client whose app *aborted* used to leave this relay streaming the whole
    /// remaining response into a conn_id the client had already forgotten.
    /// Real time: nothing here waits on a timer, and the SSE receiver is
    /// drained so every delivery succeeds on its first attempt.
    #[tokio::test]
    async fn an_abort_releases_a_target_that_is_still_sending() {
        const CONN_ID: u32 = 47;

        // Target that streams until its socket fails, and reports when it did.
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = target.accept().await else { return };
            let chunk = vec![7u8; 65536];
            while sock.write_all(&chunk).await.is_ok() {}
            let _ = closed_tx.send(());
        });

        let stream = TcpStream::connect(target_addr).await.unwrap();
        let (tcp_read, tcp_write) = stream.into_split();

        // A healthy, draining client, so the response is flowing when the
        // abort lands and nothing else could end the read task.
        let (sse_tx, mut sse_rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move { while sse_rx.recv().await.is_some() {} });

        let crypto = Arc::new(Crypto::new("test-password").unwrap());
        let abort = Arc::new(Notify::new());
        let session = Arc::new(Mutex::new(Session {
            tcp_writers: HashMap::new(),
            aborts: HashMap::new(),
            #[cfg(unix)]
            pty_resize: HashMap::new(),
            sse_tx,
        }));
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(4);
        {
            let mut sess = session.lock().await;
            sess.tcp_writers.insert(CONN_ID, write_tx);
            sess.aborts.insert(CONN_ID, abort.clone());
        }

        let relay = tokio::spawn(relay_tcp_connection(
            CONN_ID,
            "127.0.0.1",
            target_addr.port(),
            tcp_read,
            tcp_write,
            write_rx,
            abort,
            session.clone(),
            crypto,
        ));

        // Let the response get going, then abort exactly as handle_send does.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!relay.is_finished(), "relay ended before the abort");
        {
            let mut sess = session.lock().await;
            sess.tcp_writers.remove(&CONN_ID);
            sess.aborts.remove(&CONN_ID).expect("abort signal was registered").notify_one();
        }

        tokio::time::timeout(Duration::from_secs(10), relay)
            .await
            .expect("relay kept pulling the target after the client aborted")
            .expect("relay task panicked");
        tokio::time::timeout(Duration::from_secs(10), closed_rx)
            .await
            .expect("target socket was not released by the abort")
            .unwrap();
        assert!(session.lock().await.aborts.is_empty(), "abort signal should be cleaned up");
    }

}
