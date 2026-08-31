# Code Architecture

## Overview

```
Local SOCKS5/HTTP client
        │
        ▼
tunnix client  (tunnix client subcommand)
  ├── proxy.rs       — Rama SOCKS5/HTTP listener and proxy services
  ├── tunnel_connector.rs — Rama ConnectRequest → encrypted envelope
  ├── relay.rs       — bidirectional data relay, conn_id counter
  ├── exec.rs        — `remote-exec` client: raw terminal, SIGWINCH, PTY stream (Unix)
  └── tunnel.rs      — HTTP/SSE tunnel to server

        │  HTTP POST /[prefix]/send/{session_id}   (client → server, encrypted)
        │  GET /[prefix]/stream/{session_id}  SSE  (server → client, encrypted)
        ▼

tunnix server  (tunnix server subcommand)
  └── server.rs      — Rama HTTP server, per-session routing, prefix stripping

        │  raw TCP
        ▼

Target service (e.g. api.example.com:443)
```

---

## Client modules

### `proxy.rs` — Rama proxy stack

`run_proxy(listen_addr, tunnel)` binds a Rama TCP listener at a typed `SocketAddress`. A generic replaying `PeekRouter` recognizes the SOCKS5 version byte and delegates full greeting validation to `Socks5Acceptor`; all other traffic falls back to Rama's auto HTTP server. The generic router is a Rama 0.4 workaround for `Socks5PeekRouter` interpreting the greeting's `NMETHODS` count as a method ID.

The SOCKS5 acceptor supports unauthenticated CONNECT for IPv4, IPv6, and domain targets. The HTTP service supports both CONNECT upgrades and plain proxy requests. Rama owns HTTP parsing, request-target adaptation, version negotiation, and hop-by-hop header removal, preserving protocol semantics that the former hand-written parser could not represent reliably.

Plain HTTP requests use an unpooled Rama client over `TunnelConnector`; the unpooled shape avoids a large debug-build future stack while each incoming request still gets full HTTP version adaptation. CONNECT and SOCKS5 use Rama's `IoForwardService` after the same connector succeeds.

### `tunnel_connector.rs` — Rama connection adapter

`TunnelConnector` implements `Service<ConnectRequest>`. For each typed Rama authority it:

1. Allocates a `conn_id` and registers its event receiver.
2. Sends the existing encrypted `Message::Connect` and waits for the POST-body ACK.
3. Creates an in-memory duplex stream after a successful ACK.
4. Returns one side to Rama and runs the existing `relay` on the other.

This boundary deliberately keeps the custom encrypted multiplexing protocol independent of Rama's proxy protocols.

### `relay.rs` — shared relay and connection counter

`CONN_COUNTER` — global `AtomicU32`, incremented by `next_conn_id()`.  
Each connection gets a unique `conn_id` used to demultiplex messages on the single SSE stream.

`relay(stream, conn_id, tunnel, event_rx)`:
- **Read direction**: reads local TCP in 32 KiB chunks → wraps them in `Message::Data` → `tunnel.send_message()`. Local EOF sends a directional `Message::Close` while the response direction keeps draining.
- **Write direction**: receives `TunnelEvent` from `event_rx` → writes `Data` bytes to local TCP. A clean remote `Close` shuts down only the local write half and keeps forwarding local input; an `Error` terminates the relay.
- Failed forwarding uses a bounded input linger to avoid turning an already-delivered FIN into an immediate RST; terminal `Close` delivery is bounded as well, and Rama's graceful executor cancels the relay during client shutdown.

### `tunnel.rs` — HTTP/SSE tunnel

Maintains a long-lived SSE connection (`GET /stream/{session_id}`) for server-to-client messages. Rama's SSE event stream handles framing and comments; the loop uses a `tokio::select!` to interleave parsed events with a `reconnect_signal` (`Notify`), allowing `send_message` to force an immediate reconnect when a POST fails rather than waiting for the underlying TCP read to time out.

`session_id` is stored in an `RwLock<String>` (not a plain `String`) so concurrent callers can read it without contention, and the hot-reload watcher can rotate it atomically on a password/header change. It is deliberately **not** rotated when the server reports an unknown session (HTTP 503): the server (re)creates a session on `GET /stream/{sid}` and announces freshness with `Reset`, so recovery only needs a forced SSE reconnect. Keeping the ID stable lets concurrent send failures converge on one reconnect instead of racing to rotate (the old "death spiral"), and preserves in-flight server relays when the server didn't actually restart.

`send_message` — tries `try_post` once; on any failure it signals `reconnect_signal`, waits up to `RECONNECT_WAIT` for `sse_ready` (fired by the SSE loop on each fresh connection), then retries `try_post` once more.

Establishing the SSE `GET` is bounded by a 15s timeout (`SSE_CONNECT_TIMEOUT`): the http_client has no global timeout (it would kill the streaming body), and a half-open pooled connection would otherwise wedge `send().await` forever — the reconnect task is the only one, so the whole tunnel would die silently. After each successful (re)connect, any `reconnect_signal` permit buffered during connection setup is drained so it can't tear down the freshly established stream.

