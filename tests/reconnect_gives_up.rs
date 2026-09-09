//! The client's reconnect loop used to retry forever at a flat interval, so a
//! server that never came back left the process alive, logging an error every
//! few seconds and failing every proxied connection. It now gives up after
//! `client.max_reconnect_attempts` consecutive attempts that get no data.
//!
//! The subtle half is the reset: reconnects are routine on a long-lived tunnel
//! (the server restarts, or evicts the session), so the budget must bound an
//! unbroken run of failures and not reconnects over the tunnel's lifetime.
//! Both halves are covered here.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

mod common;
use crate::common::no_inherited_proxy;

fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind");
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
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn read_log(log_path: &Path) -> String {
    let mut buf = String::new();
    std::fs::File::open(log_path)
        .and_then(|mut f| f.read_to_string(&mut buf))
        .ok();
    buf
}

/// Wait for a substring to appear in a log file.
fn wait_for_log(log_path: &Path, target: &str, timeout_ms: u64) -> bool {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if read_log(log_path).contains(target) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Wait for `target` to appear at least `n` times.
fn wait_for_log_count(log_path: &Path, target: &str, n: usize, timeout_ms: u64) -> bool {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if read_log(log_path).matches(target).count() >= n {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("failed to create temp dir");
    tmp
}

/// The server falls back to ./config.toml and then ~/.config/tunnix/config.toml
/// when no --config is given, so every server here gets an empty one of its
/// own: an ambient path_prefix would change what these tests exercise.
fn write_server_config(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("server.toml");
    std::fs::write(&path, "[server]\n").expect("write server config");
    path
}

fn spawn_server(bin: &str, server_config: &Path, port: u16) -> KillOnDrop {
    let mut cmd = Command::new(bin);
    cmd.args([
        "server",
        "--config",
        server_config.to_str().unwrap(),
        "--listen",
        &format!("127.0.0.1:{}", port),
        "-p",
        "test",
    ]);
    no_inherited_proxy(&mut cmd);
    KillOnDrop(
        cmd.stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start server"),
    )
}

fn spawn_client(bin: &str, config: &Path, log: &Path) -> Child {
    let mut cmd = Command::new(bin);
    cmd.args([
        "client",
        "--config",
        config.to_str().unwrap(),
        "--log",
        log.to_str().unwrap(),
        "-p",
        "test",
    ]);
    no_inherited_proxy(&mut cmd);
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start client")
}

fn write_client_config(path: &Path, server_port: u16, proxy_port: u16, max_attempts: u32) {
    let content = format!(
        r#"[client]
server_url = "http://127.0.0.1:{server_port}"
local_addr = "127.0.0.1:{proxy_port}"
password = ""
reconnect_interval = 1
max_reconnect_attempts = {max_attempts}
"#,
    );
    std::fs::write(path, content).expect("failed to write config");
}

/// Wait for the child to exit, returning its status.
fn wait_for_exit(child: &mut Child, timeout_ms: u64) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("try_wait failed: {}", e),
        }
    }
    None
}

#[test]
fn test_client_exits_after_max_reconnect_attempts() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");
    let tmp = tmp_dir("tunnix_itest_giveup");
    let server_config = write_server_config(&tmp);
    let config_path = tmp.join("config.toml");
    let log_path = tmp.join("client.log");

    let port = find_free_port();
    let proxy_port = find_free_port();
    write_client_config(&config_path, port, proxy_port, 2);

    let server = spawn_server(bin, &server_config, port);
    assert!(wait_for_server(port, 10_000), "server did not become ready");

    let mut client = spawn_client(bin, &config_path, &log_path);
    assert!(
        wait_for_log(&log_path, "Tunnel established", 15_000),
        "client did not establish tunnel:\n{}",
        read_log(&log_path)
    );

    // Take the server away for good. Two failed attempts at a 1s interval
    // should be spent in a few seconds; allow generous slack for a loaded CI
    // box, but far less than the forever the old loop would have run for.
    drop(server);

    let status = wait_for_exit(&mut client, 30_000);
    let log = read_log(&log_path);
    let status = match status {
        Some(s) => s,
        None => {
            let _ = client.kill();
            panic!("client kept retrying instead of giving up:\n{}", log);
        }
    };

    assert!(!status.success(), "expected a failure exit, got {:?}", status);
    assert!(
        log.contains("giving up"),
        "expected the give-up reason in the log:\n{}",
        log
    );
    assert!(
        log.contains("2 consecutive attempt(s)"),
        "expected exactly the configured budget to be spent:\n{}",
        log
    );
}

#[test]
fn test_healthy_reconnect_resets_the_budget() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");
    let tmp = tmp_dir("tunnix_itest_giveup_reset");
    let server_config = write_server_config(&tmp);
    let config_path = tmp.join("config.toml");
    let log_path = tmp.join("client.log");

    let port = find_free_port();
    let proxy_port = find_free_port();
    write_client_config(&config_path, port, proxy_port, 3);

    let server = spawn_server(bin, &server_config, port);
    assert!(wait_for_server(port, 10_000), "server did not become ready");

    let mut client = spawn_client(bin, &config_path, &log_path);
    assert!(
        wait_for_log(&log_path, "Tunnel established", 15_000),
        "client did not establish tunnel:\n{}",
        read_log(&log_path)
    );

    // Bounce the server on the same port more times than the budget allows.
    // Each bounce costs the client a reconnect, but every one of them lands on
    // a live server and gets a Reset frame, so no two failures are
    // consecutive: a lifetime-counting budget would have killed the client on
    // the third bounce.
    let mut current = Some(server);
    for bounce in 1..=4 {
        drop(current.take());
        current = Some(spawn_server(bin, &server_config, port));
        assert!(
            wait_for_server(port, 10_000),
            "server did not come back for bounce {}",
            bounce
        );
        // +1 for the initial connect.
        assert!(
            wait_for_log_count(&log_path, "SSE stream connected", bounce + 1, 20_000),
            "client did not reconnect after bounce {}:\n{}",
            bounce,
            read_log(&log_path)
        );
        assert!(
            client.try_wait().expect("try_wait failed").is_none(),
            "client gave up across healthy reconnects:\n{}",
            read_log(&log_path)
        );
    }

    // Same client, server now gone for good: the budget still applies.
    drop(current.take());
    let status = wait_for_exit(&mut client, 30_000);
    let log = read_log(&log_path);
    match status {
        Some(s) => assert!(!s.success(), "expected a failure exit, got {:?}", s),
        None => {
            let _ = client.kill();
            panic!("client kept retrying instead of giving up:\n{}", log);
        }
    }
    assert!(
        log.contains("3 consecutive attempt(s)"),
        "expected the full budget to be spent after the final kill:\n{}",
        log
    );
}
