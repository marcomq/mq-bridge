//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Where a source resumes and what the delivery guarantee is.

use super::*;

/// Inputs that forget a message once they hand it over, so a crash in the route loses it.
pub(crate) fn source_is_at_most_once(endpoint_type: &EndpointType) -> bool {
    match endpoint_type {
        EndpointType::ZeroMq(_) => true,
        EndpointType::Nats(config) => config.no_jetstream,
        EndpointType::Mqtt(config) => config.qos == Some(0),
        EndpointType::Http(config) => config.fire_and_forget,
        // A listener cannot tell its client that a frame was rejected.
        EndpointType::WebSocket(_) => true,
        // The dynamic client reads a server stream that has no acknowledgement.
        EndpointType::Grpc(config) => {
            !config.server_mode
                && (config.descriptor_set_bytes.is_some()
                    || config.descriptor_set_path.is_some()
                    || config.reflection)
        }
        EndpointType::Custom { name, config } => crate::extensions::get_endpoint_factory(name)
            .is_some_and(|factory| !factory.acknowledges(config)),
        _ => false,
    }
}

/// Whether `no_resume` reaches this input. A compiled-out endpoint is served by a registered
/// factory, and the search presets build their reader without it, so both still resume.
#[allow(clippy::match_like_matches_macro)] // Every `cfg!` is `true` in a full build.
pub(super) fn input_applies_no_resume(endpoint_type: &EndpointType) -> bool {
    match endpoint_type {
        EndpointType::ObjectStore(_) => cfg!(feature = "object-store"),
        EndpointType::HttpBulk(_) => cfg!(feature = "http-bulk"),
        EndpointType::ClickHouse(_) => cfg!(feature = "clickhouse"),
        EndpointType::Sqlx(_) => cfg!(any(feature = "sqlite", feature = "sqlx")),
        EndpointType::MongoDb(_) => cfg!(feature = "mongodb"),
        EndpointType::Ref(_) => true,
        _ => false,
    }
}

/// Whether every run of this input reads the full data set from the start. `false` unless the
/// configuration shows it: no saved position, nothing consumed, no dedup across runs.
/// `no_resume` ignores saved checkpoints where the consumer applies it.
pub(super) fn source_replays_from_start(
    endpoint: &Endpoint,
    no_resume: bool,
    depth: usize,
) -> bool {
    const MAX_DEPTH: usize = 16;
    let no_resume = no_resume && input_applies_no_resume(&endpoint.endpoint_type);
    // A checkpoint opens only with both a cursor id and a store.
    let unsaved = |cursor_id: &Option<String>, store: &Option<String>| {
        no_resume || cursor_id.is_none() || store.is_none()
    };
    if depth > MAX_DEPTH
        || endpoint
            .middlewares
            .iter()
            .any(|middleware| matches!(middleware, Middleware::Deduplication(_)))
    {
        return false;
    }
    match &endpoint.endpoint_type {
        EndpointType::File(config) => matches!(
            config.mode,
            None | Some(crate::models::FileConsumerMode::Consume { delete: false })
        ),
        EndpointType::Static(_) => true,
        EndpointType::ObjectStore(config) => unsaved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::HttpBulk(config) => config.read.as_ref().is_some_and(|read| {
            read.stream.is_none()
                && read.cursor.start.is_none()
                && unsaved(&read.cursor_id, &read.checkpoint_store)
        }),
        EndpointType::Meilisearch(config) => unsaved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::Qdrant(config) => unsaved(&config.cursor_id, &config.checkpoint_store),
        // Without `cursor_id` the cursor is only kept across reconnects of one run.
        EndpointType::Sqlx(config) => {
            config.cursor_column.is_some()
                && (no_resume || config.cursor_id.is_none())
                && config.publication.is_none()
                && !config.delete_after_read
        }
        EndpointType::ClickHouse(config) => {
            config.cursor_column.is_some() && unsaved(&config.cursor_id, &config.checkpoint_store)
        }
        EndpointType::MongoDb(config) => {
            config.resolved_consume() == crate::models::MongoConsume::Snapshot
        }
        EndpointType::Sequence(config) => {
            config.cursor_id.is_none()
                && config
                    .endpoints
                    .iter()
                    .all(|endpoint| source_replays_from_start(endpoint, false, depth + 1))
        }
        EndpointType::Ref(name) => get_endpoint(name)
            .is_some_and(|referenced| source_replays_from_start(&referenced, no_resume, depth + 1)),
        EndpointType::Custom { name, config } => crate::extensions::get_endpoint_factory(name)
            .is_some_and(|factory| factory.replays_from_start(config)),
        _ => false,
    }
}

