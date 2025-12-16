// To run these tests, use the command from the project root:
// cargo test --test integration_test -- --ignored --nocapture --test-threads=1

mod integration;

/*
#[cfg(all(
    feature = "nats",
    feature = "kafka",
    feature = "amqp",
    feature = "mqtt",
    feature = "http"
))]
#[tokio::test]
async fn test_all_pipelines_together() {
    // integration::all_endpoints::test_all_pipelines_together().await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
async fn test_amqp_pipeline() {
    integration::amqp::test_amqp_pipeline().await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
async fn test_amqp_performance_pipeline() {
    integration::amqp::test_amqp_performance_pipeline().await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
async fn test_amqp_performance_direct() {
    integration::amqp::test_amqp_performance_direct().await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
async fn test_kafka_pipeline() {
    integration::kafka::test_kafka_pipeline().await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
async fn test_kafka_performance_pipeline() {
    integration::kafka::test_kafka_performance_pipeline().await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
async fn test_kafka_performance_direct() {
    integration::kafka::test_kafka_performance_direct().await;
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn test_mqtt_pipeline() {
    integration::mqtt::test_mqtt_pipeline().await;
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn test_mqtt_performance_pipeline() {
    integration::mqtt::test_mqtt_performance_pipeline().await;
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn test_mqtt_performance_direct() {
    integration::mqtt::test_mqtt_performance_direct().await;
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn test_nats_pipeline() {
    integration::nats::test_nats_pipeline().await;
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn test_nats_performance_pipeline() {
    integration::nats::test_nats_performance_pipeline().await;
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn test_nats_performance_direct() {
    integration::nats::test_nats_performance_direct().await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
async fn test_mongodb_performance_pipeline() {
    integration::mongodb::test_mongodb_performance_pipeline().await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
async fn test_mongodb_performance_direct() {
    integration::mongodb::test_mongodb_performance_direct().await;
}
*/

#[tokio::test]
async fn test_memory_performance_pipeline() {
    integration::memory::test_memory_performance_pipeline().await;
}
