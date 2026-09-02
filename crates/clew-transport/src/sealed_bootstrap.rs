use std::{str::FromStr, time::Duration};

use clew_core::{ControllerId, SiteId};
use serde::{Serialize, de::DeserializeOwned};
use snow::{
    Builder, HandshakeState, TransportState,
    params::NoiseParams,
    resolvers::{CryptoResolver, DefaultResolver},
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

const SEALED_BOOTSTRAP_NOISE_PATTERN: &str = "Noise_NK_25519_ChaChaPoly_BLAKE2s";
const SEALED_BOOTSTRAP_PROLOGUE_DOMAIN: &[u8] = b"clew/sealed-bootstrap/v1\0";
const SEALED_BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_NOISE_PACKET: usize = 65_535;
pub const MAX_SEALED_BOOTSTRAP_PLAINTEXT: usize = 60 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedBootstrapContext {
    pub controller_id: ControllerId,
    pub site_id: SiteId,
}

#[derive(Debug)]
pub struct SealedBootstrapSession {
    transport: TransportState,
    poisoned: bool,
}

impl SealedBootstrapSession {
    pub async fn connect<S>(
        stream: &mut S,
        context: SealedBootstrapContext,
        controller_bootstrap_noise_public_key: [u8; 32],
    ) -> Result<Self, SealedBootstrapError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if controller_bootstrap_noise_public_key == [0_u8; 32] {
            return Err(SealedBootstrapError::InvalidStaticKey);
        }
        let prologue = prologue(context);
        let builder = Builder::new(noise_params()?)
            .prologue(&prologue)?
            .remote_public_key(&controller_bootstrap_noise_public_key)?;
        let mut handshake = builder.build_initiator()?;
        initiator_handshake(stream, &mut handshake).await?;
        Ok(Self {
            transport: handshake.into_transport_mode()?,
            poisoned: false,
        })
    }

    pub async fn accept<S>(
        stream: &mut S,
        context: SealedBootstrapContext,
        controller_bootstrap_noise_static_secret: [u8; 32],
    ) -> Result<Self, SealedBootstrapError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let prologue = prologue(context);
        let builder = Builder::new(noise_params()?)
            .prologue(&prologue)?
            .local_private_key(&controller_bootstrap_noise_static_secret)?;
        let mut handshake = builder.build_responder()?;
        responder_handshake(stream, &mut handshake).await?;
        Ok(Self {
            transport: handshake.into_transport_mode()?,
            poisoned: false,
        })
    }

    pub async fn send<S, T>(
        &mut self,
        stream: &mut S,
        value: &T,
    ) -> Result<(), SealedBootstrapError>
    where
        S: AsyncWrite + Unpin,
        T: Serialize,
    {
        if self.poisoned {
            return Err(SealedBootstrapError::SessionPoisoned);
        }
        let result = write_encrypted_json(stream, &mut self.transport, value).await;
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    pub async fn recv<S, T>(&mut self, stream: &mut S) -> Result<T, SealedBootstrapError>
    where
        S: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        if self.poisoned {
            return Err(SealedBootstrapError::SessionPoisoned);
        }
        let result = read_encrypted_json(stream, &mut self.transport).await;
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

pub fn noise_static_public(
    controller_bootstrap_noise_static_secret: [u8; 32],
) -> Result<[u8; 32], SealedBootstrapError> {
    let params = noise_params()?;
    let resolver = DefaultResolver;
    let mut dh = resolver
        .resolve_dh(&params.dh)
        .ok_or(SealedBootstrapError::DhUnavailable)?;
    if dh.priv_len() != 32 || dh.pub_len() != 32 {
        return Err(SealedBootstrapError::InvalidStaticKey);
    }
    dh.set(&controller_bootstrap_noise_static_secret);
    let public = dh.pubkey();
    if public.len() != 32 {
        return Err(SealedBootstrapError::InvalidStaticKey);
    }
    let mut result = [0_u8; 32];
    result.copy_from_slice(public);
    if result == [0_u8; 32] {
        return Err(SealedBootstrapError::InvalidStaticKey);
    }
    Ok(result)
}

fn noise_params() -> Result<NoiseParams, SealedBootstrapError> {
    NoiseParams::from_str(SEALED_BOOTSTRAP_NOISE_PATTERN)
        .map_err(|_| SealedBootstrapError::InvalidNoisePattern)
}

fn prologue(context: SealedBootstrapContext) -> Vec<u8> {
    let mut value = Vec::with_capacity(SEALED_BOOTSTRAP_PROLOGUE_DOMAIN.len() + 4 + 16 + 16);
    value.extend_from_slice(SEALED_BOOTSTRAP_PROLOGUE_DOMAIN);
    value.extend_from_slice(&clew_proto::WIRE_MAJOR.to_be_bytes());
    value.extend_from_slice(context.controller_id.as_bytes());
    value.extend_from_slice(context.site_id.as_bytes());
    value
}

async fn initiator_handshake<S>(
    stream: &mut S,
    handshake: &mut HandshakeState,
) -> Result<(), SealedBootstrapError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut output = vec![0_u8; MAX_NOISE_PACKET];
    let written = handshake.write_message(&[], &mut output)?;
    write_packet_timed(stream, &output[..written]).await?;
    let packet = read_packet_timed(stream).await?;
    let mut scratch = vec![0_u8; MAX_NOISE_PACKET];
    handshake.read_message(&packet, &mut scratch)?;
    if !handshake.is_handshake_finished() {
        return Err(SealedBootstrapError::HandshakeIncomplete);
    }
    Ok(())
}

