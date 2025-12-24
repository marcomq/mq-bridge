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
use mongodb::{Client, Collection};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

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

/// A publisher that inserts messages into a MongoDB collection.
pub struct MongoDbPublisher {
    collection: Collection<Document>,
}

impl MongoDbPublisher {
    pub async fn new(config: &MongoDbConfig, collection_name: &str) -> anyhow::Result<Self> {
        let client = Client::with_uri_str(&config.url).await?;
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
        Ok(Self { collection })
    }
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        let id_uuid = mongodb::bson::Uuid::from_bytes(message.message_id.to_be_bytes());
        let metadata = to_document(&message.metadata)
            .context("Failed to serialize metadata to BSON document")?;

        // Manually construct the document to handle u64 message_id for BSON.
        // BSON only supports i64, so we do a wrapping conversion.
        let doc = doc! {
            "_id": id_uuid,
            "payload": Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: message.payload.to_vec() }),
            "metadata": metadata,
            "locked_until": null,
            "created_at": mongodb::bson::DateTime::now()
        };
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
        crate::traits::send_batch_helper(self, messages, |publisher, message| {
            Box::pin(publisher.send(message))
        })
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A consumer that receives messages from a MongoDB collection, treating it like a queue.
pub struct MongoDbConsumer {
    collection: Collection<Document>,
    change_stream: Option<tokio::sync::Mutex<ChangeStream<ChangeStreamEvent<Document>>>>,
    polling_interval: Duration,
}

