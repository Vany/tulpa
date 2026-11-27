use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, join};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// A `ProxyCommand` child's bidirectional standard I/O transport.
pub struct ProxyTransport {
    _child: Child,
    stream: tokio::io::Join<ChildStdout, ChildStdin>,
}

impl ProxyTransport {
    /// Runs `command` through `/bin/sh` with piped input/output and inherited stderr.
    ///
    /// # Errors
    ///
    /// Returns an error when the shell or proxy process cannot be spawned.
    pub fn spawn(command: &str) -> Result<Self, TransportError> {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| TransportError::new(format!("ProxyCommand spawn failed: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::new("ProxyCommand stdout is unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::new("ProxyCommand stdin is unavailable"))?;
        Ok(Self {
            _child: child,
            stream: join(stdout, stdin),
        })
    }
}

impl fmt::Debug for ProxyTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProxyTransport { child: running }")
    }
}

impl AsyncRead for ProxyTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl AsyncWrite for ProxyTransport {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(context, data)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

/// A `ProxyCommand` process setup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct TransportError(String);

impl TransportError {
    /// Creates an error containing no secret values.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TransportError {}
