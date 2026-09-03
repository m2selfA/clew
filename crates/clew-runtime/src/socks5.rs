use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use clew_core::{DeviceId, ForwardConnectionId, ForwardId, ProxyId};
use clew_transport::{
    HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION, TcpForwardDestination, TcpForwardErrorCode,
    TcpForwardReply,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use crate::{
    RemoteHub,
    forward::{open_forward_connection, pump_forward_connection},
};

pub const HARD_MAX_SOCKS5_LISTENERS: usize = 64;
const SOCKS5_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_NO_AUTH: u8 = 0x00;
const SOCKS5_NO_ACCEPTABLE_AUTH: u8 = 0xff;
const SOCKS5_CONNECT: u8 = 0x01;
const SOCKS5_REP_GENERAL_FAILURE: u8 = 0x01;
const SOCKS5_REP_NOT_ALLOWED: u8 = 0x02;
const SOCKS5_REP_CONNECTION_REFUSED: u8 = 0x05;
const SOCKS5_REP_TTL_EXPIRED: u8 = 0x06;
const SOCKS5_REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const SOCKS5_REP_ADDRESS_NOT_SUPPORTED: u8 = 0x08;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Socks5Info {
    pub proxy_id: ProxyId,
    pub device_id: DeviceId,
    pub listen_addr: SocketAddr,
}

#[derive(Clone)]
pub struct Socks5ProxyManager {
    inner: Arc<Socks5ProxyManagerInner>,
}

struct Socks5ProxyManagerInner {
    remote: RemoteHub,
    proxies: Mutex<BTreeMap<ProxyId, Socks5Entry>>,
}

struct Socks5Entry {
    info: Socks5Info,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for Socks5ProxyManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.proxies.lock().map(|map| map.len()).unwrap_or(0);
        formatter
            .debug_struct("Socks5ProxyManager")
            .field("listener_count", &count)
            .finish()
    }
}

impl Drop for Socks5ProxyManagerInner {
    fn drop(&mut self) {
        if let Ok(proxies) = self.proxies.get_mut() {
            for entry in proxies.values() {
                let _ = entry.shutdown.send(true);
                entry.task.abort();
            }
        }
    }
}

