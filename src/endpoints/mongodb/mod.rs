use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{MongoDbConfig, MongoDbFormat};
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, Received, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use futures::StreamExt;
use mongodb::{
    bson::{doc, to_bson, to_document, Bson, Document},
    change_stream::ChangeStream,
    error::ErrorKind,
    options::{
        FindOneAndUpdateOptions, FindOptions, FullDocumentType, ReturnDocument, UpdateOptions,
    },
};
use mongodb::{
    change_stream::event::{ChangeStreamEvent, OperationType, ResumeToken},
    IndexModel,
};
use mongodb::{Client, Collection, Database};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tracing::{info, trace, warn};

#[cfg(feature = "aggregate")]
mod aggregate;
mod consumer;
#[cfg(feature = "dedup")]
mod dedup;
mod publisher;
mod readers;

pub use consumer::MongoDbConsumer;
pub use publisher::MongoDbPublisher;
pub use readers::{MongoDbChangeStreamReader, MongoDbIdReader};

#[cfg(feature = "aggregate")]
pub(crate) use aggregate::build_mongo_state_store;
#[cfg(feature = "dedup")]
pub(crate) use dedup::build_mongo_dedup_store;
pub(crate) use readers::{change_stream_checkpoint, is_change_stream_unsupported};
// Referenced only by the unit tests below.
#[cfg(test)]
pub(crate) use publisher::{tag_outcome, OUTCOME_EXISTED, OUTCOME_INSERTED, OUTCOME_KEY};
#[cfg(test)]
pub(crate) use readers::{decode_resume_token, encode_resume_token, full_document_match};

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
            Bson::String(s) => {
                // Preserve the raw UTF-8 bytes of the string, not a JSON-encoded string
                s.into_bytes().into()
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

/// Payload field → `_id`. Fails fast if the payload isn't JSON, the field is absent, or it's null.
fn extract_id_bson(payload: &[u8], id_field: &str) -> anyhow::Result<Bson> {
    let json: serde_json::Value = serde_json::from_slice(payload)
        .with_context(|| format!("id_field '{}' requires a JSON payload", id_field))?;
    let value = json
        .get(id_field)
        .ok_or_else(|| anyhow!("id_field '{}' not found in payload", id_field))?;
    let bson = to_bson(value)
        .with_context(|| format!("id_field '{}' value is not valid BSON", id_field))?;
    if matches!(bson, Bson::Null) {
        return Err(anyhow!("id_field '{}' resolved to null", id_field));
    }
    // MongoDB forbids an array `_id`.
    if matches!(bson, Bson::Array(_)) {
        return Err(anyhow!(
            "id_field '{}' resolved to an array, which is not a valid _id",
            id_field
        ));
    }
    Ok(bson)
}

fn render_id_bson(message: &CanonicalMessage, template: &CompiledTemplate) -> anyhow::Result<Bson> {
    let rendered = template
        .render_resolved(Some(message))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("id_field template did not fully resolve for this message"))?;
    Ok(Bson::String(
        String::from_utf8(rendered).expect("interpolation renders UTF-8 by construction"),
    ))
}

fn explicit_id_bson(
    message: &CanonicalMessage,
    id_field: Option<&str>,
    id_template: Option<&CompiledTemplate>,
) -> anyhow::Result<Option<Bson>> {
    match (id_field, id_template) {
        (Some(field), None) => extract_id_bson(&message.payload, field).map(Some),
        (None, Some(template)) => render_id_bson(message, template).map(Some),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => unreachable!("MongoDB id sources are validated at startup"),
    }
}

