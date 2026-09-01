use std::{io, time::Duration};

use tokio::time::{Instant, sleep};

use crate::LocalEndpoint;

const CONNECT_RETRY_WINDOW: Duration = Duration::from_secs(5);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);

#[cfg(windows)]
mod platform {
    use std::io;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    use super::LocalEndpoint;

    pub(crate) type LocalStream = NamedPipeServer;
    pub(crate) type LocalClientStream = NamedPipeClient;

    pub(crate) struct LocalListener {
        name: String,
        next: NamedPipeServer,
    }

    impl LocalListener {
        pub(crate) fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
            let LocalEndpoint::WindowsNamedPipe(name) = endpoint;
            let next = make_server(name, true)?;
            Ok(Self {
                name: name.clone(),
                next,
            })
        }

        pub(crate) async fn accept(&mut self) -> io::Result<LocalStream> {
            self.next.connect().await?;
            let replacement = make_server(&self.name, false)?;
            Ok(std::mem::replace(&mut self.next, replacement))
        }
    }

    fn make_server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options.reject_remote_clients(true).max_instances(32);
        if first {
            options.first_pipe_instance(true);
        }
        options.create(name)
    }

    pub(crate) async fn try_connect(endpoint: &LocalEndpoint) -> io::Result<LocalClientStream> {
        let LocalEndpoint::WindowsNamedPipe(name) = endpoint;
        ClientOptions::new().open(name)
    }
}

#[cfg(unix)]
mod platform {
    use std::{fs, io};

    use tokio::net::{UnixListener, UnixStream};

    use super::LocalEndpoint;

    pub(crate) type LocalStream = UnixStream;
    pub(crate) type LocalClientStream = UnixStream;

    pub(crate) struct LocalListener {
        listener: UnixListener,
        path: std::path::PathBuf,
    }

    impl LocalListener {
        pub(crate) fn bind(endpoint: &LocalEndpoint) -> io::Result<Self> {
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

        pub(crate) async fn accept(&mut self) -> io::Result<LocalStream> {
            self.listener.accept().await.map(|(stream, _)| stream)
        }
    }

    impl Drop for LocalListener {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    pub(crate) async fn try_connect(endpoint: &LocalEndpoint) -> io::Result<LocalClientStream> {
        let LocalEndpoint::UnixSocket(path) = endpoint;
        UnixStream::connect(path).await
    }
}

pub(crate) use platform::{LocalClientStream, LocalListener, LocalStream};

pub(crate) async fn connect(endpoint: &LocalEndpoint) -> io::Result<LocalClientStream> {
    let deadline = Instant::now() + CONNECT_RETRY_WINDOW;
    loop {
        match platform::try_connect(endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                sleep(CONNECT_RETRY_DELAY).await;
                if Instant::now() >= deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}
