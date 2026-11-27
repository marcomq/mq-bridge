use std::env;
use std::fs;
use std::process::Command;

#[test]
#[ignore = "Requires git, internet access, and takes time. Runs downstream integration tests."]
fn armature_messaging_test() {
    // External downstream compatibility check. This clones the Armature repo,
    // patches its mq-bridge usage, and runs its tests; keep it explicit opt-in.
    // Define paths: Use system temp dir to avoid locking/conflicts with local target dir
    let target_dir = env::temp_dir().join("mq_bridge_integration_test");
    let test_dir = target_dir.join("armature_test");

    // Clean up previous run
    if test_dir.exists() {
        fs::remove_dir_all(&test_dir).expect("Failed to clean up previous test run");
    }
    fs::create_dir_all(&test_dir).expect("Failed to create test directory");
    let test_dir = test_dir
        .canonicalize()
        .expect("Failed to canonicalize test dir");

    let repo_url = "https://github.com/pegasusheavy/armature.git";
    let branch = "develop";
    let subdirectory = "armature-messaging";

    println!("Cloning {} (branch: {})...", repo_url, branch);
    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--no-checkout",
            "--branch",
            branch,
            repo_url,
            ".",
        ])
        .current_dir(&test_dir)
        .status()
        .expect("Failed to execute git clone");
    assert!(status.success(), "Failed to clone armature repo");

    let status = Command::new("git")
        .args(["sparse-checkout", "set", "--no-cone", "/*", "!/benchmarks"])
        .current_dir(&test_dir)
        .status()
        .expect("Failed to set sparse-checkout");
    assert!(status.success(), "Failed to set sparse-checkout");

    let status = Command::new("git")
        .args(["checkout"])
        .current_dir(&test_dir)
        .status()
        .expect("Failed to checkout");
    assert!(status.success(), "Failed to checkout armature repo");

    // Every workspace member is a submodule upstream, so a plain checkout leaves empty
    // gitlink directories and cargo cannot read the members listed in the root manifest.
    let status = Command::new("git")
        .args([
            "submodule",
            "update",
            "--init",
            "--depth",
            "1",
            "--filter=blob:none",
        ])
        .current_dir(&test_dir)
        .status()
        .expect("Failed to execute git submodule update");
    assert!(status.success(), "Failed to init armature submodules");

    let project_dir = test_dir.join(subdirectory);
    assert!(
        project_dir.exists(),
        "armature-messaging directory not found in cloned repo"
    );
    let project_dir = project_dir
        .canonicalize()
        .expect("Failed to canonicalize project dir");

    // Get absolute path to current mq-bridge
    let mq_bridge_path = env::current_dir()
        .expect("Failed to get current dir")
        .canonicalize()
        .expect("Failed to canonicalize path");

    // Patch dependency using cargo add to point to local version
    let cargo_bin = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // Armature's lock pins versions below mq-bridge's floors (e.g. rdkafka-sys, uuid) and
    // `cargo add` keeps them, so resolve afresh as a downstream `cargo update` would.
    match fs::remove_file(test_dir.join("Cargo.lock")) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            panic!("Failed to remove armature's Cargo.lock: {error}")
        }
        _ => {}
    }
    println!(
        "Patching mq-bridge dependency to local path: {:?}",
        mq_bridge_path
    );
    let status = Command::new(&cargo_bin)
        .args([
            "add",
            "mq-bridge",
            "--path",
            mq_bridge_path.to_str().unwrap(),
        ])
        .current_dir(&project_dir)
        .status()
        .expect("Failed to execute cargo add");
    assert!(status.success(), "Failed to patch mq-bridge dependency");

    println!(
        "Running armature-messaging tests in {:?} using {}...",
        project_dir, cargo_bin
    );
    assert!(project_dir.exists(), "Project directory missing");
    let status = Command::new(&cargo_bin)
        .arg("test")
        .arg("--features=mq-bridge")
        .arg("--")
        .arg("--ignored")
        .current_dir(&project_dir)
        .env("CARGO_TARGET_DIR", "target")
        .env_remove("RUSTC_WRAPPER")
        .status()
        .expect("Failed to execute cargo test");

    assert!(
        status.success(),
        "armature-messaging tests failed with local mq-bridge changes"
    );
}
