use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{DeviceId, ForwardConnectionId, ForwardId};
use clew_transport::{
    HARD_MAX_TCP_FORWARD_CHUNK_BYTES, HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS,
    HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION, HARD_MAX_TCP_FORWARD_READ_WAIT_MS,
    TcpForwardDestination, TcpForwardReply, TcpForwardRequest,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use crate::RemoteHub;

pub const HARD_MAX_FORWARD_LISTENERS: usize = 64;
const LOCAL_READ_WAIT: Duration = Duration::from_millis(25);
const REMOTE_READ_WAIT_MS: u32 = 100;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardInfo {
    pub forward_id: ForwardId,
    pub device_id: DeviceId,
    pub listen_addr: SocketAddr,
    pub destination: TcpForwardDestination,
}

#[derive(Clone)]
pub struct TcpForwardManager {
    inner: Arc<TcpForwardManagerInner>,
}

struct TcpForwardManagerInner {
    remote: RemoteHub,
    forwards: Mutex<BTreeMap<ForwardId, ForwardEntry>>,
}

struct ForwardEntry {
    info: ForwardInfo,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for TcpForwardManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.forwards.lock().map(|map| map.len()).unwrap_or(0);
        formatter
            .debug_struct("TcpForwardManager")
            .field("listener_count", &count)
            .finish()
    }
}

impl Drop for TcpForwardManagerInner {
    fn drop(&mut self) {
        if let Ok(forwards) = self.forwards.get_mut() {
            for entry in forwards.values() {
                let _ = entry.shutdown.send(true);
                entry.task.abort();
            }
        }
    }
}