`send_connect()` — sends a `Connect` message and synchronously reads the **HTTP response body** as the ACK. This is distinct from the SSE stream; the ACK is the POST response, not an SSE event.

`register_connection(conn_id)` → returns an `mpsc::Receiver<TunnelEvent>`. The SSE reader dispatches to these receivers by `conn_id`.

---

## Server modules

### `src/server.rs` — Rama HTTP server

`ServerService` performs only the hot-reloadable typed path-prefix rewrite (while leaving bare `/` and `/health` untouched), then delegates method/path matching and typed `session_id` extraction to Rama's `Router`. Four routes are served through Rama's auto HTTP server; TLS is handled by the reverse proxy / Cloud Shell:

| Route | Purpose |
|-------|---------|
| `GET /` or `GET /health` | Liveness check (always, even with a path_prefix configured) |
| `GET /[prefix]/stream/{session_id}` | Opens SSE stream; server pushes encrypted `TunnelEvent`s to client |
| `POST /[prefix]/send/{session_id}` | Receives encrypted message; for `Connect`, returns encrypted ACK as response body; for `Data`/`Close`, returns empty 200. Returns `503 Service Unavailable` with body `"unknown session"` if the session is not found. |

`path_prefix` is deserialized as a Rama URI path from `[server] path_prefix = "/my-path"` and stripped with `Uri::path_mut()` before routing. Bare `/` and `/health` always match regardless of prefix, so load-balancer probes work without knowing the prefix.

Rama 0.4's router performs case-insensitive, once-percent-decoded path matching, which is broader than the legacy raw-path matcher. This temporary dependency constraint and its strict-policy follow-up are recorded in [Enriched Context](enriched-context.md).

**Session lifecycle** — `handle_stream` uses `entry().or_insert_with()` to create-or-reuse a `Session` keyed by `session_id`, then always overwrites `sse_tx` with the fresh channel. This preserves `tcp_writers` (active TCP relay tasks) across SSE reconnections. Each `relay_tcp_connection` read task fetches `sse_tx` from the session on every send rather than capturing it at spawn time, so existing relays automatically start writing to the new SSE channel after a client reconnect.

The server decrypts every incoming body and encrypts every outgoing SSE event using the shared `Crypto` instance (ChaCha20-Poly1305, Argon2id key derivation).

Both daemon listeners use Rama's graceful executor. A shutdown signal stops new accepts, closes the SSE stream, cancels long-lived tunnel/PTY/transfer work through a shared guard, and allows up to 30 seconds for tracked tasks to drain.

**PTY relay (`Exec`, Unix only)** — when `allow_exec` is set, an `Exec` message makes the server allocate a pseudo-terminal via `portable-pty`, spawn the command (or `$SHELL`) against the slave side, and relay the master FD over the same `Data`/`Close` path as a TCP connection. The blocking PTY FD is wrapped in an `AsyncFd` so reads don't block the runtime. `Resize` applies a new size to the live PTY; the child's exit code is returned as `ExitStatus` just before `Close`. A watchdog kills the orphaned child if the SSE stream stays disconnected past a few seconds. When `allow_exec` is false (the default) the server rejects `Exec` with an `Error`. See `handle_send` / `relay_pty_connection` in `src/server.rs`.

---

## Encryption

`src/crypto.rs` — `Crypto` struct:
- Key derivation: Argon2id from the shared password + a fixed salt.
- Encryption: ChaCha20-Poly1305 with a 12-byte nonce (an 8-byte monotonic counter plus 4 random bytes) prepended to each ciphertext.
- Each `Message` is serialized with `Message::to_bytes()`, encrypted, then sent on the wire.

---

## Message protocol

`src/protocol.rs` — `Message` enum:

| Variant | Direction | Purpose |
|---------|-----------|---------|
| `Connect { conn_id, host, port }` | client → server | Open connection to target |
| `Data { conn_id, data }` | both | Raw payload bytes |
| `Close { conn_id }` | both | Directional byte-stream EOF for TCP relays; terminal close for exec/transfer sessions |
| `Error { conn_id, message }` | both | Error notification |
| `Ping` / `Pong` | both | Keep-alive |
| `Exec { conn_id, cmd, cols, rows, term }` | client → server | Open a PTY for `conn_id` and run `cmd` (`None` = interactive `$SHELL`/`/bin/sh`); `cols`/`rows`/`term` seed the PTY size and type. Requires `allow_exec` on the server (Unix only). |
| `Resize { conn_id, cols, rows }` | client → server | Client terminal resized (SIGWINCH); server applies the new size to the PTY |
| `ExitStatus { conn_id, code }` | server → client | Child process exit code, sent just before `Close` |

`conn_id` is a `u32` that demultiplexes many logical connections over the single SSE stream. An `Exec` connection reuses the same `Data`/`Close` flow as a TCP connection: after the PTY is open, its byte stream rides over `Data` messages exactly like proxied traffic.
