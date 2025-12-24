use mq_bridge::models::{Endpoint, Route};
use std::time::Instant;

use crate::integration::common::format_pretty;

mod integration;

// run in release:
// cargo test --package mq-bridge --test memory_test --features integration-test --release -- test_memory_to_memory_pipeline --exact --nocapture

#[tokio::test]
#[ignore] // This is a performance test, run it explicitly
async fn test_memory_to_memory_pipeline() {
    integration::common::setup_logging();

    println!("--- Generating Test messages ---");
    let num_messages = if cfg!(debug_assertions) {
        1000_000
    } else {
        10_000_000
    };

    let messages_to_send = integration::common::generate_test_messages(num_messages);

    let input = Endpoint::new_memory("mem-in", 200);
    let output = Endpoint::new_memory("mem-out", 200);
    let route = Route::new(input, output);
    let in_channel = route.input.channel().unwrap();
    let out_channel = route.output.channel().unwrap();

    println!("--- Starting Memory-to-Memory Pipeline Test ---");

    let start_time = Instant::now();

    // Run the pipeline and the message sending concurrently.
    let (run_result, _) = tokio::join!(
        // The route will run until the input channel is closed and empty.
        route.run_until_err("mem_2_mem", None),
        // This task sends all messages and then closes the channel,
        // which signals the route to stop.
        async {
            in_channel.fill_messages(messages_to_send).await.unwrap();
            in_channel.close();
        }
    );

    // run_result will have Err("Memory channel closed.")
    run_result.ok();

    let received = out_channel.drain_messages();
    let duration = start_time.elapsed();

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
