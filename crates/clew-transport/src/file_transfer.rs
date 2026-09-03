use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_core::{ControllerId, DeviceId, SiteId, TransferId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    EMPTY_SHA256_HEX, FileResumeDescriptor, FileResumeError, FileTransferDirection, InnerMessage,
    InnerSessionError, MAX_FILE_RESUME_PATH_BYTES,
};

pub const FILE_TRANSFER_MANIFEST_VERSION: u32 = 1;
pub const MAX_FILE_TRANSFER_MANIFEST_BYTES: usize = 16 * 1024;
pub const MIN_FILE_CHUNK_BYTES: u32 = 4 * 1024;
pub const MAX_FILE_CHUNK_BYTES: u32 = 32 * 1024;
pub const MAX_FILE_CHUNK_BASE64_BYTES: usize = ((MAX_FILE_CHUNK_BYTES as usize + 2) / 3) * 4;
pub const MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES: usize = 48 * 1024;
pub const MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES: usize = 56 * 1024;
const MAX_FILE_TRANSFER_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileConflictPolicy {
    FailIfExists,
    ReplaceExisting,
    RenameIfExists,
}

/// Peer-visible single-file transfer contract.
///
/// Controller-local paths are deliberately absent. For DeviceToController transfers the
/// Controller's private destination/conflict policy remains Controller-local state keyed by
/// TransferId. ControllerToDevice carries only the device-side conflict policy because that is
/// necessary for the Target to finalize safely.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferManifest {
    pub version: u32,
    pub transfer_id: TransferId,
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub device_id: DeviceId,
    pub direction: FileTransferDirection,
    pub device_path: String,
    pub total_size: u64,
    pub chunk_size: u32,
    pub final_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_conflict_policy: Option<FileConflictPolicy>,
}

