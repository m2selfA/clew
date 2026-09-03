use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
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
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use crate::{
    RemoteHub,
    forward::{open_forward_connection, pump_forward_connection_with_initial},
};

pub const HARD_MAX_HTTP_CONNECT_LISTENERS: usize = 64;
pub const HARD_MAX_HTTP_CONNECT_HEADER_BYTES: usize = 16 * 1024;
const HTTP_CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_READ_CHUNK_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HttpConnectInfo {
    pub proxy_id: ProxyId,
    pub device_id: DeviceId,
    pub listen_addr: SocketAddr,
}

#[derive(Clone)]
pub struct HttpConnectProxyManager {
    inner: Arc<HttpConnectProxyManagerInner>,
}

struct HttpConnectProxyManagerInner {
    remote: RemoteHub,
    proxies: Mutex<BTreeMap<ProxyId, HttpConnectEntry>>,
}

struct HttpConnectEntry {
    info: HttpConnectInfo,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl std::fmt::Debug for HttpConnectProxyManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.proxies.lock().map(|map| map.len()).unwrap_or(0);
        formatter
            .debug_struct("HttpConnectProxyManager")
            .field("listener_count", &count)
            .finish()
    }
}

impl Drop for HttpConnectProxyManagerInner {
    fn drop(&mut self) {
        if let Ok(proxies) = self.proxies.get_mut() {
            for entry in proxies.values() {
                let _ = entry.shutdown.send(true);
                entry.task.abort();
            }
        }
    }
}

