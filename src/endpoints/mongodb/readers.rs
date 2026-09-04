//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// A non-destructive, **one-shot** reader over an arbitrary MongoDB collection (`consume: snapshot`).
/// Pages by `_id` (`find({_id:{$gt:last}}).sort({_id:1})`), never mutates the source, and ends the
/// route once the collection is drained. At-least-once.
///
/// It deliberately does not resume across runs and does not provide a point-in-time snapshot.
/// Each `_id` page is a separate query, so concurrent inserts above the current high-water mark may
/// be included, inserts below it may be missed, and deletes may disappear before a later page reads
/// them. Carrying the cursor across runs would turn that visibility boundary into silent data loss,
/// which is why `cursor_id` is rejected at startup. Incremental reads need commit order, i.e. a
/// change stream (`capture_all`) on a replica set. The checkpoint plumbing below is kept for that
/// future, not reachable today.
pub struct MongoDbIdReader {
    collection: Collection<Document>,
    db: Database,
    checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>>,
    last_id: Arc<Mutex<Option<Bson>>>,
    receive_query: Option<Document>,
}

impl MongoDbIdReader {
    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB id-cursor reader"))?;
        let client = create_client(config).await?;
        let db = client.database(&config.database);
        let collection: Collection<Document> = db.collection(collection_name);

        let receive_query = if let Some(q) = &config.receive_query {
            let doc: Document = serde_json::from_str(q)
                .context("Failed to parse 'receive_query' from configuration as a JSON document")?;
            Some(doc)
        } else {
            None
        };

        if config.cursor_id.is_some() {
            return Err(anyhow!(
                "MongoDB 'snapshot' does not support 'cursor_id' (collection '{}'). Resuming above a \
                 stored `_id` skips anything a concurrent writer commits below it, which is silent \
                 data loss. Use 'capture_all' on a replica set to read incrementally.",
                collection_name
            ));
        }

        let checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>> = if let Some(cid) =
            &config.cursor_id
        {
            use crate::checkpoint::CheckpointBackend;
            let backend = match &config.checkpoint_store {
                // Absent: a dedicated per-source collection so the source is never written.
                None => CheckpointBackend::Source {
                    name: crate::checkpoint::default_meta_name(collection_name),
                },
                Some(spec) => crate::checkpoint::parse_checkpoint_store(spec)?,
            };
            let store: Arc<dyn crate::checkpoint::CheckpointStore> = match backend {
                CheckpointBackend::Source { name } => Arc::new(MongoCollectionCheckpointStore {
                    meta: db.collection::<Document>(&name),
                    doc_id: crate::checkpoint::checkpoint_key(collection_name, cid),
                }),
                external => {
                    crate::checkpoint::build_external_store(external, collection_name, cid).await?
                }
            };
            Some(store)
        } else {
            None
        };

        let last_id = match &checkpoint {
            Some(cp) => cp.load().await?.and_then(|s| {
                let decoded = decode_id(&s);
                if decoded.is_none() {
                    warn!(value = %s, "Ignoring unparseable mongo id cursor; starting from beginning");
                }
                decoded
            }),
            None => None,
        };
        info!(collection = %collection_name, "MongoDB snapshot reader initialized; reads the current contents once, then ends the route");

