use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IrohProtocol {
    InnerSession,
    Bootstrap,
    Connector,
}

#[derive(Clone, Debug)]
pub struct IrohOuter {
    endpoint: Endpoint,
}

impl IrohOuter {
    pub async fn bind() -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(supported_alpns())
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    pub async fn bind_with_secret(secret: [u8; 32]) -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(supported_alpns())
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    pub async fn bind_direct_only() -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0DisableRelay)
            .alpns(supported_alpns())
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    pub async fn bind_direct_only_with_secret(secret: [u8; 32]) -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0DisableRelay)
            .secret_key(SecretKey::from_bytes(&secret))
            .alpns(supported_alpns())
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub async fn online_addr(&self) -> Result<EndpointAddr, IrohOuterError> {
        tokio::time::timeout(Duration::from_secs(20), self.endpoint.online())
            .await
            .map_err(|_| IrohOuterError::RelayOnlineTimeout)?;
        Ok(self.endpoint.addr())
    }

    pub async fn relay_only_addr(&self) -> Result<EndpointAddr, IrohOuterError> {
        let EndpointAddr { id, addrs } = self.online_addr().await?;
        let mut relay_only = EndpointAddr {
            id,
            addrs: Default::default(),
        };
        for address in addrs {
            if address.is_relay() {
                relay_only.addrs.insert(address);
            }
        }
        if relay_only.addrs.is_empty() {
            return Err(IrohOuterError::NoRelayAddress);
        }
        Ok(relay_only)
    }

    pub async fn connect(&self, addr: EndpointAddr) -> Result<IrohStream, IrohOuterError> {
        self.connect_with_protocol(addr, IrohProtocol::InnerSession)
            .await
    }

    pub async fn connect_bootstrap(
        &self,
        addr: EndpointAddr,
    ) -> Result<IrohStream, IrohOuterError> {
        self.connect_with_protocol(addr, IrohProtocol::Bootstrap)
            .await
    }

    pub async fn connect_connector(
        &self,
        addr: EndpointAddr,
    ) -> Result<IrohStream, IrohOuterError> {
        self.connect_with_protocol(addr, IrohProtocol::Connector)
            .await
    }

    async fn connect_with_protocol(
        &self,
        addr: EndpointAddr,
        protocol: IrohProtocol,
    ) -> Result<IrohStream, IrohOuterError> {
        let connection = self
            .endpoint
            .connect(addr, protocol.alpn())
            .await
            .map_err(IrohOuterError::from_display)?;
        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(IrohStream {
            connection,
            send,
            recv,
        })
    }

    pub async fn accept(&self) -> Result<IrohStream, IrohOuterError> {
        let (protocol, stream) = self.accept_classified().await?;
        if protocol != IrohProtocol::InnerSession {
            return Err(IrohOuterError::UnexpectedProtocol(protocol));
        }
        Ok(stream)
    }

    pub async fn accept_classified(&self) -> Result<(IrohProtocol, IrohStream), IrohOuterError> {
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(IrohOuterError::EndpointClosed)?;
        let connection = incoming.await.map_err(IrohOuterError::from_display)?;
        let protocol = IrohProtocol::from_alpn(connection.alpn())?;
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok((
            protocol,
            IrohStream {
                connection,
                send,
                recv,
            },
        ))
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

impl IrohProtocol {
    const fn alpn(self) -> &'static [u8] {
        match self {
            Self::InnerSession => clew_proto::ALPN,
            Self::Bootstrap => clew_proto::BOOTSTRAP_ALPN,
            Self::Connector => clew_proto::CONNECTOR_ALPN,
        }
    }

    fn from_alpn(alpn: &[u8]) -> Result<Self, IrohOuterError> {
        if alpn == clew_proto::ALPN {
            Ok(Self::InnerSession)
        } else if alpn == clew_proto::BOOTSTRAP_ALPN {
            Ok(Self::Bootstrap)
        } else if alpn == clew_proto::CONNECTOR_ALPN {
            Ok(Self::Connector)
        } else {
            Err(IrohOuterError::UnsupportedAlpn)
        }
    }
}

fn supported_alpns() -> Vec<Vec<u8>> {
    vec![
        clew_proto::ALPN.to_vec(),
        clew_proto::BOOTSTRAP_ALPN.to_vec(),
        clew_proto::CONNECTOR_ALPN.to_vec(),
    ]
}

