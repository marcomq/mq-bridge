use std::env;
use std::fs;
use std::process::Command;

#[test]
#[ignore = "Requires git, internet access, and takes time. Runs downstream integration tests."]
fn armature_messaging_test() {
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
    dbg!(&test_dir);

    let repo_url = "https://github.com/pegasusheavy/armature.git";
    let branch = "develop";
    let subdirectory = "armature-messaging";

    // 1. Clone Repo
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

    let project_dir = test_dir.join(subdirectory);
    assert!(
        project_dir.exists(),
        "armature-messaging directory not found in cloned repo"
    );
    let project_dir = project_dir
        .canonicalize()
        .expect("Failed to canonicalize project dir");

    // 2. Get absolute path to current mq-bridge
    let mq_bridge_path = env::current_dir()
        .expect("Failed to get current dir")
        .canonicalize()
        .expect("Failed to canonicalize path");

    // 3. Patch dependency using cargo add to point to local version
    let cargo_bin = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
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

    // Patch armature-messaging source code to be compatible with mq-bridge 0.2.0 breaking changes
    // We need to convert Option<CanonicalMessage> to MessageDisposition using .into()
    let source_path = project_dir.join("src/mq_bridge.rs");
    if source_path.exists() {
        println!("Patching {:?} for API compatibility...", source_path);
        let content = fs::read_to_string(&source_path).expect("Failed to read mq_bridge.rs");
        let new_content = content
            .replace("(received.commit)(None)", "(received.commit)(None.into())")
            .replace(
                "(received.commit)(Some(response))",
                "(received.commit)(Some(response).into())",
            );
        fs::write(&source_path, new_content).expect("Failed to write patched mq_bridge.rs");
    }

    // 4. Run tests
    println!(
        "Running armature-messaging tests in {:?} using {}...",
        project_dir, cargo_bin
    );
    assert!(project_dir.exists(), "Project directory missing");
    let status = Command::new(&cargo_bin)
        .arg("test")
        .arg("--features=mq-bridge-full")
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
