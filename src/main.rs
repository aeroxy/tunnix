mod config;
mod crypto;
#[cfg(unix)]
mod exec;
mod http_proxy;
mod protocol;
mod proxy;
mod relay;
mod reload;
mod server;
mod socks5;
mod tunnel;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing::{info, warn, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Config;
use crate::crypto::Crypto;
use crate::reload::{CliOverrides, HotServerConfig};

#[derive(Parser, Debug)]
#[command(name = "tunnix", version, about = "encrypted proxy tunnel over HTTP/SSE")]
struct Args {
    /// Config file path
    #[arg(short = 'f', long, global = true)]
    config: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true)]
    log_level: Option<String>,

    /// Write logs to file; omit value to use ./tunnix.log
    #[arg(long, num_args(0..=1), default_missing_value = "./tunnix.log", global = true)]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the server
    Server(ServerArgs),
    /// Run the client
    Client(ClientArgs),
    /// Run a command (or interactive shell) on the server (requires server allow_exec)
    #[cfg(unix)]
    RemoteExec(RemoteExecArgs),
}

#[derive(clap::Args, Debug)]
struct ServerArgs {
    /// Address to listen on (overrides config)
    #[arg(short, long)]
    listen: Option<String>,

    /// Password for encryption (overrides config)
    #[arg(short, long, env = "TUNNIX_PASSWORD")]
    password: Option<String>,

    /// Allow remote command execution (exposes a shell — RCE). Off unless set here or in config.
    #[arg(long)]
    allow_exec: bool,
}

#[derive(clap::Args, Debug)]
struct ClientArgs {
    /// Server URL (overrides config)
    #[arg(short, long)]
    server: Option<String>,

    /// Password for encryption (overrides config)
    #[arg(short, long, env = "TUNNIX_PASSWORD")]
    password: Option<String>,

    /// Local proxy address for SOCKS5 + HTTP (overrides config)
    #[arg(short, long)]
    local_addr: Option<String>,

    /// Custom cookie header (overrides config)
    #[arg(short, long)]
    cookie: Option<String>,
}

#[cfg(unix)]
#[derive(clap::Args, Debug)]
struct RemoteExecArgs {
    /// Server URL (overrides config)
    #[arg(short, long)]
    server: Option<String>,

    /// Password for encryption (overrides config)
    #[arg(short, long, env = "TUNNIX_PASSWORD")]
    password: Option<String>,

    /// Custom cookie header (overrides config)
    #[arg(short, long)]
    cookie: Option<String>,

    /// Command to run; omit for an interactive shell
    #[arg(trailing_var_arg = true)]
    cmd: Vec<String>,
}

