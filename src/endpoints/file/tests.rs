use crate::endpoints::file::{FileConsumer, FilePublisher};
#[allow(unused_imports)]
use crate::models::{
    Compression, CsvConfig, CsvNested, FileConfig, FileConsumerMode, FileFormat, NameBy,
};
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
    assert_eq!(offset_content.parse::<u64>().unwrap(), 5);

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
    assert_eq!(offset_content.parse::<u64>().unwrap(), 10);
}

#[tokio::test]
async fn test_file_group_offset_stops_before_a_nack() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("group_nack.log");
    let offset_path = dir.path().join("group_nack.log.g.offset");
    tokio::fs::write(&file_path, b"msg1\nmsg2\nmsg3\n")
        .await
        .unwrap();

    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        mode: Some(FileConsumerMode::GroupSubscribe {
            group_id: "g".to_string(),
            read_from_tail: false,
        }),
        ..Default::default()
    };
    let mut consumer = FileConsumer::new(&config).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut messages = Vec::new();
    let mut commits = Vec::new();
    while messages.len() < 3 {
        let batch = consumer.receive_batch(3 - messages.len()).await.unwrap();
        messages.extend(batch.messages);
        commits.push(batch.commit);
    }
    assert_eq!(
        commits.len(),
        1,
        "the three lines should arrive as one batch"
    );

    use crate::traits::MessageDisposition::{Ack, Nack};
    (commits.remove(0))(vec![Ack, Nack, Ack]).await.unwrap();

    let stored = tokio::fs::read_to_string(&offset_path).await.unwrap();
    assert_eq!(stored.parse::<u64>().unwrap(), 5);
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
    assert_eq!(content, "name,age\nalice,30\nbob,25\n");

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
    // A leading BOM does not stop the quote after it from opening the field.
    assert!(csv_ends_inside_quotes(b"\xef\xbb\xbf\"i\n"));
}

/// The quote scan jumps between quotes; it must land in the same state as the plain
/// byte-at-a-time walk, wherever the input is split.
#[test]
fn test_csv_quote_state_matches_bytewise_walk() {
    use super::CsvQuoteState;

    // (in_quotes, field_is_empty, pending_quote) after a byte-wise walk.
    fn walk(state: (bool, bool, bool), bytes: &[u8]) -> (bool, bool, bool) {
        let (mut in_quotes, mut field_is_empty, mut pending_quote) = state;
        for &b in bytes {
            if pending_quote {
                pending_quote = false;
                if b == b'"' {
                    continue;
                }
                in_quotes = false;
            }
            if in_quotes {
                pending_quote = b == b'"';
            } else if b == b'"' && field_is_empty {
                in_quotes = true;
            } else {
                field_is_empty = b == b',';
            }
        }
        (in_quotes, field_is_empty, pending_quote)
    }

    // Short tokens exercise every transition; the long run crosses the scalar probe.
    let tokens: [&[u8]; 5] = [b"\"", b",", b"a", b"\n", &[b'x'; 20]];
    let mut inputs: Vec<Vec<u8>> = vec![Vec::new()];
    for _ in 0..6 {
        inputs = inputs
            .iter()
            .flat_map(|prefix| tokens.iter().map(move |t| [prefix.as_slice(), t].concat()))
            .collect();
        for input in &inputs {
            for split in 0..=input.len() {
                let (head, tail) = input.split_at(split);
                let mut state = CsvQuoteState::default();
                state.feed(head);
                state.feed(tail);
                let expected = walk(walk((false, true, false), head), tail);
                assert_eq!(
                    (state.in_quotes, state.field_is_empty, state.pending_quote),
                    expected,
                    "input {:?} split at {split}",
                    String::from_utf8_lossy(input)
                );
            }
        }
    }
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
        let mut cols = parse_row(&String::from_utf8_lossy(header));
        let original: std::collections::HashSet<String> = cols.iter().cloned().collect();
        let mut used = std::collections::HashSet::new();
        for col in &mut cols {
            if !used.insert(col.clone()) {
                *col = (2..)
                    .map(|n| format!("{col}_{n}"))
                    .find(|c| !original.contains(c) && !used.contains(c))
                    .unwrap();
                used.insert(col.clone());
            }
        }
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
        header in r#"[a-c",\\ \r\x01é🎉]{0,12}"#,
        record in r#"[a-c0-9",\\\n\r\t \x00\x1f\x7fé世🎉]{0,40}"#,
    ) {
        // Blank records are skipped rather than decoded, unlike the reference.
        proptest::prop_assume!(!header.is_empty() && !record.is_empty());
        proptest::prop_assert_eq!(
            csv_decode(header.as_bytes(), record.as_bytes()),
            csv_reference::encode(header.as_bytes(), record.as_bytes())
        );
    }
}

/// Quoting is decided per field: exactly the characters that would change how the
/// reader frames or splits the record trigger it, and nothing else is touched.
#[test]
fn csv_append_field_quotes_exactly_when_needed() {
    use super::csv_append_field;
    let cases: &[(&str, &[u8], &str)] = &[
        ("plain", b"\n", "plain"),
        ("", b"\n", ""),
        ("  padded  ", b"\n", "  padded  "),
        ("tab\there", b"\n", "tab\there"),
        ("héllo 世界 🎉", b"\n", "héllo 世界 🎉"),
        ("back\\slash", b"\n", "back\\slash"),
        ("a,b", b"\n", "\"a,b\""),
        ("\"", b"\n", "\"\"\"\""),
        ("say \"hi\"", b"\n", "\"say \"\"hi\"\"\""),
        ("a\nb", b"\n", "\"a\nb\""),
        ("a\rb", b"\n", "\"a\rb\""),
        ("a\r\nb", b"\r\n", "\"a\r\nb\""),
        ("trailing\r", b"\n", "\"trailing\r\""),
        // A custom delimiter only matters when it is the one in use.
        ("a|b", b"\n", "a|b"),
        ("a|b", b"|", "\"a|b\""),
        ("a;b", b";;", "a;b"),
        ("a;;b", b";;", "\"a;;b\""),
        // A tail that starts the delimiter would complete it one byte early.
        (";", b";;", "\";\""),
        ("ab", b"bc", "\"ab\""),
        ("ba", b"bc", "ba"),
        // A leading U+FEFF would be read back as a byte-order mark; elsewhere it is data.
        ("\u{feff}id", b"\n", "\"\u{feff}id\""),
        ("id\u{feff}", b"\n", "id\u{feff}"),
    ];
    for (input, delimiter, expected) in cases {
        let mut buf = b"prefix".to_vec();
        csv_append_field(&mut buf, input, delimiter, Default::default()).unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!("prefix{expected}"),
            "field {input:?} with delimiter {:?}",
            String::from_utf8_lossy(delimiter)
        );
    }
}

