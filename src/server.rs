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
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
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
    /// Per-conn_id sink for client→target bytes. Covers both TCP connections and
    /// PTYs (remote exec) — a PTY is just another duplex byte stream.
    tcp_writers: HashMap<u32, mpsc::Sender<Vec<u8>>>,
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
        #[cfg(unix)]
        Message::Exec { conn_id, cmd, cols, rows } => {
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
            builder.env("TERM", "xterm-256color");

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
            let sse_tx = {
                let mut sess = session.lock().await;
                sess.tcp_writers.remove(&conn_id);
                sess.pty_resize.remove(&conn_id);
                sess.sse_tx.clone()
            };
            let msg = Message::Error {
                conn_id: Some(conn_id),
                message: format!("PTY setup failed: {}", e),
            };
            if let Ok(bytes) = msg.to_bytes() {
                if let Ok(encrypted) = crypto.encrypt(&bytes) {
                    let _ = sse_tx.send(encrypted).await;
                }
            }
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
    let session_watch = session.clone();
    let sse_dead = async move {
        let mut closed_secs: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let sess = session_watch.lock().await;
            if sess.sse_tx.is_closed() {
                closed_secs += 1;
                if closed_secs >= 6 {
                    return;
                }
            } else {
                closed_secs = 0;
            }
        }
    };
    // Box::pin so we can re-poll across loop iterations; the async block
    // holds a !Unpin MutexGuard across .await.
    let mut sse_dead = Box::pin(sse_dead);

    let mut resize_rx = resize_rx;
    let code = loop {
        tokio::select! {
            res = &mut wait_handle => break res.unwrap_or(-1),
            _ = &mut write_task => {
                // Client went away (Close removed the writer sender). Kill the child.
                let _ = killer.kill();
                break (&mut wait_handle).await.unwrap_or(-1);
            }
            _ = &mut sse_dead => {
                debug!("[{}] SSE stream closed continuously; killing orphaned PTY child", conn_id);
                let _ = killer.kill();
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
        if let Ok(bytes) = msg.to_bytes() {
            if let Ok(encrypted) = crypto.encrypt(&bytes) {
                let mut sent = false;
                for _ in 0..50 {
                    let sse_tx = {
                        let sess = session.lock().await;
                        sess.sse_tx.clone()
                    };
                    if sse_tx.send(encrypted.clone()).await.is_ok() {
                        sent = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if !sent {
                    error!("[{}] failed to deliver shutdown message", conn_id);
                }
            }
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
    let bytes = match msg.to_bytes() {
        Ok(b) => b,
        Err(e) => { error!("[{}] PTY serialize: {}", conn_id, e); return false; }
    };
    let encrypted = match crypto.encrypt(&bytes) {
        Ok(e) => e,
        Err(e) => { error!("[{}] PTY encrypt: {}", conn_id, e); return false; }
    };
    for _ in 0..50 {
        let sse_tx = {
            let sess = session.lock().await;
            sess.sse_tx.clone()
        };
        if sse_tx.send(encrypted.clone()).await.is_ok() {
            return true;
        }
        debug!("[{}] SSE stream replaced or closed; retrying", conn_id);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    error!("[{}] SSE reconnect timed out; dropping PTY output", conn_id);
    false
}
