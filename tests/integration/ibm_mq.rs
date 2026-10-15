// tests/integration/ibm_mq.rs
#![allow(dead_code)]

use mq_bridge::endpoints::ibm_mq::{IbmMqConsumer, IbmMqPublisher};
use mq_bridge::models::{Endpoint, EndpointType, IbmMqConfig, Route};
/// This test requires a running IBM MQ instance.
/// You can use the provided docker-compose file:
/// `docker-compose -f tests/integration/docker-compose/ibm_mq.yml up -d`
///
/// The test assumes the following:
/// - Queue Manager: QM1
/// - Queue: DEV.QUEUE.1
/// - Channel: DEV.APP.SVRCONN
/// - User: app
/// - Password: adminpass
///
/// You might need to create the queue and channel manually on the queue manager if they don't exist.
/// You can do this by executing into the container and using `runmqsc`.
/// Example commands:
/// `docker exec -it mq /opt/mqm/bin/runmqsc QM1`
/// Then, inside runmqsc:
/// `DEFINE QLOCAL('DEV.QUEUE.1')`
/// `DEFINE CHANNEL('DEV.APP.SVRCONN') CHLTYPE(SVRCONN)`
/// `SET CHLAUTH('DEV.APP.SVRCONN') TYPE(BLOCKUSER) USERLIST('nobody')`
/// `SET CHLAUTH('*') TYPE(ADDRESSMAP) ADDRESS('*') USERSRC(NOACCESS) ACTION(ADD)`
/// `SET CHLAUTH('DEV.APP.SVRCONN') TYPE(ADDRESSMAP) ADDRESS('*') USERSRC(CHANNEL) CHCKCLNT(ASQMGR) ACTION(ADD)`
/// `ALTER AUTHINFO(SYSTEM.DEFAULT.AUTHINFO.IDPWOS) AUTHTYPE(IDPWOS) ADOPTCTX(YES)`
/// `REFRESH SECURITY(*)`
use mq_bridge::test_utils::{
    add_performance_result, generate_test_messages, run_direct_perf_test, run_test_with_docker,
    run_test_with_docker_controller, setup_logging, verify_subscriber_logic,
};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use std::sync::Arc;
use std::time::Instant;

/// Whether the IBM MQ client is usable here (always for static link; for the
/// dlopen build, only if the runtime client library loads). Lets the `test_all_*`
/// runners skip IBM MQ where no client is installed instead of failing on connect.
pub fn client_available() -> bool {
    mq_bridge::endpoints::ibm_mq::ibm_mq_client_available()
}

fn get_config() -> IbmMqConfig {
    IbmMqConfig {
        username: Some("app".to_string()),
        password: Some("adminpass".to_string()),
        queue_manager: "QM1".to_string(),
        url: "localhost(1414)".to_string(),
        channel: "DEV.APP.SVRCONN".to_string(),
        ..Default::default()
    }
}

pub async fn test_ibm_mq_subscriber_logic() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let topic_name = "DEV.BASE.TOPIC";
        let mut config = get_config();
        config.topic = Some(topic_name.to_string());
        config.queue = None;

        let publisher = Arc::new(IbmMqPublisher::new(&config).await.unwrap());
        let sub1 = Arc::new(tokio::sync::Mutex::new(
            IbmMqConsumer::new(&config).await.unwrap(),
        ));
        let sub2 = Arc::new(tokio::sync::Mutex::new(
            IbmMqConsumer::new(&config).await.unwrap(),
        ));

        verify_subscriber_logic(publisher, sub1, sub2).await;
        println!("IBM MQ Subscriber logic test passed!");
    })
    .await;
}

