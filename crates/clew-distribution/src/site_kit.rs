use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use clew_host::{
    ClientFlavor, OutfitPreset, OutfitProfile, SignedSiteClew, SiteKitContract, TargetPlatform,
    verify_outfit_asset_bytes,
};
use flate2::{Compression, write::GzEncoder};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::{
    ArtifactInfo, DistributionError, HELPER_ROLE_DIR, PayloadFile, ROLE_HINT_FILE, ReleasePlatform,
    SITE_KIT_SCHEMA_VERSION, SiteKitArtifactManifest, SiteKitPayloadManifest, USE_ROLE_DIR,
    ValidatedClientFlavorArtifact, read_zip_entry_exact, sha256_bytes, sha256_file,
};

const MACOS_ROLE_APP: &str = "Clew Role.app";
const MAX_SITE_KIT_FILES: usize = 512;
const MAX_SITE_LABEL_BYTES: usize = 160;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteKitAsset {
    pub asset_id: String,
    pub extension: String,
    pub bytes: Vec<u8>,
}

pub struct SiteKitAssemblyRequest<'a> {
    pub artifact: &'a ValidatedClientFlavorArtifact,
    pub site_label: &'a str,
    pub site_file: &'a SignedSiteClew,
    pub site_bytes: &'a [u8],
    pub assets: &'a [SiteKitAsset],
    pub out_dir: &'a Path,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteKitAssemblyResult {
    pub archive_path: PathBuf,
    pub manifest_path: PathBuf,
    pub checksums_path: PathBuf,
    pub manifest: SiteKitArtifactManifest,
}

#[derive(Clone)]
struct AssemblyFile {
    path: String,
    bytes: Vec<u8>,
    mode: u32,
}

pub fn assemble_site_kit(
    request: SiteKitAssemblyRequest<'_>,
) -> Result<SiteKitAssemblyResult, DistributionError> {
    let artifact = request.artifact;
    if artifact.platform == ReleasePlatform::Windows
        && artifact
            .release
            .payload
            .site_kit_launcher
            .as_ref()
            .is_none_or(|launcher| {
                launcher.schema_version != super::SITE_KIT_LAUNCHER_SCHEMA_VERSION
            })
    {
        return Err(site_error(
            "Windows single-launcher Site Kit requires the current role-chooser launcher contract",
        ));
    }
    if !super::site_kit_distribution_eligible(artifact) {
        return Err(site_error(
            "product Site Kit assembly requires a release-ready runtime or a verified unsigned 0.x runtime",
        ));
    }
    let native = native_release_platform()?;
    if artifact.platform != native {
        return Err(DistributionError::NativeAssemblyRequired {
            artifact: artifact.platform,
            native,
        });
    }
    validate_site_binding(artifact, request.site_file, request.site_bytes)?;
    let profile = request
        .site_file
        .payload
        .outfit_profile
        .clone()
        .unwrap_or_else(|| OutfitProfile::preset(OutfitPreset::ClewOriginal));
    let assets = validate_assets(&profile, request.assets)?;
    let label = sanitize_site_label(request.site_label)?;
    let target_platform = target_platform(artifact.platform);
    let contract = SiteKitContract::for_platform(target_platform);
    let archive_name = contract.archive_name(&label);
    let stem = archive_stem(&archive_name)?;
    fs::create_dir_all(request.out_dir)?;
    let archive_path = request.out_dir.join(&archive_name);
    let manifest_path = request.out_dir.join(format!("{stem}.site-kit.json"));
    let checksums_path = request.out_dir.join("SHA256SUMS");
    for path in [&archive_path, &manifest_path] {
        if path.exists() {
            return Err(DistributionError::OutputAlreadyExists(path.clone()));
        }
    }

    let mut common = common_files(
        contract.start_here_name,
        artifact.platform,
        &profile,
        request.site_bytes,
        &assets,
    )?;
    if artifact.platform != ReleasePlatform::Windows {
        append_legacy_role_markers(&mut common);
    }
    let release_archive = artifact.root.join(&artifact.entry.artifact_file);
    let release_stem = release_package_stem(&artifact.release.payload);
    let payload_files = match artifact.platform {
        ReleasePlatform::Windows => {
            append_windows_files(&mut common, artifact, &release_archive, &release_stem)?;
            common.sort_by(|left, right| left.path.cmp(&right.path));
            validate_assembly_files(&common)?;
            records(&common)?
        }
        ReleasePlatform::Linux => {
            append_linux_files(&mut common, artifact, &release_archive, &release_stem)?;
            common.sort_by(|left, right| left.path.cmp(&right.path));
            validate_assembly_files(&common)?;
            records(&common)?
        }
        ReleasePlatform::Macos => macos_expected_records(&common, artifact)?,
    };
    let payload = SiteKitPayloadManifest {
        schema_version: SITE_KIT_SCHEMA_VERSION,
        source_cache_key: artifact.entry.cache_key.clone(),
        client_flavor: artifact.entry.client_flavor.clone(),
        target: artifact.entry.target.clone(),
        source_release_sha256: artifact.entry.artifact_sha256.clone(),
        site_sha256: sha256_bytes(request.site_bytes),
        runtime_release_ready: artifact.entry.release_ready,
        files: payload_files,
    };
    let payload_json = serde_json::to_vec_pretty(&payload)?;

    let staging = staging_root(request.out_dir, &stem)?;
    let staged_archive = staging.join(&archive_name);
    match artifact.platform {
        ReleasePlatform::Windows => {
            write_zip(&staged_archive, &stem, &common, &payload_json)?;
            verify_zip(&staged_archive, &stem, &payload)?;
        }
        ReleasePlatform::Linux => {
            write_tar_gz(&staged_archive, &stem, &common, &payload_json)?;
        }
        ReleasePlatform::Macos => {
            write_macos_site_kit(
                &release_archive,
                &release_stem,
                &staged_archive,
                &stem,
                &common,
                &payload,
                &payload_json,
            )?;
        }
    }
    let artifact_info = ArtifactInfo {
        file: archive_name.clone(),
        size: fs::metadata(&staged_archive)?.len(),
        sha256: sha256_file(&staged_archive)?,
    };
    let manifest = SiteKitArtifactManifest {
        payload,
        artifact: artifact_info,
    };
    let staged_manifest = staging.join(format!("{stem}.site-kit.json"));
    write_synced(&staged_manifest, &serde_json::to_vec_pretty(&manifest)?)?;
    let staged_sums = staging.join("SHA256SUMS");
    write_synced(
        &staged_sums,
        format!(
            "{}  {}\n{}  {}\n",
            manifest.artifact.sha256,
            archive_name,
            sha256_file(&staged_manifest)?,
            staged_manifest
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| site_error("Site Kit sidecar filename is not UTF-8"))?
        )
        .as_bytes(),
    )?;

    publish_file(&staged_archive, &archive_path)?;
    publish_file(&staged_manifest, &manifest_path)?;
    if checksums_path.exists() {
        fs::remove_file(&checksums_path)?;
    }
    publish_file(&staged_sums, &checksums_path)?;
    let _ = fs::remove_dir(&staging);
    Ok(SiteKitAssemblyResult {
        archive_path,
        manifest_path,
        checksums_path,
        manifest,
    })
}

