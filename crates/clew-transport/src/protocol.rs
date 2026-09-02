use std::time::Duration;

use clew_core::{
    DeviceId, DeviceRecord, HARD_MAX_READ_RESULT_BYTES, HARD_MAX_READ_ROOT_BYTES, InviteId,
};
use clew_identity::{DevicePublicIdentity, EnrollmentReceipt, SignedSiteBootstrapPass};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

use crate::{InnerMessage, InnerSessionError};

pub const MAX_BOOTSTRAP_FRAME_BYTES: usize = 64 * 1024;
const BOOTSTRAP_IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REMOTE_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapMemberMode {
    #[default]
    ExecutePreferred,
    ConnectorOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BootstrapRequest {
    Claim {
        bootstrap: SignedSiteBootstrapPass,
        device_identity: DevicePublicIdentity,
        hostname: String,
        #[serde(default)]
        mode: BootstrapMemberMode,
    },
    Persisted {
        invite_id: InviteId,
        device_id: DeviceId,
        persist_ack_token: [u8; 32],
        hostname: String,
    },
    ActivatedAck {
        invite_id: InviteId,
        device_id: DeviceId,
    },
    ActivationConfirmedAck {
        invite_id: InviteId,
        device_id: DeviceId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BootstrapResponse {
    Claimed(EnrollmentReceipt),
    Activated(DeviceRecord),
    ActivationConfirmed {
        invite_id: InviteId,
        device_id: DeviceId,
    },
    Error(BootstrapErrorBody),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapErrorCode {
    InvalidRequest,
    Denied,
    State,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BootstrapErrorBody {
    pub code: BootstrapErrorCode,
    pub message: String,
}

impl BootstrapErrorBody {
    #[must_use]
    pub fn new(code: BootstrapErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_REMOTE_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_REMOTE_ERROR_MESSAGE_BYTES);
        }
        Self { code, message }
    }
}

pub async fn write_bootstrap<T, S>(stream: &mut S, value: &T) -> Result<(), BootstrapProtocolError>
where
    T: Serialize,
    S: AsyncWrite + Unpin,
{
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > MAX_BOOTSTRAP_FRAME_BYTES {
        return Err(BootstrapProtocolError::FrameTooLarge(encoded.len()));
    }
    timeout(BOOTSTRAP_IO_TIMEOUT, async {
        stream.write_u32(encoded.len() as u32).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| BootstrapProtocolError::Timeout)??;
    Ok(())
}

pub async fn read_bootstrap<T, S>(stream: &mut S) -> Result<T, BootstrapProtocolError>
where
    T: DeserializeOwned,
    S: AsyncRead + Unpin,
{
    let length = timeout(BOOTSTRAP_IO_TIMEOUT, stream.read_u32())
        .await
        .map_err(|_| BootstrapProtocolError::Timeout)?? as usize;
    if length > MAX_BOOTSTRAP_FRAME_BYTES {
        return Err(BootstrapProtocolError::FrameTooLarge(length));
    }
    let mut encoded = vec![0_u8; length];
    timeout(BOOTSTRAP_IO_TIMEOUT, stream.read_exact(&mut encoded))
        .await
        .map_err(|_| BootstrapProtocolError::Timeout)??;
    Ok(serde_json::from_slice(&encoded)?)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadRequest {
    pub path: String,
    pub offset: u64,
    pub limit: u32,
}

impl ReadRequest {
    pub fn new(
        path: impl Into<String>,
        offset: u64,
        limit: u32,
    ) -> Result<Self, ReadProtocolError> {
        let request = Self {
            path: path.into(),
            offset,
            limit,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), ReadProtocolError> {
        if self.path.trim().is_empty() || self.path.len() > HARD_MAX_READ_ROOT_BYTES {
            return Err(ReadProtocolError::InvalidPath);
        }
        if self.limit == 0 || self.limit > HARD_MAX_READ_RESULT_BYTES {
            return Err(ReadProtocolError::InvalidLimit(self.limit));
        }
        Ok(())
    }

    pub fn into_message(self) -> Result<InnerMessage, ReadProtocolError> {
        self.validate()?;
        Ok(InnerMessage::new("read", serde_json::to_vec(&self)?)?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, ReadProtocolError> {
        if message.kind != "read" {
            return Err(ReadProtocolError::UnexpectedKind(message.kind.clone()));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    NotFile,
    Io,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReadErrorBody {
    pub code: ReadErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadReply {
    Data(Vec<u8>),
    Error(ReadErrorBody),
}

impl ReadReply {
    pub fn data(data: Vec<u8>) -> Result<Self, ReadProtocolError> {
        if data.len() > HARD_MAX_READ_RESULT_BYTES as usize {
            return Err(ReadProtocolError::ResultTooLarge(data.len()));
        }
        Ok(Self::Data(data))
    }

    #[must_use]
    pub fn error(code: ReadErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_REMOTE_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_REMOTE_ERROR_MESSAGE_BYTES);
        }
        Self::Error(ReadErrorBody { code, message })
    }

    pub fn into_message(self) -> Result<InnerMessage, ReadProtocolError> {
        match self {
            Self::Data(data) => Ok(InnerMessage::new("read_result", data)?),
            Self::Error(error) => Ok(InnerMessage::new(
                "read_error",
                serde_json::to_vec(&error)?,
            )?),
        }
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, ReadProtocolError> {
        match message.kind.as_str() {
            "read_result" => Self::data(message.payload.clone()),
            "read_error" => {
                let error: ReadErrorBody = serde_json::from_slice(&message.payload)?;
                Ok(Self::Error(error))
            }
            _ => Err(ReadProtocolError::UnexpectedKind(message.kind.clone())),
        }
    }
}

#[derive(Debug, Error)]
pub enum BootstrapProtocolError {
    #[error("bootstrap JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bootstrap I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("bootstrap frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("bootstrap I/O timed out")]
    Timeout,
}

#[derive(Debug, Error)]
pub enum ReadProtocolError {
    #[error("read path must be 1..={HARD_MAX_READ_ROOT_BYTES} UTF-8 bytes")]
    InvalidPath,
    #[error("read limit must be 1..={HARD_MAX_READ_RESULT_BYTES}, got {0}")]
    InvalidLimit(u32),
    #[error("read result is too large: {0} bytes")]
    ResultTooLarge(usize),
    #[error("unexpected inner message kind: {0}")]
    UnexpectedKind(String),
    #[error("read protocol JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test]
    async fn bootstrap_frame_rejects_oversize_before_allocating_body() {
        let (mut writer, mut reader) = duplex(64);
        let task = tokio::spawn(async move {
            writer
                .write_u32((MAX_BOOTSTRAP_FRAME_BYTES + 1) as u32)
                .await
                .unwrap();
        });
        assert!(matches!(
            read_bootstrap::<BootstrapRequest, _>(&mut reader).await,
            Err(BootstrapProtocolError::FrameTooLarge(size)) if size == MAX_BOOTSTRAP_FRAME_BYTES + 1
        ));
        task.await.unwrap();
    }

    #[test]
    fn read_request_and_raw_reply_keep_hard_bounds() {
        let request = ReadRequest::new("D:/shared/data.mrc", 5, 4096).unwrap();
        assert_eq!(
            ReadRequest::from_message(&request.clone().into_message().unwrap()).unwrap(),
            request
        );
        let data = vec![7_u8; HARD_MAX_READ_RESULT_BYTES as usize];
        let reply = ReadReply::data(data.clone()).unwrap();
        let decoded = ReadReply::from_message(&reply.into_message().unwrap()).unwrap();
        assert_eq!(decoded, ReadReply::Data(data));
        assert!(ReadRequest::new("x", 0, HARD_MAX_READ_RESULT_BYTES + 1).is_err());
    }
}
