//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The `sequence` input: read several endpoints one after another.
//!
//! Each endpoint is drained before the next begins, and the last one streams until
//! the route stops. The motivating case is a change-data-capture backfill — snapshot
//! a table, then tail its replication stream — but the shape is the same for any
//! history-then-live pair: a file backfill before a tail, an object-store prefix
//! before a topic.
//!
//! Gaplessness comes from ordering: before the first phase reads a single message,
//! every later phase is asked to pin the position it will resume from (see
//! `pin_resume_position`). For Postgres CDC that creates the replication slot, so the
//! server retains WAL from that moment while the snapshot runs.

use crate::checkpoint::CheckpointStore;
use crate::models::{Endpoint, SequenceConfig};
use crate::outcomes::ReceivedBatch;
use crate::traits::{BoxFuture, ConsumerError, EndpointStatus, MessageConsumer};
use anyhow::anyhow;
use async_trait::async_trait;
use std::any::Any;
use std::sync::Arc;
use tracing::{info, warn};

pub struct SequenceConsumer {
    route_name: String,
    endpoints: Vec<Endpoint>,
    /// Index into `endpoints` of the phase being read now.
    phase: usize,
    /// Built on first use, so a later phase opens no connection while an earlier one runs.
    current: Option<Box<dyn MessageConsumer>>,
    /// The route's own drain intent, forwarded only to the last phase.
    exit_on_empty: bool,
    marker: Option<Arc<dyn CheckpointStore>>,
}

impl SequenceConsumer {
    pub async fn new(route_name: &str, config: &SequenceConfig) -> anyhow::Result<Self> {
        if config.endpoints.is_empty() {
            return Err(anyhow!(
                "[route:{route_name}] sequence needs at least one endpoint"
            ));
        }

        let marker = match (&config.cursor_id, &config.checkpoint_store) {
            (Some(cid), Some(spec)) => {
                Some(match crate::checkpoint::parse_checkpoint_store(spec)? {
                    // A `sequence` has no source datastore to keep the marker in, so a
                    // schemeless spec names a local file — same as `postgres_cdc` reads it.
                    crate::checkpoint::CheckpointBackend::Source { .. } => {
                        Arc::new(crate::checkpoint::FileCheckpointStore::new(
                            spec.clone(),
                            crate::checkpoint::checkpoint_key("sequence", cid),
                        )) as Arc<dyn CheckpointStore>
                    }
                    external => {
                        crate::checkpoint::build_external_store(external, "sequence", cid).await?
                    }
                })
            }
            (None, Some(_)) => {
                return Err(anyhow!(
                    "[route:{route_name}] sequence checkpoint_store needs a cursor_id to key the phase marker"
                ));
            }
            (Some(_), None) => {
                return Err(anyhow!(
                    "[route:{route_name}] sequence cursor_id needs a checkpoint_store to persist the phase marker"
                ));
            }
            (None, None) => None,
        };

        let phase = match &marker {
            Some(store) => Self::load_phase(store.as_ref(), config.endpoints.len()).await?,
            None => 0,
        };

        // Pin every phase that has yet to run, before the current one reads anything.
        for endpoint in &config.endpoints[phase + 1..] {
            crate::endpoints::pin_resume_position(route_name, endpoint).await?;
        }

        if phase > 0 {
            info!(
                route = %route_name,
                phase = phase + 1,
                of = config.endpoints.len(),
                "sequence: resuming at a later phase from the stored marker"
            );
        }

        Ok(Self {
            route_name: route_name.to_string(),
            endpoints: config.endpoints.clone(),
            phase,
            current: None,
            exit_on_empty: false,
            marker,
        })
    }

    async fn load_phase(store: &dyn CheckpointStore, len: usize) -> anyhow::Result<usize> {
        let Some(raw) = store.load().await? else {
            return Ok(0);
        };
        match raw.trim().parse::<usize>() {
            // A marker written by a longer sequence must not index past this one.
            Ok(phase) if phase < len => Ok(phase),
            Ok(phase) => {
                warn!(
                    stored = phase,
                    endpoints = len,
                    "sequence: stored phase is out of range for this sequence; starting over"
                );
                Ok(0)
            }
            Err(e) => {
                warn!(value = %raw, error = %e, "sequence: ignoring unparseable phase marker");
                Ok(0)
            }
        }
    }

