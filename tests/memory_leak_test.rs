//! Regression tests for unbounded memory growth in the route runner.
//!
//! Background: through mq-bridge 0.2.21, the route runner spawned one commit task
//! per batch into a `tokio::task::JoinSet` but only reaped it at shutdown. A
//! `JoinSet` retains completed tasks (handle + output) until they are joined, so
//! every processed batch leaked an entry. A `static` consumer produces messages
//! with no delay, so the loop spun as fast as the CPU allowed and the process
//! climbed to tens of GB within minutes. The fix reaps finished commit tasks each
//! iteration (and caps in-flight commits via a semaphore) inside
//! `send_batch_and_commit`, which both the sequential and concurrent runners call.
//!
//! These tests drive a `static -> null` route (the null publisher is instant, so
//! the loop spins at maximum rate — the worst case for the old leak) and assert
//! the process RSS stays bounded after a warmup. They are soak tests, so they are
//! `#[ignore]`d by default. Run them explicitly with:
//!
//! ```text
//! cargo test --test memory_leak_test -- --ignored --nocapture
//! ```

use std::time::{Duration, Instant};

use mq_bridge::Route;
use mq_bridge::models::{Endpoint, EndpointType, StaticConfig};

/// Current resident set size of this process, in bytes.
///
/// Reads `/proc/self/statm` on Linux; falls back to `ps -o rss=` elsewhere
/// (e.g. macOS). Returns `None` if RSS can't be determined, so callers can skip
/// the assertion rather than fail spuriously on an unsupported platform.
fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_size = 4096u64; // getpagesize() is 4 KiB on all common Linux targets
        return Some(resident_pages * page_size);
    }

    #[allow(unreachable_code)]
    {
        // macOS / other unix: shell out to `ps`. `ps -o rss=` reports KiB.
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let kib: u64 = String::from_utf8(out.stdout).ok()?.trim().parse().ok()?;
        Some(kib * 1024)
    }
}

/// Sample RSS a few times and take the max, to smooth over allocator jitter.
fn sample_rss_bytes() -> Option<u64> {
    let mut max = None;
    for _ in 0..3 {
        if let Some(rss) = current_rss_bytes() {
            max = Some(max.map_or(rss, |m: u64| m.max(rss)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    max
}

/// Run a `static -> null` route for `duration` and return RSS growth (bytes)
/// between a post-warmup baseline and the end. The null publisher is instant, so
/// the runner spins at maximum throughput — if commit tasks accumulate, RSS climbs
/// fast.
async fn measure_static_to_null_growth(concurrency: usize, duration: Duration) -> Option<i64> {
    let input = Endpoint::new(EndpointType::Static(StaticConfig::from(
        "leak-regression-payload",
    )));
    let output = Endpoint::new(EndpointType::Null);

    let route = Route::new(input, output)
        .with_concurrency(concurrency)
        .with_batch_size(1); // batch_size 1 maximises batches/sec — the old leak's worst case

    let handle = route
        .run(&format!("memory-leak-static-null-c{concurrency}"))
        .await
        .expect("route should start");

    // Warm up so steady-state pools/buffers are allocated before the baseline.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let baseline = sample_rss_bytes();

    let start = Instant::now();
    while start.elapsed() < duration {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let after = sample_rss_bytes();

    handle.stop().await;
    let _ = handle.join().await;

    match (baseline, after) {
        (Some(b), Some(a)) => Some(a as i64 - b as i64),
        _ => None,
    }
}

/// Maximum RSS growth tolerated over the soak window. With the fix, growth is
/// near-zero (a few MB of allocator jitter). The old `JoinSet` leak grew GB in
/// this window, so the threshold cleanly separates fixed from regressed.
const MAX_GROWTH_BYTES: i64 = 96 * 1024 * 1024; // 96 MiB

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak test; run with --ignored"]
async fn static_to_null_sequential_does_not_leak() {
    let Some(growth) = measure_static_to_null_growth(1, Duration::from_secs(8)).await else {
        eprintln!("RSS unavailable on this platform; skipping leak assertion");
        return;
    };
    println!("sequential (concurrency=1) RSS growth: {} bytes", growth);
    assert!(
        growth < MAX_GROWTH_BYTES,
        "RSS grew {growth} bytes (> {MAX_GROWTH_BYTES}); commit-task JoinSet may be leaking again"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak test; run with --ignored"]
async fn static_to_null_concurrent_does_not_leak() {
    let Some(growth) = measure_static_to_null_growth(4, Duration::from_secs(8)).await else {
        eprintln!("RSS unavailable on this platform; skipping leak assertion");
        return;
    };
    println!("concurrent (concurrency=4) RSS growth: {} bytes", growth);
    assert!(
        growth < MAX_GROWTH_BYTES,
        "RSS grew {growth} bytes (> {MAX_GROWTH_BYTES}); commit-task JoinSet may be leaking again"
    );
}
