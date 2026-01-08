#![allow(dead_code, unused_imports)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use mq_bridge::{
    models::{Endpoint, Middleware, RetryMiddleware},
    CanonicalMessage, Handled, Route,
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

    let route = Route::new(input, output).add_handler("my_message", move |msg: MyTypedMessage| {
        let success_clone_2 = success_clone.clone();
        async move {
            assert_eq!(msg.id, 123);
            assert_eq!(msg.content, "hello");
            success_clone_2.store(true, Ordering::SeqCst);
            Ok(Handled::Ack)
        }
    });

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 123,
        content: "hello".into(),
    };

    let canonical_message = CanonicalMessage::from_type(&message)
        .unwrap()
        .with_type_key("my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    route.run_until_err("test", None, None).await.ok();

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
    let canonical_message =
        CanonicalMessage::new("invalid json".as_bytes().to_vec(), None).with_type_key("my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // The error is non-retryable, so it is logged and the message is dropped. The route continues.
    assert!(res.is_ok());

    // No message should be published to the output
    assert_eq!(out_channel.len(), 0);
}

#[tokio::test]
async fn test_retryable_error_without_middleware_crashes_route() {
    let input = Endpoint::new_memory("in_retry_crash", 10);
    let output = Endpoint::new_memory("out_retry_crash", 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |_msg: MyTypedMessage| async move {
            Err(mq_bridge::HandlerError::Retryable(anyhow::anyhow!(
                "Temporary failure"
            )))
        },
    );

    let in_channel = route.input.channel().unwrap();
    let message = MyTypedMessage {
        id: 1,
        content: "retry".into(),
    };
    let canonical_message = CanonicalMessage::from_type(&message)
        .unwrap()
        .with_type_key("my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // Should return Err because it's retryable and no middleware handles it
    assert!(res.is_err());
}

#[tokio::test]
async fn test_retryable_error_with_middleware_succeeds() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    let input = Endpoint::new_memory("in_retry_success", 10);
    let mut output = Endpoint::new_memory("out_retry_success", 10);

    // Add RetryMiddleware
    output.middlewares.push(Middleware::Retry(RetryMiddleware {
        max_attempts: 3,
        initial_interval_ms: 10,
        max_interval_ms: 100,
        multiplier: 1.0,
    }));

    let route = Route::new(input, output).add_handler("my_message", move |msg: MyTypedMessage| {
        let attempts = attempts_clone.clone();
        async move {
            let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if count < 3 {
                Err(mq_bridge::HandlerError::Retryable(anyhow::anyhow!(
                    "Temporary failure attempt {}",
                    count
                )))
            } else {
                Ok(Handled::Publish(CanonicalMessage::from_type(&msg).unwrap()))
            }
        }
    });

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 1,
        content: "retry".into(),
    };
    let canonical_message = CanonicalMessage::from_type(&message)
        .unwrap()
        .with_type_key("my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // Should succeed because middleware retries
    assert!(res.is_ok());
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(out_channel.len(), 1);
}

#[tokio::test]
async fn test_route_with_typed_handler_failure_handler() {
    let input = Endpoint::new_memory("in_fail_handler", 10);
    let output = Endpoint::new_memory("out_fail_handler", 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| async move {
            assert_eq!(msg.id, 456);
            Err(mq_bridge::HandlerError::NonRetryable(anyhow::anyhow!(
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
        .with_type_key("my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // The error is non-retryable, so it is logged and the message is dropped. The route continues.
    assert!(res.is_ok());

    // No message should be published to the output
    assert_eq!(out_channel.len(), 0);
}
