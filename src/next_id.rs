use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use std::cell::{Cell, RefCell};
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    static RNG: RefCell<SmallRng> = RefCell::new(SmallRng::from_rng(&mut rand::rng()));
    static LAST_MS: Cell<u64> = const { Cell::new(0) };
    static COUNTER: Cell<u16> = const { Cell::new(0) };
}

/// Generates a unique identifier that is composed of the current
/// timestamp in milliseconds, a counter that increments every time
/// the timestamp changes, and a random number. The identifier is
/// a `u128` value, where the timestamp is in the most significant
/// 48 bits, the counter is in the next 16 bits, and the random
/// number is in the least significant 64 bits. This ensures that
/// the identifier is unique and unpredictable, even when called
/// concurrently from multiple threads.
/// This is not random enough for cryptography!
pub fn next_id() -> u128 {
    let current_timestamp = fast_time_ms();

    let next_counter = COUNTER.with(|counter| {
        let last_timestamp = LAST_MS.with(|last| last.get());
        if last_timestamp != current_timestamp {
            LAST_MS.with(|last| last.set(current_timestamp));
            counter.set(0);
            0
        } else {
            let inc_counter = counter.get().wrapping_add(1);
            counter.set(inc_counter);
            inc_counter
        }
    });

    let rand_nr = RNG.with(|random_nr| random_nr.borrow_mut().next_u64());

    ((current_timestamp as u128) << 80) | ((next_counter as u128) << 64) | rand_nr as u128
}

fn fast_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_next_id_performance() {
        let start = std::time::Instant::now();
        for _ in 0..1_000_000 {
            let _ = next_id();
        }
        println!("Generated 1,000,000 IDs in {:?}", start.elapsed());
    }

    #[test]
    fn test_next_id_uniqueness() {
        let mut set = std::collections::HashSet::with_capacity(1_000_000);
        for _ in 0..1_000_000 {
            let id = next_id();
            assert!(set.insert(id), "Duplicate ID generated: {:032x}", id);
        }
    }

    #[test]
    fn test_next_id_ordering() {
        let mut last_id = 0;
        for _ in 0..1_000_000 {
            let id = next_id();
            if last_id != 0 {
                assert!(
                    id > last_id,
                    "IDs are not ordered: {:032x} <= {:032x}",
                    id,
                    last_id
                );
            }
            last_id = id;
        }
    }
}
