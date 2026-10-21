//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use super::*;
use crate::support::lookup_batch::{self, ListQuery};
use mongodb::options::UpdateModifications;

pub struct MongoDbPublisher {
    collection: Collection<Document>,
    db: Database,
    // Retains the shared registry entry so concurrent publishers reuse this client/pool.
    _shared_client: std::sync::Arc<Client>,
    collection_name: String,
    request_reply: bool,
    request_timeout: Duration,
    reply_polling_interval: Duration,
    format: MongoDbFormat,
    id_field: Option<String>,
    id_template: Option<CompiledTemplate>,
    report_outcome: bool,
    find: Option<CompiledTemplate>,
    /// Set when `find` holds `"field": {"$in": [${…}]}`: one query answers a whole batch.
    find_list: Option<(ListQuery, CompiledTemplate)>,
    aggregate_query: Option<CompiledTemplate>,
    /// Set when `aggregate_query` holds `"field": {"$in": [${…}]}`: one aggregation answers a batch.
    aggregate_list: Option<(ListQuery, CompiledTemplate)>,
    /// Opt-in write: upsert this update on the `find` match and answer with the result.
    update: Option<CompiledTemplate>,
    /// Set by `update_batch_field`: a key's messages share one `update` call.
    update_batch_field: Option<String>,
}

/// Metadata key carrying the insert outcome when `report_outcome` is enabled.
pub(crate) const OUTCOME_KEY: &str = "mongodb.outcome";
pub(crate) const OUTCOME_INSERTED: &str = "inserted";
pub(crate) const OUTCOME_EXISTED: &str = "existed";
/// Metadata key telling whether a `find` matched a document.
const FOUND_KEY: &str = "mongodb.found";
/// Distinct `update` keys written in parallel per batch; one key's messages run in order.
const UPDATE_CONCURRENCY: usize = 16;
/// Most messages of one key folded into one `update` call; bounds the snapshots it stores.
const MAX_FOLDED_UPDATES: usize = 64;

