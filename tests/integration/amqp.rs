#![allow(dead_code)]

use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
use mq_bridge::test_utils::{
    PERF_TEST_MESSAGE_COUNT, add_performance_result, run_chaos_pipeline_test, run_direct_perf_test, run_performance_pipeline_test, run_pipeline_test, run_test_with_docker, run_test_with_docker_controller, setup_logging, verify_subscriber_logic
};
use std::sync::Arc;

const CONFIG_YAML: &str = r#"
routes:
  memory_to_amqp:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "amqp-test-in" }
    output:
      middlewares:
        - retry:
            max_attempts: 10
            initial_interval_ms: 500
            max_interval_ms: 2000
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp" }

  amqp_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      amqp: { url: "amqp://guest:guest@localhost:5672/%2f", queue: "test_queue_amqp", prefetch_count: 1000 }
    output:
      memory: { topic: "amqp-test-out", capacity: {out_capacity} }
"#;

pub async fn test_amqp_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_pipeline_test("AMQP", &config_yaml).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_publisher_handles_nack() {
    use mq_bridge::traits::MessagePublisher;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let nack_queue = "test_nack_queue";
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(nack_queue.to_string()),
            no_declare_queue: true, // The test manually declares the queue with special args
            ..Default::default()
        };

        let conn = lapin::Connection::connect(&config.url, lapin::ConnectionProperties::default())
            .await
            .unwrap();
        let channel = conn.create_channel().await.unwrap();
        // Manually create a queue that will cause a NACK.
        // A queue with max-length 0 and overflow "reject-publish" will reject messages.
        let mut args = lapin::types::FieldTable::default();
        args.insert("x-max-length".into(), lapin::types::AMQPValue::LongInt(0));
        args.insert(
            "x-overflow".into(),
            lapin::types::AMQPValue::LongString("reject-publish".into()),
        );
        channel
            .queue_declare(
                nack_queue,
                lapin::options::QueueDeclareOptions::default(),
                args,
            )
            .await
            .unwrap();

        // Create our publisher
        let publisher = AmqpPublisher::new(&config).await.unwrap();

        // Send a message that should be NACKed
        let msg = mq_bridge::CanonicalMessage::from("this will be nacked");
        let result = publisher.send(msg).await;

        // Assert that we received a Retryable error because of the NACK
        assert!(result.is_err(), "Expected send to fail with a NACK");
        let err = result.unwrap_err();
        assert!(matches!(
            err,
            mq_bridge::traits::PublisherError::Retryable(_)
        ));
        assert!(
            err.to_string().contains("Broker Nacked the message"),
            "Error message should indicate a NACK"
        );

        println!("AMQP NACK handling test passed!");
    })
    .await;
}

pub async fn test_amqp_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let exchange = format!("sub_logic_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            exchange: Some(exchange),
            subscribe_mode: true,
            ..Default::default()
        };

        let publisher = Arc::new(AmqpPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(AmqpConsumer::new(&config).await.unwrap()));
        let sub2 = Arc::new(tokio::sync::Mutex::new(AmqpConsumer::new(&config).await.unwrap()));

        verify_subscriber_logic(publisher, sub1, sub2).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_publisher_handles_disconnect() {
    use mq_bridge::models::{
        Endpoint, EndpointType, FaultMode, Middleware, RandomPanicMiddleware, RetryMiddleware,
    };
    use mq_bridge::Route;

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let in_topic = "amqp_disconnect_in";
        let out_queue = "amqp_disconnect_out";
        let verify_topic = "amqp_disconnect_verify";

        // 1. The route that will experience the fault.
        // The input needs NACK support to re-deliver the message after the route restarts.
        let mut input_config = mq_bridge::models::MemoryConfig::new(in_topic, Some(10));
        input_config.enable_nack = true;
        let input_ep = Endpoint::new(EndpointType::Memory(input_config));

        let output_ep = Endpoint::new(EndpointType::Amqp(mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(out_queue.to_string()),
            ..Default::default()
        }))
        .add_middleware(Middleware::RandomPanic(RandomPanicMiddleware {
            mode: FaultMode::Disconnect,
            trigger_on_message: Some(1),
            enabled: true,
            ..Default::default()
        }))
        .add_middleware(Middleware::Retry(RetryMiddleware {
            max_attempts: 2,
            initial_interval_ms: 10,
            ..Default::default()
        }));

        let route_to_test = Route::new(input_ep.clone(), output_ep);
        route_to_test.deploy("amqp_fault_test").await.unwrap();

        // 2. A verifier route to get the message out of AMQP.
        let amqp_input_ep = Endpoint::new(EndpointType::Amqp(mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(out_queue.to_string()),
            ..Default::default()
        }));
        let verify_output_ep = Endpoint::new_memory(verify_topic, 10);
        let verifier_route = Route::new(amqp_input_ep, verify_output_ep.clone());
        verifier_route.deploy("amqp_verifier").await.unwrap();

        // 3. Send a message.
        let input_channel = input_ep.channel().unwrap();
        let test_payload = "this message should survive a disconnect";
        input_channel
            .send_message(test_payload.into())
            .await
            .unwrap();

        // 4. Wait for the route to fail and restart.
        // The fault is injected -> NonRetryable error -> route restarts after 5s.
        // 6 seconds should be enough for recovery and processing.
        println!("Waiting for route to recover from simulated disconnect...");
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;

        // 5. Verify the message arrived at the final destination.
        let verify_channel = verify_output_ep.channel().unwrap();
        let received_msgs = verify_channel.drain_messages();

        assert_eq!(
            received_msgs.len(),
            1,
            "Expected exactly one message to be received after recovery"
        );
        assert_eq!(received_msgs[0].get_payload_str(), test_payload);

        println!("AMQP disconnect handling test passed!");

        // 6. Cleanup.
        Route::stop("amqp_fault_test").await;
        Route::stop("amqp_verifier").await;
    })
    .await;
}

