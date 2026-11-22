//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Several routes declared, started and stopped from one place.

use super::Route;
use crate::models::{Endpoint, EndpointType};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

/// A route of a [`Topology`] that could not be checked or started.
#[derive(Debug, thiserror::Error)]
#[error("topology route '{route}' failed")]
pub struct TopologyError {
    pub route: String,
    #[source]
    pub source: anyhow::Error,
}

impl TopologyError {
    fn new(route: &str, source: anyhow::Error) -> Self {
        Self {
            route: route.to_string(),
            source,
        }
    }
}

/// Routes that belong together. They start in declaration order and stop in reverse.
///
/// ```no_run
/// use mq_bridge::models::Endpoint;
/// use mq_bridge::{Route, Topology};
///
/// # async fn example() -> anyhow::Result<()> {
/// let topic = |name| Endpoint::new_memory(name, 1024);
/// let deployed = Topology::new("orders")
///     .route("orders_out", Route::new(topic("orders"), topic("archive")))
///     .route(
///         "orders_in",
///         Route::new(topic("incoming"), topic("orders")).with_batch_size(1),
///     )
///     .deploy()
///     .await?;
/// println!("{}", deployed.diagram());
/// deployed.stop().await;
/// # Ok(())
/// # }
/// ```
pub struct Topology {
    name: String,
    routes: Vec<(String, Route)>,
    stop_order: Vec<String>,
}

impl Topology {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            routes: Vec::new(),
            stop_order: Vec::new(),
        }
    }

    /// Adds a route under its deployment name.
    pub fn route(mut self, name: impl Into<String>, route: Route) -> Self {
        self.routes.push((name.into(), route));
        self
    }

    /// Stops these routes first and in this order, then the others in reverse declaration order.
    pub fn stop_order(mut self, names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.stop_order = names.into_iter().map(Into::into).collect();
        self
    }

    /// Checks the declaration, then starts the routes in order. Nothing starts if the check
    /// fails. If a route fails to start, the ones already started are stopped in stop order.
    pub async fn deploy(self) -> Result<DeployedTopology, TopologyError> {
        self.check()?;
        let stop_order = self.resolved_stop_order();
        let diagram = self.diagram();

        let mut started = HashMap::new();
        for (name, route) in self.routes {
            match route.deploy_tracked(&name).await {
                Ok(deployment) => {
                    started.insert(name, deployment);
                }
                Err(source) => {
                    stop_in_order(&in_stop_order(&stop_order, &started)).await;
                    return Err(TopologyError::new(&name, source));
                }
            }
        }
        Ok(DeployedTopology {
            routes: in_stop_order(&stop_order, &started),
            diagram,
        })
    }

    fn check(&self) -> Result<(), TopologyError> {
        let mut names = HashSet::new();
        for (name, _) in &self.routes {
            if !names.insert(name.as_str()) {
                return Err(TopologyError::new(name, anyhow::anyhow!("declared twice")));
            }
        }
        match self
            .stop_order
            .iter()
            .find(|name| !names.contains(name.as_str()))
        {
            Some(name) => Err(TopologyError::new(
                name,
                anyhow::anyhow!("is in stop_order but not declared"),
            )),
            None => Ok(()),
        }
    }

    fn resolved_stop_order(&self) -> Vec<String> {
        let mut order: Vec<String> = Vec::with_capacity(self.routes.len());
        let reversed = self.routes.iter().rev().map(|(name, _)| name);
        for name in self.stop_order.iter().chain(reversed) {
            if !order.contains(name) {
                order.push(name.clone());
            }
        }
        order
    }

    fn diagram(&self) -> String {
        let mut diagram = format!("{}\n", self.name);
        for (name, route) in &self.routes {
            let input = endpoint_label(&route.input);
            let ends_in_handler = route.output.handler.is_some()
                && matches!(route.output.endpoint_type, EndpointType::Null);
            let output = if ends_in_handler {
                "handler".to_string()
            } else {
                endpoint_label(&route.output)
            };
            let _ = writeln!(diagram, "  {name}: {input} -> {output}");
            let outputs = route
                .output
                .handler
                .iter()
                .flat_map(|handler| handler.declared_outputs());
            for (output, target) in outputs {
                let _ = writeln!(diagram, "    {output} -> {target}");
            }
        }
        diagram
    }
}

fn in_stop_order(order: &[String], started: &HashMap<String, u64>) -> Vec<(String, u64)> {
    order
        .iter()
        .filter_map(|name| Some((name.clone(), *started.get(name)?)))
        .collect()
}

async fn stop_in_order(routes: &[(String, u64)]) {
    for (name, deployment) in routes {
        Route::stop_deployment(name, *deployment).await;
    }
}

