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
//! Listing a path is only needed for a library that is not installed under its
//! conventional name. A route asking for an endpoint no factory provides falls
//! back to searching for `libmq_bridge_<name>` (see `mq_bridge::plugin::discovery`),
//! so an installed plugin needs no entry here at all.
//!
//! An extension compiled into this build wins over an installed plugin of the
//! same name, so one binary behaves the same wherever it runs.
//! [`OVERRIDE_VAR`] reverses that per name — how an extension is updated without
//! a new release of this app — and a plugin found but not preferred is reported.
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
        .get_or_init(|| register_each().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

/// Each extension crate this build compiled in. They are cargo features, so a
/// build that leaves one out reaches it through the plugin search path instead.
fn register_each() -> anyhow::Result<()> {
    #[cfg(feature = "pulsar")]
    register_builtin("pulsar", mq_bridge_pulsar::register)?;
    #[cfg(feature = "meilisearch")]
    register_builtin("meilisearch", mq_bridge_meilisearch::register)?;
    Ok(())
}

/// Set to `1`/`true`, or to a comma-separated list of endpoint names, to prefer
/// an installed plugin over the copy compiled into this build.
pub const OVERRIDE_VAR: &str = "MQB_PLUGIN_OVERRIDE";

/// Registers the linked-in copy of `name`, unless an installed plugin should
/// take its place.
///
/// The built-in wins by default, so the same binary behaves the same wherever it
/// runs. An installed plugin replaces it only on request, which is what lets an
/// extension be updated without a new release of this app. Either way a plugin
/// found on the search path is reported, since a library installed and silently
/// ignored is the confusing case.
///
/// Preference is by name, not by version: an *older* installed plugin replaces a
/// newer built-in, because a plugin's version cannot be read without loading it.
#[cfg(any(feature = "pulsar", feature = "meilisearch"))]
fn register_builtin(name: &str, register: fn() -> anyhow::Result<()>) -> anyhow::Result<()> {
    // `MQB_PLUGIN_DISCOVERY=0` resolves endpoints only from what the host
    // registered, so it switches off the probe and the override alike.
    if !mq_bridge::plugin::discovery_enabled() {
        return register();
    }
    let dirs = mq_bridge::plugin::plugin_search_path();
    register_builtin_in(&dirs, name, override_requested(name), register)
}

#[cfg(any(feature = "pulsar", feature = "meilisearch"))]
fn register_builtin_in(
    dirs: &[PathBuf],
    name: &str,
    prefer_installed: bool,
    register: fn() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if prefer_installed {
        // Loading here rather than leaving it to the search a route triggers
        // keeps the registry complete at startup, so `endpoint_config_schemas`
        // — and the UI schema built from it — describes the copy actually in use.
        if let Some(info) = mq_bridge::plugin::discover_endpoint_plugin_in(dirs, name)? {
            tracing::info!(
                "using the installed `{name}` plugin {} from {} instead of the built-in copy",
                info.version,
                info.path.display()
            );
            return Ok(());
        }
        return register();
    }
    if let Some(path) = installed_plugin_path(dirs, name) {
        tracing::warn!(
            "an installed `{name}` plugin was found at {} but the built-in copy is in use; \
             set {OVERRIDE_VAR}={name} to prefer it",
            path.display()
        );
    }
    register()
}

/// The installed library providing `name`, found without loading it — so the
/// built-in path stays free of another crate's initialisation.
#[cfg(any(feature = "pulsar", feature = "meilisearch"))]
fn installed_plugin_path(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let file_name = mq_bridge::plugin::library_file_name(name);
    dirs.iter()
        .map(|dir| dir.join(&file_name))
        .find(|candidate| candidate.is_file())
}

#[cfg(any(feature = "pulsar", feature = "meilisearch"))]
fn override_requested(name: &str) -> bool {
    std::env::var(OVERRIDE_VAR).is_ok_and(|value| override_requested_in(&value, name))
}

#[cfg(any(feature = "pulsar", feature = "meilisearch"))]
fn override_requested_in(value: &str, name: &str) -> bool {
    let value = value.trim();
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "all"
    ) {
        return true;
    }
    value
        .split(',')
        .any(|entry| entry.trim().eq_ignore_ascii_case(name))
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
        assert!(load_trusted_plugins(&[], &HashMap::new())
            .unwrap()
            .is_empty());
        assert!(load_trusted_plugins(&["  ".to_string()], &HashMap::new())
            .unwrap()
            .is_empty());
        #[cfg(feature = "pulsar")]
        assert!(mq_bridge::extensions::get_endpoint_factory("pulsar").is_some());
        #[cfg(feature = "meilisearch")]
        assert!(mq_bridge::extensions::get_endpoint_factory("meilisearch").is_some());
    }

    #[cfg(any(feature = "pulsar", feature = "meilisearch"))]
    mod builtin_preference {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// A directory of its own per test, so the probe sees only what the test
        /// put there and cases can run in parallel.
        fn scratch_dir() -> PathBuf {
            let dir = std::env::temp_dir().join(format!("mqb-plugin-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// A file under the conventional name. Never loaded on the paths that
        /// only probe, so its contents do not have to be a real library.
        fn install_stub(dir: &std::path::Path, name: &str) -> PathBuf {
            let path = dir.join(mq_bridge::plugin::library_file_name(name));
            std::fs::write(&path, b"not a library").unwrap();
            path
        }

        static NOTHING_INSTALLED: AtomicUsize = AtomicUsize::new(0);
        static NOT_PREFERRED: AtomicUsize = AtomicUsize::new(0);
        static PREFERRED_BUT_ABSENT: AtomicUsize = AtomicUsize::new(0);
        static DISCOVERY_OFF: AtomicUsize = AtomicUsize::new(0);

        #[test]
        fn the_builtin_registers_when_nothing_is_installed() {
            let dir = scratch_dir();
            fn register() -> anyhow::Result<()> {
                NOTHING_INSTALLED.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            register_builtin_in(std::slice::from_ref(&dir), "meilisearch", false, register)
                .unwrap();

            assert_eq!(NOTHING_INSTALLED.load(Ordering::SeqCst), 1);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn an_installed_plugin_does_not_replace_the_builtin_unasked() {
            let dir = scratch_dir();
            install_stub(&dir, "meilisearch");
            fn register() -> anyhow::Result<()> {
                NOT_PREFERRED.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            // The stub is not a loadable library, so reaching for it would fail:
            // succeeding proves the built-in path never touched it.
            register_builtin_in(std::slice::from_ref(&dir), "meilisearch", false, register)
                .unwrap();

            assert_eq!(NOT_PREFERRED.load(Ordering::SeqCst), 1);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn preferring_an_installed_plugin_falls_back_when_none_is_installed() {
            let dir = scratch_dir();
            fn register() -> anyhow::Result<()> {
                PREFERRED_BUT_ABSENT.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            register_builtin_in(std::slice::from_ref(&dir), "meilisearch", true, register).unwrap();

            assert_eq!(PREFERRED_BUT_ABSENT.load(Ordering::SeqCst), 1);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn preferring_an_installed_plugin_reaches_for_it() {
            let dir = scratch_dir();
            let path = install_stub(&dir, "meilisearch");
            fn register() -> anyhow::Result<()> {
                panic!("the built-in must not be registered once the installed copy is preferred")
            }

            // The stub cannot load, and that failure is the evidence: the
            // override branch went to the file rather than to the built-in.
            let error =
                register_builtin_in(std::slice::from_ref(&dir), "meilisearch", true, register)
                    .expect_err("a stub library cannot load");

            assert!(
                format!("{error:#}").contains(&path.display().to_string()),
                "{error:#}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn discovery_off_registers_the_builtin_without_probing() {
            fn register() -> anyhow::Result<()> {
                DISCOVERY_OFF.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            // Asserted through the same entry point the switch guards, against a
            // directory holding an unloadable stub.
            let dir = scratch_dir();
            install_stub(&dir, "meilisearch");
            if mq_bridge::plugin::discovery_enabled() {
                register_builtin_in(std::slice::from_ref(&dir), "meilisearch", false, register)
                    .unwrap();
            } else {
                register().unwrap();
            }

            assert_eq!(DISCOVERY_OFF.load(Ordering::SeqCst), 1);
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn a_blanket_value_prefers_every_extension() {
            for value in ["1", "true", "TRUE", " yes ", "on", "all"] {
                assert!(override_requested_in(value, "meilisearch"), "{value}");
                assert!(override_requested_in(value, "pulsar"), "{value}");
            }
        }

        #[test]
        fn a_list_prefers_only_the_names_it_holds() {
            assert!(override_requested_in("meilisearch", "meilisearch"));
            assert!(override_requested_in(" meilisearch , pulsar ", "pulsar"));
            assert!(!override_requested_in("meilisearch", "pulsar"));
        }

        #[test]
        fn an_off_value_prefers_nothing() {
            for value in ["", "  ", "0", "false", "off", "no"] {
                assert!(!override_requested_in(value, "meilisearch"), "{value}");
            }
        }
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
