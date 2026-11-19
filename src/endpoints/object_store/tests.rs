use super::*;
use crate::support::source_ranges::SourcePartition;
use crate::traits::{MessageConsumer, MessagePublisher};
use object_store::memory::InMemory;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Key an idempotent write lands on, derived rather than spelled out: the part-name
/// format is pinned by the `source_ranges` tests, not by these.
fn part_key(start: u64, end: u64, extension: &str) -> ObjPath {
    let source = SourcePartition {
        topic: "orders".to_string(),
        partition: 0,
    };
    ObjPath::from(format!(
        "data/{}",
        finalized_name(&source, start, end, extension).unwrap()
    ))
}

#[test]
fn checkpoint_overlap_detection() {
    // Same bucket, checkpoint nested under the source prefix -> overlap.
    assert!(object_urls_overlap(
        "s3://bucket/data",
        "s3://bucket/data/cursors"
    ));
    // Identical location -> overlap.
    assert!(object_urls_overlap("s3://bucket/data", "s3://bucket/data"));
    // Sibling prefixes in the same bucket -> safe.
    assert!(!object_urls_overlap(
        "s3://bucket/data",
        "s3://bucket/cursors"
    ));
    // String-prefix but distinct segment -> safe.
    assert!(!object_urls_overlap(
        "s3://bucket/data",
        "s3://bucket/database"
    ));
    // Different bucket -> safe.
    assert!(!object_urls_overlap("s3://bucket/data", "s3://other/data"));
}

#[test]
fn civil_date_from_unix_ms() {
    // 2026-07-17T00:00:00Z = 1_784_246_400_000 ms.
    assert_eq!(civil_from_unix_ms(1_784_246_400_000), (2026, 7, 17));
    // Epoch.
    assert_eq!(civil_from_unix_ms(0), (1970, 1, 1));
    // A leap day: 2024-02-29T12:00:00Z.
    assert_eq!(civil_from_unix_ms(1_709_208_000_000), (2024, 2, 29));
}

#[test]
fn extension_reflects_compression_and_encryption() {
    assert_eq!(
        extension_for(&FileFormat::Normal, Compression::None, false),
        "jsonl"
    );
    assert_eq!(
        extension_for(&FileFormat::Normal, Compression::Gzip, false),
        "jsonl.gz"
    );
    assert_eq!(
        extension_for(&FileFormat::Raw, Compression::Lz4, false),
        "bin.lz4"
    );
    assert_eq!(
        extension_for(&FileFormat::Normal, Compression::Zstd, false),
        "jsonl.zst"
    );
    // Encrypted objects are ciphertext -> trailing `.enc`, never a bare `.gz`.
    assert_eq!(
        extension_for(&FileFormat::Normal, Compression::Gzip, true),
        "jsonl.gz.enc"
    );
    assert_eq!(
        extension_for(&FileFormat::Raw, Compression::None, true),
        "bin.enc"
    );
}

fn json_msg(v: serde_json::Value) -> CanonicalMessage {
    CanonicalMessage::new(serde_json::to_vec(&v).unwrap(), None)
}

fn test_publisher(store: Arc<dyn ObjectStore>) -> ObjectStorePublisher {
    ObjectStorePublisher {
        store,
        base: ObjPath::from("data"),
        delimiter: vec![b'\n'],
        format: FileFormat::Normal,
        csv: CsvDialect::default(),
        #[cfg(feature = "compression")]
        compression: Compression::None,
        #[cfg(feature = "encryption")]
        crypto: None,
        date_partition: false,
        hive_partition: false,
        extension: "jsonl".to_string(),
        name_by: NameBy::WriteTime,
        covered_ranges: Arc::new(Mutex::new(CoveredRanges::default())),
        recovered: Arc::new(tokio::sync::OnceCell::new()),
        keys: Arc::new(StdMutex::new(SequentialGenerator::new())),
        staging: None,
    }
}

#[test]
fn object_keys_sort_in_allocation_order_across_clones() {
    let publisher = test_publisher(Arc::new(InMemory::new()));
    let clone = publisher.clone();
    let keys: Vec<String> = (0..1000)
        .map(|i| {
            if i % 2 == 0 { &publisher } else { &clone }
                .next_key()
                .to_string()
        })
        .collect();
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn hive_style_names_the_date_folders() {
    let mut publisher = test_publisher(Arc::new(InMemory::new()));
    publisher.date_partition = true;
    publisher.hive_partition = true;
    let key = publisher.next_key().to_string();
    let parts: Vec<&str> = key.split('/').collect();
    assert_eq!(parts.len(), 5, "{key}");
    assert!(
        parts[1].starts_with("year=") && parts[1].len() == 9,
        "{key}"
    );
    assert!(
        parts[2].starts_with("month=") && parts[2].len() == 8,
        "{key}"
    );
    assert!(parts[3].starts_with("day=") && parts[3].len() == 6, "{key}");
}

fn kafka_message(offset: i64) -> CanonicalMessage {
    let mut message = json_msg(serde_json::json!({ "offset": offset }));
    message.set_metadata("mqb.src.kafka_topic", "orders");
    message.set_metadata("mqb.src.kafka_partition", "0");
    message.set_metadata("mqb.src.kafka_offset", offset.to_string());
    message
}

/// A store whose `put_opts` parks on a barrier, so a put can only complete once as many
/// puts are in flight as the barrier expects. A publisher that serialises its writes
/// never gets there and hangs, which is what turns "slower" into a failing test.
#[derive(Debug)]
struct BarrierStore {
    inner: InMemory,
    puts: tokio::sync::Barrier,
}

impl BarrierStore {
    fn new(concurrent_puts: usize) -> Self {
        Self {
            inner: InMemory::new(),
            puts: tokio::sync::Barrier::new(concurrent_puts),
        }
    }
}

impl std::fmt::Display for BarrierStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BarrierStore")
    }
}

