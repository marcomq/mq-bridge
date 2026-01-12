use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::MongoDbConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError,
    Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
use mongodb::{
    bson::{doc, to_document, Binary, Bson, Document},
    change_stream::ChangeStream,
    error::ErrorKind,
    options::FindOneAndUpdateOptions,
};
use mongodb::{change_stream::event::ChangeStreamEvent, IndexModel};
use mongodb::{Client, Collection, Database};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use tracing::{info, trace, warn};

/// A helper struct for deserialization that matches the BSON structure exactly.
/// The payload is read as a BSON Binary type, which we then manually convert.
#[derive(Serialize, Deserialize, Debug)]
struct MongoMessageRaw {
    #[serde(rename = "_id")]
    id: mongodb::bson::Uuid,
    payload: Binary,
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

        Ok(CanonicalMessage {
            message_id,
            payload: raw.payload.bytes.into(),
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

fn message_to_document(message: &CanonicalMessage) -> anyhow::Result<Document> {
    if message
        .metadata
        .get("mq_bridge.original_format")
        .map(|s| s.as_str())
        == Some("raw")
    {
        if let Ok(doc) = serde_json::from_slice::<Document>(&message.payload) {
            return Ok(doc);
        }
        // If parsing fails, fall through to standard wrapping
    }

    let id_uuid = mongodb::bson::Uuid::from_bytes(message.message_id.to_be_bytes());
    let metadata =
        to_document(&message.metadata).context("Failed to serialize metadata to BSON document")?;

    Ok(doc! {
        "_id": id_uuid,
        "payload": Bson::Binary(mongodb::bson::Binary {
            subtype: mongodb::bson::spec::BinarySubtype::Generic,
            bytes: message.payload.to_vec() }),
        "metadata": metadata,
        "locked_until": null,
        "created_at": mongodb::bson::DateTime::now()
    })
}

/// A publisher that inserts messages into a MongoDB collection.
pub struct MongoDbPublisher {
    collection: Collection<Document>,
    collection_name: String,
}

impl MongoDbPublisher {
    pub async fn new(config: &MongoDbConfig, collection_name: &str) -> anyhow::Result<Self> {
        let client = create_client(config).await?;
        let db = client.database(&config.database);
        let collection = db.collection(collection_name);
        info!(database = %config.database, collection = %collection_name, "MongoDB publisher connected");

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
        Ok(Self {
            collection,
            collection_name: collection_name.to_string(),
        })
    }
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(message_id = %format!("{:032x}", message.message_id), collection = %self.collection_name, "Publishing document to MongoDB");
        let doc = message_to_document(&message).map_err(PublisherError::NonRetryable)?;
        self.collection
            .insert_one(doc)
            .await
            .context("Failed to insert document into MongoDB")?;

        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Publishing batch of documents to MongoDB");
        let mut docs = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();

        for message in &messages {
            match message_to_document(message) {
                Ok(doc) => docs.push(doc),
                Err(e) => {
                    failed_messages.push((message.clone(), PublisherError::NonRetryable(e)));
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

        // Use ordered: false to attempt inserting all documents even if some fail (e.g. duplicates).
        // This improves throughput.
        let options = mongodb::options::InsertManyOptions::builder()
            .ordered(false)
            .build();

        match self
            .collection
            .insert_many(docs)
            .with_options(options)
            .await
        {
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
            Err(e) => Err(PublisherError::Retryable(anyhow::anyhow!(
                "MongoDB bulk write failed: {}",
                e
            ))),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A consumer that receives messages from a MongoDB collection, treating it like a queue.
pub struct MongoDbConsumer {
    collection: Collection<Document>,
    db: Database,
    change_stream: Option<tokio::sync::Mutex<ChangeStream<ChangeStreamEvent<Document>>>>,
    polling_interval: Duration,
    collection_name: String,
}

impl MongoDbConsumer {
    pub async fn new(config: &MongoDbConfig, collection_name: &str) -> anyhow::Result<Self> {
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

        let change_stream = match change_stream_result {
            Ok(stream) => {
                info!("MongoDB is a replica set/sharded cluster. Using change stream.");
                Some(tokio::sync::Mutex::new(stream))
            }
            Err(e) if matches!(*e.kind, ErrorKind::Command(ref cmd_err) if cmd_err.code == 40573) =>
            {
                info!("MongoDB is a single instance (ChangeStream support check failed). Falling back to polling for consumer.");
                None
            }
            Err(e) => return Err(e.into()), // For any other error, we propagate it.
        };

        info!(database = %config.database, collection = %collection_name, "MongoDB consumer connected and watching for changes");

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
            // This works for both standalone and replica sets and ensures we drain backlogs fast.
            // Unlike receive_batch which uses a 3-step process (find, update, find),
            // try_claim_document uses find_one_and_update which is a single round-trip.
            if let Some(claimed) = self.try_claim_document(doc! {}).await? {
                return Ok(claimed);
            }

            // If no document found, wait.
            if let Some(stream_mutex) = &self.change_stream {
                // --- Change Stream Path ---
                // Wait for an event to wake us up.
                let mut stream = stream_mutex.lock().await;
                match stream.next().await {
                    Some(Ok(_)) => continue, // Event received, loop back to try claiming documents.
                    Some(Err(e)) => return Err(ConsumerError::Connection(e.into())),
                    None => return Err(anyhow!("MongoDB change stream ended unexpectedly").into()),
                }
            }

            // --- Polling Path (Standalone) ---
            tokio::time::sleep(self.polling_interval).await;
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            // Always try to poll for a batch first. This ensures high throughput for backlogs
            // and works for both standalone (polling) and replica sets (hybrid).
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
                match stream.next().await {
                    Some(Ok(_)) => {} // Event received, loop back to try claiming documents.
                    Some(Err(e)) => return Err(ConsumerError::Connection(e.into())),
                    None => return Err(anyhow!("MongoDB change stream ended unexpectedly").into()),
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
            "$or": [
                { "locked_until": { "$exists": false } },
                { "locked_until": null },
                { "locked_until": { "$lt": now } }
            ]
        }
    }

    /// Atomically finds and claims one or more documents.
    /// This is the core logic for both single and batch receives in polling mode.
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
        // We re-apply the `locked_until` filter to prevent a race condition
        // where another consumer locks the documents between our find and update.
        let mut update_filter = doc! { "_id": { "$in": &ids_to_claim } };
        update_filter.extend(base_filter);

        let update = doc! { "$set": { "locked_until": locked_until } };
        let update_result = self.collection.update_many(update_filter, update).await?;

        // 3. If we successfully modified any documents, retrieve their full content.
        if update_result.modified_count > 0 {
            self.get_documents_by_ids(&ids_to_claim).await
        } else {
            // This means another consumer claimed the documents in a race.
            // Return an empty vec, and the caller will loop to try again.
            Ok(Vec::new())
        }
    }

    /// Atomically finds and locks a document matching the filter.
    /// If the filter is empty, it finds any available document.
    /// If a document is successfully claimed, it returns the message and commit function.
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

                // Optimization: Avoid cloning the document (which copies the payload) if it looks like a standard message.
                // We assume that if 'payload' is a Binary, it is a standard message.
                let is_standard_msg = doc
                    .get("payload")
                    .map(|b| matches!(b, Bson::Binary(_)))
                    .unwrap_or(false);
                let msg = if is_standard_msg {
                    match mongodb::bson::from_document::<MongoMessageRaw>(doc.clone()) {
                        Ok(raw_msg) => raw_msg.try_into().unwrap_or(document_to_canonical(doc)?),
                        Err(_) => document_to_canonical(doc)?,
                    }
                } else {
                    document_to_canonical(doc)?
                };

                let reply_collection_name = msg.metadata.get("reply_to").cloned();
                let db = self.db.clone();
                let collection_clone = self.collection.clone();

                let commit = Box::new(move |response: Option<CanonicalMessage>| {
                    Box::pin(async move {
                        if let (Some(resp), Some(coll_name)) = (response, reply_collection_name) {
                            if let Ok(doc) = message_to_document(&resp) {
                                let reply_coll = db.collection::<Document>(&coll_name);
                                if let Err(e) = reply_coll.insert_one(doc).await {
                                    tracing::error!(collection = %coll_name, error = %e, "Failed to insert MongoDB reply");
                                    return Err(anyhow::anyhow!("Failed to insert MongoDB reply: {}", e));
                                }
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
                                // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
                                tracing::error!(mongodb_id = %id_val, error = %e, "Failed to ack/delete MongoDB message");
                                return Err(anyhow::anyhow!("Failed to ack/delete MongoDB message: {}", e));
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

    /// Retrieves documents by their ObjectIds.
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

            let is_standard_msg = doc
                .get("payload")
                .map(|b| matches!(b, Bson::Binary(_)))
                .unwrap_or(false);
            let msg = if is_standard_msg {
                mongodb::bson::from_document::<MongoMessageRaw>(doc)
                    .map_err(|e| anyhow!("Failed to parse standard MongoDB message: {}", e))?
                    .try_into()?
            } else {
                document_to_canonical(doc)?
            };
            reply_infos.push(msg.metadata.get("reply_to").cloned());
            messages.push(msg);

            ids.push(id_val);
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Received batch of MongoDB documents");
        let collection_clone = self.collection.clone();
        let db = self.db.clone();

        let commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                if let Some(resps) = responses {
                    for (reply_coll_opt, resp) in reply_infos.iter().zip(resps) {
                        if let Some(coll_name) = reply_coll_opt {
                            if let Ok(doc) = message_to_document(&resp) {
                                let reply_coll = db.collection::<Document>(coll_name);
                                if let Err(e) = reply_coll.insert_one(doc).await {
                                    tracing::error!(collection = %coll_name, response_id = %format!("{:032x}", resp.message_id), error = %e, "Failed to insert MongoDB batch reply");
                                    return Err(anyhow::anyhow!("Failed to insert MongoDB batch reply: {}", e));
                                }
                            }
                        }
                    }
                }

                if ids.is_empty() {
                    return Ok(());
                }
                let filter = doc! { "_id": { "$in": &ids } };
                // Ack failure may result in redelivery. Enable deduplication middleware to handle duplicates.
                if let Err(e) = collection_clone.delete_many(filter).await {
                    tracing::error!(error = %e, "Failed to bulk-ack/delete MongoDB messages");
                    return Err(anyhow::anyhow!("Failed to bulk-ack/delete MongoDB messages: {}", e));
                } else {
                    trace!(
                        count = ids.len(),
                        "MongoDB messages acknowledged and deleted"
                    );
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok((messages, commit))
    }
}

enum SubscriberStream {
    ChangeStream(Box<ChangeStream<ChangeStreamEvent<Document>>>),
    Polling {
        collection: Collection<Document>,
        last_id: Option<mongodb::bson::Uuid>,
        interval: Duration,
    },
}

pub struct MongoDbSubscriber {
    inner: tokio::sync::Mutex<SubscriberStream>,
    collection_name: String,
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
    pub async fn new(config: &MongoDbConfig, collection_name: &str) -> anyhow::Result<Self> {
        let client = create_client(config).await?;
        let db = client.database(&config.database);
        let collection = db.collection::<Document>(collection_name);

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
                    "Failed to create TTL index on subscriber collection {} : {}",
                    collection_name, e
                );
            }
        }

        // Watch for inserts to treat them as new events.
        let pipeline = [doc! { "$match": { "operationType": "insert" } }];
        let change_stream_result = collection.watch().pipeline(pipeline).await;

        let inner = match change_stream_result {
            Ok(stream) => {
                info!(database = %config.database, collection = %collection_name, "MongoDB subscriber watching for events (Change Stream)");
                SubscriberStream::ChangeStream(Box::new(stream))
            }
            Err(e) if matches!(*e.kind, ErrorKind::Command(ref cmd_err) if cmd_err.code == 40573) =>
            {
                info!("MongoDB is a single instance (ChangeStream support check failed). Falling back to polling for subscriber.");

                // Find the last ID to start consuming from "now"
                let last_doc = collection
                    .find_one(doc! {})
                    .sort(doc! { "_id": -1 })
                    .await?;

                let mut last_id = None;
                if let Some(last_doc) = last_doc {
                    if let Some(Bson::Binary(binary)) = last_doc.get("_id") {
                        if let Ok(uuid) = binary.to_uuid() {
                            last_id = Some(uuid);
                        }
                    }
                }
                SubscriberStream::Polling {
                    collection,
                    last_id,
                    interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
                }
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            inner: tokio::sync::Mutex::new(inner),
            collection_name: collection_name.to_string(),
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbSubscriber {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut inner = self.inner.lock().await;

        match &mut *inner {
            SubscriberStream::ChangeStream(stream) => {
                let event = stream
                    .next()
                    .await
                    .ok_or(ConsumerError::EndOfStream)?
                    .context("Error reading from MongoDB change stream")?;

                let doc = event
                    .full_document
                    .ok_or_else(|| anyhow!("Change stream event missing full_document"))?;
                let msg = match mongodb::bson::from_document::<MongoMessageRaw>(doc.clone()) {
                    Ok(raw_msg) => raw_msg.try_into().unwrap_or(document_to_canonical(doc)?),
                    Err(_) => document_to_canonical(doc)?,
                };

                trace!(message_id = %format!("{:032x}", msg.message_id), collection = %self.collection_name, "Received MongoDB change stream event");
                Ok(ReceivedBatch {
                    messages: vec![msg],
                    commit: Box::new(|_| Box::pin(async { Ok(()) })),
                })
            }
            SubscriberStream::Polling {
                collection,
                last_id,
                interval,
            } => loop {
                let mut filter = doc! {};
                if let Some(id) = last_id {
                    filter = doc! { "_id": { "$gt": id } };
                }

                let mut cursor = collection
                    .find(filter)
                    .sort(doc! { "_id": 1 })
                    .limit(max_messages as i64)
                    .await
                    .map_err(|e| ConsumerError::Connection(e.into()))?;

                let mut messages = Vec::new();
                while let Some(doc_result) = cursor.next().await {
                    let doc = doc_result.map_err(|e| ConsumerError::Connection(e.into()))?;

                    if let Some(Bson::Binary(binary)) = doc.get("_id") {
                        if let Ok(uuid) = binary.to_uuid() {
                            *last_id = Some(uuid);
                        }
                    }

                    let msg = match mongodb::bson::from_document::<MongoMessageRaw>(doc.clone()) {
                        Ok(raw_msg) => raw_msg.try_into().unwrap_or(document_to_canonical(doc)?),
                        Err(_) => document_to_canonical(doc)?,
                    };
                    messages.push(msg);
                }

                if !messages.is_empty() {
                    trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Received batch of MongoDB documents via polling");
                    return Ok(ReceivedBatch {
                        messages,
                        commit: Box::new(|_| Box::pin(async { Ok(()) })),
                    });
                }

                tokio::time::sleep(*interval).await;
            },
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
