use crate::config::AppConfig;
use crate::status_registry::InstanceKind;
use crate::ui_app::UiApp;
use anyhow::Result;
use mq_bridge::models::{Endpoint, EndpointType, HttpConfig, Route};
use mq_bridge::{CanonicalMessage, HandlerError};
use std::net::ToSocketAddrs;

#[derive(Clone)]
struct WebUiHttpHandler {
    app: UiApp,
    bound_to_loopback: bool,
}

struct WebUiStatusGuard(UiApp);

impl Drop for WebUiStatusGuard {
    fn drop(&mut self) {
        self.0.remove_status_registry_lease();
    }
}

impl WebUiHttpHandler {
    async fn handle(&self, mut msg: CanonicalMessage) -> Result<mq_bridge::Handled, HandlerError> {
        // Written by the server on every request, so a client cannot set it with
        // a header of the same name (mq-bridge copies request headers straight
        // into metadata, and this insert lands after that).
        //
        // This is the listener's address, not the connection's: mq-bridge's
        // accept loop discards the peer address, so a per-connection check is
        // not available yet. A wildcard bind therefore denies everyone, and a
        // loopback bind trusts the listener — which a port-forward onto
        // loopback (`ssh -L`, `kubectl port-forward`) can still satisfy from
        // off-box. Tighten to the real peer address once mq-bridge exposes it.
        msg.metadata.insert(
            "mqb_peer_loopback".to_string(),
            self.bound_to_loopback.to_string(),
        );
        self.app.handle_ui_message(msg, true).await
    }
}

/// Start Web UI
pub async fn start_web_server(
    bind_addr: String,
    initial_config: AppConfig,
    startup_plugins: Vec<String>,
    metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
    config_file_path: String,
) -> Result<(), anyhow::Error> {
    let bind_addr = bind_addr.to_string();
    let app = UiApp::new_with_startup_plugins(
        initial_config,
        metrics_handle,
        config_file_path,
        &startup_plugins,
    )?
    .with_instance_kind(InstanceKind::WebUi);
    let _status_guard = WebUiStatusGuard(app.clone());
    app.spawn_status_registry_publisher();
    let bound_to_loopback = bind_addr
        .to_socket_addrs()
        .map(|addresses| {
            let addresses: Vec<_> = addresses.collect();
            !addresses.is_empty() && addresses.iter().all(|address| address.ip().is_loopback())
        })
        .unwrap_or(false);

    let input = Endpoint {
        endpoint_type: EndpointType::Http(HttpConfig {
            url: bind_addr,
            workers: Some(100),
            ..Default::default()
        }),
        ..Default::default()
    };

    let output = Endpoint {
        endpoint_type: EndpointType::Response(Default::default()),
        ..Default::default()
    };

    let web_handler = WebUiHttpHandler {
        app: app.clone(),
        bound_to_loopback,
    };
    let mut web_route = Route::new(input, output).with_handler(move |msg| {
        let handler = web_handler.clone();
        async move { handler.handle(msg).await }
    });
    web_route.options.concurrency = 100;

    let handle = web_route.run("web_ui").await?;
    let consumer_app = app.clone();
    tokio::spawn(async move {
        consumer_app.start_configured_consumers().await;
    });
    let _ = handle.join().await;

    Ok(())
}
