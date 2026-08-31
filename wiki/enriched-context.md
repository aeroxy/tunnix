# Enriched Context

Design decisions, constraints, and non-obvious facts about tunnix.

---

## Why HTTP/SSE instead of WebSocket

Cloud Shell Web Preview proxies HTTP traffic but strips or mangles WebSocket upgrade headers in some configurations. HTTP/SSE avoids that: the downstream (server→client) is a plain `text/event-stream` GET, and the upstream (client→server) is a series of POST requests. Cloud Shell can pass these ordinary HTTP exchanges without WebSocket support; the Rama client can negotiate HTTP/1.1 or HTTP/2 with an HTTPS reverse proxy.

HTTP/2 is a deliberate capability expansion in this migration. The old server explicitly used Hyper's HTTP/1 builder, while reqwest was built with default features disabled and without its `http2` feature. Rama's `default_http` TLS profile and auto server support both HTTP/1.1 and HTTP/2; SSE's forbidden connection-specific headers are filtered on HTTP/2.

The control-plane URL intentionally accepts both `http://` and `https://`: direct and loopback deployments can reach tunnix's plaintext server, while public deployments normally use HTTPS at Cloud Shell or a reverse proxy that terminates TLS. This is independent of the proxied target protocol—the local SOCKS5/HTTP listener can carry arbitrary TCP, including end-to-end target TLS, inside the encrypted tunnix envelope.

Plain HTTP proxy requests are temporarily unpooled on Rama 0.4 because the pooled custom-transport stack overflowed Tokio's default debug worker stack. [Upstream change #1141](https://github.com/plabayo/rama/pull/1141) fixes separate pool reuse and waiter correctness problems in Rama 0.5 development; it does not establish that the stack overflow is fixed. Reproduce that failure on 0.5 before restoring and load-testing pooling.

The client uses a typed SSE data reader that decodes encrypted base64 frames directly into bytes. [Upstream change #1140](https://github.com/plabayo/rama/pull/1140) optimizes Rama's core SSE decoder in 0.5 development, but application-specific typed decoding still avoids a temporary `String`.

TCP relays preserve half-close semantics in both directions. Local input EOF shuts down only target input and keeps delivering the target response. Target output EOF sends a clean FIN to the local application while keeping client input registered and forwarding until the application sends its own directional `Close`. Only an actual forwarding failure switches to discarded-input cleanup, bounded to five seconds so a broken relay cannot linger indefinitely or immediately turn an already-delivered FIN into RST.

SSE reconnection preserves live target writers, but the current tunnel protocol has no sequence numbers or delivery acknowledgements. A frame accepted into an old server-side SSE queue can therefore be lost if that response is replaced before the frame reaches the client; retrying a later terminal message on the new queue cannot prove continuity. Lossless connection preservation across SSE epochs requires a protocol follow-up with sequencing, acknowledgement, replay, and client-side deduplication. Until then, reconnect preservation is best-effort rather than a wire-fidelity guarantee.

The README previously said "WebSocket tunnel" — that was aspirational documentation from an earlier design. The transport has always been HTTP/SSE in the actual implementation.

---

## Why dual-protocol on the same port

Many tools (system proxy settings, ClashX, curl via `http_proxy` env var) default to HTTP proxy. Others (older tools, some CLI utilities) prefer SOCKS5. Running both on one port means a single `local_addr` in config works for everything.

Rama's `Socks5PeekRouter` peeks and validates the SOCKS5 greeting before falling back to the HTTP server. The peeked bytes are replayed to the selected service, so neither protocol parser loses input.

---

## CONNECT ACK is in the POST response body, not SSE

When the client sends a `Connect` message via POST, the server makes the outbound TCP connection and returns the ACK (`Data { data: [] }` or `Error`) **as the HTTP response body of that same POST request**. The client calls `tunnel.send_connect()` which synchronously reads the response body and decrypts it.

This is intentional: it gives the client a synchronous acknowledgment without needing to coordinate a round-trip through the SSE stream. If you're debugging connect failures, check the POST response body, not the SSE channel.

---

## SSE reconnect preserves the server-side session

The SSE loop in `tunnel.rs` reconnects automatically on error. Reconnecting with the same session ID replaces only the session's `sse_tx`; existing TCP relay tasks look up that sender for every message and continue on the new stream.

If the server has restarted and no longer knows the session, the new stream sends `Reset`. The client then clears its response channels so orphaned local relays close instead of stalling.

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

Rama parses the absolute-form target and adapts it to the origin connection. It also applies the negotiated HTTP version and removes hop-by-hop headers, avoiding the casing, ordering, IPv6-authority, and framing errors possible with the old hand-written parser.

---

## Credentials in config.toml

`config.toml` contains real Cloud Shell JWT cookies. This file is gitignored (or should be). `config.example.toml` is the template with placeholder tokens — always edit the example when changing the config schema, never commit the live `config.toml`.

---

## Buffer sizes

The relay uses 32 KB read buffers (`relay.rs`). The SSE event channel per connection has a buffer of 256 messages (`tunnel.rs: mpsc::channel(256)`). These are not configurable at runtime; change them in code if throughput is a bottleneck.

---

## `remote-exec` allocates a PTY in canonical mode

`remote-exec` always allocates a pseudo-terminal (PTY) for the child process, with the slave PTY left in its default canonical (line-buffered) mode. This is the right shape for interactive use (`vim`, `bash`, top) but has two consequences for non-interactive use:

*   **Piped stdin ending without a trailing newline gets one injected before EOF.** `printf abc | tunnix remote-exec sha256sum` hashes `"abc\n"`, not `"abc"`. The PTY's terminal driver delivers line-buffered data only on `\n` or VEOF; VEOF on a non-empty buffer flushes it but does *not* signal EOF. To get a real EOF the driver must first see the line terminator, so `exec.rs` synthesizes a `'\n'` before the `\x04` it sends on local EOF. The output the child sees is exactly what it would see if you had typed `abc<Enter><Ctrl-D>` interactively — but it is *not* byte-identical to the bytes the caller sent on the pipe.
*   **Binary data through `remote-exec` is not safe.** Canonical mode also drops/parities certain bytes (^C, ^Z, ^\). For byte-fidelity use, run the command locally and tunnel only its TCP traffic through tunnix; do not pipe binary into `remote-exec`.

If a non-PTY / non-canonical mode is needed, that's a separate feature (`--no-pty` or similar), not a bug to fix in the current PTY path.

---

## Intentional security trade-offs

These are deliberate, operator-driven decisions. Do not "fix" them in a security-pass without coordination.

*   **`ServerVerifyMode::Disable` in `reload.rs::build_http_client`** — retained from the former reqwest client's `danger_accept_invalid_certs(true)` for compatibility with TLS-inspecting deployments. This is not a desirable permanent default: the follow-up is an explicit verification policy with normal system roots, custom CA roots, and an opt-in insecure mode.
*   **`tunnix server --allow-exec`** — opt-in only, default `false`, and the server prints a loud warning at startup when it's on. Anyone holding the server password can run a shell on the box. This feature is designed to give remote user GOD MODE to the server.
