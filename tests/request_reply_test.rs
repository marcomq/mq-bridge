#![allow(unused_imports, dead_code)]
mod integration;

use mq_bridge::test_utils::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use mq_bridge::CanonicalMessage;

/// Helper to run a simple service loop that receives one message and replies.
async fn run_service_reply(mut consumer: Box<dyn MessageConsumer>, response_payload: &[u8]) {
    let receive_future = consumer.receive();
    match tokio::time::timeout(std::time::Duration::from_secs(5), receive_future).await {
        Ok(Ok(received)) => {
            let response = CanonicalMessage::new(response_payload.to_vec(), None);
            let _ = (received.commit)(mq_bridge::traits::MessageDisposition::Reply(response)).await;
        }
        Ok(Err(e)) => panic!("Service consumer failed to receive: {:?}", e),
        Err(_) => panic!("Service consumer receive timed out"),
    }
}

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore]
async fn test_kafka_request_reply() {
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

        let received_resp = client_consumer.receive().await.unwrap();
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

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore]
async fn test_nats_request_reply() {
    use mq_bridge::endpoints::nats::{NatsConsumer, NatsPublisher};
    use mq_bridge::traits::Sent;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/nats.yml", || async {
        let subject = "req_rep_subject";
        let stream_name = "req_rep_stream";

        let service_config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            no_jetstream: true,
            ..Default::default()
        };

        let client_config = mq_bridge::models::NatsConfig {
            url: "nats://localhost:4222".to_string(),
            request_reply: true,
            ..Default::default()
        };

        let mut service_endpoint = service_config;
        service_endpoint.subject = Some(subject.to_string());
        service_endpoint.stream = Some("ignored".to_string());
        let service_consumer = NatsConsumer::new(&service_endpoint).await.unwrap();

        let mut pub_config = client_config.clone();
        pub_config.subject = Some(subject.to_string());
        pub_config.stream = Some(stream_name.to_string());
        let publisher = NatsPublisher::new(&pub_config).await.unwrap();

        tokio::spawn(async move {
            run_service_reply(Box::new(service_consumer), b"pong").await;
        });

        let msg = CanonicalMessage::new(b"ping".to_vec(), None);
        let result = publisher.send(msg).await.unwrap();

        match result {
            Sent::Response(resp) => {
                assert_eq!(resp.payload.to_vec(), b"pong");
            }
            _ => panic!("Expected response"),
        }
        println!("NATS Request-Reply test passed!");
    })
    .await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore]
async fn test_mongodb_request_reply_pattern() {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    use mq_bridge::traits::Sent;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let req_collection = "req_rep_collection";
        let db_name = "mq_bridge_test_req_rep";

        // 1. Setup the "service" side (the consumer that replies)
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

        // 2. Setup the "client" side (the publisher that sends and waits)
        let client_config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.to_string(),
            request_reply: true, // Enable request-reply mode
            ..Default::default()
        };
        let mut pub_config = client_config.clone();
        pub_config.collection = Some(req_collection.to_string());
        let client_publisher = MongoDbPublisher::new(&pub_config).await.unwrap();

        // 3. Send request and wait for response
        let request_msg = CanonicalMessage::new(b"mongo_request".to_vec(), None);
        let result = client_publisher.send(request_msg).await.unwrap();

        // 4. Assert the response
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

#[cfg(feature = "amqp")]
#[tokio::test]
#[ignore]
async fn test_amqp_request_reply() {
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

        client_publisher.send(req_msg).await.unwrap();

        let received_resp = client_consumer.receive().await.unwrap();

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
#[tokio::test]
#[ignore]
async fn test_mqtt_request_reply() {
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

        let mut service_config = config.clone();
        service_config.client_id = Some("service_sub".to_string());
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

        client_publisher.send(req_msg).await.unwrap();

        let received_resp = client_consumer.receive().await.unwrap();

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
