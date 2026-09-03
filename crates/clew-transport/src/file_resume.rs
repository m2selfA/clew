use clew_core::{ControllerId, DeviceId, HARD_MAX_READ_ROOT_BYTES, SiteId, TransferId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const FILE_RESUME_DESCRIPTOR_VERSION: u32 = 1;
pub const MAX_FILE_RESUME_DESCRIPTOR_BYTES: usize = 8 * 1024;
pub const MAX_FILE_RESUME_PATH_BYTES: usize = HARD_MAX_READ_ROOT_BYTES;
pub const EMPTY_SHA256_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferDirection {
    ControllerToDevice,
    DeviceToController,
}

/// Peer-visible resume checkpoint. Controller-local paths stay private and are keyed by TransferId.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileResumeDescriptor {
    pub version: u32,
    /// Stable logical transfer identity only. This is never an authorization token.
    pub transfer_id: TransferId,
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub device_id: DeviceId,
    pub direction: FileTransferDirection,
    pub device_path: String,
    pub total_size: u64,
    pub checkpoint_revision: u64,
    pub confirmed_offset: u64,
    pub confirmed_prefix_sha256: String,
    pub final_sha256: Option<String>,
}

impl FileResumeDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transfer_id: TransferId,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        direction: FileTransferDirection,
        device_path: impl Into<String>,
        total_size: u64,
        checkpoint_revision: u64,
        confirmed_offset: u64,
        confirmed_prefix_sha256: impl Into<String>,
        final_sha256: Option<String>,
    ) -> Result<Self, FileResumeError> {
        let descriptor = Self {
            version: FILE_RESUME_DESCRIPTOR_VERSION,
            transfer_id,
            controller_id,
            site_id,
            device_id,
            direction,
            device_path: device_path.into(),
            total_size,
            checkpoint_revision,
            confirmed_offset,
            confirmed_prefix_sha256: confirmed_prefix_sha256.into(),
            final_sha256,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), FileResumeError> {
        if self.version != FILE_RESUME_DESCRIPTOR_VERSION {
            return Err(FileResumeError::UnsupportedVersion(self.version));
        }
        validate_path("device_path", &self.device_path)?;
        if self.checkpoint_revision == 0 {
            return Err(FileResumeError::InvalidCheckpointRevision);
        }
        if self.confirmed_offset > self.total_size {
            return Err(FileResumeError::OffsetBeyondEnd {
                offset: self.confirmed_offset,
                total_size: self.total_size,
            });
        }
        validate_sha256("confirmed_prefix_sha256", &self.confirmed_prefix_sha256)?;
        if self.confirmed_offset == 0 && self.confirmed_prefix_sha256 != EMPTY_SHA256_HEX {
            return Err(FileResumeError::InvalidEmptyPrefixHash);
        }
        match &self.final_sha256 {
            Some(final_sha256) => {
                validate_sha256("final_sha256", final_sha256)?;
                if self.confirmed_offset == self.total_size
                    && final_sha256 != &self.confirmed_prefix_sha256
                {
                    return Err(FileResumeError::CompletedHashMismatch);
                }
            }
            None if self.confirmed_offset == self.total_size => {
                return Err(FileResumeError::MissingFinalSha256ForComplete);
            }
            None => {}
        }
        Ok(())
    }

    pub fn validate_successor_of(
        &self,
        previous: &FileResumeDescriptor,
    ) -> Result<(), FileResumeError> {
        previous.validate()?;
        self.validate()?;
        if previous.is_complete() {
            return Err(FileResumeError::AlreadyComplete);
        }
        if self.transfer_id != previous.transfer_id {
            return Err(FileResumeError::ScopeChanged("transfer_id"));
        }
        if self.controller_id != previous.controller_id {
            return Err(FileResumeError::ScopeChanged("controller_id"));
        }
        if self.site_id != previous.site_id {
            return Err(FileResumeError::ScopeChanged("site_id"));
        }
        if self.device_id != previous.device_id {
            return Err(FileResumeError::ScopeChanged("device_id"));
        }
        if self.direction != previous.direction {
            return Err(FileResumeError::ScopeChanged("direction"));
        }
        if self.device_path != previous.device_path {
            return Err(FileResumeError::ScopeChanged("device_path"));
        }
        if self.total_size != previous.total_size {
            return Err(FileResumeError::ScopeChanged("total_size"));
        }
        if self.checkpoint_revision <= previous.checkpoint_revision {
            return Err(FileResumeError::StaleCheckpointRevision {
                previous: previous.checkpoint_revision,
                next: self.checkpoint_revision,
            });
        }
        if self.confirmed_offset < previous.confirmed_offset {
            return Err(FileResumeError::OffsetRegressed {
                previous: previous.confirmed_offset,
                next: self.confirmed_offset,
            });
        }
        if self.confirmed_offset == previous.confirmed_offset
            && self.confirmed_prefix_sha256 != previous.confirmed_prefix_sha256
        {
            return Err(FileResumeError::PrefixHashChangedAtSameOffset);
        }
        if let Some(previous_final) = &previous.final_sha256
            && self.final_sha256.as_ref() != Some(previous_final)
        {
            return Err(FileResumeError::FinalHashChanged);
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, FileResumeError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_FILE_RESUME_DESCRIPTOR_BYTES {
            return Err(FileResumeError::EncodedTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, FileResumeError> {
        if encoded.len() > MAX_FILE_RESUME_DESCRIPTOR_BYTES {
            return Err(FileResumeError::EncodedTooLarge(encoded.len()));
        }
        let descriptor: Self = serde_json::from_slice(encoded)?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.confirmed_offset == self.total_size
    }
}

fn validate_path(field: &'static str, path: &str) -> Result<(), FileResumeError> {
    if path.trim().is_empty() || path.len() > MAX_FILE_RESUME_PATH_BYTES || path.contains('\0') {
        return Err(FileResumeError::InvalidPath { field });
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), FileResumeError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FileResumeError::InvalidSha256 { field });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum FileResumeError {
    #[error("unsupported file resume descriptor version {0}")]
    UnsupportedVersion(u32),
    #[error("{field} must be 1..={MAX_FILE_RESUME_PATH_BYTES} UTF-8 bytes without NUL")]
    InvalidPath { field: &'static str },
    #[error("file resume checkpoint revision must be non-zero")]
    InvalidCheckpointRevision,
    #[error("confirmed offset {offset} exceeds total file size {total_size}")]
    OffsetBeyondEnd { offset: u64, total_size: u64 },
    #[error("{field} must be canonical lowercase SHA-256 hex")]
    InvalidSha256 { field: &'static str },
    #[error("offset zero must use the SHA-256 of the empty prefix")]
    InvalidEmptyPrefixHash,
    #[error("a completed checkpoint requires an expected final SHA-256")]
    MissingFinalSha256ForComplete,
    #[error("a completed checkpoint must match the expected final SHA-256")]
    CompletedHashMismatch,
    #[error("completed transfers cannot accept another resume checkpoint")]
    AlreadyComplete,
    #[error("file resume transfer scope changed at {0}")]
    ScopeChanged(&'static str),
    #[error("checkpoint revision must advance beyond {previous}, got {next}")]
    StaleCheckpointRevision { previous: u64, next: u64 },
    #[error("confirmed offset regressed from {previous} to {next}")]
    OffsetRegressed { previous: u64, next: u64 },
    #[error("prefix SHA-256 changed without advancing the confirmed offset")]
    PrefixHashChangedAtSameOffset,
    #[error("known final SHA-256 cannot be removed or changed across resume checkpoints")]
    FinalHashChanged,
    #[error("file resume descriptor exceeds {MAX_FILE_RESUME_DESCRIPTOR_BYTES} bytes: {0}")]
    EncodedTooLarge(usize),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_descriptor() -> FileResumeDescriptor {
        FileResumeDescriptor::new(
            TransferId::new(),
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
            FileTransferDirection::ControllerToDevice,
            "D:/device/target.bin",
            1_024,
            1,
            0,
            EMPTY_SHA256_HEX,
            Some("11".repeat(32)),
        )
        .unwrap()
    }

    #[test]
    fn resume_descriptor_roundtrips_and_binds_transfer_scope() {
        let descriptor = sample_descriptor();
        let encoded = descriptor.encode().unwrap();
        assert!(encoded.len() < MAX_FILE_RESUME_DESCRIPTOR_BYTES);
        assert_eq!(FileResumeDescriptor::decode(&encoded).unwrap(), descriptor);
        assert!(!descriptor.is_complete());
    }

    #[test]
    fn resume_checkpoint_bounds_hashes_and_completion_fail_closed() {
        let mut descriptor = sample_descriptor();
        descriptor.confirmed_offset = descriptor.total_size + 1;
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::OffsetBeyondEnd { .. })
        ));

        let mut descriptor = sample_descriptor();
        descriptor.confirmed_prefix_sha256 = "AA".repeat(32);
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::InvalidSha256 { .. })
        ));

        let mut descriptor = sample_descriptor();
        descriptor.confirmed_prefix_sha256 = "22".repeat(32);
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::InvalidEmptyPrefixHash)
        ));

        let mut descriptor = sample_descriptor();
        descriptor.confirmed_offset = descriptor.total_size;
        descriptor.confirmed_prefix_sha256 = "22".repeat(32);
        descriptor.final_sha256 = None;
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::MissingFinalSha256ForComplete)
        ));

        descriptor.final_sha256 = Some("11".repeat(32));
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::CompletedHashMismatch)
        ));

        descriptor.final_sha256 = Some("22".repeat(32));
        descriptor.validate().unwrap();
        assert!(descriptor.is_complete());
    }

    #[test]
    fn resume_successor_keeps_scope_and_checkpoint_monotonic() {
        let previous = sample_descriptor();
        let mut next = previous.clone();
        next.checkpoint_revision = 2;
        next.confirmed_offset = 512;
        next.confirmed_prefix_sha256 = "22".repeat(32);
        next.validate_successor_of(&previous).unwrap();

        let mut changed_device = next.clone();
        changed_device.device_id = DeviceId::new();
        assert!(matches!(
            changed_device.validate_successor_of(&previous),
            Err(FileResumeError::ScopeChanged("device_id"))
        ));

        let mut stale = next.clone();
        stale.checkpoint_revision = previous.checkpoint_revision;
        assert!(matches!(
            stale.validate_successor_of(&previous),
            Err(FileResumeError::StaleCheckpointRevision { .. })
        ));

        let mut regressed = next.clone();
        regressed.checkpoint_revision = 3;
        regressed.confirmed_offset = 256;
        regressed.confirmed_prefix_sha256 = "33".repeat(32);
        assert!(matches!(
            regressed.validate_successor_of(&next),
            Err(FileResumeError::OffsetRegressed { .. })
        ));

        let mut same_offset_changed_hash = next.clone();
        same_offset_changed_hash.checkpoint_revision = 3;
        same_offset_changed_hash.confirmed_prefix_sha256 = "44".repeat(32);
        assert!(matches!(
            same_offset_changed_hash.validate_successor_of(&next),
            Err(FileResumeError::PrefixHashChangedAtSameOffset)
        ));

        let mut final_changed = next.clone();
        final_changed.final_sha256 = Some("33".repeat(32));
        assert!(matches!(
            final_changed.validate_successor_of(&previous),
            Err(FileResumeError::FinalHashChanged)
        ));
    }

    #[test]
    fn resume_descriptor_rejects_oversized_or_ambiguous_paths_before_use() {
        let mut descriptor = sample_descriptor();
        descriptor.device_path = "x".repeat(MAX_FILE_RESUME_PATH_BYTES + 1);
        assert!(matches!(
            descriptor.validate(),
            Err(FileResumeError::InvalidPath {
                field: "device_path"
            })
        ));

        let oversized = vec![b' '; MAX_FILE_RESUME_DESCRIPTOR_BYTES + 1];
        assert!(matches!(
            FileResumeDescriptor::decode(&oversized),
            Err(FileResumeError::EncodedTooLarge(actual))
                if actual == MAX_FILE_RESUME_DESCRIPTOR_BYTES + 1
        ));
    }
}
