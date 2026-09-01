use std::{
    fs,
    future::Future,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use clew_core::STATE_SCHEMA_VERSION;
use thiserror::Error;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::{
    ControllerConfig, LocalApiClient, LocalApiClientError,
    local_api::{
        ControllerStatus, LocalApiSecret, LocalApiState, MAX_LOCAL_API_CONNECTIONS,
        serve_connection,
    },
    lock::{ControllerOwnership, OwnershipAttempt},
    transport::LocalListener,
};

pub enum ControllerStart {
    Primary(ControllerRuntime),
    Existing(ControllerStatus),
}

pub struct ControllerRuntime {
    config: ControllerConfig,
    listener: LocalListener,
    secret: LocalApiSecret,
    state: Arc<LocalApiState>,
    // Keep ownership last so the IPC endpoint and in-memory state are dropped first.
    _ownership: ControllerOwnership,
}

impl ControllerRuntime {
    #[must_use]
    pub fn status(&self) -> &ControllerStatus {
        &self.state.status
    }

    #[must_use]
    pub fn config(&self) -> &ControllerConfig {
        &self.config
    }

    pub async fn serve_until<F>(mut self, shutdown: F) -> Result<(), ControllerError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut handlers = JoinSet::new();
        loop {
            if handlers.len() >= MAX_LOCAL_API_CONNECTIONS {
                tokio::select! {
                    _ = &mut shutdown => {
                        handlers.abort_all();
                        while handlers.join_next().await.is_some() {}
                        return Ok(());
                    }
                    _ = handlers.join_next() => {}
                }
                continue;
            }

            tokio::select! {
                _ = &mut shutdown => {
                    handlers.abort_all();
                    while handlers.join_next().await.is_some() {}
                    return Ok(());
                }
                accepted = self.listener.accept() => {
                    let stream = accepted?;
                    let secret = self.secret.clone();
                    let state = (*self.state).clone();
                    handlers.spawn(async move {
                        serve_connection(stream, secret, state).await;
                    });
                }
                _ = handlers.join_next(), if !handlers.is_empty() => {}
            }
        }
    }
}

impl Drop for ControllerRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.config.state_layout().local_api_secret_path());
    }
}

pub async fn start_controller(
    config: ControllerConfig,
) -> Result<ControllerStart, ControllerError> {
    config.prepare_state_dir()?;
    let layout = config.state_layout();
    let instance_id = Uuid::new_v4().to_string();

    let ownership = match ControllerOwnership::try_acquire(&layout, &instance_id)? {
        OwnershipAttempt::Acquired(ownership) => ownership,
        OwnershipAttempt::Busy => {
            let status = LocalApiClient::new(config).controller_status().await?;
            return Ok(ControllerStart::Existing(status));
        }
    };

    let secret = LocalApiSecret::rotate(&layout)?;
    let listener = LocalListener::bind(&config.local_endpoint())?;
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ControllerError::ClockBeforeUnixEpoch)?
        .as_millis()
        .try_into()
        .map_err(|_| ControllerError::ClockOverflow)?;
    let state = Arc::new(LocalApiState {
        status: ControllerStatus {
            ready: true,
            pid: std::process::id(),
            instance_id,
            started_unix_ms,
            state_schema_version: STATE_SCHEMA_VERSION,
            local_api_version: crate::LOCAL_API_VERSION,
        },
        devices: Vec::new(),
    });

    Ok(ControllerStart::Primary(ControllerRuntime {
        config,
        _ownership: ownership,
        listener,
        secret,
        state,
    }))
}

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("controller I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    LocalApi(#[from] LocalApiClientError),
    #[error("system clock is before the Unix epoch")]
    ClockBeforeUnixEpoch,
    #[error("system clock value does not fit in milliseconds")]
    ClockOverflow,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn graceful_shutdown_releases_owner_and_endpoint() {
        let temp = tempdir().unwrap();
        let config = ControllerConfig::new(temp.path());
        let primary = match start_controller(config.clone()).await.unwrap() {
            ControllerStart::Primary(runtime) => runtime,
            ControllerStart::Existing(_) => panic!("unexpected existing controller"),
        };
        let first_instance = primary.status().instance_id.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(primary.serve_until(async move {
            let _ = shutdown_rx.await;
        }));

        let observed = LocalApiClient::new(config.clone())
            .controller_status()
            .await
            .unwrap();
        assert_eq!(observed.instance_id, first_instance);
        assert!(observed.ready);

        shutdown_tx.send(()).unwrap();
        task.await.unwrap().unwrap();

        let restarted = start_controller(config).await.unwrap();
        assert!(matches!(restarted, ControllerStart::Primary(_)));
    }

    #[tokio::test]
    async fn second_start_observes_existing_controller_instead_of_owning_state() {
        let temp = tempdir().unwrap();
        let config = ControllerConfig::new(temp.path());
        let primary = match start_controller(config.clone()).await.unwrap() {
            ControllerStart::Primary(runtime) => runtime,
            ControllerStart::Existing(_) => panic!("unexpected existing controller"),
        };
        let first_instance = primary.status().instance_id.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(primary.serve_until(async move {
            let _ = shutdown_rx.await;
        }));

        let second = start_controller(config).await.unwrap();
        match second {
            ControllerStart::Existing(status) => assert_eq!(status.instance_id, first_instance),
            ControllerStart::Primary(_) => panic!("parallel controller ownership was created"),
        }

        shutdown_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
