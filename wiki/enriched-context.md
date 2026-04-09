# Enriched Context

Design decisions, constraints, and non-obvious facts about tunnix.

---

## Why HTTP/SSE instead of WebSocket

Cloud Shell Web Preview proxies HTTP traffic but strips or mangles WebSocket upgrade headers in some configurations. HTTP/SSE avoids that: the downstream (server→client) is a plain `text/event-stream` GET, and the upstream (client→server) is a series of POST requests. Both are standard HTTP/1.1, which Cloud Shell passes through reliably.

The README previously said "WebSocket tunnel" — that was aspirational documentation from an earlier design. The transport has always been HTTP/SSE in the actual implementation.

---

## Why dual-protocol on the same port

Many tools (system proxy settings, ClashX, curl via `http_proxy` env var) default to HTTP proxy. Others (older tools, some CLI utilities) prefer SOCKS5. Running both on one port means a single `local_addr` in config works for everything.

Protocol detection is zero-cost: a single `peek` of one byte. SOCKS5 always starts with `0x05`; HTTP always starts with an ASCII letter. There is no overlap.

---

## CONNECT ACK is in the POST response body, not SSE

When the client sends a `Connect` message via POST, the server makes the outbound TCP connection and returns the ACK (`Data { data: [] }` or `Error`) **as the HTTP response body of that same POST request**. The client calls `tunnel.send_connect()` which synchronously reads the response body and decrypts it.

This is intentional: it gives the client a synchronous acknowledgment without needing to coordinate a round-trip through the SSE stream. If you're debugging connect failures, check the POST response body, not the SSE channel.

---

## SSE reconnect loses in-flight connections

The SSE loop in `tunnel.rs` reconnects automatically on error. A new SSE connection creates a fresh session on the server (`sse_tx`/`sse_rx` pair). Any `conn_id`s registered against the old session lose their SSE pipe — `TunnelEvent` receivers will never see data again and will stall.

In practice this is acceptable because any active TCP connections through the proxy will also break when the SSE drops. The user's application reconnects, which creates new `conn_id`s registered against the new session.

---

## `protocol` field in config.toml was a dead stub

The old `config.toml` had `protocol = "socks5"` under `[client]`. It was never read by the code (`ClientConfig` struct had no `protocol` field). It has been removed. The client now always accepts both protocols on `local_addr`.

---

## Plain HTTP proxy rewrites the request line

For plain HTTP (non-CONNECT) requests, browsers send an absolute-form URI:
```
GET http://example.com/path HTTP/1.1
```

The target server expects origin-form:
```
GET /path HTTP/1.1
```

`http_proxy.rs` rewrites the first line before forwarding. Headers are passed through verbatim. This is standard HTTP/1.1 proxy behavior (RFC 7230 §5.3.2).

---

## Credentials in config.toml

`config.toml` contains real Cloud Shell JWT cookies. This file is gitignored (or should be). `config.example.toml` is the template with placeholder tokens — always edit the example when changing the config schema, never commit the live `config.toml`.

---

## Buffer sizes

The relay uses 32 KB read buffers (`relay.rs`). The SSE event channel per connection has a buffer of 256 messages (`tunnel.rs: mpsc::channel(256)`). These are not configurable at runtime; change them in code if throughput is a bottleneck.
