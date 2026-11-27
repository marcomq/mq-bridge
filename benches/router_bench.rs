//! Microbenchmark isolating HTTP route lookup — bottleneck #1 from the
//! throughput analysis.
//!
//! `SharedHttpRouter::match_route` (src/endpoints/http.rs) takes a blocking
//! `std::sync::Mutex` over a `HashMap` and linearly scans it on EVERY request.
//! This bench reproduces that exact access pattern in isolation and compares it
//! with two lock-free alternatives, so we can see — before touching production
//! code — whether the per-request router lock is what caps multicore scaling:
//!
//!   * `mutex_hashmap`  — current design: `Mutex<HashMap>` + scan + `Arc` clone
//!   * `arcswap_vec`    — proposed: `ArcSwap<Vec<..>>` snapshot + scan + `Arc` clone
//!   * `arcswap_single` — proposed fast path for the common single-route server
//!
//! Each is measured single-threaded (latency) and under N-thread contention
//! (aggregate throughput). The hypothesis is confirmed if the mutex variant
//! collapses as threads are added while the ArcSwap variants stay flat.
//!
//! Run with: `cargo bench --bench router_bench`

use arc_swap::{ArcSwap, ArcSwapOption};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Stand-in for `HttpConsumerState`: the two fields `match_route` actually reads,
/// plus padding so the `Arc` payload is a realistic size (the real struct holds a
/// channel sender, a semaphore, several strings, etc.). The padding keeps the
/// comparison honest — a trivially tiny `Arc` would understate clone/cache costs.
struct RouteState {
    path: Option<String>,
    method: Option<&'static str>,
    id: u64,
    _padding: [u64; 8],
}

impl RouteState {
    fn new(id: u64, path: Option<&str>, method: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            path: path.map(String::from),
            method,
            id,
            _padding: [0; 8],
        })
    }
}

fn route_matches_path(state: &RouteState, path: &str) -> bool {
    match &state.path {
        Some(route_path) => route_path == path,
        None => true,
    }
}

fn route_matches_method(state: &RouteState, method: &str) -> bool {
    match state.method {
        Some(route_method) => route_method == method,
        None => true,
    }
}

fn route_specificity(state: &RouteState) -> (u8, u8) {
    (
        u8::from(state.path.is_some()),
        u8::from(state.method.is_some()),
    )
}

/// Faithful copy of `match_route`'s best-specificity scan.
fn scan<'a, I>(routes: I, path: &str, method: &str) -> Option<Arc<RouteState>>
where
    I: Iterator<Item = &'a Arc<RouteState>>,
{
    let mut best: Option<Arc<RouteState>> = None;
    let mut best_specificity = (0u8, 0u8);
    for state in routes {
        if !route_matches_path(state, path) {
            continue;
        }
        if route_matches_method(state, method) {
            let specificity = route_specificity(state);
            if best.is_none() || specificity > best_specificity {
                best_specificity = specificity;
                best = Some(Arc::clone(state));
            }
        }
    }
    best
}

/// Current production design: blocking lock + `HashMap` scan per lookup.
struct MutexRouter {
    routes: Mutex<HashMap<u64, Arc<RouteState>>>,
}
impl MutexRouter {
    fn lookup(&self, path: &str, method: &str) -> Option<Arc<RouteState>> {
        let routes = self.routes.lock().unwrap();
        scan(routes.values(), path, method)
    }
}

/// Proposed design: lock-free snapshot + scan (handles multi-route servers).
struct ArcSwapRouter {
    routes: ArcSwap<Vec<Arc<RouteState>>>,
}
impl ArcSwapRouter {
    fn lookup(&self, path: &str, method: &str) -> Option<Arc<RouteState>> {
        let routes = self.routes.load();
        scan(routes.iter(), path, method)
    }
}

/// Proposed fast path for the overwhelmingly common single-route server: a
/// lock-free pointer load with no scan at all.
struct ArcSwapSingleRouter {
    only: ArcSwapOption<RouteState>,
}
impl ArcSwapSingleRouter {
    fn lookup(&self, path: &str, method: &str) -> Option<Arc<RouteState>> {
        let only = self.only.load_full()?;
        if route_matches_path(&only, path) && route_matches_method(&only, method) {
            Some(only)
        } else {
            None
        }
    }
}

