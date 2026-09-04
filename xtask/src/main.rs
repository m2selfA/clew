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
use image::{DynamicImage, ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

const PRODUCT: &str = "clew";
const APP_NAME: &str = "Clew";
const APP_ID: &str = "io.clew.app";
const RELEASE_SCHEMA_VERSION: u32 = 2;
const SIGNED_RELEASE_SCHEMA_VERSION: u32 = 3;
const MAX_EMBEDDED_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIGNED_PAYLOAD_FILES: usize = 128;
const MAX_SIGNED_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SIGNED_PAYLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MACOS_CODE_RESOURCES: &str = "Clew.app/Contents/_CodeSignature/CodeResources";

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
        } => package(
            &repo,
            target,
            &profile,
            &out_dir,
            no_build,
            allow_dirty,
            skip_smoke,
        )?,
        Task::SignPackage {
            manifest,
            out_dir,
            signer,
        } => sign_package(&repo, &manifest, &out_dir, signer)?,
    }
    Ok(())
}

fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask manifest has no repository parent".into())
}

fn package(
    repo: &Path,
    target: Option<String>,
    profile: &str,
    out_dir: &Path,
    no_build: bool,
    allow_dirty: bool,
    skip_smoke: bool,
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
    if !no_build {
        run_cargo_build(repo, &target, profile, source_date_epoch, platform)?;
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
    let app_svg = fs::read(repo.join("assets/icons/app.svg"))?;
    let layout = build_package_layout(
        platform,
        &target,
        &binary_bytes,
        macos_launcher.as_deref(),
        &readme_bytes,
        &app_svg,
        env!("CARGO_PKG_VERSION"),
    )?;
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

fn validate_signable_artifact(manifest: &ArtifactManifest) -> Result<(), Box<dyn Error>> {
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
    if payload.dirty {
        return Err("dirty artifacts are never accepted by the release signing pipeline".into());
    }
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
        ReleasePlatform::Linux => {
            return Err("Linux release signing is not defined in V6b-3".into());
        }
        _ => return Err("release sidecar layout does not match its target platform".into()),
    }
    if payload.files.is_empty() || payload.files.len() > MAX_SIGNED_PAYLOAD_FILES {
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
        let _ = u32::from_str_radix(&file.mode, 8)?;
    }
    Ok(())
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
        ReleasePlatform::Macos => BTreeSet::from([MACOS_CODE_RESOURCES]),
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
        } else if platform == ReleasePlatform::Macos && path == MACOS_CODE_RESOURCES {
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
    let binary = root.join(&payload.entrypoint);
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
    verify_windows_signature(signtool, &binary)
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
    let entry = format!("{package_stem}/{}", payload.entrypoint);
    let record = payload
        .files
        .iter()
        .find(|record| record.path == payload.entrypoint)
        .ok_or("signed Windows entrypoint is missing from manifest")?;
    let bytes = read_zip_entry_bounded(&mut archive, &entry, record.size, Some(record.size))?;
    let temp = tempfile::tempdir()?;
    let binary = temp.path().join("clew.exe");
    fs::write(&binary, bytes)?;
    verify_windows_signature(signtool, &binary)
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
    write_macos_distribution_zip(&app, &submission)?;
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
        .filter(|file| file.path != MACOS_CODE_RESOURCES)
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
    readme: &[u8],
    app_svg: &[u8],
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
                        macos_info_plist(version).into_bytes(),
                        0o644,
                    ),
                    archive_file("Clew.app/Contents/MacOS/Clew", launcher.to_vec(), 0o755),
                    archive_file(
                        "Clew.app/Contents/Resources/AppIcon.icns",
                        macos_icns(app_svg)?,
                        0o644,
                    ),
                    archive_file("Clew.app/Contents/Resources/clew", binary.to_vec(), 0o755),
                    archive_file("README.md", readme.to_vec(), 0o644),
                ],
            }
        }
        ReleasePlatform::Linux => PackageLayout {
            name: "linux-portable".into(),
            app_id: APP_ID.into(),
            entrypoint: "bin/clew".into(),
            cli_binary: "bin/clew".into(),
            files: vec![
                archive_file("bin/clew", binary.to_vec(), 0o755),
                archive_file(
                    "share/applications/io.clew.app.desktop",
                    linux_desktop_entry().into_bytes(),
                    0o644,
                ),
                archive_file(
                    "share/icons/hicolor/scalable/apps/clew.svg",
                    app_svg.to_vec(),
                    0o644,
                ),
                archive_file("README.md", readme.to_vec(), 0o644),
            ],
        },
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

