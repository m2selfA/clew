use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fs::{self, File},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::{Parser, Subcommand};
use clew_host::{
    ClientFlavor, MAX_OUTFIT_BUILD_SPEC_BYTES, OutfitAssetRef, OutfitBuildSpec, OutfitPreset,
    OutfitProfile, SignedSiteClew, SiteKitContract, TargetPlatform, verify_outfit_asset_bytes,
};
use flate2::{Compression, write::GzEncoder};
use image::{DynamicImage, ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

const PRODUCT: &str = "clew";
const APP_ID: &str = "io.clew.app";
const RELEASE_SCHEMA_VERSION: u32 = 2;
const SIGNED_RELEASE_SCHEMA_VERSION: u32 = 3;
const MAX_EMBEDDED_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIGNED_PAYLOAD_FILES: usize = 128;
const MAX_SIGNED_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SIGNED_PAYLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MACOS_CODE_RESOURCES: &str = "Clew.app/Contents/_CodeSignature/CodeResources";
const CLIENT_FLAVOR_CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_CLIENT_FLAVOR_CACHE_ENTRY_BYTES: u64 = 64 * 1024;
const SITE_KIT_LAUNCHER_SCHEMA_VERSION: u32 = 1;
const MACOS_ROLE_APP: &str = "Clew Role.app";
const MACOS_ROLE_CODE_RESOURCES: &str = "Clew Role.app/Contents/_CodeSignature/CodeResources";

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Clew repository maintenance tasks")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Build and package one unsigned native/cross-target Clew portable artifact.
    Package {
        /// Rust target triple. Defaults to rustc's host triple.
        #[arg(long)]
        target: Option<String>,
        /// Cargo profile used for the Clew binary.
        #[arg(long, default_value = "release")]
        profile: String,
        /// Output directory for archive, manifest, and SHA256SUMS.
        #[arg(long, default_value = "dist")]
        out_dir: PathBuf,
        /// Package an already-built binary instead of invoking cargo build.
        #[arg(long)]
        no_build: bool,
        /// Allow a dirty tracked worktree. The manifest records dirty=true.
        #[arg(long)]
        allow_dirty: bool,
        /// Skip native --version/--help execution smoke.
        #[arg(long)]
        skip_smoke: bool,
        /// Secret-free Outfit build export produced by `clew outfit export-build`.
        #[arg(long, value_name = "DIR")]
        outfit_build: Option<PathBuf>,
    },
    /// Sign an existing clean unsigned Clew package without mutating build outputs.
    SignPackage {
        /// Sidecar .release.json produced by `cargo xtask package`.
        #[arg(long)]
        manifest: PathBuf,
        /// Output directory for the signed archive, manifest, and SHA256SUMS.
        #[arg(long, default_value = "dist-signed")]
        out_dir: PathBuf,
        #[command(subcommand)]
        signer: Signer,
    },
    /// Independently verify an unsigned or signed Clew release artifact.
    VerifyPackage {
        /// Sidecar .release.json belonging to the artifact being verified.
        #[arg(long)]
        manifest: PathBuf,
        /// Optional explicit signtool.exe path for signed Windows verification.
        #[arg(long)]
        signtool: Option<PathBuf>,
    },
    /// Verify and publish one reusable ClientFlavor artifact into a content-checked cache.
    CacheClientFlavor {
        /// Sidecar .release.json belonging to the artifact being cached.
        #[arg(long)]
        manifest: PathBuf,
        /// Cache root. Each semantic ClientFlavor/signing identity gets one immutable entry.
        #[arg(long, default_value = "dist-client-flavors")]
        cache_dir: PathBuf,
        /// Optional explicit signtool.exe path for signed Windows verification.
        #[arg(long)]
        signtool: Option<PathBuf>,
        /// Permit clean unsigned Windows/macOS artifacts for release-pipeline rehearsal only.
        #[arg(long)]
        allow_unsigned_rehearsal: bool,
    },
    /// Assemble one friend-facing Site Kit from an immutable ClientFlavor cache entry and signed site.clew.
    AssembleSiteKit {
        /// Immutable ClientFlavor cache entry created by `cache-client-flavor`.
        #[arg(long, value_name = "DIR")]
        cache_entry: PathBuf,
        /// Human-readable Site label used only for the outer archive filename.
        #[arg(long, value_name = "NAME")]
        site_label: String,
        /// Signed site.clew plus any sibling outfit-assets/ exported by the Controller.
        #[arg(long, value_name = "FILE")]
        site: PathBuf,
        /// Output directory for the Site Kit archive and sidecar manifest.
        #[arg(long, default_value = "dist-site-kits")]
        out_dir: PathBuf,
        /// Permit an unsigned Windows/macOS cache entry for rehearsal only.
        #[arg(long)]
        allow_unsigned_rehearsal: bool,
    },
}

