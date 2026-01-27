fn main() {
    if std::env::var("CARGO_FEATURE_IBM_MQ").is_ok() {
        // Ensure rebuild when these environment variables change
        println!("cargo:rerun-if-env-changed=MQ_INSTALLATION_PATH");
        println!("cargo:rerun-if-env-changed=MQ_HOME");

        let mq_home = std::env::var("MQ_INSTALLATION_PATH")
            .or_else(|_| std::env::var("MQ_HOME"))
            .unwrap_or_else(|_| "/opt/mqm".to_string());

        // Use lib64 on 64-bit systems, lib otherwise
        let lib_dir = if cfg!(target_pointer_width = "64") {
            "lib64"
        } else {
            "lib"
        };
        let lib_path = format!("{}/{}", mq_home, lib_dir);

        println!("cargo:rustc-link-search=native={}", lib_path);
        // In production, you might prefer setting LD_LIBRARY_PATH instead of hardcoding rpath,
        // but rpath is convenient for containerized deployments.
        if cfg!(target_family = "unix") {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
        }
    }
}
