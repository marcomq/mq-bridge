/// When built with `--features ibm-mq`, point the linker at the IBM MQ client
/// library so the desktop binary links against `libmqic_r`. Mirrors the logic in
/// `crates/cli/build.rs` so `cargo install --features ibm-mq` works for the
/// desktop app too.
fn configure_ibm_mq() {
    use std::env;

    if env::var("CARGO_FEATURE_IBM_MQ").is_err() {
        return;
    }

    println!("cargo:rerun-if-env-changed=MQ_INSTALLATION_PATH");
    println!("cargo:rerun-if-env-changed=MQ_HOME");

    let mq_home = env::var("MQ_INSTALLATION_PATH")
        .or_else(|_| env::var("MQ_HOME"))
        .unwrap_or_else(|_| "/opt/mqm".to_string());

    let target_pointer_width =
        env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_else(|_| "64".to_string());
    let lib_dir = if target_pointer_width == "64" {
        "lib64"
    } else {
        "lib"
    };
    let lib_path = format!("{}/{}", mq_home, lib_dir);

    println!("cargo:rustc-link-search=native={}", lib_path);

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
    }

    // On macOS no rpath is embedded by default, so the app can't find the IBM MQ
    // client dylib at runtime unless the user sets DYLD_LIBRARY_PATH manually.
    // Embed the installation lib dir as an rpath so it loads out of the box.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure_ibm_mq();

    let cargo_version = env!("CARGO_PKG_VERSION");
    let config_path = "tauri.conf.json";

    println!("cargo:rerun-if-changed={config_path}");

    let content = std::fs::read_to_string(config_path)
        .map_err(|error| format!("Failed to read {config_path}: {error}"))?;
    let mut config = serde_json::from_str::<serde_json::Value>(&content)
        .map_err(|error| format!("Failed to parse {config_path}: {error}"))?;

    match config["version"].as_str() {
        Some(current_version) if current_version != cargo_version => {
            config["version"] = serde_json::Value::String(cargo_version.to_string());
            let updated_content = serde_json::to_string_pretty(&config)
                .map_err(|error| format!("Failed to format {config_path}: {error}"))?;
            std::fs::write(config_path, updated_content)
                .map_err(|error| format!("Failed to write {config_path}: {error}"))?;
        }
        Some(_) => {}
        None => {
            return Err(format!("Missing string version field in {config_path}").into());
        }
    }

    tauri_build::build();
    Ok(())
}
