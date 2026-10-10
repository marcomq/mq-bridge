use super::*;
use crate::models::{DirSpoolConfig, SpoolClaim, SpoolDone, SpoolFsync};
use crate::traits::{DisconnectOutcome, MessageConsumer, MessagePublisher};
use tempfile::tempdir;

fn config(path: &std::path::Path) -> DirSpoolConfig {
    DirSpoolConfig::new(path.to_str().unwrap())
}

fn message(payload: &str, kind: &str) -> CanonicalMessage {
    let mut msg = CanonicalMessage::from_vec(payload.as_bytes().to_vec());
    msg.metadata.insert("kind".to_string(), kind.to_string());
    msg
}

/// Every file in `dir`, sorted, so an assertion can name exactly what a spool holds.
fn entries(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Closes a producer the way the route does, with the outcome in scope.
async fn close_producer(publisher: &DirSpoolPublisher, outcome: DisconnectOutcome) {
    crate::traits::with_disconnect_outcome(outcome, async {
        publisher
            .on_disconnect_hook()
            .expect("the publisher always has closing work to do")
            .await
            .unwrap();
    })
    .await;
}

/// Every file under `dir`, as sorted relative paths, so an assertion can name the shape of
/// a sharded spool.
fn tree_entries(dir: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, at: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Drains the consumer until the queue runs out, acking everything it hands out. Under
/// `stop_on_done` the queue running out is reported as `EndOfStream` rather than as an
/// empty batch, and both end the drain; the signal is repeatable, so a caller can still
/// assert on it afterwards.
async fn drain(consumer: &mut DirSpoolConsumer, max: usize) -> Vec<CanonicalMessage> {
    let mut collected = Vec::new();
    loop {
        let batch = match consumer.receive_batch(max).await {
            Ok(batch) => batch,
            Err(ConsumerError::EndOfStream) => return collected,
            Err(error) => panic!("unexpected consumer error: {error:?}"),
        };
        if batch.messages.is_empty() {
            return collected;
        }
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        collected.extend(batch.messages);
        (batch.commit)(acks).await.unwrap();
    }
}

#[test]
fn render_name_supports_the_documented_placeholders() {
    assert_eq!(render_name("{seq}", 7, 0), "7");
    assert_eq!(render_name("{seq:06}", 7, 0), "000007");
    // The printf spelling from the feature request.
    assert_eq!(render_name("{seq:06d}", 7, 0), "000007");
    assert_eq!(render_name("chunk-{seq:03}", 42, 0), "chunk-042");
    // An unknown placeholder is copied through rather than dropped, so a typo is
    // visible in the file name instead of silently collapsing every chunk onto one.
    assert_eq!(render_name("{nope}-{seq:02}", 1, 0), "{nope}-01");
    assert!(render_name("{seq:04}_{timestamp}", 3, 0).starts_with("0003_"));
}

/// Pins the spelling documented in docs/CONFIGURATION.md, including the defaults an
/// omitted field falls back to.
#[test]
fn parses_the_documented_yaml_shape() {
    use crate::models::{Endpoint, EndpointType};

    let endpoint: Endpoint = serde_yaml_ng::from_str(
        r#"
dir_spool:
  path: "/tmp/video_telemetry_spool"
  naming_pattern: "{seq:09}_{timestamp}"
  shard_depth: 2
  shard_width: 3
  payload_extension: ".h264"
  metadata_extension: ""
  atomic: true
  emit_done: success
"#,
    )
    .expect("the documented publisher config must parse");
    let EndpointType::DirSpool(cfg) = endpoint.endpoint_type else {
        panic!("expected a dir_spool endpoint");
    };
    assert_eq!(cfg.path, "/tmp/video_telemetry_spool");
    assert_eq!(cfg.naming_pattern, "{seq:09}_{timestamp}");
    assert_eq!(cfg.shard_depth, 2);
    assert_eq!(cfg.shard_width, 3);
    // A leading dot is accepted and stripped, so `.h264` and `h264` mean the same thing.
    assert_eq!(cfg.payload_suffix(), "h264");
    // The video example writes no sidecar: a frame chunk is self-describing.
    assert_eq!(cfg.metadata_suffix(), None);
    assert_eq!(cfg.emit_done, SpoolDone::Success);
    // Consumer-side fields keep their defaults on a publisher block.
    assert!(cfg.drain_on_read);
    assert!(!cfg.stop_on_done);
    assert_eq!(cfg.poll_interval_ms, 100);
    // The three control files, and exclusive locking, unless told otherwise.
    assert_eq!(cfg.done_file, "DONE");
    assert_eq!(cfg.producer_file, "PRODUCER");
    assert_eq!(cfg.consumer_file, "CONSUMER");
    assert_eq!(cfg.claim, SpoolClaim::Exclusive);

    let endpoint: Endpoint = serde_yaml_ng::from_str(
        r#"
dir_spool:
  path: "/tmp/video_telemetry_spool"
  payload_extension: ".h264"
  drain_on_read: true
  stop_on_done: true
"#,
    )
    .expect("the documented consumer config must parse");
    let EndpointType::DirSpool(cfg) = endpoint.endpoint_type else {
        panic!("expected a dir_spool endpoint");
    };
    assert!(cfg.stop_on_done);
    assert_eq!(cfg.naming_pattern, "{seq:09}");
    // A flat spool unless asked otherwise, and the default padding is wide enough to shard.
    assert_eq!(cfg.shard_depth, 0);
    assert_eq!(cfg.shard_width, 3);
    // The general default keeps metadata, and pays for it with a second file per message.
    assert_eq!(cfg.metadata_suffix(), Some("json"));
    assert_eq!(cfg.fsync, SpoolFsync::Chunk);
}

/// `spool` is the short spelling the feature request used; both must reach the same type.
#[test]
fn accepts_the_spool_alias() {
    use crate::models::{Endpoint, EndpointType};

    let endpoint: Endpoint =
        serde_yaml_ng::from_str("spool:\n  path: \"/tmp/s\"\n").expect("alias must parse");
    assert!(matches!(endpoint.endpoint_type, EndpointType::DirSpool(_)));
}

/// A misspelled field must name itself rather than fall through to the custom-endpoint
/// path, which would report an opaque unknown-endpoint failure instead.
#[test]
fn rejects_an_unknown_field() {
    use crate::models::Endpoint;

    let error =
        serde_yaml_ng::from_str::<Endpoint>("dir_spool:\n  path: \"/tmp/s\"\n  drain: true\n")
            .expect_err("an unknown field must be rejected");
    assert!(
        error.to_string().contains("drain"),
        "the error should name the offending field, got: {error}"
    );
}

/// `emit_done` used to be a boolean. It is three-valued now, and the old spelling must
/// fail loudly rather than be coerced into one of the three.
#[test]
fn rejects_the_boolean_spelling_of_emit_done() {
    use crate::models::Endpoint;

    let error =
        serde_yaml_ng::from_str::<Endpoint>("dir_spool:\n  path: \"/tmp/s\"\n  emit_done: true\n")
            .expect_err("a boolean must be rejected");
    // serde reports it as a type error rather than by field name, but a real config parse
    // carries the route and the line: "spool.output: invalid type: boolean `true`, expected
    // string or map at line 6 column 5".
    assert!(
        error.to_string().contains("invalid type: boolean"),
        "unexpected error: {error}"
    );
}

#[test]
fn leading_sequence_reads_the_padded_prefix() {
    assert_eq!(leading_sequence("000012_1700000000000"), Some(12));
    assert_eq!(leading_sequence("000012"), Some(12));
    assert_eq!(leading_sequence("chunk-1"), None);
}

#[tokio::test]
async fn roundtrips_payload_and_metadata_through_the_spool() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![
            message("frame-one", "video"),
            message("frame-two", "video"),
        ])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;

    assert_eq!(received.len(), 2);
    assert_eq!(&received[0].payload[..], b"frame-one");
    assert_eq!(&received[1].payload[..], b"frame-two");
    assert_eq!(received[0].metadata.get("kind").unwrap(), "video");
    // `drain_on_read` defaults to true, so an acked chunk leaves no chunk behind. The two
    // live endpoints' claim files are all that is left, and neither is a chunk.
    assert_eq!(entries(dir.path()), vec!["CONSUMER", "PRODUCER"]);
}

