use crate::relay::{next_conn_id, relay};
use crate::tunnel::Tunnel;
use anyhow::{anyhow, bail, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info};
use tunnix_common::protocol::Message;

pub async fn handle_http_proxy_client(mut stream: TcpStream, tunnel: Arc<Tunnel>) -> Result<()> {
    // Read headers byte-by-byte until \r\n\r\n.
    // The stream cursor lands right at the start of any request body.
    let mut header_buf = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        header_buf.push(byte[0]);
        if header_buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if header_buf.len() > 16384 {
            bail!("HTTP headers too large");
        }
    }

    let header_str = std::str::from_utf8(&header_buf)
        .map_err(|_| anyhow!("Non-UTF-8 in HTTP headers"))?;

    let mut lines = header_str.split("\r\n");
    let request_line = lines.next().ok_or_else(|| anyhow!("Empty HTTP request"))?;

    let mut parts = request_line.splitn(3, ' ');
    let method = parts.next().ok_or_else(|| anyhow!("Missing HTTP method"))?;
    let uri = parts.next().ok_or_else(|| anyhow!("Missing HTTP URI"))?;
    let version = parts.next().unwrap_or("HTTP/1.1");

    // Collect remaining header lines, excluding the blank terminal line
    let remaining_headers: String = lines
        .filter(|l| !l.is_empty())
        .map(|l| format!("{}\r\n", l))
        .collect();

    if method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(stream, tunnel, uri).await
    } else {
        handle_plain_http(stream, tunnel, method, uri, version, &remaining_headers).await
    }
}

async fn handle_connect(mut stream: TcpStream, tunnel: Arc<Tunnel>, authority: &str) -> Result<()> {
    let (host, port) = parse_authority(authority)?;
    let conn_id = next_conn_id();
    info!("[{}] HTTP CONNECT {}:{}", conn_id, host, port);

    let event_rx = tunnel.register_connection(conn_id).await;
    let connect_msg = Message::Connect {
        conn_id,
        host: host.clone(),
        port,
    };

    match tunnel.send_connect(&connect_msg).await {
        Ok(Some(Message::Data { data, .. })) if data.is_empty() => {
            debug!("[{}] CONNECT ACK", conn_id);
        }
        Ok(Some(Message::Error { message: msg, .. })) => {
            error!("[{}] CONNECT failed: {}", conn_id, msg);
            stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
        Ok(_) | Err(_) => {
            error!("[{}] CONNECT: no ACK from tunnel", conn_id);
            stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
    }

    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    info!("[{}] HTTP CONNECT tunnel established to {}:{}", conn_id, host, port);

    relay(stream, conn_id, tunnel, event_rx).await;
    info!("[{}] HTTP CONNECT closed for {}:{}", conn_id, host, port);
    Ok(())
}

async fn handle_plain_http(
    mut stream: TcpStream,
    tunnel: Arc<Tunnel>,
    method: &str,
    uri: &str,
    version: &str,
    headers: &str,
) -> Result<()> {
    let (host, port, path) = parse_http_uri(uri)?;
    let conn_id = next_conn_id();
    info!("[{}] HTTP {} {}:{}{}", conn_id, method, host, port, path);

    let event_rx = tunnel.register_connection(conn_id).await;
    let connect_msg = Message::Connect {
        conn_id,
        host: host.clone(),
        port,
    };

    match tunnel.send_connect(&connect_msg).await {
        Ok(Some(Message::Data { data, .. })) if data.is_empty() => {
            debug!("[{}] HTTP plain ACK", conn_id);
        }
        Ok(Some(Message::Error { message: msg, .. })) => {
            error!("[{}] HTTP plain connect failed: {}", conn_id, msg);
            stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
        Ok(_) | Err(_) => {
            error!("[{}] HTTP plain: no ACK from tunnel", conn_id);
            stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
    }

    // Forward the rewritten request (absolute URL → relative path) through the tunnel.
    // The relay loop below handles any body bytes still in the TCP stream.
    let request = format!("{} {} {}\r\n{}\r\n", method, path, version, headers);
    tunnel
        .send_message(&Message::Data {
            conn_id,
            data: request.into_bytes(),
        })
        .await?;

    relay(stream, conn_id, tunnel, event_rx).await;
    info!("[{}] HTTP {} closed for {}:{}{}", conn_id, method, host, port, path);
    Ok(())
}

/// Parse `host:port` from a CONNECT authority string.
/// Defaults to port 443 if no port is specified.
fn parse_authority(authority: &str) -> Result<(String, u16)> {
    match authority.rsplit_once(':') {
        Some((host, port_str)) => {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid port in CONNECT authority: {}", port_str))?;
            Ok((host.to_string(), port))
        }
        None => Ok((authority.to_string(), 443)),
    }
}

/// Parse `http://host:port/path` into (host, port, path).
/// Defaults to port 80 for http:// and 443 for https://.
fn parse_http_uri(uri: &str) -> Result<(String, u16, String)> {
    let (default_port, without_scheme) = if let Some(rest) = uri.strip_prefix("https://") {
        (443u16, rest)
    } else if let Some(rest) = uri.strip_prefix("http://") {
        (80u16, rest)
    } else {
        (80u16, uri)
    };

    let (authority, path) = match without_scheme.find('/') {
        Some(pos) => (&without_scheme[..pos], without_scheme[pos..].to_string()),
        None => (without_scheme, "/".to_string()),
    };

    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p_str)) => {
            let port = p_str
                .parse::<u16>()
                .map_err(|_| anyhow!("Invalid port in URI: {}", p_str))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), default_port),
    };

    Ok((host, port, path))
}
