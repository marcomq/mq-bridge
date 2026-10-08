//! `include/mq_bridge.h` is generated from this crate's sources.
//!
//! The header is checked in so C users need no Rust toolchain; this test keeps
//! it from drifting. `MQB_BLESS=1` rewrites it instead of comparing.

use std::path::PathBuf;

/// Hash of the header that `MQB_API_VERSION` was last reviewed against.
const REVIEWED_HEADER_HASH: u64 = 0x3427_f899_9040_7a8b;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn generate() -> String {
    let dir = crate_dir();
    let config =
        cbindgen::Config::from_file(dir.join("cbindgen.toml")).expect("read cbindgen.toml");
    let mut out = Vec::new();
    // cbindgen follows the `mod` declarations from here.
    cbindgen::Builder::new()
        .with_config(config)
        .with_src(dir.join("src/lib.rs"))
        .generate()
        .expect("cbindgen generates the header")
        .write(&mut out);
    String::from_utf8(out).expect("the header is UTF-8")
}

#[test]
fn the_checked_in_header_matches_the_rust_api() {
    let generated = generate();
    let path = crate_dir().join("../../include/mq_bridge.h");
    if std::env::var_os("MQB_BLESS").is_some() {
        std::fs::write(&path, generated).expect("write the header");
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "include/mq_bridge.h is stale; regenerate it with \
         `MQB_BLESS=1 cargo test -p mq-bridge-c --no-default-features --test c_header` \
         and review MQB_API_VERSION"
    );
}

/// FNV-1a over the header without its `MQB_API_VERSION` define, so a bump alone
/// does not change it. Hand-rolled: std's hasher is not stable across releases.
fn header_hash(header: &str) -> u64 {
    header
        .lines()
        .filter(|line| !line.starts_with("#define MQB_API_VERSION"))
        .flat_map(|line| line.bytes().chain(*b"\n"))
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

/// Fails on any header change, `MQB_BLESS` or not: updating the hash by hand is
/// the point at which someone decides whether the change needs a version bump.
#[test]
fn the_api_version_was_reviewed_for_this_header() {
    let hash = header_hash(&generate());
    assert!(
        hash == REVIEWED_HEADER_HASH,
        "the C header changed since MQB_API_VERSION was last reviewed. Decide whether \
         the change needs a bump of MQB_API_VERSION in src/lib.rs (anything that can \
         break a program built against the old header does), then set \
         REVIEWED_HEADER_HASH in tests/c_header.rs to {hash:#018x}"
    );
}
