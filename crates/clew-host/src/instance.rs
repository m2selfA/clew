use std::{
    fs::{self, File, OpenOptions, TryLockError},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use clew_core::{ControllerId, SiteId, StateLayout};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    time::{Instant, sleep, timeout},
};

use crate::ClientFlavorId;

const INSTANCE_KEY_DOMAIN: &[u8] = b"clew/host-instance-key/v1\0";
#[cfg(windows)]
const PIPE_NAME_DOMAIN: &[u8] = b"clew/host-instance-pipe/v1\0";
#[cfg(unix)]
const UNIX_SOCKET_DOMAIN: &[u8] = b"clew/host-instance-socket/v1\0";
#[cfg(all(unix, not(target_os = "macos")))]
const UNIX_SOCKET_SAFE_BYTES: usize = 96;
const MAX_WAKE_FRAME: usize = 4 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_WINDOW: Duration = Duration::from_secs(5);
const RETRY_DELAY: Duration = Duration::from_millis(25);
const SECRET_BYTES: usize = 32;
const SECRET_HEX_LEN: usize = SECRET_BYTES * 2;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct HostInstanceKey([u8; 32]);

impl HostInstanceKey {
    #[must_use]
    pub fn membership(controller_id: ControllerId, site_id: SiteId) -> Self {
        derive_key(&[
            b"membership\0".as_slice(),
            controller_id.as_bytes(),
            site_id.as_bytes(),
        ])
    }

    #[must_use]
    pub fn missing_invite(flavor_id: ClientFlavorId) -> Self {
        derive_key(&[b"missing\0".as_slice(), flavor_id.as_bytes()])
    }

    #[must_use]
    pub fn path_component(&self) -> String {
        hex(&self.0[..16])
    }
}

impl std::fmt::Debug for HostInstanceKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.path_component())
    }
}

fn derive_key(parts: &[&[u8]]) -> HostInstanceKey {
    let mut hasher = Sha256::new();
    hasher.update(INSTANCE_KEY_DOMAIN);
    for part in parts {
        hasher.update(part);
    }
    HostInstanceKey(hasher.finalize().into())
}

pub enum HostInstanceStart {
    Primary(HostInstance),
    ExistingWoken,
}

pub struct HostInstance {
    runtime_dir: PathBuf,
    listener: LocalListener,
    secret: String,
    // Ownership must be released last, after IPC is gone.
    _ownership: HostOwnership,
}

impl HostInstance {
    pub async fn serve_until<F>(
        mut self,
        shutdown: F,
        wake_tx: Option<mpsc::UnboundedSender<()>>,
    ) -> Result<(), HostInstanceError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => return Ok(()),
                accepted = self.listener.accept() => {
                    let mut stream = accepted?;
                    if handle_wake(&mut stream, &self.secret).await? {
                        if let Some(tx) = &wake_tx {
                            let _ = tx.send(());
                        }
                    }
                }
            }
        }
    }
}

impl Drop for HostInstance {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.runtime_dir.join("runtime.secret"));
    }
}

pub async fn acquire_host_instance(
    layout: &StateLayout,
    key: HostInstanceKey,
) -> Result<HostInstanceStart, HostInstanceError> {
    let runtime_dir = layout
        .version_root()
        .join("host-runtime")
        .join(key.path_component());
    fs::create_dir_all(&runtime_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))?;
    }
    match HostOwnership::try_acquire(&runtime_dir)? {
        OwnershipAttempt::Busy => {
            wake_existing(layout, key, &runtime_dir).await?;
            Ok(HostInstanceStart::ExistingWoken)
        }
        OwnershipAttempt::Acquired(ownership) => {
            let secret = rotate_secret(&runtime_dir.join("runtime.secret"))?;
            let endpoint = endpoint(layout, key, &runtime_dir);
            let listener = LocalListener::bind(&endpoint)?;
            Ok(HostInstanceStart::Primary(HostInstance {
                runtime_dir,
                listener,
                secret,
                _ownership: ownership,
            }))
        }
    }
}

enum OwnershipAttempt {
    Acquired(HostOwnership),
    Busy,
}

struct HostOwnership {
    _file: File,
}

impl HostOwnership {
    fn try_acquire(runtime_dir: &Path) -> Result<OwnershipAttempt, std::io::Error> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(runtime_dir.join("runtime.lock"))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(OwnershipAttempt::Busy),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "pid={}", std::process::id())?;
        file.sync_data()?;
        Ok(OwnershipAttempt::Acquired(Self { _file: file }))
    }
}

#[derive(Clone, Debug)]
enum LocalEndpoint {
    #[cfg(windows)]
    WindowsNamedPipe(String),
    #[cfg(unix)]
    UnixSocket(PathBuf),
}

