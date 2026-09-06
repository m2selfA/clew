use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write as _,
    path::{Component, Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clew_core::{ReadPolicy, RequestId};
use clew_transport::{
    FsControlResult, FsManagedTempKind, FsManagedTempResource, FsMutationErrorCode,
    FsMutationReply, FsMutationRequest, FsTrashItem, HARD_MAX_FS_CONTROL_ITEMS,
};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::target_path::expand_target_path;

const LEDGER_VERSION: u32 = 1;
const LEDGER_MAX_BYTES: u64 = 512 * 1024;
const PURGE_CONFIRM_TTL_MS: u64 = 5 * 60 * 1000;
#[cfg(windows)]
const WINDOWS_FILETIME_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;

#[cfg(windows)]
#[derive(Clone, Debug)]
struct WindowsRecycleRecord {
    info_path: PathBuf,
    data_path: PathBuf,
    original_path: PathBuf,
    deleted_unix_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ManagedFsLedger {
    #[serde(default = "ledger_version")]
    version: u32,
    #[serde(default)]
    trash: BTreeMap<String, ManagedTrashRecord>,
    #[serde(default)]
    temp: BTreeMap<String, FsManagedTempResource>,
    #[serde(default)]
    purge: BTreeMap<String, PendingPurge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManagedTrashRecord {
    item: FsTrashItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingPurge {
    token: String,
    expires_unix_ms: u64,
}

const fn ledger_version() -> u32 {
    LEDGER_VERSION
}

pub(crate) fn execute_control(
    policy: &ReadPolicy,
    managed_root: Option<&Path>,
    request_id: RequestId,
    request: FsMutationRequest,
) -> FsMutationReply {
    match request {
        FsMutationRequest::CreateDirectory { path } => create_directory(policy, &path),
        FsMutationRequest::Copy {
            source,
            destination,
        } => copy_file(policy, &source, &destination),
        FsMutationRequest::Move {
            source,
            destination,
        } => move_path(policy, &source, &destination),
        FsMutationRequest::Trash { path } => trash_path(policy, managed_root, request_id, &path),
        FsMutationRequest::TrashList => with_ledger(managed_root, |_, ledger| {
            Ok(FsControlResult::TrashItems {
                items: ledger
                    .trash
                    .values()
                    .take(HARD_MAX_FS_CONTROL_ITEMS)
                    .map(|record| record.item.clone())
                    .collect(),
            })
        }),
        FsMutationRequest::TrashRestore { trash_id } => trash_restore(managed_root, &trash_id),
        FsMutationRequest::TrashPurgePrepare { trash_id } => {
            trash_purge_prepare(managed_root, &trash_id)
        }
        FsMutationRequest::TrashPurgeConfirm {
            trash_id,
            confirmation_token,
        } => trash_purge_confirm(managed_root, &trash_id, &confirmation_token),
        FsMutationRequest::TempCreate {
            temp_kind,
            description,
            ttl_ms,
        } => temp_create(managed_root, request_id, temp_kind, description, ttl_ms),
        FsMutationRequest::TempList => with_ledger(managed_root, |_, ledger| {
            Ok(FsControlResult::TempItems {
                items: ledger
                    .temp
                    .values()
                    .take(HARD_MAX_FS_CONTROL_ITEMS)
                    .cloned()
                    .collect(),
            })
        }),
        FsMutationRequest::TempRelease { resource_id } => temp_release(managed_root, &resource_id),
        FsMutationRequest::TempGc => temp_gc(managed_root),
        FsMutationRequest::Write { .. } | FsMutationRequest::Edit { .. } => Err(control_error(
            FsMutationErrorCode::InvalidRequest,
            "small-text mutation was routed to the filesystem control executor",
        )),
    }
    .map(FsMutationReply::Control)
    .unwrap_or_else(|reply| reply)
}

fn create_directory(
    policy: &ReadPolicy,
    requested: &str,
) -> Result<FsControlResult, FsMutationReply> {
    let target = prepare_new_target(policy, requested)?;
    fs::create_dir(&target).map_err(|_| control_io("directory creation failed"))?;
    Ok(FsControlResult::CreatedDirectory {
        path: path_string(&target)?,
    })
}

fn copy_file(
    policy: &ReadPolicy,
    source: &str,
    destination: &str,
) -> Result<FsControlResult, FsMutationReply> {
    let source = canonical_existing_target(policy, source)?;
    let metadata =
        fs::symlink_metadata(&source).map_err(|_| control_io("copy source metadata failed"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(control_error(
            FsMutationErrorCode::NotFile,
            "copy currently supports regular files only; use directory transfer for directories",
        ));
    }
    let destination = prepare_new_target(policy, destination)?;
    fs::copy(&source, &destination).map_err(|_| control_io("file copy failed"))?;
    Ok(FsControlResult::Copied {
        source: path_string(&source)?,
        destination: path_string(&destination)?,
    })
}

fn move_path(
    policy: &ReadPolicy,
    source: &str,
    destination: &str,
) -> Result<FsControlResult, FsMutationReply> {
    let source = canonical_existing_target(policy, source)?;
    let metadata =
        fs::symlink_metadata(&source).map_err(|_| control_io("move source metadata failed"))?;
    if metadata.file_type().is_symlink() {
        return Err(control_denied(
            "move source cannot be a symlink/reparse point",
        ));
    }
    let destination = prepare_new_target(policy, destination)?;
    fs::rename(&source, &destination).map_err(|_| {
        control_io("atomic move failed; cross-filesystem moves are intentionally not emulated")
    })?;
    Ok(FsControlResult::Moved {
        source: path_string(&source)?,
        destination: path_string(&destination)?,
    })
}

fn trash_path(
    policy: &ReadPolicy,
    managed_root: Option<&Path>,
    request_id: RequestId,
    requested: &str,
) -> Result<FsControlResult, FsMutationReply> {
    let target = canonical_existing_target(policy, requested)?;
    let metadata =
        fs::symlink_metadata(&target).map_err(|_| control_io("trash target metadata failed"))?;
    if metadata.file_type().is_symlink() {
        return Err(control_denied(
            "trash target cannot be a symlink/reparse point",
        ));
    }
    let trash_target = trash_api_path(&target);
    let original_path = path_string(&trash_target)?;
    let trash_id = request_id.to_string();

    #[cfg(windows)]
    let before = windows_recycle_snapshot(&trash_target)?;
    #[cfg(target_os = "linux")]
    let before = system_trash_items()?
        .into_iter()
        .map(|item| system_item_key(&item))
        .collect::<BTreeSet<_>>();

    trash::delete(&trash_target)
        .map_err(|_| control_io("operating-system trash operation failed"))?;

    #[cfg(windows)]
    let (deleted_unix_ms, system_id) = {
        let record = find_new_windows_recycle_record(&before, &trash_target)?.ok_or_else(|| {
            control_io(
                "Windows moved the path to Recycle Bin but Clew could not verify its recovery metadata",
            )
        })?;
        (
            record.deleted_unix_ms,
            Some(record.info_path.to_string_lossy().into_owned()),
        )
    };
    #[cfg(target_os = "linux")]
    let (deleted_unix_ms, system_id) = {
        let item = find_new_system_trash_item(&before, &trash_target)?.ok_or_else(|| {
            control_io("the path reached Trash but Clew could not verify its recovery metadata")
        })?;
        (
            item.time_deleted.max(0) as u64 * 1000,
            Some(system_item_key(&item)),
        )
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let (deleted_unix_ms, system_id) = (unix_ms()?, None);

    let item = FsTrashItem {
        trash_id: trash_id.clone(),
        original_path,
        deleted_unix_ms,
    };
    with_ledger(managed_root, |_, ledger| {
        ledger.trash.insert(
            trash_id.clone(),
            ManagedTrashRecord {
                item: item.clone(),
                system_id,
            },
        );
        Ok(FsControlResult::Trashed(item))
    })
}

fn trash_restore(
    managed_root: Option<&Path>,
    trash_id: &str,
) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |_, ledger| {
        let record = ledger
            .trash
            .get(trash_id)
            .cloned()
            .ok_or_else(|| control_not_found("Clew trash record was not found"))?;
        let system_id = record.system_id.as_deref().ok_or_else(|| {
            control_unsupported("this platform cannot enumerate/restore exact Trash items yet")
        })?;
        system_restore(system_id)?;
        ledger.trash.remove(trash_id);
        ledger.purge.remove(trash_id);
        Ok(FsControlResult::Restored {
            trash_id: trash_id.to_owned(),
            path: record.item.original_path,
        })
    })
}

fn trash_purge_prepare(
    managed_root: Option<&Path>,
    trash_id: &str,
) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |_, ledger| {
        let record = ledger
            .trash
            .get(trash_id)
            .ok_or_else(|| control_not_found("Clew trash record was not found"))?;
        if record.system_id.is_none() {
            return Err(control_unsupported(
                "this platform cannot precisely purge tracked Trash items yet",
            ));
        }
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| control_io("secure confirmation token generation failed"))?;
        let token = hex_bytes(&random);
        let expires_unix_ms = unix_ms()?.saturating_add(PURGE_CONFIRM_TTL_MS);
        ledger.purge.insert(
            trash_id.to_owned(),
            PendingPurge {
                token: token.clone(),
                expires_unix_ms,
            },
        );
        Ok(FsControlResult::PurgePrepared {
            trash_id: trash_id.to_owned(),
            confirmation_token: token,
            expires_unix_ms,
        })
    })
}

fn trash_purge_confirm(
    managed_root: Option<&Path>,
    trash_id: &str,
    confirmation_token: &str,
) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |_, ledger| {
        let pending = ledger
            .purge
            .get(trash_id)
            .cloned()
            .ok_or_else(|| control_denied("permanent delete requires a fresh prepare step"))?;
        if pending.token != confirmation_token || unix_ms()? > pending.expires_unix_ms {
            return Err(control_denied(
                "permanent delete confirmation token is wrong or expired",
            ));
        }
        let record = ledger
            .trash
            .get(trash_id)
            .cloned()
            .ok_or_else(|| control_not_found("Clew trash record was not found"))?;
        let system_id = record.system_id.as_deref().ok_or_else(|| {
            control_unsupported("this platform cannot precisely purge tracked Trash items yet")
        })?;
        system_purge(system_id)?;
        ledger.trash.remove(trash_id);
        ledger.purge.remove(trash_id);
        Ok(FsControlResult::Purged {
            trash_id: trash_id.to_owned(),
        })
    })
}

