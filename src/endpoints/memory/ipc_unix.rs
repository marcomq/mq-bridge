//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

#![cfg(unix)]

use super::framed::{self, FramedIo};
use super::transport::TransportChannel;
use crate::CanonicalMessage;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Which end of the socket this transport owns.
///
/// The socket is unidirectional in practice: the consumer binds and reads, the
/// publisher connects and writes. Recording the role lets a misdirected send
/// fail loudly instead of writing into a peer that never reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Consumer side: binds the socket and accepts connections.
    Server,
    /// Publisher side: connects and sends.
    Client,
}

/// Unix Domain Socket transport for local IPC
#[derive(Clone)]
pub struct UnixIpcTransport {
    inner: Arc<UnixIpcTransportInner>,
}

struct UnixIpcTransportInner {
    socket_path: String,
    capacity: usize,
    role: Role,
    // Server mode only: accepts incoming connections.
    listener: Mutex<Option<UnixListener>>,
    // The active connection, framed. Server-side this is empty until the first
    // accept. The codec owns the partial-frame buffer, which is what makes
    // reads cancel safe.
    conn: Mutex<Option<FramedIo<UnixStream>>>,
    closed: Mutex<bool>,
}

impl UnixIpcTransport {
    /// Create a new Unix IPC transport as a server (consumer side)
    pub async fn new_server(socket_path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        let socket_path = socket_path.as_ref();

        // Remove existing socket if it exists
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }

        // Create parent directory if needed
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
            // Set restrictive permissions on directory (0700)
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(parent)?.permissions();
                perms.set_mode(0o700);
                std::fs::set_permissions(parent, perms)?;
            }
        }

        let listener = UnixListener::bind(socket_path)?;

        // Set restrictive permissions on socket (0600)
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(socket_path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(socket_path, perms)?;
        }

        info!(path = %socket_path.display(), "Unix IPC server listening");

        Ok(Self {
            inner: Arc::new(UnixIpcTransportInner {
                socket_path: socket_path.to_string_lossy().to_string(),
                capacity,
                role: Role::Server,
                listener: Mutex::new(Some(listener)),
                conn: Mutex::new(None),
                closed: Mutex::new(false),
            }),
        })
    }

    /// Create a new Unix IPC transport as a client (publisher side)
    pub async fn new_client(socket_path: impl AsRef<Path>, capacity: usize) -> Result<Self> {
        let socket_path = socket_path.as_ref();

        let stream = UnixStream::connect(socket_path).await?;

        info!(path = %socket_path.display(), "Unix IPC client connected");

        Ok(Self {
            inner: Arc::new(UnixIpcTransportInner {
                socket_path: socket_path.to_string_lossy().to_string(),
                capacity,
                role: Role::Client,
                listener: Mutex::new(None),
                conn: Mutex::new(Some(framed::wrap(stream))),
                closed: Mutex::new(false),
            }),
        })
    }

    /// Accept a connection (server mode)
    async fn accept_connection(&self) -> Result<UnixStream> {
        let listener_guard = self.inner.listener.lock().await;
        if let Some(listener) = listener_guard.as_ref() {
            let (stream, _addr) = listener.accept().await?;
            debug!(path = %self.inner.socket_path, "Accepted Unix IPC connection");
            Ok(stream)
        } else {
            Err(anyhow!("Unix IPC transport not in server mode"))
        }
    }

    fn is_disconnected(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                )
            })
    }
}

#[async_trait]
impl TransportChannel for UnixIpcTransport {
    async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> Result<()> {
        if *self.inner.closed.lock().await {
            return Err(anyhow!("Unix IPC transport is closed"));
        }

        // A server writing here would push bytes at a publisher that only ever
        // sends, silently stranding them and eventually blocking on a full
        // socket buffer. Refuse instead.
        if self.inner.role == Role::Server {
            return Err(anyhow!(
                "Unix IPC transport at '{}' is the consumer (server) side and cannot send; \
                 the socket carries publisher -> consumer traffic only",
                self.inner.socket_path
            ));
        }

        let mut conn_guard = self.inner.conn.lock().await;
        if let Some(conn) = conn_guard.as_mut() {
            let bytes = framed::send_batch(conn, &messages, &self.inner.socket_path).await?;
            debug!(
                path = %self.inner.socket_path,
                count = messages.len(),
                bytes,
                "Sent batch via Unix IPC"
            );
            Ok(())
        } else {
            Err(anyhow!("Unix IPC transport has no active connection"))
        }
    }