#[tokio::test]
async fn preserves_binary_payloads_byte_for_byte() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    // Invalid UTF-8 plus embedded NULs and newlines: the delimiter framing the `file`
    // endpoint uses would corrupt this, which is why the spool writes whole files.
    let blob: Vec<u8> = vec![0x00, 0xff, b'\n', 0xfe, b'\r', 0x80, 0x00];

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![CanonicalMessage::from_vec(blob.clone())])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload[..], &blob[..]);
}

#[tokio::test]
async fn delivers_chunks_in_sequence_order_across_batches() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    for index in 0..12 {
        publisher
            .send_batch(vec![message(&format!("chunk-{index}"), "seq")])
            .await
            .unwrap();
    }

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    // A batch size below the queue depth forces several polls, which is where a naive
    // lexical sort over unpadded names would reorder 10 ahead of 2.
    let received = drain(&mut consumer, 5).await;
    let payloads: Vec<String> = received
        .iter()
        .map(|m| String::from_utf8(m.payload.to_vec()).unwrap())
        .collect();
    let expected: Vec<String> = (0..12).map(|index| format!("chunk-{index}")).collect();
    assert_eq!(payloads, expected);
}

/// The headline scenario — a finished producer and a consumer draining a backlog in small
/// batches — must not re-list the directory once per batch.
#[tokio::test]
async fn drains_a_backlog_without_rescanning_the_directory_per_batch() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    let backlog: Vec<CanonicalMessage> = (0..200)
        .map(|index| message(&format!("chunk-{index}"), "seq"))
        .collect();
    publisher.send_batch(backlog).await.unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 5).await;
    assert_eq!(received.len(), 200);
    // One scan fills the cache with all 200 names, the second finds the queue empty and
    // ends the drain. The unbatched version of this cost 41 scans of 200 names.
    assert_eq!(
        consumer.scans, 2,
        "40 batches out of one listing should not have rescanned the directory"
    );
}

/// A nack has to be redelivered promptly even when a deep backlog is cached behind it,
/// which is what the requeue path in front of the cached listing is for.
#[tokio::test]
async fn redelivers_a_nacked_chunk_ahead_of_a_cached_backlog() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    let backlog: Vec<CanonicalMessage> = (0..50)
        .map(|index| message(&format!("chunk-{index}"), "seq"))
        .collect();
    publisher.send_batch(backlog).await.unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let batch = consumer.receive_batch(1).await.unwrap();
    assert_eq!(&batch.messages[0].payload[..], b"chunk-0");
    (batch.commit)(vec![MessageDisposition::Nack])
        .await
        .unwrap();

    // Back at the head of the queue, not behind the 49 chunks still in the cache.
    let again = consumer.receive_batch(1).await.unwrap();
    assert_eq!(&again.messages[0].payload[..], b"chunk-0");
    (again.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    assert_eq!(consumer.scans, 1, "a redelivery must not force a rescan");

    // And the rest of the queue still comes out in order behind it.
    let rest = drain(&mut consumer, 7).await;
    let payloads: Vec<String> = rest
        .iter()
        .map(|m| String::from_utf8(m.payload.to_vec()).unwrap())
        .collect();
    let expected: Vec<String> = (1..50).map(|index| format!("chunk-{index}")).collect();
    assert_eq!(payloads, expected);
}

/// A nack can land after a refill scan has already picked the chunk back up, putting the
/// same name on both sides of the merge. One entry has to survive, not two: the second
/// would deliver the chunk twice.
#[tokio::test]
async fn a_requeued_chunk_the_scan_already_found_is_merged_once() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("chunk-0", "seq")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let batch = consumer.receive_batch(1).await.unwrap();
    (batch.commit)(vec![MessageDisposition::Nack])
        .await
        .unwrap();

    // Stands in for a scan that overlapped the nack: the claim is already released, so
    // the listing collects a name the requeue is still holding.
    consumer.refill_ready(8).await.unwrap();
    assert_eq!(consumer.ready.len(), 1);

    let again = consumer.receive_batch(4).await.unwrap();
    assert_eq!(again.messages.len(), 1);
    assert_eq!(&again.messages[0].payload[..], b"chunk-0");
    (again.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    assert!(consumer.ready.is_empty(), "the merge kept a duplicate");
}

/// A cached listing goes stale when something else empties the directory — an operator, a
/// foreign tool, or a second drainer under `claim: warn`/`off`. The batch that finds nothing
/// must rescan rather than report the queue empty.
#[tokio::test]
async fn rescans_when_the_cached_listing_has_been_drained_by_someone_else() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("taken", "x"), message("mine", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    // Fill the cache with both names, hand out neither.
    consumer.list_ready(0).await.unwrap();
    assert_eq!(consumer.ready.len(), 2);
    // Something else takes the chunk the cache is about to hand out, and a third
    // chunk arrives that the stale cache has never seen.
    std::fs::remove_file(dir.path().join("000000000.bin")).unwrap();
    std::fs::remove_file(dir.path().join("000000000.json")).unwrap();
    std::fs::remove_file(dir.path().join("000000001.bin")).unwrap();
    std::fs::remove_file(dir.path().join("000000001.json")).unwrap();
    publisher
        .send_batch(vec![message("arrived-later", "x")])
        .await
        .unwrap();

    let batch = consumer.receive_batch(2).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(&batch.messages[0].payload[..], b"arrived-later");
}

#[tokio::test]
async fn resumes_the_sequence_after_a_publisher_restart() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let first = DirSpoolPublisher::new(&cfg).await.unwrap();
    first.send_batch(vec![message("a", "x")]).await.unwrap();
    first.send_batch(vec![message("b", "x")]).await.unwrap();
    drop(first);

    // A fresh publisher over the same directory must append, not overwrite chunk 0.
    let second = DirSpoolPublisher::new(&cfg).await.unwrap();
    second.send_batch(vec![message("c", "x")]).await.unwrap();
    drop(second);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    let payloads: Vec<String> = received
        .iter()
        .map(|m| String::from_utf8(m.payload.to_vec()).unwrap())
        .collect();
    assert_eq!(payloads, vec!["a", "b", "c"]);
}

#[tokio::test]
async fn never_hands_out_a_partially_written_chunk() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    // Stand in for a writer that died mid-chunk: the staging file is present but was
    // never renamed into place.
    std::fs::write(dir.path().join("000000.bin.tmp"), b"half a frame").unwrap();
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("whole", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload[..], b"whole");
    // The debris is still there — it is the operator's to inspect, not ours to delete.
    assert!(dir.path().join("000000.bin.tmp").exists());
}

#[tokio::test]
async fn ends_the_stream_once_the_queue_is_empty_and_done_is_present() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;
    cfg.stop_on_done = true;
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("last", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    // An empty queue is not the end while production is still open: another producer may
    // yet be coming, and only the sentinel says otherwise.
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
    let waiting = consumer.receive_batch(10).await;
    assert!(
        matches!(&waiting, Ok(batch) if batch.messages.is_empty()),
        "an empty spool without a sentinel must keep the stream open, got {waiting:?}"
    );

    // The route calls this hook when the publisher goes away: sentinel down, lock released.
    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(dir.path().join("DONE").exists());
    assert!(!dir.path().join("PRODUCER").exists());
    match consumer.receive_batch(10).await {
        Err(ConsumerError::EndOfStream) => {}
        other => panic!("expected EndOfStream once production finished, got {other:?}"),
    }
}