fn validate_site_binding(
    artifact: &ValidatedClientFlavorArtifact,
    site: &SignedSiteClew,
    site_bytes: &[u8],
) -> Result<(), DistributionError> {
    let decoded = SignedSiteClew::from_bytes(site_bytes)
        .map_err(|error| site_error(format!("site.clew bytes are invalid: {error}")))?;
    if &decoded != site {
        return Err(site_error("site.clew object differs from supplied bytes"));
    }
    site.verify()
        .map_err(|error| site_error(format!("site.clew signature is invalid: {error}")))?;
    let flavor = ClientFlavor {
        runtime_version: artifact.entry.version.clone(),
        platform: target_platform(artifact.platform),
        arch: artifact.arch.clone(),
        outfit_id: artifact.entry.client_flavor.outfit_id.clone(),
        outfit_revision: artifact.entry.client_flavor.outfit_revision,
    };
    site.verify_for_flavor(&flavor)
        .map_err(|error| site_error(format!("site.clew does not match ClientFlavor: {error}")))?;
    if site.payload.client_flavor_id.path_component() != artifact.entry.client_flavor.id {
        return Err(site_error(
            "site.clew ClientFlavorId differs from imported artifact",
        ));
    }
    if let Some(profile) = &site.payload.outfit_profile {
        let build_key = profile
            .build_cache_key()
            .map_err(|error| site_error(format!("site Outfit is invalid: {error}")))?;
        if build_key != artifact.entry.client_flavor.build_cache_key {
            return Err(site_error(
                "site Outfit build key differs from imported artifact",
            ));
        }
    }
    Ok(())
}

fn validate_assets(
    profile: &OutfitProfile,
    assets: &[SiteKitAsset],
) -> Result<BTreeMap<String, SiteKitAsset>, DistributionError> {
    let expected = profile
        .imported_asset_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    if assets.len() != expected.len() {
        return Err(site_error(
            "Site Kit Outfit asset count differs from signed profile",
        ));
    }
    let mut output = BTreeMap::new();
    for asset in assets {
        if !expected.contains(&asset.asset_id) {
            return Err(site_error(format!(
                "Site Kit contains unreferenced Outfit asset {}",
                asset.asset_id
            )));
        }
        if !matches!(asset.extension.as_str(), "png" | "svg") {
            return Err(site_error("Outfit asset extension must be png or svg"));
        }
        verify_outfit_asset_bytes(&asset.asset_id, &asset.bytes)
            .map_err(|error| site_error(format!("Outfit asset failed verification: {error}")))?;
        if output
            .insert(asset.asset_id.clone(), asset.clone())
            .is_some()
        {
            return Err(site_error("Site Kit contains duplicate Outfit asset id"));
        }
    }
    if output.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(site_error("Site Kit is missing a signed Outfit asset"));
    }
    Ok(output)
}

