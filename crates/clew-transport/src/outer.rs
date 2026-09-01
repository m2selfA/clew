use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr,
    endpoint::{Connection, RecvStream, SendStream, presets},
};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[derive(Clone, Debug)]
pub struct IrohOuter {
    endpoint: Endpoint,
}

impl IrohOuter {
    pub async fn bind() -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0)
            .alpns(vec![clew_proto::ALPN.to_vec()])
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    pub async fn bind_direct_only() -> Result<Self, IrohOuterError> {
        let endpoint = Endpoint::builder(presets::N0DisableRelay)
            .alpns(vec![clew_proto::ALPN.to_vec()])
            .bind()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(Self { endpoint })
    }

    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub async fn relay_only_addr(&self) -> Result<EndpointAddr, IrohOuterError> {
        tokio::time::timeout(Duration::from_secs(20), self.endpoint.online())
            .await
            .map_err(|_| IrohOuterError::RelayOnlineTimeout)?;
        let EndpointAddr { id, addrs } = self.endpoint.addr();
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
        let connection = self
            .endpoint
            .connect(addr, clew_proto::ALPN)
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
        let incoming = self
            .endpoint
            .accept()
            .await
            .ok_or(IrohOuterError::EndpointClosed)?;
        let connection = incoming.await.map_err(IrohOuterError::from_display)?;
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(IrohOuterError::from_display)?;
        Ok(IrohStream {
            connection,
            send,
            recv,
        })
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
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