fn macos_info_plist(version: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>CFBundleDevelopmentRegion</key><string>en</string>\n  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>\n  <key>CFBundleExecutable</key><string>Clew</string>\n  <key>CFBundleIconFile</key><string>AppIcon</string>\n  <key>CFBundleIdentifier</key><string>{APP_ID}</string>\n  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>\n  <key>CFBundleName</key><string>{APP_NAME}</string>\n  <key>CFBundlePackageType</key><string>APPL</string>\n  <key>CFBundleShortVersionString</key><string>{version}</string>\n  <key>CFBundleVersion</key><string>{version}</string>\n  <key>NSHighResolutionCapable</key><true/>\n</dict>\n</plist>\n"
    )
}

fn linux_desktop_entry() -> String {
    format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName={APP_NAME}\nComment=Agent-facing remote capability bridge\nTryExec=clew\nExec=clew gui\nIcon=clew\nTerminal=false\nCategories=Network;\nStartupNotify=true\n"
    )
}

fn macos_icns(svg: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
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
        let png = render_svg_png(svg, size)?;
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

fn render_svg_png(svg: &[u8], size: u32) -> Result<Vec<u8>, Box<dyn Error>> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(svg, &options)?;
    let source = tree.size();
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size, size).ok_or("icon pixmap allocation failed")?;
    let transform = resvg::tiny_skia::Transform::from_scale(
        size as f32 / source.width(),
        size as f32 / source.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    unpremultiply_rgba(&mut rgba);
    let image = RgbaImage::from_raw(size, size, rgba).ok_or("invalid rendered RGBA icon")?;
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
) -> Result<(), Box<dyn Error>> {
    let build = |binary: &str, feature: Option<&str>| -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("cargo");
        command.args(["build", "--locked", "--bin", binary]);
        if let Some(feature) = feature {
            command.args(["--features", feature]);
        }
        let status = command
            .args(["--target", target, "--profile", profile])
            .env("SOURCE_DATE_EPOCH", source_date_epoch.to_string())
            .env("CARGO_INCREMENTAL", "0")
            .current_dir(repo)
            .status()?;
        if !status.success() {
            return Err(format!("cargo build for {binary} failed with {status}").into());
        }
        Ok(())
    };

    build(PRODUCT, None)?;
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
    zip.start_file(format!("{package_stem}/release-manifest.json"), regular)?;
    zip.write_all(payload_manifest)?;
    zip.finish()?;
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
        let windows = build_package_layout(
            ReleasePlatform::Windows,
            "x86_64-pc-windows-msvc",
            b"win-binary",
            None,
            b"readme",
            svg,
            "1.2.3",
        )
        .unwrap();
        assert_eq!(windows.name, "windows-portable");
        assert_eq!(windows.entrypoint, "clew.exe");
        assert_eq!(windows.cli_binary, "clew.exe");
        assert_eq!(windows.files.len(), 2);

        let macos = build_package_layout(
            ReleasePlatform::Macos,
            "aarch64-apple-darwin",
            b"mac-cli",
            Some(b"mach-o-launcher"),
            b"readme",
            svg,
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
            .find(|file| file.path.ends_with("Info.plist"))
            .unwrap();
        let plist = std::str::from_utf8(&plist.bytes).unwrap();
        assert!(plist.contains("<string>io.clew.app</string>"));
        assert!(plist.contains("<key>CFBundleExecutable</key><string>Clew</string>"));
        assert!(plist.contains("<string>1.2.3</string>"));
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
            b"readme",
            svg,
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
        assert!(desktop.contains("Name=Clew\n"));
        assert!(desktop.contains("TryExec=clew"));
        assert!(desktop.contains("Exec=clew gui"));
        assert!(desktop.contains("Terminal=false"));
        assert!(
            linux
                .files
                .iter()
                .any(|file| file.path == "share/icons/hicolor/scalable/apps/clew.svg")
        );
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
