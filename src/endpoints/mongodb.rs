use crate::models::MongoDbConfig;
use crate::traits::{BoxFuture, CommitFunc, MessageConsumer, MessagePublisher};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
use mongodb::{
    bson::{doc, to_document, Binary, Bson, Document},
    change_stream::ChangeStream,
    error::ErrorKind,
};
use mongodb::{
    change_stream::event::ChangeStreamEvent, options::FindOneAndUpdateOptions, IndexModel,
};
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
    message_id: i64,
    payload: Binary,
    metadata: Option<Document>,
}

impl TryFrom<MongoMessageRaw> for CanonicalMessage {
    type Error = anyhow::Error;

    fn try_from(raw: MongoMessageRaw) -> Result<Self, Self::Error> {
        let metadata: Option<HashMap<String, String>> = raw
            .metadata
            .map(mongodb::bson::from_document)
            .transpose()
            .context("Failed to deserialize metadata from BSON document")?;

        Ok(CanonicalMessage {
            message_id: Some(raw.message_id as u64),
            payload: raw.payload.bytes,
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
        Ok(Self { collection })
    }
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, message: CanonicalMessage) -> anyhow::Result<Option<CanonicalMessage>> {
        let object_id = mongodb::bson::oid::ObjectId::new();
        let mut msg_with_metadata = message;
        msg_with_metadata
            .metadata
            .get_or_insert_with(Default::default)
            .insert("mongodb_object_id".to_string(), object_id.to_string());

        if msg_with_metadata.message_id.is_none() {
            // If no message_id is present, generate one from the ObjectId.
            // We combine the 4-byte timestamp with the last 4 bytes, which include
            // the 3-byte incrementing counter. This creates a highly unique ID.
            let oid_bytes = object_id.bytes();
            let mut id_bytes = [0u8; 8];
            id_bytes[0..4].copy_from_slice(&oid_bytes[0..4]); // Timestamp
            id_bytes[4..8].copy_from_slice(&oid_bytes[8..12]); // Last byte of random + 3-byte counter
            msg_with_metadata.message_id = Some(u64::from_be_bytes(id_bytes));
        }
        let message_id_i64: Option<i64> = msg_with_metadata.message_id.map(|id| id as i64);

        // Manually construct the document to handle u64 message_id for BSON.
        // BSON only supports i64, so we do a wrapping conversion.
        let doc = doc! {
            "_id": object_id,
            "message_id": message_id_i64, // Convert u64 to i64
            "payload": Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: msg_with_metadata.payload.clone() }),
            "metadata": to_document(&msg_with_metadata.metadata)?,
            "locked_until": null
        };

        self.collection.insert_one(doc).await?;

        Ok(Some(msg_with_metadata))
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
            polling_interval: Duration::from_millis(100),
        })
    }
}

#[async_trait]
impl MessageConsumer for MongoDbConsumer {
    async fn receive(&mut self) -> anyhow::Result<(CanonicalMessage, CommitFunc)> {
        loop {
            // This outer loop handles both polling and change stream logic.
            if let Some(stream_mutex) = &self.change_stream {
                // --- Change Stream Path ---
                let mut stream = stream_mutex.lock().await;
                if let Some(event_result) = stream.next().await {
                    let event = event_result.context("Error reading from change stream")?;
                    if let Some(doc_id) = event
                        .full_document
                        .as_ref()
                        .and_then(|d| d.get_object_id("_id").ok())
                    {
                        // Attempt to claim the specific document from the event.
                        // Retry a few times to handle replication lag/visibility delays.
                        for _ in 0..3 {
                            if let Some(claimed) =
                                self.try_claim_document(doc! {"_id": doc_id}).await?
                            {
                                return Ok(claimed);
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        // If we failed, another consumer got it. Log and wait for the next event.
                        warn!(mongodb_object_id = %doc_id, "Failed to claim document from change stream event after retries. Another consumer may have claimed it.");
                    }
                    continue; // Go to the next change stream event
                } else {
                    return Err(anyhow!("MongoDB change stream ended unexpectedly"));
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl MongoDbConsumer {
    /// Atomically finds and locks a document matching the filter.
    /// If the filter is empty, it finds any available document.
    /// If a document is successfully claimed, it returns the message and commit function.
    async fn try_claim_document(
        &self,
        extra_filter: Document,
    ) -> anyhow::Result<Option<(CanonicalMessage, CommitFunc)>> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs() as i64;
        let lock_duration_secs = 60;
        let locked_until = now + lock_duration_secs;

        let mut filter = doc! {
            "$or": [
                { "locked_until": { "$exists": false } },
                { "locked_until": null },
                { "locked_until": { "$lt": now } }
            ]
        };
        filter.extend(extra_filter);

        let update = doc! { "$set": { "locked_until": locked_until } };

        let options = FindOneAndUpdateOptions::builder()
            .projection(doc! { "message_id": 1, "payload": 1, "metadata": 1, "_id": 1 })
            .sort(doc! { "_id": 1 }) // Process oldest documents first (FIFO)
            .build();

        match self
            .collection
            .find_one_and_update(filter, update)
            .with_options(options)
            .await
        {
            Ok(Some(doc)) => {
                let raw_msg: MongoMessageRaw = mongodb::bson::from_document(doc.clone())
                    .context("Failed to deserialize MongoDB document")?;
                let msg: CanonicalMessage = raw_msg.try_into()?;

                let object_id = doc
                    .get_object_id("_id")
                    .map_err(|_| anyhow!("Could not find or parse _id in returned document"))?;

                let collection_clone = self.collection.clone();

                let commit = Box::new(move |_response| {
                    Box::pin(async move {
                        match collection_clone.delete_one(doc! { "_id": object_id }).await {
                            Ok(delete_result) => {
                                if delete_result.deleted_count == 1 {
                                    tracing::trace!(mongodb_object_id = %object_id, "MongoDB message acknowledged and deleted");
                                } else {
                                    warn!(mongodb_object_id = %object_id, "Attempted to ack/delete MongoDB message, but it was not found (already deleted?)");
                                }
                            }
                            Err(e) => {
                                tracing::error!(mongodb_object_id = %object_id, error = %e, "Failed to ack/delete MongoDB message");
                            }
                        }
                    }) as BoxFuture<'static, ()>
                });

                Ok(Some((msg, commit)))
            }
            Ok(None) => Ok(None), // No document found or claimed
            Err(e) => Err(e.into()),
        }
    }
}
