## Communication Style

- Explain the *why* (root cause, design intent) before proposing a fix.
- Be concise — one sentence is better than three.
- No trailing summaries; the diff speaks for itself.

## Code Preferences

- **Delete dead code** — don't suppress warnings with `#[allow(dead_code)]` or keep methods "for future use".
- **No speculative abstractions** — only add helpers when they're actually called in more than one place.
- **No backward-compat shims** — if something is removed, remove it cleanly.

## Config & CLI

- The single binary `tunnix` takes a subcommand: `tunnix server` or `tunnix client`.
- Both subcommands default to reading `config.toml` from the current working directory when no `--config` flag is provided; fall back to `Config::default()` if the file doesn't exist.
- CLI flags always override config file values. `TUNNIX_PASSWORD` env var sets the password.
- Required fields (e.g. password, server URL) are validated after merging config + CLI, so they can be satisfied by either source.
- `config.toml` is the live file with real credentials — never commit it. `config.example.toml` is the committed template; keep it in sync with any config schema changes.
- `path_prefix` is a server-side option (`[server] path_prefix = "/foo"`). The client embeds the same prefix in `server_url` — no separate client config field needed.

## Architecture Notes

- **Single crate**: `src/` contains all server and client code (crypto, protocol, config, server, client, dual-protocol listener).
- **Transport**: HTTP/SSE. Client uploads via `POST /[prefix]/send/{session_id}`, downloads via SSE on `GET /[prefix]/stream/{session_id}`.
- **Path prefix**: The server strips `path_prefix` from incoming paths before routing. Bare `/` and `/health` always match regardless of prefix (load-balancer probes). See `src/server.rs`.
- **CONNECT ACK**: The server returns the ACK as the HTTP response body of the POST to `/send/{session_id}`. The client must decrypt the response body using `send_connect()`.
- **Dual-protocol listener**: The client listens on one port for both SOCKS5 and HTTP proxy. Protocol is detected by peeking the first byte (`0x05` = SOCKS5, ASCII letter = HTTP). See `src/proxy.rs`.
- **Session lifecycle**: SSE reconnect replaces the session on the server (new `sse_tx`/`sse_rx`). In-flight connections from the old session lose their SSE pipe — handle reconnects carefully.
- **Multiplexing**: Multiple connections share one SSE stream, demuxed by `conn_id` (global `AtomicU32` in `src/relay.rs`).

## Development

When preparing a new release:
1. Bump the version in `Cargo.toml`.
2. Build the release binary: `make release-all`
3. Zip the binary inside the release folder: `zip -j target/release/tunnix_macos_arm64.zip target/release/tunnix && zip -j target/x86_64-unknown-linux-gnu/release/tunnix_linux_x86_64.zip target/x86_64-unknown-linux-gnu/release/tunnix`
4. Calculate the SHA256: `shasum -a 256 target/release/tunnix_macos_arm64.zip target/x86_64-unknown-linux-gnu/release/tunnix_linux_x86_64.zip`
5. Update `Formula/tunnix.rb` with the new version, URLs, and SHA256s.

## Wiki

- [Architecture](wiki/architecture.md) — module map, data flow, message protocol
- [Enriched Context](wiki/enriched-context.md) — design decisions, non-obvious constraints, gotchas