#[derive(Debug, Subcommand)]
enum Signer {
    /// Authenticode-sign the Windows executable using a certificate already in an OS store.
    Windows {
        /// SHA-1 thumbprint of the code-signing certificate. No PFX password is accepted here.
        #[arg(long)]
        cert_sha1: String,
        /// RFC3161 timestamp service URL passed to SignTool /tr.
        #[arg(long)]
        timestamp_url: String,
        /// Optional explicit signtool.exe path. Otherwise PATH and Windows Kits are searched.
        #[arg(long)]
        signtool: Option<PathBuf>,
        /// Select the LocalMachine certificate store instead of CurrentUser.
        #[arg(long)]
        machine_store: bool,
    },
    /// Developer ID-sign, notarize, staple, and verify the macOS app bundle.
    Macos {
        /// Exact Developer ID Application identity accepted by codesign.
        #[arg(long)]
        identity: String,
        /// notarytool keychain profile name; credentials stay in the macOS Keychain.
        #[arg(long)]
        notary_profile: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleasePlatform {
    Windows,
    Macos,
    Linux,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchiveFile {
    path: String,
    bytes: Vec<u8>,
    mode: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PackageLayout {
    name: String,
    app_id: String,
    entrypoint: String,
    cli_binary: String,
    files: Vec<ArchiveFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct PayloadFile {
    path: String,
    size: u64,
    sha256: String,
    mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ToolchainInfo {
    release: String,
    commit_hash: String,
    host: String,
    llvm_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct SigningInfo {
    mechanism: String,
    identity: String,
    timestamped: bool,
    notarized: bool,
    stapled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notary_submission_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ReleaseClientFlavorInfo {
    id: String,
    outfit_id: String,
    outfit_revision: u32,
    build_cache_key: String,
    app_display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publisher_label: Option<String>,
    icon_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon_asset_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct SiteKitLauncherInfo {
    schema_version: u32,
    executable_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle_root: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildIconFormat {
    Svg,
    Png,
}

impl BuildIconFormat {
    const fn label(self) -> &'static str {
        match self {
            Self::Svg => "svg",
            Self::Png => "png",
        }
    }
}

#[derive(Clone, Debug)]
struct BuildBranding {
    profile: OutfitProfile,
    build_cache_key: String,
    icon_bytes: Vec<u8>,
    icon_path: PathBuf,
    icon_format: BuildIconFormat,
    icon_asset_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct PayloadManifest {
    schema_version: u32,
    product: String,
    version: String,
    target: String,
    profile: String,
    archive_format: String,
    layout: String,
    app_id: String,
    entrypoint: String,
    cli_binary: String,
    source_commit: String,
    source_date_epoch: u64,
    rustc: ToolchainInfo,
    cargo_lock_sha256: String,
    dirty: bool,
    unsigned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signing: Option<SigningInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_flavor: Option<ReleaseClientFlavorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    site_kit_launcher: Option<SiteKitLauncherInfo>,
    files: Vec<PayloadFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ArtifactInfo {
    file: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ArtifactManifest {
    payload: PayloadManifest,
    artifact: ArtifactInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct ClientFlavorCacheEntry {
    schema_version: u32,
    cache_key: String,
    client_flavor: ReleaseClientFlavorInfo,
    version: String,
    target: String,
    profile: String,
    source_commit: String,
    release_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signing: Option<SigningInfo>,
    artifact_file: String,
    artifact_sha256: String,
    manifest_file: String,
    manifest_sha256: String,
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

fn client_flavor_cache_key(
    client_flavor: &ReleaseClientFlavorInfo,
    payload: &PayloadManifest,
) -> Result<String, Box<dyn Error>> {
    let (signing_mechanism, signing_identity) = payload
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
        signing_mechanism,
        signing_identity,
    };
    Ok(format!(
        "client-flavor-v1-{}",
        sha256_bytes(&serde_json::to_vec(&material)?)
    ))
}

const SITE_KIT_SCHEMA_VERSION: u32 = 1;
const USE_ROLE_DIR: &str = "1 Use this computer";
const HELPER_ROLE_DIR: &str = "2 Help nearby computers";
const ROLE_HINT_FILE: &str = "role-hint.clew";

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct SiteKitPayloadManifest {
    schema_version: u32,
    source_cache_key: String,
    client_flavor: ReleaseClientFlavorInfo,
    target: String,
    source_release_sha256: String,
    site_sha256: String,
    runtime_release_ready: bool,
    files: Vec<PayloadFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
struct SiteKitArtifactManifest {
    payload: SiteKitPayloadManifest,
    artifact: ArtifactInfo,
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let repo = repo_root()?;
    match cli.command {
        Task::Package {
            target,
            profile,
            out_dir,
            no_build,
            allow_dirty,
            skip_smoke,
            outfit_build,
        } => package(
            &repo,
            target,
            &profile,
            &out_dir,
            no_build,
            allow_dirty,
            skip_smoke,
            outfit_build.as_deref(),
        )?,
        Task::SignPackage {
            manifest,
            out_dir,
            signer,
        } => sign_package(&repo, &manifest, &out_dir, signer)?,
        Task::VerifyPackage { manifest, signtool } => {
            verify_package(&repo, &manifest, signtool.as_deref())?
        }
        Task::CacheClientFlavor {
            manifest,
            cache_dir,
            signtool,
            allow_unsigned_rehearsal,
        } => cache_client_flavor(
            &repo,
            &manifest,
            &cache_dir,
            signtool.as_deref(),
            allow_unsigned_rehearsal,
        )?,
        Task::AssembleSiteKit {
            cache_entry,
            site_label,
            site,
            out_dir,
            allow_unsigned_rehearsal,
        } => assemble_site_kit(
            &repo,
            &cache_entry,
            &site_label,
            &site,
            &out_dir,
            allow_unsigned_rehearsal,
        )?,
    }
    Ok(())
}

fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask manifest has no repository parent".into())
}

fn assemble_site_kit(
    repo: &Path,
    cache_entry: &Path,
    site_label: &str,
    site: &Path,
    out_dir: &Path,
    allow_unsigned_rehearsal: bool,
) -> Result<(), Box<dyn Error>> {
    let cache_root = project_path(repo, cache_entry);
    let entry_path = cache_root.join("cache-entry.json");
    let metadata = fs::metadata(&entry_path)?;
    if !metadata.is_file() || metadata.len() > MAX_CLIENT_FLAVOR_CACHE_ENTRY_BYTES {
        return Err("Site Kit ClientFlavor cache entry is not bounded metadata".into());
    }
    let cache: ClientFlavorCacheEntry = serde_json::from_slice(&fs::read(&entry_path)?)?;
    if cache.schema_version != CLIENT_FLAVOR_CACHE_SCHEMA_VERSION {
        return Err("unsupported ClientFlavor cache schema for Site Kit assembly".into());
    }
    verify_client_flavor_cache_entry(&cache_root, &cache)?;
    if !cache.release_ready && !allow_unsigned_rehearsal {
        return Err("Site Kit assembly requires a release-ready ClientFlavor; unsigned Windows/macOS is rehearsal-only".into());
    }

    let release_manifest_path = cache_root.join(&cache.manifest_file);
    let release = read_artifact_manifest(&release_manifest_path)?;
    if release.payload.dirty {
        return Err("dirty release artifacts cannot be assembled into Site Kits".into());
    }
    let signed = !release.payload.unsigned;
    let platform = if signed {
        validate_signed_artifact(&release)?
    } else {
        validate_unsigned_artifact(&release)?
    };
    let expected_release_ready = match platform {
        ReleasePlatform::Linux => !signed,
        ReleasePlatform::Windows | ReleasePlatform::Macos => signed,
    };
    if cache.release_ready != expected_release_ready
        || release.payload.client_flavor.as_ref() != Some(&cache.client_flavor)
        || release.payload.version != cache.version
        || release.payload.target != cache.target
        || release.payload.profile != cache.profile
        || release.payload.source_commit != cache.source_commit
        || release.payload.signing != cache.signing
        || release.artifact.file != cache.artifact_file
        || release.artifact.sha256 != cache.artifact_sha256
    {
        return Err("ClientFlavor cache metadata does not match its release artifact".into());
    }
    let semantic_cache_key = client_flavor_cache_key(&cache.client_flavor, &release.payload)?;
    let cache_dir_name = cache_root
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("ClientFlavor cache directory name must be UTF-8")?;
    if cache.cache_key != semantic_cache_key || cache_dir_name != semantic_cache_key {
        return Err(
            "ClientFlavor cache key does not match release semantics or directory identity".into(),
        );
    }
    if cache.release_ready && matches!(platform, ReleasePlatform::Windows | ReleasePlatform::Macos)
    {
        let host_platform = release_platform(&rustc_info(repo)?.host)?;
        if host_platform != platform {
            return Err("release-ready Windows/macOS Site Kits must be assembled and natively verified on the target operating system".into());
        }
    }
    let launcher = release
        .payload
        .site_kit_launcher
        .as_ref()
        .ok_or("ClientFlavor release artifact does not contain the V6c Site Kit launcher")?;
    validate_site_kit_launcher(Some(launcher), platform)?;

    let site_path = project_path(repo, site);
    let site_file = SignedSiteClew::read(&site_path)?;
    site_file.verify()?;
    if site_file.payload.client_flavor_id.path_component() != cache.client_flavor.id {
        return Err("site.clew ClientFlavorId does not match the cached runtime".into());
    }
    let target_platform = target_platform_for_release(platform);
    let arch = release
        .payload
        .target
        .split('-')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or("release target omitted architecture")?;
    let runtime_flavor = ClientFlavor {
        runtime_version: release.payload.version.clone(),
        platform: target_platform,
        arch: arch.to_owned(),
        outfit_id: cache.client_flavor.outfit_id.clone(),
        outfit_revision: cache.client_flavor.outfit_revision,
    };
    site_file.verify_for_flavor(&runtime_flavor)?;
    if let Some(profile) = &site_file.payload.outfit_profile
        && profile.build_cache_key()? != cache.client_flavor.build_cache_key
    {
        return Err("site.clew Outfit build key does not match the cached ClientFlavor".into());
    }

    let release_archive = cache_root.join(&cache.artifact_file);
    if fs::metadata(&release_archive)?.len() != release.artifact.size
        || sha256_file(&release_archive)? != release.artifact.sha256
    {
        return Err("cached release archive size/hash changed before Site Kit assembly".into());
    }
    let release_stem = release_package_stem(&release.payload);
    let site_bytes = fs::read(&site_path)?;
    let mut files = build_site_kit_files(
        &release_archive,
        &release_stem,
        &release.payload,
        platform,
        &site_file,
        &site_path,
        &site_bytes,
    )?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    validate_archive_files(&files)?;
    let file_records = files
        .iter()
        .map(payload_file)
        .collect::<Result<Vec<_>, _>>()?;
    let payload = SiteKitPayloadManifest {
        schema_version: SITE_KIT_SCHEMA_VERSION,
        source_cache_key: cache.cache_key.clone(),
        client_flavor: cache.client_flavor.clone(),
        target: cache.target.clone(),
        source_release_sha256: cache.artifact_sha256.clone(),
        site_sha256: sha256_bytes(&site_bytes),
        runtime_release_ready: cache.release_ready,
        files: file_records,
    };
    let payload_json = json_bytes(&payload)?;

    let cleaned_label = sanitize_site_label(site_label)?;
    let contract = SiteKitContract::for_platform(target_platform);
    let archive_name = contract.archive_name(&cleaned_label);
    let stem = site_kit_archive_stem(&archive_name)?;
    let out_root = project_path(repo, out_dir);
    fs::create_dir_all(&out_root)?;
    let archive_path = out_root.join(&archive_name);
    if platform == ReleasePlatform::Macos && cache.release_ready {
        write_release_ready_macos_site_kit(
            &release_archive,
            &release_stem,
            &archive_path,
            &stem,
            &files,
            &payload,
            &payload_json,
        )?;
    } else {
        write_site_kit_archive(platform, &archive_path, &stem, &files, &payload_json)?;
        if matches!(platform, ReleasePlatform::Windows | ReleasePlatform::Macos) {
            verify_zip_site_kit(&archive_path, &stem, &payload)?;
        }
        if platform == ReleasePlatform::Windows && cache.release_ready {
            verify_release_ready_windows_site_kit(&archive_path, &stem, &payload)?;
        }
    }
    let artifact = ArtifactInfo {
        file: archive_name.clone(),
        size: fs::metadata(&archive_path)?.len(),
        sha256: sha256_file(&archive_path)?,
    };
    let sidecar = SiteKitArtifactManifest { payload, artifact };
    let sidecar_name = format!("{stem}.site-kit.json");
    let sidecar_path = out_root.join(&sidecar_name);
    fs::write(&sidecar_path, json_bytes(&sidecar)?)?;
    write_site_kit_checksums(&out_root, &archive_name, &sidecar_name)?;

    println!("site_kit={}", archive_path.display());
    println!("sha256={}", sidecar.artifact.sha256);
    println!("manifest={}", sidecar_path.display());
    println!("client_flavor={}", cache.client_flavor.id);
    println!("release_ready={}", cache.release_ready);
    Ok(())
}

fn project_path(repo: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.join(path)
    }
}

fn target_platform_for_release(platform: ReleasePlatform) -> TargetPlatform {
    match platform {
        ReleasePlatform::Windows => TargetPlatform::Windows,
        ReleasePlatform::Macos => TargetPlatform::MacOs,
        ReleasePlatform::Linux => TargetPlatform::Linux,
    }
}

fn release_package_stem(payload: &PayloadManifest) -> String {
    if payload.unsigned {
        format!("clew-v{}-{}", payload.version, payload.target)
    } else {
        format!("clew-v{}-{}-signed", payload.version, payload.target)
    }
}

fn sanitize_site_label(value: &str) -> Result<String, Box<dyn Error>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 160 {
        return Err("Site Kit label must be 1..160 UTF-8 bytes".into());
    }
    let mut output = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_control() || matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            output.push('-');
        } else {
            output.push(ch);
        }
    }
    let cleaned = output.trim_matches([' ', '.', '-']).to_owned();
    if cleaned.is_empty() {
        return Err("Site Kit label contains no usable filename characters".into());
    }
    Ok(cleaned)
}

fn site_kit_archive_stem(name: &str) -> Result<String, Box<dyn Error>> {
    name.strip_suffix(".zip")
        .or_else(|| name.strip_suffix(".tar.gz"))
        .map(str::to_owned)
        .ok_or_else(|| "Site Kit archive name has unsupported extension".into())
}

fn build_site_kit_files(
    release_archive: &Path,
    release_stem: &str,
    release: &PayloadManifest,
    platform: ReleasePlatform,
    site: &SignedSiteClew,
    site_path: &Path,
    site_bytes: &[u8],
) -> Result<Vec<ArchiveFile>, Box<dyn Error>> {
    let profile = site
        .payload
        .outfit_profile
        .clone()
        .unwrap_or_else(|| OutfitProfile::preset(OutfitPreset::ClewOriginal));
    let contract = SiteKitContract::for_platform(target_platform_for_release(platform));
    let mut files = vec![
        archive_file("site.clew", site_bytes.to_vec(), 0o600),
        archive_file(
            contract.start_here_name,
            site_kit_start_html(&profile).into_bytes(),
            0o644,
        ),
        archive_file(
            "Message to collaborator.txt",
            format!("{}\n", profile.distribution_copy.chat_message_template).into_bytes(),
            0o644,
        ),
    ];
    append_site_outfit_assets(&mut files, site_path, &profile)?;

    let mut archive = ZipArchive::new(File::open(release_archive)?)?;
    match platform {
        ReleasePlatform::Windows => {
            let runtime =
                read_release_payload_file(&mut archive, release_stem, release, "clew.exe")?;
            files.push(archive_file(".clew-runtime/clew.exe", runtime, 0o755));
            let launcher_path = release
                .site_kit_launcher
                .as_ref()
                .ok_or("Windows release omitted Site Kit launcher metadata")?
                .executable_path
                .as_str();
            let launcher =
                read_release_payload_file(&mut archive, release_stem, release, launcher_path)?;
            for (role_dir, marker) in [
                (USE_ROLE_DIR, b"use-this-machine\n".as_slice()),
                (HELPER_ROLE_DIR, b"connector-only\n".as_slice()),
            ] {
                files.push(archive_file(
                    format!("{role_dir}/Clew.exe"),
                    launcher.clone(),
                    0o755,
                ));
                files.push(archive_file(
                    format!("{role_dir}/{ROLE_HINT_FILE}"),
                    marker.to_vec(),
                    0o644,
                ));
            }
        }
        ReleasePlatform::Macos => {
            append_release_prefix(
                &mut files,
                &mut archive,
                release_stem,
                release,
                "Clew.app/",
                ".clew-runtime/Clew.app/",
            )?;
            for (role_dir, marker) in [
                (USE_ROLE_DIR, b"use-this-machine\n".as_slice()),
                (HELPER_ROLE_DIR, b"connector-only\n".as_slice()),
            ] {
                append_release_prefix(
                    &mut files,
                    &mut archive,
                    release_stem,
                    release,
                    "Clew Role.app/",
                    &format!("{role_dir}/Clew.app/"),
                )?;
                files.push(archive_file(
                    format!("{role_dir}/{ROLE_HINT_FILE}"),
                    marker.to_vec(),
                    0o644,
                ));
            }
        }
        ReleasePlatform::Linux => {
            let runtime =
                read_release_payload_file(&mut archive, release_stem, release, "bin/clew")?;
            files.push(archive_file(".clew-runtime/clew", runtime, 0o755));
            let launcher_path = release
                .site_kit_launcher
                .as_ref()
                .ok_or("Linux release omitted Site Kit launcher metadata")?
                .executable_path
                .as_str();
            let launcher =
                read_release_payload_file(&mut archive, release_stem, release, launcher_path)?;
            for (role_dir, marker) in [
                (USE_ROLE_DIR, b"use-this-machine\n".as_slice()),
                (HELPER_ROLE_DIR, b"connector-only\n".as_slice()),
            ] {
                files.push(archive_file(
                    format!("{role_dir}/Clew"),
                    launcher.clone(),
                    0o755,
                ));
                files.push(archive_file(
                    format!("{role_dir}/{ROLE_HINT_FILE}"),
                    marker.to_vec(),
                    0o644,
                ));
            }
        }
    }
    Ok(files)
}

fn append_site_outfit_assets(
    files: &mut Vec<ArchiveFile>,
    site_path: &Path,
    profile: &OutfitProfile,
) -> Result<(), Box<dyn Error>> {
    let Some(site_root) = site_path.parent() else {
        return Err("site.clew has no parent directory".into());
    };
    let assets_root = site_root.join("outfit-assets");
    for asset_id in profile.imported_asset_ids() {
        let mut found = None;
        for extension in ["png", "svg"] {
            let candidate = assets_root.join(format!("{asset_id}.{extension}"));
            if candidate.is_file() {
                if found.is_some() {
                    return Err(format!("multiple Site Kit assets found for {asset_id}").into());
                }
                found = Some((extension, candidate));
            }
        }
        let (extension, path) =
            found.ok_or_else(|| format!("missing Site Kit asset {asset_id}"))?;
        let bytes = fs::read(path)?;
        verify_outfit_asset_bytes(&asset_id, &bytes)?;
        files.push(archive_file(
            format!("outfit-assets/{asset_id}.{extension}"),
            bytes,
            0o644,
        ));
    }
    Ok(())
}

fn read_release_payload_file(
    archive: &mut ZipArchive<File>,
    release_stem: &str,
    payload: &PayloadManifest,
    path: &str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let record = payload
        .files
        .iter()
        .find(|record| record.path == path)
        .ok_or_else(|| format!("release payload omitted required Site Kit path {path}"))?;
    let name = format!("{release_stem}/{path}");
    let bytes = read_zip_entry_bounded(archive, &name, record.size, Some(record.size))?;
    if sha256_bytes(&bytes) != record.sha256 {
        return Err(format!("release payload hash differs for Site Kit path {path}").into());
    }
    Ok(bytes)
}

fn append_release_prefix(
    files: &mut Vec<ArchiveFile>,
    archive: &mut ZipArchive<File>,
    release_stem: &str,
    payload: &PayloadManifest,
    source_prefix: &str,
    target_prefix: &str,
) -> Result<(), Box<dyn Error>> {
    let records = payload
        .files
        .iter()
        .filter(|record| record.path.starts_with(source_prefix))
        .cloned()
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Err(format!("release payload omitted required prefix {source_prefix}").into());
    }
    for record in records {
        let suffix = record
            .path
            .strip_prefix(source_prefix)
            .ok_or("release prefix mapping failed")?;
        let bytes = read_release_payload_file(archive, release_stem, payload, &record.path)?;
        let mode = u32::from_str_radix(&record.mode, 8)?;
        files.push(archive_file(
            format!("{target_prefix}{suffix}"),
            bytes,
            mode,
        ));
    }
    Ok(())
}

fn site_kit_start_html(profile: &OutfitProfile) -> String {
    let title = html_escape(&profile.distribution_copy.start_here_title);
    let body = html_escape(&profile.distribution_copy.start_here_body);
    let support = profile
        .distribution_copy
        .support_contact
        .as_ref()
        .map(|value| format!("<p>Support: {}</p>", html_escape(value)))
        .unwrap_or_default();
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1><p>{body}</p><ol><li>On the computer you want to use remotely, open <b>{USE_ROLE_DIR}</b> and start Clew.</li><li>If that computer cannot reach the internet, copy this same Site Kit to a nearby online computer, open <b>{HELPER_ROLE_DIR}</b>, and start Clew there.</li></ol><p>Keep this complete Site Kit together. The helper does not receive file or shell authority and cannot read the end-to-end protected session.</p>{support}</body></html>\n"
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn validate_archive_files(files: &[ArchiveFile]) -> Result<(), Box<dyn Error>> {
    let mut previous: Option<&str> = None;
    for file in files {
        validate_archive_relative_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err("Site Kit paths must be unique and strictly sorted".into());
        }
        previous = Some(&file.path);
    }
    Ok(())
}

fn write_site_kit_checksums(
    root: &Path,
    archive_name: &str,
    sidecar_name: &str,
) -> Result<(), Box<dyn Error>> {
    let archive_sha = sha256_file(&root.join(archive_name))?;
    let sidecar_sha = sha256_file(&root.join(sidecar_name))?;
    fs::write(
        root.join("SHA256SUMS"),
        format!("{archive_sha}  {archive_name}\n{sidecar_sha}  {sidecar_name}\n"),
    )?;
    Ok(())
}

fn cache_client_flavor(
    repo: &Path,
    manifest: &Path,
    cache_dir: &Path,
    signtool: Option<&Path>,
    allow_unsigned_rehearsal: bool,
) -> Result<(), Box<dyn Error>> {
    verify_package(repo, manifest, signtool)?;
    let manifest_path = if manifest.is_absolute() {
        manifest.to_path_buf()
    } else {
        repo.join(manifest)
    };
    let artifact_manifest = read_artifact_manifest(&manifest_path)?;
    if artifact_manifest.payload.dirty {
        return Err("dirty release artifacts are never cacheable ClientFlavors".into());
    }
    let client_flavor = artifact_manifest.payload.client_flavor.clone().ok_or(
        "release artifact predates ClientFlavor provenance and cannot enter the V6c cache",
    )?;
    validate_release_client_flavor(Some(&client_flavor))?;
    let platform = release_platform(&artifact_manifest.payload.target)?;
    let release_ready = match platform {
        ReleasePlatform::Linux => {
            artifact_manifest.payload.schema_version == RELEASE_SCHEMA_VERSION
                && artifact_manifest.payload.unsigned
                && artifact_manifest.payload.signing.is_none()
        }
        ReleasePlatform::Windows | ReleasePlatform::Macos => {
            artifact_manifest.payload.schema_version == SIGNED_RELEASE_SCHEMA_VERSION
                && !artifact_manifest.payload.unsigned
                && artifact_manifest.payload.signing.is_some()
        }
    };
    if !release_ready && !allow_unsigned_rehearsal {
        return Err("Windows/macOS ClientFlavor cache entries must be fully signed; pass --allow-unsigned-rehearsal only for pipeline rehearsal".into());
    }
    let cache_key = client_flavor_cache_key(&client_flavor, &artifact_manifest.payload)?;
    let manifest_file = manifest_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("release manifest filename must be UTF-8")?
        .to_owned();
    validate_release_filename(&artifact_manifest.artifact.file)?;
    if manifest_file.contains('/')
        || manifest_file.contains('\\')
        || !manifest_file.ends_with(".release.json")
    {
        return Err("release manifest filename is unsafe for ClientFlavor cache".into());
    }
    let source_dir = manifest_path
        .parent()
        .ok_or("release manifest has no parent directory")?;
    let source_archive = source_dir.join(&artifact_manifest.artifact.file);
    let manifest_sha256 = sha256_file(&manifest_path)?;
    let expected_entry = ClientFlavorCacheEntry {
        schema_version: CLIENT_FLAVOR_CACHE_SCHEMA_VERSION,
        cache_key: cache_key.clone(),
        client_flavor,
        version: artifact_manifest.payload.version.clone(),
        target: artifact_manifest.payload.target.clone(),
        profile: artifact_manifest.payload.profile.clone(),
        source_commit: artifact_manifest.payload.source_commit.clone(),
        release_ready,
        signing: artifact_manifest.payload.signing.clone(),
        artifact_file: artifact_manifest.artifact.file.clone(),
        artifact_sha256: artifact_manifest.artifact.sha256.clone(),
        manifest_file: manifest_file.clone(),
        manifest_sha256,
    };
    let cache_root = if cache_dir.is_absolute() {
        cache_dir.to_path_buf()
    } else {
        repo.join(cache_dir)
    };
    fs::create_dir_all(&cache_root)?;
    let target = cache_root.join(&cache_key);
    if target.exists() {
        verify_client_flavor_cache_entry(&target, &expected_entry)?;
        verify_package(repo, &target.join(&manifest_file), signtool)?;
        println!("cache_hit=true");
        println!("cache_key={cache_key}");
        println!("cache_entry={}", target.display());
        println!("release_ready={release_ready}");
        return Ok(());
    }
    let mut staging = None;
    for attempt in 0..32_u32 {
        let candidate =
            cache_root.join(format!(".{cache_key}.{}-{attempt}.tmp", std::process::id()));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                staging = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let staging = staging.ok_or("could not allocate ClientFlavor cache staging directory")?;
    let result = (|| -> Result<(), Box<dyn Error>> {
        copy_file_synced(
            &source_archive,
            &staging.join(&expected_entry.artifact_file),
        )?;
        copy_file_synced(&manifest_path, &staging.join(&manifest_file))?;
        let entry_path = staging.join("cache-entry.json");
        let mut file = File::create(&entry_path)?;
        file.write_all(&json_bytes(&expected_entry)?)?;
        file.sync_all()?;
        drop(file);
        verify_client_flavor_cache_entry(&staging, &expected_entry)?;
        fs::rename(&staging, &target)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        if target.exists() {
            verify_client_flavor_cache_entry(&target, &expected_entry)?;
            verify_package(repo, &target.join(&manifest_file), signtool)?;
            println!("cache_hit=true");
            println!("cache_key={cache_key}");
            println!("cache_entry={}", target.display());
            println!("release_ready={release_ready}");
            return Ok(());
        }
        return Err(error);
    }
    verify_package(repo, &target.join(&manifest_file), signtool)?;
    println!("cache_hit=false");
    println!("cache_key={cache_key}");
    println!("cache_entry={}", target.display());
    println!("release_ready={release_ready}");
    Ok(())
}

fn copy_file_synced(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    let bytes = fs::read(source)?;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    let mut file = options.open(target)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn verify_client_flavor_cache_entry(
    root: &Path,
    expected: &ClientFlavorCacheEntry,
) -> Result<(), Box<dyn Error>> {
    let entry_path = root.join("cache-entry.json");
    let metadata = fs::metadata(&entry_path)?;
    if !metadata.is_file() || metadata.len() > MAX_CLIENT_FLAVOR_CACHE_ENTRY_BYTES {
        return Err("ClientFlavor cache metadata is not a bounded regular file".into());
    }
    let actual: ClientFlavorCacheEntry = serde_json::from_slice(&fs::read(&entry_path)?)?;
    if &actual != expected {
        return Err("existing ClientFlavor cache entry conflicts with requested artifact".into());
    }
    let actual_files = fs::read_dir(root)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| std::io::Error::other("non-UTF-8 ClientFlavor cache filename"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let expected_files = BTreeSet::from([
        "cache-entry.json".to_owned(),
        expected.artifact_file.clone(),
        expected.manifest_file.clone(),
    ]);
    if actual_files != expected_files {
        return Err("ClientFlavor cache entry file set is not exact".into());
    }
    if sha256_file(&root.join(&expected.artifact_file))? != expected.artifact_sha256
        || sha256_file(&root.join(&expected.manifest_file))? != expected.manifest_sha256
    {
        return Err("ClientFlavor cache entry hash verification failed".into());
    }
    Ok(())
}

fn load_build_branding(
    repo: &Path,
    outfit_build: Option<&Path>,
) -> Result<BuildBranding, Box<dyn Error>> {
    let default_icon = repo.join("assets/icons/app.svg");
    let Some(root) = outfit_build else {
        let profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        return Ok(BuildBranding {
            build_cache_key: profile.build_cache_key()?,
            profile,
            icon_bytes: fs::read(&default_icon)?,
            icon_path: default_icon,
            icon_format: BuildIconFormat::Svg,
            icon_asset_id: None,
        });
    };
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        repo.join(root)
    };
    let spec_path = root.join("outfit-build.json");
    let metadata = fs::metadata(&spec_path)?;
    if !metadata.is_file() || metadata.len() > MAX_OUTFIT_BUILD_SPEC_BYTES as u64 {
        return Err("Outfit build spec is not a bounded regular file".into());
    }
    let spec = OutfitBuildSpec::decode(&fs::read(&spec_path)?)?;
    let mut expected_root = BTreeSet::from(["outfit-build.json".to_owned()]);
    if !spec.assets.is_empty() {
        expected_root.insert("assets".into());
    }
    let actual_root = fs::read_dir(&root)?
        .map(|entry| {
            entry?
                .file_name()
                .into_string()
                .map_err(|_| std::io::Error::other("non-UTF-8 Outfit build export entry"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if actual_root != expected_root {
        return Err("Outfit build export contains undeclared top-level entries".into());
    }
    let expected_asset_paths = spec
        .assets
        .iter()
        .map(|asset| asset.relative_path.clone())
        .collect::<BTreeSet<_>>();
    if !spec.assets.is_empty() {
        let actual_asset_paths = fs::read_dir(root.join("assets"))?
            .map(|entry| {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| std::io::Error::other("non-UTF-8 Outfit build asset name"))?;
                Ok(format!("assets/{name}"))
            })
            .collect::<Result<BTreeSet<String>, std::io::Error>>()?;
        if actual_asset_paths != expected_asset_paths {
            return Err("Outfit build export asset files differ from the declared set".into());
        }
    }
    for asset in &spec.assets {
        let path = root.join(&asset.relative_path);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != u64::from(asset.byte_len)
        {
            return Err(format!(
                "Outfit build asset is not the declared regular file: {}",
                asset.relative_path
            )
            .into());
        }
        verify_outfit_asset_bytes(&asset.asset_id, &fs::read(path)?)?;
    }
    let (icon_path, icon_bytes, icon_format, icon_asset_id) = match &spec.profile.visuals.app_icon {
        OutfitAssetRef::BuiltIn { key } if key == "clew-original" => (
            default_icon.clone(),
            fs::read(&default_icon)?,
            BuildIconFormat::Svg,
            None,
        ),
        OutfitAssetRef::BuiltIn { key } => {
            return Err(format!("unsupported built-in release app icon {key:?}").into());
        }
        OutfitAssetRef::Imported { asset_id } => {
            let asset = spec
                .assets
                .iter()
                .find(|asset| &asset.asset_id == asset_id)
                .ok_or("Outfit app icon was not exported")?;
            let format = if asset.relative_path.ends_with(".png") {
                BuildIconFormat::Png
            } else if asset.relative_path.ends_with(".svg") {
                BuildIconFormat::Svg
            } else {
                return Err("Outfit app icon has an unsupported format".into());
            };
            let path = root.join(&asset.relative_path);
            (
                path.clone(),
                fs::read(path)?,
                format,
                Some(asset_id.clone()),
            )
        }
    };
    Ok(BuildBranding {
        profile: spec.profile,
        build_cache_key: spec.build_cache_key,
        icon_bytes,
        icon_path,
        icon_format,
        icon_asset_id,
    })
}

fn release_client_flavor_info(
    branding: &BuildBranding,
    platform: ReleasePlatform,
    target: &str,
) -> Result<ReleaseClientFlavorInfo, Box<dyn Error>> {
    let arch = target
        .split('-')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or("release target omitted architecture")?;
    let target_platform = match platform {
        ReleasePlatform::Windows => TargetPlatform::Windows,
        ReleasePlatform::Macos => TargetPlatform::MacOs,
        ReleasePlatform::Linux => TargetPlatform::Linux,
    };
    let flavor = ClientFlavor::from_outfit_target(&branding.profile, target_platform, arch)?;
    Ok(ReleaseClientFlavorInfo {
        id: flavor.id()?.path_component(),
        outfit_id: branding.profile.outfit_id.clone(),
        outfit_revision: branding.profile.revision,
        build_cache_key: branding.build_cache_key.clone(),
        app_display_name: branding.profile.identity.app_display_name.clone(),
        publisher_label: branding.profile.identity.publisher_label.clone(),
        icon_format: branding.icon_format.label().into(),
        icon_asset_id: branding.icon_asset_id.clone(),
    })
}

fn site_kit_launcher_info(platform: ReleasePlatform) -> SiteKitLauncherInfo {
    let (executable_path, bundle_root) = match platform {
        ReleasePlatform::Windows => ("clew-role-launcher.exe".into(), None),
        ReleasePlatform::Macos => (
            format!("{MACOS_ROLE_APP}/Contents/MacOS/Clew Role"),
            Some(MACOS_ROLE_APP.into()),
        ),
        ReleasePlatform::Linux => ("bin/clew-role-launcher".into(), None),
    };
    SiteKitLauncherInfo {
        schema_version: SITE_KIT_LAUNCHER_SCHEMA_VERSION,
        executable_path,
        bundle_root,
    }
}

fn package(
    repo: &Path,
    target: Option<String>,
    profile: &str,
    out_dir: &Path,
    no_build: bool,
    allow_dirty: bool,
    skip_smoke: bool,
    outfit_build: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    validate_profile(profile)?;
    let rustc = rustc_info(repo)?;
    let host = rustc.host.clone();
    let target = target.unwrap_or_else(|| host.clone());
    validate_target(&target)?;
    let dirty = tracked_worktree_dirty(repo)?;
    if dirty && !allow_dirty {
        return Err("worktree is dirty; commit/revert it or pass --allow-dirty".into());
    }
    let source_commit = git_output(repo, &["rev-parse", "HEAD"])?;
    let source_date_epoch = source_date_epoch(repo)?;
    let cargo_lock_sha256 = sha256_file(&repo.join("Cargo.lock"))?;

    let platform = release_platform(&target)?;
    let branding = load_build_branding(repo, outfit_build)?;
    if no_build && outfit_build.is_some() {
        return Err("--no-build cannot be used with --outfit-build; the native binary must be rebuilt with the selected Outfit".into());
    }
    if !no_build {
        run_cargo_build(
            repo,
            &target,
            profile,
            source_date_epoch,
            platform,
            &branding,
        )?;
    }
    let binary = built_named_binary_path(repo, &target, profile, PRODUCT);
    if !binary.is_file() {
        return Err(format!("Clew binary does not exist: {}", binary.display()).into());
    }
    let macos_launcher = if platform == ReleasePlatform::Macos {
        let path = built_named_binary_path(repo, &target, profile, "clew-app");
        if !path.is_file() {
            return Err(format!("Clew macOS launcher does not exist: {}", path.display()).into());
        }
        Some(fs::read(path)?)
    } else {
        None
    };
    let role_launcher_path = built_named_binary_path(repo, &target, profile, "clew-role-launcher");
    if !role_launcher_path.is_file() {
        return Err(format!(
            "Clew Site Kit role launcher does not exist: {}",
            role_launcher_path.display()
        )
        .into());
    }
    let role_launcher = fs::read(&role_launcher_path)?;

    let out_dir = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        repo.join(out_dir)
    };
    fs::create_dir_all(&out_dir)?;

    let package_stem = format!("clew-v{}-{target}", env!("CARGO_PKG_VERSION"));
    let archive_name = format!("{package_stem}.zip");
    let archive_path = out_dir.join(&archive_name);
    let sidecar_name = format!("{package_stem}.release.json");
    let sidecar_path = out_dir.join(sidecar_name);

    let binary_bytes = fs::read(&binary)?;
    let readme_bytes = fs::read(repo.join("README.md"))?;
    let layout = build_package_layout(
        platform,
        &target,
        &binary_bytes,
        macos_launcher.as_deref(),
        &role_launcher,
        &readme_bytes,
        &branding,
        env!("CARGO_PKG_VERSION"),
    )?;
    let client_flavor = release_client_flavor_info(&branding, platform, &target)?;
    let site_kit_launcher = site_kit_launcher_info(platform);
    let files = layout
        .files
        .iter()
        .map(payload_file)
        .collect::<Result<Vec<_>, _>>()?;
    let payload = PayloadManifest {
        schema_version: RELEASE_SCHEMA_VERSION,
        product: PRODUCT.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        target: target.clone(),
        profile: profile.to_owned(),
        archive_format: "zip".into(),
        layout: layout.name.clone(),
        app_id: layout.app_id.clone(),
        entrypoint: layout.entrypoint.clone(),
        cli_binary: layout.cli_binary.clone(),
        source_commit,
        source_date_epoch,
        rustc,
        cargo_lock_sha256,
        dirty,
        unsigned: true,
        signing: None,
        client_flavor: Some(client_flavor),
        site_kit_launcher: Some(site_kit_launcher),
        files,
    };
    let payload_json = json_bytes(&payload)?;
    write_zip(&archive_path, &package_stem, &layout.files, &payload_json)?;
    if !skip_smoke && target == host {
        smoke_archive(&archive_path, &package_stem, &payload, true)?;
    }

    let archive_size = fs::metadata(&archive_path)?.len();
    let archive_sha256 = sha256_file(&archive_path)?;
    let artifact_manifest = ArtifactManifest {
        payload,
        artifact: ArtifactInfo {
            file: archive_name.clone(),
            size: archive_size,
            sha256: archive_sha256.clone(),
        },
    };
    fs::write(&sidecar_path, json_bytes(&artifact_manifest)?)?;
    refresh_checksums(&out_dir)?;

    println!("artifact={}", archive_path.display());
    println!("sha256={archive_sha256}");
    println!("manifest={}", sidecar_path.display());
    println!("unsigned=true");
    Ok(())
}

fn verify_package(
    repo: &Path,
    manifest: &Path,
    signtool: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let manifest_path = if manifest.is_absolute() {
        manifest.to_path_buf()
    } else {
        repo.join(manifest)
    };
    let artifact_manifest = read_artifact_manifest(&manifest_path)?;
    validate_release_filename(&artifact_manifest.artifact.file)?;
    let payload = &artifact_manifest.payload;
    let signed = match (
        payload.schema_version,
        payload.unsigned,
        payload.signing.is_some(),
    ) {
        (RELEASE_SCHEMA_VERSION, true, false) => false,
        (SIGNED_RELEASE_SCHEMA_VERSION, false, true) => true,
        _ => return Err("release sidecar has an unsupported signing/schema state".into()),
    };
    let platform = if signed {
        validate_signed_artifact(&artifact_manifest)?
    } else {
        validate_unsigned_artifact(&artifact_manifest)?
    };
    let stem = if signed {
        format!("clew-v{}-{}-signed", payload.version, payload.target)
    } else {
        format!("clew-v{}-{}", payload.version, payload.target)
    };
    let expected_archive = format!("{stem}.zip");
    if artifact_manifest.artifact.file != expected_archive {
        return Err("release archive filename does not match manifest state/version/target".into());
    }
    let archive_path = manifest_path
        .parent()
        .ok_or("release manifest has no parent directory")?
        .join(&artifact_manifest.artifact.file);
    if fs::metadata(&archive_path)?.len() != artifact_manifest.artifact.size
        || sha256_file(&archive_path)? != artifact_manifest.artifact.sha256
    {
        return Err("release archive size/hash differs from release sidecar".into());
    }
    let host = rustc_info(repo)?.host;
    if !signed {
        if signtool.is_some() {
            return Err("--signtool is only valid when verifying a signed Windows artifact".into());
        }
        smoke_archive(&archive_path, &stem, payload, payload.target == host)?;
    } else {
        if release_platform(&host)? != platform {
            return Err("signed artifacts require native operating-system verification".into());
        }
        match platform {
            ReleasePlatform::Windows => {
                smoke_archive(&archive_path, &stem, payload, payload.target == host)?;
                let tool = resolve_windows_signtool(signtool)?;
                verify_windows_archive_signature(&tool, &archive_path, &stem, payload)?;
            }
            ReleasePlatform::Macos => {
                if signtool.is_some() {
                    return Err("--signtool is only valid for signed Windows verification".into());
                }
                smoke_macos_signed_archive(&archive_path, &stem, payload, payload.target == host)?;
            }
            ReleasePlatform::Linux => {
                return Err("Linux signed release verification is not defined in V6b-3".into());
            }
        }
    }
    println!("verified=true");
    println!("artifact={}", archive_path.display());
    println!("sha256={}", artifact_manifest.artifact.sha256);
    println!("schema_version={}", payload.schema_version);
    println!("unsigned={}", payload.unsigned);
    Ok(())
}

fn sign_package(
    repo: &Path,
    manifest: &Path,
    out_dir: &Path,
    signer: Signer,
) -> Result<(), Box<dyn Error>> {
    let manifest_path = if manifest.is_absolute() {
        manifest.to_path_buf()
    } else {
        repo.join(manifest)
    };
    let artifact_manifest = read_artifact_manifest(&manifest_path)?;
    validate_signable_artifact(&artifact_manifest)?;
    let payload = &artifact_manifest.payload;
    let input_dir = manifest_path
        .parent()
        .ok_or("release manifest has no parent directory")?;
    validate_release_filename(&artifact_manifest.artifact.file)?;
    let archive_path = input_dir.join(&artifact_manifest.artifact.file);
    if fs::metadata(&archive_path)?.len() != artifact_manifest.artifact.size
        || sha256_file(&archive_path)? != artifact_manifest.artifact.sha256
    {
        return Err("unsigned archive size/hash differs from release sidecar".into());
    }
    let expected_archive = format!("clew-v{}-{}.zip", payload.version, payload.target);
    if artifact_manifest.artifact.file != expected_archive {
        return Err("unsigned archive filename does not match manifest version/target".into());
    }
    let unsigned_stem = expected_archive
        .strip_suffix(".zip")
        .ok_or("release archive must end in .zip")?;
    let platform = release_platform(&payload.target)?;
    let signer_platform = match &signer {
        Signer::Windows { .. } => ReleasePlatform::Windows,
        Signer::Macos { .. } => ReleasePlatform::Macos,
    };
    if platform != signer_platform {
        return Err("signer does not match the unsigned artifact platform".into());
    }
    let host = rustc_info(repo)?.host;
    if release_platform(&host)? != platform {
        return Err("signing must run on the same operating-system family as the artifact".into());
    }
    smoke_archive(
        &archive_path,
        unsigned_stem,
        payload,
        payload.target == host,
    )?;

    let signed_stem = format!("{unsigned_stem}-signed");
    let temp = tempfile::tempdir()?;
    let root = temp.path().join(&signed_stem);
    fs::create_dir(&root)?;
    materialize_payload(&archive_path, unsigned_stem, payload, &root)?;

    let mut windows_signtool = None;
    let signing = match signer {
        Signer::Windows {
            cert_sha1,
            timestamp_url,
            signtool,
            machine_store,
        } => {
            let cert_sha1 = normalize_certificate_sha1(&cert_sha1)?;
            validate_timestamp_url(&timestamp_url)?;
            let tool = resolve_windows_signtool(signtool.as_deref())?;
            sign_windows_payload(
                &tool,
                &root,
                payload,
                &cert_sha1,
                &timestamp_url,
                machine_store,
            )?;
            windows_signtool = Some(tool);
            SigningInfo {
                mechanism: "windows-authenticode".into(),
                identity: cert_sha1,
                timestamped: true,
                notarized: false,
                stapled: false,
                notary_submission_id: None,
            }
        }
        Signer::Macos {
            identity,
            notary_profile,
        } => sign_macos_payload(&root, payload, &identity, &notary_profile)?,
    };

    let signed_files = collect_signed_files(&root, platform, &payload.files)?;
    let mut signed_payload = payload.clone();
    signed_payload.schema_version = SIGNED_RELEASE_SCHEMA_VERSION;
    signed_payload.unsigned = false;
    signed_payload.signing = Some(signing);
    signed_payload.files = signed_files
        .iter()
        .map(payload_file)
        .collect::<Result<Vec<_>, _>>()?;
    let payload_json = json_bytes(&signed_payload)?;

    let out_dir = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        repo.join(out_dir)
    };
    fs::create_dir_all(&out_dir)?;
    let archive_name = format!("{signed_stem}.zip");
    let signed_archive_path = out_dir.join(&archive_name);
    let sidecar_path = out_dir.join(format!("{signed_stem}.release.json"));
    match platform {
        ReleasePlatform::Windows => {
            write_zip(
                &signed_archive_path,
                &signed_stem,
                &signed_files,
                &payload_json,
            )?;
            smoke_archive(
                &signed_archive_path,
                &signed_stem,
                &signed_payload,
                payload.target == host,
            )?;
            verify_windows_archive_signature(
                windows_signtool
                    .as_deref()
                    .ok_or("Windows signer did not retain SignTool path")?,
                &signed_archive_path,
                &signed_stem,
                &signed_payload,
            )?;
        }
        ReleasePlatform::Macos => {
            fs::write(root.join("release-manifest.json"), &payload_json)?;
            write_macos_distribution_zip(&root, &signed_archive_path)?;
            smoke_macos_signed_archive(
                &signed_archive_path,
                &signed_stem,
                &signed_payload,
                payload.target == host,
            )?;
        }
        ReleasePlatform::Linux => {
            return Err("Linux release signing is not defined in V6b-3".into());
        }
    }

    let archive_size = fs::metadata(&signed_archive_path)?.len();
    let archive_sha256 = sha256_file(&signed_archive_path)?;
    let signed_manifest = ArtifactManifest {
        payload: signed_payload,
        artifact: ArtifactInfo {
            file: archive_name,
            size: archive_size,
            sha256: archive_sha256.clone(),
        },
    };
    fs::write(&sidecar_path, json_bytes(&signed_manifest)?)?;
    refresh_checksums(&out_dir)?;
    println!("artifact={}", signed_archive_path.display());
    println!("sha256={archive_sha256}");
    println!("manifest={}", sidecar_path.display());
    println!("unsigned=false");
    Ok(())
}

fn read_artifact_manifest(path: &Path) -> Result<ArtifactManifest, Box<dyn Error>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_EMBEDDED_MANIFEST_BYTES {
        return Err("release sidecar is not a bounded regular file".into());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn validate_release_filename(name: &str) -> Result<(), Box<dyn Error>> {
    if name.is_empty()
        || name.len() > 255
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || !name.ends_with(".zip")
    {
        return Err("release artifact filename is unsafe".into());
    }
    Ok(())
}

fn validate_unsigned_artifact(
    manifest: &ArtifactManifest,
) -> Result<ReleasePlatform, Box<dyn Error>> {
    let payload = &manifest.payload;
    if payload.schema_version != RELEASE_SCHEMA_VERSION
        || payload.product != PRODUCT
        || payload.app_id != APP_ID
        || payload.archive_format != "zip"
        || !payload.unsigned
        || payload.signing.is_some()
    {
        return Err("release sidecar is not an unsigned Clew schema-2 artifact".into());
    }
    let platform = validate_payload_layout(payload)?;
    validate_release_client_flavor(payload.client_flavor.as_ref())?;
    validate_site_kit_launcher(payload.site_kit_launcher.as_ref(), platform)?;
    validate_payload_shape(payload, platform, false)?;
    Ok(platform)
}

fn validate_signable_artifact(manifest: &ArtifactManifest) -> Result<(), Box<dyn Error>> {
    let platform = validate_unsigned_artifact(manifest)?;
    if manifest.payload.dirty {
        return Err("dirty artifacts are never accepted by the release signing pipeline".into());
    }
    if platform == ReleasePlatform::Linux {
        return Err("Linux release signing is not defined in V6b-3".into());
    }
    Ok(())
}

fn validate_signed_artifact(
    manifest: &ArtifactManifest,
) -> Result<ReleasePlatform, Box<dyn Error>> {
    let payload = &manifest.payload;
    if payload.schema_version != SIGNED_RELEASE_SCHEMA_VERSION
        || payload.product != PRODUCT
        || payload.app_id != APP_ID
        || payload.archive_format != "zip"
        || payload.dirty
        || payload.unsigned
    {
        return Err("release sidecar is not a clean signed Clew schema-3 artifact".into());
    }
    let signing = payload
        .signing
        .as_ref()
        .ok_or("signed release sidecar omitted signing metadata")?;
    let platform = validate_payload_layout(payload)?;
    validate_release_client_flavor(payload.client_flavor.as_ref())?;
    validate_site_kit_launcher(payload.site_kit_launcher.as_ref(), platform)?;
    validate_payload_shape(payload, platform, true)?;
    match platform {
        ReleasePlatform::Windows => {
            if signing.mechanism != "windows-authenticode"
                || !signing.timestamped
                || signing.notarized
                || signing.stapled
                || signing.notary_submission_id.is_some()
                || normalize_certificate_sha1(&signing.identity)? != signing.identity
            {
                return Err("Windows signed release metadata is inconsistent".into());
            }
        }
        ReleasePlatform::Macos => {
            let submission_id = signing
                .notary_submission_id
                .as_deref()
                .ok_or("notarized macOS release omitted submission id")?;
            validate_signing_label(&signing.identity, "Developer ID identity")?;
            validate_signing_label(submission_id, "notary submission id")?;
            if signing.mechanism != "macos-developer-id-notarized"
                || !signing.timestamped
                || !signing.notarized
                || !signing.stapled
            {
                return Err("macOS signed release metadata is inconsistent".into());
            }
        }
        ReleasePlatform::Linux => {
            return Err("Linux signed release verification is not defined in V6b-3".into());
        }
    }
    Ok(platform)
}

fn validate_payload_layout(payload: &PayloadManifest) -> Result<ReleasePlatform, Box<dyn Error>> {
    validate_target(&payload.target)?;
    validate_profile(&payload.profile)?;
    let platform = release_platform(&payload.target)?;
    match platform {
        ReleasePlatform::Windows
            if payload.layout == "windows-portable"
                && payload.entrypoint == "clew.exe"
                && payload.cli_binary == "clew.exe" => {}
        ReleasePlatform::Macos
            if payload.layout == "macos-app"
                && payload.entrypoint == "Clew.app/Contents/MacOS/Clew"
                && payload.cli_binary == "Clew.app/Contents/Resources/clew" => {}
        ReleasePlatform::Linux
            if payload.layout == "linux-portable"
                && payload.entrypoint == "bin/clew"
                && payload.cli_binary == "bin/clew" => {}
        _ => return Err("release sidecar layout does not match its target platform".into()),
    }
    Ok(platform)
}

fn validate_payload_shape(
    payload: &PayloadManifest,
    platform: ReleasePlatform,
    signed: bool,
) -> Result<(), Box<dyn Error>> {
    let expected_paths = expected_payload_paths(payload, platform, signed)?;
    let actual_paths = payload
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    if actual_paths != expected_paths {
        return Err("release payload file set does not match the frozen platform layout".into());
    }
    if payload.files.len() > MAX_SIGNED_PAYLOAD_FILES {
        return Err("release payload file count is outside signing bounds".into());
    }
    let mut total = 0_u64;
    let mut previous: Option<&str> = None;
    for file in &payload.files {
        validate_archive_relative_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err("release payload files are not unique and strictly sorted".into());
        }
        previous = Some(&file.path);
        if file.size > MAX_SIGNED_FILE_BYTES {
            return Err("release payload file exceeds signing size bound".into());
        }
        total = total
            .checked_add(file.size)
            .ok_or("release payload total size overflow")?;
        if total > MAX_SIGNED_PAYLOAD_BYTES {
            return Err("release payload exceeds signing total size bound".into());
        }
        let launcher_executable = payload
            .site_kit_launcher
            .as_ref()
            .map(|launcher| launcher.executable_path.as_str());
        let expected_mode = if file.path == payload.entrypoint
            || file.path == payload.cli_binary
            || launcher_executable == Some(file.path.as_str())
        {
            "0755"
        } else {
            "0644"
        };
        if file.mode != expected_mode {
            return Err(format!(
                "release payload mode differs for {}: {} != {expected_mode}",
                file.path, file.mode
            )
            .into());
        }
    }
    Ok(())
}

fn validate_release_client_flavor(
    flavor: Option<&ReleaseClientFlavorInfo>,
) -> Result<(), Box<dyn Error>> {
    let Some(flavor) = flavor else {
        return Ok(());
    };
    if flavor.id.len() != 64
        || !flavor
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || !flavor.build_cache_key.starts_with("outfit-v1-")
        || flavor.build_cache_key.len() != 74
        || !flavor.build_cache_key[10..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || flavor.outfit_revision == 0
        || flavor.outfit_id.is_empty()
        || flavor.outfit_id.len() > 64
        || flavor.app_display_name.trim().is_empty()
        || flavor.app_display_name.len() > 96
        || flavor.app_display_name.chars().any(char::is_control)
        || !matches!(flavor.icon_format.as_str(), "svg" | "png")
    {
        return Err("release ClientFlavor metadata is invalid".into());
    }
    if let Some(publisher) = &flavor.publisher_label
        && (publisher.trim().is_empty()
            || publisher.len() > 96
            || publisher.chars().any(char::is_control))
    {
        return Err("release ClientFlavor publisher label is invalid".into());
    }
    if let Some(asset_id) = &flavor.icon_asset_id {
        let Some(hash) = asset_id.strip_prefix("sha256-") else {
            return Err("release ClientFlavor icon asset id is invalid".into());
        };
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("release ClientFlavor icon asset id is invalid".into());
        }
    }
    Ok(())
}

fn validate_site_kit_launcher(
    launcher: Option<&SiteKitLauncherInfo>,
    platform: ReleasePlatform,
) -> Result<(), Box<dyn Error>> {
    let Some(launcher) = launcher else {
        return Ok(());
    };
    if launcher.schema_version != SITE_KIT_LAUNCHER_SCHEMA_VERSION {
        return Err("unsupported Site Kit launcher metadata schema".into());
    }
    validate_archive_relative_path(&launcher.executable_path)?;
    match platform {
        ReleasePlatform::Windows => {
            if launcher.executable_path != "clew-role-launcher.exe"
                || launcher.bundle_root.is_some()
            {
                return Err("Windows Site Kit launcher metadata is invalid".into());
            }
        }
        ReleasePlatform::Macos => {
            if launcher.executable_path != format!("{MACOS_ROLE_APP}/Contents/MacOS/Clew Role")
                || launcher.bundle_root.as_deref() != Some(MACOS_ROLE_APP)
            {
                return Err("macOS Site Kit launcher metadata is invalid".into());
            }
        }
        ReleasePlatform::Linux => {
            if launcher.executable_path != "bin/clew-role-launcher"
                || launcher.bundle_root.is_some()
            {
                return Err("Linux Site Kit launcher metadata is invalid".into());
            }
        }
    }
    Ok(())
}

fn expected_payload_paths(
    payload: &PayloadManifest,
    platform: ReleasePlatform,
    signed: bool,
) -> Result<Vec<String>, Box<dyn Error>> {
    let linux_png = payload
        .client_flavor
        .as_ref()
        .is_some_and(|flavor| flavor.icon_format == "png");
    let has_role_launcher = payload.site_kit_launcher.is_some();
    let mut paths = match platform {
        ReleasePlatform::Windows => vec!["README.md".into(), "clew.exe".into()],
        ReleasePlatform::Macos => {
            let mut paths = vec![
                "Clew.app/Contents/Info.plist".into(),
                "Clew.app/Contents/MacOS/Clew".into(),
                "Clew.app/Contents/Resources/AppIcon.icns".into(),
                "Clew.app/Contents/Resources/clew".into(),
                "README.md".into(),
            ];
            if signed {
                paths.push(MACOS_CODE_RESOURCES.into());
            }
            paths
        }
        ReleasePlatform::Linux => {
            if signed {
                return Err("Linux signed release layout is not defined in V6b-3".into());
            }
            vec![
                "README.md".into(),
                "bin/clew".into(),
                "share/applications/io.clew.app.desktop".into(),
                if linux_png {
                    "share/icons/hicolor/256x256/apps/clew.png".into()
                } else {
                    "share/icons/hicolor/scalable/apps/clew.svg".into()
                },
            ]
        }
    };
    if has_role_launcher {
        match platform {
            ReleasePlatform::Windows => paths.push("clew-role-launcher.exe".into()),
            ReleasePlatform::Macos => {
                paths.extend([
                    format!("{MACOS_ROLE_APP}/Contents/Info.plist"),
                    format!("{MACOS_ROLE_APP}/Contents/MacOS/Clew Role"),
                    format!("{MACOS_ROLE_APP}/Contents/Resources/AppIcon.icns"),
                ]);
                if signed {
                    paths.push(MACOS_ROLE_CODE_RESOURCES.into());
                }
            }
            ReleasePlatform::Linux => paths.push("bin/clew-role-launcher".into()),
        }
    }
    paths.sort();
    Ok(paths)
}

fn materialize_payload(
    archive_path: &Path,
    package_stem: &str,
    payload: &PayloadManifest,
    root: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    for record in &payload.files {
        let entry_name = format!("{package_stem}/{}", record.path);
        let bytes =
            read_zip_entry_bounded(&mut archive, &entry_name, record.size, Some(record.size))?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(
                format!("unsigned archive payload hash differs for {}", record.path).into(),
            );
        }
        let output = root.join(&record.path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&output, bytes)?;
        set_recorded_mode(&output, &record.mode)?;
    }
    Ok(())
}

fn set_recorded_mode(path: &Path, mode: &str) -> Result<(), Box<dyn Error>> {
    let parsed = u32::from_str_radix(mode, 8)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(parsed))?;
    }
    #[cfg(not(unix))]
    let _ = (path, parsed);
    Ok(())
}

fn collect_signed_files(
    root: &Path,
    platform: ReleasePlatform,
    unsigned_files: &[PayloadFile],
) -> Result<Vec<ArchiveFile>, Box<dyn Error>> {
    let mut relative_paths = Vec::new();
    collect_regular_paths(root, root, &mut relative_paths)?;
    if relative_paths.len() > MAX_SIGNED_PAYLOAD_FILES {
        return Err("signed payload contains too many files".into());
    }
    relative_paths.sort();
    let unsigned_paths = unsigned_files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let signed_paths = relative_paths
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if !unsigned_paths.is_subset(&signed_paths) {
        return Err("signed payload lost an unsigned input file".into());
    }
    let allowed_extra = match platform {
        ReleasePlatform::Windows => BTreeSet::new(),
        ReleasePlatform::Macos => {
            let mut allowed = BTreeSet::from([MACOS_CODE_RESOURCES]);
            if unsigned_paths
                .iter()
                .any(|path| path.starts_with("Clew Role.app/"))
            {
                allowed.insert(MACOS_ROLE_CODE_RESOURCES);
            }
            allowed
        }
        ReleasePlatform::Linux => return Err("Linux signed payload is unsupported".into()),
    };
    let actual_extra = signed_paths
        .difference(&unsigned_paths)
        .copied()
        .collect::<BTreeSet<_>>();
    if actual_extra != allowed_extra {
        return Err(format!("signing produced unexpected payload paths: {actual_extra:?}").into());
    }

    let mut output = Vec::with_capacity(relative_paths.len());
    let mut total = 0_u64;
    for path in relative_paths {
        let full = root.join(&path);
        let bytes = fs::read(&full)?;
        if bytes.len() as u64 > MAX_SIGNED_FILE_BYTES {
            return Err(format!("signed payload file exceeds size bound: {path}").into());
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("signed payload total size overflow")?;
        if total > MAX_SIGNED_PAYLOAD_BYTES {
            return Err("signed payload exceeds total size bound".into());
        }
        let mode = if let Some(record) = unsigned_files.iter().find(|record| record.path == path) {
            u32::from_str_radix(&record.mode, 8)?
        } else if platform == ReleasePlatform::Macos
            && matches!(
                path.as_str(),
                MACOS_CODE_RESOURCES | MACOS_ROLE_CODE_RESOURCES
            )
        {
            0o644
        } else {
            return Err(format!("signed payload mode is undefined for {path}").into());
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let actual_mode = fs::metadata(&full)?.permissions().mode() & 0o777;
            if actual_mode != mode {
                return Err(format!(
                    "signed payload mode changed for {path}: {actual_mode:04o} != {mode:04o}"
                )
                .into());
            }
        }
        output.push(archive_file(path, bytes, mode));
    }
    Ok(output)
}

fn collect_regular_paths(
    root: &Path,
    current: &Path,
    output: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err("signed payload must not contain symlinks".into());
        }
        if metadata.is_dir() {
            collect_regular_paths(root, &path, output)?;
            continue;
        }
        if !metadata.is_file() {
            return Err("signed payload contains a non-file/non-directory entry".into());
        }
        let relative = path.strip_prefix(root)?;
        let relative = relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .ok_or("signed payload path is not UTF-8")
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        if relative == "release-manifest.json" {
            continue;
        }
        validate_archive_relative_path(&relative)?;
        output.push(relative);
        if output.len() > MAX_SIGNED_PAYLOAD_FILES {
            return Err("signed payload contains too many files".into());
        }
    }
    Ok(())
}

fn normalize_certificate_sha1(value: &str) -> Result<String, Box<dyn Error>> {
    let normalized = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b':')
        .collect::<Vec<_>>();
    if normalized.len() != 40 || !normalized.iter().all(u8::is_ascii_hexdigit) {
        return Err(
            "Windows certificate SHA-1 thumbprint must contain exactly 40 hex digits".into(),
        );
    }
    Ok(String::from_utf8(normalized)?.to_ascii_uppercase())
}

fn validate_timestamp_url(value: &str) -> Result<(), Box<dyn Error>> {
    if value.len() > 2048
        || !(value.starts_with("http://") || value.starts_with("https://"))
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(
            "timestamp URL must be a bounded absolute HTTP(S) URL without whitespace".into(),
        );
    }
    Ok(())
}

fn validate_signing_label(value: &str, field: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(
            format!("{field} is empty, oversized, padded, or contains control bytes").into(),
        );
    }
    Ok(())
}

fn resolve_windows_signtool(explicit: Option<&Path>) -> Result<PathBuf, Box<dyn Error>> {
    if !cfg!(windows) {
        return Err("Windows Authenticode signing must run on Windows".into());
    }
    if let Some(path) = explicit {
        if path.components().count() > 1 && !path.is_file() {
            return Err(
                format!("explicit SignTool path does not exist: {}", path.display()).into(),
            );
        }
        return Ok(path.to_path_buf());
    }
    if let Ok(output) = Command::new("where.exe").arg("signtool.exe").output()
        && output.status.success()
        && let Some(line) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|line| !line.trim().is_empty())
    {
        return Ok(PathBuf::from(line.trim()));
    }
    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x86"
    };
    let mut candidates = Vec::new();
    for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Some(base) = env::var_os(variable) else {
            continue;
        };
        let root = PathBuf::from(base)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let candidate = entry.path().join(arch).join("signtool.exe");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort();
    candidates
        .pop()
        .ok_or_else(|| "SignTool was not found in PATH or standard Windows Kits locations".into())
}

fn sign_windows_payload(
    signtool: &Path,
    root: &Path,
    payload: &PayloadManifest,
    cert_sha1: &str,
    timestamp_url: &str,
    machine_store: bool,
) -> Result<(), Box<dyn Error>> {
    if !cfg!(windows) {
        return Err("Windows Authenticode signing must run on Windows".into());
    }
    for relative in windows_signable_paths(payload)? {
        let binary = root.join(relative);
        let mut sign = Command::new(signtool);
        sign.arg("sign");
        if machine_store {
            sign.arg("/sm");
        }
        sign.args([
            "/sha1",
            cert_sha1,
            "/fd",
            "SHA256",
            "/tr",
            timestamp_url,
            "/td",
            "SHA256",
        ])
        .arg(&binary);
        command_output_combined(&mut sign)?;
        verify_windows_signature(signtool, &binary)?;
    }
    Ok(())
}

fn windows_signable_paths(payload: &PayloadManifest) -> Result<Vec<&str>, Box<dyn Error>> {
    let mut paths = vec![payload.entrypoint.as_str()];
    if let Some(launcher) = &payload.site_kit_launcher {
        paths.push(launcher.executable_path.as_str());
    }
    paths.sort_unstable();
    paths.dedup();
    for path in &paths {
        if !path.to_ascii_lowercase().ends_with(".exe")
            || !payload.files.iter().any(|record| record.path == *path)
        {
            return Err("Windows signable executable is absent from release payload".into());
        }
    }
    Ok(paths)
}

fn verify_windows_signature(signtool: &Path, binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut verify = Command::new(signtool);
    verify.args(["verify", "/pa", "/all", "/v"]).arg(binary);
    command_output_combined(&mut verify)?;
    Ok(())
}

fn verify_windows_archive_signature(
    signtool: &Path,
    archive_path: &Path,
    package_stem: &str,
    payload: &PayloadManifest,
) -> Result<(), Box<dyn Error>> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let temp = tempfile::tempdir()?;
    for (index, relative) in windows_signable_paths(payload)?.into_iter().enumerate() {
        let entry = format!("{package_stem}/{relative}");
        let record = payload
            .files
            .iter()
            .find(|record| record.path == relative)
            .ok_or("signed Windows executable is missing from manifest")?;
        let bytes = read_zip_entry_bounded(&mut archive, &entry, record.size, Some(record.size))?;
        let binary = temp.path().join(format!("signed-{index}.exe"));
        fs::write(&binary, bytes)?;
        verify_windows_signature(signtool, &binary)?;
    }
    Ok(())
}

fn sign_macos_payload(
    root: &Path,
    payload: &PayloadManifest,
    identity: &str,
    notary_profile: &str,
) -> Result<SigningInfo, Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("Developer ID signing/notarization must run on macOS".into());
    }
    validate_signing_label(identity, "Developer ID identity")?;
    validate_signing_label(notary_profile, "notarytool keychain profile")?;
    let app = root.join("Clew.app");
    let cli = root.join(&payload.cli_binary);
    let mut sign_cli = Command::new("codesign");
    sign_cli
        .args([
            "--force",
            "--sign",
            identity,
            "--timestamp",
            "--options",
            "runtime",
            "--identifier",
        ])
        .arg(format!("{APP_ID}.cli"))
        .arg(&cli);
    command_output_combined(&mut sign_cli)?;
    verify_macos_signed_code(&cli)?;

