//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

pub struct MongoDbPublisher {
    collection: Collection<Document>,
    meta_collection: Collection<Document>,
    db: Database,
    // Retains the shared registry entry so concurrent publishers reuse this client/pool.
    _shared_client: std::sync::Arc<Client>,
    collection_name: String,
    request_reply: bool,
    request_timeout: Duration,
    reply_polling_interval: Duration,
    format: MongoDbFormat,
    id_field: Option<String>,
    id_template: Option<CompiledTemplate>,
    report_outcome: bool,
}

/// Metadata key carrying the insert outcome when `report_outcome` is enabled.
pub(crate) const OUTCOME_KEY: &str = "mongodb.outcome";
pub(crate) const OUTCOME_INSERTED: &str = "inserted";
pub(crate) const OUTCOME_EXISTED: &str = "existed";

fn mongodb_uses_sequencer(request_reply: bool, format: &MongoDbFormat) -> bool {
    !request_reply && !matches!(format, MongoDbFormat::Raw)
}

pub(crate) fn namespaced_sequencer_id(collection_name: &str) -> String {
    format!("{}:sequencer", collection_name)
}

impl MongoDbPublisher {
    fn uses_sequencer(&self) -> bool {
        mongodb_uses_sequencer(self.request_reply, &self.format)
    }

    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let id_template = config
            .id_field
            .as_deref()
            .filter(|value| value.contains("${"))
            .map(|template| {
                let compiled = CompiledTemplate::compile(template, None)
                    .context("invalid MongoDB `id_field` template")?;
                if !compiled.is_dynamic() || !compiled.has_only_replay_stable_tokens() {
                    anyhow::bail!(
                        "MongoDB `id_field` template must contain only replay-stable payload or metadata tokens"
                    );
                }
                Ok(compiled)
            })
            .transpose()?;
        let id_field = config
            .id_field
            .as_ref()
            .filter(|value| !value.contains("${"))
            .cloned();
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB publisher"))?;
        let shared_client = create_shared_client(config).await?;
        let client = (*shared_client).clone();
        let db = client.database(&config.database);

        if let Some(capped_size) = config.capped_size_bytes {
            let collections = db
                .list_collection_names()
                .filter(doc! { "name": collection_name })
                .await?;
            if collections.is_empty() {
                info!(collection = %collection_name, size = %capped_size, "Creating capped collection");
                db.create_collection(collection_name)
                    .capped(true)
                    .size(capped_size as u64)
                    .await?;
            }
        }

        let collection = db.collection(collection_name);
        let meta_collection_name = config
            .meta_collection
            .clone()
            .unwrap_or_else(|| collection_name.to_string());
        let meta_collection = db.collection(&meta_collection_name);

        if mongodb_uses_sequencer(config.request_reply, &config.format) {
            // Ensure unique index on seq. The sequencer doc has 'seq_counter', so it won't conflict.
            let index_options = mongodb::options::IndexOptions::builder()
                .unique(true)
                .sparse(true) // Only index documents that have the seq field
                .build();
            let index_model = IndexModel::builder()
                .keys(doc! { "seq": 1 })
                .options(index_options)
                .build();
            if let Err(e) = collection.create_index(index_model).await {
                warn!(
                    "Failed to create seq index on collection {}: {}",
                    collection_name, e
                );
            }
        }
        info!(database = %config.database, collection = %collection_name, request_reply = %config.request_reply, "MongoDB publisher connected");

        if let Some(ttl) = config.ttl_seconds {
            let options = mongodb::options::IndexOptions::builder()
                .expire_after(Duration::from_secs(ttl))
                .build();
            let model = IndexModel::builder()
                .keys(doc! { "created_at": 1 })
                .options(options)
                .build();
            if let Err(e) = collection.create_index(model).await {
                warn!(
                    "Failed to create TTL index on publisher collection {} : {}",
                    collection_name, e
                );
            }
        }

