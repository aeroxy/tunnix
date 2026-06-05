use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use tracing::{info, warn};

use crate::config::Config;
use crate::crypto::Crypto;

pub struct CliOverrides {
    pub server_password: bool,
    pub server_allow_exec: bool,
    pub server_allow_transfer: bool,
    pub client_password: bool,
    pub client_headers: bool,
}

pub struct HotServerConfig {
    pub crypto: Arc<Crypto>,
    pub path_prefix: String,
    pub root_redirect: Option<String>,
    pub root_html: Option<String>,
    pub health_body: String,
    pub allow_exec: bool,
    pub allow_transfer: bool,
}

pub struct HotClientConfig {
    pub crypto: Arc<Crypto>,
    pub http_client: reqwest::Client,
    pub server_base_url: String,
}

pub fn build_http_client(headers: &HashMap<String, String>) -> anyhow::Result<reqwest::Client> {
    let mut default_headers = reqwest::header::HeaderMap::new();
    for (key, value) in headers {
        default_headers.insert(
            reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
            reqwest::header::HeaderValue::from_str(value)?,
        );
    }
    Ok(reqwest::Client::builder()
        .default_headers(default_headers)
        // Intentional: deployments behind a corporate TLS-inspecting proxy
        // (MITM CA) rely on this so the client trusts the proxy's re-signed
        // cert. Do NOT remove without coordinating with operators using such
        // proxies — they would otherwise fail every TLS handshake.
        .danger_accept_invalid_certs(true)
        .build()?)
}

