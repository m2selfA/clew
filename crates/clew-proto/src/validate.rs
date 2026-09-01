use clew_core::DeviceId;
use prost::Message;
use thiserror::Error;

use crate::{HARD_MAX_CONCURRENT_REQUESTS, HARD_MAX_FRAME_SIZE, WIRE_MAJOR, v1};

const ID_BYTES: usize = 16;
const MAX_SOFTWARE_VERSION_BYTES: usize = 128;
const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

pub trait ValidateWire {
    fn validate_wire(&self) -> Result<(), WireValidationError>;
}

pub fn decode_wire_message<M>(input: &[u8]) -> Result<M, WireValidationError>
where
    M: Message + Default,
{
    check_frame_size(input.len())?;
    M::decode(input).map_err(WireValidationError::Decode)
}

pub fn encode_wire_message<M>(message: &M) -> Result<Vec<u8>, WireValidationError>
where
    M: Message,
{
    check_frame_size(message.encoded_len())?;
    Ok(message.encode_to_vec())
}

impl ValidateWire for v1::Hello {
    fn validate_wire(&self) -> Result<(), WireValidationError> {
        if self.wire_major != WIRE_MAJOR {
            return Err(WireValidationError::UnsupportedWireMajor {
                found: self.wire_major,
                supported: WIRE_MAJOR,
            });
        }
        if self.capability_version == 0 {
            return Err(WireValidationError::OutOfRange {
                field: "capability_version",
                value: 0,
                min: 1,
                max: u64::MAX,
            });
        }
        if self.software_version.is_empty() {
            return Err(WireValidationError::MissingField("software_version"));
        }
        if self.software_version.len() > MAX_SOFTWARE_VERSION_BYTES {
            return Err(WireValidationError::StringTooLong {
                field: "software_version",
                actual: self.software_version.len(),
                max: MAX_SOFTWARE_VERSION_BYTES,
            });
        }
        if self.role == 0 || v1::PeerRole::try_from(self.role).is_err() {
            return Err(WireValidationError::InvalidEnum {
                field: "role",
                value: self.role,
            });
        }
        if let Some(device_id) = &self.device_id {
            validate_id_bytes("device_id", device_id)?;
        }
        if self.features.contains(&0) {
            return Err(WireValidationError::InvalidEnum {
                field: "features",
                value: 0,
            });
        }
        if self.max_frame_size == 0 || self.max_frame_size > HARD_MAX_FRAME_SIZE {
            return Err(WireValidationError::OutOfRange {
                field: "max_frame_size",
                value: u64::from(self.max_frame_size),
                min: 1,
                max: u64::from(HARD_MAX_FRAME_SIZE),
            });
        }
        if self.max_concurrent_requests == 0
            || self.max_concurrent_requests > HARD_MAX_CONCURRENT_REQUESTS
        {
            return Err(WireValidationError::OutOfRange {
                field: "max_concurrent_requests",
                value: u64::from(self.max_concurrent_requests),
                min: 1,
                max: u64::from(HARD_MAX_CONCURRENT_REQUESTS),
            });
        }
        Ok(())
    }
}

impl ValidateWire for v1::RequestEnvelope {
    fn validate_wire(&self) -> Result<(), WireValidationError> {
        validate_id_bytes("request_id", &self.request_id)?;
        if let Some(trace_id) = &self.trace_id {
            validate_id_bytes("trace_id", trace_id)?;
        }
        if self.body.is_none() {
            return Err(WireValidationError::MissingField("body"));
        }
        Ok(())
    }
}

impl ValidateWire for v1::ResponseEnvelope {
    fn validate_wire(&self) -> Result<(), WireValidationError> {
        validate_id_bytes("request_id", &self.request_id)?;
        if self.result.is_none() {
            return Err(WireValidationError::MissingField("result"));
        }
        if let Some(v1::response_envelope::Result::Error(error)) = &self.result {
            error.validate_wire()?;
        }
        Ok(())
    }
}

impl ValidateWire for v1::Error {
    fn validate_wire(&self) -> Result<(), WireValidationError> {
        if self.code == 0 {
            return Err(WireValidationError::InvalidEnum {
                field: "error.code",
                value: 0,
            });
        }
        if self.message.len() > MAX_ERROR_MESSAGE_BYTES {
            return Err(WireValidationError::StringTooLong {
                field: "error.message",
                actual: self.message.len(),
                max: MAX_ERROR_MESSAGE_BYTES,
            });
        }
        Ok(())
    }
}

pub fn hello_device_id(hello: &v1::Hello) -> Result<Option<DeviceId>, WireValidationError> {
    let Some(raw) = &hello.device_id else {
        return Ok(None);
    };
    validate_id_bytes("device_id", raw)?;
    DeviceId::try_from(raw.as_slice())
        .map(Some)
        .map_err(|_| WireValidationError::ZeroId("device_id"))
}

fn check_frame_size(actual: usize) -> Result<(), WireValidationError> {
    let max = HARD_MAX_FRAME_SIZE as usize;
    if actual > max {
        return Err(WireValidationError::FrameTooLarge { actual, max });
    }
    Ok(())
}

