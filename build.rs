fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only the `ibm-mq-static` path needs this. It was previously gated on
    // CARGO_FEATURE_IBM_MQ — the *dlopen* feature — which is exactly backwards:
    // dlopen resolves the client by name at runtime and never consults the
    // link search path, so the only effect was to bake a /opt/mqm/lib64 RPATH
    // into every binary built with `full`. That entry is dead weight when the
    // directory is absent, and an RPATH naming a directory an unprivileged user
    // could later create is somewhere a library can be injected from.
    //
    // libmqm-sys emits its own `rustc-link-search` under its `link_mqm`
    // feature; what it does not emit is an rpath, so the binary would otherwise
    // need LD_LIBRARY_PATH to find libmqm_r at run time. That is what this adds.
    if std::env::var("CARGO_FEATURE_IBM_MQ_STATIC").is_ok() {
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
    // Only compile protos if the grpc feature is enabled and the build dependency is present.
    // Note: Cargo features for build-dependencies are separate, but we check the env var
    // to see if the feature was requested for the package.
    #[cfg(feature = "grpc")]
    {
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
        // Where protoc comes from, in precedence order:
        //
        //   1. $PROTOC, if the caller set it. It used to be overwritten
        //      unconditionally, which left a distro or conda-forge build no way
        //      to point this at the protoc it packages.
        //   2. the prebuilt binary from protoc-bin-vendored, if the
        //      `vendored-protoc` feature is on (`full` enables it).
        //   3. `protoc` on PATH, which is prost-build's own default.
        //
        // So `--features full` still needs nothing installed, and
        // `--features full-dynamic` takes protoc from the environment.
        println!("cargo:rerun-if-env-changed=PROTOC");
        #[cfg(feature = "vendored-protoc")]
        if std::env::var_os("PROTOC").is_none() {
            std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
        }
        println!("cargo:rerun-if-changed=src/endpoints/grpc/proto/mqbridge/bridge.proto");
        tonic_prost_build::configure()
            .file_descriptor_set_path(out_dir.join("mqbridge_descriptor.bin"))
            .compile_protos(
                &["src/endpoints/grpc/proto/mqbridge/bridge.proto"],
                &["src/endpoints/grpc/proto"],
            )?;
        // Test-only fixture, excluded from the published crate. Guarding on its presence
        // keeps downstream builds from compiling it and from failing when it is absent.
        println!("cargo:rerun-if-changed=tests/fixtures/grpc_dynamic.proto");
        if std::path::Path::new("tests/fixtures/grpc_dynamic.proto").exists() {
            tonic_prost_build::configure()
                .file_descriptor_set_path(out_dir.join("grpc_dynamic_test_descriptor.bin"))
                .compile_protos(&["tests/fixtures/grpc_dynamic.proto"], &["tests/fixtures"])?;
        }
    }
    Ok(())
}
