fn main() {
    if std::env::var("CARGO_FEATURE_IBM_MQ").is_ok() {
        println!("cargo:rerun-if-env-changed=MQ_INSTALLATION_PATH");
        println!("cargo:rerun-if-env-changed=MQ_HOME");

        let mq_home = std::env::var("MQ_INSTALLATION_PATH")
            .or_else(|_| std::env::var("MQ_HOME"))
            .unwrap_or_else(|_| "/opt/mqm".to_string());

        let target_pointer_width =
            std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_else(|_| "64".to_string());
        let lib_dir = if target_pointer_width == "64" {
            "lib64"
        } else {
            "lib"
        };
        let lib_path = format!("{}/{}", mq_home, lib_dir);

        println!("cargo:rustc-link-search=native={}", lib_path);

        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
        }

        // On macOS no rpath is embedded by default, so the app can't find the IBM MQ
        // client dylib at runtime unless the user sets DYLD_LIBRARY_PATH manually.
        // Embed the installation lib dir as an rpath so it loads out of the box.
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
        }
    }
}
