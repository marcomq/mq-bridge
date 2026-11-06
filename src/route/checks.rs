//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Checks run before a route starts: fault middleware, plugins, replay-safe sinks.

use super::*;

pub(super) fn check_fault_middleware_allowed(
    endpoint: &Endpoint,
    route_name: &str,
    role: &str,
    depth: usize,
    visited: &mut std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Err(anyhow::anyhow!(
            "[route:{}] Endpoint policy recursion depth exceeded limit of {}",
            route_name,
            MAX_DEPTH
        ));
    }

    if endpoint
        .middlewares
        .iter()
        .any(|m| matches!(m, Middleware::RandomPanic(cfg) if cfg.enabled))
    {
        return Err(anyhow::anyhow!(
            "[route:{}] random_panic middleware is disabled by default for {} endpoints; set allow_fault_injection: true to enable it",
            route_name,
            role
        ));
    }

    for nested in crate::endpoints::streaming_endpoints(&endpoint.endpoint_type)
        .into_iter()
        .flatten()
    {
        check_fault_middleware_allowed(nested, route_name, role, depth + 1, visited)?;
    }
    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            if !visited.insert(name.clone()) {
                return Ok(());
            }
            if let Some(referenced) = get_endpoint(name) {
                check_fault_middleware_allowed(&referenced, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Fanout(endpoints) => {
            for endpoint in endpoints {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Switch(cfg) => {
            for endpoint in cfg.cases.values() {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
            if let Some(endpoint) = &cfg.default {
                check_fault_middleware_allowed(endpoint, route_name, role, depth + 1, visited)?;
            }
        }
        EndpointType::Reader(inner) => {
            check_fault_middleware_allowed(inner, route_name, role, depth + 1, visited)?;
        }
        _ => {}
    }

    Ok(())
}

pub(super) fn endpoint_tree_has_buffer(
    endpoint: &Endpoint,
    visited_refs: &mut HashSet<String>,
) -> bool {
    if endpoint
        .middlewares
        .iter()
        .any(|middleware| matches!(middleware, Middleware::Buffer(_)))
    {
        return true;
    }

    if endpoint.middlewares.iter().any(|middleware| {
        matches!(middleware, Middleware::Dlq(config) if endpoint_tree_has_buffer(&config.endpoint, visited_refs))
    }) {
        return true;
    }

    match &endpoint.endpoint_type {
        EndpointType::Ref(name) => {
            visited_refs.insert(name.clone())
                && get_endpoint(name)
                    .is_some_and(|referenced| endpoint_tree_has_buffer(&referenced, visited_refs))
        }
        EndpointType::Fanout(endpoints) => endpoints
            .iter()
            .any(|endpoint| endpoint_tree_has_buffer(endpoint, visited_refs)),
        EndpointType::Switch(config) => config
            .cases
            .values()
            .chain(config.when.iter().map(|case| &case.to))
            .chain(config.default.iter().map(Box::as_ref))
            .any(|endpoint| endpoint_tree_has_buffer(endpoint, visited_refs)),
        EndpointType::Reader(inner) => endpoint_tree_has_buffer(inner, visited_refs),
        EndpointType::Request(config) => {
            endpoint_tree_has_buffer(&config.to, visited_refs)
                || endpoint_tree_has_buffer(&config.forward_to, visited_refs)
        }
        _ => false,
    }
}

/// Loads the plugins this endpoint tree names, so route analysis sees their factories.
#[cfg(feature = "plugin")]
pub(super) fn load_named_plugins(
    endpoint: &Endpoint,
    depth: usize,
    visited_refs: &mut HashSet<String>,
) -> anyhow::Result<()> {
    const MAX_DEPTH: usize = 16;
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut nested: Vec<&Endpoint> = Vec::new();
    for middleware in &endpoint.middlewares {
        match middleware {
            Middleware::Custom { name, .. } => {
                if crate::extensions::get_middleware_factory(name).is_none() {
                    crate::plugin::discover_middleware_plugin_or_scan(name)?;
                }
            }
            Middleware::Dlq(config) => nested.push(&config.endpoint),
            Middleware::Lookup(config) => {
                nested.extend(config.from.iter());
                nested.extend(config.entries.iter().map(|entry| &entry.from));
            }
            _ => {}
        }
    }
    match &endpoint.endpoint_type {
        EndpointType::Custom { name, .. } => {
            if crate::endpoints::named_endpoint_factory(name).is_none() {
                crate::plugin::discover_endpoint_plugin_or_scan(name)?;
            }
        }
        EndpointType::Ref(name) => {
            if visited_refs.insert(name.clone()) {
                if let Some(referenced) = get_endpoint(name) {
                    load_named_plugins(&referenced, depth + 1, visited_refs)?;
                }
            }
        }
        EndpointType::Fanout(endpoints) => nested.extend(endpoints),
        EndpointType::Sequence(config) => nested.extend(&config.endpoints),
        EndpointType::Switch(config) => nested.extend(
            config
                .cases
                .values()
                .chain(config.when.iter().map(|case| &case.to))
                .chain(config.default.as_deref()),
        ),
        EndpointType::Reader(inner) => nested.push(inner),
        EndpointType::Request(config) => nested.extend([&*config.to, &*config.forward_to]),
        EndpointType::Http(config) => nested.extend(config.stream_response_to.as_deref()),
        EndpointType::HttpBulk(config) => nested.extend(config.query.iter().flat_map(|query| {
            query
                .stream_to
                .as_deref()
                .into_iter()
                .chain(query.cancel_from.as_deref())
        })),
        EndpointType::OpenAiChat(config) => nested.extend(
            config
                .stream_to
                .as_deref()
                .into_iter()
                .chain(config.cancel_from.as_deref()),
        ),
        _ => {}
    }
    for endpoint in nested {
        load_named_plugins(endpoint, depth + 1, visited_refs)?;
    }
    Ok(())
}

/// The name of the first sink in this output that replaces its target with each run's data.
pub(super) fn endpoint_tree_full_replay_sink(
    endpoint: &Endpoint,
    visited_refs: &mut HashSet<String>,
) -> Option<String> {
    match &endpoint.endpoint_type {
        EndpointType::Custom { name, config } => crate::extensions::get_endpoint_factory(name)
            .filter(|factory| factory.requires_full_replay(config))
            .map(|_| name.clone()),
        EndpointType::Ref(name) => {
            if !visited_refs.insert(name.clone()) {
                return None;
            }
            get_endpoint(name)
                .and_then(|referenced| endpoint_tree_full_replay_sink(&referenced, visited_refs))
        }
        EndpointType::Fanout(endpoints) => endpoints
            .iter()
            .find_map(|endpoint| endpoint_tree_full_replay_sink(endpoint, visited_refs)),
        EndpointType::Switch(config) => config
            .cases
            .values()
            .chain(config.when.iter().map(|case| &case.to))
            .chain(config.default.iter().map(Box::as_ref))
            .find_map(|endpoint| endpoint_tree_full_replay_sink(endpoint, visited_refs)),
        EndpointType::Request(config) => endpoint_tree_full_replay_sink(&config.to, visited_refs)
            .or_else(|| endpoint_tree_full_replay_sink(&config.forward_to, visited_refs)),
        _ => None,
    }
}

impl Route {
    /// Validates the route configuration, checking if endpoints are supported and correctly configured.
    /// Core types like file, memory, and response are always supported.
    /// # Arguments
    /// * `name` - The name of the route
    /// * `allowed_endpoints` - An optional list of allowed endpoint types
    pub fn check(
        &self,
        name: &str,
        allowed_endpoints: Option<&[&str]>,
    ) -> anyhow::Result<Vec<String>> {
        self.check_with_resume(name, allowed_endpoints, false)
    }

    /// Loads the plugins the route names before anything asks the factory registry.
    pub(super) fn load_named_plugins(&self) -> anyhow::Result<()> {
        #[cfg(feature = "plugin")]
        {
            let mut visited_refs = HashSet::new();
            load_named_plugins(&self.input, 0, &mut visited_refs)?;
            load_named_plugins(&self.output, 0, &mut visited_refs)?;
        }
        Ok(())
    }

    pub(super) fn check_with_resume(
        &self,
        name: &str,
        allowed_endpoints: Option<&[&str]>,
        no_resume: bool,
    ) -> anyhow::Result<Vec<String>> {
        self.options.validate()?;
        self.load_named_plugins()
            .map_err(|error| anyhow::anyhow!("[route:{name}] {error:#}"))?;
        if !self.options.allow_fault_injection {
            check_fault_middleware_allowed(
                &self.input,
                name,
                "input",
                0,
                &mut std::collections::HashSet::new(),
            )?;
            check_fault_middleware_allowed(
                &self.output,
                name,
                "output",
                0,
                &mut std::collections::HashSet::new(),
            )?;
        }

        let mut warnings = Vec::new();
        warnings.extend(crate::endpoints::check_consumer(
            name,
            &self.input,
            allowed_endpoints,
        )?);
        warnings.extend(crate::endpoints::check_publisher(
            name,
            &self.output,
            allowed_endpoints,
        )?);
        if let Some(warning) = self.options.in_flight_warning() {
            warnings.push(format!("Route '{name}': {warning}"));
        }
        if self.options.concurrency > 1
            && (endpoint_tree_has_buffer(&self.input, &mut HashSet::new())
                || endpoint_tree_has_buffer(&self.output, &mut HashSet::new()))
        {
            warnings.push(format!(
                "Route '{name}' uses buffer middleware with concurrency {}. Buffering preserves \
                 order within each batch, but concurrent destination writes may complete out of \
                 source order. Use concurrency: 1 when destination order must be preserved.",
                self.options.concurrency
            ));
        }
        if self
            .resolved_input()
            .is_some_and(|input| dedup_keys_on_minted_message_id(&input))
        {
            warnings.push(format!(
                "Route '{name}' deduplicates on `message_id`, which this input mints fresh on \
                 every read, so a redelivery never matches. Set the deduplication `key` to a \
                 business key, or to `${{metadata:mqb.id}}` derived by the `id` middleware."
            ));
        }
        if let Some(required) = self.options.required_delivery {
            let actual = self.delivery_guarantee();
            if actual < required {
                return Err(anyhow::anyhow!(
                    "Route '{name}' requires {required} delivery, but its configuration supports \
                     only {actual}. Make the sink write idempotent (Mongo `id_field`, SQL `ON \
                     CONFLICT … DO NOTHING`, file/object_store `name_by: source_position`), \
                     see docs/DELIVERY.md."
                ));
            }
        }
        if let Some(sink) = endpoint_tree_full_replay_sink(&self.output, &mut HashSet::new()) {
            if !source_replays_from_start(&self.input, no_resume, 0) {
                let input = match self.resolved_input().as_deref().map(|i| &i.endpoint_type) {
                    Some(EndpointType::Custom { name, .. }) => name.clone(),
                    Some(endpoint_type) => endpoint_type.name().to_string(),
                    None => self.input.endpoint_type.name().to_string(),
                };
                return Err(anyhow::anyhow!(
                    "Route '{name}': the '{sink}' output replaces its target with the data of \
                     each run, but the '{input}' input does not read the full data set on every \
                     run: it resumes from a saved position, consumes what it reads, or \
                     deduplicates across runs. Use an input that reads everything each time, \
                     see docs/DELIVERY.md."
                ));
            }
        }
        Ok(warnings)
    }
}
