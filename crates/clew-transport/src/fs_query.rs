use clew_core::{HARD_MAX_READ_RESULT_BYTES, HARD_MAX_READ_ROOT_BYTES};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{InnerMessage, InnerSessionError};

pub const HARD_MAX_FS_PATTERN_BYTES: usize = 1024;
pub const HARD_MAX_FS_RESULT_ITEMS: u32 = 1024;
pub const HARD_MAX_FS_SCAN_ENTRIES: usize = 100_000;
const MAX_FS_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FsQueryRequest {
    PathInfo {
        path: String,
    },
    Glob {
        root: String,
        pattern: String,
        #[serde(default)]
        cursor: u64,
        limit: u32,
        max_bytes: u32,
    },
}

impl FsQueryRequest {
    pub fn path_info(path: impl Into<String>) -> Result<Self, FsQueryProtocolError> {
        let request = Self::PathInfo { path: path.into() };
        request.validate()?;
        Ok(request)
    }

    pub fn glob(
        root: impl Into<String>,
        pattern: impl Into<String>,
        cursor: u64,
        limit: u32,
        max_bytes: u32,
    ) -> Result<Self, FsQueryProtocolError> {
        let request = Self::Glob {
            root: root.into(),
            pattern: pattern.into(),
            cursor,
            limit,
            max_bytes,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), FsQueryProtocolError> {
        match self {
            Self::PathInfo { path } => validate_path(path),
            Self::Glob {
                root,
                pattern,
                limit,
                max_bytes,
                ..
            } => {
                validate_path(root)?;
                let pattern = pattern.trim();
                if pattern.is_empty()
                    || pattern.len() > HARD_MAX_FS_PATTERN_BYTES
                    || pattern.contains('\0')
                {
                    return Err(FsQueryProtocolError::InvalidPattern);
                }
                if *limit == 0 || *limit > HARD_MAX_FS_RESULT_ITEMS {
                    return Err(FsQueryProtocolError::InvalidLimit(*limit));
                }
                if *max_bytes == 0 || *max_bytes > HARD_MAX_READ_RESULT_BYTES {
                    return Err(FsQueryProtocolError::InvalidByteLimit(*max_bytes));
                }
                Ok(())
            }
        }
    }

    pub fn into_message(self) -> Result<InnerMessage, FsQueryProtocolError> {
        self.validate()?;
        Ok(InnerMessage::new("fs_query", serde_json::to_vec(&self)?)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FsQueryProtocolError> {
        if message.kind != "fs_query" {
            return Err(FsQueryProtocolError::UnexpectedKind(message.kind.clone()));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

fn validate_path(path: &str) -> Result<(), FsQueryProtocolError> {
    let path = path.trim();
    if path.is_empty() || path.len() > HARD_MAX_READ_ROOT_BYTES || path.contains('\0') {
        return Err(FsQueryProtocolError::InvalidPath);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsPathKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsPathInfo {
    pub path: String,
    pub kind: FsPathKind,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsGlobPage {
    pub entries: Vec<FsPathInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsQueryErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    NotDirectory,
    Io,
    Timeout,
    ScanLimit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FsQueryErrorBody {
    pub code: FsQueryErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FsQueryReply {
    PathInfo(FsPathInfo),
    Glob(FsGlobPage),
    Error(FsQueryErrorBody),
}

impl FsQueryReply {
    #[must_use]
    pub fn error(code: FsQueryErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_FS_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_FS_ERROR_MESSAGE_BYTES);
        }
        Self::Error(FsQueryErrorBody { code, message })
    }

    pub fn into_message(self) -> Result<InnerMessage, FsQueryProtocolError> {
        validate_reply(&self)?;
        let payload = serde_json::to_vec(&self)?;
        if payload.len() > HARD_MAX_READ_RESULT_BYTES as usize {
            return Err(FsQueryProtocolError::ReplyTooLarge(payload.len()));
        }
        Ok(InnerMessage::new("fs_query_result", payload)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, FsQueryProtocolError> {
        if message.kind != "fs_query_result" {
            return Err(FsQueryProtocolError::UnexpectedKind(message.kind.clone()));
        }
        if message.payload.len() > HARD_MAX_READ_RESULT_BYTES as usize {
            return Err(FsQueryProtocolError::ReplyTooLarge(message.payload.len()));
        }
        let reply: Self = serde_json::from_slice(&message.payload)?;
        validate_reply(&reply)?;
        Ok(reply)
    }
}

fn validate_reply(reply: &FsQueryReply) -> Result<(), FsQueryProtocolError> {
    match reply {
        FsQueryReply::PathInfo(info) => validate_info(info),
        FsQueryReply::Glob(page) => {
            if page.entries.len() > HARD_MAX_FS_RESULT_ITEMS as usize {
                return Err(FsQueryProtocolError::TooManyResultItems(page.entries.len()));
            }
            for info in &page.entries {
                validate_info(info)?;
            }
            Ok(())
        }
        FsQueryReply::Error(error) => {
            if error.message.len() > MAX_FS_ERROR_MESSAGE_BYTES {
                return Err(FsQueryProtocolError::ErrorMessageTooLarge);
            }
            Ok(())
        }
    }
}

fn validate_info(info: &FsPathInfo) -> Result<(), FsQueryProtocolError> {
    if info.path.is_empty() || info.path.len() > HARD_MAX_READ_ROOT_BYTES {
        return Err(FsQueryProtocolError::InvalidPath);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum FsQueryProtocolError {
    #[error("filesystem query path must be 1..={HARD_MAX_READ_ROOT_BYTES} UTF-8 bytes")]
    InvalidPath,
    #[error("filesystem glob pattern must be 1..={HARD_MAX_FS_PATTERN_BYTES} UTF-8 bytes")]
    InvalidPattern,
    #[error("filesystem result item limit must be 1..={HARD_MAX_FS_RESULT_ITEMS}, got {0}")]
    InvalidLimit(u32),
    #[error("filesystem result byte limit must be 1..={HARD_MAX_READ_RESULT_BYTES}, got {0}")]
    InvalidByteLimit(u32),
    #[error("filesystem query reply contains too many items: {0}")]
    TooManyResultItems(usize),
    #[error("filesystem query reply exceeds the hard result bound: {0} bytes")]
    ReplyTooLarge(usize),
    #[error("filesystem query error message exceeds its hard bound")]
    ErrorMessageTooLarge,
    #[error("unexpected inner message kind: {0}")]
    UnexpectedKind(String),
    #[error("filesystem query JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_info_and_glob_roundtrip_with_bounds() {
        let info = FsQueryRequest::path_info("/shared/data.bin").unwrap();
        assert_eq!(
            FsQueryRequest::from_message(&info.clone().into_message().unwrap()).unwrap(),
            info
        );

        let glob = FsQueryRequest::glob("/shared", "**/*.rs", 7, 32, 16_384).unwrap();
        assert_eq!(
            FsQueryRequest::from_message(&glob.clone().into_message().unwrap()).unwrap(),
            glob
        );
        assert!(FsQueryRequest::glob("/shared", "**", 0, 0, 1024).is_err());
        assert!(
            FsQueryRequest::glob("/shared", "**", 0, 1, HARD_MAX_READ_RESULT_BYTES + 1).is_err()
        );

        let reply = FsQueryReply::Glob(FsGlobPage {
            entries: vec![FsPathInfo {
                path: "/shared/src/lib.rs".into(),
                kind: FsPathKind::File,
                size: 123,
                modified_unix_ms: Some(42),
            }],
            next_cursor: Some(8),
            truncated: true,
        });
        assert_eq!(
            FsQueryReply::from_message(&reply.clone().into_message().unwrap()).unwrap(),
            reply
        );

        let oversized = FsQueryReply::Glob(FsGlobPage {
            entries: (0..32)
                .map(|index| FsPathInfo {
                    path: format!("/shared/{index}-{}", "x".repeat(1_800)),
                    kind: FsPathKind::File,
                    size: 1,
                    modified_unix_ms: None,
                })
                .collect(),
            next_cursor: None,
            truncated: false,
        });
        assert!(matches!(
            oversized.into_message(),
            Err(FsQueryProtocolError::ReplyTooLarge(_))
        ));
    }
}
