use std::{fmt, path::Path};

use clew_core::{ReadPolicy, site_access_credential_id};
use clew_identity::{
    ControllerIdentity, ControllerPublicIdentity, IdentityError, SignedSiteBootstrapPass,
};
use iroh::EndpointAddr;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{OutfitPreset, OutfitProfile};

const SITE_CLEW_VERSION: u32 = 1;
const CLIENT_FLAVOR_DOMAIN: &[u8] = b"clew/client-flavor/v1\0";
const MAX_SITE_CLEW_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetPlatform {
    Windows,
    MacOs,
    Linux,
}

impl TargetPlatform {
    #[must_use]
    pub const fn current() -> Self {
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            Self::Linux
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
            Self::Linux => "Linux",
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ClientFlavorId([u8; 32]);

impl ClientFlavorId {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn path_component(&self) -> String {
        hex(&self.0)
    }
}

impl fmt::Debug for ClientFlavorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.path_component())
    }
}

impl fmt::Display for ClientFlavorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.path_component())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientFlavor {
    pub runtime_version: String,
    pub platform: TargetPlatform,
    pub arch: String,
    pub outfit_id: String,
    pub outfit_revision: u32,
}

impl ClientFlavor {
    #[must_use]
    pub fn clew_original_current() -> Self {
        Self::from_outfit_current(&OutfitProfile::preset(OutfitPreset::ClewOriginal))
            .expect("built-in Clew Original outfit must remain valid")
    }

    pub fn from_outfit_current(outfit: &OutfitProfile) -> Result<Self, SiteClewError> {
        Self::from_outfit_target(outfit, TargetPlatform::current(), std::env::consts::ARCH)
    }

