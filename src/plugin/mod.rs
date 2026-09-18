//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Loading endpoint plugins from native shared libraries.
//!
//! An endpoint can live in its own crate and its own release cycle, be compiled
//! to a `cdylib`, and be loaded into any mq-bridge process at runtime:
//!
//! ```no_run
//! let info = mq_bridge::plugin::load_endpoint_plugin("./libmq_bridge_pulsar.so")?;
//! println!("registered endpoint `{}` (plugin {})", info.name, info.version);
//! # Ok::<_, anyhow::Error>(())
//! ```
//!
//! The endpoint is then usable from configuration under its plugin-provided
//! name, exactly like a factory registered through [`crate::extensions`]:
//!
//! ```yaml
//! input:
//!   custom:
//!     name: pulsar
//!     config: { url: "pulsar://localhost:6650" }
//! ```
//!
//! Loading is explicit and never implicit: installing a package does not
//! register anything. It is also permanent — a loaded library is kept for the
//! life of the process, because unloading while endpoint handles or in-flight
//! batches still exist cannot be made safe.
//!
//! Plugins are native code with the full privileges of the host process. Treat
//! them like any other native dependency, not like sandboxed scripts.
//!
//! Endpoint authors do not implement [`crate::support::plugin_abi`] by hand:
//! enable the `plugin-sdk` feature and use [`export_endpoint_plugin!`](crate::export_endpoint_plugin)
//! from [`sdk`], then check the result with [`conformance`].

#[cfg(feature = "plugin-sdk")]
pub mod conformance;
mod endpoint;
mod message;
mod middleware;
#[cfg(feature = "plugin-sdk")]
pub mod sdk;
#[cfg(feature = "test-utils")]
pub mod test_support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::support::plugin_abi::{
    check_compatibility, MqbBuffer, MqbFactoryHandle, MqbPluginEntry, MqbPluginVTable, MQB_OK,
    MQB_PLUGIN_ENTRY_SYMBOL,
};
use anyhow::{anyhow, Context};

use crate::extensions::{
    get_endpoint_factory, get_middleware_factory, register_endpoint_factory,
    register_middleware_factory, unregister_endpoint_factory,
};

pub use endpoint::PluginEndpointFactory;
pub use middleware::PluginMiddlewareFactory;

/// What a successfully loaded plugin reported about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// Endpoint name the plugin was registered under.
    pub name: String,
    /// Plugin's own version string.
    pub version: String,
    /// ABI major version the plugin was built against.
    pub abi_major: u32,
    /// ABI minor version the plugin was built against.
    pub abi_minor: u32,
    /// Whether the plugin can create input endpoints.
    pub supports_consumer: bool,
    /// Whether the plugin can create output endpoints.
    pub supports_publisher: bool,
    /// Whether the plugin also provides a middleware under the same name.
    pub supports_middleware: bool,
    /// Resolved path of the loaded library.
    pub path: PathBuf,
}

/// A loaded plugin library plus the factory created from it.
///
/// Held in [`loaded_plugins`] for the life of the process so neither the
/// library nor the factory can be dropped while an endpoint still points at it.
pub(crate) struct LoadedPlugin {
    /// Kept only to keep the library mapped; every code pointer below lives in it.
    _library: libloading::Library,
    table: *const MqbPluginVTable,
    factory: MqbFactoryHandle,
    info: PluginInfo,
}

/// Safety: the ABI requires every handle to be usable from any thread, and the
/// table is immutable `'static` data inside the library.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

impl LoadedPlugin {
    pub(crate) fn table(&self) -> &MqbPluginVTable {
        // Safety: validated at load time and kept alive by `_library`.
        unsafe { &*self.table }
    }

    pub(crate) fn factory(&self) -> MqbFactoryHandle {
        self.factory
    }

    pub(crate) fn name(&self) -> &str {
        &self.info.name
    }

    /// Consumes an error buffer produced by a failed call and returns its text.
    pub(crate) fn take_error(&self, buffer: MqbBuffer) -> String {
        if buffer.is_empty() {
            return "plugin reported no error message".to_string();
        }
        // Safety: the plugin owns the buffer until it is handed back below.
        let text = unsafe { String::from_utf8_lossy(buffer.as_bytes()).into_owned() };
        unsafe { (self.table().buffer_free)(buffer) };
        text
    }
}

fn loaded_plugins() -> &'static Mutex<HashMap<PathBuf, Arc<LoadedPlugin>>> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Arc<LoadedPlugin>>>> = OnceLock::new();
    LOADED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Loads a native endpoint plugin and registers the endpoint it provides.
