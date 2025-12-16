use hot_queue::models::{Endpoint, EndpointType, MemoryConfig, Route};
use std::time::{Duration, Instant};

mod integration;

// run in release:
// cargo test --package hot_queue --test memory_test --features integration-test --release -- test_memory_to_memory_pipeline --exact --nocapture

#[tokio::test]
#[ignore] // This is a performance test, run it explicitly
async fn test_memory_to_memory_pipeline() {
    integration::common::setup_logging();

    let num_messages = 1500_000;
    let messages_to_send = integration::common::generate_test_messages(num_messages);

    let route = Route {
        input: Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: "mem-in".to_string(),
            capacity: Some(200), // A reasonable capacity for the input channel
        })),
        output: Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: "mem-out".to_string(),
            capacity: Some(num_messages + 10_000), // Ensure output can hold all messages
        })),
        concurrency: 1,
    };

    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    let run_handle = route.run_until_err("mem_2_mem", None);

    println!("--- Starting Memory-to-Memory Pipeline Test ---");

    let start_time = Instant::now();
    let fill_task = tokio::spawn(async move {
        in_channel.fill_messages(messages_to_send).await.unwrap();
        in_channel.close();
    });

    // Wait for the bridge to finish processing.
    let _ = tokio::time::timeout(Duration::from_secs(20), run_handle)
        .await
        .expect("Test timed out waiting for bridge to complete");

    // Ensure the fill task also completed without error
    fill_task.await.unwrap();

    let received = out_channel.drain_messages();
    let duration = start_time.elapsed();

    let msgs_per_sec = num_messages as f64 / duration.as_secs_f64();
    println!(
        "Processed {} messages in {:.2?} ({:.2} msgs/sec)",
        num_messages, duration, msgs_per_sec
    );
    println!("-------------------------------------------------");

    assert_eq!(received.len(), num_messages);
}
