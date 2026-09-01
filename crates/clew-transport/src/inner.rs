use std::{
    str::{self, FromStr},
    time::Duration,
};

use clew_core::{ControllerId, DeviceId, SiteId};
use clew_identity::{
    ActiveDeviceIdentity, ControllerIdentity, ControllerPublicIdentity, DeviceIdentity,
    DevicePublicIdentity, IdentityError, StoredControllerIdentity,
};
use serde::{Deserialize, Serialize};
use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_NOISE_PACKET: usize = 65_535;
const MAX_IDENTITY_PROOF: usize = 8 * 1024;
pub const MAX_INNER_PLAINTEXT: usize = 60 * 1024;
const MAX_MESSAGE_KIND_BYTES: usize = 64;
const BUSINESS_FRAME_HEADER_BYTES: usize = 4 + 8 + 1 + 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InnerRole {
    Controller,
    Device,
}

#[derive(Clone)]
pub struct ControllerSessionIdentity {
    pub identity: ControllerIdentity,
    pub noise_static_secret: [u8; 32],
    pub expected_device: DevicePublicIdentity,
    pub device_id: DeviceId,
    pub site_id: SiteId,
}

impl ControllerSessionIdentity {
    #[must_use]
    pub fn from_stored(
        stored: &StoredControllerIdentity,
        expected_device: DevicePublicIdentity,
        device_id: DeviceId,
        site_id: SiteId,
    ) -> Self {
        Self {
            identity: stored.identity().clone(),
            noise_static_secret: stored.noise_static_secret(),
            expected_device,
            device_id,
            site_id,
        }
    }
}

#[derive(Clone)]
pub struct ControllerSessionAuthority {
    pub identity: ControllerIdentity,
    pub noise_static_secret: [u8; 32],
}

impl ControllerSessionAuthority {
    #[must_use]
    pub fn from_stored(stored: &StoredControllerIdentity) -> Self {
        Self {
            identity: stored.identity().clone(),
            noise_static_secret: stored.noise_static_secret(),
        }
    }
}

impl std::fmt::Debug for ControllerSessionAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerSessionAuthority")
            .field("controller_id", &self.identity.controller_id())
            .field("noise_static_secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceSessionClaim {
    pub device_identity: DevicePublicIdentity,
    pub device_id: DeviceId,
    pub site_id: SiteId,
}

