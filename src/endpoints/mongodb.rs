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
use std::time::{Duration, SystemTime};
use tracing::{info, warn};

/// A helper struct for deserialization that matches the BSON structure exactly.
/// The payload is read as a BSON Binary type, which we then manually convert.
#[derive(Serialize, Deserialize, Debug)]
struct MongoMessageRaw {
    message_id: i64,
    payload: Binary,
    metadata: Document,
}

impl TryFrom<MongoMessageRaw> for CanonicalMessage {
    type Error = anyhow::Error;

    fn try_from(raw: MongoMessageRaw) -> Result<Self, Self::Error> {
        let metadata = mongodb::bson::from_document(raw.metadata)
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
            .insert("mongodb_object_id".to_string(), object_id.to_string());
        let message_id_i64: Option<i64> = if let Some(id) = msg_with_metadata.message_id {
            Some(id as i64)
        } else {
            None
        };
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
        if let Some(stream_mutex) = &self.change_stream {
            // Change Stream Path: More efficient for replica sets
            let mut stream = stream_mutex.lock().await;
            loop {
                let event = match stream.next().await {
                    Some(Ok(evt)) => evt,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Err(anyhow!("MongoDB change stream ended unexpectedly")),
                };

                let doc_id = match event
                    .full_document
                    .as_ref()
                    .and_then(|d| d.get_object_id("_id").ok())
                {
                    Some(id) => id,
                    None => continue, // Not an insert or document missing _id, wait for next event
                };

                // Attempt to claim this specific document
                if let Some((msg, commit)) = self.try_claim_document(doc! {"_id": doc_id}).await? {
                    return Ok((msg, commit));
                }
                // If we failed to claim it, it means another consumer got it first.
                // We just loop and wait for the next change event.
            }
        } else {
            // Polling Path: Fallback for standalone instances
            loop {
                if let Some((msg, commit)) = self.try_claim_document(doc! {}).await? {
                    return Ok((msg, commit));
                }
                // No document found, wait before polling again
                tokio::time::sleep(self.polling_interval).await;
            }
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
        loop {
            // Atomically find a message that is not locked or whose lock has expired,
            // and lock it for this consumer for 60 seconds.
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
            filter.extend(extra_filter.clone());

            let update = doc! { "$set": { "locked_until": locked_until } };

            // Projection to only return the fields we need, reducing data over the wire.
            let projection = doc! {
                "message_id": 1,
                "payload": 1,
                "metadata": 1,
                "_id": 1
            };

            let options = FindOneAndUpdateOptions::builder()
                .projection(projection)
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

                    // The commit function will delete the message from the collection.
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

                    return Ok(Some((msg, commit)));
                }
                Ok(None) => {
                    // No document found or claimed
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}