/// Every byte class the escaper distinguishes must produce a JSON string that decodes
/// back to the input, with no raw control character left in the output.
#[test]
fn json_append_escaped_output_is_valid_json_for_every_byte_class() {
    use super::json_append_escaped;
    let mut inputs: Vec<String> = (0u8..0x20).map(|b| char::from(b).to_string()).collect();
    inputs.extend(
        [
            "\"",
            "\\",
            "\u{7f}",
            "\u{2028}\u{2029}",
            "é",
            "世",
            "🎉",
            "",
            "plain",
            "mixed \"q\" \\ \n\t\u{1} é世🎉 \u{7f} end",
            "\\\"\\\"",
        ]
        .map(String::from),
    );
    for input in &inputs {
        let mut buf = b"\"".to_vec();
        json_append_escaped(&mut buf, input.as_bytes());
        buf.push(b'"');
        assert!(
            buf.iter().all(|&b| b >= 0x20),
            "raw control byte left in {:?}",
            String::from_utf8_lossy(&buf)
        );
        assert_eq!(
            &serde_json::from_slice::<String>(&buf).unwrap(),
            input,
            "escaped as {:?}",
            String::from_utf8_lossy(&buf)
        );
    }

    let spelled = |s: &str| {
        let mut buf = Vec::new();
        json_append_escaped(&mut buf, s.as_bytes());
        String::from_utf8(buf).unwrap()
    };
    assert_eq!(spelled("\u{8}\u{c}\n\r\t"), r"\b\f\n\r\t");
    assert_eq!(spelled("\u{0}\u{1f}"), r"\u0000\u001f");
    assert_eq!(spelled("\u{7f}\u{2028}"), "\u{7f}\u{2028}");
}