#[async_trait]
impl ObjectStore for BarrierStore {
    async fn put_opts(
        &self,
        location: &ObjPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.puts.wait().await;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjPath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjPath>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjPath,
        to: &ObjPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Counts `list` calls, so a test can pin how often recovery scans the prefix.
#[derive(Debug)]
struct CountingStore {
    inner: InMemory,
    lists: Arc<AtomicUsize>,
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore")
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &ObjPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjPath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjPath>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjPath,
        to: &ObjPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// `list` fails, `put` works. `permanent` picks whether the failure is one retrying can
/// ever clear.
#[derive(Debug)]
struct UnlistableStore {
    inner: InMemory,
    permanent: bool,
    lists: Arc<AtomicUsize>,
}

impl std::fmt::Display for UnlistableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UnlistableStore")
    }
}

#[async_trait]
impl ObjectStore for UnlistableStore {
    async fn put_opts(
        &self,
        location: &ObjPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjPath,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        _prefix: Option<&ObjPath>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        let permanent = self.permanent;
        futures::stream::once(async move {
            Err(if permanent {
                ObjectStoreError::PermissionDenied {
                    path: "data".to_string(),
                    source: "listing denied".into(),
                }
            } else {
                ObjectStoreError::Generic {
                    store: "UnlistableStore",
                    source: "listing unavailable".into(),
                }
            })
        })
        .boxed()
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjPath,
        to: &ObjPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// Recovery is an optimisation and runs only once a PUT reported the object already on the
/// store, so a bucket that denies `ListObjects` must cost throughput, not the batch. The
/// old code propagated the listing error and failed a replay whose data was already safe.
#[tokio::test]
async fn a_replay_survives_a_prefix_it_may_not_list() {
    let store: Arc<dyn ObjectStore> = Arc::new(UnlistableStore {
        inner: InMemory::new(),
        permanent: false,
        lists: Arc::new(AtomicUsize::new(0)),
    });
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;

    let batch = || vec![kafka_message(0), kafka_message(1)];
    assert!(matches!(
        publisher.send_batch(batch()).await.unwrap(),
        SentBatch::Ack
    ));

    // Same offsets again, on a fresh publisher so nothing is remembered in-process. The
    // listing it wants before writing is the one this store refuses.
    let mut replay = test_publisher(store.clone());
    replay.name_by = NameBy::SourcePosition;
    assert!(
        matches!(replay.send_batch(batch()).await.unwrap(), SentBatch::Ack),
        "a failed prefix listing must not fail a batch that is already on the store"
    );
    assert!(
        !replay.recovered.initialized(),
        "a failed recovery must stay retryable"
    );
}

/// The idempotent path must not trade the worker pool for its ordering guarantee. Naming
/// objects by source range is what makes concurrent writes safe, so the range bookkeeping
/// may only be locked around the map itself — never around the encode and the PUT, which
/// would leave an ordered sink publishing one object at a time.
#[tokio::test]
async fn idempotent_sends_are_not_serialised_by_the_covered_range_lock() {
    let store = Arc::new(BarrierStore::new(2));
    let mut publisher = test_publisher(store);
    publisher.name_by = NameBy::SourcePosition;
    let other = publisher.clone();

    // Disjoint offset runs, so both batches genuinely write and neither is skipped.
    let first = tokio::spawn(async move {
        publisher
            .send_batch_by_source_position(vec![kafka_message(0)])
            .await
    });
    let second = tokio::spawn(async move {
        other
            .send_batch_by_source_position(vec![kafka_message(1)])
            .await
    });

    let sends = async {
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), sends)
        .await
        .expect(
        "both puts must be in flight at once; the covered-range lock is being held across the put",
    );
}

/// The guarantee the idempotent sink exists to make, and the one the whole ordering
/// thread turns on: object order is *source* order even when batches are written
/// concurrently and out of order. Dispatches the batches in reverse and lets them race,
/// which is what the worker pool does to source order above `concurrency: 1`.
#[tokio::test]
async fn idempotent_objects_replay_in_source_order_when_written_concurrently() {
    const BATCHES: i64 = 10;

    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    // Raw writes the payload verbatim, so a record read back is the source row itself.
    publisher.format = FileFormat::Raw;

    let mut writes = tokio::task::JoinSet::new();
    for batch in (0..BATCHES).rev() {
        let publisher = publisher.clone();
        writes.spawn(async move {
            let base = batch * 2;
            publisher
                .send_batch(vec![kafka_message(base), kafka_message(base + 1)])
                .await
        });
    }
    while let Some(joined) = writes.join_next().await {
        joined.unwrap().unwrap();
    }

    // Lexicographic listing is the only ordering a bucket offers, so that is what a
    // reader gets and what this has to reproduce.
    let mut stream = store.list(Some(&ObjPath::from("data")));
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        names.push(item.unwrap().location);
    }
    names.sort();
    assert_eq!(names.len(), BATCHES as usize, "one part file per batch");

    let mut replayed = Vec::new();
    for name in &names {
        let bytes = store.get(name).await.unwrap().bytes().await.unwrap();
        for (start, end, _) in split_records(&bytes, b"\n", None) {
            let value: serde_json::Value = serde_json::from_slice(&bytes[start..end]).unwrap();
            replayed.push(value["offset"].as_i64().unwrap());
        }
    }
    assert_eq!(replayed, (0..BATCHES * 2).collect::<Vec<_>>());
}

/// Recovery is an optimisation, not a correctness requirement, so it must not cost a
/// prefix-wide LIST on the common path: writing into a fresh prefix never triggers it.
/// Recovery has to run before the first write, so it can no longer be free on a fresh
/// prefix — but it must stay *one* listing per publisher rather than one per batch.
#[tokio::test]
async fn a_prefix_is_listed_once_per_publisher() {
    let lists = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
        inner: InMemory::new(),
        lists: lists.clone(),
    });
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    for offset in 0..8 {
        publisher
            .send_batch(vec![kafka_message(offset)])
            .await
            .unwrap();
    }
    assert_eq!(
        lists.load(Ordering::SeqCst),
        1,
        "eight batches must share one listing, not pay for one each"
    );

    let mut written = store.list(Some(&ObjPath::from("data")));
    let mut count = 0;
    while written.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, 8, "an empty prefix must not filter anything out");
}