/// `success` is the strict reading: only a route that reached the end of its input and
/// wrote everything it accepted has finished producing.
#[tokio::test]
async fn emit_done_success_writes_the_sentinel_only_on_a_completed_pass() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;

    for outcome in [DisconnectOutcome::Stopped, DisconnectOutcome::Failed] {
        let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
        publisher.send_batch(vec![message("x", "x")]).await.unwrap();
        close_producer(&publisher, outcome).await;
        assert!(
            !dir.path().join("DONE").exists(),
            "{outcome:?} is not a finished production, so no sentinel"
        );
        drop(publisher);
    }

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(dir.path().join("DONE").exists());
}

/// `end` is the loose reading: nothing more is coming from here, whatever the reason.
#[tokio::test]
async fn emit_done_end_writes_the_sentinel_however_the_pass_ended() {
    for outcome in [
        DisconnectOutcome::Completed,
        DisconnectOutcome::Stopped,
        DisconnectOutcome::Failed,
    ] {
        let dir = tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.emit_done = SpoolDone::End;

        let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
        close_producer(&publisher, outcome).await;
        assert!(
            dir.path().join("DONE").exists(),
            "'end' must write the sentinel after {outcome:?}"
        );
    }
}

#[tokio::test]
async fn emit_done_never_writes_no_sentinel() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    assert_eq!(cfg.emit_done, SpoolDone::Never);

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(!dir.path().join("DONE").exists());
}

/// A chunk this producer accepted but could not write means production did not succeed,
/// even when the route itself ran to the end of its input — the route may have sent the
/// message to a DLQ, but the spool is missing it either way.
#[tokio::test]
async fn emit_done_success_holds_back_when_a_chunk_could_not_be_written() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;
    // A directory where the first chunk's payload has to be staged: the write cannot
    // succeed. It has to be the `.tmp` name rather than the final one, or the publisher
    // would read the blockage as an existing chunk and number itself past it.
    std::fs::create_dir(dir.path().join("000000000.bin.tmp")).unwrap();

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    let sent = publisher
        .send_batch(vec![message("doomed", "x")])
        .await
        .unwrap();
    assert!(
        matches!(sent, SentBatch::Partial { .. }),
        "the write should have failed, got {sent:?}"
    );

    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(
        !dir.path().join("DONE").exists(),
        "a producer that dropped a chunk must not declare production successful"
    );
}

/// SPOOL-04: a write that `retry` repeats successfully no longer holds the sentinel back.
#[tokio::test]
async fn emit_done_success_recovers_once_the_failed_chunk_is_written() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;
    std::fs::create_dir(dir.path().join("000000000.bin.tmp")).unwrap();

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    let sent = publisher
        .send_batch(vec![message("retried", "x")])
        .await
        .unwrap();
    let SentBatch::Partial { failed, .. } = sent else {
        panic!("the write should have failed");
    };
    let (retried, _) = failed.into_iter().next().unwrap();

    // Another message under the same id is not the retry and must not clear the failure.
    let mut impostor = message("other", "x");
    impostor.message_id = retried.message_id;
    publisher.send_batch(vec![impostor]).await.unwrap();
    assert_eq!(publisher.unwritten_len.load(Ordering::Relaxed), 1);

    let sent = publisher.send_batch(vec![retried]).await.unwrap();
    assert!(matches!(sent, SentBatch::Ack), "the retry should succeed");

    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(dir.path().join("DONE").exists());
}

/// Past the tracking limit the producer keeps the sentinel back for good.
#[tokio::test]
async fn emit_done_success_stays_quiet_past_the_tracking_limit() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();

    for index in 0..=MAX_UNWRITTEN_TRACKED {
        publisher.note_unwritten(&message(&index.to_string(), "x"));
    }
    assert_eq!(
        publisher.unwritten_len.load(Ordering::Relaxed),
        MAX_UNWRITTEN_TRACKED
    );
    assert!(publisher.write_failed.load(Ordering::Relaxed));
}

/// An unexplained close — a caller that uses the plain disconnect hook — is read as a stop,
/// because it is not evidence that production finished.
#[tokio::test]
async fn a_close_without_an_outcome_is_not_a_success() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher.on_disconnect_hook().unwrap().await.unwrap();
    assert!(!dir.path().join("DONE").exists());
    // The lock is still released, so the next producer is not blocked.
    assert!(!dir.path().join("PRODUCER").exists());
}

/// The point of keeping a sentinel rather than reading the producer lock: production can
/// span several producers, which run one at a time, and only the last one declares the end.
#[tokio::test]
async fn production_can_span_several_producers_and_only_the_last_declares_done() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.stop_on_done = true;
    cfg.poll_interval_ms = 1;

    let first = DirSpoolPublisher::new(&cfg).await.unwrap();
    first.send_batch(vec![message("a", "x")]).await.unwrap();
    // Closes without the sentinel: more production is coming.
    close_producer(&first, DisconnectOutcome::Completed).await;
    drop(first);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
    let waiting = consumer.receive_batch(10).await;
    assert!(
        matches!(&waiting, Ok(batch) if batch.messages.is_empty()),
        "the gap between two producers must not end the stream, got {waiting:?}"
    );

    // The second producer takes the freed lock, and this one is the last.
    let mut last_cfg = config(dir.path());
    last_cfg.emit_done = SpoolDone::Success;
    let last = DirSpoolPublisher::new(&last_cfg).await.unwrap();
    last.send_batch(vec![message("b", "x")]).await.unwrap();
    close_producer(&last, DisconnectOutcome::Completed).await;
    drop(last);

    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload[..], b"b");
    match consumer.receive_batch(10).await {
        Err(ConsumerError::EndOfStream) => {}
        other => panic!("expected EndOfStream once the last producer declared done, got {other:?}"),
    }
}

/// The backlog comes first: a producer that finished long ago does not cut the queue short.
#[tokio::test]
async fn drains_the_backlog_before_ending_the_stream() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.stop_on_done = true;
    cfg.emit_done = SpoolDone::Success;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("one", "x"), message("two", "x")])
        .await
        .unwrap();
    close_producer(&publisher, DisconnectOutcome::Completed).await;
    drop(publisher);
    assert!(dir.path().join("DONE").exists());

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 1).await;
    assert_eq!(received.len(), 2);
    match consumer.receive_batch(10).await {
        Err(ConsumerError::EndOfStream) => {}
        other => panic!("expected EndOfStream once the queue drained, got {other:?}"),
    }
}

#[tokio::test]
async fn keeps_tailing_when_stop_on_done_is_off() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;
    std::fs::write(dir.path().join("DONE"), b"").unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    // An empty batch, not an end of stream: the default is a directory tail.
    let batch = consumer.receive_batch(10).await.unwrap();
    assert!(batch.messages.is_empty());
}

#[tokio::test]
async fn a_nacked_chunk_stays_on_disk_and_is_redelivered() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("retry-me", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    (batch.commit)(vec![MessageDisposition::Nack])
        .await
        .unwrap();
    assert!(dir.path().join("000000000.bin").exists());

    let again = consumer.receive_batch(10).await.unwrap();
    assert_eq!(again.messages.len(), 1);
    assert_eq!(&again.messages[0].payload[..], b"retry-me");
}

#[tokio::test]
async fn without_drain_on_read_each_chunk_is_delivered_once_and_kept() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.drain_on_read = false;
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("archive", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    // Read but not consumed: the files survive for a second reader or an operator.
    assert!(dir.path().join("000000000.bin").exists());
    assert!(dir.path().join("000000000.json").exists());
}

#[tokio::test]
async fn reads_a_bare_payload_file_written_without_a_sidecar() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    // What a foreign producer (a Python script, say) would leave behind.
    std::fs::write(dir.path().join("000001.bin"), b"foreign").unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload[..], b"foreign");
    assert!(received[0].metadata.is_empty());
}

