//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Test helpers for endpoint-plugin authors.
//!
//! A plugin's real test is loading the compiled artifact, so a test needs the
//! path of a freshly built `cdylib`. [`build_plugin_cdylib`] produces it by
//! building the package and reading the artifact path back out of cargo, which
//! keeps the test independent of target directory layout and file extensions.
//!
//! ```no_run
//! # #[tokio::test]
//! # async fn plugin_round_trip() -> anyhow::Result<()> {
//! let library = mq_bridge::plugin::test_support::build_plugin_cdylib(".", "mq-bridge-pulsar")?;
//! mq_bridge::plugin::load_endpoint_plugin(&library)?;
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};

/// Builds `package` as a shared library and returns the artifact path.
///
/// `manifest_dir` is any directory inside the package's workspace. The build
/// uses the same profile as the running test, so a `cargo test --release` run
/// loads a release plugin.
pub fn build_plugin_cdylib(
    manifest_dir: impl AsRef<Path>,
    package: &str,
) -> anyhow::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut command = Command::new(cargo);
    command
        .current_dir(manifest_dir.as_ref())
        .args([
            "build",
            "--message-format=json-render-diagnostics",
            "--package",
            package,
        ])
        // Nested cargo runs inherit the outer build's flags otherwise, which
        // rebuilds the world under a different fingerprint.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    if !cfg!(debug_assertions) {
        command.arg("--release");
    }

    let output = command
        .output()
        .with_context(|| format!("failed to run cargo to build plugin `{package}`"))?;
    if !output.status.success() {
        bail!(
            "building plugin `{package}` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    artifact_path(&output.stdout, package).ok_or_else(|| {
        anyhow!(
            "cargo built `{package}` but produced no cdylib artifact; \
             add `crate-type = [\"cdylib\"]` to its [lib] section"
        )
    })
}

/// Picks the package's shared-library artifact out of cargo's JSON message stream.
fn artifact_path(stdout: &[u8], package: &str) -> Option<PathBuf> {
    let wanted = package.replace('-', "_");
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| {
            message["target"]["name"]
                .as_str()
                .is_some_and(|name| name.replace('-', "_") == wanted)
        })
        .filter(|message| {
            message["target"]["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "cdylib"))
        })
        .filter_map(|message| {
            message["filenames"]
                .as_array()?
                .iter()
                .filter_map(|name| name.as_str())
                .find(|name| is_shared_library(name))
                .map(PathBuf::from)
        })
        .next_back()
}

fn is_shared_library(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "so" | "dylib" | "dll"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_path_picks_the_packages_shared_library() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"other","kind":["cdylib"]},"filenames":["/t/libother.so"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"my-plugin","kind":["lib","cdylib"]},"filenames":["/t/libmy_plugin.rlib","/t/libmy_plugin.dylib"]}"#,
            "\n",
            r#"{"reason":"build-finished","success":true}"#,
        );
        assert_eq!(
            artifact_path(stdout.as_bytes(), "my-plugin"),
            Some(PathBuf::from("/t/libmy_plugin.dylib"))
        );
    }

    #[test]
    fn artifact_path_ignores_rlib_only_packages() {
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"plain","kind":["lib"]},"filenames":["/t/libplain.rlib"]}"#;
        assert_eq!(artifact_path(stdout.as_bytes(), "plain"), None);
    }
}
