use std::{
    env,
    error::Error,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

const PRODUCT: &str = "clew";
const RELEASE_SCHEMA_VERSION: u32 = 1;
const MAX_EMBEDDED_MANIFEST_BYTES: u64 = 1024 * 1024;

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
struct PayloadManifest {
    schema_version: u32,
    product: String,
    version: String,
    target: String,
    profile: String,
    archive_format: String,
    source_commit: String,
    source_date_epoch: u64,
    rustc: ToolchainInfo,
    cargo_lock_sha256: String,
    dirty: bool,
    unsigned: bool,
    files: Vec<PayloadFile>,
}

#[derive(Debug, Serialize)]
struct ArtifactInfo {
    file: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize)]
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

    if !no_build {
        run_cargo_build(repo, &target, profile, source_date_epoch)?;
    }
    let binary = built_binary_path(repo, &target, profile);
    if !binary.is_file() {
        return Err(format!("Clew binary does not exist: {}", binary.display()).into());
    }

    let out_dir = if out_dir.is_absolute() {
        out_dir.to_path_buf()
    } else {
        repo.join(out_dir)
    };
    fs::create_dir_all(&out_dir)?;

    let binary_name = if target.contains("windows") {
        "clew.exe"
    } else {
        "clew"
    };
    let package_stem = format!("clew-v{}-{target}", env!("CARGO_PKG_VERSION"));
    let archive_name = format!("{package_stem}.zip");
    let archive_path = out_dir.join(&archive_name);
    let sidecar_name = format!("{package_stem}.release.json");
    let sidecar_path = out_dir.join(sidecar_name);

    let binary_bytes = fs::read(&binary)?;
    let readme_bytes = fs::read(repo.join("README.md"))?;
    let payload = PayloadManifest {
        schema_version: RELEASE_SCHEMA_VERSION,
        product: PRODUCT.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        target: target.clone(),
        profile: profile.to_owned(),
        archive_format: "zip".into(),
        source_commit,
        source_date_epoch,
        rustc,
        cargo_lock_sha256,
        dirty,
        unsigned: true,
        files: vec![
            PayloadFile {
                path: binary_name.into(),
                size: binary_bytes.len() as u64,
                sha256: sha256_bytes(&binary_bytes),
                mode: "0755".into(),
            },
            PayloadFile {
                path: "README.md".into(),
                size: readme_bytes.len() as u64,
                sha256: sha256_bytes(&readme_bytes),
                mode: "0644".into(),
            },
        ],
    };
    let payload_json = json_bytes(&payload)?;
    write_zip(
        &archive_path,
        &package_stem,
        binary_name,
        &binary_bytes,
        &readme_bytes,
        &payload_json,
    )?;
    if !skip_smoke && target == host {
        smoke_archive(&archive_path, &package_stem, binary_name, &payload)?;
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

fn run_cargo_build(
    repo: &Path,
    target: &str,
    profile: &str,
    source_date_epoch: u64,
) -> Result<(), Box<dyn Error>> {
    let status = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--bin",
            PRODUCT,
            "--target",
            target,
            "--profile",
            profile,
        ])
        .env("SOURCE_DATE_EPOCH", source_date_epoch.to_string())
        .env("CARGO_INCREMENTAL", "0")
        .current_dir(repo)
        .status()?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}").into());
    }
    Ok(())
}

fn built_binary_path(repo: &Path, target: &str, profile: &str) -> PathBuf {
    let binary = if target.contains("windows") {
        "clew.exe"
    } else {
        "clew"
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
    binary_name: &str,
    expected_payload: &PayloadManifest,
) -> Result<(), Box<dyn Error>> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let mut expected_names = vec![
        format!("{package_stem}/{binary_name}"),
        format!("{package_stem}/README.md"),
        format!("{package_stem}/release-manifest.json"),
    ];
    let mut actual_names = (0..archive.len())
        .map(|index| archive.by_index(index).map(|entry| entry.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    actual_names.sort();
    expected_names.sort();
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

    let binary_record = expected_payload
        .files
        .iter()
        .find(|file| file.path == binary_name)
        .ok_or("release payload does not describe the Clew binary")?;
    let binary_entry = format!("{package_stem}/{binary_name}");
    let binary_bytes = read_zip_entry_bounded(
        &mut archive,
        &binary_entry,
        binary_record.size,
        Some(binary_record.size),
    )?;
    if sha256_bytes(&binary_bytes) != binary_record.sha256 {
        return Err("archived Clew binary hash differs from release manifest".into());
    }

    let temp = tempfile::tempdir()?;
    let extracted = temp.path().join(binary_name);
    fs::write(&extracted, &binary_bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&extracted, fs::Permissions::from_mode(0o755))?;
    }
    smoke_binary(&extracted)
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
    binary_name: &str,
    binary: &[u8],
    readme: &[u8],
    payload_manifest: &[u8],
) -> Result<(), Box<dyn Error>> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let timestamp = DateTime::default();
    let executable = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(timestamp)
        .unix_permissions(0o755);
    let regular = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(timestamp)
        .unix_permissions(0o644);

    zip.start_file(format!("{package_stem}/{binary_name}"), executable)?;
    zip.write_all(binary)?;
    zip.start_file(format!("{package_stem}/README.md"), regular)?;
    zip.write_all(readme)?;
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