    let role_app = if let Some(launcher) = &payload.site_kit_launcher {
        let bundle = launcher
            .bundle_root
            .as_deref()
            .ok_or("macOS Site Kit launcher metadata omitted bundle root")?;
        let role_app = root.join(bundle);
        let role_executable = root.join(&launcher.executable_path);
        let mut sign_role_executable = Command::new("codesign");
        sign_role_executable
            .args([
                "--force",
                "--sign",
                identity,
                "--timestamp",
                "--options",
                "runtime",
                "--identifier",
            ])
            .arg(format!("{APP_ID}.role.launcher"))
            .arg(&role_executable);
        command_output_combined(&mut sign_role_executable)?;
        verify_macos_signed_code(&role_executable)?;

        let mut sign_role_app = Command::new("codesign");
        sign_role_app
            .args([
                "--force",
                "--sign",
                identity,
                "--timestamp",
                "--options",
                "runtime",
            ])
            .arg(&role_app);
        command_output_combined(&mut sign_role_app)?;
        verify_macos_signed_code(&role_app)?;
        Some(role_app)
    } else {
        None
    };

    let mut sign_app = Command::new("codesign");
    sign_app
        .args([
            "--force",
            "--sign",
            identity,
            "--timestamp",
            "--options",
            "runtime",
        ])
        .arg(&app);
    command_output_combined(&mut sign_app)?;
    verify_macos_signed_code(&app)?;

