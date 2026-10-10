#![allow(dead_code, unused_imports)]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use mq_bridge::test_utils::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use mq_bridge::CanonicalMessage;
use mq_bridge::{
    models::{Endpoint, Middleware, RetryMiddleware},
    msg, Handled, Route,
};
use serde::{Deserialize, Serialize};
use std::env;

/// Helper to run a simple service loop that receives messages and replies.
async fn run_service_reply(mut consumer: Box<dyn MessageConsumer>, response_payload: &[u8]) {
    // Run continuously: receive and reply to messages until the consumer errors or test ends.
    loop {
        match consumer.receive().await {
            Ok(received) => {
                let response = CanonicalMessage::new(response_payload.to_vec(), None);
                if let Err(e) =
                    (received.commit)(mq_bridge::traits::MessageDisposition::Reply(response)).await
                {
                    tracing::error!("Failed to commit reply: {:?}", e);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Service consumer receive failed, exiting service loop: {:?}",
                    e
                );
                break;
            }
        }
    }
}

/// Helper to run a simple service loop that receives messages and ACKs them (no reply).
async fn run_service_ack(mut consumer: Box<dyn MessageConsumer>) {
    loop {
        match consumer.receive().await {
            Ok(received) => {
                if let Err(e) = (received.commit)(mq_bridge::traits::MessageDisposition::Ack).await
                {
                    tracing::error!("Failed to commit ack: {:?}", e);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Service consumer receive failed, exiting service loop: {:?}",
                    e
                );
                break;
            }
        }
    }
}

/// Helper to run a single receive-and-reply (with timeout).
async fn run_service_reply_once(mut consumer: Box<dyn MessageConsumer>, response_payload: &[u8]) {
    match tokio::time::timeout(std::time::Duration::from_secs(5), consumer.receive()).await {
        Ok(Ok(received)) => {
            let response = CanonicalMessage::new(response_payload.to_vec(), None);
            if let Err(e) =
                (received.commit)(mq_bridge::traits::MessageDisposition::Reply(response)).await
            {
                tracing::error!("Failed to commit reply once: {:?}", e);
            }
        }
        Ok(Err(e)) => tracing::warn!("Service consumer failed to receive once: {:?}", e),
        Err(_) => tracing::warn!("Service consumer receive timed out (once)"),
    }
}

/// Helper to run a single receive-and-ack (with timeout).
async fn run_service_ack_once(mut consumer: Box<dyn MessageConsumer>) {
    match tokio::time::timeout(std::time::Duration::from_secs(5), consumer.receive()).await {
        Ok(Ok(received)) => {
            if let Err(e) = (received.commit)(mq_bridge::traits::MessageDisposition::Ack).await {
                tracing::error!("Failed to commit ack once: {:?}", e);
            }
        }
        Ok(Err(e)) => tracing::warn!("Service consumer failed to receive once: {:?}", e),
        Err(_) => tracing::warn!("Service consumer receive timed out (once)"),
    }
}

/// Send a NATS request-reply request, retrying on transient "no responders".
///
/// The responder's subscription interest is registered on its own connection;
/// at broker startup it can briefly be invisible to the requester's separate
/// connection, yielding a 503 "no responders". A 503 means the request reached
/// nobody, so retrying is safe — the responder still replies to the first
/// request that lands.
#[cfg(feature = "nats")]
async fn nats_request_with_retry(
    publisher: &mq_bridge::endpoints::nats::NatsPublisher,
    payload: &[u8],
) -> mq_bridge::traits::Sent {
    // Retry "no responders" until a generous deadline rather than a fixed attempt
    // count, so the request-reply tests absorb slower NATS or responder startup.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let msg = CanonicalMessage::new(payload.to_vec(), None);
        match tokio::time::timeout(std::time::Duration::from_secs(5), publisher.send(msg))
            .await
            .expect("Timed out waiting for NATS request response")
        {
            Ok(sent) => return sent,
            Err(e) if e.to_string().contains("no responders") => {
                if std::time::Instant::now() >= deadline {
                    panic!("NATS request kept getting 'no responders' until deadline: {e:?}");
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(e) => panic!("NATS request failed: {e:?}"),
        }
    }
}

#[cfg(feature = "kafka")]
pub async fn test_kafka_request_reply() {
    use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let request_topic = "test_req_rep_topic";
        let reply_topic = "test_req_rep_reply_topic";

        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            group_id: Some("req_rep_group".to_string()),
            producer_options: Some(vec![("acks".to_string(), "1".to_string())]),
            ..Default::default()
        };

        let mut req_config = config.clone();
        req_config.topic = Some(request_topic.to_string());
        let client_publisher = KafkaPublisher::new(&req_config).await.unwrap();

        let mut rep_config = config.clone();
        rep_config.topic = Some(reply_topic.to_string());
        let _ = KafkaPublisher::new(&rep_config).await.unwrap();

        let mut service_endpoint = config.clone();
        service_endpoint.topic = Some(request_topic.to_string());
        let service_consumer = KafkaConsumer::new(&service_endpoint).await.unwrap();

        let mut reply_config = config.clone();
        reply_config.group_id = Some("reply_group".to_string());
        let mut client_endpoint = reply_config;
        client_endpoint.topic = Some(reply_topic.to_string());
        let mut client_consumer = KafkaConsumer::new(&client_endpoint).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"response").await;
        });

        let correlation_id = "cid-12345";
        let mut req_msg = CanonicalMessage::new(b"request".to_vec(), None);
        req_msg
            .metadata
            .insert("reply_to".to_string(), reply_topic.to_string());
        req_msg
            .metadata
            .insert("correlation_id".to_string(), correlation_id.to_string());

        client_publisher.send(req_msg).await.unwrap();

        let received_resp =
            tokio::time::timeout(std::time::Duration::from_secs(5), client_consumer.receive())
                .await
                .expect("Timed out waiting for Kafka response")
                .unwrap();
        assert_eq!(received_resp.message.payload, b"response".as_slice());
        assert_eq!(
            received_resp
                .message
                .metadata
                .get("correlation_id")
                .map(|s| s.as_str()),
            Some(correlation_id)
        );
        println!("Kafka Request-Reply test passed!");
    })
    .await;
}

