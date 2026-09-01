//! Half-close semantics through the SOCKS5 relay.
//!
//! A client that finishes sending (shutdown(WR)) must still receive the full
//! response. The relay used to tear the response direction down as soon as the
//! upload direction hit EOF, so the reply was silently truncated or lost.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const RESPONSE_LEN: usize = 256 * 1024;

fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind");
    listener.local_addr().unwrap().port()
}

fn health_check(port: u16) -> bool {
    let addr = format!("127.0.0.1:{}", port);
    if let Ok(mut stream) = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(1)) {
        let req = "GET /health HTTP/1.0\r\n\r\n";
        let _ = stream.write_all(req.as_bytes());
        let mut resp = String::new();
        if stream.read_to_string(&mut resp).is_ok() {
            return resp.contains("200 OK");
        }
    }
    false
}

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

/// SOCKS5 no-auth handshake + CONNECT to 127.0.0.1:`port`.
fn socks5_connect(proxy_port: u16, port: u16) -> TcpStream {
    let addr = format!("127.0.0.1:{}", proxy_port);
    let mut sock = TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5))
        .expect("failed to connect to proxy");
    sock.set_read_timeout(Some(Duration::from_secs(30))).unwrap();

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

#[test]
fn test_response_survives_client_half_close() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    let tmp = std::env::temp_dir().join(format!("tunnix_half_close_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let config_path = tmp.join("config.toml");
    let log_path = tmp.join("client.log");

    let server_port = find_free_port();
    let proxy_port = find_free_port();
    let target_port = spawn_reply_after_eof_target();

    write_config(&config_path, server_port, proxy_port);

    let _server = spawn_direct(
        bin,
        &[
            "server",
            "--listen",
            &format!("127.0.0.1:{}", server_port),
            "-p",
            "test",
        ],
    );
    assert!(wait_for_server(server_port, 10_000), "server did not become ready");

    let _client = spawn_direct(
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

    let mut sock = socks5_connect(proxy_port, target_port);

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

    let _ = std::fs::remove_dir_all(&tmp);
}