    let submission = root
        .parent()
        .ok_or("signed staging root has no parent")?
        .join("notary-submit.zip");
    write_macos_distribution_zip(root, &submission)?;
    let mut notary = Command::new("xcrun");
    notary
        .args(["notarytool", "submit"])
        .arg(&submission)
        .args([
            "--keychain-profile",
            notary_profile,
            "--wait",
            "--output-format",
            "json",
        ]);
    let output = command_output(&mut notary)?;
    let response: serde_json::Value = serde_json::from_str(&output)?;
    let status = response
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or("notarytool JSON response omitted status")?;
    let submission_id = response
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("notarytool JSON response omitted submission id")?
        .to_owned();
    if status != "Accepted" {
        return Err(format!("Apple notarization did not accept the package: {status}").into());
    }
    let mut staple = Command::new("xcrun");
    staple.args(["stapler", "staple"]).arg(&app);
    command_output_combined(&mut staple)?;
    if let Some(role_app) = &role_app {
        let mut staple_role = Command::new("xcrun");
        staple_role.args(["stapler", "staple"]).arg(role_app);
        command_output_combined(&mut staple_role)?;
    }
    verify_macos_distribution(root, payload)?;
    Ok(SigningInfo {
        mechanism: "macos-developer-id-notarized".into(),
        identity: identity.to_owned(),
        timestamped: true,
        notarized: true,
        stapled: true,
        notary_submission_id: Some(submission_id),
    })
}

