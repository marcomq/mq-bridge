// To run these tests, use the command from the project root:
// cargo test --test integration_test -- --ignored --nocapture --test-threads=1

mod integration;
mod request_reply_test;

#[allow(dead_code)]
fn should_run(test_name: &str) -> bool {
    let filter = std::env::var("MQB_TEST_BACKEND")
        .unwrap_or_default()
        .to_lowercase();
    if filter.is_empty() {
        return true;
    }
    test_name.to_lowercase().contains(&filter)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose"]
async fn test_all_subscriber_logic() {
   println!("--- Running All Subscriber and Request-Reply Logic Tests ---");

    // --- Subscriber Logic ---
    #[cfg(feature = "ibm-mq")]
    {
        if should_run("ibm-mq") {
            println!("\n\n>>> Starting IBM MQ Subscriber Logic Test...");
            integration::ibm_mq::test_ibm_mq_subscriber_logic().await;
        }
    }
    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Subscriber Logic Test...");
            integration::kafka::test_kafka_subscriber_logic().await;
        }
    }
    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Subscriber Logic Test...");
            integration::mongodb::test_mongodb_subscriber_logic().await;
        }
    }
    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Subscriber Logic Test...");
            integration::amqp::test_amqp_subscriber_logic().await;
        }
    }
    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Subscriber Logic Test...");
            integration::nats::test_nats_subscriber_logic().await;
        }
    }
    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Subscriber Logic Test...");
            integration::mqtt::test_mqtt_subscriber_logic().await;
        }
    }
    if should_run("file") {
        println!("\n\n>>> Starting File Subscriber Logic Test...");
        integration::file::test_file_subscriber_logic().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"] // This is a performance test, run it explicitly
async fn test_all_performance_direct() {
    // This instance will print the summary table when it's dropped at the end of the test.
    let _summary_printer = mq_bridge::test_utils::PerformanceSummaryPrinter;

    println!("--- Running All Direct Performance Tests ---");
    println!("Tests are run sequentially to ensure accurate measurements.");

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb_rs") {
            println!("\n\n>>> Starting MongoDB Replica Set Direct Performance Test...");
            integration::mongodb::test_mongodb_replica_set_performance_direct().await;
        }
        if should_run("mongodb_direct") {
            println!("\n\n>>> Starting MongoDB Direct Performance Test...");
            integration::mongodb::test_mongodb_performance_direct().await;
        }
    }
    #[cfg(feature = "aws")]
    {
        if should_run("aws") {
            println!("\n\n>>> Starting AWS Direct Performance Test...");
            integration::aws::test_aws_performance_direct().await;
        }
    }
    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Direct Performance Test...");
            integration::nats::test_nats_performance_direct().await;
        }
    }
    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Direct Performance Test...");
            integration::mqtt::test_mqtt_performance_direct().await;
        }
    }
    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Direct Performance Test...");
            integration::kafka::test_kafka_performance_direct().await;
        }
    }
    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Direct Performance Test...");
            integration::amqp::test_amqp_performance_direct().await;
        }
    }
    #[cfg(feature = "ibm-mq")]
    {
        if should_run("ibm-mq") {
            println!("\n\n>>> Starting IBM MQ Direct Performance Test...");
            integration::ibm_mq::test_ibm_mq_performance_direct().await;
        }
    }
    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") {
            println!("\n\n>>> Starting SQLx Direct Performance Test...");
            integration::sqlx::test_sqlx_performance_direct().await;
        }
    }

    // The summary table will be printed here when `_summary_printer` is dropped.
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_chaos() {
    println!("--- Running All Chaos Tests ---");
    println!("Tests are run sequentially.");

    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Chaos Test...");
            integration::kafka::test_kafka_chaos().await;
        }
    }

    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Chaos Test...");
            integration::nats::test_nats_chaos().await;
        }
    }

    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Chaos Test...");
            integration::amqp::test_amqp_chaos().await;
        }
    }

    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Chaos Test...");
            // MQTT chaos tests are currently flaky due to issues with session persistence/QoS handling
            // in the test environment (Mosquitto + rumqttc).
            integration::mqtt::test_mqtt_chaos().await;
        }
    }

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Chaos Test...");
            integration::mongodb::test_mongodb_chaos().await;
        }
    }

    #[cfg(feature = "ibm-mq")]
    {
        if should_run("ibm-mq") {
            println!("\n\n>>> Starting IBM MQ Chaos Test...");
            integration::ibm_mq::test_ibm_mq_chaos().await;
        }
    }

    // AWS chaos test is excluded by default as it requires LocalStack which can be heavy/flaky in some envs
    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") {
            println!("\n\n>>> Starting SQLx Chaos Test...");
            integration::sqlx::test_sqlx_chaos().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose, takes long time to run"]
async fn test_all_status() {
    println!("--- Running All Status Tests ---");
    println!("Tests are run sequentially.");

    #[cfg(feature = "kafka")]
    {
        if should_run("kafka") {
            println!("\n\n>>> Starting Kafka Status Test...");
            integration::kafka::test_kafka_status().await;
        }
    }

    #[cfg(feature = "nats")]
    {
        if should_run("nats") {
            println!("\n\n>>> Starting NATS Status Test...");
            integration::nats::test_nats_status().await;
        }
    }

    #[cfg(feature = "amqp")]
    {
        if should_run("amqp") {
            println!("\n\n>>> Starting AMQP Status Test...");
            integration::amqp::test_amqp_status().await;
        }
    }

    #[cfg(feature = "mqtt")]
    {
        if should_run("mqtt") {
            println!("\n\n>>> Starting MQTT Status Test...");
            integration::mqtt::test_mqtt_status().await;
        }
    }

    #[cfg(feature = "mongodb")]
    {
        if should_run("mongodb") {
            println!("\n\n>>> Starting MongoDB Status Test...");
            integration::mongodb::test_mongodb_status().await;
        }
    }

    #[cfg(feature = "ibm-mq")]
    {
        if should_run("ibm-mq") {
            println!("\n\n>>> Starting IBM MQ Status Test...");
            integration::ibm_mq::test_ibm_mq_status().await;
        }
    }

    #[cfg(feature = "sqlx")]
    {
        if should_run("sqlx") {
            println!("\n\n>>> Starting SQLx Status Test...");
            integration::sqlx::test_sqlx_status().await;
        }
    }

    #[cfg(feature = "aws")]
    {
        if should_run("aws") {
            println!("\n\n>>> Starting AWS Status Test...");
            integration::aws::test_aws_status().await;
        }
    }
}
