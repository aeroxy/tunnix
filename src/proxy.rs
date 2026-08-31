use std::{convert::Infallible, sync::Arc, time::Duration};

use anyhow::Result;
use rama::{
    graceful::Shutdown,
    http::{
        client::EasyHttpWebClient,
        layer::{
            remove_header::{RemoveRequestHeaderLayer, RemoveResponseHeaderLayer},
            trace::TraceLayer,
            upgrade::{EagerHttpProxyConnector, UpgradeLayer},
        },
        matcher::MethodMatcher,
        server::HttpServer,
        service::web::response::IntoResponse,
        Request, StatusCode,
    },
    io::peek::PeekRouter,
    layer::ConsumeErrLayer,
    net::{address::SocketAddress, proxy::IoForwardService},
    proxy::socks5::{server::Connector as Socks5Connector, Socks5Acceptor},
    rt::Executor,
    service::service_fn,
    tcp::server::TcpListener,
    telemetry::tracing::{debug, info},
    tls::client::TlsClientConfig,
    Layer, Service,
};

use crate::{tunnel::Tunnel, tunnel_connector::TunnelConnector};

pub async fn run_proxy(
    listen_addr: SocketAddress,
    tunnel: Arc<Tunnel>,
    exec: Executor,
    shutdown: Shutdown,
) -> Result<()> {
    let listener = TcpListener::bind_address(listen_addr, exec.clone())
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let connector = TunnelConnector::new(tunnel, exec.clone());

    let web_client = EasyHttpWebClient::connector_builder()
        .with_custom_transport_connector(connector.clone())
        .with_dns_connector(())
        .without_tls_proxy_support()
        .without_proxy_support()
        .with_tls_support_using_rustls(TlsClientConfig::default_http())
        .with_default_http_connector(exec.clone())
        .without_connection_pool()
        .build_client()
        .boxed();

    let plain_http = service_fn(move |request: Request| {
        let web_client = web_client.clone();
        async move {
            match web_client.serve(request).await {
                Ok(response) => Ok::<_, Infallible>(response),
                Err(error) => {
                    debug!(?error, "HTTP proxy request failed");
                    Ok(StatusCode::BAD_GATEWAY.into_response())
                }
            }
        }
    });

    let connect =
        EagerHttpProxyConnector::new(connector.clone(), IoForwardService::new(exec.clone()));
    let http = HttpServer::auto(exec.clone()).service(
        (
            TraceLayer::new_for_http(),
            ConsumeErrLayer::default(),
            UpgradeLayer::new(exec.clone(), MethodMatcher::CONNECT, connect),
            RemoveResponseHeaderLayer::hop_by_hop(),
            RemoveRequestHeaderLayer::hop_by_hop(),
        )
            .into_layer(plain_http),
    );

    let socks = Socks5Acceptor::new(exec.clone()).with_connector(
        Socks5Connector::new(connector, IoForwardService::new(exec.clone()))
            .with_hide_local_address(true),
    );
    // Socks5PeekRouter in Rama 0.4 interprets the greeting's NMETHODS count as
    // a method ID and rejects valid counts such as four. Match the protocol
    // version with Rama's generic replaying router until the dependency
    // includes the upstream fix.
    let proxy = PeekRouter::from_prefix(b"\x05", socks).with_fallback(http);

    info!(
        %listen_addr,
        protocols = "socks5,http",
        "proxy listening"
    );
    shutdown.spawn_task(listener.serve(proxy));
    drop(exec);
    let elapsed = shutdown
        .shutdown_with_limit(Duration::from_secs(30))
        .await
        .map_err(|error| anyhow::anyhow!("graceful proxy shutdown timed out: {error}"))?;
    info!(?elapsed, "proxy shutdown complete");
    Ok(())
}