fn endpoint(layout: &StateLayout, key: HostInstanceKey, _runtime_dir: &Path) -> LocalEndpoint {
    #[cfg(windows)]
    {
        let mut hasher = Sha256::new();
        hasher.update(PIPE_NAME_DOMAIN);
        hasher.update(layout.root().to_string_lossy().to_lowercase().as_bytes());
        hasher.update(key.0);
        let digest = hasher.finalize();
        LocalEndpoint::WindowsNamedPipe(format!(r"\\.\pipe\clew-host-{}", hex(&digest[..8])))
    }
    #[cfg(target_os = "macos")]
    {
        LocalEndpoint::UnixSocket(short_unix_socket_path(layout, key))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use std::os::unix::ffi::OsStrExt;

        let candidate = _runtime_dir.join("wake.sock");
        if candidate.as_os_str().as_bytes().len() < UNIX_SOCKET_SAFE_BYTES {
            LocalEndpoint::UnixSocket(candidate)
        } else {
            LocalEndpoint::UnixSocket(short_unix_socket_path(layout, key))
        }
    }
}

#[cfg(unix)]
fn short_unix_socket_path(layout: &StateLayout, key: HostInstanceKey) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(UNIX_SOCKET_DOMAIN);
    hasher.update(layout.root().to_string_lossy().as_bytes());
    hasher.update(key.0);
    let digest = hasher.finalize();
    std::env::temp_dir().join(format!("clew-host-{}.sock", hex(&digest[..8])))
}

#[cfg(windows)]
mod platform {
    use std::io;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    use super::LocalEndpoint;

    pub(super) type LocalStream = NamedPipeServer;
    pub(super) type LocalClientStream = NamedPipeClient;

    pub(super) struct LocalListener {
        name: String,
        next: NamedPipeServer,
    }

    impl LocalListener {
        pub(super) fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
            let LocalEndpoint::WindowsNamedPipe(name) = endpoint;
            let next = make_server(name, true)?;
            Ok(Self {
                name: name.clone(),
                next,
            })
        }

        pub(super) async fn accept(&mut self) -> io::Result<LocalStream> {
            self.next.connect().await?;
            let replacement = make_server(&self.name, false)?;
            Ok(std::mem::replace(&mut self.next, replacement))
        }
    }

    fn make_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options.reject_remote_clients(true).max_instances(4);
        if first {
            options.first_pipe_instance(true);
        }
        options.create(name)
    }

    pub(super) async fn try_connect(endpoint: &LocalEndpoint) -> io::Result<LocalClientStream> {
        let LocalEndpoint::WindowsNamedPipe(name) = endpoint;
        ClientOptions::new().open(name)
    }
}

#[cfg(unix)]
mod platform {
    use std::{fs, io};

    use tokio::net::{UnixListener, UnixStream};

    use super::LocalEndpoint;

    pub(super) type LocalStream = UnixStream;
    pub(super) type LocalClientStream = UnixStream;

    pub(super) struct LocalListener {
        listener: UnixListener,
        path: PathBuf,
    }

    use std::path::PathBuf;

    impl LocalListener {
        pub(super) fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
            use std::os::unix::fs::PermissionsExt;
            let LocalEndpoint::UnixSocket(path) = endpoint;
            if path.exists() {
                fs::remove_file(path)?;
            }
            let listener = UnixListener::bind(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(Self {
                listener,
                path: path.clone(),
            })
        }

        pub(super) async fn accept(&mut self) -> io::Result<LocalStream> {
            self.listener.accept().await.map(|(stream, _)| stream)
        }
    }

