use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

use clew_core::{
    ActivityEvent, ActivityResult, ControllerId, ControllerSiteRecord, DeviceId, DeviceRecord,
    DeviceSummary, InviteId, ReadPolicy, SiteId, StateLayout,
};
use clew_host::{HostRoleHint, OutfitAssetRef, OutfitPreset, OutfitProfile, SignedSiteClew};
use clew_identity::{PermissionGrant, RecoveryReview, SiteBootstrapSpec, StoredControllerIdentity};
use clew_transport::{
    FsGlobPage, FsGrepPage, FsMutationErrorBody, FsMutationErrorCode, FsMutationReply,
    FsMutationRequest, FsMutationResult, FsPathInfo, FsQueryErrorBody, FsQueryErrorCode,
    FsQueryReply, FsQueryRequest, FsWritePrecondition, IrohOuter, ReadErrorCode, ReadReply,
    ReadRequest, noise_static_public,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::{Instant, sleep, timeout},
};

use crate::{
    ControllerConfig, ControllerControlStore, LocalEndpoint, OutfitAssetInfo, OutfitAssetStore,
    OutfitEditPatch, OutfitLibrary, OutfitLibraryEntry, RemoteHub, export_controller_backup,
    transport,
};

pub const LOCAL_API_VERSION: u32 = 1;
pub const MAX_LOCAL_API_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_LOCAL_API_CONNECTIONS: usize = 16;
const LOCAL_API_IO_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_API_RESPONSE_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_ACTIVITY_LIST_LIMIT: u32 = 200;
const MAX_BACKUP_PATH_BYTES: usize = 4096;
const MAX_ASSET_IMPORT_PATH_BYTES: usize = 4096;
const SECRET_BYTES: usize = 32;
const SECRET_HEX_LEN: usize = SECRET_BYTES * 2;
const SECRET_LOAD_RETRY_WINDOW: Duration = Duration::from_secs(5);
const SECRET_LOAD_RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub ready: bool,
    pub controller_id: ControllerId,
    pub pid: u32,
    pub instance_id: String,
    pub started_unix_ms: u64,
    pub state_schema_version: u32,
    pub local_api_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_endpoint_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InviteIssueRequest {
    pub site_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outfit_id: Option<String>,
    pub roots: Vec<String>,
    pub max_claims: u32,
    pub valid_for_ms: u64,
    pub deployment_window_ms: u64,
    pub max_result_bytes: u32,
    pub read_timeout_ms: u32,
    #[serde(default)]
    pub allow_write: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InviteIssueResult {
    pub site_file: SignedSiteClew,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteReadRequest {
    pub device_id: DeviceId,
    pub path: String,
    pub offset: u64,
    pub limit: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteReadResult {
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemotePathInfoRequest {
    pub device_id: DeviceId,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteGlobRequest {
    pub device_id: DeviceId,
    pub root: String,
    pub pattern: String,
    #[serde(default)]
    pub cursor: u64,
    pub limit: u32,
    pub max_bytes: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteGrepRequest {
    pub device_id: DeviceId,
    pub root: String,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<String>,
    #[serde(default)]
    pub cursor: u64,
    pub limit: u32,
    pub max_bytes: u32,
    pub max_scan_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteWriteRequest {
    pub device_id: DeviceId,
    pub path: String,
    pub contents: String,
    pub precondition: FsWritePrecondition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteEditRequest {
    pub device_id: DeviceId,
    pub path: String,
    pub expected_sha256: String,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActivityList {
    pub events: Vec<ActivityEvent>,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackupExportRequest {
    pub path: String,
    pub passphrase: String,
}

impl std::fmt::Debug for BackupExportRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackupExportRequest")
            .field("path", &self.path)
            .field("passphrase", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryStatus {
    pub review: Option<RecoveryReview>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitList {
    pub entries: Vec<OutfitLibraryEntry>,
    pub default_outfit_id: String,
    pub recent_outfit_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitCreateRequest {
    pub outfit_id: String,
    pub display_name: String,
    pub preset: OutfitPreset,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitCloneRequest {
    pub source_id: String,
    pub outfit_id: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitSetFieldRequest {
    pub outfit_id: String,
    pub field: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitUpdateRequest {
    pub outfit_id: String,
    pub patch: OutfitEditPatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitAssetImportRequest {
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitSetAssetRequest {
    pub outfit_id: String,
    pub slot: String,
    pub asset_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitAssetList {
    pub assets: Vec<OutfitAssetInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitAssetDataResponse {
    pub info: OutfitAssetInfo,
    pub data_base64: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutfitAssetPreviewResponse {
    pub asset_id: String,
    pub width: u32,
    pub height: u32,
    pub rgba_base64: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalApiErrorCode {
    Unauthorized,
    UnsupportedVersion,
    InvalidRequest,
    Denied,
    Unavailable,
    Internal,
}

#[derive(Clone)]
pub(crate) struct LocalApiSecret(String);

impl std::fmt::Debug for LocalApiSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalApiSecret([REDACTED])")
    }
}

impl LocalApiSecret {
    pub(crate) fn rotate(layout: &StateLayout) -> Result<Self, std::io::Error> {
        let mut raw = [0_u8; SECRET_BYTES];
        getrandom::fill(&mut raw).map_err(|error| {
            std::io::Error::other(format!("secure random generation failed: {error}"))
        })?;
        let encoded = encode_hex(&raw);
        write_secret_file(&layout.local_api_secret_path(), encoded.as_bytes())?;
        Ok(Self(encoded))
    }

    fn load(layout: &StateLayout) -> Result<Self, std::io::Error> {
        let path = layout.local_api_secret_path();
        let metadata = fs::metadata(&path)?;
        if metadata.len() != SECRET_HEX_LEN as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "local API credential has invalid length",
            ));
        }
        let mut file = fs::File::open(path)?;
        let mut encoded = String::with_capacity(SECRET_HEX_LEN);
        file.read_to_string(&mut encoded)?;
        if encoded.len() != SECRET_HEX_LEN || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "local API credential is malformed",
            ));
        }
        Ok(Self(encoded))
    }

    fn matches(&self, candidate: &str) -> bool {
        if self.0.len() != candidate.len() {
            return false;
        }
        let mut difference = 0_u8;
        for (expected, actual) in self.0.bytes().zip(candidate.bytes()) {
            difference |= expected ^ actual;
        }
        difference == 0
    }

    fn expose_for_request(&self) -> &str {
        &self.0
    }
}

fn write_secret_file(path: &Path, secret: &[u8]) -> Result<(), std::io::Error> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(secret)?;
    file.sync_data()?;
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct LocalApiState {
    pub status: ControllerStatus,
    pub controller_identity: StoredControllerIdentity,
    pub controller_outer: Option<IrohOuter>,
    pub control: Arc<Mutex<ControllerControlStore>>,
    pub outfits: Arc<Mutex<OutfitLibrary>>,
    pub outfit_assets: Arc<Mutex<OutfitAssetStore>>,
    pub remote: RemoteHub,
    pub shutdown_tx: watch::Sender<bool>,
}

#[derive(Serialize, Deserialize)]
struct LocalRequest {
    api_version: u32,
    auth: String,
    method: LocalMethod,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum LocalMethod {
    ControllerStatus,
    DeviceList,
    InviteIssue(InviteIssueRequest),
    InviteClose {
        invite_id: InviteId,
    },
    DeviceRename {
        device_id: DeviceId,
        display_name: String,
    },
    DeviceRevoke {
        device_id: DeviceId,
    },
    Read(RemoteReadRequest),
    PathInfo(RemotePathInfoRequest),
    Glob(RemoteGlobRequest),
    Grep(RemoteGrepRequest),
    Write(RemoteWriteRequest),
    Edit(RemoteEditRequest),
    ActivityList {
        limit: u32,
    },
    ActivityClear,
    OutfitList,
    OutfitShow {
        outfit_id: String,
    },
    OutfitCreate(OutfitCreateRequest),
    OutfitClone(OutfitCloneRequest),
    OutfitSetDefault {
        outfit_id: String,
    },
    OutfitSetField(OutfitSetFieldRequest),
    OutfitUpdate(OutfitUpdateRequest),
    OutfitAssetList,
    OutfitAssetImport(OutfitAssetImportRequest),
    OutfitAssetGet {
        asset_id: String,
    },
    OutfitAssetPreview {
        asset_id: String,
        max_edge: u32,
    },
    OutfitSetAsset(OutfitSetAssetRequest),
    BackupExport(BackupExportRequest),
    RecoveryStatus,
    RecoveryConfirm,
    ControllerShutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum LocalResponse {
    ControllerStatus(ControllerStatus),
    DeviceList(DeviceList),
    InviteIssued(InviteIssueResult),
    DeviceRenamed(DeviceRecord),
    ReadResult(RemoteReadResult),
    ReadError(clew_transport::ReadErrorBody),
    PathInfo(FsPathInfo),
    Glob(FsGlobPage),
    Grep(FsGrepPage),
    FsQueryError(FsQueryErrorBody),
    FsMutationResult(FsMutationResult),
    FsMutationError(FsMutationErrorBody),
    ActivityList(ActivityList),
    OutfitList(OutfitList),
    OutfitProfile(OutfitProfile),
    OutfitAssetList(OutfitAssetList),
    OutfitAssetInfo(OutfitAssetInfo),
    OutfitAssetData(OutfitAssetDataResponse),
    OutfitAssetPreview(OutfitAssetPreviewResponse),
    RecoveryStatus(RecoveryStatus),
    Ack,
    Error(LocalApiErrorBody),
}

#[derive(Debug, Serialize, Deserialize)]
struct LocalApiErrorBody {
    code: LocalApiErrorCode,
    message: String,
}

pub(crate) async fn serve_connection(
    mut stream: transport::LocalStream,
    secret: LocalApiSecret,
    state: LocalApiState,
) {
    let (response, shutdown_after_reply) =
        match timeout(LOCAL_API_IO_TIMEOUT, read_frame(&mut stream)).await {
            Ok(Ok(frame)) => match serde_json::from_slice::<LocalRequest>(&frame) {
                Ok(request) => dispatch(request, &secret, &state).await,
                Err(_) => (
                    LocalResponse::Error(LocalApiErrorBody {
                        code: LocalApiErrorCode::InvalidRequest,
                        message: "invalid local API request".into(),
                    }),
                    false,
                ),
            },
            Ok(Err(_)) | Err(_) => return,
        };

    if let Ok(encoded) = serde_json::to_vec(&response) {
        if timeout(LOCAL_API_IO_TIMEOUT, write_frame(&mut stream, &encoded))
            .await
            .is_ok()
        {
            let _ = stream.shutdown().await;
            if shutdown_after_reply {
                let _ = state.shutdown_tx.send(true);
            }
        }
    }
}

async fn dispatch(
    request: LocalRequest,
    secret: &LocalApiSecret,
    state: &LocalApiState,
) -> (LocalResponse, bool) {
    if !secret.matches(&request.auth) {
        return (
            LocalResponse::Error(LocalApiErrorBody {
                code: LocalApiErrorCode::Unauthorized,
                message: "local API authentication failed".into(),
            }),
            false,
        );
    }
    if request.api_version != LOCAL_API_VERSION {
        return (
            LocalResponse::Error(LocalApiErrorBody {
                code: LocalApiErrorCode::UnsupportedVersion,
                message: format!(
                    "unsupported local API version {}; this build supports {}",
                    request.api_version, LOCAL_API_VERSION
                ),
            }),
            false,
        );
    }

    match request.method {
        LocalMethod::ControllerStatus => {
            (LocalResponse::ControllerStatus(state.status.clone()), false)
        }
        LocalMethod::DeviceList => (device_list_response(state), false),
        LocalMethod::InviteIssue(request) => (issue_invite_response(state, request).await, false),
        LocalMethod::InviteClose { invite_id } => {
            let response = with_control(state, |store| {
                store.transaction(|snapshot| {
                    snapshot.registry.close_invite(invite_id);
                    Ok(())
                })
            })
            .map(|()| LocalResponse::Ack)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::DeviceRename {
            device_id,
            display_name,
        } => {
            let response = with_control(state, |store| {
                store.transaction(|snapshot| {
                    Ok(snapshot.catalog.rename_device(device_id, &display_name)?)
                })
            })
            .map(LocalResponse::DeviceRenamed)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::DeviceRevoke { device_id } => {
            let response = with_control(state, |store| {
                store.transaction(|snapshot| {
                    snapshot.registry.revoke_device(device_id)?;
                    snapshot.catalog.revoke_device(device_id)?;
                    Ok(())
                })
            })
            .map(|()| {
                state.remote.disconnect(device_id);
                LocalResponse::Ack
            })
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::Read(request) => (read_response(state, request).await, false),
        LocalMethod::PathInfo(request) => (path_info_response(state, request).await, false),
        LocalMethod::Glob(request) => (glob_response(state, request).await, false),
        LocalMethod::Grep(request) => (grep_response(state, request).await, false),
        LocalMethod::Write(request) => (write_response(state, request).await, false),
        LocalMethod::Edit(request) => (edit_response(state, request).await, false),
        LocalMethod::ActivityList { limit } => {
            if limit == 0 || limit > MAX_ACTIVITY_LIST_LIMIT {
                return (
                    local_error(
                        LocalApiErrorCode::InvalidRequest,
                        format!("activity limit must be within 1..={MAX_ACTIVITY_LIST_LIMIT}"),
                    ),
                    false,
                );
            }
            let response = with_control(state, |store| {
                let events = &store.snapshot().activity;
                let start = events.len().saturating_sub(limit as usize);
                Ok(ActivityList {
                    events: events[start..].to_vec(),
                })
            })
            .map(LocalResponse::ActivityList)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::ActivityClear => {
            let response = with_control(state, ControllerControlStore::clear_activity)
                .map(|()| LocalResponse::Ack)
                .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitList => {
            let response = with_outfits(state, |store| {
                Ok(OutfitList {
                    entries: store.list(),
                    default_outfit_id: store.snapshot().default_outfit_id.clone(),
                    recent_outfit_id: store.snapshot().recent_outfit_id.clone(),
                })
            })
            .map(LocalResponse::OutfitList)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitShow { outfit_id } => {
            let response = with_outfits(state, |store| store.get(&outfit_id))
                .map(LocalResponse::OutfitProfile)
                .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitCreate(request) => {
            let response = with_outfits(state, |store| {
                store.create_from_preset(request.outfit_id, request.display_name, request.preset)
            })
            .map(LocalResponse::OutfitProfile)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitClone(request) => {
            let response = with_outfits(state, |store| {
                store.clone_outfit(&request.source_id, request.outfit_id, request.display_name)
            })
            .map(LocalResponse::OutfitProfile)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitSetDefault { outfit_id } => {
            let response = with_outfits(state, |store| store.set_default(&outfit_id))
                .map(|()| LocalResponse::Ack)
                .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitSetField(request) => {
            let response = with_outfits(state, |store| {
                store.set_field(&request.outfit_id, &request.field, request.value)
            })
            .map(LocalResponse::OutfitProfile)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitUpdate(request) => {
            let response = with_outfits(state, |store| {
                store.update_editable(&request.outfit_id, request.patch)
            })
            .map(LocalResponse::OutfitProfile)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitAssetList => {
            let response = with_outfit_assets(state, |store| store.list())
                .map(|assets| LocalResponse::OutfitAssetList(OutfitAssetList { assets }))
                .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitAssetImport(request) => {
            let response = outfit_asset_import_response(state, request);
            (response, false)
        }
        LocalMethod::OutfitAssetGet { asset_id } => {
            let response = with_outfit_assets(state, |store| store.read(&asset_id))
                .map(|asset| {
                    LocalResponse::OutfitAssetData(OutfitAssetDataResponse {
                        info: asset.info,
                        data_base64: BASE64_STANDARD.encode(asset.bytes),
                    })
                })
                .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitAssetPreview { asset_id, max_edge } => {
            let response =
                with_outfit_assets(state, |store| store.render_preview(&asset_id, max_edge))
                    .map(|preview| {
                        LocalResponse::OutfitAssetPreview(OutfitAssetPreviewResponse {
                            asset_id: preview.asset_id,
                            width: preview.width,
                            height: preview.height,
                            rgba_base64: BASE64_STANDARD.encode(preview.rgba),
                        })
                    })
                    .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::OutfitSetAsset(request) => {
            let response = match with_outfit_assets(state, |store| store.read(&request.asset_id)) {
                Ok(_) => with_outfits(state, |store| {
                    store.set_asset(
                        &request.outfit_id,
                        &request.slot,
                        OutfitAssetRef::Imported {
                            asset_id: request.asset_id,
                        },
                    )
                })
                .map(LocalResponse::OutfitProfile)
                .unwrap_or_else(LocalResponse::Error),
                Err(error) => LocalResponse::Error(error),
            };
            (response, false)
        }
        LocalMethod::BackupExport(request) => (backup_export_response(state, request).await, false),
        LocalMethod::RecoveryStatus => {
            let response = with_control(state, |store| {
                Ok(RecoveryStatus {
                    review: store.recovery_review(),
                })
            })
            .map(LocalResponse::RecoveryStatus)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::RecoveryConfirm => {
            let response = with_control(state, |store| {
                Ok(RecoveryStatus {
                    review: store.confirm_recovery_review()?,
                })
            })
            .map(LocalResponse::RecoveryStatus)
            .unwrap_or_else(LocalResponse::Error);
            (response, false)
        }
        LocalMethod::ControllerShutdown => (LocalResponse::Ack, true),
    }
}

fn device_list_response(state: &LocalApiState) -> LocalResponse {
    let store = match state.control.lock() {
        Ok(store) => store,
        Err(_) => {
            return local_error(
                LocalApiErrorCode::Internal,
                "controller state is unavailable",
            );
        }
    };
    let mut devices = Vec::with_capacity(store.snapshot().catalog.devices.len());
    for record in store.snapshot().catalog.devices.values() {
        let Some(site) = store.snapshot().catalog.site(record.device.site_id) else {
            continue;
        };
        let allowed = !record.revoked && !site.revoked;
        let mut summary = DeviceSummary::from_record(
            &record.device,
            site.site_name.clone(),
            allowed && state.remote.is_online(record.device.device_id),
        );
        if !allowed {
            summary.executable = false;
            summary.connector = false;
        }
        devices.push(summary);
    }
    devices.sort_by(|left, right| {
        left.site_name
            .cmp(&right.site_name)
            .then_with(|| left.display_name.cmp(&right.display_name))
            .then_with(|| left.device_id.to_string().cmp(&right.device_id.to_string()))
    });
    LocalResponse::DeviceList(DeviceList { devices })
}

async fn issue_invite_response(
    state: &LocalApiState,
    request: InviteIssueRequest,
) -> LocalResponse {
    let read_policy = match ReadPolicy::new(
        request.roots,
        request.max_result_bytes,
        request.read_timeout_ms,
    ) {
        Ok(policy) => policy,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    if request.valid_for_ms == 0 {
        return local_error(
            LocalApiErrorCode::InvalidRequest,
            "invite validity must be greater than zero",
        );
    }
    let now = match current_unix_ms() {
        Ok(now) => now,
        Err(error) => return error,
    };
    let Some(expires_unix_ms) = now.checked_add(request.valid_for_ms) else {
        return local_error(LocalApiErrorCode::InvalidRequest, "invite expiry overflows");
    };
    let site_id = SiteId::new();
    let invite_id = InviteId::new();
    let pass = match state
        .controller_identity
        .identity()
        .issue_site_bootstrap(SiteBootstrapSpec {
            site_id,
            invite_id,
            site_name: request.site_name.clone(),
            grant: if request.allow_write {
                PermissionGrant::EXECUTE_READ_WRITE_CONNECTOR
            } else {
                PermissionGrant::EXECUTE_READ_CONNECTOR
            },
            not_before_unix_ms: now,
            expires_unix_ms,
            deployment_window_ms: request.deployment_window_ms,
            max_claims: request.max_claims,
        }) {
        Ok(pass) => pass,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    let controller_endpoint = match &state.controller_outer {
        Some(outer) => match outer.online_addr().await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                return local_error(
                    LocalApiErrorCode::Unavailable,
                    format!("Controller remote endpoint is not online: {error}"),
                );
            }
        },
        None => {
            return local_error(
                LocalApiErrorCode::Unavailable,
                "Controller remote endpoint is unavailable",
            );
        }
    };
    let outfit_profile = match with_outfits(state, |store| {
        let outfit_id = request
            .outfit_id
            .clone()
            .unwrap_or_else(|| store.snapshot().default_outfit_id.clone());
        let profile = store.get(&outfit_id)?;
        store.mark_recent(&outfit_id)?;
        Ok(profile)
    }) {
        Ok(profile) => profile,
        Err(error) => return LocalResponse::Error(error),
    };
    let controller_bootstrap_noise_public_key =
        match noise_static_public(state.controller_identity.bootstrap_noise_static_secret()) {
            Ok(key) => key,
            Err(error) => {
                return local_error(
                    LocalApiErrorCode::Internal,
                    format!("Controller bootstrap key derivation failed: {error}"),
                );
            }
        };
    let site_file = match SignedSiteClew::issue_networked_outfit_sealed(
        state.controller_identity.identity(),
        outfit_profile,
        pass.clone(),
        HostRoleHint::ExecutePreferred,
        controller_endpoint,
        read_policy.clone(),
        controller_bootstrap_noise_public_key,
    ) {
        Ok(site_file) => site_file,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    let committed = with_control(state, |store| {
        store.transaction(|snapshot| {
            snapshot.catalog.upsert_site(ControllerSiteRecord {
                site_id,
                site_name: request.site_name,
                read_policy,
                revoked: false,
            })?;
            snapshot.registry.register_pass(&pass)?;
            Ok(())
        })
    });
    match committed {
        Ok(()) => LocalResponse::InviteIssued(InviteIssueResult { site_file }),
        Err(error) => LocalResponse::Error(error),
    }
}

async fn backup_export_response(
    state: &LocalApiState,
    request: BackupExportRequest,
) -> LocalResponse {
    if request.path.is_empty() || request.path.len() > MAX_BACKUP_PATH_BYTES {
        return local_error(
            LocalApiErrorCode::InvalidRequest,
            format!("backup path must be 1..={MAX_BACKUP_PATH_BYTES} UTF-8 bytes"),
        );
    }
    let created_unix_ms = match current_unix_ms() {
        Ok(value) => value,
        Err(error) => return error,
    };
    let snapshot = match state.control.lock() {
        Ok(store) => store.snapshot().clone(),
        Err(_) => {
            return local_error(
                LocalApiErrorCode::Internal,
                "controller state is unavailable",
            );
        }
    };
    let identity = state.controller_identity.clone();
    let path = std::path::PathBuf::from(request.path);
    let passphrase = request.passphrase;
    match tokio::task::spawn_blocking(move || {
        export_controller_backup(&path, &passphrase, &identity, &snapshot, created_unix_ms)
    })
    .await
    {
        Ok(Ok(())) => LocalResponse::Ack,
        Ok(Err(error)) => local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
        Err(_) => local_error(LocalApiErrorCode::Internal, "backup export worker failed"),
    }
}

async fn path_info_response(
    state: &LocalApiState,
    request: RemotePathInfoRequest,
) -> LocalResponse {
    let wire_request = match FsQueryRequest::path_info(&request.path) {
        Ok(request) => request,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    fs_query_response(
        state,
        request.device_id,
        wire_request,
        "path_info",
        request.path,
        None,
    )
    .await
}

async fn glob_response(state: &LocalApiState, request: RemoteGlobRequest) -> LocalResponse {
    let wire_request = match FsQueryRequest::glob(
        &request.root,
        &request.pattern,
        request.cursor,
        request.limit,
        request.max_bytes,
    ) {
        Ok(request) => request,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    fs_query_response(
        state,
        request.device_id,
        wire_request,
        "glob",
        request.root,
        Some(request.max_bytes),
    )
    .await
}

async fn grep_response(state: &LocalApiState, request: RemoteGrepRequest) -> LocalResponse {
    let wire_request = match FsQueryRequest::grep(
        &request.root,
        &request.pattern,
        request.include,
        request.cursor,
        request.limit,
        request.max_bytes,
        request.max_scan_bytes,
    ) {
        Ok(request) => request,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    fs_query_response(
        state,
        request.device_id,
        wire_request,
        "grep",
        request.root,
        Some(request.max_bytes),
    )
    .await
}

async fn write_response(state: &LocalApiState, request: RemoteWriteRequest) -> LocalResponse {
    let wire_request =
        match FsMutationRequest::write(&request.path, request.contents, request.precondition) {
            Ok(request) => request,
            Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
        };
    fs_mutation_response(
        state,
        request.device_id,
        wire_request,
        "write",
        request.path,
    )
    .await
}

async fn edit_response(state: &LocalApiState, request: RemoteEditRequest) -> LocalResponse {
    let wire_request = match FsMutationRequest::edit(
        &request.path,
        request.expected_sha256,
        request.old,
        request.new,
    ) {
        Ok(request) => request,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    fs_mutation_response(state, request.device_id, wire_request, "edit", request.path).await
}

async fn fs_mutation_response(
    state: &LocalApiState,
    device_id: DeviceId,
    wire_request: FsMutationRequest,
    operation: &'static str,
    path_summary: String,
) -> LocalResponse {
    let (site_id, timeout_ms) = match state.control.lock() {
        Ok(store) => {
            let Some(device) = store.snapshot().catalog.device(device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(site) = store.snapshot().catalog.site(device.device.site_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(enrollment) = store.snapshot().registry.device(device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            if device.revoked
                || site.revoked
                || !device.device.capabilities.execute
                || !enrollment.effective_grant.write
                || !site.read_policy.allows_read()
            {
                return local_error(
                    LocalApiErrorCode::Denied,
                    "filesystem mutation is not permitted",
                );
            }
            (site.site_id, site.read_policy.timeout_ms)
        }
        Err(_) => {
            return local_error(
                LocalApiErrorCode::Internal,
                "controller state is unavailable",
            );
        }
    };
    let started = Instant::now();
    let remote_timeout = Duration::from_millis(u64::from(timeout_ms).saturating_add(2_000));
    let remote_result = timeout(
        remote_timeout,
        state.remote.fs_mutation(device_id, wire_request),
    )
    .await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let (response, activity_result, transferred_bytes) = match remote_result {
        Err(_) => (
            local_error(
                LocalApiErrorCode::Unavailable,
                "remote filesystem mutation timed out",
            ),
            ActivityResult::TimedOut,
            0,
        ),
        Ok(Err(_)) => (
            local_error(
                LocalApiErrorCode::Unavailable,
                "device is offline or reconnecting",
            ),
            ActivityResult::Failed,
            0,
        ),
        Ok(Ok(FsMutationReply::Result(result))) => {
            let bytes = result.size;
            (
                LocalResponse::FsMutationResult(result),
                ActivityResult::Succeeded,
                bytes,
            )
        }
        Ok(Ok(FsMutationReply::Error(error))) => {
            let result = match error.code {
                FsMutationErrorCode::Denied => ActivityResult::Denied,
                FsMutationErrorCode::Timeout => ActivityResult::TimedOut,
                _ => ActivityResult::Failed,
            };
            (LocalResponse::FsMutationError(error), result, 0)
        }
    };
    if let Ok(now) = unix_ms_value()
        && let Ok(mut store) = state.control.lock()
    {
        let _ = store.record_activity(
            now,
            site_id,
            device_id,
            operation,
            Some(path_summary),
            activity_result,
            duration_ms,
            transferred_bytes,
        );
    }
    response
}

async fn fs_query_response(
    state: &LocalApiState,
    device_id: DeviceId,
    wire_request: FsQueryRequest,
    operation: &'static str,
    path_summary: String,
    requested_max_bytes: Option<u32>,
) -> LocalResponse {
    let (site_id, policy) = match state.control.lock() {
        Ok(store) => {
            let Some(device) = store.snapshot().catalog.device(device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(site) = store.snapshot().catalog.site(device.device.site_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(enrollment) = store.snapshot().registry.device(device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            if device.revoked
                || site.revoked
                || !device.device.capabilities.execute
                || !enrollment.effective_grant.read
                || !site.read_policy.allows_read()
                || requested_max_bytes
                    .is_some_and(|max_bytes| max_bytes > site.read_policy.max_result_bytes)
            {
                return local_error(
                    LocalApiErrorCode::Denied,
                    "read-only filesystem query is not permitted",
                );
            }
            (site.site_id, site.read_policy.clone())
        }
        Err(_) => {
            return local_error(
                LocalApiErrorCode::Internal,
                "controller state is unavailable",
            );
        }
    };
    let expected_kind = match &wire_request {
        FsQueryRequest::PathInfo { .. } => "path_info",
        FsQueryRequest::Glob { .. } => "glob",
        FsQueryRequest::Grep { .. } => "grep",
    };
    let started = Instant::now();
    let remote_timeout = Duration::from_millis(u64::from(policy.timeout_ms).saturating_add(2_000));
    let remote_result = timeout(
        remote_timeout,
        state.remote.fs_query(device_id, wire_request),
    )
    .await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let (response, activity_result, transferred_bytes) = match remote_result {
        Err(_) => (
            local_error(
                LocalApiErrorCode::Unavailable,
                "remote filesystem query timed out",
            ),
            ActivityResult::TimedOut,
            0,
        ),
        Ok(Err(_)) => (
            local_error(
                LocalApiErrorCode::Unavailable,
                "device is offline or reconnecting",
            ),
            ActivityResult::Failed,
            0,
        ),
        Ok(Ok(FsQueryReply::PathInfo(info))) if expected_kind == "path_info" => {
            let bytes = serde_json::to_vec(&info).map_or(0, |encoded| encoded.len() as u64);
            (
                LocalResponse::PathInfo(info),
                ActivityResult::Succeeded,
                bytes,
            )
        }
        Ok(Ok(FsQueryReply::Glob(page))) if expected_kind == "glob" => {
            match serde_json::to_vec(&page) {
                Ok(encoded)
                    if requested_max_bytes
                        .is_some_and(|max_bytes| encoded.len() > max_bytes as usize) =>
                {
                    (
                        local_error(
                            LocalApiErrorCode::Internal,
                            "device returned a filesystem result larger than requested",
                        ),
                        ActivityResult::Failed,
                        0,
                    )
                }
                Ok(encoded) => (
                    LocalResponse::Glob(page),
                    ActivityResult::Succeeded,
                    encoded.len() as u64,
                ),
                Err(_) => (
                    local_error(
                        LocalApiErrorCode::Internal,
                        "device returned an invalid filesystem result",
                    ),
                    ActivityResult::Failed,
                    0,
                ),
            }
        }
        Ok(Ok(FsQueryReply::Grep(page))) if expected_kind == "grep" => {
            match serde_json::to_vec(&page) {
                Ok(encoded)
                    if requested_max_bytes
                        .is_some_and(|max_bytes| encoded.len() > max_bytes as usize) =>
                {
                    (
                        local_error(
                            LocalApiErrorCode::Internal,
                            "device returned a filesystem result larger than requested",
                        ),
                        ActivityResult::Failed,
                        0,
                    )
                }
                Ok(encoded) => (
                    LocalResponse::Grep(page),
                    ActivityResult::Succeeded,
                    encoded.len() as u64,
                ),
                Err(_) => (
                    local_error(
                        LocalApiErrorCode::Internal,
                        "device returned an invalid filesystem result",
                    ),
                    ActivityResult::Failed,
                    0,
                ),
            }
        }
        Ok(Ok(FsQueryReply::Error(error))) => {
            let result = match error.code {
                FsQueryErrorCode::Denied => ActivityResult::Denied,
                FsQueryErrorCode::Timeout => ActivityResult::TimedOut,
                _ => ActivityResult::Failed,
            };
            (LocalResponse::FsQueryError(error), result, 0)
        }
        Ok(Ok(_)) => (
            local_error(
                LocalApiErrorCode::Internal,
                "device returned the wrong filesystem query result kind",
            ),
            ActivityResult::Failed,
            0,
        ),
    };
    if let Ok(now) = unix_ms_value()
        && let Ok(mut store) = state.control.lock()
    {
        let _ = store.record_activity(
            now,
            site_id,
            device_id,
            operation,
            Some(path_summary),
            activity_result,
            duration_ms,
            transferred_bytes,
        );
    }
    response
}

async fn read_response(state: &LocalApiState, request: RemoteReadRequest) -> LocalResponse {
    let (site_id, policy) = match state.control.lock() {
        Ok(store) => {
            let Some(device) = store.snapshot().catalog.device(request.device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(site) = store.snapshot().catalog.site(device.device.site_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(enrollment) = store.snapshot().registry.device(request.device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            if device.revoked
                || site.revoked
                || !device.device.capabilities.execute
                || !enrollment.effective_grant.read
                || !site.read_policy.allows_read()
                || request.limit > site.read_policy.max_result_bytes
            {
                return local_error(LocalApiErrorCode::Denied, "read is not permitted");
            }
            (site.site_id, site.read_policy.clone())
        }
        Err(_) => {
            return local_error(
                LocalApiErrorCode::Internal,
                "controller state is unavailable",
            );
        }
    };

    let wire_request = match ReadRequest::new(&request.path, request.offset, request.limit) {
        Ok(request) => request,
        Err(error) => return local_error(LocalApiErrorCode::InvalidRequest, error.to_string()),
    };
    let started = Instant::now();
    let remote_timeout = Duration::from_millis(u64::from(policy.timeout_ms).saturating_add(2_000));
    let remote_result = timeout(
        remote_timeout,
        state.remote.read(request.device_id, wire_request),
    )
    .await;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let (response, activity_result, transferred_bytes) = match remote_result {
        Err(_) => (
            local_error(LocalApiErrorCode::Unavailable, "remote read timed out"),
            ActivityResult::TimedOut,
            0,
        ),
        Ok(Err(_)) => (
            local_error(
                LocalApiErrorCode::Unavailable,
                "device is offline or reconnecting",
            ),
            ActivityResult::Failed,
            0,
        ),
        Ok(Ok(ReadReply::Data(data))) => {
            let bytes = data.len() as u64;
            (
                LocalResponse::ReadResult(RemoteReadResult { data }),
                ActivityResult::Succeeded,
                bytes,
            )
        }
        Ok(Ok(ReadReply::Error(error))) => {
            let result = match error.code {
                ReadErrorCode::Denied => ActivityResult::Denied,
                ReadErrorCode::Timeout => ActivityResult::TimedOut,
                _ => ActivityResult::Failed,
            };
            (LocalResponse::ReadError(error), result, 0)
        }
    };
    if let Ok(now) = unix_ms_value()
        && let Ok(mut store) = state.control.lock()
    {
        let _ = store.record_activity(
            now,
            site_id,
            request.device_id,
            "read",
            Some(request.path),
            activity_result,
            duration_ms,
            transferred_bytes,
        );
    }
    response
}

fn with_control<R>(
    state: &LocalApiState,
    operation: impl FnOnce(&mut ControllerControlStore) -> Result<R, crate::ControlStoreError>,
) -> Result<R, LocalApiErrorBody> {
    let mut store = state.control.lock().map_err(|_| LocalApiErrorBody {
        code: LocalApiErrorCode::Internal,
        message: "controller state is unavailable".into(),
    })?;
    operation(&mut store).map_err(|error| LocalApiErrorBody {
        code: LocalApiErrorCode::InvalidRequest,
        message: error.to_string(),
    })
}

fn outfit_asset_import_response(
    state: &LocalApiState,
    request: OutfitAssetImportRequest,
) -> LocalResponse {
    let path = request.path.trim();
    if path.is_empty() || path.len() > MAX_ASSET_IMPORT_PATH_BYTES {
        return local_error(
            LocalApiErrorCode::InvalidRequest,
            "asset import path is empty or too long",
        );
    }
    with_outfit_assets(state, |store| store.import_path(Path::new(path)))
        .map(LocalResponse::OutfitAssetInfo)
        .unwrap_or_else(LocalResponse::Error)
}

fn with_outfit_assets<R>(
    state: &LocalApiState,
    operation: impl FnOnce(&OutfitAssetStore) -> Result<R, crate::OutfitAssetError>,
) -> Result<R, LocalApiErrorBody> {
    let store = state.outfit_assets.lock().map_err(|_| LocalApiErrorBody {
        code: LocalApiErrorCode::Internal,
        message: "outfit asset store is unavailable".into(),
    })?;
    operation(&store).map_err(|error| LocalApiErrorBody {
        code: LocalApiErrorCode::InvalidRequest,
        message: error.to_string(),
    })
}

fn with_outfits<R>(
    state: &LocalApiState,
    operation: impl FnOnce(&mut OutfitLibrary) -> Result<R, crate::OutfitStoreError>,
) -> Result<R, LocalApiErrorBody> {
    let mut store = state.outfits.lock().map_err(|_| LocalApiErrorBody {
        code: LocalApiErrorCode::Internal,
        message: "outfit library is unavailable".into(),
    })?;
    operation(&mut store).map_err(|error| LocalApiErrorBody {
        code: LocalApiErrorCode::InvalidRequest,
        message: error.to_string(),
    })
}

fn current_unix_ms() -> Result<u64, LocalResponse> {
    unix_ms_value().map_err(|_| {
        local_error(
            LocalApiErrorCode::Internal,
            "system clock is before the Unix epoch or out of range",
        )
    })
}

fn unix_ms_value() -> Result<u64, ()> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis()
        .try_into()
        .map_err(|_| ())
}

fn local_error(code: LocalApiErrorCode, message: impl Into<String>) -> LocalResponse {
    LocalResponse::Error(LocalApiErrorBody {
        code,
        message: message.into(),
    })
}

#[derive(Clone, Debug)]
pub struct LocalApiClient {
    config: ControllerConfig,
}

impl LocalApiClient {
    #[must_use]
    pub fn new(config: ControllerConfig) -> Self {
        Self { config }
    }

    pub async fn controller_status(&self) -> Result<ControllerStatus, LocalApiClientError> {
        match self.request(LocalMethod::ControllerStatus).await? {
            LocalResponse::ControllerStatus(status) => Ok(status),
            LocalResponse::Error(error) => Err(LocalApiClientError::Remote {
                code: error.code,
                message: error.message,
            }),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn device_list(&self) -> Result<DeviceList, LocalApiClientError> {
        match self.request(LocalMethod::DeviceList).await? {
            LocalResponse::DeviceList(devices) => Ok(devices),
            LocalResponse::Error(error) => Err(LocalApiClientError::Remote {
                code: error.code,
                message: error.message,
            }),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn invite_issue(
        &self,
        request: InviteIssueRequest,
    ) -> Result<InviteIssueResult, LocalApiClientError> {
        match self.request(LocalMethod::InviteIssue(request)).await? {
            LocalResponse::InviteIssued(result) => Ok(result),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn invite_close(&self, invite_id: InviteId) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::InviteClose { invite_id })
            .await
    }

    pub async fn device_rename(
        &self,
        device_id: DeviceId,
        display_name: String,
    ) -> Result<DeviceRecord, LocalApiClientError> {
        match self
            .request(LocalMethod::DeviceRename {
                device_id,
                display_name,
            })
            .await?
        {
            LocalResponse::DeviceRenamed(device) => Ok(device),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn device_revoke(&self, device_id: DeviceId) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::DeviceRevoke { device_id })
            .await
    }

    pub async fn read(
        &self,
        request: RemoteReadRequest,
    ) -> Result<RemoteReadResult, LocalApiClientError> {
        match self.request(LocalMethod::Read(request)).await? {
            LocalResponse::ReadResult(result) => Ok(result),
            LocalResponse::ReadError(error) => Err(LocalApiClientError::ReadRemote {
                code: error.code,
                message: error.message,
            }),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn path_info(
        &self,
        request: RemotePathInfoRequest,
    ) -> Result<FsPathInfo, LocalApiClientError> {
        match self.request(LocalMethod::PathInfo(request)).await? {
            LocalResponse::PathInfo(info) => Ok(info),
            LocalResponse::FsQueryError(error) => Err(LocalApiClientError::FsQueryRemote {
                code: error.code,
                message: error.message,
            }),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn glob(
        &self,
        request: RemoteGlobRequest,
    ) -> Result<FsGlobPage, LocalApiClientError> {
        match self.request(LocalMethod::Glob(request)).await? {
            LocalResponse::Glob(page) => Ok(page),
            LocalResponse::FsQueryError(error) => Err(LocalApiClientError::FsQueryRemote {
                code: error.code,
                message: error.message,
            }),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn grep(
        &self,
        request: RemoteGrepRequest,
    ) -> Result<FsGrepPage, LocalApiClientError> {
        match self.request(LocalMethod::Grep(request)).await? {
            LocalResponse::Grep(page) => Ok(page),
            LocalResponse::FsQueryError(error) => Err(LocalApiClientError::FsQueryRemote {
                code: error.code,
                message: error.message,
            }),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn write(
        &self,
        request: RemoteWriteRequest,
    ) -> Result<FsMutationResult, LocalApiClientError> {
        self.fs_mutation(LocalMethod::Write(request)).await
    }

    pub async fn edit(
        &self,
        request: RemoteEditRequest,
    ) -> Result<FsMutationResult, LocalApiClientError> {
        self.fs_mutation(LocalMethod::Edit(request)).await
    }

    async fn fs_mutation(
        &self,
        method: LocalMethod,
    ) -> Result<FsMutationResult, LocalApiClientError> {
        match self.request(method).await? {
            LocalResponse::FsMutationResult(result) => Ok(result),
            LocalResponse::FsMutationError(error) => Err(LocalApiClientError::FsMutationRemote {
                code: error.code,
                message: error.message,
            }),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn activity_list(&self, limit: u32) -> Result<ActivityList, LocalApiClientError> {
        match self.request(LocalMethod::ActivityList { limit }).await? {
            LocalResponse::ActivityList(result) => Ok(result),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn activity_clear(&self) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::ActivityClear).await
    }

    pub async fn outfit_list(&self) -> Result<OutfitList, LocalApiClientError> {
        match self.request(LocalMethod::OutfitList).await? {
            LocalResponse::OutfitList(result) => Ok(result),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_show(
        &self,
        outfit_id: String,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitShow { outfit_id }).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_create(
        &self,
        request: OutfitCreateRequest,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitCreate(request)).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_clone(
        &self,
        request: OutfitCloneRequest,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitClone(request)).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_set_default(&self, outfit_id: String) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::OutfitSetDefault { outfit_id })
            .await
    }

    pub async fn outfit_set_field(
        &self,
        request: OutfitSetFieldRequest,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitSetField(request)).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_update(
        &self,
        request: OutfitUpdateRequest,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitUpdate(request)).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_asset_list(&self) -> Result<OutfitAssetList, LocalApiClientError> {
        match self.request(LocalMethod::OutfitAssetList).await? {
            LocalResponse::OutfitAssetList(result) => Ok(result),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_asset_import(
        &self,
        request: OutfitAssetImportRequest,
    ) -> Result<OutfitAssetInfo, LocalApiClientError> {
        match self
            .request(LocalMethod::OutfitAssetImport(request))
            .await?
        {
            LocalResponse::OutfitAssetInfo(info) => Ok(info),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_asset_get(
        &self,
        asset_id: String,
    ) -> Result<OutfitAssetDataResponse, LocalApiClientError> {
        match self
            .request(LocalMethod::OutfitAssetGet { asset_id })
            .await?
        {
            LocalResponse::OutfitAssetData(data) => Ok(data),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_asset_preview(
        &self,
        asset_id: String,
        max_edge: u32,
    ) -> Result<OutfitAssetPreviewResponse, LocalApiClientError> {
        match self
            .request(LocalMethod::OutfitAssetPreview { asset_id, max_edge })
            .await?
        {
            LocalResponse::OutfitAssetPreview(preview) => Ok(preview),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn outfit_set_asset(
        &self,
        request: OutfitSetAssetRequest,
    ) -> Result<OutfitProfile, LocalApiClientError> {
        match self.request(LocalMethod::OutfitSetAsset(request)).await? {
            LocalResponse::OutfitProfile(profile) => Ok(profile),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn backup_export(
        &self,
        request: BackupExportRequest,
    ) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::BackupExport(request)).await
    }

    pub async fn recovery_status(&self) -> Result<RecoveryStatus, LocalApiClientError> {
        match self.request(LocalMethod::RecoveryStatus).await? {
            LocalResponse::RecoveryStatus(status) => Ok(status),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn recovery_confirm(&self) -> Result<RecoveryStatus, LocalApiClientError> {
        match self.request(LocalMethod::RecoveryConfirm).await? {
            LocalResponse::RecoveryStatus(status) => Ok(status),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    pub async fn controller_shutdown(&self) -> Result<(), LocalApiClientError> {
        self.expect_ack(LocalMethod::ControllerShutdown).await
    }

    async fn expect_ack(&self, method: LocalMethod) -> Result<(), LocalApiClientError> {
        match self.request(method).await? {
            LocalResponse::Ack => Ok(()),
            LocalResponse::Error(error) => Err(remote_error(error)),
            _ => Err(LocalApiClientError::UnexpectedResponse),
        }
    }

    async fn request(&self, method: LocalMethod) -> Result<LocalResponse, LocalApiClientError> {
        let secret = load_secret_with_retry(&self.config.state_layout()).await?;
        let endpoint: LocalEndpoint = self.config.local_endpoint();
        let mut stream = transport::connect(&endpoint).await?;
        let request = LocalRequest {
            api_version: LOCAL_API_VERSION,
            auth: secret.expose_for_request().to_owned(),
            method,
        };
        let encoded = serde_json::to_vec(&request)?;
        timeout(LOCAL_API_IO_TIMEOUT, write_frame(&mut stream, &encoded))
            .await
            .map_err(|_| LocalApiClientError::TimedOut)??;
        let response = timeout(LOCAL_API_RESPONSE_TIMEOUT, read_frame(&mut stream))
            .await
            .map_err(|_| LocalApiClientError::TimedOut)??;
        Ok(serde_json::from_slice(&response)?)
    }
}

fn remote_error(error: LocalApiErrorBody) -> LocalApiClientError {
    LocalApiClientError::Remote {
        code: error.code,
        message: error.message,
    }
}

async fn load_secret_with_retry(layout: &StateLayout) -> Result<LocalApiSecret, std::io::Error> {
    let deadline = Instant::now() + SECRET_LOAD_RETRY_WINDOW;
    loop {
        match LocalApiSecret::load(layout) {
            Ok(secret) => return Ok(secret),
            Err(error) if Instant::now() < deadline => {
                sleep(SECRET_LOAD_RETRY_DELAY).await;
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn read_frame<S>(stream: &mut S) -> Result<Vec<u8>, FrameError>
where
    S: AsyncRead + Unpin,
{
    let length = stream.read_u32().await? as usize;
    if length > MAX_LOCAL_API_FRAME_SIZE {
        return Err(FrameError::TooLarge {
            actual: length,
            max: MAX_LOCAL_API_FRAME_SIZE,
        });
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame<S>(stream: &mut S, payload: &[u8]) -> Result<(), FrameError>
where
    S: AsyncWrite + Unpin,
{
    if payload.len() > MAX_LOCAL_API_FRAME_SIZE {
        return Err(FrameError::TooLarge {
            actual: payload.len(),
            max: MAX_LOCAL_API_FRAME_SIZE,
        });
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

#[derive(Debug, Error)]
enum FrameError {
    #[error("local API I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("local API frame is {actual} bytes; maximum is {max}")]
    TooLarge { actual: usize, max: usize },
}

#[derive(Debug, Error)]
pub enum LocalApiClientError {
    #[error("local API I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("local API JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("local API framing failed: {0}")]
    Frame(String),
    #[error("controller rejected local API request ({code:?}): {message}")]
    Remote {
        code: LocalApiErrorCode,
        message: String,
    },
    #[error("remote read failed ({code:?}): {message}")]
    ReadRemote {
        code: ReadErrorCode,
        message: String,
    },
    #[error("remote filesystem query failed ({code:?}): {message}")]
    FsQueryRemote {
        code: FsQueryErrorCode,
        message: String,
    },
    #[error("remote filesystem mutation failed ({code:?}): {message}")]
    FsMutationRemote {
        code: FsMutationErrorCode,
        message: String,
    },
    #[error("local API request timed out")]
    TimedOut,
    #[error("controller returned a response for a different local API method")]
    UnexpectedResponse,
}

impl From<FrameError> for LocalApiClientError {
    fn from(error: FrameError) -> Self {
        Self::Frame(error.to_string())
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use clew_identity::ControllerIdentityStore;
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn test_state() -> LocalApiState {
        let temp = tempfile::tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let controller_identity = ControllerIdentityStore::new(layout.clone())
            .load_or_create()
            .unwrap();
        let controller_id = controller_identity.identity().controller_id();
        LocalApiState {
            status: ControllerStatus {
                ready: true,
                controller_id,
                pid: 42,
                instance_id: "instance".into(),
                started_unix_ms: 1,
                state_schema_version: clew_core::STATE_SCHEMA_VERSION,
                local_api_version: LOCAL_API_VERSION,
                remote_endpoint_id: None,
            },
            controller_identity,
            controller_outer: None,
            control: Arc::new(Mutex::new(
                ControllerControlStore::load_or_create(layout.clone(), controller_id).unwrap(),
            )),
            outfits: Arc::new(Mutex::new(
                OutfitLibrary::load_or_create(layout.clone()).unwrap(),
            )),
            outfit_assets: Arc::new(Mutex::new(
                OutfitAssetStore::load_or_create(layout).unwrap(),
            )),
            remote: RemoteHub::default(),
            shutdown_tx: watch::channel(false).0,
        }
    }

    #[tokio::test]
    async fn auth_is_required_before_method_dispatch() {
        let (response, shutdown_after_reply) = dispatch(
            LocalRequest {
                api_version: LOCAL_API_VERSION,
                auth: "wrong".into(),
                method: LocalMethod::ControllerStatus,
            },
            &LocalApiSecret("a".repeat(SECRET_HEX_LEN)),
            &test_state(),
        )
        .await;
        assert!(!shutdown_after_reply);
        assert!(matches!(
            response,
            LocalResponse::Error(LocalApiErrorBody {
                code: LocalApiErrorCode::Unauthorized,
                ..
            })
        ));
    }

    #[test]
    fn maximum_asset_data_response_stays_within_local_api_frame_bound() {
        let bytes = vec![0_u8; crate::MAX_OUTFIT_ASSET_BYTES];
        let response = LocalResponse::OutfitAssetData(OutfitAssetDataResponse {
            info: OutfitAssetInfo {
                asset_id: format!("sha256-{}", "a".repeat(64)),
                format: crate::OutfitAssetFormat::Png,
                byte_len: u32::try_from(bytes.len()).unwrap(),
                width: 2048,
                height: 2048,
            },
            data_base64: BASE64_STANDARD.encode(bytes),
        });
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= MAX_LOCAL_API_FRAME_SIZE);
    }

    #[test]
    fn maximum_asset_preview_response_stays_within_local_api_frame_bound() {
        let rgba = vec![0_u8; (crate::MAX_OUTFIT_PREVIEW_EDGE as usize).pow(2) * 4];
        let response = LocalResponse::OutfitAssetPreview(OutfitAssetPreviewResponse {
            asset_id: format!("sha256-{}", "b".repeat(64)),
            width: crate::MAX_OUTFIT_PREVIEW_EDGE,
            height: crate::MAX_OUTFIT_PREVIEW_EDGE,
            rgba_base64: BASE64_STANDARD.encode(rgba),
        });
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() <= MAX_LOCAL_API_FRAME_SIZE);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_payload_allocation() {
        let (mut writer, mut reader) = duplex(16);
        let task = tokio::spawn(async move {
            writer
                .write_u32((MAX_LOCAL_API_FRAME_SIZE + 1) as u32)
                .await
                .unwrap();
        });
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(FrameError::TooLarge {
                actual,
                max: MAX_LOCAL_API_FRAME_SIZE
            }) if actual == MAX_LOCAL_API_FRAME_SIZE + 1
        ));
        task.await.unwrap();
    }
}