fn temp_create(
    managed_root: Option<&Path>,
    request_id: RequestId,
    kind: FsManagedTempKind,
    description: String,
    ttl_ms: u64,
) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |root, ledger| {
        let resource_id = request_id.to_string();
        if ledger.temp.contains_key(&resource_id) {
            return Ok(FsControlResult::TempCreated(
                ledger.temp[&resource_id].clone(),
            ));
        }
        let resource_root = root.join("temp").join(&resource_id);
        fs::create_dir_all(&resource_root)
            .map_err(|_| control_io("managed temporary resource root creation failed"))?;
        let resource_path = match kind {
            FsManagedTempKind::File => {
                let path = resource_root.join("resource.tmp");
                File::options()
                    .create_new(true)
                    .write(true)
                    .open(&path)
                    .map_err(|_| control_io("managed temporary file creation failed"))?;
                path
            }
            FsManagedTempKind::Directory => {
                let path = resource_root.join("workspace");
                fs::create_dir(&path)
                    .map_err(|_| control_io("managed temporary directory creation failed"))?;
                path
            }
        };
        let created_unix_ms = unix_ms()?;
        let expires_unix_ms = created_unix_ms.saturating_add(ttl_ms);
        let resource = FsManagedTempResource {
            resource_id: resource_id.clone(),
            kind,
            path: path_string(&resource_path)?,
            description: description.trim().to_owned(),
            created_unix_ms,
            expires_unix_ms,
        };
        write_about_file(&resource_root, &resource)?;
        ledger.temp.insert(resource_id, resource.clone());
        Ok(FsControlResult::TempCreated(resource))
    })
}

