use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use clew_core::{ControllerId, MAX_STATE_DOCUMENT_SIZE};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use crate::{ControllerIdentity, EnrollmentRegistry, StoredControllerIdentity};

const BACKUP_VERSION: u32 = 1;
const BACKUP_AAD: &[u8] = b"clew/controller-backup/v1";
const BACKUP_SALT_BYTES: usize = 16;
const BACKUP_NONCE_BYTES: usize = 24;
const BACKUP_KEY_BYTES: usize = 32;
const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
const ARGON2_ITERATIONS: u32 = 3;
const ARGON2_LANES: u32 = 1;
const MIN_PASSPHRASE_BYTES: usize = 12;
const MAX_PASSPHRASE_BYTES: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
pub struct ControllerBackupPayload {
    controller_secret_key: [u8; 32],
    transport_identity_secret: [u8; 32],
    pub registry: EnrollmentRegistry,
    pub created_unix_ms: u64,
}

impl std::fmt::Debug for ControllerBackupPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControllerBackupPayload")
            .field("controller_id", &self.registry.controller_id())
            .field("created_unix_ms", &self.created_unix_ms)
            .field("controller_secret_key", &"[REDACTED]")
            .field("transport_identity_secret", &"[REDACTED]")
            .field("registry", &self.registry)
            .finish()
    }
}

impl ControllerBackupPayload {
    pub fn capture(
        stored: &StoredControllerIdentity,
        registry: EnrollmentRegistry,
        created_unix_ms: u64,
    ) -> Result<Self, BackupError> {
        if stored.identity().controller_id() != registry.controller_id() {
            return Err(BackupError::ControllerMismatch);
        }
        Ok(Self {
            controller_secret_key: stored.identity().secret_bytes(),
            transport_identity_secret: stored.noise_static_secret(),
            registry,
            created_unix_ms,
        })
    }

    #[cfg(test)]
    fn from_parts(
        identity: &ControllerIdentity,
        transport_identity_secret: [u8; 32],
        registry: EnrollmentRegistry,
        created_unix_ms: u64,
    ) -> Result<Self, BackupError> {
        if identity.controller_id() != registry.controller_id() {
            return Err(BackupError::ControllerMismatch);
        }
        Ok(Self {
            controller_secret_key: identity.secret_bytes(),
            transport_identity_secret,
            registry,
            created_unix_ms,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EncryptedControllerBackup {
    pub version: u32,
    pub salt: [u8; BACKUP_SALT_BYTES],
    pub nonce: [u8; BACKUP_NONCE_BYTES],
    pub ciphertext: Vec<u8>,
}

pub struct RestoredController {
    pub identity: ControllerIdentity,
    pub transport_identity_secret: [u8; 32],
    pub registry: EnrollmentRegistry,
    pub recovery_review: RecoveryReview,
}

impl std::fmt::Debug for RestoredController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestoredController")
            .field("controller_id", &self.identity.controller_id())
            .field("transport_identity_secret", &"[REDACTED]")
            .field("registry", &self.registry)
            .field("recovery_review", &self.recovery_review)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryReview {
    pub restored_controller_id: ControllerId,
    pub remote_access_paused: bool,
    pub historical_bootstrap_closed: bool,
}

pub fn encrypt_controller_backup(
    payload: &ControllerBackupPayload,
    passphrase: &str,
) -> Result<EncryptedControllerBackup, BackupError> {
    validate_passphrase(passphrase)?;
    validate_payload(payload)?;
    let plaintext = Zeroizing::new(serde_json::to_vec(payload)?);
    if plaintext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(plaintext.len()));
    }
    let mut salt = [0_u8; BACKUP_SALT_BYTES];
    getrandom::fill(&mut salt).map_err(|error| BackupError::Random(error.to_string()))?;
    let mut nonce = [0_u8; BACKUP_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|error| BackupError::Random(error.to_string()))?;
    let key = derive_backup_key(passphrase, &salt)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_slice()).map_err(|_| BackupError::CipherInit)?;
    let nonce_array = XNonce::try_from(nonce.as_slice()).map_err(|_| BackupError::CipherInit)?;
    let ciphertext = cipher
        .encrypt(
            &nonce_array,
            Payload {
                msg: plaintext.as_slice(),
                aad: BACKUP_AAD,
            },
        )
        .map_err(|_| BackupError::AuthenticationFailed)?;
    if ciphertext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(ciphertext.len()));
    }
    Ok(EncryptedControllerBackup {
        version: BACKUP_VERSION,
        salt,
        nonce,
        ciphertext,
    })
}