fn message_to_document(
    message: &CanonicalMessage,
    format: &MongoDbFormat,
    id_field: Option<&str>,
    id_template: Option<&CompiledTemplate>,
) -> anyhow::Result<Document> {
    // If request-reply metadata is present, we must use the wrapped format to preserve it,
    // regardless of whether the original format was raw.
    let force_wrapped = message.metadata.contains_key("correlation_id")
        || message.metadata.contains_key("reply_to");

    let explicit_id = explicit_id_bson(message, id_field, id_template)?;

    if !force_wrapped && matches!(format, MongoDbFormat::Raw) {
        if let Ok(mut doc) = serde_json::from_slice::<Document>(&message.payload) {
            if let Some(id) = &explicit_id {
                doc.insert("_id", id.clone());
            }
            return Ok(doc);
        }
        // If parsing fails, fall through to standard wrapping
    }

    let id_uuid = mongodb::bson::Uuid::from_bytes(message.message_id.to_be_bytes());

    let mut metadata = message.metadata.clone();
    // Source/provenance keys are per-hop context, not stored document fields.
    metadata.retain(|key, _| !crate::canonical_message::is_source_metadata_key(key));
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
            // Fallback to binary if not valid JSON
            Bson::Binary(mongodb::bson::Binary {
                subtype: mongodb::bson::spec::BinarySubtype::Generic,
                bytes: message.payload.to_vec(),
            })
        }
    } else if matches!(format, MongoDbFormat::Text) {
        if let Ok(text) = std::str::from_utf8(&message.payload) {
            metadata.insert("type".to_string(), "text".to_string());
            Bson::String(text.to_string())
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

    let id_bson = explicit_id.unwrap_or_else(|| Bson::from(id_uuid));

    Ok(doc! {
        "_id": id_bson,
        "payload": payload_bson,
        "metadata": metadata_doc,
        "locked_until": null,
        "created_at": mongodb::bson::DateTime::now()
    })
}

/// True when `doc` is a wrapped mq-bridge message whose fields are already known to
/// convert cleanly. That guarantee is what lets `parse_mongodb_document` take the fields
/// by value: the raw fallback needs the document intact, so it may only be bypassed when
/// the conversion cannot fail into it.
fn is_wrapped_message(doc: &Document) -> bool {
    if !doc.contains_key("payload") {
        return false;
    }
    if !matches!(doc.get("_id"), Some(Bson::Binary(b)) if b.to_uuid().is_ok()) {
        return false;
    }
    // Non-string metadata values fail `HashMap<String, String>` decoding, which today
    // falls back to the raw path — so those documents must keep taking it.
    match doc.get("metadata") {
        None | Some(Bson::Null) => true,
        Some(Bson::Document(m)) => m.values().all(|v| matches!(v, Bson::String(_))),
        Some(_) => false,
    }
}

fn parse_mongodb_document(mut doc: Document) -> anyhow::Result<CanonicalMessage> {
    // Remove the wrapped fields in place; `from_document` consumes them, so this
    // avoids cloning every document to keep a rarely-used raw fallback.
    if is_wrapped_message(&doc) {
        if let (Some(Bson::Binary(bin)), Some(payload)) = (doc.remove("_id"), doc.remove("payload"))
        {
            if let Ok(id) = bin.to_uuid() {
                let metadata = match doc.remove("metadata") {
                    Some(Bson::Document(m)) => Some(m),
                    _ => None,
                };
                return MongoMessageRaw {
                    id,
                    payload,
                    metadata,
                }
                .try_into();
            }
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
        let doc = message_to_document(&resp, &MongoDbFormat::Normal, None, None).map_err(|e| {
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
/// --- Non-destructive `_id`-cursor reader (arbitrary collections) ---
/// Encodes a BSON `_id` into a portable, tagged string for checkpoint persistence.
/// Supports the homogeneous `_id` types a real collection uses: ObjectId, BSON UUID
/// (subtype 4, as mq-bridge's own publisher writes), and integers. Returns `None` for
/// unsupported types (cursor is then not persisted).
fn encode_id(id: &Bson) -> Option<String> {
    match id {
        Bson::ObjectId(oid) => Some(format!("oid:{}", oid.to_hex())),
        Bson::Binary(bin)
            if bin.subtype == mongodb::bson::spec::BinarySubtype::Uuid
                || bin.subtype == mongodb::bson::spec::BinarySubtype::UuidOld =>
        {
            bin.to_uuid().ok().map(|u| format!("uuid:{}", u))
        }
        Bson::Int64(n) => Some(format!("int:{}", n)),
        Bson::Int32(n) => Some(format!("int:{}", n)),
        Bson::String(s) => Some(format!("str:{}", s)),
        _ => None,
    }
}

/// Checkpoint store backed by a `mqb_cursors` collection in the source database.
struct MongoCollectionCheckpointStore {
    meta: Collection<Document>,
    doc_id: String,
}

#[async_trait]
impl crate::checkpoint::CheckpointStore for MongoCollectionCheckpointStore {
    async fn load(&self) -> anyhow::Result<Option<String>> {
        let doc = self
            .meta
            .find_one(doc! { "_id": self.doc_id.clone() })
            .await?;
        Ok(doc.and_then(|d| d.get_str("last_value").ok().map(|s| s.to_string())))
    }

    async fn save(&self, value: &str) -> anyhow::Result<()> {
        self.meta
            .update_one(
                doc! { "_id": self.doc_id.clone() },
                doc! { "$set": { "last_value": value } },
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await?;
        Ok(())
    }

    async fn clear(&self) -> anyhow::Result<()> {
        self.meta
            .delete_one(doc! { "_id": self.doc_id.clone() })
            .await?;
        Ok(())
    }
}

/// Build a checkpoint store on an **external** MongoDB deployment (its own client), used when
/// `checkpoint_store` is a `mongodb://host/db[/collection]` URL.
pub(crate) async fn build_mongo_checkpoint_store(
    url: &str,
    database: &str,
    collection: Option<String>,
    source_name: &str,
    cursor_id: &str,
) -> anyhow::Result<Arc<dyn crate::checkpoint::CheckpointStore>> {
    let client = Client::with_uri_str(url)
        .await
        .with_context(|| format!("Failed to connect checkpoint store at '{}'", url))?;
    let db = client.database(database);
    let meta_name = match collection {
        Some(collection) => collection,
        None => default_meta_collection(&db, source_name).await?,
    };
    Ok(Arc::new(MongoCollectionCheckpointStore {
        meta: db.collection::<Document>(&meta_name),
        doc_id: crate::checkpoint::checkpoint_key(source_name, cursor_id),
    }))
}

/// The default meta collection for `source_name`. A name that sanitization changed gets a hash
/// suffix, unless a collection under the unsuffixed name already holds cursors.
pub(crate) async fn default_meta_collection(
    db: &mongodb::Database,
    source_name: &str,
) -> anyhow::Result<String> {
    let legacy = crate::checkpoint::default_meta_name(source_name);
    let Some(unique) = crate::checkpoint::disambiguated_meta_name(source_name) else {
        return Ok(legacy);
    };
    let existing = db
        .list_collection_names()
        .filter(doc! { "name": &legacy })
        .await
        .with_context(|| format!("Failed to look up meta collection '{legacy}'"))?;
    Ok(if existing.is_empty() { unique } else { legacy })
}

/// Returns a shared MongoDB client for this connection, building one on first use.
/// The collection/database are handles off the client, so a single client serves all.
async fn create_shared_client(config: &MongoDbConfig) -> anyhow::Result<std::sync::Arc<Client>> {
    let identity = crate::support::connection_registry::connection_identity((
        &config.url,
        &config.username,
        &config.password,
        config.tls.required,
        &config.tls.ca_file,
        &config.tls.cert_file,
        &config.tls.cert_password,
        config.tls.accept_invalid_certs,
    ));
    let config_clone = config.clone();
    crate::support::connection_registry::get_or_create(
        "mongodb-client",
        identity,
        config.shared.unwrap_or(true),
        move || async move { create_client(&config_clone).await },
    )
    .await
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

#[cfg(test)]
mod tests;
