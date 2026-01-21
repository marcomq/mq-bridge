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
pub fn next_id() -> u128 {
    let ms = fast_time_ms();

    let ctr = COUNTER.with(|c| {
        let last = LAST_MS.with(|l| l.get());
        if last != ms {
            LAST_MS.with(|l| l.set(ms));
            c.set(0);
            0
        } else {
            let v = c.get().wrapping_add(1);
            c.set(v);
            v
        }
    });

    let rand = RNG.with(|r| r.borrow_mut().next_u64());

    ((ms as u128) << 80) | ((ctr as u128) << 64) | rand as u128
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
}
