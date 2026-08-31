use anyhow::Result;
use rama::{
    http::HeaderMap,
    net::{address::SocketAddress, uri::Uri},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub client: ClientConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    /// HTTP server socket address.
    #[serde(default = "default_listen_addr")]
    pub listen: SocketAddress,

    /// Password for encryption/authentication
    #[serde(default)]
    pub password: String,

    /// Maximum concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout: u64,

    /// URI path prefix for all routes (e.g. "/tunnix").
    #[serde(default)]
    pub path_prefix: Option<Uri>,

    /// 301 redirect for GET / (and GET /{prefix}). Overrides root_html if both are set.
    #[serde(default)]
    pub root_redirect: Option<Uri>,

    /// Local HTML file to serve at GET / (and GET /{prefix}).
    #[serde(default)]
    pub root_html: Option<String>,

    /// Response body for GET /health (default: "ok").
    #[serde(default = "default_health_response")]
    pub health_response: String,

    /// Allow remote command execution (`tunnix remote-exec`). This exposes an
    /// interactive shell on this machine to anyone holding the password — i.e.
    /// remote code execution. Disabled by default.
    #[serde(default)]
    pub allow_exec: bool,

    /// Allow file transfer (`tunnix push` / `tunnix pull`). This lets anyone
    /// holding the password read and write arbitrary files on this machine.
    /// Disabled by default.
    #[serde(default)]
    pub allow_transfer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientConfig {
    /// HTTP server URL (e.g., "https://example.com")
    #[serde(default)]
    pub server_url: Option<Uri>,

    /// Password for encryption/authentication
    #[serde(default)]
    pub password: String,

    /// Local SOCKS5/HTTP proxy socket address.
    #[serde(default = "default_local_addr")]
    pub local_addr: SocketAddress,

    /// Custom headers for HTTP/SSE requests, preserving field-line order and repeats.
    #[serde(default)]
    pub headers: HeaderMap,

    /// Reconnect interval in seconds
    #[serde(default = "default_reconnect_interval")]
    pub reconnect_interval: u64,

    /// Expected response body from server /health (default: "ok"). Connection fails if mismatch.
    #[serde(default = "default_health_expected")]
    pub health_expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoggingConfig {
    /// Log level: trace, debug, info, warn, error
    #[serde(default = "default_log_level")]
    pub level: String,
}

// Default values
fn default_listen_addr() -> SocketAddress {
    SocketAddress::default_ipv4(8080)
}

fn default_local_addr() -> SocketAddress {
    SocketAddress::local_ipv4(7890)
}

fn default_max_connections() -> usize {
    1000
}

fn default_timeout() -> u64 {
    300 // 5 minutes
}

fn default_reconnect_interval() -> u64 {
    5
}

fn default_health_response() -> String {
    "ok".to_string()
}

fn default_health_expected() -> String {
    "ok".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen_addr(),
            password: String::new(),
            max_connections: default_max_connections(),
            timeout: default_timeout(),
            path_prefix: None,
            root_redirect: None,
            root_html: None,
            health_response: default_health_response(),
            allow_exec: false,
            allow_transfer: false,
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            password: String::new(),
            local_addr: default_local_addr(),
            headers: HeaderMap::new(),
            reconnect_interval: default_reconnect_interval(),
            health_expected: default_health_expected(),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

impl Config {
    /// Load config from TOML file
    pub fn from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

pub fn normalize_server_url(uri: &Uri) -> Result<Uri> {
    match uri.scheme() {
        Some(scheme) if scheme.is_http() => {}
        Some(scheme) => anyhow::bail!("Server URL must use http or https, not {scheme}"),
        None => anyhow::bail!("Server URL must be absolute and include http:// or https://"),
    }
    if uri.authority().is_none() {
        anyhow::bail!("Server URL must include a host");
    }
    if uri.query().is_some() || uri.fragment().is_some() {
        anyhow::bail!("Server URL cannot include a query or fragment");
    }

    let mut normalized = uri.clone();
    normalized.path_mut().trim_trailing_slash();
    Ok(normalized)
}

pub fn normalize_path_prefix(prefix: Option<&Uri>) -> Result<Option<Uri>> {
    let Some(prefix) = prefix else {
        return Ok(None);
    };
    if prefix.scheme().is_some()
        || prefix.authority().is_some()
        || prefix.query().is_some()
        || prefix.fragment().is_some()
    {
        anyhow::bail!("Path prefix must contain only an absolute path");
    }

    let mut prefix = prefix.clone();
    prefix.path_mut().trim_trailing_slash();
    let path = prefix.path_or_root();
    if path == "/" {
        return Ok(None);
    }
    if !path.starts_with('/') {
        anyhow::bail!("Path prefix must start with /");
    }
    Ok(Some(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_server_url_deserializes_as_a_typed_uri() {
        let config: Config = toml::from_str(
            r#"
            [client]
            server_url = "https://example.com/tunnix/"
            "#,
        )
        .unwrap();
        let url = normalize_server_url(config.client.server_url.as_ref().unwrap()).unwrap();
        assert_eq!(url.to_string(), "https://example.com/tunnix");
    }

    #[test]
    fn server_url_rejects_non_http_and_ambiguous_suffixes() {
        for raw in [
            "socks5://example.com",
            "https://example.com/path?query=1",
            "https://example.com/path#fragment",
        ] {
            assert!(normalize_server_url(&Uri::parse(raw).unwrap()).is_err());
        }
    }

    #[test]
    fn server_paths_deserialize_and_normalize_as_typed_uris() {
        let config: Config = toml::from_str(
            r#"
            [server]
            path_prefix = "/tunnix/"
            root_redirect = "/docs"
            "#,
        )
        .unwrap();

        let prefix = normalize_path_prefix(config.server.path_prefix.as_ref())
            .unwrap()
            .unwrap();
        assert_eq!(prefix.to_string(), "/tunnix");
        assert_eq!(config.server.root_redirect.unwrap().to_string(), "/docs");
        assert!(
            normalize_path_prefix(Some(&Uri::parse("https://example.com/tunnix").unwrap()))
                .is_err()
        );
    }

    #[test]
    fn client_headers_preserve_order_and_repeated_field_lines() {
        let config: Config = toml::from_str(
            r#"
            [client]
            headers = [
                ["X-Trace", "first"],
                ["Cookie", "session=abc"],
                ["X-Trace", "second"],
            ]
            "#,
        )
        .unwrap();

        let headers = config
            .client
            .headers
            .ordered_iter()
            .map(|(name, value)| {
                (
                    name.as_original_str().into_owned(),
                    value.to_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            headers,
            [
                ("X-Trace".to_owned(), "first".to_owned()),
                ("Cookie".to_owned(), "session=abc".to_owned()),
                ("X-Trace".to_owned(), "second".to_owned()),
            ]
        );
    }
}
