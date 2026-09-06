use crate::endpoints::file::{FileConsumer, FilePublisher};
#[allow(unused_imports)]
use crate::models::{Compression, FileConfig, FileConsumerMode, FileFormat, NameBy};
use crate::msg;
use crate::traits::MessageConsumer;
use crate::traits::MessagePublisher;
use serde_json::json;
use tempfile::tempdir;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_gzip_roundtrip() {
    use std::io::Read as _;

    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.jsonl.gz");
    let path = file_path.to_str().unwrap().to_string();

    let config = FileConfig {
        path: path.clone(),
        format: FileFormat::Raw,
        compression: Compression::Gzip,
        ..Default::default()
    };

    // Write two batches -> two gzip members appended to the same file.
    let sink = FilePublisher::new(&config).await.unwrap();
    let m1 = msg!(json!({"id": 1, "name": "alice"}));
    let m2 = msg!(json!({"id": 2, "name": "bob"}));
    let m3 = msg!(json!({"id": 3, "name": "carol"}));
    sink.send_batch(vec![m1.clone(), m2.clone()]).await.unwrap();
    sink.send_batch(vec![m3.clone()]).await.unwrap();
    drop(sink);

    // The file is a valid standard gzip stream (concatenated members),
    // decodable by any gzip tool.
    let mut raw = std::fs::File::open(&file_path).unwrap();
    let mut compressed = Vec::new();
    std::io::Read::read_to_end(&mut raw, &mut compressed).unwrap();
    let mut decoded = String::new();
    flate2::read::MultiGzDecoder::new(&compressed[..])
        .read_to_string(&mut decoded)
        .unwrap();
    assert_eq!(decoded.lines().count(), 3);

    // Read the records back through the consumer. Bound the loop so an empty
    // stream can't retry forever (mirrors `collect_compressed`'s 5s cap).
    let mut source = FileConsumer::new(&config).await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut got = Vec::new();
        while got.len() < 3 {
            let batch = source.receive_batch(10).await.unwrap();
            if batch.messages.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            }
            let len = batch.messages.len();
            for m in &batch.messages {
                got.push(m.payload.clone());
            }
            let _ = (batch.commit)(vec![crate::traits::MessageDisposition::Ack; len]).await;
        }
        got
    })
    .await
    .expect("timed out collecting gzip messages");
    assert_eq!(got, vec![m1.payload, m2.payload, m3.payload]);
}

#[cfg(any(feature = "compression", feature = "encryption"))]
async fn collect_compressed(source: &mut FileConsumer, n: usize) -> Vec<bytes::Bytes> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut got = Vec::new();
        while got.len() < n {
            let batch = source.receive_batch(10).await.unwrap();
            if batch.messages.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            }
            for m in &batch.messages {
                got.push(m.payload.clone());
            }
        }
        got
    })
    .await
    .expect("timed out collecting gzip messages")
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_lz4_roundtrip() {
    // Two batches -> two concatenated lz4 frames; the consumer decodes both.
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("data.jsonl.lz4")
        .to_str()
        .unwrap()
        .to_string();
    let config = FileConfig {
        path,
        format: FileFormat::Raw,
        compression: Compression::Lz4,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let m1 = msg!(json!({"id": 1}));
    let m2 = msg!(json!({"id": 2}));
    let m3 = msg!(json!({"id": 3}));
    sink.send_batch(vec![m1.clone(), m2.clone()]).await.unwrap();
    sink.send_batch(vec![m3.clone()]).await.unwrap();
    drop(sink);

    let mut source = FileConsumer::new(&config).await.unwrap();
    assert_eq!(
        collect_compressed(&mut source, 3).await,
        vec![m1.payload, m2.payload, m3.payload]
    );
}

#[cfg(all(feature = "compression", feature = "encryption"))]
#[tokio::test]
async fn test_file_encrypted_compressed_roundtrip() {
    use base64::Engine as _;

    // compress-then-encrypt with length-prefix framing across multiple
    // batches: the on-disk bytes are ciphertext, and the consumer reads the
    // original records back.
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.enc").to_str().unwrap().to_string();
    let encryption = Some(crate::models::EncryptionConfig {
        key: base64::engine::general_purpose::STANDARD.encode([42u8; 32]),
        ..Default::default()
    });
    let config = FileConfig {
        path: path.clone(),
        format: FileFormat::Raw,
        compression: Compression::Gzip,
        encryption: encryption.clone(),
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let m1 = msg!(json!({"id": 1, "name": "alice"}));
    let m2 = msg!(json!({"id": 2, "name": "bob"}));
    let m3 = msg!(json!({"id": 3, "name": "carol"}));
    sink.send_batch(vec![m1.clone(), m2.clone()]).await.unwrap();
    sink.send_batch(vec![m3.clone()]).await.unwrap();
    drop(sink);

    // The raw file is not a gzip stream (it is framed ciphertext).
    let raw = std::fs::read(&path).unwrap();
    assert!(!raw.is_empty());
    let mut decoded = Vec::new();
    assert!(std::io::Read::read_to_end(
        &mut flate2::read::MultiGzDecoder::new(&raw[..]),
        &mut decoded
    )
    .is_err());
    // The plaintext does not appear anywhere in the file.
    assert!(!raw.windows(5).any(|w| w == b"alice"));

    let mut source = FileConsumer::new(&config).await.unwrap();
    assert_eq!(
        collect_compressed(&mut source, 3).await,
        vec![m1.payload.clone(), m2.payload.clone(), m3.payload.clone()]
    );

    // A consumer with a different key must fail, not emit garbage.
    let wrong_key = FileConfig {
        encryption: Some(crate::models::EncryptionConfig {
            key: base64::engine::general_purpose::STANDARD.encode([1u8; 32]),
            ..Default::default()
        }),
        ..config.clone()
    };
    let mut source = FileConsumer::new(&wrong_key).await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            match source.receive_batch(10).await {
                Ok(b) if b.messages.is_empty() => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await
                }
                other => break other,
            }
        }
    })
    .await;
    // A codec/key mismatch is a permanent decode failure: it must surface as
    // ConsumerError::Permanent (which fails the route), never as data or a clean
    // EndOfStream that would masquerade as success.
    assert!(
        matches!(got, Ok(Err(crate::traits::ConsumerError::Permanent(_)))),
        "expected ConsumerError::Permanent, got {got:?}"
    );
}

