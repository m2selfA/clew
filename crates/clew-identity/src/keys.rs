use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use clew_core::{ControllerId, StableIdError};

const CONTROLLER_ID_DOMAIN: &[u8] = b"clew/controller-id/v1\0";
const SITE_CONFIG_SIGNATURE_DOMAIN: &[u8] = b"clew/site-config-signature/v1\0";
const SESSION_BINDING_SIGNATURE_DOMAIN: &[u8] = b"clew/inner-session-binding/v1\0";
const CONNECTOR_LEASE_SIGNATURE_DOMAIN: &[u8] = b"clew/connector-lease/v1\0";
const DEVICE_NOISE_STATIC_INFO: &[u8] = b"clew/device-noise-static/v1";

#[derive(Clone)]
pub struct ControllerIdentity {
    signing_key: SigningKey,
}

impl std::fmt::Debug for ControllerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerIdentity")
            .field("controller_id", &self.controller_id())
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControllerPublicIdentity {
    pub controller_id: ControllerId,
    pub public_key: [u8; 32],
}

#[derive(Clone)]
pub struct DeviceIdentity {
    signing_key: SigningKey,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("public", &self.public_identity())
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct DevicePublicIdentity {
    pub public_key: [u8; 32],
}

impl ControllerIdentity {
    pub fn generate() -> Result<Self, IdentityError> {
        Ok(Self::from_secret(random_secret_32()?))
    }

    #[must_use]
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&secret),
        }
    }

    #[must_use]
    pub fn controller_id(&self) -> ControllerId {
        controller_id_from_public_key(&self.signing_key.verifying_key().to_bytes())
            .expect("versioned controller fingerprint cannot be nil")
    }

    #[must_use]
    pub fn public_identity(&self) -> ControllerPublicIdentity {
        ControllerPublicIdentity {
            controller_id: self.controller_id(),
            public_key: self.signing_key.verifying_key().to_bytes(),
        }
    }

    pub fn sign_site_config<T: Serialize>(&self, payload: &T) -> Result<Vec<u8>, IdentityError> {
        self.sign_payload(SITE_CONFIG_SIGNATURE_DOMAIN, payload)
    }

    pub fn sign_session_binding<T: Serialize>(
        &self,
        payload: &T,
    ) -> Result<Vec<u8>, IdentityError> {
        self.sign_payload(SESSION_BINDING_SIGNATURE_DOMAIN, payload)
    }

    pub fn sign_connector_lease<T: Serialize>(
        &self,
        payload: &T,
    ) -> Result<Vec<u8>, IdentityError> {
        self.sign_payload(CONNECTOR_LEASE_SIGNATURE_DOMAIN, payload)
    }

    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }

    pub(crate) fn sign_payload<T: Serialize>(
        &self,
        domain: &[u8],
        payload: &T,
    ) -> Result<Vec<u8>, IdentityError> {
        let message = signed_message(domain, payload)?;
        Ok(self.signing_key.sign(&message).to_bytes().to_vec())
    }
}

impl ControllerPublicIdentity {
    pub fn validate(&self) -> Result<(), IdentityError> {
        let expected = controller_id_from_public_key(&self.public_key)?;
        if expected != self.controller_id {
            return Err(IdentityError::ControllerIdMismatch);
        }
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        if key.is_weak() {
            return Err(IdentityError::WeakPublicKey);
        }
        Ok(())
    }

    pub fn verify_site_config<T: Serialize>(
        &self,
        payload: &T,
        signature: &[u8],
    ) -> Result<(), IdentityError> {
        self.verify_payload(SITE_CONFIG_SIGNATURE_DOMAIN, payload, signature)
    }

    pub fn verify_session_binding<T: Serialize>(
        &self,
        payload: &T,
        signature: &[u8],
    ) -> Result<(), IdentityError> {
        self.verify_payload(SESSION_BINDING_SIGNATURE_DOMAIN, payload, signature)
    }

    pub fn verify_connector_lease<T: Serialize>(
        &self,
        payload: &T,
        signature: &[u8],
    ) -> Result<(), IdentityError> {
        self.verify_payload(CONNECTOR_LEASE_SIGNATURE_DOMAIN, payload, signature)
    }

    pub(crate) fn verify_payload<T: Serialize>(
        &self,
        domain: &[u8],
        payload: &T,
        signature: &[u8],
    ) -> Result<(), IdentityError> {
        self.validate()?;
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        let signature = Signature::from_slice(signature)
            .map_err(|_| IdentityError::InvalidSignatureLength(signature.len()))?;
        let message = signed_message(domain, payload)?;
        key.verify_strict(&message, &signature)
            .map_err(|_| IdentityError::InvalidSignature)
    }

    #[must_use]
    pub fn matches_pin(&self, candidate: &Self) -> bool {
        self == candidate
    }
}

