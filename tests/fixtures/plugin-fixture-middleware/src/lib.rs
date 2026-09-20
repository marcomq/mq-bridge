//! A plugin with a middleware and no endpoint.
//!
//! Discovery resolves an endpoint name, so this library can never answer one —
//! it exists to prove the host says so, instead of registering half a plugin and
//! failing later with an unrelated error. The middleware is the plugin
//! fixture's, linked as an rlib; only the entry point is new.

mq_bridge::export_middleware_plugin! {
    name: "middleware-only-fixture",
    middleware: mq_bridge_plugin_fixture::FixtureMiddlewareFactory,
}