impl FileTransferManifest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transfer_id: TransferId,
        controller_id: ControllerId,
        site_id: SiteId,
        device_id: DeviceId,
        direction: FileTransferDirection,
        device_path: impl Into<String>,
        total_size: u64,
        chunk_size: u32,
        final_sha256: impl Into<String>,
        device_conflict_policy: Option<FileConflictPolicy>,
    ) -> Result<Self, FileTransferError> {
        let manifest = Self {
            version: FILE_TRANSFER_MANIFEST_VERSION,
            transfer_id,
            controller_id,
            site_id,
            device_id,
            direction,
            device_path: device_path.into(),
            total_size,
            chunk_size,
            final_sha256: final_sha256.into(),
            device_conflict_policy,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), FileTransferError> {
        if self.version != FILE_TRANSFER_MANIFEST_VERSION {
            return Err(FileTransferError::UnsupportedManifestVersion(self.version));
        }
        if self.device_path.trim().is_empty()
            || self.device_path.len() > MAX_FILE_RESUME_PATH_BYTES
            || self.device_path.contains('\0')
        {
            return Err(FileTransferError::InvalidDevicePath);
        }
        if self.chunk_size < MIN_FILE_CHUNK_BYTES
            || self.chunk_size > MAX_FILE_CHUNK_BYTES
            || !self.chunk_size.is_power_of_two()
        {
            return Err(FileTransferError::InvalidChunkSize(self.chunk_size));
        }
        validate_sha256(&self.final_sha256)?;
        if self.total_size == 0 && self.final_sha256 != EMPTY_SHA256_HEX {
            return Err(FileTransferError::EmptyFileHashMismatch);
        }
        match (self.direction, self.device_conflict_policy) {
            (FileTransferDirection::ControllerToDevice, Some(_))
            | (FileTransferDirection::DeviceToController, None) => {}
            (FileTransferDirection::ControllerToDevice, None) => {
                return Err(FileTransferError::MissingDeviceConflictPolicy);
            }
            (FileTransferDirection::DeviceToController, Some(_)) => {
                return Err(FileTransferError::ControllerPrivateConflictPolicyLeaked);
            }
        }
        Ok(())
    }

    pub fn chunk_count(&self) -> Result<u64, FileTransferError> {
        self.validate()?;
        Ok(self.total_size.div_ceil(u64::from(self.chunk_size)))
    }

    pub fn encode(&self) -> Result<Vec<u8>, FileTransferError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_FILE_TRANSFER_MANIFEST_BYTES {
            return Err(FileTransferError::ManifestTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, FileTransferError> {
        if encoded.len() > MAX_FILE_TRANSFER_MANIFEST_BYTES {
            return Err(FileTransferError::ManifestTooLarge(encoded.len()));
        }
        let manifest: Self = serde_json::from_slice(encoded)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn initial_resume_descriptor(&self) -> Result<FileResumeDescriptor, FileTransferError> {
        self.validate()?;
        Ok(FileResumeDescriptor::new(
            self.transfer_id,
            self.controller_id,
            self.site_id,
            self.device_id,
            self.direction,
            self.device_path.clone(),
            self.total_size,
            1,
            0,
            EMPTY_SHA256_HEX,
            Some(self.final_sha256.clone()),
        )?)
    }

    pub fn validate_chunk(&self, chunk: &FileTransferChunk) -> Result<(), FileTransferError> {
        self.validate()?;
        chunk.validate()?;
        if chunk.transfer_id != self.transfer_id {
            return Err(FileTransferError::ChunkTransferMismatch);
        }
        if self.total_size == 0 || chunk.offset >= self.total_size {
            return Err(FileTransferError::ChunkOffsetBeyondEnd {
                offset: chunk.offset,
                total_size: self.total_size,
            });
        }
        if chunk.offset % u64::from(self.chunk_size) != 0 {
            return Err(FileTransferError::ChunkOffsetUnaligned {
                offset: chunk.offset,
                chunk_size: self.chunk_size,
            });
        }
        let bytes = chunk.decode_bytes()?;
        let expected_len =
            u64::from(self.chunk_size).min(self.total_size.saturating_sub(chunk.offset)) as usize;
        if bytes.len() != expected_len {
            return Err(FileTransferError::UnexpectedChunkLength {
                expected: expected_len,
                actual: bytes.len(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferChunk {
    pub transfer_id: TransferId,
    pub offset: u64,
    pub sha256: String,
    pub data_base64: String,
}

impl FileTransferChunk {
    pub fn from_bytes(
        transfer_id: TransferId,
        offset: u64,
        data: &[u8],
    ) -> Result<Self, FileTransferError> {
        if data.is_empty() || data.len() > MAX_FILE_CHUNK_BYTES as usize {
            return Err(FileTransferError::InvalidChunkLength(data.len()));
        }
        let chunk = Self {
            transfer_id,
            offset,
            sha256: sha256_hex(data),
            data_base64: BASE64_STANDARD.encode(data),
        };
        chunk.validate()?;
        Ok(chunk)
    }

    pub fn validate(&self) -> Result<(), FileTransferError> {
        validate_sha256(&self.sha256)?;
        if self.data_base64.len() > MAX_FILE_CHUNK_BASE64_BYTES {
            return Err(FileTransferError::ChunkEncodingTooLarge(
                self.data_base64.len(),
            ));
        }
        let data = BASE64_STANDARD.decode(&self.data_base64)?;
        if data.is_empty() || data.len() > MAX_FILE_CHUNK_BYTES as usize {
            return Err(FileTransferError::InvalidChunkLength(data.len()));
        }
        if sha256_hex(&data) != self.sha256 {
            return Err(FileTransferError::ChunkHashMismatch);
        }
        Ok(())
    }

    pub fn decode_bytes(&self) -> Result<Vec<u8>, FileTransferError> {
        self.validate()?;
        Ok(BASE64_STANDARD.decode(&self.data_base64)?)
    }

    pub fn encode(&self) -> Result<Vec<u8>, FileTransferError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES {
            return Err(FileTransferError::ChunkDocumentTooLarge(encoded.len()));
        }
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, FileTransferError> {
        if encoded.len() > MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES {
            return Err(FileTransferError::ChunkDocumentTooLarge(encoded.len()));
        }
        let chunk: Self = serde_json::from_slice(encoded)?;
        chunk.validate()?;
        Ok(chunk)
    }
}

#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_sha256(value: &str) -> Result<(), FileTransferError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(FileTransferError::InvalidSha256);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FileTransferRequest {
    PutBegin { manifest: FileTransferManifest },
    PutChunk { chunk: FileTransferChunk },
    Status { transfer_id: TransferId },
    Finalize { transfer_id: TransferId },
    Cancel { transfer_id: TransferId },
}

impl FileTransferRequest {
    pub fn validate(&self) -> Result<(), FileTransferError> {
        match self {
            Self::PutBegin { manifest } => {
                manifest.validate()?;
                if manifest.direction != FileTransferDirection::ControllerToDevice {
                    return Err(FileTransferError::WrongDirection);
                }
            }
            Self::PutChunk { chunk } => chunk.validate()?,
            Self::Status { .. } | Self::Finalize { .. } | Self::Cancel { .. } => {}
        }
        Ok(())
    }

    pub fn into_message(self) -> Result<InnerMessage, FileTransferError> {
        self.validate()?;
        let payload = serde_json::to_vec(&self)?;
        if payload.len() > MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES {
            return Err(FileTransferError::RpcPayloadTooLarge(payload.len()));
        }
        Ok(InnerMessage::new("file_transfer", payload)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FileTransferError> {
        if message.kind != "file_transfer" {
            return Err(FileTransferError::UnexpectedKind(message.kind.clone()));
        }
        if message.payload.len() > MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES {
            return Err(FileTransferError::RpcPayloadTooLarge(message.payload.len()));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferPhase {
    Receiving,
    ReadyToFinalize,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferStatus {
    pub descriptor: FileResumeDescriptor,
    pub phase: FileTransferPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_device_path: Option<String>,
}

impl FileTransferStatus {
    pub fn validate(&self) -> Result<(), FileTransferError> {
        self.descriptor.validate()?;
        match self.phase {
            FileTransferPhase::Receiving if self.descriptor.is_complete() => {
                return Err(FileTransferError::InvalidPhase);
            }
            FileTransferPhase::ReadyToFinalize | FileTransferPhase::Completed
                if !self.descriptor.is_complete() =>
            {
                return Err(FileTransferError::InvalidPhase);
            }
            _ => {}
        }
        match (self.phase, &self.final_device_path) {
            (FileTransferPhase::Completed, Some(path)) => {
                if path.trim().is_empty()
                    || path.len() > MAX_FILE_RESUME_PATH_BYTES
                    || path.contains('\0')
                {
                    return Err(FileTransferError::InvalidDevicePath);
                }
            }
            (FileTransferPhase::Completed, None) => return Err(FileTransferError::InvalidPhase),
            (_, Some(_)) => return Err(FileTransferError::InvalidPhase),
            (_, None) => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTransferErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    Conflict,
    OutOfOrder,
    Capacity,
    HashMismatch,
    Io,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileTransferErrorBody {
    pub code: FileTransferErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FileTransferReply {
    Status(FileTransferStatus),
    Cancelled { transfer_id: TransferId },
    Error(FileTransferErrorBody),
}

impl FileTransferReply {
    #[must_use]
    pub fn error(code: FileTransferErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_FILE_TRANSFER_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_FILE_TRANSFER_ERROR_MESSAGE_BYTES);
        }
        Self::Error(FileTransferErrorBody { code, message })
    }

    pub fn validate(&self) -> Result<(), FileTransferError> {
        match self {
            Self::Status(status) => status.validate(),
            Self::Cancelled { .. } => Ok(()),
            Self::Error(error) => {
                if error.message.len() > MAX_FILE_TRANSFER_ERROR_MESSAGE_BYTES {
                    return Err(FileTransferError::ErrorMessageTooLarge);
                }
                Ok(())
            }
        }
    }

    pub fn into_message(self) -> Result<InnerMessage, FileTransferError> {
        self.validate()?;
        let payload = serde_json::to_vec(&self)?;
        if payload.len() > MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES {
            return Err(FileTransferError::RpcPayloadTooLarge(payload.len()));
        }
        Ok(InnerMessage::new("file_transfer_result", payload)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FileTransferError> {
        if message.kind != "file_transfer_result" {
            return Err(FileTransferError::UnexpectedKind(message.kind.clone()));
        }
        if message.payload.len() > MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES {
            return Err(FileTransferError::RpcPayloadTooLarge(message.payload.len()));
        }
        let reply: Self = serde_json::from_slice(&message.payload)?;
        reply.validate()?;
        Ok(reply)
    }
}

#[derive(Debug, Error)]
pub enum FileTransferError {
    #[error("unsupported file transfer manifest version {0}")]
    UnsupportedManifestVersion(u32),
    #[error("file transfer device path is invalid or exceeds its hard bound")]
    InvalidDevicePath,
    #[error(
        "file transfer chunk size must be a power of two within {MIN_FILE_CHUNK_BYTES}..={MAX_FILE_CHUNK_BYTES}, got {0}"
    )]
    InvalidChunkSize(u32),
    #[error("file transfer SHA-256 must be canonical lowercase hex")]
    InvalidSha256,
    #[error("empty file manifest must use the SHA-256 of empty bytes")]
    EmptyFileHashMismatch,
    #[error("Controller-to-device transfer requires an explicit device conflict policy")]
    MissingDeviceConflictPolicy,
    #[error("Device-to-controller manifest must not expose Controller-private conflict policy")]
    ControllerPrivateConflictPolicyLeaked,
    #[error("file transfer manifest exceeds {MAX_FILE_TRANSFER_MANIFEST_BYTES} bytes: {0}")]
    ManifestTooLarge(usize),
    #[error("file transfer chunk length must be 1..={MAX_FILE_CHUNK_BYTES}, got {0}")]
    InvalidChunkLength(usize),
    #[error("file transfer chunk Base64 encoding exceeds its hard bound: {0} bytes")]
    ChunkEncodingTooLarge(usize),
    #[error(
        "file transfer chunk document exceeds {MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES} bytes: {0}"
    )]
    ChunkDocumentTooLarge(usize),
    #[error("file transfer chunk SHA-256 does not match decoded bytes")]
    ChunkHashMismatch,
    #[error("file transfer chunk belongs to a different TransferId")]
    ChunkTransferMismatch,
    #[error("file transfer chunk offset {offset} is beyond total size {total_size}")]
    ChunkOffsetBeyondEnd { offset: u64, total_size: u64 },
    #[error("file transfer chunk offset {offset} is not aligned to chunk size {chunk_size}")]
    ChunkOffsetUnaligned { offset: u64, chunk_size: u32 },
    #[error("file transfer chunk length mismatch: expected {expected}, got {actual}")]
    UnexpectedChunkLength { expected: usize, actual: usize },
    #[error("file transfer request uses the wrong direction for this operation")]
    WrongDirection,
    #[error("file transfer status phase is inconsistent with its resume descriptor")]
    InvalidPhase,
    #[error("file transfer RPC payload exceeds {MAX_FILE_TRANSFER_RPC_PAYLOAD_BYTES} bytes: {0}")]
    RpcPayloadTooLarge(usize),
    #[error("file transfer error message exceeds its hard bound")]
    ErrorMessageTooLarge,
    #[error("unexpected file transfer message kind: {0}")]
    UnexpectedKind(String),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
    #[error(transparent)]
    Resume(#[from] FileResumeError),
    #[error("file transfer JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("file transfer Base64 failed: {0}")]
    Base64(#[from] base64::DecodeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(direction: FileTransferDirection, size: u64) -> FileTransferManifest {
        let final_sha256 = if size == 0 {
            EMPTY_SHA256_HEX.to_owned()
        } else {
            "11".repeat(32)
        };
        FileTransferManifest::new(
            TransferId::new(),
            ControllerId::new(),
            SiteId::new(),
            DeviceId::new(),
            direction,
            "/device/data.bin",
            size,
            4 * 1024,
            final_sha256,
            (direction == FileTransferDirection::ControllerToDevice)
                .then_some(FileConflictPolicy::FailIfExists),
        )
        .unwrap()
    }

    #[test]
    fn manifest_roundtrip_binds_resume_scope_without_controller_local_path() {
        let manifest = manifest(FileTransferDirection::ControllerToDevice, 5_000);
        assert_eq!(manifest.chunk_count().unwrap(), 2);
        let encoded = manifest.encode().unwrap();
        assert!(encoded.len() < MAX_FILE_TRANSFER_MANIFEST_BYTES);
        assert_eq!(FileTransferManifest::decode(&encoded).unwrap(), manifest);
        let resume = manifest.initial_resume_descriptor().unwrap();
        assert_eq!(resume.transfer_id, manifest.transfer_id);
        assert_eq!(resume.device_path, manifest.device_path);
        assert_eq!(resume.total_size, manifest.total_size);
        assert_eq!(resume.confirmed_offset, 0);
        assert_eq!(resume.confirmed_prefix_sha256, EMPTY_SHA256_HEX);
        assert_eq!(resume.final_sha256.as_ref(), Some(&manifest.final_sha256));
        let json = String::from_utf8(encoded).unwrap();
        assert!(!json.contains("controller_path"));
    }

    #[test]
    fn direction_controls_only_peer_visible_device_conflict_policy() {
        assert!(
            FileTransferManifest::new(
                TransferId::new(),
                ControllerId::new(),
                SiteId::new(),
                DeviceId::new(),
                FileTransferDirection::ControllerToDevice,
                "/x",
                1,
                4096,
                "11".repeat(32),
                None,
            )
            .is_err()
        );
        assert!(
            FileTransferManifest::new(
                TransferId::new(),
                ControllerId::new(),
                SiteId::new(),
                DeviceId::new(),
                FileTransferDirection::DeviceToController,
                "/x",
                1,
                4096,
                "11".repeat(32),
                Some(FileConflictPolicy::ReplaceExisting),
            )
            .is_err()
        );
        assert!(
            manifest(FileTransferDirection::DeviceToController, 1)
                .device_conflict_policy
                .is_none()
        );
    }

    #[test]
    fn deterministic_chunks_verify_alignment_length_and_hash() {
        let manifest = manifest(FileTransferDirection::ControllerToDevice, 5_000);
        let first =
            FileTransferChunk::from_bytes(manifest.transfer_id, 0, &vec![7_u8; 4096]).unwrap();
        manifest.validate_chunk(&first).unwrap();
        let last =
            FileTransferChunk::from_bytes(manifest.transfer_id, 4096, &vec![8_u8; 904]).unwrap();
        manifest.validate_chunk(&last).unwrap();

        let short = FileTransferChunk::from_bytes(manifest.transfer_id, 0, b"short").unwrap();
        assert!(matches!(
            manifest.validate_chunk(&short),
            Err(FileTransferError::UnexpectedChunkLength { .. })
        ));
        let unaligned =
            FileTransferChunk::from_bytes(manifest.transfer_id, 1, &vec![7_u8; 4096]).unwrap();
        assert!(matches!(
            manifest.validate_chunk(&unaligned),
            Err(FileTransferError::ChunkOffsetUnaligned { .. })
        ));
        let mut corrupt = first;
        corrupt.data_base64 = BASE64_STANDARD.encode(vec![9_u8; 4096]);
        assert!(matches!(
            corrupt.validate(),
            Err(FileTransferError::ChunkHashMismatch)
        ));
        let encoded = last.encode().unwrap();
        assert!(encoded.len() < MAX_FILE_TRANSFER_CHUNK_ENCODED_BYTES);
        assert_eq!(FileTransferChunk::decode(&encoded).unwrap(), last);

        let oversized_encoding = FileTransferChunk {
            transfer_id: manifest.transfer_id,
            offset: 0,
            sha256: "11".repeat(32),
            data_base64: "A".repeat(MAX_FILE_CHUNK_BASE64_BYTES + 1),
        };
        assert!(matches!(
            oversized_encoding.validate(),
            Err(FileTransferError::ChunkEncodingTooLarge(_))
        ));
    }

    #[test]
    fn put_rpc_separates_receiving_ready_and_completed_phases() {
        let put_manifest = manifest(FileTransferDirection::ControllerToDevice, 5_000);
        let begin = FileTransferRequest::PutBegin {
            manifest: put_manifest.clone(),
        };
        assert_eq!(
            FileTransferRequest::from_message(&begin.clone().into_message().unwrap()).unwrap(),
            begin
        );

        let receiving = FileTransferStatus {
            descriptor: put_manifest.initial_resume_descriptor().unwrap(),
            phase: FileTransferPhase::Receiving,
            final_device_path: None,
        };
        receiving.validate().unwrap();

        let mut complete_descriptor = receiving.descriptor.clone();
        complete_descriptor.checkpoint_revision = 2;
        complete_descriptor.confirmed_offset = put_manifest.total_size;
        complete_descriptor.confirmed_prefix_sha256 = put_manifest.final_sha256.clone();
        let ready = FileTransferReply::Status(FileTransferStatus {
            descriptor: complete_descriptor.clone(),
            phase: FileTransferPhase::ReadyToFinalize,
            final_device_path: None,
        });
        assert_eq!(
            FileTransferReply::from_message(&ready.clone().into_message().unwrap()).unwrap(),
            ready
        );
        let completed = FileTransferStatus {
            descriptor: complete_descriptor,
            phase: FileTransferPhase::Completed,
            final_device_path: Some("/device/data.bin".into()),
        };
        completed.validate().unwrap();

        let invalid = FileTransferStatus {
            phase: FileTransferPhase::Completed,
            final_device_path: None,
            ..completed
        };
        assert!(matches!(
            invalid.validate(),
            Err(FileTransferError::InvalidPhase)
        ));
        assert!(matches!(
            FileTransferRequest::PutBegin {
                manifest: manifest(FileTransferDirection::DeviceToController, 1),
            }
            .validate(),
            Err(FileTransferError::WrongDirection)
        ));
    }

    #[test]
    fn empty_file_and_chunk_bounds_fail_closed() {
        let empty = manifest(FileTransferDirection::DeviceToController, 0);
        assert_eq!(empty.chunk_count().unwrap(), 0);
        let huge = manifest(FileTransferDirection::DeviceToController, u64::MAX);
        assert_eq!(
            huge.chunk_count().unwrap(),
            u64::MAX.div_ceil(u64::from(huge.chunk_size))
        );
        empty.initial_resume_descriptor().unwrap();
        assert!(
            FileTransferManifest::new(
                TransferId::new(),
                ControllerId::new(),
                SiteId::new(),
                DeviceId::new(),
                FileTransferDirection::DeviceToController,
                "/x",
                0,
                4096,
                "11".repeat(32),
                None,
            )
            .is_err()
        );
        assert!(FileTransferChunk::from_bytes(TransferId::new(), 0, &[]).is_err());
        let mut invalid_chunk_size = empty.clone();
        invalid_chunk_size.chunk_size = 0;
        assert!(matches!(
            invalid_chunk_size.chunk_count(),
            Err(FileTransferError::InvalidChunkSize(0))
        ));
        assert!(
            FileTransferChunk::from_bytes(
                TransferId::new(),
                0,
                &vec![0_u8; MAX_FILE_CHUNK_BYTES as usize + 1]
            )
            .is_err()
        );
    }
}
