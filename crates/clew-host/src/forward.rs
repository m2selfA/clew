use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use clew_core::{ForwardConnectionId, ForwardId};
use clew_transport::{
    HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION, TcpForwardDestination, TcpForwardErrorCode,
    TcpForwardReply, TcpForwardRequest,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore},
    time::timeout,
};

#[derive(Clone)]
pub struct HostTcpForwardService {
    inner: Arc<StdMutex<HostTcpForwardState>>,
    capacity: Arc<Semaphore>,
}

impl Default for HostTcpForwardService {
    fn default() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(HostTcpForwardState::default())),
            capacity: Arc::new(Semaphore::new(HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION)),
        }
    }
}

#[derive(Default)]
struct HostTcpForwardState {
    connections: BTreeMap<ForwardConnectionId, HostTcpForwardEntry>,
}

struct HostTcpForwardEntry {
    forward_id: ForwardId,
    destination: TcpForwardDestination,
    io: Arc<AsyncMutex<HostTcpForwardConnection>>,
    _capacity_permit: OwnedSemaphorePermit,
}

struct HostTcpForwardConnection {
    stream: TcpStream,
    write_closed: bool,
}

impl std::fmt::Debug for HostTcpForwardService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .inner
            .lock()
            .map(|state| state.connections.len())
            .unwrap_or(0);
        formatter
            .debug_struct("HostTcpForwardService")
            .field("connection_count", &count)
            .finish()
    }
}

