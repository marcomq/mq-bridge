#![allow(unused_imports, dead_code)]
mod integration;

use integration::common::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use mq_bridge::CanonicalMessage;

/// Helper to run a simple service loop that receives one message and replies.
async fn run_service_reply(mut consumer: Box<dyn MessageConsumer>, response_payload: &[u8]) {
    let receive_future = consumer.receive();
    match tokio::time::timeout(std::time::Duration::from_secs(5), receive_future).await {
        Ok(Ok(received)) => {
            let response = CanonicalMessage::new(response_payload.to_vec(), None);
            let _ = (received.commit)(Some(response)).await;
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
            brokers: "localhost:9092".to_string(),
            group_id: Some("req_rep_group".to_string()),
            producer_options: Some(vec![("acks".to_string(), "1".to_string())]),
            ..Default::default()
        };

        let client_publisher = KafkaPublisher::new(&config, request_topic).await.unwrap();
        let _ = KafkaPublisher::new(&config, reply_topic).await.unwrap();
        let service_consumer = KafkaConsumer::new(&config, request_topic).await.unwrap();

        let mut reply_config = config.clone();
        reply_config.group_id = Some("reply_group".to_string());
        let mut client_consumer = KafkaConsumer::new(&reply_config, reply_topic)
            .await
            .unwrap();

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

        let service_consumer = NatsConsumer::new(&service_config, "ignored", subject)
            .await
            .unwrap();
        let publisher = NatsPublisher::new(&client_config, stream_name, subject)
            .await
            .unwrap();

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
async fn test_mongodb_request_reply() {
    use mq_bridge::endpoints::mongodb::{MongoDbConsumer, MongoDbPublisher};
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/mongodb.yml", || async {
        let req_collection = "req_collection";
        let reply_collection = "reply_collection";
        let db_name = "mq_bridge_test_req_rep";

        let config = mq_bridge::models::MongoDbConfig {
            url: "mongodb://localhost:27017".to_string(),
            database: db_name.to_string(),
            ..Default::default()
        };

        let publisher = MongoDbPublisher::new(&config, req_collection)
            .await
            .unwrap();
        let consumer = MongoDbConsumer::new(&config, req_collection).await.unwrap();

        tokio::spawn(async move {
            run_service_reply(Box::new(consumer), b"response_payload").await;
        });

        let mut msg = CanonicalMessage::new(b"request_payload".to_vec(), None);
        msg.metadata
            .insert("reply_to".to_string(), reply_collection.to_string());
        publisher.send(msg).await.unwrap();

        // Verify reply in DB
        let client = mongodb::Client::with_uri_str(&config.url).await.unwrap();
        let db = client.database(db_name);
        let coll = db.collection::<mongodb::bson::Document>(reply_collection);

        let mut found = false;
        for _ in 0..20 {
            if let Ok(Some(doc)) = coll.find_one(mongodb::bson::doc! {}).await {
                if let Ok(binary) = doc.get_binary_generic("payload") {
                    if binary == b"response_payload" {
                        found = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(found, "Reply not found in collection");
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

        let client_publisher = AmqpPublisher::new(&config, req_queue).await.unwrap();
        let mut client_consumer = AmqpConsumer::new(&config, reply_queue).await.unwrap();
        let service_consumer = AmqpConsumer::new(&config, req_queue).await.unwrap();

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

        let client_publisher = MqttPublisher::new(&config, req_topic, "client_pub")
            .await
            .unwrap();
        let mut client_consumer = MqttConsumer::new(&config, reply_topic, "client_sub")
            .await
            .unwrap();
        let service_consumer = MqttConsumer::new(&config, req_topic, "service_sub")
            .await
            .unwrap();

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