#[tokio::test]
async fn test_reading_compressed_file_without_codec_is_rejected() {
    // A gzip file read with no `compression` configured must be rejected at connect,
    // not read as plaintext and split into binary "messages" under a clean success.
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.bin");
    // gzip magic + arbitrary bytes.
    std::fs::write(&path, [0x1f, 0x8b, 0x08, 0x00, 0x11, 0x22]).unwrap();
    let config = FileConfig {
        path: path.to_string_lossy().to_string(),
        ..Default::default()
    };
    let err = match FileConsumer::new(&config).await {
        Ok(_) => panic!("expected a rejection reading a gzip file without `compression`"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("gzip"), "unexpected error: {err}");
    // A plaintext JSON file is accepted.
    std::fs::write(&path, b"{\"a\":1}\n").unwrap();
    assert!(FileConsumer::new(&config).await.is_ok());
}

#[test]
fn test_sniff_compression_magic() {
    use super::sniff_compression_magic;
    let dir = tempdir().unwrap();
    let cases: &[(&[u8], Option<&str>)] = &[
        (&[0x1f, 0x8b, 0x08], Some("gzip")),
        (&[0x28, 0xb5, 0x2f, 0xfd], Some("zstd")),
        (&[0x04, 0x22, 0x4d, 0x18], Some("lz4")),
        (b"{\"json\":1}", None),
        (&[0x1f], None), // too short to be gzip
        (b"", None),
    ];
    for (i, (bytes, want)) in cases.iter().enumerate() {
        let p = dir.path().join(format!("f{i}"));
        std::fs::write(&p, bytes).unwrap();
        assert_eq!(
            sniff_compression_magic(&p.to_string_lossy()),
            *want,
            "case {i}"
        );
    }
    assert_eq!(sniff_compression_magic("/no/such/file"), None);
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_gzip_incremental_growth() {
    // A second batch appended as a new gzip member to the same file must be
    // picked up by an already-running consumer via the growth re-scan (which
    // re-decompresses from the start and skips already-emitted records).
    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("grow.jsonl.gz")
        .to_str()
        .unwrap()
        .to_string();
    let config = FileConfig {
        path,
        format: FileFormat::Raw,
        compression: Compression::Gzip,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let mut source = FileConsumer::new(&config).await.unwrap();

    let a = msg!(json!({"seq": 1}));
    sink.send_batch(vec![a.clone()]).await.unwrap();
    assert_eq!(collect_compressed(&mut source, 1).await, vec![a.payload]);

    // Append a second member after the consumer already drained the first.
    let b = msg!(json!({"seq": 2}));
    let c = msg!(json!({"seq": 3}));
    sink.send_batch(vec![b.clone(), c.clone()]).await.unwrap();
    assert_eq!(
        collect_compressed(&mut source, 2).await,
        vec![b.payload, c.payload]
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_compression_rejects_unsupported_modes() {
    let dir = tempdir().unwrap();
    for mode in [
        FileConsumerMode::Consume { delete: true },
        FileConsumerMode::Subscribe { delete: false },
        FileConsumerMode::Subscribe { delete: true },
        FileConsumerMode::GroupSubscribe {
            group_id: "g".to_string(),
            read_from_tail: false,
        },
    ] {
        let path = dir.path().join("m.gz").to_str().unwrap().to_string();
        let config = FileConfig {
            path,
            format: FileFormat::Raw,
            compression: Compression::Gzip,
            mode: Some(mode.clone()),
            ..Default::default()
        };
        assert!(
            FileConsumer::new(&config).await.is_err(),
            "expected rejection for mode {mode:?}"
        );
    }
}

#[tokio::test]
async fn test_file_sink_and_source_integration() {
    // Setup a temporary directory and file path
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("test.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    let config = FileConfig {
        path: file_path_str.clone(),
        ..Default::default()
    };
    let sink = FilePublisher::new(&config).await.unwrap();

    let msg1 = msg!(json!({"hello": "world"}));
    let msg2 = msg!(json!({"foo": "bar"}));

    sink.send_batch(vec![msg1.clone(), msg2.clone()])
        .await
        .unwrap();
    // Explicitly flush to ensure data is written before we try to read it.
    sink.flush().await.unwrap();
    // Drop the sink to release the file lock on some OSes before the source tries to open it.
    drop(sink);

    // Create a FileConsumer to read from the same file
    let mut source = FileConsumer::new(&config).await.unwrap();

    // Receive the messages and verify them
    let received1 = source.receive().await.unwrap();
    let _ = (received1.commit)(crate::traits::MessageDisposition::Ack).await; // Commit is a no-op, but we should call it

    assert_eq!(received1.message.message_id, msg1.message_id);
    assert_eq!(received1.message.payload, msg1.payload);

    let batch = source.receive_batch(1).await.unwrap();
    let (received_msgs, commit2) = (batch.messages, batch.commit);
    let len = received_msgs.len();
    let received_msg2 = received_msgs.into_iter().next().unwrap();
    let _ = commit2(vec![crate::traits::MessageDisposition::Ack; len]).await;
    assert_eq!(received_msg2.message_id, msg2.message_id);
    assert_eq!(received_msg2.payload, msg2.payload);

    // After draining, the consumer surfaces a one-shot empty batch (the
    //    drain marker) so a route can pause or exit_on_empty can fire.
    let drained = source.receive_batch(1).await.unwrap();
    assert!(
        drained.messages.is_empty(),
        "Expected an empty drain marker after the file was drained"
    );

    // With the marker already emitted and no new data, a further read
    //    blocks (times out) until new data arrives.
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        source.receive_batch(1),
    )
    .await;
    assert!(result.is_err(), "Expected timeout waiting for new data");
}

#[tokio::test]
async fn test_file_sink_creates_directory() {
    let dir = tempdir().unwrap();
    let nested_dir_path = dir.path().join("nested");
    let file_path = nested_dir_path.join("test.log");

    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        ..Default::default()
    };
    let sink_result = FilePublisher::new(&config).await;

    assert!(sink_result.is_ok());
    assert!(nested_dir_path.exists());
    assert!(file_path.exists());
}

#[tokio::test]
async fn idempotent_file_sink_replays_only_uncovered_kafka_offsets_after_restart() {
    fn kafka_message(offset: i64) -> crate::CanonicalMessage {
        let mut message = msg!(json!({ "offset": offset }));
        message
            .metadata
            .insert("mqb.src.kafka_topic".into(), "orders".into());
        message
            .metadata
            .insert("mqb.src.kafka_partition".into(), "0".into());
        message
            .metadata
            .insert("mqb.src.kafka_offset".into(), offset.to_string());
        message
    }

    let dir = tempdir().unwrap();
    let output = dir.path().join("parts");
    let config = FileConfig {
        path: output.to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        ..Default::default()
    };
    let publisher = FilePublisher::new(&config).await.unwrap();
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    drop(publisher);

    // Debris from the crashed run is reaped; a staging file young enough to belong to a
    // concurrent writer is left alone.
    let stale = output.join(".stage-crash");
    tokio::fs::write(&stale, b"incomplete").await.unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
        .unwrap();
    tokio::fs::write(output.join(".stage-inflight"), b"other worker")
        .await
        .unwrap();

    let restarted = FilePublisher::new(&config).await.unwrap();
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1), kafka_message(2)])
        .await
        .unwrap();

    let mut names = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        vec![
            ".stage-inflight".to_string(),
            "part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl".to_string(),
            "part-orders-0000000000-00000000000000000002-00000000000000000002.jsonl".to_string(),
        ]
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn idempotent_file_parts_are_compressed_and_named_for_it() {
    fn kafka_message(offset: i64) -> crate::CanonicalMessage {
        let mut message = msg!(json!({ "offset": offset }));
        message
            .metadata
            .insert("mqb.src.kafka_topic".into(), "orders".into());
        message
            .metadata
            .insert("mqb.src.kafka_partition".into(), "0".into());
        message
            .metadata
            .insert("mqb.src.kafka_offset".into(), offset.to_string());
        message
    }

    let dir = tempdir().unwrap();
    let output = dir.path().join("parts");
    let config = FileConfig {
        path: output.to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        compression: crate::models::Compression::Gzip,
        ..Default::default()
    };
    let publisher = FilePublisher::new(&config).await.unwrap();
    publisher
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    drop(publisher);

    // One part file, named for the codec, holding one gzip member with both records.
    let part =
        output.join("part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl.gz");
    let raw = std::fs::read(&part).unwrap();
    let plain =
        crate::support::compression::decompress_all(crate::models::Compression::Gzip, &raw, None)
            .unwrap();
    assert_eq!(
        plain
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .count(),
        2
    );

    // The restart parses the longer extension, so the covered offsets are not rewritten.
    let restarted = FilePublisher::new(&config).await.unwrap();
    restarted
        .send_batch(vec![kafka_message(0), kafka_message(1)])
        .await
        .unwrap();
    let names = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        vec![
            "part-orders-0000000000-00000000000000000000-00000000000000000001.jsonl.gz".to_string()
        ]
    );
}

#[tokio::test]
async fn idempotent_file_sink_rejects_records_without_source_metadata() {
    let dir = tempdir().unwrap();
    let output = dir.path().join("parts");
    let config = FileConfig {
        path: output.to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        ..Default::default()
    };
    let publisher = FilePublisher::new(&config).await.unwrap();

    assert!(publisher
        .send_batch(vec![msg!(json!({ "id": 1 }))])
        .await
        .is_err());
    assert!(std::fs::read_dir(output).unwrap().next().is_none());
}

#[tokio::test]
async fn file_source_metadata_numbers_records_and_feeds_an_idempotent_sink() {
    use crate::traits::MessageConsumer;

    let dir = tempdir().unwrap();
    let input = dir.path().join("orders.jsonl");
    std::fs::write(&input, "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n").unwrap();

    let source = FileConfig {
        path: input.to_string_lossy().into_owned(),
        source_metadata: true,
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&source).await.unwrap();
    let batch = consumer.receive_batch(10).await.unwrap();
    assert_eq!(batch.messages.len(), 3);

    // Records are numbered by index, not byte offset, so they stay consecutive.
    let records = batch
        .messages
        .iter()
        .map(|m| m.metadata.get("mqb.src.file_record").unwrap().as_str())
        .collect::<Vec<_>>();
    assert_eq!(records, vec!["0", "1", "2"]);

    // The whole batch lands as one part file covering records 0-2.
    let output = dir.path().join("parts");
    let sink = FileConfig {
        path: output.to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        ..Default::default()
    };
    let publisher = FilePublisher::new(&sink).await.unwrap();
    publisher.send_batch(batch.messages).await.unwrap();

    let names = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with(".stage"))
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 1, "one object per contiguous run: {names:?}");
    assert!(
        names[0].ends_with("-00000000000000000000-00000000000000000002.jsonl"),
        "unexpected part name {}",
        names[0]
    );
}

#[tokio::test]
async fn resuming_file_modes_get_a_run_epoch_so_reruns_cannot_reuse_a_record_index() {
    use crate::support::source_ranges::SourcePosition;
    use crate::traits::MessageConsumer;

    let dir = tempdir().unwrap();
    let input = dir.path().join("orders.jsonl");
    std::fs::write(&input, "{\"id\":1}\n{\"id\":2}\n").unwrap();

    let config = FileConfig {
        path: input.to_string_lossy().into_owned(),
        source_metadata: true,
        mode: Some(FileConsumerMode::GroupSubscribe {
            group_id: "g1".into(),
            read_from_tail: false,
        }),
        ..Default::default()
    };

    // Allowed, not rejected: the setup is only weaker, not wrong.
    let mut first = FileConsumer::new(&config).await.unwrap();
    let batch = first.receive_batch(10).await.unwrap();
    assert!(!batch.messages.is_empty());
    let first_run = SourcePosition::from_message(&batch.messages[0]).unwrap();
    assert!(batch.messages[0]
        .metadata
        .contains_key("mqb.src.file_epoch"));

    // A second run restarts the record index at 0, so the epoch is what stops it from
    // naming those records the same as the first run's and having them dropped.
    let mut second = FileConsumer::new(&config).await.unwrap();
    let batch = second.receive_batch(10).await.unwrap();
    let second_run = SourcePosition::from_message(&batch.messages[0]).unwrap();

    assert_ne!(first_run.source, second_run.source);
    // A later run reads later records, so its objects must sort after the earlier run's.
    assert!(first_run.source < second_run.source);
}

#[tokio::test]
async fn run_epochs_are_distinct_for_consumers_created_in_the_same_millisecond() {
    use crate::support::source_ranges::SourcePosition;

    let dir = tempdir().unwrap();
    let input = dir.path().join("orders.jsonl");
    std::fs::write(&input, "{\"id\":1}\n").unwrap();

    let config = FileConfig {
        path: input.to_string_lossy().into_owned(),
        source_metadata: true,
        mode: Some(FileConsumerMode::GroupSubscribe {
            group_id: "same-ms".into(),
            read_from_tail: false,
        }),
        ..Default::default()
    };

    // No sleep between them: the epoch is allocated monotonically, not read off the clock.
    let mut first = FileConsumer::new(&config).await.unwrap();
    let mut second = FileConsumer::new(&config).await.unwrap();

    let a = first.receive_batch(10).await.unwrap();
    let b = second.receive_batch(10).await.unwrap();
    let first_run = SourcePosition::from_message(&a.messages[0]).unwrap();
    let second_run = SourcePosition::from_message(&b.messages[0]).unwrap();

    assert!(
        first_run.source < second_run.source,
        "epochs must be distinct and increasing: {:?} vs {:?}",
        first_run.source,
        second_run.source
    );
}

