use std::{env, fs, path::PathBuf};

use clew_core::StateLayout;
#[cfg(windows)]
use sha2::{Digest, Sha256};
use thiserror::Error;

#[cfg(windows)]
const PIPE_NAME_DOMAIN: &[u8] = b"clew/local-api-pipe/v1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerConfig {
    state_root: PathBuf,
}

impl ControllerConfig {
    #[must_use]
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        Self {
            state_root: state_root.into(),
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
            let normalized = self.state_root.to_string_lossy().to_lowercase();
            let mut hasher = Sha256::new();
            hasher.update(PIPE_NAME_DOMAIN);
            hasher.update(normalized.as_bytes());
            let digest = hasher.finalize();
            let suffix = hex_prefix(&digest[..8]);
            LocalEndpoint::WindowsNamedPipe(format!(r"\\.\pipe\clew-controller-{suffix}"))
        }
        #[cfg(unix)]
        {
            LocalEndpoint::UnixSocket(self.state_layout().local_api_socket_path())
        }
    }
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

#[cfg(windows)]
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

    #[cfg(windows)]
    #[test]
    fn windows_endpoint_is_local_named_pipe() {
        let LocalEndpoint::WindowsNamedPipe(name) =
            ControllerConfig::new(r"C:\Users\test\AppData\Local\Clew").local_endpoint();
        assert!(name.starts_with(r"\\.\pipe\clew-controller-"));
    }
}
