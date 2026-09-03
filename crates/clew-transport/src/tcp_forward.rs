use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use clew_core::{ForwardConnectionId, ForwardId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{InnerMessage, InnerSessionError};

pub const HARD_MAX_TCP_FORWARD_HOST_BYTES: usize = 255;
pub const HARD_MAX_TCP_FORWARD_CHUNK_BYTES: u32 = 12 * 1024;
pub const HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS: u32 = 10_000;
pub const HARD_MAX_TCP_FORWARD_READ_WAIT_MS: u32 = 250;
pub const HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION: usize = 64;
const MAX_TCP_FORWARD_ERROR_MESSAGE_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TcpForwardDestination {
    pub host: String,
    pub port: u16,
}

impl TcpForwardDestination {
    pub fn new(host: impl Into<String>, port: u16) -> Result<Self, TcpForwardProtocolError> {
        let destination = Self {
            host: host.into(),
            port,
        };
        destination.validate()?;
        Ok(destination)
    }

    pub fn validate(&self) -> Result<(), TcpForwardProtocolError> {
        let host = self.host.trim();
        if host.is_empty()
            || host.len() > HARD_MAX_TCP_FORWARD_HOST_BYTES
            || !host.is_ascii()
            || host
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            || self.port == 0
        {
            return Err(TcpForwardProtocolError::InvalidDestination);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TcpForwardRequest {
    Open {
        forward_id: ForwardId,
        connection_id: ForwardConnectionId,
        destination: TcpForwardDestination,
        connect_timeout_ms: u32,
    },
    Exchange {
        connection_id: ForwardConnectionId,
        write_base64: String,
        write_eof: bool,
        max_read_bytes: u32,
        read_wait_ms: u32,
    },
    Close {
        connection_id: ForwardConnectionId,
    },
}

impl TcpForwardRequest {
    pub fn open(
        forward_id: ForwardId,
        connection_id: ForwardConnectionId,
        destination: TcpForwardDestination,
        connect_timeout_ms: u32,
    ) -> Result<Self, TcpForwardProtocolError> {
        let request = Self::Open {
            forward_id,
            connection_id,
            destination,
            connect_timeout_ms,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn exchange(
        connection_id: ForwardConnectionId,
        write: &[u8],
        write_eof: bool,
        max_read_bytes: u32,
        read_wait_ms: u32,
    ) -> Result<Self, TcpForwardProtocolError> {
        if write.len() > HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize {
            return Err(TcpForwardProtocolError::ChunkTooLarge(write.len()));
        }
        let request = Self::Exchange {
            connection_id,
            write_base64: BASE64_STANDARD.encode(write),
            write_eof,
            max_read_bytes,
            read_wait_ms,
        };
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<(), TcpForwardProtocolError> {
        match self {
            Self::Open {
                destination,
                connect_timeout_ms,
                ..
            } => {
                destination.validate()?;
                if *connect_timeout_ms == 0
                    || *connect_timeout_ms > HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS
                {
                    return Err(TcpForwardProtocolError::InvalidConnectTimeout(
                        *connect_timeout_ms,
                    ));
                }
            }
            Self::Exchange {
                write_base64,
                max_read_bytes,
                read_wait_ms,
                ..
            } => {
                let write = BASE64_STANDARD.decode(write_base64)?;
                if write.len() > HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize {
                    return Err(TcpForwardProtocolError::ChunkTooLarge(write.len()));
                }
                if *max_read_bytes == 0 || *max_read_bytes > HARD_MAX_TCP_FORWARD_CHUNK_BYTES {
                    return Err(TcpForwardProtocolError::InvalidReadLimit(*max_read_bytes));
                }
                if *read_wait_ms == 0 || *read_wait_ms > HARD_MAX_TCP_FORWARD_READ_WAIT_MS {
                    return Err(TcpForwardProtocolError::InvalidReadWait(*read_wait_ms));
                }
            }
            Self::Close { .. } => {}
        }
        Ok(())
    }

    pub fn write_bytes(&self) -> Result<Vec<u8>, TcpForwardProtocolError> {
        match self {
            Self::Exchange { write_base64, .. } => Ok(BASE64_STANDARD.decode(write_base64)?),
            _ => Err(TcpForwardProtocolError::UnexpectedRequestShape),
        }
    }

    pub fn into_message(self) -> Result<InnerMessage, TcpForwardProtocolError> {
        self.validate()?;
        Ok(InnerMessage::new(
            "tcp_forward",
            serde_json::to_vec(&self)?,
        )?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, TcpForwardProtocolError> {
        if message.kind != "tcp_forward" {
            return Err(TcpForwardProtocolError::UnexpectedKind(
                message.kind.clone(),
            ));
        }
        let request: Self = serde_json::from_slice(&message.payload)?;
        request.validate()?;
        Ok(request)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TcpForwardErrorCode {
    InvalidRequest,
    Denied,
    NotFound,
    Capacity,
    ConnectFailed,
    Io,
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TcpForwardErrorBody {
    pub code: TcpForwardErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TcpForwardReply {
    Opened {
        connection_id: ForwardConnectionId,
    },
    Exchanged {
        connection_id: ForwardConnectionId,
        read_base64: String,
        read_eof: bool,
    },
    Closed {
        connection_id: ForwardConnectionId,
    },
    Error(TcpForwardErrorBody),
}

impl TcpForwardReply {
    pub fn exchanged(
        connection_id: ForwardConnectionId,
        read: &[u8],
        read_eof: bool,
    ) -> Result<Self, TcpForwardProtocolError> {
        if read.len() > HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize {
            return Err(TcpForwardProtocolError::ChunkTooLarge(read.len()));
        }
        Ok(Self::Exchanged {
            connection_id,
            read_base64: BASE64_STANDARD.encode(read),
            read_eof,
        })
    }

    #[must_use]
    pub fn error(code: TcpForwardErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_TCP_FORWARD_ERROR_MESSAGE_BYTES {
            message.truncate(MAX_TCP_FORWARD_ERROR_MESSAGE_BYTES);
        }
        Self::Error(TcpForwardErrorBody { code, message })
    }

    pub fn read_bytes(&self) -> Result<Vec<u8>, TcpForwardProtocolError> {
        match self {
            Self::Exchanged { read_base64, .. } => Ok(BASE64_STANDARD.decode(read_base64)?),
            _ => Err(TcpForwardProtocolError::UnexpectedReplyShape),
        }
    }

    pub fn into_message(self) -> Result<InnerMessage, TcpForwardProtocolError> {
        validate_reply(&self)?;
        Ok(InnerMessage::new(
            "tcp_forward_result",
            serde_json::to_vec(&self)?,
        )?)
    }

    pub fn from_message(message: &InnerMessage) -> Result<Self, TcpForwardProtocolError> {
        if message.kind != "tcp_forward_result" {
            return Err(TcpForwardProtocolError::UnexpectedKind(
                message.kind.clone(),
            ));
        }
        let reply: Self = serde_json::from_slice(&message.payload)?;
        validate_reply(&reply)?;
        Ok(reply)
    }
}

fn validate_reply(reply: &TcpForwardReply) -> Result<(), TcpForwardProtocolError> {
    match reply {
        TcpForwardReply::Exchanged { read_base64, .. } => {
            let read = BASE64_STANDARD.decode(read_base64)?;
            if read.len() > HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize {
                return Err(TcpForwardProtocolError::ChunkTooLarge(read.len()));
            }
        }
        TcpForwardReply::Error(error) => {
            if error.message.len() > MAX_TCP_FORWARD_ERROR_MESSAGE_BYTES {
                return Err(TcpForwardProtocolError::ErrorMessageTooLarge);
            }
        }
        TcpForwardReply::Opened { .. } | TcpForwardReply::Closed { .. } => {}
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum TcpForwardProtocolError {
    #[error(
        "TCP forward destination must be an ASCII host <= {HARD_MAX_TCP_FORWARD_HOST_BYTES} bytes and a nonzero port"
    )]
    InvalidDestination,
    #[error(
        "TCP forward connect timeout must be 1..={HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS} ms, got {0}"
    )]
    InvalidConnectTimeout(u32),
    #[error("TCP forward max read bytes must be 1..={HARD_MAX_TCP_FORWARD_CHUNK_BYTES}, got {0}")]
    InvalidReadLimit(u32),
    #[error("TCP forward read wait must be 1..={HARD_MAX_TCP_FORWARD_READ_WAIT_MS} ms, got {0}")]
    InvalidReadWait(u32),
    #[error("TCP forward data chunk exceeds hard bound: {0} bytes")]
    ChunkTooLarge(usize),
    #[error("unexpected TCP forward request shape")]
    UnexpectedRequestShape,
    #[error("unexpected TCP forward reply shape")]
    UnexpectedReplyShape,
    #[error("unexpected inner message kind: {0}")]
    UnexpectedKind(String),
    #[error("TCP forward error message exceeds hard bound")]
    ErrorMessageTooLarge,
    #[error("TCP forward JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TCP forward Base64 failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_forward_open_exchange_and_reply_roundtrip_with_bounds() {
        let forward_id = ForwardId::new();
        let connection_id = ForwardConnectionId::new();
        let open = TcpForwardRequest::open(
            forward_id,
            connection_id,
            TcpForwardDestination::new("127.0.0.1", 8080).unwrap(),
            5_000,
        )
        .unwrap();
        assert_eq!(
            TcpForwardRequest::from_message(&open.clone().into_message().unwrap()).unwrap(),
            open
        );
        let exchange =
            TcpForwardRequest::exchange(connection_id, b"hello", true, 4096, 50).unwrap();
        assert_eq!(exchange.write_bytes().unwrap(), b"hello");
        let reply = TcpForwardReply::exchanged(connection_id, b"world", false).unwrap();
        assert_eq!(
            TcpForwardReply::from_message(&reply.clone().into_message().unwrap()).unwrap(),
            reply
        );
        assert_eq!(reply.read_bytes().unwrap(), b"world");
        assert!(TcpForwardDestination::new("bad host", 80).is_err());
        assert!(
            TcpForwardRequest::exchange(
                connection_id,
                &vec![0_u8; HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize + 1],
                false,
                1,
                1,
            )
            .is_err()
        );
    }
}
