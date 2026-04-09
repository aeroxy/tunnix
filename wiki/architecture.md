# Code Architecture

## Overview

```
Local SOCKS5/HTTP client
        │
        ▼
tunnix client  (tunnix client subcommand)
  ├── proxy.rs       — TCP listener, protocol detection
  ├── socks5.rs      — SOCKS5 handshake handler
  ├── http_proxy.rs  — HTTP CONNECT + plain HTTP handler
  ├── relay.rs       — bidirectional data relay, conn_id counter
  └── tunnel.rs      — HTTP/SSE tunnel to server

        │  HTTP POST /[prefix]/send/{session_id}   (client → server, encrypted)
        │  GET /[prefix]/stream/{session_id}  SSE  (server → client, encrypted)
        ▼

tunnix server  (tunnix server subcommand)
  └── server.rs      — hyper HTTP/1.1 server, per-session routing, prefix stripping

        │  raw TCP
        ▼

Target service (e.g. api.example.com:443)
```

---

## Client modules

### `proxy.rs` — listener and protocol dispatcher

`run_proxy(listen_addr, tunnel)` binds the TCP listener and accepts connections.

For each connection it calls `dispatch()`, which peeks at **one byte** without consuming it:

| First byte | Protocol |
|------------|----------|
| `0x05` | SOCKS5 — handed to `socks5::handle_socks5_client` |
| ASCII letter (`A`–`Z`, `a`–`z`) | HTTP — handed to `http_proxy::handle_http_proxy_client` |
| anything else | connection dropped with an error log |

`TcpStream::peek` is used so the handler still sees the first byte when it reads.

### `socks5.rs` — SOCKS5 handshake

Implements RFC 1928 for the CONNECT command only (no BIND, no UDP_ASSOCIATE). Steps:

1. Auth negotiation — only no-auth (`0x00`) is accepted.
2. Request parsing — IPv4 (`0x01`), domain (`0x03`), IPv6 (`0x04`).
3. Calls `tunnel.send_connect()` and waits for an ACK in the HTTP response body.
4. Sends SOCKS5 success reply (`0x05 0x00 ...`) to the client.
5. Calls `relay::relay()` to begin bidirectional data forwarding.

### `http_proxy.rs` — HTTP proxy handshake

Reads the request headers byte-by-byte until `\r\n\r\n`, then branches:

**CONNECT (for HTTPS)**
1. Parses `CONNECT host:port HTTP/1.x`.
2. Sends tunnel `Connect` message; waits for ACK.
3. Replies `HTTP/1.1 200 Connection Established\r\n\r\n`.
4. Calls `relay::relay()` — the client then does TLS directly through the tunnel.

**Plain HTTP (GET/POST/etc.)**
1. Parses the absolute URI (`http://host/path`), rewrites it to a relative path.
2. Sends tunnel `Connect` to `host:port`.
3. Forwards the rewritten request headers as a `Data` message through the tunnel.
4. Calls `relay::relay()` — body bytes and the server response flow through from here.

On any tunnel error, sends `HTTP/1.1 502 Bad Gateway\r\n\r\n` before closing.

### `relay.rs` — shared relay and connection counter

`CONN_COUNTER` — global `AtomicU32`, incremented by `next_conn_id()`.  
Each connection gets a unique `conn_id` used to demultiplex messages on the single SSE stream.

`relay(stream, conn_id, tunnel, event_rx)`:
- **Read task**: reads from TCP in 32 KB chunks → wraps in `Message::Data` → `tunnel.send_message()`.  
  On EOF, sends `Message::Close` and calls `tunnel.unregister_connection()`.
- **Write task**: receives `TunnelEvent` from `event_rx` → writes `Data` bytes to TCP.  
  Stops on `Close` or `Error`.
- `tokio::select!` on both tasks — whichever finishes first cancels the other.

### `tunnel.rs` — HTTP/SSE tunnel

Maintains a long-lived SSE connection (`GET /stream/{session_id}`) for server-to-client messages.

Sends client-to-server messages via `POST /send/{session_id}` with an encrypted binary body.

`send_connect()` — sends a `Connect` message and synchronously reads the **HTTP response body** as the ACK. This is distinct from the SSE stream; the ACK is the POST response, not an SSE event.

`register_connection(conn_id)` → returns an `mpsc::Receiver<TunnelEvent>`. The SSE reader dispatches to these receivers by `conn_id`.

---

## Server modules

### `server/src/server.rs` — HTTP/1.1 server

Three routes (all over plain HTTP/1.1 — TLS is handled by the reverse proxy / Cloud Shell):

| Route | Purpose |
|-------|---------|
| `GET /` or `GET /health` | Liveness check (always, even with a path_prefix configured) |
| `GET /[prefix]/stream/{session_id}` | Opens SSE stream; server pushes encrypted `TunnelEvent`s to client |
| `POST /[prefix]/send/{session_id}` | Receives encrypted message; for `Connect`, returns encrypted ACK as response body; for `Data`/`Close`, returns empty 200 |

`path_prefix` is configured in `[server] path_prefix = "/my-path"` and is stripped from incoming paths before routing. The bare `/health` always matches regardless of prefix, so load-balancer probes work without knowing the prefix.

The server decrypts every incoming body and encrypts every outgoing SSE event using the shared `Crypto` instance (ChaCha20-Poly1305, Argon2id key derivation).

---

## Encryption

`common/src/crypto.rs` — `Crypto` struct:
- Key derivation: Argon2id from the shared password + a fixed salt.
- Encryption: ChaCha20-Poly1305 with a random 12-byte nonce prepended to each ciphertext.
- Each `Message` is serialized with `Message::to_bytes()`, encrypted, then sent on the wire.

---

## Message protocol

`common/src/protocol.rs` — `Message` enum:

| Variant | Direction | Purpose |
|---------|-----------|---------|
| `Connect { conn_id, host, port }` | client → server | Open connection to target |
| `Data { conn_id, data }` | both | Raw payload bytes |
| `Close { conn_id }` | both | Connection closed |
| `Error { conn_id, message }` | both | Error notification |
| `Ping` / `Pong` | both | Keep-alive |

`conn_id` is a `u32` that demultiplexes many logical connections over the single SSE stream.
