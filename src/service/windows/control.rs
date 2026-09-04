use std::{io, os::windows::io::AsRawHandle, sync::Arc, time::Duration};

use clew_host::HostLaunchState;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    sync::{RwLock, watch},
    task::JoinSet,
    time::{Instant, sleep, timeout},
};
use windows_sys::Win32::{Foundation::HANDLE, System::Pipes::GetNamedPipeServerProcessId};

use super::security::PipeSecurityAttributes;

pub const MACHINE_CONTROL_PIPE: &str = r"\\.\pipe\clew-machine-control-v1";
const API_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024;
const PIPE_MAX_INSTANCES: usize = 8;
const MAX_ACTIVE_CONNECTIONS: usize = PIPE_MAX_INSTANCES - 1;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_WINDOW: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineRuntimePhase {
    Starting,
    AwaitingEnrollment,
    ServingConnector,
    Stopping,
}

impl MachineRuntimePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::AwaitingEnrollment => "awaiting_enrollment",
            Self::ServingConnector => "serving_connector",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MachineRuntimeStatus {
    pub phase: MachineRuntimePhase,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub executable: bool,
    pub connector: bool,
}

impl MachineRuntimeStatus {
    pub fn starting() -> Self {
        Self {
            phase: MachineRuntimePhase::Starting,
            role: "connector_only".into(),
            site_name: None,
            device_id: None,
            executable: false,
            connector: true,
        }
    }

    pub fn from_host_state(phase: MachineRuntimePhase, state: &HostLaunchState) -> Self {
        Self {
            phase,
            role: "connector_only".into(),
            site_name: state.site_name().map(str::to_owned),
            device_id: state.device_id().map(|id| id.to_string()),
            executable: false,
            connector: true,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlRequest {
    api_version: u32,
    method: ControlMethod,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlMethod {
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
struct ControlResponse {
    api_version: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<MachineRuntimeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct MachineControlListener {
    name: &'static str,
    authorized_user_sid: String,
    service_sid: String,
    next: NamedPipeServer,
}

impl MachineControlListener {
    fn bind(authorized_user_sid: &str, service_sid: &str) -> io::Result<Self> {
        let next = make_server(MACHINE_CONTROL_PIPE, authorized_user_sid, service_sid, true)?;
        Ok(Self {
            name: MACHINE_CONTROL_PIPE,
            authorized_user_sid: authorized_user_sid.to_owned(),
            service_sid: service_sid.to_owned(),
            next,
        })
    }

    async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        self.next.connect().await?;
        let replacement = make_server(
            self.name,
            &self.authorized_user_sid,
            &self.service_sid,
            false,
        )?;
        Ok(std::mem::replace(&mut self.next, replacement))
    }
}

fn make_server(
    name: &str,
    authorized_user_sid: &str,
    service_sid: &str,
    first: bool,
) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .reject_remote_clients(true)
        .max_instances(PIPE_MAX_INSTANCES);
    if first {
        options.first_pipe_instance(true);
    }
    let mut security = PipeSecurityAttributes::new(authorized_user_sid, service_sid)?;
    unsafe { options.create_with_security_attributes_raw(name, security.as_mut_ptr()) }
}

pub struct MachineControlServer {
    listener: MachineControlListener,
}

impl MachineControlServer {
    pub fn bind(authorized_user_sid: &str, service_sid: &str) -> io::Result<Self> {
        Ok(Self {
            listener: MachineControlListener::bind(authorized_user_sid, service_sid)?,
        })
    }

    pub async fn serve(
        mut self,
        status: Arc<RwLock<MachineRuntimeStatus>>,
        mut shutdown: watch::Receiver<bool>,
    ) -> io::Result<()> {
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        handlers.abort_all();
                        while handlers.join_next().await.is_some() {}
                        return Ok(());
                    }
                }
                accepted = self.listener.accept(), if handlers.len() < MAX_ACTIVE_CONNECTIONS => {
                    let mut stream = match accepted {
                        Ok(stream) => stream,
                        Err(error) => {
                            handlers.abort_all();
                            while handlers.join_next().await.is_some() {}
                            return Err(error);
                        }
                    };
                    let status = Arc::clone(&status);
                    handlers.spawn(async move {
                        let _ = handle_connection(&mut stream, status).await;
                    });
                }
                _ = handlers.join_next(), if !handlers.is_empty() => {}
            }
        }
    }
}

async fn handle_connection(
    stream: &mut NamedPipeServer,
    status: Arc<RwLock<MachineRuntimeStatus>>,
) -> io::Result<()> {
    let payload = timeout(IO_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "machine control read timed out"))??;
    let request: ControlRequest = serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let response = if request.api_version != API_VERSION {
        ControlResponse {
            api_version: API_VERSION,
            ok: false,
            status: None,
            error: Some("unsupported machine control API version".into()),
        }
    } else {
        match request.method {
            ControlMethod::Status => ControlResponse {
                api_version: API_VERSION,
                ok: true,
                status: Some(status.read().await.clone()),
                error: None,
            },
        }
    };
    let encoded = serde_json::to_vec(&response)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    timeout(IO_TIMEOUT, write_frame(stream, &encoded))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "machine control write timed out")
        })??;
    let _ = stream.shutdown().await;
    Ok(())
}

pub async fn query_status(expected_service_pid: u32) -> io::Result<MachineRuntimeStatus> {
    if expected_service_pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Windows service is not running",
        ));
    }
    let mut stream = connect().await?;
    verify_server_pid(&stream, expected_service_pid)?;
    let encoded = serde_json::to_vec(&ControlRequest {
        api_version: API_VERSION,
        method: ControlMethod::Status,
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    timeout(IO_TIMEOUT, write_frame(&mut stream, &encoded))
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "machine control write timed out")
        })??;
    let payload = timeout(IO_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "machine control read timed out"))??;
    let response: ControlResponse = serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if response.api_version != API_VERSION || !response.ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            response
                .error
                .unwrap_or_else(|| "machine control response was rejected".into()),
        ));
    }
    response.status.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "machine control response omitted runtime status",
        )
    })
}

async fn connect() -> io::Result<NamedPipeClient> {
    let deadline = Instant::now() + CONNECT_WINDOW;
    loop {
        match ClientOptions::new().open(MACHINE_CONTROL_PIPE) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                sleep(CONNECT_RETRY).await;
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn verify_server_pid(stream: &NamedPipeClient, expected_service_pid: u32) -> io::Result<()> {
    let mut server_pid = 0_u32;
    let handle = stream.as_raw_handle() as HANDLE;
    if unsafe { GetNamedPipeServerProcessId(handle, &mut server_pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if server_pid != expected_service_pid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "machine control pipe server PID {server_pid} does not match SCM service PID {expected_service_pid}"
            ),
        ));
    }
    Ok(())
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "machine control frame exceeds the bounded size",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "machine control frame exceeds the bounded size",
        ));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_status_never_claims_execute_authority() {
        let status = MachineRuntimeStatus::starting();
        assert!(!status.executable);
        assert!(status.connector);
        assert_eq!(status.role, "connector_only");
    }

    #[tokio::test]
    async fn frame_bounds_reject_oversized_payload() {
        let payload = vec![0_u8; MAX_FRAME_BYTES + 1];
        let mut sink = tokio::io::sink();
        assert_eq!(
            write_frame(&mut sink, &payload).await.unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
