//! Postgres logical-replication CDC source endpoint (pgoutput).
//!
//! Source-only. Streams row-level changes from a Postgres logical replication
//! slot, decodes the pgoutput protocol into flat JSON rows tagged with a
//! `postgres.operation` marker, and maps mq-bridge's ack/nack model onto the
//! slot's confirmed-LSN feedback:
//!
//! * **Ack** a batch → advance the confirmed LSN and feed it back to the server
//!   (`standby_status_update`), so the slot's `confirmed_flush_lsn` moves forward
//!   and WAL up to that point may be recycled.
//! * **Nack** / no commit → the confirmed LSN is **not** advanced, so on
//!   reconnect replication resumes from the last durably-acknowledged position —
//!   at-least-once, no data loss across an in-flight restart.
//!
//! The confirmed LSN is cumulative (like Kafka offsets), so
//! [`commit_requires_order`](MessageConsumer::commit_requires_order) is `true`.

mod pgoutput;
pub(crate) mod replication;
mod state;

use crate::canonical_message::CanonicalMessage;
use crate::checkpoint::{checkpoint_key, CheckpointStore, FileCheckpointStore};
use crate::errors::ConsumerError;
use crate::models::PostgresCdcConfig;
use crate::traits::{BatchCommitFunc, MessageConsumer, MessageDisposition};
use crate::ReceivedBatch;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::FutureExt;
use pgwire_replication::{Lsn, ReplicationClient};
use replication::ReplicationEvent;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use pgoutput::decoder::decode_message;
use pgoutput::messages::{Delete, Insert, Message, Relation, Truncate, Update};
use pgoutput::registry::RelationRegistry;
use pgoutput::tuple_to_json_object;
use state::{format_lsn, parse_lsn};

/// A logical-replication CDC consumer for a single Postgres publication + slot.
pub struct PostgresCdcConsumer {
    client: ReplicationClient,
    registry: RelationRegistry,
    /// Rows decoded within the currently-open transaction (flushed to `ready`
    /// on COMMIT, once their durable resume position — the commit `end_lsn` — is known).
    tx_buffer: Vec<CanonicalMessage>,
    /// Committed rows awaiting delivery, each paired with the LSN to confirm on ack.
    ready: VecDeque<(CanonicalMessage, u64)>,
    /// Highest durably-acknowledged LSN; fed back to the server as the slot's confirmed position.
    confirmed: Arc<AtomicU64>,
    checkpoint: Option<Arc<dyn CheckpointStore>>,
    /// Connection URL used on teardown to advance the slot durably (see `Drop`).
    url: String,
    /// Slot name used on teardown to advance the slot durably (see `Drop`).
    slot_name: String,
    /// TLS config used on teardown to reopen the control-plane connection (see `Drop`).
    tls: crate::models::TlsConfig,
    ended: bool,
    /// A failure hit while topping up a batch, reported after that batch is delivered.
    deferred_error: Option<ConsumerError>,
    /// `temporary_slot`: drop the slot on teardown instead of advancing it.
    drop_slot_on_stop: bool,
    /// Set once teardown has run, so the `Drop` fallback never repeats the hook's work.
    teardown_done: AtomicBool,
    /// Drain mode: only then does an idle replication read time out into an empty batch.
    exit_on_empty: bool,
    /// Resolved once at construction from endpoint config and the legacy fallback.
    source_metadata: bool,
}