impl Socks5ProxyManager {
    #[must_use]
    pub fn new(remote: RemoteHub) -> Self {
        Self {
            inner: Arc::new(Socks5ProxyManagerInner {
                remote,
                proxies: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub async fn add(
        &self,
        device_id: DeviceId,
        listen_port: u16,
    ) -> Result<Socks5Info, Socks5ProxyManagerError> {
        {
            let proxies = self
                .inner
                .proxies
                .lock()
                .map_err(|_| Socks5ProxyManagerError::StatePoisoned)?;
            if proxies.len() >= HARD_MAX_SOCKS5_LISTENERS {
                return Err(Socks5ProxyManagerError::Capacity);
            }
        }
        let listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listen_port,
        ))
        .await?;
        let listen_addr = listener.local_addr()?;
        let mut proxy_id = ProxyId::new();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let remote = self.inner.remote.clone();
        let info = {
            let mut proxies = self
                .inner
                .proxies
                .lock()
                .map_err(|_| Socks5ProxyManagerError::StatePoisoned)?;
            if proxies.len() >= HARD_MAX_SOCKS5_LISTENERS {
                return Err(Socks5ProxyManagerError::Capacity);
            }
            while proxies.contains_key(&proxy_id) {
                proxy_id = ProxyId::new();
            }
            let info = Socks5Info {
                proxy_id,
                device_id,
                listen_addr,
            };
            let task_info = info.clone();
            let task = tokio::spawn(serve_socks5_listener(
                remote,
                task_info,
                listener,
                shutdown_rx,
            ));
            proxies.insert(
                proxy_id,
                Socks5Entry {
                    info: info.clone(),
                    shutdown,
                    task,
                },
            );
            info
        };
        Ok(info)
    }

    pub fn list(&self) -> Result<Vec<Socks5Info>, Socks5ProxyManagerError> {
        Ok(self
            .inner
            .proxies
            .lock()
            .map_err(|_| Socks5ProxyManagerError::StatePoisoned)?
            .values()
            .map(|entry| entry.info.clone())
            .collect())
    }

    pub async fn remove(&self, proxy_id: ProxyId) -> Result<Socks5Info, Socks5ProxyManagerError> {
        let entry = self
            .inner
            .proxies
            .lock()
            .map_err(|_| Socks5ProxyManagerError::StatePoisoned)?
            .remove(&proxy_id)
            .ok_or(Socks5ProxyManagerError::NotFound(proxy_id))?;
        let _ = entry.shutdown.send(true);
        let info = entry.info.clone();
        let _ = timeout(Duration::from_secs(2), entry.task).await;
        Ok(info)
    }
}

async fn serve_socks5_listener(
    remote: RemoteHub,
    info: Socks5Info,
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
                    serve_socks5_connection(remote, info, stream, shutdown, permit).await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn serve_socks5_connection(
    remote: RemoteHub,
    info: Socks5Info,
    mut local: TcpStream,
    mut shutdown: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    let destination = tokio::select! {
        _ = shutdown.changed() => return,
        result = timeout(SOCKS5_HANDSHAKE_TIMEOUT, negotiate_socks5(&mut local)) => {
            match result {
                Ok(Ok(destination)) => destination,
                _ => return,
            }
        }
    };
    let connection_id = ForwardConnectionId::new();
    let forward_id = ForwardId::new();
    let opened = tokio::select! {
        _ = shutdown.changed() => return,
        result = open_forward_connection(
            &remote,
            info.device_id,
            forward_id,
            connection_id,
            destination,
        ) => result,
    };
    let (generation, reply) = match opened {
        Ok(result) => result,
        Err(_) => {
            let _ = write_socks5_reply(&mut local, SOCKS5_REP_GENERAL_FAILURE).await;
            return;
        }
    };
    match reply {
        TcpForwardReply::Opened {
            connection_id: actual,
        } if actual == connection_id => {
            if write_socks5_reply(&mut local, 0x00).await.is_err() {
                return;
            }
        }
        TcpForwardReply::Error(error) => {
            let _ = write_socks5_reply(&mut local, map_forward_error_to_socks(error.code)).await;
            return;
        }
        _ => {
            let _ = write_socks5_reply(&mut local, SOCKS5_REP_GENERAL_FAILURE).await;
            return;
        }
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

async fn negotiate_socks5<S>(stream: &mut S) -> Result<TcpForwardDestination, Socks5HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).await?;
    if greeting[0] != SOCKS5_VERSION || greeting[1] == 0 {
        return Err(Socks5HandshakeError::InvalidGreeting);
    }
    let mut methods = vec![0_u8; greeting[1] as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&SOCKS5_NO_AUTH) {
        stream
            .write_all(&[SOCKS5_VERSION, SOCKS5_NO_ACCEPTABLE_AUTH])
            .await?;
        stream.flush().await?;
        return Err(Socks5HandshakeError::NoAcceptableAuth);
    }
    stream.write_all(&[SOCKS5_VERSION, SOCKS5_NO_AUTH]).await?;
    stream.flush().await?;

    let mut head = [0_u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != SOCKS5_VERSION || head[2] != 0 {
        let _ = write_socks5_reply(stream, SOCKS5_REP_GENERAL_FAILURE).await;
        return Err(Socks5HandshakeError::InvalidRequest);
    }
    if head[1] != SOCKS5_CONNECT {
        let _ = write_socks5_reply(stream, SOCKS5_REP_COMMAND_NOT_SUPPORTED).await;
        return Err(Socks5HandshakeError::UnsupportedCommand(head[1]));
    }
    let host = match head[3] {
        0x01 => {
            let mut octets = [0_u8; 4];
            stream.read_exact(&mut octets).await?;
            Ipv4Addr::from(octets).to_string()
        }
        0x04 => {
            let mut octets = [0_u8; 16];
            stream.read_exact(&mut octets).await?;
            Ipv6Addr::from(octets).to_string()
        }
        0x03 => {
            let length = stream.read_u8().await? as usize;
            if length == 0 {
                let _ = write_socks5_reply(stream, SOCKS5_REP_ADDRESS_NOT_SUPPORTED).await;
                return Err(Socks5HandshakeError::InvalidDomain);
            }
            let mut domain = vec![0_u8; length];
            stream.read_exact(&mut domain).await?;
            match String::from_utf8(domain) {
                Ok(domain) => domain,
                Err(_) => {
                    let _ = write_socks5_reply(stream, SOCKS5_REP_ADDRESS_NOT_SUPPORTED).await;
                    return Err(Socks5HandshakeError::InvalidDomain);
                }
            }
        }
        other => {
            let _ = write_socks5_reply(stream, SOCKS5_REP_ADDRESS_NOT_SUPPORTED).await;
            return Err(Socks5HandshakeError::UnsupportedAddressType(other));
        }
    };
    let port = stream.read_u16().await?;
    match TcpForwardDestination::new(host, port) {
        Ok(destination) => Ok(destination),
        Err(_) => {
            let _ = write_socks5_reply(stream, SOCKS5_REP_ADDRESS_NOT_SUPPORTED).await;
            Err(Socks5HandshakeError::InvalidDestination)
        }
    }
}

async fn write_socks5_reply<S>(stream: &mut S, reply: u8) -> Result<(), std::io::Error>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(&[SOCKS5_VERSION, reply, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    stream.flush().await
}

fn map_forward_error_to_socks(code: TcpForwardErrorCode) -> u8 {
    match code {
        TcpForwardErrorCode::Denied => SOCKS5_REP_NOT_ALLOWED,
        TcpForwardErrorCode::ConnectFailed => SOCKS5_REP_CONNECTION_REFUSED,
        TcpForwardErrorCode::Timeout => SOCKS5_REP_TTL_EXPIRED,
        TcpForwardErrorCode::InvalidRequest
        | TcpForwardErrorCode::NotFound
        | TcpForwardErrorCode::Capacity
        | TcpForwardErrorCode::Io => SOCKS5_REP_GENERAL_FAILURE,
    }
}

#[derive(Debug, Error)]
enum Socks5HandshakeError {
    #[error("SOCKS5 I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid SOCKS5 greeting")]
    InvalidGreeting,
    #[error("SOCKS5 client offered no supported authentication method")]
    NoAcceptableAuth,
    #[error("invalid SOCKS5 request")]
    InvalidRequest,
    #[error("unsupported SOCKS5 command {0}")]
    UnsupportedCommand(u8),
    #[error("unsupported SOCKS5 address type {0}")]
    UnsupportedAddressType(u8),
    #[error("invalid SOCKS5 domain")]
    InvalidDomain,
    #[error("invalid SOCKS5 destination")]
    InvalidDestination,
}

#[derive(Debug, Error)]
pub enum Socks5ProxyManagerError {
    #[error("SOCKS5 listener I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SOCKS5 proxy manager state is poisoned")]
    StatePoisoned,
    #[error("SOCKS5 listener capacity is exhausted")]
    Capacity,
    #[error("SOCKS5 proxy {0} was not found")]
    NotFound(ProxyId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn socks5_parser_accepts_connect_and_rejects_udp_associate() {
        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move { negotiate_socks5(&mut server).await.unwrap() });
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0_u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);
        client
            .write_all(&[
                5, 1, 0, 3, 9, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0x1f, 0x90,
            ])
            .await
            .unwrap();
        assert_eq!(
            server_task.await.unwrap(),
            TcpForwardDestination::new("localhost", 8080).unwrap()
        );

        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move { negotiate_socks5(&mut server).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.read_exact(&mut method).await.unwrap();
        client
            .write_all(&[5, 3, 0, 1, 127, 0, 0, 1, 0, 53])
            .await
            .unwrap();
        let mut reply = [0_u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], SOCKS5_REP_COMMAND_NOT_SUPPORTED);
        assert!(matches!(
            server_task.await.unwrap(),
            Err(Socks5HandshakeError::UnsupportedCommand(3))
        ));

        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move { negotiate_socks5(&mut server).await });
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.read_exact(&mut method).await.unwrap();
        client
            .write_all(&[5, 1, 0, 3, 1, 0xff, 0, 80])
            .await
            .unwrap();
        let mut reply = [0_u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], SOCKS5_REP_ADDRESS_NOT_SUPPORTED);
        assert!(matches!(
            server_task.await.unwrap(),
            Err(Socks5HandshakeError::InvalidDomain)
        ));
    }

    #[tokio::test]
    async fn socks5_manager_is_loopback_owned_until_remove() {
        let manager = Socks5ProxyManager::new(RemoteHub::default());
        let info = manager.add(DeviceId::new(), 0).await.unwrap();
        assert!(info.listen_addr.ip().is_loopback());
        assert_ne!(info.listen_addr.port(), 0);
        assert_eq!(manager.list().unwrap(), vec![info.clone()]);
        let mut client = TcpStream::connect(info.listen_addr).await.unwrap();
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut method = [0_u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [5, 0]);
        client
            .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, 0, 9])
            .await
            .unwrap();
        let mut reply = [0_u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], SOCKS5_REP_GENERAL_FAILURE);
        manager.remove(info.proxy_id).await.unwrap();
        assert!(manager.list().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(TcpStream::connect(info.listen_addr).await.is_err());
    }
}
