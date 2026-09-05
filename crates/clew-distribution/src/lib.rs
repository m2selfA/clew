#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

mod site_kit;
pub use site_kit::{
    SiteKitAssemblyRequest, SiteKitAssemblyResult, SiteKitAsset, assemble_site_kit,
};

pub const RELEASE_SCHEMA_VERSION: u32 = 2;
pub const SIGNED_RELEASE_SCHEMA_VERSION: u32 = 3;
pub const CLIENT_FLAVOR_CACHE_SCHEMA_VERSION: u32 = 1;
pub const LEGACY_SITE_KIT_LAUNCHER_SCHEMA_VERSION: u32 = 1;
pub const SITE_KIT_LAUNCHER_SCHEMA_VERSION: u32 = 2;
pub const SITE_KIT_SCHEMA_VERSION: u32 = 1;
pub const USE_ROLE_DIR: &str = "1 Use this computer";
pub const HELPER_ROLE_DIR: &str = "2 Help nearby computers";
pub const ROLE_HINT_FILE: &str = "role-hint.clew";
pub const MAX_CLIENT_FLAVOR_CACHE_ENTRY_BYTES: u64 = 64 * 1024;
pub const MAX_RELEASE_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_RELEASE_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_RELEASE_PAYLOAD_FILES: usize = 128;
pub const MAX_STORED_CLIENT_FLAVORS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleasePlatform {
    Windows,
    Macos,
    Linux,
}