#[cfg(feature = "kafka")]
pub async fn test_kafka_request_reply_multiple_sequential() {
    use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
    use mq_bridge::models::KafkaConfig;
    use mq_bridge::CanonicalMessage;
    use std::collections::HashSet;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let request_topic = format!("test_req_rep_topic_{}", fast_uuid_v7::gen_id_str());
        let reply_topic = format!("test_req_rep_reply_{}", fast_uuid_v7::gen_id_str());

        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            ..Default::default()
        };

        let mut req_config = config.clone();
        req_config.topic = Some(request_topic.clone());
        let client_publisher = KafkaPublisher::new(&req_config).await.unwrap();

        let mut reply_config = config.clone();
        reply_config.topic = Some(reply_topic.clone());
        let _ = KafkaPublisher::new(&reply_config).await.unwrap();
        reply_config.group_id = Some(format!("reply_group_{}", fast_uuid_v7::gen_id_str()));
        let mut client_consumer = KafkaConsumer::new(&reply_config).await.unwrap();

        let mut service_endpoint = config.clone();
        service_endpoint.topic = Some(request_topic.clone());
        // Run the service as a proper consumer (so it has a producer for replies)
        service_endpoint.group_id = Some(format!("service_group_{}", fast_uuid_v7::gen_id_str()));
        let service_consumer = KafkaConsumer::new(&service_endpoint).await.unwrap();

        // Wait for the consumer to report healthy/ready (retry for up to ~5s)
        let mut attempts = 0;
        loop {
            let status = service_consumer.status().await;
            if status.healthy {
                break;
            }
            if attempts >= 10 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            attempts += 1;
        }

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"kafka_multi_resp").await;
        });

        let mut expected: HashSet<String> = HashSet::new();
        for i in 0..8 {
            let cid = format!("cid-{}", i);
            let mut msg = CanonicalMessage::new(format!("req-{}", i).into_bytes(), None);
            msg.metadata
                .insert("reply_to".to_string(), reply_topic.clone());
            msg.metadata
                .insert("correlation_id".to_string(), cid.clone());
            expected.insert(cid.clone());
            client_publisher.send(msg).await.unwrap();
        }

        let mut received = HashSet::new();
        for _ in 0..8 {
            let rec =
                tokio::time::timeout(std::time::Duration::from_secs(5), client_consumer.receive())
                    .await
                    .expect("Timed out waiting for Kafka sequential response")
                    .unwrap();
            let cid = rec.message.metadata.get("correlation_id").cloned();
            if let Some(c) = cid {
                received.insert(c);
            }
            let _ = (rec.commit)(mq_bridge::traits::MessageDisposition::Ack).await;
            assert_eq!(rec.message.get_payload_str(), "kafka_multi_resp");
        }

        assert_eq!(expected, received);
        println!("Kafka sequential multi-request request-reply test passed!");
    })
    .await;
}

#[cfg(feature = "kafka")]
pub async fn test_kafka_request_reply_lost_response() {
    use mq_bridge::endpoints::kafka::{KafkaConsumer, KafkaPublisher};
    use mq_bridge::models::KafkaConfig;
    use mq_bridge::CanonicalMessage;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/kafka.yml", || async {
        let request_topic = format!("test_req_rep_lost_{}", fast_uuid_v7::gen_id_str());
        let reply_topic = format!("test_req_rep_lost_reply_{}", fast_uuid_v7::gen_id_str());

        let config = mq_bridge::models::KafkaConfig {
            url: "localhost:9092".to_string(),
            ..Default::default()
        };

        let mut req_config = config.clone();
        req_config.topic = Some(request_topic.clone());
        let client_publisher = KafkaPublisher::new(&req_config).await.unwrap();

        let mut reply_config = config.clone();
        reply_config.topic = Some(reply_topic.clone());
        let _ = KafkaPublisher::new(&reply_config).await.unwrap();
        reply_config.group_id = Some(format!("reply_group_lost_{}", fast_uuid_v7::gen_id_str()));
        let mut client_consumer = KafkaConsumer::new(&reply_config).await.unwrap();

        // Service that ACKs without replying
        let mut service_endpoint = config.clone();
        service_endpoint.topic = Some(request_topic.clone());
        let service_consumer = KafkaConsumer::new(&service_endpoint).await.unwrap();

        tokio::spawn(async move {
            run_service_ack(Box::new(service_consumer)).await;
        });

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let mut req_msg = CanonicalMessage::new(b"lost_request".to_vec(), None);
        req_msg
            .metadata
            .insert("reply_to".to_string(), reply_topic.clone());
        req_msg
            .metadata
            .insert("correlation_id".to_string(), "lost-cid".to_string());

        client_publisher.send(req_msg).await.unwrap();

        // Expect no reply within short timeout
        let res =
            tokio::time::timeout(std::time::Duration::from_secs(2), client_consumer.receive())
                .await;
        assert!(res.is_err(), "Expected no reply (timed out), but got one");
        println!("Kafka lost-response simulation passed (no reply received)");
    })
    .await;
}

#[cfg(feature = "nats")]
pub async fn test_nats_request_reply() {
    use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
    use mq_bridge::traits::Sent;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let subject = "req_rep_subject";
        let stream_name = "req_rep_stream";

        // Create publisher first (ensures JetStream stream exists)
        let mut pub_config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            request_reply: true,
            ..Default::default()
        };
        pub_config.subject = Some(subject.to_string());
        pub_config.stream = Some(stream_name.to_string());
        // JetStream enabled (default)
        let publisher = NatsPublisher::new(&pub_config).await.unwrap();

        // Now create the JetStream consumer (service side)
        let mut service_endpoint = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            no_jetstream: true,
            ..Default::default()
        };
        service_endpoint.subject = Some(subject.to_string());
        service_endpoint.stream = Some(stream_name.to_string());
        // Native NATS request-reply needs a live Core subscription as the responder.
        let service_consumer = NatsConsumer::new(&service_endpoint).await.unwrap();

        // Spawn the reply service after both publisher and consumer are ready
        let (service_ready_tx, service_ready_rx) = tokio::sync::oneshot::channel();
        let service_task = tokio::spawn(async move {
            let _ = service_ready_tx.send(());
            run_service_reply_once(Box::new(service_consumer), b"pong").await;
        });
        service_ready_rx.await.unwrap();

        // Send the request and check the response
        let result = nats_request_with_retry(&publisher, b"ping").await;

        match result {
            Sent::Response(resp) => {
                assert_eq!(resp.payload.to_vec(), b"pong");
            }
            _ => panic!("Expected response"),
        }
        service_task.await.unwrap();
        println!("NATS Request-Reply test passed!");
    })
    .await;
}

