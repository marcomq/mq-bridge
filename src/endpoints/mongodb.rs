use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{MongoDbConfig, MongoDbFormat};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
use mongodb::{
    bson::{doc, to_document, Bson, Document},
    change_stream::ChangeStream,
    error::ErrorKind,
    options::{FindOneAndUpdateOptions, FindOptions, ReturnDocument, UpdateOptions},
};
use mongodb::{change_stream::event::ChangeStreamEvent, IndexModel};
use mongodb::{Client, Collection, Database};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, trace, warn};

/// A helper struct for deserialization that matches the BSON structure exactly.
/// The payload is read as a BSON Binary type, which we then manually convert.
#[derive(Serialize, Deserialize, Debug)]
struct MongoMessageRaw {
    #[serde(rename = "_id")]
    id: mongodb::bson::Uuid,
    payload: Bson,
    metadata: Option<Document>,
}

impl TryFrom<MongoMessageRaw> for CanonicalMessage {
    type Error = anyhow::Error;

    fn try_from(raw: MongoMessageRaw) -> Result<Self, Self::Error> {
        let metadata: HashMap<String, String> = raw
            .metadata
            .map(mongodb::bson::from_document)
            .transpose()
            .context("Failed to deserialize metadata from BSON document")?
            .unwrap_or_default();

        let message_id = u128::from_be_bytes(raw.id.bytes());

        let payload = match raw.payload {
            Bson::Binary(bin) => bin.bytes.into(),
            Bson::Document(doc) => {
                let json = serde_json::to_vec(&doc)?;
                json.into()
            }
            Bson::Array(arr) => {
                let json = serde_json::to_vec(&arr)?;
                json.into()
            }
            _ => {
                let json_val: serde_json::Value = mongodb::bson::from_bson(raw.payload)?;
                serde_json::to_vec(&json_val)?.into()
            }
        };

        Ok(CanonicalMessage {
            message_id,
            payload,
            metadata,
        })
    }
}

fn document_to_canonical(doc: Document) -> anyhow::Result<CanonicalMessage> {
    let payload = serde_json::to_vec(&doc)?;
    let mut msg = CanonicalMessage::new(payload, None);
    msg.metadata
        .insert("mq_bridge.original_format".to_string(), "raw".to_string());
    Ok(msg)
}

fn message_to_document(
    message: &CanonicalMessage,
    format: &MongoDbFormat,
) -> anyhow::Result<Document> {
    // If request-reply metadata is present, we must use the wrapped format to preserve it,
    // regardless of whether the original format was raw.
    let force_wrapped = message.metadata.contains_key("correlation_id")
        || message.metadata.contains_key("reply_to");

    if !force_wrapped && matches!(format, MongoDbFormat::Raw) {
        if let Ok(doc) = serde_json::from_slice::<Document>(&message.payload) {
            return Ok(doc);
        }
        // If parsing fails, fall through to standard wrapping
    }

    let id_uuid = mongodb::bson::Uuid::from_bytes(message.message_id.to_be_bytes());

    let mut metadata = message.metadata.clone();
    let payload_bson = if matches!(format, MongoDbFormat::Json) {
        if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&message.payload) {
            if let Ok(bson_val) = mongodb::bson::to_bson(&json_val) {
                metadata.insert("type".to_string(), "json".to_string());
                bson_val
            } else {
                Bson::Binary(mongodb::bson::Binary {
                    subtype: mongodb::bson::spec::BinarySubtype::Generic,
                    bytes: message.payload.to_vec(),
                })
            }
        } else {
            Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: message.payload.to_vec(),
            })
        }
    } else {
        Bson::Binary(mongodb::bson::Binary {
            subtype: mongodb::bson::spec::BinarySubtype::Generic,
            bytes: message.payload.to_vec(),
        })
    };

    let metadata_doc =
        to_document(&metadata).context("Failed to serialize metadata to BSON document")?;

    Ok(doc! {
        "_id": id_uuid,
        "payload": payload_bson,
        "metadata": metadata_doc,
        "locked_until": null,
        "created_at": mongodb::bson::DateTime::now()
    })
}