    fn is_last_phase(&self) -> bool {
        self.phase + 1 >= self.endpoints.len()
    }

    /// Builds the current phase's consumer. An intermediate phase always drains — it has
    /// to report empty for the handoff to fire — while the last phase inherits the route's
    /// intent so a streaming source still blocks.
    async fn open_current(&mut self) -> Result<&mut Box<dyn MessageConsumer>, ConsumerError> {
        if self.current.is_none() {
            let endpoint = &self.endpoints[self.phase];
            let mut consumer =
                crate::endpoints::create_consumer_from_route(&self.route_name, endpoint)
                    .await
                    .map_err(|e| {
                        ConsumerError::Permanent(anyhow!(
                            "[route:{}] sequence phase {} of {} ({}) failed to start: {e}",
                            self.route_name,
                            self.phase + 1,
                            self.endpoints.len(),
                            endpoint.endpoint_type.name()
                        ))
                    })?;
            let drain = if self.is_last_phase() {
                self.exit_on_empty
            } else {
                true
            };
            consumer.set_exit_on_empty(drain);
            if let Some(hook) = consumer.on_connect_hook() {
                hook.await.map_err(ConsumerError::Permanent)?;
            }
            info!(
                route = %self.route_name,
                phase = self.phase + 1,
                of = self.endpoints.len(),
                endpoint = endpoint.endpoint_type.name(),
                "sequence: phase started"
            );
            self.current = Some(consumer);
        }
        Ok(self.current.as_mut().expect("just built"))
    }

    /// Closes the drained phase and records the next one, so a restart does not replay it.
    async fn advance(&mut self) {
        if let Some(mut consumer) = self.current.take() {
            if let Err(e) = consumer.close().await {
                warn!(
                    route = %self.route_name,
                    phase = self.phase + 1,
                    error = %e,
                    "sequence: closing a drained phase failed; continuing to the next"
                );
            }
        }
        self.phase += 1;
        info!(
            route = %self.route_name,
            phase = self.phase + 1,
            of = self.endpoints.len(),
            "sequence: previous phase drained, handing off"
        );
        if let Some(store) = &self.marker {
            if let Err(e) = store.save(&self.phase.to_string()).await {
                // The handoff itself still holds: the next phase resumes from the position
                // pinned before the run. Only restart-skipping is lost, so this is not fatal.
                warn!(
                    route = %self.route_name,
                    error = %e,
                    "sequence: saving the phase marker failed; a restart will replay from phase 1"
                );
            }
        }
    }
}

