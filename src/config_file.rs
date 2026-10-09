//! Short entrypoints that load routes and publishers from a config document,
//! mirroring `Route.from_file` / `Publisher.from_file` of the Python and Node
//! bindings. YAML needs the `yaml` feature; JSON always works.
//!
//! A document is `{routes: {...}, publishers: {...}}`, a bare `{name: route}`
//! map, or a single route/endpoint body. An `mq-bridge-app` export's `config:`
//! root is unwrapped first.

use crate::models::{Config, Endpoint, PublisherConfig, Route};
use crate::{CanonicalMessage, Publisher, Sent};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

struct ConfigDocument {
    routes: Config,
    publishers: PublisherConfig,
}

#[derive(Deserialize)]
struct NamedPublisher {
    name: String,
    endpoint: Endpoint,
}

fn parse_str(text: &str) -> anyhow::Result<Value> {
    #[cfg(feature = "yaml")]
    let value = serde_yaml_ng::from_str(text).context("failed to parse YAML config")?;
    #[cfg(not(feature = "yaml"))]
    let value = serde_json::from_str(text)
        .context("failed to parse JSON config (enable the `yaml` feature for YAML)")?;
    Ok(value)
}

fn read_file(path: &Path) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    parse_str(&text).with_context(|| format!("invalid config in {}", path.display()))
}

/// Strips an export's `config:` root. A `config` key next to other keys is refused: it is
/// either a broken export or a route map with a route named `config`.
fn unwrap_config_root(value: Value) -> anyhow::Result<Value> {
    match value {
        Value::Object(mut map) if map.contains_key("config") => {
            if map.len() > 1 {
                let siblings: Vec<&str> = map
                    .keys()
                    .filter(|key| *key != "config")
                    .map(String::as_str)
                    .collect();
                anyhow::bail!(
                    "ambiguous config: the top-level key 'config' stands next to '{}'. An export \
                     root has only 'config'; a route cannot be named 'config'.",
                    siblings.join("', '")
                );
            }
            Ok(map.remove("config").unwrap_or_default())
        }
        other => Ok(other),
    }
}

fn document_from_value(value: Value) -> anyhow::Result<ConfigDocument> {
    document_from_unwrapped(unwrap_config_root(value)?)
}

fn document_from_unwrapped(value: Value) -> anyhow::Result<ConfigDocument> {
    if let Value::Object(map) = &value {
        if map.contains_key("routes") || map.contains_key("publishers") {
            let routes = match map.get("routes") {
                Some(section) => serde_json::from_value(section.clone())
                    .context("failed to parse 'routes' section")?,
                None => Config::new(),
            };
            let publishers = match map.get("publishers") {
                Some(section) => parse_publishers_section(section.clone())?,
                None => PublisherConfig::new(),
            };
            return Ok(ConfigDocument { routes, publishers });
        }
    }
    let routes = serde_json::from_value(value).context("failed to parse config as a route map")?;
    Ok(ConfigDocument {
        routes,
        publishers: PublisherConfig::new(),
    })
}

fn parse_publishers_section(value: Value) -> anyhow::Result<PublisherConfig> {
    match value {
        Value::Object(_) => {
            serde_json::from_value(value).context("failed to parse 'publishers' section")
        }
        Value::Array(_) => {
            let entries: Vec<NamedPublisher> = serde_json::from_value(value)
                .context("failed to parse 'publishers' array section")?;
            let mut publishers = PublisherConfig::new();
            for entry in entries {
                if publishers.contains_key(&entry.name) {
                    anyhow::bail!(
                        "duplicate publisher name '{}' in 'publishers' array section",
                        entry.name
                    );
                }
                publishers.insert(entry.name, entry.endpoint);
            }
            Ok(publishers)
        }
        other => anyhow::bail!(
            "failed to parse 'publishers' section: expected a map or array, got {other}"
        ),
    }
}

fn named_route(value: Value, name: &str) -> anyhow::Result<Route> {
    let value = unwrap_config_root(value)?;
    let note = match document_from_unwrapped(value.clone()) {
        Ok(mut document) => match document.routes.remove(name) {
            Some(route) => return Ok(route),
            None => available_note(document.routes.keys()),
        },
        Err(e) => document_error_note(&e),
    };
    serde_json::from_value(value).with_context(|| {
        format!(
            "No route named '{name}' found, and the config could not be parsed as a single route{note}"
        )
    })
}

