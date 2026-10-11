//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! MongoDB-backed state store, selected by a `store:` URL on the `aggregate` middleware.

use super::*;
use crate::middleware::aggregate::store::{StateStore, StateWrite};

/// Statements per `update` command and keys per `find`.
const CHUNK: usize = 1000;
const DUPLICATE_KEY: i64 = 11000;

/// One document per key: `{_id, state, v}` with `state` as JSON text and `v` counting its
/// writes. A write is an upsert filtered on the expected `v`: when the version moved the
/// filter misses, the upsert tries to insert the existing `_id` and fails with E11000. Each
/// document is written atomically; a batch is not.
struct MongoStateStore {
    db: Database,
    collection: String,
}

fn as_i64(value: Option<&Bson>) -> Option<i64> {
    match value? {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        Bson::Double(n) => Some(*n as i64),
        _ => None,
    }
}

impl MongoStateStore {
    /// Runs one unordered `update` command and returns the statements that lost the race.
    async fn update(&self, writes: &[StateWrite]) -> anyhow::Result<Vec<usize>> {
        let updates: Vec<Document> = writes
            .iter()
            .map(|w| {
                doc! {
                    "q": { "_id": &w.key, "v": w.expected },
                    "u": { "$set": { "state": &w.state, "v": w.expected + 1 } },
                    "upsert": true,
                }
            })
            .collect();
        let command = doc! { "update": &self.collection, "ordered": false, "updates": updates };
        let reply = self.db.run_command(command).await?;
        let mut lost = Vec::new();
        for error in reply
            .get_array("writeErrors")
            .map_or(&[][..], Vec::as_slice)
        {
            let error = error.as_document().context("malformed writeErrors entry")?;
            if as_i64(error.get("code")) != Some(DUPLICATE_KEY) {
                return Err(anyhow!("aggregate MongoDB write failed: {error}"));
            }
            let index = as_i64(error.get("index")).context("writeErrors entry has no index")?;
            lost.push(usize::try_from(index)?);
        }
        if let Some(error) = reply.get("writeConcernError") {
            return Err(anyhow!(
                "aggregate MongoDB write was not acknowledged: {error}"
            ));
        }
        Ok(lost)
    }
}

#[async_trait]
impl StateStore for MongoStateStore {
    async fn load_many(&self, keys: &[String]) -> anyhow::Result<HashMap<String, (String, i64)>> {
        let coll = self.db.collection::<Document>(&self.collection);
        let mut found = HashMap::with_capacity(keys.len());
        for chunk in keys.chunks(CHUNK) {
            let mut cursor = coll.find(doc! { "_id": { "$in": chunk } }).await?;
            while let Some(row) = cursor.next().await {
                let row = row?;
                let version = as_i64(row.get("v")).context("aggregate state has no version")?;
                found.insert(
                    row.get_str("_id")?.to_string(),
                    (row.get_str("state")?.to_string(), version),
                );
            }
        }
        Ok(found)
    }

    async fn store_many(&self, writes: &[StateWrite]) -> anyhow::Result<Vec<usize>> {
        // One command runs on one server thread; chunks of a large batch run side by side.
        let chunks = writes.chunks(CHUNK).map(|chunk| self.update(chunk));
        let mut lost = Vec::new();
        for (n, chunk) in futures::future::join_all(chunks)
            .await
            .into_iter()
            .enumerate()
        {
            lost.extend(chunk?.into_iter().map(|i| n * CHUNK + i));
        }
        Ok(lost)
    }
}

/// Builds a state store on a MongoDB deployment (its own client). All instances of a route
/// must share the same URL and collection.
pub(crate) async fn build_mongo_state_store(
    url: &str,
    database: &str,
    collection: &str,
) -> anyhow::Result<Arc<dyn StateStore>> {
    let client = Client::with_uri_str(url).await.with_context(|| {
        format!(
            "Failed to connect aggregate store at '{}'",
            crate::support::redact::url_password(url)
        )
    })?;
    Ok(Arc::new(MongoStateStore {
        db: client.database(database),
        collection: collection.to_string(),
    }))
}
