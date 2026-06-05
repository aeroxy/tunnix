use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        thread::sleep(Duration::from_millis(200));
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

fn start_server(bin: &str, port: u16, allow_transfer: bool) -> KillOnDrop {
    let mut args = vec![
        "server".to_string(),
        "--listen".to_string(),
        format!("127.0.0.1:{}", port),
        "-p".to_string(),
        "test".to_string(),
    ];
    if allow_transfer {
        args.push("--allow-transfer".to_string());
    }
    KillOnDrop(
        Command::new(bin)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start server"),
    )
}

/// Run `tunnix push|pull <paths...>` and return whether it exited successfully.
/// `paths` is the cp-style source(s)... + destination tail.
fn run_transfer(bin: &str, port: u16, sub: &str, paths: &[&str]) -> bool {
    let mut args = vec![
        sub.to_string(),
        "-s".to_string(),
        format!("http://127.0.0.1:{}", port),
        "-p".to_string(),
        "test".to_string(),
    ];
    args.extend(paths.iter().map(|s| s.to_string()));
    Command::new(bin)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run transfer")
        .success()
}

#[test]
fn test_push_pull_roundtrip() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    let tmp = std::env::temp_dir().join(format!("tunnix_transfer_itest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    // Build a source tree: a small text file, a nested binary file.
    let src = tmp.join("src");
    std::fs::create_dir_all(src.join("nested")).unwrap();
    std::fs::write(src.join("a.txt"), b"hello transfer").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("nested/big.bin"), &big).unwrap();

    let port = find_free_port();
    let _server = start_server(bin, port, true);
    assert!(wait_for_server(port, 10_000), "server did not become ready");

    // --- push: src -> server-side `remote/` (unpacks to remote/src/...) ---
    let remote = tmp.join("remote");
    assert!(
        run_transfer(bin, port, "push", &[src.to_str().unwrap(), remote.to_str().unwrap()]),
        "push failed"
    );
    assert_eq!(
        std::fs::read(remote.join("src/a.txt")).unwrap(),
        b"hello transfer"
    );
    assert_eq!(std::fs::read(remote.join("src/nested/big.bin")).unwrap(), big);

    // --- pull: server-side `remote/src` -> local `pulled/` ---
    let pulled = tmp.join("pulled");
    let remote_src = remote.join("src");
    assert!(
        run_transfer(bin, port, "pull", &[remote_src.to_str().unwrap(), pulled.to_str().unwrap()]),
        "pull failed"
    );
    assert_eq!(
        std::fs::read(pulled.join("src/a.txt")).unwrap(),
        b"hello transfer"
    );
    assert_eq!(std::fs::read(pulled.join("src/nested/big.bin")).unwrap(), big);

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_push_pull_multiple_sources() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    let tmp = std::env::temp_dir().join(format!("tunnix_transfer_multi_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    // Two standalone files and one directory — all in one transfer.
    std::fs::write(tmp.join("one.txt"), b"first").unwrap();
    std::fs::write(tmp.join("two.txt"), b"second").unwrap();
    let dir = tmp.join("dir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("inner.txt"), b"nested").unwrap();

    let port = find_free_port();
    let _server = start_server(bin, port, true);
    assert!(wait_for_server(port, 10_000), "server did not become ready");

    // push one.txt two.txt dir/ -> remote/  (last arg is the dest)
    let remote = tmp.join("remote");
    assert!(
        run_transfer(
            bin,
            port,
            "push",
            &[
                tmp.join("one.txt").to_str().unwrap(),
                tmp.join("two.txt").to_str().unwrap(),
                dir.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        ),
        "multi-source push failed"
    );
    assert_eq!(std::fs::read(remote.join("one.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(remote.join("two.txt")).unwrap(), b"second");
    assert_eq!(std::fs::read(remote.join("dir/inner.txt")).unwrap(), b"nested");

    // pull the three back together -> pulled/
    let pulled = tmp.join("pulled");
    assert!(
        run_transfer(
            bin,
            port,
            "pull",
            &[
                remote.join("one.txt").to_str().unwrap(),
                remote.join("two.txt").to_str().unwrap(),
                remote.join("dir").to_str().unwrap(),
                pulled.to_str().unwrap(),
            ],
        ),
        "multi-source pull failed"
    );
    assert_eq!(std::fs::read(pulled.join("one.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(pulled.join("two.txt")).unwrap(), b"second");
    assert_eq!(std::fs::read(pulled.join("dir/inner.txt")).unwrap(), b"nested");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_transfer_denied_when_disabled() {
    let bin = std::env!("CARGO_BIN_EXE_tunnix");

    let tmp = std::env::temp_dir().join(format!("tunnix_transfer_denied_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"nope").unwrap();

    let port = find_free_port();
    let _server = start_server(bin, port, false); // allow_transfer OFF
    assert!(wait_for_server(port, 10_000), "server did not become ready");

    let remote = tmp.join("remote");
    assert!(
        !run_transfer(bin, port, "push", &[src.to_str().unwrap(), remote.to_str().unwrap()]),
        "push should fail when transfers are disabled"
    );
    assert!(
        !run_transfer(bin, port, "pull", &[remote.to_str().unwrap(), tmp.join("pulled").to_str().unwrap()]),
        "pull should fail when transfers are disabled"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