/// Whether this input durably saves how far it has read: a checkpoint or a replication slot.
/// `sequence` children ignore `no_resume`, so they are checked without it.
pub(super) fn input_saves_position(endpoint: &Endpoint, no_resume: bool, depth: usize) -> bool {
    const MAX_DEPTH: usize = 16;
    let no_resume = no_resume && input_applies_no_resume(&endpoint.endpoint_type);
    let saved = |cursor_id: &Option<String>, store: &Option<String>| {
        !no_resume && cursor_id.is_some() && store.is_some()
    };
    match &endpoint.endpoint_type {
        _ if depth > MAX_DEPTH => false,
        EndpointType::PostgresCdc(config) => !config.temporary_slot,
        EndpointType::Sqlx(config) => {
            config.publication.is_some()
                || (!no_resume && config.cursor_id.is_some() && config.cursor_column.is_some())
        }
        EndpointType::MongoDb(config) => {
            !no_resume
                && config.cursor_id.is_some()
                && matches!(
                    config.resolved_consume(),
                    crate::models::MongoConsume::CaptureNew
                        | crate::models::MongoConsume::CaptureAll
                )
        }
        EndpointType::ClickHouse(config) => saved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::ObjectStore(config) => saved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::HttpBulk(config) => config
            .read
            .as_ref()
            .is_some_and(|read| saved(&read.cursor_id, &read.checkpoint_store)),
        EndpointType::Meilisearch(config) => saved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::Qdrant(config) => saved(&config.cursor_id, &config.checkpoint_store),
        EndpointType::Sequence(config) => {
            (config.cursor_id.is_some() && config.checkpoint_store.is_some())
                || config
                    .endpoints
                    .iter()
                    .any(|endpoint| input_saves_position(endpoint, false, depth + 1))
        }
        EndpointType::Ref(name) => get_endpoint(name)
            .is_some_and(|referenced| input_saves_position(&referenced, no_resume, depth + 1)),
        _ => false,
    }
}

/// Applies `rewrite` to every sink a publish can reach (fanout, switch, request, ref, dlq)
/// and collects what it reports; a ref is inlined only when one of its sinks changed.
pub(super) fn rewrite_sinks(
    endpoint: &mut Endpoint,
    depth: usize,
    rewrite: &mut dyn FnMut(&mut Endpoint) -> Option<String>,
) -> Vec<String> {
    if depth > 16 {
        return Vec::new();
    }
    let mut changed: Vec<String> = Vec::new();
    for middleware in endpoint.middlewares.iter_mut() {
        if let Middleware::Dlq(config) = middleware {
            changed.extend(rewrite_sinks(&mut config.endpoint, depth + 1, rewrite));
        }
    }
    changed.extend(match &mut endpoint.endpoint_type {
        EndpointType::Fanout(outputs) => outputs
            .iter_mut()
            .flat_map(|output| rewrite_sinks(output, depth + 1, rewrite))
            .collect(),
        EndpointType::Switch(config) => {
            let mut changed = Vec::new();
            for output in config.cases.values_mut() {
                changed.extend(rewrite_sinks(output, depth + 1, rewrite));
            }
            for case in &mut config.when {
                changed.extend(rewrite_sinks(&mut case.to, depth + 1, rewrite));
            }
            if let Some(output) = config.default.as_deref_mut() {
                changed.extend(rewrite_sinks(output, depth + 1, rewrite));
            }
            changed
        }
        EndpointType::Request(config) => {
            let mut changed = rewrite_sinks(&mut config.to, depth + 1, rewrite);
            changed.extend(rewrite_sinks(&mut config.forward_to, depth + 1, rewrite));
            changed
        }
        EndpointType::Ref(name) => match get_endpoint(name) {
            // Merged as publisher creation does: the ref's middlewares wrap the target's.
            Some(mut resolved) if endpoint.handler.is_none() || resolved.handler.is_none() => {
                let changed = rewrite_sinks(&mut resolved, depth + 1, rewrite);
                if !changed.is_empty() {
                    resolved.middlewares.append(&mut endpoint.middlewares);
                    resolved.handler = resolved.handler.take().or(endpoint.handler.take());
                    *endpoint = resolved;
                }
                changed
            }
            _ => Vec::new(),
        },
        _ => rewrite(endpoint).into_iter().collect(),
    });
    changed
}

/// Turns fsync off for local object-store sinks that leave it unset, name by write time and
/// sit behind an input that replays everything each run: a rerun rewrites what a crash lost.
pub(super) fn relax_object_store_fsync(
    route_name: &str,
    input: &Endpoint,
    output: &Endpoint,
    source_has_position: bool,
    no_resume: bool,
) -> Option<Endpoint> {
    if !source_replays_from_start(input, no_resume, 0) {
        return None;
    }
    let mut output = output.clone();
    let urls = rewrite_sinks(
        &mut output,
        0,
        &mut |endpoint| match &mut endpoint.endpoint_type {
            EndpointType::ObjectStore(config)
                if config.fsync.is_none()
                    && config.url.starts_with("file:")
                    && config.resolved_name_by(source_has_position) == NameBy::WriteTime =>
            {
                config.fsync = Some(crate::models::ObjectStoreFsync::Off);
                Some(config.url.clone())
            }
            _ => None,
        },
    );
    if urls.is_empty() {
        return None;
    }
    info!(
        route = route_name,
        urls = ?urls,
        "The input replays from the start each run, so write-time named object-store sinks skip fsync; set `fsync: batch` to sync anyway"
    );
    Some(output)
}