fn common_files(
    start_here_name: &str,
    platform: ReleasePlatform,
    profile: &OutfitProfile,
    site_bytes: &[u8],
    assets: &BTreeMap<String, SiteKitAsset>,
) -> Result<Vec<AssemblyFile>, DistributionError> {
    let mut files = vec![
        file("site.clew", site_bytes.to_vec(), 0o600),
        file(
            start_here_name,
            start_html(profile, platform).into_bytes(),
            0o644,
        ),
        file(
            "Message to collaborator.txt",
            format!("{}\n", profile.distribution_copy.chat_message_template).into_bytes(),
            0o644,
        ),
    ];
    for asset in assets.values() {
        files.push(file(
            format!("outfit-assets/{}.{}", asset.asset_id, asset.extension),
            asset.bytes.clone(),
            0o644,
        ));
    }
    Ok(files)
}

fn append_legacy_role_markers(files: &mut Vec<AssemblyFile>) {
    files.push(file(
        format!("{USE_ROLE_DIR}/{ROLE_HINT_FILE}"),
        b"use-this-machine\n".to_vec(),
        0o644,
    ));
    files.push(file(
        format!("{HELPER_ROLE_DIR}/{ROLE_HINT_FILE}"),
        b"connector-only\n".to_vec(),
        0o644,
    ));
}

fn append_windows_files(
    files: &mut Vec<AssemblyFile>,
    artifact: &ValidatedClientFlavorArtifact,
    archive_path: &Path,
    release_stem: &str,
) -> Result<(), DistributionError> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let runtime = read_release_file(
        &mut archive,
        release_stem,
        artifact,
        &artifact.release.payload.cli_binary,
    )?;
    let launcher_path = &artifact
        .release
        .payload
        .site_kit_launcher
        .as_ref()
        .ok_or_else(|| site_error("release omitted Site Kit launcher"))?
        .executable_path;
    let launcher = read_release_file(&mut archive, release_stem, artifact, launcher_path)?;
    files.push(file(".clew-runtime/clew.exe", runtime, 0o755));
    files.push(file("Clew.exe", launcher, 0o755));
    Ok(())
}

fn append_linux_files(
    files: &mut Vec<AssemblyFile>,
    artifact: &ValidatedClientFlavorArtifact,
    archive_path: &Path,
    release_stem: &str,
) -> Result<(), DistributionError> {
    let mut archive = ZipArchive::new(File::open(archive_path)?)?;
    let runtime = read_release_file(
        &mut archive,
        release_stem,
        artifact,
        &artifact.release.payload.cli_binary,
    )?;
    let launcher_path = &artifact
        .release
        .payload
        .site_kit_launcher
        .as_ref()
        .ok_or_else(|| site_error("release omitted Site Kit launcher"))?
        .executable_path;
    let launcher = read_release_file(&mut archive, release_stem, artifact, launcher_path)?;
    files.push(file(".clew-runtime/clew", runtime, 0o755));
    files.push(file(
        format!("{USE_ROLE_DIR}/Clew"),
        launcher.clone(),
        0o755,
    ));
    files.push(file(format!("{HELPER_ROLE_DIR}/Clew"), launcher, 0o755));
    Ok(())
}

fn macos_expected_records(
    common: &[AssemblyFile],
    artifact: &ValidatedClientFlavorArtifact,
) -> Result<Vec<PayloadFile>, DistributionError> {
    let mut output = records(common)?;
    map_release_prefix(
        &mut output,
        artifact,
        "Clew.app/",
        ".clew-runtime/Clew.app/",
    )?;
    map_release_prefix(
        &mut output,
        artifact,
        &format!("{MACOS_ROLE_APP}/"),
        &format!("{USE_ROLE_DIR}/Clew.app/"),
    )?;
    map_release_prefix(
        &mut output,
        artifact,
        &format!("{MACOS_ROLE_APP}/"),
        &format!("{HELPER_ROLE_DIR}/Clew.app/"),
    )?;
    output.sort_by(|left, right| left.path.cmp(&right.path));
    if output.len() > MAX_SITE_KIT_FILES {
        return Err(site_error("macOS Site Kit exceeds file-count bound"));
    }
    for pair in output.windows(2) {
        if pair[0].path >= pair[1].path {
            return Err(site_error("macOS Site Kit paths are duplicate or unsorted"));
        }
    }
    Ok(output)
}