///
/// Call once, before starting any route that uses the endpoint. Loading the
/// same file twice is a no-op that returns the original registration, so
/// several components can each ensure their endpoint is present.
///
/// Fails if the file cannot be loaded, does not export the discovery symbol,
/// was built against an incompatible ABI major version, or provides an endpoint
/// name that a different factory already occupies.
pub fn load_endpoint_plugin(path: impl AsRef<Path>) -> anyhow::Result<PluginInfo> {
    let path = path.as_ref();
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("plugin library not found: {}", path.display()))?;

    let mut loaded = loaded_plugins()
        .lock()
        .map_err(|_| anyhow!("plugin registry lock poisoned"))?;
    if let Some(existing) = loaded.get(&resolved) {
        return Ok(existing.info.clone());
    }

    let plugin = Arc::new(open_plugin(&resolved)?);
    let info = plugin.info.clone();
    // Nothing to register: returning here drops `plugin`, which frees the
    // factory. Accepting it would leave a library mapped that no route can use.
    if !(info.supports_consumer || info.supports_publisher || info.supports_middleware) {
        return Err(anyhow!(
            "plugin {} declares no capabilities: it provides neither an endpoint nor a middleware",
            resolved.display()
        ));
    }
    // A name already taken by a different factory is a configuration mistake:
    // silently replacing it would reroute traffic to the wrong place.
    let taken = (info.supports_consumer || info.supports_publisher)
        && get_endpoint_factory(plugin.name()).is_some();
    let taken_middleware =
        info.supports_middleware && get_middleware_factory(plugin.name()).is_some();
    if taken || taken_middleware {
        return Err(anyhow!(
            "`{}` from {} is already registered by another factory; \
             unload it or rename the plugin's endpoint",
            plugin.name(),
            resolved.display()
        ));
    }

    if info.supports_consumer || info.supports_publisher {
        register_endpoint_factory(
            plugin.name(),
            Arc::new(PluginEndpointFactory::new(Arc::clone(&plugin))),
        )?;
    }
    if info.supports_middleware {
        if let Err(error) = register_middleware_factory(
            plugin.name(),
            Arc::new(PluginMiddlewareFactory::new(Arc::clone(&plugin))),
        ) {
            if info.supports_consumer || info.supports_publisher {
                unregister_endpoint_factory(plugin.name());
            }
            return Err(error);
        }
    }
    loaded.insert(resolved, plugin);
    tracing::info!(
        name = %info.name,
        version = %info.version,
        endpoint = info.supports_consumer || info.supports_publisher,
        middleware = info.supports_middleware,
        path = %info.path.display(),
        "loaded plugin",
    );
    Ok(info)
}

/// Endpoint names currently provided by loaded plugins.
pub fn loaded_endpoint_plugins() -> Vec<PluginInfo> {
    loaded_plugins()
        .lock()
        .map(|loaded| loaded.values().map(|p| p.info.clone()).collect())
        .unwrap_or_default()
}

fn open_plugin(path: &Path) -> anyhow::Result<LoadedPlugin> {
    // Safety: dlopen runs the library's initialisers — inherently trusting the
    // file, as documented above.
    let library = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("failed to load plugin library {}", path.display()))?;

    let table = unsafe {
        let entry: libloading::Symbol<MqbPluginEntry> =
            library.get(MQB_PLUGIN_ENTRY_SYMBOL).with_context(|| {
                format!(
                    "{} does not export `{}`; it is not an mq-bridge endpoint plugin",
                    path.display(),
                    String::from_utf8_lossy(
                        &MQB_PLUGIN_ENTRY_SYMBOL[..MQB_PLUGIN_ENTRY_SYMBOL.len() - 1]
                    ),
                )
            })?;
        entry()
    };
    if table.is_null() {
        return Err(anyhow!(
            "plugin {} returned a null function table",
            path.display()
        ));
    }
    // Safety: non-null and owned by the library, which stays loaded.
    let table_ref = unsafe { &*table };
    check_compatibility(table_ref)
        .map_err(|mismatch| anyhow!("cannot load plugin {}: {mismatch}", path.display()))?;

    let name = read_static_str(table_ref.name, "name")
        .with_context(|| format!("plugin {}", path.display()))?;
    if name.is_empty() {
        return Err(anyhow!(
            "plugin {} reports an empty endpoint name",
            path.display()
        ));
    }
    let version = read_static_str(table_ref.version, "version").unwrap_or_default();

    let mut factory = MqbFactoryHandle::NULL;
    let mut error = MqbBuffer::EMPTY;
    let status = unsafe { (table_ref.factory_create)(&mut factory, &mut error) };

    let plugin = LoadedPlugin {
        _library: library,
        table,
        factory,
        info: PluginInfo {
            name,
            version,
            abi_major: table_ref.abi_major,
            abi_minor: table_ref.abi_minor,
            supports_consumer: table_ref.capabilities
                & crate::support::plugin_abi::MQB_CAP_CONSUMER
                != 0,
            supports_publisher: table_ref.capabilities
                & crate::support::plugin_abi::MQB_CAP_PUBLISHER
                != 0,
            supports_middleware: table_ref.capabilities
                & crate::support::plugin_abi::MQB_CAP_MIDDLEWARE
                != 0,
            path: path.to_path_buf(),
        },
    };
    if status != MQB_OK {
        return Err(anyhow!(
            "plugin {} failed to create its endpoint factory: {}",
            path.display(),
            plugin.take_error(error)
        ));
    }
    if plugin.factory.is_null() {
        return Err(anyhow!(
            "plugin {} reported success but returned a null factory",
            path.display()
        ));
    }
    Ok(plugin)
}

fn read_static_str(
    slice: crate::support::plugin_abi::MqbSlice,
    field: &str,
) -> anyhow::Result<String> {
    if slice.len != 0 && slice.ptr.is_null() {
        return Err(anyhow!("plugin `{field}` slice has a null pointer"));
    }
    // Safety: the ABI requires these to point at `'static` data in the library.
    let bytes = unsafe { slice.as_bytes() };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .with_context(|| format!("plugin `{field}` is not valid UTF-8"))
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // Only reachable if loading failed before the plugin was published;
        // successfully loaded plugins live in `loaded_plugins` forever.
        if !self.factory.is_null() {
            unsafe { (self.table().factory_free)(self.factory) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reports_the_path() {
        let err = load_endpoint_plugin("/nonexistent/libnope.so").unwrap_err();
        assert!(
            err.to_string().contains("plugin library not found"),
            "{err:#}"
        );
    }

    #[test]
    fn a_library_without_the_entry_symbol_is_rejected() {
        // Any loadable native library that is not a plugin: the host's own
        // executable is guaranteed to exist and be loadable.
        let exe = std::env::current_exe().unwrap();
        let err = load_endpoint_plugin(exe).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains("not an mq-bridge endpoint plugin") || text.contains("failed to load"),
            "{text}"
        );
    }
}
