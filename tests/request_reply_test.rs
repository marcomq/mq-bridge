mod integration;

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore]
async fn test_kafka_request_reply() {
    integration::kafka::test_kafka_request_reply().await;
}

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore]
async fn test_nats_request_reply() {
    integration::nats::test_nats_request_reply().await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore]
async fn test_mongodb_request_reply() {
    integration::mongodb::test_mongodb_request_reply().await;
}
