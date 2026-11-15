//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `load_mode: replace`: a drain writes into a staging collection of its own that replaces the
//! target with one `renameCollection` when the route completes. Readers see the old documents
//! until then. A lease in [`LEASE_COLLECTION`], renewed while the load runs, tells a later load
//! which staging collections a crashed one left behind.

use super::*;
use crate::traits::DisconnectOutcome;
use tracing::{info, warn};

/// MongoDB's code for a collection that does not exist.
const NAMESPACE_NOT_FOUND: i32 = 26;
/// One document per running load: `_id` is its staging collection, `expires_at` server time.
pub(super) const LEASE_COLLECTION: &str = "mqb_load_leases";
const LEASE: Duration = Duration::from_secs(60);
const LEASE_RENEWAL: Duration = Duration::from_secs(15);

#[derive(Default)]
struct LoadState {
    started: bool,
    /// Why the load can no longer replace the target; every later write fails with it.
    poisoned: Option<String>,
    finished: bool,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

pub(super) struct StagedLoad {
    client: Client,
    db: Database,
    target: String,
    staging: String,
    state: tokio::sync::Mutex<LoadState>,
}

impl StagedLoad {
    pub(super) fn new(client: Client, db: Database, target: &str) -> Self {
        Self {
            client,
            db,
            target: target.to_string(),
            staging: format!("{}{:032x}", staging_prefix(target), fast_uuid_v7::gen_id()),
            state: Default::default(),
        }
    }

    pub(super) fn staging(&self) -> &str {
        &self.staging
    }

    /// Runs one write into the staging collection, creating it on the first one. Any failure
    /// poisons the load and stops the route, instead of replacing the target with missing documents.
    pub(super) async fn write<T>(
        &self,
        write: impl std::future::Future<Output = Result<T, PublisherError>>,
    ) -> Result<T, PublisherError> {
        {
            let mut state = self.state.lock().await;
            if state.finished {
                return Err(PublisherError::Connection(anyhow!(
                    "load_mode: the load was already finished"
                )));
            }
            if let Some(reason) = &state.poisoned {
                return Err(PublisherError::Connection(anyhow!(
                    "load_mode: an earlier write of this load failed, the target stays unchanged: {reason}"
                )));
            }
            if !state.started {
                state.started = true;
                if let Err(e) = self.create_staging().await {
                    return Err(Self::fail(&mut state, e));
                }
                state.heartbeat = Some(tokio::spawn(renew_lease(
                    self.leases(),
                    self.staging.clone(),
                )));
            }
        }
        match write.await {
            Ok(value) => Ok(value),
            Err(PublisherError::Retryable(e))
            | Err(PublisherError::NonRetryable(e))
            | Err(PublisherError::Connection(e)) => {
                Err(Self::fail(&mut *self.state.lock().await, e))
            }
        }
    }

    fn fail(state: &mut LoadState, e: anyhow::Error) -> PublisherError {
        state.poisoned.get_or_insert_with(|| format!("{e:#}"));
        PublisherError::Connection(
            e.context("load_mode: write failed, the target collection stays unchanged"),
        )
    }

    fn leases(&self) -> Collection<Document> {
        self.db.collection(LEASE_COLLECTION)
    }

    /// Takes the lease, drops what crashed loads into the target left behind, and creates the
    /// staging collection with the target's options (validator, collation, capped size).
    async fn create_staging(&self) -> anyhow::Result<()> {
        self.leases()
            .update_one(
                doc! { "_id": &self.staging },
                vec![doc! { "$set": { "target": &self.target, "expires_at": lease_expiry() } }],
            )
            .upsert(true)
            .await
            .context("load_mode: taking the load's lease failed")?;
        self.drop_abandoned().await?;
        let mut create = doc! { "create": &self.staging };
        let listed = self
            .db
            .run_command(doc! { "listCollections": 1, "filter": { "name": &self.target } })
            .await?;
        if let Some(spec) = first_batch(&listed)?.first() {
            let spec = spec
                .as_document()
                .context("listCollections returned a non-document")?;
            if spec.get_str("type").unwrap_or("collection") != "collection" {
                anyhow::bail!(
                    "load_mode: '{}' is not a plain collection; it cannot be replaced",
                    self.target
                );
            }
            if let Ok(options) = spec.get_document("options") {
                create.extend(options.clone());
            }
        }
        self.db
            .run_command(create)
            .await
            .with_context(|| format!("load_mode: creating '{}' failed", self.staging))?;
        Ok(())
    }

    async fn drop_staging(&self) -> mongodb::error::Result<()> {
        self.db.collection::<Document>(&self.staging).drop().await
    }

    /// Drops the target's staging collections whose load holds no live lease. The lease was
    /// taken first, so two loads starting together see each other's.
    async fn drop_abandoned(&self) -> anyhow::Result<()> {
        let prefix = staging_prefix(&self.target);
        let staged = self
            .db
            .list_collection_names()
            .filter(doc! { "name": { "$regex": format!("^{}", regex_escape(&prefix)) } })
            .await?;
        let live: Vec<String> = self
            .leases()
            .find(doc! { "_id": { "$in": &staged }, "$expr": { "$gt": ["$expires_at", "$$NOW"] } })
            .projection(doc! { "_id": 1 })
            .await?
            .filter_map(|lease| async move { lease.ok()?.get_str("_id").ok().map(str::to_string) })
            .collect()
            .await;
        for name in staged.iter().filter(|name| !live.contains(name)) {
            warn!(collection = %name, "load_mode: dropping the staging collection of a load that did not finish");
            self.db.collection::<Document>(name).drop().await?;
        }
        self.leases()
            .delete_many(
                doc! { "target": &self.target, "$expr": { "$lte": ["$expires_at", "$$NOW"] } },
            )
            .await?;
        Ok(())
    }