pub fn decrypt_controller_backup(
    backup: &EncryptedControllerBackup,
    passphrase: &str,
    empty_state: bool,
) -> Result<RestoredController, BackupError> {
    if !empty_state {
        return Err(BackupError::RestoreRequiresEmptyState);
    }
    validate_passphrase(passphrase)?;
    if backup.version != BACKUP_VERSION {
        return Err(BackupError::UnsupportedVersion(backup.version));
    }
    if backup.ciphertext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(backup.ciphertext.len()));
    }
    let key = derive_backup_key(passphrase, &backup.salt)?;
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_slice()).map_err(|_| BackupError::CipherInit)?;
    let nonce_array =
        XNonce::try_from(backup.nonce.as_slice()).map_err(|_| BackupError::CipherInit)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &nonce_array,
                Payload {
                    msg: &backup.ciphertext,
                    aad: BACKUP_AAD,
                },
            )
            .map_err(|_| BackupError::AuthenticationFailed)?,
    );
    if plaintext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(plaintext.len()));
    }
    let mut payload: ControllerBackupPayload = serde_json::from_slice(plaintext.as_slice())?;
    validate_payload(&payload)?;
    let identity = ControllerIdentity::from_secret(payload.controller_secret_key);
    if identity.controller_id() != payload.registry.controller_id() {
        payload.controller_secret_key.zeroize();
        payload.transport_identity_secret.zeroize();
        return Err(BackupError::ControllerMismatch);
    }
    let transport_identity_secret = payload.transport_identity_secret;
    payload.controller_secret_key.zeroize();
    payload.transport_identity_secret.zeroize();
    payload.registry.close_all_invites();
    let restored_controller_id = identity.controller_id();
    Ok(RestoredController {
        identity,
        transport_identity_secret,
        registry: payload.registry,
        recovery_review: RecoveryReview {
            restored_controller_id,
            remote_access_paused: true,
            historical_bootstrap_closed: true,
        },
    })
}

pub fn backup_to_json(backup: &EncryptedControllerBackup) -> Result<Vec<u8>, BackupError> {
    if backup.version != BACKUP_VERSION {
        return Err(BackupError::UnsupportedVersion(backup.version));
    }
    if backup.ciphertext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(backup.ciphertext.len()));
    }
    let encoded = serde_json::to_vec_pretty(backup)?;
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(encoded.len()));
    }
    Ok(encoded)
}

pub fn backup_from_json(input: &[u8]) -> Result<EncryptedControllerBackup, BackupError> {
    if input.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(input.len()));
    }
    #[derive(Deserialize)]
    struct Header {
        version: u32,
    }
    let header: Header = serde_json::from_slice(input)?;
    if header.version != BACKUP_VERSION {
        return Err(BackupError::UnsupportedVersion(header.version));
    }
    let backup: EncryptedControllerBackup = serde_json::from_slice(input)?;
    if backup.ciphertext.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(BackupError::PayloadTooLarge(backup.ciphertext.len()));
    }
    Ok(backup)
}

fn validate_payload(payload: &ControllerBackupPayload) -> Result<(), BackupError> {
    let identity = ControllerIdentity::from_secret(payload.controller_secret_key);
    if identity.controller_id() != payload.registry.controller_id() {
        return Err(BackupError::ControllerMismatch);
    }
    Ok(())
}

fn validate_passphrase(passphrase: &str) -> Result<(), BackupError> {
    let length = passphrase.len();
    if !(MIN_PASSPHRASE_BYTES..=MAX_PASSPHRASE_BYTES).contains(&length) {
        return Err(BackupError::InvalidPassphraseLength(length));
    }
    Ok(())
}