async fn responder_handshake<S>(
    stream: &mut S,
    handshake: &mut HandshakeState,
) -> Result<(), SealedBootstrapError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let packet = read_packet_timed(stream).await?;
    let mut scratch = vec![0_u8; MAX_NOISE_PACKET];
    handshake.read_message(&packet, &mut scratch)?;
    let mut output = vec![0_u8; MAX_NOISE_PACKET];
    let written = handshake.write_message(&[], &mut output)?;
    write_packet_timed(stream, &output[..written]).await?;
    if !handshake.is_handshake_finished() {
        return Err(SealedBootstrapError::HandshakeIncomplete);
    }
    Ok(())
}

async fn write_encrypted_json<S, T>(
    stream: &mut S,
    transport: &mut TransportState,
    value: &T,
) -> Result<(), SealedBootstrapError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let plaintext = serde_json::to_vec(value)?;
    if plaintext.is_empty() || plaintext.len() > MAX_SEALED_BOOTSTRAP_PLAINTEXT {
        return Err(SealedBootstrapError::PlaintextTooLarge(plaintext.len()));
    }
    let mut ciphertext = vec![0_u8; plaintext.len() + 16];
    let written = transport.write_message(&plaintext, &mut ciphertext)?;
    write_packet_timed(stream, &ciphertext[..written]).await
}

async fn read_encrypted_json<S, T>(
    stream: &mut S,
    transport: &mut TransportState,
) -> Result<T, SealedBootstrapError>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let ciphertext = read_packet_timed(stream).await?;
    let mut plaintext = vec![0_u8; ciphertext.len()];
    let written = transport.read_message(&ciphertext, &mut plaintext)?;
    plaintext.truncate(written);
    if plaintext.is_empty() || plaintext.len() > MAX_SEALED_BOOTSTRAP_PLAINTEXT {
        return Err(SealedBootstrapError::PlaintextTooLarge(plaintext.len()));
    }
    Ok(serde_json::from_slice(&plaintext)?)
}

async fn write_packet_timed<S>(stream: &mut S, payload: &[u8]) -> Result<(), SealedBootstrapError>
where
    S: AsyncWrite + Unpin,
{
    timeout(SEALED_BOOTSTRAP_TIMEOUT, write_packet(stream, payload))
        .await
        .map_err(|_| SealedBootstrapError::Timeout)??;
    Ok(())
}

async fn read_packet_timed<S>(stream: &mut S) -> Result<Vec<u8>, SealedBootstrapError>
where
    S: AsyncRead + Unpin,
{
    timeout(SEALED_BOOTSTRAP_TIMEOUT, read_packet(stream))
        .await
        .map_err(|_| SealedBootstrapError::Timeout)?
}

