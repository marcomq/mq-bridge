//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

#![cfg(any(unix, windows))]

// Imports below are used only by the `#[tokio::test]` functions, which are
// stripped when this file is included into the performance bench (a non-test
// build), so allow them to appear unused there.
#[allow(unused_imports)]
use mq_bridge::endpoints::memory::transport::{TransportChannel, TransportUrl};
#[allow(unused_imports)]
use mq_bridge::CanonicalMessage;

#[cfg(unix)]
#[allow(unused_imports)]
use mq_bridge::endpoints::memory::ipc_unix::UnixIpcTransport;
#[cfg(windows)]
#[allow(unused_imports)]
use mq_bridge::endpoints::memory::ipc_windows::WindowsIpcTransport;

#[cfg(any(unix, windows))]
#[allow(dead_code)]
async fn connect_with_retry<T, F, Fut>(mut make_client: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
    let mut last_error = None;
    loop {
        match make_client().await {
            Ok(client) => return client,
            Err(error) if tokio::time::Instant::now() < deadline => {
                last_error = Some(error);
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                panic!(
                    "timed out connecting IPC client: {}",
                    last_error.unwrap_or(error)
                );
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn test_unix_ipc_basic_roundtrip() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test-basic.sock");

    // Create server (consumer)
    let server = UnixIpcTransport::new_server(&socket_path, 10)
        .await
        .unwrap();

    // Create client (publisher) in a separate task
    let socket_path_clone = socket_path.clone();
    let client_handle = tokio::spawn(async move {
        connect_with_retry(|| UnixIpcTransport::new_client(&socket_path_clone, 10)).await
    });

    let client = client_handle.await.unwrap();

    // Send a message from client
    let mut msg = CanonicalMessage::from_vec(b"Hello IPC!");
    msg.metadata
        .insert("test_key".to_string(), "test_value".to_string());

    client.send_batch(vec![msg.clone()]).await.unwrap();

    // Receive on server
    let received = server.recv_batch().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].payload.as_ref(), b"Hello IPC!");
    assert_eq!(
        received[0].metadata.get("test_key"),
        Some(&"test_value".to_string())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_unix_ipc_multiple_messages() {
    use tempfile::TempDir;

    let temp_dir = TempDir::new().unwrap();
    let socket_path = temp_dir.path().join("test-multi.sock");

    let server = UnixIpcTransport::new_server(&socket_path, 10)
        .await
        .unwrap();

    let socket_path_clone = socket_path.clone();
    let client_handle = tokio::spawn(async move {
        connect_with_retry(|| UnixIpcTransport::new_client(&socket_path_clone, 10)).await
    });

    let client = client_handle.await.unwrap();

    // Send multiple messages in a batch
    let messages: Vec<_> = (0..5)
        .map(|i| CanonicalMessage::from_vec(format!("Message {}", i).into_bytes()))
        .collect();

    client.send_batch(messages.clone()).await.unwrap();

    // Receive the batch
    let received = server.recv_batch().await.unwrap();
    assert_eq!(received.len(), 5);
    for (i, msg) in received.iter().enumerate() {
        assert_eq!(msg.payload.as_ref(), format!("Message {}", i).as_bytes());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn test_unix_ipc_url_parsing() {
    // Test memory URL
    let url = TransportUrl::parse("memory://test-topic").unwrap();
    assert!(matches!(url, TransportUrl::Memory { .. }));

    // Test unix socket URL
    let url = TransportUrl::parse("unix:///tmp/test.sock").unwrap();
    assert!(matches!(url, TransportUrl::Unix { .. }));

    // Test IPC with absolute path
    let url = TransportUrl::parse("ipc:///var/run/test.sock").unwrap();
    assert!(matches!(url, TransportUrl::Unix { .. }));

    // Test IPC with name (should resolve to default path)
    let url = TransportUrl::parse("ipc://worker-queue").unwrap();
    assert!(matches!(url, TransportUrl::Unix { .. }));
}

#[cfg(windows)]
#[tokio::test]
async fn test_windows_pipe_basic_roundtrip() {
    let pipe_name = format!("mq-bridge-test-{}", uuid::Uuid::new_v4());

    // Create server
    let server = WindowsIpcTransport::new_server(&pipe_name, 10)
        .await
        .unwrap();

    // Create client in a separate task
    let pipe_name_clone = pipe_name.clone();
    let client_handle = tokio::spawn(async move {
        connect_with_retry(|| WindowsIpcTransport::new_client(&pipe_name_clone, 10)).await
    });

    // Wait for connection
    server.wait_for_connection().await.unwrap();
    let client = client_handle.await.unwrap();

    // Send a message
    let msg = CanonicalMessage::from_vec(b"Hello Named Pipe!");

    client.send_batch(vec![msg.clone()]).await.unwrap();

    // Receive on server
    let received = server.recv_batch().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].payload.as_ref(), b"Hello Named Pipe!");
}

#[cfg(windows)]
#[tokio::test]
async fn test_windows_pipe_url_parsing() {
    // Test pipe URL
    let url = TransportUrl::parse("pipe://worker-queue").unwrap();
    assert!(matches!(url, TransportUrl::Pipe { .. }));

    // Test IPC on Windows (should resolve to pipe)
    let url = TransportUrl::parse("ipc://worker-queue").unwrap();
    assert!(matches!(url, TransportUrl::Pipe { .. }));
}

#[tokio::test]
async fn test_transport_url_display() {
    let url = TransportUrl::parse("memory://test").unwrap();
    assert_eq!(url.display_name(), "memory://test");

    #[cfg(unix)]
    {
        let url = TransportUrl::parse("unix:///tmp/test.sock").unwrap();
        assert_eq!(url.display_name(), "unix:///tmp/test.sock");
    }

    #[cfg(windows)]
    {
        let url = TransportUrl::parse("pipe://test").unwrap();
        assert_eq!(url.display_name(), "pipe://test");
    }
}