impl std::fmt::Debug for ControllerSessionIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerSessionIdentity")
            .field("controller_id", &self.identity.controller_id())
            .field("expected_device", &self.expected_device)
            .field("device_id", &self.device_id)
            .field("site_id", &self.site_id)
            .field("noise_static_secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
pub struct DeviceSessionIdentity {
    pub identity: DeviceIdentity,
    pub pinned_controller: ControllerPublicIdentity,
    pub device_id: DeviceId,
    pub site_id: SiteId,
}

impl DeviceSessionIdentity {
    #[must_use]
    pub fn from_active(active: &ActiveDeviceIdentity) -> Self {
        Self {
            identity: active.identity().clone(),
            pinned_controller: active.controller(),
            device_id: active.device_id(),
            site_id: active.site_id(),
        }
    }
}

impl std::fmt::Debug for DeviceSessionIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceSessionIdentity")
            .field("device", &self.identity.public_identity())
            .field("pinned_controller", &self.pinned_controller)
            .field("device_id", &self.device_id)
            .field("site_id", &self.site_id)
            .field("noise_static_secret", &"[DERIVED+REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InnerMessage {
    pub kind: String,
    pub payload: Vec<u8>,
}

impl InnerMessage {
    pub fn new(kind: impl Into<String>, payload: Vec<u8>) -> Result<Self, InnerSessionError> {
        let message = Self {
            kind: kind.into(),
            payload,
        };
        message.validate()?;
        Ok(message)
    }

    fn validate(&self) -> Result<(), InnerSessionError> {
        if self.kind.is_empty() || self.kind.len() > MAX_MESSAGE_KIND_BYTES {
            return Err(InnerSessionError::InvalidMessageKind);
        }
        let encoded_len = BUSINESS_FRAME_HEADER_BYTES
            .checked_add(self.kind.len())
            .and_then(|value| value.checked_add(self.payload.len()))
            .ok_or(InnerSessionError::PlaintextTooLarge(usize::MAX))?;
        if encoded_len > MAX_INNER_PLAINTEXT {
            return Err(InnerSessionError::PlaintextTooLarge(encoded_len));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct InnerSession {
    transport: TransportState,
    send_sequence: u64,
    recv_sequence: u64,
    poisoned: bool,
    peer_role: InnerRole,
    controller_id: ControllerId,
    device_id: DeviceId,
    site_id: SiteId,
}

impl InnerSession {
    pub async fn connect<S>(
        stream: &mut S,
        identity: DeviceSessionIdentity,
    ) -> Result<Self, InnerSessionError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        identity.pinned_controller.validate()?;
        identity.identity.public_identity().validate()?;
        let mut handshake = build_handshake(true, &identity.identity.noise_static_secret())?;
        initiator_handshake(stream, &mut handshake).await?;
        let transcript = handshake.get_handshake_hash().to_vec();
        let mut transport = handshake.into_transport_mode()?;

        let body = DeviceProofBody {
            wire_major: clew_proto::WIRE_MAJOR,
            role: InnerRole::Device,
            transcript_hash: transcript.clone(),
            controller_id: identity.pinned_controller.controller_id,
            site_id: identity.site_id,
            device_id: identity.device_id,
            device_identity: identity.identity.public_identity(),
        };
        let proof = DeviceProof {
            signature: identity.identity.sign_session_binding(&body)?,
            body,
        };
        write_encrypted_json(stream, &mut transport, &proof, MAX_IDENTITY_PROOF).await?;
        let controller_proof: ControllerProof =
            read_encrypted_json(stream, &mut transport, MAX_IDENTITY_PROOF).await?;
        validate_controller_proof(
            &controller_proof,
            &identity.pinned_controller,
            &transcript,
            identity.site_id,
            identity.device_id,
        )?;

        Ok(Self {
            transport,
            send_sequence: 0,
            recv_sequence: 0,
            poisoned: false,
            peer_role: InnerRole::Controller,
            controller_id: identity.pinned_controller.controller_id,
            device_id: identity.device_id,
            site_id: identity.site_id,
        })
    }

    pub async fn accept<S>(
        stream: &mut S,
        identity: ControllerSessionIdentity,
    ) -> Result<Self, InnerSessionError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        identity.expected_device.validate()?;
        let expected_device = identity.expected_device;
        let expected_device_id = identity.device_id;
        let expected_site_id = identity.site_id;
        Self::accept_authorized(
            stream,
            ControllerSessionAuthority {
                identity: identity.identity,
                noise_static_secret: identity.noise_static_secret,
            },
            move |claim| {
                claim.device_identity == expected_device
                    && claim.device_id == expected_device_id
                    && claim.site_id == expected_site_id
            },
        )
        .await
    }

    pub async fn accept_authorized<S, F>(
        stream: &mut S,
        authority: ControllerSessionAuthority,
        authorize: F,
    ) -> Result<Self, InnerSessionError>
    where
        S: AsyncRead + AsyncWrite + Unpin,
        F: FnOnce(&DeviceSessionClaim) -> bool,
    {
        authority.identity.public_identity().validate()?;
        let mut handshake = build_handshake(false, &authority.noise_static_secret)?;
        responder_handshake(stream, &mut handshake).await?;
        let transcript = handshake.get_handshake_hash().to_vec();
        let mut transport = handshake.into_transport_mode()?;

        let device_proof: DeviceProof =
            read_encrypted_json(stream, &mut transport, MAX_IDENTITY_PROOF).await?;
        let claim = validate_untrusted_device_proof(
            &device_proof,
            authority.identity.controller_id(),
            &transcript,
        )?;
        if !authorize(&claim) {
            return Err(InnerSessionError::IdentityBindingMismatch);
        }

        let body = ControllerProofBody {
            wire_major: clew_proto::WIRE_MAJOR,
            role: InnerRole::Controller,
            transcript_hash: transcript,
            controller_id: authority.identity.controller_id(),
            site_id: claim.site_id,
            device_id: claim.device_id,
            controller_identity: authority.identity.public_identity(),
        };
        let proof = ControllerProof {
            signature: authority.identity.sign_session_binding(&body)?,
            body,
        };
        write_encrypted_json(stream, &mut transport, &proof, MAX_IDENTITY_PROOF).await?;

        Ok(Self {
            transport,
            send_sequence: 0,
            recv_sequence: 0,
            poisoned: false,
            peer_role: InnerRole::Device,
            controller_id: authority.identity.controller_id(),
            device_id: claim.device_id,
            site_id: claim.site_id,
        })
    }

    #[must_use]
    pub const fn peer_role(&self) -> InnerRole {
        self.peer_role
    }

    #[must_use]
    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device_id
    }

    #[must_use]
    pub const fn site_id(&self) -> SiteId {
        self.site_id
    }

    pub async fn send<S>(
        &mut self,
        stream: &mut S,
        message: &InnerMessage,
    ) -> Result<(), InnerSessionError>
    where
        S: AsyncWrite + Unpin,
    {
        let ciphertext = self.seal(message)?;
        if let Err(error) = write_packet(stream, &ciphertext).await {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    pub async fn recv<S>(&mut self, stream: &mut S) -> Result<InnerMessage, InnerSessionError>
    where
        S: AsyncRead + Unpin,
    {
        if self.poisoned {
            return Err(InnerSessionError::SessionPoisoned);
        }
        let ciphertext = match read_packet(stream).await {
            Ok(ciphertext) => ciphertext,
            Err(error) => {
                self.poisoned = true;
                return Err(error);
            }
        };
        self.open(&ciphertext)
    }

    fn seal(&mut self, message: &InnerMessage) -> Result<Vec<u8>, InnerSessionError> {
        if self.poisoned {
            return Err(InnerSessionError::SessionPoisoned);
        }
        message.validate()?;
        let plaintext = encode_business_frame(self.send_sequence, message)?;
        let mut ciphertext = vec![0_u8; plaintext.len() + 16];
        let written = self.transport.write_message(&plaintext, &mut ciphertext)?;
        ciphertext.truncate(written);
        self.send_sequence = self
            .send_sequence
            .checked_add(1)
            .ok_or(InnerSessionError::SequenceExhausted)?;
        Ok(ciphertext)
    }

    fn open(&mut self, ciphertext: &[u8]) -> Result<InnerMessage, InnerSessionError> {
        if self.poisoned {
            return Err(InnerSessionError::SessionPoisoned);
        }
        let result = (|| {
            if ciphertext.len() > MAX_NOISE_PACKET {
                return Err(InnerSessionError::CiphertextTooLarge(ciphertext.len()));
            }
            let mut plaintext = vec![0_u8; ciphertext.len()];
            let written = self.transport.read_message(ciphertext, &mut plaintext)?;
            plaintext.truncate(written);
            if plaintext.len() > MAX_INNER_PLAINTEXT {
                return Err(InnerSessionError::PlaintextTooLarge(plaintext.len()));
            }
            let (wire_major, sequence, message) = decode_business_frame(&plaintext)?;
            if wire_major != clew_proto::WIRE_MAJOR {
                return Err(InnerSessionError::WrongWireMajor(wire_major));
            }
            if sequence != self.recv_sequence {
                return Err(InnerSessionError::UnexpectedSequence {
                    expected: self.recv_sequence,
                    actual: sequence,
                });
            }
            message.validate()?;
            self.recv_sequence = self
                .recv_sequence
                .checked_add(1)
                .ok_or(InnerSessionError::SequenceExhausted)?;
            Ok(message)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
}

fn encode_business_frame(
    sequence: u64,
    message: &InnerMessage,
) -> Result<Vec<u8>, InnerSessionError> {
    message.validate()?;
    let kind_len: u8 = message
        .kind
        .len()
        .try_into()
        .map_err(|_| InnerSessionError::InvalidMessageKind)?;
    let payload_len: u32 = message
        .payload
        .len()
        .try_into()
        .map_err(|_| InnerSessionError::PlaintextTooLarge(message.payload.len()))?;
    let total = BUSINESS_FRAME_HEADER_BYTES
        .checked_add(kind_len as usize)
        .and_then(|value| value.checked_add(payload_len as usize))
        .ok_or(InnerSessionError::MalformedBusinessFrame)?;
    if total > MAX_INNER_PLAINTEXT {
        return Err(InnerSessionError::PlaintextTooLarge(total));
    }
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&clew_proto::WIRE_MAJOR.to_be_bytes());
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.push(kind_len);
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(message.kind.as_bytes());
    frame.extend_from_slice(&message.payload);
    Ok(frame)
}

fn decode_business_frame(frame: &[u8]) -> Result<(u32, u64, InnerMessage), InnerSessionError> {
    if frame.len() < BUSINESS_FRAME_HEADER_BYTES || frame.len() > MAX_INNER_PLAINTEXT {
        return Err(InnerSessionError::MalformedBusinessFrame);
    }
    let wire_major = u32::from_be_bytes(frame[0..4].try_into().expect("fixed header slice"));
    let sequence = u64::from_be_bytes(frame[4..12].try_into().expect("fixed header slice"));
    let kind_len = frame[12] as usize;
    let payload_len =
        u32::from_be_bytes(frame[13..17].try_into().expect("fixed header slice")) as usize;
    if kind_len == 0 || kind_len > MAX_MESSAGE_KIND_BYTES {
        return Err(InnerSessionError::InvalidMessageKind);
    }
    let kind_start = BUSINESS_FRAME_HEADER_BYTES;
    let payload_start = kind_start
        .checked_add(kind_len)
        .ok_or(InnerSessionError::MalformedBusinessFrame)?;
    let end = payload_start
        .checked_add(payload_len)
        .ok_or(InnerSessionError::MalformedBusinessFrame)?;
    if end != frame.len() {
        return Err(InnerSessionError::MalformedBusinessFrame);
    }
    let kind = str::from_utf8(&frame[kind_start..payload_start])
        .map_err(|_| InnerSessionError::InvalidMessageKindEncoding)?
        .to_owned();
    let message = InnerMessage {
        kind,
        payload: frame[payload_start..end].to_vec(),
    };
    message.validate()?;
    Ok((wire_major, sequence, message))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DeviceProofBody {
    wire_major: u32,
    role: InnerRole,
    transcript_hash: Vec<u8>,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    device_identity: DevicePublicIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct DeviceProof {
    body: DeviceProofBody,
    signature: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ControllerProofBody {
    wire_major: u32,
    role: InnerRole,
    transcript_hash: Vec<u8>,
    controller_id: ControllerId,
    site_id: SiteId,
    device_id: DeviceId,
    controller_identity: ControllerPublicIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ControllerProof {
    body: ControllerProofBody,
    signature: Vec<u8>,
}

fn validate_untrusted_device_proof(
    proof: &DeviceProof,
    controller_id: ControllerId,
    transcript: &[u8],
) -> Result<DeviceSessionClaim, InnerSessionError> {
    if proof.body.wire_major != clew_proto::WIRE_MAJOR
        || proof.body.role != InnerRole::Device
        || proof.body.transcript_hash != transcript
        || proof.body.controller_id != controller_id
    {
        return Err(InnerSessionError::IdentityBindingMismatch);
    }
    proof.body.device_identity.validate()?;
    proof
        .body
        .device_identity
        .verify_session_binding(&proof.body, &proof.signature)?;
    Ok(DeviceSessionClaim {
        device_identity: proof.body.device_identity,
        device_id: proof.body.device_id,
        site_id: proof.body.site_id,
    })
}

fn validate_controller_proof(
    proof: &ControllerProof,
    expected: &ControllerPublicIdentity,
    transcript: &[u8],
    site_id: SiteId,
    device_id: DeviceId,
) -> Result<(), InnerSessionError> {
    if proof.body.wire_major != clew_proto::WIRE_MAJOR
        || proof.body.role != InnerRole::Controller
        || proof.body.transcript_hash != transcript
        || proof.body.controller_id != expected.controller_id
        || proof.body.site_id != site_id
        || proof.body.device_id != device_id
        || &proof.body.controller_identity != expected
    {
        return Err(InnerSessionError::IdentityBindingMismatch);
    }
    expected.verify_session_binding(&proof.body, &proof.signature)?;
    Ok(())
}

fn build_handshake(
    initiator: bool,
    static_secret: &[u8; 32],
) -> Result<HandshakeState, InnerSessionError> {
    let params =
        NoiseParams::from_str(NOISE_PATTERN).map_err(|_| InnerSessionError::InvalidNoisePattern)?;
    let builder = Builder::new(params).local_private_key(static_secret)?;
    if initiator {
        Ok(builder.build_initiator()?)
    } else {
        Ok(builder.build_responder()?)
    }
}

async fn initiator_handshake<S>(
    stream: &mut S,
    handshake: &mut HandshakeState,
) -> Result<(), InnerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut output = vec![0_u8; MAX_NOISE_PACKET];
    let written = handshake.write_message(&[], &mut output)?;
    write_packet_timed(stream, &output[..written]).await?;

    let packet = read_packet_timed(stream).await?;
    let mut scratch = vec![0_u8; MAX_NOISE_PACKET];
    handshake.read_message(&packet, &mut scratch)?;

    let written = handshake.write_message(&[], &mut output)?;
    write_packet_timed(stream, &output[..written]).await
}

async fn responder_handshake<S>(
    stream: &mut S,
    handshake: &mut HandshakeState,
) -> Result<(), InnerSessionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let packet = read_packet_timed(stream).await?;
    let mut scratch = vec![0_u8; MAX_NOISE_PACKET];
    handshake.read_message(&packet, &mut scratch)?;

    let mut output = vec![0_u8; MAX_NOISE_PACKET];
    let written = handshake.write_message(&[], &mut output)?;
    write_packet_timed(stream, &output[..written]).await?;

    let packet = read_packet_timed(stream).await?;
    handshake.read_message(&packet, &mut scratch)?;
    Ok(())
}

async fn write_encrypted_json<S, T>(
    stream: &mut S,
    transport: &mut TransportState,
    value: &T,
    max_plaintext: usize,
) -> Result<(), InnerSessionError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let plaintext = serde_json::to_vec(value)?;
    if plaintext.len() > max_plaintext {
        return Err(InnerSessionError::IdentityProofTooLarge(plaintext.len()));
    }
    let mut ciphertext = vec![0_u8; plaintext.len() + 16];
    let written = transport.write_message(&plaintext, &mut ciphertext)?;
    write_packet_timed(stream, &ciphertext[..written]).await
}

async fn read_encrypted_json<S, T>(
    stream: &mut S,
    transport: &mut TransportState,
    max_plaintext: usize,
) -> Result<T, InnerSessionError>
where
    S: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let ciphertext = read_packet_timed(stream).await?;
    let mut plaintext = vec![0_u8; ciphertext.len()];
    let written = transport.read_message(&ciphertext, &mut plaintext)?;
    plaintext.truncate(written);
    if plaintext.len() > max_plaintext {
        return Err(InnerSessionError::IdentityProofTooLarge(plaintext.len()));
    }
    Ok(serde_json::from_slice(&plaintext)?)
}

async fn write_packet_timed<S>(stream: &mut S, payload: &[u8]) -> Result<(), InnerSessionError>
where
    S: AsyncWrite + Unpin,
{
    timeout(HANDSHAKE_TIMEOUT, write_packet(stream, payload))
        .await
        .map_err(|_| InnerSessionError::HandshakeTimeout)??;
    Ok(())
}

async fn read_packet_timed<S>(stream: &mut S) -> Result<Vec<u8>, InnerSessionError>
where
    S: AsyncRead + Unpin,
{
    timeout(HANDSHAKE_TIMEOUT, read_packet(stream))
        .await
        .map_err(|_| InnerSessionError::HandshakeTimeout)?
}

async fn write_packet<S>(stream: &mut S, payload: &[u8]) -> Result<(), InnerSessionError>
where
    S: AsyncWrite + Unpin,
{
    if payload.len() > MAX_NOISE_PACKET {
        return Err(InnerSessionError::CiphertextTooLarge(payload.len()));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_packet<S>(stream: &mut S) -> Result<Vec<u8>, InnerSessionError>
where
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length > MAX_NOISE_PACKET {
        return Err(InnerSessionError::CiphertextTooLarge(length));
    }
    let mut packet = vec![0_u8; length];
    stream.read_exact(&mut packet).await?;
    Ok(packet)
}

#[derive(Debug, Error)]
pub enum InnerSessionError {
    #[error("invalid built-in Noise pattern")]
    InvalidNoisePattern,
    #[error("Noise protocol failed: {0}")]
    Noise(#[from] snow::Error),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("inner-session JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("inner-session I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("inner-session handshake timed out")]
    HandshakeTimeout,
    #[error("identity proof binding does not match the expected peer/context")]
    IdentityBindingMismatch,
    #[error("identity proof is too large: {0} bytes")]
    IdentityProofTooLarge(usize),
    #[error("inner ciphertext is too large: {0} bytes")]
    CiphertextTooLarge(usize),
    #[error("inner plaintext is too large: {0} bytes")]
    PlaintextTooLarge(usize),
    #[error("inner message kind must be 1..={MAX_MESSAGE_KIND_BYTES} bytes")]
    InvalidMessageKind,
    #[error("inner business frame is malformed")]
    MalformedBusinessFrame,
    #[error("inner message kind is not valid UTF-8")]
    InvalidMessageKindEncoding,
    #[error("inner frame uses wire major {0}")]
    WrongWireMajor(u32),
    #[error("inner frame sequence mismatch: expected {expected}, got {actual}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("inner session is poisoned after a prior authentication/protocol failure")]
    SessionPoisoned,
    #[error("inner frame sequence exhausted")]
    SequenceExhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identities() -> (ControllerSessionIdentity, DeviceSessionIdentity) {
        let controller = ControllerIdentity::from_secret([11_u8; 32]);
        let device = DeviceIdentity::from_secret([12_u8; 32]);
        let site_id = SiteId::new();
        let device_id = DeviceId::new();
        (
            ControllerSessionIdentity {
                identity: controller.clone(),
                noise_static_secret: [13_u8; 32],
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

    async fn session_pair() -> (InnerSession, InnerSession) {
        let (controller, device) = identities();
        let (mut left, mut right) = tokio::io::duplex(128 * 1024);
        let controller_task =
            tokio::spawn(async move { InnerSession::accept(&mut left, controller).await.unwrap() });
        let device_session = InnerSession::connect(&mut right, device).await.unwrap();
        (controller_task.await.unwrap(), device_session)
    }

    #[tokio::test]
    async fn mutually_authenticated_session_binds_context() {
        let (controller, device) = session_pair().await;
        assert_eq!(controller.peer_role(), InnerRole::Device);
        assert_eq!(device.peer_role(), InnerRole::Controller);
        assert_eq!(controller.controller_id(), device.controller_id());
        assert_eq!(controller.device_id(), device.device_id());
        assert_eq!(controller.site_id(), device.site_id());
    }

    #[tokio::test]
    async fn dynamic_authorizer_resolves_device_only_after_encrypted_proof() {
        let (controller, device) = identities();
        let expected_public = device.identity.public_identity();
        let expected_device_id = device.device_id;
        let expected_site_id = device.site_id;
        let (mut left, mut right) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move {
            InnerSession::accept_authorized(
                &mut left,
                ControllerSessionAuthority {
                    identity: controller.identity,
                    noise_static_secret: controller.noise_static_secret,
                },
                move |claim| {
                    claim.device_identity == expected_public
                        && claim.device_id == expected_device_id
                        && claim.site_id == expected_site_id
                },
            )
            .await
        });
        let client = InnerSession::connect(&mut right, device).await.unwrap();
        let accepted = server.await.unwrap().unwrap();
        assert_eq!(accepted.device_id(), client.device_id());
        assert_eq!(accepted.site_id(), client.site_id());
    }

    #[tokio::test]
    async fn dynamic_authorizer_rejects_self_signed_but_unknown_device() {
        let (controller, device) = identities();
        let (mut left, mut right) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move {
            InnerSession::accept_authorized(
                &mut left,
                ControllerSessionAuthority {
                    identity: controller.identity,
                    noise_static_secret: controller.noise_static_secret,
                },
                |_| false,
            )
            .await
        });
        let _client = tokio::spawn(async move { InnerSession::connect(&mut right, device).await });
        assert!(matches!(
            server.await.unwrap(),
            Err(InnerSessionError::IdentityBindingMismatch)
        ));
    }

    #[tokio::test]
    async fn wrong_controller_pin_fails_closed() {
        let (controller, mut device) = identities();
        device.pinned_controller = ControllerIdentity::from_secret([99_u8; 32]).public_identity();
        let (mut left, mut right) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move { InnerSession::accept(&mut left, controller).await });
        let client = InnerSession::connect(&mut right, device).await;
        assert!(client.is_err(), "wrong Controller pin must fail closed");
        let _ = server.await;
    }

    #[tokio::test]
    async fn wrong_device_key_fails_closed() {
        let (mut controller, device) = identities();
        controller.expected_device = DeviceIdentity::from_secret([98_u8; 32]).public_identity();
        let (mut left, mut right) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move { InnerSession::accept(&mut left, controller).await });
        let _client = tokio::spawn(async move { InnerSession::connect(&mut right, device).await });
        assert!(matches!(
            server.await.unwrap(),
            Err(InnerSessionError::IdentityBindingMismatch)
        ));
    }

    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected write failure",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn post_handshake_io_failure_poison_session() {
        let (_controller, mut device) = session_pair().await;
        let message = InnerMessage::new("read", b"payload".to_vec()).unwrap();
        let mut failing = FailingWriter;
        assert!(matches!(
            device.send(&mut failing, &message).await,
            Err(InnerSessionError::Io(_))
        ));
        assert!(matches!(
            device.send(&mut failing, &message).await,
            Err(InnerSessionError::SessionPoisoned)
        ));

        let (mut controller, _device) = session_pair().await;
        let mut eof = tokio::io::empty();
        assert!(matches!(
            controller.recv(&mut eof).await,
            Err(InnerSessionError::Io(_))
        ));
        assert!(matches!(
            controller.recv(&mut eof).await,
            Err(InnerSessionError::SessionPoisoned)
        ));
    }

    #[tokio::test]
    async fn binary_business_frame_carries_max_v1_read_payload_without_json_expansion() {
        let (mut controller, mut device) = session_pair().await;
        let payload = vec![0xA5; clew_core::HARD_MAX_READ_RESULT_BYTES as usize];
        let message = InnerMessage::new("read_result", payload.clone()).unwrap();
        let ciphertext = device.seal(&message).unwrap();
        assert!(ciphertext.len() < MAX_INNER_PLAINTEXT + 16);
        let decoded = controller.open(&ciphertext).unwrap();
        assert_eq!(decoded.kind, "read_result");
        assert_eq!(decoded.payload, payload);
    }

    #[tokio::test]
    async fn business_plaintext_is_absent_and_replay_or_corruption_fails() {
        let (mut controller, mut device) = session_pair().await;
        let message =
            InnerMessage::new("read", br#"{\"path\":\"C:/secret/project.txt\"}"#.to_vec()).unwrap();
        let ciphertext = device.seal(&message).unwrap();
        assert!(!ciphertext.windows(4).any(|window| window == b"read"));
        assert!(
            !ciphertext
                .windows(b"secret/project.txt".len())
                .any(|window| window == b"secret/project.txt")
        );
        assert_eq!(controller.open(&ciphertext).unwrap(), message);
        assert!(matches!(
            controller.open(&ciphertext),
            Err(InnerSessionError::Noise(_))
        ));

        let next = InnerMessage::new("read", b"next".to_vec()).unwrap();
        let valid_after_replay = device.seal(&next).unwrap();
        assert!(matches!(
            controller.open(&valid_after_replay),
            Err(InnerSessionError::SessionPoisoned)
        ));

        let (mut controller, mut device) = session_pair().await;
        let mut corrupted = device.seal(&next).unwrap();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0x80;
        assert!(matches!(
            controller.open(&corrupted),
            Err(InnerSessionError::Noise(_))
        ));
        let after_corruption = device
            .seal(&InnerMessage::new("read", b"after".to_vec()).unwrap())
            .unwrap();
        assert!(matches!(
            controller.open(&after_corruption),
            Err(InnerSessionError::SessionPoisoned)
        ));
    }
}
