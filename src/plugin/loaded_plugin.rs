//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! What the host keeps of a plugin once it is open: its table, its factory and what it reported.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;

use crate::support::plugin_abi::{MqbBuffer, MqbFactoryHandle, MqbPluginVTable};

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
    /// JSON Schema of the endpoint's `config` object, as the plugin described
    /// it. Kept as text because that is what crossed the ABI; parse it with
    /// [`PluginInfo::endpoint_schema`].
    pub config_schema: Option<String>,
    /// JSON Schema of the middleware's `config` object. See
    /// [`PluginInfo::config_schema`].
    pub middleware_config_schema: Option<String>,
}

impl PluginInfo {
    /// The endpoint's configuration schema, parsed.
    ///
    /// Validated at load time, so this only returns `None` when the plugin
    /// described nothing.
    pub fn endpoint_schema(&self) -> Option<serde_json::Value> {
        self.config_schema
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
    }

    /// The middleware's configuration schema, parsed.
    pub fn middleware_schema(&self) -> Option<serde_json::Value> {
        self.middleware_config_schema
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
    }
}

/// A loaded plugin library plus the factory created from it.
///
/// Held in `loaded_plugins()` for the life of the process so neither the
/// library nor the factory can be dropped while an endpoint still points at it.
pub(crate) struct LoadedPlugin {
    /// Kept only to keep the library mapped; every code pointer below lives in it.
    /// `None` for a table registered from this process.
    pub(super) _library: Option<Arc<libloading::Library>>,
    pub(super) table: *const MqbPluginVTable,
    pub(super) factory: MqbFactoryHandle,
    pub(super) info: PluginInfo,
}

/// Safety: the ABI requires every handle to be usable from any thread, and the
/// table is immutable `'static` data in the library or the host program.
unsafe impl Send for LoadedPlugin {}
unsafe impl Sync for LoadedPlugin {}

impl LoadedPlugin {
    /// A table handed over with an endpoint the other side lends, not loaded from a library.
    ///
    /// Safety: `table` must stay valid for as long as the result lives.
    pub(crate) unsafe fn lent(table: *const MqbPluginVTable) -> Self {
        Self {
            _library: None,
            table,
            factory: MqbFactoryHandle::NULL,
            info: PluginInfo {
                name: "lent".to_string(),
                version: String::new(),
                abi_major: 0,
                abi_minor: 0,
                supports_consumer: true,
                supports_publisher: true,
                supports_middleware: false,
                path: PathBuf::new(),
                config_schema: None,
                middleware_config_schema: None,
            },
        }
    }

    pub(crate) fn table(&self) -> &MqbPluginVTable {
        // Safety: validated at load time; kept alive by `_library` or the registrant.
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

    /// Consumes a buffer whose bytes have to be text, rather than repairing
    /// them: a schema silently mangled into replacement characters still parses.
    pub(crate) fn take_buffer_utf8(&self, buffer: MqbBuffer) -> anyhow::Result<String> {
        if buffer.is_empty() {
            return Ok(String::new());
        }
        // Safety: the plugin owns the buffer until it is handed back below.
        let text = unsafe { std::str::from_utf8(buffer.as_bytes()).map(str::to_owned) };
        unsafe { (self.table().buffer_free)(buffer) };
        text.map_err(|e| anyhow!("it returned a buffer that is not UTF-8: {e}"))
    }

    /// Consumes a buffer the plugin filled on success and returns its text.
    pub(crate) fn take_buffer(&self, buffer: MqbBuffer) -> String {
        if buffer.is_empty() {
            return String::new();
        }
        // Safety: the plugin owns the buffer until it is handed back below.
        let text = unsafe { String::from_utf8_lossy(buffer.as_bytes()).into_owned() };
        unsafe { (self.table().buffer_free)(buffer) };
        text
    }
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
