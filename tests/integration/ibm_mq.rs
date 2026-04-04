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
/// - Password: admin
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
    add_performance_result, generate_test_messages, run_chaos_pipeline_test, run_direct_perf_test,
    run_test_with_docker, run_test_with_docker_controller, setup_logging,
    verify_subscriber_logic,
};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use std::sync::Arc;
use std::time::Instant;

fn get_config() -> IbmMqConfig {
    IbmMqConfig {
        username: Some("app".to_string()),
        password: Some("admin".to_string()),
        queue_manager: "QM1".to_string(),
        url: "localhost(1414)".to_string(),
        channel: "DEV.APP.SVRCONN".to_string(),
        ..Default::default()
    }
}

fn get_config() -> IbmMqConfig {
    IbmMqConfig {
        username: Some("app".to_string()),
        password: Some("admin".to_string()),
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
        let sub1 = Arc::new(tokio::sync::Mutex::new(IbmMqConsumer::new(&config).await.unwrap()));
        let sub2 = Arc::new(tokio::sync::Mutex::new(IbmMqConsumer::new(&config).await.unwrap()));

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

        // Wait for messages
        let start = Instant::now();
        loop {
            if out_channel.len() >= num_messages {
                break;
            }
            if start.elapsed().as_secs() > 30 {
                panic!("Timeout waiting for messages in pipeline");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        handle.abort();
        println!("IBM MQ Pipeline test passed!");
    })
    .await;
}

pub async fn test_ibm_mq_chaos() {
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
        password: "admin"
  ibm_mq_to_memory:
    input:
      ibmmq:
        queue_manager: "QM1"
        url: "localhost(1414)"
        channel: "DEV.APP.SVRCONN"
        queue: "DEV.QUEUE.1"
        username: "app"
        password: "admin"
    output:
      memory:
        topic: "chaos_out"
"#;
            run_chaos_pipeline_test("ibm_mq", config_yaml, controller, "mq").await;
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

        let mut received_count = 0;
        let recv_start = Instant::now();
        while received_count < num_messages {
            let batch = consumer.receive_batch(100).await.unwrap();
            received_count += batch.messages.len();
            if recv_start.elapsed().as_secs() > 30 {
                panic!("Timeout receiving messages");
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
