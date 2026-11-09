//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Opening a plugin library and registering what it provides: steps 1-3 in the module docs.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context};

use super::{
    crash, host, loaded_plugins, plugin_loading_disabled, register_plugin, unregister_plugin,
    LoadedPlugin, PluginInfo,
};
use crate::extensions::{get_endpoint_factory, get_middleware_factory};
use crate::support::plugin_abi::{
    check_compatibility, MqbBuffer, MqbFactoryHandle, MqbPluginEntry, MqbPluginListEntry,
    MqbPluginVTable, MQB_OK, MQB_PLUGIN_ENTRY_SYMBOL, MQB_PLUGIN_LIST_SYMBOL, MQB_SCHEMA_ENDPOINT,
    MQB_SCHEMA_MIDDLEWARE,
};

/// Loads a native endpoint plugin and registers the endpoint it provides.
///
/// Call once, before starting any route that uses the endpoint. Loading the
/// same file twice is a no-op that returns the original registration, so
/// several components can each ensure their endpoint is present.
///
/// A library that exports several plugins has all of them registered; this
/// returns the first. See [`load_endpoint_plugins`] for the whole list.
///
/// Fails if the file cannot be loaded, does not export the discovery symbol,
/// was built against an incompatible ABI major version, or provides an endpoint
/// name that a different factory already occupies.
pub fn load_endpoint_plugin(path: impl AsRef<Path>) -> anyhow::Result<PluginInfo> {
    let path = path.as_ref();
    let mut infos = load_endpoint_plugins(path)?;
    if infos.is_empty() {
        // Discovery loaded it before and every name it exports was already taken.
        return Err(anyhow!(
            "{} registered nothing: every name it exports is already taken",
            path.display()
        ));
    }
    Ok(infos.swap_remove(0))
}

/// Loads a native plugin library and registers every plugin it exports.
///
/// All or nothing: if one of them cannot be registered, none is.
pub fn load_endpoint_plugins(path: impl AsRef<Path>) -> anyhow::Result<Vec<PluginInfo>> {
    load_plugins(path.as_ref(), false)
}

/// Like [`load_endpoint_plugins`], but a name another factory already holds is skipped
/// instead of failing the library, so its other names still register.
pub(crate) fn load_discovered_plugins(path: &Path) -> anyhow::Result<Vec<PluginInfo>> {
    load_plugins(path, true)
}

fn load_plugins(path: &Path, skip_taken: bool) -> anyhow::Result<Vec<PluginInfo>> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("plugin library not found: {}", path.display()))?;

    let mut loaded = loaded_plugins()
        .lock()
        .map_err(|_| anyhow!("plugin registry lock poisoned"))?;
    if let Some(existing) = loaded.get(&resolved) {
        return Ok(existing.iter().map(|p| p.info.clone()).collect());
    }

    // Returning an error drops `plugins`, which frees their factories.
    let mut plugins: Vec<Arc<LoadedPlugin>> =
        open_plugins(&resolved)?.into_iter().map(Arc::new).collect();
    let mut endpoints = HashSet::new();
    let mut middlewares = HashSet::new();
    let mut skipped = HashSet::new();
    for (index, plugin) in plugins.iter().enumerate() {
        let info = &plugin.info;
        let endpoint = info.supports_consumer || info.supports_publisher;
        if !(endpoint || info.supports_middleware) {
            return Err(anyhow!(
                "plugin `{}` in {} declares no capabilities: it provides neither an endpoint \
                 nor a middleware",
                info.name,
                resolved.display()
            ));
        }
        // A name already taken by a different factory is a configuration mistake:
        // silently replacing it would reroute traffic to the wrong place.
        let taken = endpoint
            && (get_endpoint_factory(&info.name).is_some() || !endpoints.insert(&info.name));
        let taken_middleware = info.supports_middleware
            && (get_middleware_factory(&info.name).is_some() || !middlewares.insert(&info.name));
        if (taken || taken_middleware) && skip_taken {
            tracing::debug!(name = %info.name, path = %resolved.display(), "skipping plugin name already registered");
            skipped.insert(index);
        } else if taken || taken_middleware {
            return Err(anyhow!(
                "`{}` from {} is already registered by another factory; \
                 unload it or rename the plugin's endpoint",
                info.name,
                resolved.display()
            ));
        }
    }

    if !skipped.is_empty() {
        // Unloading is not safe once the library ran, even if none of its names register.
        if let Some(library) = plugins.first().and_then(|p| p._library.clone()) {
            std::mem::forget(library);
        }
        plugins = plugins
            .into_iter()
            .enumerate()
            .filter_map(|(index, plugin)| (!skipped.contains(&index)).then_some(plugin))
            .collect();
    }
    for (index, plugin) in plugins.iter().enumerate() {
        if let Err(error) = register_plugin(plugin) {
            for registered in &plugins[..index] {
                unregister_plugin(&registered.info);
            }
            return Err(error);
        }
    }
    let infos: Vec<PluginInfo> = plugins.iter().map(|p| p.info.clone()).collect();
    loaded.insert(resolved, plugins);
    for info in &infos {
        tracing::info!(
            name = %info.name,
            version = %info.version,
            endpoint = info.supports_consumer || info.supports_publisher,
            middleware = info.supports_middleware,
            path = %info.path.display(),
            "loaded plugin",
        );
    }
    Ok(infos)
}

