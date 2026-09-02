use clew_core::{ControllerId, DeviceId, SiteId};
use clew_identity::{ControllerIdentity, ControllerPublicIdentity, IdentityError};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONNECTOR_LEASE_VERSION: u32 = 1;
pub const MAX_CONNECTOR_LEASE_LIFETIME_MS: u64 = 10 * 60 * 1000;
pub const MAX_CONNECTOR_LEASE_ENCODED_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorLeaseRole {
    Connector,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConnectorLease {
    pub version: u32,
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub connector_device_id: DeviceId,
    pub connector_endpoint_id: [u8; 32],
    pub role: ConnectorLeaseRole,
    pub issued_unix_ms: u64,
    pub expires_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedConnectorLease {
    pub payload: ConnectorLease,
    pub signature: Vec<u8>,
}

impl SignedConnectorLease {
    pub fn issue(
        controller: &ControllerIdentity,
        site_id: SiteId,
        connector_device_id: DeviceId,
        connector_endpoint_id: EndpointId,
        issued_unix_ms: u64,
        expires_unix_ms: u64,
    ) -> Result<Self, ConnectorLeaseError> {
        let payload = ConnectorLease {
            version: CONNECTOR_LEASE_VERSION,
            controller_id: controller.controller_id(),
            site_id,
            connector_device_id,
            connector_endpoint_id: *connector_endpoint_id.as_bytes(),
            role: ConnectorLeaseRole::Connector,
            issued_unix_ms,
            expires_unix_ms,
        };
        validate_payload(&payload)?;
        let signature = controller.sign_connector_lease(&payload)?;
        let signed = Self { payload, signature };
        signed.check_size()?;
        Ok(signed)
    }

    pub fn verify_for_candidate(
        &self,
        pinned_controller: &ControllerPublicIdentity,
        expected_site_id: SiteId,
        expected_endpoint_id: EndpointId,
        now_unix_ms: u64,
    ) -> Result<DeviceId, ConnectorLeaseError> {
        self.check_size()?;
        validate_payload(&self.payload)?;
        if self.payload.controller_id != pinned_controller.controller_id {
            return Err(ConnectorLeaseError::ControllerMismatch);
        }
        pinned_controller.verify_connector_lease(&self.payload, &self.signature)?;
        if self.payload.site_id != expected_site_id {
            return Err(ConnectorLeaseError::SiteMismatch);
        }
        if self.payload.connector_endpoint_id != *expected_endpoint_id.as_bytes() {
            return Err(ConnectorLeaseError::EndpointMismatch);
        }
        if now_unix_ms < self.payload.issued_unix_ms {
            return Err(ConnectorLeaseError::NotYetValid);
        }
        if now_unix_ms >= self.payload.expires_unix_ms {
            return Err(ConnectorLeaseError::Expired);
        }
        Ok(self.payload.connector_device_id)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, ConnectorLeaseError> {
        self.check_size()?;
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ConnectorLeaseError> {
        if bytes.len() > MAX_CONNECTOR_LEASE_ENCODED_BYTES {
            return Err(ConnectorLeaseError::EncodedTooLarge(bytes.len()));
        }
        let signed: Self = serde_json::from_slice(bytes)?;
        signed.check_size()?;
        validate_payload(&signed.payload)?;
        Ok(signed)
    }

    fn check_size(&self) -> Result<(), ConnectorLeaseError> {
        let len = serde_json::to_vec(self)?.len();
        if len > MAX_CONNECTOR_LEASE_ENCODED_BYTES {
            return Err(ConnectorLeaseError::EncodedTooLarge(len));
        }
        Ok(())
    }
}

fn validate_payload(payload: &ConnectorLease) -> Result<(), ConnectorLeaseError> {
    if payload.version != CONNECTOR_LEASE_VERSION {
        return Err(ConnectorLeaseError::UnsupportedVersion(payload.version));
    }
    if payload.role != ConnectorLeaseRole::Connector {
        return Err(ConnectorLeaseError::WrongRole);
    }
    let lifetime = payload
        .expires_unix_ms
        .checked_sub(payload.issued_unix_ms)
        .ok_or(ConnectorLeaseError::InvalidLifetime)?;
    if lifetime == 0 || lifetime > MAX_CONNECTOR_LEASE_LIFETIME_MS {
        return Err(ConnectorLeaseError::InvalidLifetime);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ConnectorLeaseError {
    #[error("connector lease identity error: {0}")]
    Identity(#[from] IdentityError),
    #[error("connector lease JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported connector lease version {0}")]
    UnsupportedVersion(u32),
    #[error("connector lease is not for Connector capability")]
    WrongRole,
    #[error("connector lease lifetime is invalid or exceeds the hard bound")]
    InvalidLifetime,
    #[error("connector lease Controller pin does not match")]
    ControllerMismatch,
    #[error("connector lease Site does not match")]
    SiteMismatch,
    #[error("connector lease endpoint does not match the discovered candidate")]
    EndpointMismatch,
    #[error("connector lease is not yet valid")]
    NotYetValid,
    #[error("connector lease has expired")]
    Expired,
    #[error("connector lease exceeds the encoded hard bound: {0} bytes")]
    EncodedTooLarge(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn endpoint(seed: u8) -> EndpointId {
        SecretKey::from_bytes(&[seed; 32]).public()
    }

    #[test]
    fn signed_lease_binds_context_and_expiry() {
        let controller = ControllerIdentity::from_secret([61_u8; 32]);
        let site = SiteId::from_bytes([62_u8; 16]).unwrap();
        let device = DeviceId::from_bytes([63_u8; 16]).unwrap();
        let ep = endpoint(64);
        let signed =
            SignedConnectorLease::issue(&controller, site, device, ep, 1_000, 61_000).unwrap();
        assert_eq!(
            SignedConnectorLease::from_bytes(&signed.to_bytes().unwrap()).unwrap(),
            signed
        );
        assert_eq!(
            signed
                .verify_for_candidate(&controller.public_identity(), site, ep, 2_000)
                .unwrap(),
            device
        );
        assert!(matches!(
            signed.verify_for_candidate(
                &controller.public_identity(),
                SiteId::from_bytes([65_u8; 16]).unwrap(),
                ep,
                2_000
            ),
            Err(ConnectorLeaseError::SiteMismatch)
        ));
        assert!(matches!(
            signed.verify_for_candidate(&controller.public_identity(), site, endpoint(66), 2_000),
            Err(ConnectorLeaseError::EndpointMismatch)
        ));
        assert!(matches!(
            signed.verify_for_candidate(&controller.public_identity(), site, ep, 61_000),
            Err(ConnectorLeaseError::Expired)
        ));
        assert!(matches!(
            signed.verify_for_candidate(&controller.public_identity(), site, ep, 999),
            Err(ConnectorLeaseError::NotYetValid)
        ));
    }

    #[test]
    fn tampering_wrong_controller_and_excess_lifetime_fail_closed() {
        let controller = ControllerIdentity::from_secret([71_u8; 32]);
        let other = ControllerIdentity::from_secret([72_u8; 32]);
        let site = SiteId::from_bytes([73_u8; 16]).unwrap();
        let device = DeviceId::from_bytes([74_u8; 16]).unwrap();
        let ep = endpoint(75);
        let signed =
            SignedConnectorLease::issue(&controller, site, device, ep, 5_000, 65_000).unwrap();
        assert!(
            signed
                .verify_for_candidate(&other.public_identity(), site, ep, 6_000)
                .is_err()
        );
        let mut tampered = signed.clone();
        tampered.payload.connector_device_id = DeviceId::from_bytes([76_u8; 16]).unwrap();
        assert!(
            tampered
                .verify_for_candidate(&controller.public_identity(), site, ep, 6_000)
                .is_err()
        );
        assert!(matches!(
            SignedConnectorLease::issue(
                &controller,
                site,
                device,
                ep,
                1,
                MAX_CONNECTOR_LEASE_LIFETIME_MS + 2
            ),
            Err(ConnectorLeaseError::InvalidLifetime)
        ));
    }
}
