mod http_proxy;
mod proxy;
mod relay;
mod server;
mod socks5;
mod tunnel;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing::{info, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tunnix_common::config::Config;
use tunnix_common::crypto::Crypto;

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
}

#[derive(clap::Args, Debug)]
struct ServerArgs {
    /// Address to listen on (overrides config)
    #[arg(short, long)]
    listen: Option<String>,

    /// Password for encryption (overrides config)
    #[arg(short, long, env = "TUNNIX_PASSWORD")]
    password: Option<String>,
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Load config from file, then apply CLI overrides
    let mut config = match &args.config {
        Some(path) => Config::from_file(path)?,
        None => Config::from_file("config.toml").unwrap_or_default(),
    };

    if let Some(log_level) = args.log_level {
        config.logging.level = log_level;
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

    match args.command {
        Command::Server(sa) => {
            if let Some(listen) = sa.listen {
                config.server.listen = listen;
            }
            if let Some(password) = sa.password {
                config.server.password = password;
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

            let crypto = Arc::new(Crypto::new(&config.server.password)?);
            info!("Encryption initialized");

            server::run_server(
                &config.server.listen,
                crypto,
                &config.server.path_prefix,
                config.server.root_redirect.clone(),
                config.server.root_html.clone(),
            )
            .await?;
        }

        Command::Client(ca) => {
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
            )
            .await?;

            info!("Tunnel established");

            proxy::run_proxy(&config.client.local_addr, tun).await?;
        }
    }

    Ok(())
}