fn verify_macos_signed_code(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut verify = Command::new("codesign");
    verify
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(path);
    command_output_combined(&mut verify)?;
    let mut display = Command::new("codesign");
    display.args(["--display", "--verbose=4"]).arg(path);
    let details = command_output_combined(&mut display)?;
    if !details.contains("runtime")
        || !details
            .lines()
            .any(|line| line.trim().starts_with("Timestamp="))
    {
        return Err(
            "Developer ID signature lacks Hardened Runtime or secure timestamp evidence".into(),
        );
    }
    Ok(())
}

fn verify_macos_distribution(root: &Path, payload: &PayloadManifest) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("macOS distribution verification must run on macOS".into());
    }
    let app = root.join("Clew.app");
    verify_macos_signed_code(&root.join(&payload.cli_binary))?;
    verify_macos_signed_code(&app)?;
    if let Some(launcher) = &payload.site_kit_launcher {
        let role_app = root.join(
            launcher
                .bundle_root
                .as_deref()
                .ok_or("macOS Site Kit launcher metadata omitted bundle root")?,
        );
        verify_macos_signed_code(&root.join(&launcher.executable_path))?;
        verify_macos_signed_code(&role_app)?;
        let mut staple_role = Command::new("xcrun");
        staple_role.args(["stapler", "validate"]).arg(&role_app);
        command_output_combined(&mut staple_role)?;
        let mut assess_role = Command::new("spctl");
        assess_role
            .args(["--assess", "--type", "exec", "--verbose=4"])
            .arg(&role_app);
        command_output_combined(&mut assess_role)?;
    }
    let mut staple = Command::new("xcrun");
    staple.args(["stapler", "validate"]).arg(&app);
    command_output_combined(&mut staple)?;
    let mut assess = Command::new("spctl");
    assess
        .args(["--assess", "--type", "exec", "--verbose=4"])
        .arg(&app);
    command_output_combined(&mut assess)?;
    Ok(())
}

fn write_macos_distribution_zip(source: &Path, archive: &Path) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("ditto packaging for notarized artifacts must run on macOS".into());
    }
    let mut command = Command::new("/usr/bin/ditto");
    command
        .args(["-c", "-k", "--keepParent"])
        .arg(source)
        .arg(archive);
    command_output_combined(&mut command)?;
    Ok(())
}

fn smoke_macos_signed_archive(
    archive_path: &Path,
    package_stem: &str,
    expected_payload: &PayloadManifest,
    execute_binary: bool,
) -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let mut extract = Command::new("/usr/bin/ditto");
    extract
        .args(["-x", "-k"])
        .arg(archive_path)
        .arg(temp.path());
    command_output_combined(&mut extract)?;
    let root = temp.path().join(package_stem);
    let embedded: PayloadManifest =
        serde_json::from_slice(&fs::read(root.join("release-manifest.json"))?)?;
    if &embedded != expected_payload {
        return Err("embedded signed release manifest differs from sidecar payload".into());
    }
    let unsigned_files = expected_payload
        .files
        .iter()
        .filter(|file| file.path != MACOS_CODE_RESOURCES && file.path != MACOS_ROLE_CODE_RESOURCES)
        .cloned()
        .collect::<Vec<_>>();
    let files = collect_signed_files(&root, ReleasePlatform::Macos, &unsigned_files)?;
    let actual = files
        .iter()
        .map(payload_file)
        .collect::<Result<Vec<_>, _>>()?;
    if actual != expected_payload.files {
        return Err("signed macOS archive payload differs after ditto extraction".into());
    }
    if execute_binary {
        smoke_binary(&root.join(&expected_payload.cli_binary))?;
    }
    verify_macos_distribution(&root, expected_payload)
}

fn validate_profile(profile: &str) -> Result<(), Box<dyn Error>> {
    if profile.is_empty()
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Cargo profile must be a simple alphanumeric/-/_ name".into());
    }
    Ok(())
}

fn validate_target(target: &str) -> Result<(), Box<dyn Error>> {
    if target.is_empty()
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Rust target triple contains unsupported characters".into());
    }
    Ok(())
}

fn rustc_info(repo: &Path) -> Result<ToolchainInfo, Box<dyn Error>> {
    let output = command_output(Command::new("rustc").arg("-vV").current_dir(repo))?;
    parse_rustc_info(&output)
}

fn parse_rustc_info(output: &str) -> Result<ToolchainInfo, Box<dyn Error>> {
    fn field(output: &str, prefix: &str) -> Result<String, Box<dyn Error>> {
        output
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::to_owned)
            .ok_or_else(|| format!("rustc -vV did not report {prefix:?}").into())
    }

    Ok(ToolchainInfo {
        release: field(output, "release: ")?,
        commit_hash: field(output, "commit-hash: ")?,
        host: field(output, "host: ")?,
        llvm_version: field(output, "LLVM version: ")?,
    })
}

fn tracked_worktree_dirty(repo: &Path) -> Result<bool, Box<dyn Error>> {
    Ok(!git_output(repo, &["status", "--porcelain"])?.is_empty())
}

fn source_date_epoch(repo: &Path) -> Result<u64, Box<dyn Error>> {
    if let Ok(value) = env::var("SOURCE_DATE_EPOCH") {
        return value
            .parse::<u64>()
            .map_err(|_| "SOURCE_DATE_EPOCH must be a non-negative integer".into());
    }
    git_output(repo, &["show", "-s", "--format=%ct", "HEAD"])?
        .parse::<u64>()
        .map_err(|_| "git commit timestamp is invalid".into())
}

