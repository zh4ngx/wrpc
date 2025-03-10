//! wRPC QUIC transport

use anyhow::Context as _;
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream, VarInt};
use tracing::{debug, error, trace, warn};
use wrpc_transport::frame::{Accept, Incoming, InvokeBuilder, Outgoing};
use wrpc_transport::Invoke;

/// QUIC server with graceful stream shutdown handling
pub type Server = wrpc_transport::Server<(), RecvStream, SendStream, ConnHandler>;

/// QUIC wRPC client
#[derive(Clone, Debug)]
pub struct Client(Connection);

/// Graceful stream shutdown handler
pub struct ConnHandler;

const DONE: VarInt = VarInt::from_u32(1);

impl wrpc_transport::frame::ConnHandler<RecvStream, SendStream> for ConnHandler {
    async fn on_ingress(mut rx: RecvStream, res: std::io::Result<()>) {
        if let Err(err) = res {
            error!(?err, "ingress failed");
        } else {
            debug!("ingress successfully complete");
        }
        if let Err(err) = rx.stop(DONE) {
            debug!(?err, "failed to close incoming stream");
        }
    }

    async fn on_egress(mut tx: SendStream, res: std::io::Result<()>) {
        if let Err(err) = res {
            error!(?err, "egress failed");
        } else {
            debug!("egress successfully complete");
        }
        match tx.stopped().await {
            Ok(None) => {
                trace!("stream successfully closed")
            }
            Ok(Some(code)) => {
                if code == DONE {
                    trace!("stream successfully closed")
                } else {
                    warn!(?code, "stream closed with code")
                }
            }
            Err(err) => {
                error!(?err, "failed to await stream close");
            }
        }
    }
}

impl From<Connection> for Client {
    fn from(conn: Connection) -> Self {
        Self(conn)
    }
}

impl Invoke for &Client {
    type Context = ();
    type Outgoing = Outgoing;
    type Incoming = Incoming;

    async fn invoke<P>(
        &self,
        (): Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
        let (tx, rx) = self
            .0
            .open_bi()
            .await
            .context("failed to open parameter stream")?;
        InvokeBuilder::<ConnHandler>::default()
            .invoke(tx, rx, instance, func, params, paths)
            .await
    }
}

impl Invoke for Client {
    type Context = ();
    type Outgoing = Outgoing;
    type Incoming = Incoming;

    async fn invoke<P>(
        &self,
        (): Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
        (&self).invoke((), instance, func, params, paths).await
    }
}

impl Accept for &Client {
    type Context = ();
    type Outgoing = SendStream;
    type Incoming = RecvStream;

    async fn accept(&self) -> std::io::Result<(Self::Context, Self::Outgoing, Self::Incoming)> {
        let (tx, rx) = self.0.accept_bi().await?;
        Ok(((), tx, rx))
    }
}

impl Accept for Client {
    type Context = ();
    type Outgoing = SendStream;
    type Incoming = RecvStream;

    async fn accept(&self) -> std::io::Result<(Self::Context, Self::Outgoing, Self::Incoming)> {
        (&self).accept().await
    }
}