fn resolve_config_path(explicit: &Option<String>) -> Option<String> {
    match explicit {
        Some(p) => Some(p.clone()),
        None => {
            if std::path::Path::new("config.toml").exists() {
                Some("config.toml".to_string())
            } else {
                None
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load config from file, then apply CLI overrides
    let mut config = match &args.config {
        Some(path) => Config::from_file(path)?,
        None => Config::from_file("config.toml").unwrap_or_default(),
    };

    #[cfg_attr(not(unix), allow(unused_variables))]
    let explicit_log_level = args.log_level.is_some();
    if let Some(log_level) = args.log_level {
        config.logging.level = log_level;
    }
    // Keep the terminal clean during an interactive remote-exec session unless
    // the user explicitly asked for a log level.
    #[cfg(unix)]
    if matches!(args.command, Command::RemoteExec(_)) && !explicit_log_level {
        config.logging.level = "error".to_string();
    }

    let level = match config.logging.level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(false);

    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::from_level(level))
        .with(stdout_layer);

    if let Some(ref log_path) = args.log {
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(log_file)
            .with_target(false)
            .with_ansi(false);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }

    let config_path = resolve_config_path(&args.config);

    match args.command {
        Command::Server(sa) => {
            let cli_overrides = Arc::new(CliOverrides {
                server_password: sa.password.is_some(),
                server_allow_exec: sa.allow_exec,
                client_password: false,
                client_headers: false,
            });

            if let Some(listen) = sa.listen {
                config.server.listen = listen;
            }
            if let Some(password) = sa.password {
                config.server.password = password;
            }
            if sa.allow_exec {
                config.server.allow_exec = true;
            }

            if config.server.password.is_empty() {
                bail!("Password is required. Set via --password, TUNNIX_PASSWORD env var, or config file.");
            }

            if let Some(ref log_path) = args.log {
                info!("Log file: {}", log_path);
            }

            info!("tunnix server v{}", env!("CARGO_PKG_VERSION"));
            info!("Listening on: {}", config.server.listen);
            if !config.server.path_prefix.is_empty() {
                info!("Path prefix: {}", config.server.path_prefix);
            }
            if config.server.allow_exec {
                warn!("Remote command execution ENABLED — anyone with the password can run a shell on this machine");
            }

            let crypto = Arc::new(Crypto::new(&config.server.password)?);
            info!("Encryption initialized");

            let hot = HotServerConfig {
                crypto,
                path_prefix: config.server.path_prefix.trim_end_matches('/').to_string(),
                root_redirect: config.server.root_redirect.clone(),
                root_html: config.server.root_html.clone(),
                health_body: config.server.health_response.clone(),
                allow_exec: config.server.allow_exec,
            };

            server::run_server(
                &config.server.listen,
                hot,
                config_path,
                cli_overrides,
            )
            .await?;
        }

        Command::Client(ca) => {
            let cli_overrides = Arc::new(CliOverrides {
                server_password: false,
                server_allow_exec: false,
                client_password: ca.password.is_some(),
                client_headers: ca.cookie.is_some(),
            });

            if let Some(server) = ca.server {
                config.client.server_url = server;
            }
            if let Some(password) = ca.password {
                config.client.password = password;
            }
            if let Some(local_addr) = ca.local_addr {
                config.client.local_addr = local_addr;
            }
            if let Some(cookie) = ca.cookie {
                config.client.headers.insert("Cookie".to_string(), cookie);
            }

            if config.client.server_url.is_empty() {
                bail!("Server URL is required. Set via --server or config file.");
            }
            if config.client.password.is_empty() {
                bail!("Password is required. Set via --password, TUNNIX_PASSWORD env var, or config file.");
            }

            // Install rustls crypto provider
            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("Failed to install rustls crypto provider");

            if let Some(ref log_path) = args.log {
                info!("Log file: {}", log_path);
            }

            info!("tunnix client v{}", env!("CARGO_PKG_VERSION"));
            info!("Server: {}", config.client.server_url);
            info!("Proxy (SOCKS5 + HTTP): {}", config.client.local_addr);

            let crypto = Arc::new(Crypto::new(&config.client.password)?);
            info!("Encryption initialized");

            let tun = tunnel::Tunnel::connect(
                &config.client.server_url,
                crypto,
                &config.client.headers,
                &config.client.health_expected,
            )
            .await?;

            info!("Tunnel established");

            if let Some(path) = config_path {
                let hot = tun.hot.clone();
                let reconnect = tun.reconnect_signal.clone();
                let session_id = tun.session_id.clone();
                let channels = tun.response_channels.clone();
                let overrides = cli_overrides.clone();
                tokio::spawn(async move {
                    reload::config_watcher_client(
                        path, hot, reconnect, session_id, channels, overrides,
                    )
                    .await;
                });
            }

            proxy::run_proxy(&config.client.local_addr, tun).await?;
        }

        #[cfg(unix)]
        Command::RemoteExec(ra) => {
            if let Some(server) = ra.server {
                config.client.server_url = server;
            }
            if let Some(password) = ra.password {
                config.client.password = password;
            }
            if let Some(cookie) = ra.cookie {
                config.client.headers.insert("Cookie".to_string(), cookie);
            }

            if config.client.server_url.is_empty() {
                bail!("Server URL is required. Set via --server or config file.");
            }
            if config.client.password.is_empty() {
                bail!("Password is required. Set via --password, TUNNIX_PASSWORD env var, or config file.");
            }

            rustls::crypto::ring::default_provider()
                .install_default()
                .expect("Failed to install rustls crypto provider");

            let crypto = Arc::new(Crypto::new(&config.client.password)?);

            let tun = tunnel::Tunnel::connect(
                &config.client.server_url,
                crypto,
                &config.client.headers,
                &config.client.health_expected,
            )
            .await?;

            let cmd = if ra.cmd.is_empty() {
                None
            } else {
                // Shell-quote each arg so spaces / quotes inside args survive
                // the trip through the server's `sh -c`. A plain join(" ")
                // would re-tokenize, e.g. `echo "a b" "c"` would become
                // `echo a b c` and the server would see four args.
                Some(
                    shlex::try_join(ra.cmd.iter().map(String::as_str))
                        .unwrap_or_else(|_| ra.cmd.join(" ")),
                )
            };

            let code = exec::run(tun, cmd).await?;
            std::process::exit(code);
        }
    }

    Ok(())
}
