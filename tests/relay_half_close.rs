//! Half-close semantics through the SOCKS5 relay, in both directions.
//!
//! Each direction of a proxied connection must end on its own. The relay used
//! to collapse both as soon as either one finished: a client that half-closed
//! after its request lost the response, and a target that finished sending
//! first had the rest of the client's upload silently discarded.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const RESPONSE_LEN: usize = 256 * 1024;
const UPLOAD_LEN: usize = 16 * 1024 * 1024;

/// Ask the OS for an unused localhost port.
fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind");
    listener.local_addr().unwrap().port()
}

/// Perform a raw GET /health and check for 200.
fn health_check(port: u16) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    if let Ok(mut stream) = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(1)) {
        // Bound the read: a socket that accepts and then says nothing would
        // otherwise hang the poll loop instead of failing this attempt.
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let req = "GET /health HTTP/1.0\r\n\r\n";
        let _ = stream.write_all(req.as_bytes());
        let mut resp = String::new();
        if stream.read_to_string(&mut resp).is_ok() {
            return resp.contains("200 OK");
        }
    }
    false
}

/// Poll the health endpoint until the server answers or the timeout expires.
fn wait_for_server(port: u16, timeout_ms: u64) -> bool {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if health_check(port) {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Wait for a substring to appear in a log file.
fn wait_for_log(log_path: &Path, target: &str, timeout_ms: u64) -> bool {
    let start = Instant::now();
    let mut buf = String::new();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        buf.clear();
        if let Ok(mut f) = std::fs::File::open(log_path) {
            if f.read_to_string(&mut buf).is_ok() && buf.contains(target) {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    false
}

/// Write a minimal client config pointing at the given server and proxy ports.
fn write_config(config_path: &Path, server_port: u16, proxy_port: u16) {
    let content = format!(
        r#"[client]
server_url = "http://127.0.0.1:{server_port}"
local_addr = "127.0.0.1:{proxy_port}"
password = ""
"#,
    );
    std::fs::write(config_path, content).expect("failed to write config");
}

/// Wraps a child process and kills it on drop.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Everything here talks over loopback, so an ambient proxy in the developer's
/// environment must not be inherited: the HTTP client honours `*_PROXY` and
/// would try to reach 127.0.0.1 through it.
fn spawn_direct(bin: &str, args: &[&str]) -> KillOnDrop {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
        cmd.env_remove(var);
    }
    KillOnDrop(
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn tunnix"),
    )
}

/// Target that only answers *after* it has seen the client's FIN: it reads to
/// EOF, then writes `RESPONSE_LEN` bytes. Any relay that collapses both
/// directions on upload EOF will never deliver this payload.
fn spawn_reply_after_eof_target() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind target");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let mut request = Vec::new();
            // Read to EOF: returns only once the proxied client half-closed.
            if sock.read_to_end(&mut request).is_err() {
                continue;
            }
            let payload: Vec<u8> = (0..RESPONSE_LEN).map(|i| (i % 251) as u8).collect();
            let _ = sock.write_all(&payload);
            let _ = sock.flush();
            let _ = sock.shutdown(Shutdown::Write);
        }
    });
    port
}

/// Target that closes its *output* first, then keeps reading: it writes a short
/// greeting, half-closes, and reports how many bytes it received afterwards.
/// A relay that drops the target writer on output EOF loses the whole upload.
fn spawn_read_after_own_eof_target() -> (u16, std::sync::mpsc::Receiver<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind target");
    let port = listener.local_addr().unwrap().port();
    let (report_tx, report_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let _ = sock.write_all(b"hi");
            let _ = sock.flush();
            // Our output is done; the peer may still be uploading.
            let _ = sock.shutdown(Shutdown::Write);

            let mut sink = Vec::new();
            let received = match sock.read_to_end(&mut sink) {
                Ok(n) => n,
                Err(_) => sink.len(),
            };
            let _ = report_tx.send(received);
        }
    });
    (port, report_rx)
}

/// Target that sends a partial response and then aborts the connection with a
/// RST, the way a crashing or resetting upstream does. The proxied client must
/// not be told this ended cleanly.
#[cfg(unix)]
fn spawn_reset_midstream_target() -> u16 {
    use std::os::fd::AsRawFd;

    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind target");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let _ = sock.write_all(b"partial");
            let _ = sock.flush();

            // SO_LINGER with a zero timeout turns close() into a RST instead of
            // a FIN, so the peer sees a failure rather than end-of-stream.
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
        }
    });
    port
}

/// SOCKS5 no-auth handshake + CONNECT to 127.0.0.1:`port`.
fn socks5_connect(proxy_port: u16, port: u16) -> TcpStream {
    let addr = format!("127.0.0.1:{}", proxy_port);
    let mut sock = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5))
        .expect("failed to connect to proxy");
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    // Bound writes too, not just reads. `test_upload_survives_target_output_eof`
    // pushes UPLOAD_LEN through this socket, so a relay that stops draining its
    // read half without closing the connection fills the socket buffer and
    // parks `write_all` forever - a hung test run rather than a failed one,
    // since cargo bounds neither. Applies per write syscall, so this trips only
    // when the relay makes no progress at all for 30s.
    sock.set_write_timeout(Some(Duration::from_secs(30))).unwrap();

    sock.write_all(&[0x05, 0x01, 0x00]).expect("greeting failed");
    let mut greeting = [0u8; 2];
    sock.read_exact(&mut greeting).expect("no auth reply");
    assert_eq!(greeting, [0x05, 0x00], "unexpected SOCKS5 auth reply");

    let mut req = vec![0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1];
    req.extend_from_slice(&port.to_be_bytes());
    sock.write_all(&req).expect("connect request failed");

    let mut reply = [0u8; 10];
    sock.read_exact(&mut reply).expect("no connect reply");
    assert_eq!(reply[0], 0x05, "unexpected SOCKS5 version in reply");
    assert_eq!(reply[1], 0x00, "SOCKS5 CONNECT was refused");

    sock
}

