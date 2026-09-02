use std::{fs, io::Read, path::Path};

use clew_core::{DeviceId, SiteId};
use clew_identity::ControllerPublicIdentity;
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ConnectorCandidate, ConnectorLeaseError, SignedConnectorLease, SiteDiscoveryTag};

pub const NEARBY_CONNECTOR_FILE_VERSION: u32 = 1;
pub const NEARBY_CONNECTOR_FILE_KIND: &str = "clew_nearby_connector";
pub const MAX_NEARBY_CONNECTOR_FILE_BYTES: usize = 32 * 1024;
pub const MAX_NEARBY_CONNECTOR_ADDRS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NearbyConnectorFile {
    pub version: u32,
    pub kind: String,
    pub site_tag: String,
    pub candidate: EndpointAddr,
    pub lease: SignedConnectorLease,
}

impl NearbyConnectorFile {
    pub fn from_helper(
        helper_addr: EndpointAddr,
        lease: SignedConnectorLease,
    ) -> Result<Self, NearbyConnectorError> {
        let mut candidate = EndpointAddr {
            id: helper_addr.id,
            addrs: Default::default(),
        };
        for addr in helper_addr.addrs {
            if addr.is_ip() {
                candidate.addrs.insert(addr);
            }
        }
        let site_tag = SiteDiscoveryTag::derive(lease.payload.controller_id, lease.payload.site_id);
        let file = Self {
            version: NEARBY_CONNECTOR_FILE_VERSION,
            kind: NEARBY_CONNECTOR_FILE_KIND.into(),
            site_tag: site_tag.to_string(),
            candidate,
            lease,
        };
        file.validate_structure()?;
        Ok(file)
    }

