//! ZeroMQ endpoints. Two interchangeable backends share the framing/format
//! `codec`:
//!
//! * [`zmq`] — the `zeromq` crate (zmq.rs), behind the `zeromq` feature.
//! * [`omq`] — `omq-tokio` (omq.rs), behind the `zeromq-omq` feature (MSRV 1.93 /
//!   edition 2024, so it is in `full` but not `portable`). Faster on the
//!   per-message `raw`/`raw_framed` path.
//!
//! `backend: try_omq` (the default) picks omq when it is compiled in and falls
//! back to zmq otherwise; naming a backend explicitly makes it a requirement.
//!
//! The backend types are re-exported here so callers keep using
//! `endpoints::zeromq::{ZeroMqConsumer, ZeroMqPublisher, ...}`.

#[cfg(any(feature = "zeromq", feature = "zeromq-omq"))]
pub(crate) mod codec;
#[cfg(feature = "zeromq-omq")]
pub mod omq;
#[cfg(feature = "zeromq")]
pub mod zmq;

#[cfg(feature = "zeromq-omq")]
pub use omq::{ZeroMqOmqConsumer, ZeroMqOmqPublisher};
#[cfg(feature = "zeromq")]
pub use zmq::{ZeroMqConsumer, ZeroMqPublisher};

use crate::models::{ZeroMqBackend, ZeroMqConfig};
use crate::traits::{MessageConsumer, MessagePublisher};
use anyhow::Result;

/// A backend actually to build with — `try_omq` is already resolved away.
#[derive(Debug, PartialEq, Eq)]
enum Resolved {
    Zmq,
    Omq,
}

/// Resolve the configured backend to a concrete one.
///
/// `try_omq` (the default) prefers omq and falls back to zmq when omq isn't
/// compiled in, so the same config runs on a build without `zeromq-omq`. An
/// explicitly named backend is a hard requirement and is kept as-is, so a
/// missing feature stays a startup error instead of silently changing backend.
fn resolve_backend(backend: &ZeroMqBackend) -> Resolved {
    match backend {
        ZeroMqBackend::Zmq => Resolved::Zmq,
        ZeroMqBackend::Omq => Resolved::Omq,
        ZeroMqBackend::TryOmq => {
            if cfg!(feature = "zeromq-omq") {
                Resolved::Omq
            } else {
                Resolved::Zmq
            }
        }
    }
}

/// Build a ZeroMQ consumer for the configured backend (`zmq` or `omq`). Each
/// backend is behind its own build feature; explicitly requesting one that
/// wasn't compiled in is a clear config error rather than a silent fallback.
pub(crate) async fn create_consumer(cfg: &ZeroMqConfig) -> Result<Box<dyn MessageConsumer>> {
    match resolve_backend(&cfg.backend) {
        Resolved::Zmq => {
            #[cfg(feature = "zeromq")]
            return Ok(Box::new(zmq::ZeroMqConsumer::new(cfg).await?) as Box<dyn MessageConsumer>);
            #[cfg(not(feature = "zeromq"))]
            return Err(anyhow::anyhow!(
                "ZeroMQ backend 'zmq' requires the `zeromq` build feature"
            ));
        }
        Resolved::Omq => {
            #[cfg(feature = "zeromq-omq")]
            return Ok(
                Box::new(omq::ZeroMqOmqConsumer::new(cfg).await?) as Box<dyn MessageConsumer>
            );
            #[cfg(not(feature = "zeromq-omq"))]
            return Err(anyhow::anyhow!(
                "ZeroMQ backend 'omq' requires the `zeromq-omq` build feature"
            ));
        }
    }
}

/// Publisher counterpart to [`create_consumer`].
pub(crate) async fn create_publisher(cfg: &ZeroMqConfig) -> Result<Box<dyn MessagePublisher>> {
    match resolve_backend(&cfg.backend) {
        Resolved::Zmq => {
            #[cfg(feature = "zeromq")]
            return Ok(Box::new(zmq::ZeroMqPublisher::new(cfg).await?) as Box<dyn MessagePublisher>);
            #[cfg(not(feature = "zeromq"))]
            return Err(anyhow::anyhow!(
                "ZeroMQ backend 'zmq' requires the `zeromq` build feature"
            ));
        }
        Resolved::Omq => {
            #[cfg(feature = "zeromq-omq")]
            return Ok(
                Box::new(omq::ZeroMqOmqPublisher::new(cfg).await?) as Box<dyn MessagePublisher>
            );
            #[cfg(not(feature = "zeromq-omq"))]
            return Err(anyhow::anyhow!(
                "ZeroMQ backend 'omq' requires the `zeromq-omq` build feature"
            ));
        }
    }
}

#[cfg(all(test, feature = "zeromq", feature = "zeromq-omq"))]
mod tests {
    //! The same scenarios against both backends.
    use super::*;
    use crate::models::{ZeroMqFormat, ZeroMqSocketType};
    use crate::traits::MessageDisposition;
    use crate::CanonicalMessage;
    use std::time::Duration;

