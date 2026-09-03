use std::collections::BTreeMap;

use clew_core::{ControllerId, DeviceId, InviteId, SiteId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    ControllerIdentity, ControllerPublicIdentity, DevicePublicIdentity, IdentityError,
    PermissionGrant, keys::random_secret_32,
};

const BOOTSTRAP_SIGNATURE_DOMAIN: &[u8] = b"clew/site-bootstrap-signature/v1\0";
const BOOTSTRAP_SECRET_DOMAIN: &[u8] = b"clew/site-bootstrap-secret/v1\0";
const BOOTSTRAP_FINGERPRINT_DOMAIN: &[u8] = b"clew/site-bootstrap-fingerprint/v1\0";
const BOOTSTRAP_VERSION: u32 = 1;
const MAX_SITE_NAME_BYTES: usize = 128;
const MAX_BOOTSTRAP_CLAIMS: u32 = 256;
const MAX_DEPLOYMENT_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteBootstrapSpec {
    pub site_id: SiteId,
    pub invite_id: InviteId,
    pub site_name: String,
    pub grant: PermissionGrant,
    pub not_before_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub deployment_window_ms: u64,
    pub max_claims: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SiteBootstrapPayload {
    pub version: u32,
    pub controller: ControllerPublicIdentity,
    pub site_id: SiteId,
    pub invite_id: InviteId,
    pub site_name: String,
    pub grant: PermissionGrant,
    pub not_before_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub deployment_window_ms: u64,
    pub max_claims: u32,
    pub bootstrap_secret_hash: [u8; 32],
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedSiteBootstrapPass {
    pub payload: SiteBootstrapPayload,
    bootstrap_secret: [u8; 32],
    pub signature: Vec<u8>,
}

impl std::fmt::Debug for SignedSiteBootstrapPass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedSiteBootstrapPass")
            .field("payload", &self.payload)
            .field("bootstrap_secret", &"[REDACTED]")
            .field("signature_len", &self.signature.len())
            .finish()
    }
}

impl SiteBootstrapSpec {
    fn validate(&self) -> Result<(), EnrollmentError> {
        let name = self.site_name.trim();
        if name.is_empty() || name.len() > MAX_SITE_NAME_BYTES {
            return Err(EnrollmentError::InvalidBootstrap(
                "site name must be 1..=128 UTF-8 bytes",
            ));
        }
        if self.expires_unix_ms <= self.not_before_unix_ms {
            return Err(EnrollmentError::InvalidBootstrap(
                "expiry must be after not-before",
            ));
        }
        if self.deployment_window_ms == 0 || self.deployment_window_ms > MAX_DEPLOYMENT_WINDOW_MS {
            return Err(EnrollmentError::InvalidBootstrap(
                "deployment window must be within 1 ms..=7 days",
            ));
        }
        if self.max_claims == 0 || self.max_claims > MAX_BOOTSTRAP_CLAIMS {
            return Err(EnrollmentError::InvalidBootstrap(
                "max claims must be within 1..=256",
            ));
        }
        Ok(())
    }
}

impl ControllerIdentity {
    pub fn issue_site_bootstrap(
        &self,
        spec: SiteBootstrapSpec,
    ) -> Result<SignedSiteBootstrapPass, EnrollmentError> {
        spec.validate()?;
        let bootstrap_secret = random_secret_32()?;
        let payload = SiteBootstrapPayload {
            version: BOOTSTRAP_VERSION,
            controller: self.public_identity(),
            site_id: spec.site_id,
            invite_id: spec.invite_id,
            site_name: spec.site_name,
            grant: spec.grant,
            not_before_unix_ms: spec.not_before_unix_ms,
            expires_unix_ms: spec.expires_unix_ms,
            deployment_window_ms: spec.deployment_window_ms,
            max_claims: spec.max_claims,
            bootstrap_secret_hash: bootstrap_secret_hash(&bootstrap_secret),
        };
        let signature = self.sign_payload(BOOTSTRAP_SIGNATURE_DOMAIN, &payload)?;
        Ok(SignedSiteBootstrapPass {
            payload,
            bootstrap_secret,
            signature,
        })
    }
}

