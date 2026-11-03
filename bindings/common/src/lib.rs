//! Shared helpers for the mq-bridge language bindings (Python and Node).
//!
//! Both bindings used to carry their own copies of the config-document loading
//! and route/publisher resolution logic. Those copies drifted (array-publisher
//! support, default-name format, and where the `config:` root gets unwrapped all
//! diverged), so the logic lives here once. Each binding keeps only its
//! `#[pyo3]` / `#[napi]` wrappers and maps these `anyhow::Result`s to its native
//! error type at the boundary.

pub mod logging;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Context;
use mq_bridge::models::{Endpoint, PublisherConfig};
use mq_bridge::Route;
use serde::Deserialize;
use tokio::runtime::{Builder, Runtime};

/// A parsed `routes:` + `publishers:` configuration document.
pub struct ConfigDocument {
    pub routes: HashMap<String, Route>,
    pub publishers: PublisherConfig,
}

/// One entry of a `publishers:` array (`- name: ... endpoint: ...`).
#[derive(Debug, Deserialize)]
struct NamedPublisher {
    name: String,
    endpoint: Endpoint,
}

/// Build the multi-threaded Tokio runtime the bindings drive their transports
/// on.
pub fn build_runtime() -> anyhow::Result<Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime")
}

/// Read and parse a YAML/JSON config file, unwrapping a `config:` export root
/// if present.
pub fn load_config_value(path: &Path) -> anyhow::Result<serde_yaml_ng::Value> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    let value = serde_yaml_ng::from_str(&raw).context("failed to parse YAML config")?;
    unwrap_config_root(value)
}

/// Load a named route from a config file.
pub fn load_named_route(path: &Path, name: Option<&str>) -> anyhow::Result<Route> {
    named_route_from_value(load_config_value(path)?, name)
}

/// Resolve a route from a config value. When `name` is `Some`, look it up in a
/// `routes:` document (falling back to a single-route body). When `name` is
/// `None`, the value must be a single bare route body. A `config:` export root
/// is unwrapped first, so file, string, and mapping inputs behave identically.
pub fn named_route_from_value(
    value: serde_yaml_ng::Value,
    name: Option<&str>,
) -> anyhow::Result<Route> {
    let value = unwrap_config_root(value)?;
    if let Some(name) = name {
        if let Ok(document) = load_document_from_value(value.clone()) {
            if let Some(route) = document.routes.get(name).cloned() {
                return Ok(route);
            }
        }
    }

    serde_yaml_ng::from_value(value).with_context(|| match name {
        Some(name) => format!(
            "No route named '{name}' found, and the config could not be parsed as a single route"
        ),
        None => "config could not be parsed as a single route body".to_string(),
    })
}

/// Load a named publisher endpoint from a config file.
pub fn load_named_publisher(path: &Path, name: Option<&str>) -> anyhow::Result<Endpoint> {
    named_publisher_from_value(load_config_value(path)?, name)
}

/// Resolve a publisher endpoint from a config value. Mirrors
/// [`named_route_from_value`]: name lookup in a `publishers:` document with a
/// single-endpoint-body fallback, and a `config:` root unwrapped first.
pub fn named_publisher_from_value(
    value: serde_yaml_ng::Value,
    name: Option<&str>,
) -> anyhow::Result<Endpoint> {
    let value = unwrap_config_root(value)?;
    if let Some(name) = name {
        if let Ok(document) = load_document_from_value(value.clone()) {
            if let Some(endpoint) = document.publishers.get(name).cloned() {
                return Ok(endpoint);
            }
        }
    }

    serde_yaml_ng::from_value(value).with_context(|| match name {
        Some(name) => format!(
            "No publisher named '{name}' found, and the config could not be parsed as a single publisher endpoint"
        ),
        None => "config could not be parsed as a single endpoint body".to_string(),
    })
}

/// Load a named consumer endpoint from a config file.
pub fn load_named_consumer(path: &Path, name: Option<&str>) -> anyhow::Result<Endpoint> {
    named_consumer_from_value(load_config_value(path)?, name)
}