/// A running server + client pair with an established tunnel. Both processes
/// are killed when this is dropped.
struct Tunnel {
    proxy_port: u16,
    tmp: std::path::PathBuf,
    _server: KillOnDrop,
    _client: KillOnDrop,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Start a server and client pair and wait until the tunnel is established.
fn start_tunnel(name: &str) -> Tunnel {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    let tmp = std::env::temp_dir().join(format!("tunnix_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let config_path = tmp.join("config.toml");
    let log_path = tmp.join("client.log");

    let server_port = find_free_port();
    let proxy_port = find_free_port();
    write_config(&config_path, server_port, proxy_port);

    // The server falls back to ./config.toml and then ~/.config/tunnix/
    // config.toml when no --config is given, so point it at an empty file
    // in the test's own directory: an ambient path_prefix or allow_exec would
    // otherwise change what these tests are exercising.
    let server_config = tmp.join("server.toml");
    std::fs::write(&server_config, "[server]\n").expect("write server config");

    let server = spawn_direct(
        bin,
        &[
            "server",
            "--config",
            server_config.to_str().unwrap(),
            "--listen",
            &format!("127.0.0.1:{}", server_port),
            "-p",
            "test",
        ],
    );
    assert!(wait_for_server(server_port, 10_000), "server did not become ready");

    let client = spawn_direct(
        bin,
        &[
            "client",
            "--config",
            config_path.to_str().unwrap(),
            "--log",
            log_path.to_str().unwrap(),
            "-p",
            "test",
        ],
    );
    assert!(
        wait_for_log(&log_path, "Tunnel established", 15_000),
        "client did not establish tunnel"
    );

    Tunnel { proxy_port, tmp, _server: server, _client: client }
}

#[test]
fn test_response_survives_client_half_close() {
    let tunnel = start_tunnel("half_close");
    let target_port = spawn_reply_after_eof_target();

    let mut sock = socks5_connect(tunnel.proxy_port, target_port);

    // Send a request, then half-close: we are done sending, but still expect
    // the whole reply back.
    sock.write_all(b"ping").expect("request write failed");
    sock.flush().unwrap();
    sock.shutdown(Shutdown::Write).expect("half-close failed");

    let mut got = Vec::new();
    sock.read_to_end(&mut got).expect("response read failed");

    assert_eq!(
        got.len(),
        RESPONSE_LEN,
        "response truncated after half-close: got {} of {} bytes",
        got.len(),
        RESPONSE_LEN
    );
    let expected: Vec<u8> = (0..RESPONSE_LEN).map(|i| (i % 251) as u8).collect();
    assert_eq!(got, expected, "response payload corrupted");
}

/// The mirror direction: the *target* finishes sending first, and the upload
/// still in progress must keep flowing. The server used to drop the target's
/// writer as soon as target output hit EOF, so everything sent afterwards was
/// silently discarded.
#[test]
fn test_upload_survives_target_output_eof() {
    let tunnel = start_tunnel("upload_after_eof");
    let (target_port, received) = spawn_read_after_own_eof_target();

    let mut sock = socks5_connect(tunnel.proxy_port, target_port);

    // Drain the response direction to EOF: the target has half-closed, so the
    // relay should deliver its greeting and then close only this direction.
    let mut greeting = Vec::new();
    sock.read_to_end(&mut greeting).expect("greeting read failed");
    assert_eq!(greeting, b"hi", "unexpected greeting from target");

    // Response direction is done. The upload direction must still work.
    let payload: Vec<u8> = (0..UPLOAD_LEN).map(|i| (i % 251) as u8).collect();
    sock.write_all(&payload).expect("upload failed");
    sock.flush().unwrap();
    sock.shutdown(Shutdown::Write).expect("upload half-close failed");

    let total = received
        .recv_timeout(Duration::from_secs(60))
        .expect("target never reported a byte count");
    assert_eq!(
        total, UPLOAD_LEN,
        "upload truncated after target output EOF: target got {} of {} bytes",
        total, UPLOAD_LEN
    );
}

/// A target that fails mid-response must not look like one that finished. The
/// relay reported both as `Close`, so the proxied app saw a clean EOF on a
/// truncated stream and had no way to tell the difference.
#[cfg(unix)]
#[test]
fn test_target_failure_is_not_a_clean_eof() {
    let tunnel = start_tunnel("target_reset");
    let target_port = spawn_reset_midstream_target();

    let mut sock = socks5_connect(tunnel.proxy_port, target_port);
    sock.write_all(b"go").expect("request write failed");
    sock.flush().unwrap();

    // Reading to the end must fail, not succeed: the response was truncated by
    // the target's reset. Whether the partial bytes land first is a race, so
    // assert only on the property that matters.
    let mut got = Vec::new();
    let result = sock.read_to_end(&mut got);

    let err = match result {
        Ok(_) => panic!(
            "truncated response was reported as a clean EOF after {} byte(s)",
            got.len()
        ),
        Err(e) => e,
    };
    // The failure has to come from the connection being reset, not from this
    // socket's own read timeout: a relay that simply stalls would otherwise
    // look identical to one that correctly signalled the failure.
    assert!(
        !matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "read timed out instead of the connection being reset: {:?}",
        err
    );
}