/// A one-column record whose only value is empty would be a blank line, which readers
/// skip; it is written `""` instead. Wider records need no such help.
#[test]
fn csv_lone_empty_cell_is_quoted() {
    use super::{csv_encode_message, csv_encode_row};
    let row = |fields: &[String]| csv_encode_row(fields, b"\n", Default::default()).unwrap();
    assert_eq!(row(&[String::new()]), b"\"\"");
    assert_eq!(row(&[String::new(), String::new()]), b",");

    let mut header = None;
    let mut row = Vec::new();
    let csv = super::CsvDialect::default();
    csv_encode_message(&raw_msg(r#"{"":""}"#), &mut header, &mut row, b"\n", &csv).unwrap();
    assert_eq!(row, b"\"\"");
    csv_encode_message(
        &raw_msg(r#"{"other":1}"#),
        &mut header,
        &mut row,
        b"\n",
        &csv,
    )
    .unwrap();
    assert_eq!(row, b"\"\"", "a missing value in a one-column file");
}

/// Rows whose keys follow the header take a path without a key map; every other shape
/// has to come out the same as before.
#[test]
fn csv_rows_encode_alike_in_and_out_of_header_order() {
    use super::csv_encode_message;
    let csv = super::CsvDialect::default();
    let mut header = None;
    let mut row = Vec::new();
    csv_encode_message(
        &raw_msg(r#"{"a":1,"b":"x"}"#),
        &mut header,
        &mut row,
        b"\n",
        &csv,
    )
    .unwrap();

    let cases: &[(&str, &str)] = &[
        (r#"{"a":2,"b":"y,z"}"#, r#"2,"y,z""#),
        (r#"{ "a" : 2.50 , "b" : [1, 2] }"#, r#"2.50,"[1, 2]""#),
        (r#"{"a":8,"b":"say \"hi\""}"#, r#"8,"say ""hi""""#),
        (r#"{"b":"q","a":3}"#, "3,q"),
        (r#"{"a":4}"#, "4,"),
        (r#"{"a":5,"b":6,"c":7}"#, "5,6"),
        (r#"{"a":1,"a":2,"b":3}"#, "2,3"),
        (r#"{"a":1,"b":3,"b":4}"#, "1,4"),
    ];
    for (payload, expected) in cases {
        csv_encode_message(&raw_msg(payload), &mut header, &mut row, b"\n", &csv).unwrap();
        assert_eq!(String::from_utf8_lossy(&row), *expected, "{payload}");
    }
    for bad in [r#"{"a":9,"b":1} x"#, r#"{"a":9,"b":"\ud800"}"#, "{}", "[1]"] {
        assert!(
            csv_encode_message(&raw_msg(bad), &mut header, &mut row, b"\n", &csv).is_err(),
            "{bad}"
        );
    }

    // Flattened columns: `a.x`, `a.y.z`, `b`.
    let mut header = None;
    let first = r#"{"a":{"x":1,"y":{"z":2}},"b":3}"#;
    csv_encode_message(&raw_msg(first), &mut header, &mut row, b"\n", &csv).unwrap();
    assert_eq!(header.as_deref().unwrap(), ["a.x", "a.y.z", "b"]);
    let cases: &[(&str, &str)] = &[
        (r#"{"a":{"x":4,"y":{"z":"p,q"}},"b":6}"#, r#"4,"p,q",6"#),
        (r#"{"b":6,"a":{"y":{"z":5},"x":4}}"#, "4,5,6"),
        (r#"{"a":{"x":4,"y":{}},"b":6}"#, "4,,6"),
        (r#"{"a":{"x":4,"y":{"z":5,"w":0}},"b":6}"#, "4,5,6"),
        (r#"{"a":{"x":4,"x":7,"y":{"z":5}},"b":6}"#, "7,5,6"),
        (r#"{"a.x":4,"a.y.z":5,"b":6}"#, "4,5,6"),
        (r#"{"a":{"x":4,"y.z":5},"b":6}"#, "4,5,6"),
        (r#"{"a":7,"b":6}"#, ",,6"),
    ];
    for (payload, expected) in cases {
        csv_encode_message(&raw_msg(payload), &mut header, &mut row, b"\n", &csv).unwrap();
        assert_eq!(String::from_utf8_lossy(&row), *expected, "{payload}");
    }
}

/// Writes `payload` as a new CSV file (header + one row) and frames it back into
/// records exactly as the file consumer does.
fn csv_write_then_frame(
    payload: &std::collections::BTreeMap<String, String>,
    delimiter: &[u8],
    csv: &super::CsvDialect,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    use super::{csv_encode_message, csv_encode_row};

    let msg = crate::CanonicalMessage::new(serde_json::to_vec(payload).unwrap(), None);
    let mut header = None;
    let mut row = Vec::new();
    assert!(csv_encode_message(&msg, &mut header, &mut row, delimiter, csv).unwrap());
    let mut file = csv_encode_row(&header.unwrap(), delimiter, csv.syntax()).unwrap();
    file.extend_from_slice(delimiter);
    file.extend_from_slice(&row);
    file.extend_from_slice(delimiter);
    let records = csv_frame(&file, delimiter, csv);
    (file, records)
}

/// Frames `file` into records exactly as the file consumer does.
fn csv_frame(file: &[u8], delimiter: &[u8], csv: &super::CsvDialect) -> Vec<Vec<u8>> {
    use super::read_record_sync;
    let mut reader = std::io::Cursor::new(file);
    let mut records = Vec::new();
    loop {
        let mut record = Vec::new();
        if read_record_sync(&mut reader, delimiter, &FileFormat::Csv, csv, &mut record).unwrap()
            == 0
        {
            break;
        }
        if record.ends_with(delimiter) {
            record.truncate(record.len() - delimiter.len());
        }
        if delimiter == b"\n" && record.ends_with(b"\r") {
            record.pop();
        }
        records.push(record);
    }
    records
}

/// Decodes framed records into one object per data row, as the file consumer does.
fn csv_rows(
    records: &[Vec<u8>],
    csv: &super::CsvDialect,
) -> Vec<std::collections::BTreeMap<String, String>> {
    use crate::endpoints::file::parse_message;
    let mut state = Some(super::CsvHeader::unread(csv.clone()));
    records
        .iter()
        .filter_map(|record| parse_message(record, &FileFormat::Csv, &mut state))
        .map(|row| serde_json::from_slice(&row.payload).unwrap())
        .collect()
}

fn csv_cell() -> impl proptest::strategy::Strategy<Value = String> {
    r#"[a-c,"'\\\n\r\t |;\x00\x01\x1f\x7fé世🎉\x{feff}\x{2028}]{0,10}"#
}

/// Separator and quote pairs, as their config spellings.
fn csv_syntax() -> impl proptest::strategy::Strategy<Value = (&'static str, &'static str)> {
    use proptest::sample::select;
    (
        select(vec![",", ";", "tab", "|", "space", "0x1f"]),
        select(vec!["\"", "'"]),
    )
}

/// `None` when the record delimiter collides with the dialect, which the config rejects.
fn csv_dialect_of((separator, quote): (&str, &str), delimiter: &str) -> Option<super::CsvDialect> {
    let config = CsvConfig {
        separator: Some(separator.to_string()),
        quote: Some(quote.to_string()),
        ..Default::default()
    };
    super::CsvDialect::from_config(&config, delimiter.as_bytes()).ok()
}

fn csv_payload(
) -> impl proptest::strategy::Strategy<Value = std::collections::BTreeMap<String, String>> {
    proptest::collection::btree_map(csv_cell(), csv_cell(), 1..6)
}

proptest::proptest! {
    /// Whatever the writer emits, the reader returns the same object: keys, values and
    /// every special character, for each delimiter the framing handles differently.
    #[test]
    fn csv_writer_output_reads_back_unchanged(
        payload in csv_payload(),
        delimiter in proptest::sample::select(vec!["\n", "\r\n", "|", ";;"]),
        syntax in csv_syntax(),
    ) {
        let Some(csv) = csv_dialect_of(syntax, delimiter) else { return Ok(()) };
        let (file, records) = csv_write_then_frame(&payload, delimiter.as_bytes(), &csv);
        proptest::prop_assert_eq!(
            records.len(), 2, "file {:?} framed wrongly", String::from_utf8_lossy(&file)
        );
        proptest::prop_assert_eq!(
            csv_rows(&records, &csv), vec![payload], "file {:?}", String::from_utf8_lossy(&file)
        );
    }

    /// The other direction: a file an independent writer produced, in any dialect,
    /// reads back as the cells that went in.
    #[test]
    fn csv_crate_output_reads_back_unchanged(
        header in proptest::collection::btree_set(csv_cell(), 1..6),
        cells in proptest::collection::vec(csv_cell(), 12),
        syntax in csv_syntax(),
    ) {
        // An unquoted leading U+FEFF is a byte-order mark to the reader.
        proptest::prop_assume!(!header.first().unwrap().starts_with('\u{feff}'));
        let csv = csv_dialect_of(syntax, "\n").unwrap();
        let header: Vec<String> = header.into_iter().collect();
        let rows: Vec<&[String]> = cells.chunks_exact(header.len()).collect();
        let mut writer = csv::WriterBuilder::new()
            .delimiter(csv.syntax().separator)
            .quote(csv.syntax().quote.unwrap())
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(Vec::new());
        writer.write_record(&header).unwrap();
        for row in &rows {
            writer.write_record(*row).unwrap();
        }
        let file = writer.into_inner().unwrap();
        let expected: Vec<std::collections::BTreeMap<String, String>> = rows
            .iter()
            .map(|row| header.iter().cloned().zip(row.iter().cloned()).collect())
            .collect();
        proptest::prop_assert_eq!(
            csv_rows(&csv_frame(&file, b"\n", &csv), &csv),
            expected,
            "file {:?}", String::from_utf8_lossy(&file)
        );
    }

    /// `separator: auto` settles on what the writer used, whatever the data cells hold.
    /// Header names are plain: the guess reads only the first record.
    #[test]
    fn csv_auto_reads_back_every_candidate_separator(
        payload in proptest::collection::btree_map("[a-c]{1,4}", csv_cell(), 2..6),
        separator in proptest::sample::select(vec![",", ";", "tab", "|"]),
    ) {
        let written = csv_dialect_of((separator, "\""), "\n").unwrap();
        let auto = csv_dialect_of(("auto", "\""), "\n").unwrap();
        let (file, _) = csv_write_then_frame(&payload, b"\n", &written);
        proptest::prop_assert_eq!(
            csv_rows(&csv_frame(&file, b"\n", &auto), &auto),
            vec![payload],
            "file {:?}", String::from_utf8_lossy(&file)
        );
    }

    /// `quote: none` reads and writes quote characters as data.
    #[test]
    fn csv_without_quoting_reads_back_unchanged(
        payload in proptest::collection::btree_map(
            r#"[a-c"' é世🎉]{0,8}"#, r#"[a-c"' ,;é世🎉]{0,8}"#, 2..6
        ),
    ) {
        let csv = csv_dialect_of(("tab", "none"), "\n").unwrap();
        let (file, records) = csv_write_then_frame(&payload, b"\n", &csv);
        proptest::prop_assert_eq!(
            csv_rows(&records, &csv), vec![payload], "file {:?}", String::from_utf8_lossy(&file)
        );
    }

    /// An independent RFC 4180 parser reads the writer's output the same way, so files
    /// written by the sink open correctly in standard tools.
    #[test]
    fn csv_writer_output_matches_the_csv_crate(
        payload in csv_payload(),
        delimiter in proptest::sample::select(vec!["\n", "\r\n", "|"]),
        syntax in csv_syntax(),
    ) {
        let Some(csv) = csv_dialect_of(syntax, delimiter) else { return Ok(()) };
        // A lone empty cell is a blank line, which the csv crate skips by design.
        proptest::prop_assume!(
            !(payload.len() == 1 && payload.iter().any(|(k, v)| k.is_empty() || v.is_empty()))
        );
        let (file, _) = csv_write_then_frame(&payload, delimiter.as_bytes(), &csv);
        let terminator = match delimiter {
            "|" => csv::Terminator::Any(b'|'),
            _ => csv::Terminator::CRLF,
        };
        let parsed: Vec<Vec<String>> = csv::ReaderBuilder::new()
            .has_headers(false)
            .delimiter(csv.syntax().separator)
            .quote(csv.syntax().quote.unwrap())
            .terminator(terminator)
            .from_reader(file.as_slice())
            .records()
            .map(|r| r.unwrap().iter().map(String::from).collect())
            .collect();
        proptest::prop_assert_eq!(
            parsed,
            vec![
                payload.keys().cloned().collect::<Vec<_>>(),
                payload.values().cloned().collect::<Vec<_>>(),
            ],
            "file {:?}", String::from_utf8_lossy(&file)
        );
    }
}

/// A header-only file has no rows: the header must not surface as a message, and the
/// columns it declares must apply to rows appended later.
#[tokio::test]
async fn test_file_csv_header_only_file_yields_no_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "a,b\n").await.unwrap();
    let mut source = FileConsumer::new(&csv_config(&path)).await.unwrap();
    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), source.receive_batch(8))
        .await
        .expect("reader hung on a header-only file")
        .unwrap();
    assert!(drained.messages.is_empty(), "{:?}", drained.messages);

    let mut file = OpenOptions::new().append(true).open(&path).await.unwrap();
    file.write_all(b"1,2\n").await.unwrap();
    file.flush().await.unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), source.receive())
        .await
        .expect("appended row never arrived")
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&received.message.payload).unwrap(),
        json!({"a": "1", "b": "2"})
    );
}

/// A blank line (commonly at the end of an exported file) carries no values and must
/// not become a row of empty strings.
#[tokio::test]
async fn test_file_csv_blank_lines_are_not_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "a,b\n1,2\n\n3,4\r\n\r\n")
        .await
        .unwrap();
    let mut source = FileConsumer::new(&csv_config(&path)).await.unwrap();
    let mut rows = Vec::new();
    while let Ok(Ok(batch)) = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        source.receive_batch(8),
    )
    .await
    {
        if batch.messages.is_empty() {
            break;
        }
        rows.extend(
            batch
                .messages
                .iter()
                .map(|m| serde_json::from_slice::<serde_json::Value>(&m.payload).unwrap()),
        );
    }
    assert_eq!(
        rows,
        vec![json!({"a": "1", "b": "2"}), json!({"a": "3", "b": "4"})]
    );

    // Consume { delete: true }: a blank line is deleted with the row behind it, never
    // in place of an unacked row.
    let path = dir.path().join("queue.csv");
    tokio::fs::write(&path, "a,b\n1,2\n\n3,4\r\n\r\n")
        .await
        .unwrap();
    let config = FileConfig {
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..csv_config(&path)
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    let batch = source.receive_batch(8).await.unwrap();
    assert_eq!(batch.messages.len(), 2);
    (batch.commit)(vec![
        crate::traits::MessageDisposition::Ack,
        crate::traits::MessageDisposition::Nack,
    ])
    .await
    .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "a,b\n\n3,4\r\n\r\n"
    );
}

/// FILE-06: `fsync: batch` syncs before the ack, `periodic` in the background and on flush.
#[tokio::test]
async fn test_file_sink_fsync_modes() {
    let dir = tempdir().unwrap();
    for fsync in ["batch", "periodic"] {
        let path = dir.path().join(format!("{fsync}.jsonl"));
        let config: FileConfig = serde_json::from_value(json!({
            "path": path.to_str().unwrap(),
            "format": "raw",
            "fsync": fsync,
            "fsync_interval_ms": 20,
        }))
        .unwrap();
        let sink = FilePublisher::new(&config).await.unwrap();
        sink.send_batch(vec![msg!("one")]).await.unwrap();
        if let super::FsyncPolicy::Periodic(dirty) = &sink.fsync {
            assert!(dirty.load(std::sync::atomic::Ordering::Acquire));
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while dirty.load(std::sync::atomic::Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("periodic sync never ran");
            sink.send_batch(vec![msg!("two")]).await.unwrap();
            sink.flush().await.unwrap();
            assert!(!dirty.load(std::sync::atomic::Ordering::Acquire));
        }
        assert!(tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .starts_with("one\n"));
    }

    let zero: FileConfig = serde_json::from_value(json!({
        "path": dir.path().join("zero.jsonl").to_str().unwrap(),
        "fsync": "periodic",
        "fsync_interval_ms": 0,
    }))
    .unwrap();
    assert!(FilePublisher::new(&zero).await.is_err());
}

/// FILE-02: two spellings of one path share the lock that serializes rewrites and appends.
#[test]
fn test_file_lock_is_shared_across_path_spellings() {
    let dir = tempdir().unwrap();
    let plain = dir.path().join("q.jsonl");
    let dotted = dir.path().join(".").join("q.jsonl");
    let a = super::get_file_lock(plain.to_str().unwrap());
    let b = super::get_file_lock(dotted.to_str().unwrap());
    assert!(std::sync::Arc::ptr_eq(&a, &b));
}

/// FILE-01: the header stays in a queue file, so a restart still names the columns
/// instead of taking the first remaining row for the header.
#[tokio::test]
async fn test_file_csv_queue_keeps_header_across_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("queue.csv");
    tokio::fs::write(&path, "a,b\n1,2\n3,4\n5,6\n")
        .await
        .unwrap();
    let config = FileConfig {
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..csv_config(&path)
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    let batch = source.receive_batch(1).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    (batch.commit)(vec![crate::traits::MessageDisposition::Ack])
        .await
        .unwrap();
    drop(source);
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "a,b\n3,4\n5,6\n"
    );

    let mut source = FileConsumer::new(&config).await.unwrap();
    let batch = source.receive_batch(8).await.unwrap();
    let rows: Vec<serde_json::Value> = batch
        .messages
        .iter()
        .map(|m| serde_json::from_slice(&m.payload).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![json!({"a": "3", "b": "4"}), json!({"a": "5", "b": "6"})]
    );
}

/// More leading blank records than one read pass holds must not surface as an empty
/// (end-of-file) batch before the header and data behind them are read.
#[tokio::test]
async fn test_file_csv_queue_many_leading_blank_lines() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("queue.csv");
    tokio::fs::write(&path, format!("{}a,b\n1,2\n", "\n".repeat(200)))
        .await
        .unwrap();
    let config = FileConfig {
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..csv_config(&path)
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    let batch = source.receive_batch(8).await.unwrap();
    assert_eq!(batch.messages.len(), 1);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&batch.messages[0].payload).unwrap(),
        json!({"a": "1", "b": "2"})
    );
}
#[tokio::test]
async fn test_file_csv_value_types_and_escaping() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("data.csv");
    let config = FileConfig {
        path: file_path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        csv: CsvConfig {
            nested: CsvNested::Json,
            ..Default::default()
        },
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
    assert_eq!(content, "\"we\"\"ird\",plain\n1,x\n");
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

fn csv_config(path: &std::path::Path) -> FileConfig {
    FileConfig {
        path: path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        ..Default::default()
    }
}

fn raw_msg(payload: &str) -> crate::CanonicalMessage {
    crate::CanonicalMessage::new(payload.as_bytes().to_vec(), None)
}

async fn read_csv_rows(config: &FileConfig, n: usize) -> Vec<serde_json::Value> {
    let mut source = FileConsumer::new(config).await.unwrap();
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        let received = source.receive().await.unwrap();
        rows.push(serde_json::from_slice(&received.message.payload).unwrap());
    }
    rows
}

/// A lone surrogate is legal JSON text but has no UTF-8 spelling, so it cannot become a
/// CSV cell. The message must fail loudly instead of writing an empty field, and must not
/// leave the header half-established for the rows after it.
#[tokio::test]
async fn test_file_csv_undecodable_string_fails_the_message() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = csv_config(&path);

    let sink = FilePublisher::new(&config).await.unwrap();
    let result = sink
        .send_batch(vec![
            raw_msg(r#"{"a":"\ud800","b":1}"#),
            raw_msg(r#"{"a":"x","b":2}"#),
        ])
        .await
        .unwrap();
    match result {
        crate::outcomes::SentBatch::Partial { failed, .. } => assert_eq!(failed.len(), 1),
        other => panic!("expected Partial, got {other:?}"),
    }
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "a,b\nx,2\n");
}

/// Nested values are JSON text inside the cell, so they keep the producer's spelling —
/// key order and number format — whether or not a top-level key forced the slow path.
#[tokio::test]
async fn test_file_csv_cells_keep_source_spelling_on_every_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = FileConfig {
        csv: CsvConfig {
            nested: CsvNested::Json,
            ..Default::default()
        },
        ..csv_config(&path)
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![
        raw_msg(r#"{"n":2.5000,"o":{"z":1e3,"a":"\ud800"},"plain":1e3}"#),
        raw_msg(r#"{"n":2.5000,"o":{"z":1e3,"a":"\ud800"},"plain":1e3}"#),
    ])
    .await
    .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    let row = r#"2.5000,"{""z"":1e3,""a"":""\ud800""}",1e3"#;
    assert_eq!(content, format!("n,o,plain\n{row}\n{row}\n"));
}

/// Pretty-printed nested JSON must not smuggle a line break into the CSV row.
#[tokio::test]
async fn test_file_csv_multiline_nested_value_stays_in_one_row() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = FileConfig {
        csv: CsvConfig {
            nested: CsvNested::Json,
            ..Default::default()
        },
        ..csv_config(&path)
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![raw_msg("{\"id\":1,\"o\":{\r\n  \"k\": [1,\n 2]\n}}")])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "id,o\n1,\"{  \"\"k\"\": [1, 2]}\"\n");
}

/// The record delimiter is configurable, so a value containing it has to be quoted like
/// one containing a newline would be.
#[tokio::test]
async fn test_file_csv_custom_delimiter_round_trips_values_containing_it() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = FileConfig {
        delimiter: Some("|".to_string()),
        ..csv_config(&path)
    };

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![
        msg!(json!({"a": "x|y", "b": "line\nbreak"})),
        msg!(json!({"a": "plain", "b": "|"})),
    ])
    .await
    .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "a,b|\"x|y\",\"line\nbreak\"|plain,\"|\"|");
    assert_eq!(
        read_csv_rows(&config, 2).await,
        vec![
            json!({"a": "x|y", "b": "line\nbreak"}),
            json!({"a": "plain", "b": "|"}),
        ]
    );
}

/// A delimiter containing the field separator or the quote cannot frame CSV at all.
#[tokio::test]
async fn test_file_csv_rejects_delimiters_that_collide_with_csv_syntax() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "a\n1\n").await.unwrap();
    for delimiter in [",", "\"", "a,b", "0x2c", "0x22"] {
        let config = FileConfig {
            delimiter: Some(delimiter.to_string()),
            ..csv_config(&path)
        };
        let err = FilePublisher::new(&config)
            .await
            .err()
            .unwrap_or_else(|| panic!("publisher accepted delimiter {delimiter:?}"));
        assert!(err.to_string().contains("delimiter"), "{err:#}");
        let err = FileConsumer::new(&config)
            .await
            .err()
            .unwrap_or_else(|| panic!("consumer accepted delimiter {delimiter:?}"));
        assert!(err.to_string().contains("delimiter"), "{err:#}");
    }
    // A plain delimiter is still fine.
    let config = FileConfig {
        delimiter: Some(";".to_string()),
        ..csv_config(&path)
    };
    assert!(FilePublisher::new(&config).await.is_ok());
}

/// Excel's "CSV UTF-8" export starts with a byte-order mark, which must not become part
/// of the first column's name.
#[tokio::test]
async fn test_file_csv_strips_a_leading_bom() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "\u{feff}id,name\r\n1,Ada\r\n")
        .await
        .unwrap();
    assert_eq!(
        read_csv_rows(&csv_config(&path), 1).await,
        vec![json!({"id": "1", "name": "Ada"})]
    );
}

