#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use mq_bridge::{
    models::{Endpoint, Route},
    CanonicalMessage, Handled,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct MyTypedMessage {
    id: u32,
    content: String,
}

#[tokio::test]
async fn test_route_with_typed_handler_success() {
    let success = Arc::new(AtomicBool::new(false));
    let success_clone = success.clone();

    let input = Endpoint::new_memory("in_success", 10);
    let output = Endpoint::new_memory("out_success", 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| {
            let success_clone_2 = success_clone.clone();
            async move {
                assert_eq!(msg.id, 123);
                assert_eq!(msg.content, "hello");
                success_clone_2.store(true, Ordering::SeqCst);
                Ok(Handled::Ack)
            }
        },
    );

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 123,
        content: "hello".into(),
    };

    let canonical_message = CanonicalMessage::from_type(&message)
        .unwrap()
        .with_metadata_kv("kind", "my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    route.run_until_err("test", None).await.ok();

    assert!(success.load(Ordering::SeqCst));
    assert_eq!(out_channel.len(), 0); // Ack should not publish
}

#[tokio::test]
async fn test_route_with_typed_handler_failure_deserialization() {
    let input = Endpoint::new_memory("in_fail_deser", 10);
    let output = Endpoint::new_memory("out_fail_deser", 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| async move {
            // This should not be called
            let _ = msg;
            unreachable!("Handler should not be called on deserialization failure");
        },
    );

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    // Send a message that will fail to deserialize into MyTypedMessage
    let canonical_message = CanonicalMessage::new("invalid json".as_bytes().to_vec(), None)
        .with_metadata_kv("kind", "my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None).await;

    // The error should be a non-retryable handler error about deserialization
    assert!(res.is_err());
    let err_string = res.unwrap_err().to_string();
    assert!(err_string.contains("Deserialization failed"));

    // No message should be published to the output
    assert_eq!(out_channel.len(), 0);
}

#[tokio::test]
async fn test_route_with_typed_handler_failure_handler() {
    let input = Endpoint::new_memory("in_fail_handler", 10);
    let output = Endpoint::new_memory("out_fail_handler", 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| async move {
            assert_eq!(msg.id, 456);
            Err(mq_bridge::errors::HandlerError::NonRetryable(anyhow::anyhow!(
                "Handler failed as expected"
            )))
        },
    );

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 456,
        content: "world".into(),
    };

    let canonical_message = CanonicalMessage::from_type(&message)
        .unwrap()
        .with_metadata_kv("kind", "my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None).await;

    // The error should be the non-retryable handler error from our handler
    assert!(res.is_err());
    let err_string = res.unwrap_err().to_string();
    assert!(err_string.contains("Handler failed as expected"));

    // No message should be published to the output
    assert_eq!(out_channel.len(), 0);
}
