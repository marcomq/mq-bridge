use std::collections::HashMap;

use mq_bridge::models::{Endpoint, EndpointType};
use mq_bridge_app::{
    config::{AppConfig, ConsumerConfig, ConsumerMessageCaptureConfig, ConsumerOutputConfig},
    ui_app::UiApp,
};

fn metrics_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .build_recorder()
        .handle()
}

fn plugin_consumer() -> ConsumerConfig {
    ConsumerConfig {
        id: "plugin-consumer".to_string(),
        name: "Plugin consumer".to_string(),
        enabled: true,
        endpoint: Endpoint::new(EndpointType::Custom {
            name: "startup-fixture".to_string(),
            config: serde_json::Value::Null,
        }),
        comment: String::new(),
        response: None,
        output: ConsumerOutputConfig::None,
        message_capture: ConsumerMessageCaptureConfig::default(),
        options: Default::default(),
    }
}

#[tokio::test]
async fn native_plugins_are_loaded_only_from_trusted_startup_config() {
    let fixture_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/plugin-startup");
    let library = mq_bridge::plugin::test_support::build_plugin_cdylib(
        &fixture_dir,
        "mq-bridge-app-plugin-startup-fixture",
    )
    .unwrap();

    let invalid = AppConfig {
        plugins: vec!["/nonexistent/libmqb-startup-fixture.so".to_string()],
        ..Default::default()
    };
    assert!(UiApp::new(invalid, metrics_handle(), "/tmp/unused.yml".to_string()).is_err());

    let untrusted = UiApp::new(
        AppConfig::default(),
        metrics_handle(),
        "/tmp/unused.yml".to_string(),
    )
    .unwrap();
    let addition = AppConfig {
        plugins: vec![library.to_string_lossy().into_owned()],
        ..Default::default()
    };
    let error = untrusted.update_config(addition).await.unwrap_err();
    assert!(error.to_string().contains("startup-only"));
    assert!(untrusted.get_config().await.plugins.is_empty());
    assert!(
        mq_bridge::plugin::loaded_endpoint_plugins()
            .iter()
            .all(|plugin| plugin.name != "startup-fixture")
    );

    let plugin_file = library.file_name().unwrap().to_string_lossy().into_owned();
    let mut trusted_config = AppConfig {
        plugins: vec![format!("${{PLUGIN_DIR}}/{plugin_file}")],
        env_vars: HashMap::from([(
            "PLUGIN_DIR".to_string(),
            library.parent().unwrap().to_string_lossy().into_owned(),
        )]),
        consumers: vec![plugin_consumer()],
        ..Default::default()
    };
    let trusted = UiApp::new(
        trusted_config.clone(),
        metrics_handle(),
        "/nonexistent/mqb-plugin-startup/config.yml".to_string(),
    )
    .unwrap();

    assert!(trusted.start_consumer("plugin-consumer").await.unwrap());
    assert!(trusted.stop_consumer("plugin-consumer").await);

    let registrations_before = mq_bridge::plugin::loaded_endpoint_plugins();
    trusted_config.plugins = vec![
        library.to_string_lossy().into_owned(),
        library.to_string_lossy().into_owned(),
    ];
    let error = trusted.update_config(trusted_config).await.unwrap_err();
    assert!(!error.to_string().contains("startup-only"));
    assert_eq!(
        mq_bridge::plugin::loaded_endpoint_plugins().len(),
        registrations_before.len()
    );
}
