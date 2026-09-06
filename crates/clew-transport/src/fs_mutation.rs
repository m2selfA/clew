use clew_core::HARD_MAX_READ_ROOT_BYTES;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{InnerMessage, InnerSessionError};

pub const HARD_MAX_WRITE_TEXT_BYTES: usize = 32 * 1024;
pub const HARD_MAX_EDIT_FRAGMENT_BYTES: usize = 16 * 1024;
pub const HARD_MAX_FS_CONTROL_DESCRIPTION_BYTES: usize = 512;
pub const HARD_MAX_FS_CONTROL_ITEMS: usize = 128;
pub const HARD_MAX_MANAGED_TEMP_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MAX_MUTATION_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsWritePrecondition {
    CreateOnly,
    MatchSha256(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsManagedTempKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsTrashItem {
    pub trash_id: String,
    pub original_path: String,
    pub deleted_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsManagedTempResource {
    pub resource_id: String,
    pub kind: FsManagedTempKind,
    pub path: String,
    pub description: String,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FsControlResult {
    CreatedDirectory {
        path: String,
    },
    Copied {
        source: String,
        destination: String,
    },
    Moved {
        source: String,
        destination: String,
    },
    Trashed(FsTrashItem),
    TrashItems {
        items: Vec<FsTrashItem>,
    },
    Restored {
        trash_id: String,
        path: String,
    },
    PurgePrepared {
        trash_id: String,
        confirmation_token: String,
        expires_unix_ms: u64,
    },
    Purged {
        trash_id: String,
    },
    TempCreated(FsManagedTempResource),
    TempItems {
        items: Vec<FsManagedTempResource>,
    },
    TempReleased {
        resource_id: String,
    },
    TempGc {
        removed_resource_ids: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FsMutationRequest {
    Write {
        path: String,
        contents: String,
        precondition: FsWritePrecondition,
    },
    Edit {
        path: String,
        expected_sha256: String,
        old: String,
        new: String,
    },
    CreateDirectory {
        path: String,
    },
    Copy {
        source: String,
        destination: String,
    },
    Move {
        source: String,
        destination: String,
    },
    Trash {
        path: String,
    },
    TrashList,
    TrashRestore {
        trash_id: String,
    },
    TrashPurgePrepare {
        trash_id: String,
    },
    TrashPurgeConfirm {
        trash_id: String,
        confirmation_token: String,
    },
    TempCreate {
        temp_kind: FsManagedTempKind,
        description: String,
        ttl_ms: u64,
    },
    TempList,
    TempRelease {
        resource_id: String,
    },
    TempGc,
}

impl FsMutationRequest {
    pub fn write(
        path: impl Into<String>,
        contents: impl Into<String>,
        precondition: FsWritePrecondition,
    ) -> Result<Self, FsMutationProtocolError> {
        let request = Self::Write {
            path: path.into(),
            contents: contents.into(),
            precondition,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn edit(
        path: impl Into<String>,
        expected_sha256: impl Into<String>,
        old: impl Into<String>,
        new: impl Into<String>,
    ) -> Result<Self, FsMutationProtocolError> {
        let request = Self::Edit {
            path: path.into(),
            expected_sha256: expected_sha256.into(),
            old: old.into(),
            new: new.into(),
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), FsMutationProtocolError> {
        match self {
            Self::Write {
                path,
                contents,
                precondition,
            } => {
                validate_path(path)?;
                if contents.len() > HARD_MAX_WRITE_TEXT_BYTES {
                    return Err(FsMutationProtocolError::ContentTooLarge(contents.len()));
                }
                if let FsWritePrecondition::MatchSha256(hash) = precondition {
                    validate_sha256(hash)?;
                }
            }
            Self::Edit {
                path,
                expected_sha256,
                old,
                new,
            } => {
                validate_path(path)?;
                validate_sha256(expected_sha256)?;
                if old.is_empty()
                    || old.len() > HARD_MAX_EDIT_FRAGMENT_BYTES
                    || new.len() > HARD_MAX_EDIT_FRAGMENT_BYTES
                {
                    return Err(FsMutationProtocolError::InvalidEditFragments);
                }
            }
            Self::CreateDirectory { path } | Self::Trash { path } => validate_path(path)?,
            Self::Copy {
                source,
                destination,
            }
            | Self::Move {
                source,
                destination,
            } => {
                validate_path(source)?;
                validate_path(destination)?;
                if source == destination {
                    return Err(FsMutationProtocolError::InvalidPath);
                }
            }
            Self::TrashList | Self::TempList | Self::TempGc => {}
            Self::TrashRestore { trash_id }
            | Self::TrashPurgePrepare { trash_id }
            | Self::TempRelease {
                resource_id: trash_id,
            } => validate_control_id(trash_id)?,
            Self::TrashPurgeConfirm {
                trash_id,
                confirmation_token,
            } => {
                validate_control_id(trash_id)?;
                validate_control_id(confirmation_token)?;
            }
            Self::TempCreate {
                description,
                ttl_ms,
                ..
            } => {
                if description.trim().is_empty()
                    || description.len() > HARD_MAX_FS_CONTROL_DESCRIPTION_BYTES
                {
                    return Err(FsMutationProtocolError::InvalidDescription);
                }
                if *ttl_ms == 0 || *ttl_ms > HARD_MAX_MANAGED_TEMP_TTL_MS {
                    return Err(FsMutationProtocolError::InvalidTempTtl(*ttl_ms));
                }
            }
        }
        Ok(())
    }

    pub fn into_message(self) -> Result<InnerMessage, FsMutationProtocolError> {
        self.validate()?;
        Ok(InnerMessage::new(
            "fs_mutation",
            serde_json::to_vec(&self)?,
        )?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FsMutationProtocolError> {
        if message.kind != "fs_mutation" {
            return Err(FsMutationProtocolError::UnexpectedKind(
                message.kind.clone(),
            ));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

fn validate_path(path: &str) -> Result<(), FsMutationProtocolError> {
    let path = path.trim();
    if path.is_empty() || path.len() > HARD_MAX_READ_ROOT_BYTES || path.contains('\0') {
        return Err(FsMutationProtocolError::InvalidPath);
    }
    Ok(())
}

fn validate_control_id(value: &str) -> Result<(), FsMutationProtocolError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(FsMutationProtocolError::InvalidControlId);
    }
    Ok(())
}

pub fn normalize_sha256_hex(hash: &str) -> Result<String, FsMutationProtocolError> {
    validate_sha256(hash)?;
    Ok(hash.to_ascii_lowercase())
}

fn validate_sha256(hash: &str) -> Result<(), FsMutationProtocolError> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(FsMutationProtocolError::InvalidSha256);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsMutationErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    AlreadyExists,
    Conflict,
    NotFile,
    ContentLimit,
    Capacity,
    Io,
    Timeout,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsMutationErrorBody {
    pub code: FsMutationErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsMutationResult {
    pub sha256: String,
    pub size: u64,
    pub created: bool,
}

impl FsControlResult {
    fn validate(&self) -> Result<(), FsMutationProtocolError> {
        let check_path = |path: &str| validate_path(path);
        let check_item = |item: &FsTrashItem| -> Result<(), FsMutationProtocolError> {
            validate_control_id(&item.trash_id)?;
            check_path(&item.original_path)
        };
        let check_temp = |item: &FsManagedTempResource| -> Result<(), FsMutationProtocolError> {
            validate_control_id(&item.resource_id)?;
            check_path(&item.path)?;
            if item.description.trim().is_empty()
                || item.description.len() > HARD_MAX_FS_CONTROL_DESCRIPTION_BYTES
            {
                return Err(FsMutationProtocolError::InvalidDescription);
            }
            Ok(())
        };
        match self {
            Self::CreatedDirectory { path } => check_path(path)?,
            Self::Copied {
                source,
                destination,
            }
            | Self::Moved {
                source,
                destination,
            } => {
                check_path(source)?;
                check_path(destination)?;
            }
            Self::Trashed(item) => check_item(item)?,
            Self::TrashItems { items } => {
                if items.len() > HARD_MAX_FS_CONTROL_ITEMS {
                    return Err(FsMutationProtocolError::TooManyControlItems(items.len()));
                }
                for item in items {
                    check_item(item)?;
                }
            }
            Self::Restored { trash_id, path } => {
                validate_control_id(trash_id)?;
                check_path(path)?;
            }
            Self::PurgePrepared {
                trash_id,
                confirmation_token,
                ..
            } => {
                validate_control_id(trash_id)?;
                validate_control_id(confirmation_token)?;
            }
            Self::Purged { trash_id } => validate_control_id(trash_id)?,
            Self::TempCreated(item) => check_temp(item)?,
            Self::TempItems { items } => {
                if items.len() > HARD_MAX_FS_CONTROL_ITEMS {
                    return Err(FsMutationProtocolError::TooManyControlItems(items.len()));
                }
                for item in items {
                    check_temp(item)?;
                }
            }
            Self::TempReleased { resource_id } => validate_control_id(resource_id)?,
            Self::TempGc {
                removed_resource_ids,
            } => {
                if removed_resource_ids.len() > HARD_MAX_FS_CONTROL_ITEMS {
                    return Err(FsMutationProtocolError::TooManyControlItems(
                        removed_resource_ids.len(),
                    ));
                }
                for id in removed_resource_ids {
                    validate_control_id(id)?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FsMutationReply {
    Result(FsMutationResult),
    Control(FsControlResult),
    Error(FsMutationErrorBody),
}

impl FsMutationReply {
    #[must_use]
    pub fn error(code: FsMutationErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_MUTATION_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_MUTATION_ERROR_MESSAGE_BYTES);
        }
        Self::Error(FsMutationErrorBody { code, message })
    }

    pub fn into_message(self) -> Result<InnerMessage, FsMutationProtocolError> {
        validate_reply(&self)?;
        Ok(InnerMessage::new(
            "fs_mutation_result",
            serde_json::to_vec(&self)?,
        )?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FsMutationProtocolError> {
        if message.kind != "fs_mutation_result" {
            return Err(FsMutationProtocolError::UnexpectedKind(
                message.kind.clone(),
            ));
        }
        let reply: Self = serde_json::from_slice(&message.payload)?;
        validate_reply(&reply)?;
        Ok(reply)
    }
}

fn validate_reply(reply: &FsMutationReply) -> Result<(), FsMutationProtocolError> {
    match reply {
        FsMutationReply::Result(result) => {
            validate_sha256(&result.sha256)?;
            if result.size > HARD_MAX_WRITE_TEXT_BYTES as u64 {
                return Err(FsMutationProtocolError::ContentTooLarge(
                    result.size.min(usize::MAX as u64) as usize,
                ));
            }
        }
        FsMutationReply::Control(result) => result.validate()?,
        FsMutationReply::Error(error) => {
            if error.message.len() > MAX_MUTATION_ERROR_MESSAGE_BYTES {
                return Err(FsMutationProtocolError::ErrorMessageTooLarge);
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum FsMutationProtocolError {
    #[error("filesystem mutation path must be 1..={HARD_MAX_READ_ROOT_BYTES} UTF-8 bytes")]
    InvalidPath,
    #[error("filesystem mutation SHA-256 must be exactly 64 hexadecimal characters")]
    InvalidSha256,
    #[error(
        "filesystem mutation content exceeds the {HARD_MAX_WRITE_TEXT_BYTES}-byte hard bound: {0}"
    )]
    ContentTooLarge(usize),
    #[error(
        "Edit old text must be non-empty and old/new must each be <= {HARD_MAX_EDIT_FRAGMENT_BYTES} bytes"
    )]
    InvalidEditFragments,
    #[error("filesystem control id/token is invalid")]
    InvalidControlId,
    #[error("managed temporary resource description is invalid")]
    InvalidDescription,
    #[error("managed temporary resource TTL is invalid: {0} ms")]
    InvalidTempTtl(u64),
    #[error("filesystem control result contains too many items: {0}")]
    TooManyControlItems(usize),
    #[error("filesystem mutation error message exceeds its hard bound")]
    ErrorMessageTooLarge,
    #[error("unexpected inner message kind: {0}")]
    UnexpectedKind(String),
    #[error("filesystem mutation JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_edit_roundtrip_with_explicit_preconditions() {
        let write =
            FsMutationRequest::write("/shared/new.txt", "hello", FsWritePrecondition::CreateOnly)
                .unwrap();
        assert_eq!(
            FsMutationRequest::from_message(&write.clone().into_message().unwrap()).unwrap(),
            write
        );

        let hash = "ab".repeat(32);
        let edit =
            FsMutationRequest::edit("/shared/existing.txt", hash.clone(), "old", "new").unwrap();
        assert_eq!(
            FsMutationRequest::from_message(&edit.clone().into_message().unwrap()).unwrap(),
            edit
        );
        assert!(FsMutationRequest::edit("/shared/x", hash, "", "new").is_err());
        assert!(
            FsMutationRequest::write(
                "/shared/x",
                "x".repeat(HARD_MAX_WRITE_TEXT_BYTES + 1),
                FsWritePrecondition::CreateOnly,
            )
            .is_err()
        );

        let reply = FsMutationReply::Result(FsMutationResult {
            sha256: "01".repeat(32),
            size: 5,
            created: true,
        });
        assert_eq!(
            FsMutationReply::from_message(&reply.clone().into_message().unwrap()).unwrap(),
            reply
        );
    }
}
