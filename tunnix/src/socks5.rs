use crate::relay::{next_conn_id, relay};
use crate::tunnel::Tunnel;
use anyhow::{bail, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info};
use tunnix_common::protocol::Message;

/// SOCKS5 constants
const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_AUTH_NONE: u8 = 0x00;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
const SOCKS5_REP_SUCCESS: u8 = 0x00;
const SOCKS5_REP_GENERAL_FAILURE: u8 = 0x01;
const SOCKS5_REP_CMD_NOT_SUPPORTED: u8 = 0x07;

pub async fn handle_socks5_client(mut stream: TcpStream, tunnel: Arc<Tunnel>) -> Result<()> {
    // Step 1: Authentication negotiation
    let mut buf = [0u8; 258];

    // Read version + nmethods
    stream.read_exact(&mut buf[..2]).await?;
    if buf[0] != SOCKS5_VERSION {
        bail!("Not SOCKS5 protocol (got version {})", buf[0]);
    }

    let nmethods = buf[1] as usize;
    stream.read_exact(&mut buf[..nmethods]).await?;

    // We only support no-auth
    let has_no_auth = buf[..nmethods].contains(&SOCKS5_AUTH_NONE);
    if !has_no_auth {
        stream.write_all(&[SOCKS5_VERSION, 0xFF]).await?;
        bail!("No acceptable auth method");
    }

    // Accept no-auth
    stream.write_all(&[SOCKS5_VERSION, SOCKS5_AUTH_NONE]).await?;

    // Step 2: Read connection request
    // VER CMD RSV ATYP DST.ADDR DST.PORT
    stream.read_exact(&mut buf[..4]).await?;

    if buf[0] != SOCKS5_VERSION {
        bail!("Invalid SOCKS5 request version");
    }

    let cmd = buf[1];
    // buf[2] is reserved
    let atyp = buf[3];

    if cmd != SOCKS5_CMD_CONNECT {
        let reply = [
            SOCKS5_VERSION,
            SOCKS5_REP_CMD_NOT_SUPPORTED,
            0x00,
            SOCKS5_ATYP_IPV4,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        stream.write_all(&reply).await?;
        bail!("Unsupported SOCKS5 command: {}", cmd);
    }

    // Parse destination address
    let host = match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            format!("{}.{}.{}.{}", addr[0], addr[1], addr[2], addr[3])
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len_buf = [0u8; 1];
            stream.read_exact(&mut len_buf).await?;
            let len = len_buf[0] as usize;
            let mut domain = vec![0u8; len];
            stream.read_exact(&mut domain).await?;
            String::from_utf8(domain)?
        }
        SOCKS5_ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            let parts: Vec<String> = addr
                .chunks(2)
                .map(|chunk| format!("{:02x}{:02x}", chunk[0], chunk[1]))
                .collect();
            parts.join(":")
        }
        _ => {
            bail!("Unsupported address type: {}", atyp);
        }
    };

    // Read port (2 bytes, big endian)
    let mut port_buf = [0u8; 2];
    stream.read_exact(&mut port_buf).await?;
    let port = u16::from_be_bytes(port_buf);

    let conn_id = next_conn_id();
    info!("[{}] SOCKS5 CONNECT {}:{}", conn_id, host, port);

    // Register connection to receive response events
    let event_rx = tunnel.register_connection(conn_id).await;

    // Send CONNECT message through tunnel; ACK comes back in the HTTP response body
    let connect_msg = Message::Connect {
        conn_id,
        host: host.clone(),
        port,
    };
    match tunnel.send_connect(&connect_msg).await {
        Ok(Some(Message::Data { data, .. })) if data.is_empty() => {
            debug!("[{}] Connect ACK received", conn_id);
        }
        Ok(Some(Message::Error { message: msg, .. })) => {
            error!("[{}] Connect failed: {}", conn_id, msg);
            let reply = [
                SOCKS5_VERSION,
                SOCKS5_REP_GENERAL_FAILURE,
                0x00,
                SOCKS5_ATYP_IPV4,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            stream.write_all(&reply).await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
        Ok(_) | Err(_) => {
            error!("[{}] Failed to send CONNECT or no ACK", conn_id);
            let reply = [
                SOCKS5_VERSION,
                SOCKS5_REP_GENERAL_FAILURE,
                0x00,
                SOCKS5_ATYP_IPV4,
                0,
                0,
                0,
                0,
                0,
                0,
            ];
            stream.write_all(&reply).await?;
            tunnel.unregister_connection(conn_id).await;
            return Ok(());
        }
    }

    // Send SOCKS5 success reply; BND.ADDR = 0.0.0.0, BND.PORT = 0
    let reply = [
        SOCKS5_VERSION,
        SOCKS5_REP_SUCCESS,
        0x00,
        SOCKS5_ATYP_IPV4,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    stream.write_all(&reply).await?;
    info!("[{}] SOCKS5 tunnel established to {}:{}", conn_id, host, port);

    relay(stream, conn_id, tunnel, event_rx).await;
    info!("[{}] SOCKS5 connection closed for {}:{}", conn_id, host, port);
    Ok(())
}