/// Moves appending `file` sinks with `fsync: off` to `periodic` when the input saves its
/// position, so a saved position never covers data a power loss can still undo.
pub(super) fn sync_file_sinks_behind_saved_position(
    route_name: &str,
    input: &Endpoint,
    output: &Endpoint,
    source_has_position: bool,
    no_resume: bool,
) -> Option<Endpoint> {
    fn visit(endpoint: &mut Endpoint, source_has_position: bool, depth: usize) -> Vec<String> {
        rewrite_sinks(
            endpoint,
            depth,
            &mut |endpoint| match &mut endpoint.endpoint_type {
                EndpointType::File(config) => {
                    // A pipe or terminal cannot be synced; part files always sync.
                    let not_a_file = std::fs::metadata(&config.path).is_ok_and(|m| !m.is_file());
                    if config.fsync != crate::models::FileFsync::Off
                        || not_a_file
                        || config.resolved_name_by(source_has_position) == NameBy::SourcePosition
                    {
                        None
                    } else {
                        config.fsync = crate::models::FileFsync::Periodic;
                        Some(config.path.clone())
                    }
                }
                _ => None,
            },
        )
    }

    if !input_saves_position(input, no_resume, 0) {
        return None;
    }
    let mut output = output.clone();
    let paths = visit(&mut output, source_has_position, 0);
    if paths.is_empty() {
        return None;
    }
    info!(
        route = route_name,
        paths = ?paths,
        "The input saves its read position, so file sinks with `fsync: off` use `fsync: periodic`; a power loss would otherwise leave the saved position ahead of the written data"
    );
    Some(output)
}

/// The `mqb.src.*` family a positioned input stamps; see [`supports_source_metadata`].
pub(super) fn source_position_prefix(endpoint_type: &EndpointType) -> Option<&'static str> {
    match endpoint_type {
        EndpointType::Kafka(_) => Some("mqb.src.kafka_"),
        EndpointType::PostgresCdc(_) => Some("mqb.src.postgres_"),
        EndpointType::Sqlx(config) if config.publication.is_some() => Some("mqb.src.postgres_"),
        EndpointType::Sqlx(config) if config.cursor_column.is_some() => Some("mqb.src.sqlx_"),
        EndpointType::MongoDb(_) if supports_source_metadata(endpoint_type) => {
            Some("mqb.src.mongodb_")
        }
        EndpointType::File(_) => Some("mqb.src.file_"),
        _ => None,
    }
}

/// Whether the input deduplicates on `message_id` although it mints a fresh one per read.
pub(super) fn dedup_keys_on_minted_message_id(input: &Endpoint) -> bool {
    let default_key = input.middlewares.iter().any(|middleware| {
        matches!(middleware, Middleware::Deduplication(config) if config.key.is_none())
    });
    default_key
        && match &input.endpoint_type {
            EndpointType::File(_)
            | EndpointType::ObjectStore(_)
            | EndpointType::Aws(_)
            | EndpointType::IbmMq(_)
            | EndpointType::ZeroMq(_)
            | EndpointType::ClickHouse(_) => true,
            // Only the queue mode reads back the id the publisher stored.
            EndpointType::MongoDb(config) => {
                config.resolved_consume() != crate::models::MongoConsume::Consumer
            }
            _ => false,
        }
}

impl Route {
    /// Whether this route's input can stamp a replay position, which is what `name_by: auto`
    /// resolves against. A `ref` input that cannot be resolved counts as no position, so `auto`
    /// falls back to `write_time` rather than failing the route on a naming choice.
    pub(super) fn source_has_position(&self) -> bool {
        self.resolved_input()
            .is_some_and(|input| supports_source_metadata(&input.endpoint_type))
    }