/// A perusal reader sees *every* chunk once, not every chunk above a watermark. Names
/// only arrive in order when one producer writes them; a second producer with the claim
/// off, or a foreign one, can drop a lexically lower name after the reader has moved past
/// it. Skipping it would lose a message, so what has been read is remembered by name.
#[tokio::test]
async fn a_non_draining_reader_still_delivers_a_late_arrival_below_what_it_has_read() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.drain_on_read = false;

    std::fs::write(dir.path().join("000005.bin"), b"five").unwrap();
    std::fs::write(dir.path().join("000006.bin"), b"six").unwrap();
    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let first = drain(&mut consumer, 10).await;
    assert_eq!(first.len(), 2);

    // Behind the reader's position, and still owed to it.
    std::fs::write(dir.path().join("000001.bin"), b"one").unwrap();
    let late = drain(&mut consumer, 10).await;
    assert_eq!(late.len(), 1, "the late chunk must not be skipped");
    assert_eq!(&late[0].payload[..], b"one");

    // And nothing already read comes back a second time.
    assert!(drain(&mut consumer, 10).await.is_empty());
}

#[tokio::test]
async fn stamps_source_metadata_when_asked() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.source_metadata = true;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("traced", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(
        received[0].metadata.get(SRC_CHUNK_KEY).unwrap(),
        "000000000"
    );
    assert_eq!(
        received[0].metadata.get(SRC_PATH_KEY).unwrap(),
        dir.path().to_str().unwrap()
    );
}

#[tokio::test]
async fn reserved_source_keys_in_a_sidecar_are_dropped() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let mut forged = message("payload", "x");
    forged
        .metadata
        .insert("mqb.src.offset".to_string(), "42".to_string());
    forged
        .metadata
        .insert(SRC_CHUNK_KEY.to_string(), "forged".to_string());
    forged
        .metadata
        .insert("kept".to_string(), "yes".to_string());
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher.send_batch(vec![forged]).await.unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received[0].metadata.get("kept").unwrap(), "yes");
    assert!(!received[0].metadata.contains_key("mqb.src.offset"));
    assert!(!received[0].metadata.contains_key(SRC_CHUNK_KEY));
}

#[tokio::test]
async fn honours_custom_extensions_and_a_disabled_sidecar() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.payload_extension = ".h264".to_string();
    cfg.metadata_extension = String::new();
    cfg.naming_pattern = "{seq:06d}".to_string();

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("nal-unit", "video")])
        .await
        .unwrap();
    assert!(dir.path().join("000000.h264").exists());
    assert!(!dir.path().join("000000.json").exists());

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload[..], b"nal-unit");
    // No sidecar means no metadata to restore.
    assert!(received[0].metadata.is_empty());
}

/// A consumer whose two extensions match would read every sidecar as a payload and deliver
/// the metadata as a message body, so it refuses the same configuration the publisher does.
#[tokio::test]
async fn both_ends_reject_a_payload_and_sidecar_sharing_one_extension() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.payload_extension = "json".to_string();
    cfg.metadata_extension = ".json".to_string();

    for error in [
        DirSpoolPublisher::new(&cfg).await.unwrap_err().to_string(),
        DirSpoolConsumer::new(&cfg).await.unwrap_err().to_string(),
    ] {
        assert!(error.contains("must differ"), "unexpected error: {error}");
    }
}

/// The front of a chunk name is the queue's order *and* its resume point, so a sink whose
/// pattern starts with anything else is refused rather than left to corrupt a second run.
#[tokio::test]
async fn rejects_a_naming_pattern_that_does_not_start_with_the_sequence() {
    let dir = tempdir().unwrap();

    for pattern in ["{timestamp}_{seq:09}", "chunk-{seq:09}", "{message_id}"] {
        let mut cfg = config(dir.path());
        cfg.naming_pattern = pattern.to_string();
        let error = DirSpoolPublisher::new(&cfg)
            .await
            .expect_err("a pattern that does not lead with the sequence must be refused")
            .to_string();
        assert!(
            error.contains("must start with the sequence"),
            "unexpected error for {pattern:?}: {error}"
        );
        // A consumer does not render names, so the same spool is still readable.
        assert!(DirSpoolConsumer::new(&cfg).await.is_ok());
    }
}

/// `{timestamp}_{seq}` was the specific trap: `leading_sequence` reads the timestamp as the
/// sequence, and a second run would resume from 1.7 trillion.
#[test]
fn leading_sequence_would_read_a_timestamp_prefix_as_the_sequence() {
    assert_eq!(
        leading_sequence("1700000000000_000000001"),
        Some(1_700_000_000_000)
    );
    // Which is why the pattern is rejected rather than documented.
    let mut cfg = DirSpoolConfig::new("/tmp/s");
    cfg.naming_pattern = "{timestamp}_{seq:09}".to_string();
    assert!(validate_naming_pattern(&cfg).is_err());
}

/// An unpadded sequence still works for fewer than ten chunks, so it warns rather than
/// failing — but it does warn, because chunk 10 sorts before chunk 2.
#[test]
fn warns_about_an_unpadded_sequence() {
    let mut cfg = DirSpoolConfig::new("/tmp/s");
    cfg.naming_pattern = "{seq}".to_string();
    assert!(validate_naming_pattern(&cfg).is_ok());
    let warning = naming_pattern_warning(&cfg).expect("an unpadded sequence must warn");
    assert!(
        warning.contains("chunk 10 sorts before chunk 2"),
        "{warning}"
    );

    cfg.naming_pattern = "{seq:09}".to_string();
    assert!(naming_pattern_warning(&cfg).is_none());
    cfg.naming_pattern = "{seq:06d}_{timestamp}".to_string();
    assert!(naming_pattern_warning(&cfg).is_none());
}

/// `fsync: off` changes what a crash costs, not what a spool contains.
#[tokio::test]
async fn round_trips_with_fsync_off() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.fsync = SpoolFsync::Off;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("unsynced", "x"), message("also", "x")])
        .await
        .unwrap();

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(received.len(), 2);
    assert_eq!(&received[0].payload[..], b"unsynced");
    assert_eq!(received[0].metadata.get("kind").unwrap(), "x");
    // Acked chunks are still deleted; only the fsyncs are gone.
    assert_eq!(entries(dir.path()), vec!["CONSUMER", "PRODUCER"]);
}

// --- Sharding ---

#[test]
fn shard_paths_split_the_leading_sequence_digits() {
    let sharding = Sharding { depth: 2, width: 3 };
    assert_eq!(sharding.path_for("000000001"), "000/000/001");
    assert_eq!(sharding.path_for("000001234"), "000/001/234");
    assert_eq!(sharding.path_for("001000000"), "001/000/000");
    // A suffix after the sequence rides along on the file name.
    assert_eq!(
        sharding.path_for("000000001_1700000000000"),
        "000/000/001_1700000000000"
    );
    // Too short to shard: returned whole, so no caller can build a path out of the spool.
    assert_eq!(sharding.path_for("00001"), "00001");
}

/// The point of the feature: 30fps with sidecars is over 200,000 files an hour, and one
/// directory does not hold that well. Sharding spreads them without changing queue order.
#[tokio::test]
async fn shards_chunks_into_subdirectories_and_reads_them_back_in_order() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.shard_depth = 2;
    cfg.shard_width = 3;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("first", "x"), message("second", "x")])
        .await
        .unwrap();

    assert_eq!(
        tree_entries(dir.path())
            .into_iter()
            .filter(|name| name.ends_with(".bin"))
            .collect::<Vec<_>>(),
        vec!["000/000/000.bin", "000/000/001.bin"],
        "chunk 1 must land at 000/000/001, and its sidecar beside it"
    );
    assert!(dir.path().join("000/000/001.json").exists());

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 10).await;
    let payloads: Vec<String> = received
        .iter()
        .map(|m| String::from_utf8(m.payload.to_vec()).unwrap())
        .collect();
    assert_eq!(payloads, vec!["first", "second"]);
}