fn temp_release(
    managed_root: Option<&Path>,
    resource_id: &str,
) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |root, ledger| {
        let resource = ledger
            .temp
            .get(resource_id)
            .cloned()
            .ok_or_else(|| control_not_found("managed temporary resource was not found"))?;
        remove_temp_resource(root, &resource)?;
        ledger.temp.remove(resource_id);
        Ok(FsControlResult::TempReleased {
            resource_id: resource_id.to_owned(),
        })
    })
}

fn temp_gc(managed_root: Option<&Path>) -> Result<FsControlResult, FsMutationReply> {
    with_ledger(managed_root, |root, ledger| {
        let now = unix_ms()?;
        let expired = ledger
            .temp
            .values()
            .filter(|resource| resource.expires_unix_ms <= now)
            .take(HARD_MAX_FS_CONTROL_ITEMS)
            .cloned()
            .collect::<Vec<_>>();
        let mut removed_resource_ids = Vec::with_capacity(expired.len());
        for resource in expired {
            remove_temp_resource(root, &resource)?;
            ledger.temp.remove(&resource.resource_id);
            removed_resource_ids.push(resource.resource_id);
        }
        Ok(FsControlResult::TempGc {
            removed_resource_ids,
        })
    })
}

fn write_about_file(
    resource_root: &Path,
    resource: &FsManagedTempResource,
) -> Result<(), FsMutationReply> {
    let text = format!(
        "Managed by Clew\nResourceId: {}\nKind: {:?}\nPurpose: {}\nCreatedUnixMs: {}\nExpiresUnixMs: {}\n\nThis directory is owned by Clew's managed temporary-resource service. Use temp_release or temp_gc instead of leaving it orphaned.\n",
        resource.resource_id,
        resource.kind,
        resource.description,
        resource.created_unix_ms,
        resource.expires_unix_ms
    );
    fs::write(resource_root.join("ABOUT.txt"), text)
        .map_err(|_| control_io("managed temporary ABOUT.txt write failed"))
}

