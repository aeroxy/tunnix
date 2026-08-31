use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arc_swap::ArcSwap;
use rama::{
    error::extra::OpaqueError,
    http::{
        client::{EasyHttpWebClient, HttpPooledConnectorConfig},
        HeaderMap, Request, Response,
    },
    layer::MapErrLayer,
    net::client::{NoProxyEnvLayer, ProxyEnvLayer, ProxyRoutesLayer},
    net::uri::Uri,
    rt::Executor,
    service::BoxService,
    telemetry::tracing::{info, warn},
    tls::client::{ServerVerifyMode, TlsClientConfig},
    Layer, Service,
};

use crate::config::{normalize_path_prefix, normalize_server_url, Config};
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
    pub path_prefix: Option<Uri>,
    pub root_redirect: Option<Uri>,
    pub root_html: Option<String>,
    pub health_body: String,
    pub allow_exec: bool,
    pub allow_transfer: bool,
}

pub struct HotClientConfig {
    pub crypto: Arc<Crypto>,
    pub http_client: HttpClient,
    pub http_headers: HeaderMap,
    pub server_base_url: Uri,
}

pub type HttpClient = BoxService<Request, Response, OpaqueError>;

const CONTROL_PLANE_POOL_MAX_CONNECTIONS: usize = 1024;
const CONTROL_PLANE_POOL_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

pub fn build_http_client(exec: Executor) -> HttpClient {
    // Compatibility with the previous reqwest client, which used
    // danger_accept_invalid_certs(true) for TLS-inspecting deployments.
    // TODO: make verification the secure default and expose explicit system,
    // custom-CA, and insecure modes instead of disabling it globally.
    let tls = TlsClientConfig::default_http().with_server_verify(ServerVerifyMode::Disable);
    let client = EasyHttpWebClient::connector_builder()
        .with_default_transport_connector()
        .with_default_dns_connector()
        .with_tls_proxy_support_using_rustls()
        .with_proxy_support()
        .with_tls_support_using_rustls(tls)
        .with_default_http_connector(exec)
        // max_total counts active plus idle connections; idle entries are
        // evicted first, but the default of 50 can still block when all 50 are
        // active (the long-lived HTTP/1 SSE response occupies one). Keep the
        // wait within the tunnel's reconnect horizon. Rama 0.5-dev #1141
        // improves pool reuse and waiter wakeups further.
        .try_with_connection_pool(HttpPooledConnectorConfig {
            max_total: CONTROL_PLANE_POOL_MAX_CONNECTIONS,
            wait_for_pool_timeout: Some(CONTROL_PLANE_POOL_WAIT_TIMEOUT),
            ..Default::default()
        })
        .expect("static HTTP pool configuration is valid")
        .build_client();

    let client = (
        NoProxyEnvLayer::default(),
        ProxyEnvLayer::default(),
        ProxyRoutesLayer::new(),
    )
        .into_layer(client);
    MapErrLayer::into_opaque_error().into_layer(client).boxed()
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
                    warn!(
                        "Config file not accessible: {}; keeping current config",
                        path
                    );
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

        let path_prefix = match normalize_path_prefix(sc.path_prefix.as_ref()) {
            Ok(prefix) => prefix,
            Err(error) => {
                warn!(%error, "invalid path prefix; keeping current config");
                current.path_prefix.clone()
            }
        };
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
    response_channels: Arc<
        tokio::sync::Mutex<HashMap<u32, tokio::sync::mpsc::Sender<crate::tunnel::TunnelEvent>>>,
    >,
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
                    warn!(
                        "Config file not accessible: {}; keeping current config",
                        path
                    );
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

        let headers_match = cc
            .headers
            .ordered_iter()
            .map(|(name, value)| (name.as_original_str(), value))
            .eq(current
                .http_headers
                .ordered_iter()
                .map(|(name, value)| (name.as_original_str(), value)));
        let new_headers = if !overrides.client_headers && !headers_match {
            changed.push("headers");
            needs_reconnect = true;
            Some(cc.headers.clone())
        } else {
            None
        };

        let new_server_url = match cc.server_url.as_ref() {
            Some(url) => match normalize_server_url(url) {
                Ok(url) if url != current.server_base_url => {
                    changed.push("server_url");
                    needs_reconnect = true;
                    Some(url)
                }
                Ok(_) => None,
                Err(e) => {
                    warn!("Invalid server URL: {}; keeping current config", e);
                    None
                }
            },
            None => {
                warn!("Reloaded config has no server URL; keeping current config");
                None
            }
        };

        if changed.is_empty() {
            continue;
        }

        last_password = cc.password.clone();

        let crypto = new_crypto.unwrap_or_else(|| current.crypto.clone());
        let http_headers = new_headers.unwrap_or_else(|| current.http_headers.clone());
        let server_base_url = new_server_url.unwrap_or_else(|| current.server_base_url.clone());

        hot.store(Arc::new(HotClientConfig {
            crypto,
            http_client: current.http_client.clone(),
            http_headers,
            server_base_url,
        }));
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