pub async fn test_amqp_chaos() {
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/amqp.yml",
        |controller| async move {
            let config_yaml = CONFIG_YAML.replace(
                "{out_capacity}",
                &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
            );
            run_chaos_pipeline_test("AMQP", &config_yaml, controller, "rabbitmq").await;
        },
    )
    .await;
}

pub async fn test_amqp_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config_yaml = CONFIG_YAML.replace(
            "{out_capacity}",
            &(PERF_TEST_MESSAGE_COUNT + 1000).to_string(),
        );
        run_performance_pipeline_test("AMQP", &config_yaml, PERF_TEST_MESSAGE_COUNT).await;
    })
    .await;
}

pub async fn test_amqp_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let queue = "perf_test_amqp_direct";
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            delayed_ack: false,
            prefetch_count: Some(1000),
            ..Default::default()
        };

        let result = run_direct_perf_test(
            "AMQP",
            || async {
                let mut pub_config = config.clone();
                pub_config.queue = Some(queue.to_string());
                Arc::new(AmqpPublisher::new(&pub_config).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.queue = Some(queue.to_string());
                endpoint.subscribe_mode = false;

                Arc::new(tokio::sync::Mutex::new(
                    AmqpConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;
        add_performance_result(result);
    })
    .await;
}

pub async fn test_amqp_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/amqp.yml",
        |controller| async move {
            let queue = "status_test_amqp";
            let config = mq_bridge::models::AmqpConfig {
                url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
                queue: Some(queue.to_string()),
                ..Default::default()
            };

            let publisher = AmqpPublisher::new(&config).await.unwrap();
            let consumer = AmqpConsumer::new(&config).await.unwrap();

            println!("[AMQP] Checking initial status...");
            sleep(Duration::from_secs(2)).await;
            let pub_status = publisher.status().await;
            let con_status = consumer.status().await;
            assert!(
                pub_status.healthy,
                "Publisher should be healthy initially. Status: {:?}",
                pub_status
            );
            assert!(
                con_status.healthy,
                "Consumer should be healthy initially. Status: {:?}",
                con_status
            );
            println!("[AMQP] Initial status check OK.");

            controller.stop_service("rabbitmq");
            println!("[AMQP] Service 'rabbitmq' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[AMQP] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!(
                        "[AMQP] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("rabbitmq");
            println!("[AMQP] Service 'rabbitmq' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                // Create new instances to force reconnection
                if let (Ok(p), Ok(c)) = (
                    AmqpPublisher::new(&config).await,
                    AmqpConsumer::new(&config).await,
                ) {
                    if p.status().await.healthy && c.status().await.healthy {
                        println!("[AMQP] Reconnect detected.");
                        break;
                    }
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!("[AMQP] Timeout waiting for reconnect.");
                }
                sleep(Duration::from_secs(2)).await;
            }
            println!("[AMQP] Status test successful.");
        },
    )
    .await;
}
