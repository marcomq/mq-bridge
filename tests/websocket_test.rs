#![cfg(feature = "websocket")]

use futures::{SinkExt, StreamExt};
use mq_bridge::endpoints::websocket::WebSocketConsumer;
use mq_bridge::endpoints::websocket::WebSocketPublisher;
use mq_bridge::models::{Endpoint, EndpointType, WebSocketConfig, WebSocketExecutionMode};
use mq_bridge::test_utils::get_free_port;
use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher};
use mq_bridge::{CanonicalMessage, Handled, HandlerError, Route};
use tokio_websockets::{ClientBuilder, Message};

async fn echo(msg: CanonicalMessage) -> Result<Handled, HandlerError> {
    Ok(Handled::Publish(msg))
}

async fn ack(_msg: CanonicalMessage) -> Result<Handled, HandlerError> {
    Ok(Handled::Ack)
}

async fn connect(
    url: impl AsRef<str>,
) -> tokio_websockets::WebSocketStream<tokio_websockets::MaybeTlsStream<tokio::net::TcpStream>> {
    let uri = url.as_ref().parse().expect("websocket URL should parse");
    let (stream, _) = ClientBuilder::from_uri(uri)
        .connect()
        .await
        .expect("client should connect");
    stream
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_endpoint_roundtrip() {
    let mut consumer =
        WebSocketConsumer::new(&WebSocketConfig::new("127.0.0.1:0").with_path("/events"))
            .await
            .expect("consumer should be created");

    let publisher = WebSocketPublisher::new(&WebSocketConfig::new(consumer.url().to_string()));

    publisher
        .send(
            CanonicalMessage::from_vec("hello integration")
                .with_metadata_kv("ws_message_type", "text"),
        )
        .await
        .expect("publisher should send");

    let mut batch = consumer
        .receive_batch(1)
        .await
        .expect("consumer should receive");
    assert_eq!(batch.messages.len(), 1);
    let message = batch.messages.pop().expect("one message");
    assert_eq!(message.get_payload_str(), "hello integration");
    assert_eq!(message.get_metadata("ws_message_type"), Some("text"));
    assert_eq!(message.get_metadata("ws_path"), Some("/events"));
    (batch.commit)(vec![MessageDisposition::Ack])
        .await
        .expect("commit should succeed");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_endpoint_handles_binary_payloads() {
    let mut consumer =
        WebSocketConsumer::new(&WebSocketConfig::new("127.0.0.1:0").with_path("/binary"))
            .await
            .expect("consumer should be created");

    let publisher = WebSocketPublisher::new(&WebSocketConfig::new(consumer.url().to_string()));

    publisher
        .send(
            CanonicalMessage::new(vec![0, 1, 2, 3, 255], None)
                .with_metadata_kv("ws_message_type", "binary"),
        )
        .await
        .expect("publisher should send");

    let mut batch = consumer
        .receive_batch(1)
        .await
        .expect("consumer should receive");
    assert_eq!(batch.messages.len(), 1);
    let message = batch.messages.pop().expect("one message");
    assert_eq!(message.payload.as_ref(), &[0, 1, 2, 3, 255]);
    assert_eq!(message.get_metadata("ws_message_type"), Some("binary"));
    (batch.commit)(vec![MessageDisposition::Ack])
        .await
        .expect("commit should succeed");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_publisher_reuses_connection_for_single_sends() {
    let mut consumer = WebSocketConsumer::new(&WebSocketConfig {
        routed_queue_capacity: Some(16),
        ..WebSocketConfig::new("127.0.0.1:0").with_path("/single-reuse")
    })
    .await
    .expect("consumer should be created");
    let publisher = WebSocketPublisher::new(&WebSocketConfig::new(consumer.url().to_string()));

    publisher
        .send(CanonicalMessage::from_vec("first").with_metadata_kv("ws_message_type", "text"))
        .await
        .expect("first message should send");
    publisher
        .send(CanonicalMessage::from_vec("second").with_metadata_kv("ws_message_type", "text"))
        .await
        .expect("second message should send");

    let mut peers = Vec::new();
    while peers.len() < 2 {
        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consumer.receive_batch(2 - peers.len()),
        )
        .await
        .expect("consumer should not time out")
        .expect("consumer should receive");
        let batch_len = batch.messages.len();
        for message in batch.messages {
            peers.push(
                message
                    .get_metadata("ws_peer_addr")
                    .expect("message should include peer addr")
                    .to_owned(),
            );
        }
        (batch.commit)(vec![MessageDisposition::Ack; batch_len])
            .await
            .expect("commit should succeed");
    }

    let first_peer = &peers[0];
    let second_peer = &peers[1];
    assert_eq!(first_peer, second_peer);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_publisher_batch_ack_means_all_messages_are_received() {
    let mut consumer = WebSocketConsumer::new(&WebSocketConfig {
        routed_queue_capacity: Some(2048),
        ..WebSocketConfig::new("127.0.0.1:0").with_path("/batch")
    })
    .await
    .expect("consumer should be created");
    let publisher = WebSocketPublisher::new(&WebSocketConfig::new(consumer.url().to_string()));

    let expected = 1000;
    let messages = (0..expected)
        .map(|index| {
            CanonicalMessage::from_vec(format!("message-{index}"))
                .with_metadata_kv("ws_message_type", "text")
        })
        .collect();
    publisher
        .send_batch(messages)
        .await
        .expect("publisher should ack only after flushing the batch");

    let mut received = 0;
    while received < expected {
        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consumer.receive_batch(expected - received),
        )
        .await
        .expect("consumer should not time out")
        .expect("consumer should receive");
        let batch_len = batch.messages.len();
        received += batch_len;
        let commit = batch.commit;
        commit(vec![MessageDisposition::Ack; batch_len])
            .await
            .expect("commit should succeed");
    }

    assert_eq!(received, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_consumer_sends_commit_reply_to_client() {
    let mut consumer =
        WebSocketConsumer::new(&WebSocketConfig::new("127.0.0.1:0").with_path("/reply"))
            .await
            .expect("consumer should be created");

    let mut stream = connect(consumer.url()).await;

    stream
        .send(Message::text("request"))
        .await
        .expect("client should send request");

    let mut batch = consumer
        .receive_batch(1)
        .await
        .expect("consumer should receive request");
    assert_eq!(batch.messages.len(), 1);
    let request = batch.messages.pop().expect("one request");
    assert_eq!(request.get_payload_str(), "request");

    (batch.commit)(vec![MessageDisposition::Reply(
        CanonicalMessage::from_vec("response").with_metadata_kv("ws_message_type", "text"),
    )])
    .await
    .expect("reply commit should succeed");

    let reply = stream
        .next()
        .await
        .expect("client should receive response")
        .expect("response frame should be valid");
    assert_eq!(reply.as_text(), Some("response"));
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_routed_consumer_replies_to_ping_exactly_once() {
    let consumer = WebSocketConsumer::new(&WebSocketConfig::new("127.0.0.1:0").with_path("/ping"))
        .await
        .expect("consumer should be created");

    let mut stream = connect(consumer.url()).await;
    stream
        .send(Message::ping("hello"))
        .await
        .expect("client should send ping");

    let pong = stream
        .next()
        .await
        .expect("client should receive pong")
        .expect("pong frame should be valid");
    assert!(pong.is_pong());
    assert_eq!(&pong.as_payload()[..], b"hello");

    // The routed transport must not emit a second (duplicate) pong frame.
    let extra = tokio::time::timeout(std::time::Duration::from_millis(150), stream.next()).await;
    assert!(extra.is_err(), "ping should produce exactly one pong reply");

    drop(consumer);
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_direct_response_route_replies_with_handler() {
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}")).with_path("/inline"),
    ));
    let output = Endpoint::new_response();
    let route = Route::new(input, output).with_handler(echo);
    let handle = route
        .run(&format!("websocket-inline-response-fast-path-{port}"))
        .await
        .expect("route should start");

    let mut stream = connect(format!("ws://127.0.0.1:{port}/inline")).await;
    stream
        .send(Message::text("request"))
        .await
        .expect("client should send request");

    let reply = stream
        .next()
        .await
        .expect("client should receive response")
        .expect("response frame should be valid");
    assert_eq!(reply.as_text(), Some("request"));

    handle.stop().await;
    handle.join().await.expect("route task should finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_direct_response_route_echoes_without_handler() {
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}")).with_path("/echo"),
    ));
    let output = Endpoint::new_response();
    let route = Route::new(input, output);
    let handle = route
        .run(&format!("websocket-direct-echo-{port}"))
        .await
        .expect("route should start");

    let mut stream = connect(format!("ws://127.0.0.1:{port}/echo")).await;
    stream
        .send(Message::binary(vec![0, 1, 2, 3]))
        .await
        .expect("client should send request");

    let reply = stream
        .next()
        .await
        .expect("client should receive response")
        .expect("response frame should be valid");
    assert!(reply.is_binary());
    assert_eq!(&reply.as_payload()[..], &[0, 1, 2, 3]);

    handle.stop().await;
    handle.join().await.expect("route task should finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_direct_response_route_ack_sends_no_reply() {
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}")).with_path("/ack"),
    ));
    let output = Endpoint::new_response();
    let route = Route::new(input, output).with_handler(ack);
    let handle = route
        .run(&format!("websocket-direct-ack-{port}"))
        .await
        .expect("route should start");

    let mut stream = connect(format!("ws://127.0.0.1:{port}/ack")).await;
    stream
        .send(Message::text("request"))
        .await
        .expect("client should send request");
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next()).await;
    assert!(result.is_err(), "ack should not produce a websocket reply");

    handle.stop().await;
    handle.join().await.expect("route task should finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_direct_response_route_replies_to_ping() {
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}"))
            .with_path("/ping")
            .with_execution_mode(WebSocketExecutionMode::DirectOnly),
    ));
    let output = Endpoint::new_response();
    let route = Route::new(input, output);
    let handle = route
        .run(&format!("websocket-direct-ping-{port}"))
        .await
        .expect("route should start");

    let mut stream = connect(format!("ws://127.0.0.1:{port}/ping")).await;
    stream
        .send(Message::ping("hello"))
        .await
        .expect("client should send ping");
    let reply = stream
        .next()
        .await
        .expect("client should receive pong")
        .expect("pong frame should be valid");
    assert!(reply.is_pong());
    assert_eq!(&reply.as_payload()[..], b"hello");

    handle.stop().await;
    handle.join().await.expect("route task should finish");
}

#[tokio::test(flavor = "multi_thread")]
async fn websocket_direct_response_route_rejects_wrong_path_with_404() {
    let port = get_free_port();
    let input = Endpoint::new(EndpointType::WebSocket(
        WebSocketConfig::new(format!("127.0.0.1:{port}"))
            .with_path("/expected")
            .with_execution_mode(WebSocketExecutionMode::DirectOnly),
    ));
    let output = Endpoint::new_response();
    let route = Route::new(input, output);
    let handle = route
        .run(&format!("websocket-direct-path-{port}"))
        .await
        .expect("route should start");

    // The upgrade handshake must be rejected before completing, so the client
    // never reaches an open WebSocket connection.
    let uri = format!("ws://127.0.0.1:{port}/wrong")
        .parse()
        .expect("websocket URL should parse");
    let result = ClientBuilder::from_uri(uri).connect().await;
    assert!(
        result.is_err(),
        "wrong path should be rejected with 404 before the upgrade completes"
    );

    handle.stop().await;
    handle.join().await.expect("route task should finish");
}