    pub fn from_outfit_target(
        outfit: &OutfitProfile,
        platform: TargetPlatform,
        arch: &str,
    ) -> Result<Self, SiteClewError> {
        outfit.validate().map_err(SiteClewError::Outfit)?;
        if arch.is_empty()
            || arch.len() > 32
            || !arch
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(SiteClewError::InvalidTargetArchitecture);
        }
        Ok(Self {
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            platform,
            arch: arch.into(),
            outfit_id: outfit.outfit_id.clone(),
            outfit_revision: outfit.revision,
        })
    }

    pub fn id(&self) -> Result<ClientFlavorId, SiteClewError> {
        let encoded = serde_json::to_vec(self)?;
        let mut hasher = Sha256::new();
        hasher.update(CLIENT_FLAVOR_DOMAIN);
        hasher.update(encoded);
        Ok(ClientFlavorId(hasher.finalize().into()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRoleHint {
    ExecutePreferred,
    ConnectorOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SiteClewPayload {
    pub version: u32,
    pub client_flavor: ClientFlavor,
    pub client_flavor_id: ClientFlavorId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outfit_profile: Option<OutfitProfile>,
    pub bootstrap: SignedSiteBootstrapPass,
    pub role_hint: HostRoleHint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_endpoint: Option<EndpointAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_policy: Option<ReadPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller_bootstrap_noise_public_key: Option<[u8; 32]>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignedSiteClew {
    pub payload: SiteClewPayload,
    pub signature: Vec<u8>,
}

impl fmt::Debug for SignedSiteClew {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedSiteClew")
            .field("payload", &self.payload)
            .field("signature_len", &self.signature.len())
            .finish()
    }
}

impl SignedSiteClew {
    #[must_use]
    pub fn site_access_credential_id(&self) -> String {
        site_access_credential_id(self.payload.bootstrap.payload.invite_id)
    }

    pub fn issue(
        controller: &ControllerIdentity,
        client_flavor: ClientFlavor,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
    ) -> Result<Self, SiteClewError> {
        Self::issue_with_network(
            controller,
            client_flavor,
            None,
            bootstrap,
            role_hint,
            None,
            None,
            None,
        )
    }

    pub fn issue_networked(
        controller: &ControllerIdentity,
        client_flavor: ClientFlavor,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
        controller_endpoint: EndpointAddr,
        read_policy: ReadPolicy,
    ) -> Result<Self, SiteClewError> {
        read_policy.validate()?;
        Self::issue_with_network(
            controller,
            client_flavor,
            None,
            bootstrap,
            role_hint,
            Some(controller_endpoint),
            Some(read_policy),
            None,
        )
    }

    pub fn issue_networked_outfit(
        controller: &ControllerIdentity,
        outfit_profile: OutfitProfile,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
        controller_endpoint: EndpointAddr,
        read_policy: ReadPolicy,
    ) -> Result<Self, SiteClewError> {
        outfit_profile.validate()?;
        read_policy.validate()?;
        let client_flavor = ClientFlavor::from_outfit_current(&outfit_profile)?;
        Self::issue_with_network(
            controller,
            client_flavor,
            Some(outfit_profile),
            bootstrap,
            role_hint,
            Some(controller_endpoint),
            Some(read_policy),
            None,
        )
    }

    pub fn issue_networked_outfit_sealed(
        controller: &ControllerIdentity,
        outfit_profile: OutfitProfile,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
        controller_endpoint: EndpointAddr,
        read_policy: ReadPolicy,
        controller_bootstrap_noise_public_key: [u8; 32],
    ) -> Result<Self, SiteClewError> {
        let client_flavor = ClientFlavor::from_outfit_current(&outfit_profile)?;
        Self::issue_networked_outfit_sealed_for_flavor(
            controller,
            outfit_profile,
            client_flavor,
            bootstrap,
            role_hint,
            controller_endpoint,
            read_policy,
            controller_bootstrap_noise_public_key,
        )
    }

    pub fn issue_networked_outfit_sealed_for_flavor(
        controller: &ControllerIdentity,
        outfit_profile: OutfitProfile,
        client_flavor: ClientFlavor,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
        controller_endpoint: EndpointAddr,
        read_policy: ReadPolicy,
        controller_bootstrap_noise_public_key: [u8; 32],
    ) -> Result<Self, SiteClewError> {
        outfit_profile.validate()?;
        read_policy.validate()?;
        if client_flavor.outfit_id != outfit_profile.outfit_id
            || client_flavor.outfit_revision != outfit_profile.revision
        {
            return Err(SiteClewError::OutfitFlavorMismatch);
        }
        Self::issue_with_network(
            controller,
            client_flavor,
            Some(outfit_profile),
            bootstrap,
            role_hint,
            Some(controller_endpoint),
            Some(read_policy),
            Some(controller_bootstrap_noise_public_key),
        )
    }

    fn issue_with_network(
        controller: &ControllerIdentity,
        client_flavor: ClientFlavor,
        outfit_profile: Option<OutfitProfile>,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
        controller_endpoint: Option<EndpointAddr>,
        read_policy: Option<ReadPolicy>,
        controller_bootstrap_noise_public_key: Option<[u8; 32]>,
    ) -> Result<Self, SiteClewError> {
        let bootstrap_controller = bootstrap.verify()?;
        if bootstrap_controller != controller.public_identity() {
            return Err(SiteClewError::ControllerMismatch);
        }
        if let Some(policy) = &read_policy {
            policy.validate()?;
        }
        validate_controller_bootstrap_noise_public_key(controller_bootstrap_noise_public_key)?;
        if let Some(profile) = &outfit_profile {
            profile.validate()?;
            if profile.outfit_id != client_flavor.outfit_id
                || profile.revision != client_flavor.outfit_revision
            {
                return Err(SiteClewError::OutfitFlavorMismatch);
            }
        }
        let client_flavor_id = client_flavor.id()?;
        let payload = SiteClewPayload {
            version: SITE_CLEW_VERSION,
            client_flavor,
            client_flavor_id,
            outfit_profile,
            bootstrap,
            role_hint,
            controller_endpoint,
            read_policy,
            controller_bootstrap_noise_public_key,
        };
        let signature = controller.sign_site_config(&payload)?;
        Ok(Self { payload, signature })
    }

    pub fn verify(&self) -> Result<ControllerPublicIdentity, SiteClewError> {
        if self.payload.version != SITE_CLEW_VERSION {
            return Err(SiteClewError::UnsupportedVersion(self.payload.version));
        }
        if self.payload.client_flavor.id()? != self.payload.client_flavor_id {
            return Err(SiteClewError::FlavorFingerprintMismatch);
        }
        if let Some(profile) = &self.payload.outfit_profile {
            profile.validate()?;
            if profile.outfit_id != self.payload.client_flavor.outfit_id
                || profile.revision != self.payload.client_flavor.outfit_revision
            {
                return Err(SiteClewError::OutfitFlavorMismatch);
            }
        }
        if let Some(policy) = &self.payload.read_policy {
            policy.validate()?;
        }
        validate_controller_bootstrap_noise_public_key(
            self.payload.controller_bootstrap_noise_public_key,
        )?;
        if self.payload.controller_endpoint.is_some() != self.payload.read_policy.is_some() {
            return Err(SiteClewError::IncompleteNetworkConfig);
        }
        let controller = self.payload.bootstrap.verify()?;
        controller.verify_site_config(&self.payload, &self.signature)?;
        Ok(controller)
    }

    pub fn verify_for_flavor(&self, runtime: &ClientFlavor) -> Result<(), SiteClewError> {
        self.effective_flavor_for_runtime(runtime).map(|_| ())
    }

    pub fn effective_flavor_for_runtime(
        &self,
        runtime: &ClientFlavor,
    ) -> Result<ClientFlavor, SiteClewError> {
        self.verify()?;
        let mut expected = runtime.clone();
        if let Some(profile) = &self.payload.outfit_profile {
            expected.outfit_id = profile.outfit_id.clone();
            expected.outfit_revision = profile.revision;
        }
        if self.payload.client_flavor != expected
            || self.payload.client_flavor_id != expected.id()?
        {
            return Err(SiteClewError::WrongClientFlavor);
        }
        Ok(expected)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, SiteClewError> {
        self.verify()?;
        let encoded = serde_json::to_vec_pretty(self)?;
        check_size(encoded.len())?;
        Ok(encoded)
    }

    pub fn from_bytes(input: &[u8]) -> Result<Self, SiteClewError> {
        check_size(input.len())?;
        #[derive(Deserialize)]
        struct Header {
            payload: PayloadHeader,
        }
        #[derive(Deserialize)]
        struct PayloadHeader {
            version: u32,
        }
        let header: Header = serde_json::from_slice(input)?;
        if header.payload.version != SITE_CLEW_VERSION {
            return Err(SiteClewError::UnsupportedVersion(header.payload.version));
        }
        let file: Self = serde_json::from_slice(input)?;
        file.verify()?;
        Ok(file)
    }

    pub fn read(path: &Path) -> Result<Self, SiteClewError> {
        let metadata = std::fs::metadata(path)?;
        check_size(metadata.len().try_into().unwrap_or(usize::MAX))?;
        let encoded = std::fs::read(path)?;
        Self::from_bytes(&encoded)
    }

    pub fn write(&self, path: &Path) -> Result<(), SiteClewError> {
        let encoded = self.to_bytes()?;
        std::fs::write(path, encoded)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteKitContract {
    pub platform: TargetPlatform,
    pub runtime_entry: &'static str,
    pub sidecar_name: &'static str,
    pub start_here_name: &'static str,
    /// Same runtime, normal friend-facing entry.
    pub use_this_machine_args: &'static [&'static str],
    /// Same runtime and site.clew, but explicitly narrows enrollment to Connector-only.
    pub help_nearby_args: &'static [&'static str],
}

impl SiteKitContract {
    #[must_use]
    pub const fn for_platform(platform: TargetPlatform) -> Self {
        match platform {
            TargetPlatform::Windows => Self {
                platform,
                runtime_entry: "Clew.exe",
                sidecar_name: "site.clew",
                start_here_name: "Start Here.html",
                use_this_machine_args: &["host"],
                help_nearby_args: &["host", "--connector-only"],
            },
            TargetPlatform::MacOs => Self {
                platform,
                runtime_entry: "Clew.app",
                sidecar_name: "site.clew",
                start_here_name: "Start Here.html",
                use_this_machine_args: &["host"],
                help_nearby_args: &["host", "--connector-only"],
            },
            TargetPlatform::Linux => Self {
                platform,
                runtime_entry: "Clew",
                sidecar_name: "site.clew",
                start_here_name: "Start Here.html",
                use_this_machine_args: &["host"],
                help_nearby_args: &["host", "--connector-only"],
            },
        }
    }

    #[must_use]
    pub fn archive_name(&self, site_name: &str) -> String {
        let cleaned = site_name.trim().replace(['/', '\\'], "-");
        match self.platform {
            TargetPlatform::Windows => format!("{cleaned}-Clew-Windows.zip"),
            TargetPlatform::MacOs => format!("{cleaned}-Clew-macOS.zip"),
            TargetPlatform::Linux => format!("{cleaned}-Clew-Linux.tar.gz"),
        }
    }
}

fn validate_controller_bootstrap_noise_public_key(
    key: Option<[u8; 32]>,
) -> Result<(), SiteClewError> {
    if key.is_some_and(|value| value == [0_u8; 32]) {
        return Err(SiteClewError::InvalidControllerBootstrapNoisePublicKey);
    }
    Ok(())
}

fn check_size(actual: usize) -> Result<(), SiteClewError> {
    if actual > MAX_SITE_CLEW_BYTES {
        return Err(SiteClewError::TooLarge {
            actual,
            max: MAX_SITE_CLEW_BYTES,
        });
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(TABLE[(byte >> 4) as usize] as char);
        output.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Error)]
pub enum SiteClewError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Outfit(#[from] crate::OutfitError),
    #[error(transparent)]
    Enrollment(#[from] clew_identity::EnrollmentError),
    #[error("site.clew JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("site.clew I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported site.clew version {0}")]
    UnsupportedVersion(u32),
    #[error(transparent)]
    Policy(#[from] clew_core::ControlModelError),
    #[error("site.clew network config must contain both endpoint and read policy")]
    IncompleteNetworkConfig,
    #[error("site.clew Controller sealed-bootstrap Noise public key is invalid")]
    InvalidControllerBootstrapNoisePublicKey,
    #[error("site.clew belongs to a different Controller")]
    ControllerMismatch,
    #[error("site.clew ClientFlavor fingerprint does not match its descriptor")]
    FlavorFingerprintMismatch,
    #[error("site.clew OutfitProfile does not match its ClientFlavor")]
    OutfitFlavorMismatch,
    #[error("target architecture is empty, too long, or contains unsupported characters")]
    InvalidTargetArchitecture,
    #[error("site.clew was generated for a different ClientFlavor")]
    WrongClientFlavor,
    #[error("site.clew is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
}

#[cfg(test)]
mod tests {
    use clew_core::{InviteId, SiteId};
    use clew_identity::{EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec};

    use super::*;

    fn file() -> SignedSiteClew {
        let controller = ControllerIdentity::from_secret([71_u8; 32]);
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Alice Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 4,
                },
            )
            .unwrap();
        SignedSiteClew::issue(
            &controller,
            ClientFlavor::clew_original_current(),
            bootstrap,
            HostRoleHint::ExecutePreferred,
        )
        .unwrap()
    }

    #[test]
    fn signed_site_file_roundtrips_and_tampering_fails() {
        let file = file();
        let encoded = file.to_bytes().unwrap();
        let decoded = SignedSiteClew::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, file);

        let mut tampered = decoded;
        tampered.payload.role_hint = HostRoleHint::ConnectorOnly;
        assert!(matches!(
            tampered.verify(),
            Err(SiteClewError::Identity(IdentityError::InvalidSignature))
        ));
    }

    #[test]
    fn target_client_flavor_is_explicit_and_arch_is_bounded() {
        let profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        let linux =
            ClientFlavor::from_outfit_target(&profile, TargetPlatform::Linux, "x86_64").unwrap();
        assert_eq!(linux.platform, TargetPlatform::Linux);
        assert_eq!(linux.arch, "x86_64");
        assert_eq!(linux.outfit_id, profile.outfit_id);
        assert_eq!(linux.outfit_revision, profile.revision);

        for invalid in ["", "x86 64", "../x86_64", "x86/64"] {
            assert!(matches!(
                ClientFlavor::from_outfit_target(&profile, TargetPlatform::Linux, invalid),
                Err(SiteClewError::InvalidTargetArchitecture)
            ));
        }
        let too_long = "a".repeat(33);
        assert!(matches!(
            ClientFlavor::from_outfit_target(&profile, TargetPlatform::Linux, &too_long),
            Err(SiteClewError::InvalidTargetArchitecture)
        ));
    }

    #[test]
    fn signed_outfit_adopts_only_outfit_dimension_of_current_runtime() {
        let controller = ControllerIdentity::from_secret([72_u8; 32]);
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Outfit Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 4,
                },
            )
            .unwrap();
        let mut profile = OutfitProfile::preset(OutfitPreset::ResearchLab);
        profile.outfit_id = "huang-lab".into();
        profile.display_name = "Huang Lab".into();
        profile.revision = 7;
        profile.validate().unwrap();
        let flavor = ClientFlavor::from_outfit_current(&profile).unwrap();
        let file = SignedSiteClew::issue_with_network(
            &controller,
            flavor.clone(),
            Some(profile.clone()),
            bootstrap,
            HostRoleHint::ExecutePreferred,
            None,
            None,
            None,
        )
        .unwrap();

        let runtime = ClientFlavor::clew_original_current();
        assert_eq!(file.effective_flavor_for_runtime(&runtime).unwrap(), flavor);
        let mut wrong_runtime = runtime;
        wrong_runtime.runtime_version.push_str("-other");
        assert!(matches!(
            file.effective_flavor_for_runtime(&wrong_runtime),
            Err(SiteClewError::WrongClientFlavor)
        ));

        let mut tampered = file;
        tampered
            .payload
            .outfit_profile
            .as_mut()
            .unwrap()
            .visuals
            .primary_color = "#123456".into();
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn networked_site_file_signs_endpoint_and_read_policy() {
        let controller = ControllerIdentity::from_secret([72_u8; 32]);
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Network Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 2,
                },
            )
            .unwrap();
        let endpoint_secret = iroh::SecretKey::from_bytes(&[9_u8; 32]);
        let endpoint = EndpointAddr {
            id: endpoint_secret.public(),
            addrs: Default::default(),
        };
        let policy = ReadPolicy::new(vec!["D:/shared".into()], 4096, 2_000).unwrap();
        let file = SignedSiteClew::issue_networked(
            &controller,
            ClientFlavor::clew_original_current(),
            bootstrap,
            HostRoleHint::ExecutePreferred,
            endpoint.clone(),
            policy.clone(),
        )
        .unwrap();
        let decoded = SignedSiteClew::from_bytes(&file.to_bytes().unwrap()).unwrap();
        assert_eq!(decoded.payload.controller_endpoint, Some(endpoint));
        assert_eq!(decoded.payload.read_policy, Some(policy));

        let mut tampered = decoded;
        tampered
            .payload
            .read_policy
            .as_mut()
            .unwrap()
            .max_result_bytes = 1;
        assert!(matches!(
            tampered.verify(),
            Err(SiteClewError::Identity(IdentityError::InvalidSignature))
        ));
    }

    #[test]
    fn sealed_networked_site_file_signs_controller_bootstrap_noise_public_key() {
        let controller = ControllerIdentity::from_secret([81_u8; 32]);
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Sealed Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 2,
                },
            )
            .unwrap();
        let endpoint = EndpointAddr {
            id: iroh::SecretKey::from_bytes(&[82_u8; 32]).public(),
            addrs: Default::default(),
        };
        let profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        let policy = ReadPolicy::new(vec!["D:/sealed".into()], 4096, 2_000).unwrap();
        let key = [83_u8; 32];
        let file = SignedSiteClew::issue_networked_outfit_sealed(
            &controller,
            profile,
            bootstrap,
            HostRoleHint::ExecutePreferred,
            endpoint,
            policy,
            key,
        )
        .unwrap();
        assert_eq!(
            file.payload.controller_bootstrap_noise_public_key,
            Some(key)
        );
        assert_eq!(
            SignedSiteClew::from_bytes(&file.to_bytes().unwrap()).unwrap(),
            file
        );

        let mut tampered = file.clone();
        tampered.payload.controller_bootstrap_noise_public_key = Some([84_u8; 32]);
        assert!(tampered.verify().is_err());
        let mut invalid = file;
        invalid.payload.controller_bootstrap_noise_public_key = Some([0_u8; 32]);
        assert!(matches!(
            invalid.verify(),
            Err(SiteClewError::InvalidControllerBootstrapNoisePublicKey)
        ));
    }

    #[test]
    fn site_kit_contract_is_per_platform_not_fat_bundle() {
        let windows = SiteKitContract::for_platform(TargetPlatform::Windows);
        let mac = SiteKitContract::for_platform(TargetPlatform::MacOs);
        let linux = SiteKitContract::for_platform(TargetPlatform::Linux);
        assert_eq!(windows.runtime_entry, "Clew.exe");
        assert_eq!(mac.runtime_entry, "Clew.app");
        assert_eq!(linux.runtime_entry, "Clew");
        assert_eq!(windows.start_here_name, "Start Here.html");
        assert_eq!(mac.start_here_name, windows.start_here_name);
        assert_eq!(linux.start_here_name, windows.start_here_name);
        assert_eq!(windows.use_this_machine_args, &["host"]);
        assert_eq!(windows.help_nearby_args, &["host", "--connector-only"]);
        assert_eq!(mac.help_nearby_args, windows.help_nearby_args);
        assert_eq!(linux.help_nearby_args, windows.help_nearby_args);
        assert!(windows.archive_name("Alice").ends_with("Windows.zip"));
    }
}