pub async fn test_ibm_mq_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let queue_name = "DEV.QUEUE.1";
        let config = get_config();

        // Seed the queue
        let mut endpoint = config.clone();
        endpoint.queue = Some(queue_name.to_string());
        endpoint.topic = None;

        let publisher = IbmMqPublisher::new(&endpoint)
            .await
            .expect("Failed to create publisher");

        let num_messages = 1000;
        let messages = generate_test_messages(num_messages);
        publisher
            .send_batch(messages)
            .await
            .expect("Failed to seed queue");

        // Setup Pipeline: IBM MQ -> Memory
        let input_ep = Endpoint {
            endpoint_type: EndpointType::IbmMq({
                let mut c = config.clone();
                c.topic = None;
                c.queue = Some(queue_name.to_string());
                c
            }),
            middlewares: vec![],
            handler: None,
        };
        let output_ep = Endpoint::new_memory("mem_out", num_messages);

        let route = Route::new(input_ep, output_ep)
            .with_concurrency(4)
            .with_batch_size(128);
        let out_channel = route.output.channel().unwrap();

        // Run route in background
        let handle = tokio::spawn(async move {
            let _ = route.run_until_err("ibm_mq_pipe", None, None).await;
        });

        // Wait for messages. The channel holds Vec<CanonicalMessage> *batches*, so
        // out_channel.len() counts batches, not messages — draining and summing the
        // batch lengths is the only correct count, and it also relieves backpressure
        // so the bounded output channel never blocks the route.
        let timeout_secs: u64 = std::env::var("MQB_TEST_DRAIN_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let start = Instant::now();
        let mut received = 0;
        let mut last_logged = 0;
        while received < num_messages {
            received += out_channel.drain_messages().len();
            if received >= last_logged + 100 {
                println!("IBM MQ pipeline: {received}/{num_messages} received");
                last_logged = received;
            }
            if received >= num_messages {
                break;
            }
            if start.elapsed().as_secs() > timeout_secs {
                panic!(
                    "Timeout waiting for messages in pipeline: {received}/{num_messages} \
                     received after {timeout_secs}s"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        handle.abort();
        println!("IBM MQ Pipeline test passed!");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker compose and the IBM MQ client"]
async fn test_ibm_mq_chaos() {
    if !mq_bridge::test_utils::should_run("ibm-mq") || !client_available() {
        return;
    }
    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/ibm_mq.yml",
        |controller| async move {
            let config_yaml = r#"
routes:
  memory_to_ibm_mq:
    input:
      memory:
        topic: "chaos_in"
        enable_nack: true
    output:
      ibmmq:
        queue_manager: "QM1"
        url: "localhost(1414)"
        channel: "DEV.APP.SVRCONN"
        queue: "DEV.QUEUE.1"
        username: "app"
        password: "adminpass"
  ibm_mq_to_memory:
    input:
      ibmmq:
        queue_manager: "QM1"
        url: "localhost(1414)"
        channel: "DEV.APP.SVRCONN"
        queue: "DEV.QUEUE.1"
        username: "app"
        password: "adminpass"
    output:
      memory:
        topic: "chaos_out"
"#;
            mq_bridge::test_utils::run_chaos_pipeline_test("ibm_mq", config_yaml, controller, "mq")
                .await;
        },
    )
    .await;
}

pub async fn test_ibm_mq_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let queue = "DEV.QUEUE.1";
        let config = get_config();

        let result = run_direct_perf_test(
            "ibm-mq",
            || async {
                let mut endpoint = config.clone();
                endpoint.queue = Some(queue.to_string());
                endpoint.topic = None;
                Arc::new(IbmMqPublisher::new(&endpoint).await.unwrap())
            },
            || async {
                let mut endpoint = config.clone();
                endpoint.queue = Some(queue.to_string());
                endpoint.topic = None;
                Arc::new(tokio::sync::Mutex::new(
                    IbmMqConsumer::new(&endpoint).await.unwrap(),
                ))
            },
        )
        .await;
        add_performance_result(result);
    })
    .await;
}

pub async fn test_ibm_mq_performance_direct2() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let queue_name = "DEV.QUEUE.1";
        let config = get_config();
        let num_messages = 1000;
        let messages = generate_test_messages(num_messages);

        let mut endpoint = config.clone();
        endpoint.queue = Some(queue_name.to_string());
        endpoint.topic = None;

        let publisher = IbmMqPublisher::new(&endpoint).await.unwrap();
        let mut consumer = IbmMqConsumer::new(&endpoint).await.unwrap();

        println!("--- Starting IBM MQ Direct Performance Test ---");
        let start = Instant::now();
        publisher.send_batch(messages).await.unwrap();
        let send_time = start.elapsed();

        let timeout_secs: u64 = std::env::var("MQB_TEST_DRAIN_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        let mut received_count = 0;
        let recv_start = Instant::now();
        while received_count < num_messages {
            let batch = consumer.receive_batch(100).await.unwrap();
            received_count += batch.messages.len();
            if recv_start.elapsed().as_secs() > timeout_secs {
                panic!(
                    "Timeout receiving messages: {received_count}/{num_messages} \
                     received after {timeout_secs}s"
                );
            }
        }
        let recv_time = recv_start.elapsed();

        let send_rate = num_messages as f64 / send_time.as_secs_f64();
        let recv_rate = num_messages as f64 / recv_time.as_secs_f64();

        println!(
            "IBM MQ Direct: Send {:.2} msg/s, Recv {:.2} msg/s",
            send_rate, recv_rate
        );
    })
    .await;
}

