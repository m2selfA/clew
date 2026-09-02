use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, copy_bidirectional};

use crate::{SignedConnectorLease, SiteDiscoveryTag};

pub const CONNECTOR_CONTROL_VERSION: u32 = 1;
pub const MAX_CONNECTOR_CONTROL_FRAME_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorTunnelPurpose {
    InnerSession,
    Bootstrap,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectorOpenRequest {
    pub version: u32,
    pub site_tag: String,
    pub purpose: ConnectorTunnelPurpose,
}

impl ConnectorOpenRequest {
    #[must_use]
    pub fn new(site_tag: SiteDiscoveryTag, purpose: ConnectorTunnelPurpose) -> Self {
        Self {
            version: CONNECTOR_CONTROL_VERSION,
            site_tag: site_tag.to_string(),
            purpose,
        }
    }

    pub fn validate(&self) -> Result<SiteDiscoveryTag, ConnectorControlError> {
        if self.version != CONNECTOR_CONTROL_VERSION {
            return Err(ConnectorControlError::UnsupportedVersion(self.version));
        }
        SiteDiscoveryTag::parse(&self.site_tag).map_err(|_| ConnectorControlError::InvalidSiteTag)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectorReady {
    pub version: u32,
    pub lease: SignedConnectorLease,
}

impl ConnectorReady {
    #[must_use]
    pub fn new(lease: SignedConnectorLease) -> Self {
        Self {
            version: CONNECTOR_CONTROL_VERSION,
            lease,
        }
    }

    pub fn validate(&self) -> Result<(), ConnectorControlError> {
        if self.version != CONNECTOR_CONTROL_VERSION {
            return Err(ConnectorControlError::UnsupportedVersion(self.version));
        }
        Ok(())
    }
}

pub async fn write_connector_open<S: AsyncWrite + Unpin>(
    stream: &mut S,
    request: &ConnectorOpenRequest,
) -> Result<(), ConnectorControlError> {
    request.validate()?;
    write_control_frame(stream, request).await
}

pub async fn read_connector_open<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<ConnectorOpenRequest, ConnectorControlError> {
    let request: ConnectorOpenRequest = read_control_frame(stream).await?;
    request.validate()?;
    Ok(request)
}

pub async fn write_connector_ready<S: AsyncWrite + Unpin>(
    stream: &mut S,
    ready: &ConnectorReady,
) -> Result<(), ConnectorControlError> {
    ready.validate()?;
    write_control_frame(stream, ready).await
}

pub async fn read_connector_ready<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<ConnectorReady, ConnectorControlError> {
    let ready: ConnectorReady = read_control_frame(stream).await?;
    ready.validate()?;
    Ok(ready)
}

async fn write_control_frame<S, T>(stream: &mut S, value: &T) -> Result<(), ConnectorControlError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let encoded = serde_json::to_vec(value)?;
    if encoded.is_empty() || encoded.len() > MAX_CONNECTOR_CONTROL_FRAME_BYTES {
        return Err(ConnectorControlError::FrameTooLarge(encoded.len()));
    }
    stream
        .write_all(&(encoded.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&encoded).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_control_frame<S, T>(stream: &mut S) -> Result<T, ConnectorControlError>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_CONNECTOR_CONTROL_FRAME_BYTES {
        return Err(ConnectorControlError::FrameTooLarge(length));
    }
    let mut encoded = vec![0_u8; length];
    stream.read_exact(&mut encoded).await?;
    Ok(serde_json::from_slice(&encoded)?)
}

#[derive(Debug, Error)]
pub enum ConnectorControlError {
    #[error("connector control I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("connector control JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connector control frame exceeds the hard bound: {0} bytes")]
    FrameTooLarge(usize),
    #[error("unsupported connector control version {0}")]
    UnsupportedVersion(u32),
    #[error("connector control Site tag is invalid")]
    InvalidSiteTag,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpaqueForwardStats {
    pub left_to_right_bytes: u64,
    pub right_to_left_bytes: u64,
}

/// Pumps one already-established Clew tunnel without interpreting its payload.
///
/// The Connector owns only the two outer streams. Authentication, Noise state,
/// message kinds and business payloads remain exclusively at the Target and
/// Controller endpoints. `copy_bidirectional` provides bounded buffering and
/// transport backpressure; this function records only aggregate byte counts.
pub async fn forward_opaque_bidirectional<L, R>(
    left: &mut L,
    right: &mut R,
) -> Result<OpaqueForwardStats, std::io::Error>
where
    L: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + AsyncWrite + Unpin,
{
    let (left_to_right_bytes, right_to_left_bytes) = copy_bidirectional(left, right).await?;
    Ok(OpaqueForwardStats {
        left_to_right_bytes,
        right_to_left_bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
        time::Duration,
    };

    use clew_core::{DeviceId, SiteId};
    use clew_identity::{ControllerIdentity, DeviceIdentity};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use crate::{
        ControllerSessionIdentity, DeviceSessionIdentity, InnerMessage, InnerSession, IrohOuter,
        IrohProtocol, ReadReply, ReadRequest, SignedConnectorLease,
    };

    use super::*;

    #[derive(Default)]
    struct ObservedBytes {
        bytes: Mutex<Vec<u8>>,
    }

    impl ObservedBytes {
        fn push(&self, bytes: &[u8]) {
            self.bytes
                .lock()
                .expect("observation mutex poisoned")
                .extend_from_slice(bytes);
        }

        fn snapshot(&self) -> Vec<u8> {
            self.bytes
                .lock()
                .expect("observation mutex poisoned")
                .clone()
        }
    }

    struct ObservedStream<S> {
        inner: S,
        observed: Arc<ObservedBytes>,
    }

    impl<S> ObservedStream<S> {
        fn new(inner: S, observed: Arc<ObservedBytes>) -> Self {
            Self { inner, observed }
        }
    }

    impl<S: AsyncRead + Unpin> AsyncRead for ObservedStream<S> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let before = buf.filled().len();
            let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
            if let Poll::Ready(Ok(())) = &poll {
                let after = buf.filled().len();
                if after > before {
                    self.observed.push(&buf.filled()[before..after]);
                }
            }
            poll
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for ObservedStream<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match Pin::new(&mut self.inner).poll_write(cx, buf) {
                Poll::Ready(Ok(written)) => {
                    self.observed.push(&buf[..written]);
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

    fn identities() -> (ControllerSessionIdentity, DeviceSessionIdentity) {
        let controller = ControllerIdentity::from_secret([31_u8; 32]);
        let device = DeviceIdentity::from_secret([32_u8; 32]);
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        (
            ControllerSessionIdentity {
                identity: controller.clone(),
                noise_static_secret: [33_u8; 32],
                expected_device: device.public_identity(),
                device_id,
                site_id,
            },
            DeviceSessionIdentity {
                identity: device,
                pinned_controller: controller.public_identity(),
                device_id,
                site_id,
            },
        )
    }

    #[tokio::test]
    async fn control_frame_rejects_oversize_before_payload_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        let task = tokio::spawn(async move {
            writer
                .write_all(&((MAX_CONNECTOR_CONTROL_FRAME_BYTES + 1) as u32).to_be_bytes())
                .await
                .unwrap();
        });
        let error = read_connector_open(&mut reader).await.unwrap_err();
        assert!(matches!(
            error,
            ConnectorControlError::FrameTooLarge(size)
                if size == MAX_CONNECTOR_CONTROL_FRAME_BYTES + 1
        ));
        task.await.unwrap();
    }

    #[test]
    fn control_open_rejects_wrong_version_and_noncanonical_site_tag() {
        let tag = SiteDiscoveryTag::derive(
            clew_core::ControllerId::from_bytes([35_u8; 16]).unwrap(),
            SiteId::from_bytes([36_u8; 16]).unwrap(),
        );
        let mut wrong_version = ConnectorOpenRequest::new(tag, ConnectorTunnelPurpose::Bootstrap);
        wrong_version.version += 1;
        assert!(matches!(
            wrong_version.validate(),
            Err(ConnectorControlError::UnsupportedVersion(_))
        ));
        let mut wrong_tag = ConnectorOpenRequest::new(tag, ConnectorTunnelPurpose::InnerSession);
        wrong_tag.site_tag.make_ascii_uppercase();
        assert!(matches!(
            wrong_tag.validate(),
            Err(ConnectorControlError::InvalidSiteTag)
        ));
    }

    #[tokio::test]
    async fn real_iroh_connector_forwards_inner_session_ciphertext_only() {
        const SECRET_PATH: &str = "C:/connector-secret-project-20260902.txt";
        const SECRET_RESULT: &[u8] = b"CLEW-CONNECTOR-CIPHERTEXT-PROOF";

        let controller_outer = IrohOuter::bind_direct_only().await.unwrap();
        let helper_outer = IrohOuter::bind_direct_only().await.unwrap();
        let target_outer = IrohOuter::bind_direct_only().await.unwrap();
        let controller_addr = controller_outer.addr();
        let helper_addr = helper_outer.addr();
        let observed = Arc::new(ObservedBytes::default());
        let (controller_identity, device_identity) = identities();
        let pinned_controller = device_identity.pinned_controller;
        let site_id = device_identity.site_id;
        let site_tag = SiteDiscoveryTag::derive(pinned_controller.controller_id, site_id);
        let helper_device_id = DeviceId::from_bytes([34_u8; 16]).unwrap();
        let lease = SignedConnectorLease::issue(
            &controller_identity.identity,
            site_id,
            helper_device_id,
            helper_addr.id,
            1_000,
            61_000,
        )
        .unwrap();

        let controller_task = tokio::spawn({
            let controller_outer = controller_outer.clone();
            async move {
                let (protocol, mut stream) = controller_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut stream).await.unwrap();
                assert_eq!(request.validate().unwrap(), site_tag);
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                let mut inner = InnerSession::accept(&mut stream, controller_identity)
                    .await
                    .unwrap();
                let request = ReadRequest::new(SECRET_PATH, 0, 64).unwrap();
                inner
                    .send(&mut stream, &request.into_message().unwrap())
                    .await
                    .unwrap();
                let message = inner.recv(&mut stream).await.unwrap();
                assert_eq!(
                    ReadReply::from_message(&message).unwrap(),
                    ReadReply::Data(SECRET_RESULT.to_vec())
                );
                inner
                    .send(
                        &mut stream,
                        &InnerMessage::new("connector_test_ack", Vec::new()).unwrap(),
                    )
                    .await
                    .unwrap();
                stream
            }
        });

        let helper_task = tokio::spawn({
            let helper_outer = helper_outer.clone();
            let observed = Arc::clone(&observed);
            async move {
                let (protocol, mut inbound) = helper_outer.accept_classified().await.unwrap();
                assert_eq!(protocol, IrohProtocol::Connector);
                let request = read_connector_open(&mut inbound).await.unwrap();
                assert_eq!(request.validate().unwrap(), site_tag);
                assert_eq!(request.purpose, ConnectorTunnelPurpose::InnerSession);
                write_connector_ready(&mut inbound, &ConnectorReady::new(lease))
                    .await
                    .unwrap();
                let mut outbound = helper_outer
                    .connect_connector(controller_addr)
                    .await
                    .unwrap();
                write_connector_open(&mut outbound, &request).await.unwrap();
                let mut inbound = ObservedStream::new(inbound, Arc::clone(&observed));
                let mut outbound = ObservedStream::new(outbound, observed);
                forward_opaque_bidirectional(&mut inbound, &mut outbound)
                    .await
                    .unwrap()
            }
        });

        let target_client = target_outer.clone();
        let helper_endpoint_id = helper_addr.id;
        let target_task = tokio::spawn(async move {
            let mut stream = target_client.connect_connector(helper_addr).await.unwrap();
            write_connector_open(
                &mut stream,
                &ConnectorOpenRequest::new(site_tag, ConnectorTunnelPurpose::InnerSession),
            )
            .await
            .unwrap();
            let ready = read_connector_ready(&mut stream).await.unwrap();
            assert_eq!(
                ready
                    .lease
                    .verify_for_candidate(&pinned_controller, site_id, helper_endpoint_id, 2_000)
                    .unwrap(),
                helper_device_id
            );
            let mut inner = InnerSession::connect(&mut stream, device_identity)
                .await
                .unwrap();
            let message = inner.recv(&mut stream).await.unwrap();
            let request = ReadRequest::from_message(&message).unwrap();
            assert_eq!(request.path, SECRET_PATH);
            inner
                .send(
                    &mut stream,
                    &ReadReply::data(SECRET_RESULT.to_vec())
                        .unwrap()
                        .into_message()
                        .unwrap(),
                )
                .await
                .unwrap();
            let ack = inner.recv(&mut stream).await.unwrap();
            assert_eq!(ack.kind, "connector_test_ack");
            assert!(ack.payload.is_empty());
            stream
        });

        let (controller_stream, target_stream) =
            tokio::time::timeout(Duration::from_secs(15), async {
                let controller_stream = controller_task.await.unwrap();
                let target_stream = target_task.await.unwrap();
                (controller_stream, target_stream)
            })
            .await
            .expect("connector end-to-end exchange timed out");
        let observed = observed.snapshot();
        assert!(!observed.is_empty());
        assert!(!contains(&observed, b"read"));
        assert!(!contains(&observed, SECRET_PATH.as_bytes()));
        assert!(!contains(&observed, SECRET_RESULT));

        helper_task.abort();
        let cancelled = helper_task.await.unwrap_err();
        assert!(cancelled.is_cancelled());
        drop(target_stream);
        drop(controller_stream);
        target_outer.close().await;
        helper_outer.close().await;
        controller_outer.close().await;
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
