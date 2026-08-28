//! Loading native mq-bridge endpoint plugins named by the config or the CLI.
//!
//! A plugin is a shared library holding an endpoint (and optionally a
//! middleware) that was never compiled into this app — Pulsar, a proprietary
//! broker, an in-house transport. Loading it registers the endpoint under its
//! own name, after which routes use it like any built-in one:
//!
//! ```yaml
//! plugins:
//!   - "${MQB_PLUGIN_DIR}/libmq_bridge_pulsar.so"
//!
//! routes:
//!   orders:
//!     input:
//!       custom:
//!         name: pulsar
//!         config: { url: "pulsar://localhost:6650" }
//! ```
//!
//! Every build can load plugins; no cargo feature is involved. Paths go through
//! the usual config placeholder expansion, so `${VAR}` works.
//!
//! Plugins must be loaded **before** any route is built, or the endpoint name is
//! not yet registered and route startup fails with a confusing "unknown
//! endpoint" error. Plugins are startup-only: changing the configured set needs
//! a restart. A loaded library stays registered for the life of the process.
//!
//! A plugin is native code with the same privileges as this process. Treat the
//! libraries you list like any other native dependency.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;

/// Registers endpoints that ship with mq-bridge-app but are maintained as
/// separate crates. Cache the result because startup has several entry paths.
pub fn register_builtin_endpoints() -> anyhow::Result<()> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| mq_bridge_pulsar::register().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn resolved_plugin_paths(
    paths: &[String],
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<Vec<PathBuf>> {
    paths
        .iter()
        .filter_map(|path| {
            let path = path.trim();
            (!path.is_empty()).then_some(path)
        })
        .map(|path| {
            let expanded = shellexpand::env_with_context_no_errors(path, |key| {
                std::env::var(key)
                    .ok()
                    .or_else(|| env_vars.get(key).cloned())
            });
            std::fs::canonicalize(expanded.as_ref())
                .with_context(|| format!("failed to resolve plugin `{path}`"))
        })
        .collect()
}

/// Resolves configured plugin paths without loading native code.
pub fn canonical_plugin_paths(
    paths: &[String],
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<HashSet<PathBuf>> {
    Ok(resolved_plugin_paths(paths, env_vars)?
        .into_iter()
        .collect())
}

/// Loads every operator-trusted startup plugin in `paths`.
///
/// Loading the same library twice is a no-op, so this is safe to call again
/// from another startup path.
///
/// A path that fails to load is fatal: continuing would only fail later, when a
/// route asks for an endpoint nobody registered.
pub fn load_trusted_plugins(
    paths: &[String],
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<Vec<mq_bridge::plugin::PluginInfo>> {
    register_builtin_endpoints().context("failed to register built-in endpoints")?;
    let paths = resolved_plugin_paths(paths, env_vars)?;
    let mut plugins = Vec::with_capacity(paths.len());
    for path in paths {
        let info = mq_bridge::plugin::load_endpoint_plugin(&path)
            .with_context(|| format!("failed to load plugin `{}`", path.display()))?;
        plugins.push(info);
    }
    Ok(plugins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_loads_nothing() {
        assert!(
            load_trusted_plugins(&[], &HashMap::new())
                .unwrap()
                .is_empty()
        );
        assert!(
            load_trusted_plugins(&["  ".to_string()], &HashMap::new())
                .unwrap()
                .is_empty()
        );
        assert!(mq_bridge::extensions::get_endpoint_factory("pulsar").is_some());
    }

    #[test]
    fn a_bad_path_names_the_plugin_in_the_error() {
        let error = load_trusted_plugins(&["/nonexistent/libnope.so".to_string()], &HashMap::new())
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("/nonexistent/libnope.so"),
            "{error:#}"
        );
    }

    #[test]
    fn inline_env_vars_expand_before_canonicalization() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let plugin = dir.join("Cargo.toml");
        let env_vars =
            HashMap::from([("PLUGIN_DIR".to_string(), dir.to_string_lossy().into_owned())]);

        let paths =
            canonical_plugin_paths(&["${PLUGIN_DIR}/Cargo.toml".to_string()], &env_vars).unwrap();

        assert_eq!(paths, HashSet::from([plugin.canonicalize().unwrap()]));
    }
}