impl SignedSiteBootstrapPass {
    pub fn verify(&self) -> Result<ControllerPublicIdentity, EnrollmentError> {
        validate_payload(&self.payload)?;
        if bootstrap_secret_hash(&self.bootstrap_secret) != self.payload.bootstrap_secret_hash {
            return Err(EnrollmentError::BootstrapSecretMismatch);
        }
        self.payload.controller.verify_payload(
            BOOTSTRAP_SIGNATURE_DOMAIN,
            &self.payload,
            &self.signature,
        )?;
        Ok(self.payload.controller)
    }

    fn fingerprint(&self) -> Result<[u8; 32], EnrollmentError> {
        let encoded = serde_json::to_vec(&(&self.payload, &self.signature))?;
        let mut hasher = Sha256::new();
        hasher.update(BOOTSTRAP_FINGERPRINT_DOMAIN);
        hasher.update(encoded);
        Ok(hasher.finalize().into())
    }
}

fn validate_payload(payload: &SiteBootstrapPayload) -> Result<(), EnrollmentError> {
    if payload.version != BOOTSTRAP_VERSION {
        return Err(EnrollmentError::UnsupportedBootstrapVersion(
            payload.version,
        ));
    }
    let spec = SiteBootstrapSpec {
        site_id: payload.site_id,
        invite_id: payload.invite_id,
        site_name: payload.site_name.clone(),
        grant: payload.grant,
        not_before_unix_ms: payload.not_before_unix_ms,
        expires_unix_ms: payload.expires_unix_ms,
        deployment_window_ms: payload.deployment_window_ms,
        max_claims: payload.max_claims,
    };
    spec.validate()?;
    payload.controller.validate()?;
    Ok(())
}

fn bootstrap_secret_hash(secret: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(BOOTSTRAP_SECRET_DOMAIN);
    hasher.update(secret);
    hasher.finalize().into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentStatus {
    PendingHostPersist,
    Active,
    Revoked,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnrollmentReceipt {
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub invite_id: InviteId,
    pub device_id: DeviceId,
    pub device_public_identity: DevicePublicIdentity,
    pub effective_grant: PermissionGrant,
    pub issued_unix_ms: u64,
    persist_ack_token: [u8; 32],
}

impl std::fmt::Debug for EnrollmentReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnrollmentReceipt")
            .field("controller_id", &self.controller_id)
            .field("site_id", &self.site_id)
            .field("invite_id", &self.invite_id)
            .field("device_id", &self.device_id)
            .field("device_public_identity", &self.device_public_identity)
            .field("effective_grant", &self.effective_grant)
            .field("issued_unix_ms", &self.issued_unix_ms)
            .field("persist_ack_token", &"[REDACTED]")
            .finish()
    }
}