/// The names a config document does define, for the missing-name error.
fn available_note<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let mut names: Vec<&str> = names.map(String::as_str).collect();
    if names.is_empty() {
        return String::new();
    }
    names.sort_unstable();
    format!(" (available: {})", names.join(", "))
}

/// Why the config did not parse as a document, for the single-item fallback error.
fn document_error_note(error: &anyhow::Error) -> String {
    format!(" (as a config document: {error:#})")
}

fn named_publisher(value: Value, name: &str) -> anyhow::Result<Endpoint> {
    let value = unwrap_config_root(value)?;
    let note = match document_from_unwrapped(value.clone()) {
        Ok(mut document) => match document.publishers.remove(name) {
            Some(endpoint) => return Ok(endpoint),
            None => available_note(document.publishers.keys()),
        },
        Err(e) => document_error_note(&e),
    };
    serde_json::from_value(value).with_context(|| {
        format!(
            "No publisher named '{name}' found, and the config could not be parsed as a single publisher endpoint{note}"
        )
    })
}

impl Route {
    /// Loads the route `name` from a YAML (with the `yaml` feature) or JSON file.
    ///
    /// Falls back to parsing the whole file as one route body when no route
    /// of that name exists. The name is not stored: pass it again to
    /// [`deploy`](Route::deploy) or [`run`](Route::run).
    pub fn from_file(path: impl AsRef<Path>, name: &str) -> anyhow::Result<Self> {
        named_route(read_file(path.as_ref())?, name)
    }

    /// Like [`Route::from_file`], but parses the config from a string.
    pub fn from_str(text: &str, name: &str) -> anyhow::Result<Self> {
        named_route(parse_str(text)?, name)
    }

    /// Like [`Route::from_file`], but takes an already-parsed config document.
    pub fn from_config(config: Value, name: &str) -> anyhow::Result<Self> {
        named_route(config, name)
    }
}

impl Publisher {
    /// Connects the publisher `name` from a config file's `publishers:` section,
    /// or the whole file parsed as one endpoint body when no such name exists.
    pub async fn from_file(path: impl AsRef<Path>, name: &str) -> anyhow::Result<Self> {
        Self::new(named_publisher(read_file(path.as_ref())?, name)?).await
    }

    /// Like [`Publisher::from_file`], but parses the config from a string.
    pub async fn from_str(text: &str, name: &str) -> anyhow::Result<Self> {
        Self::new(named_publisher(parse_str(text)?, name)?).await
    }

    /// Serializes `value` to JSON and sends it as one message.
    pub async fn send_json<T: Serialize + ?Sized>(&self, value: &T) -> anyhow::Result<Sent> {
        self.send(CanonicalMessage::from_json(serde_json::to_value(value)?)?)
            .await
    }
}

