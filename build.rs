fn main() {
    if std::env::var("CARGO_FEATURE_IBM_MQ").is_ok() {
        let mq_home = std::env::var("MQ_INSTALLATION_PATH")
            .or_else(|_| std::env::var("MQ_HOME"))
            .unwrap_or_else(|_| "/opt/mqm".to_string());

        let lib_path = format!("{}/lib64", mq_home);

        println!("cargo:rustc-link-search=native={}", lib_path);
        // In production, you might prefer setting LD_LIBRARY_PATH instead of hardcoding rpath,
        // but rpath is convenient for containerized deployments.
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_path);
    }
}