#[tokio::test]
async fn test_file_csv_bom_before_a_multiline_quoted_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "\u{feff}\"i\nd\",name\r\n1,Ada\r\n")
        .await
        .unwrap();
    assert_eq!(
        read_csv_rows(&csv_config(&path), 1).await,
        vec![json!({"i\nd": "1", "name": "Ada"})]
    );
}

/// Appending to a CSV file that already has a header must write in that header's column
/// order, not the payload's sorted keys, or every new row lands in the wrong columns.
#[tokio::test]
async fn test_file_csv_append_follows_the_existing_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "name,\"a,ge\"\nalice,30\n")
        .await
        .unwrap();
    let config = csv_config(&path);

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"a,ge": "25", "name": "bob"}))])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "name,\"a,ge\"\nalice,30\nbob,25\n");
}

/// CRLF files: the record's trailing CR is framing, a CR inside quotes is data.
#[tokio::test]
async fn test_file_csv_reads_crlf_files() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "a,b\r\n1,\"x\r\ny\"\r\n2,\"\"\r\n")
        .await
        .unwrap();
    assert_eq!(
        read_csv_rows(&csv_config(&path), 2).await,
        vec![json!({"a": "1", "b": "x\r\ny"}), json!({"a": "2", "b": ""})]
    );
}