impl PostgresCdcConsumer {
    pub async fn new(config: &PostgresCdcConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, false).await
    }

    pub async fn new_with_source_metadata(
        config: &PostgresCdcConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        let source_metadata = crate::canonical_message::source_metadata_enabled_for_endpoint(
            source_metadata || config.source_metadata,
        );
        if config.url.trim().is_empty() {
            return Err(anyhow!("postgres_cdc: `url` is required"));
        }
        // `publication` and `slot_name` flow into replication commands
        // (START_REPLICATION ... SLOT ... PUBLICATION ...). The slot-lifecycle
        // SQL binds them as parameters, but restrict them to a safe identifier
        // charset as defence-in-depth against injection into the replication path.
        if !is_valid_pg_ident(&config.publication) {
            return Err(anyhow!(
                "postgres_cdc: `publication` must be a non-empty [A-Za-z0-9_] identifier"
            ));
        }
        if !is_valid_pg_ident(&config.slot_name) {
            return Err(anyhow!(
                "postgres_cdc: `slot_name` must be a non-empty [A-Za-z0-9_] identifier"
            ));
        }

        if config.create_publication {
            replication::ensure_publication(
                &config.url,
                &config.publication,
                &config.publication_tables,
                &config.tls,
            )
            .await?;
        }

        replication::ensure_slot(
            &config.url,
            &config.slot_name,
            config.create_slot,
            config.temporary_slot,
            &config.tls,
        )
        .await?;

        // Optional secondary checkpoint (the slot's confirmed_flush_lsn is the
        // authoritative durable position; this file mirror seeds a slot-advance
        // on reconnect and aids observability).
        let checkpoint: Option<Arc<dyn CheckpointStore>> = match &config.checkpoint_store {
            Some(spec) => {
                let cid = config
                    .cursor_id
                    .clone()
                    .unwrap_or_else(|| config.slot_name.clone());
                // Only a local file path or a `file://` URL is a valid checkpoint
                // store; reject any other URL scheme rather than treating it as a
                // literal path.
                let path = if let Some(rest) = spec.strip_prefix("file://") {
                    rest.to_string()
                } else if let Some((scheme, _)) = spec.split_once("://") {
                    return Err(anyhow!(
                        "postgres_cdc: checkpoint_store scheme `{scheme}://` is not supported; \
                         use a local file path or a file:// URL"
                    ));
                } else {
                    spec.to_string()
                };
                Some(Arc::new(FileCheckpointStore::new(
                    path,
                    checkpoint_key("postgres_cdc", &cid),
                )))
            }
            None => None,
        };

        let start_lsn: Option<u64> = match &checkpoint {
            Some(cp) => cp.load().await?.and_then(|s| match parse_lsn(&s) {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(value = %s, error = %e, "postgres_cdc: ignoring unparseable checkpoint LSN");
                    None
                }
            }),
            None => None,
        };

        if let Some(lsn) = start_lsn {
            replication::advance_slot(&config.url, &config.slot_name, lsn, &config.tls).await?;
        }

        let client = replication::start_replication(config, start_lsn).await?;
        info!(
            slot = %config.slot_name,
            publication = %config.publication,
            resume_lsn = ?start_lsn.map(format_lsn),
            "postgres_cdc: replication stream started"
        );

        Ok(Self {
            client,
            registry: RelationRegistry::new(),
            tx_buffer: Vec::new(),
            ready: VecDeque::new(),
            confirmed: Arc::new(AtomicU64::new(start_lsn.unwrap_or(0))),
            checkpoint,
            url: config.url.clone(),
            slot_name: config.slot_name.clone(),
            tls: config.tls.clone(),
            ended: false,
            deferred_error: None,
            drop_slot_on_stop: config.temporary_slot,
            teardown_done: AtomicBool::new(false),
            exit_on_empty: false,
            source_metadata,
        })
    }

    /// Release the slot: stop the stream, then either drop the slot (ephemeral run) or
    /// advance its `confirmed_flush_lsn` to the last acked position.
    ///
    /// Runs at most once. Both branches need a fresh control-plane connection —
    /// `pg_replication_slot_advance` and `pg_drop_replication_slot` are ordinary SQL, not
    /// replication commands — which is why this belongs in `on_disconnect_hook` (awaited by
    /// the route while the runtime is still healthy) rather than in `Drop`, where opening a
    /// connection during runtime teardown fails with "task was cancelled".
    async fn teardown(&self) {
        if self.teardown_done.swap(true, Ordering::AcqRel) {
            return;
        }
        let lsn = self.confirmed.load(Ordering::Acquire);
        if lsn == 0 && !self.drop_slot_on_stop {
            return;
        }
        // CopyDone; the worker exits and the server releases the slot shortly after. Both
        // helpers below poll `pg_replication_slots.active` until then.
        self.client.stop();
        if self.drop_slot_on_stop {
            // Ephemeral run: the slot is discarded, so its confirmed_flush_lsn is
            // irrelevant — dropping it releases the retained WAL outright.
            if let Err(e) = replication::drop_slot(&self.url, &self.slot_name, &self.tls).await {
                warn!(error = %e, "postgres_cdc: dropping ephemeral slot on shutdown failed");
            }
        } else if let Err(e) =
            replication::advance_slot_when_inactive(&self.url, &self.slot_name, lsn, &self.tls)
                .await
        {
            warn!(error = %e, "postgres_cdc: durable slot advance on shutdown failed");
        }
    }

    /// Push the current confirmed LSN to the server as the slot's applied position.
    fn feedback(&mut self) {
        let lsn = self.confirmed.load(Ordering::Acquire);
        self.client.update_applied_lsn(Lsn::from_u64(lsn));
    }

    /// Handle one replication event, staging rows and flushing committed
    /// transactions into the ready queue.
    fn handle_event(&mut self, ev: ReplicationEvent) -> anyhow::Result<()> {
        match ev {
            ReplicationEvent::Begin { .. } => {
                self.tx_buffer.clear();
            }
            ReplicationEvent::Commit { end_lsn, .. } => {
                let lsn = end_lsn.as_u64();
                let lsn_str = format_lsn(lsn);
                for (ordinal, mut msg) in self.tx_buffer.drain(..).enumerate() {
                    let dedup_id = msg.metadata.get("postgres.key").map(|key| {
                        let schema = msg
                            .metadata
                            .get("postgres.schema")
                            .map_or("", |s| s.as_str());
                        let table = msg
                            .metadata
                            .get("postgres.table")
                            .map_or("", |s| s.as_str());
                        let operation = msg
                            .metadata
                            .get("postgres.operation")
                            .map_or("", |s| s.as_str());
                        // Include op + in-tx ordinal so multiple changes to the same key
                        // in one commit get distinct (still deterministic) ids.
                        cdc_dedup_id(schema, table, key, lsn, operation, ordinal)
                    });
                    if let Some(id) = dedup_id {
                        msg.message_id = id;
                    }
                    msg.metadata
                        .insert("postgres.lsn".to_string(), lsn_str.clone());
                    if self.source_metadata {
                        add_source_metadata(&mut msg, &self.slot_name, lsn, ordinal);
                    }
                    self.ready.push_back((msg, lsn));
                }
            }
            ReplicationEvent::XLogData { data, .. } => {
                let msg = decode_message(&data)?;
                self.stage_message(msg)?;
            }
            ReplicationEvent::KeepAlive { .. } => {
                // The library replies to keepalives itself; nothing to stage.
            }
            ReplicationEvent::Message { prefix, .. } => {
                // Logical decoding messages (pg_logical_emit_message) carry no
                // row-change data; skip them rather than terminating the stream.
                debug!(%prefix, "postgres_cdc: ignoring logical decoding message");
            }
            other => {
                // An unrecognized variant may carry change data — fail fast
                // rather than silently drop it.
                return Err(anyhow!(
                    "postgres_cdc: unhandled replication event {other:?}"
                ));
            }
        }
        Ok(())
    }

    fn stage_message(&mut self, msg: Message) -> anyhow::Result<()> {
        match msg {
            Message::Relation(r) => self.registry.insert(r),
            Message::Insert(i) => {
                let m = stage_insert(self.registry.get(i.relation_oid)?, &i);
                self.tx_buffer.push(m);
            }
            Message::Update(u) => {
                let m = stage_update(self.registry.get(u.relation_oid)?, &u);
                self.tx_buffer.push(m);
            }
            Message::Delete(d) => {
                let m = stage_delete(self.registry.get(d.relation_oid)?, &d);
                self.tx_buffer.push(m);
            }
            Message::Truncate(t) => {
                let mut staged = Vec::new();
                for oid in &t.relation_oids {
                    match self.registry.get(*oid) {
                        Ok(rel) => staged.push(stage_truncate(rel, &t)),
                        Err(e) => {
                            debug!(oid, error = %e, "postgres_cdc: ignoring truncate for unknown relation")
                        }
                    }
                }
                self.tx_buffer.extend(staged);
            }
            // Begin/Commit arrive as protocol-level ReplicationEvent variants,
            // handled in handle_event; Origin/Type are accepted and ignored.
            Message::Begin(_) | Message::Commit(_) | Message::Origin | Message::Type => {}
        }
        Ok(())
    }
}