    fn free_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("tcp://{}", listener.local_addr().unwrap())
    }

    fn config(
        backend: &ZeroMqBackend,
        url: &str,
        socket_type: ZeroMqSocketType,
        bind: bool,
    ) -> ZeroMqConfig {
        ZeroMqConfig {
            url: url.to_string(),
            socket_type: Some(socket_type),
            bind,
            backend: backend.clone(),
            ..Default::default()
        }
    }

    async fn receive_one(
        consumer: &mut Box<dyn MessageConsumer>,
        wait: Duration,
    ) -> Option<CanonicalMessage> {
        let batch = tokio::time::timeout(wait, consumer.receive_batch(1))
            .await
            .ok()?
            .ok()?;
        let message = batch.messages.into_iter().next()?;
        let _ = (batch.commit)(vec![MessageDisposition::Ack]).await;
        Some(message)
    }

    /// Sends until a message arrives: what is sent before the peers know each other may be lost.
    async fn arrives(
        publisher: &dyn MessagePublisher,
        consumer: &mut Box<dyn MessageConsumer>,
        mut message: CanonicalMessage,
    ) -> Option<CanonicalMessage> {
        message.metadata.insert("kind".into(), "order".into());
        for _ in 0..40 {
            let _ = publisher.send_batch(vec![message.clone()]).await;
            if let Some(got) = receive_one(consumer, Duration::from_millis(250)).await {
                return Some(got);
            }
        }
        None
    }

    async fn pub_sub_keeps_metadata(backend: ZeroMqBackend) {
        let url = free_url();
        let publisher = create_publisher(&config(&backend, &url, ZeroMqSocketType::Pub, true))
            .await
            .unwrap();
        let mut consumer = create_consumer(&config(&backend, &url, ZeroMqSocketType::Sub, false))
            .await
            .unwrap();
        let got = arrives(publisher.as_ref(), &mut consumer, "body".into())
            .await
            .expect("nothing arrived");
        assert_eq!(got.get_payload_str(), "body");
        assert_eq!(got.metadata.get("kind").map(String::as_str), Some("order"));
    }

    #[tokio::test]
    async fn pub_sub_keeps_metadata_zmq() {
        pub_sub_keeps_metadata(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn pub_sub_keeps_metadata_omq() {
        pub_sub_keeps_metadata(ZeroMqBackend::Omq).await;
    }

    async fn sub_filters_on_the_pub_topic(backend: ZeroMqBackend) {
        let url = free_url();
        let mut publisher_config = config(&backend, &url, ZeroMqSocketType::Pub, true);
        publisher_config.topic = Some("orders".into());
        let publisher = create_publisher(&publisher_config).await.unwrap();
        let mut subscribed = Vec::new();
        for topic in ["orders", "invoices"] {
            let mut sub = config(&backend, &url, ZeroMqSocketType::Sub, false);
            sub.topic = Some(topic.into());
            subscribed.push(create_consumer(&sub).await.unwrap());
        }
        let got = arrives(publisher.as_ref(), &mut subscribed[0], "body".into())
            .await
            .expect("nothing arrived");
        assert_eq!(got.metadata.get("kind").map(String::as_str), Some("order"));
        let other = receive_one(&mut subscribed[1], Duration::from_millis(300)).await;
        assert!(other.is_none(), "got a message of another topic");
    }

    #[tokio::test]
    async fn sub_filters_on_the_pub_topic_zmq() {
        sub_filters_on_the_pub_topic(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn sub_filters_on_the_pub_topic_omq() {
        sub_filters_on_the_pub_topic(ZeroMqBackend::Omq).await;
    }

    async fn dropped_consumer_frees_its_address(backend: ZeroMqBackend) {
        let url = free_url();
        let cfg = config(&backend, &url, ZeroMqSocketType::Pull, true);
        drop(create_consumer(&cfg).await.unwrap());
        let mut last = None;
        for _ in 0..20 {
            match create_consumer(&cfg).await {
                Ok(_) => return,
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("address still taken: {last:?}");
    }

    #[tokio::test]
    async fn dropped_consumer_frees_its_address_zmq() {
        dropped_consumer_frees_its_address(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn dropped_consumer_frees_its_address_omq() {
        dropped_consumer_frees_its_address(ZeroMqBackend::Omq).await;
    }

    async fn push_survives_a_peer_restart(backend: ZeroMqBackend) {
        let url = free_url();
        let pull = config(&backend, &url, ZeroMqSocketType::Pull, true);
        let mut consumer = create_consumer(&pull).await.unwrap();
        let publisher = create_publisher(&config(&backend, &url, ZeroMqSocketType::Push, false))
            .await
            .unwrap();
        assert!(arrives(publisher.as_ref(), &mut consumer, "one".into())
            .await
            .is_some());
        drop(consumer);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut consumer = None;
        for _ in 0..40 {
            // Keep sending: this is what frees the address on the old backend.
            let _ = publisher.send_batch(vec!["x".into()]).await;
            if let Ok(c) = create_consumer(&pull).await {
                consumer = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut consumer = consumer.expect("could not bind again");
        assert!(
            arrives(publisher.as_ref(), &mut consumer, "two".into())
                .await
                .is_some(),
            "nothing arrives after the peer came back"
        );
    }

    #[tokio::test]
    async fn push_survives_a_peer_restart_zmq() {
        push_survives_a_peer_restart(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn push_survives_a_peer_restart_omq() {
        push_survives_a_peer_restart(ZeroMqBackend::Omq).await;
    }

    async fn pub_survives_a_peer_restart(backend: ZeroMqBackend) {
        let url = free_url();
        let sub = config(&backend, &url, ZeroMqSocketType::Sub, true);
        let mut consumer = create_consumer(&sub).await.unwrap();
        let publisher = create_publisher(&config(&backend, &url, ZeroMqSocketType::Pub, false))
            .await
            .unwrap();
        assert!(arrives(publisher.as_ref(), &mut consumer, "one".into())
            .await
            .is_some());
        drop(consumer);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut consumer = create_consumer(&sub).await.expect("could not bind again");
        assert!(
            arrives(publisher.as_ref(), &mut consumer, "two".into())
                .await
                .is_some(),
            "nothing arrives after the peer came back"
        );
    }

    #[tokio::test]
    async fn pub_survives_a_peer_restart_zmq() {
        pub_survives_a_peer_restart(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn pub_survives_a_peer_restart_omq() {
        pub_survives_a_peer_restart(ZeroMqBackend::Omq).await;
    }

    async fn pull_survives_a_peer_restart(backend: ZeroMqBackend) {
        let url = free_url();
        let push = config(&backend, &url, ZeroMqSocketType::Push, true);
        let publisher = create_publisher(&push).await.unwrap();
        let mut consumer = create_consumer(&config(&backend, &url, ZeroMqSocketType::Pull, false))
            .await
            .unwrap();
        assert!(arrives(publisher.as_ref(), &mut consumer, "one".into())
            .await
            .is_some());
        drop(publisher);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let publisher = create_publisher(&push).await.expect("could not bind again");
        let mut errors = Vec::new();
        for _ in 0..40 {
            let _ = publisher.send_batch(vec!["two".into()]).await;
            match tokio::time::timeout(Duration::from_millis(250), consumer.receive_batch(1)).await
            {
                Ok(Ok(batch)) if !batch.messages.is_empty() => return,
                Ok(Err(e)) => errors.push(e.to_string()),
                _ => {}
            }
        }
        panic!("nothing arrives after the peer came back; errors: {errors:?}");
    }

    // No `zmq` variant: zmq.rs does not reconnect a PULL socket and reports no lost peer.
    #[tokio::test]
    async fn pull_survives_a_peer_restart_omq() {
        pull_survives_a_peer_restart(ZeroMqBackend::Omq).await;
    }

    async fn bound_req_recovers_after_a_timeout(backend: ZeroMqBackend) {
        let url = free_url();
        let mut req = config(&backend, &url, ZeroMqSocketType::Req, true);
        req.request_timeout_ms = Some(300);
        req.format = ZeroMqFormat::Json;
        let publisher = create_publisher(&req).await.unwrap();
        let mut rep = config(&backend, &url, ZeroMqSocketType::Rep, false);
        rep.format = ZeroMqFormat::Json;
        let mut consumer = create_consumer(&rep).await.unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;
        // First request: the responder holds it past the timeout.
        let first = publisher.send_batch(vec!["one".into()]);
        let held = async {
            let batch = consumer.receive_batch(1).await.unwrap();
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(batch);
        };
        let (first, ()) = tokio::join!(first, held);
        assert!(first.is_err(), "expected a timeout, got {first:?}");
        drop(consumer);

        let mut consumer = create_consumer(&rep).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let answer = async {
            loop {
                let Ok(batch) = consumer.receive_batch(1).await else {
                    continue;
                };
                let replies = vec![MessageDisposition::Reply("pong".into()); batch.messages.len()];
                let _ = (batch.commit)(replies).await;
            }
        };
        let requests = async {
            let mut outcomes = Vec::new();
            for _ in 0..10 {
                let sent = format!("{:?}", publisher.send_batch(vec!["two".into()]).await);
                if sent.contains("pong") {
                    return Ok(());
                }
                outcomes.push(sent);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(outcomes)
        };
        tokio::select! {
            () = answer => unreachable!(),
            outcome = requests => outcome.expect("the REQ publisher never recovered"),
        }
    }

    #[tokio::test]
    async fn bound_req_recovers_after_a_timeout_zmq() {
        bound_req_recovers_after_a_timeout(ZeroMqBackend::Zmq).await;
    }
    #[tokio::test]
    async fn bound_req_recovers_after_a_timeout_omq() {
        bound_req_recovers_after_a_timeout(ZeroMqBackend::Omq).await;
    }
}