impl MongoDbPublisher {
    pub async fn new(config: &MongoDbConfig) -> anyhow::Result<Self> {
        let id_template = config
            .id_field
            .as_deref()
            .filter(|value| value.contains("${"))
            .map(|template| {
                let compiled = CompiledTemplate::compile(template, None)
                    .context("invalid MongoDB `id_field` template")?;
                if !compiled.is_dynamic() || !compiled.has_only_replay_stable_tokens() {
                    anyhow::bail!(
                        "MongoDB `id_field` template must contain only replay-stable payload or metadata tokens"
                    );
                }
                Ok(compiled)
            })
            .transpose()?;
        let id_field = config
            .id_field
            .as_ref()
            .filter(|value| !value.contains("${"))
            .cloned();
        let collection_name = config
            .collection
            .as_deref()
            .ok_or_else(|| anyhow!("Collection name is required for MongoDB publisher"))?;
        let find = config
            .find
            .as_deref()
            .map(|t| {
                CompiledTemplate::compile(t, Some("application/json"))
                    .context("invalid MongoDB `find` template")
            })
            .transpose()?;
        let find_list = list_query(config.find.as_deref(), "find")?;
        let aggregate_query = config
            .aggregate_query
            .as_deref()
            .map(|t| {
                CompiledTemplate::compile(t, Some("application/json"))
                    .context("invalid MongoDB `aggregate_query` template")
            })
            .transpose()?;
        let aggregate_list = list_query(config.aggregate_query.as_deref(), "aggregate_query")?;
        if let Some((list, _)) = &aggregate_list {
            check_batch_pipeline(list)?;
        }
        if aggregate_query.is_some() && (find.is_some() || config.update.is_some()) {
            anyhow::bail!("MongoDB `aggregate_query` cannot be combined with `find` or `update`");
        }
        let update = config
            .update
            .as_deref()
            .map(|t| {
                CompiledTemplate::compile(t, Some("application/json"))
                    .context("invalid MongoDB `update` template")
            })
            .transpose()?;
        if update.is_some() && find.is_none() {
            anyhow::bail!("MongoDB `update` requires `find`");
        }
        if update.is_some() && find_list.is_some() {
            anyhow::bail!("MongoDB `update` needs one key per message; `find` must not use `$in`");
        }
        let update_batch_field = config.update_batch_field.clone();
        if let Some(field) = &update_batch_field {
            if update.is_none() {
                anyhow::bail!("MongoDB `update_batch_field` requires `update`");
            }
            if !field.starts_with("_mqb") || field.contains('.') {
                anyhow::bail!(
                    "MongoDB `update_batch_field` must be a top-level field name starting with `_mqb`"
                );
            }
        }
        let shared_client = create_shared_client(config).await?;
        let client = (*shared_client).clone();
        let db = client.database(&config.database);
        if find.is_some() || aggregate_query.is_some() {
            if update.is_some() {
                info!(database = %config.database, collection = %collection_name, "MongoDB find publisher connected; `update` writes on every lookup");
            } else if aggregate_query.is_some() {
                info!(database = %config.database, collection = %collection_name, "MongoDB aggregate_query publisher connected");
            } else {
                info!(database = %config.database, collection = %collection_name, "MongoDB find publisher connected");
            }
            return Ok(Self {
                collection: db.collection(collection_name),
                db,
                _shared_client: shared_client,
                collection_name: collection_name.to_string(),
                request_reply: false,
                request_timeout: Duration::ZERO,
                reply_polling_interval: Duration::ZERO,
                format: config.format.clone(),
                id_field: None,
                id_template: None,
                report_outcome: false,
                find,
                find_list,
                aggregate_query,
                aggregate_list,
                update,
                update_batch_field,
            });
        }

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
            _shared_client: shared_client,
            collection_name: collection_name.to_string(),
            request_reply: config.request_reply,
            request_timeout: Duration::from_millis(config.request_timeout_ms.unwrap_or(30000)),
            reply_polling_interval: Duration::from_millis(config.reply_polling_ms.unwrap_or(50)),
            format: config.format.clone(),
            id_field,
            id_template,
            report_outcome: config.report_outcome,
            find: None,
            find_list: None,
            aggregate_query: None,
            aggregate_list: None,
            update: None,
            update_batch_field: None,
        })
    }

    /// Runs the rendered `find` filter and answers with the first match, or an empty payload.
    async fn find_one(
        &self,
        template: &CompiledTemplate,
        message: &CanonicalMessage,
    ) -> Result<Sent, PublisherError> {
        if let Some((list, element)) = &self.find_list {
            let found = self
                .find_many(list, element, std::slice::from_ref(message))
                .await?
                .pop()
                .flatten();
            let payload = match &found {
                Some(doc) => {
                    serde_json::to_vec(doc).map_err(|e| PublisherError::NonRetryable(e.into()))?
                }
                None => Vec::new(),
            };
            let mut response = CanonicalMessage::new(payload, Some(message.message_id));
            response
                .metadata
                .insert(FOUND_KEY.to_string(), found.is_some().to_string());
            return Ok(Sent::Response(response));
        }
        let found = match &self.update {
            // A message without a key writes nothing: it would share one counter with all such.
            Some(update) => match template.render_resolved(Some(message)) {
                // One message has nothing to fold: the plain update is faster.
                Some(rendered) => {
                    self.find_and_update(find_filter(&rendered)?, update, message)
                        .await?
                }
                None => None,
            },
            None => self
                .collection
                .find_one(find_filter(&template.render(Some(message)))?)
                .await
                .map_err(|e| PublisherError::Retryable(e.into()))?,
        };
        let payload = match &found {
            Some(doc) => serde_json::to_vec(&Bson::Document(doc.clone()).into_relaxed_extjson())
                .map_err(|e| PublisherError::NonRetryable(e.into()))?,
            None => Vec::new(),
        };
        let mut response = CanonicalMessage::new(payload, Some(message.message_id));
        response
            .metadata
            .insert(FOUND_KEY.to_string(), found.is_some().to_string());
        Ok(Sent::Response(response))
    }

    /// Upserts the rendered `update` on `filter` and returns the document after the update.
    async fn find_and_update(
        &self,
        filter: Document,
        update: &CompiledTemplate,
        message: &CanonicalMessage,
    ) -> Result<Option<Document>, PublisherError> {
        let update = update_modifications(&update.render(Some(message)))?;
        let options = FindOneAndUpdateOptions::builder()
            .upsert(true)
            .return_document(ReturnDocument::After)
            .build();
        let doc = self
            .collection
            .find_one_and_update(filter, update)
            .with_options(options)
            .await
            .map_err(update_error)?;
        // Snapshots left by an earlier folded batch are not part of the answer.
        Ok(doc.map(|mut doc| {
            if let Some(field) = &self.update_batch_field {
                doc.remove(field);
            }
            doc
        }))
    }

    /// Answers every message with its updated document: distinct keys in parallel, the
    /// messages of one key in order, so each sees the state its predecessors left.
    async fn update_many(
        &self,
        find: &CompiledTemplate,
        update: &CompiledTemplate,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let mut groups: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
        let mut by_filter: HashMap<Vec<u8>, usize> = HashMap::new();
        // Keyless messages join no group and stay unanswered, like `find_many`'s.
        for (i, message) in messages.iter().enumerate() {
            let Some(rendered) = find.render_resolved(Some(message)) else {
                continue;
            };
            let group = *by_filter.entry(rendered.clone()).or_insert_with(|| {
                groups.push((rendered, Vec::new()));
                groups.len() - 1
            });
            groups[group].1.push(i);
        }
        let failed = &std::sync::atomic::AtomicBool::new(false);
        let folded_ids = &std::sync::Mutex::new(Vec::new());
        let mut answers = futures::stream::iter(groups)
            .map(|(rendered, indices)| async move {
                // After a failure, keys not yet started stay unwritten for the retry.
                if failed.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(Vec::new());
                }
                let mut out = Vec::with_capacity(indices.len());
                let filter = match find_filter(&rendered) {
                    Ok(filter) => filter,
                    Err(e) => {
                        let e = rejected_in_batch(e)?;
                        warn!(error = %e, "MongoDB `update` lookup: invalid filter; answering not found");
                        out.extend(indices.into_iter().map(|i| (i, None)));
                        return Ok(out);
                    }
                };
                for chunk in indices.chunks(MAX_FOLDED_UPDATES) {
                    let fold = self.update_batch_field.as_ref().filter(|_| chunk.len() > 1);
                    if let Some(field) = fold {
                        let batch: Vec<&CanonicalMessage> =
                            chunk.iter().map(|&i| &messages[i]).collect();
                        match self.update_folded(field, &filter, update, &batch).await {
                            Some(Ok(docs)) => {
                                if let Some(id) = docs.last().and_then(|d| d.as_ref()?.get("_id")) {
                                    folded_ids.lock().unwrap().push(id.clone());
                                }
                                out.extend(chunk.iter().copied().zip(
                                    docs.into_iter()
                                        .map(|d| d.map(|d| Bson::Document(d).into_relaxed_extjson())),
                                ));
                                continue;
                            }
                            // Nothing was written: redo it message by message to find the culprit.
                            Some(Err(e)) => {
                                rejected_in_batch(e)?;
                            }
                            None => {}
                        }
                    }
                    for &i in chunk {
                        let doc = match self.find_and_update(filter.clone(), update, &messages[i]).await {
                            Ok(doc) => doc,
                            // One rejected message must neither stall nor drop the rest of the batch.
                            Err(e) => {
                                let e = rejected_in_batch(e)?;
                                warn!(error = %e, "MongoDB `update` lookup rejected; answering not found");
                                None
                            }
                        };
                        out.push((i, doc.map(|d| Bson::Document(d).into_relaxed_extjson())));
                    }
                }
                Ok::<_, PublisherError>(out)
            })
            .buffer_unordered(UPDATE_CONCURRENCY);
        let mut results = vec![None; messages.len()];
        let mut error = None;
        // Keys already in flight finish rather than being cancelled mid-write.
        while let Some(group) = answers.next().await {
            match group {
                Ok(group) => group.into_iter().for_each(|(i, doc)| results[i] = doc),
                Err(e) => {
                    failed.store(true, std::sync::atomic::Ordering::Relaxed);
                    error.get_or_insert(e);
                }
            }
        }
        drop(answers);
        if let Some(field) = &self.update_batch_field {
            let ids = std::mem::take(&mut *folded_ids.lock().unwrap());
            self.clear_snapshots(field, ids).await;
        }
        error.map_or(Ok(results), Err)
    }

    /// Removes the snapshots folded writes left, so no intermediate value outlives its batch.
    /// Best effort: the next folded write of a key clears them too.
    async fn clear_snapshots(&self, field: &str, ids: Vec<Bson>) {
        if ids.is_empty() {
            return;
        }
        let filter = doc! { "_id": { "$in": ids }, field: { "$exists": true } };
        if let Err(e) = self
            .collection
            .update_many(filter, doc! { "$unset": { field: "" } })
            .await
        {
            warn!(error = %e, field, "MongoDB `update` batch could not clear its snapshots");
        }
    }

    /// Runs a key's messages, in order, as one atomic `update` call that records each one's
    /// document in `field`. `None` when an update cannot be chained; nothing is written then.
    async fn update_folded(
        &self,
        field: &str,
        filter: &Document,
        update: &CompiledTemplate,
        messages: &[&CanonicalMessage],
    ) -> Option<Result<Vec<Option<Document>>, PublisherError>> {
        let mut chained = Vec::with_capacity(messages.len());
        let mut touched: Vec<String> = Vec::new();
        for message in messages {
            let parsed = update_modifications(&update.render(Some(message))).ok()?;
            let (steps, fields) = chainable_steps(parsed, field)?;
            for f in fields {
                if !touched.contains(&f) {
                    touched.push(f);
                }
            }
            chained.push(steps);
        }
        // One call checks `filter` once; a guard on a written field must be rechecked per message.
        if filter_reads(filter, &touched) {
            return None;
        }
        let options = FindOneAndUpdateOptions::builder()
            .upsert(true)
            .return_document(ReturnDocument::After)
            .build();
        let result = self
            .collection
            .find_one_and_update(filter.clone(), folded_update(field, &touched, chained))
            .with_options(options)
            .await
            .map_err(update_error);
        // Past this point the write happened, so a result that does not split is not redone.
        Some(result.map(
            |doc| match doc.and_then(|doc| unfold(doc, field, &touched, messages.len())) {
                Some(docs) => docs.into_iter().map(Some).collect(),
                None => {
                    warn!(
                        field,
                        "MongoDB `update` batch lost its snapshots; answering not found"
                    );
                    vec![None; messages.len()]
                }
            },
        ))
    }

    /// Answers every message with one `$in` query per chunk of distinct keys.
    async fn find_many(
        &self,
        list: &ListQuery,
        element: &CompiledTemplate,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let (elements, keys) = list_keys(element, messages);
        let mut records = Vec::new();
        for chunk in lookup_batch::distinct(&keys).chunks(lookup_batch::MAX_KEYS_PER_QUERY) {
            let listed: Vec<&str> = chunk
                .iter()
                .filter_map(|&i| elements[i].as_deref())
                .collect();
            let rendered = format!("{}{}{}", list.before, listed.join(", "), list.after);
            let filter = find_filter(rendered.as_bytes())?;
            let mut cursor = self
                .collection
                .find(filter)
                .await
                .map_err(|e| PublisherError::Retryable(e.into()))?;
            let wanted: std::collections::HashSet<&str> =
                chunk.iter().filter_map(|&i| keys[i].as_deref()).collect();
            let mut matched = std::collections::HashSet::with_capacity(wanted.len());
            // Keep the first document per key and stop once every key has one.
            while let Some(doc) = cursor.next().await {
                let doc = doc.map_err(|e| PublisherError::Retryable(e.into()))?;
                let record = Bson::Document(doc).into_relaxed_extjson();
                match lookup_batch::field(&record, &list.key) {
                    // `answer` reports the missing key field.
                    None => records.push(record),
                    Some(v) => {
                        if let Some(k) = lookup_batch::key_of(v) {
                            if wanted.contains(k.as_str()) && matched.insert(k) {
                                records.push(record);
                            }
                        }
                    }
                }
                if matched.len() == wanted.len() {
                    break;
                }
            }
        }
        lookup_batch::answer(&keys, records, &list.key).map_err(PublisherError::NonRetryable)
    }

    /// Runs the rendered `aggregate_query` and answers with its first result, or an empty payload.
    async fn aggregate_one(
        &self,
        template: &CompiledTemplate,
        message: &CanonicalMessage,
    ) -> Result<Sent, PublisherError> {
        let found = match &self.aggregate_list {
            Some((list, element)) => self
                .aggregate_many(list, element, std::slice::from_ref(message))
                .await?
                .pop()
                .flatten(),
            None => {
                let stages = pipeline_stages(&template.render(Some(message)))?;
                let mut cursor = self
                    .collection
                    .aggregate(stages)
                    .batch_size(1)
                    .await
                    .map_err(query_error)?;
                match cursor.next().await {
                    Some(doc) => {
                        Some(Bson::Document(doc.map_err(query_error)?).into_relaxed_extjson())
                    }
                    None => None,
                }
            }
        };
        let payload = match &found {
            Some(doc) => {
                serde_json::to_vec(doc).map_err(|e| PublisherError::NonRetryable(e.into()))?
            }
            None => Vec::new(),
        };
        let mut response = CanonicalMessage::new(payload, Some(message.message_id));
        response
            .metadata
            .insert(FOUND_KEY.to_string(), found.is_some().to_string());
        Ok(Sent::Response(response))
    }

    /// Answers every message with one aggregation per chunk of distinct keys. A result is
    /// matched by the `$in` field, or by `_id` when it has none (the key of a `$group`).
    async fn aggregate_many(
        &self,
        list: &ListQuery,
        element: &CompiledTemplate,
        messages: &[CanonicalMessage],
    ) -> Result<Vec<Option<serde_json::Value>>, PublisherError> {
        let (elements, keys) = list_keys(element, messages);
        let mut by_key: HashMap<String, serde_json::Value> = HashMap::new();
        for chunk in lookup_batch::distinct(&keys).chunks(lookup_batch::MAX_KEYS_PER_QUERY) {
            let listed: Vec<&str> = chunk
                .iter()
                .filter_map(|&i| elements[i].as_deref())
                .collect();
            let rendered = format!("{}{}{}", list.before, listed.join(", "), list.after);
            let mut cursor = self
                .collection
                .aggregate(pipeline_stages(rendered.as_bytes())?)
                .await
                .map_err(query_error)?;
            let wanted: std::collections::HashSet<&str> =
                chunk.iter().filter_map(|&i| keys[i].as_deref()).collect();
            let mut matched = 0;
            while let Some(doc) = cursor.next().await {
                let record = Bson::Document(doc.map_err(query_error)?).into_relaxed_extjson();
                let key = lookup_batch::field(&record, &list.key)
                    .or_else(|| record.get("_id"))
                    .ok_or_else(|| {
                        PublisherError::NonRetryable(anyhow!(
                            "lookup: an `aggregate_query` result has neither `{}` nor `_id`; keep one, it is how results are matched to messages",
                            list.key
                        ))
                    })?;
                if let Some(k) = lookup_batch::key_of(key) {
                    if wanted.contains(k.as_str()) && !by_key.contains_key(&k) {
                        by_key.insert(k, record);
                        matched += 1;
                    }
                }
                if matched == wanted.len() {
                    break;
                }
            }
        }
        Ok(keys
            .iter()
            .map(|k| k.as_ref().and_then(|k| by_key.get(k).cloned()))
            .collect())
    }

    async fn recover_correlation_id_from_duplicate(
        &self,
        message: &mut CanonicalMessage,
    ) -> Result<(), PublisherError> {
        // Look up by the same `_id` message_to_document wrote: the id_field value when
        // configured, else the message_id UUID. Otherwise an explicit-id duplicate is
        // never found and the request retries forever.
        let id_bson =
            match explicit_id_bson(message, self.id_field.as_deref(), self.id_template.as_ref())
                .map_err(PublisherError::NonRetryable)?
            {
                Some(id) => id,
                None => Bson::from(mongodb::bson::Uuid::from_bytes(
                    message.message_id.to_be_bytes(),
                )),
            };
        let filter = doc! { "_id": id_bson };
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

    fn outcome_or_ack(&self, message: CanonicalMessage, outcome: &str) -> Sent {
        tag_outcome(self.report_outcome, message, outcome)
    }
}

