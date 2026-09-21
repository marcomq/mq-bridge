//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// A shared, multi-instance deduplication store on MongoDB. State lives one doc per key
/// (`_id` = hex-encoded key, `expireAt` = absolute expiry, `state` = `pending|processed`, and
/// `response` = the stored reply under `replay_response`). A TTL
/// index garbage-collects expired docs, but `reserve` never trusts the sweeper for correctness
/// (it lags up to ~60s): it judges expiry itself via the `expireAt <= now` filter, so a
/// stale-but-unswept doc is still reclaimable. At-least-once, like the local sled store.
struct MongoDedupStore {
    coll: Collection<Document>,
    ttl_seconds: u64,
}

const STATE_FIELD: &str = "state";
const STATE_PENDING: &str = "pending";
const STATE_PROCESSED: &str = "processed";
const RESPONSE_FIELD: &str = "response";

#[async_trait]
impl crate::middleware::deduplication::DedupStore for MongoDedupStore {
    async fn reserve(
        &self,
        key: &[u8],
        now: u64,
    ) -> Result<crate::middleware::deduplication::Reservation, ConsumerError> {
        use crate::middleware::deduplication::{hex_key, Reservation, PENDING_TTL_SECS};
        let id = hex_key(key);
        let now_ms = now as i64 * 1000;
        let now_date = mongodb::bson::DateTime::from_millis(now_ms);
        let pending_date =
            mongodb::bson::DateTime::from_millis(now_ms + PENDING_TTL_SECS as i64 * 1000);
        // Matches only a *missing* or *expired* entry; a live one fails the filter, so the upsert
        // attempts an insert on the existing `_id` and raises E11000.
        let filter = doc! { "_id": &id, "expireAt": { "$lte": now_date } };
        let update = doc! { "$set": { "expireAt": pending_date, STATE_FIELD: STATE_PENDING } };
        let opts = FindOneAndUpdateOptions::builder()
            .upsert(true)
            .return_document(ReturnDocument::Before)
            .build();
        match self
            .coll
            .find_one_and_update(filter, update)
            .with_options(opts)
            .await
        {
            Ok(_) => Ok(Reservation::Claimed),
            Err(e) => {
                // An upserting findAndModify reports the conflict as a command error, not a
                // write error; missing that failed the route on every contested key.
                let is_dup = match &*e.kind {
                    ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) => {
                        w.code == 11000
                    }
                    ErrorKind::Command(c) => c.code == 11000,
                    _ => false,
                };
                if !is_dup {
                    return Err(ConsumerError::Connection(anyhow!(
                        "Deduplication MongoDB reserve failed: {e}"
                    )));
                }
                // A live entry exists. Only a committed one makes this copy a duplicate; a
                // doc that vanished in between is treated as held, so the caller re-checks.
                let existing = self.coll.find_one(doc! { "_id": &id }).await.map_err(|e| {
                    ConsumerError::Connection(anyhow!(
                        "Deduplication MongoDB state lookup failed: {e}"
                    ))
                })?;
                Ok(match existing {
                    // Docs written before `state` existed carry no field and were committed.
                    Some(doc) if doc.get_str(STATE_FIELD).ok() != Some(STATE_PENDING) => {
                        Reservation::Processed
                    }
                    _ => Reservation::InFlight,
                })
            }
        }
    }

    async fn mark_processed(&self, key: &[u8], now: u64) {
        use crate::middleware::deduplication::hex_key;
        let id = hex_key(key);
        let expire_date =
            mongodb::bson::DateTime::from_millis((now as i64 + self.ttl_seconds as i64) * 1000);
        if let Err(e) = self
            .coll
            .update_one(
                doc! { "_id": &id },
                // A plain ack supersedes a reply stored by an earlier, expired processing.
                doc! {
                    "$set": { "expireAt": expire_date, STATE_FIELD: STATE_PROCESSED },
                    "$unset": { RESPONSE_FIELD: "" },
                },
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
        {
            warn!("Failed to mark dedup key processed in MongoDB: {}", e);
        }
    }

    async fn mark_processed_with_response(&self, key: &[u8], now: u64, response: &[u8]) {
        use crate::middleware::deduplication::hex_key;
        let id = hex_key(key);
        let expire_date =
            mongodb::bson::DateTime::from_millis((now as i64 + self.ttl_seconds as i64) * 1000);
        let response = mongodb::bson::Binary {
            subtype: mongodb::bson::spec::BinarySubtype::Generic,
            bytes: response.to_vec(),
        };
        if let Err(e) = self
            .coll
            .update_one(
                doc! { "_id": &id },
                doc! { "$set": {
                    "expireAt": expire_date,
                    STATE_FIELD: STATE_PROCESSED,
                    RESPONSE_FIELD: response,
                } },
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
        {
            warn!("Failed to mark dedup key processed in MongoDB: {}", e);
        }
    }

    async fn stored_response(&self, key: &[u8]) -> Option<Vec<u8>> {
        let id = crate::middleware::deduplication::hex_key(key);
        let doc = self
            .coll
            .find_one(doc! { "_id": &id })
            .projection(doc! { RESPONSE_FIELD: 1 })
            .await
            .map_err(|e| warn!("Failed to read dedup reply from MongoDB: {}", e))
            .ok()??;
        doc.get_binary_generic(RESPONSE_FIELD).ok().cloned()
    }

    async fn release(&self, key: &[u8]) {
        let id = crate::middleware::deduplication::hex_key(key);
        if let Err(e) = self
            .coll
            .delete_one(doc! { "_id": &id, STATE_FIELD: STATE_PENDING })
            .await
        {
            warn!("Failed to release dedup key in MongoDB: {}", e);
        }
    }
}

/// Build a deduplication store on a MongoDB deployment (its own client), selected by a
/// `mongodb://host/db[/collection]` `store:` URL. All instances of a route must share the same
/// URL and collection for dedup to be effective; the collection defaults to `mqb_dedup_<route>`.
pub(crate) async fn build_mongo_dedup_store(
    url: &str,
    database: &str,
    collection: Option<String>,
    ttl_seconds: u64,
    route_name: &str,
) -> anyhow::Result<Arc<dyn crate::middleware::deduplication::DedupStore>> {
    let client = Client::with_uri_str(url)
        .await
        .with_context(|| format!("Failed to connect deduplication store at '{}'", url))?;
    let db = client.database(database);
    let coll_name = collection.unwrap_or_else(|| {
        format!(
            "mqb_dedup_{}",
            crate::checkpoint::sanitize_ident(route_name)
        )
    });
    let coll = db.collection::<Document>(&coll_name);
    // TTL index on the absolute `expireAt` date (expire_after 0 => expire exactly at that time).
    // GC only; correctness is enforced by `reserve` (see MongoDedupStore).
    let index = IndexModel::builder()
        .keys(doc! { "expireAt": 1 })
        .options(
            mongodb::options::IndexOptions::builder()
                .expire_after(Duration::from_secs(0))
                .build(),
        )
        .build();
    if let Err(e) = coll.create_index(index).await {
        warn!(
            "Failed to create TTL index on dedup collection {}: {}",
            coll_name, e
        );
    }
    Ok(Arc::new(MongoDedupStore { coll, ttl_seconds }))
}
