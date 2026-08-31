use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rama::utils::octets::kib;

fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_log(path: &Path, needle: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if std::fs::read_to_string(path).is_ok_and(|log| log.contains(needle)) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn wait_for_server(port: u16) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Ok(mut stream) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            let _ = stream.write_all(b"GET /health HTTP/1.0\r\n\r\n");
            let mut response = String::new();
            if stream.read_to_string(&mut response).is_ok() && response.contains("200 OK") {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn read_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("read headers");
        bytes.push(byte[0]);
        assert!(bytes.len() < kib(16), "headers exceed test limit");
    }
    bytes
}

fn read_response(mut stream: TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let headers = read_headers(&mut stream);
    let headers_text = String::from_utf8(headers).expect("utf-8 response headers");
    let content_length = headers_text
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
        })
        .expect("response content-length");
    let mut body = vec![0; content_length];
    stream.read_exact(&mut body).expect("read response body");
    format!("{headers_text}{}", String::from_utf8(body).unwrap())
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn http_socks5_and_connect_share_the_rama_listener() {
    let target = TcpListener::bind("127.0.0.1:0").expect("bind target");
    let target_port = target.local_addr().unwrap().port();
    let target_thread = thread::spawn(move || {
        for request_index in 0..3 {
            let (mut stream, _) = target.accept().expect("accept target request");
            let request = String::from_utf8(read_headers(&mut stream)).expect("utf-8 request");
            if request_index == 0 {
                let custom_headers = request
                    .lines()
                    .filter(|line| line.starts_with("X-MiXeD:") || line.starts_with("x-second:"))
                    .collect::<Vec<_>>();
                assert_eq!(
                    custom_headers,
                    ["X-MiXeD: one", "x-second: middle", "X-MiXeD: two"]
                );
            }
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .expect("request path");
            let body = format!("target:{path}");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write target response");
        }
    });

    let bin = std::env!("CARGO_BIN_EXE_tunnix");
    let server_port = find_free_port();
    let proxy_port = find_free_port();
    let temp = std::env::temp_dir().join(format!("tunnix_proxy_itest_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).unwrap();
    let client_log = temp.join("client.log");

    let _server = KillOnDrop(
        Command::new(bin)
            .args([
                "server",
                "--listen",
                &format!("127.0.0.1:{server_port}"),
                "--password",
                "test",
            ])
            .current_dir(&temp)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tunnix server"),
    );
    assert!(wait_for_server(server_port), "server did not start");

    let _client = KillOnDrop(
        Command::new(bin)
            .args([
                "client",
                "--server",
                &format!("http://127.0.0.1:{server_port}"),
                "--password",
                "test",
                "--local-addr",
                &format!("127.0.0.1:{proxy_port}"),
                "--log",
                client_log.to_str().unwrap(),
            ])
            .current_dir(&temp)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tunnix client"),
    );
    assert!(
        wait_for_log(&client_log, "proxy listening"),
        "client did not start: {}",
        std::fs::read_to_string(&client_log).unwrap_or_default()
    );

    let mut http = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).unwrap();
    write!(
        http,
        "GET http://127.0.0.1:{target_port}/plain HTTP/1.1\r\nHost: 127.0.0.1:{target_port}\r\nX-MiXeD: one\r\nx-second: middle\r\nX-MiXeD: two\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    assert!(read_response(http).ends_with("target:/plain"));

    let mut socks = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).unwrap();
    socks.write_all(&[5, 1, 0]).unwrap();
    let mut auth = [0; 2];
    socks.read_exact(&mut auth).unwrap();
    assert_eq!(auth, [5, 0]);
    let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
    connect.extend_from_slice(&target_port.to_be_bytes());
    socks.write_all(&connect).unwrap();
    let mut reply = [0; 10];
    socks.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[..2], &[5, 0]);
    socks
        .write_all(b"GET /socks HTTP/1.1\r\nHost: target\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(read_response(socks).ends_with("target:/socks"));

    let mut connect = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy_port)).unwrap();
    write!(
        connect,
        "CONNECT 127.0.0.1:{target_port} HTTP/1.1\r\nHost: 127.0.0.1:{target_port}\r\n\r\n"
    )
    .unwrap();
    let response = String::from_utf8(read_headers(&mut connect)).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    connect
        .write_all(b"GET /connect HTTP/1.1\r\nHost: target\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(read_response(connect).ends_with("target:/connect"));

    target_thread.join().unwrap();
    let _ = std::fs::remove_dir_all(temp);
}