    /// The input with any `ref` chain followed, carrying the middlewares of every hop.
    /// `None` when a ref cannot be resolved.
    pub(super) fn resolved_input(&self) -> Option<std::borrow::Cow<'_, Endpoint>> {
        // A ref may name another ref, so follow the chain the way `resolve_endpoint_recursive`
        // does. The depth bound doubles as the cycle guard: a loop simply runs out of it.
        const MAX_DEPTH: usize = 16;
        let EndpointType::Ref(name) = &self.input.endpoint_type else {
            return Some(std::borrow::Cow::Borrowed(&self.input));
        };
        let mut middlewares = self.input.middlewares.clone();
        let mut name = name.clone();
        for _ in 0..MAX_DEPTH {
            let mut referenced = get_endpoint(&name)?;
            middlewares.append(&mut referenced.middlewares);
            match referenced.endpoint_type {
                EndpointType::Ref(next) => name = next,
                _ => {
                    referenced.middlewares = middlewares;
                    return Some(std::borrow::Cow::Owned(referenced));
                }
            }
        }
        None
    }

    /// Whether the output carries a matching middleware, following any `ref` chain.
    pub(super) fn output_has_middleware(&self, matches: impl Fn(&Middleware) -> bool) -> bool {
        const MAX_DEPTH: usize = 16;
        if self.output.middlewares.iter().any(&matches) {
            return true;
        }
        let EndpointType::Ref(name) = &self.output.endpoint_type else {
            return false;
        };
        let mut name = name.clone();
        for _ in 0..MAX_DEPTH {
            let Some(referenced) = get_endpoint(&name) else {
                return false;
            };
            if referenced.middlewares.iter().any(&matches) {
                return true;
            }
            match referenced.endpoint_type {
                EndpointType::Ref(next) => name = next,
                _ => return false,
            }
        }
        false
    }

    /// Returns the sink mechanism that makes replayed writes idempotent, when it can be
    /// established from configuration alone. This is informational: the route remains
    /// at-least-once internally, while the observable sink result is effectively-once.
    pub(super) fn inferred_idempotency_mechanism(&self, output: &Endpoint) -> Option<&'static str> {
        if output.handler.is_some() {
            return None;
        }
        let input = self.resolved_input();
        let source_has_position = input
            .as_ref()
            .is_some_and(|input| supports_source_metadata(&input.endpoint_type));
        let position_prefix = input
            .as_ref()
            .and_then(|input| source_position_prefix(&input.endpoint_type));
        // A key built from `mqb.src.*` is only replay-stable when the input stamps that position.
        let replayable = |key: &str| {
            key.match_indices("mqb.src.")
                .all(|(at, _)| position_prefix.is_some_and(|prefix| key[at..].starts_with(prefix)))
        };
        match &output.endpoint_type {
            EndpointType::MongoDb(config) if config.id_field.as_deref().is_some_and(replayable) => {
                Some("MongoDB unique _id")
            }
            EndpointType::Sqlx(config) => {
                let raw = config.insert_query.as_deref()?;
                if !replayable(raw) {
                    return None;
                }
                raw.to_ascii_uppercase()
                    .split_once("ON CONFLICT")
                    .is_some_and(|(_, conflict_action)| conflict_action.contains("DO NOTHING"))
                    .then_some("SQL unique-key conflict handling")
            }
            EndpointType::File(config)
                if config.resolved_name_by(source_has_position) == NameBy::SourcePosition =>
            {
                Some("part names carrying the source range")
            }
            EndpointType::ObjectStore(config)
                if config.resolved_name_by(source_has_position) == NameBy::SourcePosition =>
            {
                Some("object names carrying the source range")
            }
            EndpointType::Custom { name, config }
                if crate::extensions::get_endpoint_factory(name)
                    .is_some_and(|factory| factory.idempotent_sink(config)) =>
            {
                Some("custom endpoint's idempotent write")
            }
            _ => None,
        }
    }

    /// The delivery guarantee this route's configuration supports, as logged at startup.
    ///
    /// Conservative: `EffectivelyOnce` only for sink writes recognisable as idempotent from
    /// configuration alone, `AtMostOnce` only for inputs with no acknowledgement at all.
    /// Everything else is `AtLeastOnce`.
    pub fn delivery_guarantee(&self) -> DeliveryGuarantee {
        if let Err(error) = self.load_named_plugins() {
            tracing::warn!("failed to load a plugin this route names: {error:#}");
        }
        let relaxed =
            relax_object_naming("", self.source_has_position(), &self.input, &self.output)
                .ok()
                .flatten()
                .map(|(output, _)| output);
        self.inferred_delivery(relaxed.as_ref().unwrap_or(&self.output))
            .0
    }

    pub(super) fn inferred_delivery(
        &self,
        output: &Endpoint,
    ) -> (DeliveryGuarantee, Option<&'static str>) {
        if self
            .resolved_input()
            .is_some_and(|input| source_is_at_most_once(&input.endpoint_type))
        {
            return (DeliveryGuarantee::AtMostOnce, None);
        }
        match self.inferred_idempotency_mechanism(output) {
            Some(mechanism) => (DeliveryGuarantee::EffectivelyOnce, Some(mechanism)),
            None => (DeliveryGuarantee::AtLeastOnce, None),
        }
    }
}