impl HostTcpForwardService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn execute(
        &self,
        request: TcpForwardRequest,
        allow_tcp_egress: bool,
    ) -> TcpForwardReply {
        if request.validate().is_err() {
            return TcpForwardReply::error(
                TcpForwardErrorCode::InvalidRequest,
                "invalid bounded TCP forward request",
            );
        }
        if !allow_tcp_egress {
            return TcpForwardReply::error(
                TcpForwardErrorCode::Denied,
                "TCP egress is not permitted by this device grant",
            );
        }

        let exchange_write = if matches!(request, TcpForwardRequest::Exchange { .. }) {
            match request.write_bytes() {
                Ok(write) => Some(write),
                Err(_) => {
                    return TcpForwardReply::error(
                        TcpForwardErrorCode::InvalidRequest,
                        "invalid TCP forward data encoding",
                    );
                }
            }
        } else {
            None
        };

        match request {
            TcpForwardRequest::Open {
                forward_id,
                connection_id,
                destination,
                connect_timeout_ms,
            } => {
                {
                    let state = match self.inner.lock() {
                        Ok(state) => state,
                        Err(_) => {
                            return TcpForwardReply::error(
                                TcpForwardErrorCode::Io,
                                "TCP forward state is unavailable",
                            );
                        }
                    };
                    if let Some(existing) = state.connections.get(&connection_id) {
                        return if existing.forward_id == forward_id
                            && existing.destination == destination
                        {
                            TcpForwardReply::Opened { connection_id }
                        } else {
                            TcpForwardReply::error(
                                TcpForwardErrorCode::InvalidRequest,
                                "TCP forward connection id is already bound to another destination",
                            )
                        };
                    }
                }
                let capacity_permit = match Arc::clone(&self.capacity).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Capacity,
                            "TCP forward session connection capacity is exhausted",
                        );
                    }
                };

                let connect = TcpStream::connect((destination.host.as_str(), destination.port));
                let stream = match timeout(
                    Duration::from_millis(connect_timeout_ms as u64),
                    connect,
                )
                .await
                {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(_)) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::ConnectFailed,
                            "TCP destination connection failed",
                        );
                    }
                    Err(_) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Timeout,
                            "TCP destination connection timed out",
                        );
                    }
                };
                let _ = stream.set_nodelay(true);
                let entry = HostTcpForwardEntry {
                    forward_id,
                    destination,
                    io: Arc::new(AsyncMutex::new(HostTcpForwardConnection {
                        stream,
                        write_closed: false,
                    })),
                    _capacity_permit: capacity_permit,
                };
                let mut state = match self.inner.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Io,
                            "TCP forward state is unavailable",
                        );
                    }
                };
                if let Some(existing) = state.connections.get(&connection_id) {
                    return if existing.forward_id == entry.forward_id
                        && existing.destination == entry.destination
                    {
                        TcpForwardReply::Opened { connection_id }
                    } else {
                        TcpForwardReply::error(
                            TcpForwardErrorCode::InvalidRequest,
                            "TCP forward connection id is already bound to another destination",
                        )
                    };
                }
                state.connections.insert(connection_id, entry);
                TcpForwardReply::Opened { connection_id }
            }
            TcpForwardRequest::Exchange {
                connection_id,
                write_eof,
                max_read_bytes,
                read_wait_ms,
                ..
            } => {
                let io = {
                    let state = match self.inner.lock() {
                        Ok(state) => state,
                        Err(_) => {
                            return TcpForwardReply::error(
                                TcpForwardErrorCode::Io,
                                "TCP forward state is unavailable",
                            );
                        }
                    };
                    let Some(entry) = state.connections.get(&connection_id) else {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::NotFound,
                            "TCP forward connection was not found",
                        );
                    };
                    Arc::clone(&entry.io)
                };
                let write = exchange_write.expect("exchange payload decoded before dispatch");
                let mut io = io.lock().await;
                if !write.is_empty() {
                    if io.write_closed {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::InvalidRequest,
                            "TCP forward write half is already closed",
                        );
                    }
                    if io.stream.write_all(&write).await.is_err() {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Io,
                            "TCP forward destination write failed",
                        );
                    }
                }
                if write_eof && !io.write_closed {
                    if io.stream.shutdown().await.is_err() {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Io,
                            "TCP forward destination half-close failed",
                        );
                    }
                    io.write_closed = true;
                }
                let mut read = vec![0_u8; max_read_bytes as usize];
                let (read_len, read_eof) = match timeout(
                    Duration::from_millis(read_wait_ms as u64),
                    io.stream.read(&mut read),
                )
                .await
                {
                    Err(_) => (0, false),
                    Ok(Ok(0)) => (0, true),
                    Ok(Ok(read_len)) => (read_len, false),
                    Ok(Err(_)) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Io,
                            "TCP forward destination read failed",
                        );
                    }
                };
                read.truncate(read_len);
                TcpForwardReply::exchanged(connection_id, &read, read_eof).unwrap_or_else(|_| {
                    TcpForwardReply::error(
                        TcpForwardErrorCode::Io,
                        "TCP forward result bound failed",
                    )
                })
            }
            TcpForwardRequest::Close { connection_id } => {
                let removed = match self.inner.lock() {
                    Ok(mut state) => state.connections.remove(&connection_id).is_some(),
                    Err(_) => {
                        return TcpForwardReply::error(
                            TcpForwardErrorCode::Io,
                            "TCP forward state is unavailable",
                        );
                    }
                };
                if removed {
                    TcpForwardReply::Closed { connection_id }
                } else {
                    TcpForwardReply::error(
                        TcpForwardErrorCode::NotFound,
                        "TCP forward connection was not found",
                    )
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_transport::HARD_MAX_TCP_FORWARD_CHUNK_BYTES;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn host_tcp_forward_is_grant_gated_bounded_and_bidirectional() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let service = HostTcpForwardService::new();
        let forward_id = ForwardId::new();
        let denied_id = ForwardConnectionId::new();
        let denied = service
            .execute(
                TcpForwardRequest::open(
                    forward_id,
                    denied_id,
                    TcpForwardDestination::new("127.0.0.1", addr.port()).unwrap(),
                    1_000,
                )
                .unwrap(),
                false,
            )
            .await;
        assert!(matches!(
            denied,
            TcpForwardReply::Error(error) if error.code == TcpForwardErrorCode::Denied
        ));

        let connection_id = ForwardConnectionId::new();
        assert_eq!(
            service
                .execute(
                    TcpForwardRequest::open(
                        forward_id,
                        connection_id,
                        TcpForwardDestination::new("127.0.0.1", addr.port()).unwrap(),
                        1_000,
                    )
                    .unwrap(),
                    true,
                )
                .await,
            TcpForwardReply::Opened { connection_id }
        );
        let exchange = service
            .execute(
                TcpForwardRequest::exchange(
                    connection_id,
                    b"ping",
                    false,
                    HARD_MAX_TCP_FORWARD_CHUNK_BYTES.min(4),
                    250,
                )
                .unwrap(),
                true,
            )
            .await;
        let TcpForwardReply::Exchanged {
            connection_id: actual,
            read_eof,
            ..
        } = &exchange
        else {
            panic!("expected TCP exchange reply");
        };
        assert_eq!(*actual, connection_id);
        assert!(!read_eof);
        assert_eq!(exchange.read_bytes().unwrap(), b"pong");
        assert_eq!(
            service
                .execute(TcpForwardRequest::Close { connection_id }, true)
                .await,
            TcpForwardReply::Closed { connection_id }
        );
        echo.await.unwrap();
    }

    #[tokio::test]
    async fn host_tcp_forward_reserves_capacity_before_outbound_connect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepted_for_server = Arc::clone(&accepted);
        let server = tokio::spawn(async move {
            let mut streams = Vec::new();
            for _ in 0..HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION {
                let (stream, _) = listener.accept().await.unwrap();
                accepted_for_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                streams.push(stream);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            streams
        });

        let service = HostTcpForwardService::new();
        let forward_id = ForwardId::new();
        for _ in 0..HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION {
            let connection_id = ForwardConnectionId::new();
            assert!(matches!(
                service
                    .execute(
                        TcpForwardRequest::open(
                            forward_id,
                            connection_id,
                            TcpForwardDestination::new("127.0.0.1", addr.port()).unwrap(),
                            1_000,
                        )
                        .unwrap(),
                        true,
                    )
                    .await,
                TcpForwardReply::Opened { .. }
            ));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while accepted.load(std::sync::atomic::Ordering::SeqCst)
                < HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("server did not accept all admitted TCP connections");

        let overflow = service
            .execute(
                TcpForwardRequest::open(
                    forward_id,
                    ForwardConnectionId::new(),
                    TcpForwardDestination::new("127.0.0.1", addr.port()).unwrap(),
                    1_000,
                )
                .unwrap(),
                true,
            )
            .await;
        assert!(matches!(
            overflow,
            TcpForwardReply::Error(error) if error.code == TcpForwardErrorCode::Capacity
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            HARD_MAX_TCP_FORWARD_CONNECTIONS_PER_SESSION,
            "capacity rejection must happen before opening a 65th outbound socket"
        );
        server.abort();
        let _ = server.await;
    }
}