/// With `report_outcome`, tag the message with `mongodb.outcome` and return it as a
/// `Sent::Response` so a downstream `switch` can branch; otherwise a plain `Ack`.
pub(crate) fn tag_outcome(
    report_outcome: bool,
    mut message: CanonicalMessage,
    outcome: &str,
) -> Sent {
    if report_outcome {
        message
            .metadata
            .insert(OUTCOME_KEY.to_string(), outcome.to_string());
        Sent::Response(message)
    } else {
        Sent::Ack
    }
}

/// What a per-document insert error means for the message that carried the document.
#[derive(Debug, PartialEq)]
pub(super) enum InsertRejection {
    /// Duplicate `_id`: the document is already there, which makes a re-run idempotent.
    AlreadyStored,
    /// The collection refuses this document; sending it again cannot succeed.
    Permanent,
    Transient,
}

pub(super) fn classify_insert_error(code: i32, message: &str) -> InsertRejection {
    match code {
        11000 => match duplicate_index(message) {
            None | Some("_id_") => InsertRejection::AlreadyStored,
            Some(_) => InsertRejection::Permanent,
        },
        // BadValue, FailedToParse, TypeMismatch, DollarPrefixedFieldName,
        // DocumentValidationFailure, BSONObjectTooLarge, KeyTooLong.
        2 | 9 | 14 | 52 | 121 | 10334 | 17280 => InsertRejection::Permanent,
        _ => InsertRejection::Transient,
    }
}