/// Deploys every route and registers every publisher of a config file.
///
/// Routes run in the background under their config names, so manage them with
/// [`stop_route`](crate::stop_route) / [`route_status`](crate::route_status)
/// and fetch publishers with [`get_publisher`](crate::get_publisher). On an
/// error, routes deployed before it keep running.
///
/// ```no_run
/// # async fn example() -> anyhow::Result<()> {
/// mq_bridge::deploy_file("routes.yaml").await?;
/// tokio::signal::ctrl_c().await?;
/// # Ok(())
/// # }
/// ```
pub async fn deploy_file(path: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = path.as_ref();
    let document = document_from_value(read_file(path)?)
        .with_context(|| format!("invalid config in {}", path.display()))?;
    for (name, endpoint) in document.publishers {
        let publisher = Publisher::new(endpoint)
            .await
            .with_context(|| format!("failed to create publisher '{name}'"))?;
        publisher.register(&name);
    }
    for (name, route) in document.routes {
        route
            .deploy(&name)
            .await
            .with_context(|| format!("failed to deploy route '{name}'"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document() -> Value {
        json!({
            "routes": {
                "orders": {
                    "input": {"memory": {"topic": "cf-orders-in"}},
                    "output": {"memory": {"topic": "cf-orders-out"}}
                }
            },
            "publishers": [
                {"name": "audit", "endpoint": {"memory": {"topic": "cf-audit"}}}
            ]
        })
    }

    #[test]
    fn route_from_config_looks_up_by_name() {
        let route = Route::from_config(document(), "orders").unwrap();
        assert!(format!("{:?}", route.input).contains("cf-orders-in"));
    }

    #[test]
    fn route_from_config_unwraps_export_root_and_falls_back_to_single_body() {
        let single = json!({"config": {
            "input": {"memory": {"topic": "cf-single"}},
            "output": {"null": null}
        }});
        let route = Route::from_config(single, "anything").unwrap();
        assert!(format!("{:?}", route.input).contains("cf-single"));
    }

    #[test]
    fn route_from_config_reports_missing_name() {
        let err = Route::from_config(document(), "missing").unwrap_err();
        assert!(err.to_string().contains("No route named 'missing'"));
        assert!(err.to_string().contains("(available: orders)"), "{err}");
        let err = named_publisher(document(), "missing").unwrap_err();
        assert!(err.to_string().contains("(available: audit)"), "{err}");
    }

    #[test]
    fn a_config_key_next_to_other_keys_is_refused_as_ambiguous() {
        let route = json!({"input": {"memory": {"topic": "cf-amb"}}, "output": {"null": null}});
        let mixed = json!({"config": route, "orders": route});
        let err = Route::from_config(mixed.clone(), "orders").unwrap_err();
        assert!(err.to_string().contains("ambiguous config"), "{err}");
        assert!(err.to_string().contains("'orders'"), "{err}");
        assert!(named_publisher(mixed, "orders").is_err());
    }

    #[test]
    fn a_broken_document_is_named_in_the_fallback_error() {
        let broken = json!({"routes": {"orders": {"input": 7}}, "publishers": "nope"});
        let err = Route::from_config(broken.clone(), "orders").unwrap_err();
        assert!(err.to_string().contains("failed to parse 'routes' section"));
        let err = named_publisher(broken, "audit").unwrap_err();
        assert!(err.to_string().contains("failed to parse 'routes' section"));
        // A document that parses adds nothing to the missing-name error.
        let err = Route::from_config(document(), "missing").unwrap_err();
        assert!(!err.to_string().contains("as a config document"));
    }

    #[test]
    fn route_from_str_parses_json() {
        let text = serde_json::to_string(&document()).unwrap();
        assert!(Route::from_str(&text, "orders").is_ok());
    }

    #[cfg(feature = "yaml")]
    #[test]
    fn route_from_str_parses_yaml() {
        let text = "orders:\n  input:\n    memory: { topic: cf-yaml-in }\n  output:\n    memory: { topic: cf-yaml-out }\n";
        let route = Route::from_str(text, "orders").unwrap();
        assert!(format!("{:?}", route.output).contains("cf-yaml-out"));
    }

    #[test]
    fn publishers_array_rejects_duplicates() {
        let section = json!([
            {"name": "a", "endpoint": {"memory": {"topic": "x"}}},
            {"name": "a", "endpoint": {"memory": {"topic": "y"}}}
        ]);
        let err = parse_publishers_section(section).unwrap_err();
        assert!(err.to_string().contains("duplicate publisher name 'a'"));
    }

    #[tokio::test]
    async fn publisher_from_file_and_deploy_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routes.json");
        std::fs::write(&path, serde_json::to_string(&document()).unwrap()).unwrap();

        let publisher = Publisher::from_file(&path, "audit").await.unwrap();
        publisher.send_json(&json!({"n": 1})).await.unwrap();
        let audit =
            crate::get_or_create_channel(&crate::models::MemoryConfig::new("cf-audit", None));
        assert_eq!(audit.drain_messages().len(), 1);

        deploy_file(&path).await.unwrap();
        assert!(crate::get_route("orders").is_some());
        assert!(crate::get_publisher("audit").is_some());
        crate::stop_route("orders").await;
    }
}
