#![forbid(unsafe_code)]

mod backup;
mod enrollment;
mod grant;
mod keys;
mod store;

pub use backup::{
    BackupError, ControllerBackupPayload, EncryptedControllerBackup, RecoveryReview,
    RestoredController, backup_from_json, backup_to_json, decrypt_controller_backup,
    encrypt_controller_backup,
};
pub use enrollment::{
    EnrollmentDeviceRecord, EnrollmentError, EnrollmentReceipt, EnrollmentRegistry,
    EnrollmentStatus, SignedSiteBootstrapPass, SiteAccessCredentialRecord, SiteBootstrapPayload,
    SiteBootstrapSpec,
};
pub use grant::PermissionGrant;
pub use keys::{
    ControllerIdentity, ControllerPublicIdentity, DeviceIdentity, DevicePublicIdentity,
    IdentityError,
};
pub use store::{
    ActiveDeviceIdentity, ControllerIdentityStore, DeviceIdentityStore, DeviceIdentityStoreError,
    PendingControllerActivation, PendingDeviceIdentity, StoredControllerIdentity,
};