fn remove_temp_resource(
    managed_root: &Path,
    resource: &FsManagedTempResource,
) -> Result<(), FsMutationReply> {
    let temp_root = managed_root.join("temp");
    let resource_root = temp_root.join(&resource.resource_id);
    if resource_root.parent() != Some(temp_root.as_path()) {
        return Err(control_denied(
            "managed temporary resource escaped its namespace",
        ));
    }
    match fs::symlink_metadata(&resource_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(control_denied(
                "managed temporary resource root was replaced by an unsafe filesystem object",
            ));
        }
        Ok(_) => fs::remove_dir_all(&resource_root)
            .map_err(|_| control_io("managed temporary resource cleanup failed"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(control_io("managed temporary resource metadata failed")),
    }
    Ok(())
}

fn canonical_existing_target(
    policy: &ReadPolicy,
    requested: &str,
) -> Result<PathBuf, FsMutationReply> {
    let requested = expand_target_path(requested)
        .map_err(|_| control_denied("filesystem control path must be absolute or use ~/..."))?;
    if !requested.is_absolute() {
        return Err(control_denied(
            "filesystem control path must be absolute or use ~/...",
        ));
    }
    let metadata = fs::symlink_metadata(&requested).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            control_not_found("filesystem control target was not found")
        } else {
            control_io("filesystem control target metadata failed")
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err(control_denied(
            "filesystem control target cannot be a symlink/reparse point",
        ));
    }
    let canonical = fs::canonicalize(&requested)
        .map_err(|_| control_io("filesystem control target canonicalization failed"))?;
    ensure_allowed_path(policy, &canonical)?;
    Ok(canonical)
}

fn prepare_new_target(policy: &ReadPolicy, requested: &str) -> Result<PathBuf, FsMutationReply> {
    let requested = expand_target_path(requested).map_err(|_| {
        control_denied("filesystem control destination must be absolute or use ~/...")
    })?;
    if !requested.is_absolute() {
        return Err(control_denied(
            "filesystem control destination must be absolute or use ~/...",
        ));
    }
    let Some(Component::Normal(name)) = requested.components().next_back() else {
        return Err(control_error(
            FsMutationErrorCode::InvalidRequest,
            "filesystem control destination must end in a normal name",
        ));
    };
    let parent = requested.parent().ok_or_else(|| {
        control_error(
            FsMutationErrorCode::InvalidRequest,
            "filesystem control destination parent is invalid",
        )
    })?;
    let parent = fs::canonicalize(parent).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            control_not_found("filesystem control destination parent was not found")
        } else {
            control_io("filesystem control destination parent canonicalization failed")
        }
    })?;
    ensure_allowed_path(policy, &parent)?;
    let target = parent.join(name);
    match fs::symlink_metadata(&target) {
        Ok(_) => Err(control_error(
            FsMutationErrorCode::AlreadyExists,
            "filesystem control destination already exists",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(target),
        Err(_) => Err(control_io("filesystem control destination metadata failed")),
    }
}

fn ensure_allowed_path(policy: &ReadPolicy, path: &Path) -> Result<(), FsMutationReply> {
    if policy.all_filesystem {
        return Ok(());
    }
    for root in &policy.roots {
        let Ok(root) = expand_target_path(root) else {
            continue;
        };
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if path.starts_with(root) {
            return Ok(());
        }
    }
    Err(control_denied(
        "filesystem control target is outside the signed roots",
    ))
}

fn with_ledger<T>(
    managed_root: Option<&Path>,
    operation: impl FnOnce(&Path, &mut ManagedFsLedger) -> Result<T, FsMutationReply>,
) -> Result<T, FsMutationReply> {
    let root = managed_root.ok_or_else(|| {
        control_unsupported("managed filesystem state requires a persistent Host state directory")
    })?;
    fs::create_dir_all(root).map_err(|_| control_io("managed filesystem state creation failed"))?;
    let mut ledger = load_ledger(root)?;
    let result = operation(root, &mut ledger)?;
    save_ledger(root, &ledger)?;
    Ok(result)
}

fn load_ledger(root: &Path) -> Result<ManagedFsLedger, FsMutationReply> {
    let path = root.join("ledger.json");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManagedFsLedger {
                version: LEDGER_VERSION,
                ..ManagedFsLedger::default()
            });
        }
        Err(_) => return Err(control_io("managed filesystem ledger metadata failed")),
    };
    if metadata.len() > LEDGER_MAX_BYTES {
        return Err(control_io(
            "managed filesystem ledger exceeds its hard bound",
        ));
    }
    let bytes = fs::read(&path).map_err(|_| control_io("managed filesystem ledger read failed"))?;
    let ledger: ManagedFsLedger = serde_json::from_slice(&bytes)
        .map_err(|_| control_io("managed filesystem ledger is malformed"))?;
    if ledger.version != LEDGER_VERSION
        || ledger.trash.len() > HARD_MAX_FS_CONTROL_ITEMS
        || ledger.temp.len() > HARD_MAX_FS_CONTROL_ITEMS
        || ledger.purge.len() > HARD_MAX_FS_CONTROL_ITEMS
    {
        return Err(control_io("managed filesystem ledger failed validation"));
    }
    Ok(ledger)
}