    impl Drop for LocalListener {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    pub(super) async fn try_connect(endpoint: &LocalEndpoint) -> io::Result<LocalClientStream> {
        let LocalEndpoint::UnixSocket(path) = endpoint;
        UnixStream::connect(path).await
    }
}

use platform::{LocalClientStream, LocalListener, LocalStream};

#[derive(Serialize, Deserialize)]
struct WakeRequest {
    auth: String,
    action: String,
}

async fn handle_wake(stream: &mut LocalStream, secret: &str) -> Result<bool, HostInstanceError> {
    let frame = match timeout(IO_TIMEOUT, read_frame(stream)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Ok(false),
    };
    let request: WakeRequest = serde_json::from_slice(&frame)?;
    if request.action != "wake" || !constant_time_eq(secret, &request.auth) {
        return Ok(false);
    }
    let _ = stream.shutdown().await;
    Ok(true)
}

async fn wake_existing(
    layout: &StateLayout,
    key: HostInstanceKey,
    runtime_dir: &Path,
) -> Result<(), HostInstanceError> {
    let secret_path = runtime_dir.join("runtime.secret");
    let secret = load_secret_with_retry(&secret_path).await?;
    let endpoint = endpoint(layout, key, runtime_dir);
    let mut stream = connect_with_retry(&endpoint).await?;
    let encoded = serde_json::to_vec(&WakeRequest {
        auth: secret,
        action: "wake".into(),
    })?;
    timeout(IO_TIMEOUT, write_frame(&mut stream, &encoded))
        .await
        .map_err(|_| HostInstanceError::Timeout)??;
    Ok(())
}

async fn connect_with_retry(endpoint: &LocalEndpoint) -> Result<LocalClientStream, std::io::Error> {
    let deadline = Instant::now() + RETRY_WINDOW;
    loop {
        match platform::try_connect(endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                sleep(RETRY_DELAY).await;
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn load_secret_with_retry(path: &Path) -> Result<String, std::io::Error> {
    let deadline = Instant::now() + RETRY_WINDOW;
    loop {
        match load_secret(path) {
            Ok(secret) => return Ok(secret),
            Err(error) if Instant::now() < deadline => {
                sleep(RETRY_DELAY).await;
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn rotate_secret(path: &Path) -> Result<String, std::io::Error> {
    let mut raw = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut raw)
        .map_err(|error| std::io::Error::other(format!("secure random failed: {error}")))?;
    let secret = hex(&raw);
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
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
    file.write_all(secret.as_bytes())?;
    file.sync_data()?;
    Ok(secret)
}

fn load_secret(path: &Path) -> Result<String, std::io::Error> {
    let metadata = fs::metadata(path)?;
    if metadata.len() != SECRET_HEX_LEN as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "host runtime secret has invalid length",
        ));
    }
    let mut secret = String::with_capacity(SECRET_HEX_LEN);
    File::open(path)?.read_to_string(&mut secret)?;
    if secret.len() != SECRET_HEX_LEN || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "host runtime secret is malformed",
        ));
    }
    Ok(secret)
}

async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, HostInstanceError> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_WAKE_FRAME {
        return Err(HostInstanceError::FrameTooLarge(length));
    }
    let mut frame = vec![0_u8; length];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

async fn write_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    payload: &[u8],
) -> Result<(), HostInstanceError> {
    if payload.len() > MAX_WAKE_FRAME {
        return Err(HostInstanceError::FrameTooLarge(payload.len()));
    }
    stream.write_u32(payload.len() as u32).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

fn constant_time_eq(expected: &str, actual: &str) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in expected.bytes().zip(actual.bytes()) {
        difference |= left ^ right;
    }
    difference == 0
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(TABLE[(byte >> 4) as usize] as char);
        output.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Error)]
pub enum HostInstanceError {
    #[error("host single-instance I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("host wake JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("host wake frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("host wake I/O timed out")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_wake_socket_stays_short_for_deep_state_root() {
        let deep_root = std::env::temp_dir()
            .join("clew-deep-state")
            .join("nested-state-directory-that-would-overflow-sun-path-because-it-is-deliberately-very-long")
            .join("another-deliberately-long-segment");
        let layout = StateLayout::new(deep_root);
        let key = HostInstanceKey::membership(ControllerId::new(), SiteId::new());
        let LocalEndpoint::UnixSocket(path) = endpoint(
            &layout,
            key,
            &layout
                .version_root()
                .join("host-runtime")
                .join(key.path_component()),
        );
        assert!(path.starts_with(std::env::temp_dir()));
        assert!(path.to_string_lossy().len() < 100);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn unix_short_wake_socket_stays_inside_private_runtime_dir() {
        let layout = StateLayout::new("/tmp/c");
        let key = HostInstanceKey::membership(ControllerId::new(), SiteId::new());
        let runtime_dir = layout
            .version_root()
            .join("host-runtime")
            .join(key.path_component());
        let LocalEndpoint::UnixSocket(path) = endpoint(&layout, key, &runtime_dir);
        assert_eq!(path, runtime_dir.join("wake.sock"));
    }

    #[tokio::test]
    async fn second_instance_wakes_existing_and_owner_releases_cleanly() {
        let temp = tempdir().unwrap();
        let layout = StateLayout::new(temp.path());
        let key = HostInstanceKey::membership(ControllerId::new(), SiteId::new());
        let first = match acquire_host_instance(&layout, key).await.unwrap() {
            HostInstanceStart::Primary(instance) => instance,
            HostInstanceStart::ExistingWoken => panic!("unexpected existing host"),
        };
        let (wake_tx, mut wake_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(first.serve_until(
            async move {
                let _ = shutdown_rx.await;
            },
            Some(wake_tx),
        ));
        assert!(matches!(
            acquire_host_instance(&layout, key).await.unwrap(),
            HostInstanceStart::ExistingWoken
        ));
        timeout(Duration::from_secs(2), wake_rx.recv())
            .await
            .unwrap()
            .unwrap();
        shutdown_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert!(matches!(
            acquire_host_instance(&layout, key).await.unwrap(),
            HostInstanceStart::Primary(_)
        ));
    }
}