#[tokio::test]
async fn consume_mode_repeats_its_record_identity_across_runs() {
    use crate::support::source_ranges::SourcePosition;
    use crate::traits::MessageConsumer;

    let dir = tempdir().unwrap();
    let input = dir.path().join("orders.jsonl");
    std::fs::write(&input, "{\"id\":1}\n{\"id\":2}\n").unwrap();

    let config = FileConfig {
        path: input.to_string_lossy().into_owned(),
        source_metadata: true,
        ..Default::default()
    };

    let mut first = FileConsumer::new(&config).await.unwrap();
    let a = first.receive_batch(10).await.unwrap();
    let mut second = FileConsumer::new(&config).await.unwrap();
    let b = second.receive_batch(10).await.unwrap();

    // No epoch: re-reading the same file must produce the same names, which is exactly
    // how the idempotent sink recognises the rewrite and skips it.
    assert!(!a.messages[0].metadata.contains_key("mqb.src.file_epoch"));
    assert_eq!(
        SourcePosition::from_message(&a.messages[0]).unwrap(),
        SourcePosition::from_message(&b.messages[0]).unwrap()
    );
}

#[tokio::test]
async fn idempotent_file_sink_replays_postgres_cdc_changes_in_one_commit() {
    fn postgres_message(ordinal: u64) -> crate::CanonicalMessage {
        let mut message = msg!(json!({ "ordinal": ordinal }));
        message
            .metadata
            .insert("mqb.src.postgres_slot".into(), "bridge_slot".into());
        message
            .metadata
            .insert("mqb.src.postgres_lsn".into(), "9876543210".into());
        message
            .metadata
            .insert("mqb.src.postgres_ordinal".into(), ordinal.to_string());
        message
    }

    let dir = tempdir().unwrap();
    let output = dir.path().join("parts");
    let config = FileConfig {
        path: output.to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        ..Default::default()
    };
    let publisher = FilePublisher::new(&config).await.unwrap();
    publisher
        .send_batch(vec![postgres_message(0), postgres_message(1)])
        .await
        .unwrap();
    publisher
        .send_batch(vec![
            postgres_message(0),
            postgres_message(1),
            postgres_message(2),
        ])
        .await
        .unwrap();

    let mut names = std::fs::read_dir(&output)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        vec![
            "part-postgres_cdc-bridge_slot-00000000009876543210-0000000000-00000000000000000000-00000000000000000001.jsonl".to_string(),
            "part-postgres_cdc-bridge_slot-00000000009876543210-0000000000-00000000000000000002-00000000000000000002.jsonl".to_string(),
        ]
    );
}

#[tokio::test]
async fn idempotent_file_sink_rejects_unsupported_output_formats() {
    // CSV still needs a header row per part file, which is unimplemented.
    let dir = tempdir().unwrap();
    let csv = FileConfig {
        path: dir.path().join("csv").to_string_lossy().into_owned(),
        name_by: NameBy::SourcePosition,
        format: FileFormat::Csv,
        ..Default::default()
    };
    assert!(FilePublisher::new(&csv).await.is_err());
}

#[tokio::test]
async fn test_file_consumer_consume_mode() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("consume.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write 3 lines
    tokio::fs::write(&file_path, b"line1\nline2\nline3\n")
        .await
        .unwrap();

    let config = FileConfig {
        path: file_path_str,
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    // Receive first message
    let received1 = consumer.receive().await.unwrap();
    assert_eq!(received1.message.payload.as_ref(), b"line1");

    // Commit first message (should remove line1)
    (received1.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Verify file content - wait for async deletion
    let mut content = String::new();
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content == "line2\nline3\n" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "line2\nline3\n");

    // Receive second message
    let received2 = consumer.receive().await.unwrap();
    assert_eq!(received2.message.payload.as_ref(), b"line2");
    (received2.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Receive third message
    let received3 = consumer.receive().await.unwrap();
    assert_eq!(received3.message.payload.as_ref(), b"line3");
    (received3.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Verify file is empty
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");
}

#[tokio::test]
async fn test_file_consumer_nack_behavior() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("nack.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write 2 lines
    tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    let batch1 = consumer.receive_batch(1).await.unwrap();
    assert_eq!(batch1.messages.len(), 1);
    assert_eq!(batch1.messages[0].payload.as_ref(), b"msg1");

    (batch1.commit)(vec![crate::traits::MessageDisposition::Nack])
        .await
        .unwrap();

    // Receive again - should get msg1 again because it wasn't removed
    let batch2 = consumer.receive_batch(1).await.unwrap();
    assert_eq!(batch2.messages.len(), 1);
    assert_eq!(batch2.messages[0].payload.as_ref(), b"msg1");

    (batch2.commit)(vec![crate::traits::MessageDisposition::Ack])
        .await
        .unwrap();

    // Receive next - should get msg2
    let batch3 = consumer.receive_batch(1).await.unwrap();
    assert_eq!(batch3.messages.len(), 1);
    assert_eq!(batch3.messages[0].payload.as_ref(), b"msg2");
}

#[tokio::test]
async fn test_file_consumer_consume_no_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("consume_no_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write 3 lines
    tokio::fs::write(&file_path, b"line1\nline2\nline3\n")
        .await
        .unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    // Receive first message
    let received1 = consumer.receive().await.unwrap();
    assert_eq!(received1.message.payload.as_ref(), b"line1");

    // Commit first message (should NOT remove line1)
    (received1.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Give some time for any potential (but unwanted) background deletion to happen
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify file content remains unchanged
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "line1\nline2\nline3\n");

    // Receive second message
    let received2 = consumer.receive().await.unwrap();
    assert_eq!(received2.message.payload.as_ref(), b"line2");
}

#[tokio::test]
async fn test_file_consumer_subscribe_mode() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("subscribe.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write initial content
    tokio::fs::write(&file_path, b"line1\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: false }),
        ..Default::default()
    };

    let mut consumer = FileConsumer::new(&config).await.unwrap();

    // Give the background tailer a moment to initialize and find its starting position.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Append new line
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&file_path)
            .await
            .unwrap();
        file.write_all(b"line2\n").await.unwrap();
    }

    // Receive new line, skipping any empty drain marker emitted while the
    // subscriber was caught up to the end of the file at startup.
    let received2 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let batch = consumer.receive_batch(2).await.unwrap();
            if !batch.messages.is_empty() {
                break batch;
            }
        }
    })
    .await
    .expect("timed out waiting for appended line");
    assert_eq!(received2.messages.len(), 1);
    assert_eq!(received2.messages[0].payload.as_ref(), b"line2");
    (received2.commit)(vec![crate::traits::MessageDisposition::Ack])
        .await
        .unwrap();

    // Verify file content is unchanged
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "line1\nline2\n");
}

#[tokio::test]
async fn test_file_consumer_consume_explicit_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("consume_explicit_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    tokio::fs::write(&file_path, b"line1\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    let received = consumer.receive().await.unwrap();
    assert_eq!(received.message.payload.as_ref(), b"line1");

    (received.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Verify file becomes empty
    let mut content = String::new();
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");
}

#[tokio::test]
async fn test_file_consumer_subscribe_with_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("subscribe_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    tokio::fs::write(&file_path, b"line1\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: true }),
        ..Default::default()
    };

    let mut sub1 = FileConsumer::new(&config).await.unwrap();
    let mut sub2 = FileConsumer::new(&config).await.unwrap();

    let msg1 = sub1.receive().await.unwrap();
    assert_eq!(msg1.message.payload.as_ref(), b"line1");

    let msg2 = sub2.receive().await.unwrap();
    assert_eq!(msg2.message.payload.as_ref(), b"line1");

    // Sub1 acks. File should NOT be deleted yet.
    (msg1.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "line1\n");

    // Sub2 acks. File should be deleted.
    (msg2.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    let mut content = String::new();
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");
}

#[tokio::test]
async fn test_file_consumer_subscribe_explicit_no_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("subscribe_no_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    tokio::fs::write(&file_path, b"line1\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: false }),
        ..Default::default()
    };

    let mut consumer = FileConsumer::new(&config).await.unwrap();
    // Give the background tailer a moment to initialize and find its starting position.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&file_path)
            .await
            .unwrap();
        file.write_all(b"line2\n").await.unwrap();
    }

    let received = consumer.receive().await.unwrap();
    assert_eq!(received.message.payload.as_ref(), b"line2");

    (received.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "line1\nline2\n");
}

use crate::models::{Endpoint, EndpointType, Route};

// Regression (issue #71): the reporter's repro — 20 numbered rows, file -> file,
// batch_size 5, concurrency 4 — used to emit whole batches out of source order
// (e.g. 15..19 first). A file is an ordered log, so `FilePublisher` declares
// `requires_ordered_publish()` and the route sequences the sends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_route_file_to_file_preserves_order_at_concurrency() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("rows.jsonl");
    let dst = dir.path().join("out.jsonl");
    let rows: String = (0..20).map(|i| format!("{i}\n")).collect();
    tokio::fs::write(&src, rows.as_bytes()).await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: src.to_str().unwrap().to_string(),
        mode: Some(FileConsumerMode::Consume { delete: false }),
        format: FileFormat::Raw,
        ..Default::default()
    }));
    let output = Endpoint::new(EndpointType::File(FileConfig {
        path: dst.to_str().unwrap().to_string(),
        format: FileFormat::Raw,
        ..Default::default()
    }));
    let route = Route::new(input, output)
        .with_concurrency(4)
        .with_batch_size(5)
        .with_exit_on_empty(true);

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        route.run_until_err("file_order_regression", None, None),
    )
    .await
    .expect("Route should drain and exit")
    .expect("Route should complete without errors");

    let content = tokio::fs::read_to_string(&dst).await.unwrap();
    let written: Vec<&str> = content.lines().collect();
    let expected: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    assert_eq!(written, expected, "File sink must preserve source order");
}

