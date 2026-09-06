//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

#![cfg(windows)]

use super::framed::{self, FramedIo};
use super::transport::TransportChannel;
use crate::CanonicalMessage;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Server-side pipe state.
///
/// A named pipe must be connected before it can be framed, so the raw handle is
/// held until the first client arrives.
enum ServerState {
    /// Created, waiting for a client.
    Idle(NamedPipeServer),
    /// Connected and wrapped in the codec.
    Connected(FramedIo<NamedPipeServer>),
    /// Transient state while transitioning.
    Empty,
}

/// Windows Named Pipe transport for local IPC
#[derive(Clone)]
pub struct WindowsIpcTransport {
    inner: Arc<WindowsIpcTransportInner>,
}

struct WindowsIpcTransportInner {
    pipe_name: String,
    capacity: usize,
    // Consumer side.
    server: Mutex<ServerState>,
    // Publisher side.
    client: Mutex<Option<FramedIo<NamedPipeClient>>>,
    closed: AtomicBool,
}

impl WindowsIpcTransport {
    /// Create a new Windows Named Pipe transport as a server (consumer side)
    pub async fn new_server(pipe_name: impl AsRef<str>, capacity: usize) -> Result<Self> {
        let pipe_name = pipe_name.as_ref();
        let full_path = format!(r"\\.\pipe\{}", pipe_name);

        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&full_path)?;

        info!(pipe = %full_path, "Windows Named Pipe server created");