/// Queue order has to survive a shard boundary, which is where a per-directory listing
/// would silently start delivering out of order.
#[tokio::test]
async fn keeps_queue_order_across_shard_boundaries() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    // Three digits over two single-character levels, so the boundary falls at chunk 10.
    cfg.naming_pattern = "{seq:03}".to_string();
    cfg.shard_depth = 2;
    cfg.shard_width = 1;
    cfg.source_metadata = true;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    for index in 0..23 {
        publisher
            .send_batch(vec![message(&format!("chunk-{index}"), "seq")])
            .await
            .unwrap();
    }
    // Spread over three leaf shards, none of which holds more than ten chunks.
    assert!(dir.path().join("0/0/9.bin").exists());
    assert!(dir.path().join("0/1/0.bin").exists());
    assert!(dir.path().join("0/2/2.bin").exists());

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    // A batch size below the shard size forces the walk to resume mid-tree.
    let received = drain(&mut consumer, 4).await;
    let payloads: Vec<String> = received
        .iter()
        .map(|m| String::from_utf8(m.payload.to_vec()).unwrap())
        .collect();
    let expected: Vec<String> = (0..23).map(|index| format!("chunk-{index}")).collect();
    assert_eq!(payloads, expected);
    assert_eq!(
        received[10].metadata.get(SRC_CHUNK_KEY).unwrap(),
        "0/1/0",
        "the chunk's recorded identity is its path relative to the spool"
    );
    // Draining prunes the shards it empties, so the spool does not accumulate directories.
    assert!(
        tree_entries(dir.path())
            .iter()
            .all(|name| !name.ends_with(".bin")),
        "holds {:?}",
        tree_entries(dir.path())
    );
    assert!(!dir.path().join("0/0").exists(), "an emptied shard must go");
    assert!(!dir.path().join("0").exists());
}

/// A restarted producer has to find the highest chunk in the tree, not just the root.
#[tokio::test]
async fn resumes_the_sequence_across_shards_after_a_restart() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.naming_pattern = "{seq:03}".to_string();
    cfg.shard_depth = 2;
    cfg.shard_width = 1;

    let first = DirSpoolPublisher::new(&cfg).await.unwrap();
    for index in 0..12 {
        first
            .send_batch(vec![message(&format!("a-{index}"), "x")])
            .await
            .unwrap();
    }
    drop(first);

    let second = DirSpoolPublisher::new(&cfg).await.unwrap();
    second.send_batch(vec![message("b", "x")]).await.unwrap();
    // Chunk 12 follows chunk 11 rather than overwriting the head of the queue.
    assert!(dir.path().join("0/1/2.bin").exists());
    drop(second);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let received = drain(&mut consumer, 100).await;
    assert_eq!(received.len(), 13);
    assert_eq!(&received[12].payload[..], b"b");
}

/// A producer writing into a shard the consumer prunes at the same moment must not lose the
/// message: the directory is simply re-created.
#[tokio::test]
async fn a_pruned_shard_is_recreated_by_the_next_write() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.naming_pattern = "{seq:03}".to_string();
    cfg.shard_depth = 2;
    cfg.shard_width = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("one", "x")])
        .await
        .unwrap();
    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
    assert!(!dir.path().join("0/0").exists(), "the shard was pruned");

    // Same publisher, same shard, now missing.
    publisher
        .send_batch(vec![message("two", "x")])
        .await
        .unwrap();
    assert!(dir.path().join("0/0/1.bin").exists());
    let received = drain(&mut consumer, 10).await;
    assert_eq!(&received[0].payload[..], b"two");
}

/// A consumer that does not know the spool is sharded would otherwise read it as
/// permanently empty, so it says so.
#[tokio::test]
async fn a_consumer_without_the_shard_depth_finds_nothing_and_warns() {
    let dir = tempdir().unwrap();
    let mut producer_cfg = config(dir.path());
    producer_cfg.shard_depth = 2;
    let publisher = DirSpoolPublisher::new(&producer_cfg).await.unwrap();
    publisher
        .send_batch(vec![message("hidden", "x")])
        .await
        .unwrap();

    // The default depth is 0: the chunk is real but out of reach, which is a
    // misconfiguration the log names (see `refill_ready`).
    let mut consumer = DirSpoolConsumer::new(&config(dir.path())).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert!(batch.messages.is_empty());
}

/// The route path with sharding on: a chunk's identity now contains a separator, and it
/// travels through the batch commit and the source metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_drains_a_sharded_spool() {
    use crate::models::{Endpoint, EndpointType, FileConfig, FileFormat};
    use crate::route::Route;

    let dir = tempdir().unwrap();
    let spool = dir.path().join("spool");
    let sink = dir.path().join("out.jsonl");

    let mut producer_cfg = DirSpoolConfig::new(spool.to_str().unwrap());
    producer_cfg.naming_pattern = "{seq:03}".to_string();
    producer_cfg.shard_depth = 2;
    producer_cfg.shard_width = 1;
    producer_cfg.emit_done = SpoolDone::Success;
    let producer = DirSpoolPublisher::new(&producer_cfg).await.unwrap();
    for index in 0..15 {
        producer
            .send_batch(vec![message(&format!("{index}"), "frame")])
            .await
            .unwrap();
    }
    close_producer(&producer, DisconnectOutcome::Completed).await;
    drop(producer);

    let mut consumer_cfg = DirSpoolConfig::new(spool.to_str().unwrap());
    consumer_cfg.naming_pattern = "{seq:03}".to_string();
    consumer_cfg.shard_depth = 2;
    consumer_cfg.shard_width = 1;
    consumer_cfg.stop_on_done = true;
    let route = Route::new(
        Endpoint::new(EndpointType::DirSpool(consumer_cfg)),
        Endpoint::new(EndpointType::File(FileConfig {
            path: sink.to_str().unwrap().to_string(),
            format: FileFormat::Raw,
            ..Default::default()
        })),
    )
    .with_concurrency(4)
    .with_batch_size(2);

    tokio::time::timeout(
        Duration::from_secs(10),
        route.run_until_err("sharded_spool_drain", None, None),
    )
    .await
    .expect("the route should end once DONE is reached and the queue is empty")
    .expect("the route should complete without errors");

    let written = tokio::fs::read_to_string(&sink).await.unwrap();
    let lines: Vec<&str> = written.lines().collect();
    let expected: Vec<String> = (0..15).map(|index| index.to_string()).collect();
    assert_eq!(lines, expected, "a sharded spool must drain in queue order");
    // Chunks gone, shards pruned, only the sentinel left.
    assert_eq!(tree_entries(&spool), vec!["DONE".to_string()]);
}

/// Sharding cuts the directory names out of the front of every rendered name, so a pattern
/// that does not start with a wide enough fixed-width sequence cannot be sharded.
#[tokio::test]
async fn rejects_sharding_a_pattern_it_cannot_split() {
    let dir = tempdir().unwrap();

    let cases: Vec<(DirSpoolConfig, &str)> = vec![
        {
            // Variable width: chunk 999 and chunk 1000 would shard to different depths.
            let mut cfg = config(dir.path());
            cfg.naming_pattern = "{seq}".to_string();
            cfg.shard_depth = 1;
            (cfg, "must start with a zero-padded sequence")
        },
        {
            let mut cfg = config(dir.path());
            cfg.naming_pattern = "{timestamp}_{seq:09}".to_string();
            cfg.shard_depth = 1;
            (cfg, "must start with a zero-padded sequence")
        },
        {
            // Nine digits, but three levels of three take all of them.
            let mut cfg = config(dir.path());
            cfg.shard_depth = 3;
            cfg.shard_width = 3;
            (cfg, "at least one has to be left for the file name")
        },
        {
            let mut cfg = config(dir.path());
            cfg.shard_depth = 2;
            cfg.shard_width = 0;
            (cfg, "'shard_width' must be at least 1")
        },
    ];

    for (cfg, expected) in cases {
        for error in [
            DirSpoolPublisher::new(&cfg).await.unwrap_err().to_string(),
            DirSpoolConsumer::new(&cfg).await.unwrap_err().to_string(),
        ] {
            assert!(
                error.contains(expected),
                "expected an error mentioning {expected:?}, got: {error}"
            );
        }
    }
    assert!(tree_entries(dir.path()).is_empty());
}

// --- Claims ---