/// Recovery now runs on every batch until it succeeds, so a bucket that will never allow
/// `ListObjects` must not pay a failed round trip and a warning per batch for the life of
/// the route.
#[tokio::test]
async fn a_permanently_denied_listing_is_only_attempted_once() {
    let lists = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn ObjectStore> = Arc::new(UnlistableStore {
        inner: InMemory::new(),
        permanent: true,
        lists: lists.clone(),
    });
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    for offset in 0..5 {
        publisher
            .send_batch(vec![kafka_message(offset)])
            .await
            .unwrap();
    }
    assert_eq!(
        lists.load(Ordering::SeqCst),
        1,
        "a listing that is denied outright must not be retried per batch"
    );
    for offset in 0..5u64 {
        assert!(
            store.get(&part_key(offset, offset, "jsonl")).await.is_ok(),
            "the sink must keep writing without the listing"
        );
    }
}

/// The replay that the collision-triggered recovery got wrong: a restart whose batches
/// fall on different boundaries names different objects, so not one PUT collides and
/// nothing would ever ask for the listing. Every record would then be written a second
/// time under a name that overlaps the first run's without matching it.
#[tokio::test]
async fn a_replay_on_shifted_batch_boundaries_writes_nothing_twice() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    let mut first = test_publisher(store.clone());
    first.name_by = NameBy::SourcePosition;
    for chunk in [[0, 1], [2, 3], [4, 5]] {
        first
            .send_batch(chunk.map(kafka_message).to_vec())
            .await
            .unwrap();
    }

    // The same six offsets, re-read in threes. No name in common with the run above.
    let mut replay = test_publisher(store.clone());
    replay.name_by = NameBy::SourcePosition;
    for chunk in [[0, 1, 2], [3, 4, 5]] {
        replay
            .send_batch(chunk.map(kafka_message).to_vec())
            .await
            .unwrap();
    }

    assert_eq!(
        keys(store.as_ref()).await,
        vec![
            "data/part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl",
            "data/part-orders-0000000000-00000000000000000002-00000000000000000003.jsonl",
            "data/part-orders-0000000000-00000000000000000004-00000000000000000005.jsonl",
        ],
        "the replay must add no object; before the fix it added three that overlap these"
    );
}

/// Sorted keys under the test prefix.
async fn keys(store: &dyn ObjectStore) -> Vec<String> {
    let mut stream = store.list(Some(&ObjPath::from("data")));
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        names.push(item.unwrap().location.to_string());
    }
    names.sort();
    names
}

/// A restart into a prefix that already holds parts recovers exactly once, before its
/// first write, and then skips the rest of the replay without rewriting anything.
#[tokio::test]
async fn a_replay_recovers_once_and_then_skips() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut first = test_publisher(store.clone());
    first.name_by = NameBy::SourcePosition;
    first
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();

    // A restarted publisher starts with no knowledge of the prefix at all.
    let mut restarted = test_publisher(store.clone());
    restarted.name_by = NameBy::SourcePosition;
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    assert!(
        restarted.recovered.initialized(),
        "a populated prefix must be recovered before it is written to"
    );

    // The replayed offsets are now known covered, so the next batch writes only the new one.
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1), kafka_message(2)])
        .await
        .unwrap();
    let mut stream = store.list(Some(&ObjPath::from("data")));
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        names.push(item.unwrap().location.to_string());
    }
    names.sort();
    assert_eq!(
        names,
        vec![
            "data/part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl"
                .to_string(),
            "data/part-orders-0000000000-00000000000000000002-00000000000000000002.jsonl"
                .to_string(),
        ]
    );
}

#[tokio::test]
async fn idempotent_sink_recovers_finalized_objects_and_leaves_foreign_keys_alone() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();

    store
        .put(
            &ObjPath::from("data/.stage-crash"),
            PutPayload::from(Vec::new()),
        )
        .await
        .unwrap();
    let mut restarted = test_publisher(store.clone());
    restarted.name_by = NameBy::SourcePosition;
    restarted.covered_ranges = Arc::new(Mutex::new(
        recover_finalized_object_ranges(store.as_ref(), &restarted.base, "jsonl")
            .await
            .unwrap(),
    ));
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1), kafka_message(2)])
        .await
        .unwrap();

    let mut stream = store.list(Some(&ObjPath::from("data")));
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        names.push(item.unwrap().location.to_string());
    }
    names.sort();
    assert_eq!(
        names,
        vec![
            // Not a parseable part name, and not ours to delete.
            "data/.stage-crash".to_string(),
            part_key(0, 1, "jsonl").to_string(),
            part_key(2, 2, "jsonl").to_string(),
        ]
    );
}

/// `list` is recursive, so a sink whose base is a *sub*prefix of ours shows up in our
/// listing with a perfectly parseable part name. Counting it would mark that range
/// covered here and silently drop the records that were never written under our prefix.
#[tokio::test]
async fn recovery_ignores_parseable_parts_under_a_nested_prefix() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let nested = ObjPath::from(format!(
        "data/other/{}",
        part_key(0, 1, "jsonl").filename().unwrap()
    ));
    store
        .put(&nested, PutPayload::from("someone else's part"))
        .await
        .unwrap();

    let base = ObjPath::from("data");
    let covered = recover_finalized_object_ranges(store.as_ref(), &base, "jsonl")
        .await
        .unwrap();

    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    publisher.covered_ranges = Arc::new(Mutex::new(covered));
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();

    let written = store
        .get(&part_key(0, 1, "jsonl"))
        .await
        .expect("the nested part must not count as coverage")
        .bytes()
        .await
        .unwrap();
    assert!(!written.is_empty());
}

#[tokio::test]
async fn idempotent_sink_does_not_overwrite_an_existing_range_object() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let key = part_key(0, 1, "jsonl");
    store
        .put(&key, PutPayload::from("already committed"))
        .await
        .unwrap();

    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .expect("an existing deterministic object is a successful retry");

    let stored = store.get(&key).await.unwrap().bytes().await.unwrap();
    assert_eq!(stored.as_ref(), b"already committed");
}