/// The index named by an E11000 message (`... index: email_1 dup key: ...`).
fn duplicate_index(message: &str) -> Option<&str> {
    let rest = message.split_once(" index: ")?.1;
    let name = rest.split(" dup key").next()?.trim();
    Some(name.rsplit(".$").next().unwrap_or(name))
}

fn insert_rejection(e: &mongodb::error::Error) -> InsertRejection {
    match &*e.kind {
        ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) => {
            classify_insert_error(w.code, &w.message)
        }
        _ => InsertRejection::Transient,
    }
}

/// A server-rejected update (bad data for the pipeline) cannot succeed on retry; a network or
/// failover error can, and so can E11000 from two upserts racing to insert the same key.
fn update_error(e: mongodb::error::Error) -> PublisherError {
    let duplicate = match &*e.kind {
        ErrorKind::Write(mongodb::error::WriteFailure::WriteError(w)) => w.code == 11000,
        ErrorKind::Command(c) => c.code == 11000,
        _ => false,
    };
    let rejected = matches!(*e.kind, ErrorKind::Command(_) | ErrorKind::Write(_))
        && !duplicate
        && !e.contains_label(mongodb::error::RETRYABLE_WRITE_ERROR);
    if rejected {
        PublisherError::NonRetryable(anyhow!(e).context("MongoDB `update` rejected"))
    } else {
        PublisherError::Retryable(e.into())
    }
}

