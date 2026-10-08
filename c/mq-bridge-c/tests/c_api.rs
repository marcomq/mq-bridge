//! A C program written against `include/mq_bridge.h` builds, links and runs.
//!
//! Compiles `examples/c-library` with the system C compiler (`$CC`, default `cc`),
//! so the header, the examples and the library are checked together.
//! `MQB_C_ASAN=1` builds the programs with AddressSanitizer.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(mut command: Command) -> String {
    let output = command
        .output()
        .unwrap_or_else(|err| panic!("could not run {command:?}: {err}"));
    assert!(
        output.status.success(),
        "{command:?} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Cargo does not build a cdylib for its own integration tests, so build it here.
/// The in-memory endpoints the examples use need no feature.
fn build_library() -> PathBuf {
    let mut cargo = Command::new(env!("CARGO"));
    cargo
        .args(["build", "-p", "mq-bridge-c", "--no-default-features"])
        .arg("--message-format=json")
        .current_dir(repo_root())
        .stderr(Stdio::inherit());
    let extension = format!(".{}", std::env::consts::DLL_EXTENSION);
    run(cargo)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["target"]["name"] == "mq_bridge")
        .flat_map(|message| message["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|file| file.as_str().map(PathBuf::from))
        .find(|file| file.to_string_lossy().ends_with(&extension))
        .expect("cargo reports the shared library it built")
}

fn compile(library: &Path, source: &str) -> PathBuf {
    let root = repo_root();
    let library_dir = library.parent().expect("the library has a directory");
    let dir = std::env::temp_dir().join(format!("mqb-c-library-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the build directory");
    let program = dir.join(source.trim_end_matches(".c"));

    let mut cc = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()));
    cc.args([
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Wno-unused-parameter",
        "-Werror",
    ])
    .args(std::env::var_os("MQB_C_ASAN").map(|_| "-fsanitize=address"))
    .arg("-I")
    .arg(root.join("include"))
    .arg(root.join("examples/c-library").join(source))
    .arg("-L")
    .arg(library_dir)
    .arg("-lmq_bridge")
    .arg(format!("-Wl,-rpath,{}", library_dir.display()))
    .arg("-o")
    .arg(&program);
    run(cc);
    program
}

#[test]
fn a_c_program_uses_the_library() {
    let library = build_library();
    let smoke = compile(&library, "smoke.c");
    let mut command = Command::new(&smoke);
    command.arg(smoke.parent().expect("the program has a directory"));
    let output = run(command);
    assert!(output.contains("smoke test passed"), "{output}");

    let publish = compile(&library, "publish.c");
    let status = Command::new(publish)
        .output()
        .expect("run the publish example")
        .status;
    assert_eq!(status.code(), Some(2), "no arguments prints the usage");
}