impl HttpConnectProxyManager {
    #[must_use]
    pub fn new(remote: RemoteHub) -> Self {
        Self {
            inner: Arc::new(HttpConnectProxyManagerInner {
                remote,
                proxies: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    pub async fn add(
        &self,
        device_id: DeviceId,
        listen_port: u16,
    ) -> Result<HttpConnectInfo, HttpConnectProxyManagerError> {
        {
            let proxies = self
                .inner
                .proxies
                .lock()
                .map_err(|_| HttpConnectProxyManagerError::StatePoisoned)?;
            if proxies.len() >= HARD_MAX_HTTP_CONNECT_LISTENERS {
                return Err(HttpConnectProxyManagerError::Capacity);
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
                .map_err(|_| HttpConnectProxyManagerError::StatePoisoned)?;
            if proxies.len() >= HARD_MAX_HTTP_CONNECT_LISTENERS {
                return Err(HttpConnectProxyManagerError::Capacity);
            }
            while proxies.contains_key(&proxy_id) {
                proxy_id = ProxyId::new();
            }
            let info = HttpConnectInfo {
                proxy_id,
                device_id,
                listen_addr,
            };
            let task = tokio::spawn(serve_http_connect_listener(
                remote,
                info.clone(),
                listener,
                shutdown_rx,
            ));
            proxies.insert(
                proxy_id,
                HttpConnectEntry {
                    info: info.clone(),
                    shutdown,
                    task,
                },
            );
            info
        };
        Ok(info)
    }

    pub fn list(&self) -> Result<Vec<HttpConnectInfo>, HttpConnectProxyManagerError> {
        Ok(self
            .inner
            .proxies
            .lock()
            .map_err(|_| HttpConnectProxyManagerError::StatePoisoned)?
            .values()
            .map(|entry| entry.info.clone())
            .collect())
    }

    pub async fn remove(
        &self,
        proxy_id: ProxyId,
    ) -> Result<HttpConnectInfo, HttpConnectProxyManagerError> {
        let entry = self
            .inner
            .proxies
            .lock()
            .map_err(|_| HttpConnectProxyManagerError::StatePoisoned)?
            .remove(&proxy_id)
            .ok_or(HttpConnectProxyManagerError::NotFound(proxy_id))?;
        let _ = entry.shutdown.send(true);
        let info = entry.info.clone();
        let _ = timeout(Duration::from_secs(2), entry.task).await;
        Ok(info)
    }
}

async fn serve_http_connect_listener(
    remote: RemoteHub,
    info: HttpConnectInfo,
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
                    serve_http_connect_connection(remote, info, stream, shutdown, permit).await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn serve_http_connect_connection(
    remote: RemoteHub,
    info: HttpConnectInfo,
    mut local: TcpStream,
    mut shutdown: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    let handshake = tokio::select! {
        _ = shutdown.changed() => return,
        result = timeout(HTTP_CONNECT_HANDSHAKE_TIMEOUT, read_http_connect_request(&mut local)) => {
            match result {
                Ok(Ok(handshake)) => handshake,
                Ok(Err(error)) => {
                    let _ = write_http_error(&mut local, error.status()).await;
                    return;
                }
                Err(_) => {
                    let _ = write_http_error(&mut local, HttpStatus::RequestTimeout).await;
                    return;
                }
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
            handshake.destination,
        ) => result,
    };
    let (generation, reply) = match opened {
        Ok(result) => result,
        Err(_) => {
            let _ = write_http_error(&mut local, HttpStatus::BadGateway).await;
            return;
        }
    };
    match reply {
        TcpForwardReply::Opened {
            connection_id: actual,
        } if actual == connection_id => {}
        TcpForwardReply::Error(error) => {
            let _ = write_http_error(&mut local, map_forward_error_to_http(error.code)).await;
            return;
        }
        _ => {
            let _ = write_http_error(&mut local, HttpStatus::BadGateway).await;
            return;
        }
    }
    if local
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || local.flush().await.is_err()
    {
        return;
    }
    pump_forward_connection_with_initial(
        remote,
        info.device_id,
        generation,
        connection_id,
        &mut local,
        shutdown,
        &handshake.initial_tunnel_bytes,
    )
    .await;
}

#[derive(Debug, Eq, PartialEq)]
struct HttpConnectHandshake {
    destination: TcpForwardDestination,
    initial_tunnel_bytes: Vec<u8>,
}

async fn read_http_connect_request<S>(
    stream: &mut S,
) -> Result<HttpConnectHandshake, HttpConnectHandshakeError>
where
    S: AsyncReadExt + Unpin,
{
    let mut received = Vec::with_capacity(1024);
    let mut chunk = [0_u8; HTTP_READ_CHUNK_BYTES];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(HttpConnectHandshakeError::UnexpectedEof);
        }
        received.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_header_end(&received) {
            let end = index + 4;
            if end > HARD_MAX_HTTP_CONNECT_HEADER_BYTES {
                return Err(HttpConnectHandshakeError::HeaderTooLarge);
            }
            break end;
        }
        if received.len() > HARD_MAX_HTTP_CONNECT_HEADER_BYTES {
            return Err(HttpConnectHandshakeError::HeaderTooLarge);
        }
    };

    let initial_tunnel_bytes = received.split_off(header_end);
    let header =
        std::str::from_utf8(&received).map_err(|_| HttpConnectHandshakeError::InvalidEncoding)?;
    if header.as_bytes().contains(&0) {
        return Err(HttpConnectHandshakeError::InvalidRequest);
    }
    let request_line = header
        .split("\r\n")
        .next()
        .ok_or(HttpConnectHandshakeError::InvalidRequest)?;
    let mut parts = request_line.split_ascii_whitespace();
    let method = parts
        .next()
        .ok_or(HttpConnectHandshakeError::InvalidRequest)?;
    let authority = parts
        .next()
        .ok_or(HttpConnectHandshakeError::InvalidRequest)?;
    let version = parts
        .next()
        .ok_or(HttpConnectHandshakeError::InvalidRequest)?;
    if parts.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(HttpConnectHandshakeError::InvalidRequest);
    }
    if method != "CONNECT" {
        return Err(HttpConnectHandshakeError::MethodNotAllowed);
    }
    let destination = parse_authority(authority)?;
    Ok(HttpConnectHandshake {
        destination,
        initial_tunnel_bytes,
    })
}

fn find_header_end(input: &[u8]) -> Option<usize> {
    input.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_authority(authority: &str) -> Result<TcpForwardDestination, HttpConnectHandshakeError> {
    let (host, port_text) = if let Some(rest) = authority.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or(HttpConnectHandshakeError::InvalidAuthority)?;
        let host = &rest[..close];
        let suffix = &rest[close + 1..];
        let port = suffix
            .strip_prefix(':')
            .ok_or(HttpConnectHandshakeError::InvalidAuthority)?;
        if host.is_empty() || port.is_empty() {
            return Err(HttpConnectHandshakeError::InvalidAuthority);
        }
        (host, port)
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or(HttpConnectHandshakeError::InvalidAuthority)?;
        if host.is_empty() || port.is_empty() || host.contains(':') {
            return Err(HttpConnectHandshakeError::InvalidAuthority);
        }
        (host, port)
    };
    if host
        .bytes()
        .any(|byte| matches!(byte, b'/' | b'\\' | b'?' | b'#' | b'@'))
    {
        return Err(HttpConnectHandshakeError::InvalidAuthority);
    }
    let port: u16 = port_text
        .parse()
        .map_err(|_| HttpConnectHandshakeError::InvalidAuthority)?;
    TcpForwardDestination::new(host, port).map_err(|_| HttpConnectHandshakeError::InvalidAuthority)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HttpStatus {
    BadRequest,
    Forbidden,
    MethodNotAllowed,
    RequestTimeout,
    HeaderTooLarge,
    BadGateway,
    ServiceUnavailable,
    GatewayTimeout,
}

impl HttpStatus {
    const fn line(self) -> &'static str {
        match self {
            Self::BadRequest => "HTTP/1.1 400 Bad Request\r\n",
            Self::Forbidden => "HTTP/1.1 403 Forbidden\r\n",
            Self::MethodNotAllowed => "HTTP/1.1 405 Method Not Allowed\r\n",
            Self::RequestTimeout => "HTTP/1.1 408 Request Timeout\r\n",
            Self::HeaderTooLarge => "HTTP/1.1 431 Request Header Fields Too Large\r\n",
            Self::BadGateway => "HTTP/1.1 502 Bad Gateway\r\n",
            Self::ServiceUnavailable => "HTTP/1.1 503 Service Unavailable\r\n",
            Self::GatewayTimeout => "HTTP/1.1 504 Gateway Timeout\r\n",
        }
    }
}

async fn write_http_error<S>(stream: &mut S, status: HttpStatus) -> Result<(), std::io::Error>
where
    S: AsyncWriteExt + Unpin,
{
    stream.write_all(status.line().as_bytes()).await?;
    stream
        .write_all(b"Connection: close\r\nContent-Length: 0\r\n\r\n")
        .await?;
    stream.flush().await
}

fn map_forward_error_to_http(code: TcpForwardErrorCode) -> HttpStatus {
    match code {
        TcpForwardErrorCode::Denied => HttpStatus::Forbidden,
        TcpForwardErrorCode::Timeout => HttpStatus::GatewayTimeout,
        TcpForwardErrorCode::Capacity => HttpStatus::ServiceUnavailable,
        TcpForwardErrorCode::InvalidRequest => HttpStatus::BadRequest,
        TcpForwardErrorCode::ConnectFailed
        | TcpForwardErrorCode::NotFound
        | TcpForwardErrorCode::Io => HttpStatus::BadGateway,
    }
}

#[derive(Debug, Error)]
enum HttpConnectHandshakeError {
    #[error("HTTP CONNECT I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP CONNECT request ended before headers completed")]
    UnexpectedEof,
    #[error("HTTP CONNECT header exceeds its hard bound")]
    HeaderTooLarge,
    #[error("HTTP CONNECT header is not valid UTF-8")]
    InvalidEncoding,
    #[error("HTTP CONNECT request line is invalid")]
    InvalidRequest,
    #[error("HTTP proxy only supports CONNECT")]
    MethodNotAllowed,
    #[error("HTTP CONNECT authority is invalid")]
    InvalidAuthority,
}

impl HttpConnectHandshakeError {
    const fn status(&self) -> HttpStatus {
        match self {
            Self::HeaderTooLarge => HttpStatus::HeaderTooLarge,
            Self::MethodNotAllowed => HttpStatus::MethodNotAllowed,
            Self::Io(_)
            | Self::UnexpectedEof
            | Self::InvalidEncoding
            | Self::InvalidRequest
            | Self::InvalidAuthority => HttpStatus::BadRequest,
        }
    }
}

#[derive(Debug, Error)]
pub enum HttpConnectProxyManagerError {
    #[error("HTTP CONNECT listener I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP CONNECT proxy manager state is poisoned")]
    StatePoisoned,
    #[error("HTTP CONNECT listener capacity is exhausted")]
    Capacity,
    #[error("HTTP CONNECT proxy {0} was not found")]
    NotFound(ProxyId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    #[tokio::test]
    async fn connect_parser_preserves_pipelined_tail_and_rejects_plain_http() {
        let (mut client, mut server) = duplex(4096);
        let server_task = tokio::spawn(async move { read_http_connect_request(&mut server).await });
        client
            .write_all(b"CONNECT localhost:8443 HTTP/1.1\r\nHost: localhost:8443\r\n\r\nTLS-TAIL")
            .await
            .unwrap();
        let handshake = server_task.await.unwrap().unwrap();
        assert_eq!(
            handshake.destination,
            TcpForwardDestination::new("localhost", 8443).unwrap()
        );
        assert_eq!(handshake.initial_tunnel_bytes, b"TLS-TAIL");

        assert_eq!(
            parse_authority("[::1]:443").unwrap(),
            TcpForwardDestination::new("::1", 443).unwrap()
        );

        let (mut client, mut server) = duplex(1024);
        let server_task = tokio::spawn(async move { read_http_connect_request(&mut server).await });
        client
            .write_all(b"GET http://example/ HTTP/1.1\r\nHost: example\r\n\r\n")
            .await
            .unwrap();
        assert!(matches!(
            server_task.await.unwrap(),
            Err(HttpConnectHandshakeError::MethodNotAllowed)
        ));
    }

    #[tokio::test]
    async fn manager_is_loopback_owned_and_returns_method_error_until_remove() {
        let manager = HttpConnectProxyManager::new(RemoteHub::default());
        let info = manager.add(DeviceId::new(), 0).await.unwrap();
        assert!(info.listen_addr.ip().is_loopback());
        assert_ne!(info.listen_addr.port(), 0);
        assert_eq!(manager.list().unwrap(), vec![info.clone()]);

        let mut client = TcpStream::connect(info.listen_addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));

        manager.remove(info.proxy_id).await.unwrap();
        assert!(manager.list().unwrap().is_empty());
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(TcpStream::connect(info.listen_addr).await.is_err());
    }
}
