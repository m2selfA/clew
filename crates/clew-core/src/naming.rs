use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::DeviceId;

const DEVICE_TAG_DOMAIN: &[u8] = b"clew/device-tag/v1\0";
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const DEVICE_TAG_LEN: usize = 5;
const MAX_DEVICE_TAG_ATTEMPTS: u32 = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceTag([u8; DEVICE_TAG_LEN]);

impl DeviceTag {
    #[must_use]
    pub fn derive(device_id: DeviceId, generation: u32) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DEVICE_TAG_DOMAIN);
        hasher.update(device_id.as_bytes());
        hasher.update(generation.to_be_bytes());
        let digest = hasher.finalize();

        let mut value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) >> 7;
        let mut encoded = [b'0'; DEVICE_TAG_LEN];
        for index in (0..DEVICE_TAG_LEN).rev() {
            encoded[index] = CROCKFORD_BASE32[(value & 0x1f) as usize];
            value >>= 5;
        }
        Self(encoded)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("generated DeviceTag is always ASCII")
    }
}

impl fmt::Display for DeviceTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for DeviceTag {
    type Err = DeviceTagParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != DEVICE_TAG_LEN {
            return Err(DeviceTagParseError::InvalidLength(value.len()));
        }
        let mut encoded = [0_u8; DEVICE_TAG_LEN];
        for (index, byte) in value.bytes().enumerate() {
            if !CROCKFORD_BASE32.contains(&byte) {
                return Err(DeviceTagParseError::InvalidCharacter(byte as char));
            }
            encoded[index] = byte;
        }
        Ok(Self(encoded))
    }
}

impl Serialize for DeviceTag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DeviceTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceTagAllocation {
    pub tag: DeviceTag,
    pub generation: u32,
}

pub fn allocate_device_tag<F>(
    device_id: DeviceId,
    start_generation: u32,
    mut is_occupied: F,
) -> Result<DeviceTagAllocation, DeviceTagAllocationError>
where
    F: FnMut(&DeviceTag) -> bool,
{
    for offset in 0..MAX_DEVICE_TAG_ATTEMPTS {
        let generation = start_generation
            .checked_add(offset)
            .ok_or(DeviceTagAllocationError::GenerationOverflow)?;
        let tag = DeviceTag::derive(device_id, generation);
        if !is_occupied(&tag) {
            return Ok(DeviceTagAllocation { tag, generation });
        }
    }
    Err(DeviceTagAllocationError::Exhausted {
        start_generation,
        attempts: MAX_DEVICE_TAG_ATTEMPTS,
    })
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DeviceTagParseError {
    #[error("DeviceTag must be exactly 5 ASCII characters, got {0}")]
    InvalidLength(usize),
    #[error("DeviceTag contains a non-Crockford character: {0:?}")]
    InvalidCharacter(char),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DeviceTagAllocationError {
    #[error("DeviceTag generation counter overflowed")]
    GenerationOverflow,
    #[error("no free DeviceTag found after {attempts} attempts from generation {start_generation}")]
    Exhausted {
        start_generation: u32,
        attempts: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_device_id() -> DeviceId {
        "4b2bc107-8bd8-4c36-a5aa-7590dfde4f21".parse().unwrap()
    }

    #[test]
    fn device_tag_is_stable_fixed_length_and_human_safe() {
        let first = DeviceTag::derive(fixed_device_id(), 0);
        let second = DeviceTag::derive(fixed_device_id(), 0);
        assert_eq!(first, second);
        assert_eq!(first.as_str().len(), 5);
        assert!(
            first
                .as_str()
                .bytes()
                .all(|byte| CROCKFORD_BASE32.contains(&byte))
        );
        assert!(!first.as_str().contains(['I', 'L', 'O', 'U']));
    }

    #[test]
    fn collision_reallocation_is_deterministic_and_keeps_five_chars() {
        let device_id = fixed_device_id();
        let occupied = DeviceTag::derive(device_id, 0);
        let first = allocate_device_tag(device_id, 0, |candidate| *candidate == occupied).unwrap();
        let second = allocate_device_tag(device_id, 0, |candidate| *candidate == occupied).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.generation, 1);
        assert_eq!(first.tag.as_str().len(), 5);
        assert_ne!(first.tag, occupied);
    }

    #[test]
    fn tag_parser_rejects_noncanonical_or_ambiguous_text() {
        assert!(matches!(
            "ABCD".parse::<DeviceTag>(),
            Err(DeviceTagParseError::InvalidLength(4))
        ));
        assert!(matches!(
            "ABCDO".parse::<DeviceTag>(),
            Err(DeviceTagParseError::InvalidCharacter('O'))
        ));
        assert!("ABCDE".parse::<DeviceTag>().is_ok());
    }
}
