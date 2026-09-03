#![forbid(unsafe_code)]

mod validate;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/clew.v1.rs"));
}

pub use validate::{
    IMPLEMENTED_FEATURES, NegotiatedLimits, ValidateWire, WireValidationError, decode_wire_message,
    encode_wire_message, feature_negotiated, hello_advertises_feature,
    hello_advertises_file_resume, hello_device_id, locally_implements_feature, negotiate_limits,
    negotiated_implemented_features,
};

pub const WIRE_MAJOR: u32 = 1;
pub const CAPABILITY_VERSION: u64 = 1;
pub const ALPN: &[u8] = b"clew/1";
pub const BOOTSTRAP_ALPN: &[u8] = b"clew/bootstrap/1";
pub const CONNECTOR_ALPN: &[u8] = b"clew/connector/1";
pub const HARD_MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;
pub const HARD_MAX_CONCURRENT_REQUESTS: u32 = 4096;