        if config.request_reply {
            let reply_collection_name = format!("{}_replies", collection_name);
            let reply_collection = db.collection::<Document>(&reply_collection_name);
            let index_model = IndexModel::builder()
                .keys(doc! { "metadata.correlation_id": 1 })
                .build();
            if let Err(e) = reply_collection.create_index(index_model).await {
                warn!(
                    "Failed to create correlation_id index on reply collection {} : {}",
                    reply_collection_name, e
                );
            }
            // Also apply TTL to the reply collection if configured, to clean up unconsumed replies.
            if let Some(ttl) = config.ttl_seconds {
                let options = mongodb::options::IndexOptions::builder()
                    .expire_after(Duration::from_secs(ttl))
                    .build();
                let model = IndexModel::builder()
                    .keys(doc! { "created_at": 1 })
                    .options(options)
                    .build();
                if let Err(e) = reply_collection.create_index(model).await {
                    warn!(
                        "Failed to create TTL index on reply collection {} : {}",
                        reply_collection_name, e
                    );
                }
            }
        }
        Ok(Self {
            collection,
            meta_collection,
            db,
            _shared_client: shared_client,
            collection_name: collection_name.to_string(),
            request_reply: config.request_reply,
            request_timeout: Duration::from_millis(config.request_timeout_ms.unwrap_or(30000)),
            reply_polling_interval: Duration::from_millis(config.reply_polling_ms.unwrap_or(50)),
            format: config.format.clone(),
            id_field,
            id_template,
            report_outcome: config.report_outcome,
        })
    }

    async fn recover_correlation_id_from_duplicate(
        &self,
        message: &mut CanonicalMessage,
    ) -> Result<(), PublisherError> {
        // Look up by the same `_id` message_to_document wrote: the id_field value when
        // configured, else the message_id UUID. Otherwise an explicit-id duplicate is
        // never found and the request retries forever.
        let id_bson =
            match explicit_id_bson(message, self.id_field.as_deref(), self.id_template.as_ref())
                .map_err(PublisherError::NonRetryable)?
            {
                Some(id) => id,
                None => Bson::from(mongodb::bson::Uuid::from_bytes(
                    message.message_id.to_be_bytes(),
                )),
            };
        let filter = doc! { "_id": id_bson };
        match self.collection.find_one(filter).await {
            Ok(Some(existing_doc)) => {
                let existing_msg = parse_mongodb_document(existing_doc).map_err(|e| {
                    PublisherError::NonRetryable(anyhow::anyhow!(
                        "Failed to parse existing document: {}",
                        e
                    ))
                })?;

                if let Some(cid) = existing_msg.metadata.get("correlation_id") {
                    message
                        .metadata
                        .insert("correlation_id".to_string(), cid.clone());
                }
                if let Some(rt) = existing_msg.metadata.get("reply_to") {
                    message.metadata.insert("reply_to".to_string(), rt.clone());
                }
                Ok(())
            }
            Ok(None) => Err(PublisherError::Retryable(anyhow::anyhow!(
                "Duplicate key error but document not found"
            ))),
            Err(e) => Err(PublisherError::Retryable(anyhow::anyhow!(
                "Failed to fetch existing document: {}",
                e
            ))),
        }
    }

    fn outcome_or_ack(&self, message: CanonicalMessage, outcome: &str) -> Sent {
        tag_outcome(self.report_outcome, message, outcome)
    }
}