fn map_release_prefix(
    output: &mut Vec<PayloadFile>,
    artifact: &ValidatedClientFlavorArtifact,
    source_prefix: &str,
    target_prefix: &str,
) -> Result<(), DistributionError> {
    let mut matched = false;
    for record in &artifact.release.payload.files {
        if let Some(suffix) = record.path.strip_prefix(source_prefix) {
            matched = true;
            output.push(PayloadFile {
                path: format!("{target_prefix}{suffix}"),
                size: record.size,
                sha256: record.sha256.clone(),
                mode: record.mode.clone(),
            });
        }
    }
    if !matched {
        return Err(site_error(format!(
            "release omitted required macOS prefix {source_prefix}"
        )));
    }
    Ok(())
}

fn read_release_file(
    archive: &mut ZipArchive<File>,
    release_stem: &str,
    artifact: &ValidatedClientFlavorArtifact,
    path: &str,
) -> Result<Vec<u8>, DistributionError> {
    let record = artifact
        .release
        .payload
        .files
        .iter()
        .find(|record| record.path == path)
        .ok_or_else(|| site_error(format!("release omitted required path {path}")))?;
    read_zip_entry_exact(archive, &format!("{release_stem}/{path}"), record.size)
}

fn write_zip(
    path: &Path,
    stem: &str,
    files: &[AssemblyFile],
    manifest: &[u8],
) -> Result<(), DistributionError> {
    let output = File::create(path)?;
    let mut zip = ZipWriter::new(output);
    let timestamp = DateTime::default();
    for entry in files {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(timestamp)
            .unix_permissions(entry.mode);
        zip.start_file(format!("{stem}/{}", entry.path), options)?;
        zip.write_all(&entry.bytes)?;
    }
    let regular = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(timestamp)
        .unix_permissions(0o644);
    zip.start_file(format!("{stem}/site-kit-manifest.json"), regular)?;
    zip.write_all(manifest)?;
    let file = zip.finish()?;
    file.sync_all()?;
    Ok(())
}

fn verify_zip(
    path: &Path,
    stem: &str,
    expected: &SiteKitPayloadManifest,
) -> Result<(), DistributionError> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let mut expected_names = expected
        .files
        .iter()
        .map(|record| format!("{stem}/{}", record.path))
        .collect::<Vec<_>>();
    expected_names.push(format!("{stem}/site-kit-manifest.json"));
    expected_names.sort();
    let mut actual = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        actual.push(archive.by_index(index)?.name().to_owned());
    }
    actual.sort();
    if actual != expected_names {
        return Err(site_error("Site Kit ZIP contains unexpected entries"));
    }
    for record in &expected.files {
        let bytes = read_zip_entry_exact(
            &mut archive,
            &format!("{stem}/{}", record.path),
            record.size,
        )?;
        if sha256_bytes(&bytes) != record.sha256 {
            return Err(site_error(format!(
                "Site Kit ZIP hash differs for {}",
                record.path
            )));
        }
    }
    Ok(())
}

