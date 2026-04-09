# Quick Start Guide

## Prerequisites

- Rust 1.70+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Access to a server (e.g., Google Cloud Shell, Codespaces, or a VPS)

## Step 1: Build

```bash
cargo build --release
ls -lh target/release/tunnix
```

## Step 2: Setup Server (e.g. Cloud Shell)

```bash
tunnix server --listen 0.0.0.0:8080 --password "your-secret-password"
```

Or with a config file:
```bash
tunnix server --config config.toml
```

## Step 3: Get Cloud Shell Cookies

1. Open Cloud Shell Web Preview on port 8080
2. Open Browser DevTools → Network tab
3. Find requests to `8080-cs-*.cloudshell.dev`
4. Copy the `Cookie` header value containing:
   - `CloudShellAuthorization=Bearer ...`
   - `CloudShellPartitionedAuthorization=Bearer ...`

## Step 4: Run Client (Local Machine)

```bash
tunnix client \
  --server "https://8080-cs-XXXXXXXX.cs-asia-southeast1-bool.cloudshell.dev" \
  --password "your-secret-password" \
  --local-addr "127.0.0.1:7890" \
  --cookie "CloudShellAuthorization=Bearer ...; CloudShellPartitionedAuthorization=Bearer ..."
```

## Step 5: Test

The proxy accepts both SOCKS5 and HTTP on the same port:

```bash
# HTTP proxy (CONNECT for HTTPS)
curl -x http://127.0.0.1:7890 https://ifconfig.me

# HTTP proxy (plain HTTP)
curl -x http://127.0.0.1:7890 http://ifconfig.me

# SOCKS5
curl --socks5 127.0.0.1:7890 https://ifconfig.me
```

## Step 6: Use with ClashX

```yaml
proxies:
  - name: tunnix
    type: http        # works with type: socks5 too
    server: 127.0.0.1
    port: 7890

proxy-groups:
  - name: Proxy
    type: select
    proxies:
      - tunnix
```

## Using Config File (Recommended)

```bash
cp config.example.toml config.toml
# Edit with your settings
tunnix client --config config.toml
tunnix server --config config.toml
```

## Development

```bash
cargo test
cargo clippy
cargo fmt
cargo run --bin tunnix -- server --log-level debug
cargo run --bin tunnix -- client --log-level debug
```

## Troubleshooting

**Client can't connect:**
- Check server is running: `netstat -tlnp | grep 8080`
- Verify the server URL uses `https://`
- Check cookies haven't expired (refresh from browser)
- Passwords must match on both sides

**Encryption errors:**
- Ensure same password on client and server

**Environment variable:**
- Set `TUNNIX_PASSWORD=your-secret` to avoid passing `--password` on the command line

**Performance issues:**
- Check network latency: `ping <your-server-host>`
- Monitor CPU on both sides