/// Test NATS request-reply with Core (no JetStream) mode for both publisher and consumer.
#[cfg(feature = "nats")]
pub async fn test_nats_core_request_reply() {
    use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
    use mq_bridge::traits::Sent;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let subject = "core_req_rep_subject";

        // Publisher in Core mode
        let mut pub_config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            request_reply: true,
            no_jetstream: true,
            ..Default::default()
        };
        pub_config.subject = Some(subject.to_string());
        pub_config.stream = Some("ignored".to_string());
        let publisher = NatsPublisher::new(&pub_config).await.unwrap();

        // Consumer in Core mode
        let mut service_endpoint = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            no_jetstream: true,
            ..Default::default()
        };
        service_endpoint.subject = Some(subject.to_string());
        service_endpoint.stream = Some("ignored".to_string());
        let service_consumer = NatsConsumer::new(&service_endpoint).await.unwrap();

        let (service_ready_tx, service_ready_rx) = tokio::sync::oneshot::channel();
        let service_task = tokio::spawn(async move {
            let _ = service_ready_tx.send(());
            run_service_reply_once(Box::new(service_consumer), b"pong").await;
        });
        service_ready_rx.await.unwrap();

        let result = nats_request_with_retry(&publisher, b"ping").await;

        match result {
            Sent::Response(resp) => {
                assert_eq!(resp.payload.to_vec(), b"pong");
            }
            _ => panic!("Expected response"),
        }
        service_task.await.unwrap();
        println!("NATS Core Request-Reply test passed!");
    })
    .await;
}

#[cfg(feature = "mongodb")]
pub async fn test_mongodb_request_reply_pattern() {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::traits::Sent;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let req_collection = "req_rep_collection";
        let db_name = "mq_bridge_test_req_rep";

        // Setup the "service" side (the consumer that replies)
        let service_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.to_string(),
            ..Default::default()
        };
        let mut service_endpoint = service_config;
        service_endpoint.collection = Some(req_collection.to_string());
        let service_consumer = MongoDbConsumer::new(&service_endpoint).await.unwrap();

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"mongo_response").await;
        });

        // Setup the "client" side (the publisher that sends and waits)
        let client_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.to_string(),
            request_reply: true, // Enable request-reply mode
            ..Default::default()
        };
        let mut pub_config = client_config.clone();
        pub_config.collection = Some(req_collection.to_string());
        let client_publisher = MongoDbPublisher::new(&pub_config).await.unwrap();

        // Send request and wait for response
        let request_msg = CanonicalMessage::new(b"mongo_request".to_vec(), None);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_publisher.send(request_msg),
        )
        .await
        .expect("Timed out waiting for MongoDB response")
        .unwrap();

        match result {
            Sent::Response(resp) => {
                assert_eq!(resp.get_payload_str(), "mongo_response");
            }
            _ => panic!("Expected Sent::Response, got {:?}", result),
        }
        println!("MongoDB Request-Reply test passed!");
    })
    .await;
}

#[cfg(feature = "mongodb")]
pub async fn test_mongodb_request_reply_multiple_sequential() {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::traits::Sent;
    use mq_bridge::CanonicalMessage;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let req_collection = format!("req_rep_collection_{}", fast_uuid_v7::gen_id_str());
        let db_suffix = fast_uuid_v7::gen_id_str();
        let db_suffix_short: String = db_suffix.chars().take(35).collect();
        let db_name = format!("mq_bridge_test_req_rep_{}", db_suffix_short);

        // Service side
        let service_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.clone(),
            ..Default::default()
        };
        let mut service_endpoint = service_config;
        service_endpoint.collection = Some(req_collection.clone());
        let service_consumer = MongoDbConsumer::new(&service_endpoint).await.unwrap();

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"mongo_multi_resp").await;
        });

        // Client side
        let client_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.clone(),
            request_reply: true,
            ..Default::default()
        };
        let mut pub_conf = client_config.clone();
        pub_conf.collection = Some(req_collection.clone());
        let client_publisher = MongoDbPublisher::new(&pub_conf).await.unwrap();

        for i in 0..8 {
            let req = CanonicalMessage::new(format!("mongo_req_{}", i).into_bytes(), None);
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client_publisher.send(req),
            )
            .await
            .expect("Timed out waiting for MongoDB sequential response")
            .unwrap();
            match res {
                Sent::Response(r) => assert_eq!(r.get_payload_str(), "mongo_multi_resp"),
                _ => panic!("Expected response for MongoDB request"),
            }
        }

        println!("MongoDB sequential multi-request request-reply test passed!");
    })
    .await;
}

#[cfg(feature = "mongodb")]
pub async fn test_mongodb_request_reply_lost_response() {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::traits::PublisherError;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let req_collection = format!("req_rep_lost_{}", fast_uuid_v7::gen_id_str());
        let db_suffix = fast_uuid_v7::gen_id_str();
        let db_suffix_short: String = db_suffix.chars().take(35).collect();
        let db_name = format!("mq_bridge_test_req_rep_lost_{}", db_suffix_short);

        let service_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.clone(),
            ..Default::default()
        };
        let mut service_endpoint = service_config;
        service_endpoint.collection = Some(req_collection.clone());
        let service_consumer = MongoDbConsumer::new(&service_endpoint).await.unwrap();

        // Service ACKs without replying
        tokio::spawn(async move {
            run_service_ack(Box::new(service_consumer)).await;
        });

        let client_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.clone(),
            request_reply: true,
            request_timeout_ms: Some(2000),
            ..Default::default()
        };
        let mut pub_conf = client_config.clone();
        pub_conf.collection = Some(req_collection.clone());
        let client_publisher = MongoDbPublisher::new(&pub_conf).await.unwrap();

        let req = mq_bridge::CanonicalMessage::new(b"lost_mongo_req".to_vec(), None);
        let res = client_publisher.send(req).await;
        match res {
            Err(PublisherError::NonRetryable(_)) => {
                // Expected: timed out waiting for reply
            }
            Ok(_) => panic!("Expected timeout/error due to missing reply"),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }

        println!("MongoDB lost-response simulation passed (timeout detected)");
    })
    .await;
}

