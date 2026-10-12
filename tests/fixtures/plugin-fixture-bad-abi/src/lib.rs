//! A plugin from the future: a valid function table whose ABI major version the
//! host cannot support. Loading it must fail with an actionable message rather
//! than calling into a table it does not understand.

use mq_bridge::plugin::sdk::{
    build_vtable, ExportedVTable, NoMiddleware, CAPABILITIES_INPUT_AND_OUTPUT,
};
use mq_bridge::support::plugin_abi::{MqbPluginVTable, MQB_PLUGIN_ABI_MAJOR};
use mq_bridge::traits::CustomEndpointFactory;

#[derive(Debug, Default)]
struct UnreachableFactory;

impl CustomEndpointFactory for UnreachableFactory {}

static VTABLE: ExportedVTable = {
    let mut table = build_vtable::<UnreachableFactory, NoMiddleware>(
        "bad-abi",
        env!("CARGO_PKG_VERSION"),
        CAPABILITIES_INPUT_AND_OUTPUT,
    );
    table.0.abi_major = MQB_PLUGIN_ABI_MAJOR + 1;
    table
};

#[unsafe(no_mangle)]
pub extern "C" fn mq_bridge_plugin_v1() -> *const MqbPluginVTable {
    VTABLE.as_ptr()
}