#[tokio::test]
async fn source_position_overrides_date_partition_rather_than_rejecting_it() {
    // `auto` resolves to source_position on most ETL inputs, so erroring here would
    // reject configs that never asked for the two to be combined.
    let publisher = ObjectStorePublisher::new(&ObjectStoreConfig {
        url: "memory:///data".to_string(),
        name_by: NameBy::SourcePosition,
        date_partition: Some(true),
        ..Default::default()
    })
    .await
    .expect("date_partition is overridden, not rejected");
    assert!(!publisher.date_partition);
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn idempotent_parts_are_compressed_and_named_for_it() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut publisher = test_publisher(store.clone());
    publisher.name_by = NameBy::SourcePosition;
    publisher.compression = Compression::Gzip;
    publisher.extension = extension_for(&FileFormat::Json, Compression::Gzip, false);
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();

    // The part name advertises the codec, and the bytes really are gzip.
    let key = part_key(0, 1, "jsonl.gz");
    let stored = store.get(&key).await.unwrap().bytes().await.unwrap();
    let plain =
        crate::support::compression::decompress_all(Compression::Gzip, &stored, None).unwrap();
    assert_eq!(split_records(&plain, b"\n", None).len(), 2);

    // A restart parses the longer extension back out, so covered offsets are not rewritten.
    let recovered = recover_finalized_object_ranges(store.as_ref(), &publisher.base, "jsonl.gz")
        .await
        .unwrap();
    let mut restarted = test_publisher(store.clone());
    restarted.name_by = NameBy::SourcePosition;
    restarted.compression = Compression::Gzip;
    restarted.extension = publisher.extension.clone();
    restarted.covered_ranges = Arc::new(Mutex::new(recovered));
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();

    let mut stream = store.list(Some(&ObjPath::from("data")));
    let mut names = Vec::new();
    while let Some(item) = stream.next().await {
        names.push(item.unwrap().location.to_string());
    }
    assert_eq!(names, vec![part_key(0, 1, "jsonl.gz").to_string()]);
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn compressed_object_round_trips() {
    for compression in [Compression::Gzip, Compression::Lz4, Compression::Zstd] {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let mut publisher = test_publisher(store.clone());
        publisher.format = FileFormat::Raw;
        publisher.compression = compression;
        publisher.extension = extension_for(&FileFormat::Raw, compression, false);
        publisher
            .send_batch(vec![
                json_msg(serde_json::json!({"n": 1})),
                json_msg(serde_json::json!({"n": 2})),
            ])
            .await
            .unwrap();

        // The stored object is really compressed: it decodes to the two JSONL rows.
        let listed = store
            .list(Some(&ObjPath::from("data")))
            .next()
            .await
            .unwrap()
            .unwrap();
        let suffix = match compression {
            Compression::Gzip => ".bin.gz",
            Compression::Zstd => ".bin.zst",
            _ => ".bin.lz4",
        };
        assert!(listed.location.to_string().ends_with(suffix));
        let bytes = store
            .get(&listed.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let decoded =
            crate::support::compression::decompress_all(compression, &bytes, None).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap().lines().count(), 2);

        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Raw,
            None,
            None,
        );
        consumer.compression = compression;
        let batch = consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 2);
        assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
        assert_eq!(batch.messages[1].payload.as_ref(), br#"{"n":2}"#);
        (batch.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .unwrap();
    }
}

#[cfg(all(feature = "compression", feature = "encryption"))]
#[tokio::test]
async fn encrypted_object_round_trips() {
    use base64::Engine as _;

    let crypto_cfg = crate::models::EncryptionConfig {
        key: base64::engine::general_purpose::STANDARD.encode([42u8; 32]),
        ..Default::default()
    };
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    let mut publisher = test_publisher(store.clone());
    publisher.format = FileFormat::Raw;
    publisher.compression = Compression::Gzip;
    publisher.crypto = Some(Arc::new(Crypto::new(&crypto_cfg).unwrap()));
    publisher
        .send_batch(vec![json_msg(serde_json::json!({"who": "alice"}))])
        .await
        .unwrap();

    // The stored object is ciphertext: not gzip, and no plaintext inside.
    let listed = store
        .list(Some(&ObjPath::from("data")))
        .next()
        .await
        .unwrap()
        .unwrap();
    let bytes = store
        .get(&listed.location)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(crate::support::compression::decompress_all(Compression::Gzip, &bytes, None).is_err());
    assert!(!bytes.windows(5).any(|w| w == b"alice"));

    let mut consumer = ObjectStoreConsumer::from_store(
        store.clone(),
        ObjPath::from("data"),
        FileFormat::Raw,
        None,
        None,
    );
    consumer.compression = Compression::Gzip;
    consumer.crypto = Some(Arc::new(Crypto::new(&crypto_cfg).unwrap()));
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].payload.as_ref(), br#"{"who":"alice"}"#);

    // A consumer with the wrong key fails cleanly.
    let mut wrong =
        ObjectStoreConsumer::from_store(store, ObjPath::from("data"), FileFormat::Raw, None, None);
    wrong.compression = Compression::Gzip;
    let wrong_cfg = crate::models::EncryptionConfig {
        key: base64::engine::general_purpose::STANDARD.encode([1u8; 32]),
        ..Default::default()
    };
    wrong.crypto = Some(Arc::new(Crypto::new(&wrong_cfg).unwrap()));
    // Nothing decoded before it, so the setting is at fault: the route stops and the
    // checkpoint stays where it is.
    let error = fail_after_retries(&mut wrong).await;
    assert!(error.contains("`encryption`"), "{error}");
    assert!(wrong.last_key.lock().await.is_none());
}

/// Reads until the consumer gives up on the object it cannot decode.
#[cfg(feature = "compression")]
async fn fail_after_retries(consumer: &mut ObjectStoreConsumer) -> String {
    for _ in 0..MAX_OBJECT_DECODE_FAILURES {
        match consumer.receive_batch(10).await {
            Ok(batch) => assert!(batch.messages.is_empty()),
            Err(ConsumerError::Permanent(error)) => return format!("{error:#}"),
            Err(other) => panic!("expected a permanent error, got {other}"),
        }
    }
    panic!("the undecodable object did not stop the source");
}

