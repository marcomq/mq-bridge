//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;

/// A shared, multi-instance deduplication store on MongoDB. State lives one doc per key
/// (`_id` = hex-encoded key, `expireAt` = absolute expiry). A TTL index garbage-collects
/// expired docs, but `reserve` never trusts the sweeper for correctness (it lags up to ~60s):
/// it judges expiry itself via the `expireAt <= now` filter, so a stale-but-unswept doc is
/// still reclaimable. At-least-once, like the local sled store.
struct MongoDedupStore {
    coll: Collection<Document>,
    ttl_seconds: u64,
}

#[async_trait]
impl crate::middleware::deduplication::DedupStore for MongoDedupStore {
    async fn reserve(&self, key: &[u8], now: u64) -> Result<bool, ConsumerError> {
        use crate::middleware::deduplication::{hex_key, PENDING_TTL_SECS};
        let id = hex_key(key);
        let now_ms = now as i64 * 1000;
        let now_date = mongodb::bson::DateTime::from_millis(now_ms);
        let pending_date =
            mongodb::bson::DateTime::from_millis(now_ms + PENDING_TTL_SECS as i64 * 1000);
        // Matches only a *missing* or *expired* entry; a live one fails the filter, so the upsert
        // attempts an insert on the existing `_id` and raises E11000 -> duplicate.
        let filter = doc! { "_id": &id, "expireAt": { "$lte": now_date } };
        let update = doc! { "$set": { "expireAt": pending_date } };
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
            Ok(_) => Ok(false),
            Err(e) => {
                let is_dup = matches!(&*e.kind, ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) if w.code == 11000);
                if is_dup {
                    Ok(true)
                } else {
                    Err(ConsumerError::Connection(anyhow!(
                        "Deduplication MongoDB reserve failed: {e}"
                    )))
                }
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
                doc! { "$set": { "expireAt": expire_date } },
            )
            .with_options(UpdateOptions::builder().upsert(true).build())
            .await
        {
            warn!("Failed to mark dedup key processed in MongoDB: {}", e);
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