    pub fn verify_routing_hint(
        &self,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<DeviceId, NearbyConnectorError> {
        self.validate_structure()?;
        let expected_tag = SiteDiscoveryTag::derive(controller.controller_id, site_id);
        if SiteDiscoveryTag::parse(&self.site_tag)? != expected_tag {
            return Err(NearbyConnectorError::SiteMismatch);
        }
        Ok(self
            .lease
            .verify_binding_for_candidate(controller, site_id, self.candidate.id)?)
    }

    pub fn verify_for_target(
        &self,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
        now_unix_ms: u64,
    ) -> Result<DeviceId, NearbyConnectorError> {
        self.verify_routing_hint(controller, site_id)?;
        Ok(self
            .lease
            .verify_for_candidate(controller, site_id, self.candidate.id, now_unix_ms)?)
    }

    #[must_use]
    pub fn connector_candidate(&self) -> ConnectorCandidate {
        ConnectorCandidate {
            addr: self.candidate.clone(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, NearbyConnectorError> {
        self.validate_structure()?;
        let encoded = serde_json::to_vec_pretty(self)?;
        check_size(encoded.len())?;
        Ok(encoded)
    }

    pub fn from_bytes(input: &[u8]) -> Result<Self, NearbyConnectorError> {
        check_size(input.len())?;
        #[derive(Deserialize)]
        struct Header {
            version: u32,
            kind: String,
        }
        let header: Header = serde_json::from_slice(input)?;
        if header.version != NEARBY_CONNECTOR_FILE_VERSION {
            return Err(NearbyConnectorError::UnsupportedVersion(header.version));
        }
        if header.kind != NEARBY_CONNECTOR_FILE_KIND {
            return Err(NearbyConnectorError::WrongKind);
        }
        let file: Self = serde_json::from_slice(input)?;
        file.validate_structure()?;
        Ok(file)
    }

    pub fn read(path: &Path) -> Result<Self, NearbyConnectorError> {
        let mut file = fs::File::open(path)?;
        let mut input = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_NEARBY_CONNECTOR_FILE_BYTES + 1) as u64)
            .read_to_end(&mut input)?;
        check_size(input.len())?;
        Self::from_bytes(&input)
    }

    fn validate_structure(&self) -> Result<(), NearbyConnectorError> {
        if self.version != NEARBY_CONNECTOR_FILE_VERSION {
            return Err(NearbyConnectorError::UnsupportedVersion(self.version));
        }
        if self.kind != NEARBY_CONNECTOR_FILE_KIND {
            return Err(NearbyConnectorError::WrongKind);
        }
        let actual_tag = SiteDiscoveryTag::parse(&self.site_tag)?;
        let expected_tag =
            SiteDiscoveryTag::derive(self.lease.payload.controller_id, self.lease.payload.site_id);
        if actual_tag != expected_tag {
            return Err(NearbyConnectorError::SiteMismatch);
        }
        if self.candidate.id.as_bytes() != &self.lease.payload.connector_endpoint_id {
            return Err(NearbyConnectorError::EndpointMismatch);
        }
        if self.candidate.addrs.is_empty() {
            return Err(NearbyConnectorError::MissingLanAddress);
        }
        if self.candidate.addrs.len() > MAX_NEARBY_CONNECTOR_ADDRS {
            return Err(NearbyConnectorError::TooManyAddresses(
                self.candidate.addrs.len(),
            ));
        }
        if self.candidate.addrs.iter().any(|addr| !addr.is_ip()) {
            return Err(NearbyConnectorError::NonLanAddress);
        }
        let lease_bytes = self.lease.to_bytes()?;
        debug_assert!(!lease_bytes.is_empty());
        Ok(())
    }
}

fn check_size(actual: usize) -> Result<(), NearbyConnectorError> {
    if actual == 0 || actual > MAX_NEARBY_CONNECTOR_FILE_BYTES {
        return Err(NearbyConnectorError::TooLarge {
            actual,
            max: MAX_NEARBY_CONNECTOR_FILE_BYTES,
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum NearbyConnectorError {
    #[error("nearby Connector file JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("nearby Connector file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Discovery(#[from] crate::ConnectorDiscoveryError),
    #[error(transparent)]
    Lease(#[from] ConnectorLeaseError),
    #[error("unsupported nearby Connector file version {0}")]
    UnsupportedVersion(u32),
    #[error("file is not a Clew nearby Connector file")]
    WrongKind,
    #[error("nearby Connector file Site does not match")]
    SiteMismatch,
    #[error("nearby Connector file endpoint does not match its signed lease")]
    EndpointMismatch,
    #[error("nearby Connector file contains no direct LAN address")]
    MissingLanAddress,
    #[error("nearby Connector file contains too many address hints: {0}")]
    TooManyAddresses(usize),
    #[error("nearby Connector file contains a non-LAN transport address")]
    NonLanAddress,
    #[error("nearby Connector file is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
}

#[cfg(test)]
mod tests {
    use clew_core::{ControllerId, DeviceId};
    use clew_identity::ControllerIdentity;
    use iroh::{SecretKey, TransportAddr};

    use super::*;

    #[test]
    fn nearby_file_is_bounded_ip_only_and_controller_verified() {
        let controller = ControllerIdentity::from_secret([141_u8; 32]);
        let site_id = SiteId::from_bytes([142_u8; 16]).unwrap();
        let device_id = DeviceId::from_bytes([143_u8; 16]).unwrap();
        let endpoint_id = SecretKey::from_bytes(&[144_u8; 32]).public();
        let mut helper_addr = EndpointAddr::new(endpoint_id);
        helper_addr
            .addrs
            .insert(TransportAddr::Ip("127.0.0.1:4242".parse().unwrap()));
        let lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            device_id,
            endpoint_id,
            1_000,
            61_000,
        )
        .unwrap();
        let file = NearbyConnectorFile::from_helper(helper_addr, lease).unwrap();
        let encoded = file.to_bytes().unwrap();
        assert!(encoded.len() < MAX_NEARBY_CONNECTOR_FILE_BYTES);
        let decoded = NearbyConnectorFile::from_bytes(&encoded).unwrap();
        assert_eq!(
            decoded
                .verify_for_target(&controller.public_identity(), site_id, 2_000)
                .unwrap(),
            device_id
        );
        assert_eq!(decoded, file);
        assert_eq!(
            decoded
                .verify_routing_hint(&controller.public_identity(), site_id)
                .unwrap(),
            device_id
        );
        assert!(matches!(
            decoded.verify_for_target(&controller.public_identity(), site_id, 61_000),
            Err(NearbyConnectorError::Lease(ConnectorLeaseError::Expired))
        ));
        assert_eq!(
            decoded
                .verify_routing_hint(&controller.public_identity(), site_id)
                .unwrap(),
            device_id
        );
    }

    #[test]
    fn wrong_site_endpoint_and_non_ip_hint_fail_closed() {
        let controller = ControllerIdentity::from_secret([151_u8; 32]);
        let site_id = SiteId::from_bytes([152_u8; 16]).unwrap();
        let device_id = DeviceId::from_bytes([153_u8; 16]).unwrap();
        let endpoint_id = SecretKey::from_bytes(&[154_u8; 32]).public();
        let lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            device_id,
            endpoint_id,
            1_000,
            61_000,
        )
        .unwrap();
        let mut missing = NearbyConnectorFile {
            version: NEARBY_CONNECTOR_FILE_VERSION,
            kind: NEARBY_CONNECTOR_FILE_KIND.into(),
            site_tag: SiteDiscoveryTag::derive(controller.controller_id(), site_id).to_string(),
            candidate: EndpointAddr::new(endpoint_id),
            lease,
        };
        assert!(matches!(
            missing.to_bytes(),
            Err(NearbyConnectorError::MissingLanAddress)
        ));
        missing.candidate.addrs.insert(TransportAddr::Relay(
            "https://relay.example".parse().unwrap(),
        ));
        assert!(matches!(
            missing.to_bytes(),
            Err(NearbyConnectorError::NonLanAddress)
        ));
        missing.candidate.id = SecretKey::from_bytes(&[155_u8; 32]).public();
        assert!(matches!(
            missing.to_bytes(),
            Err(NearbyConnectorError::EndpointMismatch)
        ));
    }

    #[test]
    fn oversized_header_is_rejected_before_json_body_parse() {
        let oversized = vec![b' '; MAX_NEARBY_CONNECTOR_FILE_BYTES + 1];
        assert!(matches!(
            NearbyConnectorFile::from_bytes(&oversized),
            Err(NearbyConnectorError::TooLarge { .. })
        ));
        let _ = ControllerId::from_bytes([0x11; 16]).unwrap();
    }
}
