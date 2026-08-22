//! Named-pipe transport used on Windows in place of Unix sockets.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions};
use tracing::{error, trace};

use crate::{BufferedReader, ConnectError, pipe_name_from_path};

pub fn ipc_endpoint_exists(socket: impl AsRef<Path>) -> bool {
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::core::HSTRING;

    let name = HSTRING::from(pipe_name_from_path(&socket).as_str());
    unsafe { WaitNamedPipeW(&name, 0) }.is_ok()
}

pub async fn validate_socket(_socket: impl AsRef<Path>) -> Result<(), ConnectError> {
    Ok(())
}

const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const CONNECT_BUDGET: Duration = Duration::from_secs(5);

fn named_pipe_connect_retryable(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(ERROR_PIPE_BUSY) | Some(ERROR_FILE_NOT_FOUND))
}

pub async fn socket_connect(socket_path: impl AsRef<Path>) -> Result<IpcStream, ConnectError> {
    let name = pipe_name_from_path(&socket_path);
    let deadline = Instant::now() + CONNECT_BUDGET;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => {
                trace!(%name, "Connected named pipe");
                return Ok(IpcStream::Client(client));
            },
            Err(err) if named_pipe_connect_retryable(&err) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            },
            Err(err) if named_pipe_connect_retryable(&err) => {
                return Err(ConnectError::Timeout);
            },
            Err(err) => {
                error!(%err, %name, "Failed to connect named pipe");
                return Err(err.into());
            },
        }
    }
}

pub async fn socket_connect_timeout(socket: impl AsRef<Path>, timeout: Duration) -> Result<IpcStream, ConnectError> {
    match tokio::time::timeout(timeout, socket_connect(&socket)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(ConnectError::Timeout),
    }
}

pub type BufferedUnixStream = BufferedReader<IpcStream>;

pub enum IpcStream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

impl AsyncRead for IpcStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(inner) => Pin::new(inner).poll_read(cx, buf),
            Self::Server(inner) => Pin::new(inner).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Client(inner) => Pin::new(inner).poll_write(cx, buf),
            Self::Server(inner) => Pin::new(inner).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(inner) => Pin::new(inner).poll_flush(cx),
            Self::Server(inner) => Pin::new(inner).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(inner) => Pin::new(inner).poll_shutdown(cx),
            Self::Server(inner) => Pin::new(inner).poll_shutdown(cx),
        }
    }
}

pub struct LocalListener {
    name: String,
    server: NamedPipeServer,
}

impl LocalListener {
    pub async fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let name = pipe_name_from_path(path);
        // Unlike the Unix socket bind, there is no file to unlink. Named pipes
        // vanish when the last handle closes. `first_pipe_instance` fails if a
        // previous server is still alive — that is the Windows equivalent of
        // EADDRINUSE.
        let server = ServerOptions::new().first_pipe_instance(true).create(&name)?;
        Ok(Self { name, server })
    }

    pub async fn accept(&mut self) -> io::Result<BufferedUnixStream> {
        self.server.connect().await?;
        // Recreate the next instance before returning so a client that races
        // into FILE_NOT_FOUND can retry (see `named_pipe_connect_retryable`).
        let next = ServerOptions::new().create(&self.name)?;
        let connected = std::mem::replace(&mut self.server, next);
        Ok(BufferedUnixStream::new(IpcStream::Server(connected)))
    }
}

impl BufferedUnixStream {
    pub async fn connect(socket: impl AsRef<Path>) -> Result<Self, ConnectError> {
        Ok(Self::new(socket_connect(socket).await?))
    }

    pub async fn connect_timeout(socket: impl AsRef<Path>, timeout: Duration) -> Result<Self, ConnectError> {
        Ok(Self::new(socket_connect_timeout(socket, timeout).await?))
    }
}

#[cfg(all(test, windows))]
mod listener_tests {
    use super::*;

    #[tokio::test]
    async fn accept_then_client_connects() {
        let path = std::env::temp_dir().join(format!("ec-ipc-{}-desktop.sock", std::process::id()));
        let mut listener = LocalListener::bind(&path).await.expect("bind");
        assert!(ipc_endpoint_exists(&path));
        let path_for_client = path.clone();
        let client = tokio::spawn(async move { socket_connect(path_for_client).await });
        let _server = listener.accept().await.expect("accept");
        client.await.expect("join").expect("connect");
    }
}
