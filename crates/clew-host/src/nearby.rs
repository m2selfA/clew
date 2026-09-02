use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use clew_core::{SiteId, StateLayout};
use clew_identity::ControllerPublicIdentity;
use clew_transport::{MAX_NEARBY_CONNECTOR_FILE_BYTES, NearbyConnectorError, NearbyConnectorFile};
use thiserror::Error;

pub const NEARBY_CONNECTOR_FILE_NAME: &str = "nearby-connection.clew";
pub const LEGACY_NEARBY_CONNECTOR_FILE_NAME: &str = "附近连接.clew";

#[derive(Clone, Debug)]
pub struct NearbyConnectorStore {
    layout: StateLayout,
}

impl NearbyConnectorStore {
    #[must_use]
    pub fn new(layout: StateLayout) -> Self {
        Self { layout }
    }

    pub fn import_path(
        &self,
        path: &Path,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<NearbyConnectorFile, NearbyConnectorStoreError> {
        let file = NearbyConnectorFile::read(path)?;
        self.import_file(&file, controller, site_id)?;
        Ok(file)
    }

    pub fn import_file(
        &self,
        file: &NearbyConnectorFile,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<(), NearbyConnectorStoreError> {
        file.verify_routing_hint(controller, site_id)?;
        write_replace(
            &self
                .layout
                .nearby_connector_import_path(controller.controller_id, site_id),
            &file.to_bytes()?,
        )
    }

    pub fn save_export(
        &self,
        file: &NearbyConnectorFile,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<(), NearbyConnectorStoreError> {
        file.verify_for_target(controller, site_id, unix_ms()?)?;
        write_replace(
            &self
                .layout
                .nearby_connector_export_path(controller.controller_id, site_id),
            &file.to_bytes()?,
        )
    }

    pub fn load_import(
        &self,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<Option<NearbyConnectorFile>, NearbyConnectorStoreError> {
        self.load_verified(
            &self
                .layout
                .nearby_connector_import_path(controller.controller_id, site_id),
            controller,
            site_id,
        )
    }

    pub fn load_export(
        &self,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<Option<NearbyConnectorFile>, NearbyConnectorStoreError> {
        self.load_verified(
            &self
                .layout
                .nearby_connector_export_path(controller.controller_id, site_id),
            controller,
            site_id,
        )
    }

    pub fn export_latest(
        &self,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
        destination: &Path,
    ) -> Result<(), NearbyConnectorStoreError> {
        let file = self
            .load_export(controller, site_id)?
            .ok_or(NearbyConnectorStoreError::NoCurrentExport)?;
        write_replace(destination, &file.to_bytes()?)
    }

    fn load_verified(
        &self,
        path: &Path,
        controller: &ControllerPublicIdentity,
        site_id: SiteId,
    ) -> Result<Option<NearbyConnectorFile>, NearbyConnectorStoreError> {
        if !path.exists() {
            return Ok(None);
        }
        let file = NearbyConnectorFile::read(path)?;
        file.verify_routing_hint(controller, site_id)?;
        Ok(Some(file))
    }
}

fn write_replace(path: &Path, bytes: &[u8]) -> Result<(), NearbyConnectorStoreError> {
    if bytes.is_empty() || bytes.len() > MAX_NEARBY_CONNECTOR_FILE_BYTES {
        return Err(NearbyConnectorStoreError::InvalidEncodedSize(bytes.len()));
    }
    let parent = path
        .parent()
        .ok_or(NearbyConnectorStoreError::InvalidStatePath)?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| {
        std::io::Error::other(format!("secure random generation failed: {error}"))
    })?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temp = parent.join(format!(
        ".nearby-connector-{}-{suffix}.tmp",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    output.write_all(bytes)?;
    output.sync_all()?;
    drop(output);
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn unix_ms() -> Result<u64, NearbyConnectorStoreError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NearbyConnectorStoreError::ClockBeforeUnixEpoch)?
        .as_millis()
        .try_into()
        .map_err(|_| NearbyConnectorStoreError::ClockOverflow)
}

#[derive(Debug, Error)]
pub enum NearbyConnectorStoreError {
    #[error(transparent)]
    File(#[from] NearbyConnectorError),
    #[error("nearby Connector state I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("nearby Connector state path is invalid")]
    InvalidStatePath,
    #[error("nearby Connector encoded size is invalid: {0} bytes")]
    InvalidEncodedSize(usize),
    #[error("no verified nearby Connector export is available")]
    NoCurrentExport,
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock value does not fit in milliseconds")]
    ClockOverflow,
}

#[cfg(test)]
mod tests {
    use clew_core::DeviceId;
    use clew_identity::ControllerIdentity;
    use clew_transport::SignedConnectorLease;
    use iroh::{EndpointAddr, SecretKey, TransportAddr};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn imported_and_exported_hints_are_separate_and_verified() {
        let temp = tempdir().unwrap();
        let store = NearbyConnectorStore::new(StateLayout::new(temp.path()));
        let controller = ControllerIdentity::from_secret([161_u8; 32]);
        let site_id = SiteId::from_bytes([162_u8; 16]).unwrap();
        let device_id = DeviceId::from_bytes([163_u8; 16]).unwrap();
        let endpoint_id = SecretKey::from_bytes(&[164_u8; 32]).public();
        let mut addr = EndpointAddr::new(endpoint_id);
        addr.addrs
            .insert(TransportAddr::Ip("127.0.0.1:4343".parse().unwrap()));
        let now = unix_ms().unwrap();
        let lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            device_id,
            endpoint_id,
            now.saturating_sub(1_000),
            now + 60_000,
        )
        .unwrap();
        let file = NearbyConnectorFile::from_helper(addr, lease).unwrap();
        let source = temp.path().join(NEARBY_CONNECTOR_FILE_NAME);
        fs::write(&source, file.to_bytes().unwrap()).unwrap();

        store
            .import_path(&source, &controller.public_identity(), site_id)
            .unwrap();
        assert!(
            store
                .load_import(&controller.public_identity(), site_id)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .load_export(&controller.public_identity(), site_id)
                .unwrap()
                .is_none()
        );

        store
            .save_export(&file, &controller.public_identity(), site_id)
            .unwrap();
        let exported = temp.path().join("copy.clew");
        store
            .export_latest(&controller.public_identity(), site_id, &exported)
            .unwrap();
        assert_eq!(NearbyConnectorFile::read(&exported).unwrap(), file);

        let expired_lease = SignedConnectorLease::issue(
            &controller,
            site_id,
            device_id,
            endpoint_id,
            now.saturating_sub(120_000),
            now.saturating_sub(60_000),
        )
        .unwrap();
        let mut expired_addr = EndpointAddr::new(endpoint_id);
        expired_addr
            .addrs
            .insert(TransportAddr::Ip("127.0.0.1:4343".parse().unwrap()));
        let expired = NearbyConnectorFile::from_helper(expired_addr, expired_lease).unwrap();
        store
            .import_file(&expired, &controller.public_identity(), site_id)
            .unwrap();
        assert_eq!(
            store
                .load_import(&controller.public_identity(), site_id)
                .unwrap()
                .expect("expired historical route binding should remain usable as a candidate"),
            expired
        );
        assert!(matches!(
            expired.verify_for_target(&controller.public_identity(), site_id, now),
            Err(NearbyConnectorError::Lease(
                clew_transport::ConnectorLeaseError::Expired
            ))
        ));
    }
}
