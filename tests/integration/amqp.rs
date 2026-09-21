#![allow(unused_imports, dead_code)]

use mq_bridge::endpoints::amqp::{AmqpConsumer, AmqpPublisher};
use mq_bridge::test_utils::{
    add_performance_result, run_chaos_pipeline_test, run_direct_perf_test,
    run_performance_pipeline_test, run_pipeline_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, should_run, verify_subscriber_logic,
    PERF_TEST_MESSAGE_COUNT,
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
    if !should_run("amqp") {
        return;
    }
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
                nack_queue.into(),
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

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_reply_publish_failure_does_not_ack_request() {
    if !should_run("amqp") {
        return;
    }
    use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher};

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let request_queue = format!("test_reply_fail_{}", fast_uuid_v7::gen_id_str());
        let missing_reply_queue = format!("missing_reply_{}", fast_uuid_v7::gen_id_str());
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(request_queue),
            ..Default::default()
        };

        let publisher = AmqpPublisher::new(&config).await.unwrap();

        {
            let mut consumer = AmqpConsumer::new(&config).await.unwrap();
            let mut request = mq_bridge::CanonicalMessage::from("request needing reply");
            request
                .metadata
                .insert("reply_to".to_string(), missing_reply_queue);
            request
                .metadata
                .insert("correlation_id".to_string(), "reply-fail-cid".to_string());

            publisher.send(request).await.unwrap();

            let received =
                tokio::time::timeout(std::time::Duration::from_secs(5), consumer.receive())
                    .await
                    .expect("Timed out waiting for AMQP request")
                    .unwrap();

            let result = (received.commit)(MessageDisposition::Reply(
                mq_bridge::CanonicalMessage::from("response that cannot be routed"),
            ))
            .await;

            assert!(
                result.is_err(),
                "Expected AMQP reply commit to fail when reply_to queue is missing"
            );
        }

        let mut retry_consumer = AmqpConsumer::new(&config).await.unwrap();
        let redelivered =
            tokio::time::timeout(std::time::Duration::from_secs(5), retry_consumer.receive())
                .await
                .expect("Timed out waiting for AMQP request redelivery")
                .unwrap();

        assert_eq!(
            redelivered.message.get_payload_str(),
            "request needing reply"
        );
        (redelivered.commit)(MessageDisposition::Ack).await.unwrap();
    })
    .await;
}

pub async fn test_amqp_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let queue = format!("sub_logic_{}", fast_uuid_v7::gen_id());
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(queue.clone()),
            subscribe_mode: true,
            ..Default::default()
        };

        let publisher = Arc::new(AmqpPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(
            AmqpConsumer::new(&config).await.unwrap(),
        ));
        let sub2 = Arc::new(tokio::sync::Mutex::new(
            AmqpConsumer::new(&config).await.unwrap(),
        ));
        // Give subscribers time to connect and finish the subscription
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        verify_subscriber_logic(publisher, sub1, sub2).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_publisher_handles_disconnect() {
    if !should_run("amqp") {
        return;
    }
    use mq_bridge::models::{
        Endpoint, EndpointType, FaultMode, Middleware, RandomPanicMiddleware, RetryMiddleware,
    };
    use mq_bridge::Route;

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let in_topic = "amqp_disconnect_in";
        let out_queue = "amqp_disconnect_out";
        let verify_topic = "amqp_disconnect_verify";

        // The route that will experience the fault.
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

        let route_to_test = Route::new(input_ep.clone(), output_ep).with_fault_injection(true);
        route_to_test.deploy("amqp_fault_test").await.unwrap();

        // A verifier route to get the message out of AMQP.
        let amqp_input_ep = Endpoint::new(EndpointType::Amqp(mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(out_queue.to_string()),
            ..Default::default()
        }));
        let verify_output_ep = Endpoint::new_memory(verify_topic, 10);
        let verifier_route = Route::new(amqp_input_ep, verify_output_ep.clone());
        verifier_route.deploy("amqp_verifier").await.unwrap();

        let input_channel = input_ep.channel().unwrap();
        let test_payload = "this message should survive a disconnect";
        input_channel
            .send_message(test_payload.into())
            .await
            .unwrap();

        // Wait for the route to fail and restart.
        // The fault is injected -> NonRetryable error -> route restarts after 5s.
        // 6 seconds should be enough for recovery and processing.
        println!("Waiting for route to recover from simulated disconnect...");
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;

        // Verify the message arrived at the final destination.
        let verify_channel = verify_output_ep.channel().unwrap();
        let received_msgs = verify_channel.drain_messages();

        assert_eq!(
            received_msgs.len(),
            1,
            "Expected exactly one message to be received after recovery"
        );
        assert_eq!(received_msgs[0].get_payload_str(), test_payload);

        println!("AMQP disconnect handling test passed!");

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

/// Identity survives an `mq-bridge → AMQP → mq-bridge` hop, and a redelivery keeps it. Before,
/// the publisher set no `message_id` property and the consumer fell back to the delivery tag,
/// which restarts at 1 per channel.
pub async fn test_amqp_message_id_round_trip() {
    use mq_bridge::traits::{MessageConsumer, MessageDisposition, MessagePublisher};
    use mq_bridge::CanonicalMessage;
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/amqp.yml", || async {
        let config = mq_bridge::models::AmqpConfig {
            url: "amqp://guest:guest@localhost:5672/%2f".to_string(),
            queue: Some(format!("id_round_trip_{}", fast_uuid_v7::gen_id())),
            ..Default::default()
        };
        let publisher = AmqpPublisher::new(&config).await.unwrap();
        let sent = CanonicalMessage::new(b"x".to_vec(), Some(0xabcdef_u128 << 64 | 42));
        publisher.send(sent.clone()).await.unwrap();

        let mut consumer = AmqpConsumer::new(&config).await.unwrap();
        let first = consumer.receive().await.unwrap();
        assert_eq!(first.message.message_id, sent.message_id);
        (first.commit)(MessageDisposition::Nack).await.unwrap();

        let again = consumer.receive().await.unwrap();
        assert_eq!(again.message.message_id, sent.message_id);
        (again.commit)(MessageDisposition::Ack).await.unwrap();
    })
    .await;
}
