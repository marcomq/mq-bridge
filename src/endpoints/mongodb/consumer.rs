//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// A consumer that receives messages from a MongoDB collection, treating it like a queue (locking).
pub struct MongoDbConsumer {
    collection: Collection<Document>,
    db: Database,
    change_stream: Option<tokio::sync::Mutex<ChangeStream<ChangeStreamEvent<Document>>>>,
    polling_interval: Duration,
    collection_name: String,
    receive_query: Option<Document>,
}

impl MongoDbConsumer {
    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB consumer"))?;
        let client = create_client(config).await?;
        // The first operation will trigger connection and topology discovery.
        client.list_database_names().await?;

        let db = client.database(&config.database);
        let collection = db.collection(collection_name);

        // Create an index on `locked_until` to speed up finding available messages.
        // This is an idempotent operation, so it's safe to run on every startup.
        info!(collection = %collection_name, "Ensuring 'locked_until' index exists...");
        let index_model = IndexModel::builder()
            .keys(doc! { "locked_until": 1 })
            .build();
        collection.create_index(index_model).await?;

        // Attempt to create a change stream. If it fails because it's a standalone instance,
        // fall back to polling.
        let pipeline = [doc! { "$match": { "operationType": "insert" } }];
        let change_stream_result = collection.watch().pipeline(pipeline).await;

        let (change_stream, mode) = match change_stream_result {
            Ok(stream) => {
                info!("MongoDB is a replica set/sharded cluster. Using change stream.");
                (Some(tokio::sync::Mutex::new(stream)), "change_stream")
            }
            Err(e) if matches!(*e.kind, ErrorKind::Command(ref cmd_err) if cmd_err.code == 40573) =>
            {
                info!("MongoDB is a single instance (ChangeStream support check failed). Falling back to polling for consumer.");
                (None, "polling")
            }
            Err(e) => return Err(e.into()), // For any other error, we propagate it.
        };

        info!(database = %config.database, collection = %collection_name, mode = %mode, "MongoDB consumer connected");

        let receive_query = if let Some(q) = &config.receive_query {
            let doc: Document = serde_json::from_str(q)
                .context("Failed to parse 'receive_query' from configuration as a JSON document")?;
            Some(doc)
        } else {
            None
        };

