use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub client: ClientConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Address to listen on (e.g., "0.0.0.0:8080")
    #[serde(default = "default_listen_addr")]
    pub listen: String,

    /// Password for encryption/authentication
    #[serde(default)]
    pub password: String,

    /// Maximum concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub timeout: u64,

    /// Path prefix for all routes (e.g. "/tunnix"). Default "" = serve at root.
    #[serde(default)]
    pub path_prefix: String,

    /// 301 redirect for GET / (and GET /{prefix}). Overrides root_html if both are set.
    #[serde(default)]
    pub root_redirect: Option<String>,

    /// Local HTML file to serve at GET / (and GET /{prefix}).
    #[serde(default)]
    pub root_html: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// WebSocket server URL (e.g., "wss://example.com")
    #[serde(default)]
    pub server_url: String,

    /// Password for encryption/authentication
    #[serde(default)]
    pub password: String,

    /// Local SOCKS5 proxy address (e.g., "127.0.0.1:7890")
    #[serde(default = "default_local_addr")]
    pub local_addr: String,

    /// Custom headers for WebSocket connection
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Reconnect interval in seconds
    #[serde(default = "default_reconnect_interval")]
    pub reconnect_interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: trace, debug, info, warn, error
    #[serde(default = "default_log_level")]
    pub level: String,
}

// Default values
fn default_listen_addr() -> String {
    "0.0.0.0:8080".to_string()
}

fn default_local_addr() -> String {
    "127.0.0.1:7890".to_string()
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
            path_prefix: String::new(),
            root_redirect: None,
            root_html: None,
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            password: String::new(),
            local_addr: default_local_addr(),
            headers: HashMap::new(),
            reconnect_interval: default_reconnect_interval(),
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