/// A truncated export must not load as if it were whole.
#[tokio::test]
async fn csv_drain_fails_on_a_quote_left_open_at_end_of_file() {
    let dir = tempdir().unwrap();
    let file_path = dir.path().join("truncated.csv");
    tokio::fs::write(&file_path, "id,name\n1,a\n2,\"cut off\n")
        .await
        .unwrap();

    let mut source = FileConsumer::new(&csv_config(&file_path)).await.unwrap();
    source.set_exit_on_empty(true);

    let mut rows = 0;
    let error = loop {
        match source.receive_batch(10).await {
            Ok(batch) => rows += batch.messages.len(),
            Err(e) => break e,
        }
    };
    assert_eq!(rows, 1);
    assert!(
        matches!(error, crate::traits::ConsumerError::Permanent(_)),
        "{error:?}"
    );
    assert!(error.to_string().contains("ends inside a quoted field"));
}

/// Pinned, not endorsed: CSV has no null, so `null` is written as the text `null` and
/// every value reads back as a string. `""`, `"null"` and `null` stay distinguishable
/// only on the write side.
#[tokio::test]
async fn test_file_csv_null_and_empty_string_spelling_is_pinned() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = csv_config(&path);

    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![msg!(json!({"a": null, "b": "", "c": "null", "d": 0}))])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "a,b,c,d\nnull,,null,0\n");
    assert_eq!(
        read_csv_rows(&config, 1).await,
        vec![json!({"a": "null", "b": "", "c": "null", "d": "0"})]
    );
}