/// Registers a plugin table that lives in this process instead of in a loaded
/// library, for a host that links its endpoint or middleware in.
///
/// Fails like [`load_endpoint_plugin`]: on an incompatible ABI, a table without
/// capabilities, or a name another factory already occupies. Registering the
/// same table twice fails too, before its factory is created a second time.
/// [`disable_plugin_loading`](super::disable_plugin_loading) does not apply: no library is opened.
///
/// # Safety
/// `table` must point to a valid [`MqbPluginVTable`] that, with all the code it
/// points at, stays valid for the life of the process.
pub unsafe fn register_plugin_table(table: *const MqbPluginVTable) -> anyhow::Result<PluginInfo> {
    const IN_PROCESS: &str = "<in-process>";
    let mut loaded = loaded_plugins()
        .lock()
        .map_err(|_| anyhow!("plugin registry lock poisoned"))?;
    if !table.is_null() && loaded.values().flatten().any(|p| p.table == table) {
        return Err(anyhow!("this plugin table is already registered"));
    }
    let plugin = Arc::new(open_plugin(None, table, Path::new(IN_PROCESS))?);
    let info = plugin.info.clone();
    let endpoint = info.supports_consumer || info.supports_publisher;
    if !(endpoint || info.supports_middleware) {
        return Err(anyhow!(
            "plugin `{}` declares no capabilities: it provides neither an endpoint nor a middleware",
            info.name
        ));
    }
    if (endpoint && get_endpoint_factory(&info.name).is_some())
        || (info.supports_middleware && get_middleware_factory(&info.name).is_some())
    {
        return Err(anyhow!(
            "`{}` is already registered by another factory",
            info.name
        ));
    }
    register_plugin(&plugin)?;
    loaded
        .entry(Path::new(IN_PROCESS).join(&info.name))
        .or_default()
        .push(plugin);
    Ok(info)
}

/// More than any real library exports; stops a list that never ends in null.
const MAX_PLUGINS_PER_LIBRARY: usize = 256;

/// The caller holds the `loaded_plugins()` lock, which is what makes the check below
/// atomic with the `dlopen` against [`disable_plugin_loading`](super::disable_plugin_loading).
fn open_plugins(path: &Path) -> anyhow::Result<Vec<LoadedPlugin>> {
    if plugin_loading_disabled() {
        return Err(anyhow!(
            "plugin loading is disabled in this process; not loading {}",
            path.display()
        ));
    }
    // Before dlopen, whose initialisers can crash too.
    crash::install();
    // Safety: dlopen runs the library's initialisers — inherently trusting the
    // file, as documented above.
    let library = unsafe { libloading::Library::new(path) }
        .with_context(|| format!("failed to load plugin library {}", path.display()))?;
    let library = Arc::new(library);
    // Never unmapped: its code may already be referenced (crash handlers, threads) if loading fails.
    std::mem::forget(Arc::clone(&library));

    let first = unsafe {
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
        crash::record_library(path, *entry as *const std::ffi::c_void);
        entry()
    };
    let mut tables = vec![first];
    // Safety: the symbol's type is fixed by the ABI.
    if let Ok(list) = unsafe { library.get::<MqbPluginListEntry>(MQB_PLUGIN_LIST_SYMBOL) } {
        for index in 1..=MAX_PLUGINS_PER_LIBRARY {
            let table = unsafe { list(index) };
            if table.is_null() {
                break;
            }
            if index == MAX_PLUGINS_PER_LIBRARY {
                return Err(anyhow!(
                    "plugin {} exports more than {MAX_PLUGINS_PER_LIBRARY} plugins",
                    path.display()
                ));
            }
            tables.push(table);
        }
    }
    tables
        .into_iter()
        .map(|table| open_plugin(Some(&library), table, path))
        .collect()
}

fn open_plugin(
    library: Option<&Arc<libloading::Library>>,
    table: *const MqbPluginVTable,
    path: &Path,
) -> anyhow::Result<LoadedPlugin> {
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

    // Before the factory exists, so even its creation logs through the host.
    if let Some(init) = table_ref.host_init_hook() {
        unsafe { init(&host::HOST_VTABLE) };
    }
    let mut factory = MqbFactoryHandle::NULL;
    let mut error = MqbBuffer::EMPTY;
    let status = unsafe { (table_ref.factory_create)(&mut factory, &mut error) };

    let mut plugin = LoadedPlugin {
        _library: library.cloned(),
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
            config_schema: None,
            middleware_config_schema: None,
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

    // Needs the factory, so it cannot happen before the checks above.
    plugin.info.config_schema = read_config_schema(&plugin, MQB_SCHEMA_ENDPOINT)
        .with_context(|| format!("plugin {} describes its endpoint badly", path.display()))?;
    plugin.info.middleware_config_schema = read_config_schema(&plugin, MQB_SCHEMA_MIDDLEWARE)
        .with_context(|| format!("plugin {} describes its middleware badly", path.display()))?;
    Ok(plugin)
}

/// Asks a plugin for one configuration schema, and refuses one the host cannot use.
///
/// A schema is read once, here, rather than wherever a form or a URI needs it:
/// a plugin that describes itself wrongly should fail to load, not fail later at
/// whichever call site happened to look first.
fn read_config_schema(plugin: &LoadedPlugin, kind: u32) -> anyhow::Result<Option<String>> {
    let Some(hook) = plugin.table().config_schema_hook() else {
        return Ok(None);
    };
    let mut schema = MqbBuffer::EMPTY;
    let mut error = MqbBuffer::EMPTY;
    let status = unsafe { hook(plugin.factory, kind, &mut schema, &mut error) };
    if status != MQB_OK {
        plugin.take_buffer(schema);
        return Err(anyhow!("{}", plugin.take_error(error)));
    }
    if schema.is_empty() {
        return Ok(None);
    }
    let text = plugin.take_buffer_utf8(schema)?;
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("the schema it returned is not JSON: {text}"))?;
    crate::support::config_schema::validate(&parsed)?;
    Ok(Some(text))
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
