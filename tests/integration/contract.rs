//! The source rows of `docs/DELIVERY.md` against real brokers. Library test (not `mqb`),
//! `#[ignore]`d: each `test_<broker>_contract` starts
//! `tests/integration/docker-compose/<broker>.yml` and runs these steps, printing each:
//! 1. nack: a nacked message comes back, `message_id` unchanged where the table says so
//! 2. crash: a message held by a consumer that died comes back
//! 3. mixed acks: nacks in between acks come back, earlier acks do not
//! 4. metadata: a metadata entry survives the hop (`keeps_metadata`)
//! 5. backpressure: with half a backlog held unacked, the rest still arrives once it is acked
//! 6. route stop: a route on the source delivers a message and stops within `STOP`
//! 7. order: messages sent one at a time arrive in that order
//! 8. sink refuses: a message the sink fails as retryable stays in the source
//!
//! `MQB_TEST_BACKEND=nats cargo test --test integration_test --features test-utils,nats \
//!  -- --ignored --nocapture integration::contract::test_nats_contract`

#![allow(dead_code, unused_imports)]

use futures::future::BoxFuture;
use mq_bridge::endpoints::{create_consumer_from_route, create_publisher_from_route};
use mq_bridge::extensions::register_endpoint_factory;
use mq_bridge::models::Endpoint;
use mq_bridge::route::RouteHandle;
use mq_bridge::test_utils::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{
    CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError,
    SentBatch,
};
use mq_bridge::{CanonicalMessage, Route};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(90);
const IDLE: Duration = Duration::from_secs(5);
const STOP: Duration = Duration::from_secs(10);

/// One broker under test. Built with [`Contract::new`]; the flags default to `true`.
pub struct Contract {
    /// Prefix of every printed line, and the `MQB_TEST_BACKEND` name that selects the test.
    pub name: &'static str,
    /// Compose file that starts the broker; it is torn down with `down -v` before and after.
    /// Empty for a source that needs no broker.
    pub compose: &'static str,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    /// "`message_id` stable across replay" in the sources table.
    pub stable_id: bool,
    /// The id the publisher was given is the one the consumer reports.
    pub hop_keeps_id: bool,
    /// Metadata set on the publisher arrives at the consumer.
    pub keeps_metadata: bool,
    /// The source hands messages out in the order they were sent.
    pub ordered: bool,
    /// Runs once the broker is up, before the source opens, e.g. to create a queue.
    pub setup: Option<fn() -> BoxFuture<'static, ()>>,
    /// Lets the lock a dead consumer left lapse now instead of after its lease.
    pub expire_locks: Option<fn() -> BoxFuture<'static, ()>>,
}

impl Contract {
    pub fn new(
        name: &'static str,
        compose: &'static str,
        input: serde_json::Value,
        output: serde_json::Value,
    ) -> Self {
        Self {
            name,
            compose,
            input,
            output,
            stable_id: true,
            hop_keeps_id: true,
            keeps_metadata: true,
            ordered: true,
            setup: None,
            expire_locks: None,
        }
    }
}

/// The payload as sent. A change-capture source carries it as the `payload` column of a row,
/// so any JSON object with a `payload` string is read that way; contract payloads are never JSON.
fn text(message: &CanonicalMessage) -> String {
    use base64::Engine;
    let raw = message.get_payload_str();
    let row: Option<serde_json::Value> = raw
        .starts_with('{')
        .then(|| serde_json::from_str(&raw).ok())
        .flatten();
    match row.as_ref().and_then(|row| row["payload"].as_str()) {
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default(),
        None => raw.into_owned(),
    }
}

struct Source {
    name: &'static str,
    endpoint: Endpoint,
    consumer: Option<Box<dyn MessageConsumer>>,
}