#[cfg(feature = "compression")]
async fn gzip_source_with(objects: &[(&str, Vec<u8>)]) -> ObjectStoreConsumer {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    for (key, body) in objects {
        store
            .put(&ObjPath::from(*key), PutPayload::from(body.clone()))
            .await
            .unwrap();
    }
    let mut consumer =
        ObjectStoreConsumer::from_store(store, ObjPath::from("data"), FileFormat::Raw, None, None);
    consumer.compression = Compression::Gzip;
    consumer
}

#[cfg(feature = "compression")]
fn gzipped(text: &str) -> Vec<u8> {
    crate::support::compression::compress_member(Compression::Gzip, text.as_bytes()).unwrap()
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn gzip_source_stops_when_its_first_object_is_no_gzip() {
    let mut consumer = gzip_source_with(&[("data/1-not-gzip.gz", br#"{"n":1}"#.to_vec())]).await;
    let error = fail_after_retries(&mut consumer).await;
    assert!(error.contains("`compression`"), "{error}");
    assert!(consumer.last_key.lock().await.is_none());
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn gzip_source_skips_a_bad_object_after_a_good_one() {
    let mut consumer = gzip_source_with(&[
        ("data/1-good.gz", gzipped("one")),
        ("data/2-not-gzip.gz", b"plain".to_vec()),
        ("data/3-good.gz", gzipped("three")),
    ])
    .await;
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let batch = consumer.receive_batch(10).await.unwrap();
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        seen.extend(
            batch
                .messages
                .iter()
                .map(|m| m.get_payload_str().to_string()),
        );
        (batch.commit)(acks).await.unwrap();
    }
    assert_eq!(seen, ["one", "three"]);
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn gzip_source_with_fail_policy_stops_at_a_bad_object_after_a_good_one() {
    let mut consumer = gzip_source_with(&[
        ("data/1-good.gz", gzipped("one")),
        ("data/2-not-gzip.gz", b"plain".to_vec()),
    ])
    .await;
    consumer.on_decode_error = DecodeErrorPolicy::Fail;
    let batch = consumer.receive_batch(10).await.unwrap();
    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    let error = fail_after_retries(&mut consumer).await;
    assert!(error.contains("on_decode_error: fail"), "{error}");
    assert_eq!(
        consumer.last_key.lock().await.as_deref(),
        Some("data/1-good.gz")
    );
}

#[tokio::test]
async fn sink_writes_object_and_source_round_trips() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    let publisher = test_publisher(store.clone());
    publisher
        .send_batch(vec![
            json_msg(serde_json::json!({"n": 1})),
            json_msg(serde_json::json!({"n": 2})),
        ])
        .await
        .unwrap();

    // One object exists under the prefix.
    let mut listed = store.list(Some(&ObjPath::from("data")));
    let first = listed.next().await.unwrap().unwrap();
    assert!(first.location.to_string().starts_with("data/"));

    let mut consumer = ObjectStoreConsumer::from_store(
        store,
        ObjPath::from("data"),
        FileFormat::Normal,
        None,
        None,
    );

    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
    assert_eq!(batch.messages[1].payload.as_ref(), br#"{"n":2}"#);
    (batch.commit)(vec![MessageDisposition::Ack; 2])
        .await
        .unwrap();

    // Cursor advanced past the object; a further read is idle (empty).
    let drained = consumer.receive_batch(10).await.unwrap();
    assert!(drained.messages.is_empty());
}

#[tokio::test]
async fn local_sink_uses_the_configured_fsync_interval() {
    let dir = tempfile::tempdir().unwrap();
    let publisher = ObjectStorePublisher::new(&ObjectStoreConfig {
        url: format!("file://{}", dir.path().display()),
        fsync_interval_ms: Some(25),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        publisher.deferred_commit_interval(),
        Some(Duration::from_millis(25))
    );
}

#[tokio::test]
async fn local_filesystem_sink_and_source_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}", dir.path().display()),
        format: FileFormat::Normal,
        date_partition: Some(false),
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let publisher = ObjectStorePublisher::new(&config).await.unwrap();
    publisher
        .send_batch(vec![json_msg(serde_json::json!({"source": "local"}))])
        .await
        .unwrap();
    assert_eq!(
        publisher.deferred_commit_interval(),
        Some(FINALIZE_INTERVAL)
    );
    let mut early = ObjectStoreConsumer::new(&config).await.unwrap();
    assert!(early.receive_batch(10).await.unwrap().messages.is_empty());
    publisher.flush().await.unwrap();

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].payload.as_ref(), br#"{"source":"local"}"#);
    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();

    let drained = consumer.receive_batch(10).await.unwrap();
    assert!(drained.messages.is_empty());
}

#[tokio::test]
async fn csv_sink_writes_a_header_into_every_object() {
    let dir = tempfile::tempdir().unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}", dir.path().display()),
        format: FileFormat::Csv,
        csv: crate::models::CsvConfig {
            separator: Some(";".to_string()),
            ..Default::default()
        },
        date_partition: Some(false),
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let publisher = ObjectStorePublisher::new(&config).await.unwrap();
    let sent = publisher
        .send_batch(vec![
            CanonicalMessage::new(br#"{"id":1,"stats":{"n":2,"avg":1.5}}"#.to_vec(), None),
            CanonicalMessage::new(b"not json".to_vec(), None),
            CanonicalMessage::new(br#"{"id":2,"stats":{"n":3,"avg":"a;b"}}"#.to_vec(), None),
        ])
        .await
        .unwrap();
    assert!(matches!(sent, SentBatch::Partial { ref failed, .. } if failed.len() == 1));
    publisher
        .send_batch(vec![CanonicalMessage::new(
            br#"{"other":"x"}"#.to_vec(),
            None,
        )])
        .await
        .unwrap();
    publisher.flush().await.unwrap();

    let mut objects: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    objects.sort();
    assert_eq!(
        objects,
        ["id;stats.n;stats.avg\n1;2;1.5\n2;3;\"a;b\"\n", "other\nx\n"]
    );

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    let mut rows = Vec::new();
    for _ in 0..2 {
        let batch = consumer.receive_batch(10).await.unwrap();
        rows.extend(batch.messages.iter().map(|m| m.payload.to_vec()));
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        (batch.commit)(acks).await.unwrap();
    }
    rows.sort();
    assert_eq!(
        rows,
        [
            br#"{"id":"1","stats.n":"2","stats.avg":"1.5"}"#.to_vec(),
            br#"{"id":"2","stats.n":"3","stats.avg":"a;b"}"#.to_vec(),
            br#"{"other":"x"}"#.to_vec(),
        ]
    );
}

#[tokio::test]
async fn csv_sink_cannot_guess_its_separator() {
    let config = ObjectStoreConfig {
        url: "memory:///data".to_string(),
        format: FileFormat::Csv,
        csv: crate::models::CsvConfig {
            separator: Some("auto".to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(ObjectStorePublisher::new(&config).await.is_err());
}

#[tokio::test]
async fn local_filesystem_source_reads_csv_drop_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("2026-09-03-orders.csv"),
        b"id,name\n1,Ada\n2,Grace\n",
    )
    .unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}", dir.path().display()),
        format: FileFormat::Csv,
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    assert_eq!(
        batch.messages[0].payload.as_ref(),
        br#"{"id":"1","name":"Ada"}"#
    );
    assert_eq!(
        batch.messages[1].payload.as_ref(),
        br#"{"id":"2","name":"Grace"}"#
    );
    (batch.commit)(vec![MessageDisposition::Ack; 2])
        .await
        .unwrap();
}

/// RFC 4180 lets a quoted field carry the record separator; the object splitter must
/// frame records the way the file reader does, CRLF included.
#[tokio::test]
async fn csv_drop_file_keeps_quoted_newlines_in_one_record() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("notes.csv"),
        b"id,note\r\n1,\"Line1\r\nLine2\"\r\n2,\"a,\"\"b\"\"\"\r\n",
    )
    .unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}", dir.path().display()),
        format: FileFormat::Csv,
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    let rows: Vec<serde_json::Value> = batch
        .messages
        .iter()
        .map(|m| serde_json::from_slice(&m.payload).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![
            serde_json::json!({"id": "1", "note": "Line1\r\nLine2"}),
            serde_json::json!({"id": "2", "note": "a,\"b\""}),
        ]
    );
    (batch.commit)(vec![MessageDisposition::Ack; 2])
        .await
        .unwrap();
}

#[test]
fn split_records_is_quote_aware_only_for_csv() {
    let data = b"a,\"x\ny\"\nb\n";
    let split = |csv: Option<&CsvDialect>| -> Vec<&[u8]> {
        split_records(data, b"\n", csv)
            .into_iter()
            .map(|(start, end, _)| &data[start..end])
            .collect()
    };
    assert_eq!(
        split(Some(&CsvDialect::default())),
        vec![&b"a,\"x\ny\""[..], b"b"]
    );
    assert_eq!(split(None), vec![&b"a,\"x"[..], b"y\"", b"b"]);
}

#[tokio::test]
async fn a_url_naming_one_object_reads_only_that_object() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.jsonl"), b"{\"n\":1}\n").unwrap();
    std::fs::write(dir.path().join("a.jsonl.bak"), b"{\"n\":2}\n").unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}/a.jsonl", dir.path().display()),
        format: FileFormat::Json,
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    assert!(consumer
        .receive_batch(10)
        .await
        .unwrap()
        .messages
        .is_empty());
}

