//! Endpoint transport and frame I/O (feature `runtime`).
//!
//! The endpoint is a Windows named pipe (`\\.\pipe\...`) or a Unix domain
//! socket path. Both are reached through tokio; no `unsafe` is used.

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::ipc::{self, FrameError};

/// A connected byte stream to the peer endpoint.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> Stream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

fn protocol_error(_: FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid IPC frame")
}

/// Writes one frame and flushes.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + ?Sized,
{
    let bytes = ipc::encode_frame(value).map_err(protocol_error)?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Reads and parses one frame. Lengths of 0 or above 262144 are refused before
/// the payload is read.
pub async fn read_message<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin + ?Sized,
    T: DeserializeOwned,
{
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).await?;
    let len = ipc::decode_len(header).map_err(protocol_error)?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    ipc::decode_payload(&payload).map_err(protocol_error)
}

/// Connects to the endpoint. Windows retries while every pipe instance is busy
/// (`ERROR_PIPE_BUSY`); the caller bounds the wait with a timeout.
#[cfg(windows)]
pub async fn connect(path: &str) -> io::Result<Box<dyn Stream>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(path) {
            Ok(client) => return Ok(Box::new(client)),
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Connects to the endpoint (Unix domain socket).
#[cfg(unix)]
pub async fn connect(path: &str) -> io::Result<Box<dyn Stream>> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Box::new(stream))
}

/// Accepts connections on an endpoint.
#[cfg(windows)]
#[derive(Debug)]
pub struct Listener {
    path: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}

#[cfg(windows)]
impl Listener {
    /// Creates the first pipe instance. Fails if the name is already served.
    pub fn bind(path: &str) -> io::Result<Self> {
        use tokio::net::windows::named_pipe::ServerOptions;

        let next = ServerOptions::new()
            .first_pipe_instance(true)
            .create(path)?;
        Ok(Self {
            path: path.to_owned(),
            next,
        })
    }

    /// Waits for the next client and returns its stream.
    pub async fn accept(&mut self) -> io::Result<Box<dyn Stream>> {
        use tokio::net::windows::named_pipe::ServerOptions;

        self.next.connect().await?;
        let fresh = ServerOptions::new().create(&self.path)?;
        let connected = std::mem::replace(&mut self.next, fresh);
        Ok(Box::new(connected))
    }
}

/// Accepts connections on an endpoint.
#[cfg(unix)]
#[derive(Debug)]
pub struct Listener {
    inner: tokio::net::UnixListener,
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Listener {
    /// Binds the socket and restricts it to the owner (mode 0600).
    pub fn bind(path: &str) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let inner = tokio::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            inner,
            path: std::path::PathBuf::from(path),
        })
    }

    /// Waits for the next client and returns its stream.
    pub async fn accept(&mut self) -> io::Result<Box<dyn Stream>> {
        let (stream, _) = self.inner.accept().await?;
        Ok(Box::new(stream))
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
