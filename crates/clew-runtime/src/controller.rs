use std::{
    fs,
    future::Future,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use clew_core::STATE_SCHEMA_VERSION;
use clew_identity::{ControllerIdentityStore, DeviceIdentityStoreError};
use clew_transport::IrohOuter;
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use uuid::Uuid;

use crate::{
    ControllerConfig, ControllerControlStore, LocalApiClient, LocalApiClientError,
    MAX_REMOTE_CONNECTIONS, OutfitAssetStore, OutfitLibrary, RemoteHub, handle_remote_connection,
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
    shutdown_rx: watch::Receiver<bool>,
    remote: IrohOuter,
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
        let mut local_shutdown = self.shutdown_rx.clone();
        let mut local_handlers = JoinSet::new();
        let mut remote_handlers = JoinSet::new();
        let (remote_accept_tx, mut remote_accept_rx) = mpsc::channel(MAX_REMOTE_CONNECTIONS);
        let remote_acceptor = self.remote.clone();
        let mut remote_accept_task = tokio::spawn(async move {
            loop {
                match remote_acceptor.accept_classified().await {
                    Ok(accepted) => {
                        if remote_accept_tx.send(accepted).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        eprintln!("Clew remote accept failed: {error}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    remote_accept_task.abort();
                    let _ = (&mut remote_accept_task).await;
                    local_handlers.abort_all();
                    remote_handlers.abort_all();
                    while local_handlers.join_next().await.is_some() {}
                    while remote_handlers.join_next().await.is_some() {}
                    self.remote.close().await;
                    return Ok(());
                }
                changed = local_shutdown.changed() => {
                    if changed.is_err() || *local_shutdown.borrow() {
                        remote_accept_task.abort();
                        let _ = (&mut remote_accept_task).await;
                        local_handlers.abort_all();
                        remote_handlers.abort_all();
                        while local_handlers.join_next().await.is_some() {}
                        while remote_handlers.join_next().await.is_some() {}
                        self.remote.close().await;
                        return Ok(());
                    }
                }
                accepted = self.listener.accept(), if local_handlers.len() < MAX_LOCAL_API_CONNECTIONS => {
                    let stream = accepted?;
                    let secret = self.secret.clone();
                    let state = (*self.state).clone();
                    local_handlers.spawn(async move {
                        serve_connection(stream, secret, state).await;
                    });
                }
                accepted = remote_accept_rx.recv(), if remote_handlers.len() < MAX_REMOTE_CONNECTIONS => {
                    let Some((protocol, stream)) = accepted else {
                        return Err(ControllerError::RemoteAcceptLoopStopped);
                    };
                    let identity = self.state.controller_identity.clone();
                    let control = Arc::clone(&self.state.control);
                    let hub = self.state.remote.clone();
                    remote_handlers.spawn(async move {
                        if let Err(error) =
                            handle_remote_connection(protocol, stream, identity, control, hub).await
                        {
                            eprintln!(
                                "Clew remote connection ended ({})",
                                error.category()
                            );
                        }
                    });
                }
                _ = local_handlers.join_next(), if !local_handlers.is_empty() => {}
                _ = remote_handlers.join_next(), if !remote_handlers.is_empty() => {}
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

    let controller_identity = ControllerIdentityStore::new(layout.clone()).load_or_create()?;
    let controller_id = controller_identity.identity().controller_id();
    let control = Arc::new(Mutex::new(ControllerControlStore::load_or_create(
        layout.clone(),
        controller_id,
    )?));
    let outfits = Arc::new(Mutex::new(OutfitLibrary::load_or_create(layout.clone())?));
    let outfit_assets = Arc::new(Mutex::new(OutfitAssetStore::load_or_create(
        layout.clone(),
    )?));
    let remote = IrohOuter::bind_with_secret(controller_identity.iroh_endpoint_secret()).await?;
    let remote_endpoint_id = remote.addr().id.to_string();
    let remote_hub = RemoteHub::default();
    let forwards = crate::TcpForwardManager::new(remote_hub.clone());
    let socks5 = crate::Socks5ProxyManager::new(remote_hub.clone());

    let secret = LocalApiSecret::rotate(&layout)?;
    let listener = LocalListener::bind(&config.local_endpoint())?;
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ControllerError::ClockBeforeUnixEpoch)?
        .as_millis()
        .try_into()
        .map_err(|_| ControllerError::ClockOverflow)?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = Arc::new(LocalApiState {
        status: ControllerStatus {
            ready: true,
            controller_id,
            pid: std::process::id(),
            instance_id,
            started_unix_ms,
            state_schema_version: STATE_SCHEMA_VERSION,
            local_api_version: crate::LOCAL_API_VERSION,
            remote_endpoint_id: Some(remote_endpoint_id),
        },
        controller_identity: controller_identity.clone(),
        controller_outer: Some(remote.clone()),
        control,
        outfits,
        outfit_assets,
        remote: remote_hub,
        forwards,
        socks5,
        shutdown_tx,
    });

    Ok(ControllerStart::Primary(ControllerRuntime {
        config,
        _ownership: ownership,
        listener,
        secret,
        state,
        shutdown_rx,
        remote,
    }))
}

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error(transparent)]
    IdentityStore(#[from] DeviceIdentityStoreError),
    #[error("controller I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    LocalApi(#[from] LocalApiClientError),
    #[error(transparent)]
    ControlStore(#[from] crate::ControlStoreError),
    #[error(transparent)]
    OutfitStore(#[from] crate::OutfitStoreError),
    #[error(transparent)]
    OutfitAsset(#[from] crate::OutfitAssetError),
    #[error(transparent)]
    RemoteTransport(#[from] clew_transport::IrohOuterError),
    #[error("remote accept loop stopped unexpectedly")]
    RemoteAcceptLoopStopped,
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
    async fn authenticated_local_api_shutdown_releases_ownership() {
        let temp = tempdir().unwrap();
        let config = ControllerConfig::new(temp.path());
        let primary = match start_controller(config.clone()).await.unwrap() {
            ControllerStart::Primary(runtime) => runtime,
            ControllerStart::Existing(_) => panic!("unexpected existing controller"),
        };
        let task = tokio::spawn(primary.serve_until(std::future::pending()));

        LocalApiClient::new(config.clone())
            .controller_shutdown()
            .await
            .unwrap();
        task.await.unwrap().unwrap();

        assert!(matches!(
            start_controller(config).await.unwrap(),
            ControllerStart::Primary(_)
        ));
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