impl Source {
    async fn open(&mut self) -> &mut Box<dyn MessageConsumer> {
        let deadline = Instant::now() + LIMIT;
        while self.consumer.is_none() {
            match create_consumer_from_route("contract", &self.endpoint).await {
                Ok(consumer) => self.consumer = Some(consumer),
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "[{}] no consumer: {error:#}",
                        self.name
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        self.consumer.as_mut().unwrap()
    }

    /// Closes the consumer as a route does before it connects again.
    async fn reconnect(&mut self) {
        if let Some(mut consumer) = self.consumer.take() {
            match tokio::time::timeout(STOP, consumer.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => println!("[{}] close failed: {error:#}", self.name),
                Err(_) => println!("[{}] close took over {STOP:?}", self.name),
            }
        }
    }

    /// Reads until each payload arrived, settling it with `disposition`. A nack ends with
    /// the first batch; `None` drops batch and consumer unsettled, as a crash does.
    async fn deliveries(
        &mut self,
        step: &str,
        payloads: &[String],
        disposition: Option<MessageDisposition>,
    ) -> HashMap<String, u128> {
        let name = self.name;
        let deadline = Instant::now() + LIMIT;
        let mut seen = HashMap::new();
        let mut idle = 0;
        while seen.len() < payloads.len() {
            assert!(
                Instant::now() < deadline,
                "[{name}] {step}: {} of {} arrived",
                seen.len(),
                payloads.len()
            );
            let consumer = self.open().await;
            let batch = match tokio::time::timeout(IDLE, consumer.receive_batch(16)).await {
                Ok(Ok(batch)) => batch,
                // What a route does with a failed read: connect again.
                Ok(Err(error)) => {
                    println!("[{name}] {step}: reconnecting after `{error}`");
                    self.reconnect().await;
                    continue;
                }
                Err(_) => {
                    idle += 1;
                    if idle == 2 {
                        println!("[{name}] {step}: nothing for 10 s, reconnecting");
                        self.reconnect().await;
                        idle = 0;
                    }
                    continue;
                }
            };
            idle = 0;
            let mut dispositions = Vec::with_capacity(batch.messages.len());
            for message in &batch.messages {
                let payload = text(message);
                if payloads.contains(&payload) {
                    seen.insert(payload, message.message_id);
                    dispositions.push(disposition.clone());
                } else {
                    dispositions.push(Some(MessageDisposition::Ack));
                }
            }
            if dispositions.iter().any(Option::is_none) {
                drop(batch);
                self.consumer = None;
                return seen;
            }
            let dispositions = dispositions.into_iter().flatten().collect();
            if let Err(error) = (batch.commit)(dispositions).await {
                println!("[{name}] {step}: commit failed with `{error:#}`, reconnecting");
                self.reconnect().await;
            }
            // A log replays from the nacked batch, so nacking on would never get past it.
            if matches!(disposition, Some(MessageDisposition::Nack)) && !seen.is_empty() {
                return seen;
            }
        }
        seen
    }

    /// Reads batches, settling each message as `settle` says, until it reports done.
    async fn read(
        &mut self,
        step: &str,
        mut settle: impl FnMut(&CanonicalMessage) -> (MessageDisposition, bool),
    ) {
        let name = self.name;
        let deadline = Instant::now() + LIMIT;
        let mut done = false;
        let mut idle = 0;
        while !done {
            assert!(Instant::now() < deadline, "[{name}] {step}: timed out");
            let consumer = self.open().await;
            let batch = match tokio::time::timeout(IDLE, consumer.receive_batch(16)).await {
                Ok(Ok(batch)) => batch,
                Ok(Err(error)) => {
                    println!("[{name}] {step}: reconnecting after `{error}`");
                    self.reconnect().await;
                    continue;
                }
                Err(_) => {
                    idle += 1;
                    if idle == 2 {
                        println!("[{name}] {step}: nothing for 10 s, reconnecting");
                        self.reconnect().await;
                        idle = 0;
                    }
                    continue;
                }
            };
            idle = 0;
            let mut dispositions = Vec::with_capacity(batch.messages.len());
            for message in &batch.messages {
                let (disposition, finished) = settle(message);
                dispositions.push(disposition);
                done |= finished;
            }
            if let Err(error) = (batch.commit)(dispositions).await {
                println!("[{name}] {step}: commit failed with `{error:#}`, reconnecting");
                self.reconnect().await;
            }
        }
    }
}

/// Sends one at a time under one `contract_key`, the record key of a partitioned output,
/// so a log holds them in this order.
async fn send_in_order(publisher: &dyn MessagePublisher, payloads: &[String], run: u128) {
    for payload in payloads {
        let message = CanonicalMessage::new(payload.clone().into_bytes(), None)
            .with_metadata_kv("contract_key", format!("{run:x}"));
        publisher.send(message).await.expect("publish");
    }
}

/// Acks A, B and D, nacks C and E, then restarts the consumer: C and E come back, and A
/// and B, acked before the first nack, do not. Catches a commit that skips past a nack.
async fn check_mixed_acks(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    let ids: Vec<String> = (0..5)
        .map(|i| format!("contract-{run:x}-mixed-{i}"))
        .collect();
    send_in_order(publisher, &ids, run).await;
    let nacked = |payload: &str| payload == ids[2] || payload == ids[4];

    let mut seen = HashSet::new();
    source
        .read("mixed acks", |message| {
            let payload = text(message);
            if !ids.contains(&payload) {
                return (MessageDisposition::Ack, false);
            }
            // A log replays from the nack, so a second sighting ends the step.
            let replay = !seen.insert(payload.clone());
            if replay || nacked(&payload) {
                (MessageDisposition::Nack, replay)
            } else {
                (MessageDisposition::Ack, seen.len() == ids.len())
            }
        })
        .await;

    source.reconnect().await;
    let mut back = HashSet::new();
    let mut acked_back = Vec::new();
    source
        .read("after mixed acks", |message| {
            let payload = text(message);
            if payload == ids[0] || payload == ids[1] {
                acked_back.push(payload);
            } else if nacked(&payload) {
                back.insert(payload);
            }
            (MessageDisposition::Ack, back.len() == 2)
        })
        .await;
    assert!(
        acked_back.is_empty(),
        "[{name}] acked messages came back: {acked_back:?}"
    );
}

/// A metadata entry set on the publisher arrives at the consumer.
async fn check_metadata(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    let payload = format!("contract-{run:x}-metadata");
    let tag = format!("{run:x}");
    let message = CanonicalMessage::new(payload.clone().into_bytes(), None)
        .with_metadata_kv("contract_tag", tag.clone());
    publisher.send_batch(vec![message]).await.expect("publish");
    let mut received = None;
    source
        .read("metadata", |message| {
            let done = text(message) == payload;
            if done {
                received = Some(message.get_metadata("contract_tag").map(str::to_string));
            }
            (MessageDisposition::Ack, done)
        })
        .await;
    assert_eq!(
        received,
        Some(Some(tag)),
        "[{name}] metadata did not survive the hop"
    );
}

/// Holds about half of a 40-message backlog unacked (fewer if the broker stops handing
/// out), pauses, acks it, then reads on: every message arrives, duplicates allowed.
async fn check_backpressure(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    let payloads: Vec<String> = (0..40)
        .map(|i| format!("contract-{run:x}-held-{i}"))
        .collect();
    let messages = payloads
        .iter()
        .map(|payload| CanonicalMessage::new(payload.clone().into_bytes(), None))
        .collect();
    publisher.send_batch(messages).await.expect("publish");

    let mut seen = HashSet::new();
    let mut held = Vec::new();
    let deadline = Instant::now() + LIMIT;
    while seen.len() < payloads.len() / 2 {
        assert!(
            Instant::now() < deadline,
            "[{name}] nothing was read while holding"
        );
        let consumer = source.open().await;
        match tokio::time::timeout(IDLE, consumer.receive_batch(8)).await {
            Ok(Ok(batch)) => {
                for message in &batch.messages {
                    let payload = text(message);
                    if payloads.contains(&payload) {
                        seen.insert(payload);
                    }
                }
                held.push(batch);
            }
            // Dropped unsettled, the held messages come back to the new consumer.
            Ok(Err(error)) => {
                println!("[{name}] backpressure: reconnecting after `{error}`");
                held.clear();
                seen.clear();
                source.reconnect().await;
            }
            // The broker stops handing out while too much is unacked.
            Err(_) if !held.is_empty() => break,
            Err(_) => {}
        }
    }
    let held_count = seen.len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for batch in held {
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        if let Err(error) = (batch.commit)(acks).await {
            println!("[{name}] backpressure: commit failed with `{error:#}`, reconnecting");
            source.reconnect().await;
            break;
        }
    }

    source
        .read("after the backlog", |message| {
            let payload = text(message);
            if payloads.contains(&payload) {
                seen.insert(payload);
            }
            (MessageDisposition::Ack, seen.len() == payloads.len())
        })
        .await;
    println!("[{name}] held {held_count} of {} unacked", payloads.len());
}

/// Runs a route from the source to memory, waits for one message to pass, then stops it.
async fn check_route_stops(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    // A second reader would compete with the route for the message.
    source.reconnect().await;
    let sink: Endpoint =
        serde_json::from_value(json!({ "memory": { "topic": format!("contract-route-{name}") } }))
            .expect("memory endpoint");
    let mut out = create_consumer_from_route("contract-out", &sink)
        .await
        .expect("memory consumer");
    let handle = Route::new(source.endpoint.clone(), sink)
        .run(&format!("contract-route-{name}"))
        .await
        .expect("route starts");

    let payload = format!("contract-{run:x}-route");
    let message = CanonicalMessage::new(payload.clone().into_bytes(), None);
    publisher.send_batch(vec![message]).await.expect("publish");
    let deadline = Instant::now() + LIMIT;
    loop {
        assert!(
            Instant::now() < deadline,
            "[{name}] the route delivered nothing"
        );
        let Ok(Ok(batch)) = tokio::time::timeout(IDLE, out.receive_batch(16)).await else {
            continue;
        };
        let found = batch.messages.iter().any(|m| text(m) == payload);
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        (batch.commit)(acks).await.expect("commit");
        if found {
            break;
        }
    }

    stop_route(name, handle).await;
}

async fn stop_route(name: &str, handle: RouteHandle) {
    let stopping = Instant::now();
    handle.stop().await;
    tokio::time::timeout(STOP, handle.join())
        .await
        .unwrap_or_else(|_| panic!("[{name}] the route did not stop within {STOP:?}"))
        .expect("route task");
    println!("[{name}] route stopped in {:.1?}", stopping.elapsed());
}

/// Ten messages sent one at a time arrive in that order; a replayed duplicate is ignored.
async fn check_order(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    let ids: Vec<String> = (0..10)
        .map(|i| format!("contract-{run:x}-order-{i}"))
        .collect();
    send_in_order(publisher, &ids, run).await;
    let mut arrived: Vec<String> = Vec::new();
    source
        .read("order", |message| {
            let payload = text(message);
            if ids.contains(&payload) && !arrived.contains(&payload) {
                arrived.push(payload);
            }
            (MessageDisposition::Ack, arrived.len() == ids.len())
        })
        .await;
    assert_eq!(arrived, ids, "[{name}] messages arrived out of order");
}

type Payloads = Arc<Mutex<Vec<String>>>;

const SINK_FACTORY: &str = "contract_refusing_sink";

/// What reached each refusing sink, by `tag`.
fn sinks() -> &'static Mutex<HashMap<String, Payloads>> {
    static SINKS: OnceLock<Mutex<HashMap<String, Payloads>>> = OnceLock::new();
    SINKS.get_or_init(Default::default)
}

/// Fails every message as retryable and records that it was handed it.
#[derive(Debug)]
struct RefusingSink(Payloads);

#[async_trait::async_trait]
impl MessagePublisher for RefusingSink {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let mut seen = self.0.lock().unwrap();
        seen.extend(messages.iter().map(text));
        let refuse = || PublisherError::Retryable(anyhow::anyhow!("contract sink refuses"));
        let failed = messages.into_iter().map(|m| (m, refuse())).collect();
        Ok(SentBatch::Partial {
            responses: None,
            failed,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
struct RefusingSinkFactory;

#[async_trait::async_trait]
impl CustomEndpointFactory for RefusingSinkFactory {
    async fn create_consumer(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        anyhow::bail!("the refusing sink is publish-only")
    }

    async fn create_publisher(
        &self,
        _: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let tag = config["tag"].as_str().unwrap_or_default();
        let seen = sinks().lock().unwrap().get(tag).cloned();
        let seen = seen.ok_or_else(|| anyhow::anyhow!("no refusing sink `{tag}`"))?;
        Ok(Box::new(RefusingSink(seen)))
    }
}

fn refusing_sink(tag: &str) -> (Endpoint, Payloads) {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        register_endpoint_factory(SINK_FACTORY, Arc::new(RefusingSinkFactory))
            .expect("register the refusing sink");
    });
    let seen = Payloads::default();
    sinks()
        .lock()
        .unwrap()
        .insert(tag.to_string(), Arc::clone(&seen));
    let config = json!({ "custom": { "name": SINK_FACTORY, "config": { "tag": tag } } });
    let endpoint = serde_json::from_value(config).expect("refusing sink endpoint");
    (endpoint, seen)
}

async fn wait_until(name: &str, step: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !done() {
        assert!(Instant::now() < deadline, "[{name}] {step}: timed out");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A message the sink fails as retryable is nacked, not acked: it is still in the source
/// once the route is stopped.
async fn check_sink_refuses(source: &mut Source, publisher: &dyn MessagePublisher, run: u128) {
    let name = source.name;
    // A second reader would compete with the route for the message.
    source.reconnect().await;
    let label = format!("contract-refuse-{name}");
    let (sink, seen) = refusing_sink(&label);
    let handle = Route::new(source.endpoint.clone(), sink)
        .run(&label)
        .await
        .expect("route starts");
    let payload = format!("contract-{run:x}-refused");
    send_in_order(publisher, std::slice::from_ref(&payload), run).await;
    wait_until(name, "refused", || seen.lock().unwrap().contains(&payload)).await;
    stop_route(name, handle).await;
    source
        .read("after the refusal", |message| {
            (MessageDisposition::Ack, text(message) == payload)
        })
        .await;
}

pub async fn check(contract: Contract) {
    if !mq_bridge::test_utils::should_run(contract.name) {
        return;
    }
    setup_logging();
    let name = contract.name;
    let compose = contract.compose;
    let steps = || async move {
        let endpoint = |value| serde_json::from_value::<Endpoint>(value).expect("endpoint config");
        let mut source = Source {
            name,
            endpoint: endpoint(contract.input),
            consumer: None,
        };
        if let Some(setup) = contract.setup {
            setup().await;
        }
        // A subscription has to exist before the messages do.
        source.open().await;
        let output = endpoint(contract.output);
        let deadline = Instant::now() + LIMIT;
        let publisher = loop {
            match create_publisher_from_route("contract", &output).await {
                Ok(publisher) => break publisher,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "[{name}] no publisher: {error:#}"
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        };

        let started = Instant::now();
        let run = fast_uuid_v7::gen_id();
        let sent: HashMap<String, u128> = (0..3u128)
            .map(|index| (format!("contract-{run:x}-{index}"), run + index + 1))
            .collect();
        let payloads: Vec<String> = sent.keys().cloned().collect();
        let messages = sent
            .iter()
            .map(|(payload, id)| CanonicalMessage::new(payload.clone().into_bytes(), Some(*id)))
            .collect();
        publisher.send_batch(messages).await.expect("publish");

        let first = source
            .deliveries("nack", &payloads, Some(MessageDisposition::Nack))
            .await;
        let second = source
            .deliveries("after the nack", &payloads, Some(MessageDisposition::Ack))
            .await;
        assert!(!first.is_empty(), "[{name}] nothing was nacked");
        println!("[{name}] 1 nack: ok after {:.1?}", started.elapsed());
        if contract.stable_id {
            for (payload, id) in &first {
                assert_eq!(
                    second.get(payload),
                    Some(id),
                    "[{name}] the id changed on redelivery"
                );
            }
        }
        if contract.hop_keeps_id {
            assert_eq!(second, sent, "[{name}] the id did not survive the hop");
        }

        let crashed = vec![format!("contract-{run:x}-crash")];
        let message = CanonicalMessage::new(crashed[0].clone().into_bytes(), Some(run + 9));
        publisher.send_batch(vec![message]).await.expect("publish");
        let before = source.deliveries("crash", &crashed, None).await;
        if let Some(expire_locks) = contract.expire_locks {
            expire_locks().await;
        }
        let after = source
            .deliveries("after the crash", &crashed, Some(MessageDisposition::Ack))
            .await;
        if contract.stable_id {
            assert_eq!(before, after, "[{name}] the id changed after a crash");
        }
        println!("[{name}] 2 crash: ok after {:.1?}", started.elapsed());

        check_mixed_acks(&mut source, publisher.as_ref(), run).await;
        println!("[{name}] 3 mixed acks: ok after {:.1?}", started.elapsed());
        if contract.keeps_metadata {
            check_metadata(&mut source, publisher.as_ref(), run).await;
            println!("[{name}] 4 metadata: ok after {:.1?}", started.elapsed());
        }
        check_backpressure(&mut source, publisher.as_ref(), run).await;
        println!(
            "[{name}] 5 backpressure: ok after {:.1?}",
            started.elapsed()
        );
        check_route_stops(&mut source, publisher.as_ref(), run).await;
        println!("[{name}] 6 route stop: ok after {:.1?}", started.elapsed());
        if contract.ordered {
            check_order(&mut source, publisher.as_ref(), run).await;
            println!("[{name}] 7 order: ok after {:.1?}", started.elapsed());
        }
        check_sink_refuses(&mut source, publisher.as_ref(), run).await;
        println!(
            "[{name}] 8 sink refuses: ok after {:.1?}",
            started.elapsed()
        );
        println!("[{name}] delivery contract holds");
    };
    if compose.is_empty() {
        steps().await;
    } else {
        run_test_with_docker(compose, steps).await;
    }
}

fn compose(file: &str) -> &'static str {
    Box::leak(format!("tests/integration/docker-compose/{file}.yml").into_boxed_str())
}

#[cfg(feature = "nats")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_nats_contract() {
    let nats =
        json!({ "url": "nats://localhost:4222", "subject": "contract.data", "stream": "contract" });
    let input = json!({ "nats": nats });
    let output = json!({ "nats": nats });
    let contract = Contract::new("nats", compose("nats"), input, output);
    check(contract).await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_contract() {
    let amqp = json!({ "url": "amqp://guest:guest@localhost:5672/%2f", "queue": "contract" });
    let input = json!({ "amqp": amqp });
    let output = json!({ "amqp": amqp });
    let contract = Contract::new("amqp", compose("amqp"), input, output);
    check(contract).await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_kafka_contract() {
    // Four partitions: only messages sharing a key keep their order.
    let input = json!({ "kafka": { "url": "localhost:9092", "topic": "contract", "group_id": "contract" } });
    let output = json!({ "kafka": { "url": "localhost:9092", "topic": "contract", "partition_key": "contract_key" } });
    let contract = Contract::new("kafka", compose("kafka"), input, output);
    check(contract).await;
}

#[cfg(feature = "redis-streams")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_streams_contract() {
    // The 60 s default would make each redelivery step wait a minute.
    let input = json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract", "group": "contract", "redelivery_timeout_ms": 1000 } });
    let output =
        json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract" } });
    let contract = Contract::new("redis_streams", compose("redis"), input, output);
    check(contract).await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_contract() {
    let mongodb = json!({ "url": "mongodb://localhost:27017", "database": "contract", "collection": "contract" });
    let mut input = mongodb.clone();
    input["consume"] = json!("consumer");
    let input = json!({ "mongodb": input });
    let output = json!({ "mongodb": mongodb });
    let mut contract = Contract::new("mongodb", compose("mongodb"), input, output);
    contract.expire_locks = Some(|| {
        Box::pin(async {
            let client = mongodb::Client::with_uri_str("mongodb://localhost:27017")
                .await
                .expect("mongodb client");
            client
                .database("contract")
                .collection::<mongodb::bson::Document>("contract")
                .update_many(
                    mongodb::bson::doc! { "locked_until": { "$ne": null } },
                    mongodb::bson::doc! { "$set": { "locked_until": 0_i64 } },
                )
                .await
                .expect("expire claims");
        })
    });
    check(contract).await;
}

#[cfg(feature = "mqtt")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mqtt_contract() {
    let mqtt = |client_id: &str| {
        json!({ "mqtt": {
            "url": "mqtt://localhost:1883", "topic": "contract", "qos": 1,
            "client_id": client_id, "clean_session": false, "protocol": "v5",
        } })
    };
    let input = mqtt("contract-in");
    let output = mqtt("contract-out");
    let contract = Contract::new("mqtt", compose("mqtt"), input, output);
    check(contract).await;
}

/// A queue table on MySQL or MariaDB; the row has no id column the reader would take one from.
#[cfg(feature = "sqlx")]
fn mysql_contract(name: &'static str, url: &'static str) -> Contract {
    let input = json!({ "sqlx": { "url": url, "table": "contract", "delete_after_read": true, "polling_interval_ms": 50 } });
    let output = json!({ "sqlx": { "url": url, "table": "contract", "auto_create_table": true } });
    let mut contract = Contract::new(name, compose(name), input, output);
    contract.stable_id = false;
    contract.hop_keeps_id = false;
    contract.keeps_metadata = false;
    contract
}

#[cfg(feature = "sqlx")]
async fn expire_mysql_locks(url: &str) {
    let pool = sqlx::MySqlPool::connect(url).await.expect("mysql pool");
    sqlx::query("UPDATE contract SET locked_until = NOW() - INTERVAL 1 SECOND WHERE locked_until IS NOT NULL")
        .execute(&pool)
        .await
        .expect("expire locks");
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mysql_contract() {
    const URL: &str = "mysql://testuser:testpass@localhost:3306/testdb";
    let mut contract = mysql_contract("mysql", URL);
    contract.expire_locks = Some(|| Box::pin(expire_mysql_locks(URL)));
    check(contract).await;
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mariadb_contract() {
    const URL: &str = "mysql://testuser:testpass@localhost:3307/testdb";
    let mut contract = mysql_contract("mariadb", URL);
    contract.expire_locks = Some(|| Box::pin(expire_mysql_locks(URL)));
    check(contract).await;
}

#[cfg(feature = "aws")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_aws_contract() {
    let sqs = json!({ "aws": {
        "queue_url": "http://localhost:4566/000000000000/contract",
        "region": "us-east-1", "endpoint_url": "http://localhost:4566",
        "access_key": "test", "secret_key": "test", "wait_time_seconds": 1,
    } });
    let mut contract = Contract::new("aws", compose("aws"), sqs.clone(), sqs);
    // A standard queue is unordered, and SQS assigns its own MessageId.
    contract.ordered = false;
    contract.hop_keeps_id = false;
    contract.setup = Some(|| {
        Box::pin(async {
            let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(aws_config::Region::new("us-east-1"))
                .endpoint_url("http://localhost:4566")
                .credentials_provider(aws_sdk_sqs::config::Credentials::new(
                    "test", "test", None, None, "test",
                ))
                .load()
                .await;
            // Short enough that the crash step does not wait out the 30 s default.
            aws_sdk_sqs::Client::new(&config)
                .create_queue()
                .queue_name("contract")
                .attributes(
                    aws_sdk_sqs::types::QueueAttributeName::VisibilityTimeout,
                    "2",
                )
                .send()
                .await
                .expect("create queue");
        })
    });
    check(contract).await;
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_postgres_contract() {
    let url = "postgres://testuser:testpass@localhost:5432/testdb";
    let input = json!({ "sqlx": { "url": url, "table": "contract", "delete_after_read": true, "polling_interval_ms": 50 } });
    let output = json!({ "sqlx": { "url": url, "table": "contract", "auto_create_table": true } });
    let mut contract = Contract::new("postgres", compose("postgres"), input, output);
    // The table row has no id column the reader would take one from.
    contract.stable_id = false;
    contract.hop_keeps_id = false;
    contract.keeps_metadata = false;
    contract.expire_locks = Some(|| {
        Box::pin(async {
            let pool = sqlx::PgPool::connect("postgres://testuser:testpass@localhost:5432/testdb")
                .await
                .expect("postgres pool");
            sqlx::query("UPDATE contract SET locked_until = NOW() - interval '1 second' WHERE locked_until IS NOT NULL")
                .execute(&pool)
                .await
                .expect("expire locks");
        })
    });
    check(contract).await;
}

/// A spool directory: no broker, so nothing to start.
#[cfg(feature = "dir-spool")]
#[tokio::test]
#[ignore = "delivery contract, run with the broker contracts"]
async fn test_dir_spool_contract() {
    let dir = tempfile::tempdir().expect("spool dir");
    let path = dir.path().to_str().expect("utf-8 path");
    let spool = json!({ "dir_spool": { "path": path, "poll_interval_ms": 20 } });
    check(Contract::new("dir_spool", "", spool.clone(), spool)).await;
}

/// The sqlx queue table read through logical replication instead of by polling.
#[cfg(feature = "postgres-cdc")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_postgres_cdc_contract() {
    const URL: &str = "postgres://testuser:testpass@localhost:5432/testdb";
    let input = json!({ "postgres_cdc": {
        "url": URL, "publication": "contract", "slot_name": "contract",
        "create_publication": true, "publication_tables": ["contract"], "status_interval_ms": 200 } });
    let output = json!({ "sqlx": { "url": URL, "table": "contract" } });
    let mut contract = Contract::new("postgres_cdc", compose("postgres_cdc"), input, output);
    contract.hop_keeps_id = false;
    contract.keeps_metadata = false;
    contract.setup = Some(|| {
        Box::pin(async {
            let pool = sqlx::PgPool::connect(URL).await.expect("postgres pool");
            sqlx::query("CREATE TABLE contract (id BIGSERIAL PRIMARY KEY, payload BYTEA NOT NULL, locked_until TIMESTAMPTZ, created_at TIMESTAMPTZ DEFAULT NOW())")
                .execute(&pool)
                .await
                .expect("create table");
        })
    });
    check(contract).await;
}
