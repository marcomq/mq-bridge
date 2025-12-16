use hot_queue::models::{Endpoint, EndpointType, MemoryConfig, Route};
use std::fmt::Display;
use std::time::{Duration, Instant};

mod integration;

// run in release:
// cargo test --package hot_queue --test memory_test --features integration-test --release -- test_memory_to_memory_pipeline --exact --nocapture


#[tokio::test]
#[ignore] // This is a performance test, run it explicitly
async fn test_memory_to_memory_pipeline() {
    integration::common::setup_logging();

    println!("--- Generating Test messages ---");
    let num_messages = 10_000_000;
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
        "Processed {} messages in {}ms ({} msgs/sec)",
        format_pretty(num_messages),
        format_pretty(duration.as_millis()),
        format_pretty(msgs_per_sec)
    );
    println!("-------------------------------------------------");

    assert_eq!(received.len(), num_messages);
}

/// Formats a number with commas as thousand separators.
/// Handles both integers and floating-point numbers.
pub fn format_pretty<N: Display>(num: N) -> String {
    let s = num.to_string();
    let mut parts = s.splitn(2, '.');
    let integer_part = parts.next().unwrap_or("");
    let fractional_part = parts.next();

    let mut formatted_integer = String::with_capacity(integer_part.len() + integer_part.len() / 3);
    let mut count = 0;
    for ch in integer_part.chars().rev() {
        if count > 0 && count % 3 == 0 {
            formatted_integer.push('_');
        }
        formatted_integer.push(ch);
        count += 1;
    }

    let formatted_integer = formatted_integer.chars().rev().collect::<String>();

    match fractional_part {
        Some(frac) => {
            let truncated_frac = if frac.len() > 2 { &frac[..2] } else { frac };
            format!("{}.{}", formatted_integer, truncated_frac)
        }
        None => formatted_integer,
    }
}