pub async fn test_ibm_mq_status() {
    use mq_bridge::traits::{MessageConsumer, MessagePublisher};
    use tokio::time::{sleep, Duration};

    setup_logging();
    run_test_with_docker_controller(
        "tests/integration/docker-compose/ibm_mq.yml",
        |controller| async move {
            let mut config = get_config();
            config.queue = Some("DEV.QUEUE.1".to_string());

            let publisher = IbmMqPublisher::new(&config).await.unwrap();
            let consumer = IbmMqConsumer::new(&config).await.unwrap();

            println!("[IBM MQ] Checking initial status...");
            sleep(Duration::from_secs(5)).await;
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
            println!("[IBM MQ] Initial status check OK.");

            controller.stop_service("mq");
            println!("[IBM MQ] Service 'mq' stopped. Waiting for disconnect detection...");

            let start = std::time::Instant::now();
            loop {
                let pub_status = publisher.status().await;
                let con_status = consumer.status().await;
                if !pub_status.healthy && !con_status.healthy {
                    println!("[IBM MQ] Disconnect detected.");
                    break;
                }
                if start.elapsed() > Duration::from_secs(20) {
                    panic!(
                        "[IBM MQ] Timeout waiting for disconnect. Pub: {:?}, Con: {:?}",
                        pub_status, con_status
                    );
                }
                sleep(Duration::from_secs(1)).await;
            }

            controller.start_service("mq");
            println!("[IBM MQ] Service 'mq' started. Waiting for reconnect...");

            let start = std::time::Instant::now();
            loop {
                // Create new instances to force reconnection
                if let (Ok(p), Ok(c)) = (
                    IbmMqPublisher::new(&config).await,
                    IbmMqConsumer::new(&config).await,
                ) {
                    let pub_status = p.status().await;
                    let con_status = c.status().await;
                    if pub_status.healthy && con_status.healthy {
                        println!("[IBM MQ] Reconnect detected.");
                        break;
                    }
                }
                if start.elapsed() > Duration::from_secs(45) {
                    // IBM MQ can be slow to start
                    panic!("[IBM MQ] Timeout waiting for reconnect.");
                }
                sleep(Duration::from_secs(2)).await;
            }
            println!("[IBM MQ] Status test successful.");
        },
    )
    .await;
}

async fn receive_one(consumer: &mut IbmMqConsumer, what: &str) -> mq_bridge::traits::ReceivedBatch {
    let deadline = Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let batch = consumer.receive_batch(1).await.unwrap();
        if !batch.messages.is_empty() {
            return batch;
        }
        assert!(Instant::now() < deadline, "{what}");
    }
}

#[tokio::test]
#[ignore = "requires docker compose and the IBM MQ client"]
async fn test_ibm_mq_unsettled_messages_are_given_back() {
    if !mq_bridge::test_utils::should_run("ibm-mq") || !client_available() {
        return;
    }
    use mq_bridge::traits::MessageDisposition;
    use mq_bridge::CanonicalMessage;

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let mut config = get_config();
        config.queue = Some("DEV.QUEUE.2".to_string());
        let publisher = IbmMqPublisher::new(&config).await.unwrap();

        // A consumer dropped with a batch in flight: its messages go back to the queue.
        publisher
            .send_batch(vec![CanonicalMessage::new(b"first".to_vec(), None)])
            .await
            .unwrap();
        let mut dropped = IbmMqConsumer::new(&config).await.unwrap();
        drop(receive_one(&mut dropped, "the message arrives").await);
        drop(dropped);
        let mut consumer = IbmMqConsumer::new(&config).await.unwrap();
        let batch = receive_one(&mut consumer, "a dropped consumer kept its message").await;
        assert_eq!(&batch.messages[0].payload[..], b"first");

        // A batch dropped without a commit is not committed by the next batch's ack.
        drop(batch);
        let again = receive_one(&mut consumer, "a dropped batch is delivered again").await;
        assert_eq!(&again.messages[0].payload[..], b"first");
        drop(again);
        publisher
            .send_batch(vec![CanonicalMessage::new(b"second".to_vec(), None)])
            .await
            .unwrap();
        let mut seen = Vec::new();
        while seen.len() < 2 {
            let batch = receive_one(&mut consumer, "both messages arrive").await;
            seen.push(batch.messages[0].payload.to_vec());
            (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        }
        seen.sort();
        assert_eq!(seen, [b"first".to_vec(), b"second".to_vec()]);
    })
    .await;
}

#[tokio::test]
#[ignore = "requires docker compose and the IBM MQ client"]
async fn test_ibm_mq_message_over_max_message_size_is_an_error() {
    if !mq_bridge::test_utils::should_run("ibm-mq") || !client_available() {
        return;
    }
    use mq_bridge::CanonicalMessage;

    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let mut config = get_config();
        config.queue = Some("DEV.QUEUE.3".to_string());
        IbmMqPublisher::new(&config)
            .await
            .unwrap()
            .send_batch(vec![CanonicalMessage::new(vec![b'x'; 100], None)])
            .await
            .unwrap();
        let mut small = config.clone();
        small.max_message_size = 16;
        let mut consumer = IbmMqConsumer::new(&small).await.unwrap();
        let refused = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            consumer.receive_batch(1),
        )
        .await
        .expect("the read returns");
        let error = format!(
            "{:?}",
            refused.expect_err("a cut-off message is not delivered")
        );
        assert!(error.contains("max_message_size"), "{error}");
        drop(consumer);

        let mut consumer = IbmMqConsumer::new(&config).await.unwrap();
        let batch = receive_one(&mut consumer, "the message is still on the queue").await;
        assert_eq!(batch.messages[0].payload.len(), 100);
        (batch.commit)(vec![mq_bridge::traits::MessageDisposition::Ack])
            .await
            .unwrap();
    })
    .await;
}
