#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    mq_bridge_app_desktop::run();
}

#[test]
fn test_config_version_matches_cargo() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    let config_json = include_str!("../tauri.conf.json");
    let v: serde_json::Value = serde_json::from_str(config_json).unwrap();
    assert_eq!(
        v["version"].as_str().unwrap(),
        cargo_version,
        "tauri.conf.json version does not match Cargo.toml"
    );
}