impl DeviceIdentity {
    pub fn generate() -> Result<Self, IdentityError> {
        Ok(Self::from_secret(random_secret_32()?))
    }

    #[must_use]
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&secret),
        }
    }

    #[must_use]
    pub fn public_identity(&self) -> DevicePublicIdentity {
        DevicePublicIdentity {
            public_key: self.signing_key.verifying_key().to_bytes(),
        }
    }

    pub fn sign_session_binding<T: Serialize>(
        &self,
        payload: &T,
    ) -> Result<Vec<u8>, IdentityError> {
        let message = signed_message(SESSION_BINDING_SIGNATURE_DOMAIN, payload)?;
        Ok(self.signing_key.sign(&message).to_bytes().to_vec())
    }

    #[must_use]
    pub fn noise_static_secret(&self) -> [u8; 32] {
        let hkdf = Hkdf::<Sha256>::new(None, &self.signing_key.to_bytes());
        let mut secret = [0_u8; 32];
        hkdf.expand(DEVICE_NOISE_STATIC_INFO, &mut secret)
            .expect("32-byte HKDF expansion is valid for SHA-256");
        secret
    }

    pub(crate) fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

impl DevicePublicIdentity {
    pub fn validate(&self) -> Result<(), IdentityError> {
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        if key.is_weak() {
            return Err(IdentityError::WeakPublicKey);
        }
        Ok(())
    }

    pub fn verify_session_binding<T: Serialize>(
        &self,
        payload: &T,
        signature: &[u8],
    ) -> Result<(), IdentityError> {
        self.validate()?;
        let key = VerifyingKey::from_bytes(&self.public_key)
            .map_err(|_| IdentityError::InvalidPublicKey)?;
        let signature = Signature::from_slice(signature)
            .map_err(|_| IdentityError::InvalidSignatureLength(signature.len()))?;
        let message = signed_message(SESSION_BINDING_SIGNATURE_DOMAIN, payload)?;
        key.verify_strict(&message, &signature)
            .map_err(|_| IdentityError::InvalidSignature)
    }
}

pub(crate) fn random_secret_32() -> Result<[u8; 32], IdentityError> {
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|error| IdentityError::Random(format!("{error}")))?;
    Ok(secret)
}

fn signed_message<T: Serialize>(domain: &[u8], payload: &T) -> Result<Vec<u8>, IdentityError> {
    let encoded = serde_json::to_vec(payload)?;
    let mut message = Vec::with_capacity(domain.len() + encoded.len());
    message.extend_from_slice(domain);
    message.extend_from_slice(&encoded);
    Ok(message)
}

fn controller_id_from_public_key(public_key: &[u8; 32]) -> Result<ControllerId, StableIdError> {
    let mut hasher = Sha256::new();
    hasher.update(CONTROLLER_ID_DOMAIN);
    hasher.update(public_key);
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // UUIDv8-style presentation for a domain-specific derived identifier.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ControllerId::from_bytes(bytes)
}

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error("invalid Ed25519 public key")]
    InvalidPublicKey,
    #[error("weak Ed25519 public key is not accepted")]
    WeakPublicKey,
    #[error("controller ID does not match the pinned public key fingerprint")]
    ControllerIdMismatch,
    #[error("invalid Ed25519 signature length {0}")]
    InvalidSignatureLength(usize),
    #[error("Ed25519 signature verification failed")]
    InvalidSignature,
    #[error("signed payload JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    StableId(#[from] StableIdError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_id_is_stable_fingerprint_of_public_key() {
        let secret = [7_u8; 32];
        let first = ControllerIdentity::from_secret(secret);
        let second = ControllerIdentity::from_secret(secret);
        assert_eq!(first.controller_id(), second.controller_id());
        assert_eq!(first.public_identity(), second.public_identity());
        assert!(first.public_identity().validate().is_ok());
    }

    #[test]
    fn fresh_controller_without_backup_has_different_pin() {
        let original = ControllerIdentity::generate().unwrap().public_identity();
        let replacement = ControllerIdentity::generate().unwrap().public_identity();
        assert_ne!(original, replacement);
        assert!(!original.matches_pin(&replacement));
    }

    #[test]
    fn device_noise_static_hkdf_is_stable_and_key_separated() {
        let secret = [41_u8; 32];
        let first = DeviceIdentity::from_secret(secret);
        let second = DeviceIdentity::from_secret(secret);
        assert_eq!(first.noise_static_secret(), second.noise_static_secret());
        assert_ne!(first.noise_static_secret(), secret);
        assert_ne!(
            first.noise_static_secret(),
            first.public_identity().public_key
        );
    }

    #[test]
    fn controller_id_tampering_fails_closed() {
        let identity = ControllerIdentity::generate().unwrap();
        let mut public = identity.public_identity();
        public.controller_id = ControllerId::new();
        assert!(matches!(
            public.validate(),
            Err(IdentityError::ControllerIdMismatch)
        ));
    }
}