fn write_tar_gz(
    path: &Path,
    stem: &str,
    files: &[AssemblyFile],
    manifest: &[u8],
) -> Result<(), DistributionError> {
    let file = File::create(path)?;
    let mut encoder = GzEncoder::new(file, Compression::best());
    for entry in files {
        write_tar_entry(
            &mut encoder,
            &format!("{stem}/{}", entry.path),
            &entry.bytes,
            entry.mode,
        )?;
    }
    write_tar_entry(
        &mut encoder,
        &format!("{stem}/site-kit-manifest.json"),
        manifest,
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
) -> Result<(), DistributionError> {
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
    let text = format!("{checksum:06o}\0 ");
    if text.len() != 8 {
        return Err(site_error("tar checksum overflow"));
    }
    header[148..156].copy_from_slice(text.as_bytes());
    writer.write_all(&header)?;
    writer.write_all(bytes)?;
    let padding = (512 - (bytes.len() % 512)) % 512;
    if padding != 0 {
        writer.write_all(&vec![0_u8; padding])?;
    }
    Ok(())
}

fn split_ustar_path(path: &str) -> Result<(&str, &str), DistributionError> {
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
    Err(site_error(format!(
        "Site Kit path exceeds ustar bounds: {path}"
    )))
}

fn copy_tar_field(target: &mut [u8], value: &[u8]) -> Result<(), DistributionError> {
    if value.len() > target.len() {
        return Err(site_error("tar field exceeds fixed width"));
    }
    target[..value.len()].copy_from_slice(value);
    Ok(())
}

fn write_tar_octal(target: &mut [u8], value: u64) -> Result<(), DistributionError> {
    if target.len() < 2 {
        return Err(site_error("tar octal field is too short"));
    }
    let digits = format!("{value:o}");
    if digits.len() + 1 > target.len() {
        return Err(site_error("tar octal value exceeds fixed width"));
    }
    let start = target.len() - digits.len() - 1;
    target[..start].fill(b'0');
    target[start..start + digits.len()].copy_from_slice(digits.as_bytes());
    target[target.len() - 1] = 0;
    Ok(())
}

fn write_macos_site_kit(
    release_archive: &Path,
    release_stem: &str,
    archive_path: &Path,
    stem: &str,
    common: &[AssemblyFile],
    expected: &SiteKitPayloadManifest,
    manifest: &[u8],
) -> Result<(), DistributionError> {
    if !cfg!(target_os = "macos") {
        return Err(site_error("macOS Site Kit assembly must run on macOS"));
    }
    let work = archive_path
        .parent()
        .ok_or_else(|| site_error("macOS Site Kit output has no parent"))?
        .join(format!(".macos-{}.work", std::process::id()));
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;
    let release_extract = work.join("release");
    fs::create_dir_all(&release_extract)?;
    run(
        Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(release_archive)
            .arg(&release_extract),
        "ditto release extraction",
    )?;
    let release_root = release_extract.join(release_stem);
    let kit_parent = work.join("kit");
    let kit_root = kit_parent.join(stem);
    fs::create_dir_all(&kit_root)?;
    for entry in common {
        write_assembly_file(&kit_root, entry)?;
    }
    ditto_copy(
        &release_root.join("Clew.app"),
        &kit_root.join(".clew-runtime").join("Clew.app"),
    )?;
    for role in [USE_ROLE_DIR, HELPER_ROLE_DIR] {
        ditto_copy(
            &release_root.join(MACOS_ROLE_APP),
            &kit_root.join(role).join("Clew.app"),
        )?;
    }
    write_synced(&kit_root.join("site-kit-manifest.json"), manifest)?;
    verify_tree(&kit_root, expected, manifest)?;
    verify_macos_apps(&kit_root)?;
    run(
        Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--keepParent"])
            .arg(&kit_root)
            .arg(archive_path),
        "ditto Site Kit packaging",
    )?;
    let final_extract = work.join("final");
    fs::create_dir_all(&final_extract)?;
    run(
        Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(archive_path)
            .arg(&final_extract),
        "ditto final Site Kit extraction",
    )?;
    let final_root = final_extract.join(stem);
    verify_tree(&final_root, expected, manifest)?;
    verify_macos_apps(&final_root)?;
    fs::remove_dir_all(&work)?;
    Ok(())
}

fn ditto_copy(source: &Path, target: &Path) -> Result<(), DistributionError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    run(
        Command::new("/usr/bin/ditto").arg(source).arg(target),
        "ditto app copy",
    )
}

fn verify_tree(
    root: &Path,
    expected: &SiteKitPayloadManifest,
    manifest: &[u8],
) -> Result<(), DistributionError> {
    let mut paths = Vec::new();
    collect_regular_paths(root, root, &mut paths)?;
    paths.sort();
    let mut expected_paths = expected
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    expected_paths.push("site-kit-manifest.json".into());
    expected_paths.sort();
    if paths != expected_paths {
        return Err(site_error(
            "Site Kit staging tree has unexpected regular files",
        ));
    }
    for record in &expected.files {
        let bytes = fs::read(root.join(&record.path))?;
        if bytes.len() as u64 != record.size || sha256_bytes(&bytes) != record.sha256 {
            return Err(site_error(format!(
                "Site Kit staging hash differs for {}",
                record.path
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let expected_mode = u32::from_str_radix(&record.mode, 8)
                .map_err(|_| site_error("invalid recorded Unix mode"))?;
            let actual = fs::metadata(root.join(&record.path))?.permissions().mode() & 0o777;
            if actual != expected_mode {
                return Err(site_error(format!(
                    "Site Kit mode differs for {}",
                    record.path
                )));
            }
        }
    }
    if fs::read(root.join("site-kit-manifest.json"))? != manifest {
        return Err(site_error("Site Kit embedded manifest changed"));
    }
    Ok(())
}

fn collect_regular_paths(
    root: &Path,
    current: &Path,
    output: &mut Vec<String>,
) -> Result<(), DistributionError> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(site_error(
                "Site Kit staging tree must not contain symlinks",
            ));
        }
        if metadata.is_dir() {
            collect_regular_paths(root, &path, output)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| site_error("Site Kit staging path escaped root"))?
                .components()
                .map(|component| {
                    component
                        .as_os_str()
                        .to_str()
                        .ok_or_else(|| site_error("Site Kit staging path is not UTF-8"))
                })
                .collect::<Result<Vec<_>, _>>()?
                .join("/");
            output.push(relative);
            if output.len() > MAX_SITE_KIT_FILES + 1 {
                return Err(site_error("Site Kit staging file count exceeds bound"));
            }
        } else {
            return Err(site_error("Site Kit staging tree contains special file"));
        }
    }
    Ok(())
}

fn verify_macos_apps(root: &Path) -> Result<(), DistributionError> {
    let main = root.join(".clew-runtime").join("Clew.app");
    verify_macos_code(&main.join("Contents").join("Resources").join("clew"))?;
    verify_macos_app(&main)?;
    for role in [USE_ROLE_DIR, HELPER_ROLE_DIR] {
        let app = root.join(role).join("Clew.app");
        verify_macos_code(&app.join("Contents").join("MacOS").join("Clew Role"))?;
        verify_macos_app(&app)?;
    }
    Ok(())
}

