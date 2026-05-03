use crate::{http_proxy, socks5};
use crate::tunnel::Tunnel;
use anyhow::{bail, Result};
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, info};

pub async fn run_proxy(listen_addr: &str, tunnel: Arc<Tunnel>) -> Result<()> {
    let listener = TcpListener::bind(listen_addr).await?;
    info!("Proxy listening on {} (SOCKS5 + HTTP)", listen_addr);

    loop {
        let (stream, addr) = listener.accept().await?;
        debug!("Client connected from {}", addr);
        let tunnel = tunnel.clone();
        tokio::spawn(async move {
            if let Err(e) = dispatch(stream, tunnel).await {
                debug!("Client error from {}: {}", addr, e);
            }
        });
    }
}

async fn dispatch(stream: tokio::net::TcpStream, tunnel: Arc<Tunnel>) -> Result<()> {
    let mut peek = [0u8; 1];
    stream.peek(&mut peek).await?;

    match peek[0] {
        // SOCKS5 handshake always starts with version byte 0x05
        0x05 => socks5::handle_socks5_client(stream, tunnel).await,
        // HTTP methods all start with ASCII uppercase letters
        b'A'..=b'Z' | b'a'..=b'z' => http_proxy::handle_http_proxy_client(stream, tunnel).await,
        b => bail!("Unknown protocol (first byte: 0x{:02x})", b),
    }
}
