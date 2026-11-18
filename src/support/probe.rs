//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Counters at the points where a fast and a slow path part, so tests can assert which one
//! ran instead of timing it. Without `test-utils`, `probe!` expands to nothing.

#[cfg(feature = "test-utils")]
static HITS: std::sync::Mutex<Vec<(&str, u64)>> = std::sync::Mutex::new(Vec::new());

/// Counts one pass through `name`; called once per batch, never per message.
#[cfg(feature = "test-utils")]
pub fn hit(name: &'static str) {
    let mut hits = HITS.lock().unwrap_or_else(|e| e.into_inner());
    match hits.iter_mut().find(|(n, _)| *n == name) {
        Some((_, count)) => *count += 1,
        None => hits.push((name, 1)),
    }
}

/// Passes through `name` since the process started.
#[cfg(feature = "test-utils")]
pub fn count(name: &str) -> u64 {
    let hits = HITS.lock().unwrap_or_else(|e| e.into_inner());
    hits.iter().find(|(n, _)| *n == name).map_or(0, |(_, c)| *c)
}

/// Marks a fast/slow path decision, e.g. `probe!("file.decode.parallel")`.
#[allow(unused_macros)]
macro_rules! probe {
    ($name:literal) => {
        #[cfg(feature = "test-utils")]
        $crate::support::probe::hit($name)
    };
}

// Builds without any probed endpoint feature have no call site.
#[allow(unused_imports)]
pub(crate) use probe;