impl EnrollmentReceipt {
    #[must_use]
    pub fn persist_ack_token(&self) -> &[u8; 32] {
        &self.persist_ack_token
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EnrollmentDeviceRecord {
    pub controller_id: ControllerId,
    pub site_id: SiteId,
    pub invite_id: InviteId,
    pub device_id: DeviceId,
    pub device_public_identity: DevicePublicIdentity,
    pub effective_grant: PermissionGrant,
    pub status: EnrollmentStatus,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EnrollmentRegistry {
    controller_id: ControllerId,
    policy_ceiling: PermissionGrant,
    invites: BTreeMap<InviteId, InviteRuntimeState>,
    devices: BTreeMap<DeviceId, EnrollmentDeviceRecord>,
}

impl std::fmt::Debug for EnrollmentRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EnrollmentRegistry")
            .field("controller_id", &self.controller_id)
            .field("policy_ceiling", &self.policy_ceiling)
            .field("invite_count", &self.invites.len())
            .field("device_count", &self.devices.len())
            .finish()
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct InviteRuntimeState {
    issued_pass_fingerprint: Option<[u8; 32]>,
    first_claim_unix_ms: Option<u64>,
    closed: bool,
    revoked: bool,
    claims: Vec<ClaimRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ClaimRecord {
    pass_fingerprint: [u8; 32],
    receipt: EnrollmentReceipt,
    status: EnrollmentStatus,
}

impl EnrollmentRegistry {
    #[must_use]
    pub fn new(controller_id: ControllerId, policy_ceiling: PermissionGrant) -> Self {
        Self {
            controller_id,
            policy_ceiling,
            invites: BTreeMap::new(),
            devices: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn controller_id(&self) -> ControllerId {
        self.controller_id
    }

    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    pub fn issue_bootstrap(
        &mut self,
        controller: &ControllerIdentity,
        spec: SiteBootstrapSpec,
    ) -> Result<SignedSiteBootstrapPass, EnrollmentError> {
        if controller.controller_id() != self.controller_id {
            return Err(EnrollmentError::WrongController);
        }
        let pass = controller.issue_site_bootstrap(spec)?;
        self.register_pass(&pass)?;
        Ok(pass)
    }

    pub fn register_pass(&mut self, pass: &SignedSiteBootstrapPass) -> Result<(), EnrollmentError> {
        let public = pass.verify()?;
        if public.controller_id != self.controller_id {
            return Err(EnrollmentError::WrongController);
        }
        let fingerprint = pass.fingerprint()?;
        let state = self.invites.entry(pass.payload.invite_id).or_default();
        match state.issued_pass_fingerprint {
            Some(existing) if existing != fingerprint => Err(EnrollmentError::PassConflict),
            Some(_) => Ok(()),
            None => {
                state.issued_pass_fingerprint = Some(fingerprint);
                Ok(())
            }
        }
    }

    pub fn claim(
        &mut self,
        pass: &SignedSiteBootstrapPass,
        device: DevicePublicIdentity,
        now_unix_ms: u64,
    ) -> Result<EnrollmentReceipt, EnrollmentError> {
        self.claim_with_ceiling(pass, device, now_unix_ms, self.policy_ceiling)
    }

    pub fn claim_with_ceiling(
        &mut self,
        pass: &SignedSiteBootstrapPass,
        device: DevicePublicIdentity,
        now_unix_ms: u64,
        claim_ceiling: PermissionGrant,
    ) -> Result<EnrollmentReceipt, EnrollmentError> {
        let public = pass.verify()?;
        device.validate()?;
        if public.controller_id != self.controller_id {
            return Err(EnrollmentError::WrongController);
        }
        let invite_id = pass.payload.invite_id;
        let fingerprint = pass.fingerprint()?;

        if let Some(state) = self.invites.get(&invite_id) {
            if state.revoked {
                return Err(EnrollmentError::InviteRevoked);
            }
            if let Some(issued) = state.issued_pass_fingerprint
                && issued != fingerprint
            {
                return Err(EnrollmentError::PassConflict);
            }
            if let Some(existing) = state
                .claims
                .iter()
                .find(|claim| claim.receipt.device_public_identity == device)
            {
                if existing.pass_fingerprint != fingerprint {
                    return Err(EnrollmentError::PassConflict);
                }
                return match existing.status {
                    EnrollmentStatus::PendingHostPersist => Ok(existing.receipt.clone()),
                    EnrollmentStatus::Active => Err(EnrollmentError::FinalizedReplay),
                    EnrollmentStatus::Revoked => Err(EnrollmentError::InviteRevoked),
                };
            }
            if state.closed {
                return Err(EnrollmentError::InviteClosed);
            }
        }

        if now_unix_ms < pass.payload.not_before_unix_ms {
            return Err(EnrollmentError::NotYetValid);
        }
        if now_unix_ms >= pass.payload.expires_unix_ms {
            return Err(EnrollmentError::Expired);
        }
        if let Some(state) = self.invites.get(&invite_id) {
            if let Some(first_claim) = state.first_claim_unix_ms
                && now_unix_ms >= first_claim.saturating_add(pass.payload.deployment_window_ms)
            {
                return Err(EnrollmentError::DeploymentWindowClosed);
            }
            if state.claims.len() >= pass.payload.max_claims as usize {
                return Err(EnrollmentError::Exhausted);
            }
        }

        let mut device_id = DeviceId::new();
        while self.devices.contains_key(&device_id) {
            device_id = DeviceId::new();
        }
        let effective_grant = pass
            .payload
            .grant
            .intersect(self.policy_ceiling)
            .intersect(claim_ceiling);
        let receipt = EnrollmentReceipt {
            controller_id: self.controller_id,
            site_id: pass.payload.site_id,
            invite_id,
            device_id,
            device_public_identity: device,
            effective_grant,
            issued_unix_ms: now_unix_ms,
            persist_ack_token: random_secret_32()?,
        };
        let state = self.invites.entry(invite_id).or_default();
        if let Some(issued) = state.issued_pass_fingerprint {
            if issued != fingerprint {
                return Err(EnrollmentError::PassConflict);
            }
        } else {
            state.issued_pass_fingerprint = Some(fingerprint);
        }
        state.first_claim_unix_ms.get_or_insert(now_unix_ms);
        state.claims.push(ClaimRecord {
            pass_fingerprint: fingerprint,
            receipt: receipt.clone(),
            status: EnrollmentStatus::PendingHostPersist,
        });
        self.devices.insert(
            device_id,
            EnrollmentDeviceRecord {
                controller_id: self.controller_id,
                site_id: pass.payload.site_id,
                invite_id,
                device_id,
                device_public_identity: device,
                effective_grant,
                status: EnrollmentStatus::PendingHostPersist,
            },
        );
        Ok(receipt)
    }

    pub fn finalize_host_persist(
        &mut self,
        invite_id: InviteId,
        device_id: DeviceId,
        persist_ack_token: &[u8; 32],
    ) -> Result<EnrollmentDeviceRecord, EnrollmentError> {
        let receipt = {
            let state = self
                .invites
                .get_mut(&invite_id)
                .ok_or(EnrollmentError::MissingClaim)?;
            if state.revoked {
                return Err(EnrollmentError::InviteRevoked);
            }
            let claim = state
                .claims
                .iter_mut()
                .find(|claim| claim.receipt.device_id == device_id)
                .ok_or(EnrollmentError::MissingClaim)?;
            if !constant_time_eq(&claim.receipt.persist_ack_token, persist_ack_token) {
                return Err(EnrollmentError::PersistTokenMismatch);
            }
            match claim.status {
                EnrollmentStatus::PendingHostPersist => {
                    claim.status = EnrollmentStatus::Active;
                }
                EnrollmentStatus::Active => {}
                EnrollmentStatus::Revoked => return Err(EnrollmentError::InviteRevoked),
            }
            claim.receipt.clone()
        };

        let device = self
            .devices
            .get_mut(&receipt.device_id)
            .ok_or(EnrollmentError::MissingClaim)?;
        device.status = EnrollmentStatus::Active;
        Ok(device.clone())
    }

    pub fn close_invite(&mut self, invite_id: InviteId) {
        self.invites.entry(invite_id).or_default().closed = true;
    }

    pub fn revoke_invite(&mut self, invite_id: InviteId) {
        let device_ids = {
            let state = self.invites.entry(invite_id).or_default();
            state.closed = true;
            state.revoked = true;
            let mut device_ids = Vec::with_capacity(state.claims.len());
            for claim in &mut state.claims {
                claim.status = EnrollmentStatus::Revoked;
                device_ids.push(claim.receipt.device_id);
            }
            device_ids
        };
        for device_id in device_ids {
            if let Some(device) = self.devices.get_mut(&device_id) {
                device.status = EnrollmentStatus::Revoked;
            }
        }
    }

    pub fn revoke_device(&mut self, device_id: DeviceId) -> Result<(), EnrollmentError> {
        let device = self
            .devices
            .get_mut(&device_id)
            .ok_or(EnrollmentError::UnknownDevice(device_id))?;
        device.status = EnrollmentStatus::Revoked;
        if let Some(invite) = self.invites.get_mut(&device.invite_id)
            && let Some(claim) = invite
                .claims
                .iter_mut()
                .find(|claim| claim.receipt.device_id == device_id)
        {
            claim.status = EnrollmentStatus::Revoked;
        }
        Ok(())
    }

    pub fn revoke_site(&mut self, site_id: SiteId) -> Vec<DeviceId> {
        let mut revoked = Vec::new();
        let device_ids: Vec<_> = self
            .devices
            .values()
            .filter(|device| device.site_id == site_id)
            .map(|device| device.device_id)
            .collect();
        for device_id in device_ids {
            if self.revoke_device(device_id).is_ok() {
                revoked.push(device_id);
            }
        }
        revoked
    }

    pub fn close_all_invites(&mut self) {
        for state in self.invites.values_mut() {
            state.closed = true;
        }
    }

    #[must_use]
    pub fn policy_ceiling(&self) -> PermissionGrant {
        self.policy_ceiling
    }

    #[must_use]
    pub fn device(&self, device_id: DeviceId) -> Option<&EnrollmentDeviceRecord> {
        self.devices.get(&device_id)
    }

    #[must_use]
    pub fn devices(&self) -> impl Iterator<Item = &EnrollmentDeviceRecord> {
        self.devices.values()
    }

    #[must_use]
    pub fn is_device_active(&self, device_id: DeviceId) -> bool {
        self.device(device_id)
            .is_some_and(|device| device.status == EnrollmentStatus::Active)
    }
}

fn constant_time_eq(expected: &[u8; 32], actual: &[u8; 32]) -> bool {
    let mut difference = 0_u8;
    for (left, right) in expected.iter().zip(actual.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[derive(Debug, Error)]
pub enum EnrollmentError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("bootstrap JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid bootstrap: {0}")]
    InvalidBootstrap(&'static str),
    #[error("unsupported bootstrap version {0}")]
    UnsupportedBootstrapVersion(u32),
    #[error("bootstrap bearer secret does not match the signed hash")]
    BootstrapSecretMismatch,
    #[error("bootstrap was signed by a different Controller")]
    WrongController,
    #[error("bootstrap payload conflicts with the registered invite")]
    PassConflict,
    #[error("bootstrap is not valid yet")]
    NotYetValid,
    #[error("bootstrap has expired")]
    Expired,
    #[error("bootstrap first-claim deployment window has closed")]
    DeploymentWindowClosed,
    #[error("invite is closed to new claims")]
    InviteClosed,
    #[error("invite has been revoked")]
    InviteRevoked,
    #[error("invite claim capacity has been exhausted")]
    Exhausted,
    #[error("finalized enrollment cannot be replayed")]
    FinalizedReplay,
    #[error("enrollment claim was not found")]
    MissingClaim,
    #[error("unknown enrolled DeviceId {0}")]
    UnknownDevice(DeviceId),
    #[error("host persist acknowledgement token does not match")]
    PersistTokenMismatch,
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};

    use clew_core::MemberCapabilities;

    use super::*;
    use crate::DeviceIdentity;

    fn controller_and_registry() -> (ControllerIdentity, EnrollmentRegistry) {
        let controller = ControllerIdentity::from_secret([11_u8; 32]);
        let ceiling = PermissionGrant {
            member: MemberCapabilities::EXECUTE_AND_CONNECTOR,
            read: true,
            write: false,
            shell: true,
            tcp_egress: false,
        };
        let registry = EnrollmentRegistry::new(controller.controller_id(), ceiling);
        (controller, registry)
    }

    fn spec(max_claims: u32) -> SiteBootstrapSpec {
        SiteBootstrapSpec {
            site_id: SiteId::new(),
            invite_id: InviteId::new(),
            site_name: "Alice Lab".into(),
            grant: PermissionGrant {
                member: MemberCapabilities::EXECUTE_ONLY,
                read: true,
                write: true,
                shell: true,
                tcp_egress: false,
            },
            not_before_unix_ms: 1_000,
            expires_unix_ms: 10_000,
            deployment_window_ms: 500,
            max_claims,
        }
    }

    #[test]
    fn signed_bootstrap_tampering_fails_closed() {
        let (controller, mut registry) = controller_and_registry();
        let mut pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        pass.payload.site_name.push_str(" tampered");
        assert!(matches!(
            pass.verify(),
            Err(EnrollmentError::Identity(IdentityError::InvalidSignature))
        ));
    }

    #[test]
    fn pending_claim_is_idempotent_but_finalized_replay_fails() {
        let (controller, mut registry) = controller_and_registry();
        let pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        let device = DeviceIdentity::from_secret([22_u8; 32]).public_identity();
        let first = registry.claim(&pass, device, 1_100).unwrap();
        let retry_after_expiry = registry.claim(&pass, device, 20_000).unwrap();
        assert_eq!(retry_after_expiry.device_id, first.device_id);
        registry
            .finalize_host_persist(first.invite_id, first.device_id, first.persist_ack_token())
            .unwrap();
        let repeated_finalize = registry
            .finalize_host_persist(first.invite_id, first.device_id, first.persist_ack_token())
            .unwrap();
        assert_eq!(repeated_finalize.device_id, first.device_id);
        assert_eq!(repeated_finalize.status, EnrollmentStatus::Active);
        assert!(matches!(
            registry.claim(&pass, device, 1_200),
            Err(EnrollmentError::FinalizedReplay)
        ));
    }

    #[test]
    fn device_and_site_revoke_mark_future_authorization_inactive() {
        let (controller, mut registry) = controller_and_registry();
        let first_pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        let first_device = DeviceIdentity::generate().unwrap().public_identity();
        let first = registry.claim(&first_pass, first_device, 1_100).unwrap();
        registry
            .finalize_host_persist(first.invite_id, first.device_id, first.persist_ack_token())
            .unwrap();
        assert!(registry.is_device_active(first.device_id));
        registry.revoke_device(first.device_id).unwrap();
        assert!(!registry.is_device_active(first.device_id));

        let mut second_spec = spec(2);
        second_spec.site_id = first.site_id;
        let second_pass = registry.issue_bootstrap(&controller, second_spec).unwrap();
        let second_device = DeviceIdentity::generate().unwrap().public_identity();
        let second = registry.claim(&second_pass, second_device, 1_100).unwrap();
        registry
            .finalize_host_persist(
                second.invite_id,
                second.device_id,
                second.persist_ack_token(),
            )
            .unwrap();
        let revoked = registry.revoke_site(first.site_id);
        assert!(revoked.contains(&first.device_id));
        assert!(revoked.contains(&second.device_id));
        assert!(!registry.is_device_active(second.device_id));
    }

    #[test]
    fn expired_revoked_and_closed_invites_fail_closed() {
        let (controller, mut registry) = controller_and_registry();
        let expired = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        let device = DeviceIdentity::generate().unwrap().public_identity();
        assert!(matches!(
            registry.claim(&expired, device, 10_000),
            Err(EnrollmentError::Expired)
        ));

        let pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        registry.close_invite(pass.payload.invite_id);
        assert!(matches!(
            registry.claim(&pass, device, 1_100),
            Err(EnrollmentError::InviteClosed)
        ));

        let pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        registry.revoke_invite(pass.payload.invite_id);
        assert!(matches!(
            registry.claim(&pass, device, 1_100),
            Err(EnrollmentError::InviteRevoked)
        ));
    }

    #[test]
    fn deployment_window_allows_bounded_multi_claim_site_kit() {
        let (controller, mut registry) = controller_and_registry();
        let pass = registry.issue_bootstrap(&controller, spec(3)).unwrap();
        let first = DeviceIdentity::generate().unwrap().public_identity();
        let second = DeviceIdentity::generate().unwrap().public_identity();
        let third = DeviceIdentity::generate().unwrap().public_identity();
        registry.claim(&pass, first, 1_100).unwrap();
        registry.claim(&pass, second, 1_500).unwrap();
        assert!(matches!(
            registry.claim(&pass, third, 1_600),
            Err(EnrollmentError::DeploymentWindowClosed)
        ));
    }

    #[test]
    fn one_time_claim_is_atomic_under_concurrency() {
        let (controller, mut registry) = controller_and_registry();
        let pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        let registry = Arc::new(Mutex::new(registry));
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for seed in [31_u8, 32_u8] {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let pass = pass.clone();
            handles.push(std::thread::spawn(move || {
                let device = DeviceIdentity::from_secret([seed; 32]).public_identity();
                barrier.wait();
                registry.lock().unwrap().claim(&pass, device, 1_100)
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(EnrollmentError::Exhausted)))
                .count(),
            1
        );
    }

    #[test]
    fn claim_ceiling_can_make_same_site_pass_connector_only_without_escalation() {
        let (controller, mut registry) = controller_and_registry();
        let mut spec = spec(2);
        spec.grant = PermissionGrant::EXECUTE_READ_CONNECTOR;
        let pass = registry.issue_bootstrap(&controller, spec).unwrap();
        let helper = DeviceIdentity::generate().unwrap().public_identity();
        let receipt = registry
            .claim_with_ceiling(&pass, helper, 1_100, PermissionGrant::CONNECTOR_ONLY)
            .unwrap();
        assert!(!receipt.effective_grant.member.execute);
        assert!(receipt.effective_grant.member.connector);
        assert!(!receipt.effective_grant.read);
        assert!(!receipt.effective_grant.write);
        assert!(!receipt.effective_grant.shell);
    }

    #[test]
    fn policy_intersection_removes_write_but_preserves_allowed_read_shell() {
        let (controller, mut registry) = controller_and_registry();
        let pass = registry.issue_bootstrap(&controller, spec(1)).unwrap();
        let device = DeviceIdentity::generate().unwrap().public_identity();
        let receipt = registry.claim(&pass, device, 1_100).unwrap();
        assert!(receipt.effective_grant.read);
        assert!(!receipt.effective_grant.write);
        assert!(receipt.effective_grant.shell);
    }
}