impl ReleasePlatform {
    pub fn from_target(target: &str) -> Result<Self, DistributionError> {
        if target.contains("windows") {
            Ok(Self::Windows)
        } else if target.contains("apple-darwin") {
            Ok(Self::Macos)
        } else if target.contains("linux") {
            Ok(Self::Linux)
        } else {
            Err(DistributionError::UnsupportedTarget(target.to_owned()))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PayloadFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ToolchainInfo {
    pub release: String,
    pub commit_hash: String,
    pub host: String,
    pub llvm_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SigningInfo {
    pub mechanism: String,
    pub identity: String,
    pub timestamped: bool,
    pub notarized: bool,
    pub stapled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notary_submission_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ReleaseClientFlavorInfo {
    pub id: String,
    pub outfit_id: String,
    pub outfit_revision: u32,
    pub build_cache_key: String,
    pub app_display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher_label: Option<String>,
    pub icon_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_asset_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SiteKitLauncherInfo {
    pub schema_version: u32,
    pub executable_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PayloadManifest {
    pub schema_version: u32,
    pub product: String,
    pub version: String,
    pub target: String,
    pub profile: String,
    pub archive_format: String,
    pub layout: String,
    pub app_id: String,
    pub entrypoint: String,
    pub cli_binary: String,
    pub source_commit: String,
    pub source_date_epoch: u64,
    pub rustc: ToolchainInfo,
    pub cargo_lock_sha256: String,
    pub dirty: bool,
    pub unsigned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<SigningInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_flavor: Option<ReleaseClientFlavorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_kit_launcher: Option<SiteKitLauncherInfo>,
    pub files: Vec<PayloadFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ArtifactInfo {
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ArtifactManifest {
    pub payload: PayloadManifest,
    pub artifact: ArtifactInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ClientFlavorCacheEntry {
    pub schema_version: u32,
    pub cache_key: String,
    pub client_flavor: ReleaseClientFlavorInfo,
    pub version: String,
    pub target: String,
    pub profile: String,
    pub source_commit: String,
    pub release_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing: Option<SigningInfo>,
    pub artifact_file: String,
    pub artifact_sha256: String,
    pub manifest_file: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SiteKitPayloadManifest {
    pub schema_version: u32,
    pub source_cache_key: String,
    pub client_flavor: ReleaseClientFlavorInfo,
    pub target: String,
    pub source_release_sha256: String,
    pub site_sha256: String,
    pub runtime_release_ready: bool,
    pub files: Vec<PayloadFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SiteKitArtifactManifest {
    pub payload: SiteKitPayloadManifest,
    pub artifact: ArtifactInfo,
}

#[derive(Serialize)]
struct ClientFlavorCacheKeyMaterial<'a> {
    client_flavor_id: &'a str,
    build_cache_key: &'a str,
    version: &'a str,
    target: &'a str,
    profile: &'a str,
    source_commit: &'a str,
    signing_mechanism: &'a str,
    signing_identity: &'a str,
}

pub fn client_flavor_cache_key(
    client_flavor: &ReleaseClientFlavorInfo,
    payload: &PayloadManifest,
) -> Result<String, DistributionError> {
    let (mechanism, identity) = payload
        .signing
        .as_ref()
        .map(|signing| (signing.mechanism.as_str(), signing.identity.as_str()))
        .unwrap_or(("unsigned", "unsigned"));
    let material = ClientFlavorCacheKeyMaterial {
        client_flavor_id: &client_flavor.id,
        build_cache_key: &client_flavor.build_cache_key,
        version: &payload.version,
        target: &payload.target,
        profile: &payload.profile,
        source_commit: &payload.source_commit,
        signing_mechanism: mechanism,
        signing_identity: identity,
    };
    Ok(format!(
        "client-flavor-v1-{}",
        sha256_bytes(&serde_json::to_vec(&material)?)
    ))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientFlavorArtifactSummary {
    pub cache_key: String,
    pub client_flavor_id: String,
    pub outfit_id: String,
    pub outfit_revision: u32,
    pub build_cache_key: String,
    pub app_display_name: String,
    pub version: String,
    pub target: String,
    pub platform: ReleasePlatform,
    pub arch: String,
    pub source_commit: String,
    #[serde(default)]
    pub site_kit_launcher_schema: u32,
    pub release_ready: bool,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct ValidatedClientFlavorArtifact {
    pub root: PathBuf,
    pub entry: ClientFlavorCacheEntry,
    pub release: ArtifactManifest,
    pub platform: ReleasePlatform,
    pub arch: String,
}

impl ValidatedClientFlavorArtifact {
    #[must_use]
    pub fn summary(&self, active: bool) -> ClientFlavorArtifactSummary {
        ClientFlavorArtifactSummary {
            cache_key: self.entry.cache_key.clone(),
            client_flavor_id: self.entry.client_flavor.id.clone(),
            outfit_id: self.entry.client_flavor.outfit_id.clone(),
            outfit_revision: self.entry.client_flavor.outfit_revision,
            build_cache_key: self.entry.client_flavor.build_cache_key.clone(),
            app_display_name: self.entry.client_flavor.app_display_name.clone(),
            version: self.entry.version.clone(),
            target: self.entry.target.clone(),
            platform: self.platform,
            arch: self.arch.clone(),
            source_commit: self.entry.source_commit.clone(),
            site_kit_launcher_schema: self
                .release
                .payload
                .site_kit_launcher
                .as_ref()
                .map_or(0, |launcher| launcher.schema_version),
            release_ready: self.entry.release_ready,
            active,
        }
    }
}

pub fn validate_cache_entry_dir(
    root: &Path,
    require_release_ready: bool,
) -> Result<ValidatedClientFlavorArtifact, DistributionError> {
    require_directory(root, "ClientFlavor cache entry")?;
    let dir_name = root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(DistributionError::InvalidCacheDirectoryName)?;
    let entry: ClientFlavorCacheEntry = read_json_bounded(
        &root.join("cache-entry.json"),
        MAX_CLIENT_FLAVOR_CACHE_ENTRY_BYTES,
    )?;
    if entry.schema_version != CLIENT_FLAVOR_CACHE_SCHEMA_VERSION {
        return Err(DistributionError::UnsupportedCacheSchema(
            entry.schema_version,
        ));
    }
    validate_cache_key(&entry.cache_key)?;
    if dir_name != entry.cache_key {
        return Err(DistributionError::CacheDirectoryKeyMismatch);
    }
    if require_release_ready && !entry.release_ready {
        return Err(DistributionError::NotReleaseReady);
    }
    validate_basename(&entry.artifact_file)?;
    validate_basename(&entry.manifest_file)?;
    let manifest_path = root.join(&entry.manifest_file);
    let artifact_path = root.join(&entry.artifact_file);
    let manifest_bytes = read_bounded_regular(&manifest_path, MAX_RELEASE_MANIFEST_BYTES)?;
    if sha256_bytes(&manifest_bytes) != entry.manifest_sha256 {
        return Err(DistributionError::ManifestHashMismatch);
    }
    let release: ArtifactManifest = serde_json::from_slice(&manifest_bytes)?;
    let artifact_metadata = regular_metadata(&artifact_path)?;
    if artifact_metadata.len() > MAX_RELEASE_ARTIFACT_BYTES
        || artifact_metadata.len() != release.artifact.size
        || release.artifact.file != entry.artifact_file
        || release.artifact.sha256 != entry.artifact_sha256
        || sha256_file(&artifact_path)? != entry.artifact_sha256
    {
        return Err(DistributionError::ArtifactMismatch);
    }
    validate_release_shape(&entry, &release)?;
    verify_release_zip(&artifact_path, &release)?;
    let platform = ReleasePlatform::from_target(&entry.target)?;
    let arch = target_arch(&entry.target)?.to_owned();
    Ok(ValidatedClientFlavorArtifact {
        root: root.to_path_buf(),
        entry,
        release,
        platform,
        arch,
    })
}

fn validate_release_shape(
    entry: &ClientFlavorCacheEntry,
    release: &ArtifactManifest,
) -> Result<(), DistributionError> {
    let payload = &release.payload;
    validate_bounded_text(&payload.product, 32, "product")?;
    validate_bounded_text(&payload.version, 64, "version")?;
    validate_target_text(&payload.target)?;
    validate_profile_text(&payload.profile)?;
    validate_bounded_text(&payload.layout, 64, "layout")?;
    validate_bounded_text(&payload.app_id, 128, "app id")?;
    validate_git_commit(&payload.source_commit)?;
    validate_sha256(&payload.cargo_lock_sha256)?;
    validate_client_flavor(&entry.client_flavor)?;
    if let Some(signing) = &payload.signing {
        validate_bounded_text(&signing.mechanism, 64, "signing mechanism")?;
        validate_bounded_text(&signing.identity, 256, "signing identity")?;
        if let Some(id) = &signing.notary_submission_id {
            validate_bounded_text(id, 128, "notary submission id")?;
        }
    }
    if payload.product != "clew"
        || payload.archive_format != "zip"
        || payload.dirty
        || payload.version != entry.version
        || payload.target != entry.target
        || payload.profile != entry.profile
        || payload.source_commit != entry.source_commit
        || payload.signing != entry.signing
        || payload.client_flavor.as_ref() != Some(&entry.client_flavor)
        || payload.site_kit_launcher.is_none()
    {
        return Err(DistributionError::ReleaseMetadataMismatch);
    }
    let launcher = payload.site_kit_launcher.as_ref().expect("checked above");
    if !matches!(
        launcher.schema_version,
        LEGACY_SITE_KIT_LAUNCHER_SCHEMA_VERSION | SITE_KIT_LAUNCHER_SCHEMA_VERSION
    ) {
        return Err(DistributionError::UnsupportedLauncherSchema(
            launcher.schema_version,
        ));
    }
    validate_relative_path(&launcher.executable_path)?;
    if !payload
        .files
        .iter()
        .any(|file| file.path == launcher.executable_path)
    {
        return Err(DistributionError::LauncherMissingFromPayload);
    }
    if let Some(bundle_root) = &launcher.bundle_root {
        validate_relative_path(bundle_root)?;
        let prefix = format!("{bundle_root}/");
        if !launcher.executable_path.starts_with(&prefix) {
            return Err(DistributionError::LauncherBundleMismatch);
        }
    }
    let expected_key = client_flavor_cache_key(&entry.client_flavor, payload)?;
    if expected_key != entry.cache_key {
        return Err(DistributionError::SemanticCacheKeyMismatch);
    }
    let platform = ReleasePlatform::from_target(&entry.target)?;
    match (
        payload.schema_version,
        payload.unsigned,
        payload.signing.as_ref(),
    ) {
        (RELEASE_SCHEMA_VERSION, true, None) => {}
        (SIGNED_RELEASE_SCHEMA_VERSION, false, Some(_)) => {}
        _ => return Err(DistributionError::InvalidSigningState),
    }
    let expected_ready = match platform {
        ReleasePlatform::Linux => payload.unsigned,
        ReleasePlatform::Windows | ReleasePlatform::Macos => !payload.unsigned,
    };
    if entry.release_ready != expected_ready {
        return Err(DistributionError::InvalidReleaseReadyState);
    }
    match (platform, payload.signing.as_ref()) {
        (ReleasePlatform::Windows, Some(signing))
            if signing.mechanism == "windows-authenticode"
                && signing.timestamped
                && !signing.notarized
                && !signing.stapled => {}
        (ReleasePlatform::Macos, Some(signing))
            if signing.mechanism == "macos-developer-id-notarized"
                && signing.timestamped
                && signing.notarized
                && signing.stapled => {}
        (ReleasePlatform::Linux, None) => {}
        (ReleasePlatform::Windows | ReleasePlatform::Macos, None) if payload.unsigned => {}
        (_, _) => return Err(DistributionError::InvalidPlatformSigningEvidence),
    }
    if payload.files.is_empty() || payload.files.len() > MAX_RELEASE_PAYLOAD_FILES {
        return Err(DistributionError::InvalidPayloadFileSet);
    }
    let mut previous: Option<&str> = None;
    for file in &payload.files {
        validate_relative_path(&file.path)?;
        validate_sha256(&file.sha256)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err(DistributionError::InvalidPayloadFileSet);
        }
        previous = Some(&file.path);
    }
    Ok(())
}

fn verify_release_zip(
    artifact_path: &Path,
    release: &ArtifactManifest,
) -> Result<(), DistributionError> {
    let payload = &release.payload;
    let stem = if payload.unsigned {
        format!("clew-v{}-{}", payload.version, payload.target)
    } else {
        format!("clew-v{}-{}-signed", payload.version, payload.target)
    };
    let mut archive = ZipArchive::new(File::open(artifact_path)?)?;
    let mut expected = payload
        .files
        .iter()
        .map(|file| format!("{stem}/{}", file.path))
        .collect::<Vec<_>>();
    expected.push(format!("{stem}/release-manifest.json"));
    expected.sort();
    let mut actual = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        actual.push(entry.name().to_owned());
    }
    actual.sort();
    if actual != expected {
        return Err(DistributionError::UnexpectedArchiveEntries);
    }
    for record in &payload.files {
        let name = format!("{stem}/{}", record.path);
        let bytes = read_zip_entry_exact(&mut archive, &name, record.size)?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(DistributionError::PayloadHashMismatch(record.path.clone()));
        }
    }
    let manifest_name = format!("{stem}/release-manifest.json");
    let embedded =
        read_zip_entry_bounded(&mut archive, &manifest_name, MAX_RELEASE_MANIFEST_BYTES)?;
    let embedded: PayloadManifest = serde_json::from_slice(&embedded)?;
    if embedded != release.payload {
        return Err(DistributionError::EmbeddedManifestMismatch);
    }
    Ok(())
}

fn read_zip_entry_exact(
    archive: &mut ZipArchive<File>,
    name: &str,
    expected_size: u64,
) -> Result<Vec<u8>, DistributionError> {
    let bytes = read_zip_entry_bounded(archive, name, expected_size)?;
    if bytes.len() as u64 != expected_size {
        return Err(DistributionError::ArchiveEntrySizeMismatch(name.to_owned()));
    }
    Ok(bytes)
}

fn read_zip_entry_bounded(
    archive: &mut ZipArchive<File>,
    name: &str,
    max_size: u64,
) -> Result<Vec<u8>, DistributionError> {
    if max_size > MAX_RELEASE_ARTIFACT_BYTES {
        return Err(DistributionError::FileTooLarge);
    }
    let entry = archive.by_name(name)?;
    if entry.is_dir() || entry.size() > max_size {
        return Err(DistributionError::ArchiveEntrySizeMismatch(name.to_owned()));
    }
    let capacity = usize::try_from(entry.size()).map_err(|_| DistributionError::FileTooLarge)?;
    let mut bytes = Vec::with_capacity(capacity);
    entry
        .take(max_size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_size {
        return Err(DistributionError::ArchiveEntrySizeMismatch(name.to_owned()));
    }
    Ok(bytes)
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ActivePointer {
    cache_key: String,
}

#[derive(Clone, Debug)]
pub struct ClientFlavorArtifactStore {
    root: PathBuf,
}

impl ClientFlavorArtifactStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, DistributionError> {
        let root = root.into();
        fs::create_dir_all(root.join("entries"))?;
        fs::create_dir_all(root.join("active"))?;
        Ok(Self { root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn import_release_ready(
        &self,
        source: &Path,
    ) -> Result<ClientFlavorArtifactSummary, DistributionError> {
        self.import_cache_entry(source, true)
    }

    pub fn import_runtime_source(
        &self,
        source: &Path,
    ) -> Result<ClientFlavorArtifactSummary, DistributionError> {
        self.ensure_roots()?;
        if source.join("cache-entry.json").is_file() {
            return self.import_cache_entry(source, false);
        }
        if source.join("release-manifest.json").is_file() {
            return self.import_release_directory(source);
        }
        Err(DistributionError::UnsupportedRuntimeSource)
    }

    fn import_cache_entry(
        &self,
        source: &Path,
        require_release_ready: bool,
    ) -> Result<ClientFlavorArtifactSummary, DistributionError> {
        self.ensure_roots()?;
        let validated = validate_cache_entry_dir(source, require_release_ready)?;
        if !require_release_ready && !site_kit_distribution_eligible(&validated) {
            return Err(DistributionError::RuntimeNotDistributionEligible);
        }
        validate_flavor_id(&validated.entry.client_flavor.id)?;
        let destination = self.root.join("entries").join(&validated.entry.cache_key);
        if destination.exists() {
            let existing = validate_cache_entry_dir(&destination, false)?;
            if existing.entry != validated.entry || existing.release != validated.release {
                return Err(DistributionError::ImmutableCacheConflict);
            }
        } else {
            self.copy_entry_atomically(&validated, &destination)?;
        }
        self.write_active_pointer(
            &validated.entry.client_flavor.id,
            &validated.entry.cache_key,
        )?;
        Ok(validated.summary(true))
    }

    fn import_release_directory(
        &self,
        source: &Path,
    ) -> Result<ClientFlavorArtifactSummary, DistributionError> {
        require_directory(source, "release directory")?;
        let payload_bytes = read_bounded_regular(
            &source.join("release-manifest.json"),
            MAX_RELEASE_MANIFEST_BYTES,
        )?;
        let payload: PayloadManifest = serde_json::from_slice(&payload_bytes)?;
        if !payload.unsigned {
            return Err(DistributionError::SignedReleaseDirectoryRequiresNativeVerification);
        }
        let client_flavor = payload
            .client_flavor
            .clone()
            .ok_or(DistributionError::ReleaseClientFlavorMissing)?;
        let cache_key = client_flavor_cache_key(&client_flavor, &payload)?;
        validate_cache_key(&cache_key)?;
        validate_flavor_id(&client_flavor.id)?;
        let platform = ReleasePlatform::from_target(&payload.target)?;
        let release_ready = match platform {
            ReleasePlatform::Linux => payload.unsigned,
            ReleasePlatform::Windows | ReleasePlatform::Macos => !payload.unsigned,
        };
        let unsigned_zero_major = payload.unsigned
            && payload.signing.is_none()
            && payload.version.split('.').next() == Some("0");
        if !release_ready && !unsigned_zero_major {
            return Err(DistributionError::RuntimeNotDistributionEligible);
        }
        let destination = self.root.join("entries").join(&cache_key);
        if destination.exists() {
            let existing = validate_cache_entry_dir(&destination, false)?;
            if existing.release.payload != payload {
                return Err(DistributionError::ImmutableCacheConflict);
            }
            self.write_active_pointer(&client_flavor.id, &cache_key)?;
            return Ok(existing.summary(true));
        }

        let stem = if payload.unsigned {
            format!("clew-v{}-{}", payload.version, payload.target)
        } else {
            format!("clew-v{}-{}-signed", payload.version, payload.target)
        };
        let artifact_file = format!("{stem}.zip");
        let manifest_file = format!("{stem}.release.json");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let staging_parent = self.root.join("entries").join(format!(
            ".staging.{}.{}.tmp",
            std::process::id(),
            nonce
        ));
        fs::create_dir(&staging_parent)?;
        let staging = staging_parent.join(&cache_key);
        fs::create_dir(&staging)?;
        let artifact_path = staging.join(&artifact_file);
        let result = (|| {
            write_release_archive_from_directory(
                source,
                &artifact_path,
                &stem,
                &payload,
                &payload_bytes,
            )?;
            let artifact = ArtifactInfo {
                file: artifact_file.clone(),
                size: fs::metadata(&artifact_path)?.len(),
                sha256: sha256_file(&artifact_path)?,
            };
            let release = ArtifactManifest {
                payload: payload.clone(),
                artifact,
            };
            let release_bytes = serde_json::to_vec_pretty(&release)?;
            fs::write(staging.join(&manifest_file), &release_bytes)?;
            let entry = ClientFlavorCacheEntry {
                schema_version: CLIENT_FLAVOR_CACHE_SCHEMA_VERSION,
                cache_key: cache_key.clone(),
                client_flavor: client_flavor.clone(),
                version: payload.version.clone(),
                target: payload.target.clone(),
                profile: payload.profile.clone(),
                source_commit: payload.source_commit.clone(),
                release_ready,
                signing: payload.signing.clone(),
                artifact_file: artifact_file.clone(),
                artifact_sha256: release.artifact.sha256.clone(),
                manifest_file: manifest_file.clone(),
                manifest_sha256: sha256_bytes(&release_bytes),
            };
            fs::write(
                staging.join("cache-entry.json"),
                serde_json::to_vec_pretty(&entry)?,
            )?;
            let validated = validate_cache_entry_dir(&staging, false)?;
            if validated.entry != entry || validated.release != release {
                return Err(DistributionError::CopyVerificationFailed);
            }
            fs::rename(&staging, &destination).map_err(DistributionError::PublishCacheEntry)?;
            Ok(validated)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_parent);
        } else {
            let _ = fs::remove_dir(&staging_parent);
        }
        let validated = result?;
        self.write_active_pointer(&client_flavor.id, &cache_key)?;
        Ok(validated.summary(true))
    }

    pub fn list(&self) -> Result<Vec<ClientFlavorArtifactSummary>, DistributionError> {
        self.ensure_roots()?;
        let entries_root = self.root.join("entries");
        let mut entries = fs::read_dir(&entries_root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        if entries.len() > MAX_STORED_CLIENT_FLAVORS {
            return Err(DistributionError::TooManyStoredClientFlavors);
        }
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let metadata = entry.file_type()?;
            if !metadata.is_dir() || metadata.is_symlink() {
                return Err(DistributionError::UnsafeStoreEntry);
            }
            let validated = validate_cache_entry_dir(&entry.path(), false)?;
            let active = self
                .read_active_key(&validated.entry.client_flavor.id)?
                .as_deref()
                == Some(validated.entry.cache_key.as_str());
            output.push(validated.summary(active));
        }
        Ok(output)
    }

    pub fn active_for_flavor(
        &self,
        client_flavor_id: &str,
    ) -> Result<Option<ValidatedClientFlavorArtifact>, DistributionError> {
        self.ensure_roots()?;
        validate_flavor_id(client_flavor_id)?;
        let Some(key) = self.read_active_key(client_flavor_id)? else {
            return Ok(None);
        };
        let artifact = validate_cache_entry_dir(&self.root.join("entries").join(&key), false)?;
        if artifact.entry.client_flavor.id != client_flavor_id {
            return Err(DistributionError::ActivePointerFlavorMismatch);
        }
        Ok(Some(artifact))
    }

    fn ensure_roots(&self) -> Result<(), DistributionError> {
        fs::create_dir_all(self.root.join("entries"))?;
        fs::create_dir_all(self.root.join("active"))?;
        Ok(())
    }

    fn copy_entry_atomically(
        &self,
        source: &ValidatedClientFlavorArtifact,
        destination: &Path,
    ) -> Result<(), DistributionError> {
        let entries_root = self.root.join("entries");
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let staging_parent =
            entries_root.join(format!(".staging.{}.{}.tmp", std::process::id(), nonce));
        if staging_parent.exists() {
            fs::remove_dir_all(&staging_parent)?;
        }
        fs::create_dir(&staging_parent)?;
        let staging = staging_parent.join(&source.entry.cache_key);
        fs::create_dir(&staging)?;
        for name in [
            "cache-entry.json",
            source.entry.manifest_file.as_str(),
            source.entry.artifact_file.as_str(),
        ] {
            let input = source.root.join(name);
            regular_metadata(&input)?;
            let output = staging.join(name);
            fs::copy(&input, &output)?;
            let file = fs::OpenOptions::new().write(true).open(&output)?;
            file.sync_all()?;
        }
        let copied = validate_cache_entry_dir(&staging, false)?;
        if copied.entry != source.entry || copied.release != source.release {
            let _ = fs::remove_dir_all(&staging_parent);
            return Err(DistributionError::CopyVerificationFailed);
        }
        fs::rename(&staging, destination).map_err(DistributionError::PublishCacheEntry)?;
        let _ = fs::remove_dir(&staging_parent);
        Ok(())
    }

    fn write_active_pointer(
        &self,
        flavor_id: &str,
        cache_key: &str,
    ) -> Result<(), DistributionError> {
        validate_flavor_id(flavor_id)?;
        validate_cache_key(cache_key)?;
        let active_root = self.root.join("active");
        let target = active_root.join(format!("{flavor_id}.json"));
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temp = active_root.join(format!(".{flavor_id}.{}.{}.tmp", std::process::id(), nonce));
        let bytes = serde_json::to_vec_pretty(&ActivePointer {
            cache_key: cache_key.to_owned(),
        })?;
        fs::write(&temp, bytes)?;
        fs::OpenOptions::new().write(true).open(&temp)?.sync_all()?;
        if target.exists() {
            fs::remove_file(&target)?;
        }
        fs::rename(&temp, target).map_err(DistributionError::PublishActivePointer)?;
        Ok(())
    }

    fn read_active_key(&self, flavor_id: &str) -> Result<Option<String>, DistributionError> {
        validate_flavor_id(flavor_id)?;
        let path = self.root.join("active").join(format!("{flavor_id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let pointer: ActivePointer = read_json_bounded(&path, 4096)?;
        validate_cache_key(&pointer.cache_key)?;
        Ok(Some(pointer.cache_key))
    }
}

fn require_directory(path: &Path, label: &'static str) -> Result<(), DistributionError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(DistributionError::UnsafePath(label));
    }
    Ok(())
}

fn regular_metadata(path: &Path) -> Result<fs::Metadata, DistributionError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DistributionError::UnsafePath("regular file"));
    }
    Ok(metadata)
}

fn read_bounded_regular(path: &Path, max: u64) -> Result<Vec<u8>, DistributionError> {
    let metadata = regular_metadata(path)?;
    if metadata.len() > max {
        return Err(DistributionError::FileTooLarge);
    }
    Ok(fs::read(path)?)
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(
    path: &Path,
    max: u64,
) -> Result<T, DistributionError> {
    Ok(serde_json::from_slice(&read_bounded_regular(path, max)?)?)
}

fn validate_client_flavor(value: &ReleaseClientFlavorInfo) -> Result<(), DistributionError> {
    validate_flavor_id(&value.id)?;
    validate_bounded_text(&value.outfit_id, 128, "Outfit id")?;
    if value.outfit_revision == 0 {
        return Err(DistributionError::InvalidBoundedText("Outfit revision"));
    }
    validate_outfit_build_cache_key(&value.build_cache_key)?;
    validate_bounded_text(&value.app_display_name, 256, "app display name")?;
    if let Some(publisher) = &value.publisher_label {
        validate_bounded_text(publisher, 256, "publisher label")?;
    }
    if !matches!(value.icon_format.as_str(), "png" | "svg") {
        return Err(DistributionError::InvalidBoundedText("icon format"));
    }
    if let Some(asset_id) = &value.icon_asset_id {
        let Some(hash) = asset_id.strip_prefix("sha256-") else {
            return Err(DistributionError::InvalidBoundedText("icon asset id"));
        };
        validate_sha256(hash)?;
    }
    Ok(())
}

fn validate_outfit_build_cache_key(value: &str) -> Result<(), DistributionError> {
    let Some(hash) = value.strip_prefix("outfit-v1-") else {
        return Err(DistributionError::InvalidBoundedText(
            "Outfit build cache key",
        ));
    };
    validate_sha256(hash)
}

fn validate_bounded_text(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), DistributionError> {
    if value.is_empty()
        || value.len() > max
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(DistributionError::InvalidBoundedText(field));
    }
    Ok(())
}

fn validate_target_text(value: &str) -> Result<(), DistributionError> {
    validate_bounded_text(value, 128, "target")?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DistributionError::InvalidTarget);
    }
    Ok(())
}

fn validate_profile_text(value: &str) -> Result<(), DistributionError> {
    validate_bounded_text(value, 64, "profile")?;
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DistributionError::InvalidBoundedText("profile"));
    }
    Ok(())
}

fn validate_git_commit(value: &str) -> Result<(), DistributionError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DistributionError::InvalidBoundedText("source commit"));
    }
    Ok(())
}

fn validate_basename(value: &str) -> Result<(), DistributionError> {
    if value.is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(DistributionError::UnsafeFileName(value.to_owned()));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), DistributionError> {
    if value.is_empty()
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('\\')
        || value.contains('\0')
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(DistributionError::UnsafeArchivePath(value.to_owned()));
    }
    Ok(())
}

fn validate_cache_key(value: &str) -> Result<(), DistributionError> {
    let Some(hash) = value.strip_prefix("client-flavor-v1-") else {
        return Err(DistributionError::InvalidCacheKey);
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DistributionError::InvalidCacheKey);
    }
    Ok(())
}

fn validate_flavor_id(value: &str) -> Result<(), DistributionError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DistributionError::InvalidClientFlavorId);
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), DistributionError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DistributionError::InvalidSha256);
    }
    Ok(())
}