fn verify_macos_app(app: &Path) -> Result<(), DistributionError> {
    verify_macos_code(app)?;
    run(
        Command::new("xcrun").args(["stapler", "validate"]).arg(app),
        "macOS staple validation",
    )?;
    run(
        Command::new("spctl")
            .args(["--assess", "--type", "exec", "--verbose=4"])
            .arg(app),
        "macOS Gatekeeper assessment",
    )
}

fn verify_macos_code(path: &Path) -> Result<(), DistributionError> {
    run(
        Command::new("codesign")
            .args(["--verify", "--strict", "--verbose=2"])
            .arg(path),
        "macOS code signature verification",
    )
}

fn run(command: &mut Command, label: &'static str) -> Result<(), DistributionError> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(DistributionError::NativeToolFailed {
            tool: label,
            status: status.to_string(),
        });
    }
    Ok(())
}

fn write_assembly_file(root: &Path, entry: &AssemblyFile) -> Result<(), DistributionError> {
    let target = root.join(&entry.path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    write_synced(&target, &entry.bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))?;
    }
    Ok(())
}

fn validate_assembly_files(files: &[AssemblyFile]) -> Result<(), DistributionError> {
    if files.is_empty() || files.len() > MAX_SITE_KIT_FILES {
        return Err(site_error("Site Kit file count is outside bound"));
    }
    let mut previous: Option<&str> = None;
    for entry in files {
        super::validate_relative_path(&entry.path)?;
        if previous.is_some_and(|path| path >= entry.path.as_str()) {
            return Err(site_error(
                "Site Kit paths must be unique and strictly sorted",
            ));
        }
        previous = Some(&entry.path);
    }
    Ok(())
}

fn records(files: &[AssemblyFile]) -> Result<Vec<PayloadFile>, DistributionError> {
    files
        .iter()
        .map(|entry| {
            Ok(PayloadFile {
                path: entry.path.clone(),
                size: entry.bytes.len() as u64,
                sha256: sha256_bytes(&entry.bytes),
                mode: format!("{:04o}", entry.mode),
            })
        })
        .collect()
}

fn file(path: impl Into<String>, bytes: Vec<u8>, mode: u32) -> AssemblyFile {
    AssemblyFile {
        path: path.into(),
        bytes,
        mode,
    }
}

fn start_html(profile: &OutfitProfile, platform: ReleasePlatform) -> String {
    let title = html_escape(&profile.distribution_copy.start_here_title);
    let body = html_escape(&profile.distribution_copy.start_here_body);
    let support = profile
        .distribution_copy
        .support_contact
        .as_ref()
        .map(|value| format!("<p>Support: {}</p>", html_escape(value)))
        .unwrap_or_default();
    let steps = if platform == ReleasePlatform::Windows {
        "<ol><li>Double-click <b>Clew.exe</b> in this folder.</li><li>Choose <b>Use this computer</b> on the computer you want to access.</li><li>If it needs a nearby online helper, copy the same Site Kit there, open <b>Clew.exe</b>, and choose <b>Help nearby computers connect</b>.</li></ol>"
    } else {
        "<ol><li>On the computer you want to use remotely, open the <b>1 Use this computer</b> launcher.</li><li>If that computer cannot reach the internet, copy this same Site Kit to a nearby online computer and open the <b>2 Help nearby computers</b> launcher.</li></ol>"
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1><p>{body}</p>{steps}<p>Keep this complete Site Kit together. The helper does not receive file or shell authority and cannot read the end-to-end protected session.</p>{support}</body></html>\n"
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

fn target_platform(platform: ReleasePlatform) -> TargetPlatform {
    match platform {
        ReleasePlatform::Windows => TargetPlatform::Windows,
        ReleasePlatform::Macos => TargetPlatform::MacOs,
        ReleasePlatform::Linux => TargetPlatform::Linux,
    }
}

fn native_release_platform() -> Result<ReleasePlatform, DistributionError> {
    if cfg!(windows) {
        Ok(ReleasePlatform::Windows)
    } else if cfg!(target_os = "macos") {
        Ok(ReleasePlatform::Macos)
    } else if cfg!(target_os = "linux") {
        Ok(ReleasePlatform::Linux)
    } else {
        Err(site_error(
            "this operating system cannot assemble Clew Site Kits",
        ))
    }
}

fn release_package_stem(payload: &super::PayloadManifest) -> String {
    if payload.unsigned {
        format!("clew-v{}-{}", payload.version, payload.target)
    } else {
        format!("clew-v{}-{}-signed", payload.version, payload.target)
    }
}

fn sanitize_site_label(value: &str) -> Result<String, DistributionError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SITE_LABEL_BYTES {
        return Err(site_error("Site Kit label is empty or oversized"));
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
        return Err(site_error(
            "Site Kit label contains no usable filename characters",
        ));
    }
    Ok(cleaned)
}

fn archive_stem(name: &str) -> Result<String, DistributionError> {
    name.strip_suffix(".zip")
        .or_else(|| name.strip_suffix(".tar.gz"))
        .map(str::to_owned)
        .ok_or_else(|| site_error("Site Kit archive extension is unsupported"))
}

fn staging_root(out_dir: &Path, stem: &str) -> Result<PathBuf, DistributionError> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let root = out_dir.join(format!(".{stem}.{}.{}.staging", std::process::id(), nonce));
    fs::create_dir(&root)?;
    Ok(root)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), DistributionError> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn publish_file(source: &Path, target: &Path) -> Result<(), DistributionError> {
    if target.exists() {
        return Err(DistributionError::OutputAlreadyExists(target.to_path_buf()));
    }
    fs::rename(source, target)?;
    Ok(())
}