    async fn recv_batch(&self) -> Result<Vec<CanonicalMessage>> {
        loop {
            if *self.inner.closed.lock().await {
                return Err(anyhow!("Unix IPC transport is closed"));
            }

            let mut conn_guard = self.inner.conn.lock().await;
            if conn_guard.is_none() {
                drop(conn_guard);
                let stream = self.accept_connection().await?;
                conn_guard = self.inner.conn.lock().await;
                *conn_guard = Some(framed::wrap(stream));
            }

            let read_result = if let Some(conn) = conn_guard.as_mut() {
                framed::recv_batch(conn).await
            } else {
                Err(anyhow!("Unix IPC transport has no active connection"))
            };

            match read_result {
                Ok(messages) => {
                    debug!(
                        path = %self.inner.socket_path,
                        count = messages.len(),
                        "Received batch via Unix IPC"
                    );
                    return Ok(messages);
                }
                Err(error) if Self::is_disconnected(&error) => {
                    warn!(path = %self.inner.socket_path, error = %error, "Unix IPC peer disconnected; waiting for a new connection");
                    *conn_guard = None;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn try_recv_batch(&self) -> Result<Option<Vec<CanonicalMessage>>> {
        // Sync context: if the connection is busy or absent there is nothing we
        // can produce without blocking.
        let Ok(mut conn_guard) = self.inner.conn.try_lock() else {
            return Ok(None);
        };
        let Some(conn) = conn_guard.as_mut() else {
            return Ok(None);
        };
        framed::try_recv_batch(conn)
    }

    fn len(&self) -> usize {
        // Whole frames already buffered by the codec, i.e. readable without IO.
        self.inner
            .conn
            .try_lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(framed::buffered_frames))
            .unwrap_or(0)
    }

    fn capacity(&self) -> Option<usize> {
        Some(self.inner.capacity)
    }

    fn is_closed(&self) -> bool {
        // This is a blocking check, but should be fast
        if let Ok(closed) = self.inner.closed.try_lock() {
            *closed
        } else {
            false
        }
    }

    fn close(&self) {
        if let Ok(mut closed) = self.inner.closed.try_lock() {
            *closed = true;
            info!(path = %self.inner.socket_path, "Closing Unix IPC transport");
        }
    }
}

impl Drop for UnixIpcTransport {
    fn drop(&mut self) {
        // Clean up socket file if we're the server
        if let Ok(listener_guard) = self.inner.listener.try_lock() {
            if listener_guard.is_some() {
                let socket_path = &self.inner.socket_path;
                if let Err(e) = std::fs::remove_file(socket_path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        warn!(path = %socket_path, error = %e, "Failed to remove Unix socket file");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_unix_ipc_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Create server
        let server = UnixIpcTransport::new_server(&socket_path, 10)
            .await
            .unwrap();

        // Create client
        let client = UnixIpcTransport::new_client(&socket_path, 10)
            .await
            .unwrap();

        // Send from client
        let msg = CanonicalMessage::from_vec(b"test");
        client.send_batch(vec![msg.clone()]).await.unwrap();

        // Receive on server
        let received = server.recv_batch().await.unwrap();
        assert_eq!(received.len(), 1);
    }

    #[tokio::test]
    async fn test_unix_ipc_close() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = UnixIpcTransport::new_server(&socket_path, 10)
            .await
            .unwrap();

        assert!(!server.is_closed());
        server.close();
        assert!(server.is_closed());
    }

    /// The server end must refuse to send rather than strand messages in a
    /// publisher's receive buffer.
    #[tokio::test]
    async fn test_unix_ipc_server_cannot_send() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = UnixIpcTransport::new_server(&socket_path, 10)
            .await
            .unwrap();
        let _client = UnixIpcTransport::new_client(&socket_path, 10)
            .await
            .unwrap();

        let err = server
            .send_batch(vec![CanonicalMessage::from_vec(b"nope")])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot send"),
            "unexpected error: {err}"
        );
    }

    /// Cancelling a read mid-frame must not desync the connection.
    #[tokio::test]
    async fn test_unix_ipc_cancelled_receive_loses_nothing() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("cancel.sock");

        let server = UnixIpcTransport::new_server(&socket_path, 10)
            .await
            .unwrap();
        let client = UnixIpcTransport::new_client(&socket_path, 10)
            .await
            .unwrap();

        // Cancel a receive that has nothing to read yet.
        tokio::select! {
            _ = server.recv_batch() => panic!("nothing has been sent yet"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        client
            .send_batch(vec![CanonicalMessage::from_vec(b"after-cancel")])
            .await
            .unwrap();

        let received = server.recv_batch().await.unwrap();
        assert_eq!(received[0].payload.as_ref(), b"after-cancel");
    }

    #[tokio::test]
    async fn test_unix_ipc_try_recv_and_len() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("try.sock");

        let server = UnixIpcTransport::new_server(&socket_path, 10)
            .await
            .unwrap();
        let client = UnixIpcTransport::new_client(&socket_path, 10)
            .await
            .unwrap();

        // No connection accepted yet, so nothing is available.
        assert!(server.try_recv_batch().unwrap().is_none());
        assert_eq!(server.len(), 0);

        client
            .send_batch(vec![CanonicalMessage::from_vec(b"first")])
            .await
            .unwrap();

        // The first recv accepts the connection and reads the frame.
        let received = server.recv_batch().await.unwrap();
        assert_eq!(received[0].payload.as_ref(), b"first");

        client
            .send_batch(vec![CanonicalMessage::from_vec(b"second")])
            .await
            .unwrap();
        // Give the write time to land in the receive buffer.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let polled = server
            .try_recv_batch()
            .unwrap()
            .expect("a frame should be readable without blocking");
        assert_eq!(polled[0].payload.as_ref(), b"second");
    }
}