fn save_ledger(root: &Path, ledger: &ManagedFsLedger) -> Result<(), FsMutationReply> {
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|_| control_io("managed filesystem ledger encoding failed"))?;
    if bytes.len() as u64 > LEDGER_MAX_BYTES {
        return Err(control_io(
            "managed filesystem ledger exceeds its hard bound",
        ));
    }
    let mut temp = NamedTempFile::new_in(root)
        .map_err(|_| control_io("managed filesystem ledger temp creation failed"))?;
    temp.write_all(&bytes)
        .map_err(|_| control_io("managed filesystem ledger temp write failed"))?;
    temp.as_file()
        .sync_all()
        .map_err(|_| control_io("managed filesystem ledger temp sync failed"))?;
    temp.persist(root.join("ledger.json"))
        .map_err(|_| control_io("managed filesystem ledger replace failed"))?;
    #[cfg(unix)]
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| control_io("managed filesystem state directory sync failed"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn system_trash_items() -> Result<Vec<trash::TrashItem>, FsMutationReply> {
    trash::os_limited::list().map_err(|_| control_io("operating-system Trash listing failed"))
}

#[cfg(windows)]
fn system_restore(system_id: &str) -> Result<(), FsMutationReply> {
    windows_restore(Path::new(system_id))
}

#[cfg(target_os = "linux")]
fn system_restore(system_id: &str) -> Result<(), FsMutationReply> {
    let item = find_system_item(system_id)?;
    trash::os_limited::restore_all([item])
        .map_err(|_| control_io("operating-system Trash restore failed"))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn system_restore(_system_id: &str) -> Result<(), FsMutationReply> {
    Err(control_unsupported(
        "precise Trash restore is not implemented on this platform",
    ))
}

#[cfg(windows)]
fn system_purge(system_id: &str) -> Result<(), FsMutationReply> {
    windows_purge(Path::new(system_id))
}

#[cfg(target_os = "linux")]
fn system_purge(system_id: &str) -> Result<(), FsMutationReply> {
    let item = find_system_item(system_id)?;
    trash::os_limited::purge_all([item])
        .map_err(|_| control_io("operating-system Trash purge failed"))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn system_purge(_system_id: &str) -> Result<(), FsMutationReply> {
    Err(control_unsupported(
        "precise Trash purge is not implemented on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn find_system_item(system_id: &str) -> Result<trash::TrashItem, FsMutationReply> {
    system_trash_items()?
        .into_iter()
        .find(|item| system_item_key(item) == system_id)
        .ok_or_else(|| {
            control_not_found("tracked item is no longer present in the operating-system Trash")
        })
}

#[cfg(windows)]
fn windows_recycle_snapshot(target: &Path) -> Result<BTreeSet<String>, FsMutationReply> {
    let root = windows_recycle_root(target)?;
    Ok(windows_recycle_info_paths(&root)?
        .into_iter()
        .map(|path| windows_path_key(&path))
        .collect())
}

#[cfg(windows)]
fn windows_recycle_root(target: &Path) -> Result<PathBuf, FsMutationReply> {
    use std::path::Prefix;

    let Some(Component::Prefix(prefix)) = target.components().next() else {
        return Err(control_unsupported(
            "Windows Recycle Bin tracking requires a local drive path",
        ));
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        _ => {
            return Err(control_unsupported(
                "Windows Recycle Bin tracking for UNC/network paths is not supported",
            ));
        }
    };
    Ok(PathBuf::from(format!(
        "{}:\\$Recycle.Bin",
        char::from(drive)
    )))
}

#[cfg(windows)]
fn windows_recycle_info_paths(root: &Path) -> Result<Vec<PathBuf>, FsMutationReply> {
    let mut paths = Vec::new();
    let sid_dirs =
        fs::read_dir(root).map_err(|_| control_io("Windows Recycle Bin root is unavailable"))?;
    for sid_entry in sid_dirs.flatten() {
        let Ok(metadata) = sid_entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(sid_entry.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("$I") {
                paths.push(entry.path());
            }
        }
    }
    Ok(paths)
}

#[cfg(windows)]
fn find_new_windows_recycle_record(
    before: &BTreeSet<String>,
    target: &Path,
) -> Result<Option<WindowsRecycleRecord>, FsMutationReply> {
    let root = windows_recycle_root(target)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut matches = Vec::new();
        for info_path in windows_recycle_info_paths(&root)? {
            if before.contains(&windows_path_key(&info_path)) {
                continue;
            }
            let Ok(record) = windows_recycle_record(&info_path) else {
                continue;
            };
            if windows_path_key(&record.original_path) == windows_path_key(target) {
                matches.push(record);
            }
        }
        if matches.len() == 1 {
            return Ok(matches.pop());
        }
        if matches.len() > 1 {
            return Err(control_io(
                "Windows Recycle Bin produced ambiguous recovery metadata for one deletion",
            ));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn windows_recycle_record(info_path: &Path) -> Result<WindowsRecycleRecord, FsMutationReply> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt as _};

    let file_name = info_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| control_io("Windows Recycle Bin metadata name is invalid"))?;
    let Some(suffix) = file_name.strip_prefix("$I") else {
        return Err(control_io(
            "Windows Recycle Bin metadata has an invalid name",
        ));
    };
    let metadata = fs::metadata(info_path)
        .map_err(|_| control_not_found("Windows Recycle Bin metadata is missing"))?;
    if metadata.len() < 24 || metadata.len() > 64 * 1024 {
        return Err(control_io("Windows Recycle Bin metadata size is invalid"));
    }
    let bytes =
        fs::read(info_path).map_err(|_| control_io("Windows Recycle Bin metadata read failed"))?;
    let version = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let filetime = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let (path_offset, path_units) = match version {
        1 => (24_usize, (bytes.len() - 24) / 2),
        2 => {
            if bytes.len() < 28 {
                return Err(control_io("Windows Recycle Bin v2 metadata is truncated"));
            }
            let units = u32::from_le_bytes(bytes[24..28].try_into().unwrap()) as usize;
            if units == 0 || units > 32_768 || 28_usize.saturating_add(units * 2) > bytes.len() {
                return Err(control_io("Windows Recycle Bin v2 path length is invalid"));
            }
            (28, units)
        }
        _ => {
            return Err(control_unsupported(
                "Windows Recycle Bin metadata version is not supported",
            ));
        }
    };
    let mut wide = bytes[path_offset..path_offset + path_units * 2]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    while wide.last() == Some(&0) {
        wide.pop();
    }
    if wide.is_empty() {
        return Err(control_io("Windows Recycle Bin original path is empty"));
    }
    let original_path = PathBuf::from(OsString::from_wide(&wide));
    let recycle_root = windows_recycle_root(&original_path)?;
    let Some(sid_dir) = info_path.parent() else {
        return Err(control_io("Windows Recycle Bin metadata parent is invalid"));
    };
    let Some(actual_root) = sid_dir.parent() else {
        return Err(control_io("Windows Recycle Bin metadata root is invalid"));
    };
    if windows_path_key(actual_root) != windows_path_key(&recycle_root) {
        return Err(control_denied(
            "Windows Recycle Bin metadata is outside the original volume's recycle root",
        ));
    }
    let data_path = sid_dir.join(format!("$R{suffix}"));
    let deleted_unix_ms = filetime.saturating_sub(WINDOWS_FILETIME_UNIX_EPOCH_100NS) / 10_000;
    Ok(WindowsRecycleRecord {
        info_path: info_path.to_path_buf(),
        data_path,
        original_path,
        deleted_unix_ms,
    })
}

#[cfg(windows)]
fn windows_restore(info_path: &Path) -> Result<(), FsMutationReply> {
    let record = windows_recycle_record(info_path)?;
    match fs::symlink_metadata(&record.original_path) {
        Ok(_) => {
            return Err(control_error(
                FsMutationErrorCode::AlreadyExists,
                "restore destination already exists",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(control_io("restore destination metadata failed")),
    }
    let parent = record
        .original_path
        .parent()
        .ok_or_else(|| control_io("restore destination parent is invalid"))?;
    if !parent.is_dir() {
        return Err(control_not_found(
            "restore destination parent no longer exists",
        ));
    }
    fs::symlink_metadata(&record.data_path)
        .map_err(|_| control_not_found("Windows Recycle Bin payload is missing"))?;
    fs::rename(&record.data_path, &record.original_path)
        .map_err(|_| control_io("Windows Recycle Bin restore move failed"))?;
    let _ = fs::remove_file(&record.info_path);
    Ok(())
}

#[cfg(windows)]
fn windows_purge(info_path: &Path) -> Result<(), FsMutationReply> {
    let record = windows_recycle_record(info_path)?;
    let metadata = fs::symlink_metadata(&record.data_path)
        .map_err(|_| control_not_found("Windows Recycle Bin payload is missing"))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(&record.data_path)
            .map_err(|_| control_io("Windows Recycle Bin directory purge failed"))?;
    } else {
        fs::remove_file(&record.data_path)
            .map_err(|_| control_io("Windows Recycle Bin file purge failed"))?;
    }
    let _ = fs::remove_file(&record.info_path);
    Ok(())
}

#[cfg(windows)]
fn trash_api_path(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = raw.strip_prefix("\\\\?\\UNC\\") {
        PathBuf::from(format!("\\\\{rest}"))
    } else if let Some(rest) = raw.strip_prefix("\\\\?\\") {
        PathBuf::from(rest)
    } else {
        PathBuf::from(raw)
    }
}

#[cfg(not(windows))]
fn trash_api_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(target_os = "linux")]
fn find_new_system_trash_item(
    before: &BTreeSet<String>,
    target: &Path,
) -> Result<Option<trash::TrashItem>, FsMutationReply> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let new_items = system_trash_items()?
            .into_iter()
            .filter(|item| !before.contains(&system_item_key(item)))
            .collect::<Vec<_>>();

        let mut exact = new_items
            .iter()
            .filter(|item| item.original_path() == target);
        if let Some(item) = exact.next() {
            if exact.next().is_none() {
                return Ok(Some(item.clone()));
            }
        }

        if new_items.len() == 1
            && target
                .file_name()
                .is_some_and(|name| new_items[0].name == name)
        {
            return Ok(new_items.into_iter().next());
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn windows_path_key(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('/', "\\");
    let normalized = if let Some(rest) = raw.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = raw.strip_prefix("\\\\?\\") {
        rest.to_owned()
    } else {
        raw
    };
    normalized.trim_end_matches('\\').to_lowercase()
}

#[cfg(target_os = "linux")]
fn system_item_key(item: &trash::TrashItem) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    hex_bytes(item.id.as_os_str().as_bytes())
}

fn path_string(path: &Path) -> Result<String, FsMutationReply> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        control_error(
            FsMutationErrorCode::InvalidRequest,
            "path is not valid UTF-8",
        )
    })
}

fn unix_ms() -> Result<u64, FsMutationReply> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| control_io("system clock is before UNIX epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| control_io("system clock overflow"))
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn control_error(code: FsMutationErrorCode, message: impl Into<String>) -> FsMutationReply {
    FsMutationReply::error(code, message)
}

fn control_denied(message: impl Into<String>) -> FsMutationReply {
    control_error(FsMutationErrorCode::Denied, message)
}

fn control_not_found(message: impl Into<String>) -> FsMutationReply {
    control_error(FsMutationErrorCode::NotFound, message)
}

fn control_io(message: impl Into<String>) -> FsMutationReply {
    control_error(FsMutationErrorCode::Io, message)
}

fn control_unsupported(message: impl Into<String>) -> FsMutationReply {
    control_error(FsMutationErrorCode::Unsupported, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};
    use tempfile::tempdir;

    fn restricted_policy(root: &Path) -> ReadPolicy {
        ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4096, 2000).unwrap()
    }

    fn expect_control(reply: FsMutationReply) -> FsControlResult {
        match reply {
            FsMutationReply::Control(result) => result,
            other => panic!("expected filesystem control result, got {other:?}"),
        }
    }

    fn expect_error_code(reply: FsMutationReply, expected: FsMutationErrorCode) {
        match reply {
            FsMutationReply::Error(error) => assert_eq!(error.code, expected),
            other => panic!("expected filesystem control error, got {other:?}"),
        }
    }

    #[test]
    fn controlled_mkdir_copy_move_obey_scope_and_never_overwrite() {
        let allowed = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let policy = restricted_policy(allowed.path());
        let managed = allowed.path().join(".clew-managed");

        let created = allowed.path().join("created");
        let result = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::CreateDirectory {
                path: created.to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(result, FsControlResult::CreatedDirectory { .. }));
        assert!(created.is_dir());

        let source = allowed.path().join("source.txt");
        fs::write(&source, b"managed-fs-copy").unwrap();
        let copy = allowed.path().join("copy.txt");
        let result = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::Copy {
                source: source.to_string_lossy().into_owned(),
                destination: copy.to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(result, FsControlResult::Copied { .. }));
        assert_eq!(fs::read(&copy).unwrap(), b"managed-fs-copy");

        expect_error_code(
            execute_control(
                &policy,
                Some(&managed),
                RequestId::new(),
                FsMutationRequest::Copy {
                    source: source.to_string_lossy().into_owned(),
                    destination: copy.to_string_lossy().into_owned(),
                },
            ),
            FsMutationErrorCode::AlreadyExists,
        );

        let moved = allowed.path().join("moved.txt");
        let result = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::Move {
                source: copy.to_string_lossy().into_owned(),
                destination: moved.to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(result, FsControlResult::Moved { .. }));
        assert!(!copy.exists());
        assert!(moved.is_file());

        let outside_target = outside.path().join("escaped");
        expect_error_code(
            execute_control(
                &policy,
                Some(&managed),
                RequestId::new(),
                FsMutationRequest::CreateDirectory {
                    path: outside_target.to_string_lossy().into_owned(),
                },
            ),
            FsMutationErrorCode::Denied,
        );
        assert!(!outside_target.exists());
    }

    #[test]
    fn all_filesystem_allows_os_visible_paths_without_fake_root_sentinel() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let policy = ReadPolicy::all_filesystem(4096, 2000).unwrap();
        let managed = first.path().join(".clew-managed");
        let target = second.path().join("created-by-all-filesystem");

        let result = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::CreateDirectory {
                path: target.to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(result, FsControlResult::CreatedDirectory { .. }));
        assert!(target.is_dir());
    }

    #[test]
    fn managed_temp_create_list_release_and_gc_stays_in_clew_namespace() {
        let state = tempdir().unwrap();
        let managed = state.path().join("managed-fs");
        let policy = ReadPolicy::all_filesystem(4096, 2000).unwrap();
        let unrelated = state.path().join("do-not-touch.txt");
        fs::write(&unrelated, b"keep").unwrap();

        let file_id = RequestId::new();
        let file = expect_control(execute_control(
            &policy,
            Some(&managed),
            file_id,
            FsMutationRequest::TempCreate {
                temp_kind: FsManagedTempKind::File,
                description: "download staging owned by test".into(),
                ttl_ms: 60_000,
            },
        ));
        let FsControlResult::TempCreated(file) = file else {
            panic!("expected temp file creation");
        };
        assert_eq!(file.resource_id, file_id.to_string());
        assert!(Path::new(&file.path).is_file());
        let resource_root = Path::new(&file.path).parent().unwrap();
        let about = fs::read_to_string(resource_root.join("ABOUT.txt")).unwrap();
        assert!(about.contains("Managed by Clew"));
        assert!(about.contains("download staging owned by test"));

        let listed = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TempList,
        ));
        let FsControlResult::TempItems { items } = listed else {
            panic!("expected temp list");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].resource_id, file.resource_id);

        let released = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TempRelease {
                resource_id: file.resource_id.clone(),
            },
        ));
        assert!(matches!(released, FsControlResult::TempReleased { .. }));
        assert!(!resource_root.exists());

        let expiring_id = RequestId::new();
        let expiring = expect_control(execute_control(
            &policy,
            Some(&managed),
            expiring_id,
            FsMutationRequest::TempCreate {
                temp_kind: FsManagedTempKind::Directory,
                description: "short lived workspace".into(),
                ttl_ms: 1,
            },
        ));
        let FsControlResult::TempCreated(expiring) = expiring else {
            panic!("expected temp directory creation");
        };
        let expiring_root = Path::new(&expiring.path).parent().unwrap().to_path_buf();
        thread::sleep(Duration::from_millis(5));
        let gc = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TempGc,
        ));
        let FsControlResult::TempGc {
            removed_resource_ids,
        } = gc
        else {
            panic!("expected temp gc result");
        };
        assert_eq!(removed_resource_ids, vec![expiring_id.to_string()]);
        assert!(!expiring_root.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an available Windows Recycle Bin; run explicitly on a real desktop acceptance machine"]
    fn windows_trash_restore_and_two_phase_purge_are_exact_and_tracked() {
        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::Builder::new()
            .prefix("clew-trash-acceptance-")
            .tempdir_in(cwd)
            .unwrap();
        let managed = root.path().join("managed-fs");
        let policy = restricted_policy(root.path());
        let path = root.path().join("trash-me.txt");
        fs::write(&path, b"recoverable").unwrap();

        let first_id = RequestId::new();
        let trashed = expect_control(execute_control(
            &policy,
            Some(&managed),
            first_id,
            FsMutationRequest::Trash {
                path: path.to_string_lossy().into_owned(),
            },
        ));
        assert!(matches!(trashed, FsControlResult::Trashed(_)));
        assert!(!path.exists());

        let restored = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TrashRestore {
                trash_id: first_id.to_string(),
            },
        ));
        assert!(matches!(restored, FsControlResult::Restored { .. }));
        assert_eq!(fs::read(&path).unwrap(), b"recoverable");

        let second_id = RequestId::new();
        expect_control(execute_control(
            &policy,
            Some(&managed),
            second_id,
            FsMutationRequest::Trash {
                path: path.to_string_lossy().into_owned(),
            },
        ));
        let prepared = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TrashPurgePrepare {
                trash_id: second_id.to_string(),
            },
        ));
        let FsControlResult::PurgePrepared {
            confirmation_token, ..
        } = prepared
        else {
            panic!("expected purge preparation");
        };

        expect_error_code(
            execute_control(
                &policy,
                Some(&managed),
                RequestId::new(),
                FsMutationRequest::TrashPurgeConfirm {
                    trash_id: second_id.to_string(),
                    confirmation_token: "wrong-token".into(),
                },
            ),
            FsMutationErrorCode::Denied,
        );

        let purged = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TrashPurgeConfirm {
                trash_id: second_id.to_string(),
                confirmation_token,
            },
        ));
        assert!(matches!(purged, FsControlResult::Purged { .. }));
        let listed = expect_control(execute_control(
            &policy,
            Some(&managed),
            RequestId::new(),
            FsMutationRequest::TrashList,
        ));
        assert_eq!(listed, FsControlResult::TrashItems { items: Vec::new() });
    }
}