fn site_error(message: impl Into<String>) -> DistributionError {
    DistributionError::SiteKit(message.into())
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::{Read as _, Write as _};

    use clew_core::{InviteId, SiteId};
    use clew_host::HostRoleHint;
    use clew_identity::{
        ControllerIdentity, EnrollmentRegistry, PermissionGrant, SiteBootstrapSpec,
    };
    use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter, write::SimpleFileOptions};

    use super::*;
    use crate::{
        ArtifactInfo, ArtifactManifest, ClientFlavorCacheEntry, PayloadManifest,
        ReleaseClientFlavorInfo, SIGNED_RELEASE_SCHEMA_VERSION, SigningInfo, SiteKitLauncherInfo,
        ToolchainInfo,
    };

    fn fixture(root: &Path) -> (ValidatedClientFlavorArtifact, SignedSiteClew, Vec<u8>) {
        let profile = OutfitProfile::preset(OutfitPreset::ClewOriginal);
        let flavor =
            ClientFlavor::from_outfit_target(&profile, TargetPlatform::Windows, "x86_64").unwrap();
        let controller = ControllerIdentity::from_secret([91_u8; 32]);
        let mut registry =
            EnrollmentRegistry::new(controller.controller_id(), PermissionGrant::EXECUTE_READ);
        let bootstrap = registry
            .issue_bootstrap(
                &controller,
                SiteBootstrapSpec {
                    site_id: SiteId::new(),
                    invite_id: InviteId::new(),
                    site_name: "Assembly Lab".into(),
                    grant: PermissionGrant::EXECUTE_READ,
                    not_before_unix_ms: 1,
                    expires_unix_ms: 10_000,
                    deployment_window_ms: 1_000,
                    max_claims: 4,
                },
            )
            .unwrap();
        let site = SignedSiteClew::issue(
            &controller,
            flavor.clone(),
            bootstrap,
            HostRoleHint::ExecutePreferred,
        )
        .unwrap();
        let site_bytes = site.to_bytes().unwrap();
        let client_flavor = ReleaseClientFlavorInfo {
            id: flavor.id().unwrap().path_component(),
            outfit_id: profile.outfit_id.clone(),
            outfit_revision: profile.revision,
            build_cache_key: profile.build_cache_key().unwrap(),
            app_display_name: profile.identity.app_display_name.clone(),
            publisher_label: None,
            icon_format: "svg".into(),
            icon_asset_id: None,
        };
        let runtime = b"signed-runtime".to_vec();
        let launcher = b"signed-role-launcher".to_vec();
        let payload = PayloadManifest {
            schema_version: SIGNED_RELEASE_SCHEMA_VERSION,
            product: "clew".into(),
            version: flavor.runtime_version.clone(),
            target: "x86_64-pc-windows-msvc".into(),
            profile: "release".into(),
            archive_format: "zip".into(),
            layout: "windows-portable".into(),
            app_id: "io.clew.app".into(),
            entrypoint: "Clew Launcher.exe".into(),
            cli_binary: "clew.exe".into(),
            source_commit: "1".repeat(40),
            source_date_epoch: 1,
            rustc: ToolchainInfo {
                release: "1.96.0".into(),
                commit_hash: "2".repeat(40),
                host: "x86_64-pc-windows-msvc".into(),
                llvm_version: "22.1.0".into(),
            },
            cargo_lock_sha256: "3".repeat(64),
            dirty: false,
            unsigned: false,
            signing: Some(SigningInfo {
                mechanism: "windows-authenticode".into(),
                identity: "0123456789ABCDEF0123456789ABCDEF01234567".into(),
                timestamped: true,
                notarized: false,
                stapled: false,
                notary_submission_id: None,
            }),
            client_flavor: Some(client_flavor.clone()),
            site_kit_launcher: Some(SiteKitLauncherInfo {
                schema_version: crate::SITE_KIT_LAUNCHER_SCHEMA_VERSION,
                executable_path: "Clew Launcher.exe".into(),
                bundle_root: None,
            }),
            files: vec![
                PayloadFile {
                    path: "Clew Launcher.exe".into(),
                    size: launcher.len() as u64,
                    sha256: sha256_bytes(&launcher),
                    mode: "0755".into(),
                },
                PayloadFile {
                    path: "clew.exe".into(),
                    size: runtime.len() as u64,
                    sha256: sha256_bytes(&runtime),
                    mode: "0755".into(),
                },
            ],
        };
        let stem = release_package_stem(&payload);
        let artifact_file = format!("{stem}.zip");
        let artifact_path = root.join(&artifact_file);
        let mut zip = ZipWriter::new(File::create(&artifact_path).unwrap());
        let executable = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(0o755);
        zip.start_file(format!("{stem}/Clew Launcher.exe"), executable)
            .unwrap();
        zip.write_all(&launcher).unwrap();
        zip.start_file(format!("{stem}/clew.exe"), executable)
            .unwrap();
        zip.write_all(&runtime).unwrap();
        zip.finish().unwrap();
        let artifact_info = ArtifactInfo {
            file: artifact_file.clone(),
            size: fs::metadata(&artifact_path).unwrap().len(),
            sha256: sha256_file(&artifact_path).unwrap(),
        };
        let release = ArtifactManifest {
            payload: payload.clone(),
            artifact: artifact_info.clone(),
        };
        let cache_key = crate::client_flavor_cache_key(&client_flavor, &payload).unwrap();
        let entry = ClientFlavorCacheEntry {
            schema_version: crate::CLIENT_FLAVOR_CACHE_SCHEMA_VERSION,
            cache_key,
            client_flavor,
            version: payload.version.clone(),
            target: payload.target.clone(),
            profile: payload.profile.clone(),
            source_commit: payload.source_commit.clone(),
            release_ready: true,
            signing: payload.signing.clone(),
            artifact_file,
            artifact_sha256: artifact_info.sha256,
            manifest_file: "unused.release.json".into(),
            manifest_sha256: "4".repeat(64),
        };
        (
            ValidatedClientFlavorArtifact {
                root: root.to_path_buf(),
                entry,
                release,
                platform: ReleasePlatform::Windows,
                arch: "x86_64".into(),
            },
            site,
            site_bytes,
        )
    }

    #[test]
    fn product_windows_site_kit_binds_runtime_roles_and_site() {
        let source = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (artifact, site, site_bytes) = fixture(source.path());
        let result = assemble_site_kit(SiteKitAssemblyRequest {
            artifact: &artifact,
            site_label: "Assembly Lab",
            site_file: &site,
            site_bytes: &site_bytes,
            assets: &[],
            out_dir: out.path(),
        })
        .unwrap();
        assert_eq!(
            result.manifest.payload.site_sha256,
            sha256_bytes(&site_bytes)
        );
        assert!(result.manifest.payload.runtime_release_ready);

        let stem =
            archive_stem(result.archive_path.file_name().unwrap().to_str().unwrap()).unwrap();
        let mut zip = ZipArchive::new(File::open(&result.archive_path).unwrap()).unwrap();
        let mut read_entry = |path: &str| {
            let mut entry = zip.by_name(&format!("{stem}/{path}")).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            bytes
        };
        assert_eq!(read_entry("Clew.exe"), b"signed-role-launcher");
        assert_eq!(read_entry(".clew-runtime/clew.exe"), b"signed-runtime");
        assert_eq!(read_entry("site.clew"), site_bytes);
    }

    #[test]
    fn product_windows_site_kit_rejects_legacy_role_launcher_contract() {
        let source = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (mut artifact, site, site_bytes) = fixture(source.path());
        artifact
            .release
            .payload
            .site_kit_launcher
            .as_mut()
            .unwrap()
            .schema_version = super::super::LEGACY_SITE_KIT_LAUNCHER_SCHEMA_VERSION;
        assert!(matches!(
            assemble_site_kit(SiteKitAssemblyRequest {
                artifact: &artifact,
                site_label: "Legacy Launcher",
                site_file: &site,
                site_bytes: &site_bytes,
                assets: &[],
                out_dir: out.path(),
            }),
            Err(DistributionError::SiteKit(message))
                if message.contains("current role-chooser launcher contract")
        ));
    }

    #[test]
    fn product_site_kit_rejects_wrong_client_flavor() {
        let source = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (mut artifact, site, site_bytes) = fixture(source.path());
        artifact.entry.client_flavor.id = "different-flavor".into();
        assert!(matches!(
            assemble_site_kit(SiteKitAssemblyRequest {
                artifact: &artifact,
                site_label: "Wrong Flavor",
                site_file: &site,
                site_bytes: &site_bytes,
                assets: &[],
                out_dir: out.path(),
            }),
            Err(DistributionError::SiteKit(_))
        ));
    }
}