        Ok(Self {
            collection,
            db,
            change_stream,
            polling_interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
            collection_name: collection_name.to_string(),
            receive_query,
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbConsumer {
    // MongoDB acks each document individually (update/delete by id), so commits
    // can run concurrently and out of order.
    fn commit_requires_order(&self) -> bool {
        false
    }
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        let extra_filter = self.receive_query.clone().unwrap_or_default();
        loop {
            // Always try to poll for a single document first using the efficient atomic operation.
            if let Some(claimed) = self.try_claim_document(extra_filter.clone()).await? {
                return Ok(claimed);
            }

            // If no document found, wait.
            if let Some(stream_mutex) = &self.change_stream {
                // --- Change Stream Path ---
                // Wait for an event to wake us up.
                let mut stream = stream_mutex.lock().await;
                // Use a timeout to ensure we periodically check for documents even if stream is silent.
                match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                    Ok(Some(Ok(_))) => continue, // Event received, loop back to try claiming documents.
                    Ok(Some(Err(e))) => return Err(ConsumerError::Connection(e.into())),
                    Ok(None) => {
                        return Err(anyhow!("MongoDB change stream ended unexpectedly").into())
                    }
                    Err(_) => continue, // Timeout, loop back to check for documents.
                }
            }

            // Standalone: Sleep for polling interval.
            tokio::time::sleep(self.polling_interval).await;
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let extra_filter = self.receive_query.clone().unwrap_or_default();
        loop {
            // Always try to poll for a batch first.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_secs() as i64;
            let lock_duration_secs = 60;
            let locked_until = now + lock_duration_secs;
            // Concurrent consumers can write the same `locked_until` second, so only
            // this token distinguishes the documents this poll actually won.
            let claim_token = fast_uuid_v7::gen_id_string();

            let claimed_docs = self
                .find_and_claim_documents(
                    extra_filter.clone(),
                    max_messages,
                    now,
                    locked_until,
                    &claim_token,
                )
                .await?;

            if !claimed_docs.is_empty() {
                let (messages, commit) =
                    self.process_claimed_documents(claimed_docs, claim_token)?;
                return Ok(ReceivedBatch { messages, commit });
            }

            // Drained: wait for the next arrival, then surface an empty batch so the
            // route can pause (empty_batch_delay_ms) or, with exit_on_empty, terminate
            // gracefully. Blocking here indefinitely would make exit_on_empty unreachable.
            if let Some(stream_mutex) = &self.change_stream {
                // Replica set: wait briefly for an insert. On an event, loop back to
                // claim immediately (low latency); on timeout, return the empty batch
                // below so exit_on_empty can fire.
                let mut stream = stream_mutex.lock().await;
                match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                    Ok(Some(Ok(_))) => continue, // Event received, loop back to claim.
                    Ok(Some(Err(e))) => return Err(ConsumerError::Connection(e.into())),
                    Ok(None) => {
                        return Err(anyhow!("MongoDB change stream ended unexpectedly").into())
                    }
                    Err(_) => {} // Timeout: fall through to return the empty batch.
                }
            } else {
                // Standalone: sleep the polling interval, then return the empty batch.
                tokio::time::sleep(self.polling_interval).await;
            }

            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
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
            let now = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let filter = if let Some(extra) = &self.receive_query {
                doc! { "$and": [Self::available_message_filter(now), extra.clone()] }
            } else {
                Self::available_message_filter(now)
            };
            match self.collection.count_documents(filter).await {
                Ok(c) => Some(c as usize),
                Err(e) => {
                    error = Some(format!("Failed to count pending documents: {}", e));
                    None
                }
            }
        } else {
            None
        };

        EndpointStatus {
            healthy,
            target: self.collection_name.clone(),
            pending,
            error,
            details: serde_json::json!({ "database": self.db.name(), "mode": if self.change_stream.is_some() { "change_stream" } else { "polling" } }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MongoDbConsumer {
    /// Creates a BSON document filter to find available (unlocked) messages.
    fn available_message_filter(now: i64) -> Document {
        doc! {
            "$and": [
                { "$or": [
                    { "locked_until": { "$exists": false } },
                    { "locked_until": null },
                    { "locked_until": { "$lt": now } }
                ] },
                { "seq_counter": { "$exists": false } },
                { "last_seq": { "$exists": false } }
            ]
        }
    }

    /// Atomically finds and claims one or more documents.
    async fn find_and_claim_documents(
        &self,
        extra_filter: Document,
        limit: usize,
        now: i64,
        locked_until: i64,
        claim_token: &str,
    ) -> anyhow::Result<Vec<Document>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let base_filter = if extra_filter.is_empty() {
            Self::available_message_filter(now)
        } else {
            doc! { "$and": [Self::available_message_filter(now), extra_filter] }
        };

        // 1. Find a batch of available documents.
        let mut cursor = self
            .collection
            .find(base_filter.clone())
            .limit(limit as i64)
            .projection(doc! { "_id": 1 })
            .sort(doc! { "_id": 1 })
            .await?;

        // Any `_id` type is claimable — ObjectId, string, integer, UUID binary. Restricting
        // this to UUIDs would silently skip documents `try_claim_document` picks up fine.
        let mut ids_to_claim: Vec<Bson> = Vec::new();
        while let Some(result) = cursor.next().await {
            if let Ok(doc) = result {
                if let Some(id) = doc.get("_id") {
                    ids_to_claim.push(id.clone());
                }
            }
        }

        if ids_to_claim.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Attempt to atomically claim the batch of documents.
        let mut update_filter = doc! { "_id": { "$in": &ids_to_claim } };
        update_filter.extend(base_filter);

        let update = doc! { "$set": { "locked_until": locked_until, "claim_token": claim_token } };
        let update_result = self.collection.update_many(update_filter, update).await?;

        // If we successfully modified any documents, retrieve their full content.
        if update_result.modified_count > 0 {
            self.get_documents_by_ids(&ids_to_claim, locked_until, claim_token)
                .await
        } else {
            Ok(Vec::new())
        }
    }

    /// Atomically finds and locks a document matching the filter.
    async fn try_claim_document(&self, extra_filter: Document) -> anyhow::Result<Option<Received>> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs() as i64;
        let lock_duration_secs = 60;
        let locked_until = now + lock_duration_secs;
        let claim_token = fast_uuid_v7::gen_id_string();

        let filter = if extra_filter.is_empty() {
            Self::available_message_filter(now)
        } else {
            doc! { "$and": [Self::available_message_filter(now), extra_filter] }
        };

        let update =
            doc! { "$set": { "locked_until": locked_until, "claim_token": claim_token.as_str() } };

        let options = FindOneAndUpdateOptions::builder()
            .projection(doc! { "_id": 1, "payload": 1, "metadata": 1 })
            .sort(doc! { "_id": 1 }) // Process oldest documents first (FIFO)
            .build();

        match self
            .collection
            .find_one_and_update(filter, update)
            .with_options(options)
            .await
        {
            Ok(Some(doc)) => {
                let id_val = doc
                    .get("_id")
                    .cloned()
                    .ok_or_else(|| anyhow!("Document missing _id"))?;

                let msg = parse_mongodb_document(doc)?;

                let reply_collection_name = msg.metadata.get("reply_to").cloned();
                let correlation_id = msg.metadata.get("correlation_id").cloned();
                let db = self.db.clone();
                let collection_clone = self.collection.clone();

                let commit = Box::new(move |disposition: MessageDisposition| {
                    Box::pin(async move {
                        match disposition {
                            MessageDisposition::Reply(resp) => {
                                if !extend_claim(&collection_clone, &id_val, &claim_token).await? {
                                    warn!(mongodb_id = %id_val, "Skipping MongoDB reply because the message claim was lost");
                                    return Ok(());
                                }
                                handle_reply(
                                    &db,
                                    reply_collection_name.as_ref(),
                                    correlation_id.as_ref(),
                                    resp,
                                )
                                .await?;
                            }
                            MessageDisposition::Ack => {}
                            MessageDisposition::Nack => {
                                collection_clone
                                    .update_one(
                                        doc! { "_id": id_val.clone(), "claim_token": claim_token.as_str() },
                                        doc! { "$set": { "locked_until": null }, "$unset": { "claim_token": "" } },
                                    )
                                    .await
                                    .context("Failed to unlock Nacked message")?;
                                return Ok(());
                            }
                        }

                        match collection_clone
                            .delete_one(
                                doc! { "_id": id_val.clone(), "claim_token": claim_token.as_str() },
                            )
                            .await
                        {
                            Ok(delete_result) => {
                                if delete_result.deleted_count == 1 {
                                    trace!(mongodb_id = %id_val, "MongoDB message acknowledged and deleted");
                                } else {
                                    warn!(mongodb_id = %id_val, "Attempted to ack/delete MongoDB message, but it was not found (already deleted, or the lock expired and it was re-claimed?)");
                                }
                            }
                            Err(e) => {
                                tracing::error!(mongodb_id = %id_val, error = %e, "Failed to ack/delete MongoDB message");
                                return Err(anyhow::anyhow!(
                                    "Failed to ack/delete MongoDB message: {}",
                                    e
                                ));
                            }
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                Ok(Some(Received {
                    message: msg,
                    commit,
                }))
            }
            Ok(None) => Ok(None), // No document found or claimed
            Err(e) => Err(e.into()),
        }
    }

    /// Retrieves the documents this claim locked.
    ///
    /// The candidate ids were gathered before the update, so a concurrent consumer may
    /// have taken some of them. `locked_until` alone cannot separate the claims — two
    /// consumers polling in the same second write the same value — so the unique
    /// `claim_token` is what restricts the result to the documents this call won.
    async fn get_documents_by_ids(
        &self,
        claimed_ids: &[Bson],
        locked_until: i64,
        claim_token: &str,
    ) -> anyhow::Result<Vec<Document>> {
        let filter = doc! {
            "_id": { "$in": claimed_ids },
            "locked_until": locked_until,
            "claim_token": claim_token
        };
        let mut cursor = self
            .collection
            .find(filter)
            .projection(doc! { "_id": 1, "payload": 1, "metadata": 1 })
            .await?;

        let mut documents = Vec::new();
        while let Some(result) = cursor.next().await {
            documents.push(result?);
        }
        Ok(documents)
    }

    /// Processes a vector of claimed BSON documents into canonical messages and a single batch commit function.
    fn process_claimed_documents(
        &self,
        docs: Vec<Document>,
        claim_token: String,
    ) -> anyhow::Result<(Vec<CanonicalMessage>, BatchCommitFunc)> {
        let mut messages = Vec::with_capacity(docs.len());
        let mut ids = Vec::with_capacity(docs.len());
        let mut reply_infos = Vec::with_capacity(docs.len());

        for doc in docs {
            let id_val = doc
                .get("_id")
                .cloned()
                .ok_or_else(|| anyhow!("Document missing _id"))?;

            let msg = parse_mongodb_document(doc)?;
            reply_infos.push((
                msg.metadata.get("reply_to").cloned(),
                msg.metadata.get("correlation_id").cloned(),
            ));
            messages.push(msg);

            ids.push(id_val);
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Received batch of MongoDB documents");
        let collection_clone = self.collection.clone();
        let db = self.db.clone();

        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                if dispositions.len() != reply_infos.len() {
                    tracing::warn!(
                        "Disposition count mismatch: expected {}, got {}",
                        reply_infos.len(),
                        dispositions.len()
                    );
                }
                process_mongodb_batch_commit(
                    &db,
                    &collection_clone,
                    &reply_infos,
                    &ids,
                    dispositions,
                    &claim_token,
                )
                .await
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok((messages, commit))
    }
}

async fn extend_claim(
    collection: &Collection<Document>,
    id: &Bson,
    claim_token: &str,
) -> anyhow::Result<bool> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs() as i64;
    let result = collection
        .update_one(
            doc! { "_id": id, "claim_token": claim_token },
            doc! { "$set": { "locked_until": now + 60 } },
        )
        .await
        .context("Failed to extend MongoDB message claim before replying")?;
    Ok(result.matched_count == 1)
}

async fn process_mongodb_batch_commit(
    db: &Database,
    collection: &Collection<Document>,
    reply_infos: &[(Option<String>, Option<String>)],
    ids: &[Bson],
    dispositions: Vec<MessageDisposition>,
    claim_token: &str,
) -> anyhow::Result<()> {
    let mut ids_to_delete = Vec::new();
    let mut ids_to_unlock = Vec::new();
    let mut errors = Vec::new();

    for (((reply_coll_opt, correlation_id_opt), disposition), id) in
        reply_infos.iter().zip(dispositions).zip(ids.iter())
    {
        // Only send a reply if the message has a 'reply_to' destination and the disposition is a Reply.
        // This allows for fire-and-forget patterns (no reply_to) or explicit replies.
        match disposition {
            MessageDisposition::Reply(resp) => {
                if !extend_claim(collection, id, claim_token).await? {
                    warn!(mongodb_id = %id, "Skipping MongoDB reply because the message claim was lost");
                    continue;
                }
                match handle_reply(
                    db,
                    reply_coll_opt.as_ref(),
                    correlation_id_opt.as_ref(),
                    resp,
                )
                .await
                {
                    Ok(_) => ids_to_delete.push(id.clone()),
                    Err(e) => {
                        tracing::error!(id = %id, error = %e, "Failed to send reply");
                        errors.push(e);
                        ids_to_unlock.push(id.clone());
                    }
                }
            }
            MessageDisposition::Ack => {
                ids_to_delete.push(id.clone());
            }
            MessageDisposition::Nack => {
                ids_to_unlock.push(id.clone());
            }
        }
    }

    if !ids_to_unlock.is_empty() {
        let filter = doc! { "_id": { "$in": &ids_to_unlock }, "claim_token": claim_token };
        let update = doc! { "$set": { "locked_until": null }, "$unset": { "claim_token": "" } };
        if let Err(e) = collection.update_many(filter, update).await {
            tracing::error!(error = %e, "Failed to unlock Nacked MongoDB messages");
            return Err(anyhow::anyhow!(
                "Failed to unlock Nacked MongoDB messages: {}",
                e
            ));
        }
    }

    if !ids_to_delete.is_empty() {
        // Scoped to this claim: a lease that expired and was re-claimed elsewhere must
        // not be deleted from under its new owner.
        let filter = doc! { "_id": { "$in": &ids_to_delete }, "claim_token": claim_token };
        // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
        if let Err(e) = collection.delete_many(filter).await {
            tracing::error!(error = %e, "Failed to bulk-ack/delete MongoDB messages");
            return Err(anyhow::anyhow!(
                "Failed to bulk-ack/delete MongoDB messages: {}",
                e
            ));
        } else {
            trace!(
                count = ids_to_delete.len(),
                "MongoDB messages acknowledged and deleted"
            );
        }
    }

    if !errors.is_empty() {
        return Err(anyhow::anyhow!(
            "Errors occurred during commit: {:?}",
            errors
        ));
    }
    Ok(())
}