pub async fn config_watcher_server(
    path: String,
    hot: Arc<ArcSwap<HotServerConfig>>,
    overrides: Arc<CliOverrides>,
) {
    let mut last_mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut last_password = String::new();
    let mut file_missing = false;

    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;

        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => {
                if file_missing {
                    info!("Config file reappeared: {}", path);
                    file_missing = false;
                }
                t
            }
            Err(_) => {
                if !file_missing {
                    warn!("Config file not accessible: {}; keeping current config", path);
                    file_missing = true;
                }
                continue;
            }
        };
        if mtime == last_mtime {
            continue;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        let new_config = match Config::from_file(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Config reload failed: {}; keeping current config", e);
                tokio::time::sleep(Duration::from_millis(500)).await;
                match Config::from_file(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Config reload retry failed: {}; keeping current config", e);
                        continue;
                    }
                }
            }
        };

        last_mtime = mtime;
        let current = hot.load();
        let sc = &new_config.server;
        let mut changed = Vec::new();

        let new_crypto = if !overrides.server_password
            && !sc.password.is_empty()
            && sc.password != last_password
        {
            let pw = sc.password.clone();
            match tokio::task::spawn_blocking(move || Crypto::new(&pw)).await {
                Ok(Ok(c)) => {
                    changed.push("password");
                    Some(Arc::new(c))
                }
                Ok(Err(e)) => {
                    warn!("Crypto derivation failed: {}", e);
                    None
                }
                Err(e) => {
                    warn!("Crypto task panicked: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let crypto = new_crypto.unwrap_or_else(|| current.crypto.clone());

        let path_prefix = sc.path_prefix.trim_end_matches('/').to_string();
        if path_prefix != current.path_prefix {
            changed.push("path_prefix");
        }
        if sc.root_redirect != current.root_redirect {
            changed.push("root_redirect");
        }
        if sc.root_html != current.root_html {
            changed.push("root_html");
        }
        let health_body = sc.health_response.clone();
        if health_body != current.health_body {
            changed.push("health_body");
        }
        // A CLI --allow-exec wins over the file, just like --password.
        let allow_exec = if overrides.server_allow_exec {
            current.allow_exec
        } else {
            sc.allow_exec
        };
        if allow_exec != current.allow_exec {
            changed.push("allow_exec");
        }
        // A CLI --allow-transfer wins over the file, just like --allow-exec.
        let allow_transfer = if overrides.server_allow_transfer {
            current.allow_transfer
        } else {
            sc.allow_transfer
        };
        if allow_transfer != current.allow_transfer {
            changed.push("allow_transfer");
        }

        if changed.is_empty() {
            continue;
        }

        last_password = sc.password.clone();
        hot.store(Arc::new(HotServerConfig {
            crypto,
            path_prefix,
            root_redirect: sc.root_redirect.clone(),
            root_html: sc.root_html.clone(),
            health_body,
            allow_exec,
            allow_transfer,
        }));

        info!("Config reloaded: {}", changed.join(", "));
    }
}

pub async fn config_watcher_client(
    path: String,
    hot: Arc<ArcSwap<HotClientConfig>>,
    reconnect_signal: Arc<tokio::sync::Notify>,
    session_id: Arc<tokio::sync::RwLock<String>>,
    response_channels: Arc<tokio::sync::Mutex<HashMap<u32, tokio::sync::mpsc::Sender<crate::tunnel::TunnelEvent>>>>,
    overrides: Arc<CliOverrides>,
) {
    let mut last_mtime = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut last_password = String::new();
    let mut last_headers: HashMap<String, String> = HashMap::new();
    let mut file_missing = false;

    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;

        let mtime = match std::fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(t) => {
                if file_missing {
                    info!("Config file reappeared: {}", path);
                    file_missing = false;
                }
                t
            }
            Err(_) => {
                if !file_missing {
                    warn!("Config file not accessible: {}; keeping current config", path);
                    file_missing = true;
                }
                continue;
            }
        };
        if mtime == last_mtime {
            continue;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;

        let new_config = match Config::from_file(&path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Config reload failed: {}; keeping current config", e);
                tokio::time::sleep(Duration::from_millis(500)).await;
                match Config::from_file(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Config reload retry failed: {}; keeping current config", e);
                        continue;
                    }
                }
            }
        };

        last_mtime = mtime;
        let current = hot.load();
        let cc = &new_config.client;
        let mut changed = Vec::new();
        let mut needs_reconnect = false;

        let new_crypto = if !overrides.client_password
            && !cc.password.is_empty()
            && cc.password != last_password
        {
            let pw = cc.password.clone();
            match tokio::task::spawn_blocking(move || Crypto::new(&pw)).await {
                Ok(Ok(c)) => {
                    changed.push("password");
                    needs_reconnect = true;
                    Some(Arc::new(c))
                }
                Ok(Err(e)) => {
                    warn!("Crypto derivation failed: {}", e);
                    None
                }
                Err(e) => {
                    warn!("Crypto task panicked: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let new_client = if !overrides.client_headers && cc.headers != last_headers {
            match build_http_client(&cc.headers) {
                Ok(c) => {
                    changed.push("headers");
                    needs_reconnect = true;
                    Some(c)
                }
                Err(e) => {
                    warn!("Failed to build HTTP client: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let new_server_url = cc.server_url.trim_end_matches('/').to_string();
        if new_server_url != current.server_base_url {
            changed.push("server_url");
            needs_reconnect = true;
        }

        if changed.is_empty() {
            continue;
        }

        last_password = cc.password.clone();
        last_headers = cc.headers.clone();

        let crypto = new_crypto.unwrap_or_else(|| current.crypto.clone());
        let http_client = new_client.unwrap_or_else(|| current.http_client.clone());
        let server_base_url = if new_server_url != current.server_base_url {
            new_server_url
        } else {
            current.server_base_url.clone()
        };

        hot.store(Arc::new(HotClientConfig { crypto, http_client, server_base_url }));
        info!("Config reloaded: {}", changed.join(", "));

        if needs_reconnect {
            let mut sid = session_id.write().await;
            *sid = format!("{:016x}", rand::random::<u64>());
            drop(sid);
            response_channels.lock().await.clear();
            reconnect_signal.notify_one();
            info!("Reconnecting with updated config");
        }
    }
}
