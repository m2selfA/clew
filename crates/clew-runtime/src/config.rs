use std::{env, fs, path::PathBuf};

use clew_core::StateLayout;
use serde::{Deserialize, Serialize};
#[cfg(any(windows, unix))]
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(windows)]
const PIPE_NAME_DOMAIN: &[u8] = b"clew/local-api-pipe/v1\0";
#[cfg(unix)]
const UNIX_SOCKET_DOMAIN: &[u8] = b"clew/local-api-socket/v1\0";
#[cfg(all(unix, not(target_os = "macos")))]
const UNIX_SOCKET_SAFE_BYTES: usize = 96;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerLifecycleOwner {
    #[default]
    Foreground,
    SystemdUser,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerConfig {
    state_root: PathBuf,
    lifecycle_owner: ControllerLifecycleOwner,
}

impl ControllerConfig {
    #[must_use]
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
            lifecycle_owner: ControllerLifecycleOwner::Foreground,
        }
    }

    pub fn for_current_user() -> Result<Self, ControllerConfigError> {
        Ok(Self::new(default_state_root()?))
    }

    #[must_use]
    pub fn state_root(&self) -> &std::path::Path {
        &self.state_root
    }

    #[must_use]
    pub fn lifecycle_owner(&self) -> ControllerLifecycleOwner {
        self.lifecycle_owner
    }

    #[must_use]
    pub fn with_lifecycle_owner(mut self, lifecycle_owner: ControllerLifecycleOwner) -> Self {
        self.lifecycle_owner = lifecycle_owner;
        self
    }

    #[must_use]
    pub fn state_layout(&self) -> StateLayout {
        StateLayout::new(self.state_root.clone())
    }

    pub(crate) fn prepare_state_dir(&self) -> Result<(), std::io::Error> {
        let version_root = self.state_layout().version_root();
        fs::create_dir_all(&version_root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&version_root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn local_endpoint(&self) -> LocalEndpoint {
        #[cfg(windows)]
        {
            let normalized = windows_endpoint_state_key(&self.state_root);
            let mut hasher = Sha256::new();
            hasher.update(PIPE_NAME_DOMAIN);
            hasher.update(normalized.as_bytes());
            let digest = hasher.finalize();
            let suffix = hex_prefix(&digest[..8]);
            LocalEndpoint::WindowsNamedPipe(format!(r"\\.\pipe\clew-controller-{suffix}"))
        }
        #[cfg(target_os = "macos")]
        {
            LocalEndpoint::UnixSocket(short_unix_controller_socket(&self.state_root))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::os::unix::ffi::OsStrExt;

            let candidate = self.state_layout().local_api_socket_path();
            if candidate.as_os_str().as_bytes().len() < UNIX_SOCKET_SAFE_BYTES {
                LocalEndpoint::UnixSocket(candidate)
            } else {
                LocalEndpoint::UnixSocket(short_unix_controller_socket(&self.state_root))
            }
        }
    }
}

#[cfg(windows)]
fn windows_endpoint_state_key(state_root: &std::path::Path) -> String {
    let absolute = if state_root.is_absolute() {
        state_root.to_path_buf()
    } else {
        env::current_dir()
            .map(|current| current.join(state_root))
            .unwrap_or_else(|_| state_root.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let _ = normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase()
}

#[cfg(unix)]
fn short_unix_controller_socket(state_root: &std::path::Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(UNIX_SOCKET_DOMAIN);
    hasher.update(state_root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let suffix = hex_prefix(&digest[..8]);
    env::temp_dir().join(format!("clew-controller-{suffix}.sock"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalEndpoint {
    #[cfg(windows)]
    WindowsNamedPipe(String),
    #[cfg(unix)]
    UnixSocket(PathBuf),
}

pub fn default_state_root() -> Result<PathBuf, ControllerConfigError> {
    #[cfg(windows)]
    {
        let root = env::var_os("LOCALAPPDATA")
            .ok_or(ControllerConfigError::MissingEnvironment("LOCALAPPDATA"))?;
        return Ok(PathBuf::from(root).join("Clew"));
    }

    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").ok_or(ControllerConfigError::MissingEnvironment("HOME"))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Clew"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(root).join("clew"));
        }
        let home = env::var_os("HOME").ok_or(ControllerConfigError::MissingEnvironment("HOME"))?;
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("clew"));
    }

    #[cfg(not(any(windows, unix)))]
    {
        Err(ControllerConfigError::UnsupportedPlatform)
    }
}

#[derive(Debug, Error)]
pub enum ControllerConfigError {
    #[error("required environment variable {0} is not set")]
    MissingEnvironment(&'static str),
    #[error("this platform does not define a Clew state directory")]
    UnsupportedPlatform,
}

#[cfg(any(windows, unix))]
fn hex_prefix(bytes: &[u8]) -> String {
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
    use super::*;

    #[test]
    fn endpoint_is_deterministic_for_state_root() {
        let config = ControllerConfig::new("test-state");
        assert_eq!(config.local_endpoint(), config.local_endpoint());
    }

    #[cfg(unix)]
    #[test]
    fn unix_endpoint_uses_short_temp_socket_for_deep_state_root() {
        let config = ControllerConfig::new(
            std::env::temp_dir()
                .join("clew-deep-state")
                .join("nested-controller-state-directory-that-would-overflow-sun-path-because-it-is-deliberately-very-long")
                .join("another-deliberately-long-segment"),
        );
        let LocalEndpoint::UnixSocket(path) = config.local_endpoint();
        assert!(path.starts_with(env::temp_dir()));
        assert!(path.to_string_lossy().len() < 100);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn unix_short_controller_socket_stays_inside_private_state_dir() {
        let config = ControllerConfig::new("/tmp/c");
        let LocalEndpoint::UnixSocket(path) = config.local_endpoint();
        assert_eq!(path, config.state_layout().local_api_socket_path());
    }

    #[cfg(windows)]
    #[test]
    fn windows_equivalent_relative_state_roots_share_pipe_name() {
        let plain = ControllerConfig::new(r"target\clew-equivalent-state");
        let dotted = ControllerConfig::new(r".\target\clew-equivalent-state");
        let absolute = ControllerConfig::new(
            env::current_dir()
                .unwrap()
                .join(r"target\clew-equivalent-state"),
        );
        assert_eq!(plain.local_endpoint(), dotted.local_endpoint());
        assert_eq!(plain.local_endpoint(), absolute.local_endpoint());
    }

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_is_local_named_pipe() {
        let LocalEndpoint::WindowsNamedPipe(name) =
            ControllerConfig::new(r"C:\Users\test\AppData\Local\Clew").local_endpoint();
        assert!(name.starts_with(r"\\.\pipe\clew-controller-"));
    }
}
