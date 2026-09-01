use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clew_core::{
    ActivityEvent, ActivityResult, ControllerId, ControllerSiteRecord, DeviceId, DeviceRecord,
    DeviceSummary, InviteId, ReadPolicy, SiteId, StateLayout,
};
use clew_host::{ClientFlavor, HostRoleHint, SignedSiteClew};
use clew_identity::{PermissionGrant, RecoveryReview, SiteBootstrapSpec, StoredControllerIdentity};
use clew_transport::{IrohOuter, ReadErrorCode, ReadReply, ReadRequest};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::{Instant, sleep, timeout},
};

use crate::{
    ControllerConfig, ControllerControlStore, LocalEndpoint, RemoteHub, export_controller_backup,
    transport,
};

pub const LOCAL_API_VERSION: u32 = 1;
pub const MAX_LOCAL_API_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_LOCAL_API_CONNECTIONS: usize = 16;
const LOCAL_API_IO_TIMEOUT: Duration = Duration::from_secs(2);
const LOCAL_API_RESPONSE_TIMEOUT: Duration = Duration::from_secs(40);
const MAX_ACTIVITY_LIST_LIMIT: u32 = 200;
const MAX_BACKUP_PATH_BYTES: usize = 4096;
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
    pub roots: Vec<String>,
    pub max_claims: u32,
    pub valid_for_ms: u64,
    pub deployment_window_ms: u64,
    pub max_result_bytes: u32,
    pub read_timeout_ms: u32,
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
    ActivityList {
        limit: u32,
    },
    ActivityClear,
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
    ActivityList(ActivityList),
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
            grant: PermissionGrant::EXECUTE_READ,
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
    let site_file = match SignedSiteClew::issue_networked(
        state.controller_identity.identity(),
        ClientFlavor::clew_original_current(),
        pass.clone(),
        HostRoleHint::ExecutePreferred,
        controller_endpoint,
        read_policy.clone(),
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

async fn read_response(state: &LocalApiState, request: RemoteReadRequest) -> LocalResponse {
    let (site_id, policy) = match state.control.lock() {
        Ok(store) => {
            let Some(device) = store.snapshot().catalog.device(request.device_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            let Some(site) = store.snapshot().catalog.site(device.device.site_id) else {
                return local_error(LocalApiErrorCode::Denied, "device is not available");
            };
            if device.revoked
                || site.revoked
                || !device.device.capabilities.execute
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
                ControllerControlStore::load_or_create(layout, controller_id).unwrap(),
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