fn parse_mongodb_document(doc: Document) -> anyhow::Result<CanonicalMessage> {
    if let Ok(raw_msg) = mongodb::bson::from_document::<MongoMessageRaw>(doc.clone()) {
        if let Ok(msg) = raw_msg.try_into() {
            return Ok(msg);
        }
    }
    document_to_canonical(doc)
}

/// Handle a reply to a MongoDB collection by inserting the response into the collection.
///
/// The reply will be inserted into the collection specified by the `reply_to` parameter.
/// If the `correlation_id` parameter is specified, it will be inserted into the reply document
/// as a field named `correlation_id` before insertion.
///
/// The function will log an error if the reply document cannot be serialized to BSON or if
/// the insertion into the collection fails.
async fn handle_reply(
    db: &Database,
    reply_to: Option<&String>,
    correlation_id: Option<&String>,
    response: CanonicalMessage,
) -> anyhow::Result<()> {
    if let Some(coll_name) = reply_to {
        let mut resp = response;
        if let Some(cid) = correlation_id {
            resp.metadata
                .insert("correlation_id".to_string(), cid.clone());
        }
        let doc = message_to_document(&resp, &MongoDbFormat::Normal).map_err(|e| {
            tracing::error!(collection = %coll_name, error = %e, "Failed to serialize MongoDB reply");
            anyhow!("Failed to serialize MongoDB reply: {}", e)
        })?;

        let reply_coll = db.collection::<Document>(coll_name);
        if let Err(e) = reply_coll.insert_one(doc).await {
            tracing::error!(collection = %coll_name, error = %e, "Failed to insert MongoDB reply");
            return Err(anyhow::anyhow!("Failed to insert MongoDB reply: {}", e,));
        }
    }
    Ok(())
}

/// A publisher that inserts messages into a MongoDB collection.
pub struct MongoDbPublisher {
    collection: Collection<Document>,
    db: Database,
    collection_name: String,
    request_reply: bool,
    request_timeout: Duration,
    reply_polling_interval: Duration,
    format: MongoDbFormat,
}