/// A started [`Topology`]. Dropping it leaves the routes running.
#[derive(Debug, Clone)]
pub struct DeployedTopology {
    routes: Vec<(String, u64)>,
    diagram: String,
}

impl DeployedTopology {
    /// Stops the routes in stop order. Skips a route that already stopped or that was
    /// deployed again under the same name since.
    pub async fn stop(&self) {
        stop_in_order(&self.routes).await;
    }

    /// The routes in start order, each with its input, output and declared handler outputs.
    pub fn diagram(&self) -> &str {
        &self.diagram
    }
}

/// A name for logs and diagrams. Holds no URL, so no credentials can leak through it.
pub(crate) fn endpoint_label(endpoint: &Endpoint) -> String {
    match &endpoint.endpoint_type {
        EndpointType::Memory(config) => format!("memory:{}", config.topic),
        EndpointType::Custom { name, .. } => name.clone(),
        other => other.name().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocking_handler::{HandlerContext, ThreadedHandler};
    use crate::models::MemoryConfig;
    use crate::{get_or_create_channel, CanonicalMessage, Handled, Publisher};
    use std::time::Duration;

    fn memory_route(topic: &str) -> Route {
        Route::new(Endpoint::new_memory(topic, 8), Endpoint::null())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_topology_runs_shows_its_outputs_and_stops() {
        let input = Endpoint::new_memory("topology_run_in", 8);
        let reports = Publisher::new(Endpoint::new_memory("topology_run_reports", 8))
            .await
            .unwrap();
        let channel = get_or_create_channel(&MemoryConfig::new("topology_run_reports", Some(8)));
        let handler = ThreadedHandler::builder(|msg: CanonicalMessage, ctx: &HandlerContext| {
            ctx.output("reports")?.blocking_send(msg)?;
            Ok(Handled::Ack)
        })
        .output("reports", reports)
        .build();

        let route = Route::handled_by(input.clone(), handler).with_batch_size(1);
        let deployed = Topology::new("run")
            .route("topology_run", route)
            .deploy()
            .await
            .unwrap();
        assert_eq!(
            deployed.diagram(),
            "run\n  topology_run: memory:topology_run_in -> handler\n    reports -> memory:topology_run_reports\n"
        );

        input
            .channel()
            .unwrap()
            .send_message("x".into())
            .await
            .unwrap();
        let mut received = Vec::new();
        for _ in 0..200 {
            received.extend(channel.drain_messages());
            if !received.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(received.len(), 1);

        deployed.stop().await;
        deployed.stop().await;
        assert!(Route::get("topology_run").is_none());
    }

    #[test]
    fn routes_stop_in_reverse_unless_listed_first() {
        let topology = Topology::new("order")
            .route("a", memory_route("topology_order_a"))
            .route("b", memory_route("topology_order_b"))
            .route("c", memory_route("topology_order_c"));
        assert_eq!(topology.resolved_stop_order(), ["c", "b", "a"]);
        let topology = topology.stop_order(["b"]);
        assert_eq!(topology.resolved_stop_order(), ["b", "c", "a"]);
    }

    #[tokio::test]
    async fn an_old_handle_does_not_stop_a_later_deployment() {
        let deployed = Topology::new("old")
            .route("topology_redeployed", memory_route("topology_old_in"))
            .deploy()
            .await
            .unwrap();
        memory_route("topology_new_in")
            .deploy("topology_redeployed")
            .await
            .unwrap();

        deployed.stop().await;

        assert!(Route::get("topology_redeployed").is_some());
        assert!(Route::stop("topology_redeployed").await);
    }

    #[tokio::test]
    async fn a_failed_start_names_the_route_and_stops_the_started_ones() {
        let missing = Endpoint::new(EndpointType::Custom {
            name: "topology_no_such_endpoint".into(),
            config: serde_json::Value::Null,
        });
        let error = Topology::new("failing")
            .route("topology_started", memory_route("topology_fail_in"))
            .route("topology_broken", Route::new(missing, Endpoint::null()))
            .deploy()
            .await
            .err()
            .unwrap();

        assert_eq!(error.route, "topology_broken");
        assert!(Route::get("topology_started").is_none());
    }

    #[tokio::test]
    async fn an_invalid_declaration_starts_nothing() {
        let first =
            || Topology::new("t").route("topology_valid", memory_route("topology_invalid_in"));
        let twice = first().route("topology_valid", memory_route("topology_invalid_in"));
        let unknown_stop = first().stop_order(["topology_typo"]);

        for (declared, route) in [(twice, "topology_valid"), (unknown_stop, "topology_typo")] {
            assert_eq!(declared.deploy().await.err().unwrap().route, route);
            assert!(Route::get("topology_valid").is_none());
        }
    }
}