fn target_arch(target: &str) -> Result<&str, DistributionError> {
    target
        .split('-')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or(DistributionError::InvalidTarget)
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex(&digest)
}

pub fn sha256_file(path: &Path) -> Result<String, DistributionError> {
    let metadata = regular_metadata(path)?;
    if metadata.len() > MAX_RELEASE_ARTIFACT_BYTES {
        return Err(DistributionError::FileTooLarge);
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
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

fn site_kit_distribution_eligible(artifact: &ValidatedClientFlavorArtifact) -> bool {
    artifact.entry.release_ready
        || (artifact.release.payload.unsigned
            && artifact.release.payload.signing.is_none()
            && artifact.entry.version.split('.').next() == Some("0"))
}

fn write_release_archive_from_directory(
    source: &Path,
    output: &Path,
    stem: &str,
    payload: &PayloadManifest,
    payload_bytes: &[u8],
) -> Result<(), DistributionError> {
    let file = File::create(output)?;
    let mut archive = ZipWriter::new(file);
    for record in &payload.files {
        validate_relative_path(&record.path)?;
        let source_path = source.join(&record.path);
        let bytes = read_bounded_regular(&source_path, record.size)?;
        if bytes.len() as u64 != record.size || sha256_bytes(&bytes) != record.sha256 {
            return Err(DistributionError::PayloadHashMismatch(record.path.clone()));
        }
        let mode = u32::from_str_radix(&record.mode, 8)
            .map_err(|_| DistributionError::InvalidPayloadFileSet)?;
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(mode);
        archive.start_file(format!("{stem}/{}", record.path), options)?;
        archive.write_all(&bytes)?;
    }
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(0o644);
    archive.start_file(format!("{stem}/release-manifest.json"), options)?;
    archive.write_all(payload_bytes)?;
    let file = archive.finish()?;
    file.sync_all()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum DistributionError {
    #[error("Site Kit assembly failed: {0}")]
    SiteKit(String),
    #[error("Site Kit artifact platform {artifact:?} requires native assembly on {native:?}")]
    NativeAssemblyRequired {
        artifact: ReleasePlatform,
        native: ReleasePlatform,
    },
    #[error("Site Kit output already exists: {0}")]
    OutputAlreadyExists(PathBuf),
    #[error("native Site Kit tool failed during {tool}: {status}")]
    NativeToolFailed { tool: &'static str, status: String },
    #[error("could not publish verified ClientFlavor cache entry: {0}")]
    PublishCacheEntry(std::io::Error),
    #[error("could not publish active ClientFlavor pointer: {0}")]
    PublishActivePointer(std::io::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("clock error: {0}")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("unsupported release target: {0}")]
    UnsupportedTarget(String),
    #[error("invalid bounded distribution field: {0}")]
    InvalidBoundedText(&'static str),
    #[error("invalid release target")]
    InvalidTarget,
    #[error(
        "runtime is not eligible for Site Kit distribution; unsigned runtimes are allowed only for 0.x releases"
    )]
    RuntimeNotDistributionEligible,
    #[error(
        "signed extracted release directories require the native-verified ClientFlavor cache path"
    )]
    SignedReleaseDirectoryRequiresNativeVerification,
    #[error("runtime source is neither a ClientFlavor cache entry nor an extracted Clew release")]
    UnsupportedRuntimeSource,
    #[error("release does not contain ClientFlavor metadata")]
    ReleaseClientFlavorMissing,
    #[error("ClientFlavor cache directory name is invalid")]
    InvalidCacheDirectoryName,
    #[error("unsupported ClientFlavor cache schema {0}")]
    UnsupportedCacheSchema(u32),
    #[error("unsupported Site Kit launcher schema {0}")]
    UnsupportedLauncherSchema(u32),
    #[error("invalid ClientFlavor cache key")]
    InvalidCacheKey,
    #[error("invalid ClientFlavor id")]
    InvalidClientFlavorId,
    #[error("invalid SHA-256 value")]
    InvalidSha256,
    #[error("unsafe filename: {0}")]
    UnsafeFileName(String),
    #[error("unsafe archive path: {0}")]
    UnsafeArchivePath(String),
    #[error("unsafe path for {0}")]
    UnsafePath(&'static str),
    #[error("file exceeds distribution safety bound")]
    FileTooLarge,
    #[error("ClientFlavor cache directory does not match cache key")]
    CacheDirectoryKeyMismatch,
    #[error("ClientFlavor is not release-ready")]
    NotReleaseReady,
    #[error("release manifest hash differs from cache metadata")]
    ManifestHashMismatch,
    #[error("release artifact differs from cache metadata")]
    ArtifactMismatch,
    #[error("release metadata differs from cache entry")]
    ReleaseMetadataMismatch,
    #[error("semantic ClientFlavor cache key differs from cache metadata")]
    SemanticCacheKeyMismatch,
    #[error("Site Kit launcher path is absent from release payload")]
    LauncherMissingFromPayload,
    #[error("Site Kit launcher executable is outside its declared bundle root")]
    LauncherBundleMismatch,
    #[error("release signing evidence does not match its platform")]
    InvalidPlatformSigningEvidence,
    #[error("invalid release signing state")]
    InvalidSigningState,
    #[error("release_ready does not match platform/signing policy")]
    InvalidReleaseReadyState,
    #[error("release payload file set is invalid")]
    InvalidPayloadFileSet,
    #[error("release archive contains unexpected entries")]
    UnexpectedArchiveEntries,
    #[error("release archive entry size differs for {0}")]
    ArchiveEntrySizeMismatch(String),
    #[error("release payload hash differs for {0}")]
    PayloadHashMismatch(String),
    #[error("embedded release manifest differs from sidecar")]
    EmbeddedManifestMismatch,
    #[error("immutable ClientFlavor cache entry conflicts with existing content")]
    ImmutableCacheConflict,
    #[error("copied ClientFlavor cache entry failed verification")]
    CopyVerificationFailed,
    #[error("too many stored ClientFlavor artifacts")]
    TooManyStoredClientFlavors,
    #[error("ClientFlavor store contains an unsafe entry")]
    UnsafeStoreEntry,
    #[error("active ClientFlavor pointer targets a different flavor")]
    ActivePointerFlavorMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_cache_fixture(root: &Path) -> (PathBuf, ClientFlavorCacheEntry) {
        use std::io::Write as _;
        use zip::{CompressionMethod, DateTime, ZipWriter, write::SimpleFileOptions};

        let flavor = ReleaseClientFlavorInfo {
            id: "sha256-0123abcd".into(),
            outfit_id: "clew-original".into(),
            outfit_revision: 1,
            build_cache_key: format!("outfit-v1-{}", "4".repeat(64)),
            app_display_name: "Clew".into(),
            publisher_label: None,
            icon_format: "svg".into(),
            icon_asset_id: None,
        };
        let binary = b"elf".to_vec();
        let payload = PayloadManifest {
            schema_version: RELEASE_SCHEMA_VERSION,
            product: "clew".into(),
            version: "0.1.0".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "release".into(),
            archive_format: "zip".into(),
            layout: "linux-portable".into(),
            app_id: "io.clew.app".into(),
            entrypoint: "bin/clew".into(),
            cli_binary: "bin/clew".into(),
            source_commit: "1".repeat(40),
            source_date_epoch: 1,
            rustc: ToolchainInfo {
                release: "1.96.0".into(),
                commit_hash: "2".repeat(40),
                host: "x86_64-unknown-linux-gnu".into(),
                llvm_version: "22.1.0".into(),
            },
            cargo_lock_sha256: "3".repeat(64),
            dirty: false,
            unsigned: true,
            signing: None,
            client_flavor: Some(flavor.clone()),
            site_kit_launcher: Some(SiteKitLauncherInfo {
                schema_version: SITE_KIT_LAUNCHER_SCHEMA_VERSION,
                executable_path: "bin/clew".into(),
                bundle_root: None,
            }),
            files: vec![PayloadFile {
                path: "bin/clew".into(),
                size: binary.len() as u64,
                sha256: sha256_bytes(&binary),
                mode: "0755".into(),
            }],
        };
        let cache_key = client_flavor_cache_key(&flavor, &payload).unwrap();
        let entry_root = root.join(&cache_key);
        fs::create_dir_all(&entry_root).unwrap();
        let stem = "clew-v0.1.0-x86_64-unknown-linux-gnu";
        let artifact_file = format!("{stem}.zip");
        let artifact_path = entry_root.join(&artifact_file);
        let file = File::create(&artifact_path).unwrap();
        let mut zip = ZipWriter::new(file);
        let executable = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(0o755);
        zip.start_file(format!("{stem}/bin/clew"), executable)
            .unwrap();
        zip.write_all(&binary).unwrap();
        let regular = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(0o644);
        zip.start_file(format!("{stem}/release-manifest.json"), regular)
            .unwrap();
        zip.write_all(&serde_json::to_vec_pretty(&payload).unwrap())
            .unwrap();
        zip.finish().unwrap();
        let artifact = ArtifactInfo {
            file: artifact_file.clone(),
            size: fs::metadata(&artifact_path).unwrap().len(),
            sha256: sha256_file(&artifact_path).unwrap(),
        };
        let release = ArtifactManifest {
            payload,
            artifact: artifact.clone(),
        };
        let manifest_file = format!("{stem}.release.json");
        let manifest_path = entry_root.join(&manifest_file);
        fs::write(&manifest_path, serde_json::to_vec_pretty(&release).unwrap()).unwrap();
        let entry = ClientFlavorCacheEntry {
            schema_version: CLIENT_FLAVOR_CACHE_SCHEMA_VERSION,
            cache_key,
            client_flavor: flavor,
            version: "0.1.0".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            profile: "release".into(),
            source_commit: "1".repeat(40),
            release_ready: true,
            signing: None,
            artifact_file,
            artifact_sha256: artifact.sha256,
            manifest_file,
            manifest_sha256: sha256_file(&manifest_path).unwrap(),
        };
        fs::write(
            entry_root.join("cache-entry.json"),
            serde_json::to_vec_pretty(&entry).unwrap(),
        )
        .unwrap();
        (entry_root, entry)
    }

    #[test]
    fn release_ready_cache_import_is_immutable_and_active() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let (entry_root, entry) = linux_cache_fixture(source.path());
        let validated = validate_cache_entry_dir(&entry_root, true).unwrap();
        assert_eq!(validated.entry, entry);
        assert_eq!(validated.platform, ReleasePlatform::Linux);

        let store = ClientFlavorArtifactStore::new(store_root.path()).unwrap();
        let imported = store.import_release_ready(&entry_root).unwrap();
        assert_eq!(imported.cache_key, entry.cache_key);
        assert!(imported.active);
        assert_eq!(store.list().unwrap(), vec![imported.clone()]);
        assert_eq!(
            store
                .active_for_flavor(&entry.client_flavor.id)
                .unwrap()
                .unwrap()
                .entry,
            entry
        );
        assert_eq!(store.import_release_ready(&entry_root).unwrap(), imported);
    }

    #[test]
    fn site_kit_runtime_policy_allows_release_ready_or_unsigned_zero_major_only() {
        let source = tempfile::tempdir().unwrap();
        let (entry_root, _) = linux_cache_fixture(source.path());
        let mut artifact = validate_cache_entry_dir(&entry_root, true).unwrap();
        assert!(site_kit_distribution_eligible(&artifact));

        artifact.entry.release_ready = false;
        artifact.entry.version = "0.2.0".into();
        artifact.release.payload.version = "0.2.0".into();
        artifact.release.payload.unsigned = true;
        artifact.release.payload.signing = None;
        assert!(site_kit_distribution_eligible(&artifact));

        artifact.entry.version = "1.0.0".into();
        artifact.release.payload.version = "1.0.0".into();
        assert!(!site_kit_distribution_eligible(&artifact));
    }

    #[test]
    fn client_flavor_summary_defaults_legacy_launcher_schema_to_zero() {
        let source = tempfile::tempdir().unwrap();
        let (entry_root, _) = linux_cache_fixture(source.path());
        let artifact = validate_cache_entry_dir(&entry_root, true).unwrap();
        let summary = artifact.summary(true);
        let mut value = serde_json::to_value(&summary).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("site_kit_launcher_schema");
        let decoded: ClientFlavorArtifactSummary = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.site_kit_launcher_schema, 0);
    }

    #[test]
    fn extracted_signed_release_requires_native_verified_cache_path() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let extracted = tempfile::tempdir().unwrap();
        let (entry_root, entry) = linux_cache_fixture(source.path());
        let mut release: ArtifactManifest =
            serde_json::from_slice(&fs::read(entry_root.join(&entry.manifest_file)).unwrap())
                .unwrap();
        release.payload.unsigned = false;
        fs::write(
            extracted.path().join("release-manifest.json"),
            serde_json::to_vec_pretty(&release.payload).unwrap(),
        )
        .unwrap();
        let store = ClientFlavorArtifactStore::new(store_root.path()).unwrap();
        assert!(matches!(
            store.import_runtime_source(extracted.path()),
            Err(DistributionError::SignedReleaseDirectoryRequiresNativeVerification)
        ));
    }

    #[test]
    fn extracted_release_import_is_verified_cached_and_active() {
        let source = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let extracted = tempfile::tempdir().unwrap();
        let (entry_root, entry) = linux_cache_fixture(source.path());
        let release: ArtifactManifest =
            serde_json::from_slice(&fs::read(entry_root.join(&entry.manifest_file)).unwrap())
                .unwrap();
        fs::create_dir_all(extracted.path().join("bin")).unwrap();
        fs::write(extracted.path().join("bin/clew"), b"elf").unwrap();
        fs::write(
            extracted.path().join("release-manifest.json"),
            serde_json::to_vec_pretty(&release.payload).unwrap(),
        )
        .unwrap();

        let store = ClientFlavorArtifactStore::new(store_root.path()).unwrap();
        let imported = store.import_runtime_source(extracted.path()).unwrap();
        assert_eq!(imported.cache_key, entry.cache_key);
        assert!(imported.active);
        assert_eq!(imported.client_flavor_id, entry.client_flavor.id);
        let active = store
            .active_for_flavor(&entry.client_flavor.id)
            .unwrap()
            .unwrap();
        assert_eq!(active.release.payload, release.payload);
        assert_eq!(store.list().unwrap(), vec![imported]);
    }

    #[test]
    fn cache_import_rejects_payload_tamper() {
        let source = tempfile::tempdir().unwrap();
        let (entry_root, entry) = linux_cache_fixture(source.path());
        let artifact = entry_root.join(&entry.artifact_file);
        fs::write(&artifact, b"tampered").unwrap();
        assert!(matches!(
            validate_cache_entry_dir(&entry_root, true),
            Err(DistributionError::ArtifactMismatch)
        ));
    }

    #[test]
    fn cache_key_validation_is_strict() {
        assert!(validate_cache_key(&format!("client-flavor-v1-{}", "a".repeat(64))).is_ok());
        assert!(validate_cache_key("client-flavor-v1-nope").is_err());
        assert!(validate_flavor_id("sha256-0123abcd").is_ok());
        assert!(validate_flavor_id("../escape").is_err());
    }

    #[test]
    fn release_platform_is_target_derived() {
        assert_eq!(
            ReleasePlatform::from_target("x86_64-pc-windows-msvc").unwrap(),
            ReleasePlatform::Windows
        );
        assert_eq!(
            ReleasePlatform::from_target("aarch64-apple-darwin").unwrap(),
            ReleasePlatform::Macos
        );
        assert_eq!(
            ReleasePlatform::from_target("x86_64-unknown-linux-gnu").unwrap(),
            ReleasePlatform::Linux
        );
        assert!(ReleasePlatform::from_target("wasm32-unknown-unknown").is_err());
    }
}