impl MongoDbConsumer {
    pub async fn new(config: &MongoDbConfig, collection_name: &str) -> anyhow::Result<Self> {
        let client = Client::with_uri_str(&config.url).await?;
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
                warn!("MongoDB is a single instance (ChangeStream support check failed). Falling back to polling for consumer.");
                None
            }
            Err(e) => return Err(e.into()), // For any other error, we propagate it.
        };

        info!(database = %config.database, collection = %collection_name, "MongoDB consumer connected and watching for changes");

        Ok(Self {
            collection,
            change_stream,
            polling_interval: Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbConsumer {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        loop {
            // This outer loop handles both polling and change stream logic.
            if let Some(stream_mutex) = &self.change_stream {
                // --- Change Stream Path ---
                let mut stream = stream_mutex.lock().await;
                if let Some(event_result) = stream.next().await {
                    let event = event_result.context("Error reading from change stream")?;
                    if let Some(id_val) = event.full_document.as_ref().and_then(|d| d.get("_id")) {
                        // Attempt to claim the specific document from the event.
                        // Retry a few times to handle replication lag/visibility delays.
                        for _ in 0..3 {
                            if let Some(claimed) =
                                self.try_claim_document(doc! {"_id": id_val}).await?
                            {
                                return Ok(claimed);
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        // If we failed, another consumer got it. Log and wait for the next event.
                        warn!(mongodb_id = %id_val, "Failed to claim document from change stream event after retries. Another consumer may have claimed it.");
                    }
                    continue; // Go to the next change stream event
                } else {
                    return Err(anyhow!("MongoDB change stream ended unexpectedly").into());
                }
            }

            // --- Polling Path ---
            // This path is used for standalone instances or as a fallback.
            // We loop here to immediately retry claiming another document if the first
            // attempt failed due to a race with another consumer.
            if let Some(claimed) = self.try_claim_document(doc! {}).await? {
                return Ok(claimed);
            }
            tokio::time::sleep(self.polling_interval).await;
        }
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            if self.change_stream.is_some() {
                let received = self.receive().await?;
                let commit_batch = crate::traits::into_batch_commit_func(received.commit);
                return Ok(ReceivedBatch {
                    messages: vec![received.message],
                    commit: commit_batch,
                });
            }

            // --- Polling Path (Optimized for Batch) ---
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("System time is before UNIX EPOCH")?
                .as_secs() as i64;
            let lock_duration_secs = 60;
            let locked_until = now + lock_duration_secs;
            let claimed_docs = self
                .find_and_claim_documents(doc! {}, max_messages, now, locked_until)
                .await?;

            if claimed_docs.is_empty() {
                tokio::time::sleep(self.polling_interval).await;
            } else {
                let (messages, commit) = self.process_claimed_documents(claimed_docs)?;
                return Ok(ReceivedBatch { messages, commit });
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
                let raw_msg: MongoMessageRaw = mongodb::bson::from_document(doc)
                    .context("Failed to deserialize MongoDB document")?;
                let msg: CanonicalMessage = raw_msg.try_into()?;

                let collection_clone = self.collection.clone();

                let commit = Box::new(move |_response| {
                    Box::pin(async move {
                        match collection_clone
                            .delete_one(doc! { "_id": id_val.clone() })
                            .await
                        {
                            Ok(delete_result) => {
                                if delete_result.deleted_count == 1 {
                                    tracing::trace!(mongodb_id = %id_val, "MongoDB message acknowledged and deleted");
                                } else {
                                    warn!(mongodb_id = %id_val, "Attempted to ack/delete MongoDB message, but it was not found (already deleted?)");
                                }
                            }
                            Err(e) => {
                                tracing::error!(mongodb_id = %id_val, error = %e, "Failed to ack/delete MongoDB message");
                            }
                        }
                    }) as BoxFuture<'static, ()>
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

        for doc in docs {
            let id_val = doc
                .get("_id")
                .cloned()
                .ok_or_else(|| anyhow!("Document missing _id"))?;
            let raw_msg: MongoMessageRaw = mongodb::bson::from_document(doc)
                .context("Failed to deserialize MongoDB document")?;
            let msg: CanonicalMessage = raw_msg.try_into()?;
            messages.push(msg);

            ids.push(id_val);
        }

        let collection_clone = self.collection.clone();
        let commit = Box::new(move |_response| {
            Box::pin(async move {
                if ids.is_empty() {
                    return;
                }
                let filter = doc! { "_id": { "$in": &ids } };
                if let Err(e) = collection_clone.delete_many(filter).await {
                    tracing::error!(error = %e, "Failed to bulk-ack/delete MongoDB messages");
                } else {
                    tracing::trace!(
                        count = ids.len(),
                        "MongoDB messages acknowledged and deleted"
                    );
                }
            }) as BoxFuture<'static, ()>
        });

        Ok((messages, commit))
    }
}

enum SubscriberStream {
    ChangeStream(ChangeStream<ChangeStreamEvent<Document>>),
    Polling {
        collection: Collection<Document>,
        last_id: Option<mongodb::bson::Uuid>,
        interval: Duration,
    },
}

pub struct MongoDbSubscriber {
    inner: tokio::sync::Mutex<SubscriberStream>,
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
        let client = Client::with_uri_str(&config.url).await?;
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
                SubscriberStream::ChangeStream(stream)
            }
            Err(e) if matches!(*e.kind, ErrorKind::Command(ref cmd_err) if cmd_err.code == 40573) =>
            {
                warn!("MongoDB is a single instance (ChangeStream support check failed). Falling back to polling for subscriber.");

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
                let raw_msg: MongoMessageRaw = mongodb::bson::from_document(doc)
                    .context("Failed to deserialize MongoDB document")?;
                let msg: CanonicalMessage = raw_msg.try_into()?;

                Ok(ReceivedBatch {
                    messages: vec![msg],
                    commit: Box::new(|_| Box::pin(async {})),
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

                    let raw_msg: MongoMessageRaw = mongodb::bson::from_document(doc)
                        .context("Failed to deserialize MongoDB document")?;
                    messages.push(raw_msg.try_into()?);
                }

                if !messages.is_empty() {
                    return Ok(ReceivedBatch {
                        messages,
                        commit: Box::new(|_| Box::pin(async {})),
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
