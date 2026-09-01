use std::{path::PathBuf, time::Duration};

use clew_core::ReadPolicy;
use clew_transport::{ReadErrorCode, ReadReply, ReadRequest};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    time::timeout,
};

#[derive(Clone, Debug)]
pub struct HostReadService {
    policy: ReadPolicy,
}

impl HostReadService {
    pub fn new(policy: ReadPolicy) -> Result<Self, clew_core::ControlModelError> {
        policy.validate()?;
        Ok(Self { policy })
    }

    #[must_use]
    pub fn policy(&self) -> &ReadPolicy {
        &self.policy
    }

    pub async fn execute(&self, request: ReadRequest) -> ReadReply {
        if request.validate().is_err() {
            return ReadReply::error(
                ReadErrorCode::InvalidRequest,
                "invalid bounded Read request",
            );
        }
        if !self.policy.allows_read() || request.limit > self.policy.max_result_bytes {
            return ReadReply::error(ReadErrorCode::Denied, "Read is outside the allowed policy");
        }
        match timeout(
            Duration::from_millis(self.policy.timeout_ms as u64),
            self.read_once(request),
        )
        .await
        {
            Ok(reply) => reply,
            Err(_) => ReadReply::error(ReadErrorCode::Timeout, "Read timed out"),
        }
    }

    async fn read_once(&self, request: ReadRequest) -> ReadReply {
        let requested = PathBuf::from(&request.path);
        if !requested.is_absolute() {
            return ReadReply::error(ReadErrorCode::Denied, "Read path must be absolute");
        }
        let target = match tokio::fs::canonicalize(&requested).await {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return ReadReply::error(ReadErrorCode::NotFound, "Read target was not found");
            }
            Err(_) => {
                return ReadReply::error(ReadErrorCode::Io, "Read target could not be opened");
            }
        };

        let mut allowed = false;
        for root in &self.policy.roots {
            let Ok(root) = tokio::fs::canonicalize(root).await else {
                continue;
            };
            if target.starts_with(&root) {
                allowed = true;
                break;
            }
        }
        if !allowed {
            return ReadReply::error(
                ReadErrorCode::Denied,
                "Read target is outside allowed roots",
            );
        }

        let metadata = match tokio::fs::metadata(&target).await {
            Ok(metadata) => metadata,
            Err(_) => return ReadReply::error(ReadErrorCode::Io, "Read metadata failed"),
        };
        if !metadata.is_file() {
            return ReadReply::error(ReadErrorCode::NotFile, "Read target is not a regular file");
        }

        let mut file = match tokio::fs::File::open(&target).await {
            Ok(file) => file,
            Err(_) => {
                return ReadReply::error(ReadErrorCode::Io, "Read target could not be opened");
            }
        };
        if file
            .seek(std::io::SeekFrom::Start(request.offset))
            .await
            .is_err()
        {
            return ReadReply::error(ReadErrorCode::Io, "Read seek failed");
        }
        let mut data = vec![0_u8; request.limit as usize];
        let read = match file.read(&mut data).await {
            Ok(read) => read,
            Err(_) => return ReadReply::error(ReadErrorCode::Io, "Read failed"),
        };
        data.truncate(read);
        ReadReply::data(data)
            .unwrap_or_else(|_| ReadReply::error(ReadErrorCode::Io, "Read result bound failed"))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clew_transport::ReadReply;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn bounded_read_honors_root_offset_and_limit() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("data.bin");
        fs::write(&file, b"0123456789").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 4, 5_000).unwrap(),
        )
        .unwrap();
        let reply = service
            .execute(ReadRequest::new(file.to_string_lossy(), 3, 4).unwrap())
            .await;
        assert_eq!(reply, ReadReply::Data(b"3456".to_vec()));
    }

    #[tokio::test]
    async fn canonical_target_outside_root_is_denied() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, b"private").unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 32, 5_000).unwrap(),
        )
        .unwrap();
        let reply = service
            .execute(ReadRequest::new(outside.to_string_lossy(), 0, 7).unwrap())
            .await;
        assert!(matches!(
            reply,
            ReadReply::Error(error) if error.code == ReadErrorCode::Denied
        ));
    }

    #[tokio::test]
    async fn directory_and_over_policy_limit_fail_closed() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("shared");
        fs::create_dir_all(&root).unwrap();
        let service = HostReadService::new(
            ReadPolicy::new(vec![root.to_string_lossy().into_owned()], 8, 5_000).unwrap(),
        )
        .unwrap();
        let directory = service
            .execute(ReadRequest::new(root.to_string_lossy(), 0, 8).unwrap())
            .await;
        assert!(matches!(
            directory,
            ReadReply::Error(error) if error.code == ReadErrorCode::NotFile
        ));
        let too_large = service
            .execute(ReadRequest::new(root.join("x").to_string_lossy(), 0, 9).unwrap())
            .await;
        assert!(matches!(
            too_large,
            ReadReply::Error(error) if error.code == ReadErrorCode::Denied
        ));
    }
}