#[tokio::test]
async fn a_url_naming_one_object_reads_it_once_it_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let config = ObjectStoreConfig {
        url: format!("file://{}/a.jsonl", dir.path().display()),
        format: FileFormat::Json,
        polling_interval_ms: Some(1),
        ..Default::default()
    };

    let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
    assert!(consumer
        .receive_batch(10)
        .await
        .unwrap()
        .messages
        .is_empty());
    std::fs::write(dir.path().join("a.jsonl"), b"{\"n\":1}\n").unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(batch.messages[0].payload.as_ref(), br#"{"n":1}"#);
}

#[tokio::test]
async fn an_exact_key_written_after_a_child_is_still_read() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut consumer = ObjectStoreConsumer::from_store(
        store.clone(),
        ObjPath::from("a"),
        FileFormat::Json,
        None,
        None,
    );
    consumer.single_object = None;
    store
        .put(&ObjPath::from("a/child"), b"{\"n\":2}\n".to_vec().into())
        .await
        .unwrap();
    store
        .put(&ObjPath::from("a"), b"{\"n\":1}\n".to_vec().into())
        .await
        .unwrap();

    let mut payloads = Vec::new();
    for _ in 0..2 {
        let batch = consumer.receive_batch(10).await.unwrap();
        payloads.extend(batch.messages.iter().map(|m| m.payload.to_vec()));
        (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
            .await
            .unwrap();
    }
    assert_eq!(
        payloads,
        vec![br#"{"n":1}"#.to_vec(), br#"{"n":2}"#.to_vec()]
    );
}

#[tokio::test]
async fn nacked_records_are_redelivered() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let publisher = test_publisher(store.clone());
    publisher
        .send_batch(vec![
            json_msg(serde_json::json!({"n": 1})),
            json_msg(serde_json::json!({"n": 2})),
        ])
        .await
        .unwrap();

    let mut consumer = ObjectStoreConsumer::from_store(
        store,
        ObjPath::from("data"),
        FileFormat::Normal,
        None,
        None,
    );

    // Ack the first, nack the second -> the second is requeued.
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    (batch.commit)(vec![MessageDisposition::Ack, MessageDisposition::Nack])
        .await
        .unwrap();

    // The nacked record is redelivered (object not yet fully acked).
    let retry = consumer.receive_batch(10).await.unwrap();
    assert_eq!(retry.messages.len(), 1);
    assert_eq!(retry.messages[0].payload.as_ref(), br#"{"n":2}"#);
    (retry.commit)(vec![MessageDisposition::Ack]).await.unwrap();

    let drained = consumer.receive_batch(10).await.unwrap();
    assert!(drained.messages.is_empty());
}

#[test]
fn parquet_extension_carries_no_compression_suffix() {
    assert_eq!(
        extension_for(&FileFormat::Parquet, Compression::Zstd, false),
        "parquet"
    );
    assert_eq!(
        extension_for(&FileFormat::Parquet, Compression::Gzip, true),
        "parquet.enc"
    );
}

#[cfg(feature = "parquet")]
async fn object_bytes(store: &dyn ObjectStore, key: &str) -> Vec<u8> {
    let key = ObjPath::from(key);
    store
        .get(&key)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn parquet_round_trips_typed_rows_for_every_codec() {
    let rows = [
        serde_json::json!({"id": 1, "name": "alice", "score": 1.5, "active": true}),
        serde_json::json!({"id": 2, "name": "bob", "score": 2.25, "active": false}),
        serde_json::json!({"id": 3, "name": "carol", "score": 3.0, "active": true}),
    ];
    for compression in [
        Compression::None,
        Compression::Gzip,
        Compression::Lz4,
        Compression::Zstd,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let config = ObjectStoreConfig {
            url: format!("file://{}", dir.path().display()),
            format: FileFormat::Parquet,
            compression,
            date_partition: Some(false),
            polling_interval_ms: Some(1),
            ..Default::default()
        };
        let publisher = ObjectStorePublisher::new(&config).await.unwrap();
        publisher
            .send_batch(rows.iter().cloned().map(json_msg).collect())
            .await
            .unwrap();
        publisher.flush().await.unwrap();

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension().unwrap(), "parquet");
        let bytes = std::fs::read(&files[0]).unwrap();
        assert!(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"));

        let mut consumer = ObjectStoreConsumer::new(&config).await.unwrap();
        let batch = consumer.receive_batch(10).await.unwrap();
        let read: Vec<serde_json::Value> = batch
            .messages
            .iter()
            .map(|m| serde_json::from_slice(&m.payload).unwrap())
            .collect();
        assert_eq!(read, rows, "{compression:?}");
        assert_eq!(
            batch.messages[0].get_metadata("mq_bridge.original_format"),
            Some("parquet")
        );
        (batch.commit)(vec![MessageDisposition::Ack; 3])
            .await
            .unwrap();
    }
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn parquet_object_is_decoded_as_it_is_consumed() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let rows: Vec<_> = (0..200).map(|i| serde_json::json!({"id": i})).collect();
    let body = crate::support::parquet::encode_rows(&rows, Compression::None).unwrap();
    store
        .put(&ObjPath::from("data/a.parquet"), PutPayload::from(body))
        .await
        .unwrap();
    let mut consumer = ObjectStoreConsumer::from_store(
        store,
        ObjPath::from("data"),
        FileFormat::Parquet,
        None,
        None,
    );
    let mut ids = Vec::new();
    while ids.len() < rows.len() {
        let batch = consumer.receive_batch(50).await.unwrap();
        assert!(!batch.messages.is_empty() && batch.messages.len() <= 50);
        assert!(consumer.buffer.lock().await.len() < 64, "decoded ahead");
        assert_eq!(*consumer.last_key.lock().await, None, "cursor moved early");
        for message in &batch.messages {
            let row: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
            ids.push(row["id"].as_i64().unwrap());
        }
        (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
            .await
            .unwrap();
    }
    assert_eq!(ids, (0..200).collect::<Vec<_>>());
    assert_eq!(
        consumer.last_key.lock().await.as_deref(),
        Some("data/a.parquet")
    );
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn parquet_object_failing_part_way_follows_on_decode_error() {
    let pad = "x".repeat(20_000);
    let rows: Vec<_> = (0..128)
        .map(|i| serde_json::json!({"id": i, "pad": if i < 64 { "" } else { pad.as_str() }}))
        .collect();
    let body = crate::support::parquet::encode_rows(&rows, Compression::Zstd).unwrap();
    // The second chunk decodes past the per-chunk bound, the first does not.
    let max_object_bytes = body.len() as u64;
    assert!(max_object_bytes * 20 < 64 * 20_000);
    for policy in [DecodeErrorPolicy::Skip, DecodeErrorPolicy::Fail] {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjPath::from("data/a.parquet"),
                PutPayload::from(body.clone()),
            )
            .await
            .unwrap();
        let mut consumer = ObjectStoreConsumer::from_store(
            store,
            ObjPath::from("data"),
            FileFormat::Parquet,
            None,
            None,
        );
        consumer.max_object_bytes = Some(max_object_bytes);
        consumer.on_decode_error = policy;

        let first = consumer.receive_batch(64).await.unwrap();
        assert_eq!(first.messages.len(), 64);
        (first.commit)(vec![MessageDisposition::Ack; 64])
            .await
            .unwrap();
        let next = consumer.receive_batch(64).await;
        let last_key = consumer.last_key.lock().await.clone();
        match policy {
            DecodeErrorPolicy::Skip => {
                assert!(next.unwrap().messages.is_empty());
                assert_eq!(last_key.as_deref(), Some("data/a.parquet"));
            }
            DecodeErrorPolicy::Fail => {
                assert!(matches!(next, Err(ConsumerError::Permanent(_))));
                assert_eq!(last_key, None);
            }
        }
    }
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn parquet_sink_fails_only_non_object_rows() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut publisher = test_publisher(store.clone());
    publisher.format = FileFormat::Parquet;
    publisher.extension = "parquet".to_string();
    let sent = publisher
        .send_batch(vec![
            json_msg(serde_json::json!({"n": 1})),
            json_msg(serde_json::json!([1, 2])),
            CanonicalMessage::new(b"not json".to_vec(), None),
            json_msg(serde_json::json!({"n": 2})),
        ])
        .await
        .unwrap();
    let SentBatch::Partial { failed, .. } = sent else {
        panic!("expected a partial send");
    };
    assert_eq!(failed.len(), 2);

    let keys = keys(store.as_ref()).await;
    assert_eq!(keys.len(), 1);
    let rows =
        crate::support::parquet::decode_rows(object_bytes(store.as_ref(), &keys[0]).await, None)
            .unwrap();
    assert_eq!(rows.len(), 2);
}

#[cfg(feature = "parquet")]
#[tokio::test]
async fn parquet_parts_by_source_position_split_at_a_bad_row() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut publisher = test_publisher(store.clone());
    publisher.format = FileFormat::Parquet;
    publisher.compression = Compression::Zstd;
    publisher.extension = extension_for(&FileFormat::Parquet, Compression::Zstd, false);
    publisher.name_by = NameBy::SourcePosition;
    let mut bad = kafka_message(1);
    bad.payload = b"\"a string\"".to_vec().into();
    let sent = publisher
        .send_batch(vec![kafka_message(0), bad, kafka_message(2)])
        .await
        .unwrap();
    assert!(matches!(sent, SentBatch::Partial { ref failed, .. } if failed.len() == 1));

    assert_eq!(
        keys(store.as_ref()).await,
        vec![
            part_key(0, 0, "parquet").to_string(),
            part_key(2, 2, "parquet").to_string(),
        ]
    );

    let mut consumer = ObjectStoreConsumer::from_store(
        store,
        ObjPath::from("data"),
        FileFormat::Parquet,
        None,
        None,
    );
    let mut offsets = Vec::new();
    for _ in 0..2 {
        let batch = consumer.receive_batch(10).await.unwrap();
        for message in &batch.messages {
            let row: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
            offsets.push(row["offset"].as_i64().unwrap());
        }
        (batch.commit)(vec![MessageDisposition::Ack; batch.messages.len()])
            .await
            .unwrap();
    }
    assert_eq!(offsets, vec![0, 2]);
}

#[cfg(feature = "test-utils")]
#[test]
fn a_large_object_decodes_in_parallel() {
    let probe = || crate::support::probe::count("file.decode.parallel");
    let before = probe();
    let object: String = (0..2048).map(|i| format!("{{\"id\":{i}}}\n")).collect();
    let msgs = split_and_parse(
        object.into(),
        b"\n",
        &FileFormat::Json,
        &CsvDialect::default(),
    );
    assert_eq!(msgs.len(), 2048);
    if std::thread::available_parallelism().map_or(1, |n| n.get()) > 1 {
        assert!(
            probe() > before,
            "a 2048-record object must decode in parallel"
        );
    }
}

/// The dialect reaches the splitter and the row decoder alike: the quoted line break
/// stays in one record, and the separator is guessed from the object's first line.
#[test]
fn csv_objects_follow_the_configured_dialect() {
    let config = crate::models::CsvConfig {
        separator: Some("auto".to_string()),
        ..Default::default()
    };
    let csv = CsvDialect::from_config(&config, b"\n").unwrap();
    let object = "\u{feff}id;note\r\n1;\"a\nb;c\"\r\n2;x,y\r\n";
    let rows: Vec<serde_json::Value> =
        split_and_parse(bytes::Bytes::from(object), b"\n", &FileFormat::Csv, &csv)
            .iter()
            .map(|msg| serde_json::from_slice(&msg.payload).unwrap())
            .collect();
    assert_eq!(
        rows,
        vec![
            serde_json::json!({"id": "1", "note": "a\nb;c"}),
            serde_json::json!({"id": "2", "note": "x,y"}),
        ]
    );
}

/// Reads `objects` one-record files through a draining route and returns what arrived.
async fn drain_through_a_route(objects: usize, empty_object: bool) -> usize {
    use crate::models::{Endpoint, EndpointType};
    let temp = tempfile::tempdir().unwrap();
    // `~` as in a Windows short path: a key has to come back from the listing unchanged.
    let dir = temp.path().join("drain~1");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..objects {
        std::fs::write(dir.join(format!("part-{i}.jsonl")), format!("{i}\n")).unwrap();
    }
    if empty_object {
        std::fs::write(dir.join("part-2x.jsonl"), "").unwrap();
    }
    let config: ObjectStoreConfig = serde_json::from_value(serde_json::json!({
        "url": url::Url::from_directory_path(&dir).unwrap().to_string(),
        "format": "raw",
    }))
    .unwrap();
    let topic = format!("object_drain_{}", fast_uuid_v7::gen_id());
    let route = crate::Route::new(
        Endpoint::new(EndpointType::ObjectStore(config)),
        Endpoint::new_memory(&topic, 100),
    )
    .with_exit_on_empty(true);
    let mut output = route.connect_to_output("object_drain").await.unwrap();
    let handle = route.run(&topic).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while handle.outcome().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the drain ends");
    handle.join().await.unwrap();
    let mut received = 0;
    while let Ok(Ok(item)) =
        tokio::time::timeout(Duration::from_millis(300), output.receive()).await
    {
        (item.commit)(MessageDisposition::Ack).await.unwrap();
        received += 1;
    }
    received
}

#[tokio::test]
async fn a_batch_dropped_without_a_commit_is_read_again() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    store
        .put(&ObjPath::from("data/a.jsonl"), b"1\n2\n".to_vec().into())
        .await
        .unwrap();
    let mut consumer =
        ObjectStoreConsumer::from_store(store, ObjPath::from("data"), FileFormat::Raw, None, None);
    drop(consumer.receive_batch(10).await.unwrap());
    let again = tokio::time::timeout(Duration::from_secs(2), consumer.receive_batch(10))
        .await
        .expect("the object is not stuck in flight")
        .unwrap();
    assert_eq!(again.messages.len(), 2);
}

#[tokio::test]
async fn a_drain_reads_every_object() {
    assert_eq!(drain_through_a_route(6, false).await, 6);
}

#[tokio::test]
async fn an_object_without_records_does_not_end_a_drain() {
    assert_eq!(drain_through_a_route(6, true).await, 6);
}