#[tokio::test]
async fn a_second_producer_is_refused_while_the_first_holds_the_claim() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let first = DirSpoolPublisher::new(&cfg).await.unwrap();
    assert!(dir.path().join("PRODUCER").exists());

    // Two producers seeded from the same sequence number would overwrite each other's
    // chunks, so this has to fail loudly rather than corrupt the queue.
    let error = DirSpoolPublisher::new(&cfg)
        .await
        .expect_err("a second producer must be refused")
        .to_string();
    assert!(
        error.contains("already held by a producer") && error.contains("PRODUCER"),
        "the error must name the holder and the file to delete, got: {error}"
    );
    // The refusal must not have disturbed the live claim.
    drop(first);
    assert!(!dir.path().join("PRODUCER").exists());
    DirSpoolPublisher::new(&cfg)
        .await
        .expect("the claim is released when the holder is dropped");
}

#[tokio::test]
async fn a_shared_spool_is_still_possible_with_claim_off_or_warn() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.claim = SpoolClaim::Off;

    let _first = DirSpoolPublisher::new(&cfg).await.unwrap();
    // Nothing claimed, nothing checked.
    assert!(!dir.path().join("PRODUCER").exists());
    let _second = DirSpoolPublisher::new(&cfg)
        .await
        .expect("'off' takes no claim and enforces none");

    let mut warned = config(dir.path());
    warned.claim = SpoolClaim::Warn;
    let _third = DirSpoolPublisher::new(&warned)
        .await
        .expect("'warn' takes the free claim");
    let _fourth = DirSpoolPublisher::new(&warned)
        .await
        .expect("'warn' logs the conflict and runs anyway");
}

/// A crash must not wedge the next start: the lock it left behind names a process that is
/// no longer running, which `pidlock` checks before refusing.
#[cfg(unix)]
#[tokio::test]
async fn a_lock_left_by_a_dead_process_is_taken_over() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    // A pid that is definitely gone, because we waited for it. `/bin/sh` rather than
    // `/bin/true`, which macOS does not ship.
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    std::fs::write(dir.path().join("PRODUCER"), dead_pid.to_string()).unwrap();

    let publisher = DirSpoolPublisher::new(&cfg)
        .await
        .expect("a lock whose owner is gone must be taken over");
    // Retaken, not merely ignored: the file now names this process.
    let held = std::fs::read_to_string(dir.path().join("PRODUCER")).unwrap();
    assert_eq!(held.trim(), std::process::id().to_string());
    drop(publisher);
}

/// The other direction: a lock naming a process that *is* running is respected, whoever
/// wrote it.
#[tokio::test]
async fn a_lock_naming_a_live_process_is_respected() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    // Our own pid stands in for "some live process wrote this".
    std::fs::write(dir.path().join("PRODUCER"), std::process::id().to_string()).unwrap();

    let error = DirSpoolPublisher::new(&cfg)
        .await
        .expect_err("a live lock must be respected, not broken")
        .to_string();
    assert!(
        error.contains(&format!("pid {}", std::process::id())),
        "the error must name the holder, got: {error}"
    );
}

/// A lock file that holds no readable pid names nobody, so there is no one to defer to and
/// nothing to alarm about: `pidlock` clears it and the endpoint starts.
#[tokio::test]
async fn a_corrupt_lock_file_is_cleared() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    std::fs::write(dir.path().join("PRODUCER"), b"not a pid").unwrap();

    let publisher = DirSpoolPublisher::new(&cfg)
        .await
        .expect("a lock file with no readable pid must be cleared");
    let held = std::fs::read_to_string(dir.path().join("PRODUCER")).unwrap();
    assert_eq!(held.trim(), std::process::id().to_string());
    drop(publisher);
}

#[tokio::test]
async fn a_draining_consumer_excludes_another_but_readers_share_the_spool() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());

    let drainer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert!(dir.path().join("CONSUMER").exists());
    let error = DirSpoolConsumer::new(&cfg)
        .await
        .expect_err("a second draining consumer must be refused")
        .to_string();
    assert!(
        error.contains("already held by a consumer"),
        "unexpected error: {error}"
    );

    // A non-draining reader deletes nothing, so it may read alongside the drainer — it
    // just gets warned that chunks will vanish from under it.
    let mut archive = config(dir.path());
    archive.drain_on_read = false;
    let _reader = DirSpoolConsumer::new(&archive)
        .await
        .expect("a non-draining reader must not be blocked by a drainer");
    drop(drainer);

    // And several non-draining readers share the spool with no claim between them.
    let _second_reader = DirSpoolConsumer::new(&archive)
        .await
        .expect("non-draining readers are a supported fan-out");
    assert!(!dir.path().join("CONSUMER").exists());
}

#[tokio::test]
async fn honours_custom_control_file_names() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.done_file = "FINISHED".to_string();
    cfg.producer_file = "writer.lock".to_string();
    cfg.consumer_file = "reader.lock".to_string();
    cfg.emit_done = SpoolDone::Success;
    cfg.stop_on_done = true;
    cfg.poll_interval_ms = 1;

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    let consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert_eq!(
        entries(dir.path()),
        vec!["reader.lock", "writer.lock"],
        "the locks must land on the configured names"
    );
    // And the configured names are what exclude a second instance.
    let error = DirSpoolPublisher::new(&cfg).await.unwrap_err().to_string();
    assert!(error.contains("writer.lock"), "unexpected error: {error}");
    drop(consumer);

    publisher
        .send_batch(vec![message("only", "x")])
        .await
        .unwrap();
    close_producer(&publisher, DisconnectOutcome::Completed).await;
    assert!(dir.path().join("FINISHED").exists());

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
    match consumer.receive_batch(10).await {
        Err(ConsumerError::EndOfStream) => {}
        other => panic!("the configured sentinel must end the stream, got {other:?}"),
    }
}

/// The control files share the directory with the chunks and with each other, so a name
/// that collides is rejected at startup rather than discovered in a directory listing.
#[tokio::test]
async fn rejects_control_file_names_that_would_collide() {
    let dir = tempdir().unwrap();

    let cases: Vec<(DirSpoolConfig, &str)> = vec![
        {
            // Ends in the payload extension: the consumer would deliver it as a message.
            let mut cfg = config(dir.path());
            cfg.done_file = "DONE.bin".to_string();
            (cfg, "payload extension")
        },
        {
            // Ends in the sidecar extension: it would be read as a chunk's metadata.
            let mut cfg = config(dir.path());
            cfg.producer_file = "producer.json".to_string();
            (cfg, "metadata extension")
        },
        {
            // Looks like a chunk that is still being written.
            let mut cfg = config(dir.path());
            cfg.consumer_file = "consumer.tmp".to_string();
            (cfg, ".tmp")
        },
        {
            // One file cannot be two locks: each role's release would delete the other's.
            let mut cfg = config(dir.path());
            cfg.consumer_file = cfg.producer_file.clone();
            (cfg, "three different files")
        },
        {
            // Case-insensitive filesystems make these one file too.
            let mut cfg = config(dir.path());
            cfg.done_file = "producer".to_string();
            (cfg, "three different files")
        },
        {
            // A path would escape the spool directory.
            let mut cfg = config(dir.path());
            cfg.done_file = "../DONE".to_string();
            (cfg, "not a path")
        },
        {
            // Names the spool directory itself rather than a file in it.
            let mut cfg = config(dir.path());
            cfg.done_file = ".".to_string();
            (cfg, "not a path")
        },
        {
            let mut cfg = config(dir.path());
            cfg.producer_file = "..".to_string();
            (cfg, "not a path")
        },
        {
            let mut cfg = config(dir.path());
            cfg.consumer_file = String::new();
            (cfg, "must not be empty")
        },
        {
            // One name cannot be both a shard directory and the sentinel.
            let mut cfg = config(dir.path());
            cfg.shard_depth = 2;
            cfg.shard_width = 3;
            cfg.done_file = "000".to_string();
            (cfg, "shape of a shard directory")
        },
    ];

    for (cfg, expected) in cases {
        // Both ends reject it, and so does route validation before either is built.
        let publisher_error = DirSpoolPublisher::new(&cfg)
            .await
            .expect_err("the publisher must reject a colliding control file")
            .to_string();
        let consumer_error = DirSpoolConsumer::new(&cfg)
            .await
            .expect_err("the consumer must reject a colliding control file")
            .to_string();
        for error in [&publisher_error, &consumer_error] {
            assert!(
                error.contains(expected),
                "expected an error mentioning {expected:?}, got: {error}"
            );
        }
        assert!(
            validate_control_files(&cfg).is_err(),
            "validation must reject {cfg:?}"
        );
    }
    // Nothing was created by any of the rejected configurations.
    assert!(
        entries(dir.path()).is_empty(),
        "holds {:?}",
        entries(dir.path())
    );
}