fn release_platform(target: &str) -> Result<ReleasePlatform, Box<dyn Error>> {
    if target.contains("windows") {
        Ok(ReleasePlatform::Windows)
    } else if target.contains("apple-darwin") {
        Ok(ReleasePlatform::Macos)
    } else if target.contains("linux") {
        Ok(ReleasePlatform::Linux)
    } else {
        Err(format!("unsupported release target platform: {target}").into())
    }
}

fn payload_file(file: &ArchiveFile) -> Result<PayloadFile, Box<dyn Error>> {
    validate_archive_relative_path(&file.path)?;
    Ok(PayloadFile {
        path: file.path.clone(),
        size: file.bytes.len() as u64,
        sha256: sha256_bytes(&file.bytes),
        mode: format!("{:04o}", file.mode),
    })
}

fn archive_file(path: impl Into<String>, bytes: Vec<u8>, mode: u32) -> ArchiveFile {
    ArchiveFile {
        path: path.into(),
        bytes,
        mode,
    }
}

fn build_package_layout(
    platform: ReleasePlatform,
    _target: &str,
    binary: &[u8],
    macos_launcher: Option<&[u8]>,
    role_launcher: &[u8],
    readme: &[u8],
    branding: &BuildBranding,
    version: &str,
) -> Result<PackageLayout, Box<dyn Error>> {
    let mut layout = match platform {
        ReleasePlatform::Windows => PackageLayout {
            name: "windows-portable".into(),
            app_id: APP_ID.into(),
            entrypoint: "clew.exe".into(),
            cli_binary: "clew.exe".into(),
            files: vec![
                archive_file("clew.exe", binary.to_vec(), 0o755),
                archive_file("clew-role-launcher.exe", role_launcher.to_vec(), 0o755),
                archive_file("README.md", readme.to_vec(), 0o644),
            ],
        },
        ReleasePlatform::Macos => {
            let launcher = macos_launcher.ok_or("macOS package requires clew-app launcher")?;
            PackageLayout {
                name: "macos-app".into(),
                app_id: APP_ID.into(),
                entrypoint: "Clew.app/Contents/MacOS/Clew".into(),
                cli_binary: "Clew.app/Contents/Resources/clew".into(),
                files: vec![
                    archive_file(
                        "Clew.app/Contents/Info.plist",
                        macos_info_plist(version, &branding.profile.identity.app_display_name)
                            .into_bytes(),
                        0o644,
                    ),
                    archive_file("Clew.app/Contents/MacOS/Clew", launcher.to_vec(), 0o755),
                    archive_file(
                        "Clew.app/Contents/Resources/AppIcon.icns",
                        macos_icns(&branding.icon_bytes)?,
                        0o644,
                    ),
                    archive_file("Clew.app/Contents/Resources/clew", binary.to_vec(), 0o755),
                    archive_file(
                        format!("{MACOS_ROLE_APP}/Contents/Info.plist"),
                        macos_role_info_plist(version, &branding.profile.identity.app_display_name)
                            .into_bytes(),
                        0o644,
                    ),
                    archive_file(
                        format!("{MACOS_ROLE_APP}/Contents/MacOS/Clew Role"),
                        role_launcher.to_vec(),
                        0o755,
                    ),
                    archive_file(
                        format!("{MACOS_ROLE_APP}/Contents/Resources/AppIcon.icns"),
                        macos_icns(&branding.icon_bytes)?,
                        0o644,
                    ),
                    archive_file("README.md", readme.to_vec(), 0o644),
                ],
            }
        }
        ReleasePlatform::Linux => {
            let (icon_path, icon_bytes) = match branding.icon_format {
                BuildIconFormat::Svg => (
                    "share/icons/hicolor/scalable/apps/clew.svg",
                    branding.icon_bytes.clone(),
                ),
                BuildIconFormat::Png => (
                    "share/icons/hicolor/256x256/apps/clew.png",
                    render_icon_png(&branding.icon_bytes, 256)?,
                ),
            };
            PackageLayout {
                name: "linux-portable".into(),
                app_id: APP_ID.into(),
                entrypoint: "bin/clew".into(),
                cli_binary: "bin/clew".into(),
                files: vec![
                    archive_file("bin/clew", binary.to_vec(), 0o755),
                    archive_file("bin/clew-role-launcher", role_launcher.to_vec(), 0o755),
                    archive_file(
                        "share/applications/io.clew.app.desktop",
                        linux_desktop_entry(&branding.profile.identity.app_display_name)
                            .into_bytes(),
                        0o644,
                    ),
                    archive_file(icon_path, icon_bytes, 0o644),
                    archive_file("README.md", readme.to_vec(), 0o644),
                ],
            }
        }
    };
    layout
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    validate_layout(&layout)?;
    Ok(layout)
}

fn validate_layout(layout: &PackageLayout) -> Result<(), Box<dyn Error>> {
    validate_archive_relative_path(&layout.entrypoint)?;
    validate_archive_relative_path(&layout.cli_binary)?;
    let mut previous: Option<&str> = None;
    for file in &layout.files {
        validate_archive_relative_path(&file.path)?;
        if previous.is_some_and(|path| path >= file.path.as_str()) {
            return Err("release payload paths must be unique and strictly sorted".into());
        }
        previous = Some(&file.path);
    }
    if !layout
        .files
        .iter()
        .any(|file| file.path == layout.entrypoint)
    {
        return Err("release entrypoint is absent from payload".into());
    }
    if !layout
        .files
        .iter()
        .any(|file| file.path == layout.cli_binary)
    {
        return Err("release CLI binary is absent from payload".into());
    }
    Ok(())
}

fn validate_archive_relative_path(path: &str) -> Result<(), Box<dyn Error>> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!("unsafe release archive path: {path:?}").into());
    }
    Ok(())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn macos_info_plist(version: &str, app_name: &str) -> String {
    let app_name = xml_escape(app_name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>CFBundleDevelopmentRegion</key><string>en</string>\n  <key>CFBundleDisplayName</key><string>{app_name}</string>\n  <key>CFBundleExecutable</key><string>Clew</string>\n  <key>CFBundleIconFile</key><string>AppIcon</string>\n  <key>CFBundleIdentifier</key><string>{APP_ID}</string>\n  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>\n  <key>CFBundleName</key><string>{app_name}</string>\n  <key>CFBundlePackageType</key><string>APPL</string>\n  <key>CFBundleShortVersionString</key><string>{version}</string>\n  <key>CFBundleVersion</key><string>{version}</string>\n  <key>NSHighResolutionCapable</key><true/>\n</dict>\n</plist>\n"
    )
}

fn macos_role_info_plist(version: &str, app_name: &str) -> String {
    let app_name = xml_escape(app_name);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>CFBundleDevelopmentRegion</key><string>en</string>\n  <key>CFBundleDisplayName</key><string>{app_name}</string>\n  <key>CFBundleExecutable</key><string>Clew Role</string>\n  <key>CFBundleIconFile</key><string>AppIcon</string>\n  <key>CFBundleIdentifier</key><string>{APP_ID}.role</string>\n  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>\n  <key>CFBundleName</key><string>{app_name}</string>\n  <key>CFBundlePackageType</key><string>APPL</string>\n  <key>CFBundleShortVersionString</key><string>{version}</string>\n  <key>CFBundleVersion</key><string>{version}</string>\n  <key>NSHighResolutionCapable</key><true/>\n</dict>\n</plist>\n"
    )
}

fn linux_desktop_entry(app_name: &str) -> String {
    let app_name = app_name.replace('\\', "\\\\");
    format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName={app_name}\nComment=Agent-facing remote capability bridge\nTryExec=clew\nExec=clew gui\nIcon=clew\nTerminal=false\nCategories=Network;\nStartupNotify=true\n"
    )
}

fn macos_icns(source: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let specs = [
        (*b"icp4", 16_u32),
        (*b"icp5", 32_u32),
        (*b"icp6", 64_u32),
        (*b"ic07", 128_u32),
        (*b"ic08", 256_u32),
        (*b"ic09", 512_u32),
        (*b"ic10", 1024_u32),
    ];
    let mut chunks = Vec::with_capacity(specs.len());
    let mut total_len: u64 = 8;
    for (kind, size) in specs {
        let png = render_icon_png(source, size)?;
        total_len = total_len
            .checked_add(8_u64 + png.len() as u64)
            .ok_or("ICNS size overflow")?;
        chunks.push((kind, png));
    }
    let total_len = u32::try_from(total_len)?;
    let mut output = Vec::with_capacity(total_len as usize);
    output.extend_from_slice(b"icns");
    output.extend_from_slice(&total_len.to_be_bytes());
    for (kind, png) in chunks {
        output.extend_from_slice(&kind);
        let chunk_len = u32::try_from(8_usize + png.len())?;
        output.extend_from_slice(&chunk_len.to_be_bytes());
        output.extend_from_slice(&png);
    }
    Ok(output)
}

fn render_icon_png(source: &[u8], size: u32) -> Result<Vec<u8>, Box<dyn Error>> {
    let image = if source.starts_with(b"\x89PNG\r\n\x1a\n") {
        image::load_from_memory_with_format(source, ImageFormat::Png)?
            .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
    } else {
        let options = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_data(source, &options)?;
        let source_size = tree.size();
        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(size, size).ok_or("icon pixmap allocation failed")?;
        let transform = resvg::tiny_skia::Transform::from_scale(
            size as f32 / source_size.width(),
            size as f32 / source_size.height(),
        );
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let mut rgba = pixmap.take();
        unpremultiply_rgba(&mut rgba);
        RgbaImage::from_raw(size, size, rgba).ok_or("invalid rendered RGBA icon")?
    };
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut cursor, ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            let value = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = value.min(255) as u8;
        }
    }
}

fn run_cargo_build(
    repo: &Path,
    target: &str,
    profile: &str,
    source_date_epoch: u64,
    platform: ReleasePlatform,
    branding: &BuildBranding,
) -> Result<(), Box<dyn Error>> {
    let build = |binary: &str, feature: Option<&str>| -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("cargo");
        command.args(["build", "--locked", "--bin", binary]);
        if let Some(feature) = feature {
            command.args(["--features", feature]);
        }
        command
            .args(["--target", target, "--profile", profile])
            .env("SOURCE_DATE_EPOCH", source_date_epoch.to_string())
            .env("CARGO_INCREMENTAL", "0")
            .env(
                "CLEW_BUILD_APP_NAME",
                &branding.profile.identity.app_display_name,
            )
            .env("CLEW_BUILD_ICON_PATH", &branding.icon_path)
            .env("CLEW_BUILD_OUTFIT_KEY", &branding.build_cache_key);
        if let Some(publisher) = &branding.profile.identity.publisher_label {
            command.env("CLEW_BUILD_PUBLISHER", publisher);
        } else {
            command.env_remove("CLEW_BUILD_PUBLISHER");
        }
        let status = command.current_dir(repo).status()?;
        if !status.success() {
            return Err(format!("cargo build for {binary} failed with {status}").into());
        }
        Ok(())
    };

    build(PRODUCT, None)?;
    build("clew-role-launcher", Some("site-kit-role-launcher"))?;
    if platform == ReleasePlatform::Macos {
        build("clew-app", Some("macos-app-launcher"))?;
    }
    Ok(())
}

fn built_named_binary_path(repo: &Path, target: &str, profile: &str, name: &str) -> PathBuf {
    let binary = if target.contains("windows") {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    repo.join("target").join(target).join(profile).join(binary)
}

fn smoke_binary(binary: &Path) -> Result<(), Box<dyn Error>> {
    let version = command_output(Command::new(binary).arg("--version").stdin(Stdio::null()))?;
    let expected = format!("clew {}", env!("CARGO_PKG_VERSION"));
    if version.trim() != expected {
        return Err(format!(
            "unexpected packaged binary version: {version:?}, expected {expected:?}"
        )
        .into());
    }
    let status = Command::new(binary)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("packaged binary --help failed with {status}").into());
    }
    Ok(())
}

fn smoke_archive(
    archive_path: &Path,
    package_stem: &str,
    expected_payload: &PayloadManifest,
    execute_binary: bool,
) -> Result<(), Box<dyn Error>> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let mut expected_names = expected_payload
        .files
        .iter()
        .map(|file| format!("{package_stem}/{}", file.path))
        .collect::<Vec<_>>();
    expected_names.push(format!("{package_stem}/release-manifest.json"));
    expected_names.sort();
    let mut actual_names = (0..archive.len())
        .map(|index| archive.by_index(index).map(|entry| entry.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    actual_names.sort();
    if actual_names != expected_names {
        return Err("release archive contains an unexpected entry set".into());
    }

    let manifest_name = format!("{package_stem}/release-manifest.json");
    let manifest_bytes = read_zip_entry_bounded(
        &mut archive,
        &manifest_name,
        MAX_EMBEDDED_MANIFEST_BYTES,
        None,
    )?;
    let embedded: PayloadManifest = serde_json::from_slice(&manifest_bytes)?;
    if &embedded != expected_payload {
        return Err("embedded release manifest differs from packaging payload".into());
    }

    let temp = tempfile::tempdir()?;
    for record in &expected_payload.files {
        validate_archive_relative_path(&record.path)?;
        let entry_name = format!("{package_stem}/{}", record.path);
        let bytes =
            read_zip_entry_bounded(&mut archive, &entry_name, record.size, Some(record.size))?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(format!("archived payload hash differs for {}", record.path).into());
        }
        let extracted = temp.path().join(&record.path);
        if let Some(parent) = extracted.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&extracted, &bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = u32::from_str_radix(&record.mode, 8)?;
            fs::set_permissions(&extracted, fs::Permissions::from_mode(mode))?;
        }
    }
    let cli_binary = temp.path().join(&expected_payload.cli_binary);
    if !cli_binary.is_file() {
        return Err("release CLI binary was not extracted".into());
    }
    if execute_binary {
        smoke_binary(&cli_binary)?;
    }
    Ok(())
}

fn read_zip_entry_bounded(
    archive: &mut ZipArchive<File>,
    name: &str,
    max_size: u64,
    exact_size: Option<u64>,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut entry = archive.by_name(name)?;
    if entry.is_dir()
        || entry.size() > max_size
        || exact_size.is_some_and(|size| entry.size() != size)
    {
        return Err(format!("release archive entry has an invalid size/type: {name}").into());
    }
    let expected_size = entry.size();
    let mut bytes = Vec::new();
    entry
        .by_ref()
        .take(max_size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected_size {
        return Err(format!("release archive entry truncated while reading: {name}").into());
    }
    Ok(bytes)
}

fn write_zip(
    path: &Path,
    package_stem: &str,
    files: &[ArchiveFile],
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    write_named_zip(
        path,
        package_stem,
        files,
        "release-manifest.json",
        payload_manifest,
    )
}

fn write_site_kit_archive(
    platform: ReleasePlatform,
    path: &Path,
    package_stem: &str,
    files: &[ArchiveFile],
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    match platform {
        ReleasePlatform::Windows | ReleasePlatform::Macos => write_named_zip(
            path,
            package_stem,
            files,
            "site-kit-manifest.json",
            payload_manifest,
        ),
        ReleasePlatform::Linux => {
            write_site_kit_tar_gz(path, package_stem, files, payload_manifest)
        }
    }
}

fn verify_zip_site_kit(
    archive_path: &Path,
    package_stem: &str,
    expected: &SiteKitPayloadManifest,
) -> Result<(), Box<dyn Error>> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let mut expected_names = expected
        .files
        .iter()
        .map(|file| format!("{package_stem}/{}", file.path))
        .collect::<Vec<_>>();
    expected_names.push(format!("{package_stem}/site-kit-manifest.json"));
    expected_names.sort();
    let mut actual_names = (0..archive.len())
        .map(|index| archive.by_index(index).map(|entry| entry.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    actual_names.sort();
    if actual_names != expected_names {
        return Err("Site Kit ZIP contains an unexpected entry set".into());
    }
    for record in &expected.files {
        let name = format!("{package_stem}/{}", record.path);
        let bytes = read_zip_entry_bounded(&mut archive, &name, record.size, Some(record.size))?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(format!("Site Kit ZIP payload hash differs for {}", record.path).into());
        }
    }
    let manifest_name = format!("{package_stem}/site-kit-manifest.json");
    let manifest = read_zip_entry_bounded(
        &mut archive,
        &manifest_name,
        MAX_EMBEDDED_MANIFEST_BYTES,
        None,
    )?;
    let embedded: SiteKitPayloadManifest = serde_json::from_slice(&manifest)?;
    if &embedded != expected {
        return Err("embedded Site Kit manifest differs from sidecar payload".into());
    }
    Ok(())
}

fn verify_release_ready_windows_site_kit(
    archive_path: &Path,
    package_stem: &str,
    payload: &SiteKitPayloadManifest,
) -> Result<(), Box<dyn Error>> {
    if !cfg!(windows) {
        return Err("release-ready Windows Site Kit verification must run on Windows".into());
    }
    let signtool = resolve_windows_signtool(None)?;
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let temp = tempfile::tempdir()?;
    let signed_paths = [
        ".clew-runtime/clew.exe",
        "1 Use this computer/Clew.exe",
        "2 Help nearby computers/Clew.exe",
    ];
    for (index, path) in signed_paths.iter().enumerate() {
        let record = payload
            .files
            .iter()
            .find(|record| record.path == *path)
            .ok_or_else(|| format!("release-ready Windows Site Kit omitted {path}"))?;
        let name = format!("{package_stem}/{path}");
        let bytes = read_zip_entry_bounded(&mut archive, &name, record.size, Some(record.size))?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(format!("release-ready Windows Site Kit hash differs for {path}").into());
        }
        let executable = temp.path().join(format!("site-kit-{index}.exe"));
        fs::write(&executable, bytes)?;
        verify_windows_signature(&signtool, &executable)?;
    }
    Ok(())
}

