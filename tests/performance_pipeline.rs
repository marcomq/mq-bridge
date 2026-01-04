mod integration;

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore]
async fn test_kafka_performance_pipeline() {
    integration::kafka::test_kafka_performance_pipeline().await;
}
#[cfg(feature = "aws")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)] // aws connect has its own task
#[ignore]
async fn test_aws_performance_pipeline() {
    integration::aws::test_aws_performance_pipeline().await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
#[ignore]
async fn test_amqp_performance_pipeline() {
    integration::amqp::test_amqp_performance_pipeline().await;
}
#[cfg(feature = "mqtt")]
#[tokio::test]
#[ignore]
async fn test_mqtt_performance_pipeline() {
    integration::mqtt::test_mqtt_performance_pipeline().await;
}

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore]
async fn test_nats_performance_pipeline() {
    integration::nats::test_nats_performance_pipeline().await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore]
async fn test_mongodb_performance_pipeline() {
    integration::mongodb::test_mongodb_performance_pipeline().await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore]
async fn test_mongodb_replica_set_pipeline() {
    integration::mongodb::test_mongodb_replica_set_pipeline().await;
}
