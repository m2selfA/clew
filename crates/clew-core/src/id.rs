use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StableIdError {
    #[error("stable IDs must be exactly 16 bytes, got {0}")]
    InvalidLength(usize),
    #[error("stable IDs cannot be nil")]
    Nil,
    #[error("invalid UUID: {0}")]
    Parse(#[from] uuid::Error),
}

macro_rules! define_stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, StableIdError> {
                Self::from_uuid(Uuid::from_bytes(bytes))
            }

            fn from_uuid(uuid: Uuid) -> Result<Self, StableIdError> {
                if uuid.is_nil() {
                    return Err(StableIdError::Nil);
                }
                Ok(Self(uuid))
            }

            #[must_use]
            pub fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }

            #[must_use]
            pub fn into_bytes(self) -> [u8; 16] {
                *self.0.as_bytes()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = StableIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_uuid(Uuid::parse_str(value)?)
            }
        }

        impl TryFrom<&[u8]> for $name {
            type Error = StableIdError;

            fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
                if value.len() != 16 {
                    return Err(StableIdError::InvalidLength(value.len()));
                }
                let mut bytes = [0_u8; 16];
                bytes.copy_from_slice(value);
                Self::from_bytes(bytes)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                raw.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_stable_id!(ControllerId);
define_stable_id!(SiteId);
define_stable_id!(DeviceId);
define_stable_id!(InviteId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ids_roundtrip_as_canonical_strings() {
        let device: DeviceId = "019c9df1-d5be-7a9e-b9d3-b612e3f6dfd0".parse().unwrap();
        let encoded = serde_json::to_string(&device).unwrap();
        assert_eq!(encoded, "\"019c9df1-d5be-7a9e-b9d3-b612e3f6dfd0\"");
        assert_eq!(serde_json::from_str::<DeviceId>(&encoded).unwrap(), device);
    }

    #[test]
    fn nil_and_wrong_length_ids_fail_closed() {
        assert!(matches!(
            DeviceId::from_bytes([0_u8; 16]),
            Err(StableIdError::Nil)
        ));
        assert!(matches!(
            DeviceId::try_from(&[1_u8; 15][..]),
            Err(StableIdError::InvalidLength(15))
        ));
        assert!(
            serde_json::from_str::<DeviceId>("\"00000000-0000-0000-0000-000000000000\"").is_err()
        );
    }

    #[test]
    fn strong_types_do_not_cross_assign() {
        let controller = ControllerId::new();
        let site = SiteId::from_bytes(controller.into_bytes()).unwrap();
        assert_eq!(controller.to_string(), site.to_string());
    }
}