impl MongoDbPublisher {
    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB publisher"))?;
        let client = create_client(config).await?;
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
            db,
            collection_name: collection_name.to_string(),
            request_reply: config.request_reply,
            request_timeout: Duration::from_millis(config.request_timeout_ms.unwrap_or(30000)),
            reply_polling_interval: Duration::from_millis(config.reply_polling_ms.unwrap_or(50)),
            format: config.format.clone(),
        })
    }

    async fn recover_correlation_id_from_duplicate(
        &self,
        message: &mut CanonicalMessage,
    ) -> Result<(), PublisherError> {
        let id_uuid = mongodb::bson::Uuid::from_bytes(message.message_id.to_be_bytes());
        let filter = doc! { "_id": id_uuid };
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
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if !self.request_reply {
            trace!(message_id = %format!("{:032x}", message.message_id), collection = %self.collection_name, "Publishing sequenced document to MongoDB");
            let mut doc = message_to_document(&message, &self.format)
                .map_err(PublisherError::NonRetryable)?;

            // Atomically increment a sequence counter. This is safe without a transaction for just getting a sequence number.
            // If the subsequent insert fails, a sequence number might be "lost", creating a gap.
            let filter = doc! { "_id": "sequencer" };
            let update = doc! { "$inc": { "seq_counter": 1_i64 } };
            let options = FindOneAndUpdateOptions::builder()
                .upsert(true)
                .return_document(ReturnDocument::After)
                .build();

            let counter_doc = self
                .collection
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
                    PublisherError::Retryable(anyhow!("Invalid seq_counter in sequencer: {}", e))
                })?;
            doc.insert("seq", seq);

            match self.collection.insert_one(doc).await {
                Ok(_) => {}
                Err(e) => {
                    if let ErrorKind::Write(mongodb::error::WriteFailure::WriteError(ref w)) =
                        *e.kind
                    {
                        if w.code == 11000 {
                            warn!(message_id = %format!("{:032x}", message.message_id), "Duplicate key error inserting into MongoDB. Treating as idempotent success.");
                            return Ok(Sent::Ack);
                        }
                    }
                    return Err(PublisherError::Retryable(
                        anyhow::anyhow!(e).context("Failed to insert document into MongoDB"),
                    ));
                }
            }

            return Ok(Sent::Ack);
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
        let doc =
            message_to_document(&message, &self.format).map_err(PublisherError::NonRetryable)?;
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

        if self.request_reply {
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Publishing batch of documents to MongoDB");
        let mut docs = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();
        let mut valid_messages = Vec::with_capacity(messages.len());

        for message in messages {
            match message_to_document(&message, &self.format) {
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

        // Atomically increment a sequence counter for the batch. This is safe without a transaction.
        // If the subsequent insert fails, sequence numbers might be "lost", creating gaps.
        let filter = doc! { "_id": "sequencer" };
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
            .collection
            .find_one_and_update(filter, update)
            .with_options(options)
            .await
            .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
        let end_seq = counter_doc
            .ok_or_else(|| {
                PublisherError::Retryable(anyhow!("Sequencer document not returned after upsert"))
            })?
            .get_i64("seq_counter")
            .map_err(|e| {
                PublisherError::Retryable(anyhow!("Invalid seq_counter in sequencer: {}", e))
            })?;
        let start_seq = end_seq - docs.len() as i64 + 1;

        for (i, doc) in docs.iter_mut().enumerate() {
            doc.insert("seq", start_seq + i as i64);
        }

        match self.collection.insert_many(docs).await {
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

                    let mut stop_processing = false;

                    for (i, msg) in valid_messages.into_iter().enumerate() {
                        if stop_processing {
                            failed_messages.push((
                                msg,
                                PublisherError::Retryable(anyhow::anyhow!(
                                    "Message not inserted (skipped due to previous error)"
                                )),
                            ));
                            continue;
                        }

                        if let Some(w) = errors_by_index.get(&i) {
                            if w.code == 11000 {
                                // Duplicate key error. Treat as success (idempotent), but it stops execution in ordered mode.
                                stop_processing = true;
                            } else {
                                let error = PublisherError::Retryable(anyhow::anyhow!(
                                    "MongoDB write error: {:?}",
                                    w
                                ));
                                failed_messages.push((msg, error));
                                stop_processing = true;
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A consumer that receives messages from a MongoDB collection, treating it like a queue (locking).
pub struct MongoDbConsumer {
    collection: Collection<Document>,
    db: Database,
    change_stream: Option<tokio::sync::Mutex<ChangeStream<ChangeStreamEvent<Document>>>>,
    polling_interval: Duration,
    collection_name: String,
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

        Ok(Self {
            collection,
            db,
            change_stream,
            polling_interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
            collection_name: collection_name.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            // Always try to poll for a single document first using the efficient atomic operation.
            if let Some(claimed) = self.try_claim_document(doc! {}).await? {
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
        loop {
            // Always try to poll for a batch first.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_secs() as i64;
            let lock_duration_secs = 60;
            let locked_until = now + lock_duration_secs;

            let claimed_docs = self
                .find_and_claim_documents(doc! {}, max_messages, now, locked_until)
                .await?;

            if !claimed_docs.is_empty() {
                let (messages, commit) = self.process_claimed_documents(claimed_docs)?;
                return Ok(ReceivedBatch { messages, commit });
            }

            // If no documents found, wait before retrying.
            if let Some(stream_mutex) = &self.change_stream {
                // Replica Set: Wait for a change stream event to wake us up.
                let mut stream = stream_mutex.lock().await;
                match tokio::time::timeout(Duration::from_secs(5), stream.next()).await {
                    Ok(Some(Ok(_))) => {} // Event received, loop back to try claiming documents.
                    Ok(Some(Err(e))) => return Err(ConsumerError::Connection(e.into())),
                    Ok(None) => {
                        return Err(anyhow!("MongoDB change stream ended unexpectedly").into())
                    }
                    Err(_) => {} // Timeout, loop back to check for documents.
                }
            } else {
                // Standalone: Sleep for polling interval.
                tokio::time::sleep(self.polling_interval).await;
            }
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
    ) -> anyhow::Result<Vec<Document>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut base_filter = Self::available_message_filter(now);
        base_filter.extend(extra_filter);

        // 1. Find a batch of available documents.
        let mut cursor = self
            .collection
            .find(base_filter.clone())
            .limit(limit as i64)
            .projection(doc! { "_id": 1 })
            .sort(doc! { "_id": 1 })
            .await?;

        let mut ids_to_claim = Vec::new();
        while let Some(result) = cursor.next().await {
            if let Ok(doc) = result {
                if let Some(Bson::Binary(binary)) = doc.get("_id") {
                    if let Ok(uuid) = binary.to_uuid() {
                        ids_to_claim.push(uuid);
                    }
                }
            }
        }

        if ids_to_claim.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Attempt to atomically claim the batch of documents.
        let mut update_filter = doc! { "_id": { "$in": &ids_to_claim } };
        update_filter.extend(base_filter);

        let update = doc! { "$set": { "locked_until": locked_until } };
        let update_result = self.collection.update_many(update_filter, update).await?;

        // 3. If we successfully modified any documents, retrieve their full content.
        if update_result.modified_count > 0 {
            self.get_documents_by_ids(&ids_to_claim).await
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

        let mut filter = Self::available_message_filter(now);
        filter.extend(extra_filter);

        let update = doc! { "$set": { "locked_until": locked_until } };

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
                                        doc! { "_id": id_val.clone() },
                                        doc! { "$set": { "locked_until": null } },
                                    )
                                    .await
                                    .context("Failed to unlock Nacked message")?;
                                return Ok(());
                            }
                        }

                        match collection_clone
                            .delete_one(doc! { "_id": id_val.clone() })
                            .await
                        {
                            Ok(delete_result) => {
                                if delete_result.deleted_count == 1 {
                                    trace!(mongodb_id = %id_val, "MongoDB message acknowledged and deleted");
                                } else {
                                    warn!(mongodb_id = %id_val, "Attempted to ack/delete MongoDB message, but it was not found (already deleted?)");
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

    /// Retrieves documents by their IDs.
    async fn get_documents_by_ids(
        &self,
        claimed_ids: &[mongodb::bson::Uuid],
    ) -> anyhow::Result<Vec<Document>> {
        let filter = doc! { "_id": { "$in": claimed_ids } };
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
                )
                .await
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok((messages, commit))
    }
}

async fn process_mongodb_batch_commit(
    db: &Database,
    collection: &Collection<Document>,
    reply_infos: &[(Option<String>, Option<String>)],
    ids: &[Bson],
    dispositions: Vec<MessageDisposition>,
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
        let filter = doc! { "_id": { "$in": &ids_to_unlock } };
        let update = doc! { "$set": { "locked_until": null } };
        if let Err(e) = collection.update_many(filter, update).await {
            tracing::error!(error = %e, "Failed to unlock Nacked MongoDB messages");
            return Err(anyhow::anyhow!(
                "Failed to unlock Nacked MongoDB messages: {}",
                e
            ));
        }
    }

    if !ids_to_delete.is_empty() {
        let filter = doc! { "_id": { "$in": &ids_to_delete } };
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

/// A subscriber that reads messages from a MongoDB collection using a monotonic sequence number.
/// This replaces the old EventStore-based implementation.
pub struct MongoDbSubscriber {
    collection: Collection<Document>,
    polling_interval: Duration,
    cursor_id: Option<String>,
    last_seq: Arc<AtomicI64>,
}

impl MongoDbSubscriber {
    /// Creates a new MongoDB subscriber.
    ///
    /// The subscriber will watch for inserts to the specified collection and treat them as new events.
    /// If the MongoDB instance does not support ChangeStreams (i.e., a single instance), it will fall back to
    /// periodically polling the collection for new messages.
    ///
    /// Note that the subscriber will start consuming from the last inserted document if ChangeStreams are not
    /// supported. If the collection is empty, it will start consuming from the next inserted document.
    ///
    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB subscriber"))?;
        let client = create_client(config).await?;
        let db = client.database(&config.database);
        let collection: Collection<Document> = db.collection(collection_name);

        let mut last_seq = 0;
        if let Some(cid) = &config.cursor_id {
            let cursor_doc_id = format!("cursor:{}", cid);
            if let Ok(Some(doc)) = collection.find_one(doc! { "_id": cursor_doc_id }).await {
                last_seq = doc.get_i64("last_seq").unwrap_or(0);
            }
        } else {
            // Ephemeral mode: start from current sequencer value
            if let Ok(Some(doc)) = collection.find_one(doc! { "_id": "sequencer" }).await {
                last_seq = doc.get_i64("seq_counter").unwrap_or(0);
            }
        }
        info!(collection = %collection_name, cursor_id = ?config.cursor_id, start_seq = %last_seq, "MongoDB sequenced subscriber initialized");

        Ok(Self {
            collection,
            polling_interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
            cursor_id: config.cursor_id.clone(),
            last_seq: Arc::new(AtomicI64::new(last_seq)),
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbSubscriber {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            // Filter for events with seq > last_seq.
            // Crucially, we must filter out the sequencer and cursor documents which might be in the same collection.
            // Events have a 'payload' field, while sequencer/cursors do not.
            let last_seq = self.last_seq.load(Ordering::Relaxed);
            let filter = doc! {
                "seq": { "$gt": last_seq },
                "payload": { "$exists": true }
            };
            let find_options = FindOptions::builder()
                .sort(doc! { "seq": 1 })
                .limit(max_messages as i64)
                .build();

            let mut cursor = self
                .collection
                .find(filter)
                .with_options(find_options)
                .await
                .map_err(|e| ConsumerError::Connection(e.into()))?;

            let mut messages = Vec::new();
            let mut seqs = Vec::new();

            while let Some(result) = cursor.next().await {
                if let Ok(doc) = result {
                    if let Ok(seq) = doc.get_i64("seq") {
                        if let Ok(msg) = parse_mongodb_document(doc) {
                            messages.push(msg);
                            seqs.push(seq);
                        }
                    }
                }
            }

            if !messages.is_empty() {
                let collection = self.collection.clone();
                let cursor_id = self.cursor_id.clone();
                let last_seq_arc = self.last_seq.clone();

                let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
                    Box::pin(async move {
                        let mut highest_acked = 0;
                        for (disp, seq) in dispositions.iter().zip(seqs.iter()) {
                            if matches!(
                                disp,
                                MessageDisposition::Ack | MessageDisposition::Reply(_)
                            ) {
                                highest_acked = *seq;
                            } else {
                                break; // Stop at first Nack
                            }
                        }

                        if highest_acked > 0 {
                            last_seq_arc.store(highest_acked, Ordering::SeqCst);
                            // Only persist if we have a cursor_id
                            if let Some(cid) = cursor_id {
                                let cursor_doc_id = format!("cursor:{}", cid);
                                if let Err(e) = collection
                                    .update_one(
                                        doc! { "_id": cursor_doc_id },
                                        doc! { "$set": { "last_seq": highest_acked } },
                                    )
                                    .with_options(UpdateOptions::builder().upsert(true).build())
                                    .await
                                {
                                    tracing::warn!(cursor_id = %cid, error = %e, "Failed to persist cursor position. Messages may be reprocessed on restart.");
                                }
                            }
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });
                return Ok(ReceivedBatch { messages, commit });
            }
            tokio::time::sleep(self.polling_interval).await;
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

async fn create_client(config: &MongoDbConfig) -> anyhow::Result<Client> {
    let mut client_options = mongodb::options::ClientOptions::parse(&config.url).await?;
    if let (Some(username), Some(password)) = (&config.username, &config.password) {
        client_options.credential = Some(
            mongodb::options::Credential::builder()
                .username(username.clone())
                .password(password.clone())
                .build(),
        );
    }

    if config.tls.required {
        let mut tls_options = mongodb::options::TlsOptions::builder().build();
        if let Some(ca_file) = &config.tls.ca_file {
            tls_options.ca_file_path = Some(std::path::PathBuf::from(ca_file));
        }
        if let Some(cert_file) = &config.tls.cert_file {
            tls_options.cert_key_file_path = Some(std::path::PathBuf::from(cert_file));
        }
        if config.tls.key_file.is_some() {
            tracing::warn!("MongoDB TLS configuration: 'key_file' is ignored. The private key must be included in the 'cert_file' (PEM format).");
        }
        if let Some(cert_password) = &config.tls.cert_password {
            tls_options.tls_certificate_key_file_password = Some(cert_password.as_bytes().to_vec());
        }
        if config.tls.accept_invalid_certs {
            tls_options.allow_invalid_certificates = Some(true);
        }
        client_options.tls = Some(mongodb::options::Tls::Enabled(tls_options));
    }
    Ok(Client::with_options(client_options)?)
}