impl TcpForwardManager {
    #[must_use]
    pub fn new(remote: RemoteHub) -> Self {
        Self {
            inner: Arc::new(TcpForwardManagerInner {
                remote,
                forwards: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub async fn add(
        &self,
        device_id: DeviceId,
        listen_port: u16,
        destination: TcpForwardDestination,
    ) -> Result<ForwardInfo, TcpForwardManagerError> {
        destination.validate()?;
        {
            let forwards = self
                .inner
                .forwards
                .lock()
                .map_err(|_| TcpForwardManagerError::StatePoisoned)?;
            if forwards.len() >= HARD_MAX_FORWARD_LISTENERS {
                return Err(TcpForwardManagerError::Capacity);
            }
        }
        let listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listen_port,
        ))
        .await?;
        let listen_addr = listener.local_addr()?;
        let mut forward_id = ForwardId::new();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let remote = self.inner.remote.clone();
        let info = {
            let mut forwards = self
                .inner
                .forwards
                .lock()
                .map_err(|_| TcpForwardManagerError::StatePoisoned)?;
            if forwards.len() >= HARD_MAX_FORWARD_LISTENERS {
                return Err(TcpForwardManagerError::Capacity);
            }
            while forwards.contains_key(&forward_id) {
                forward_id = ForwardId::new();
            }
            let info = ForwardInfo {
                forward_id,
                device_id,
                listen_addr,
                destination,
            };
            let task_info = info.clone();
            let task = tokio::spawn(serve_listener(remote, task_info, listener, shutdown_rx));
            forwards.insert(
                forward_id,
                ForwardEntry {
                    info: info.clone(),
                    shutdown,
                    task,
                },
            );
            info
        };
        Ok(info)
    }

    pub fn list(&self) -> Result<Vec<ForwardInfo>, TcpForwardManagerError> {
        Ok(self
            .inner
            .forwards
            .lock()
            .map_err(|_| TcpForwardManagerError::StatePoisoned)?
            .values()
            .map(|entry| entry.info.clone())
            .collect())
    }

    pub async fn remove(
        &self,
        forward_id: ForwardId,
    ) -> Result<ForwardInfo, TcpForwardManagerError> {
        let entry = self
            .inner
            .forwards
            .lock()
            .map_err(|_| TcpForwardManagerError::StatePoisoned)?
            .remove(&forward_id)
            .ok_or(TcpForwardManagerError::NotFound(forward_id))?;
        let _ = entry.shutdown.send(true);
        let info = entry.info.clone();
        let _ = timeout(Duration::from_secs(2), entry.task).await;
        Ok(info)
    }
}

async fn serve_listener(
    remote: RemoteHub,
    info: ForwardInfo,
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
) {
    let capacity = Arc::new(Semaphore::new(HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION));
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break; };
                let Ok(permit) = Arc::clone(&capacity).try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let remote = remote.clone();
                let info = info.clone();
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    serve_local_connection(remote, info, stream, shutdown, permit).await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn serve_local_connection(
    remote: RemoteHub,
    info: ForwardInfo,
    mut local: TcpStream,
    mut shutdown: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    let connection_id = ForwardConnectionId::new();
    let opened = tokio::select! {
        _ = shutdown.changed() => return,
        result = open_forward_connection(
            &remote,
            info.device_id,
            info.forward_id,
            connection_id,
            info.destination.clone(),
        ) => result,
    };
    let Ok((
        generation,
        TcpForwardReply::Opened {
            connection_id: actual,
        },
    )) = opened
    else {
        return;
    };
    if actual != connection_id {
        return;
    }
    pump_forward_connection(
        remote,
        info.device_id,
        generation,
        connection_id,
        &mut local,
        shutdown,
    )
    .await;
}

pub(crate) async fn open_forward_connection(
    remote: &RemoteHub,
    device_id: DeviceId,
    forward_id: ForwardId,
    connection_id: ForwardConnectionId,
    destination: TcpForwardDestination,
) -> Result<(u64, TcpForwardReply), crate::RemoteHubError> {
    let open = TcpForwardRequest::open(
        forward_id,
        connection_id,
        destination,
        HARD_MAX_TCP_FORWARD_CONNECT_TIMEOUT_MS,
    )?;
    remote.tcp_forward_open(device_id, open).await
}

pub(crate) async fn pump_forward_connection(
    remote: RemoteHub,
    device_id: DeviceId,
    generation: u64,
    connection_id: ForwardConnectionId,
    local: &mut TcpStream,
    shutdown: watch::Receiver<bool>,
) {
    pump_forward_connection_with_initial(
        remote,
        device_id,
        generation,
        connection_id,
        local,
        shutdown,
        &[],
    )
    .await;
}

pub(crate) async fn pump_forward_connection_with_initial(
    remote: RemoteHub,
    device_id: DeviceId,
    generation: u64,
    connection_id: ForwardConnectionId,
    local: &mut TcpStream,
    mut shutdown: watch::Receiver<bool>,
    initial_write: &[u8],
) {
    let _ = local.set_nodelay(true);
    let mut initial_offset = 0_usize;
    let mut local_eof_sent = false;
    let mut local_buffer = vec![0_u8; HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize];
    loop {
        let (write, write_eof) = if initial_offset < initial_write.len() {
            let end = initial_offset
                .saturating_add(HARD_MAX_TCP_FORWARD_CHUNK_BYTES as usize)
                .min(initial_write.len());
            let write = initial_write[initial_offset..end].to_vec();
            initial_offset = end;
            (write, false)
        } else if local_eof_sent {
            (Vec::new(), false)
        } else {
            match tokio::select! {
                _ = shutdown.changed() => break,
                result = timeout(LOCAL_READ_WAIT, local.read(&mut local_buffer)) => result,
            } {
                Err(_) => (Vec::new(), false),
                Ok(Ok(0)) => {
                    local_eof_sent = true;
                    (Vec::new(), true)
                }
                Ok(Ok(read)) => (local_buffer[..read].to_vec(), false),
                Ok(Err(_)) => break,
            }
        };
        let request = match TcpForwardRequest::exchange(
            connection_id,
            &write,
            write_eof,
            HARD_MAX_TCP_FORWARD_CHUNK_BYTES,
            REMOTE_READ_WAIT_MS.min(HARD_MAX_TCP_FORWARD_READ_WAIT_MS),
        ) {
            Ok(request) => request,
            Err(_) => break,
        };
        let reply = tokio::select! {
            _ = shutdown.changed() => break,
            result = remote.tcp_forward_on_generation(device_id, generation, request) => {
                let Ok(reply) = result else { break; };
                reply
            }
        };
        let TcpForwardReply::Exchanged {
            connection_id: actual,
            read_eof,
            ..
        } = &reply
        else {
            break;
        };
        if *actual != connection_id {
            break;
        }
        let Ok(read) = reply.read_bytes() else {
            break;
        };
        if !read.is_empty() && local.write_all(&read).await.is_err() {
            break;
        }
        if *read_eof {
            let _ = local.shutdown().await;
            break;
        }
    }
    let _ = remote
        .tcp_forward_on_generation(
            device_id,
            generation,
            TcpForwardRequest::Close { connection_id },
        )
        .await;
}

#[derive(Debug, Error)]
pub enum TcpForwardManagerError {
    #[error(transparent)]
    Protocol(#[from] clew_transport::TcpForwardProtocolError),
    #[error("TCP forward listener I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("TCP forward manager state is poisoned")]
    StatePoisoned,
    #[error("TCP forward listener capacity is exhausted")]
    Capacity,
    #[error("TCP forward {0} was not found")]
    NotFound(ForwardId),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manager_owns_loopback_listener_until_explicit_remove() {
        let manager = TcpForwardManager::new(RemoteHub::default());
        let info = manager
            .add(
                DeviceId::new(),
                0,
                TcpForwardDestination::new("127.0.0.1", 9).unwrap(),
            )
            .await
            .unwrap();
        assert!(info.listen_addr.ip().is_loopback());
        assert_ne!(info.listen_addr.port(), 0);
        assert_eq!(manager.list().unwrap(), vec![info.clone()]);
        let _client = TcpStream::connect(info.listen_addr).await.unwrap();
        manager.remove(info.forward_id).await.unwrap();
        assert!(manager.list().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(TcpStream::connect(info.listen_addr).await.is_err());
    }
}