/// A producer opening the spool means production is live again, so the sentinel from an
/// earlier run has to go — otherwise a `stop_on_done` consumer would exit as soon as its
/// queue first ran dry, abandoning everything this producer is about to write.
#[tokio::test]
async fn a_restarted_producer_clears_the_sentinel_and_reopens_the_stream() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.stop_on_done = true;
    cfg.emit_done = SpoolDone::Success;
    cfg.poll_interval_ms = 1;

    // First run: produce, declare done, drain to the end of the stream.
    let first = DirSpoolPublisher::new(&cfg).await.unwrap();
    first
        .send_batch(vec![message("run-one", "x")])
        .await
        .unwrap();
    close_producer(&first, DisconnectOutcome::Completed).await;
    drop(first);
    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
    assert!(matches!(
        consumer.receive_batch(10).await,
        Err(ConsumerError::EndOfStream)
    ));
    drop(consumer);
    // The marker is sitting there, which is exactly what would end run two prematurely.
    assert!(dir.path().join("DONE").exists());

    // Second run: the new producer clears the sentinel as it opens, so a consumer that
    // outpaces it — an empty queue part-way through the run — waits instead of ending on
    // the last run's marker.
    let second = DirSpoolPublisher::new(&cfg).await.unwrap();
    assert!(!dir.path().join("DONE").exists());
    assert!(dir.path().join("PRODUCER").exists());
    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let waiting = consumer.receive_batch(10).await;
    assert!(
        matches!(&waiting, Ok(batch) if batch.messages.is_empty()),
        "a restarted producer must reopen the stream, got {waiting:?}"
    );
    second
        .send_batch(vec![message("run-two-first", "x")])
        .await
        .unwrap();
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
    // Caught up again, mid-run: still not the end.
    let waiting = consumer.receive_batch(10).await;
    assert!(
        matches!(&waiting, Ok(batch) if batch.messages.is_empty()),
        "an empty queue part-way through a run must not end the stream, got {waiting:?}"
    );
    second
        .send_batch(vec![message("run-two-last", "x")])
        .await
        .unwrap();
    let received = drain(&mut consumer, 10).await;
    assert_eq!(&received[0].payload[..], b"run-two-last");

    // And run two ends on run two's own marker.
    close_producer(&second, DisconnectOutcome::Completed).await;
    assert!(matches!(
        consumer.receive_batch(10).await,
        Err(ConsumerError::EndOfStream)
    ));
}

/// The whole point of the endpoint: a producer that runs to completion and exits, and a
/// consumer that starts afterwards and still gets every chunk, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_drains_a_spool_its_producer_has_already_finished_writing() {
    use crate::models::{Endpoint, EndpointType, FileConfig, FileFormat};
    use crate::route::Route;

    let dir = tempdir().unwrap();
    let spool = dir.path().join("spool");
    let sink = dir.path().join("out.jsonl");

    // Producer: fill the spool and mark it finished, then go away entirely.
    let mut producer_cfg = DirSpoolConfig::new(spool.to_str().unwrap());
    producer_cfg.emit_done = SpoolDone::Success;
    let producer = DirSpoolPublisher::new(&producer_cfg).await.unwrap();
    for index in 0..20 {
        producer
            .send_batch(vec![message(&format!("{index}"), "frame")])
            .await
            .unwrap();
    }
    close_producer(&producer, DisconnectOutcome::Completed).await;
    drop(producer);

    let mut consumer_cfg = DirSpoolConfig::new(spool.to_str().unwrap());
    consumer_cfg.stop_on_done = true;
    let route = Route::new(
        Endpoint::new(EndpointType::DirSpool(consumer_cfg)),
        Endpoint::new(EndpointType::File(FileConfig {
            path: sink.to_str().unwrap().to_string(),
            format: FileFormat::Raw,
            ..Default::default()
        })),
    )
    .with_concurrency(4)
    .with_batch_size(3);

    tokio::time::timeout(
        Duration::from_secs(10),
        route.run_until_err("spool_drain", None, None),
    )
    .await
    .expect("the route should end once DONE is reached and the queue is empty")
    .expect("the route should complete without errors");

    let written = tokio::fs::read_to_string(&sink).await.unwrap();
    let lines: Vec<&str> = written.lines().collect();
    let expected: Vec<String> = (0..20).map(|index| index.to_string()).collect();
    assert_eq!(lines, expected, "the spool must drain in queue order");
    // Every chunk was acked and both ends have closed, so all that is left is the
    // sentinel: the two locks were released by the producer and the route's consumer.
    assert_eq!(entries(&spool), vec!["DONE".to_string()]);
}

/// The producing half of the same story, driven by a real route rather than a hand-closed
/// publisher: the route decides the pass completed, and that verdict has to reach the sink
/// for `emit_done: success` to mean anything. Nothing between the route and the publisher
/// forwards it explicitly, so this is what proves the scoped outcome arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_writing_into_a_spool_marks_it_done_when_it_completes() {
    use crate::models::{Endpoint, EndpointType};
    use crate::route::Route;

    let dir = tempdir().unwrap();
    let source = dir.path().join("source");
    let sink = dir.path().join("sink");

    let mut source_cfg = DirSpoolConfig::new(source.to_str().unwrap());
    source_cfg.emit_done = SpoolDone::Success;
    let producer = DirSpoolPublisher::new(&source_cfg).await.unwrap();
    for index in 0..20 {
        producer
            .send_batch(vec![message(&format!("{index}"), "frame")])
            .await
            .unwrap();
    }
    close_producer(&producer, DisconnectOutcome::Completed).await;
    drop(producer);

    let mut consumer_cfg = DirSpoolConfig::new(source.to_str().unwrap());
    consumer_cfg.stop_on_done = true;
    let mut sink_cfg = DirSpoolConfig::new(sink.to_str().unwrap());
    sink_cfg.emit_done = SpoolDone::Success;
    let route = Route::new(
        Endpoint::new(EndpointType::DirSpool(consumer_cfg)),
        Endpoint::new(EndpointType::DirSpool(sink_cfg)),
    )
    .with_concurrency(4)
    .with_batch_size(3);

    // A loaded Windows CI runner has overrun 10s here once. The directory contents
    // distinguish a route still moving chunks from one that has stalled.
    tokio::time::timeout(
        Duration::from_secs(20),
        route.run_until_err("spool_to_spool", None, None),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the route should end once the source is drained; source holds {:?}, sink holds {:?}",
            tree_entries(&source),
            tree_entries(&sink)
        )
    })
    .expect("the route should complete without errors");

    assert!(
        sink.join("DONE").exists(),
        "a completed route must leave the sentinel; sink holds {:?}",
        entries(&sink)
    );

    // And a consumer downstream of it can now drain to a clean end.
    let mut downstream_cfg = DirSpoolConfig::new(sink.to_str().unwrap());
    downstream_cfg.stop_on_done = true;
    let mut downstream = DirSpoolConsumer::new(&downstream_cfg).await.unwrap();
    let drained = drain(&mut downstream, 20).await;
    assert_eq!(drained.len(), 20);
}

#[tokio::test]
async fn drains_immediately_under_exit_on_empty() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    // Long enough that a poll-interval sleep would dominate the test's runtime.
    cfg.poll_interval_ms = 5_000;

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    consumer.set_exit_on_empty(true);
    let started = std::time::Instant::now();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert!(batch.messages.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a drain must not wait out the idle poll interval"
    );
}

