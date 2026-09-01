use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
};

use clew_core::{ControllerId, DeviceSummary, StateLayout};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::{Instant, sleep, timeout},
};

use crate::{ControllerConfig, LocalEndpoint, transport};

pub const LOCAL_API_VERSION: u32 = 1;
pub const MAX_LOCAL_API_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_LOCAL_API_CONNECTIONS: usize = 16;
const LOCAL_API_IO_TIMEOUT: Duration = Duration::from_secs(2);
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalApiErrorCode {
    Unauthorized,
    UnsupportedVersion,
    InvalidRequest,
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
    pub devices: Vec<DeviceSummary>,
    pub shutdown_tx: watch::Sender<bool>,
}

#[derive(Serialize, Deserialize)]
struct LocalRequest {
    api_version: u32,
    auth: String,
    method: LocalMethod,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LocalMethod {
    ControllerStatus,
    DeviceList,
    ControllerShutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
enum LocalResponse {
    ControllerStatus(ControllerStatus),
    DeviceList(DeviceList),
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
                Ok(request) => dispatch(request, &secret, &state),
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

fn dispatch(
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
        LocalMethod::DeviceList => (
            LocalResponse::DeviceList(DeviceList {
                devices: state.devices.clone(),
            }),
            false,
        ),
        LocalMethod::ControllerShutdown => (LocalResponse::Ack, true),
    }
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

    pub async fn controller_shutdown(&self) -> Result<(), LocalApiClientError> {
        match self.request(LocalMethod::ControllerShutdown).await? {
            LocalResponse::Ack => Ok(()),
            LocalResponse::Error(error) => Err(LocalApiClientError::Remote {
                code: error.code,
                message: error.message,
            }),
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
        let response = timeout(LOCAL_API_IO_TIMEOUT, read_frame(&mut stream))
            .await
            .map_err(|_| LocalApiClientError::TimedOut)??;
        Ok(serde_json::from_slice(&response)?)
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
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    fn test_state() -> LocalApiState {
        LocalApiState {
            status: ControllerStatus {
                ready: true,
                controller_id: ControllerId::new(),
                pid: 42,
                instance_id: "instance".into(),
                started_unix_ms: 1,
                state_schema_version: clew_core::STATE_SCHEMA_VERSION,
                local_api_version: LOCAL_API_VERSION,
            },
            devices: Vec::new(),
            shutdown_tx: watch::channel(false).0,
        }
    }

    #[test]
    fn auth_is_required_before_method_dispatch() {
        let (response, shutdown_after_reply) = dispatch(
            LocalRequest {
                api_version: LOCAL_API_VERSION,
                auth: "wrong".into(),
                method: LocalMethod::ControllerStatus,
            },
            &LocalApiSecret("a".repeat(SECRET_HEX_LEN)),
            &test_state(),
        );
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
