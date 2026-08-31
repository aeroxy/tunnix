# Hot Reload

Runtime config reload without process restart.

---

## How it works

A background task polls `config.toml` mtime every 3 seconds. On change, it waits 200ms (debounce for editors that truncate-then-write), re-parses the file, and swaps the affected fields atomically via `ArcSwap`. If the parse fails, it retries once after 500ms, then skips that cycle — the process continues with the last good config.

The hot config is split into two structs behind `ArcSwap`:

- **Server**: `HotServerConfig` — `crypto`, `path_prefix`, `root_redirect`, `root_html`, `health_body`
- **Client**: `HotClientConfig` — `crypto`, Rama HTTP client, headers, typed server URI

Handlers load a snapshot (`ArcSwap::load()`) at the top of each request. A config swap mid-request is invisible — the request finishes with the snapshot it started with.

---

## What's hot-reloadable

| Field | Server | Client | Notes |
|-------|--------|--------|-------|
| `password` | Yes | Yes | Argon2id derivation runs in `spawn_blocking` |
| `headers` | — | Yes | Replaces the validated Rama `HeaderMap` and reconnects |
| `server_url` | — | Yes | Replaces the typed Rama URI and reconnects |
| `path_prefix` | Yes | — | |
| `root_redirect` | Yes | — | |
| `root_html` | Yes | — | |
| `health_response` | Yes | — | |

## What requires a restart

| Field | Why |
|-------|-----|
| `server.listen` | TCP listener is already bound |
| `client.local_addr` | SOCKS5/HTTP listener is already bound |
| `logging.level` | Tracing subscriber is initialized once at startup |

---

## CLI override protection

`CliOverrides` tracks which fields were set via CLI flags (`--password`, `--cookie`, etc.). The config watcher skips those fields on reload — CLI always wins, even across multiple file changes.

---

## Password change behavior

**Server**: New `Crypto` instance is derived in `spawn_blocking` and swapped in. In-flight relay connections (`relay_tcp_connection`) keep their captured `Arc<Crypto>` until they close. New connections get the updated crypto.

**Client**: New crypto is swapped in, then the watcher generates a new session ID, clears response channels, and fires `reconnect_signal`. The SSE loop reconnects with the new crypto and http_client. Both sides must update the password — during the transition window, connections encrypted with the old key will fail to decrypt on the updated side.

---

## Header change behavior (client only)

Headers deserialize directly into Rama's typed `HeaderMap` from an ordered list of `[name, value]` pairs. Repeated field lines, insertion order, and original name casing are retained; any ordered change swaps the map and reconnects the SSE loop.

---

## Error handling

| Scenario | Behavior |
|----------|----------|
| TOML parse error | Warning logged with "keeping current config", retry once after 500ms, skip if still failing |
| Crypto derivation failure | Warning logged, keep old crypto |
| Config file deleted | One-shot warning: "Config file not accessible: ...; keeping current config". Silently polls until the file reappears, then logs "Config file reappeared" and resumes |
| Permission denied | Same as deleted |
| Empty password in file | Skipped (validated as non-empty before deriving) |

---

## Key files

- `src/reload.rs` — `CliOverrides`, `HotServerConfig`, `HotClientConfig`, watcher loops, `build_http_client`
- `src/server.rs` — `ServerState.hot: Arc<ArcSwap<HotServerConfig>>`
- `src/tunnel.rs` — `Tunnel.hot: Arc<ArcSwap<HotClientConfig>>`
