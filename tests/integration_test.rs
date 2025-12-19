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


#[cfg(feature = "mongodb")]
#[tokio::test]
async fn test_mongodb_performance_pipeline() {
    integration::mongodb::test_mongodb_performance_pipeline().await;
}

*/

#[tokio::test]
#[ignore] // This is a performance test, run it explicitly
async fn test_all_performance_direct() {
    // This instance will print the summary table when it's dropped at the end of the test.
    let _summary_printer = integration::common::PerformanceSummaryPrinter;

    println!("--- Running All Direct Performance Tests ---");
    println!("Tests are run sequentially to ensure accurate measurements.");

    #[cfg(feature = "nats")]
    {
        println!("\n\n>>> Starting NATS Direct Performance Test...");
        integration::nats::test_nats_performance_direct().await;
    }
    #[cfg(feature = "mongodb")]
    {
        println!("\n\n>>> Starting MongoDB Direct Performance Test...");
        integration::mongodb::test_mongodb_performance_direct().await;
    }
    #[cfg(feature = "amqp")]
    {
        println!("\n\n>>> Starting AMQP Direct Performance Test...");
        integration::amqp::test_amqp_performance_direct().await;
    }
    #[cfg(feature = "kafka")]
    {
        println!("\n\n>>> Starting Kafka Direct Performance Test...");
        integration::kafka::test_kafka_performance_direct().await;
    }
    #[cfg(feature = "mqtt")]
    {
        println!("\n\n>>> Starting MQTT Direct Performance Test...");
        integration::mqtt::test_mqtt_performance_direct().await;
    }

    // The summary table will be printed here when `_summary_printer` is dropped.
}