fn build_routes(n: usize) -> Vec<Arc<RouteState>> {
    (0..n)
        .map(|i| RouteState::new(i as u64, Some(&format!("/r{i}")), Some("POST")))
        .collect()
}

/// Run `work` on `threads` OS threads, each performing `iters` lookups, started
/// together via a barrier. Returns wall-clock elapsed for the whole batch.
///
/// criterion divides this by `iters`, so with `Throughput::Elements(threads)`
/// the reported rate is aggregate lookups/sec across all threads — exactly the
/// number that should plateau if a shared lock is the bottleneck.
fn contended(threads: usize, iters: u64, work: Arc<dyn Fn() + Send + Sync>) -> Duration {
    let barrier = Arc::new(Barrier::new(threads + 1));
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let work = Arc::clone(&work);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..iters {
                work();
            }
        }));
    }
    let start = Instant::now();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    start.elapsed()
}

fn bench_router(c: &mut Criterion) {
    // A handful of routes so the scan has real work; lookups target the last one
    // to force a full scan (worst case for the linear-scan designs).
    let n_routes = 4usize;
    let target_path = format!("/r{}", n_routes - 1);
    let single_path = "/only";

    let mutex = Arc::new(MutexRouter {
        routes: Mutex::new(
            build_routes(n_routes)
                .into_iter()
                .map(|state| (state.id, state))
                .collect(),
        ),
    });
    let arcswap = Arc::new(ArcSwapRouter {
        routes: ArcSwap::from_pointee(build_routes(n_routes)),
    });
    let single = Arc::new(ArcSwapSingleRouter {
        only: ArcSwapOption::new(Some(RouteState::new(0, Some(single_path), Some("POST")))),
    });

    // ---- single-threaded latency ----
    {
        let mut group = c.benchmark_group("router_lookup/single_thread");
        group.throughput(Throughput::Elements(1));
        group.bench_function("mutex_hashmap", |b| {
            b.iter(|| black_box(mutex.lookup(black_box(&target_path), black_box("POST"))))
        });
        group.bench_function("arcswap_vec", |b| {
            b.iter(|| black_box(arcswap.lookup(black_box(&target_path), black_box("POST"))))
        });
        group.bench_function("arcswap_single", |b| {
            b.iter(|| black_box(single.lookup(black_box(single_path), black_box("POST"))))
        });
        group.finish();
    }

    // ---- contended aggregate throughput ----
    let max_threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let thread_counts: Vec<usize> = {
        let mut counts = vec![1usize, max_threads / 2, max_threads];
        counts.retain(|&t| t >= 1);
        counts.sort_unstable();
        counts.dedup();
        counts
    };

    let mut group = c.benchmark_group("router_lookup/contended");
    for &threads in &thread_counts {
        group.throughput(Throughput::Elements(threads as u64));

        group.bench_with_input(
            BenchmarkId::new("mutex_hashmap", threads),
            &threads,
            |b, &t| {
                b.iter_custom(|iters| {
                    let router = Arc::clone(&mutex);
                    let path = target_path.clone();
                    contended(
                        t,
                        iters,
                        Arc::new(move || {
                            black_box(router.lookup(&path, "POST"));
                        }),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("arcswap_vec", threads),
            &threads,
            |b, &t| {
                b.iter_custom(|iters| {
                    let router = Arc::clone(&arcswap);
                    let path = target_path.clone();
                    contended(
                        t,
                        iters,
                        Arc::new(move || {
                            black_box(router.lookup(&path, "POST"));
                        }),
                    )
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("arcswap_single", threads),
            &threads,
            |b, &t| {
                b.iter_custom(|iters| {
                    let router = Arc::clone(&single);
                    contended(
                        t,
                        iters,
                        Arc::new(move || {
                            black_box(router.lookup(single_path, "POST"));
                        }),
                    )
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_router);
criterion_main!(benches);