/// Resolve a consumer (input) endpoint from a config value. Mirrors
/// [`named_publisher_from_value`]: when `name` is `Some`, look it up in a
/// `consumers:` section; otherwise (or on a miss) parse the value as a single
/// bare endpoint body. A `config:` export root is unwrapped first.
pub fn named_consumer_from_value(
    value: serde_yaml_ng::Value,
    name: Option<&str>,
) -> anyhow::Result<Endpoint> {
    let value = unwrap_config_root(value)?;
    if let Some(name) = name {
        if let Some(map) = value.as_mapping() {
            if let Some(section) = map.get(serde_yaml_ng::Value::String("consumers".to_string())) {
                if let Ok(consumers) =
                    serde_yaml_ng::from_value::<HashMap<String, Endpoint>>(section.clone())
                {
                    if let Some(endpoint) = consumers.get(name) {
                        return Ok(endpoint.clone());
                    }
                }
            }
        }
    }

    serde_yaml_ng::from_value(value).with_context(|| match name {
        Some(name) => format!(
            "No consumer named '{name}' found, and the config could not be parsed as a single consumer endpoint"
        ),
        None => "config could not be parsed as a single endpoint body".to_string(),
    })
}

/// Parse a value that may be a `{routes, publishers}` document, a bare
/// `{name: route}` map, or (without those sections) a plain route map.
pub fn load_document_from_value(value: serde_yaml_ng::Value) -> anyhow::Result<ConfigDocument> {
    let value = unwrap_config_root(value)?;
    let section_key = |name: &str| serde_yaml_ng::Value::String(name.to_string());
    let routes_key = section_key("routes");
    let publishers_key = section_key("publishers");

    if let Some(map) = value.as_mapping() {
        if map.contains_key(&routes_key) || map.contains_key(&publishers_key) {
            let routes = map
                .get(&routes_key)
                .map_or_else(
                    || Ok(HashMap::new()),
                    |section| serde_yaml_ng::from_value(section.clone()),
                )
                .context("failed to parse 'routes' section")?;
            let publishers = map.get(&publishers_key).map_or_else(
                || Ok(PublisherConfig::new()),
                |section| parse_publishers_section(section.clone()),
            )?;
            return Ok(ConfigDocument { routes, publishers });
        }
    }

    let routes = serde_yaml_ng::from_value(value).context("failed to parse YAML as a route map")?;
    Ok(ConfigDocument {
        routes,
        publishers: PublisherConfig::new(),
    })
}

/// Parse a `publishers:` section, accepting either a `{name: endpoint}` map or a
/// `[{name, endpoint}]` array.
pub fn parse_publishers_section(value: serde_yaml_ng::Value) -> anyhow::Result<PublisherConfig> {
    match value {
        serde_yaml_ng::Value::Mapping(_) => {
            serde_yaml_ng::from_value(value).context("failed to parse 'publishers' section")
        }
        serde_yaml_ng::Value::Sequence(_) => {
            let entries: Vec<NamedPublisher> = serde_yaml_ng::from_value(value)
                .context("failed to parse 'publishers' array section")?;
            let mut publishers = PublisherConfig::new();
            for entry in entries {
                if publishers.contains_key(&entry.name) {
                    return Err(anyhow::anyhow!(
                        "duplicate publisher name '{}' in 'publishers' array section",
                        entry.name
                    ));
                }
                publishers.insert(entry.name, entry.endpoint);
            }
            Ok(publishers)
        }
        other => Err(anyhow::anyhow!(
            "failed to parse 'publishers' section: expected a map or array, got {other:?}"
        )),
    }
}