    /// Ends the heartbeat and gives the lease back.
    async fn release(&self, state: &mut LoadState) {
        if let Some(heartbeat) = state.heartbeat.take() {
            heartbeat.abort();
        }
        if state.started {
            if let Err(e) = self
                .leases()
                .delete_one(doc! { "_id": &self.staging })
                .await
            {
                warn!(collection = %self.staging, error = %e, "load_mode: releasing the lease failed; it expires by itself");
            }
        }
    }

    /// Copies the target's indexes onto the staging collection, then swaps it in.
    async fn swap(&self) -> anyhow::Result<()> {
        let listed = match self
            .db
            .run_command(doc! { "listIndexes": &self.target })
            .await
        {
            Ok(listed) => Some(listed),
            Err(e) if matches!(*e.kind, ErrorKind::Command(ref c) if c.code == NAMESPACE_NOT_FOUND) => {
                None
            }
            Err(e) => return Err(anyhow!(e).context("listing the target's indexes failed")),
        };
        let mut indexes = Vec::new();
        if let Some(listed) = &listed {
            for index in first_batch(listed)? {
                let Some(index) = index.as_document() else {
                    continue;
                };
                // Both exist on the staging collection already: `_id_` always, a clustered one from `create`.
                if index.get_str("name").ok() == Some("_id_")
                    || index.get_bool("clustered").unwrap_or(false)
                {
                    continue;
                }
                let mut index = index.clone();
                index.remove("v");
                index.remove("ns");
                indexes.push(index);
            }
        }
        if !indexes.is_empty() {
            self.db
                .run_command(doc! { "createIndexes": &self.staging, "indexes": indexes })
                .await
                .context("building the target's indexes on the loaded documents failed")?;
        }
        let db = self.db.name();
        self.client
            .database("admin")
            .run_command(doc! {
                "renameCollection": format!("{db}.{}", self.staging),
                "to": format!("{db}.{}", self.target),
                "dropTarget": true,
            })
            .await
            .context("renameCollection failed")?;
        Ok(())
    }

    /// Swaps a completed load in; drops the staging collection otherwise.
    pub(super) async fn finish(&self) -> anyhow::Result<()> {
        let outcome = crate::traits::disconnect_outcome().unwrap_or(DisconnectOutcome::Stopped);
        let mut state = self.state.lock().await;
        state.finished = true;
        let result = self.swap_or_discard(outcome, &mut state).await;
        self.release(&mut state).await;
        result
    }

    async fn swap_or_discard(
        &self,
        outcome: DisconnectOutcome,
        state: &mut LoadState,
    ) -> anyhow::Result<()> {
        let target = &self.target;
        match (outcome, state.poisoned.take(), state.started) {
            (DisconnectOutcome::Completed, None, true) => match self.swap().await {
                Ok(()) => {
                    info!(collection = %target, "load_mode: load swapped in");
                    Ok(())
                }
                Err(e) => {
                    self.discard().await;
                    Err(e.context(format!(
                        "load_mode: replacing '{target}' failed, it is unchanged"
                    )))
                }
            },
            (DisconnectOutcome::Completed, None, false) => {
                warn!(collection = %target, "load_mode replace: the source was empty, the collection is left unchanged");
                Ok(())
            }
            (outcome, poisoned, started) => {
                if started {
                    self.discard().await;
                }
                warn!(collection = %target, ?outcome, "load_mode: load discarded, the collection is unchanged");
                match (outcome, poisoned) {
                    (DisconnectOutcome::Completed, Some(reason)) => Err(anyhow!(
                        "load_mode: a write into '{target}' failed, the load was discarded: {reason}"
                    )),
                    _ => Ok(()),
                }
            }
        }
    }

    async fn discard(&self) {
        if let Err(e) = self.drop_staging().await {
            warn!(collection = %self.staging, error = %e, "load_mode: dropping the staging collection failed; the next load drops it");
        }
    }
}

impl Drop for StagedLoad {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.state.get_mut().heartbeat.take() {
            heartbeat.abort();
        }
    }
}

fn staging_prefix(target: &str) -> String {
    format!("{target}_mqb_load_")
}

/// Server time plus [`LEASE`], so clients with skewed clocks agree on expiry.
fn lease_expiry() -> Document {
    doc! { "$add": ["$$NOW", LEASE.as_millis() as i64] }
}

async fn renew_lease(leases: Collection<Document>, staging: String) {
    loop {
        tokio::time::sleep(LEASE_RENEWAL).await;
        let renewed = leases
            .update_one(
                doc! { "_id": &staging },
                vec![doc! { "$set": { "expires_at": lease_expiry() } }],
            )
            .await;
        if let Err(e) = renewed {
            warn!(collection = %staging, error = %e, "load_mode: renewing the load's lease failed");
        }
    }
}

fn regex_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escape = !c.is_alphanumeric() && c != '_';
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

/// The documents of a command's single-batch cursor reply.
fn first_batch(reply: &Document) -> anyhow::Result<&Vec<Bson>> {
    Ok(reply.get_document("cursor")?.get_array("firstBatch")?)
}
