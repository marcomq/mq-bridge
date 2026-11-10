//! Source-position ranges used by idempotent sinks.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use tracing::warn;

use crate::CanonicalMessage;

const TOPIC_KEY: &str = "mqb.src.kafka_topic";
const PARTITION_KEY: &str = "mqb.src.kafka_partition";
const OFFSET_KEY: &str = "mqb.src.kafka_offset";
const POSTGRES_SLOT_KEY: &str = "mqb.src.postgres_slot";
const POSTGRES_LSN_KEY: &str = "mqb.src.postgres_lsn";
const POSTGRES_ORDINAL_KEY: &str = "mqb.src.postgres_ordinal";
const MONGODB_NAMESPACE_KEY: &str = "mqb.src.mongodb_namespace";
const MONGODB_CLUSTER_TIME_KEY: &str = "mqb.src.mongodb_cluster_time";
const MONGODB_ORDINAL_KEY: &str = "mqb.src.mongodb_ordinal";
const MONGODB_SNAPSHOT_INDEX_KEY: &str = "mqb.src.mongodb_snapshot_index";
const FILE_PATH_KEY: &str = "mqb.src.file_path";
const FILE_RECORD_KEY: &str = "mqb.src.file_record";
const FILE_EPOCH_KEY: &str = "mqb.src.file_epoch";
const SQLX_TABLE_KEY: &str = "mqb.src.sqlx_table";
const SQLX_CURSOR_KEY: &str = "mqb.src.sqlx_cursor";