/// Strip a `config:` export root if present (e.g. an `mq-bridge-app` export),
/// returning the inner document. Idempotent. A `config` key next to other keys is
/// refused: it is either a broken export or a route map with a route named `config`.
fn unwrap_config_root(value: serde_yaml_ng::Value) -> anyhow::Result<serde_yaml_ng::Value> {
    if let Some(map) = value.as_mapping() {
        let config_key = serde_yaml_ng::Value::String("config".to_string());
        if let Some(config) = map.get(&config_key) {
            if map.len() > 1 {
                let siblings: Vec<&str> = map
                    .keys()
                    .filter(|key| **key != config_key)
                    .map(|key| key.as_str().unwrap_or("?"))
                    .collect();
                return Err(anyhow::anyhow!(
                    "ambiguous config: the top-level key 'config' stands next to '{}'. An export \
                     root has only 'config'; a route cannot be named 'config'.",
                    siblings.join("', '")
                ));
            }
            return Ok(config.clone());
        }
    }
    Ok(value)
}

/// Map an optional name from a binding boundary to `None` when missing or empty,
/// so callers can omit it (or pass `""`) to mean "no name".
pub fn normalize_name(name: Option<&str>) -> Option<&str> {
    name.filter(|n| !n.is_empty())
}

/// Generated identity for a route built without an explicit name.
pub fn default_route_name() -> String {
    format!("route-{}", fast_uuid_v7::gen_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    // An mq-bridge-app export: a `config:` root wrapping a document whose
    // `publishers:` section is an array (the two shapes that had drifted
    // between the bindings).
    const EXPORT: &str = r#"
config:
  publishers:
    - name: incoming
      endpoint:
        memory:
          topic: orders
          capacity: 16
  routes:
    orders_route:
      input:
        memory:
          topic: orders
          capacity: 16
      output:
        response: {}
"#;

    fn yaml(s: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn a_config_key_next_to_other_keys_is_refused_as_ambiguous() {
        let mixed = yaml("config:\n  routes: {}\norders:\n  input: {}\n");
        let error = named_route_from_value(mixed, Some("orders")).unwrap_err();
        assert!(error.to_string().contains("ambiguous config"), "{error}");
        assert!(load_document_from_value(yaml(EXPORT)).is_ok());
    }

    #[test]
    fn publishers_section_accepts_array_and_map() {
        let array = yaml("- name: out\n  endpoint:\n    memory:\n      topic: t\n");
        assert!(parse_publishers_section(array).unwrap().contains_key("out"));

        let map = yaml("out:\n  memory:\n    topic: t\n");
        assert!(parse_publishers_section(map).unwrap().contains_key("out"));
    }

    #[test]
    fn named_lookups_unwrap_config_export_root() {
        // The `config:` root is unwrapped, the array publishers parsed, and the
        // named entries resolved.
        let route = named_route_from_value(yaml(EXPORT), Some("orders_route"));
        assert!(route.is_ok(), "route: {:?}", route.err());

        let publisher = named_publisher_from_value(yaml(EXPORT), Some("incoming"));
        assert!(publisher.is_ok(), "publisher: {:?}", publisher.err());
    }

    #[test]
    fn document_exposes_routes_and_publishers() {
        let doc = load_document_from_value(unwrap_config_root(yaml(EXPORT)).unwrap()).unwrap();
        assert!(doc.routes.contains_key("orders_route"));
        assert!(doc.publishers.contains_key("incoming"));
    }

    #[test]
    fn named_consumer_resolves_section_and_bare_body() {
        // Named lookup in a `consumers:` section.
        let doc = yaml("consumers:\n  orders:\n    memory:\n      topic: t\n      capacity: 16\n");
        assert!(named_consumer_from_value(doc, Some("orders")).is_ok());

        // Bare single-endpoint body (no name).
        let bare = yaml("memory:\n  topic: t\n  capacity: 16\n");
        assert!(named_consumer_from_value(bare, None).is_ok());

        // A `config:` export root is unwrapped before the `consumers:` lookup.
        let export = yaml(
            "config:\n  consumers:\n    orders:\n      memory:\n        topic: t\n        capacity: 16\n",
        );
        assert!(named_consumer_from_value(export, Some("orders")).is_ok());
    }

    #[test]
    fn normalize_name_treats_empty_as_none() {
        assert_eq!(normalize_name(Some("x")), Some("x"));
        assert_eq!(normalize_name(Some("")), None);
        assert_eq!(normalize_name(None), None);
    }
}