async fn write_packet<S>(stream: &mut S, payload: &[u8]) -> Result<(), SealedBootstrapError>
where
    S: AsyncWrite + Unpin,
{
    if payload.is_empty() || payload.len() > MAX_NOISE_PACKET {
        return Err(SealedBootstrapError::CiphertextTooLarge(payload.len()));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_packet<S>(stream: &mut S) -> Result<Vec<u8>, SealedBootstrapError>
where
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_NOISE_PACKET {
        return Err(SealedBootstrapError::CiphertextTooLarge(length));
    }
    let mut packet = vec![0_u8; length];
    stream.read_exact(&mut packet).await?;
    Ok(packet)
}

#[derive(Debug, Error)]
pub enum SealedBootstrapError {
    #[error("invalid built-in sealed-bootstrap Noise pattern")]
    InvalidNoisePattern,
    #[error("sealed-bootstrap Noise DH implementation is unavailable")]
    DhUnavailable,
    #[error("sealed-bootstrap Controller static key is invalid")]
    InvalidStaticKey,
    #[error("sealed-bootstrap Noise protocol failed: {0}")]
    Noise(#[from] snow::Error),
    #[error("sealed-bootstrap JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("sealed-bootstrap I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("sealed-bootstrap I/O timed out")]
    Timeout,
    #[error("sealed-bootstrap handshake did not reach transport mode")]
    HandshakeIncomplete,
    #[error("sealed-bootstrap ciphertext is invalid or too large: {0} bytes")]
    CiphertextTooLarge(usize),
    #[error("sealed-bootstrap plaintext exceeds the hard bound: {0} bytes")]
    PlaintextTooLarge(usize),
    #[error("sealed-bootstrap session is poisoned after a prior protocol/authentication failure")]
    SessionPoisoned,
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use clew_core::{InviteId, MemberCapabilities};
    use clew_identity::{ControllerIdentity, DeviceIdentity, PermissionGrant, SiteBootstrapSpec};
    use tokio::io::ReadBuf;

    use crate::{BootstrapErrorBody, BootstrapErrorCode, BootstrapRequest, BootstrapResponse};

    use super::*;

    #[derive(Default)]
    struct Tap(Mutex<Vec<u8>>);

    impl Tap {
        fn push(&self, bytes: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(bytes);
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    struct Tapped<S> {
        inner: S,
        tap: Arc<Tap>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Tapped<S> {
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
                    self.tap.push(&buf.filled()[before..after]);
                }
            }
            poll
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Tapped<S> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match Pin::new(&mut self.inner).poll_write(cx, buf) {
                Poll::Ready(Ok(written)) => {
                    self.tap.push(&buf[..written]);
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

    #[test]
    fn static_public_key_is_stable_and_nonzero() {
        let first = noise_static_public([91_u8; 32]).unwrap();
        assert_eq!(first, noise_static_public([91_u8; 32]).unwrap());
        assert_ne!(first, [0_u8; 32]);
        assert_ne!(first, noise_static_public([92_u8; 32]).unwrap());
    }

    #[tokio::test]
    async fn actual_bootstrap_request_and_response_are_ciphertext_on_the_wire() {
        const SECRET_SITE: &str = "SEALED-BOOTSTRAP-SECRET-SITE";
        const SECRET_HOST: &str = "SEALED-BOOTSTRAP-SECRET-HOST";
        const SECRET_RESPONSE: &str = "SEALED-BOOTSTRAP-SECRET-RESPONSE";

        let controller = ControllerIdentity::from_secret([93_u8; 32]);
        let noise_secret = [94_u8; 32];
        let site_id = SiteId::from_bytes([95_u8; 16]).unwrap();
        let invite_id = InviteId::from_bytes([96_u8; 16]).unwrap();
        let bootstrap = controller
            .issue_site_bootstrap(SiteBootstrapSpec {
                site_id,
                invite_id,
                site_name: SECRET_SITE.into(),
                grant: PermissionGrant {
                    member: MemberCapabilities::EXECUTE_ONLY,
                    read: true,
                    write: false,
                    shell: false,
                },
                not_before_unix_ms: 1,
                expires_unix_ms: u64::MAX - 1,
                deployment_window_ms: 60_000,
                max_claims: 4,
            })
            .unwrap();
        let device = DeviceIdentity::from_secret([97_u8; 32]);
        let request = BootstrapRequest::Claim {
            bootstrap,
            device_identity: device.public_identity(),
            hostname: SECRET_HOST.into(),
            mode: crate::BootstrapMemberMode::ExecutePreferred,
        };
        let response = BootstrapResponse::Error(BootstrapErrorBody::new(
            BootstrapErrorCode::Denied,
            SECRET_RESPONSE,
        ));
        let context = SealedBootstrapContext {
            controller_id: controller.controller_id(),
            site_id,
        };
        let public = noise_static_public(noise_secret).unwrap();
        let tap = Arc::new(Tap::default());
        let (left, right) = tokio::io::duplex(128 * 1024);
        let mut target_stream = Tapped {
            inner: left,
            tap: Arc::clone(&tap),
        };
        let mut controller_stream = Tapped {
            inner: right,
            tap: Arc::clone(&tap),
        };

        let controller_task = tokio::spawn(async move {
            let mut session =
                SealedBootstrapSession::accept(&mut controller_stream, context, noise_secret)
                    .await
                    .unwrap();
            let received: BootstrapRequest = session.recv(&mut controller_stream).await.unwrap();
            match received {
                BootstrapRequest::Claim { hostname, .. } => assert_eq!(hostname, SECRET_HOST),
                _ => panic!("unexpected sealed bootstrap request"),
            }
            session
                .send(&mut controller_stream, &response)
                .await
                .unwrap();
            controller_stream
        });

        let mut target_session =
            SealedBootstrapSession::connect(&mut target_stream, context, public)
                .await
                .unwrap();
        target_session
            .send(&mut target_stream, &request)
            .await
            .unwrap();
        let received: BootstrapResponse = target_session.recv(&mut target_stream).await.unwrap();
        match received {
            BootstrapResponse::Error(error) => assert_eq!(error.message, SECRET_RESPONSE),
            _ => panic!("unexpected sealed bootstrap response"),
        }
        let controller_stream = controller_task.await.unwrap();
        drop(controller_stream);
        drop(target_stream);

        let wire = tap.bytes();
        assert!(!contains(&wire, SECRET_SITE.as_bytes()));
        assert!(!contains(&wire, SECRET_HOST.as_bytes()));
        assert!(!contains(&wire, SECRET_RESPONSE.as_bytes()));
    }

    #[tokio::test]
    async fn wrong_controller_pin_or_site_context_fails_closed() {
        let controller = ControllerIdentity::from_secret([101_u8; 32]);
        let secret = [102_u8; 32];
        let context = SealedBootstrapContext {
            controller_id: controller.controller_id(),
            site_id: SiteId::from_bytes([103_u8; 16]).unwrap(),
        };
        let wrong_site = SealedBootstrapContext {
            controller_id: context.controller_id,
            site_id: SiteId::from_bytes([104_u8; 16]).unwrap(),
        };
        let (mut left, mut right) = tokio::io::duplex(16 * 1024);
        let responder = tokio::spawn(async move {
            SealedBootstrapSession::accept(&mut right, context, secret).await
        });
        let result = SealedBootstrapSession::connect(
            &mut left,
            wrong_site,
            noise_static_public(secret).unwrap(),
        )
        .await;
        assert!(result.is_err());
        let _ = responder.await;

        let (mut left, mut right) = tokio::io::duplex(16 * 1024);
        let responder = tokio::spawn(async move {
            SealedBootstrapSession::accept(&mut right, context, secret).await
        });
        let result = SealedBootstrapSession::connect(
            &mut left,
            context,
            noise_static_public([105_u8; 32]).unwrap(),
        )
        .await;
        assert!(result.is_err());
        let _ = responder.await;
    }

    #[tokio::test]
    async fn oversized_ciphertext_length_fails_before_allocation() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        let task = tokio::spawn(async move {
            writer
                .write_u32((MAX_NOISE_PACKET + 1) as u32)
                .await
                .unwrap();
        });
        assert!(matches!(
            read_packet(&mut reader).await,
            Err(SealedBootstrapError::CiphertextTooLarge(size)) if size == MAX_NOISE_PACKET + 1
        ));
        task.await.unwrap();
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