/// Build a change-event message: JSON body + `postgres.operation`/schema/table metadata.
/// When the relation has replica-identity key columns, their values are recorded as
/// `postgres.key`; the deterministic dedup `message_id` is stamped later at commit (once
/// the LSN is known) in [`PostgresCdcConsumer::handle_event`].
fn cdc_message(rel: &Relation, operation: &str, body: serde_json::Value) -> CanonicalMessage {
    let key = body
        .as_object()
        .and_then(|obj| replica_key_string(rel, obj));
    let payload = serde_json::to_vec(&body).unwrap_or_default();
    let mut msg = CanonicalMessage::new_bytes(payload.into(), None);
    msg.metadata
        .insert("postgres.operation".to_string(), operation.to_string());
    msg.metadata
        .insert("postgres.schema".to_string(), rel.namespace.clone());
    msg.metadata
        .insert("postgres.table".to_string(), rel.name.clone());
    if let Some(key) = key {
        msg.metadata.insert("postgres.key".to_string(), key);
    }
    msg
}

/// The relation's replica-identity key column values (bit 0 of the column flags) from a
/// decoded row, serialized as a typed JSON array so distinct value combinations can never
/// collide (unlike a delimiter join). `None` when the relation exposes no key (no PK /
/// replica identity NOTHING), in which case no deterministic id can be derived.
fn replica_key_string(
    rel: &Relation,
    body: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<serde_json::Value> = Vec::new();
    for col in &rel.columns {
        if col.flags & 1 == 1 {
            parts.push(
                body.get(&col.name)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            );
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(serde_json::Value::Array(parts).to_string())
    }
}

fn recv_error(e: impl std::fmt::Display) -> ConsumerError {
    let msg = format!("postgres_cdc: recv failed: {e}");
    // A vanished slot never comes back on its own; don't reconnect-loop.
    if replication::is_missing_slot_error(&msg) {
        ConsumerError::Permanent(anyhow!(msg))
    } else {
        ConsumerError::Connection(anyhow!(msg))
    }
}

/// Deterministic dedup id for a change event: FNV-1a 128-bit over
/// `schema.table\0key\0operation\0lsn\0ordinal`. A replayed change (same key, op and
/// in-tx position at the same commit LSN) hashes identically so the dedup middleware /
/// sink can drop it; distinct changes — including several to one key in one transaction —
/// differ by op, LSN, or ordinal.
fn cdc_dedup_id(
    schema: &str,
    table: &str,
    key: &str,
    lsn: u64,
    operation: &str,
    ordinal: usize,
) -> u128 {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let lsn_bytes = lsn.to_be_bytes();
    let ordinal_bytes = (ordinal as u64).to_be_bytes();
    let parts: [&[u8]; 11] = [
        schema.as_bytes(),
        b".",
        table.as_bytes(),
        b"\0",
        key.as_bytes(),
        b"\0",
        operation.as_bytes(),
        b"\0",
        &lsn_bytes,
        b"\0",
        &ordinal_bytes,
    ];
    let mut h = OFFSET;
    for part in parts {
        for &b in part {
            h ^= b as u128;
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

fn add_source_metadata(message: &mut CanonicalMessage, slot: &str, lsn: u64, ordinal: usize) {
    message
        .metadata
        .insert("mqb.src.postgres_slot".into(), slot.into());
    message
        .metadata
        .insert("mqb.src.postgres_lsn".into(), lsn.to_string());
    message
        .metadata
        .insert("mqb.src.postgres_ordinal".into(), ordinal.to_string());
}

/// A safe Postgres identifier for a publication / replication slot: non-empty
/// and `[A-Za-z0-9_]` only. Slot names are additionally lowercased by Postgres,
/// but rejecting anything outside this set is enough to keep the value from
/// escaping into the replication command it is interpolated into.
pub(crate) fn is_valid_pg_ident(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn stage_insert(rel: &Relation, i: &Insert) -> CanonicalMessage {
    let body = serde_json::Value::Object(tuple_to_json_object(rel, &i.new));
    cdc_message(rel, "insert", body)
}

fn stage_update(rel: &Relation, u: &Update) -> CanonicalMessage {
    let body = serde_json::Value::Object(tuple_to_json_object(rel, &u.new));
    cdc_message(rel, "update", body)
}

fn stage_delete(rel: &Relation, d: &Delete) -> CanonicalMessage {
    // For a delete the only data available is the old tuple (key columns, or the
    // full row under REPLICA IDENTITY FULL).
    let body = serde_json::Value::Object(tuple_to_json_object(rel, &d.old));
    cdc_message(rel, "delete", body)
}

fn stage_truncate(rel: &Relation, _t: &Truncate) -> CanonicalMessage {
    cdc_message(rel, "truncate", serde_json::json!({}))
}

#[async_trait]
impl MessageConsumer for PostgresCdcConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    /// Release the slot while the runtime is still healthy — see `teardown`.
    fn on_disconnect_hook(&self) -> Option<crate::traits::BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            self.teardown().await;
            Ok(())
        }))
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        // Report the latest durably-acknowledged position before blocking.
        self.feedback();

        if self.ready.is_empty() {
            if let Some(e) = self.deferred_error.take() {
                return Err(e);
            }
        }

        // Pump the replication stream until a committed transaction is ready.
        while self.ready.is_empty() {
            if self.ended {
                return Err(ConsumerError::EndOfStream);
            }
            // Drain mode: a brief idle timeout yields an empty batch (no-op commit, so the
            // slot is not advanced past unconsumed changes) and lets --drain fire.
            // `recv()` is cancel-safe — it awaits a channel; wire framing runs in a
            // background worker with its own resumable reader — so the timeout drop here
            // loses nothing.
            let Some(ev) = crate::traits::drain_gated(self.exit_on_empty, self.client.recv()).await
            else {
                return Ok(ReceivedBatch::empty());
            };
            match ev {
                Ok(None) | Ok(Some(ReplicationEvent::StoppedAt { .. })) => {
                    self.ended = true;
                    return Err(ConsumerError::EndOfStream);
                }
                Ok(Some(ev)) => {
                    self.handle_event(ev).map_err(ConsumerError::Connection)?;
                }
                Err(e) => return Err(recv_error(e)),
            }
        }

        // Top the batch up from events that already arrived, without waiting for more:
        // a backlog of single-row transactions otherwise leaves as one-row batches.
        // `unconstrained`: tokio's coop budget would report the channel empty after
        // ~128 events, capping batches at ~40 transactions. Bounded by event count too:
        // rows of a still-open transaction don't grow `ready`, so it alone may never fill.
        let mut event_budget = max_messages.saturating_mul(4).max(64);
        while self.deferred_error.is_none() && self.ready.len() < max_messages && event_budget > 0 {
            event_budget -= 1;
            match tokio::task::unconstrained(self.client.recv()).now_or_never() {
                None => break,
                Some(Ok(None)) | Some(Ok(Some(ReplicationEvent::StoppedAt { .. }))) => {
                    self.ended = true;
                    break;
                }
                Some(Ok(Some(ev))) => {
                    if let Err(e) = self.handle_event(ev) {
                        self.deferred_error = Some(ConsumerError::Connection(e));
                        break;
                    }
                }
                Some(Err(e)) => {
                    self.deferred_error = Some(recv_error(e));
                    break;
                }
            }
        }

        let n = self.ready.len().min(max_messages);
        let mut messages = Vec::with_capacity(n);
        let mut lsns = Vec::with_capacity(n);
        for _ in 0..n {
            let (msg, lsn) = self.ready.pop_front().expect("ready is non-empty");
            messages.push(msg);
            lsns.push(lsn);
        }

        let confirmed = self.confirmed.clone();
        let checkpoint = self.checkpoint.clone();
        let commit: BatchCommitFunc = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Advance to the LSN of the last consecutively-acked message.
                // A Nack (or an absent disposition) stops the advance there so
                // the un-acked change is redelivered on reconnect.
                let mut target = 0u64;
                for (disp, lsn) in dispositions.iter().zip(lsns.iter()) {
                    match disp {
                        MessageDisposition::Nack => break,
                        MessageDisposition::Ack | MessageDisposition::Reply(_) => target = *lsn,
                    }
                }
                if target > 0 {
                    let mut cur = confirmed.load(Ordering::Acquire);
                    while target > cur {
                        match confirmed.compare_exchange(
                            cur,
                            target,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => break,
                            Err(actual) => cur = actual,
                        }
                    }
                    if let Some(cp) = &checkpoint {
                        if let Err(e) = cp.save(&format_lsn(target)).await {
                            warn!(error = %e, "postgres_cdc: checkpoint save failed");
                        }
                    }
                }
                Ok(())
            })
        });

        Ok(ReceivedBatch { messages, commit })
    }

    /// The confirmed LSN is cumulative — acking a later LSN implicitly confirms
    /// everything before it — so commits must be applied in receive order.
    fn commit_requires_order(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Durably persist the last acked position on teardown.
///
/// The replication library's in-band standby-status feedback is asynchronous —
/// the worker sends it on its `status_interval` tick and, critically, does *not*
/// flush a final update on `stop()`/`shutdown()`. A consumer torn down right
/// after an ack can therefore lose that ack's durability: the server's
/// `confirmed_flush_lsn` never advances, and after a restart the slot replays
/// changes that were already acknowledged.
///
/// To close that gap we stop the stream (so the server releases the slot) and
/// advance the slot's `confirmed_flush_lsn` synchronously via SQL —
/// `pg_replication_slot_advance` requires an *inactive* slot, which is why this
/// cannot be done per-commit while the stream is live.
///
/// This needs `block_in_place`, so it only runs on a multi-thread runtime; on a
/// current-thread runtime (or off-runtime) we fall back to the best-effort async
/// feedback already sent during streaming.
impl Drop for PostgresCdcConsumer {
    /// Best-effort fallback for consumers dropped without a graceful stop (direct API use,
    /// a panicking route). The route path runs `on_disconnect_hook` first, and `teardown`
    /// is single-shot, so this is normally a no-op.
    ///
    /// It stays best-effort on purpose: opening the control-plane connection from here
    /// fails once the runtime has started cancelling tasks, which is exactly why the real
    /// teardown moved into the disconnect hook.
    fn drop(&mut self) {
        if self.teardown_done.load(Ordering::Acquire) {
            return;
        }
        let lsn = self.confirmed.load(Ordering::Acquire);
        // With no acked position and nothing to clean up there is no teardown work.
        if lsn == 0 && !self.drop_slot_on_stop {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            return;
        }
        tokio::task::block_in_place(|| handle.block_on(self.teardown()));
    }
}

#[cfg(test)]
mod cdc_id_tests {
    use super::*;
    use pgoutput::messages::{ColumnDesc, ReplicaIdentity};

    fn rel_with_key() -> Relation {
        Relation {
            oid: 16384,
            namespace: "public".into(),
            name: "orders".into(),
            replica_identity: ReplicaIdentity::Default,
            columns: vec![
                ColumnDesc {
                    flags: 1,
                    name: "id".into(),
                    type_oid: 23,
                    type_modifier: -1,
                },
                ColumnDesc {
                    flags: 0,
                    name: "body".into(),
                    type_oid: 25,
                    type_modifier: -1,
                },
            ],
        }
    }

    #[test]
    fn replica_key_uses_only_key_columns() {
        let rel = rel_with_key();
        let body = serde_json::json!({ "id": 42, "body": "hi" });
        let key = replica_key_string(&rel, body.as_object().unwrap());
        assert_eq!(key.as_deref(), Some("[42]"));
    }

    #[test]
    fn replica_key_none_without_key_columns() {
        let mut rel = rel_with_key();
        rel.columns[0].flags = 0;
        let body = serde_json::json!({ "id": 42 });
        assert!(replica_key_string(&rel, body.as_object().unwrap()).is_none());
    }

    #[test]
    fn cdc_message_sets_postgres_key() {
        let rel = rel_with_key();
        let body = serde_json::json!({ "id": 7, "body": "x" });
        let msg = cdc_message(&rel, "insert", body);
        assert_eq!(
            msg.metadata.get("postgres.key").map(String::as_str),
            Some("[7]")
        );
    }

    #[test]
    fn dedup_id_is_stable_and_lsn_sensitive() {
        // Same change (key + op + lsn + ordinal) → identical id (a replay deduplicates).
        let a = cdc_dedup_id("public", "orders", "42", 100, "insert", 0);
        let b = cdc_dedup_id("public", "orders", "42", 100, "insert", 0);
        assert_eq!(a, b);
        // Different lsn → different id (distinct updates are not collapsed).
        assert_ne!(a, cdc_dedup_id("public", "orders", "42", 101, "insert", 0));
        // Different key → different id.
        assert_ne!(a, cdc_dedup_id("public", "orders", "43", 100, "insert", 0));
        // Different operation on the same key/lsn → different id.
        assert_ne!(a, cdc_dedup_id("public", "orders", "42", 100, "update", 0));
        // Different in-transaction ordinal → different id (same key twice in one commit).
        assert_ne!(a, cdc_dedup_id("public", "orders", "42", 100, "insert", 1));
    }

    #[test]
    fn source_metadata_identifies_each_change_in_a_commit() {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        add_source_metadata(&mut message, "bridge_slot", 9_876_543_210, 1);

        assert_eq!(
            message
                .metadata
                .get("mqb.src.postgres_slot")
                .map(String::as_str),
            Some("bridge_slot")
        );
        assert_eq!(
            message
                .metadata
                .get("mqb.src.postgres_lsn")
                .map(String::as_str),
            Some("9876543210")
        );
        assert_eq!(
            message
                .metadata
                .get("mqb.src.postgres_ordinal")
                .map(String::as_str),
            Some("1")
        );
    }
}
