use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Find an available TCP port on localhost
fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind");
    listener.local_addr().unwrap().port()
}

/// Perform a raw GET /health and check for 200
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

/// Wait for server to respond to health checks
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

/// Wait for a substring to appear in a log file
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

/// Read entire log file content
fn read_log(log_path: &Path) -> String {
    let mut buf = String::new();
    std::fs::File::open(log_path)
        .and_then(|mut f| f.read_to_string(&mut buf))
        .ok();
    buf
}

/// Write a minimal client config for the given server port and proxy port
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

/// Wraps a Child process and kills it on drop
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn test_client_reconnects_on_server_url_change() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    // Create temp directory for config and logs
    let tmp = std::env::temp_dir().join("tunnix_itest_reconnect");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("failed to create temp dir");

    let config_path = tmp.join("config.toml");
    let log_path = tmp.join("client.log");

    // Find ports
    let port_a = find_free_port();
    let port_b = find_free_port();
    let proxy_port = find_free_port();

    eprintln!(
        "test: ports server_a={} server_b={} proxy={}",
        port_a, port_b, proxy_port
    );

    // --- Phase 1: Connect to server A ---

    write_config(&config_path, port_a, proxy_port);

    let _server_a = KillOnDrop(
        Command::new(bin)
            .args([
                "server",
                "--listen",
                &format!("127.0.0.1:{}", port_a),
                "-p",
                "test",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start server A"),
    );

    assert!(
        wait_for_server(port_a, 10_000),
        "server A did not become ready"
    );

    let _client = KillOnDrop(
        Command::new(bin)
            .args([
                "client",
                "--config",
                config_path.to_str().unwrap(),
                "--log",
                log_path.to_str().unwrap(),
                "-p",
                "test",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start client"),
    );

    assert!(
        wait_for_log(&log_path, "Tunnel established", 15_000),
        "client did not establish tunnel to server A"
    );

    let initial_log = read_log(&log_path);
    assert!(
        initial_log.contains(&format!("127.0.0.1:{}", port_a)),
        "expected client to connect to server A, got:\n{}",
        initial_log
    );

    // --- Phase 2: Switch to server B ---

    let _server_b = KillOnDrop(
        Command::new(bin)
            .args([
                "server",
                "--listen",
                &format!("127.0.0.1:{}", port_b),
                "-p",
                "test",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start server B"),
    );

    assert!(
        wait_for_server(port_b, 10_000),
        "server B did not become ready"
    );

    // Give the mtime a chance to differ from the initial write
    thread::sleep(Duration::from_millis(100));

    // Rewrite config pointing to server B
    write_config(&config_path, port_b, proxy_port);

    // The config watcher polls every 3s + 200ms debounce + reconnect time
    assert!(
        wait_for_log(&log_path, &format!("127.0.0.1:{}", port_b), 25_000),
        "client did not reconnect to server B.\nFull log:\n{}",
        read_log(&log_path)
    );

    // Confirm we see a reconnect event in the log
    let final_log = read_log(&log_path);
    assert!(
        final_log.contains("Reconnecting with updated config"),
        "expected reconnect signal in log:\n{}",
        final_log
    );
    assert!(
        final_log.contains(&format!("127.0.0.1:{}", port_b)),
        "expected client to eventually connect to server B:\n{}",
        final_log
    );

    eprintln!("test PASSED");
}