#[cfg(feature = "amqp")]
pub async fn test_amqp_request_reply() {
    use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let req_queue = "test_req_rep_queue";
        let reply_queue = "test_req_rep_reply_queue";

        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            ..Default::default()
        };

        let mut pub_config = config.clone();
        pub_config.queue = Some(req_queue.to_string());
        let client_publisher = AmqpPublisher::new(&pub_config).await.unwrap();
        let mut client_endpoint = config.clone();
        client_endpoint.queue = Some(reply_queue.to_string());
        client_endpoint.subscribe_mode = false;
        let mut client_consumer = AmqpConsumer::new(&client_endpoint).await.unwrap();
        let mut service_endpoint = config.clone();
        service_endpoint.queue = Some(req_queue.to_string());
        service_endpoint.subscribe_mode = false;
        let service_consumer = AmqpConsumer::new(&service_endpoint).await.unwrap();

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"response").await;
        });

        let correlation_id = "cid-amqp-123";
        let mut req_msg = CanonicalMessage::new(b"request".to_vec(), None);
        req_msg
            .metadata
            .insert("reply_to".to_string(), reply_queue.to_string());
        req_msg
            .metadata
            .insert("correlation_id".to_string(), correlation_id.to_string());

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_publisher.send(req_msg),
        )
        .await
        .expect("Timed out sending AMQP request")
        .unwrap();

        let received_resp =
            tokio::time::timeout(std::time::Duration::from_secs(5), client_consumer.receive())
                .await
                .expect("Timed out waiting for AMQP response")
                .unwrap();

        assert_eq!(received_resp.message.payload, b"response".as_slice());
        assert_eq!(
            received_resp
                .message
                .metadata
                .get("correlation_id")
                .map(|s| s.as_str()),
            Some(correlation_id)
        );
        println!("AMQP Request-Reply test passed!");
    })
    .await;
}

#[cfg(feature = "mqtt")]
pub async fn test_mqtt_request_reply() {
    use mq_bridge::endpoints::mqtt::{MqttConsumer, MqttPublisher};
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mqtt.yml", || async {
        let req_topic = "test/req_rep";
        let reply_topic = "test/req_rep/reply";

        let config = mq_bridge::models::MqttConfig {
            url: "mqtt://localhost:1883".to_string(),
            clean_session: false,
            ..Default::default()
        };

        let mut pub_config = config.clone();
        pub_config.topic = Some(req_topic.to_string());
        pub_config.client_id = Some("client_pub".to_string());
        let client_publisher = MqttPublisher::new(&pub_config).await.unwrap();
        let mut client_config = config.clone();
        client_config.client_id = Some("client_sub".to_string());
        let mut client_endpoint = client_config;
        client_endpoint.topic = Some(reply_topic.to_string());
        let mut client_consumer = MqttConsumer::new(&client_endpoint).await.unwrap();

        let service_config = config.clone();
        let mut service_endpoint = service_config;
        service_endpoint.topic = Some(req_topic.to_string());
        let service_consumer = MqttConsumer::new(&service_endpoint).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"response").await;
        });

        let correlation_data = "cid-mqtt-123";
        let mut req_msg = CanonicalMessage::new(b"request".to_vec(), None);
        req_msg
            .metadata
            .insert("reply_to".to_string(), reply_topic.to_string());
        req_msg
            .metadata
            .insert("correlation_id".to_string(), correlation_data.to_string());

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client_publisher.send(req_msg),
        )
        .await
        .expect("Timed out sending MQTT request")
        .unwrap();

        let received_resp =
            tokio::time::timeout(std::time::Duration::from_secs(5), client_consumer.receive())
                .await
                .expect("Timed out waiting for MQTT response")
                .unwrap();

        assert_eq!(received_resp.message.payload, b"response".as_slice());
        assert_eq!(
            received_resp
                .message
                .metadata
                .get("correlation_id")
                .map(|s| s.as_str()),
            Some(correlation_data)
        );
        println!("MQTT Request-Reply test passed!");
    })
    .await;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
struct MyTypedMessage {
    id: u32,
    content: String,
}

fn get_unique_topic(base: &str) -> String {
    format!("{}_{}", base, fast_uuid_v7::gen_id())
}

fn test_env_ms(key: &str, default_ms: u64) -> std::time::Duration {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_millis(default_ms))
}

fn test_env_secs(key: &str, default_s: u64) -> std::time::Duration {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(default_s))
}

