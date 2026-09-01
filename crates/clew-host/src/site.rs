use std::{fmt, path::Path};

use clew_identity::{
    ControllerIdentity, ControllerPublicIdentity, IdentityError, SignedSiteBootstrapPass,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::OutfitRuntimeView;

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
        let outfit = OutfitRuntimeView::clew_original();
        Self {
            runtime_version: env!("CARGO_PKG_VERSION").into(),
            platform: TargetPlatform::current(),
            arch: std::env::consts::ARCH.into(),
            outfit_id: outfit.outfit_id.into(),
            outfit_revision: outfit.revision,
        }
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
    pub bootstrap: SignedSiteBootstrapPass,
    pub role_hint: HostRoleHint,
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
    pub fn issue(
        controller: &ControllerIdentity,
        client_flavor: ClientFlavor,
        bootstrap: SignedSiteBootstrapPass,
        role_hint: HostRoleHint,
    ) -> Result<Self, SiteClewError> {
        let bootstrap_controller = bootstrap.verify()?;
        if bootstrap_controller != controller.public_identity() {
            return Err(SiteClewError::ControllerMismatch);
        }
        let client_flavor_id = client_flavor.id()?;
        let payload = SiteClewPayload {
            version: SITE_CLEW_VERSION,
            client_flavor,
            client_flavor_id,
            bootstrap,
            role_hint,
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
        let controller = self.payload.bootstrap.verify()?;
        controller.verify_site_config(&self.payload, &self.signature)?;
        Ok(controller)
    }

    pub fn verify_for_flavor(&self, expected: &ClientFlavor) -> Result<(), SiteClewError> {
        self.verify()?;
        if &self.payload.client_flavor != expected
            || self.payload.client_flavor_id != expected.id()?
        {
            return Err(SiteClewError::WrongClientFlavor);
        }
        Ok(())
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
}

impl SiteKitContract {
    #[must_use]
    pub const fn for_platform(platform: TargetPlatform) -> Self {
        match platform {
            TargetPlatform::Windows => Self {
                platform,
                runtime_entry: "Clew.exe",
                sidecar_name: "site.clew",
                start_here_name: "开始这里.html",
            },
            TargetPlatform::MacOs => Self {
                platform,
                runtime_entry: "Clew.app",
                sidecar_name: "site.clew",
                start_here_name: "开始这里.html",
            },
            TargetPlatform::Linux => Self {
                platform,
                runtime_entry: "Clew",
                sidecar_name: "site.clew",
                start_here_name: "开始这里.html",
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
    Enrollment(#[from] clew_identity::EnrollmentError),
    #[error("site.clew JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("site.clew I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported site.clew version {0}")]
    UnsupportedVersion(u32),
    #[error("site.clew belongs to a different Controller")]
    ControllerMismatch,
    #[error("site.clew ClientFlavor fingerprint does not match its descriptor")]
    FlavorFingerprintMismatch,
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
    fn site_kit_contract_is_per_platform_not_fat_bundle() {
        let windows = SiteKitContract::for_platform(TargetPlatform::Windows);
        let mac = SiteKitContract::for_platform(TargetPlatform::MacOs);
        let linux = SiteKitContract::for_platform(TargetPlatform::Linux);
        assert_eq!(windows.runtime_entry, "Clew.exe");
        assert_eq!(mac.runtime_entry, "Clew.app");
        assert_eq!(linux.runtime_entry, "Clew");
        assert!(windows.archive_name("Alice").ends_with("Windows.zip"));
    }
}
