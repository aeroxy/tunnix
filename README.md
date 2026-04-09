# tunnix

An encrypted SOCKS5/HTTP proxy tunnel over HTTP/SSE.

tunnix routes your SOCKS5 and HTTP(S) proxy traffic through a plain HTTP connection, end-to-end encrypted with ChaCha20-Poly1305. It is designed for environments that serve HTTP but block direct TCP — Cloud Shell, Codespaces, Gitpod, or any host behind a reverse proxy.

## Features

- **End-to-end encryption** — ChaCha20-Poly1305, Argon2id key derivation
- **HTTP/SSE transport** — no WebSocket required; works wherever plain HTTP works
- **Dual-protocol listener** — SOCKS5 and HTTP proxy on the same port, auto-detected
- **Connection multiplexing** — many connections share one SSE stream
- **Custom header injection** — for cookie-authenticated reverse proxies
- **Path prefix support** — serve under a sub-path (`/foo/bar/stream/...`) to coexist with other apps on the same host
- **Single binary** — `tunnix server` / `tunnix client`

## Quick Start

```bash
# Server
tunnix server --listen 0.0.0.0:8080 --password "your-secret"

# Client
tunnix client \
  --server https://your-host.example.com \
  --password "your-secret" \
  --local-addr 127.0.0.1:7890

# Test
curl -x http://127.0.0.1:7890 https://ifconfig.me
curl --socks5 127.0.0.1:7890 https://ifconfig.me
```

## Deployment Scenarios

### Google Cloud Shell

Cloud Shell's Web Preview issues a temporary HTTPS URL for your HTTP server. tunnix runs inside Cloud Shell and the client connects using the preview URL with Cloud Shell's authorization cookies.

**Server (inside Cloud Shell terminal):**
```bash
tunnix server --listen 0.0.0.0:8080 --password "your-secret"
```
Open Web Preview on port 8080 to get the preview URL.

**Get cookies:** Browser DevTools → Network tab → any request to `*.cloudshell.dev` → copy the `Cookie` header.

**Client (local machine):**
```bash
tunnix client \
  --server "https://8080-cs-XXXX.cs-region.cloudshell.dev" \
  --password "your-secret" \
  --cookie "CloudShellAuthorization=Bearer ...; CloudShellPartitionedAuthorization=Bearer ..."
```

### GitHub Codespaces

Codespaces exposes forwarded ports via a GitHub-authenticated HTTPS URL.

**Server (inside Codespace terminal):**
```bash
tunnix server --listen 0.0.0.0:8080 --password "your-secret"
```
In the Ports panel, set port 8080 visibility to **Public** (or pass a GitHub token).

**Client:**
```bash
tunnix client \
  --server "https://your-codespace-name-8080.app.github.dev" \
  --password "your-secret"
```

### Gitpod

Same pattern as Codespaces. Make the port public in the Gitpod ports UI.

**Client:**
```bash
tunnix client \
  --server "https://8080-your-workspace.ws-eu.gitpod.io" \
  --password "your-secret"
```

### Behind nginx (path prefix)

When tunnix shares a host with other services, use `path_prefix` to scope all its routes under a sub-path. nginx handles TLS; tunnix binds to a local port.

**`config.toml` (server):**
```toml
[server]
listen = "127.0.0.1:9000"
password = "your-secret"
path_prefix = "/tunnix"
```

**nginx snippet:**
```nginx
location /tunnix/ {
    proxy_pass http://127.0.0.1:9000;
    proxy_http_version 1.1;
    proxy_buffering off;
    proxy_cache off;
    proxy_set_header Connection "";
    proxy_set_header X-Accel-Buffering "no";
    proxy_read_timeout 3600s;
}
```

**Client:**
```bash
tunnix client --server "https://your-domain.com/tunnix" --password "your-secret"
```

> The bare `/health` endpoint always responds regardless of prefix, so load-balancer probes continue to work.

### Railway / Render / Fly.io

These platforms run long-lived processes and assign a public HTTPS URL. They set a `$PORT` environment variable.

**Dockerfile (minimal):**
```dockerfile
FROM debian:bookworm-slim
COPY tunnix /usr/local/bin/tunnix
CMD tunnix server --listen "0.0.0.0:$PORT" --password "$TUNNIX_PASSWORD"
```

Set `TUNNIX_PASSWORD` as an environment secret in the platform dashboard.

**Client:**
```bash
tunnix client \
  --server "https://your-app.railway.app" \
  --password "your-secret"
```

> **Vercel is not recommended.** Serverless functions have short execution timeouts (10–60 s depending on plan) that are incompatible with long-lived SSE streams. Use a container-based platform instead.

## Configuration

Copy `config.example.toml` to `config.toml` and customize:

```toml
[server]
listen = "0.0.0.0:8080"
password = "your-secret"
# path_prefix = "/tunnix"   # optional; leave empty for root

[client]
server_url = "https://your-host.example.com"
password = "your-secret"
local_addr = "127.0.0.1:7890"

[client.headers]
# Cookie = "..."   # only needed for cookie-authenticated hosts

[logging]
level = "info"
# file = "./tunnix.log"
```

Run with a config file:
```bash
tunnix server --config config.toml
tunnix client --config config.toml
```

CLI flags always override config file values. The password can also be supplied via the `TUNNIX_PASSWORD` environment variable.

## Building

```bash
cargo build --release
# Binary: target/release/tunnix
```

Cross-compile for Linux (requires [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)):
```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu
```

Or use `make`:
```bash
make release          # native
make release-linux    # Linux x86_64
make release-all      # both
```

## Architecture

```
Local SOCKS5/HTTP client
        │
        ▼
  tunnix client
  ├── proxy.rs       — TCP listener; detects protocol (0x05=SOCKS5, letter=HTTP)
  ├── socks5.rs      — SOCKS5 handshake (RFC 1928, CONNECT only)
  ├── http_proxy.rs  — HTTP CONNECT + plain HTTP forwarding
  ├── relay.rs       — bidirectional relay; connection ID counter
  └── tunnel.rs      — HTTP/SSE tunnel to server
          │
          │  POST /[prefix]/send/{session}    encrypted binary body
          │  GET  /[prefix]/stream/{session}  SSE text/event-stream
          ▼
  tunnix server
  └── server.rs      — hyper HTTP/1.1 server; session routing; prefix stripping
          │
          │  raw TCP
          ▼
  Target (e.g. api.example.com:443)
```

The client auto-detects the incoming protocol by peeking the first byte:
- `0x05` → SOCKS5
- ASCII letter → HTTP proxy (`CONNECT` for HTTPS, method for plain HTTP)

## Security

- Argon2id key derivation from the shared password
- ChaCha20-Poly1305 AEAD, per-message random nonce
- No plaintext payload logging
- Use a strong, randomly generated password — it is the only credential

## Use with Clash / ClashX

```yaml
proxies:
  - name: tunnix
    type: socks5      # or type: http
    server: 127.0.0.1
    port: 7890
```

## License

MIT