#[tokio::test(flavor = "multi_thread")]
async fn test_route_with_typed_handler_success() {
    let success = Arc::new(AtomicBool::new(false));
    let success_clone = success.clone();

    let in_topic = get_unique_topic("in_success");
    let out_topic = get_unique_topic("out_success");
    let input = Endpoint::new_memory(&in_topic, 10);
    let output = Endpoint::new_memory(&out_topic, 10);

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

    let canonical_message = msg!(&message, "my_message");

    route
        .deploy("test_route_with_typed_handler_success")
        .await
        .unwrap();

    in_channel.send_message(canonical_message).await.unwrap();
    let start = std::time::Instant::now();
    while !success.load(Ordering::SeqCst) {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Timeout waiting for handler");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    Route::stop("test_route_with_typed_handler_success").await;

    assert!(success.load(Ordering::SeqCst));
    assert_eq!(out_channel.len(), 0); // Ack should not publish
}

#[tokio::test]
async fn test_route_with_typed_handler_failure_deserialization() {
    let in_topic = get_unique_topic("in_fail_deser");
    let out_topic = get_unique_topic("out_fail_deser");
    let input = Endpoint::new_memory(&in_topic, 10);
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| async move {
            // This should not be called
            let _ = msg;
            if false {
                return;
            }
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
    let in_topic = get_unique_topic("in_retry_crash");
    let out_topic = get_unique_topic("out_retry_crash");
    let input = Endpoint::new_memory(&in_topic, 10);
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |_msg: MyTypedMessage| async move {
            // Use a connection error to trigger an intentional route crash/restart
            let res: Result<Handled, mq_bridge::HandlerError> = Err(
                mq_bridge::HandlerError::Connection(anyhow::anyhow!("Temporary failure")),
            );
            res
        },
    );

    let in_channel = route.input.channel().unwrap();
    let message = MyTypedMessage {
        id: 1,
        content: "retry".into(),
    };

    let canonical_message = msg!(&message, "my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // Should return Err because it's retryable and no middleware handles it
    assert!(res.is_err());
}

/// The handler sits outside every output middleware, so `retry` covers only the publish.
/// A `Retryable` handler error therefore propagates to the route instead of re-running
/// the handler.
#[tokio::test(flavor = "multi_thread")]
async fn test_retryable_handler_error_is_not_retried_by_output_middleware() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts_clone = attempts.clone();

    let in_topic = get_unique_topic("in_retry_handler_once");
    let out_topic = get_unique_topic("out_retry_handler_once");
    let input = Endpoint::new_memory(&in_topic, 10);
    let mut output = Endpoint::new_memory(&out_topic, 10);

    output.middlewares.push(Middleware::Retry(RetryMiddleware {
        max_attempts: 3,
        initial_interval_ms: 10,
        max_interval_ms: 100,
        multiplier: 1.0,
    }));

    let route = Route::new(input, output).add_handler("my_message", move |_msg: MyTypedMessage| {
        let attempts = attempts_clone.clone();
        async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            let res: Result<Handled, mq_bridge::HandlerError> = Err(
                mq_bridge::HandlerError::Retryable(anyhow::anyhow!("Temporary failure")),
            );
            res
        }
    });

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 1,
        content: "retry".into(),
    };
    let canonical_message = msg!(&message, "my_message");

    let route_name = "test_retryable_handler_error_is_not_retried_by_output_middleware";
    route.deploy(route_name).await.unwrap();

    in_channel.send_message(canonical_message).await.unwrap();
    let start = std::time::Instant::now();
    while attempts.load(Ordering::SeqCst) == 0 {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Timeout waiting for the handler to run");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // Comfortably longer than all three retry attempts (10ms apart) would take.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    Route::stop(route_name).await;

    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "retry middleware must not re-run the handler"
    );
    assert_eq!(out_channel.len(), 0);
}

