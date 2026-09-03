use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_core::{HARD_MAX_READ_ROOT_BYTES, TaskId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{InnerMessage, InnerSessionError};

pub const HARD_MAX_SHELL_COMMAND_BYTES: usize = 8 * 1024;
pub const HARD_MAX_SHELL_ENV_ENTRIES: usize = 32;
pub const HARD_MAX_SHELL_ENV_KEY_BYTES: usize = 128;
pub const HARD_MAX_SHELL_ENV_VALUE_BYTES: usize = 2 * 1024;
pub const HARD_MAX_SHELL_ENV_TOTAL_BYTES: usize = 16 * 1024;
pub const HARD_MAX_SHELL_TIMEOUT_MS: u32 = 30 * 60 * 1_000;
pub const HARD_MAX_SHELL_TASKS_PER_SESSION: usize = 64;
pub const HARD_MAX_SHELL_RETAINED_BYTES_PER_STREAM: usize = 32 * 1024;
pub const HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM: u32 = 12 * 1024;
const MAX_SHELL_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ShellTaskRequest {
    Start {
        command: String,
        cwd: String,
        #[serde(default)]
        env: BTreeMap<String, String>,
        timeout_ms: u32,
    },
    Status {
        task_id: TaskId,
    },
    Attach {
        task_id: TaskId,
        #[serde(default)]
        stdout_offset: u64,
        #[serde(default)]
        stderr_offset: u64,
        max_bytes_per_stream: u32,
    },
    Cancel {
        task_id: TaskId,
    },
}

impl ShellTaskRequest {
    pub fn start(
        command: impl Into<String>,
        cwd: impl Into<String>,
        env: BTreeMap<String, String>,
        timeout_ms: u32,
    ) -> Result<Self, ShellTaskProtocolError> {
        let request = Self::Start {
            command: command.into(),
            cwd: cwd.into(),
            env,
            timeout_ms,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ShellTaskProtocolError> {
        match self {
            Self::Start {
                command,
                cwd,
                env,
                timeout_ms,
            } => {
                validate_command(command)?;
                validate_cwd(cwd)?;
                validate_env(env)?;
                if *timeout_ms == 0 || *timeout_ms > HARD_MAX_SHELL_TIMEOUT_MS {
                    return Err(ShellTaskProtocolError::InvalidTimeout(*timeout_ms));
                }
            }
            Self::Attach {
                max_bytes_per_stream,
                ..
            } => {
                if *max_bytes_per_stream == 0
                    || *max_bytes_per_stream > HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM
                {
                    return Err(ShellTaskProtocolError::InvalidAttachLimit(
                        *max_bytes_per_stream,
                    ));
                }
            }
            Self::Status { .. } | Self::Cancel { .. } => {}
        }
        Ok(())
    }

    pub fn into_message(self) -> Result<InnerMessage, ShellTaskProtocolError> {
        self.validate()?;
        Ok(InnerMessage::new("shell_task", serde_json::to_vec(&self)?)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, ShellTaskProtocolError> {
        if message.kind != "shell_task" {
            return Err(ShellTaskProtocolError::UnexpectedKind(message.kind.clone()));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

fn validate_command(command: &str) -> Result<(), ShellTaskProtocolError> {
    let command = command.trim();
    if command.is_empty() || command.len() > HARD_MAX_SHELL_COMMAND_BYTES || command.contains('\0')
    {
        return Err(ShellTaskProtocolError::InvalidCommand);
    }
    Ok(())
}

fn validate_cwd(cwd: &str) -> Result<(), ShellTaskProtocolError> {
    let cwd = cwd.trim();
    if cwd.is_empty() || cwd.len() > HARD_MAX_READ_ROOT_BYTES || cwd.contains('\0') {
        return Err(ShellTaskProtocolError::InvalidCwd);
    }
    Ok(())
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), ShellTaskProtocolError> {
    if env.len() > HARD_MAX_SHELL_ENV_ENTRIES {
        return Err(ShellTaskProtocolError::TooManyEnvEntries(env.len()));
    }
    let mut total = 0_usize;
    for (key, value) in env {
        if key.is_empty()
            || key.len() > HARD_MAX_SHELL_ENV_KEY_BYTES
            || !key
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
            || key.as_bytes()[0].is_ascii_digit()
            || value.len() > HARD_MAX_SHELL_ENV_VALUE_BYTES
            || value.contains('\0')
        {
            return Err(ShellTaskProtocolError::InvalidEnvEntry);
        }
        total = total
            .checked_add(key.len())
            .and_then(|value_so_far| value_so_far.checked_add(value.len()))
            .ok_or(ShellTaskProtocolError::EnvTooLarge(usize::MAX))?;
        if total > HARD_MAX_SHELL_ENV_TOTAL_BYTES {
            return Err(ShellTaskProtocolError::EnvTooLarge(total));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTaskPhase {
    Running,
    Exited,
    TimedOut,
    Cancelled,
    Failed,
}

impl ShellTaskPhase {
    #[must_use]
    pub const fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellTaskStatus {
    pub task_id: TaskId,
    pub phase: ShellTaskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout_base_offset: u64,
    pub stdout_next_offset: u64,
    pub stderr_base_offset: u64,
    pub stderr_next_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellOutputChunk {
    pub requested_offset: u64,
    pub start_offset: u64,
    pub next_offset: u64,
    pub retained_base_offset: u64,
    pub retained_next_offset: u64,
    pub lost_prefix: bool,
    pub data_base64: String,
}

impl ShellOutputChunk {
    pub fn from_bytes(
        requested_offset: u64,
        start_offset: u64,
        next_offset: u64,
        retained_base_offset: u64,
        retained_next_offset: u64,
        lost_prefix: bool,
        data: &[u8],
    ) -> Result<Self, ShellTaskProtocolError> {
        if data.len() > HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM as usize {
            return Err(ShellTaskProtocolError::OutputChunkTooLarge(data.len()));
        }
        let chunk = Self {
            requested_offset,
            start_offset,
            next_offset,
            retained_base_offset,
            retained_next_offset,
            lost_prefix,
            data_base64: BASE64_STANDARD.encode(data),
        };
        validate_output_chunk(&chunk)?;
        Ok(chunk)
    }

    pub fn decode(&self) -> Result<Vec<u8>, ShellTaskProtocolError> {
        validate_output_chunk(self)?;
        Ok(BASE64_STANDARD.decode(&self.data_base64)?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellTaskOutput {
    pub status: ShellTaskStatus,
    pub stdout: ShellOutputChunk,
    pub stderr: ShellOutputChunk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTaskErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    Capacity,
    SpawnFailed,
    Io,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShellTaskErrorBody {
    pub code: ShellTaskErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ShellTaskReply {
    Started { task_id: TaskId },
    Status(ShellTaskStatus),
    Output(ShellTaskOutput),
    CancelAccepted { task_id: TaskId },
    Error(ShellTaskErrorBody),
}

impl ShellTaskReply {
    #[must_use]
    pub fn error(code: ShellTaskErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_SHELL_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_SHELL_ERROR_MESSAGE_BYTES);
        }
        Self::Error(ShellTaskErrorBody { code, message })
    }

    pub fn into_message(self) -> Result<InnerMessage, ShellTaskProtocolError> {
        validate_reply(&self)?;
        Ok(InnerMessage::new(
            "shell_task_result",
            serde_json::to_vec(&self)?,
        )?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, ShellTaskProtocolError> {
        if message.kind != "shell_task_result" {
            return Err(ShellTaskProtocolError::UnexpectedKind(message.kind.clone()));
        }
        let reply: Self = serde_json::from_slice(&message.payload)?;
        validate_reply(&reply)?;
        Ok(reply)
    }
}

fn validate_status(status: &ShellTaskStatus) -> Result<(), ShellTaskProtocolError> {
    if status.stdout_base_offset > status.stdout_next_offset
        || status.stderr_base_offset > status.stderr_next_offset
    {
        return Err(ShellTaskProtocolError::InvalidOffsets);
    }
    Ok(())
}

fn validate_output_chunk(chunk: &ShellOutputChunk) -> Result<(), ShellTaskProtocolError> {
    if chunk.retained_base_offset > chunk.retained_next_offset
        || chunk.start_offset < chunk.retained_base_offset
        || chunk.next_offset < chunk.start_offset
        || chunk.next_offset > chunk.retained_next_offset
    {
        return Err(ShellTaskProtocolError::InvalidOffsets);
    }
    let data = BASE64_STANDARD.decode(&chunk.data_base64)?;
    if data.len() > HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM as usize {
        return Err(ShellTaskProtocolError::OutputChunkTooLarge(data.len()));
    }
    let data_len = u64::try_from(data.len()).map_err(|_| ShellTaskProtocolError::InvalidOffsets)?;
    if chunk.next_offset.saturating_sub(chunk.start_offset) != data_len {
        return Err(ShellTaskProtocolError::InvalidOffsets);
    }
    let expected_lost_prefix = chunk.requested_offset < chunk.retained_base_offset;
    if chunk.lost_prefix != expected_lost_prefix {
        return Err(ShellTaskProtocolError::InvalidOffsets);
    }
    Ok(())
}

fn validate_reply(reply: &ShellTaskReply) -> Result<(), ShellTaskProtocolError> {
    match reply {
        ShellTaskReply::Started { .. } | ShellTaskReply::CancelAccepted { .. } => Ok(()),
        ShellTaskReply::Status(status) => validate_status(status),
        ShellTaskReply::Output(output) => {
            validate_status(&output.status)?;
            validate_output_chunk(&output.stdout)?;
            validate_output_chunk(&output.stderr)
        }
        ShellTaskReply::Error(error) => {
            if error.message.len() > MAX_SHELL_ERROR_MESSAGE_BYTES {
                return Err(ShellTaskProtocolError::ErrorMessageTooLarge);
            }
            Ok(())
        }
    }
}

#[derive(Debug, Error)]
pub enum ShellTaskProtocolError {
    #[error("Shell command must be 1..={HARD_MAX_SHELL_COMMAND_BYTES} UTF-8 bytes")]
    InvalidCommand,
    #[error("Shell cwd must be 1..={HARD_MAX_READ_ROOT_BYTES} UTF-8 bytes")]
    InvalidCwd,
    #[error("Shell environment contains too many entries: {0}")]
    TooManyEnvEntries(usize),
    #[error("Shell environment entry is invalid or exceeds its per-entry hard bound")]
    InvalidEnvEntry,
    #[error("Shell environment exceeds the {HARD_MAX_SHELL_ENV_TOTAL_BYTES}-byte hard bound: {0}")]
    EnvTooLarge(usize),
    #[error("Shell timeout must be within 1..={HARD_MAX_SHELL_TIMEOUT_MS} ms, got {0}")]
    InvalidTimeout(u32),
    #[error(
        "Shell attach limit must be within 1..={HARD_MAX_SHELL_ATTACH_BYTES_PER_STREAM} bytes per stream, got {0}"
    )]
    InvalidAttachLimit(u32),
    #[error("Shell output chunk exceeds its hard bound: {0} bytes")]
    OutputChunkTooLarge(usize),
    #[error("Shell task offsets are inconsistent")]
    InvalidOffsets,
    #[error("Shell task error message exceeds its hard bound")]
    ErrorMessageTooLarge,
    #[error("unexpected inner message kind: {0}")]
    UnexpectedKind(String),
    #[error("Shell task JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Shell task Base64 failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_task_roundtrip_and_bounds_are_strict() {
        let mut env = BTreeMap::new();
        env.insert("MODE".into(), "test".into());
        let start = ShellTaskRequest::start("echo hi", "/shared", env, 5_000).unwrap();
        assert_eq!(
            ShellTaskRequest::from_message(&start.clone().into_message().unwrap()).unwrap(),
            start
        );
        assert!(
            ShellTaskRequest::start(
                "x".repeat(HARD_MAX_SHELL_COMMAND_BYTES + 1),
                "/shared",
                BTreeMap::new(),
                5_000,
            )
            .is_err()
        );
        assert!(ShellTaskRequest::start("echo hi", "/shared", BTreeMap::new(), 0).is_err());

        let task_id = TaskId::new();
        let output = ShellTaskReply::Output(ShellTaskOutput {
            status: ShellTaskStatus {
                task_id,
                phase: ShellTaskPhase::Running,
                exit_code: None,
                stdout_base_offset: 10,
                stdout_next_offset: 15,
                stderr_base_offset: 0,
                stderr_next_offset: 0,
            },
            stdout: ShellOutputChunk::from_bytes(0, 10, 15, 10, 15, true, b"hello").unwrap(),
            stderr: ShellOutputChunk::from_bytes(0, 0, 0, 0, 0, false, b"").unwrap(),
        });
        let decoded =
            ShellTaskReply::from_message(&output.clone().into_message().unwrap()).unwrap();
        assert_eq!(decoded, output);
        let ShellTaskReply::Output(output) = decoded else {
            unreachable!();
        };
        assert_eq!(output.stdout.decode().unwrap(), b"hello");
    }
}
