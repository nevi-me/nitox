use futures::{
    future::{self, Either},
    prelude::*,
};
use std::{
    net::SocketAddr,
    sync::{Arc, RwLock},
};
use tokio_executor;

use error::NatsError;
use protocol::Op;

use super::connection_inner::NatsConnectionInner;

macro_rules! reco {
    ($conn:ident) => {
        if let Ok(mut state) = $conn.state.write() {
            *state = NatsConnectionState::Disconnected;
        }

        tokio_executor::spawn($conn.reconnect().map_err(|e| {
            debug!(target: "nitox", "Reconnection error: {}", e);
            ()
        }));
    };
}

/// State of the raw connection
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NatsConnectionState {
    Connected,
    Reconnecting,
    Disconnected,
}

/// Represents a connection to a NATS server. Implements `Sink` and `Stream`
#[derive(Debug)]
pub struct NatsConnection {
    /// indicates if the connection is made over TLS
    pub(crate) is_tls: bool,
    /// Server standardized IP address
    pub(crate) addr: SocketAddr,
    /// Host of the server; Only used if connecting to a TLS-enabled server
    pub(crate) host: Option<String>,
    /// Inner dual `Stream`/`Sink` of the TCP connection
    pub(crate) inner: Arc<RwLock<NatsConnectionInner>>,
    /// Current state of the connection
    pub(crate) state: Arc<RwLock<NatsConnectionState>>,
}

impl NatsConnection {
    /// Tries to reconnect once to the server; Only used internally. Blocks polling during reconnecting
    /// by forcing the object to return `Async::NotReady`/`AsyncSink::NotReady`
    fn reconnect(&self) -> impl Future<Item = (), Error = NatsError> {
        if let Ok(mut state) = self.state.write() {
            *state = NatsConnectionState::Reconnecting;
        }

        let inner_arc = Arc::clone(&self.inner);
        let inner_state = Arc::clone(&self.state);
        let is_tls = self.is_tls;
        let maybe_host = self.host.clone();
        NatsConnectionInner::connect_tcp(&self.addr)
            .and_then(move |socket| {
                if is_tls {
                    Either::A(
                        // This unwrap is safe because the value would always be present if `is_tls` is true
                        NatsConnectionInner::upgrade_tcp_to_tls(&maybe_host.unwrap(), socket)
                            .map(NatsConnectionInner::from),
                    )
                } else {
                    Either::B(future::ok(NatsConnectionInner::from(socket)))
                }
            }).and_then(move |inner| {
                let res = if let Ok(mut inner_conn) = inner_arc.write() {
                    *inner_conn = inner;
                    if let Ok(mut state) = inner_state.write() {
                        *state = NatsConnectionState::Connected;
                    }
                    debug!(target: "nitox", "Successfully swapped reconnected underlying connection");
                    Ok(())
                } else {
                    debug!(target: "nitox", "Cannot reconnect to server");
                    Err(NatsError::CannotReconnectToServer)
                };

                drop(inner_arc);

                res
            })
    }
}

impl Sink for NatsConnection {
    type SinkError = NatsError;
    type SinkItem = Op;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        if let Ok(mut inner) = self.inner.try_write() {
            match inner.start_send(item.clone()) {
                Err(NatsError::ServerDisconnected(_)) => {
                    reco!(self);
                    Ok(AsyncSink::NotReady(item))
                }
                poll_res => poll_res,
            }
        } else {
            Ok(AsyncSink::NotReady(item))
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        if let Ok(mut inner) = self.inner.try_write() {
            match inner.poll_complete() {
                Err(NatsError::ServerDisconnected(_)) => {
                    reco!(self);
                    Ok(Async::NotReady)
                }
                poll_res => poll_res,
            }
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl Stream for NatsConnection {
    type Error = NatsError;
    type Item = Op;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if let Ok(mut inner) = self.inner.try_write() {
            match inner.poll() {
                Err(NatsError::ServerDisconnected(_)) => {
                    reco!(self);
                    Ok(Async::NotReady)
                }
                poll_res => poll_res,
            }
        } else {
            Ok(Async::NotReady)
        }
    }
}
