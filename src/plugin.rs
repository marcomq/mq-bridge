//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Native plugins: endpoints and middleware loaded from shared libraries at runtime.
//!
//! ```no_run
//! let info = mq_bridge::plugin::load_endpoint_plugin("./libmq_bridge_pulsar.so")?;
//! println!("registered `{}` (plugin {})", info.name, info.version);
//! # Ok::<_, anyhow::Error>(())
//! ```
//!
//! A route then names it like any registered factory: `custom: { name: pulsar, config: {..} }`.
//! Plugins are native code with the full privileges of the process, not sandboxed scripts.
//! Authors use [`sdk`] (`plugin-sdk` feature) and check the result with [`conformance`];
//! nobody implements [`crate::support::plugin_abi`] by hand.
//!
//! # Storage
//!
//! | What | Where | Holds |
//! |---|---|---|
//! | endpoint registry | `CUSTOM_ENDPOINT_REGISTRY` in [`crate::extensions`] | name → `CustomEndpointFactory` |
//! | middleware registry | `CUSTOM_MIDDLEWARE_REGISTRY` in [`crate::extensions`] | name → `CustomMiddlewareFactory` |
//! | open libraries | `loaded_plugins()` here | canonical path → `LoadedPlugin`s, never removed |
//! | kill switch | `LOADING_DISABLED` here | set once by [`disable_plugin_loading`] |
//! | discovery switch | [`discovery`] (`MQB_PLUGIN_DISCOVERY`, [`enable_plugin_discovery`]) | off by default |
//!
//! A plugin is nothing special once loaded: it sits in the same two registries as a Rust
//! factory from [`crate::extensions`], wrapped in [`PluginEndpointFactory`] /
//! [`PluginMiddlewareFactory`], which turn trait calls into ABI calls.
//! `loaded_plugins()` only keeps the library mapped and answers "is this file loaded?".
//!
//! # Loading (`load.rs`)
//!
//! 1. **Open**: `dlopen` the file, call `mq_bridge_plugin_v1` (and `mq_bridge_plugin_v1_at`
//!    for a library with several plugins). Each returns a function table.
//! 2. **Check**: the ABI major must match; read name, version and capability bits; hand the
//!    plugin the host table (`host.rs`: logging, metrics); create its factory; read its
//!    config schemas.
//! 3. **Register**: `register_plugin` puts the endpoint and/or middleware into the
//!    registries under the plugin's name, all or nothing per library; then the library goes
//!    into `loaded_plugins()`. Loading the same file again returns the first result.
//!
//! # When registration happens
//!
//! - Explicitly: [`load_endpoint_plugin`] (CLI `--plugin`, config `plugins:`),
//!   [`load_installed_plugin`], or [`register_plugin_table`] for a table linked into the host.
//! - On demand, only with discovery on: when a route names something no registry holds,
//!   [`discovery`] loads `libmq_bridge_<name>` from the search path, else scans every
//!   library there (one library such as `full` provides many names).
//!
//! # Adding and overriding
//!
//! - A **new name** is simply added: every lookup asks the registry first.
//! - An **existing name is never replaced**: an explicit load fails on a name already
//!   registered; discovery skips that name and registers the library's other names.
//! - A **built-in compiled into the host always wins**; the registry is not asked for it.
//!   When its cargo feature is off, the host falls back to a plugin: an endpoint such as
//!   `kafka: {..}` resolves its type name through the registry and discovery
//!   (`builtin_endpoint_factory` in `endpoints`); `deduplication`, `compression`,
//!   `encryption`, `filter` and `aggregate` go, as one list, to the plugin middleware
//!   `builtin_middleware` (`served_by_plugin` in `middleware`). `metrics` and `otel` never do.
//! - To **replace a built-in**, the host must not register its own copy: the app's
//!   `MQB_PLUGIN_OVERRIDE` loads the installed plugin instead of registering the built-in.
//!
//! # Middleware that wraps the endpoint (ABI 1.5)
//!
//! A plain plugin middleware maps or drops messages. One with `MQB_CAP_MIDDLEWARE_WRAP`
//! gets the inner endpoint instead: the host exports it as a function table with the same
//! [`sdk`] code a plugin uses to export its own, and the plugin drives it as an endpoint
//! (`LoadedPlugin::lent`).
//!
//! A loaded library is never unloaded: endpoint handles and in-flight batches may still
//! point into it.
//!
//! # Files
//!
//! | File | Side | Does |
//! |---|---|---|
//! | `load.rs`, `loaded_plugin.rs` | host | open, check, register; the kept table + factory |
//! | [`discovery`] | host | search path, find a library by name, trust check |
//! | `endpoint.rs`, `middleware.rs` | host | ABI table → `CustomEndpointFactory` / `CustomMiddlewareFactory` |
//! | `message.rs`, `completion.rs` | both | message copies across the ABI; awaiting async calls |
//! | `host.rs`, `crash.rs` | host | table given to plugins (logs, metrics); crash reports naming the library |
//! | `forward.rs` | plugin | sends the plugin's logs and metrics to the host |
//! | [`sdk`] | plugin | Rust trait objects → ABI table; also used by the host to lend endpoints |
//! | [`conformance`], `test_support` | tests | the suite every plugin should pass |