/// In a batch, a non-retryable error is the message's own and is answered as not found;
/// a retryable one still fails the batch.
fn rejected_in_batch(e: PublisherError) -> Result<anyhow::Error, PublisherError> {
    match e {
        PublisherError::NonRetryable(e) => Ok(e),
        other => Err(other),
    }
}

/// Parses a rendered `update` into an update document or an aggregation pipeline.
pub(super) fn update_modifications(rendered: &[u8]) -> Result<UpdateModifications, PublisherError> {
    let parsed = serde_json::from_slice::<serde_json::Value>(rendered)
        .map_err(anyhow::Error::from)
        .and_then(|v| Ok(Bson::try_from(v)?));
    let invalid =
        |e: anyhow::Error| PublisherError::NonRetryable(e.context("invalid MongoDB `update`"));
    match parsed.map_err(invalid)? {
        Bson::Document(d) => Ok(UpdateModifications::Document(d)),
        Bson::Array(stages) => stages
            .into_iter()
            .map(|stage| match stage {
                Bson::Document(d) => Ok(d),
                other => Err(anyhow!("pipeline stage is a {:?}", other.element_type())),
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(UpdateModifications::Pipeline)
            .map_err(invalid),
        other => Err(invalid(anyhow!(
            "update is a {:?}, not a document or pipeline",
            other.element_type()
        ))),
    }
}

/// A rendered `update` as steps that can run after another message's, and the
/// top-level fields they write; `None` when it cannot chain: other steps, operators, dotted
/// operator paths, or a write to `field`.
pub(super) fn chainable_steps(
    update: UpdateModifications,
    field: &str,
) -> Option<(Vec<Document>, Vec<String>)> {
    let steps = match update {
        UpdateModifications::Pipeline(steps) => steps,
        UpdateModifications::Document(update) => operator_steps(update)?,
        _ => return None,
    };
    let mut touched: Vec<String> = Vec::new();
    for step in &steps {
        let mut entries = step.iter();
        let (Some((name, spec)), None) = (entries.next(), entries.next()) else {
            return None;
        };
        let paths: Vec<&str> = match (name.as_str(), spec) {
            ("$set" | "$addFields", Bson::Document(fields)) => {
                fields.keys().map(String::as_str).collect()
            }
            ("$unset", Bson::String(path)) => vec![path.as_str()],
            ("$unset", Bson::Array(paths)) => paths
                .iter()
                .map(|p| match p {
                    Bson::String(p) => Some(p.as_str()),
                    _ => None,
                })
                .collect::<Option<_>>()?,
            _ => return None,
        };
        for path in paths {
            let top = path.split('.').next().unwrap_or(path);
            if top == field {
                return None;
            }
            if !touched.iter().any(|t| t == top) {
                touched.push(top.to_string());
            }
        }
    }
    Some((steps, touched))
}

/// An operator update as steps with the same effect. Covers `$set`, `$inc`, `$mul`,
/// `$min`, `$max` and `$unset` on distinct top-level fields; a dotted path could cross an array.
fn operator_steps(update: Document) -> Option<Vec<Document>> {
    let mut set = Document::new();
    let mut unset = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for (op, fields) in update {
        let Bson::Document(fields) = fields else {
            return None;
        };
        for (path, value) in fields {
            let plain = !path.is_empty() && !path.contains(['.', '$']);
            if !plain || paths.contains(&path) {
                return None;
            }
            paths.push(path.clone());
            let current = Bson::String(format!("${path}"));
            let value = doc! { "$literal": value };
            let missing = doc! { "$eq": [{ "$type": current.clone() }, "missing"] };
            let expr = match op.as_str() {
                "$set" => Bson::Document(value),
                "$inc" => Bson::Document(doc! { "$add": [{ "$ifNull": [current, 0] }, value] }),
                "$mul" => {
                    Bson::Document(doc! { "$multiply": [{ "$ifNull": [current, 0] }, value] })
                }
                "$min" => Bson::Document(doc! { "$cond": [
                    { "$or": [missing, { "$lt": [value.clone(), current.clone()] }] }, value, current
                ] }),
                "$max" => Bson::Document(doc! { "$cond": [
                    { "$or": [missing, { "$gt": [value.clone(), current.clone()] }] }, value, current
                ] }),
                "$unset" => {
                    unset.push(Bson::String(path));
                    continue;
                }
                _ => return None,
            };
            set.insert(path, expr);
        }
    }
    let mut steps = Vec::new();
    if !set.is_empty() {
        steps.push(doc! { "$set": set });
    }
    if !unset.is_empty() {
        steps.push(doc! { "$unset": unset });
    }
    (!steps.is_empty()).then_some(steps)
}

/// Whether `filter` may read one of the `touched` top-level fields: as a key, as a `$field`
/// reference in an expression, or through `$where` or `$jsonSchema`, which cannot be inspected.
pub(super) fn filter_reads(filter: &Document, touched: &[String]) -> bool {
    fn top(path: &str) -> &str {
        path.split('.').next().unwrap_or(path)
    }
    fn value_reads(value: &Bson, touched: &[String]) -> bool {
        match value {
            Bson::Document(d) => filter_reads(d, touched),
            Bson::Array(items) => items.iter().any(|v| value_reads(v, touched)),
            Bson::String(s) => s
                .strip_prefix('$')
                .is_some_and(|path| touched.iter().any(|t| t == top(path))),
            _ => false,
        }
    }
    filter.iter().any(|(key, value)| {
        matches!(key.as_str(), "$where" | "$jsonSchema")
            || touched.iter().any(|t| t == top(key))
            || value_reads(value, touched)
    })
}

/// Chains each message's steps, snapshotting the `touched` fields into `field` between
/// messages. The last message's document is the result itself, so it needs no snapshot.
pub(super) fn folded_update(
    field: &str,
    touched: &[String],
    chained: Vec<Vec<Document>>,
) -> Vec<Document> {
    // A missing field is left out of the snapshot, so `unfold` can tell it from a null.
    let fields: Document = touched
        .iter()
        .map(|t| (t.clone(), Bson::String(format!("${t}"))))
        .collect();
    let snapshot = doc! { "$set": { field: { "$concatArrays": [
        { "$ifNull": [format!("${field}"), []] },
        [fields],
    ] } } };
    let last = chained.len().saturating_sub(1);
    let mut update = vec![doc! { "$unset": field }];
    for (i, steps) in chained.into_iter().enumerate() {
        update.extend(steps);
        if i < last {
            update.push(snapshot.clone());
        }
    }
    update
}

/// Splits a folded result into one document per message, or `None` if the snapshots are off:
/// each snapshot overlays the final document's `touched` fields.
pub(super) fn unfold(
    mut doc: Document,
    field: &str,
    touched: &[String],
    messages: usize,
) -> Option<Vec<Document>> {
    let snapshots = match doc.remove(field) {
        Some(Bson::Array(snapshots)) => snapshots,
        None => Vec::new(),
        Some(_) => return None,
    };
    let mut docs = Vec::with_capacity(messages);
    for snapshot in snapshots {
        let Bson::Document(mut snapshot) = snapshot else {
            return None;
        };
        let mut before = Document::new();
        for (key, value) in &doc {
            if !touched.contains(key) {
                before.insert(key.clone(), value.clone());
            } else if let Some(value) = snapshot.remove(key) {
                before.insert(key.clone(), value);
            }
        }
        before.extend(snapshot);
        docs.push(before);
    }
    docs.push(doc);
    (docs.len() == messages).then_some(docs)
}

/// Parses a rendered `find` filter into a BSON document.
fn find_filter(rendered: &[u8]) -> Result<Document, PublisherError> {
    serde_json::from_slice::<serde_json::Value>(rendered)
        .map_err(anyhow::Error::from)
        .and_then(|v| Ok(Bson::try_from(v)?))
        .and_then(|b| match b {
            Bson::Document(d) => Ok(d),
            other => Err(anyhow!(
                "filter is a {:?}, not a document",
                other.element_type()
            )),
        })
        .map_err(|e| PublisherError::NonRetryable(e.context("invalid MongoDB `find` filter")))
}

/// Each message's rendered `$in` element and its key; a token without a value renders neither,
/// so that message is simply not found.
fn list_keys(
    element: &CompiledTemplate,
    messages: &[CanonicalMessage],
) -> (Vec<Option<String>>, Vec<Option<String>>) {
    let elements: Vec<Option<String>> = messages
        .iter()
        .map(|m| {
            element
                .render_resolved(Some(m))
                .and_then(|b| String::from_utf8(b).ok())
        })
        .collect();
    let keys: Vec<Option<String>> = elements
        .iter()
        .map(|e| {
            e.as_deref()
                .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
                .and_then(|v| lookup_batch::key_of(&v))
        })
        .collect();
    (elements, keys)
}

/// The batched form of a `find` / `aggregate_query` template, when its token sits inside `$in`.
fn list_query(
    template: Option<&str>,
    option: &str,
) -> anyhow::Result<Option<(ListQuery, CompiledTemplate)>> {
    let Some(list) = template
        .map(lookup_batch::parse_mongodb)
        .transpose()?
        .flatten()
    else {
        return Ok(None);
    };
    let element = format!("{}{}{}", list.prefix, list.token, list.suffix);
    let element = CompiledTemplate::compile(&element, Some("application/json"))
        .with_context(|| format!("invalid MongoDB `{option}` template"))?;
    Ok(Some((list, element)))
}

/// Parses a rendered `aggregate_query` into its stages.
pub(super) fn pipeline_stages(rendered: &[u8]) -> Result<Vec<Document>, PublisherError> {
    serde_json::from_slice::<serde_json::Value>(rendered)
        .map_err(anyhow::Error::from)
        .and_then(|v| Ok(Bson::try_from(v)?))
        .and_then(|b| match b {
            Bson::Array(stages) => stages
                .into_iter()
                .map(|stage| match stage {
                    Bson::Document(d) => Ok(d),
                    other => Err(anyhow!("pipeline stage is a {:?}", other.element_type())),
                })
                .collect(),
            other => Err(anyhow!(
                "pipeline is a {:?}, not an array of stages",
                other.element_type()
            )),
        })
        .map_err(|e| PublisherError::NonRetryable(e.context("invalid MongoDB `aggregate_query`")))
}

/// Stages that act on all documents together: in batch form they mix or cap different keys.
const CROSS_KEY_STAGES: &[&str] = &[
    "$limit",
    "$skip",
    "$sample",
    "$count",
    "$facet",
    "$bucket",
    "$bucketAuto",
    "$sortByCount",
];

/// A batched `aggregate_query` runs once for many keys, so its stages must keep them apart:
/// none of [`CROSS_KEY_STAGES`], and `$group` / `$setWindowFields` by the `$in` field.
pub(super) fn check_batch_pipeline(list: &ListQuery) -> anyhow::Result<()> {
    // `0` stands in for the key; a pipeline that does not parse then fails per batch instead.
    let sample = format!(
        "{}{}0{}{}",
        list.before, list.prefix, list.suffix, list.after
    );
    let Ok(stages) = pipeline_stages(sample.as_bytes()) else {
        return Ok(());
    };
    let mut key = format!("${}", list.key);
    for stage in &stages {
        let Some((name, spec)) = stage.iter().next() else {
            continue;
        };
        if CROSS_KEY_STAGES.contains(&name.as_str()) {
            anyhow::bail!(
                "MongoDB `aggregate_query`: `{name}` would act on the whole batch, not per key. With `$in` the pipeline runs for many keys at once; drop `{name}`, or match the key directly (`\"{}\": \"${{payload:…}}\"`) to run once per message",
                list.key
            );
        }
        let by = match name.as_str() {
            "$group" => "_id",
            "$setWindowFields" => "partitionBy",
            _ => continue,
        };
        if spec.as_document().and_then(|d| d.get_str(by).ok()) != Some(key.as_str()) {
            anyhow::bail!(
                "MongoDB `aggregate_query`: with `$in`, `{name}` must use `{by}: \"{key}\"`, the key results are matched by"
            );
        }
        if name == "$group" {
            key = "$_id".to_string();
        }
    }
    Ok(())
}

/// Server codes of a failover, shutdown, timeout or lost cursor: the same aggregation can
/// succeed later.
const TRANSIENT_COMMAND_CODES: &[i32] = &[
    6, 7, 43, 50, 89, 91, 189, 262, 9001, 10107, 11600, 11601, 11602, 13435, 13436,
];

/// A server-rejected aggregation (a bad stage, or data it cannot compute) fails the same way
/// on retry; a network, failover or timeout error does not.
pub(super) fn query_error(e: mongodb::error::Error) -> PublisherError {
    match &*e.kind {
        ErrorKind::Command(c) if !TRANSIENT_COMMAND_CODES.contains(&c.code) => {
            PublisherError::NonRetryable(anyhow!(e).context("MongoDB `aggregate_query` rejected"))
        }
        _ => PublisherError::Retryable(e.into()),
    }
}

#[async_trait]
impl MessagePublisher for MongoDbPublisher {
    async fn send(&self, mut message: CanonicalMessage) -> Result<Sent, PublisherError> {
        if let Some(template) = &self.aggregate_query {
            return self.aggregate_one(template, &message).await;
        }
        if let Some(template) = &self.find {
            return self.find_one(template, &message).await;
        }
        if !self.request_reply {
            trace!(message_id = %format!("{:032x}", message.message_id), collection = %self.collection_name, "Publishing document to MongoDB");
            let doc = message_to_document(
                &message,
                &self.format,
                self.id_field.as_deref(),
                self.id_template.as_ref(),
            )
            .map_err(PublisherError::NonRetryable)?;

            match self.collection.insert_one(doc).await {
                Ok(_) => {}
                Err(e) => {
                    let rejection = insert_rejection(&e);
                    let error =
                        anyhow::anyhow!(e).context("Failed to insert document into MongoDB");
                    return match rejection {
                        InsertRejection::AlreadyStored => {
                            warn!(message_id = %format!("{:032x}", message.message_id), "Duplicate key error inserting into MongoDB. Treating as idempotent success.");
                            Ok(self.outcome_or_ack(message, OUTCOME_EXISTED))
                        }
                        InsertRejection::Permanent => Err(PublisherError::NonRetryable(error)),
                        InsertRejection::Transient => Err(PublisherError::Retryable(error)),
                    };
                }
            }

            return Ok(self.outcome_or_ack(message, OUTCOME_INSERTED));
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
        let doc = message_to_document(
            &message,
            &self.format,
            self.id_field.as_deref(),
            self.id_template.as_ref(),
        )
        .map_err(PublisherError::NonRetryable)?;
        match self.collection.insert_one(doc).await {
            Ok(_) => {}
            Err(e) => {
                let rejection = insert_rejection(&e);
                if rejection == InsertRejection::AlreadyStored {
                    warn!(message_id = %format!("{:032x}", message.message_id), "Duplicate key error inserting request into MongoDB. Treating as idempotent success.");
                    self.recover_correlation_id_from_duplicate(&mut message)
                        .await?;
                    if let Some(cid) = message.metadata.get("correlation_id") {
                        correlation_id = cid.clone();
                    }
                } else {
                    let error = anyhow::anyhow!(e)
                        .context("Failed to insert request document into MongoDB");
                    return Err(if rejection == InsertRejection::Permanent {
                        PublisherError::NonRetryable(error)
                    } else {
                        PublisherError::Retryable(error)
                    });
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
                    let mut response_msg = parse_mongodb_document(doc).map_err(|e| {
                        PublisherError::NonRetryable(anyhow!("Failed to parse response: {}", e))
                    })?;
                    // A batch matches each reply to its request by id.
                    response_msg.message_id = message.message_id;
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

        if self.request_reply
            || self.report_outcome
            || self.find.is_some()
            || self.aggregate_query.is_some()
        {
            // report_outcome needs a per-message Response, so fan out through single send.
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        trace!(count = messages.len(), collection = %self.collection_name, message_ids = ?LazyMessageIds(&messages), "Publishing batch of documents to MongoDB");
        let mut docs = Vec::with_capacity(messages.len());
        let mut failed_messages = Vec::new();
        let mut valid_messages = Vec::with_capacity(messages.len());

        for message in messages {
            match message_to_document(
                &message,
                &self.format,
                self.id_field.as_deref(),
                self.id_template.as_ref(),
            ) {
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

        // Unordered: an ordered insert stops at the first error, so one duplicate
        // `_id` would leave the rest uninserted and indistinguishable from a
        // failure. Capped collections take unordered inserts too and still store
        // in $natural order.
        match self.collection.insert_many(docs).ordered(false).await {
            Ok(_) => Ok(SentBatch::from_failures(failed_messages)),
            Err(e) => {
                if let ErrorKind::InsertMany(ref err) = *e.kind {
                    let mut errors_by_index = HashMap::new();
                    if let Some(write_errors) = &err.write_errors {
                        for we in write_errors {
                            errors_by_index.insert(we.index, we);
                        }
                    }

                    // If we have a write concern error, assume all failed to be safe (potential rollback).
                    // A retry of a stored document is a duplicate `_id`, so it is idempotent.
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

                    // Every document was attempted, so an index with no write
                    // error was inserted and each error stands on its own.
                    for (i, msg) in valid_messages.into_iter().enumerate() {
                        if let Some(w) = errors_by_index.get(&i) {
                            let error = anyhow::anyhow!("MongoDB write error: {:?}", w);
                            match classify_insert_error(w.code, &w.message) {
                                InsertRejection::AlreadyStored => {}
                                InsertRejection::Permanent => {
                                    failed_messages.push((msg, PublisherError::NonRetryable(error)))
                                }
                                InsertRejection::Transient => {
                                    failed_messages.push((msg, PublisherError::Retryable(error)))
                                }
                            }
                        }
                    }

                    Ok(SentBatch::from_failures(failed_messages))
                } else {
                    Err(PublisherError::Retryable(anyhow!(e)))
                }
            }
        }
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.db.run_command(doc! { "ping": 1 }).await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        EndpointStatus {
            healthy,
            target: self.collection_name.clone(),
            error,
            details: serde_json::json!({ "database": self.db.name(), "request_reply": self.request_reply }),
            ..Default::default()
        }
    }

    async fn lookup_batch(
        &self,
        requests: &[CanonicalMessage],
    ) -> Option<Result<Vec<Option<serde_json::Value>>, PublisherError>> {
        if let (Some(find), Some(update)) = (&self.find, &self.update) {
            return Some(self.update_many(find, update, requests).await);
        }
        if let Some((list, element)) = &self.aggregate_list {
            return Some(self.aggregate_many(list, element, requests).await);
        }
        let (list, element) = self.find_list.as_ref()?;
        Some(self.find_many(list, element, requests).await)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