/// With `report_outcome`, tag the message with `mongodb.outcome` and return it as a
/// `Sent::Response` so a downstream `switch` can branch; otherwise a plain `Ack`.
pub(crate) fn tag_outcome(
    report_outcome: bool,
    mut message: CanonicalMessage,
    outcome: &str,
) -> Sent {
    if report_outcome {
        message
            .metadata
            .insert(OUTCOME_KEY.to_string(), outcome.to_string());
        Sent::Response(message)
    } else {
        Sent::Ack
    }
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if !self.request_reply {
            trace!(message_id = %format!("{:032x}", message.message_id), collection = %self.collection_name, uses_sequencer = self.uses_sequencer(), "Publishing document to MongoDB");
            let mut doc = message_to_document(
                &message,
                &self.format,
                self.id_field.as_deref(),
                self.id_template.as_ref(),
            )
            .map_err(PublisherError::NonRetryable)?;

            if self.uses_sequencer() {
                // Atomically increment a sequence counter. This is safe without a transaction for just getting a sequence number.
                // If the subsequent insert fails, a sequence number might be "lost", creating a gap.
                let filter = doc! {
                    "_id": namespaced_sequencer_id(&self.collection_name)
                };
                let update = doc! { "$inc": { "seq_counter": 1_i64 } };
                let options = FindOneAndUpdateOptions::builder()
                    .upsert(true)
                    .return_document(ReturnDocument::After)
                    .build();

                let counter_doc = self
                    .meta_collection
                    .find_one_and_update(filter, update)
                    .with_options(options)
                    .await
                    .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
                let seq = counter_doc
                    .ok_or_else(|| {
                        PublisherError::Retryable(anyhow!(
                            "Sequencer document not returned after upsert"
                        ))
                    })?
                    .get_i64("seq_counter")
                    .map_err(|e| {
                        PublisherError::Retryable(anyhow!(
                            "Invalid seq_counter in sequencer: {}",
                            e
                        ))
                    })?;
                doc.insert("seq", seq);
            }

            match self.collection.insert_one(doc).await {
                Ok(_) => {}
                Err(e) => {
                    if let ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref w)) =
                        *e.kind
                    {
                        if w.code == 11000 {
                            warn!(message_id = %format!("{:032x}", message.message_id), "Duplicate key error inserting into MongoDB. Treating as idempotent success.");
                            return Ok(self.outcome_or_ack(message, OUTCOME_EXISTED));
                        }
                    }
                    return Err(PublisherError::Retryable(
                        anyhow::anyhow!(e).context("Failed to insert document into MongoDB"),
                    ));
                }
            }

            return Ok(self.outcome_or_ack(message, OUTCOME_INSERTED));
        }

        // --- Request-Reply Logic ---
        let mut correlation_id = if let Some(cid) = message.metadata.get("correlation_id") {
            cid.clone()
        } else {
            fast_uuid_v7::gen_id_string()
        };
        // Convention: reply collection is named <request_collection>_replies
        let reply_collection_name = format!("{}_replies", self.collection_name);

        message
            .metadata
            .insert("correlation_id".to_string(), correlation_id.clone());
        message
            .metadata
            .insert("reply_to".to_string(), reply_collection_name.clone());

        trace!(message_id = %format!("{:032x}", message.message_id), correlation_id = %correlation_id, collection = %self.collection_name, "Publishing request document to MongoDB");
        let doc = message_to_document(
            &message,
            &self.format,
            self.id_field.as_deref(),
            self.id_template.as_ref(),
        )
        .map_err(PublisherError::NonRetryable)?;
        match self.collection.insert_one(doc).await {
            Ok(_) => {}
            Err(e) => {
                let is_duplicate = matches!(&*e.kind, ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) if w.code == 11000);
                if is_duplicate {
                    warn!(message_id = %format!("{:032x}", message.message_id), "Duplicate key error inserting request into MongoDB. Treating as idempotent success.");
                    self.recover_correlation_id_from_duplicate(&mut message)
                        .await?;
                    if let Some(cid) = message.metadata.get("correlation_id") {
                        correlation_id = cid.clone();
                    }
                } else {
                    return Err(PublisherError::Retryable(
                        anyhow::anyhow!(e)
                            .context("Failed to insert request document into MongoDB"),
                    ));
                }
            }
        }

        // Now, wait for the response by polling the reply collection.
        let reply_collection = self.db.collection::<Document>(&reply_collection_name);
        let filter = doc! { "metadata.correlation_id": correlation_id.clone() };

        let timeout = self.request_timeout;
        let start = Instant::now();
        let mut current_sleep = self.reply_polling_interval;

        loop {
            if start.elapsed() > timeout {
                return Err(PublisherError::NonRetryable(anyhow!(
                    "Request timed out waiting for MongoDB response"
                )));
            }

            match reply_collection.find_one_and_delete(filter.clone()).await {
                Ok(Some(doc)) => {
                    trace!(correlation_id = %correlation_id, "Received MongoDB response");
                    let response_msg = parse_mongodb_document(doc).map_err(|e| {
                        PublisherError::NonRetryable(anyhow!("Failed to parse response: {}", e))
                    })?;
                    return Ok(Sent::Response(response_msg));
                }
                Ok(None) => {
                    tokio::time::sleep(current_sleep).await;
                    current_sleep = std::cmp::min(
                        current_sleep + current_sleep / 2,
                        Duration::from_millis(500),
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Error polling for MongoDB reply. Retrying...");
                    tokio::time::sleep(current_sleep).await;
                }
            }
        }
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        if self.request_reply || self.report_outcome {
            // report_outcome needs a per-message Response, so fan out through single send.
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Publishing batch of documents to MongoDB");
        let mut docs = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();
        let mut valid_messages = Vec::with_capacity(messages.len());

        for message in messages {
            match message_to_document(
                &message,
                &self.format,
                self.id_field.as_deref(),
                self.id_template.as_ref(),
            ) {
                Ok(doc) => {
                    docs.push(doc);
                    valid_messages.push(message);
                }
                Err(e) => {
                    failed_messages.push((message, PublisherError::NonRetryable(e)));
                }
            }
        }

        if docs.is_empty() {
            if failed_messages.is_empty() {
                return Ok(SentBatch::Ack);
            } else {
                return Ok(SentBatch::Partial {
                    responses: None,
                    failed: failed_messages,
                });
            }
        }

        if self.uses_sequencer() {
            // Atomically increment a sequence counter for the batch. This is safe without a transaction.
            // If the subsequent insert fails, sequence numbers might be "lost", creating gaps.
            let filter = doc! {
                "_id": namespaced_sequencer_id(&self.collection_name)
            };
            let update = doc! { "$inc": { "seq_counter": docs.len() as i64 } };
            let options = FindOneAndUpdateOptions::builder()
                .upsert(true)
                .return_document(ReturnDocument::After)
                .write_concern(
                    mongodb::options::WriteConcern::builder()
                        .w(mongodb::options::Acknowledgment::Majority)
                        .build(),
                )
                .build();
            let counter_doc = self
                .meta_collection
                .find_one_and_update(filter, update)
                .with_options(options)
                .await
                .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
            let end_seq = counter_doc
                .ok_or_else(|| {
                    PublisherError::Retryable(anyhow!(
                        "Sequencer document not returned after upsert"
                    ))
                })?
                .get_i64("seq_counter")
                .map_err(|e| {
                    PublisherError::Retryable(anyhow!("Invalid seq_counter in sequencer: {}", e))
                })?;
            let start_seq = end_seq - docs.len() as i64 + 1;

            for (i, doc) in docs.iter_mut().enumerate() {
                doc.insert("seq", start_seq + i as i64);
            }
        }

        // Unordered: an ordered insert stops at the first error, so one duplicate
        // `_id` would leave the rest uninserted and indistinguishable from a
        // failure. Capped collections take unordered inserts too and still store
        // in $natural order, so `seq` and insertion order both survive.
        match self.collection.insert_many(docs).ordered(false).await {
            Ok(_) => {
                if failed_messages.is_empty() {
                    Ok(SentBatch::Ack)
                } else {
                    Ok(SentBatch::Partial {
                        responses: None,
                        failed: failed_messages,
                    })
                }
            }
            Err(e) => {
                if let ErrorKind::InsertMany(ref err) = *e.kind {
                    let mut errors_by_index = HashMap::new();
                    if let Some(write_errors) = &err.write_errors {
                        for we in write_errors {
                            errors_by_index.insert(we.index, we);
                        }
                    }

                    // If we have a write concern error, assume all failed to be safe (potential rollback).
                    // Since we have unique indexes, retrying is idempotent.
                    if err.write_concern_error.is_some() {
                        warn!("MongoDB write concern error detected. Retrying entire batch.");
                        for msg in valid_messages {
                            failed_messages.push((
                                msg,
                                PublisherError::Retryable(anyhow::anyhow!(
                                    "MongoDB write concern error"
                                )),
                            ));
                        }
                        return Ok(SentBatch::Partial {
                            responses: None,
                            failed: failed_messages,
                        });
                    }

                    // Every document was attempted, so an index with no write
                    // error was inserted and each error stands on its own.
                    for (i, msg) in valid_messages.into_iter().enumerate() {
                        if let Some(w) = errors_by_index.get(&i) {
                            // Duplicate `_id`: the document is already stored, which
                            // is what `id_field` uses to make a re-run idempotent.
                            if w.code != 11000 {
                                failed_messages.push((
                                    msg,
                                    PublisherError::Retryable(anyhow::anyhow!(
                                        "MongoDB write error: {:?}",
                                        w
                                    )),
                                ));
                            }
                        }
                    }

                    Ok(SentBatch::Partial {
                        responses: None,
                        failed: failed_messages,
                    })
                } else {
                    Err(PublisherError::Retryable(anyhow!(e)))
                }
            }
        }
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.db.run_command(doc! { "ping": 1 }).await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        EndpointStatus {
            healthy,
            target: self.collection_name.clone(),
            error,
            details: serde_json::json!({ "database": self.db.name(), "request_reply": self.request_reply }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
