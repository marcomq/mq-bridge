//! `include/mq_bridge_plugin.h` is generated from `src/support/plugin_abi.rs`.
//!
//! The header is checked in so C authors need no Rust toolchain; this test keeps
//! it from drifting. `MQB_BLESS=1` rewrites it instead of comparing.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn generate() -> String {
    let root = repo_root();
    let config = cbindgen::Config::from_file(root.join("include/cbindgen.toml"))
        .expect("read include/cbindgen.toml");
    let mut out = Vec::new();
    cbindgen::Builder::new()
        .with_config(config)
        .with_src(root.join("src/support/plugin_abi.rs"))
        .generate()
        .expect("cbindgen generates the header")
        .write(&mut out);
    strip_doc_links(&String::from_utf8(out).expect("the header is UTF-8"))
}

/// Rustdoc links read as noise in C: "[`X`](path)" and "[`X`]" become "`X`".
fn strip_doc_links(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut rest = header;
    while let Some(start) = rest.find("[`") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find("`]") else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(&after[..end + 1]);
        rest = &after[end + 2..];
        if rest.starts_with('(') {
            if let Some(close) = rest.find(')') {
                rest = &rest[close + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[test]
fn the_checked_in_header_matches_the_rust_abi() {
    let generated = generate();
    let path = repo_root().join("include/mq_bridge_plugin.h");
    if std::env::var_os("MQB_BLESS").is_some() {
        std::fs::write(&path, generated).expect("write the header");
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "include/mq_bridge_plugin.h is stale; regenerate it with \
         `MQB_BLESS=1 cargo test -p mq-bridge-plugin-fixture --test plugin_c_header`"
    );
}