/// A repeated header name must not become a repeated JSON key, which readers collapse to
/// one value. Repeats get a numeric suffix that never collides with a real column.
#[test]
fn test_csv_duplicate_header_names_get_unique_keys() {
    assert_eq!(
        csv_decode(b"a,a,b", b"1,2,3"),
        br#"{"a":"1","a_2":"2","b":"3"}"#
    );
    assert_eq!(
        csv_decode(b"a,a_2,a,a", b"1,2,3,4"),
        br#"{"a":"1","a_2":"2","a_3":"3","a_4":"4"}"#
    );
    assert_eq!(csv_decode(b",", b"1,2"), br#"{"":"1","_2":"2"}"#);
}

/// Appending to a file whose header repeats a name keeps rows aligned with the renamed
/// columns the reader produces.
#[tokio::test]
async fn test_file_csv_append_to_a_header_with_repeated_names() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    tokio::fs::write(&path, "a,a\n1,2\n").await.unwrap();
    let config = csv_config(&path);
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![raw_msg(r#"{"a_2":"4","a":"3"}"#)])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "a,a\n1,2\n3,4\n"
    );
    assert_eq!(
        read_csv_rows(&config, 2).await,
        vec![json!({"a": "1", "a_2": "2"}), json!({"a": "3", "a_2": "4"})]
    );
}

#[test]
fn test_csv_header_bom_is_stripped_in_the_decoder() {
    assert_eq!(
        csv_decode("\u{feff}id,x".as_bytes(), b"1,2"),
        br#"{"id":"1","x":"2"}"#
    );
    // Only a leading BOM is framing; one inside a value is data.
    assert_eq!(
        csv_decode(b"id", "\u{feff}1".as_bytes()),
        "{\"id\":\"\u{feff}1\"}".as_bytes()
    );
}

/// `json` keeps the payload's own bytes, but a raw line break in them would split the
/// JSON-lines record. Outside strings a line break is only whitespace, so it is dropped.
#[tokio::test]
async fn test_json_format_multiline_payload_stays_on_one_line() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.jsonl");
    let config = FileConfig {
        path: path.to_str().unwrap().to_string(),
        format: FileFormat::Json,
        ..Default::default()
    };

    let pretty = "{\r\n  \"a\": \"x\\ny\",\n  \"b\": [1,\n 2]\n}";
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![raw_msg(pretty), raw_msg(r#"{"c":1}"#)])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content.lines().count(), 2, "file content: {content:?}");

    let mut source = FileConsumer::new(&config).await.unwrap();
    let first = source.receive().await.unwrap().message;
    assert_eq!(
        first.get_payload_str(),
        "{  \"a\": \"x\\ny\",  \"b\": [1, 2]}"
    );
    let second = source.receive().await.unwrap().message;
    assert_eq!(second.get_payload_str(), r#"{"c":1}"#);
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
        "name,age\nalice,30\nbob,25\ncarol,41\n"
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
        "name,age\nalice,30\nbob,25\n"
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
        let line = encode_record(&msg, &FileFormat::Json, b"\n").expect("record encodes");

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
        let line = encode_record(&msg, &format, b"\n").unwrap();
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

    let line = encode_record(&msg, &FileFormat::Json, b"\n").unwrap();
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

#[tokio::test]
async fn test_file_rejects_parquet_format() {
    let dir = tempdir().unwrap();
    let config = FileConfig {
        path: dir
            .path()
            .join("data.parquet")
            .to_str()
            .unwrap()
            .to_string(),
        format: FileFormat::Parquet,
        ..Default::default()
    };
    let error = FilePublisher::new(&config).await.err().unwrap();
    assert!(error.to_string().contains("only supported by object_store"));
    let error = FileConsumer::new(&config).await.err().unwrap();
    assert!(error.to_string().contains("only supported by object_store"));
}

/// Splits `file` on `delimiter` and decodes each record, as the plain reader does.
fn frame_and_parse(
    file: &[u8],
    delimiter: &[u8],
    format: &FileFormat,
) -> Vec<crate::CanonicalMessage> {
    use crate::endpoints::file::parse_message;
    let mut records: Vec<&[u8]> = Vec::new();
    let mut rest = file;
    while let Some(at) = memchr::memmem::find(rest, delimiter) {
        records.push(&rest[..at]);
        rest = &rest[at + delimiter.len()..];
    }
    assert!(rest.is_empty(), "trailing bytes after the last delimiter");
    records
        .into_iter()
        .map(|record| parse_message(record, format, &mut None).expect("record decodes"))
        .collect()
}

proptest::proptest! {
    /// A custom delimiter inside a string must not split a JSON record: every format that
    /// writes JSON escapes it, and the reader gets the same message back.
    #[test]
    fn json_records_escape_a_custom_delimiter(
        text in r#"[a-c|;:\-"\\\n\x1e é世🎉]{0,16}"#,
        delimiter in proptest::sample::select(vec!["|", ";;", "é", "---", "\u{1e}", "\n"]),
        format in proptest::sample::select(vec![FileFormat::Normal, FileFormat::Json, FileFormat::Text]),
    ) {
        use crate::endpoints::file::encode_record;
        let payload = serde_json::to_vec(&json!({ text.clone(): [text.clone(), {"n": -1.5}] })).unwrap();
        let mut msg = crate::CanonicalMessage::new(payload.clone(), None);
        msg.metadata.insert("k".to_string(), text.clone());

        let mut file = Vec::new();
        for _ in 0..2 {
            file.extend_from_slice(&encode_record(&msg, &format, delimiter.as_bytes()).unwrap());
            file.extend_from_slice(delimiter.as_bytes());
        }
        let read = frame_and_parse(&file, delimiter.as_bytes(), &format);
        proptest::prop_assert_eq!(read.len(), 2, "file {:?}", String::from_utf8_lossy(&file));
        for got in read {
            let got_payload: serde_json::Value = serde_json::from_slice(&got.payload).unwrap();
            let want: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            proptest::prop_assert_eq!(got_payload, want);
            if format != FileFormat::Json {
                proptest::prop_assert_eq!(got.metadata.get("k"), Some(&text));
            }
        }
    }
}

/// Where the delimiter is JSON syntax itself, no escape exists: the message fails
/// instead of being written as a record the reader would split.
#[test]
fn json_record_fails_when_the_delimiter_is_json_syntax() {
    use crate::endpoints::file::encode_record;
    let msg = raw_msg(r#"{"a":1,"b":2}"#);
    let error = encode_record(&msg, &FileFormat::Json, b",").unwrap_err();
    assert!(
        error.to_string().contains("outside a JSON string"),
        "{error}"
    );
    // `raw` promises the payload's bytes untouched, so it is never rewritten.
    assert_eq!(
        encode_record(&msg, &FileFormat::Raw, b",").unwrap(),
        msg.payload
    );
}

#[tokio::test]
async fn test_file_json_custom_delimiter_inside_values_round_trips() {
    let dir = tempdir().unwrap();
    let config = FileConfig {
        path: dir.path().join("data.jsonl").to_str().unwrap().to_string(),
        format: FileFormat::Json,
        delimiter: Some("|".to_string()),
        ..Default::default()
    };
    let payloads = [json!({"a|b": "x|y"}), json!({"plain": 1})];
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(payloads.iter().map(|p| msg!(p.clone())).collect())
        .await
        .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    let mut source = FileConsumer::new(&config).await.unwrap();
    for want in payloads {
        let got = source.receive().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&got.message.payload).unwrap(),
            want
        );
    }
}

fn csv_dialect_config(path: &std::path::Path, csv: CsvConfig) -> FileConfig {
    FileConfig {
        path: path.to_str().unwrap().to_string(),
        format: FileFormat::Csv,
        mode: Some(FileConsumerMode::Consume { delete: false }),
        csv,
        ..Default::default()
    }
}

/// The dialects other tools export: Excel, `mongoexport --type=tsv`, `psql --csv` with a
/// custom delimiter, and headerless dumps.
#[tokio::test]
async fn test_file_csv_reads_other_dialects() {
    let separator = |value: &str| CsvConfig {
        separator: Some(value.to_string()),
        ..Default::default()
    };
    let cases: Vec<(&str, CsvConfig, &str, serde_json::Value)> = vec![
        (
            "excel: semicolon, byte-order mark, CRLF",
            separator(";"),
            "\u{feff}id;name;note\r\n1;\"Müller; Hans\";a,b\r\n",
            json!([{"id": "1", "name": "Müller; Hans", "note": "a,b"}]),
        ),
        (
            "tab",
            separator("tab"),
            "id\tname\n1\tAda Lovelace\n2\t\"multi\nline\"\n",
            json!([{"id": "1", "name": "Ada Lovelace"}, {"id": "2", "name": "multi\nline"}]),
        ),
        (
            "space",
            separator("space"),
            "id name\n1 \"Ada Lovelace\"\n",
            json!([{"id": "1", "name": "Ada Lovelace"}]),
        ),
        (
            "no quote character: a quote is data",
            CsvConfig {
                quote: Some("none".to_string()),
                ..separator("|")
            },
            "id|note\n1|\"open\n2|say \"hi\"\n",
            json!([{"id": "1", "note": "\"open"}, {"id": "2", "note": "say \"hi\""}]),
        ),
        (
            "single quotes",
            CsvConfig {
                quote: Some("'".to_string()),
                ..Default::default()
            },
            "id,note\n1,'it''s, fine'\n",
            json!([{"id": "1", "note": "it's, fine"}]),
        ),
        (
            "no header: the first record is data",
            CsvConfig {
                header: Some(false),
                columns: vec!["id".to_string(), "name".to_string()],
                ..Default::default()
            },
            "\u{feff}1,Ada\n2,Grace\n",
            json!([{"id": "1", "name": "Ada"}, {"id": "2", "name": "Grace"}]),
        ),
        (
            "columns rename the header",
            CsvConfig {
                columns: vec!["key".to_string(), "value".to_string()],
                ..Default::default()
            },
            "Spalte 1,Spalte 2\n1,Ada\n",
            json!([{"key": "1", "value": "Ada"}]),
        ),
        (
            "auto: semicolon",
            separator("auto"),
            "\u{feff}id;name\r\n1;\"a;b\"\r\n",
            json!([{"id": "1", "name": "a;b"}]),
        ),
        (
            "auto: tab",
            separator("auto"),
            "id\tname\n1\ta,b\n",
            json!([{"id": "1", "name": "a,b"}]),
        ),
        (
            "auto: comma stays comma",
            separator("auto"),
            "id,name\n1,a;b\n",
            json!([{"id": "1", "name": "a;b"}]),
        ),
    ];
    for (what, csv, content, expected) in cases {
        let expected = expected.as_array().unwrap().clone();
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.csv");
        tokio::fs::write(&path, content).await.unwrap();
        for mode in [
            FileConsumerMode::Consume { delete: false },
            FileConsumerMode::Consume { delete: true },
        ] {
            let config = FileConfig {
                mode: Some(mode.clone()),
                ..csv_dialect_config(&path, csv.clone())
            };
            let rows = read_csv_rows(&config, expected.len()).await;
            assert_eq!(rows, expected, "{what} ({mode:?})");
        }
    }
}

#[tokio::test]
async fn test_file_csv_dialect_round_trip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let config = csv_dialect_config(
        &path,
        CsvConfig {
            separator: Some(";".to_string()),
            ..Default::default()
        },
    );
    let sink = FilePublisher::new(&config).await.unwrap();
    sink.send_batch(vec![
        msg!(json!({"id": 1, "note": "a;b", "plain": "x,y"})),
        msg!(json!({"id": 2, "note": "say \"hi\"", "plain": ""})),
    ])
    .await
    .unwrap();
    sink.flush().await.unwrap();
    drop(sink);

    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "id;note;plain\n1;\"a;b\";x,y\n2;\"say \"\"hi\"\"\";\n"
    );
    assert_eq!(
        read_csv_rows(&config, 2).await,
        vec![
            json!({"id": "1", "note": "a;b", "plain": "x,y"}),
            json!({"id": "2", "note": "say \"hi\"", "plain": ""}),
        ]
    );
}

/// `columns` on a sink is a projection in a fixed order; `header: false` writes rows only.
#[tokio::test]
async fn test_file_csv_sink_columns_and_no_header() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.tsv");
    let config = csv_dialect_config(
        &path,
        CsvConfig {
            separator: Some("tab".to_string()),
            quote: Some("none".to_string()),
            header: Some(false),
            columns: vec!["name".to_string(), "id".to_string()],
            ..Default::default()
        },
    );
    let sink = FilePublisher::new(&config).await.unwrap();
    let sent = sink
        .send_batch(vec![
            msg!(json!({"id": 1, "name": "Ada", "dropped": true})),
            msg!(json!({"id": 2, "name": "has\ttab"})),
            msg!(json!({"id": 3})),
        ])
        .await
        .unwrap();
    sink.flush().await.unwrap();
    assert!(
        matches!(&sent, crate::traits::SentBatch::Partial { failed, .. } if failed.len() == 1),
        "a value holding the separator has no spelling without a quote character"
    );
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "Ada\t1\n\t3\n"
    );
}

