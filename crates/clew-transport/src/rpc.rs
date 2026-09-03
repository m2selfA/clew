use std::str;

use clew_core::{RequestId, StableIdError};
use thiserror::Error;

use crate::{InnerMessage, InnerSessionError};

pub const RPC_REQUEST_MESSAGE_KIND: &str = "rpc_request";
pub const RPC_REPLY_MESSAGE_KIND: &str = "rpc_reply";
const RPC_NESTED_HEADER_BYTES: usize = 16 + 1 + 4;
const MAX_NESTED_KIND_BYTES: usize = 64;

pub fn wrap_rpc_request(
    request_id: RequestId,
    message: InnerMessage,
) -> Result<InnerMessage, RpcProtocolError> {
    wrap_rpc(RPC_REQUEST_MESSAGE_KIND, request_id, message)
}

pub fn unwrap_rpc_request(
    message: &InnerMessage,
) -> Result<(RequestId, InnerMessage), RpcProtocolError> {
    unwrap_rpc(RPC_REQUEST_MESSAGE_KIND, message)
}

pub fn wrap_rpc_reply(
    request_id: RequestId,
    message: InnerMessage,
) -> Result<InnerMessage, RpcProtocolError> {
    wrap_rpc(RPC_REPLY_MESSAGE_KIND, request_id, message)
}

pub fn unwrap_rpc_reply(
    expected_request_id: RequestId,
    message: &InnerMessage,
) -> Result<InnerMessage, RpcProtocolError> {
    let (actual_request_id, nested) = unwrap_rpc(RPC_REPLY_MESSAGE_KIND, message)?;
    if actual_request_id != expected_request_id {
        return Err(RpcProtocolError::RequestIdMismatch {
            expected: expected_request_id,
            actual: actual_request_id,
        });
    }
    Ok(nested)
}

fn wrap_rpc(
    outer_kind: &'static str,
    request_id: RequestId,
    message: InnerMessage,
) -> Result<InnerMessage, RpcProtocolError> {
    let kind_len: u8 = message
        .kind
        .len()
        .try_into()
        .map_err(|_| RpcProtocolError::MalformedNestedFrame)?;
    if kind_len == 0 || kind_len as usize > MAX_NESTED_KIND_BYTES {
        return Err(RpcProtocolError::MalformedNestedFrame);
    }
    let payload_len: u32 = message
        .payload
        .len()
        .try_into()
        .map_err(|_| RpcProtocolError::MalformedNestedFrame)?;
    let total = RPC_NESTED_HEADER_BYTES
        .checked_add(kind_len as usize)
        .and_then(|value| value.checked_add(payload_len as usize))
        .ok_or(RpcProtocolError::MalformedNestedFrame)?;
    let mut payload = Vec::with_capacity(total);
    payload.extend_from_slice(request_id.as_bytes());
    payload.push(kind_len);
    payload.extend_from_slice(&payload_len.to_be_bytes());
    payload.extend_from_slice(message.kind.as_bytes());
    payload.extend_from_slice(&message.payload);
    Ok(InnerMessage::new(outer_kind, payload)?)
}

fn unwrap_rpc(
    expected_outer_kind: &'static str,
    message: &InnerMessage,
) -> Result<(RequestId, InnerMessage), RpcProtocolError> {
    if message.kind != expected_outer_kind {
        return Err(RpcProtocolError::UnexpectedOuterKind(message.kind.clone()));
    }
    if message.payload.len() < RPC_NESTED_HEADER_BYTES {
        return Err(RpcProtocolError::MalformedNestedFrame);
    }
    let request_id = RequestId::try_from(&message.payload[..16])?;
    let kind_len = message.payload[16] as usize;
    let payload_len = u32::from_be_bytes(
        message.payload[17..21]
            .try_into()
            .expect("fixed RPC nested header slice"),
    ) as usize;
    if kind_len == 0 || kind_len > MAX_NESTED_KIND_BYTES {
        return Err(RpcProtocolError::MalformedNestedFrame);
    }
    let kind_start = RPC_NESTED_HEADER_BYTES;
    let payload_start = kind_start
        .checked_add(kind_len)
        .ok_or(RpcProtocolError::MalformedNestedFrame)?;
    let end = payload_start
        .checked_add(payload_len)
        .ok_or(RpcProtocolError::MalformedNestedFrame)?;
    if end != message.payload.len() {
        return Err(RpcProtocolError::MalformedNestedFrame);
    }
    let kind = str::from_utf8(&message.payload[kind_start..payload_start])
        .map_err(|_| RpcProtocolError::InvalidNestedKindEncoding)?;
    let nested = InnerMessage::new(kind, message.payload[payload_start..end].to_vec())?;
    Ok((request_id, nested))
}

#[derive(Debug, Error)]
pub enum RpcProtocolError {
    #[error("unexpected RPC envelope kind {0}")]
    UnexpectedOuterKind(String),
    #[error("RPC nested business frame is malformed")]
    MalformedNestedFrame,
    #[error("RPC nested business kind is not valid UTF-8")]
    InvalidNestedKindEncoding,
    #[error("RPC reply request id mismatch: expected {expected}, got {actual}")]
    RequestIdMismatch {
        expected: RequestId,
        actual: RequestId,
    },
    #[error(transparent)]
    StableId(#[from] StableIdError),
    #[error(transparent)]
    Inner(#[from] InnerSessionError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::HARD_MAX_READ_RESULT_BYTES;

    use crate::{ReadReply, ReadRequest};

    #[test]
    fn rpc_envelope_preserves_typed_and_binary_payloads_with_request_correlation() {
        let request_id = RequestId::new();
        let request = ReadRequest::new("/shared/data.bin", 7, 4096)
            .unwrap()
            .into_message()
            .unwrap();
        let wrapped = wrap_rpc_request(request_id, request.clone()).unwrap();
        let (decoded_id, decoded_request) = unwrap_rpc_request(&wrapped).unwrap();
        assert_eq!(decoded_id, request_id);
        assert_eq!(decoded_request, request);

        let reply = ReadReply::data(vec![0xa5; HARD_MAX_READ_RESULT_BYTES as usize])
            .unwrap()
            .into_message()
            .unwrap();
        let wrapped = wrap_rpc_reply(request_id, reply.clone()).unwrap();
        assert_eq!(unwrap_rpc_reply(request_id, &wrapped).unwrap(), reply);
        assert!(matches!(
            unwrap_rpc_reply(RequestId::new(), &wrapped),
            Err(RpcProtocolError::RequestIdMismatch { .. })
        ));
    }

    #[test]
    fn rpc_envelope_rejects_zero_id_truncation_and_wrong_outer_kind() {
        let request_id = RequestId::new();
        let nested = InnerMessage::new("read", b"{}".to_vec()).unwrap();
        let mut wrapped = wrap_rpc_request(request_id, nested).unwrap();
        wrapped.payload[..16].fill(0);
        assert!(matches!(
            unwrap_rpc_request(&wrapped),
            Err(RpcProtocolError::StableId(StableIdError::Nil))
        ));

        let mut wrapped =
            wrap_rpc_request(request_id, InnerMessage::new("read", vec![]).unwrap()).unwrap();
        wrapped.payload.pop();
        assert!(matches!(
            unwrap_rpc_request(&wrapped),
            Err(RpcProtocolError::MalformedNestedFrame)
        ));
        let bare = InnerMessage::new("read", vec![]).unwrap();
        assert!(matches!(
            unwrap_rpc_request(&bare),
            Err(RpcProtocolError::UnexpectedOuterKind(_))
        ));
    }
}