/// Width of a `u64` embedded in a source key. Numbers inside the key must be fixed-width
/// or ASCII sort disagrees with numeric sort and the sink replays out of order.
const POSITION_WIDTH: usize = 20;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourcePartition {
    pub topic: String,
    pub partition: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourcePosition {
    pub source: SourcePartition,
    pub offset: u64,
}

#[derive(Debug)]
pub struct SourceRun {
    pub source: SourcePartition,
    pub start: u64,
    pub end: u64,
    pub messages: Vec<CanonicalMessage>,
}

impl SourcePosition {
    pub fn from_message(message: &CanonicalMessage) -> Result<Self> {
        if message.has_metadata(TOPIC_KEY)
            || message.has_metadata(PARTITION_KEY)
            || message.has_metadata(OFFSET_KEY)
        {
            return Self::from_kafka_message(message);
        }
        if message.has_metadata(POSTGRES_SLOT_KEY)
            || message.has_metadata(POSTGRES_LSN_KEY)
            || message.has_metadata(POSTGRES_ORDINAL_KEY)
        {
            return Self::from_postgres_message(message);
        }
        if message.has_metadata(MONGODB_NAMESPACE_KEY) {
            return Self::from_mongodb_message(message);
        }
        if message.has_metadata(FILE_PATH_KEY) || message.has_metadata(FILE_RECORD_KEY) {
            return Self::from_file_message(message);
        }
        if message.has_metadata(SQLX_TABLE_KEY) || message.has_metadata(SQLX_CURSOR_KEY) {
            return Self::from_sqlx_message(message);
        }
        bail!("idempotent sink requires Kafka, postgres_cdc, mongodb, file or sqlx source-position metadata");
    }

    fn from_kafka_message(message: &CanonicalMessage) -> Result<Self> {
        let topic = message
            .get_metadata(TOPIC_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("idempotent sink requires {TOPIC_KEY}"))?
            .to_owned();
        let partition = parse_signed_position(message, PARTITION_KEY)?;
        let offset = parse_unsigned_position(message, OFFSET_KEY)?;
        if partition < 0 {
            bail!("idempotent sink requires a non-negative Kafka partition");
        }
        let partition = i32::try_from(partition)
            .map_err(|_| anyhow!("idempotent sink Kafka partition is out of range"))?;

        Ok(Self {
            source: SourcePartition { topic, partition },
            offset,
        })
    }

    fn from_postgres_message(message: &CanonicalMessage) -> Result<Self> {
        let slot = message
            .get_metadata(POSTGRES_SLOT_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("idempotent sink requires {POSTGRES_SLOT_KEY}"))?;
        let lsn = parse_unsigned_position(message, POSTGRES_LSN_KEY)?;
        let ordinal = parse_unsigned_position(message, POSTGRES_ORDINAL_KEY)?;

        Ok(Self {
            // A commit LSN is shared by all changes in the transaction. Including it
            // in the source key makes the transaction ordinal a unique range offset.
            // Padded: the LSN is part of the key text, so an unpadded one sorts
            // 10000000000 before 9876543210.
            source: SourcePartition {
                topic: format!("postgres_cdc-{slot}-{lsn:00$}", POSITION_WIDTH),
                partition: 0,
            },
            offset: ordinal,
        })
    }

    /// MongoDB has two phases and they must sort in the order they are read: the initial
    /// snapshot of existing documents, then the change stream. The phase is numbered into
    /// the key (`0snapshot` before `1cdc`) so a change never replays ahead of the document
    /// it modifies.
    fn from_mongodb_message(message: &CanonicalMessage) -> Result<Self> {
        let namespace = message
            .get_metadata(MONGODB_NAMESPACE_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("idempotent sink requires {MONGODB_NAMESPACE_KEY}"))?;

        // Snapshot documents are paged by ascending `_id`, so their scan index is a
        // contiguous, deterministic position.
        if message.has_metadata(MONGODB_SNAPSHOT_INDEX_KEY) {
            let index = parse_unsigned_position(message, MONGODB_SNAPSHOT_INDEX_KEY)?;
            return Ok(Self {
                source: SourcePartition {
                    topic: format!("mongodb-{namespace}-0snapshot"),
                    partition: 0,
                },
                offset: index,
            });
        }

        // A cluster time is shared by every change in a transaction, so it identifies the
        // group and the ordinal positions the change inside it.
        let cluster_time = parse_unsigned_position(message, MONGODB_CLUSTER_TIME_KEY)?;
        let ordinal = parse_unsigned_position(message, MONGODB_ORDINAL_KEY)?;
        Ok(Self {
            source: SourcePartition {
                topic: format!(
                    "mongodb-{namespace}-1cdc-{cluster_time:00$}",
                    POSITION_WIDTH
                ),
                partition: 0,
            },
            offset: ordinal,
        })
    }

    /// The position is the record's index in the file, not its byte offset: the sink groups
    /// records into one object by consecutive position, and byte offsets are never
    /// consecutive, so they would force one object per record.
    fn from_file_message(message: &CanonicalMessage) -> Result<Self> {
        let path = message
            .get_metadata(FILE_PATH_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("idempotent sink requires {FILE_PATH_KEY}"))?;
        let record = parse_unsigned_position(message, FILE_RECORD_KEY)?;
        // Modes that do not start at byte 0 carry a run epoch, so a restart cannot reuse
        // the record indexes of the run before it. Sorted ahead of the index because a
        // later run always reads later records.
        let identity = sanitize_path_identity(path);
        let topic = match message.get_metadata(FILE_EPOCH_KEY) {
            Some(_) => {
                let epoch = parse_unsigned_position(message, FILE_EPOCH_KEY)?;
                format!("file-{identity}-{epoch:00$}", POSITION_WIDTH)
            }
            None => format!("file-{identity}"),
        };
        Ok(Self {
            source: SourcePartition {
                topic,
                partition: 0,
            },
            offset: record,
        })
    }

    /// A polling cursor is a replay position in its own right, so the cursor value *is* the
    /// offset — the same shape Kafka Connect's S3 sink uses for a Kafka offset. Only an
    /// integer cursor reaches here; the reader rejects text cursors, which have no
    /// contiguous ordering, before stamping.
    fn from_sqlx_message(message: &CanonicalMessage) -> Result<Self> {
        let table = message
            .get_metadata(SQLX_TABLE_KEY)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("idempotent sink requires {SQLX_TABLE_KEY}"))?;
        let cursor = parse_unsigned_position(message, SQLX_CURSOR_KEY)?;

        Ok(Self {
            source: SourcePartition {
                topic: format!("sqlx-{table}"),
                partition: 0,
            },
            offset: cursor,
        })
    }
}

