use mq_bridge_app::{
    config::{ConsumerConfig, PublisherClient},
    ui_app::{
        ConsumerStatusResponse, FeatureAvailabilityResponse, PeerStatusResponse, PublishRequest,
        RuntimeStatusResponse, StorageSecurityInfoResponse,
    },
};
use serde_json::json;

fn main() {
    let schemas = json!({
        "AppConfig": mq_bridge_app::config::app_config_schema(),
        "ConsumerConfig": schemars::schema_for!(ConsumerConfig),
        "PublisherClient": schemars::schema_for!(PublisherClient),
        "PublishRequest": schemars::schema_for!(PublishRequest),
        "RuntimeStatusResponse": schemars::schema_for!(RuntimeStatusResponse),
        "PeerStatusResponse": schemars::schema_for!(PeerStatusResponse),
        "ConsumerStatusResponse": schemars::schema_for!(ConsumerStatusResponse),
        "StorageSecurityInfoResponse": schemars::schema_for!(StorageSecurityInfoResponse),
        "FeatureAvailabilityResponse": schemars::schema_for!(FeatureAvailabilityResponse),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&schemas).expect("ui type schemas serialize")
    );
}
