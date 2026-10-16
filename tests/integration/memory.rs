#![allow(unused_imports, dead_code)]
use mq_bridge::models::{Endpoint, Route};
use mq_bridge::test_utils::format_pretty;
use std::time::Instant;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "Performance test"] // This is a performance test, run it explicitly
pub async fn test_memory_to_memory_pipeline() {
    mq_bridge::test_utils::setup_logging();

    println!("--- Generating Test messages ---");
    let num_messages = if cfg!(debug_assertions) {
        1_000_000
    } else {
        10_000_000
    };

    let messages_to_send = mq_bridge::test_utils::generate_test_messages(num_messages);

    let input = Endpoint::new_memory(IN_TOPIC, 200);
    let output = Endpoint::new_memory(OUT_TOPIC, num_messages);
    let route = Route::new(input, output).with_batch_size(100);
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

use mq_bridge::test_utils::{run_performance_pipeline_test, setup_logging};
const PERF_TEST_MESSAGE_COUNT: usize = 1_250_000;

pub const IN_TOPIC: &str = "mem-in";
pub const OUT_TOPIC: &str = "mem-out";
const PERF_TEST_CONCURRENCY: usize = 1;
const CONFIG_YAML: &str = r#"
metrics:
  enabled: false

routes:
  memory_to_internal:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-in-internal" }
    output:
      memory: { topic: "test-intermediate-memory", capacity: {out_capacity} }

  internal_to_memory:
    concurrency: 4
    batch_size: 128
    input:
      memory: { topic: "test-intermediate-memory", capacity: {out_capacity}  }
    output:
      memory: { topic: "test-out-internal", capacity: {out_capacity} }
"#;

#[tokio::test(flavor = "multi_thread")]
async fn test_memory_concurrency() {
    setup_logging();
    let input = Endpoint::new_memory("con_in_mem", 10);
    let output = Endpoint::new_memory("con_out_mem", 10);
    mq_bridge::test_utils::run_concurrency_test(input.clone(), output, input).await;
}