#[async_trait]
impl MessageConsumer for SequenceConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let last = self.is_last_phase();
            let consumer = self.open_current().await?;
            match consumer.receive_batch(max_messages).await {
                // An empty batch from an intermediate phase is the handoff signal, not a
                // drained route — the route would stop if this reached it.
                Ok(batch) if batch.messages.is_empty() && !last => {
                    self.advance().await;
                }
                Ok(batch) => return Ok(batch),
                Err(ConsumerError::EndOfStream) if !last => {
                    self.advance().await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
        if self.is_last_phase() {
            if let Some(consumer) = self.current.as_mut() {
                consumer.set_exit_on_empty(exit_on_empty);
            }
        }
    }

    /// Conservative: the phases are built one at a time, so the ones not yet open cannot
    /// be asked. `true` is the safe answer for all of them.
    fn commit_requires_order(&self) -> bool {
        true
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        let hook = self.current.as_ref().and_then(|c| c.on_disconnect_hook())?;
        Some(hook)
    }

    async fn status(&self) -> EndpointStatus {
        match &self.current {
            Some(consumer) => consumer.status().await,
            None => EndpointStatus {
                healthy: true,
                ..Default::default()
            },
        }
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        match self.current.take() {
            Some(mut consumer) => consumer.close().await,
            None => Ok(()),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryPublisher;
    use crate::models::{EndpointType, MemoryConfig};
    use crate::traits::MessagePublisher;
    use crate::CanonicalMessage;

    fn memory_endpoint(topic: &str) -> Endpoint {
        Endpoint::new(EndpointType::Memory(MemoryConfig {
            topic: topic.to_string(),
            capacity: Some(64),
            ..Default::default()
        }))
    }

    fn config(endpoints: Vec<Endpoint>) -> SequenceConfig {
        SequenceConfig {
            endpoints,
            cursor_id: None,
            checkpoint_store: None,
        }
    }

    async fn seed(topic: &str, payloads: &[&str]) {
        let publisher = MemoryPublisher::new_local(topic, 64);
        for p in payloads {
            publisher.send(CanonicalMessage::from(*p)).await.unwrap();
        }
    }

    /// Reads until `want` messages have arrived, so the 1s drain handoff between phases
    /// does not make the test depend on timing.
    async fn drain(consumer: &mut SequenceConsumer, want: usize) -> Vec<String> {
        let mut got = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while got.len() < want {
                let batch = consumer.receive_batch(8).await.unwrap();
                for m in &batch.messages {
                    got.push(m.get_payload_str().into_owned());
                }
                if batch.messages.is_empty() {
                    break;
                }
            }
        })
        .await
        .expect("sequence must not hang");
        got
    }

    #[tokio::test]
    async fn a_drained_phase_hands_off_to_the_next() {
        seed("seq_handoff_a", &["a1", "a2"]).await;
        seed("seq_handoff_b", &["b1"]).await;

        let cfg = config(vec![
            memory_endpoint("seq_handoff_a"),
            memory_endpoint("seq_handoff_b"),
        ]);
        let mut consumer = SequenceConsumer::new("t", &cfg).await.unwrap();

        // The snapshot phase must come out before the stream phase, in order.
        assert_eq!(drain(&mut consumer, 3).await, ["a1", "a2", "b1"]);
    }

    #[tokio::test]
    async fn the_last_phase_reports_empty_so_the_route_can_drain() {
        let cfg = config(vec![
            memory_endpoint("seq_drain_a"),
            memory_endpoint("seq_drain_b"),
        ]);
        let mut consumer = SequenceConsumer::new("t", &cfg).await.unwrap();
        consumer.set_exit_on_empty(true);

        let batch = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            consumer.receive_batch(8),
        )
        .await
        .expect("a fully drained sequence must return, not loop")
        .unwrap();
        assert!(batch.messages.is_empty());
    }

    #[tokio::test]
    async fn the_phase_marker_skips_phases_already_done() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("marker.json").to_str().unwrap().to_string();
        let cfg = SequenceConfig {
            endpoints: vec![
                memory_endpoint("seq_marker_a"),
                memory_endpoint("seq_marker_b"),
            ],
            cursor_id: Some("seq_marker".to_string()),
            checkpoint_store: Some(store),
        };

        seed("seq_marker_a", &["a1"]).await;
        seed("seq_marker_b", &["b1"]).await;
        let mut first = SequenceConsumer::new("t", &cfg).await.unwrap();
        assert_eq!(drain(&mut first, 2).await, ["a1", "b1"]);
        drop(first);

        // Phase 1 is recorded as done, so a restart must not read it again even though
        // there is fresh data waiting there.
        seed("seq_marker_a", &["a2"]).await;
        seed("seq_marker_b", &["b2"]).await;
        let mut second = SequenceConsumer::new("t", &cfg).await.unwrap();
        assert_eq!(drain(&mut second, 1).await, ["b2"]);
    }

    #[tokio::test]
    async fn an_empty_sequence_is_rejected() {
        let err = SequenceConsumer::new("t", &config(vec![]))
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one endpoint"), "{err}");
    }

    #[tokio::test]
    async fn a_marker_needs_both_halves_of_its_config() {
        let mut cfg = config(vec![memory_endpoint("seq_half")]);
        cfg.cursor_id = Some("x".to_string());
        let err = SequenceConsumer::new("t", &cfg)
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("checkpoint_store"), "{err}");

        let mut cfg = config(vec![memory_endpoint("seq_half")]);
        cfg.checkpoint_store = Some("/tmp/ignored.json".to_string());
        let err = SequenceConsumer::new("t", &cfg)
            .await
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("cursor_id"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_phase_names_itself() {
        // `switch` is output-only, so building it as a phase fails; the error should say
        // which phase of the sequence could not start.
        let cfg = config(vec![Endpoint::new(EndpointType::Switch(
            crate::models::SwitchConfig {
                metadata_key: String::new(),
                cases: Default::default(),
                when: Vec::new(),
                default: None,
            },
        ))]);
        let mut consumer = SequenceConsumer::new("t", &cfg).await.unwrap();
        let err = consumer.receive_batch(1).await.unwrap_err().to_string();
        assert!(err.contains("phase 1 of 1"), "{err}");
    }

    #[test]
    fn sequence_is_input_only() {
        let endpoint = Endpoint::new(EndpointType::Sequence(config(vec![memory_endpoint(
            "seq_out",
        )])));
        let err = crate::endpoints::check_publisher("t", &endpoint, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("only supported as an input"), "{err}");
    }

    // `postgres_cdc` desugars `capture_all` onto a sequence, so the same guardrail has to hold
    // there — otherwise the ergonomic spelling would be the unsafe one.
    #[cfg(feature = "postgres-cdc")]
    #[test]
    fn capture_all_rejects_a_temporary_slot_too() {
        use crate::models::{PostgresCdcConfig, PostgresConsume};

        let endpoint = |consume, temporary_slot| {
            Endpoint::new(EndpointType::PostgresCdc(PostgresCdcConfig {
                url: "postgres://localhost/db".to_string(),
                publication: "pub1".to_string(),
                consume: Some(consume),
                temporary_slot,
                ..Default::default()
            }))
        };

        let err = crate::endpoints::check_consumer(
            "t",
            &endpoint(PostgresConsume::CaptureAll, true),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("temporary_slot"), "{err}");

        // `capture_new` reads nothing before the stream, so an ephemeral slot is still fine.
        crate::endpoints::check_consumer("t", &endpoint(PostgresConsume::CaptureNew, true), None)
            .expect("capture_new may use a temporary slot");
    }

    // The default must stay `capture_new`: an upgrade that silently started backfilling whole
    // tables would be a behaviour change, not a new feature.
    #[cfg(feature = "postgres-cdc")]
    #[test]
    fn postgres_cdc_still_defaults_to_capture_new() {
        use crate::models::{PostgresCdcConfig, PostgresConsume};

        let cfg = PostgresCdcConfig::default();
        assert_eq!(cfg.resolved_consume(), PostgresConsume::CaptureNew);

        let parsed: PostgresCdcConfig = serde_json::from_str(
            r#"{"url":"postgres://localhost/db","publication":"p","consume":"capture_all"}"#,
        )
        .expect("consume should deserialize in snake_case");
        assert_eq!(parsed.resolved_consume(), PostgresConsume::CaptureAll);
    }

    #[cfg(feature = "postgres-cdc")]
    #[test]
    fn a_temporary_slot_cannot_follow_an_earlier_phase() {
        let cdc = |temporary_slot| {
            Endpoint::new(EndpointType::PostgresCdc(
                crate::models::PostgresCdcConfig {
                    url: "postgres://localhost/db".to_string(),
                    publication: "pub1".to_string(),
                    temporary_slot,
                    ..Default::default()
                },
            ))
        };
        let endpoint = Endpoint::new(EndpointType::Sequence(config(vec![
            memory_endpoint("seq_pg"),
            cdc(true),
        ])));
        let err = crate::endpoints::check_consumer("t", &endpoint, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("temporary_slot"), "{err}");

        // A permanent slot is the supported spelling and must pass the config check.
        let ok = Endpoint::new(EndpointType::Sequence(config(vec![
            memory_endpoint("seq_pg"),
            cdc(false),
        ])));
        crate::endpoints::check_consumer("t", &ok, None).expect("a permanent slot is allowed");
    }
}