#[derive(Debug)]
pub struct IrohStream {
    connection: Connection,
    send: SendStream,
    recv: RecvStream,
}

impl IrohStream {
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
}

impl AsyncRead for IrohStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl AsyncWrite for IrohStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

#[derive(Debug, Error)]
pub enum IrohOuterError {
    #[error("iroh endpoint is closed")]
    EndpointClosed,
    #[error("iroh endpoint did not become relay-online before timeout")]
    RelayOnlineTimeout,
    #[error("iroh endpoint has no advertised relay address")]
    NoRelayAddress,
    #[error("iroh negotiated an unsupported ALPN")]
    UnsupportedAlpn,
    #[error("iroh connection used unexpected protocol {0:?}")]
    UnexpectedProtocol(IrohProtocol),
    #[error("iroh outer transport failed: {0}")]
    Iroh(String),
}

impl IrohOuterError {
    fn from_display(error: impl std::fmt::Display) -> Self {
        Self::Iroh(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TapStream<S> {
        inner: S,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl<S> TapStream<S> {
        fn new(inner: S, written: Arc<Mutex<Vec<u8>>>) -> Self {
            Self { inner, written }
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for TapStream<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for TapStream<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match Pin::new(&mut self.inner).poll_write(cx, buf) {
                Poll::Ready(Ok(written)) => {
                    self.written
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buf[..written]);
                    Poll::Ready(Ok(written))
                }
                other => other,
            }
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    #[tokio::test]
    async fn bootstrap_alpn_is_classified_separately_from_inner_session() {
        let server = IrohOuter::bind_direct_only().await.unwrap();
        let client = IrohOuter::bind_direct_only().await.unwrap();
        let server_addr = server.addr();
        let accepted = tokio::spawn({
            let server = server.clone();
            async move { server.accept_classified().await.unwrap().0 }
        });
        let mut stream = client.connect_bootstrap(server_addr).await.unwrap();
        stream.write_all(b"bootstrap").await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(accepted.await.unwrap(), IrohProtocol::Bootstrap);
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn explicit_secret_keeps_endpoint_id_stable_across_rebind() {
        let secret = [73_u8; 32];
        let first = IrohOuter::bind_direct_only_with_secret(secret)
            .await
            .unwrap();
        let first_id = first.addr().id;
        first.close().await;
        let second = IrohOuter::bind_direct_only_with_secret(secret)
            .await
            .unwrap();
        assert_eq!(second.addr().id, first_id);
        second.close().await;
    }

    #[tokio::test]
    async fn direct_only_outer_stream_is_bidirectional() {
        let server = IrohOuter::bind_direct_only().await.unwrap();
        let client = IrohOuter::bind_direct_only().await.unwrap();
        let server_addr = server.addr();
        let server_accept = server.clone();
        let accept = tokio::spawn(async move {
            let mut stream = server_accept.accept().await.unwrap();
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
            stream.flush().await.unwrap();
            let mut ack = [0_u8; 1];
            stream.read_exact(&mut ack).await.unwrap();
            assert_eq!(&ack, b"!");
        });
        let mut stream = client.connect(server_addr).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        stream.write_all(b"!").await.unwrap();
        stream.flush().await.unwrap();
        accept.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    #[ignore = "requires public n0 relay connectivity"]
    async fn public_relay_dial_carries_inner_session_without_rekeying() {
        use clew_core::{DeviceId, SiteId};
        use clew_identity::{ControllerIdentity, DeviceIdentity};

        use crate::inner::{
            ControllerSessionIdentity, DeviceSessionIdentity, InnerMessage, InnerSession,
        };

        let controller_identity = ControllerIdentity::from_secret([31_u8; 32]);
        let device_identity = DeviceIdentity::from_secret([32_u8; 32]);
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let controller_session = ControllerSessionIdentity {
            identity: controller_identity.clone(),
            noise_static_secret: [33_u8; 32],
            expected_device: device_identity.public_identity(),
            device_id,
            site_id,
        };
        let device_session = DeviceSessionIdentity {
            identity: device_identity,
            pinned_controller: controller_identity.public_identity(),
            device_id,
            site_id,
        };

        let server = IrohOuter::bind().await.unwrap();
        let client = IrohOuter::bind().await.unwrap();
        let relay_addr = server.relay_only_addr().await.unwrap();
        let server_accept = server.clone();
        let server_task = tokio::spawn(async move {
            let mut stream = server_accept.accept().await.unwrap();
            let mut inner = InnerSession::accept(&mut stream, controller_session)
                .await
                .unwrap();
            let request = inner.recv(&mut stream).await.unwrap();
            assert_eq!(request.kind, "probe");
            let response = InnerMessage::new("probe_result", b"relay-ok".to_vec()).unwrap();
            inner.send(&mut stream, &response).await.unwrap();
            let ack = inner.recv(&mut stream).await.unwrap();
            assert_eq!(ack.kind, "ack");
        });

        let mut stream = client.connect(relay_addr).await.unwrap();
        let mut inner = InnerSession::connect(&mut stream, device_session)
            .await
            .unwrap();
        inner
            .send(
                &mut stream,
                &InnerMessage::new("probe", b"path-may-migrate".to_vec()).unwrap(),
            )
            .await
            .unwrap();
        let response = inner.recv(&mut stream).await.unwrap();
        assert_eq!(response.payload, b"relay-ok");
        inner
            .send(&mut stream, &InnerMessage::new("ack", Vec::new()).unwrap())
            .await
            .unwrap();

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn iroh_outer_carries_mutually_authenticated_inner_session() {
        use clew_core::{DeviceId, SiteId};
        use clew_identity::{ControllerIdentity, DeviceIdentity};

        use crate::inner::{
            ControllerSessionIdentity, DeviceSessionIdentity, InnerMessage, InnerSession,
        };

        let controller_identity = ControllerIdentity::from_secret([21_u8; 32]);
        let device_identity = DeviceIdentity::from_secret([22_u8; 32]);
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        let controller_session = ControllerSessionIdentity {
            identity: controller_identity.clone(),
            noise_static_secret: [23_u8; 32],
            expected_device: device_identity.public_identity(),
            device_id,
            site_id,
        };
        let device_session = DeviceSessionIdentity {
            identity: device_identity,
            pinned_controller: controller_identity.public_identity(),
            device_id,
            site_id,
        };

        let server = IrohOuter::bind_direct_only().await.unwrap();
        let client = IrohOuter::bind_direct_only().await.unwrap();
        let server_addr = server.addr();
        let server_accept = server.clone();
        let server_task = tokio::spawn(async move {
            let mut stream = server_accept.accept().await.unwrap();
            let mut inner = InnerSession::accept(&mut stream, controller_session)
                .await
                .unwrap();
            let request = inner.recv(&mut stream).await.unwrap();
            assert_eq!(request.kind, "read");
            assert_eq!(request.payload, b"C:/private/data.mrc");
            let response = InnerMessage::new("read_result", b"ok".to_vec()).unwrap();
            inner.send(&mut stream, &response).await.unwrap();
            let ack = inner.recv(&mut stream).await.unwrap();
            assert_eq!(ack.kind, "ack");
        });

        let captured_outer = Arc::new(Mutex::new(Vec::new()));
        let outer = client.connect(server_addr).await.unwrap();
        let mut stream = TapStream::new(outer, Arc::clone(&captured_outer));
        let mut inner = InnerSession::connect(&mut stream, device_session)
            .await
            .unwrap();
        let request = InnerMessage::new("read", b"C:/private/data.mrc".to_vec()).unwrap();
        inner.send(&mut stream, &request).await.unwrap();
        let response = inner.recv(&mut stream).await.unwrap();
        assert_eq!(response.kind, "read_result");
        assert_eq!(response.payload, b"ok");
        let ack = InnerMessage::new("ack", Vec::new()).unwrap();
        inner.send(&mut stream, &ack).await.unwrap();

        let captured_outer = captured_outer.lock().unwrap().clone();
        assert!(
            !captured_outer
                .windows(br#"\"kind\":\"read\""#.len())
                .any(|window| window == br#"\"kind\":\"read\""#)
        );
        assert!(
            !captured_outer
                .windows(b"C:/private/data.mrc".len())
                .any(|window| window == b"C:/private/data.mrc")
        );

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }
}
