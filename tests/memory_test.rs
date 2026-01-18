use mq_bridge::models::{Endpoint, Route};
use std::time::Instant;

use mq_bridge::test_utils::format_pretty;

mod integration;

// run in release:
// cargo test --package mq-bridge --test memory_test --release -- test_memory_to_memory_pipeline --exact --nocapture --ignored

#[tokio::test(flavor = "multi_thread")]
#[ignore = "Performance test"] // This is a performance test, run it explicitly
async fn test_memory_to_memory_pipeline() {
    mq_bridge::test_utils::setup_logging();

    println!("--- Generating Test messages ---");
    let num_messages = if cfg!(debug_assertions) {
        1_000_000
    } else {
        10_000_000
    };

    let messages_to_send = mq_bridge::test_utils::generate_test_messages(num_messages);

    let input = Endpoint::new_memory("mem-in", 200);
    let output = Endpoint::new_memory("mem-out", num_messages);
    let route = Route::new(input, output);
    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    println!("--- Starting Memory-to-Memory Pipeline Test ---");

    route.deploy("mem_2_mem").await.unwrap();

    let start_time = Instant::now();

    in_channel.fill_messages(messages_to_send).await.unwrap();
    in_channel.close();

    let mut received = Vec::with_capacity(num_messages);
    while received.len() < num_messages {
        let batch = out_channel.drain_messages();
        received.extend(batch);
        if received.len() >= num_messages {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let duration = start_time.elapsed();
    Route::stop("mem_2_mem").await;

    let msgs_per_sec = num_messages as f64 / duration.as_secs_f64();

    // Print results in a table format
    println!("\n--- Performance Test Results ---");
    println!("{:<25} | {:<25}", "Test Name", "Pipeline Performance");
    println!("{:-<25}-|-{:-<25}", "", "");
    println!(
        "{:<25} | {:<25}",
        "Memory to Memory Pipeline",
        format_pretty(msgs_per_sec)
    );
    println!("-------------------------------------------------");
    println!(
        "Processed {} messages in {:.2?}",
        format_pretty(num_messages),
        duration
    );

    println!("-------------------------------------------------");

    assert_eq!(received.len(), num_messages);
}