/// A source path is not usable as a key component verbatim — `finalized_name` rejects `/`,
/// and object stores treat it as a directory separator. Keeps the file name for
/// readability and appends a hash of the full path so two same-named files in different
/// directories stay distinct.
fn sanitize_path_identity(path: &str) -> String {
    // FNV-1a, spelled out: `DefaultHasher` is explicitly not stable across Rust releases,
    // and this digest goes into object names that must match after an upgrade.
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_bytes() {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }

    let name = path.rsplit('/').next().unwrap_or(path);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("{safe}-{digest:016x}")
}

fn parse_signed_position(message: &CanonicalMessage, key: &str) -> Result<i64> {
    message
        .get_metadata(key)
        .ok_or_else(|| anyhow!("idempotent sink requires {key}"))?
        .parse()
        .map_err(|_| anyhow!("idempotent sink requires an integer {key}"))
}

fn parse_unsigned_position(message: &CanonicalMessage, key: &str) -> Result<u64> {
    message
        .get_metadata(key)
        .ok_or_else(|| anyhow!("idempotent sink requires {key}"))?
        .parse()
        .map_err(|_| anyhow!("idempotent sink requires a non-negative integer {key}"))
}

#[derive(Clone, Debug, Default)]
pub struct CoveredRanges {
    ranges: BTreeMap<SourcePartition, Vec<(u64, u64)>>,
}

