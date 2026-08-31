use std::{io, sync::Arc};

use rama::{
    net::client::{
        ConnectRequest, ConnectionError, ConnectionErrorKind, EstablishedClientConnection,
    },
    rt::Executor,
    telemetry::tracing::{debug, info},
    utils::octets::kib,
    Service, ServiceInput,
};

use crate::{
    protocol::Message,
    relay::{next_conn_id, relay},
    tunnel::Tunnel,
};

#[derive(Clone)]
pub struct TunnelConnector {
    tunnel: Arc<Tunnel>,
    exec: Executor,
}

impl TunnelConnector {
    pub fn new(tunnel: Arc<Tunnel>, exec: Executor) -> Self {
        Self { tunnel, exec }
    }
}

impl Service<ConnectRequest> for TunnelConnector {
    type Output =
        EstablishedClientConnection<ServiceInput<tokio::io::DuplexStream>, ConnectRequest>;
    type Error = ConnectionError;

    async fn serve(&self, input: ConnectRequest) -> Result<Self::Output, Self::Error> {
        let host = input.authority.host.to_string();
        let port = input.authority.port;
        let conn_id = next_conn_id();
        info!(conn_id, %host, port, "opening tunnel connection");

        let event_rx = self.tunnel.register_connection(conn_id).await;
        let connect = Message::Connect {
            conn_id,
            host: host.clone(),
            port,
        };

        match self.tunnel.send_connect(&connect).await {
            Ok(Some(Message::Data { data, .. })) if data.is_empty() => {
                debug!(conn_id, "tunnel connection acknowledged");
            }
            Ok(Some(Message::Error { message, .. })) => {
                self.tunnel.unregister_connection(conn_id).await;
                return Err(connect_error(format!(
                    "server could not connect to {host}:{port}: {message}"
                )));
            }
            Ok(Some(message)) => {
                self.tunnel.unregister_connection(conn_id).await;
                return Err(connect_error(format!(
                    "unexpected CONNECT response for {host}:{port}: {message:?}"
                )));
            }
            Ok(None) => {
                self.tunnel.unregister_connection(conn_id).await;
                return Err(connect_error(format!(
                    "empty CONNECT response for {host}:{port}"
                )));
            }
            Err(error) => {
                self.tunnel.unregister_connection(conn_id).await;
                return Err(connect_error(format!(
                    "CONNECT request for {host}:{port} failed: {error:#}"
                )));
            }
        }

        let (client_io, relay_io) = tokio::io::duplex(kib(64));
        let tunnel = self.tunnel.clone();
        self.exec.spawn_cancellable_task(async move {
            relay(relay_io, conn_id, tunnel, event_rx).await;
            info!(conn_id, %host, port, "tunnel connection closed");
        });

        Ok(EstablishedClientConnection {
            input,
            conn: ServiceInput::new(client_io),
        })
    }
}

fn connect_error(message: String) -> ConnectionError {
    ConnectionError::transport(io::Error::other(message), ConnectionErrorKind::Unavailable)
}