mod completion;
#[cfg(feature = "plugin-sdk")]
pub mod conformance;
mod crash;
pub mod discovery;
mod endpoint;
mod forward;
mod host;
mod load;
mod loaded_plugin;
mod message;
mod middleware;
#[cfg(feature = "plugin-sdk")]
pub mod sdk;
// The host lends endpoints to wrapping middleware through the same export code.
#[cfg(not(feature = "plugin-sdk"))]
#[allow(dead_code, unused_imports)]
mod sdk;
#[cfg(feature = "test-utils")]
pub mod test_support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::extensions::{
    register_endpoint_factory, register_middleware_factory, unregister_endpoint_factory,
    unregister_middleware_factory,
};

pub use crate::support::config_schema::{
    endpoint_uri_schema, UriPosition, UriSchema, INFER_SCALARS_ANNOTATION, URI_ANNOTATION,
};
pub use discovery::{
    discover_all_endpoint_plugins, discover_all_endpoint_plugins_in, discover_endpoint_plugin,
    discover_endpoint_plugin_in, discover_middleware_plugin, discover_middleware_plugin_in,
    discovery_enabled, enable_plugin_discovery, library_file_name, load_installed_plugin,
    plugin_search_path, search_path_hint,
};
pub(crate) use discovery::{discover_endpoint_plugin_or_scan, discover_middleware_plugin_or_scan};
pub use endpoint::PluginEndpointFactory;
pub use host::PLUGIN_LOG_TARGET;
pub(crate) use load::load_discovered_plugins;
pub use load::{load_endpoint_plugin, load_endpoint_plugins, register_plugin_table};
pub(crate) use loaded_plugin::LoadedPlugin;
pub use loaded_plugin::PluginInfo;
pub use middleware::PluginMiddlewareFactory;

static LOADING_DISABLED: AtomicBool = AtomicBool::new(false);

/// Every opened library, by canonical path; in-process tables under `<in-process>/<name>`.
fn loaded_plugins() -> &'static Mutex<HashMap<PathBuf, Vec<Arc<LoadedPlugin>>>> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Vec<Arc<LoadedPlugin>>>>> = OnceLock::new();
    LOADED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Switches plugin loading off for the rest of the process: no library is opened after
/// this, by path or by discovery. There is no way back. Plugins already loaded stay.
///
/// For a host that must guarantee no native code is loaded at runtime, without relying
/// on the environment ([`discovery::DISCOVERY_VAR`] only covers discovery).
pub fn disable_plugin_loading() {
    // Held across the store so a load already past its check finishes before this returns.
    let _loading = loaded_plugins()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    LOADING_DISABLED.store(true, Ordering::SeqCst);
}

/// Whether [`disable_plugin_loading`] has been called.
pub fn plugin_loading_disabled() -> bool {
    LOADING_DISABLED.load(Ordering::SeqCst)
}

/// The plugins already loaded from `path`, if any.
pub(crate) fn loaded_from(path: &Path) -> Option<Vec<PluginInfo>> {
    let resolved = std::fs::canonicalize(path).ok()?;
    let loaded = loaded_plugins().lock().ok()?;
    let plugins = loaded.get(&resolved)?;
    Some(plugins.iter().map(|p| p.info.clone()).collect())
}

/// Endpoint names currently provided by loaded plugins.
pub fn loaded_endpoint_plugins() -> Vec<PluginInfo> {
    loaded_plugins()
        .lock()
        .map(|loaded| loaded.values().flatten().map(|p| p.info.clone()).collect())
        .unwrap_or_default()
}

/// Puts a plugin's endpoint and middleware into the [`crate::extensions`] registries.
fn register_plugin(plugin: &Arc<LoadedPlugin>) -> anyhow::Result<()> {
    let info = &plugin.info;
    let endpoint = info.supports_consumer || info.supports_publisher;
    if endpoint {
        register_endpoint_factory(
            plugin.name(),
            Arc::new(PluginEndpointFactory::new(Arc::clone(plugin))),
        )?;
    }
    if info.supports_middleware {
        if let Err(error) = register_middleware_factory(
            plugin.name(),
            Arc::new(PluginMiddlewareFactory::new(Arc::clone(plugin))),
        ) {
            if endpoint {
                unregister_endpoint_factory(plugin.name());
            }
            return Err(error);
        }
    }
    Ok(())
}

fn unregister_plugin(info: &PluginInfo) {
    if info.supports_consumer || info.supports_publisher {
        unregister_endpoint_factory(&info.name);
    }
    if info.supports_middleware {
        unregister_middleware_factory(&info.name);
    }
}