// A buffering publisher only returns from `send_batch` once its buffer flushed, so
// sequencing sends must not let it wait for a batch that is itself waiting to be sent.
// `max_delay_ms` guarantees the flush, but the interaction is worth pinning: this hangs
// if the ordered path ever gates on something the sink needs first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_route_ordered_file_sink_with_buffer_does_not_stall() {
    use crate::models::{BufferMiddleware, Middleware};

    let dir = tempdir().unwrap();
    let src = dir.path().join("buf_rows.jsonl");
    let dst = dir.path().join("buf_out.jsonl");
    let rows: String = (0..20).map(|i| format!("{i}\n")).collect();
    tokio::fs::write(&src, rows.as_bytes()).await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: src.to_str().unwrap().to_string(),
        mode: Some(FileConsumerMode::Consume { delete: false }),
        format: FileFormat::Raw,
        ..Default::default()
    }));
    let mut output = Endpoint::new(EndpointType::File(FileConfig {
        path: dst.to_str().unwrap().to_string(),
        format: FileFormat::Raw,
        ..Default::default()
    }));
    // Buffer larger than one batch, so every flush is timer-driven.
    output
        .middlewares
        .push(Middleware::Buffer(BufferMiddleware {
            max_messages: 50,
            max_delay_ms: 20,
        }));

    let route = Route::new(input, output)
        .with_concurrency(4)
        .with_batch_size(5)
        .with_exit_on_empty(true);

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        route.run_until_err("file_order_buffer", None, None),
    )
    .await
    .expect("Ordered sends through a buffer must not stall")
    .expect("Route should complete without errors");

    let content = tokio::fs::read_to_string(&dst).await.unwrap();
    let written: Vec<&str> = content.lines().collect();
    let expected: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    assert_eq!(written, expected);
}

#[tokio::test]
async fn test_route_file_consume_explicit_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("route_consume_explicit_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();
    tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..Default::default()
    }));
    let output = Endpoint::new_memory("out_consume_explicit_delete", 10);
    let route = Route::new(input, output.clone());

    let handle = route
        .run("test_route_consume_explicit_delete")
        .await
        .unwrap();

    let channel = output.channel().unwrap();
    // Wait for message
    let mut received = Vec::new();
    for _ in 0..20 {
        if !channel.is_empty() {
            received = channel.drain_messages();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(received.len(), 1);
    assert_eq!(&received[0].payload.to_vec(), b"msg1");

    // Verify deletion
    let mut content = String::new();
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");

    handle.stop().await;
}

#[tokio::test]
async fn test_route_file_subscribe_with_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("route_subscribe_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();
    tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: true }),
        ..Default::default()
    }));
    let output = Endpoint::new_memory("out_subscribe_delete", 10);
    let route = Route::new(input, output.clone());

    let handle = route.run("test_route_subscribe_delete").await.unwrap();

    let channel = output.channel().unwrap();
    let mut received = Vec::new();
    for _ in 0..20 {
        if !channel.is_empty() {
            received = channel.drain_messages();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(received.len(), 1);

    // Verify deletion
    let mut content = String::new();
    for _ in 0..20 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");

    handle.stop().await;
}

#[tokio::test]
async fn test_route_file_subscribe_explicit_no_delete() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("route_subscribe_no_delete.log");
    let file_path_str = file_path.to_str().unwrap().to_string();
    tokio::fs::write(&file_path, b"msg1\n").await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: false }),
        ..Default::default()
    }));
    let output = Endpoint::new_memory("out_subscribe_no_delete", 10);
    let route = Route::new(input, output.clone());

    let handle = route.run("test_route_subscribe_no_delete").await.unwrap();

    let channel = output.channel().unwrap();
    let mut received = Vec::new();
    for _ in 0..20 {
        if !channel.is_empty() {
            received = channel.drain_messages();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(received.len(), 0);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "msg1\n");

    handle.stop().await;
}

#[tokio::test]
async fn test_route_file_consume_all_lines() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("consume_all.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write 10 lines
    let mut content = String::new();
    for i in 0..10 {
        content.push_str(&format!("msg{}\n", i));
    }
    tokio::fs::write(&file_path, content).await.unwrap();

    let input = Endpoint::new(EndpointType::File(FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..Default::default()
    }));
    let output = Endpoint::new_memory("out_consume_all", 100);
    let route = Route::new(input, output.clone());

    let handle = route.run("test_route_consume_all").await.unwrap();

    let channel = output.channel().unwrap();
    // Wait for messages
    let mut received_count = 0;
    for _ in 0..100 {
        received_count += channel.drain_messages().len();
        if received_count >= 10 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(received_count, 10);

    // Verify file is empty
    let mut content = String::new();
    for _ in 0..40 {
        content = tokio::fs::read_to_string(&file_path).await.unwrap();
        if content.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(content, "");

    handle.stop().await;
}

#[tokio::test]
async fn test_file_consumer_group_id_persistence() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("group_id.log");
    let file_path_str = file_path.to_str().unwrap().to_string();
    let offset_path = dir.path().join("group_id.log.my_group.offset");

    // Write initial content
    tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::GroupSubscribe {
            group_id: "my_group".to_string(),
            read_from_tail: false,
        }),
        ..Default::default()
    };

    let mut consumer1 = FileConsumer::new(&config).await.unwrap();
    // Allow thread to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let batch1 = consumer1.receive_batch(1).await.unwrap();
    assert_eq!(batch1.messages[0].payload.as_ref(), b"msg1");

    // Commit msg1 -> should write offset
    (batch1.commit)(vec![crate::traits::MessageDisposition::Ack])
        .await
        .unwrap();

    // Verify offset file exists and contains correct offset (length of "msg1\n" is 5)
    let offset_content = tokio::fs::read_to_string(&offset_path).await.unwrap();
    assert_eq!(offset_content, "5");

    drop(consumer1);

    // Second consumer (simulating restart) should start from offset 5 (msg2)
    let mut consumer2 = FileConsumer::new(&config).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let batch2 = consumer2.receive_batch(1).await.unwrap();
    assert_eq!(batch2.messages[0].payload.as_ref(), b"msg2");

    (batch2.commit)(vec![crate::traits::MessageDisposition::Ack])
        .await
        .unwrap();

    // Verify offset updated (5 + length of "msg2\n" (5) = 10)
    let offset_content = tokio::fs::read_to_string(&offset_path).await.unwrap();
    assert_eq!(offset_content, "10");
}

#[tokio::test]
async fn test_file_consumer_group_id_init_from_start() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("group_id_start.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Write initial content
    tokio::fs::write(&file_path, b"msg1\nmsg2\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::GroupSubscribe {
            group_id: "my_group_start".to_string(),
            read_from_tail: false,
        }),
        ..Default::default()
    };

    // Consumer should start from beginning (msg1)
    let mut consumer = FileConsumer::new(&config).await.unwrap();
    // Allow thread to start
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let batch = consumer.receive_batch(2).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    assert_eq!(batch.messages[0].payload.as_ref(), b"msg1");
    assert_eq!(batch.messages[1].payload.as_ref(), b"msg2");
}

#[tokio::test]
async fn test_file_tail_concurrent_publish_and_consume() {
    // This test verifies that the tail reader can work concurrently with the publisher
    // writing to the file. This is critical for Windows compatibility where file locking
    // semantics may prevent concurrent access if not handled correctly.
    // Note: Even though this runs in the same process, Windows file sharing modes are
    // enforced per-handle. Since the consumer does not participate in the `FILE_LOCKS`
    // mutex used by the publisher, this effectively tests that the OS allows the
    // publisher to open/write while the consumer has the file open for reading.
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("concurrent.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Create file with initial message
    tokio::fs::write(&file_path, b"msg0\n").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: false }),
        ..Default::default()
    };

    // Start the tail consumer
    let mut consumer = FileConsumer::new(&config).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Spawn a task that continuously publishes messages while the consumer is reading
    let publisher_path = file_path_str.clone();
    let publish_handle = tokio::spawn(async move {
        let pub_config = FileConfig {
            path: publisher_path,
            mode: Some(FileConsumerMode::Subscribe { delete: false }),
            ..Default::default()
        };
        let publisher = FilePublisher::new(&pub_config).await.unwrap();

        // Send enough messages to ensure overlap between reading and writing
        for i in 1..=100 {
            let msg = msg!(json!({"id": i, "data": format!("message_{}", i)}));
            publisher.send_batch(vec![msg]).await.unwrap();
            // Small delay to allow consumer to catch up and potentially open the file
            if i % 10 == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    });

    // Consumer should be able to read messages while publisher is writing
    let mut received_count = 0;
    let mut message_ids = Vec::new();

    // We expect 100 published messages (initial msg0 is skipped in Subscribe mode)
    let expected_count = 100;
    let start = std::time::Instant::now();

    while received_count < expected_count {
        if start.elapsed() > std::time::Duration::from_secs(10) {
            break;
        }
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            consumer.receive_batch(10),
        )
        .await
        {
            Ok(Ok(batch)) => {
                for msg in &batch.messages {
                    received_count += 1;
                    if let Ok(json_msg) = serde_json::from_slice::<serde_json::Value>(&msg.payload)
                    {
                        if let Some(id) = json_msg.get("id").and_then(|v| v.as_i64()) {
                            message_ids.push(id);
                        }
                    }
                }
                (batch.commit)(vec![
                    crate::traits::MessageDisposition::Ack;
                    batch.messages.len()
                ])
                .await
                .unwrap();
            }
            Ok(Err(_)) => break, // Stream ended
            Err(_) => continue,  // Timeout waiting for message
        }
    }

    publish_handle.await.unwrap();

    // Verify we received at least some messages from the publisher
    // We should receive the messages from the concurrent publisher
    assert_eq!(
        received_count, expected_count,
        "Expected {} messages, got {}. This may indicate file locking issues on this platform.",
        expected_count, received_count
    );

    // Verify the file still exists and can be read (not locked/deleted)
    let final_content = tokio::fs::read_to_string(&file_path)
        .await
        .expect("File should still be readable after concurrent access");
    assert!(
        !final_content.is_empty(),
        "File should contain messages after concurrent access"
    );
}