/// A publisher middleware knows nothing about spools: it forwards the disconnect hook and
/// nothing else. The outcome still has to reach the sink through it, or `emit_done: success`
/// silently degrades to never writing the sentinel behind any middleware at all.
#[tokio::test]
async fn the_outcome_reaches_the_sink_through_an_unaware_wrapper() {
    use crate::traits::{MessagePublisher, PublisherError, Sent, SentBatch};
    use std::any::Any;

    struct PassThrough(std::sync::Arc<dyn MessagePublisher>);

    #[async_trait::async_trait]
    impl MessagePublisher for PassThrough {
        fn on_disconnect_hook(&self) -> Option<crate::traits::BoxFuture<'_, anyhow::Result<()>>> {
            self.0.on_disconnect_hook()
        }

        async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
            self.0.send(message).await
        }

        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            self.0.send_batch(messages).await
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.emit_done = SpoolDone::Success;

    let publisher: std::sync::Arc<dyn MessagePublisher> =
        std::sync::Arc::new(DirSpoolPublisher::new(&cfg).await.unwrap());
    let wrapped = PassThrough(publisher);
    wrapped.send(message("payload", "kind")).await.unwrap();

    crate::traits::with_disconnect_outcome(DisconnectOutcome::Completed, async {
        wrapped.on_disconnect_hook().unwrap().await.unwrap();
    })
    .await;

    assert!(
        dir.path().join("DONE").exists(),
        "a wrapper that only forwards the hook must not lose the outcome; spool held {:?}",
        entries(dir.path())
    );
}

/// A spool written flat and later sharded holds chunks at two levels, and the deeper one is
/// the higher sequence: finding the flat one first must not end the search, or the next
/// chunk overwrites the sharded one.
#[tokio::test]
async fn resumes_past_a_flat_chunk_that_a_sharded_one_outranks() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.naming_pattern = "{seq:03}".to_string();
    cfg.shard_depth = 2;
    cfg.shard_width = 1;

    // Chunk 5 written before sharding was turned on, chunk 12 after.
    std::fs::write(dir.path().join("005.bin"), b"flat").unwrap();
    std::fs::create_dir_all(dir.path().join("0/1")).unwrap();
    std::fs::write(dir.path().join("0/1/2.bin"), b"sharded").unwrap();

    assert_eq!(
        highest_sequence(dir.path(), "bin", Sharding::from_config(&cfg))
            .await
            .unwrap(),
        Some(12)
    );

    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("next", "x")])
        .await
        .unwrap();
    assert!(
        dir.path().join("0/1/3.bin").exists(),
        "chunk 13 follows chunk 12; spool held {:?}",
        tree_entries(dir.path())
    );
}

/// An unreadable chunk sorts at the head of the listing, so it must not be able to hold the
/// queue there for the life of the consumer.
#[tokio::test]
async fn a_chunk_that_never_reads_is_set_aside() {
    let dir = tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.poll_interval_ms = 1;

    // A directory where a payload file belongs: listed as a chunk, unreadable as one.
    std::fs::create_dir(dir.path().join("000000000.bin")).unwrap();
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("good", "x")])
        .await
        .unwrap();
    drop(publisher);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let first = consumer.receive_batch(10).await.unwrap();
    assert_eq!(
        first.messages.len(),
        1,
        "the readable chunk in the batch is still delivered"
    );
    (first.commit)(vec![MessageDisposition::Ack]).await.unwrap();

    for _ in 0..MAX_CHUNK_READ_FAILURES {
        assert!(consumer
            .receive_batch(10)
            .await
            .unwrap()
            .messages
            .is_empty());
    }
    assert!(
        consumer.claimed.lock().unwrap().contains("000000000"),
        "the unreadable chunk is set aside rather than kept at the queue head"
    );
    assert!(
        dir.path().join("000000000.bin").exists(),
        "and left on disk for the operator"
    );
    assert!(consumer.read_failures.is_empty());
}

/// The queue is not at its end while a delivered chunk can still be nacked back into it.
#[tokio::test]
async fn end_of_queue_waits_for_unsettled_chunks() {
    for stop_on_done in [false, true] {
        let dir = tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.stop_on_done = stop_on_done;
        let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
        publisher.send_batch(vec![message("a", "x")]).await.unwrap();
        drop(publisher);
        std::fs::write(dir.path().join("DONE"), b"").unwrap();

        let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
        consumer.set_exit_on_empty(!stop_on_done);
        let first = consumer.receive_batch(10).await.unwrap();
        assert_eq!(first.messages.len(), 1);
        let nack = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            (first.commit)(vec![MessageDisposition::Nack])
                .await
                .unwrap();
        });

        let again = consumer
            .receive_batch(10)
            .await
            .expect("an unsettled chunk is not the end of the stream");
        assert_eq!(again.messages.len(), 1, "the nacked chunk is redelivered");
        nack.await.unwrap();
        (again.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert!(drain(&mut consumer, 10).await.is_empty());
    }
}

/// A commit that is dropped unused settles nothing, so its chunks go back in the queue.
#[tokio::test]
async fn a_dropped_commit_requeues_its_chunks() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher.send_batch(vec![message("a", "x")]).await.unwrap();
    drop(publisher);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    consumer.set_exit_on_empty(true);
    drop(consumer.receive_batch(10).await.unwrap());
    assert_eq!(drain(&mut consumer, 10).await.len(), 1);
}

/// A slow settlement is still an open batch: the end of the queue is not reported over it.
#[tokio::test]
async fn end_of_queue_outlasts_the_settle_timeout() {
    for stop_on_done in [false, true] {
        let dir = tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.stop_on_done = stop_on_done;
        let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
        publisher.send_batch(vec![message("a", "x")]).await.unwrap();
        drop(publisher);
        std::fs::write(dir.path().join("DONE"), b"").unwrap();

        let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
        consumer.set_exit_on_empty(!stop_on_done);
        consumer.settle_timeout = Duration::from_millis(20);
        let first = consumer.receive_batch(10).await.unwrap();
        let early = tokio::time::timeout(Duration::from_millis(300), consumer.receive_batch(10));
        assert!(early.await.is_err(), "ended with a batch still unsettled");

        (first.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();
        assert_eq!(consumer.receive_batch(10).await.unwrap().messages.len(), 1);
    }
}

/// A commit cancelled part-way releases the chunks it had not settled yet.
#[tokio::test]
async fn a_cancelled_commit_requeues_the_unsettled_chunks() {
    let dir = tempdir().unwrap();
    let cfg = config(dir.path());
    let publisher = DirSpoolPublisher::new(&cfg).await.unwrap();
    publisher
        .send_batch(vec![message("a", "x"), message("b", "x")])
        .await
        .unwrap();
    drop(publisher);

    let mut consumer = DirSpoolConsumer::new(&cfg).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    let mut commit = (batch.commit)(vec![MessageDisposition::Ack; 2]);
    // The first delete is a blocking call, so one poll leaves the commit in the middle.
    assert!(futures::poll!(commit.as_mut()).is_pending());
    drop(commit);

    assert!(!consumer.claimed.lock().unwrap().contains("000000001"));
    assert!(consumer
        .requeued
        .lock()
        .unwrap()
        .contains(&"000000001".to_string()));
}

#[test]
fn a_pattern_with_digits_right_after_the_sequence_is_rejected() {
    for pattern in ["{seq:09}{timestamp}", "{seq:09}{message_id}", "{seq:09}7"] {
        let mut config = DirSpoolConfig::new("/tmp/spool");
        config.naming_pattern = pattern.to_string();
        assert!(validate_naming_pattern(&config).is_err(), "{pattern}");
    }
    for pattern in ["{seq:09}", "{seq:09}-{timestamp}", "{seq:09}_{message_id}"] {
        let mut config = DirSpoolConfig::new("/tmp/spool");
        config.naming_pattern = pattern.to_string();
        assert!(validate_naming_pattern(&config).is_ok(), "{pattern}");
    }
}