fn derive_backup_key(
    passphrase: &str,
    salt: &[u8; BACKUP_SALT_BYTES],
) -> Result<Zeroizing<[u8; BACKUP_KEY_BYTES]>, BackupError> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_LANES,
        Some(BACKUP_KEY_BYTES),
    )
    .map_err(|error| BackupError::Kdf(error.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; BACKUP_KEY_BYTES]);
    argon2
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut_slice())
        .map_err(|error| BackupError::Kdf(error.to_string()))?;
    Ok(key)
}

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("backup passphrase must be 12..=1024 UTF-8 bytes, got {0}")]
    InvalidPassphraseLength(usize),
    #[error("controller backup payload is {0} bytes; maximum is 16 MiB")]
    PayloadTooLarge(usize),
    #[error("unsupported controller backup version {0}")]
    UnsupportedVersion(u32),
    #[error("secure random generation failed: {0}")]
    Random(String),
    #[error("Argon2id key derivation failed: {0}")]
    Kdf(String),
    #[error("XChaCha20-Poly1305 cipher initialization failed")]
    CipherInit,
    #[error("controller backup authentication failed")]
    AuthenticationFailed,
    #[error("controller backup ControllerKey does not match the embedded ControllerId")]
    ControllerMismatch,
    #[error("controller backup restore requires an empty local Controller state")]
    RestoreRequiresEmptyState,
    #[error("controller backup JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use clew_core::{InviteId, MemberCapabilities, SiteId};

    use super::*;
    use crate::{PermissionGrant, SiteBootstrapSpec};

    fn payload() -> (ControllerBackupPayload, crate::SignedSiteBootstrapPass) {
        let identity = ControllerIdentity::from_secret([51_u8; 32]);
        let mut registry = EnrollmentRegistry::new(
            identity.controller_id(),
            PermissionGrant {
                member: MemberCapabilities::EXECUTE_ONLY,
                read: true,
                write: false,
                shell: false,
            },
        );
        let pass = registry
            .issue_bootstrap(
                &identity,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Recovery Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 100_000,
                    deployment_window_ms: 10_000,
                    max_claims: 2,
                },
            )
            .unwrap();
        (
            ControllerBackupPayload::from_parts(&identity, [61_u8; 32], registry, 100).unwrap(),
            pass,
        )
    }

    #[test]
    fn encrypted_backup_roundtrip_enters_recovery_review_and_closes_old_invites() {
        let (payload, pass) = payload();
        let controller_id = payload.registry.controller_id();
        let encrypted =
            encrypt_controller_backup(&payload, "correct horse battery staple").unwrap();
        let encoded = backup_to_json(&encrypted).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("controller_secret_key"));
        let decoded = backup_from_json(&encoded).unwrap();
        let mut restored =
            decrypt_controller_backup(&decoded, "correct horse battery staple", true).unwrap();
        assert_eq!(restored.identity.controller_id(), controller_id);
        assert!(restored.recovery_review.remote_access_paused);
        assert!(restored.recovery_review.historical_bootstrap_closed);
        let device = crate::DeviceIdentity::generate().unwrap().public_identity();
        assert!(matches!(
            restored.registry.claim(&pass, device, 200),
            Err(crate::EnrollmentError::InviteClosed)
        ));
    }

    #[test]
    fn wrong_password_or_tampered_ciphertext_fails_authentication() {
        let (payload, _) = payload();
        let mut encrypted =
            encrypt_controller_backup(&payload, "correct horse battery staple").unwrap();
        assert!(matches!(
            decrypt_controller_backup(&encrypted, "wrong password!!", true),
            Err(BackupError::AuthenticationFailed)
        ));
        encrypted.ciphertext[0] ^= 1;
        assert!(matches!(
            decrypt_controller_backup(&encrypted, "correct horse battery staple", true),
            Err(BackupError::AuthenticationFailed)
        ));
    }

    #[test]
    fn restore_requires_empty_state_before_decryption() {
        let (payload, _) = payload();
        let encrypted =
            encrypt_controller_backup(&payload, "correct horse battery staple").unwrap();
        assert!(matches!(
            decrypt_controller_backup(&encrypted, "correct horse battery staple", false),
            Err(BackupError::RestoreRequiresEmptyState)
        ));
    }
}