/// Simulates an external process (like a Python script or log writer) appending to the file.
/// Unlike `test_file_tail_concurrent_publish_and_consume`, this test:
/// 1. Does not use `FilePublisher` (bypassing internal `FILE_LOCKS`).
/// 2. Keeps the file handle open across multiple writes (simulating a long-running writer),
///    which stresses file locking/sharing semantics on OSs like Windows.
#[tokio::test]
async fn test_file_subscribe_concurrent_external_write() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("external_write.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    // Create empty file
    tokio::fs::write(&file_path, b"").await.unwrap();

    let config = FileConfig {
        path: file_path_str.clone(),
        mode: Some(FileConsumerMode::Subscribe { delete: false }),
        ..Default::default()
    };

    let mut consumer = FileConsumer::new(&config).await.unwrap();

    // Give the background tailer a moment to initialize
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let file_path_clone = file_path.clone();
    let write_task = tokio::spawn(async move {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&file_path_clone)
            .await
            .unwrap();

        for i in 0..5 {
            let line = format!("message {}\n", i);
            file.write_all(line.as_bytes()).await.unwrap();
            file.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    for i in 0..5 {
        let received = tokio::time::timeout(std::time::Duration::from_secs(5), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();

        let expected_payload = format!("message {}", i);
        assert_eq!(received.message.get_payload_str().trim(), expected_payload);
        (received.commit)(crate::traits::MessageDisposition::Ack)
            .await
            .unwrap();
    }

    write_task.await.unwrap();
}

#[tokio::test]
async fn test_file_custom_delimiter() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("custom_delim.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    let config = FileConfig {
        path: file_path_str.clone(),
        delimiter: Some("|".to_string()),
        format: FileFormat::Raw,
        mode: Some(FileConsumerMode::Consume { delete: false }),
        ..Default::default()
    };

    let publisher = FilePublisher::new(&config).await.unwrap();
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    let msg1 = crate::CanonicalMessage::from("msg1");
    let msg2 = crate::CanonicalMessage::from("msg2");

    publisher.send_batch(vec![msg1, msg2]).await.unwrap();
    publisher.flush().await.unwrap();
    drop(publisher); // Release lock

    // Verify file content has pipes
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "msg1|msg2|");

    let received1 = consumer.receive().await.unwrap();
    assert_eq!(received1.message.get_payload_str(), "msg1");

    let received2 = consumer.receive().await.unwrap();
    assert_eq!(received2.message.get_payload_str(), "msg2");
}

#[tokio::test]
async fn test_file_xml_delimiter() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("xml_delim.log");
    let file_path_str = file_path.to_str().unwrap().to_string();

    let config = FileConfig {
        path: file_path_str.clone(),
        delimiter: Some("</message>".to_string()),
        format: FileFormat::Raw,
        mode: Some(FileConsumerMode::Consume { delete: false }),
        ..Default::default()
    };

    let publisher = FilePublisher::new(&config).await.unwrap();
    let mut consumer = FileConsumer::new(&config).await.unwrap();

    let msg1 = crate::CanonicalMessage::from("<xml>content1");
    let msg2 = crate::CanonicalMessage::from("<xml>content2");

    publisher.send_batch(vec![msg1, msg2]).await.unwrap();
    publisher.flush().await.unwrap();
    drop(publisher); // Release lock

    // Verify file content has tags
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "<xml>content1</message><xml>content2</message>");

    let received1 = consumer.receive().await.unwrap();
    assert_eq!(received1.message.get_payload_str(), "<xml>content1");

    let received2 = consumer.receive().await.unwrap();
    assert_eq!(received2.message.get_payload_str(), "<xml>content2");
}

#[tokio::test]
async fn test_file_formats_and_fallbacks() {
    let dir = tempdir().unwrap();

    let json_path = dir.path().join("json.log");
    let json_config = FileConfig {
        path: json_path.to_str().unwrap().to_string(),
        format: FileFormat::Json,
        ..Default::default()
    };

    let json_publisher = FilePublisher::new(&json_config).await.unwrap();
    let mut json_consumer = FileConsumer::new(&json_config).await.unwrap();

    let json_payload = json!({"key": "value", "num": 123});
    let msg = msg!(json_payload.clone());

    json_publisher.send_batch(vec![msg.clone()]).await.unwrap();
    json_publisher.flush().await.unwrap();
    drop(json_publisher); // Release lock

    let received = json_consumer.receive().await.unwrap();
    let received_json: serde_json::Value =
        serde_json::from_slice(&received.message.payload).unwrap();
    assert_eq!(received_json, json_payload);
    (received.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    let text_path = dir.path().join("text.log");
    let text_config = FileConfig {
        path: text_path.to_str().unwrap().to_string(),
        format: FileFormat::Text,
        ..Default::default()
    };

    let text_publisher = FilePublisher::new(&text_config).await.unwrap();
    let mut text_consumer = FileConsumer::new(&text_config).await.unwrap();

    let text_payload = "Hello World";
    let msg = crate::CanonicalMessage::from(text_payload);

    text_publisher.send_batch(vec![msg.clone()]).await.unwrap();
    text_publisher.flush().await.unwrap();
    drop(text_publisher);

    let received = text_consumer.receive().await.unwrap();
    assert_eq!(received.message.get_payload_str(), text_payload);
    (received.commit)(crate::traits::MessageDisposition::Ack)
        .await
        .unwrap();

    // Test Fallback (Corrupted/Raw line in Json format)
    // We append a raw line that isn't the expected JSON wrapper structure
    {
        let mut file = OpenOptions::new()
            .append(true)
            .open(&json_path)
            .await
            .unwrap();
        file.write_all(b"Not a JSON wrapper\n").await.unwrap();
    }

    let received_fallback = json_consumer.receive().await.unwrap();
    // Should be treated as raw
    assert_eq!(
        received_fallback.message.get_payload_str(),
        "Not a JSON wrapper"
    );
    assert_eq!(
        received_fallback
            .message
            .metadata
            .get("mq_bridge.original_format")
            .map(|s| s.as_str()),
        Some("raw")
    );
}

#[tokio::test]
async fn test_file_csv_round_trip() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let file_path_str = file_path.to_str().unwrap().to_string();

    let config = FileConfig {
        path: file_path_str.clone(),
        format: FileFormat::Csv,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let msg1 = msg!(json!({"name": "alice", "age": "30"}));
    let msg2 = msg!(json!({"name": "bob", "age": "25"}));
    sink.send_batch(vec![msg1, msg2]).await.unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "age,name\n30,alice\n25,bob\n");

    let mut source = FileConsumer::new(&config).await.unwrap();
    let received1 = source.receive().await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&received1.message.payload).unwrap(),
        json!({"name": "alice", "age": "30"})
    );
    let received2 = source.receive().await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&received2.message.payload).unwrap(),
        json!({"name": "bob", "age": "25"})
    );
}

/// RFC 4180 lets a quoted field carry the record separator. Splitting the file on `\n`
/// before parsing turned such a row into two malformed ones — silent corruption for any
/// export with free-text notes or addresses.
#[tokio::test]
async fn test_file_csv_reads_a_newline_inside_a_quoted_field_as_one_record() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let file_path_str = file_path.to_str().unwrap().to_string();

    tokio::fs::write(
        &file_path,
        "id,name,note\n\
         1,Simple,plain\n\
         2,\"With, comma\",\"a \"\"quoted\"\" word\"\n\
         3,\"Line1\nLine2\",\n\
         4,héllo 世界,🎉\n",
    )
    .await
    .unwrap();

    let config = FileConfig {
        path: file_path_str,
        format: FileFormat::Csv,
        ..Default::default()
    };

    let mut source = FileConsumer::new(&config).await.unwrap();
    let mut rows = Vec::new();
    for _ in 0..4 {
        let received = source.receive().await.unwrap();
        rows.push(serde_json::from_slice::<serde_json::Value>(&received.message.payload).unwrap());
    }

    assert_eq!(
        rows,
        vec![
            json!({"id": "1", "name": "Simple", "note": "plain"}),
            json!({"id": "2", "name": "With, comma", "note": "a \"quoted\" word"}),
            json!({"id": "3", "name": "Line1\nLine2", "note": ""}),
            json!({"id": "4", "name": "héllo 世界", "note": "🎉"}),
        ]
    );
}

#[test]
fn test_csv_ends_inside_quotes_tracks_field_starts() {
    use super::csv_ends_inside_quotes;
    assert!(csv_ends_inside_quotes(b"3,\"Line1\n"));
    assert!(!csv_ends_inside_quotes(b"3,\"Line1\nLine2\",\n"));
    // Doubled quotes are an escape, not a close.
    assert!(csv_ends_inside_quotes(b"1,\"a \"\"b\n"));
    assert!(!csv_ends_inside_quotes(b"1,\"a \"\"b\"\n"));
    // A quote that does not start a field is literal data, matching `parse_csv_row`.
    assert!(!csv_ends_inside_quotes(b"1,in\"ch\n"));
}

/// The row decoder that shipped before the fused span parser: one `String` per field,
/// then a JSON object built from them. Kept here as the executable definition of the
/// behaviour the fast parser must reproduce byte for byte, quirks included.
mod csv_reference {
    fn parse_row(line: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_quotes {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        cur.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    cur.push(c);
                }
            } else if c == '"' && cur.is_empty() {
                in_quotes = true;
            } else if c == ',' {
                fields.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        }
        fields.push(cur);
        fields
    }

    fn escape(buf: &mut String, s: &str) {
        for c in s.chars() {
            match c {
                '"' => buf.push_str("\\\""),
                '\\' => buf.push_str("\\\\"),
                '\n' => buf.push_str("\\n"),
                '\r' => buf.push_str("\\r"),
                '\t' => buf.push_str("\\t"),
                '\u{08}' => buf.push_str("\\b"),
                '\u{0C}' => buf.push_str("\\f"),
                c if (c as u32) < 0x20 => {
                    use std::fmt::Write;
                    let _ = write!(buf, "\\u{:04x}", c as u32);
                }
                c => buf.push(c),
            }
        }
    }

    pub(super) fn encode(header: &[u8], record: &[u8]) -> Vec<u8> {
        let cols = parse_row(&String::from_utf8_lossy(header));
        let fields = parse_row(&String::from_utf8_lossy(record));
        let mut out = String::new();
        out.push('{');
        for (i, col) in cols.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('"');
            escape(&mut out, col);
            out.push_str("\":\"");
            escape(&mut out, fields.get(i).map_or("", |s| s.as_str()));
            out.push('"');
        }
        out.push('}');
        out.into_bytes()
    }
}