fn validate_id_bytes(field: &'static str, bytes: &[u8]) -> Result<(), WireValidationError> {
    if bytes.len() != ID_BYTES {
        return Err(WireValidationError::InvalidLength {
            field,
            actual: bytes.len(),
            expected: ID_BYTES,
        });
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(WireValidationError::ZeroId(field));
    }
    Ok(())
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WireValidationError {
    #[error("wire frame is {actual} bytes; maximum is {max}")]
    FrameTooLarge { actual: usize, max: usize },
    #[error("protobuf decode failed: {0}")]
    Decode(#[source] prost::DecodeError),
    #[error("unsupported wire major {found}; this build speaks {supported}")]
    UnsupportedWireMajor { found: u32, supported: u32 },
    #[error("missing required wire field {0}")]
    MissingField(&'static str),
    #[error("wire field {field} has length {actual}; expected {expected}")]
    InvalidLength {
        field: &'static str,
        actual: usize,
        expected: usize,
    },
    #[error("wire field {0} cannot be an all-zero id")]
    ZeroId(&'static str),
    #[error("wire field {field} has unknown or unspecified enum value {value}")]
    InvalidEnum { field: &'static str, value: i32 },
    #[error("wire field {field}={value} is outside [{min}, {max}]")]
    OutOfRange {
        field: &'static str,
        value: u64,
        min: u64,
        max: u64,
    },
    #[error("wire field {field} is {actual} bytes; maximum is {max}")]
    StringTooLong {
        field: &'static str,
        actual: usize,
        max: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CAPABILITY_VERSION;

    fn sample_hello() -> v1::Hello {
        v1::Hello {
            wire_major: WIRE_MAJOR,
            capability_version: CAPABILITY_VERSION,
            software_version: "0.1.0".into(),
            role: 1,
            device_id: Some(
                "4b2bc107-8bd8-4c36-a5aa-7590dfde4f21"
                    .parse::<DeviceId>()
                    .unwrap()
                    .into_bytes()
                    .to_vec(),
            ),
            features: vec![1, 2, 999],
            max_frame_size: 1024 * 1024,
            max_concurrent_requests: 64,
        }
    }

    #[test]
    fn hello_roundtrip_preserves_unknown_features_for_forward_compatibility() {
        let hello = sample_hello();
        hello.validate_wire().unwrap();
        let encoded = encode_wire_message(&hello).unwrap();
        let decoded = decode_wire_message::<v1::Hello>(encoded.as_slice()).unwrap();
        decoded.validate_wire().unwrap();
        assert_eq!(decoded, hello);
        assert!(decoded.features.contains(&999));
        assert_eq!(
            hello_device_id(&decoded).unwrap(),
            hello_device_id(&hello).unwrap()
        );
    }

    #[test]
    fn wrong_wire_major_and_bad_device_id_fail_closed() {
        let mut hello = sample_hello();
        hello.wire_major += 1;
        assert!(matches!(
            hello.validate_wire(),
            Err(WireValidationError::UnsupportedWireMajor { .. })
        ));

        let mut hello = sample_hello();
        hello.device_id = Some(vec![7; 15]);
        assert!(matches!(
            hello.validate_wire(),
            Err(WireValidationError::InvalidLength {
                field: "device_id",
                actual: 15,
                expected: 16
            })
        ));
    }

    #[test]
    fn envelopes_require_nonzero_ids_and_a_body_or_result() {
        let request = v1::RequestEnvelope {
            request_id: vec![1; 16],
            trace_id: None,
            deadline_ms: Some(1_000),
            body: None,
        };
        assert_eq!(
            request.validate_wire(),
            Err(WireValidationError::MissingField("body"))
        );

        let response = v1::ResponseEnvelope {
            request_id: vec![0; 16],
            result: Some(v1::response_envelope::Result::Success(v1::Success {})),
        };
        assert_eq!(
            response.validate_wire(),
            Err(WireValidationError::ZeroId("request_id"))
        );
    }

    #[test]
    fn truncated_protobuf_is_rejected() {
        assert!(matches!(
            decode_wire_message::<v1::Hello>(&[0x08, 0x80]),
            Err(WireValidationError::Decode(_))
        ));
    }

    #[test]
    fn oversized_frame_is_rejected_before_protobuf_decoding() {
        let oversized = vec![0_u8; HARD_MAX_FRAME_SIZE as usize + 1];
        assert!(matches!(
            decode_wire_message::<v1::Hello>(&oversized),
            Err(WireValidationError::FrameTooLarge { actual, max })
                if actual == HARD_MAX_FRAME_SIZE as usize + 1
                    && max == HARD_MAX_FRAME_SIZE as usize
        ));
    }

    #[test]
    fn peer_advertised_bounds_are_enforced() {
        let mut hello = sample_hello();
        hello.max_frame_size = HARD_MAX_FRAME_SIZE + 1;
        assert!(matches!(
            hello.validate_wire(),
            Err(WireValidationError::OutOfRange {
                field: "max_frame_size",
                ..
            })
        ));

        let mut hello = sample_hello();
        hello.max_concurrent_requests = 0;
        assert!(matches!(
            hello.validate_wire(),
            Err(WireValidationError::OutOfRange {
                field: "max_concurrent_requests",
                ..
            })
        ));
    }
}
