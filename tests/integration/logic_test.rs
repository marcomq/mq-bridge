#![allow(unused_imports, dead_code)]

use mq_bridge::models::{Endpoint, Route};
use mq_bridge::{CanonicalMessage, Handled, Publisher, Sent};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn test_memory_request_reply_logic() {
    // This test verifies the Request -> Route -> Handler -> Response Endpoint flow.
    let in_topic = "req_rep_in";
    
    // 1. Define a route with a handler that produces a reply.
    let handler = |mut msg: CanonicalMessage| async move {
        let payload = msg.get_payload_str();
        msg.set_payload_str(format!("reply_to_{}", payload));
        Ok(Handled::Publish(msg))
    };

    let route = Route::new(
        Endpoint::new_memory(in_topic, 100),
        Endpoint::new_response(),
    ).with_handler(handler);

    route.deploy("logic_req_rep").await.unwrap();

    // 2. Create a publisher with request_reply mode enabled.
    let mut config = mq_bridge::models::MemoryConfig::new(in_topic, Some(100));
    config.request_reply = true;
    config.request_timeout_ms = Some(2000);
    
    let publisher = Publisher::new(Endpoint {
        endpoint_type: mq_bridge::models::EndpointType::Memory(config),
        ..Default::default()
    }).await.unwrap();

    // 3. Send request and verify response.
    let result = publisher.send("hello".into()).await.unwrap();
    
    if let Sent::Response(resp) = result {
        assert_eq!(resp.get_payload_str(), "reply_to_hello");
    } else {
        panic!("Expected Sent::Response, got {:?}", result);
    }

    Route::stop("logic_req_rep").await;
}

#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread")]
async fn test_http_request_reply_pattern() {
    // Verifies Request-Reply over a real HTTP boundary.
    let port = 12345;
    let addr = format!("127.0.0.1:{}", port);
    
    let http_in = Endpoint {
        endpoint_type: mq_bridge::models::EndpointType::Http(mq_bridge::models::HttpConfig {
            url: addr.clone(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let handler = |mut msg: CanonicalMessage| async move {
        msg.set_payload_str("http_pong");
        Ok(Handled::Publish(msg))
    };

    let route = Route::new(http_in, Endpoint::new_response()).with_handler(handler);
    route.deploy("http_logic").await.unwrap();

    // Use reqwest to simulate an external client.
    let client = reqwest::Client::new();
    let res = client.post(format!("http://{}", addr))
        .body("ping")
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), reqwest::StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "http_pong");

    Route::stop("http_logic").await;
}