        Ok(Self {
            inner: Arc::new(WindowsIpcTransportInner {
                pipe_name: full_path,
                capacity,
                server: Mutex::new(ServerState::Idle(server)),
                client: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Create a new Windows Named Pipe transport as a client (publisher side)
    pub async fn new_client(pipe_name: impl AsRef<str>, capacity: usize) -> Result<Self> {
        let pipe_name = pipe_name.as_ref();
        let full_path = format!(r"\\.\pipe\{}", pipe_name);

        let client = ClientOptions::new().open(&full_path)?;

        info!(pipe = %full_path, "Windows Named Pipe client connected");

        Ok(Self {
            inner: Arc::new(WindowsIpcTransportInner {
                pipe_name: full_path,
                capacity,
                server: Mutex::new(ServerState::Empty),
                client: Mutex::new(Some(framed::wrap(client))),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Wait for a client connection (server mode)
    pub async fn wait_for_connection(&self) -> Result<()> {
        let mut guard = self.inner.server.lock().await;
        match std::mem::replace(&mut *guard, ServerState::Empty) {
            ServerState::Connected(conn) => {
                *guard = ServerState::Connected(conn);
                Ok(())
            }
            ServerState::Idle(server) => {
                match server.connect().await {
                    Ok(()) => {
                        debug!(pipe = %self.inner.pipe_name, "Client connected to Named Pipe");
                        *guard = ServerState::Connected(framed::wrap(server));
                        Ok(())
                    }
                    Err(e) => {
                        // Keep the handle so a later attempt can retry.
                        *guard = ServerState::Idle(server);
                        Err(e.into())
                    }
                }
            }
            ServerState::Empty => Err(anyhow!(
                "Windows Named Pipe transport not in server mode: {}",
                self.inner.pipe_name
            )),
        }
    }

    /// Whether a read error means the peer hung up (so we should re-accept) rather than a
    /// fatal fault. Mirrors `is_disconnected` in ipc_unix.rs.
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
impl TransportChannel for WindowsIpcTransport {
    async fn send_batch(&self, messages: Vec<CanonicalMessage>) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(anyhow!("Windows Named Pipe transport is closed"));
        }

        // Only the publisher side sends; see the matching guard in ipc_unix.rs.
        let mut client_guard = self.inner.client.lock().await;
        if let Some(conn) = client_guard.as_mut() {
            let bytes = framed::send_batch(conn, &messages, &self.inner.pipe_name).await?;
            debug!(
                pipe = %self.inner.pipe_name,
                count = messages.len(),
                bytes,
                "Sent batch via Named Pipe"
            );
            Ok(())
        } else {
            Err(anyhow!(
                "Windows Named Pipe transport at '{}' is the consumer (server) side and cannot \
                 send; the pipe carries publisher -> consumer traffic only",
                self.inner.pipe_name
            ))
        }
    }

    async fn recv_batch(&self) -> Result<Vec<CanonicalMessage>> {
        loop {
            if self.inner.closed.load(Ordering::SeqCst) {
                return Err(anyhow!("Windows Named Pipe transport is closed"));
            }

            // Wait for connection if we haven't connected yet (server mode)
            self.wait_for_connection().await?;

            let read_result = {
                let mut guard = self.inner.server.lock().await;
                let ServerState::Connected(conn) = &mut *guard else {
                    return Err(anyhow!(
                        "Windows Named Pipe transport not in server mode: {}",
                        self.inner.pipe_name
                    ));
                };
                framed::recv_batch(conn).await
            };

            match read_result {
                Ok(messages) => {
                    debug!(
                        pipe = %self.inner.pipe_name,
                        count = messages.len(),
                        "Received batch via Named Pipe"
                    );
                    return Ok(messages);
                }
                // The client went away. Drop the spent handle and stand up a fresh pipe
                // instance so the next publisher reconnect is served, mirroring ipc_unix.rs.
                Err(error) if Self::is_disconnected(&error) => {
                    warn!(pipe = %self.inner.pipe_name, error = %error, "Named Pipe peer disconnected; waiting for a new connection");
                    let mut guard = self.inner.server.lock().await;
                    match ServerOptions::new().create(&self.inner.pipe_name) {
                        Ok(server) => *guard = ServerState::Idle(server),
                        Err(e) => {
                            *guard = ServerState::Empty;
                            return Err(anyhow!(
                                "Failed to recreate Named Pipe instance at '{}': {}",
                                self.inner.pipe_name,
                                e
                            ));
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn try_recv_batch(&self) -> Result<Option<Vec<CanonicalMessage>>> {
        let Ok(mut guard) = self.inner.server.try_lock() else {
            return Ok(None);
        };
        let ServerState::Connected(conn) = &mut *guard else {
            return Ok(None);
        };
        framed::try_recv_batch(conn)
    }

    fn len(&self) -> usize {
        // Whole frames already buffered by the codec, i.e. readable without IO.
        match self.inner.server.try_lock() {
            Ok(guard) => match &*guard {
                ServerState::Connected(conn) => framed::buffered_frames(conn),
                _ => 0,
            },
            Err(_) => 0,
        }
    }

    fn capacity(&self) -> Option<usize> {
        Some(self.inner.capacity)
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    fn close(&self) {
        if !self.inner.closed.swap(true, Ordering::SeqCst) {
            info!(pipe = %self.inner.pipe_name, "Closing Windows Named Pipe transport");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_windows_pipe_roundtrip() {
        let pipe_name = format!("mq-bridge-test-{}", fast_uuid_v7::gen_id_str());

        // Create server
        let server = WindowsIpcTransport::new_server(&pipe_name, 10)
            .await
            .unwrap();

        // Create client in a separate task
        let pipe_name_clone = pipe_name.clone();
        let client_task = tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            WindowsIpcTransport::new_client(&pipe_name_clone, 10)
                .await
                .unwrap()
        });

        // Wait for connection
        server.wait_for_connection().await.unwrap();

        let client = client_task.await.unwrap();

        // Send from client
        let msg = CanonicalMessage::from_vec(b"test");
        client.send_batch(vec![msg.clone()]).await.unwrap();

        // Receive on server
        let received = server.recv_batch().await.unwrap();
        assert_eq!(received.len(), 1);
    }

    #[tokio::test]
    async fn test_windows_pipe_close() {
        let pipe_name = format!("mq-bridge-test-{}", fast_uuid_v7::gen_id_str());

        let server = WindowsIpcTransport::new_server(&pipe_name, 10)
            .await
            .unwrap();

        assert!(!server.is_closed());
        server.close();
        assert!(server.is_closed());
    }

    /// The server end must refuse to send rather than strand messages in a
    /// publisher's receive buffer.
    #[tokio::test]
    async fn test_windows_pipe_server_cannot_send() {
        let pipe_name = format!("mq-bridge-test-{}", fast_uuid_v7::gen_id_str());

        let server = WindowsIpcTransport::new_server(&pipe_name, 10)
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
    async fn test_windows_pipe_cancelled_receive_loses_nothing() {
        let pipe_name = format!("mq-bridge-test-{}", fast_uuid_v7::gen_id_str());

        let server = WindowsIpcTransport::new_server(&pipe_name, 10)
            .await
            .unwrap();
        let client = WindowsIpcTransport::new_client(&pipe_name, 10)
            .await
            .unwrap();
        server.wait_for_connection().await.unwrap();

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
}