#[tokio::test]
async fn test_route_with_typed_handler_failure_handler() {
    let in_topic = get_unique_topic("in_fail_handler");
    let out_topic = get_unique_topic("out_fail_handler");
    let input = Endpoint::new_memory(&in_topic, 10);
    let output = Endpoint::new_memory(&out_topic, 10);

    let route = Route::new(input, output).add_handler(
        "my_message",
        move |msg: MyTypedMessage| async move {
            assert_eq!(msg.id, 456);
            Result::<(), mq_bridge::HandlerError>::Err(mq_bridge::HandlerError::NonRetryable(
                anyhow::anyhow!("Handler failed as expected"),
            ))
        },
    );

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let message = MyTypedMessage {
        id: 456,
        content: "world".into(),
    };

    let canonical_message = msg!(&message, "my_message");

    in_channel.send_message(canonical_message).await.unwrap();
    in_channel.close();

    let res = route.run_until_err("test", None, None).await;

    // The error is non-retryable, so it is logged and the message is dropped. The route continues.
    assert!(res.is_ok());

    // No message should be published to the output
    assert_eq!(out_channel.len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_commit_concurrency_limit() {
    use mq_bridge::CanonicalMessage;
    use mq_bridge::{
        models::{Endpoint, Middleware, Route},
        traits::{ConsumerError, CustomMiddlewareFactory, MessageConsumer, ReceivedBatch},
    };
    use std::any::Any;
    use std::sync::Arc;
    use std::time::Duration;

    #[derive(Debug)]
    struct SlowCommitMiddleware {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for SlowCommitMiddleware {
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            struct Wrapper {
                inner: Box<dyn MessageConsumer>,
                delay: Duration,
            }

            #[async_trait::async_trait]
            impl MessageConsumer for Wrapper {
                async fn receive_batch(
                    &mut self,
                    max_messages: usize,
                ) -> Result<ReceivedBatch, ConsumerError> {
                    let mut batch = self.inner.receive_batch(max_messages).await?;
                    let original_commit = batch.commit;
                    let delay = self.delay;
                    batch.commit = Box::new(move |resp| {
                        Box::pin(async move {
                            tokio::time::sleep(delay).await;
                            original_commit(resp).await
                        })
                    });
                    Ok(batch)
                }
                fn as_any(&self) -> &dyn Any {
                    self
                }
            }
            Ok(Box::new(Wrapper {
                inner: consumer,
                delay: self.delay,
            }))
        }
    }

    let run_test_case = |limit: usize| async move {
        let test_id = fast_uuid_v7::gen_id();
        let factory = Arc::new(SlowCommitMiddleware {
            delay: Duration::from_millis(50),
        });
        let middleware_name = format!("slow_commit_{}_{}", limit, test_id);
        mq_bridge::route::register_middleware_factory(&middleware_name, factory).unwrap();

        let input = Endpoint::new_memory(&format!("in_limit_{}_{}", limit, test_id), 100)
            .add_middleware(Middleware::Custom {
                name: middleware_name.clone(),
                config: serde_json::Value::Null,
            });
        let output = Endpoint::new_memory(&format!("out_limit_{}_{}", limit, test_id), 200);
        let route = Route::new(input, output)
            .with_concurrency(2)
            .with_batch_size(4)
            .with_commit_concurrency_limit(limit);

        let in_channel = route.input.channel().unwrap();
        let out_channel = route.output.channel().unwrap();

        let expected: usize = 80;
        for i in 0..expected {
            in_channel
                .send_message(CanonicalMessage::from(format!("msg{}", i)))
                .await
                .unwrap();
        }

        let start = std::time::Instant::now();
        let route_name = format!("test_commit_concurrency_limit_{}_{}", limit, test_id);
        route.deploy(&route_name).await.unwrap();

        // Add a timeout to prevent hanging forever (configurable via env var)
        let wait = async {
            let mut received: usize = 0;
            while received < expected {
                let batch = out_channel.drain_messages();
                if !batch.is_empty() {
                    received += batch.len();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let commit_timeout = test_env_secs("MQB_COMMIT_WAIT_TIMEOUT_SECS", 30);
        let timeout = tokio::time::timeout(commit_timeout, wait).await;
        assert!(
            timeout.is_ok(),
            "Timed out waiting for all messages to be committed"
        );

        let duration = start.elapsed();
        Route::stop(&route_name).await;
        (duration, expected)
    };

    // Regression: low limits must not stall/deadlock in concurrent mode.
    let (duration_low, expected_low) = run_test_case(1).await;
    assert!(
        duration_low < Duration::from_secs(20),
        "Low commit limit took too long (possible stall): {:?}",
        duration_low
    );
    assert_eq!(expected_low, 80);

    // Higher limits should also complete successfully.
    let (duration_high, expected_high) = run_test_case(32).await;
    assert!(
        duration_high < Duration::from_secs(20),
        "High commit limit took too long: {:?}",
        duration_high
    );
    assert_eq!(expected_high, 80);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_delay_middleware_in_route() {
    use mq_bridge::models::{DelayMiddleware, Endpoint, EndpointType, Middleware, Route};
    use std::time::{Duration, Instant};

    // Input: Static consumer that produces "hello"
    // We apply delay middleware to it.
    let input = Endpoint::new(EndpointType::Static("hello".into()))
        .add_middleware(Middleware::Delay(DelayMiddleware { delay_ms: 100 }));

    // Output: Memory
    let out_topic = get_unique_topic("delay_route_out");
    let output = Endpoint::new_memory(&out_topic, 100);

    let route = Route::new(input, output).with_batch_size(1);
    let out_channel = route.output.channel().unwrap();

    let route_name = format!("test_delay_middleware_{}", fast_uuid_v7::gen_id());
    let start = Instant::now();

    route.deploy(&route_name).await.unwrap();

    // Allow route task to start; configurable via env var to avoid slowing defaults
    let init_delay = test_env_ms("MQB_TEST_INIT_DELAY_MS", 200);
    tokio::time::sleep(init_delay).await;

    // Wait for 3 messages.
    // 1st message: delay 100ms -> receive -> send.
    // 2nd message: delay 100ms -> receive -> send.
    // 3rd message: delay 100ms -> receive -> send.
    // Total time should be around 300ms + overhead.

    while out_channel.len() < 3 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        if start.elapsed() > Duration::from_secs(2) {
            panic!("Timeout waiting for delayed messages");
        }
    }

    let elapsed = start.elapsed();
    Route::stop(&route_name).await;

    // With 100ms delay, 3 messages should take at least 300ms.
    assert!(
        elapsed >= Duration::from_millis(300),
        "Route was too fast: {:?}",
        elapsed
    );
}

#[tokio::test]
async fn test_custom_endpoint_factory_programmatic() {
    use mq_bridge::models::{Endpoint, EndpointType, Route};
    use mq_bridge::traits::{CustomEndpointFactory, MessageConsumer, MessagePublisher};
    use std::sync::Arc;

    #[derive(Debug)]
    struct MyFactory;

    #[async_trait::async_trait]
    impl CustomEndpointFactory for MyFactory {
        async fn create_consumer(
            &self,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            Ok(Box::new(
                mq_bridge::endpoints::structural::static_endpoint::StaticRequestConsumer::new(
                    &mq_bridge::models::StaticConfig::from("custom_msg"),
                )
                .unwrap(),
            ))
        }
        async fn create_publisher(
            &self,
            _route_name: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(
                mq_bridge::endpoints::structural::null::NullPublisher,
            ))
        }
    }

    mq_bridge::route::register_endpoint_factory("my_factory", Arc::new(MyFactory)).unwrap();

    let input = Endpoint::new(EndpointType::Custom {
        name: "my_factory".to_string(),
        config: serde_json::Value::Null,
    });
    let output = Endpoint::new(EndpointType::Custom {
        name: "my_factory".to_string(),
        config: serde_json::Value::Null,
    });

    let route = Route::new(input, output);

    // Run the route for a short duration to ensure it initializes and processes messages
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        route.run_until_err("custom_test", None, None),
    )
    .await;

    // We expect a timeout because StaticConsumer produces infinitely and NullPublisher accepts infinitely.
    assert!(
        result.is_err(),
        "Route should have run indefinitely until timeout"
    );
}

#[tokio::test]
async fn test_custom_components_yaml_configuration() {
    use mq_bridge::models::{Config, EndpointType, Middleware};
    use mq_bridge::route::{register_endpoint_factory, register_middleware_factory};
    use mq_bridge::traits::{
        CustomEndpointFactory, CustomMiddlewareFactory, MessageConsumer, MessagePublisher,
    };
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    // Define Custom Factories
    #[derive(Debug)]
    struct YamlEndpointFactory;
    #[async_trait::async_trait]
    impl CustomEndpointFactory for YamlEndpointFactory {
        async fn create_consumer(
            &self,
            _route: &str,
            config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            let content = config
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            Ok(Box::new(
                mq_bridge::endpoints::structural::static_endpoint::StaticRequestConsumer::new(
                    &mq_bridge::models::StaticConfig::from(content),
                )
                .unwrap(),
            ))
        }
        async fn create_publisher(
            &self,
            _route: &str,
            _config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessagePublisher>> {
            Ok(Box::new(
                mq_bridge::endpoints::structural::null::NullPublisher,
            ))
        }
    }

    #[derive(Debug)]
    struct YamlMiddlewareFactory {
        flag: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl CustomMiddlewareFactory for YamlMiddlewareFactory {
        async fn apply_consumer(
            &self,
            consumer: Box<dyn MessageConsumer>,
            _route: &str,
            config: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn MessageConsumer>> {
            if config.get("active").and_then(|v| v.as_bool()) == Some(true) {
                self.flag.store(true, Ordering::SeqCst);
            }
            Ok(consumer)
        }
    }

    let mw_flag = Arc::new(AtomicBool::new(false));
    register_endpoint_factory("my_yaml_endpoint", Arc::new(YamlEndpointFactory)).unwrap();
    register_middleware_factory(
        "my_yaml_middleware",
        Arc::new(YamlMiddlewareFactory {
            flag: mw_flag.clone(),
        }),
    )
    .unwrap();

    let yaml = r#"
    yaml_test_route:
      input:
        middlewares:
          - my_yaml_middleware:
              active: true
        my_yaml_endpoint:
          content: "yaml_msg"
      output:
        my_yaml_endpoint: {}
    "#;

    let config: Config = serde_yaml_ng::from_str(yaml).expect("Failed to parse YAML");
    let route = config
        .get("yaml_test_route")
        .expect("Route not found")
        .clone();

    if let EndpointType::Custom { name, config } = &route.input.endpoint_type {
        assert_eq!(name, "my_yaml_endpoint");
        assert_eq!(config["content"], "yaml_msg");
    } else {
        panic!("Input endpoint should be Custom");
    }

    if let Middleware::Custom { name, config } = &route.input.middlewares[0] {
        assert_eq!(name, "my_yaml_middleware");
        assert_eq!(config["active"], true);
    } else {
        panic!("Input middleware should be Custom");
    }

    // Run Route to verify factory resolution
    let handle = tokio::spawn(async move { route.run_until_err("yaml_test", None, None).await });

    // Give it a moment to initialize and process one message
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Check if middleware ran
    assert!(
        mw_flag.load(Ordering::SeqCst),
        "Middleware should have been executed"
    );

    handle.abort();
}

pub async fn test_memory_request_reply() {
    use fast_uuid_v7::gen_id_str;
    use mq_bridge::endpoints::memory::MemoryPublisher;
    use mq_bridge::models::Endpoint;
    use mq_bridge::models::MemoryConfig;
    use mq_bridge::traits::Sent;
    use mq_bridge::CanonicalMessage;
    use mq_bridge::Handled;
    use mq_bridge::Route;

    let topic = format!("mem_rr_topic_{}", gen_id_str());
    let input_endpoint = Endpoint::new_memory(&topic, 10);
    let output_endpoint = Endpoint::new_response();
    let handler = |mut msg: CanonicalMessage| async move {
        let request_payload = msg.get_payload_str();
        let response_payload = format!("reply to {}", request_payload);
        msg.set_payload_str(response_payload);
        Ok(Handled::Publish(msg))
    };

    let route = Route::new(input_endpoint, output_endpoint).with_handler(handler);
    route.deploy("mem_rr_test").await.unwrap();

    // Create a publisher with request_reply = true
    let publisher = MemoryPublisher::new(&MemoryConfig {
        topic: topic.clone(),
        capacity: Some(10),
        request_reply: true,
        request_timeout_ms: Some(2000),
        ..Default::default()
    })
    .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        publisher.send("direct request".into()),
    )
    .await
    .expect("Timed out waiting for memory response")
    .unwrap();

    if let Sent::Response(response_msg) = result {
        assert_eq!(response_msg.get_payload_str(), "reply to direct request");
    } else {
        panic!("Expected Sent::Response, got {:?}", result);
    }

    // Clean up
    Route::stop("mem_rr_test").await;
    println!("Memory Request-Reply test passed!");
}

pub async fn test_memory_request_reply_multiple_sequential() {
    use fast_uuid_v7::gen_id_str;
    use mq_bridge::endpoints::memory::MemoryPublisher;
    use mq_bridge::models::{Endpoint, MemoryConfig};
    use mq_bridge::traits::Sent;
    use mq_bridge::CanonicalMessage;
    use mq_bridge::Handled;
    use mq_bridge::Route;

    let topic = format!("mem_rr_multi_seq_{}", gen_id_str());
    let input_endpoint = Endpoint::new_memory(&topic, 100);
    let output_endpoint = Endpoint::new_response();
    let handler = |mut msg: CanonicalMessage| async move {
        let request_payload = msg.get_payload_str();
        let response_payload = format!("reply to {}", request_payload);
        msg.set_payload_str(response_payload);
        Ok(Handled::Publish(msg))
    };

    let route = Route::new(input_endpoint, output_endpoint).with_handler(handler);
    route.deploy("mem_rr_multi_seq_test").await.unwrap();

    let publisher = MemoryPublisher::new(&MemoryConfig {
        topic: topic.clone(),
        capacity: Some(100),
        request_reply: true,
        request_timeout_ms: Some(5000),
        ..Default::default()
    })
    .unwrap();

    for i in 0..8 {
        let payload = format!("seq-{}", i);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            publisher.send(payload.clone().into()),
        )
        .await
        .expect("Timed out waiting for memory sequential response")
        .unwrap();
        if let Sent::Response(response_msg) = result {
            assert_eq!(
                response_msg.get_payload_str(),
                format!("reply to {}", payload)
            );
        } else {
            panic!("Expected Sent::Response, got {:?}", result);
        }
    }

    Route::stop("mem_rr_multi_seq_test").await;
    println!("Memory sequential multi-request request-reply test passed!");
}

pub async fn test_memory_request_reply_multiple_concurrent() {
    use fast_uuid_v7::gen_id_str;
    use mq_bridge::endpoints::memory::MemoryPublisher;
    use mq_bridge::models::{Endpoint, MemoryConfig};
    use mq_bridge::traits::Sent;
    use mq_bridge::CanonicalMessage;
    use mq_bridge::Handled;
    use mq_bridge::Route;

    let topic = format!("mem_rr_multi_con_{}", gen_id_str());
    let input_endpoint = Endpoint::new_memory(&topic, 100);
    let output_endpoint = Endpoint::new_response();
    let handler = |mut msg: CanonicalMessage| async move {
        let request_payload = msg.get_payload_str();
        // Small, deterministic jitter to increase out-of-order likelihood
        let mut sleep_ms = 0u64;
        if let Some(pos) = request_payload.rfind('-') {
            if let Ok(n) = request_payload[pos + 1..].parse::<u64>() {
                sleep_ms = (n % 5) * 10;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;

        let response_payload = format!("reply to {}", request_payload);
        msg.set_payload_str(response_payload);
        Ok(Handled::Publish(msg))
    };

    let route = Route::new(input_endpoint, output_endpoint).with_handler(handler);
    route.deploy("mem_rr_multi_con_test").await.unwrap();

    let publisher = MemoryPublisher::new(&MemoryConfig {
        topic: topic.clone(),
        capacity: Some(100),
        request_reply: true,
        request_timeout_ms: Some(5000),
        ..Default::default()
    })
    .unwrap();

    let mut handles = Vec::new();
    let n = 16usize;
    for i in 0..n {
        let p = publisher.clone();
        let payload = format!("con-{}", i);
        handles.push(tokio::spawn(async move {
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                p.send(payload.clone().into()),
            )
            .await
            .expect("Timed out waiting for memory concurrent response")
            .unwrap();
            match res {
                Sent::Response(resp) => Ok((i, resp.get_payload_str().to_string())),
                _ => Err(format!("Expected response for payload {}", payload)),
            }
        }));
    }

    for h in handles {
        let out = h.await.unwrap();
        match out {
            Ok((i, resp_payload)) => {
                assert_eq!(resp_payload, format!("reply to con-{}", i));
            }
            Err(e) => panic!("{}", e),
        }
    }

    Route::stop("mem_rr_multi_con_test").await;
    println!("Memory concurrent multi-request request-reply test passed!");
}

// --- Structural endpoints through normal configuration + factory wiring ---
//
// The structural endpoints have unit tests against hand-built mocks. These build
// them the way a user does: from `EndpointType` config, via the endpoint factory.

#[tokio::test]
async fn test_fanout_endpoint_delivers_batch_to_every_configured_destination() {
    use mq_bridge::endpoints::create_publisher_from_route;
    use mq_bridge::models::EndpointType;
    use mq_bridge::traits::SentBatch;

    let left = Endpoint::new_memory(&get_unique_topic("fanout_left"), 10);
    let right = Endpoint::new_memory(&get_unique_topic("fanout_right"), 10);
    let left_channel = left.channel().unwrap();
    let right_channel = right.channel().unwrap();

    let fanout = Endpoint::new(EndpointType::Fanout(vec![left, right]));
    let publisher = create_publisher_from_route("fanout_config_test", &fanout)
        .await
        .unwrap();

    let sent = publisher
        .send_batch(vec![
            CanonicalMessage::from("one"),
            CanonicalMessage::from("two"),
        ])
        .await
        .unwrap();
    assert!(matches!(sent, SentBatch::Ack));

    for channel in [&left_channel, &right_channel] {
        let payloads: Vec<String> = channel
            .drain_messages()
            .iter()
            .map(|m| m.get_payload_str().to_string())
            .collect();
        assert_eq!(payloads, ["one", "two"]);
    }
}

#[tokio::test]
async fn test_request_endpoint_forwards_response_from_configured_target() {
    use mq_bridge::endpoints::create_publisher_from_route;
    use mq_bridge::models::{EndpointType, RequestForwardConfig};

    let forward_to = Endpoint::new_memory(&get_unique_topic("request_forward"), 10);
    let forward_channel = forward_to.channel().unwrap();

    // `reader` over a `static` source is a request-capable target: its send returns
    // the read message as the response.
    let request = Endpoint::new(EndpointType::Request(RequestForwardConfig {
        to: Box::new(Endpoint::new(EndpointType::Reader(Box::new(
            Endpoint::new(EndpointType::Static("pong".into())),
        )))),
        forward_to: Box::new(forward_to),
    }));

    let publisher = create_publisher_from_route("request_config_test", &request)
        .await
        .unwrap();
    publisher
        .send(CanonicalMessage::from("ping"))
        .await
        .unwrap();

    let forwarded = forward_channel.drain_messages();
    assert_eq!(forwarded.len(), 1);
    assert_eq!(forwarded[0].get_payload_str(), "pong");
}

#[tokio::test]
async fn test_request_endpoint_rejects_a_target_that_returns_no_response() {
    use mq_bridge::endpoints::create_publisher_from_route;
    use mq_bridge::models::{EndpointType, RequestForwardConfig};

    // A plain memory sink acks without producing a response, so nothing could ever be
    // forwarded: the endpoint is refused instead of acking every message.
    let request = Endpoint::new(EndpointType::Request(RequestForwardConfig {
        to: Box::new(Endpoint::new_memory(
            &get_unique_topic("request_no_reply_target"),
            10,
        )),
        forward_to: Box::new(Endpoint::new_memory(
            &get_unique_topic("request_no_reply_forward"),
            10,
        )),
    }));
    let err = match create_publisher_from_route("request_no_reply_test", &request).await {
        Ok(_) => panic!("a request whose target cannot reply must be refused"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("`to` never returns a response"), "{err}");

    // Discarding the response is still a valid use: the target receives the request.
    let target = Endpoint::new_memory(&get_unique_topic("request_discard_target"), 10);
    let target_channel = target.channel().unwrap();
    let request = Endpoint::new(EndpointType::Request(RequestForwardConfig {
        to: Box::new(target),
        forward_to: Box::new(Endpoint::new(EndpointType::Null)),
    }));
    let publisher = create_publisher_from_route("request_discard_test", &request)
        .await
        .unwrap();
    publisher
        .send(CanonicalMessage::from("ping"))
        .await
        .unwrap();
    let received = target_channel.drain_messages();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].get_payload_str(), "ping");
}
