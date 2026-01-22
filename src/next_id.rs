//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use std::cell::{Cell, RefCell};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

thread_local! {
    static RNG: RefCell<SmallRng> = RefCell::new(SmallRng::from_rng(&mut rand::rng()));
    static LAST_MS: Cell<u64> = const { Cell::new(0) };
    static COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// Generates a unique identifier compatible with UUID v7.
///
/// The identifier is a `u128` value composed of:
/// - 48 bits: Current timestamp in milliseconds.
/// -  4 bits: Version (7).
/// - 12 bits: Counter (High 12 bits of 18).
/// -  2 bits: Variant (2).
/// -  6 bits: Counter (Low 6 bits of 18).
/// - 56 bits: Random number.
///
/// This layout effectively provides an 18-bit counter (supporting ~262k IDs/ms)
/// by utilizing the standard `rand_a` field and the upper bits of `rand_b`.
///
/// **Note on Sorting:**
/// Since the counter is thread-local and resets every millisecond, IDs generated
/// concurrently by multiple threads within the same millisecond are not guaranteed
/// to be globally monotonic.
/// This is not random enough for cryptography!
pub fn now_v7() -> u128 {
    let mut current_timestamp = fast_time_ms();

    let (timestamp, counter) = COUNTER.with(|counter| {
        let last_timestamp = LAST_MS.with(|last| last.get());
        if current_timestamp > last_timestamp {
            LAST_MS.with(|last| last.set(current_timestamp));
            counter.set(0);
            (current_timestamp, 0)
        } else {
            // Time hasn't moved forward (or went backward).
            // We stick to the last timestamp to ensure monotonicity.
            current_timestamp = last_timestamp;

            let c = counter.get();
            // If counter is exhausted (18 bits = 262,143), increment timestamp to preserve monotonicity
            if c >= 0x3FFFF {
                current_timestamp += 1;
                LAST_MS.with(|last| last.set(current_timestamp));
                counter.set(0);
                (current_timestamp, 0)
            } else {
                let inc = c.wrapping_add(1);
                counter.set(inc);
                (last_timestamp, inc)
            }
        }
    });

    // Use 18 bits for counter: 12 in rand_a, 6 in rand_b high.
    // This allows ~262k IDs per millisecond per thread.
    let rand_a = (counter >> 6) & 0xFFF;
    let rand_b_high = counter & 0x3F;

    let rand_nr = RNG.with(|random_nr| random_nr.borrow_mut().next_u64());

    let timestamp_part = (timestamp as u128) << 80;
    let version_part = 7u128 << 76; // Version 7 (0111)
    let counter_part = (rand_a as u128) << 64; // 12 bits of counter
    let variant_part = 2u128 << 62; // Variant 1 (10..)
                                    // 56 bits of randomness + 6 bits of counter
    let rand_b_low = rand_nr & 0x00FF_FFFF_FFFF_FFFF;
    let random_part = ((rand_b_high as u128) << 56) | (rand_b_low as u128);

    timestamp_part | version_part | counter_part | variant_part | random_part
}

/// Generates a UUID v7 string using the `now_v7` function.
///
/// The returned string is in the format `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`.
///
/// **Note on Sorting:**
/// Since the counter is thread-local and resets every millisecond, IDs generated
/// concurrently by multiple threads within the same millisecond are not guaranteed
/// to be globally monotonic.
///
/// This is not random enough for cryptography!
pub fn now_v7_string() -> String {
    Uuid::from_u128(now_v7()).to_string()
}

/// Returns the current time in milliseconds since the Unix epoch.
///
/// It returns `0` if the system clock hasn't started yet.
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
            let _ = now_v7();
        }
        println!("Generated 1,000,000 IDs in {:?}", start.elapsed());
    }

    #[test]
    fn test_next_id_uniqueness() {
        let mut set = std::collections::HashSet::with_capacity(1_000_000);
        for _ in 0..1_000_000 {
            let id = now_v7();
            assert!(set.insert(id), "Duplicate ID generated: {:032x}", id);
        }
    }

    #[test]
    /// IDs are sorted correctly per thread.
    /// Capacity is ~262k IDs per ms (18 bits).
    fn test_next_id_ordering() {
        let mut last_id = 0;
        for _ in 0..1_000_000 {
            let id = now_v7();
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

    #[test]
    fn test_next_id_string() {
        let id_str = now_v7_string();
        assert_eq!(id_str.len(), 36);
        assert!(uuid::Uuid::parse_str(&id_str).is_ok());
    }
}