fn write_release_ready_macos_site_kit(
    release_archive: &Path,
    release_stem: &str,
    archive_path: &Path,
    package_stem: &str,
    files: &[ArchiveFile],
    payload: &SiteKitPayloadManifest,
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("release-ready macOS Site Kit assembly must run on macOS".into());
    }
    let temp = tempfile::tempdir()?;
    let release_extract = temp.path().join("release");
    fs::create_dir_all(&release_extract)?;
    let mut extract = Command::new("/usr/bin/ditto");
    extract
        .args(["-x", "-k"])
        .arg(release_archive)
        .arg(&release_extract);
    command_output_combined(&mut extract)?;
    let release_root = release_extract.join(release_stem);
    if !release_root.is_dir() {
        return Err("signed macOS release ZIP did not contain its expected root directory".into());
    }

    let kit_parent = temp.path().join("site-kit");
    let kit_root = kit_parent.join(package_stem);
    fs::create_dir_all(&kit_root)?;
    for file in files {
        if is_macos_site_kit_app_path(&file.path) {
            continue;
        }
        let target = kit_root.join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, &file.bytes)?;
        set_recorded_mode(&target, &format!("{:04o}", file.mode))?;
    }
    copy_with_ditto(
        &release_root.join("Clew.app"),
        &kit_root.join(".clew-runtime").join("Clew.app"),
    )?;
    for role_dir in [USE_ROLE_DIR, HELPER_ROLE_DIR] {
        copy_with_ditto(
            &release_root.join(MACOS_ROLE_APP),
            &kit_root.join(role_dir).join("Clew.app"),
        )?;
    }
    let embedded_manifest = kit_root.join("site-kit-manifest.json");
    fs::write(&embedded_manifest, payload_manifest)?;
    set_recorded_mode(&embedded_manifest, "0644")?;
    verify_site_kit_staging_tree(&kit_root, files, payload_manifest)?;
    verify_macos_site_kit_apps(&kit_root)?;

    let mut package = Command::new("/usr/bin/ditto");
    package
        .args(["-c", "-k", "--keepParent"])
        .arg(&kit_root)
        .arg(archive_path);
    command_output_combined(&mut package)?;

    let final_extract = temp.path().join("final");
    fs::create_dir_all(&final_extract)?;
    let mut final_unpack = Command::new("/usr/bin/ditto");
    final_unpack
        .args(["-x", "-k"])
        .arg(archive_path)
        .arg(&final_extract);
    command_output_combined(&mut final_unpack)?;
    let final_root = final_extract.join(package_stem);
    verify_site_kit_staging_tree(&final_root, files, payload_manifest)?;
    verify_macos_site_kit_apps(&final_root)?;
    let final_manifest: SiteKitPayloadManifest =
        serde_json::from_slice(&fs::read(final_root.join("site-kit-manifest.json"))?)?;
    if &final_manifest != payload {
        return Err("final macOS Site Kit manifest changed during ditto packaging".into());
    }
    Ok(())
}

fn is_macos_site_kit_app_path(path: &str) -> bool {
    path.starts_with(".clew-runtime/Clew.app/")
        || path.starts_with("1 Use this computer/Clew.app/")
        || path.starts_with("2 Help nearby computers/Clew.app/")
}

fn copy_with_ditto(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut command = Command::new("/usr/bin/ditto");
    command.arg(source).arg(target);
    command_output_combined(&mut command)?;
    Ok(())
}

fn verify_site_kit_staging_tree(
    root: &Path,
    expected_files: &[ArchiveFile],
    expected_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    let mut actual_paths = Vec::new();
    collect_regular_paths(root, root, &mut actual_paths)?;
    actual_paths.sort();
    let mut expected_paths = expected_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    expected_paths.push("site-kit-manifest.json".into());
    expected_paths.sort();
    if actual_paths != expected_paths {
        return Err("macOS Site Kit staging tree has an unexpected regular-file set".into());
    }
    for expected in expected_files {
        let path = root.join(&expected.path);
        let bytes = fs::read(&path)?;
        if bytes.len() != expected.bytes.len()
            || sha256_bytes(&bytes) != sha256_bytes(&expected.bytes)
        {
            return Err(
                format!("macOS Site Kit staging hash differs for {}", expected.path).into(),
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
            if mode != expected.mode {
                return Err(format!(
                    "macOS Site Kit staging mode differs for {}: {mode:04o} != {:04o}",
                    expected.path, expected.mode
                )
                .into());
            }
        }
    }
    if fs::read(root.join("site-kit-manifest.json"))? != expected_manifest {
        return Err("macOS Site Kit staging embedded manifest changed".into());
    }
    Ok(())
}

fn verify_macos_site_kit_apps(root: &Path) -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("macOS Site Kit native verification must run on macOS".into());
    }
    let main = root.join(".clew-runtime").join("Clew.app");
    verify_macos_signed_code(&main.join("Contents").join("Resources").join("clew"))?;
    verify_macos_stapled_app(&main)?;
    for role_dir in [USE_ROLE_DIR, HELPER_ROLE_DIR] {
        let app = root.join(role_dir).join("Clew.app");
        verify_macos_signed_code(&app.join("Contents").join("MacOS").join("Clew Role"))?;
        verify_macos_stapled_app(&app)?;
    }
    Ok(())
}

fn verify_macos_stapled_app(app: &Path) -> Result<(), Box<dyn Error>> {
    verify_macos_signed_code(app)?;
    let mut staple = Command::new("xcrun");
    staple.args(["stapler", "validate"]).arg(app);
    command_output_combined(&mut staple)?;
    let mut assess = Command::new("spctl");
    assess
        .args(["--assess", "--type", "exec", "--verbose=4"])
        .arg(app);
    command_output_combined(&mut assess)?;
    Ok(())
}

fn write_named_zip(
    path: &Path,
    package_stem: &str,
    files: &[ArchiveFile],
    manifest_name: &str,
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    validate_archive_relative_path(manifest_name)?;
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let timestamp = DateTime::default();
    for payload in files {
        validate_archive_relative_path(&payload.path)?;
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(timestamp)
            .unix_permissions(payload.mode);
        zip.start_file(format!("{package_stem}/{}", payload.path), options)?;
        zip.write_all(&payload.bytes)?;
    }
    let regular = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(timestamp)
        .unix_permissions(0o644);
    zip.start_file(format!("{package_stem}/{manifest_name}"), regular)?;
    zip.write_all(payload_manifest)?;
    let file = zip.finish()?;
    file.sync_all()?;
    Ok(())
}

fn write_site_kit_tar_gz(
    path: &Path,
    package_stem: &str,
    files: &[ArchiveFile],
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut encoder = GzEncoder::new(file, Compression::best());
    for payload in files {
        validate_archive_relative_path(&payload.path)?;
        write_tar_entry(
            &mut encoder,
            &format!("{package_stem}/{}", payload.path),
            &payload.bytes,
            payload.mode,
        )?;
    }
    write_tar_entry(
        &mut encoder,
        &format!("{package_stem}/site-kit-manifest.json"),
        payload_manifest,
        0o644,
    )?;
    encoder.write_all(&[0_u8; 1024])?;
    let file = encoder.finish()?;
    file.sync_all()?;
    Ok(())
}

fn write_tar_entry(
    writer: &mut impl Write,
    path: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), Box<dyn Error>> {
    let (prefix, name) = split_ustar_path(path)?;
    let mut header = [0_u8; 512];
    copy_tar_field(&mut header[0..100], name.as_bytes())?;
    write_tar_octal(&mut header[100..108], u64::from(mode))?;
    write_tar_octal(&mut header[108..116], 0)?;
    write_tar_octal(&mut header[116..124], 0)?;
    write_tar_octal(&mut header[124..136], bytes.len() as u64)?;
    write_tar_octal(&mut header[136..148], 0)?;
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    if !prefix.is_empty() {
        copy_tar_field(&mut header[345..500], prefix.as_bytes())?;
    }
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
    let checksum_text = format!("{checksum:06o}\0 ");
    if checksum_text.len() != 8 {
        return Err("tar checksum overflow".into());
    }
    header[148..156].copy_from_slice(checksum_text.as_bytes());
    writer.write_all(&header)?;
    writer.write_all(bytes)?;
    let padding = (512 - (bytes.len() % 512)) % 512;
    if padding != 0 {
        writer.write_all(&vec![0_u8; padding])?;
    }
    Ok(())
}

fn split_ustar_path(path: &str) -> Result<(&str, &str), Box<dyn Error>> {
    if path.as_bytes().len() <= 100 {
        return Ok(("", path));
    }
    for (index, _) in path.match_indices('/').rev() {
        let prefix = &path[..index];
        let name = &path[index + 1..];
        if prefix.as_bytes().len() <= 155 && name.as_bytes().len() <= 100 {
            return Ok((prefix, name));
        }
    }
    Err(format!("Site Kit path exceeds ustar bounds: {path}").into())
}

fn copy_tar_field(target: &mut [u8], value: &[u8]) -> Result<(), Box<dyn Error>> {
    if value.len() > target.len() {
        return Err("tar field exceeds fixed width".into());
    }
    target[..value.len()].copy_from_slice(value);
    Ok(())
}

fn write_tar_octal(target: &mut [u8], value: u64) -> Result<(), Box<dyn Error>> {
    if target.len() < 2 {
        return Err("tar octal field is too short".into());
    }
    let digits = format!("{value:o}");
    if digits.len() + 1 > target.len() {
        return Err("tar octal value exceeds fixed width".into());
    }
    let start = target.len() - digits.len() - 1;
    target[..start].fill(b'0');
    target[start..start + digits.len()].copy_from_slice(digits.as_bytes());
    target[target.len() - 1] = 0;
    Ok(())
}

fn refresh_checksums(out_dir: &Path) -> Result<(), Box<dyn Error>> {
    let mut artifacts = fs::read_dir(out_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && (path.extension().is_some_and(|extension| extension == "zip")
                    || path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".release.json")))
        })
        .collect::<Vec<_>>();
    artifacts.sort();
    let mut output = String::new();
    for artifact in artifacts {
        let name = artifact
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("release artifact filename is not UTF-8")?;
        output.push_str(&format!("{}  {name}\n", sha256_file(&artifact)?));
    }
    fs::write(out_dir.join("SHA256SUMS"), output)?;
    Ok(())
}

fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn Error>> {
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
    Ok(hex_digest(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize())
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    command_output(Command::new("git").args(args).current_dir(repo))
}

fn command_output(command: &mut Command) -> Result<String, Box<dyn Error>> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "command failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn command_output_combined(command: &mut Command) -> Result<String, Box<dyn Error>> {
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");
    if !output.status.success() {
        return Err(format!("command failed with {}: {}", output.status, combined.trim()).into());
    }
    Ok(combined.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_and_profile_validation_are_bounded() {
        assert!(validate_target("x86_64-pc-windows-msvc").is_ok());
        assert!(validate_target("../escape").is_err());
        assert!(validate_profile("release").is_ok());
        assert!(validate_profile("release/dev").is_err());
    }

    #[test]
    fn site_kit_labels_and_linux_tar_are_bounded_and_deterministic() {
        assert_eq!(sanitize_site_label(" Alice/Lab:*? ").unwrap(), "Alice-Lab");
        assert!(sanitize_site_label("   ").is_err());
        let temp = tempfile::tempdir().unwrap();
        let files = vec![
            archive_file("1 Use this computer/Clew", b"launcher".to_vec(), 0o755),
            archive_file("site.clew", b"signed-site".to_vec(), 0o600),
        ];
        let first = temp.path().join("first.tar.gz");
        let second = temp.path().join("second.tar.gz");
        write_site_kit_tar_gz(&first, "Lab-Clew-Linux", &files, b"{}\n").unwrap();
        write_site_kit_tar_gz(&second, "Lab-Clew-Linux", &files, b"{}\n").unwrap();
        assert_eq!(sha256_file(&first).unwrap(), sha256_file(&second).unwrap());

        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(File::open(&first).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        assert!(decoded.ends_with(&[0_u8; 1024]));
        let mut names = Vec::new();
        let mut offset = 0_usize;
        while offset + 512 <= decoded.len() {
            let header = &decoded[offset..offset + 512];
            if header.iter().all(|byte| *byte == 0) {
                break;
            }
            let field = |range: std::ops::Range<usize>| {
                let bytes = &header[range];
                let end = bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(bytes.len());
                String::from_utf8(bytes[..end].to_vec()).unwrap()
            };
            let name = field(0..100);
            let prefix = field(345..500);
            names.push(if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            });
            let size_text = std::str::from_utf8(&header[124..136])
                .unwrap()
                .trim_matches(char::from(0))
                .trim();
            let size = usize::from_str_radix(size_text, 8).unwrap_or(0);
            offset += 512 + size.div_ceil(512) * 512;
        }
        assert_eq!(
            names,
            vec![
                "Lab-Clew-Linux/1 Use this computer/Clew",
                "Lab-Clew-Linux/site.clew",
                "Lab-Clew-Linux/site-kit-manifest.json",
            ]
        );
    }

    #[test]
    fn release_platform_and_archive_paths_fail_closed() {
        assert_eq!(
            release_platform("x86_64-pc-windows-msvc").unwrap(),
            ReleasePlatform::Windows
        );
        assert_eq!(
            release_platform("aarch64-apple-darwin").unwrap(),
            ReleasePlatform::Macos
        );
        assert_eq!(
            release_platform("x86_64-unknown-linux-gnu").unwrap(),
            ReleasePlatform::Linux
        );
        assert!(release_platform("wasm32-wasip2").is_err());
        for unsafe_path in ["", "/clew", "../clew", "a/../clew", "a\\clew", "a//clew"] {
            assert!(validate_archive_relative_path(unsafe_path).is_err());
        }
        assert!(validate_archive_relative_path("Clew.app/Contents/MacOS/Clew").is_ok());
    }

    #[test]
    fn signing_inputs_are_bounded_and_secret_free() {
        assert_eq!(
            normalize_certificate_sha1(
                "AA BB:CC DD EE FF 00 11 22 33 44 55 66 77 88 99 AA BB CC DD"
            )
            .unwrap(),
            "AABBCCDDEEFF00112233445566778899AABBCCDD"
        );
        assert!(normalize_certificate_sha1("abcd").is_err());
        assert!(validate_timestamp_url("https://timestamp.example.test").is_ok());
        assert!(validate_timestamp_url("file:///secret").is_err());
        assert!(
            validate_signing_label("Developer ID Application: Example (TEAMID)", "identity")
                .is_ok()
        );
        assert!(validate_signing_label(" padded ", "identity").is_err());
        assert!(validate_signing_label("line\nbreak", "identity").is_err());
    }

    #[test]
    fn unsigned_schema_two_omits_signing_and_signed_schema_three_roundtrips() {
        let mut payload = sample_payload(ReleasePlatform::Windows);
        let unsigned = json_bytes(&payload).unwrap();
        let unsigned_text = std::str::from_utf8(&unsigned).unwrap();
        assert!(!unsigned_text.contains("\"signing\""));
        assert_eq!(
            serde_json::from_slice::<PayloadManifest>(&unsigned).unwrap(),
            payload
        );

        payload.schema_version = SIGNED_RELEASE_SCHEMA_VERSION;
        payload.unsigned = false;
        payload.signing = Some(SigningInfo {
            mechanism: "windows-authenticode".into(),
            identity: "A".repeat(40),
            timestamped: true,
            notarized: false,
            stapled: false,
            notary_submission_id: None,
        });
        let signed = json_bytes(&payload).unwrap();
        assert!(
            std::str::from_utf8(&signed)
                .unwrap()
                .contains("\"signing\"")
        );
        assert_eq!(
            serde_json::from_slice::<PayloadManifest>(&signed).unwrap(),
            payload
        );
    }

    #[test]
    fn signable_artifact_rejects_dirty_signed_and_linux_inputs() {
        let windows = sample_artifact(sample_payload(ReleasePlatform::Windows));
        assert!(validate_signable_artifact(&windows).is_ok());

        let mut dirty = windows.clone();
        dirty.payload.dirty = true;
        assert!(validate_signable_artifact(&dirty).is_err());

        let mut already_signed = windows.clone();
        already_signed.payload.schema_version = SIGNED_RELEASE_SCHEMA_VERSION;
        already_signed.payload.unsigned = false;
        already_signed.payload.signing = Some(SigningInfo {
            mechanism: "windows-authenticode".into(),
            identity: "A".repeat(40),
            timestamped: true,
            notarized: false,
            stapled: false,
            notary_submission_id: None,
        });
        assert!(validate_signable_artifact(&already_signed).is_err());

        let linux = sample_artifact(sample_payload(ReleasePlatform::Linux));
        assert!(validate_signable_artifact(&linux).is_err());
    }

    #[test]
    fn release_payload_shape_rejects_extra_files_and_wrong_modes() {
        let windows = sample_artifact(sample_payload(ReleasePlatform::Windows));
        assert!(validate_unsigned_artifact(&windows).is_ok());

        let mut extra = windows.clone();
        extra.payload.files.push(PayloadFile {
            path: "evil.dll".into(),
            size: 4,
            sha256: sha256_bytes(b"evil"),
            mode: "0644".into(),
        });
        assert!(validate_unsigned_artifact(&extra).is_err());

        let mut wrong_mode = windows;
        wrong_mode
            .payload
            .files
            .iter_mut()
            .find(|file| file.path == "clew.exe")
            .unwrap()
            .mode = "0644".into();
        assert!(validate_unsigned_artifact(&wrong_mode).is_err());
    }

    #[test]
    fn signed_release_metadata_and_macos_code_resources_are_strict() {
        let mut windows = sample_artifact(sample_payload(ReleasePlatform::Windows));
        windows.payload.schema_version = SIGNED_RELEASE_SCHEMA_VERSION;
        windows.payload.unsigned = false;
        windows.payload.signing = Some(SigningInfo {
            mechanism: "windows-authenticode".into(),
            identity: "A".repeat(40),
            timestamped: true,
            notarized: false,
            stapled: false,
            notary_submission_id: None,
        });
        assert_eq!(
            validate_signed_artifact(&windows).unwrap(),
            ReleasePlatform::Windows
        );
        windows.payload.signing.as_mut().unwrap().timestamped = false;
        assert!(validate_signed_artifact(&windows).is_err());

        let mut macos = sample_artifact(sample_payload(ReleasePlatform::Macos));
        macos.payload.schema_version = SIGNED_RELEASE_SCHEMA_VERSION;
        macos.payload.unsigned = false;
        macos.payload.files.insert(
            4,
            PayloadFile {
                path: MACOS_CODE_RESOURCES.into(),
                size: 6,
                sha256: sha256_bytes(b"sealed"),
                mode: "0644".into(),
            },
        );
        macos.payload.signing = Some(SigningInfo {
            mechanism: "macos-developer-id-notarized".into(),
            identity: "Developer ID Application: Example (TEAMID)".into(),
            timestamped: true,
            notarized: true,
            stapled: true,
            notary_submission_id: Some("00000000-0000-0000-0000-000000000000".into()),
        });
        assert_eq!(
            validate_signed_artifact(&macos).unwrap(),
            ReleasePlatform::Macos
        );
        macos
            .payload
            .files
            .retain(|file| file.path != MACOS_CODE_RESOURCES);
        assert!(validate_signed_artifact(&macos).is_err());
    }

    #[test]
    fn signed_file_collection_allows_only_macos_code_resources() {
        let temp = tempfile::tempdir().unwrap();
        let base = PayloadFile {
            path: "Clew.app/Contents/Info.plist".into(),
            size: 5,
            sha256: sha256_bytes(b"plist"),
            mode: "0644".into(),
        };
        let plist = temp.path().join(&base.path);
        fs::create_dir_all(plist.parent().unwrap()).unwrap();
        fs::write(&plist, b"plist").unwrap();
        set_recorded_mode(&plist, &base.mode).unwrap();
        let resources = temp.path().join(MACOS_CODE_RESOURCES);
        fs::create_dir_all(resources.parent().unwrap()).unwrap();
        fs::write(&resources, b"signed-resources").unwrap();
        set_recorded_mode(&resources, "0644").unwrap();

        let files =
            collect_signed_files(temp.path(), ReleasePlatform::Macos, &[base.clone()]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|file| file.path == base.path));
        assert!(files.iter().any(|file| file.path == MACOS_CODE_RESOURCES));

        fs::write(temp.path().join("unexpected.txt"), b"nope").unwrap();
        assert!(collect_signed_files(temp.path(), ReleasePlatform::Macos, &[base]).is_err());
    }

    #[test]
    fn target_layouts_bind_native_identity_and_entrypoints() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><rect width="16" height="16" fill="#123456"/></svg>"##;
        let mut profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        profile.identity.app_display_name = "Lab Connect".into();
        let branding = BuildBranding {
            build_cache_key: profile.build_cache_key().unwrap(),
            profile,
            icon_bytes: svg.to_vec(),
            icon_path: PathBuf::from("app.svg"),
            icon_format: BuildIconFormat::Svg,
            icon_asset_id: None,
        };
        let windows = build_package_layout(
            ReleasePlatform::Windows,
            "x86_64-pc-windows-msvc",
            b"win-binary",
            None,
            b"role-launcher",
            b"readme",
            &branding,
            "1.2.3",
        )
        .unwrap();
        assert_eq!(windows.name, "windows-portable");
        assert_eq!(windows.entrypoint, "clew.exe");
        assert_eq!(windows.cli_binary, "clew.exe");
        assert_eq!(windows.files.len(), 3);
        assert!(
            windows
                .files
                .iter()
                .any(|file| file.path == "clew-role-launcher.exe" && file.mode == 0o755)
        );

        let macos = build_package_layout(
            ReleasePlatform::Macos,
            "aarch64-apple-darwin",
            b"mac-cli",
            Some(b"mach-o-launcher"),
            b"role-launcher",
            b"readme",
            &branding,
            "1.2.3",
        )
        .unwrap();
        assert_eq!(macos.name, "macos-app");
        assert_eq!(macos.app_id, APP_ID);
        assert_eq!(macos.entrypoint, "Clew.app/Contents/MacOS/Clew");
        assert_eq!(macos.cli_binary, "Clew.app/Contents/Resources/clew");
        let plist = macos
            .files
            .iter()
            .find(|file| file.path == "Clew.app/Contents/Info.plist")
            .unwrap();
        let plist = std::str::from_utf8(&plist.bytes).unwrap();
        assert!(plist.contains("<string>io.clew.app</string>"));
        assert!(plist.contains("<string>Lab Connect</string>"));
        assert!(plist.contains("<key>CFBundleExecutable</key><string>Clew</string>"));
        assert!(plist.contains("<string>1.2.3</string>"));
        let role_plist = macos
            .files
            .iter()
            .find(|file| file.path == format!("{MACOS_ROLE_APP}/Contents/Info.plist"))
            .unwrap();
        let role_plist = std::str::from_utf8(&role_plist.bytes).unwrap();
        assert!(role_plist.contains("<string>io.clew.app.role</string>"));
        assert!(role_plist.contains("<key>CFBundleExecutable</key><string>Clew Role</string>"));
        assert!(macos.files.iter().any(|file| {
            file.path == format!("{MACOS_ROLE_APP}/Contents/MacOS/Clew Role")
                && file.mode == 0o755
                && file.bytes == b"role-launcher"
        }));
        let icns = macos
            .files
            .iter()
            .find(|file| file.path.ends_with("AppIcon.icns"))
            .unwrap();
        assert_eq!(&icns.bytes[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(icns.bytes[4..8].try_into().unwrap()) as usize,
            icns.bytes.len()
        );
        for kind in [
            b"icp4", b"icp5", b"icp6", b"ic07", b"ic08", b"ic09", b"ic10",
        ] {
            assert!(icns.bytes.windows(4).any(|window| window == kind));
        }

        let linux = build_package_layout(
            ReleasePlatform::Linux,
            "x86_64-unknown-linux-gnu",
            b"linux-binary",
            None,
            b"role-launcher",
            b"readme",
            &branding,
            "1.2.3",
        )
        .unwrap();
        assert_eq!(linux.name, "linux-portable");
        assert_eq!(linux.entrypoint, "bin/clew");
        let desktop = linux
            .files
            .iter()
            .find(|file| file.path.ends_with("io.clew.app.desktop"))
            .unwrap();
        let desktop = std::str::from_utf8(&desktop.bytes).unwrap();
        assert!(desktop.contains("Name=Lab Connect\n"));
        assert!(desktop.contains("TryExec=clew"));
        assert!(desktop.contains("Exec=clew gui"));
        assert!(desktop.contains("Terminal=false"));
        assert!(
            linux
                .files
                .iter()
                .any(|file| file.path == "share/icons/hicolor/scalable/apps/clew.svg")
        );
        assert!(
            linux
                .files
                .iter()
                .any(|file| file.path == "bin/clew-role-launcher" && file.mode == 0o755)
        );

        let mut png_branding = branding.clone();
        png_branding.icon_bytes = render_icon_png(svg, 32).unwrap();
        png_branding.icon_format = BuildIconFormat::Png;
        png_branding.icon_asset_id = Some(format!("sha256-{}", "a".repeat(64)));
        let linux_png = build_package_layout(
            ReleasePlatform::Linux,
            "x86_64-unknown-linux-gnu",
            b"linux-binary",
            None,
            b"role-launcher",
            b"readme",
            &png_branding,
            "1.2.3",
        )
        .unwrap();
        assert!(linux_png.files.iter().any(|file| {
            file.path == "share/icons/hicolor/256x256/apps/clew.png"
                && file.bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        }));
    }

    #[test]
    fn rustc_verbose_version_is_captured_for_reproduction() {
        let info = parse_rustc_info(
            "rustc 1.96.0 (abc123 2026-05-25)\ncommit-hash: abc123\nhost: x86_64-pc-windows-msvc\nrelease: 1.96.0\nLLVM version: 22.1.2",
        )
        .unwrap();
        assert_eq!(info.release, "1.96.0");
        assert_eq!(info.commit_hash, "abc123");
        assert_eq!(info.host, "x86_64-pc-windows-msvc");
        assert_eq!(info.llvm_version, "22.1.2");
    }

    #[test]
    fn checksum_file_covers_archive_and_release_sidecar_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.zip"), b"archive").unwrap();
        fs::write(temp.path().join("a.release.json"), b"manifest").unwrap();
        fs::write(temp.path().join("notes.txt"), b"ignore").unwrap();
        refresh_checksums(temp.path()).unwrap();
        let sums = fs::read_to_string(temp.path().join("SHA256SUMS")).unwrap();
        assert!(sums.contains("  a.zip\n"));
        assert!(sums.contains("  a.release.json\n"));
        assert!(!sums.contains("notes.txt"));
    }

    fn sample_payload(platform: ReleasePlatform) -> PayloadManifest {
        let (target, layout, entrypoint, cli_binary, files) = match platform {
            ReleasePlatform::Windows => (
                "x86_64-pc-windows-msvc",
                "windows-portable",
                "clew.exe",
                "clew.exe",
                vec![
                    PayloadFile {
                        path: "README.md".into(),
                        size: 6,
                        sha256: sha256_bytes(b"readme"),
                        mode: "0644".into(),
                    },
                    PayloadFile {
                        path: "clew.exe".into(),
                        size: 3,
                        sha256: sha256_bytes(b"exe"),
                        mode: "0755".into(),
                    },
                ],
            ),
            ReleasePlatform::Macos => (
                "x86_64-apple-darwin",
                "macos-app",
                "Clew.app/Contents/MacOS/Clew",
                "Clew.app/Contents/Resources/clew",
                vec![
                    PayloadFile {
                        path: "Clew.app/Contents/Info.plist".into(),
                        size: 5,
                        sha256: sha256_bytes(b"plist"),
                        mode: "0644".into(),
                    },
                    PayloadFile {
                        path: "Clew.app/Contents/MacOS/Clew".into(),
                        size: 3,
                        sha256: sha256_bytes(b"app"),
                        mode: "0755".into(),
                    },
                    PayloadFile {
                        path: "Clew.app/Contents/Resources/AppIcon.icns".into(),
                        size: 4,
                        sha256: sha256_bytes(b"icon"),
                        mode: "0644".into(),
                    },
                    PayloadFile {
                        path: "Clew.app/Contents/Resources/clew".into(),
                        size: 3,
                        sha256: sha256_bytes(b"cli"),
                        mode: "0755".into(),
                    },
                    PayloadFile {
                        path: "README.md".into(),
                        size: 6,
                        sha256: sha256_bytes(b"readme"),
                        mode: "0644".into(),
                    },
                ],
            ),
            ReleasePlatform::Linux => (
                "x86_64-unknown-linux-gnu",
                "linux-portable",
                "bin/clew",
                "bin/clew",
                vec![PayloadFile {
                    path: "bin/clew".into(),
                    size: 3,
                    sha256: sha256_bytes(b"elf"),
                    mode: "0755".into(),
                }],
            ),
        };
        PayloadManifest {
            schema_version: RELEASE_SCHEMA_VERSION,
            product: PRODUCT.into(),
            version: "1.2.3".into(),
            target: target.into(),
            profile: "release".into(),
            archive_format: "zip".into(),
            layout: layout.into(),
            app_id: APP_ID.into(),
            entrypoint: entrypoint.into(),
            cli_binary: cli_binary.into(),
            source_commit: "0".repeat(40),
            source_date_epoch: 1,
            rustc: ToolchainInfo {
                release: "1.96.0".into(),
                commit_hash: "0".repeat(40),
                host: target.into(),
                llvm_version: "22.1.0".into(),
            },
            cargo_lock_sha256: "0".repeat(64),
            dirty: false,
            unsigned: true,
            signing: None,
            client_flavor: None,
            site_kit_launcher: None,
            files,
        }
    }

    fn sample_artifact(payload: PayloadManifest) -> ArtifactManifest {
        ArtifactManifest {
            artifact: ArtifactInfo {
                file: format!("clew-v{}-{}.zip", payload.version, payload.target),
                size: 1,
                sha256: "0".repeat(64),
            },
            payload,
        }
    }

    #[test]
    fn json_encoding_is_newline_terminated() {
        #[derive(Serialize)]
        struct Value {
            stable: bool,
        }
        let encoded = json_bytes(&Value { stable: true }).unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
    }
}