/// Decodes one header + one data record through the production path.
fn csv_decode(header: &[u8], record: &[u8]) -> Vec<u8> {
    use crate::endpoints::file::parse_message;
    let mut state = None;
    assert!(
        parse_message(header, &FileFormat::Csv, &mut state).is_none(),
        "the header record establishes columns and yields no message"
    );
    parse_message(record, &FileFormat::Csv, &mut state)
        .expect("a data record always yields a message")
        .payload
        .to_vec()
}

/// Every quirk of the old row decoder, pinned so the fast parser cannot quietly
/// reinterpret a real file: quotes that open only on an empty field, doubled quotes,
/// delimiters and newlines inside quotes, control characters, and multi-byte UTF-8.
#[test]
fn csv_fast_parser_matches_the_reference_byte_for_byte() {
    let header = b"a,b,c".as_slice();
    let records: &[&[u8]] = &[
        b"1,2,3",
        b"",
        b",,",
        b"1,2",
        b"1,2,3,4,5",
        // A quote opens a section only while the field is empty.
        b"1,in\"ch,3",
        b"1,\"a\"x,3",
        b"1,\"\"abc,3",
        // Doubled quotes are an escape, not a close.
        b"1,\"a \"\"quoted\"\" word\",3",
        b"1,\"\"\"\",3",
        // Delimiters and newlines survive inside a quoted field.
        b"1,\"With, comma\",3",
        b"1,\"Line1\nLine2\",",
        // Characters JSON has to escape.
        b"1,back\\slash,tab\there",
        b"1,\x01\x1f,3",
        b"1,\"quote\"\"and\\slash\",3",
        // Multi-byte UTF-8 must pass through untouched.
        "1,héllo 世界,🎉".as_bytes(),
        // Invalid UTF-8 is replaced, not rejected.
        b"1,\xff\xfe,3",
    ];

    for record in records {
        assert_eq!(
            csv_decode(header, record),
            csv_reference::encode(header, record),
            "record {:?} decoded differently",
            String::from_utf8_lossy(record)
        );
    }
}

/// Headers get the same treatment as values, including names that need JSON escaping.
#[test]
fn csv_fast_parser_matches_the_reference_for_awkward_headers() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"\"a,b\",c", b"1,2"),
        (b"a\"b,c", b"1,2"),
        (b"\"quote\"\"name\",c", b"1,2"),
        (b"back\\slash,c", b"1,2"),
        ("héllo,世界".as_bytes(), "1,2".as_bytes()),
        (b"a", b"1,2,3"),
        (b"", b"1"),
    ];

    for (header, record) in cases {
        assert_eq!(
            csv_decode(header, record),
            csv_reference::encode(header, record),
            "header {:?} decoded differently",
            String::from_utf8_lossy(header)
        );
    }
}

proptest::proptest! {
    /// Random records, including ones no CSV writer would produce, must decode
    /// identically to the reference.
    #[test]
    fn csv_fast_parser_matches_the_reference_on_arbitrary_records(
        header in "[a-c\",\\\\ ]{0,12}",
        record in "[a-c0-9\",\\\\\\n\\t ]{0,40}",
    ) {
        proptest::prop_assert_eq!(
            csv_decode(header.as_bytes(), record.as_bytes()),
            csv_reference::encode(header.as_bytes(), record.as_bytes())
        );
    }
}

#[tokio::test]
async fn test_file_csv_value_types_and_escaping() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![
        msg!(json!({
            "a_num": 42,
            "b_float": 1234.56,
            "c_bool": true,
            "d_null": null,
            "e_quoted": "say \"hi\", ok",
            "f_nested": {"x": 1},
            "g_empty": ""
        })),
        msg!(json!({
            "a_num": -7,
            "b_float": 0.5,
            "c_bool": false,
            "d_null": null,
            "e_quoted": "line\nbreak",
            "f_nested": [1, 2],
            "g_empty": "plain"
        })),
    ])
    .await
    .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(
        content,
        "a_num,b_float,c_bool,d_null,e_quoted,f_nested,g_empty\n\
         42,1234.56,true,null,\"say \"\"hi\"\", ok\",\"{\"\"x\"\":1}\",\n\
         -7,0.5,false,null,\"line\nbreak\",\"[1,2]\",plain\n"
    );
}

/// Keys containing JSON escapes cannot be borrowed from the payload, so they
/// take the parsed-`Value` fallback path; the output must be identical.
#[tokio::test]
async fn test_file_csv_escaped_keys_fallback() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"we\"ird": 1, "plain": "x"}))])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content, "plain,\"we\"\"ird\"\nx,1\n");
}

#[tokio::test]
async fn test_file_csv_rejects_non_object_payload() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let result = sink.send_batch(vec![msg!(json!([1, 2, 3]))]).await.unwrap();
    match result {
        crate::outcomes::SentBatch::Partial { failed, .. } => assert_eq!(failed.len(), 1),
        other => panic!("expected Partial, got {other:?}"),
    }
}

#[tokio::test]
async fn test_file_csv_rejects_empty_object_payload() {
    // An empty object carries no columns; if it established the header, every later row
    // in the file would be written against an empty column set.
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let result = sink
        .send_batch(vec![msg!(json!({})), msg!(json!({"a": 1, "b": 2}))])
        .await
        .unwrap();
    match result {
        crate::outcomes::SentBatch::Partial { failed, .. } => assert_eq!(failed.len(), 1),
        other => panic!("expected Partial, got {other:?}"),
    }

    // The surviving message still got a real header and row.
    let content = tokio::fs::read_to_string(&file_path).await.unwrap();
    assert_eq!(content.trim_end(), "a,b\n1,2");
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_csv_compressed_roundtrip() {
    // The header row goes into the first member only, so the decompressed stream is a
    // plain CSV file even though it was written as two gzip members.
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv.gz").to_str().unwrap().to_string();
    let config = FileConfig {
        path: path.clone(),
        format: FileFormat::Csv,
        compression: Compression::Gzip,
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![
        msg!(json!({"name": "alice", "age": "30"})),
        msg!(json!({"name": "bob", "age": "25"})),
    ])
    .await
    .unwrap();
    sink.send_batch(vec![msg!(json!({"name": "carol", "age": "41"}))])
        .await
        .unwrap();
    drop(sink);

    let raw = std::fs::read(&path).unwrap();
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::MultiGzDecoder::new(&raw[..]),
        &mut decoded,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        "age,name\n30,alice\n25,bob\n41,carol\n"
    );

    let mut source = FileConsumer::new(&config).await.unwrap();
    let got = collect_compressed(&mut source, 3).await;
    let rows: Vec<serde_json::Value> = got
        .iter()
        .map(|p| serde_json::from_slice(p).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![
            json!({"age": "30", "name": "alice"}),
            json!({"age": "25", "name": "bob"}),
            json!({"age": "41", "name": "carol"}),
        ]
    );
}

#[cfg(feature = "encryption")]
#[tokio::test]
async fn test_file_csv_encrypted_roundtrip() {
    use base64::Engine as _;

    let dir = tempdir().unwrap();
    let path = dir
        .path()
        .join("data.csv.enc")
        .to_str()
        .unwrap()
        .to_string();
    let config = FileConfig {
        path: path.clone(),
        format: FileFormat::Csv,
        encryption: Some(crate::models::EncryptionConfig {
            key: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
            ..Default::default()
        }),
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"name": "alice", "age": "30"}))])
        .await
        .unwrap();
    sink.send_batch(vec![msg!(json!({"name": "bob", "age": "25"}))])
        .await
        .unwrap();
    drop(sink);

    // The rows and the header are ciphertext on disk.
    let raw = std::fs::read(&path).unwrap();
    assert!(!raw.windows(5).any(|w| w == b"alice"));
    assert!(!raw.windows(4).any(|w| w == b"name"));

    let mut source = FileConsumer::new(&config).await.unwrap();
    let got = collect_compressed(&mut source, 2).await;
    let rows: Vec<serde_json::Value> = got
        .iter()
        .map(|p| serde_json::from_slice(p).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![
            json!({"age": "30", "name": "alice"}),
            json!({"age": "25", "name": "bob"}),
        ]
    );
}

#[tokio::test]
async fn test_file_normal_format_preserves_id_of_raw_origin_message() {
    // The sink's format decides the encoding, not the message's origin: a message that
    // came from a `raw` file (or any endpoint that marks it raw) keeps its id and
    // metadata when written to a `normal` file, so the id survives every hop.
    let dir = tempdir().unwrap();
    let path = dir.path().join("out.log").to_str().unwrap().to_string();
    let config = FileConfig {
        path: path.clone(),
        ..Default::default()
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    let msg = crate::CanonicalMessage::from("hello")
        .with_raw_format()
        .with_metadata_kv("kind", "greeting");
    let id = msg.message_id;
    sink.send_batch(vec![msg]).await.unwrap();
    drop(sink);

    let mut source = FileConsumer::new(&config).await.unwrap();
    let received = source.receive().await.unwrap().message;
    assert_eq!(received.message_id, id);
    assert_eq!(received.get_payload_str(), "hello");
    assert_eq!(
        received.metadata.get("kind").map(String::as_str),
        Some("greeting")
    );
}

#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_csv_compressed_restart_writes_no_second_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("d.csv.gz").to_str().unwrap().to_string();
    let config = FileConfig {
        path: path.clone(),
        format: FileFormat::Csv,
        compression: Compression::Gzip,
        ..Default::default()
    };
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"name": "alice", "age": "30"}))])
        .await
        .unwrap();
    drop(sink);
    // Fresh publisher (process restart): the header must not be written again.
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"name": "bob", "age": "25"}))])
        .await
        .unwrap();
    drop(sink);

    let raw = std::fs::read(&path).unwrap();
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(
        &mut flate2::read::MultiGzDecoder::new(&raw[..]),
        &mut decoded,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        "age,name\n30,alice\n25,bob\n"
    );
}