/// What the `aggregate` middleware emits: its fields nest under `into`. A CSV sink
/// spreads them over `parent.child` columns unless told to keep the JSON text.
#[tokio::test]
async fn test_file_csv_flattens_nested_objects() {
    let rows = || {
        vec![
            raw_msg(r#"{"sensor":"a","stats":{"n":1,"avg":2.50,"ema":{"fast":2.5}},"tags":[1,2]}"#),
            raw_msg(r#"{"stats":{"ema":{"fast":3.0},"avg":3.25,"n":2},"sensor":"b","tags":[]}"#),
            raw_msg(r#"{"sensor":"c","stats":null,"tags":[3]}"#),
        ]
    };
    let dir = tempdir().unwrap();

    let path = dir.path().join("flat.csv");
    let sink = FilePublisher::new(&csv_dialect_config(&path, CsvConfig::default()))
        .await
        .unwrap();
    sink.send_batch(rows()).await.unwrap();
    sink.flush().await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "sensor,stats.n,stats.avg,stats.ema.fast,tags\n\
         a,1,2.50,2.5,\"[1,2]\"\n\
         b,2,3.25,3.0,[]\n\
         c,,,,[3]\n"
    );

    let path = dir.path().join("json.csv");
    let json = CsvConfig {
        nested: CsvNested::Json,
        ..Default::default()
    };
    let sink = FilePublisher::new(&csv_dialect_config(&path, json))
        .await
        .unwrap();
    sink.send_batch(rows().into_iter().take(1).collect())
        .await
        .unwrap();
    sink.flush().await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "sensor,stats,tags\n\
         a,\"{\"\"n\"\":1,\"\"avg\"\":2.50,\"\"ema\"\":{\"\"fast\"\":2.5}}\",\"[1,2]\"\n"
    );
}

#[tokio::test]
async fn test_file_csv_rejects_ambiguous_dialects() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("data.csv");
    let auto = CsvConfig {
        separator: Some("auto".to_string()),
        ..Default::default()
    };
    assert!(FilePublisher::new(&csv_dialect_config(&path, auto))
        .await
        .is_err());
    let headless = CsvConfig {
        header: Some(false),
        ..Default::default()
    };
    assert!(FileConsumer::new(&csv_dialect_config(&path, headless))
        .await
        .is_err());
    let config = FileConfig {
        delimiter: Some(",".to_string()),
        ..csv_dialect_config(&path, CsvConfig::default())
    };
    assert!(FileConsumer::new(&config).await.is_err());
}