impl CoveredRanges {
    pub fn from_finalized_names<'a>(
        names: impl IntoIterator<Item = &'a str>,
        extension: &str,
    ) -> Self {
        let mut covered = Self::default();
        for name in names {
            if let Some((source, start, end)) = parse_finalized_name(name, extension) {
                // `parse_finalized_name` already guarantees a valid range.
                covered
                    .insert(source, start, end)
                    .expect("validated finalized range");
            }
        }
        covered
    }

    pub fn insert(&mut self, source: SourcePartition, start: u64, end: u64) -> Result<()> {
        if end < start {
            bail!("invalid covered source-position range {start}..={end}");
        }

        let ranges = self.ranges.entry(source).or_default();
        ranges.push((start, end));
        ranges.sort_unstable();

        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges.drain(..) {
            if let Some((_, previous_end)) = merged.last_mut() {
                if start <= previous_end.saturating_add(1) {
                    *previous_end = (*previous_end).max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        *ranges = merged;
        Ok(())
    }

    /// Folds another set in, keeping the merged-interval invariant. Used when a sink
    /// recovers what is already on the store after it has begun writing.
    pub fn merge(&mut self, other: CoveredRanges) -> Result<()> {
        for (source, ranges) in other.ranges {
            for (start, end) in ranges {
                self.insert(source.clone(), start, end)?;
            }
        }
        Ok(())
    }

    pub fn contains(&self, position: &SourcePosition) -> bool {
        self.ranges.get(&position.source).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|(start, end)| *start <= position.offset && position.offset <= *end)
        })
    }

    pub fn filter_uncovered(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<Vec<CanonicalMessage>> {
        messages
            .into_iter()
            .filter_map(|message| match SourcePosition::from_message(&message) {
                Ok(position) if self.contains(&position) => None,
                Ok(_) => Some(Ok(message)),
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn uncovered_runs(&self, messages: Vec<CanonicalMessage>) -> Result<Vec<SourceRun>> {
        let mut grouped = BTreeMap::<SourcePartition, BTreeMap<u64, CanonicalMessage>>::new();
        for message in messages {
            let position = SourcePosition::from_message(&message)?;
            if !self.contains(&position) {
                let offset = position.offset;
                // One source position maps to one part-file record, so a second message at
                // the same position (a middleware that fans one record out) is dropped.
                if grouped
                    .entry(position.source.clone())
                    .or_default()
                    .insert(offset, message)
                    .is_some()
                {
                    warn!(
                        source = ?position.source,
                        offset,
                        "idempotent sink dropped a second message at the same source position; \
                         middleware that fans one source record out is not replay-safe"
                    );
                }
            }
        }

        let mut runs = Vec::new();
        for (source, offsets) in grouped {
            let mut current: Option<SourceRun> = None;
            for (offset, message) in offsets {
                if let Some(run) = current.as_mut() {
                    if offset == run.end.saturating_add(1) {
                        run.end = offset;
                        run.messages.push(message);
                        continue;
                    }
                }
                if let Some(run) = current.take() {
                    runs.push(run);
                }
                current = Some(SourceRun {
                    source: source.clone(),
                    start: offset,
                    end: offset,
                    messages: vec![message],
                });
            }
            if let Some(run) = current {
                runs.push(run);
            }
        }
        Ok(runs)
    }
}

pub fn finalized_name(
    source: &SourcePartition,
    start: u64,
    end: u64,
    extension: &str,
) -> Result<String> {
    if source.topic.is_empty()
        || source.topic.contains('/')
        || source.partition < 0
        || extension.is_empty()
        || extension.contains('/')
        || end < start
    {
        bail!("invalid idempotent sink finalized name components");
    }
    // Zero-padded to fixed width so lexicographic listing order == numeric order.
    // `parse_finalized_name` still accepts legacy unpadded names.
    Ok(format!(
        "part-{topic}-{partition:010}-{start:0width$}-{end:0width$}.{extension}",
        topic = source.topic,
        partition = source.partition,
        width = POSITION_WIDTH,
    ))
}

pub fn parse_finalized_name(name: &str, extension: &str) -> Option<(SourcePartition, u64, u64)> {
    let suffix = format!(".{extension}");
    let stem = name.strip_prefix("part-")?.strip_suffix(&suffix)?;
    let mut parts = stem.rsplitn(4, '-');
    let end = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    let partition = parts.next()?.parse().ok()?;
    let topic = parts.next()?.to_string();
    if topic.is_empty() || partition < 0 || end < start {
        return None;
    }
    Some((SourcePartition { topic, partition }, start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: &str, partition: i32, offset: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(TOPIC_KEY, topic);
        message.set_metadata(PARTITION_KEY, partition.to_string());
        message.set_metadata(OFFSET_KEY, offset.to_string());
        message
    }

    fn postgres_message(slot: &str, lsn: u64, ordinal: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(POSTGRES_SLOT_KEY, slot);
        message.set_metadata(POSTGRES_LSN_KEY, lsn.to_string());
        message.set_metadata(POSTGRES_ORDINAL_KEY, ordinal.to_string());
        message
    }

    #[test]
    fn source_position_requires_valid_kafka_metadata() {
        let error = SourcePosition::from_message(&CanonicalMessage::new(b"payload".to_vec(), None))
            .unwrap_err();
        assert!(error.to_string().contains("source-position metadata"));

        let mut invalid = message("orders", 0, 1);
        invalid.set_metadata(OFFSET_KEY, "not-an-offset");
        assert!(SourcePosition::from_message(&invalid).is_err());
    }

    fn sqlx_message(table: &str, cursor: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(SQLX_TABLE_KEY, table.to_string());
        message.set_metadata(SQLX_CURSOR_KEY, cursor.to_string());
        message
    }

    /// The cursor value is the offset directly, so consecutive rows coalesce into one run
    /// and one object — a gap in the ids is what splits them.
    #[test]
    fn sqlx_cursor_positions_coalesce_consecutive_rows() {
        let runs = CoveredRanges::default()
            .uncovered_runs(vec![
                sqlx_message("public.orders", 41),
                sqlx_message("public.orders", 42),
                sqlx_message("public.orders", 77),
            ])
            .unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].source.topic, "sqlx-public.orders");
        assert_eq!(
            (runs[0].start, runs[0].end, runs[0].messages.len()),
            (41, 42, 2)
        );
        assert_eq!((runs[1].start, runs[1].end), (77, 77));

        // A schema-qualified table carries a `.`, which has to survive into a part name.
        assert_eq!(
            finalized_name(&runs[0].source, runs[0].start, runs[0].end, "jsonl").unwrap(),
            "part-sqlx-public.orders-0000000000-00000000000000000041-00000000000000000042.jsonl"
        );
    }

    #[test]
    fn sqlx_cursor_positions_require_a_table_and_numeric_cursor() {
        let mut missing_table = sqlx_message("orders", 1);
        missing_table.remove_metadata(SQLX_TABLE_KEY);
        assert!(SourcePosition::from_message(&missing_table).is_err());

        let mut text_cursor = sqlx_message("orders", 1);
        text_cursor.set_metadata(SQLX_CURSOR_KEY, "2026-08-18T00:00:00Z");
        assert!(SourcePosition::from_message(&text_cursor).is_err());
    }

    #[test]
    fn postgres_cdc_positions_preserve_each_change_in_a_commit() {
        let runs = CoveredRanges::default()
            .uncovered_runs(vec![
                postgres_message("bridge_slot", 9_876_543_210, 0),
                postgres_message("bridge_slot", 9_876_543_210, 1),
                postgres_message("bridge_slot", 9_876_543_211, 0),
            ])
            .unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].source.topic,
            "postgres_cdc-bridge_slot-00000000009876543210"
        );
        assert_eq!(
            (runs[0].start, runs[0].end, runs[0].messages.len()),
            (0, 1, 2)
        );
        assert_eq!(
            runs[1].source.topic,
            "postgres_cdc-bridge_slot-00000000009876543211"
        );
        assert_eq!(
            (runs[1].start, runs[1].end, runs[1].messages.len()),
            (0, 0, 1)
        );
    }

    #[test]
    fn covered_ranges_merge_and_filter_each_record() {
        let source = SourcePartition {
            topic: "orders".into(),
            partition: 1,
        };
        let mut covered = CoveredRanges::default();
        covered.insert(source.clone(), 10, 12).unwrap();
        covered.insert(source.clone(), 13, 15).unwrap();
        covered.insert(source, 4, 6).unwrap();

        let messages = vec![
            message("orders", 1, 4),
            message("orders", 1, 7),
            message("orders", 1, 11),
            message("orders", 1, 16),
            message("orders", 2, 11),
        ];
        let uncovered = covered.filter_uncovered(messages).unwrap();
        let offsets = uncovered
            .iter()
            .map(|message| SourcePosition::from_message(message).unwrap())
            .map(|position| (position.source.partition, position.offset))
            .collect::<Vec<_>>();

        assert_eq!(offsets, vec![(1, 7), (1, 16), (2, 11)]);
    }

    /// `merge` is what folds a recovered listing into what a publisher has already written, so
    /// it has to coalesce across the two sets rather than just concatenate them: an interval
    /// recovered from the store that overlaps or abuts one held in memory must come out as a
    /// single range, or `contains` reports a gap that is not there.
    #[test]
    fn covered_ranges_merge_coalesces_across_sets_and_keeps_partitions_apart() {
        let source = |partition| SourcePartition {
            topic: "orders".into(),
            partition,
        };

        let mut covered = CoveredRanges::default();
        covered.insert(source(0), 10, 12).unwrap();
        covered.insert(source(0), 30, 31).unwrap();
        covered.insert(source(1), 10, 12).unwrap();

        let mut recovered = CoveredRanges::default();
        // overlaps 10..=12, abuts 30..=31, and lands nowhere near either
        recovered.insert(source(0), 11, 20).unwrap();
        recovered.insert(source(0), 32, 35).unwrap();
        recovered.insert(source(0), 50, 50).unwrap();
        // same offsets under a different partition stay their own interval
        recovered.insert(source(1), 40, 41).unwrap();

        covered.merge(recovered).unwrap();

        assert_eq!(
            covered.ranges.get(&source(0)),
            Some(&vec![(10, 20), (30, 35), (50, 50)])
        );
        assert_eq!(
            covered.ranges.get(&source(1)),
            Some(&vec![(10, 12), (40, 41)])
        );
    }

    #[test]
    fn finalized_names_round_trip_and_reject_staging_or_malformed_names() {
        let source = SourcePartition {
            topic: "orders-eu".into(),
            partition: 2,
        };
        let name = finalized_name(&source, 41, 59, "jsonl").unwrap();
        assert_eq!(
            name,
            "part-orders-eu-0000000002-00000000000000000041-00000000000000000059.jsonl"
        );
        assert_eq!(
            parse_finalized_name(&name, "jsonl"),
            Some((source.clone(), 41, 59))
        );
        // legacy unpadded names still read
        assert_eq!(
            parse_finalized_name("part-orders-eu-2-41-59.jsonl", "jsonl"),
            Some((source, 41, 59))
        );
        assert_eq!(
            parse_finalized_name(".stage-part-orders-2-41-59.jsonl", "jsonl"),
            None
        );
        assert_eq!(
            parse_finalized_name("part-orders-2-59-41.jsonl", "jsonl"),
            None
        );
        assert_eq!(
            parse_finalized_name("part-orders-2-41-59.tmp", "jsonl"),
            None
        );
    }

    #[test]
    fn finalized_names_sort_lexicographically_in_numeric_order() {
        let ranges = [(0u64, 9u64), (10, 19), (99, 99), (100, 100), (100, 1000)];
        for partition in [9i32, 10] {
            let source = SourcePartition {
                topic: "orders".into(),
                partition,
            };
            let names = ranges
                .iter()
                .map(|(start, end)| finalized_name(&source, *start, *end, "jsonl").unwrap())
                .collect::<Vec<_>>();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted);
        }

        let low = finalized_name(
            &SourcePartition {
                topic: "orders".into(),
                partition: 9,
            },
            0,
            0,
            "jsonl",
        )
        .unwrap();
        let high = finalized_name(
            &SourcePartition {
                topic: "orders".into(),
                partition: 10,
            },
            0,
            0,
            "jsonl",
        )
        .unwrap();
        assert!(low < high);
    }

    fn mongo_change(namespace: &str, cluster_time: u64, ordinal: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(MONGODB_NAMESPACE_KEY, namespace);
        message.set_metadata(MONGODB_CLUSTER_TIME_KEY, cluster_time.to_string());
        message.set_metadata(MONGODB_ORDINAL_KEY, ordinal.to_string());
        message
    }

    fn mongo_snapshot(namespace: &str, index: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(MONGODB_NAMESPACE_KEY, namespace);
        message.set_metadata(MONGODB_SNAPSHOT_INDEX_KEY, index.to_string());
        message
    }

    fn file_record(path: &str, record: u64) -> CanonicalMessage {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), None);
        message.set_metadata(FILE_PATH_KEY, path);
        message.set_metadata(FILE_RECORD_KEY, record.to_string());
        message
    }

    #[test]
    fn mongodb_snapshot_sorts_before_the_change_stream_that_follows_it() {
        let snapshot = SourcePosition::from_message(&mongo_snapshot("shop.orders", 0)).unwrap();
        let change = SourcePosition::from_message(&mongo_change("shop.orders", 1, 0)).unwrap();
        assert!(snapshot.source < change.source);

        // The written names must sort the same way, or a change replays ahead of the
        // document it modifies.
        let snapshot_name = finalized_name(&snapshot.source, 0, 9, "jsonl").unwrap();
        let change_name = finalized_name(&change.source, 0, 9, "jsonl").unwrap();
        assert!(snapshot_name < change_name);
    }

    #[test]
    fn mongodb_cluster_times_sort_numerically_across_digit_widths() {
        let earlier = SourcePosition::from_message(&mongo_change("shop.orders", 9_999, 0)).unwrap();
        let later = SourcePosition::from_message(&mongo_change("shop.orders", 10_000, 0)).unwrap();
        assert!(
            finalized_name(&earlier.source, 0, 0, "jsonl").unwrap()
                < finalized_name(&later.source, 0, 0, "jsonl").unwrap()
        );
    }

    #[test]
    fn mongodb_changes_in_one_cluster_time_form_one_contiguous_run() {
        let runs = CoveredRanges::default()
            .uncovered_runs(vec![
                mongo_change("shop.orders", 77, 0),
                mongo_change("shop.orders", 77, 1),
                mongo_change("shop.orders", 78, 0),
            ])
            .unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].start, runs[0].end), (0, 1));
        assert_eq!((runs[1].start, runs[1].end), (0, 0));
    }

    #[test]
    fn file_records_form_one_run_and_distinguish_same_named_files() {
        let runs = CoveredRanges::default()
            .uncovered_runs(vec![
                file_record("/data/in/orders.jsonl", 0),
                file_record("/data/in/orders.jsonl", 1),
                file_record("/data/in/orders.jsonl", 2),
            ])
            .unwrap();
        // Consecutive record indexes coalesce into a single object; byte offsets would not.
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].start, runs[0].end), (0, 2));

        // Same file name, different directory: distinct partitions.
        let a = SourcePosition::from_message(&file_record("/data/a/orders.jsonl", 0)).unwrap();
        let b = SourcePosition::from_message(&file_record("/data/b/orders.jsonl", 0)).unwrap();
        assert_ne!(a.source, b.source);

        // The identity survives `finalized_name`'s rejection of path separators.
        assert!(finalized_name(&a.source, 0, 2, "jsonl").is_ok());
    }

    #[test]
    fn startup_recovery_uses_finalized_ranges_but_ignores_staging() {
        let covered = CoveredRanges::from_finalized_names(
            [
                "part-orders-0-10-12.jsonl",
                ".stage-orders-0-13-15.jsonl",
                "part-orders-0-14-15.jsonl",
                "part-orders-1-0-2.jsonl",
                "readme.jsonl",
            ],
            "jsonl",
        );

        let uncovered = covered
            .filter_uncovered(vec![
                message("orders", 0, 11),
                message("orders", 0, 13),
                message("orders", 0, 14),
                message("orders", 1, 1),
            ])
            .unwrap();
        let offsets = uncovered
            .iter()
            .map(|message| SourcePosition::from_message(message).unwrap().offset)
            .collect::<Vec<_>>();

        assert_eq!(offsets, vec![13]);
    }

    #[test]
    fn uncovered_runs_split_gaps_and_coalesce_duplicate_offsets() {
        let mut covered = CoveredRanges::default();
        covered
            .insert(
                SourcePartition {
                    topic: "orders".into(),
                    partition: 0,
                },
                2,
                2,
            )
            .unwrap();

        let runs = covered
            .uncovered_runs(vec![
                message("orders", 0, 3),
                message("orders", 0, 1),
                message("orders", 0, 3),
                message("orders", 1, 0),
                message("orders", 0, 5),
            ])
            .unwrap();

        let ranges = runs
            .iter()
            .map(|run| (run.source.partition, run.start, run.end, run.messages.len()))
            .collect::<Vec<_>>();
        assert_eq!(
            ranges,
            vec![(0, 1, 1, 1), (0, 3, 3, 1), (0, 5, 5, 1), (1, 0, 0, 1)]
        );
    }
}