/// `normal`/`text` decode the payload in one pass (see `RawPayload`), with a
/// fallback to JSON text for anything that is not a byte array. Every shape a
/// payload can take is pinned here, because the fast path and the fallback have
/// to agree with what the previous `serde_json::Value` decode produced.
#[test]
fn test_parse_message_payload_shapes() {
    use crate::endpoints::file::parse_message;

    let line = |payload: &str| {
        format!(r#"{{"message_id":"019f9b12-d786-7ebe-a7ec-a1aa71bc47ae","payload":{payload}}}"#)
            .into_bytes()
    };
    let decoded = |payload: &str, format: FileFormat| -> Vec<u8> {
        let mut header = None;
        parse_message(&line(payload), &format, &mut header)
            .expect("line decodes")
            .payload
            .to_vec()
    };

    // Byte arrays — the fast path — become the bytes themselves.
    assert_eq!(decoded("[104,105]", FileFormat::Normal), b"hi");
    assert_eq!(decoded("[]", FileFormat::Normal), b"");
    assert_eq!(decoded("[0,255]", FileFormat::Normal), vec![0u8, 255]);
    // A string payload is taken verbatim.
    assert_eq!(decoded(r#""hi""#, FileFormat::Normal), b"hi");
    assert_eq!(decoded(r#""hi""#, FileFormat::Text), b"hi");

    // Anything that is not a byte array falls back to its JSON text, including
    // arrays that only stop being byte-like partway through.
    assert_eq!(decoded("[1,2,300]", FileFormat::Normal), b"[1,2,300]");
    assert_eq!(decoded("[1,-2]", FileFormat::Normal), b"[1,-2]");
    assert_eq!(decoded("[1.5]", FileFormat::Normal), b"[1.5]");
    assert_eq!(decoded(r#"[1,"a"]"#, FileFormat::Normal), br#"[1,"a"]"#);
    assert_eq!(decoded("[[1],2]", FileFormat::Normal), b"[[1],2]");
    assert_eq!(decoded("[null]", FileFormat::Normal), b"[null]");
    assert_eq!(decoded(r#"{"a":1}"#, FileFormat::Normal), br#"{"a":1}"#);
    assert_eq!(decoded("5", FileFormat::Normal), b"5");
    assert_eq!(decoded("true", FileFormat::Normal), b"true");
    assert_eq!(decoded("null", FileFormat::Normal), b"null");

    // `json` keeps the payload as a JSON value, so a byte array stays an array.
    assert_eq!(decoded("[104,105]", FileFormat::Json), b"[104,105]");

    // message_id and metadata survive the fast path.
    let mut header = None;
    let msg = parse_message(
        br#"{"message_id":"019f9b12-d786-7ebe-a7ec-a1aa71bc47ae","payload":[104,105],"metadata":{"k":"v"}}"#,
        &FileFormat::Normal,
        &mut header,
    )
    .expect("line decodes");
    assert_eq!(msg.payload.to_vec(), b"hi");
    assert_eq!(msg.metadata.get("k").map(String::as_str), Some("v"));
    assert_eq!(
        format!("{:032x}", msg.message_id),
        "019f9b12d7867ebea7eca1aa71bc47ae"
    );

    // A line that is not the promised envelope is kept verbatim and marked.
    let mut header = None;
    let msg =
        parse_message(b"not json at all", &FileFormat::Normal, &mut header).expect("line decodes");
    assert_eq!(msg.payload.to_vec(), b"not json at all");
    assert_eq!(
        msg.metadata
            .get("mq_bridge.original_format")
            .map(String::as_str),
        Some("raw")
    );
}

/// `json` copies the payload's own bytes out of the line, so everything a
/// `serde_json::Value` round trip would quietly normalise stays put.
#[test]
fn test_json_format_payload_is_copied_verbatim() {
    use crate::endpoints::file::parse_message;

    let decoded = |payload: &str| -> Vec<u8> {
        let line = format!(
            r#"{{"message_id":"019f9b12-d786-7ebe-a7ec-a1aa71bc47ae","payload":{payload}}}"#
        );
        let mut header = None;
        parse_message(line.as_bytes(), &FileFormat::Json, &mut header)
            .expect("line decodes")
            .payload
            .to_vec()
    };

    // Key order is the producer's, not alphabetical: without `preserve_order`
    // a `Value` is a `BTreeMap` and would have re-sorted these.
    assert_eq!(decoded(r#"{"b":1,"a":2}"#), br#"{"b":1,"a":2}"#);
    // Numbers keep their source spelling, so a double cannot shift a ULP on the
    // way through regardless of the `float-roundtrip` feature.
    assert_eq!(decoded("1e3"), b"1e3");
    assert_eq!(decoded("2.5000"), b"2.5000");
    assert_eq!(decoded("0.1234567890123456789"), b"0.1234567890123456789");
    // Interior spacing is part of those bytes too.
    assert_eq!(decoded(r#"{"a": 1}"#), br#"{"a": 1}"#);
    // Scalars and nulls are unchanged from the `Value` path.
    assert_eq!(decoded("null"), b"null");
    assert_eq!(decoded("true"), b"true");
    assert_eq!(decoded(r#""hi""#), br#""hi""#);
    // A lone surrogate is legal JSON text but not a legal Rust `String`, so the
    // `Value` gate used to reject the whole line and discard its metadata.
    assert_eq!(decoded(r#""\ud800""#), br#""\ud800""#);
}

/// The `json` sink writes the payload's own bytes into the wrapper for the same
/// reason the source reads them out of it, so a round trip changes nothing.
#[test]
fn test_json_format_round_trip_preserves_payload_bytes() {
    use crate::endpoints::file::{encode_record, parse_message};
    use crate::CanonicalMessage;

    for payload in [
        r#"{"b":1,"a":2}"#,
        r#"{"z":{"y":1e3,"x":2.5000}}"#,
        r#"{"a": 1}"#,
        r#""\ud800""#,
        "0.1234567890123456789",
    ] {
        let mut msg = CanonicalMessage::new(payload.as_bytes().to_vec(), None);
        msg.metadata.insert("k".to_string(), "v".to_string());
        let line = encode_record(&msg, &FileFormat::Json).expect("record encodes");

        let mut header = None;
        let back = parse_message(&line, &FileFormat::Json, &mut header).expect("line decodes");
        assert_eq!(
            String::from_utf8_lossy(&back.payload),
            payload,
            "payload changed on the way through"
        );
        assert_eq!(back.metadata.get("k").map(String::as_str), Some("v"));
        assert_eq!(back.message_id, msg.message_id);
    }
}

/// Reads batches until the consumer surfaces an empty (drain) batch, returning
/// the total record count. Fails if no batch arrives within the timeout.
async fn drain_count(source: &mut FileConsumer) -> usize {
    use crate::traits::MessageDisposition;
    let mut count = 0;
    loop {
        let batch =
            tokio::time::timeout(std::time::Duration::from_secs(5), source.receive_batch(64))
                .await
                .expect("timed out reading file")
                .expect("receive_batch errored");
        if batch.messages.is_empty() {
            return count;
        }
        let n = batch.messages.len();
        count += n;
        (batch.commit)(vec![MessageDisposition::Ack; n])
            .await
            .unwrap();
    }
}

fn no_trailing_newline_body(n: usize) -> Vec<u8> {
    // `n` records joined by `n - 1` newlines: the final record has no trailing '\n'.
    (0..n)
        .map(|i| format!("{{\"i\":{i}}}"))
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

// Issue 4 (regression): draining a complete file whose last record has no trailing
// newline must deliver that record. Before the fix the tail reader treated it as a
// torn mid-write and dropped it (200 records -> only 199 delivered).
#[tokio::test]
async fn test_file_tail_drain_emits_final_line_without_newline() {
    const N: usize = 200;
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("no_trailing_newline.jsonl");
    tokio::fs::write(&file_path, no_trailing_newline_body(N))
        .await
        .unwrap();

    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Raw,
        ..Default::default()
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    // Drain mode: a final record with no delimiter is a whole record.
    source.set_exit_on_empty(true);

    assert_eq!(
        drain_count(&mut source).await,
        N,
        "drain must deliver the final newline-less record"
    );
}

// Complement: in live-tail mode (no drain intent) the final record without a
// delimiter is withheld as a possible torn write, and no EOF marker is emitted while
// it is pending — so the consumer delivers N-1 records and then blocks for more data.
#[tokio::test]
async fn test_file_tail_live_withholds_final_line_without_newline() {
    const N: usize = 200;
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("no_trailing_newline.jsonl");
    tokio::fs::write(&file_path, no_trailing_newline_body(N))
        .await
        .unwrap();

    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Raw,
        ..Default::default()
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    // No set_exit_on_empty(true): live tail withholds the torn final record.

    let mut count = 0;
    // Loops until the timeout elapses: blocked waiting for the writer to finish the final record.
    while let Ok(batch) = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        source.receive_batch(64),
    )
    .await
    {
        let batch = batch.expect("receive_batch errored");
        // A partial final record is pending, so no empty marker is emitted;
        // every batch that arrives carries data.
        assert!(
            !batch.messages.is_empty(),
            "unexpected empty marker in live tail"
        );
        count += batch.messages.len();
    }
    assert_eq!(
        count,
        N - 1,
        "live tail withholds the final record until its delimiter arrives"
    );
}

/// A source path that cannot be opened must end a drain with a permanent error,
/// not block forever (missing file, missing parent directory, unreadable file).
#[tokio::test]
async fn drain_fails_on_unopenable_source_path() {
    let dir = tempfile::tempdir().unwrap();
    let unreadable = dir.path().join("locked.jsonl");
    std::fs::write(&unreadable, b"{\"a\":1}\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    let mut paths = vec![
        dir.path().join("missing.jsonl").display().to_string(),
        dir.path()
            .join("no-such-dir/in.jsonl")
            .display()
            .to_string(),
    ];
    // A mode-000 file is still readable by root; only assert on it where the
    // permission actually bites.
    if std::fs::File::open(&unreadable).is_err() {
        paths.push(unreadable.display().to_string());
    }

    for path in paths {
        let mut source = FileConsumer::new(&FileConfig {
            path: path.clone(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("construction should succeed for {path}: {e}"));
        source.set_exit_on_empty(true);

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), source.receive_batch(16))
                .await
                .unwrap_or_else(|_| panic!("receive_batch hung on {path}"));

        match result {
            Err(crate::errors::ConsumerError::Permanent(e)) => {
                assert!(
                    e.to_string().contains(&path),
                    "error should name the path: {e}"
                );
            }
            other => panic!("expected a permanent error for {path}, got {other:?}"),
        }
    }
}

/// A directory given as a file source is permanent nonsense: reject it at
/// construction rather than reporting a clean, empty drain.
#[tokio::test]
async fn directory_as_source_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let err = match FileConsumer::new(&FileConfig {
        path: dir.path().display().to_string(),
        ..Default::default()
    })
    .await
    {
        Ok(_) => panic!("a directory is not a readable file source"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("is a directory"), "got: {err}");
}

/// A live tail (no drain) still waits for a file that does not exist yet.
#[tokio::test]
async fn live_tail_waits_for_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("later.jsonl");
    let mut source = FileConsumer::new(&FileConfig {
        path: path.display().to_string(),
        ..Default::default()
    })
    .await
    .unwrap();

    let writer = path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::fs::write(&writer, b"{\"a\":1}\n").unwrap();
    });

    let batch = tokio::time::timeout(std::time::Duration::from_secs(5), source.receive_batch(16))
        .await
        .expect("live tail should pick the file up once it appears")
        .expect("receive_batch errored");
    assert_eq!(batch.messages.len(), 1);
}

#[cfg(all(feature = "encryption", feature = "compression"))]
mod at_rest_codec_mismatch {
    use super::*;
    use crate::models::EncryptionConfig;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    fn encryption() -> Option<EncryptionConfig> {
        Some(EncryptionConfig {
            key: KEY.to_string(),
            ..Default::default()
        })
    }

    async fn write_encrypted(path: &str, compression: Compression) {
        let publisher = FilePublisher::new(&FileConfig {
            path: path.to_string(),
            compression,
            encryption: encryption(),
            ..Default::default()
        })
        .await
        .unwrap();
        publisher
            .send_batch(vec![msg!(json!({"a": 1})), msg!(json!({"a": 2}))])
            .await
            .unwrap();
    }

    /// An encrypted file read with no `encryption` configured used to emit its
    /// ciphertext as messages under a clean success.
    #[tokio::test]
    async fn encrypted_source_without_encryption_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("enc.jsonl").display().to_string();
        write_encrypted(&path, Compression::Gzip).await;

        let err = match FileConsumer::new(&FileConfig {
            path: path.clone(),
            ..Default::default()
        })
        .await
        {
            Ok(_) => panic!("an encrypted file must not be read as plaintext"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("looks encrypted"), "got: {err}");
    }

    /// Decryption succeeds whatever the inner codec is, so a missing
    /// `compression` used to surface the compressed bytes as one message.
    #[tokio::test]
    async fn decrypted_compression_mismatch_is_permanent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("enc-gzip.jsonl").display().to_string();
        write_encrypted(&path, Compression::Gzip).await;

        let mut source = FileConsumer::new(&FileConfig {
            path: path.clone(),
            encryption: encryption(),
            ..Default::default()
        })
        .await
        .unwrap();
        source.set_exit_on_empty(true);

        let mut last = None;
        for _ in 0..20 {
            let received =
                tokio::time::timeout(std::time::Duration::from_secs(10), source.receive_batch(16))
                    .await
                    .expect("receive_batch timed out");
            match received {
                Err(crate::errors::ConsumerError::Permanent(e)) => {
                    last = Some(e.to_string());
                    break;
                }
                Ok(batch) => assert!(
                    batch.messages.is_empty(),
                    "gzip bytes must not be emitted as messages"
                ),
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        let err = last.expect("expected a permanent decode error");
        assert!(err.contains("Giving up decoding"), "got: {err}");
    }

    /// The matching configuration still round-trips.
    #[tokio::test]
    async fn encrypted_and_compressed_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.jsonl").display().to_string();
        write_encrypted(&path, Compression::Gzip).await;

        let mut source = FileConsumer::new(&FileConfig {
            path: path.clone(),
            compression: Compression::Gzip,
            encryption: encryption(),
            ..Default::default()
        })
        .await
        .unwrap();
        source.set_exit_on_empty(true);
        let batch =
            tokio::time::timeout(std::time::Duration::from_secs(10), source.receive_batch(16))
                .await
                .expect("receive_batch timed out")
                .unwrap();
        assert_eq!(batch.messages.len(), 2);
    }
}

/// A payload that is neither JSON nor UTF-8 — what the `compression` and `encryption`
/// middlewares produce — must survive a `json`/`text` sink. It used to come back as the
/// *textual* byte array `[40,181,47,…]`, so the reader's first byte was `[` (91).
#[test]
fn binary_payload_round_trips_through_json_and_text_formats() {
    use crate::endpoints::file::{encode_record, parse_message};

    let payload = vec![0x28u8, 0xb5, 0x2f, 0xfd, 0x00, 0xff, 0xfe];
    let msg = crate::CanonicalMessage::new(payload.clone(), Some(7));

    for format in [FileFormat::Json, FileFormat::Text] {
        let line = encode_record(&msg, &format).unwrap();
        let parsed = parse_message(&line, &format, &mut None).expect("record must parse");
        assert_eq!(
            parsed.payload.as_ref(),
            payload.as_slice(),
            "{format:?} must preserve a binary payload verbatim"
        );
        assert!(
            !parsed.metadata.contains_key("mq_bridge.payload_bytes"),
            "the byte marker is a storage detail and must not leak downstream"
        );
    }
}

/// The marker is honoured only at the value this crate writes, and only when the payload
/// really is a byte array — a producer's own key of that name must not redirect decoding.
#[test]
fn byte_payload_marker_is_only_honoured_when_it_is_ours() {
    use crate::endpoints::file::parse_message;

    // A marked *string* payload is not ours: it stays the JSON text it was.
    let line =
        br#"{"message_id":"1","payload":"hello","metadata":{"mq_bridge.payload_bytes":"1"}}"#;
    let parsed = parse_message(line, &FileFormat::Json, &mut None).unwrap();
    assert_eq!(parsed.payload.as_ref(), br#""hello""#);

    // A foreign value under the same key is the producer's data and survives untouched.
    let line =
        br#"{"message_id":"2","payload":[1,2,3],"metadata":{"mq_bridge.payload_bytes":"theirs"}}"#;
    let parsed = parse_message(line, &FileFormat::Json, &mut None).unwrap();
    assert_eq!(parsed.payload.as_ref(), b"[1,2,3]");
    assert_eq!(
        parsed
            .metadata
            .get("mq_bridge.payload_bytes")
            .map(String::as_str),
        Some("theirs")
    );
}

/// A binary payload still round-trips when the message already carries the reserved key.
/// `mq_bridge.*` is the crate's namespace — as with `mq_bridge.dlq.*` and
/// `mq_bridge.retry.attempt`, a value a producer puts there is ours to overwrite.
#[test]
fn pre_existing_marker_does_not_break_a_binary_round_trip() {
    use crate::endpoints::file::{encode_record, parse_message};

    let payload = vec![0x28u8, 0xb5, 0x2f, 0xfd, 0x00];
    let mut msg = crate::CanonicalMessage::new(payload.clone(), Some(9));
    msg.metadata
        .insert("mq_bridge.payload_bytes".to_string(), "theirs".to_string());

    let line = encode_record(&msg, &FileFormat::Json).unwrap();
    let parsed = parse_message(&line, &FileFormat::Json, &mut None).unwrap();
    assert_eq!(parsed.payload.as_ref(), payload.as_slice());
}

/// The reader decodes a batch across cores; that must be invisible. At every size around
/// the split threshold the parallel decode has to match a plain sequential one, record
/// for record, including the header row it swallows and the offsets it stamps.
#[test]
fn parallel_record_decode_matches_a_sequential_one() {
    use crate::endpoints::file::{decode_records, parse_message, CsvHeader, RecordSpan};
    use std::sync::Arc;

    let header = b"id,name,amount,note".to_vec();
    let row = |i: usize| format!(r#"{i},"a,b {i}",{i}.5,"say ""hi"" {i}""#).into_bytes();

    for count in [0, 1, 2, 63, 64, 65, 127, 1024] {
        for with_header in [true, false] {
            let mut buf: Vec<u8> = Vec::new();
            let mut spans: Vec<RecordSpan> = Vec::new();
            let mut records: Vec<Vec<u8>> = Vec::new();
            if with_header {
                records.push(header.clone());
            }
            records.extend((0..count).map(row));
            for (i, record) in records.iter().enumerate() {
                let start = buf.len();
                buf.extend_from_slice(record);
                spans.push((start, buf.len(), i as u64 + 1));
            }

            let mut state = (!with_header).then(|| Arc::new(CsvHeader::parse(&header)));
            let actual = decode_records(&mut buf, &spans, &FileFormat::Csv, &mut state, true);
            // The batch buffer is lent to the workers and must come back intact, or the
            // reader silently reallocates it every batch.
            assert_eq!(
                buf.len(),
                records.iter().map(Vec::len).sum::<usize>(),
                "{count} records, header={with_header}: buffer not handed back"
            );

            let mut expected_state = (!with_header).then(|| CsvHeader::parse(&header));
            let expected: Vec<_> = spans
                .iter()
                .filter_map(|&(start, end, position)| {
                    let mut msg =
                        parse_message(&buf[start..end], &FileFormat::Csv, &mut expected_state)?;
                    msg.metadata
                        .insert("file_offset".to_string(), position.to_string());
                    Some(msg)
                })
                .collect();

            assert_eq!(
                actual.len(),
                expected.len(),
                "{count} records, header={with_header}: wrong count"
            );
            for (got, want) in actual.iter().zip(&expected) {
                assert_eq!(got.payload, want.payload, "payload differs");
                assert_eq!(
                    got.metadata.get("file_offset"),
                    want.metadata.get("file_offset"),
                    "offset differs"
                );
            }
        }
    }
}
