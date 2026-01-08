// tests/integration/ibm_mq.rs
#![allow(dead_code)]

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
use crate::integration::common::{
    add_performance_result, generate_test_messages, run_direct_perf_test, run_test_with_docker,
    setup_logging,
};
use mq_bridge::endpoints::ibm_mq::{IbmMqConsumer, IbmMqPublisher};
use mq_bridge::models::{ConsumerMode, Endpoint, EndpointType, IbmMqConfig, IbmMqEndpoint, Route};
use mq_bridge::traits::{MessageConsumer, MessagePublisher};
use std::sync::Arc;
use std::time::Instant;

fn get_config() -> IbmMqConfig {
    IbmMqConfig {
        user: Some("app".to_string()),
        password: Some("admin".to_string()),
        queue_manager: "QM1".to_string(),
        connection_name: "localhost(1414)".to_string(),
        channel: "DEV.APP.SVRCONN".to_string(),
        ..Default::default()
    }
}

pub async fn test_ibm_mq_performance_pipeline() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let queue_name = "DEV.QUEUE.1";
        let config = get_config();

        // Seed the queue
        let publisher = IbmMqPublisher::new(config.clone(), queue_name.to_string())
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
            endpoint_type: EndpointType::IbmMq(IbmMqEndpoint {
                config: config.clone(),
                topic: None,
                queue: Some(queue_name.to_string()),
            }),
            mode: ConsumerMode::Consume,
            middlewares: vec![],
            handler: None,
        };
        let output_ep = Endpoint::new_memory("mem_out", num_messages);

        let mut route = Route::new(input_ep, output_ep);
        route.concurrency = 4;
        route.batch_size = 128;
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

pub async fn test_ibm_mq_performance_direct() {
    setup_logging();
    run_test_with_docker("tests/integration/docker-compose/ibm_mq.yml", || async {
        let queue = "DEV.QUEUE.1";
        let config = get_config();

        let result = run_direct_perf_test(
            "IBM-MQ",
            || async {
                Arc::new(
                    IbmMqPublisher::new(config.clone(), queue.to_string())
                        .await
                        .unwrap(),
                )
            },
            || async {
                Arc::new(tokio::sync::Mutex::new(
                    IbmMqConsumer::new(config.clone(), queue.to_string())
                        .await
                        .unwrap(),
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
        let publisher = IbmMqPublisher::new(config.clone(), queue_name.to_string())
            .await
            .unwrap();
        let mut consumer = IbmMqConsumer::new(config.clone(), queue_name.to_string())
            .await
            .unwrap();

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
