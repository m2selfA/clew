use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use clew_core::{MAX_STATE_DOCUMENT_SIZE, StateLayout};
use clew_identity::{
    BackupError, ControllerBackupPayload, ControllerIdentityStore, DeviceIdentityStoreError,
    RecoveryReview, RestoredController, StoredControllerIdentity, backup_from_json, backup_to_json,
    decrypt_controller_backup, encrypt_controller_backup,
};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    ControlStoreError, ControllerConfig, ControllerControlSnapshot, ControllerControlStore,
    lock::{ControllerOwnership, OwnershipAttempt},
};

pub fn export_controller_backup(
    path: &Path,
    passphrase: &str,
    identity: &StoredControllerIdentity,
    snapshot: &ControllerControlSnapshot,
    created_unix_ms: u64,
) -> Result<(), ControllerBackupIoError> {
    let payload = ControllerBackupPayload::capture(
        identity,
        snapshot.registry.clone(),
        snapshot.catalog.clone(),
        created_unix_ms,
    )?;
    let encrypted = encrypt_controller_backup(&payload, passphrase)?;
    let encoded = backup_to_json(&encrypted)?;
    write_new_backup_file(path, &encoded)
}

pub fn restore_controller_backup(
    config: &ControllerConfig,
    path: &Path,
    passphrase: &str,
) -> Result<RecoveryReview, ControllerBackupIoError> {
    config.prepare_state_dir()?;
    let layout = config.state_layout();
    let instance_id = format!("restore-{}", Uuid::new_v4());
    let _ownership = match ControllerOwnership::try_acquire(&layout, &instance_id)? {
        OwnershipAttempt::Acquired(ownership) => ownership,
        OwnershipAttempt::Busy => return Err(ControllerBackupIoError::ControllerRunning),
    };
    ensure_empty_controller_state(&layout)?;

    let encoded = read_bounded(path)?;
    let encrypted = backup_from_json(&encoded)?;
    let RestoredController {
        identity,
        transport_identity_secret,
        registry,
        catalog,
        recovery_review,
    } = decrypt_controller_backup(&encrypted, passphrase, true)?;
    let controller_id = identity.controller_id();

    let identity_store = ControllerIdentityStore::new(layout.clone());
    identity_store.restore_empty(identity, transport_identity_secret)?;
    let restored = (|| -> Result<(), ControllerBackupIoError> {
        let mut control = ControllerControlStore::load_or_create(layout.clone(), controller_id)?;
        control.replace_restored_state(registry, catalog, recovery_review)?;
        Ok(())
    })();
    if let Err(error) = restored {
        rollback_controller_restore(&layout);
        return Err(error);
    }
    Ok(recovery_review)
}

fn ensure_empty_controller_state(layout: &StateLayout) -> Result<(), ControllerBackupIoError> {
    for path in controller_state_paths(layout) {
        if path.exists() {
            return Err(ControllerBackupIoError::StateNotEmpty(path));
        }
    }
    Ok(())
}

fn controller_state_paths(layout: &StateLayout) -> [PathBuf; 3] {
    [
        layout.controller_state_path(),
        layout.controller_control_slot_a_path(),
        layout.controller_control_slot_b_path(),
    ]
}

fn rollback_controller_restore(layout: &StateLayout) {
    for path in controller_state_paths(layout) {
        let _ = fs::remove_file(path);
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, ControllerBackupIoError> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_STATE_DOCUMENT_SIZE as u64 {
        return Err(ControllerBackupIoError::FileTooLarge(metadata.len()));
    }
    let encoded = fs::read(path)?;
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(ControllerBackupIoError::FileTooLarge(encoded.len() as u64));
    }
    Ok(encoded)
}

fn write_new_backup_file(path: &Path, encoded: &[u8]) -> Result<(), ControllerBackupIoError> {
    if encoded.len() > MAX_STATE_DOCUMENT_SIZE {
        return Err(ControllerBackupIoError::FileTooLarge(encoded.len() as u64));
    }
    if path.exists() {
        return Err(ControllerBackupIoError::DestinationExists(path.to_owned()));
    }
    let parent = path.parent().ok_or(ControllerBackupIoError::InvalidPath)?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let file_name = path
        .file_name()
        .ok_or(ControllerBackupIoError::InvalidPath)?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), ControllerBackupIoError> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(encoded)?;
        file.sync_all()?;
        if path.exists() {
            return Err(ControllerBackupIoError::DestinationExists(path.to_owned()));
        }
        fs::rename(&temporary, path)?;
        sync_parent(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), std::io::Error> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum ControllerBackupIoError {
    #[error(transparent)]
    Backup(#[from] BackupError),
    #[error(transparent)]
    Identity(#[from] DeviceIdentityStoreError),
    #[error(transparent)]
    Control(#[from] ControlStoreError),
    #[error("controller backup I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Controller is running; stop it before restoring a backup")]
    ControllerRunning,
    #[error("Controller backup restore requires empty state; found {0}")]
    StateNotEmpty(PathBuf),
    #[error("controller backup destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("controller backup path is invalid")]
    InvalidPath,
    #[error("controller backup file is too large: {0} bytes")]
    FileTooLarge(u64),
}

#[cfg(test)]
mod tests {
    use clew_identity::ControllerIdentityStore;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn backup_file_roundtrip_restores_same_controller_into_recovery_review() {
        let temp = tempdir().unwrap();
        let source_layout = StateLayout::new(temp.path().join("source"));
        fs::create_dir_all(source_layout.version_root()).unwrap();
        let identity = ControllerIdentityStore::new(source_layout.clone())
            .load_or_create()
            .unwrap();
        let control = ControllerControlStore::load_or_create(
            source_layout,
            identity.identity().controller_id(),
        )
        .unwrap();
        let backup_path = temp.path().join("controller.clew-backup");
        export_controller_backup(
            &backup_path,
            "correct horse battery staple",
            &identity,
            control.snapshot(),
            123,
        )
        .unwrap();

        let restored_config = ControllerConfig::new(temp.path().join("restored"));
        let review = restore_controller_backup(
            &restored_config,
            &backup_path,
            "correct horse battery staple",
        )
        .unwrap();
        assert_eq!(
            review.restored_controller_id,
            identity.identity().controller_id()
        );
        assert!(review.remote_access_paused);
        assert!(review.historical_bootstrap_closed);
        let restored_identity = ControllerIdentityStore::new(restored_config.state_layout())
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(
            restored_identity.identity().controller_id(),
            identity.identity().controller_id()
        );
        let restored_control = ControllerControlStore::load_or_create(
            restored_config.state_layout(),
            identity.identity().controller_id(),
        )
        .unwrap();
        assert_eq!(restored_control.recovery_review(), Some(review));
    }

    #[test]
    fn restore_refuses_nonempty_controller_state_before_decryption() {
        let temp = tempdir().unwrap();
        let config = ControllerConfig::new(temp.path().join("state"));
        config.prepare_state_dir().unwrap();
        ControllerIdentityStore::new(config.state_layout())
            .load_or_create()
            .unwrap();
        let backup = temp.path().join("not-even-read.backup");
        assert!(matches!(
            restore_controller_backup(&config, &backup, "correct horse battery staple"),
            Err(ControllerBackupIoError::StateNotEmpty(_))
        ));
    }
}