#[tokio::test]
async fn blank_lines_are_not_records() {
    for format in [FileFormat::Raw, FileFormat::Json] {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("blank.jsonl");
        tokio::fs::write(&file_path, "{\"a\":1}\n\n\r\n{\"a\":2}\n")
            .await
            .unwrap();
        let config = FileConfig {
            path: file_path.to_str().unwrap().to_string(),
            format,
            ..Default::default()
        };
        let mut source = FileConsumer::new(&config).await.unwrap();
        source.set_exit_on_empty(true);
        assert_eq!(drain_count(&mut source).await, 2);
    }
}

/// FILE-03: a final record without its delimiter may still be growing, so a delete-mode
/// reader holds it back; one that stays unchanged is the file's last record.
#[tokio::test]
async fn test_file_queue_holds_back_a_record_still_being_written() {
    use std::io::Write;

    for mode in [
        FileConsumerMode::Consume { delete: true },
        FileConsumerMode::Subscribe { delete: true },
    ] {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"b\":").unwrap();
        let config = FileConfig {
            mode: Some(mode.clone()),
            ..FileConfig::new(path.to_str().unwrap())
        };
        let mut source = FileConsumer::new(&config).await.unwrap();
        let batch = source.receive_batch(8).await.unwrap();
        assert_eq!(batch.messages.len(), 1, "{mode:?}");
        (batch.commit)(vec![crate::traits::MessageDisposition::Ack])
            .await
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"2}\n{\"c\":3}")
            .unwrap();

        let mut rest = Vec::new();
        while rest.len() < 2 {
            let batch =
                tokio::time::timeout(std::time::Duration::from_secs(5), source.receive_batch(8))
                    .await
                    .unwrap_or_else(|_| panic!("{mode:?}: got only {rest:?}"))
                    .unwrap();
            let acks = vec![crate::traits::MessageDisposition::Ack; batch.messages.len()];
            rest.extend(
                batch
                    .messages
                    .iter()
                    .map(|m| String::from_utf8_lossy(&m.payload).into_owned()),
            );
            (batch.commit)(acks).await.unwrap();
        }
        assert_eq!(rest, ["{\"b\":2}", "{\"c\":3}"], "{mode:?}");
    }
}

/// FILE-04: a compressed file whose last record has no delimiter still delivers it.
#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_compressed_final_record_without_delimiter() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tail.txt.gz");
    let member =
        crate::support::compression::compress_member(Compression::Gzip, b"one\ntwo").unwrap();
    std::fs::write(&path, member).unwrap();
    let config = FileConfig {
        format: FileFormat::Raw,
        compression: Compression::Gzip,
        ..FileConfig::new(path.to_str().unwrap())
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    assert_eq!(collect_compressed(&mut source, 2).await, ["one", "two"]);
}

/// FILE-05: a compressed CSV file that grows keeps its header; the first new row is data.
#[cfg(feature = "compression")]
#[tokio::test]
async fn test_file_compressed_csv_keeps_header_when_the_file_grows() {
    use std::io::Write;

    let dir = tempdir().unwrap();
    let path = dir.path().join("grow.csv.gz");
    let member = |data: &[u8]| {
        crate::support::compression::compress_member(Compression::Gzip, data).unwrap()
    };
    std::fs::write(&path, member(b"a,b\n1,2\n")).unwrap();
    let config = FileConfig {
        compression: Compression::Gzip,
        ..csv_config(&path)
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    assert_eq!(collect_compressed(&mut source, 1).await.len(), 1);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&member(b"3,4\n5,6\n"))
        .unwrap();
    let rows: Vec<serde_json::Value> = collect_compressed(&mut source, 2)
        .await
        .iter()
        .map(|payload| serde_json::from_slice(payload).unwrap())
        .collect();
    assert_eq!(
        rows,
        vec![json!({"a": "3", "b": "4"}), json!({"a": "5", "b": "6"})]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_file_queue_does_not_redeliver_after_a_failed_delete() {
    use crate::traits::MessageDisposition::Ack;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let path = dir.path().join("queue.jsonl");
    std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
    let config = FileConfig {
        mode: Some(FileConsumerMode::Consume { delete: true }),
        ..FileConfig::new(path.to_str().unwrap())
    };
    let mut source = FileConsumer::new(&config).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let first = source.receive_batch(1).await.unwrap();
    // The rewrite needs a temp file next to the queue file, which a read-only directory refuses.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    (first.commit)(vec![Ack]).await.unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut seen = Vec::new();
    while let Ok(Ok(batch)) = tokio::time::timeout(
        std::time::Duration::from_millis(700),
        source.receive_batch(8),
    )
    .await
    {
        let acks = vec![Ack; batch.messages.len()];
        seen.extend(
            batch
                .messages
                .iter()
                .map(|m| String::from_utf8_lossy(&m.payload).into_owned()),
        );
        (batch.commit)(acks).await.unwrap();
        if seen.len() > 4 {
            break;
        }
    }
    assert_eq!(seen, ["{\"b\":2}", "{\"c\":3}"]);
}