        Ok(Self {
            collection,
            db,
            checkpoint,
            last_id: Arc::new(Mutex::new(last_id)),
            receive_query,
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbIdReader {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        // `_id` before this batch, for rollback on nack (see the commit closure).
        let resume_from = self.last_id.lock().unwrap().clone();

        let mut messages = Vec::new();
        let mut ids: Vec<Bson> = Vec::new();

        // Page until we collect at least one message or a query returns no documents (truly
        // drained). This keeps an empty batch meaning "drained": a whole page of unreadable
        // docs is skipped-with-progress rather than stalling the reader or exiting early.
        loop {
            let last = self.last_id.lock().unwrap().clone();
            let mut filter = match &last {
                Some(v) => doc! { "_id": { "$gt": v.clone() } },
                None => doc! {},
            };
            if let Some(extra) = &self.receive_query {
                filter = if filter.is_empty() {
                    extra.clone()
                } else {
                    doc! { "$and": [filter, extra.clone()] }
                };
            }

            let find_options = FindOptions::builder()
                .sort(doc! { "_id": 1 })
                .limit(max_messages as i64)
                .build();

            let mut cursor = self
                .collection
                .find(filter)
                .with_options(find_options)
                .await
                .map_err(|e| ConsumerError::Connection(e.into()))?;

            let mut docs_in_page = 0usize;
            while let Some(result) = cursor.next().await {
                // A cursor error mid-page is a real failure; surface it instead of treating the
                // truncated page as "drained".
                let doc = result.map_err(|e| ConsumerError::Connection(e.into()))?;
                docs_in_page += 1;
                let Some(id) = doc.get("_id").cloned() else {
                    warn!("MongoDB document without an `_id`; skipping");
                    continue;
                };
                match parse_mongodb_document(doc) {
                    Ok(msg) => {
                        messages.push(msg);
                        ids.push(id.clone());
                    }
                    Err(e) => warn!(error = %e, "Skipping unparseable MongoDB document"),
                }
                // Advance past this `_id` whether or not it parsed, so a bad doc can't stall paging.
                *self.last_id.lock().unwrap() = Some(id);
            }

            // Got messages, or the collection is exhausted -> stop; otherwise the whole page was
            // skipped and more may follow, so page again.
            if !messages.is_empty() || docs_in_page == 0 {
                break;
            }
        }

        if messages.is_empty() {
            // Drained. End the route rather than returning an empty batch: polling on would turn a
            // snapshot into a tail, and an `_id` cursor cannot tail a collection being written.
            return Err(ConsumerError::EndOfStream);
        }

        let checkpoint = self.checkpoint.clone();
        let last_id = self.last_id.clone();
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Highest `_id` of a contiguous run of Acks from the front (stop at first Nack).
                let mut acked = 0usize;
                for disp in dispositions.iter().take(ids.len()) {
                    if matches!(disp, MessageDisposition::Ack | MessageDisposition::Reply(_)) {
                        acked += 1;
                    } else {
                        break;
                    }
                }
                let boundary: Option<Bson> = if acked == 0 {
                    resume_from
                } else {
                    Some(ids[acked - 1].clone())
                };
                // If any doc was not acked, roll the in-memory read cursor back to the
                // committed boundary so nacked/unprocessed docs are re-read on the next
                // page (at-least-once) instead of being skipped until a restart.
                if acked < ids.len() {
                    *last_id.lock().unwrap() = boundary.clone();
                }
                if let (Some(id), Some(cp)) = (boundary, checkpoint) {
                    match encode_id(&id) {
                        Some(s) => {
                            if let Err(e) = cp.save(&s).await {
                                tracing::warn!(error = %e, "Failed to persist mongo id cursor. Messages may be reprocessed on restart.");
                            }
                        }
                        None => tracing::warn!(
                            "Unsupported _id type for cursor persistence; not checkpointing"
                        ),
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let mut error = None;
        let healthy = match self.db.run_command(doc! { "ping": 1 }).await {
            Ok(_) => true,
            Err(e) => {
                error = Some(e.to_string());
                false
            }
        };
        let pending = if healthy {
            let last = self.last_id.lock().unwrap().clone();
            let filter = match &last {
                Some(v) => doc! { "_id": { "$gt": v.clone() } },
                None => doc! {},
            };
            match self.collection.count_documents(filter).await {
                Ok(c) => Some(c as usize),
                Err(e) => {
                    error = Some(format!("Failed to count pending: {}", e));
                    None
                }
            }
        } else {
            None
        };

        EndpointStatus {
            healthy,
            target: self.collection.name().to_string(),
            pending,
            capacity: None,
            details: serde_json::json!({ "mode": "snapshot" }),
            error,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Serializes a change-stream resume token to a canonical extended-JSON string for durable
/// checkpointing. Canonical extJSON preserves the token's BSON types (including any `_typeBits`
/// binary) so it round-trips exactly through [`decode_resume_token`].
pub(crate) fn encode_resume_token(token: &ResumeToken) -> anyhow::Result<String> {
    let doc = to_document(token).context("Failed to serialize resume token")?;
    let value = Bson::Document(doc).into_canonical_extjson();
    serde_json::to_string(&value).context("Failed to encode resume token")
}

/// Parses a resume token previously produced by [`encode_resume_token`]. Returns `None` on a
/// malformed value so the reader starts from the current stream position rather than failing.
pub(crate) fn decode_resume_token(s: &str) -> Option<ResumeToken> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    let bson = Bson::try_from(value).ok()?;
    mongodb::bson::from_bson::<ResumeToken>(bson).ok()
}

/// Opens a change stream on `collection` with an optional resume position, using `updateLookup`
/// so update/replace events carry the full post-image.
async fn open_change_stream(
    collection: &Collection<Document>,
    pipeline: &[Document],
    resume_after: Option<ResumeToken>,
) -> anyhow::Result<ChangeStream<ChangeStreamEvent<Document>>> {
    let mut watch = collection
        .watch()
        .pipeline(pipeline.to_vec())
        .full_document(FullDocumentType::UpdateLookup);
    if let Some(token) = resume_after {
        watch = watch.resume_after(token);
    }
    let name = collection.name().to_string();
    watch.await.map_err(|e| {
        // Preserve the source `mongodb::error::Error` (via `.context`, not stringified) so callers
        // can downcast it — `capture_all` only falls back to the `_id` reader for code 40573.
        anyhow::Error::new(e).context(format!("Failed to open MongoDB change stream for '{name}'"))
    })
}

/// True only for the MongoDB "change streams require a replica set" error (code 40573) — the one
/// case where `capture_all` may fall back to the insert-only `_id` reader. Auth, network, and
/// configuration failures return false so they propagate instead of being silently downgraded.
pub(crate) fn is_change_stream_unsupported(err: &anyhow::Error) -> bool {
    err.downcast_ref::<mongodb::error::Error>()
        .is_some_and(|e| matches!(&*e.kind, ErrorKind::Command(cmd) if cmd.code == 40573))
}

/// While idle (no matching changes), the CDC reader periodically advances its durable checkpoint to
/// the change stream's `postBatchResumeToken` so a long-idle stream's saved token can't age out of
/// the oplog window. This interval bounds how stale that saved position can get.
const IDLE_RESUME_REFRESH: Duration = Duration::from_secs(10);

/// A real change-data-capture reader over an arbitrary MongoDB collection. Tails the collection's
/// change stream (requires a replica set), emitting insert/update/replace/delete events with the
/// full post-image (`updateLookup`), and persists the resume token (keyed by `cursor_id`) to a
/// pluggable checkpoint store so a restart resumes exactly after the last acked change.
/// At-least-once. Backs the `capture_new`/`capture_all` modes; unlike the insert-only `_id` reader
/// it captures updates and deletes, not just appends.
pub struct MongoDbChangeStreamReader {
    collection: Collection<Document>,
    db: Database,
    collection_name: String,
    checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>>,
    cursor_id: Option<String>,
    receive_query: Option<Document>,
    pipeline: Vec<Document>,
    // Wrapped in a Mutex so the reader is `Sync` (a bare `ChangeStream` is `Send` but not `Sync`),
    // which the `MessageConsumer` trait's `&self` methods require. `None` while the initial
    // snapshot is draining; opened (at `pending_resume`) when the snapshot completes.
    stream: tokio::sync::Mutex<Option<ChangeStream<ChangeStreamEvent<Document>>>>,
    // Stream start position captured before the snapshot; the stream is opened here after it drains.
    pending_resume: Mutex<Option<ResumeToken>>,
    // Snapshot paging position (`_id > last`), shared with the commit closure for nack rollback.
    snapshot_last_id: Arc<Mutex<Option<Bson>>>,
    // Idle resume-token refresh state. `inflight` counts delivered-but-not-yet-committed batches;
    // `refresh_clean` is cleared for the session's remainder once a streaming batch is nacked (a
    // redelivery gap then exists). Idle refresh only persists the postBatchResumeToken when nothing
    // is in flight AND clean — so it can never advance past an un-acked change. `last_saved_token`
    // dedupes redundant writes when the token hasn't moved.
    inflight: Arc<AtomicUsize>,
    refresh_clean: Arc<AtomicBool>,
    last_saved_token: Arc<Mutex<Option<String>>>,
    /// Resolved once at construction from endpoint config and the legacy fallback.
    source_metadata: bool,
    /// `<database>.<collection>`, precomputed for the `mqb.src.mongodb_namespace` key.
    namespace: String,
    /// Contiguous positions an idempotent sink needs. Kept apart so the two phases cannot
    /// share a sequence: snapshot documents are numbered within the scan, changes within
    /// their cluster time.
    snapshot_ordinals: Arc<Mutex<OrdinalCounter>>,
    cdc_ordinals: Arc<Mutex<OrdinalCounter>>,
    exit_on_empty: bool,
}

/// Hands out consecutive ordinals within a group, restarting at 0 when the group changes.
#[derive(Default)]
pub(super) struct OrdinalCounter {
    group: Option<u64>,
    next: u64,
}

impl OrdinalCounter {
    fn next_in(&mut self, group: u64) -> u64 {
        if self.group != Some(group) {
            self.group = Some(group);
            self.next = 0;
        }
        let ordinal = self.next;
        self.next += 1;
        ordinal
    }
}

impl MongoDbChangeStreamReader {
    /// `snapshot` = read the existing documents before streaming changes (`capture_all`); when false
    /// only new changes are streamed (`capture_new`).
    pub async fn new(config: &MongoDbConfig, snapshot: bool) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, snapshot, false).await
    }

    pub async fn new_with_source_metadata(
        config: &MongoDbConfig,
        snapshot: bool,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        Self::new_with_source_metadata_and_no_resume(config, snapshot, source_metadata, false).await
    }

    pub(crate) async fn new_with_source_metadata_and_no_resume(
        config: &MongoDbConfig,
        snapshot: bool,
        source_metadata: bool,
        no_resume: bool,
    ) -> anyhow::Result<Self> {
        let source_metadata = crate::canonical_message::source_metadata_enabled_for_endpoint(
            source_metadata || config.source_metadata,
        );
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB CDC reader"))?;
        let client = create_client(config).await?;
        let db = client.database(&config.database);
        let collection: Collection<Document> = db.collection(collection_name);

        // Optional filter: a `$match` stage on the change stream, and the equivalent `find` filter
        // for the snapshot phase.
        let receive_query = if let Some(q) = &config.receive_query {
            let doc: Document = serde_json::from_str(q)
                .context("Failed to parse 'receive_query' from configuration as a JSON document")?;
            Some(doc)
        } else {
            None
        };
        // A change stream sees event *envelopes*, not raw documents, so a `receive_query` on
        // document fields must target the `fullDocument` namespace or it would match nothing and
        // silently drop every event. The snapshot phase keeps the raw predicate (it queries the
        // collection directly). Note: delete events carry no `fullDocument`, so document-field
        // filters exclude deletes.
        let pipeline: Vec<Document> = receive_query
            .as_ref()
            .map(|q| vec![doc! { "$match": full_document_match(q) }])
            .unwrap_or_default();

        let checkpoint: Option<Arc<dyn crate::checkpoint::CheckpointStore>> = if no_resume {
            None
        } else if let Some(cid) = &config.cursor_id {
            use crate::checkpoint::CheckpointBackend;
            let backend = match &config.checkpoint_store {
                None => CheckpointBackend::Source {
                    name: crate::checkpoint::default_meta_name(collection_name),
                },
                Some(spec) => crate::checkpoint::parse_checkpoint_store(spec)?,
            };
            let store: Arc<dyn crate::checkpoint::CheckpointStore> = match backend {
                CheckpointBackend::Source { name } => Arc::new(MongoCollectionCheckpointStore {
                    meta: db.collection::<Document>(&name),
                    doc_id: crate::checkpoint::checkpoint_key(collection_name, cid),
                }),
                external => {
                    crate::checkpoint::build_external_store(external, collection_name, cid).await?
                }
            };
            Some(store)
        } else {
            warn!(
                collection = %collection_name,
                "MongoDB CDC reader has no cursor_id; resume is disabled and every restart starts from the current stream position. Set cursor_id to persist progress."
            );
            None
        };

        let resume_token = match &checkpoint {
            Some(cp) => cp.load().await?.and_then(|s| {
                let decoded = decode_resume_token(&s);
                if decoded.is_none() {
                    warn!(value = %s, "Ignoring unparseable mongo resume token; starting from current stream position");
                }
                decoded
            }),
            None => None,
        };

        // Cold start with `capture_all`: capture the current stream position, then snapshot the
        // existing documents before streaming from that position (no gap; at-least-once). The
        // stream is opened later, when the snapshot drains, so no change-stream cursor is held open
        // during a potentially long snapshot.
        let take_snapshot = resume_token.is_none() && snapshot;
        let (stream, pending_resume) = if take_snapshot {
            let probe = open_change_stream(&collection, &pipeline, None).await?;
            match probe.resume_token() {
                Some(token) => {
                    info!(collection = %collection_name, "MongoDB CDC reader starting initial snapshot");
                    (None, Some(token))
                }
                None => {
                    warn!(collection = %collection_name, "Server did not provide a resume token; skipping snapshot and streaming new changes only");
                    (Some(probe), None)
                }
            }
        } else {
            (
                Some(open_change_stream(&collection, &pipeline, resume_token.clone()).await?),
                None,
            )
        };

        info!(collection = %collection_name, cursor_id = ?config.cursor_id, resumed = %resume_token.is_some(), snapshot = %pending_resume.is_some(), "MongoDB CDC reader initialized");

        Ok(Self {
            collection,
            db,
            collection_name: collection_name.to_string(),
            checkpoint,
            cursor_id: config.cursor_id.clone(),
            receive_query,
            pipeline,
            stream: tokio::sync::Mutex::new(stream),
            pending_resume: Mutex::new(pending_resume),
            snapshot_last_id: Arc::new(Mutex::new(None)),
            inflight: Arc::new(AtomicUsize::new(0)),
            refresh_clean: Arc::new(AtomicBool::new(true)),
            last_saved_token: Arc::new(Mutex::new(
                resume_token
                    .as_ref()
                    .and_then(|t| encode_resume_token(t).ok()),
            )),
            source_metadata,
            namespace: format!("{}.{}", config.database, collection_name),
            snapshot_ordinals: Arc::new(Mutex::new(OrdinalCounter::default())),
            cdc_ordinals: Arc::new(Mutex::new(OrdinalCounter::default())),
            exit_on_empty: false,
        })
    }

    /// Pages the initial snapshot by `_id` (like the resumable reader), returning `None` once the
    /// collection is exhausted so the caller can hand off to the change stream.
    async fn snapshot_batch(
        &self,
        max_messages: usize,
    ) -> Result<Option<ReceivedBatch>, ConsumerError> {
        let resume_from = self.snapshot_last_id.lock().unwrap().clone();
        let last = resume_from.clone();
        // Never snapshot the bridge's own sequencer bookkeeping doc (see `available_message_filter`).
        let mut filter = match &last {
            Some(v) => doc! { "_id": { "$gt": v.clone() }, "seq_counter": { "$exists": false } },
            None => doc! { "seq_counter": { "$exists": false } },
        };
        if let Some(extra) = &self.receive_query {
            filter = doc! { "$and": [filter, extra.clone()] };
        }
        let find_options = FindOptions::builder()
            .sort(doc! { "_id": 1 })
            .limit(max_messages as i64)
            .build();
        let mut cursor = self
            .collection
            .find(filter)
            .with_options(find_options)
            .await
            .map_err(|e| ConsumerError::Connection(e.into()))?;

        let mut messages = Vec::new();
        let mut ids: Vec<Bson> = Vec::new();
        while let Some(result) = cursor.next().await {
            let doc = result.map_err(|e| ConsumerError::Connection(e.into()))?;
            let Some(id) = doc.get("_id").cloned() else {
                warn!("MongoDB snapshot document without an `_id`; skipping");
                continue;
            };
            match serde_json::to_vec(&doc) {
                Ok(payload) => {
                    let mut msg = CanonicalMessage::new(payload, None);
                    msg.metadata
                        .insert("mongodb.operation".to_string(), "insert".to_string());
                    msg.metadata
                        .insert("mongodb.snapshot".to_string(), "true".to_string());
                    if let Some(enc) = encode_id(&id) {
                        msg.metadata.insert("mongodb.document_id".to_string(), enc);
                    }
                    if self.source_metadata {
                        add_snapshot_source_metadata(
                            &mut msg,
                            &self.namespace,
                            &id,
                            &self.snapshot_ordinals,
                        );
                    }
                    messages.push(msg);
                    ids.push(id.clone());
                }
                Err(e) => warn!(error = %e, "Skipping unserializable MongoDB snapshot document"),
            }
            *self.snapshot_last_id.lock().unwrap() = Some(id);
        }

        // Exhausted: no more snapshot documents. The caller hands off to the change stream.
        if messages.is_empty() {
            return Ok(None);
        }

        let last_id = self.snapshot_last_id.clone();
        // Gate idle refresh: an un-acked snapshot batch still in flight when streaming begins must
        // block the postBatchResumeToken from being persisted, or its docs would be lost on restart.
        let inflight = self.inflight.clone();
        let refresh_clean = self.refresh_clean.clone();
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                let mut acked = 0usize;
                for disp in dispositions.iter().take(ids.len()) {
                    if matches!(disp, MessageDisposition::Ack | MessageDisposition::Reply(_)) {
                        acked += 1;
                    } else {
                        break;
                    }
                }
                // Roll the snapshot cursor back to the last acked `_id` so nacked docs are re-read.
                if acked < ids.len() {
                    let boundary = if acked == 0 {
                        resume_from
                    } else {
                        Some(ids[acked - 1].clone())
                    };
                    *last_id.lock().unwrap() = boundary;
                    // Latch the gap: once the stream opens, snapshot docs can only be recovered by
                    // re-snapshotting from the start, so no resume token may be persisted this
                    // session. Blocks both idle refresh and later streaming-batch commits.
                    refresh_clean.store(false, Ordering::Release);
                }
                inflight.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        self.inflight.fetch_add(1, Ordering::AcqRel);
        Ok(Some(ReceivedBatch { messages, commit }))
    }

    /// Maps a change event into a canonical message, tagging the operation and document `_id`.
    /// Returns `None` for events carrying no usable payload (e.g. an update whose post-image was
    /// already deleted by the time of the lookup).
    pub(super) fn event_to_message(
        event: &ChangeStreamEvent<Document>,
        source_metadata: bool,
        namespace: &str,
        ordinals: &Mutex<OrdinalCounter>,
    ) -> Option<CanonicalMessage> {
        let (op, payload) = match event.operation_type {
            OperationType::Insert | OperationType::Update | OperationType::Replace => {
                let doc = event.full_document.as_ref()?;
                // Skip the bridge's own sequencer bookkeeping doc (its `$inc` updates and insert).
                if doc.contains_key("seq_counter") {
                    return None;
                }
                (op_str(&event.operation_type), serde_json::to_vec(doc).ok()?)
            }
            OperationType::Delete => {
                // No post-image on delete; carry the document key so the sink can act on the `_id`.
                let key = event.document_key.clone().unwrap_or_default();
                ("delete", serde_json::to_vec(&key).ok()?)
            }
            _ => return None, // drop/rename/invalidate/other: not row-level data changes
        };

        let mut msg = CanonicalMessage::new(payload, None);
        msg.metadata
            .insert("mongodb.operation".to_string(), op.to_string());
        if let Some(id) = event.document_key.as_ref().and_then(|k| k.get("_id")) {
            if let Some(enc) = encode_id(id) {
                msg.metadata.insert("mongodb.document_id".to_string(), enc);
            }
        }
        if source_metadata {
            add_source_metadata(&mut msg, event, namespace, ordinals);
        }
        Some(msg)
    }

    /// Called while the stream is idle: persist the change stream's postBatchResumeToken so the
    /// durable checkpoint tracks the oplog even with no matching changes. Only advances when no
    /// batch is in flight (`inflight == 0`) and no un-acked gap exists (`refresh_clean`), so the
    /// persisted token is always a safe resume point that can't skip a delivered-but-un-acked
    /// change. During idle there are no matching changes, so the token only moves past irrelevant
    /// oplog entries — nothing is lost.
    /// `token` is the stream's postBatchResumeToken, extracted by the caller *before* any await (a
    /// shared `&ChangeStream` is not `Send`, so it can't be held across the checkpoint write).
    async fn refresh_idle_checkpoint(&self, token: Option<ResumeToken>) {
        let Some(cp) = &self.checkpoint else { return };
        if !self.refresh_clean.load(Ordering::Acquire) {
            return;
        }
        if self.inflight.load(Ordering::Acquire) != 0 {
            return;
        }
        let Some(token) = token else {
            return;
        };
        let encoded = match encode_resume_token(&token) {
            Ok(s) => s,
            Err(_) => return,
        };
        // Skip the write if the position hasn't moved since the last persist.
        if self.last_saved_token.lock().unwrap().as_deref() == Some(encoded.as_str()) {
            return;
        }
        if let Err(e) = cp.save(&encoded).await {
            tracing::warn!(error = %e, "Failed to persist idle mongo resume token");
            return;
        }
        *self.last_saved_token.lock().unwrap() = Some(encoded);
    }
}

/// Rewrite a document-field filter so it targets a change event's `fullDocument` namespace.
/// Field keys are prefixed with `fullDocument.`; top-level logical operators (`$and`/`$or`/`$nor`/
/// `$not`) are preserved and their nested sub-filters rewritten recursively. Field-level operators
/// (`$gt`, `$in`, …) inside a value are left untouched. Delete events have no `fullDocument`, so
/// such filters naturally exclude them.
pub(crate) fn full_document_match(query: &Document) -> Document {
    let mut out = Document::new();
    for (key, value) in query {
        if key.starts_with('$') {
            out.insert(key.clone(), rewrite_operator_value(value));
        } else {
            out.insert(format!("fullDocument.{key}"), value.clone());
        }
    }
    out
}

/// Recurse into the value of a logical operator: `$and`/`$or`/`$nor` take an array of sub-filters,
/// `$not` a single one. Nested document-field predicates are rewritten; everything else is copied.
fn rewrite_operator_value(value: &Bson) -> Bson {
    match value {
        Bson::Array(items) => Bson::Array(
            items
                .iter()
                .map(|item| match item {
                    Bson::Document(d) => Bson::Document(full_document_match(d)),
                    other => other.clone(),
                })
                .collect(),
        ),
        Bson::Document(d) => Bson::Document(full_document_match(d)),
        other => other.clone(),
    }
}

/// Tags a change event with its position in the oplog.
///
/// The resume token is the authoritative one — it is what the reader checkpoints and what a
/// restart resumes after. `cluster_time` is packed into the same `(seconds << 32) | increment`
/// u64 the server orders the oplog by, so it sorts correctly and is comparable across events;
/// note that every change in one transaction shares a cluster time, so it identifies a group
/// rather than a single change.
fn add_source_metadata(
    message: &mut CanonicalMessage,
    event: &ChangeStreamEvent<Document>,
    namespace: &str,
    ordinals: &Mutex<OrdinalCounter>,
) {
    if let Ok(token) = encode_resume_token(&event.id) {
        message
            .metadata
            .insert("mqb.src.mongodb_resume_token".to_string(), token);
    }
    let Some(ts) = event.cluster_time else {
        // Without a cluster time the event has no orderable position. The namespace is left
        // off as well: on its own it would select the sink's CDC path, which then fails the
        // whole batch on the missing cluster time.
        warn!(
            "MongoDB change event has no clusterTime; omitting mqb.src.mongodb_* source position"
        );
        return;
    };
    message.metadata.insert(
        "mqb.src.mongodb_namespace".to_string(),
        namespace.to_string(),
    );
    let packed = (u64::from(ts.time) << 32) | u64::from(ts.increment);
    message.metadata.insert(
        "mqb.src.mongodb_cluster_time".to_string(),
        packed.to_string(),
    );
    // Every change in a transaction shares a cluster time; the ordinal separates them
    // so an idempotent sink can name them as one contiguous range.
    let ordinal = ordinals
        .lock()
        .expect("ordinal counter poisoned")
        .next_in(packed);
    message
        .metadata
        .insert("mqb.src.mongodb_ordinal".to_string(), ordinal.to_string());
}

/// Tags a snapshot document with the stable identity used when it is redelivered.
pub(super) fn add_snapshot_source_metadata(
    message: &mut CanonicalMessage,
    namespace: &str,
    id: &Bson,
    ordinals: &Mutex<OrdinalCounter>,
) {
    message.metadata.insert(
        "mqb.src.mongodb_namespace".to_string(),
        namespace.to_string(),
    );
    if let Ok(document_id) = serde_json::to_string(id) {
        message
            .metadata
            .insert("mqb.src.mongodb_document_id".to_string(), document_id);
    }
    // The scan is ordered by ascending `_id`, so this index is deterministic: a restart
    // re-scans from the start and reproduces the same names, which an idempotent sink skips.
    // Group 0 — the whole snapshot is one sequence.
    let index = ordinals
        .lock()
        .expect("ordinal counter poisoned")
        .next_in(0);
    message.metadata.insert(
        "mqb.src.mongodb_snapshot_index".to_string(),
        index.to_string(),
    );
}

/// The change-event operation name stored in message metadata.
fn op_str(op: &OperationType) -> &'static str {
    match op {
        OperationType::Insert => "insert",
        OperationType::Update => "update",
        OperationType::Replace => "replace",
        OperationType::Delete => "delete",
        _ => "other",
    }
}

#[async_trait]
impl MessageConsumer for MongoDbChangeStreamReader {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        let mut stream_guard = self.stream.lock().await;
        // Snapshot phase (opt-in cold start): drain existing documents, then open the stream at the
        // pre-snapshot position and fall through to streaming.
        if stream_guard.is_none() {
            if let Some(batch) = self.snapshot_batch(max_messages).await? {
                return Ok(batch);
            }
            let token = self.pending_resume.lock().unwrap().take();
            let opened = open_change_stream(&self.collection, &self.pipeline, token)
                .await
                .map_err(ConsumerError::Connection)?;
            info!(collection = %self.collection_name, "MongoDB CDC snapshot complete; streaming changes");
            *stream_guard = Some(opened);
        }
        let stream = stream_guard.as_mut().expect("stream opened above");

        let mut messages = Vec::new();
        // Per-message resume token: resuming `after` the last acked event's token gives
        // at-least-once (un-acked events are re-delivered on restart).
        let mut tokens: Vec<ResumeToken> = Vec::new();

        // Block for the first change (the route cancels this future on shutdown), then coalesce any
        // immediately-available events into the batch with a short timeout. While idle, periodically
        // advance the durable checkpoint to the stream's postBatchResumeToken so it can't age out of
        // the oplog (guarded so it never skips an un-acked change).
        //
        // Polls with `next_if_any` rather than `StreamExt::next`: a `next()` future cancelled by the
        // timeout leaves the driver's stream state non-`Idle`, and `resume_token()` panics on any
        // other state. `next_if_any` borrows the stream instead of replacing that state, so the
        // token stays readable when the timeout fires mid-poll.
        let mut last_refresh = Instant::now();
        let drain_deadline = self
            .exit_on_empty
            .then(|| Instant::now() + crate::traits::drain_idle_timeout());
        loop {
            let poll = if let Some(deadline) = drain_deadline {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    stream.next_if_any(),
                )
                .await
            } else {
                tokio::time::timeout(IDLE_RESUME_REFRESH, stream.next_if_any()).await
            };
            match poll {
                Ok(Ok(Some(event))) => {
                    let token = event.id.clone();
                    if let Some(msg) = Self::event_to_message(
                        &event,
                        self.source_metadata,
                        &self.namespace,
                        &self.cdc_ordinals,
                    ) {
                        messages.push(msg);
                        tokens.push(token);
                    }
                    if !messages.is_empty() {
                        break;
                    }
                    // Event carried no payload (e.g. a drop); keep waiting for a real change.
                }
                Ok(Err(e)) => return Err(ConsumerError::Connection(e.into())),
                // No change ready: the getMore came back empty, or it outran the refresh interval.
                Ok(Ok(None)) => {
                    if !stream.is_alive() {
                        return Err(anyhow!("MongoDB change stream ended unexpectedly").into());
                    }
                    if last_refresh.elapsed() >= IDLE_RESUME_REFRESH {
                        // Extract the token synchronously (stream ref isn't `Send`), then persist.
                        let token = stream.resume_token();
                        self.refresh_idle_checkpoint(token).await;
                        last_refresh = Instant::now();
                    }
                }
                Err(_) if self.exit_on_empty => return Ok(ReceivedBatch::empty()),
                Err(_) => {
                    let token = stream.resume_token();
                    self.refresh_idle_checkpoint(token).await;
                    last_refresh = Instant::now();
                }
            }
        }

        while messages.len() < max_messages {
            match tokio::time::timeout(Duration::from_millis(10), stream.next_if_any()).await {
                Ok(Ok(Some(event))) => {
                    let token = event.id.clone();
                    if let Some(msg) = Self::event_to_message(
                        &event,
                        self.source_metadata,
                        &self.namespace,
                        &self.cdc_ordinals,
                    ) {
                        messages.push(msg);
                        tokens.push(token);
                    }
                }
                Ok(Err(e)) => return Err(ConsumerError::Connection(e.into())),
                Ok(Ok(None)) | Err(_) => break, // no more events ready right now
            }
        }

        trace!(count = messages.len(), collection = %self.collection_name, "Received batch of MongoDB change events");

        let checkpoint = self.checkpoint.clone();
        let inflight = self.inflight.clone();
        let refresh_clean = self.refresh_clean.clone();
        let last_saved_token = self.last_saved_token.clone();
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Resume token of the last contiguous Ack from the front (stop at first Nack).
                let mut acked = 0usize;
                for disp in dispositions.iter().take(tokens.len()) {
                    if matches!(disp, MessageDisposition::Ack | MessageDisposition::Reply(_)) {
                        acked += 1;
                    } else {
                        break;
                    }
                }
                // An earlier batch (a snapshot batch, or a prior streaming batch) left an un-acked
                // redelivery gap and latched `refresh_clean` off. Commits run in delivery order
                // (ordered sequencer), so a token from this batch would sit past that gap: do not
                // persist it even when this batch is itself fully acked.
                let prior_gap = !refresh_clean.load(Ordering::Acquire);
                if acked > 0 && !prior_gap {
                    if let Some(cp) = checkpoint {
                        match encode_resume_token(&tokens[acked - 1]) {
                            Ok(s) => {
                                if let Err(e) = cp.save(&s).await {
                                    tracing::warn!(error = %e, "Failed to persist mongo resume token. Changes may be reprocessed on restart.");
                                } else {
                                    *last_saved_token.lock().unwrap() = Some(s);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to encode mongo resume token; not checkpointing")
                            }
                        }
                    }
                }
                // This batch's own nack opens a gap (checkpoint deliberately behind delivered
                // events); latch idle refresh and all later commits off for the session so nothing
                // can skip past it.
                if acked < tokens.len() {
                    refresh_clean.store(false, Ordering::Release);
                }
                inflight.fetch_sub(1, Ordering::AcqRel);
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        self.inflight.fetch_add(1, Ordering::AcqRel);
        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.db.run_command(doc! { "ping": 1 }).await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        // "snapshot" until the initial snapshot drains and the change stream opens, then "streaming".
        let phase = match self.stream.try_lock() {
            Ok(g) if g.is_none() => "snapshot",
            Ok(_) => "streaming",
            Err(_) => "streaming", // stream in use by receive_batch → past the snapshot phase
        };
        let resume_token = self.last_saved_token.lock().unwrap().clone();
        EndpointStatus {
            healthy,
            target: self.collection_name.clone(),
            error,
            details: serde_json::json!({
                "cursor_id": self.cursor_id,
                "mode": "cdc",
                "phase": phase,
                "in_flight_batches": self.inflight.load(Ordering::Acquire),
                "resume_token": resume_token,
            }),
            ..Default::default()
        }
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
