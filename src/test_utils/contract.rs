//! The delivery contract: what `docs/DELIVERY.md` promises about an endpoint, checked against a
//! running one. Built-in brokers and plugins run the same [`Check`]s.
//!
//! Put `$ID` in the queue names and every check gets its own queue and they run side by side.
//! Without it they run one after the other on the shared queue.
//!
//! ```no_run
//! # async fn example() {
//! use mq_bridge::test_utils::contract::Contract;
//! use serde_json::json;
//!
//! let endpoint = json!({ "nats": {
//!     "url": "nats://localhost:4222", "subject": "contract.$ID", "stream": "contract_$ID"
//! } });
//! let report = Contract::new("my-plugin", endpoint.clone(), endpoint)
//!     .unordered()
//!     .run()
//!     .await;
//! report.assert_passed();
//! # }
//! ```

use crate::endpoints::{create_consumer_from_route, create_publisher_from_route};
use crate::extensions::register_endpoint_factory;
use crate::models::Endpoint;
use crate::route::RouteHandle;
use crate::traits::{
    CustomEndpointFactory, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError,
    SentBatch,
};
use crate::{CanonicalMessage, Route};
use anyhow::{anyhow, bail, ensure, Result};
use futures::future::BoxFuture;
use futures::StreamExt;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const STOP: Duration = Duration::from_secs(10);
// The service is already healthy: an endpoint that cannot connect by now will not.
const CONNECT: Duration = Duration::from_secs(20);
const BACKLOG: usize = 1000;
const CONCURRENT: usize = 200;
const REJECTED_OUT_OF: usize = 10;
const LARGE_PAYLOAD: usize = 128 * 1024;
const QUEUE_MARKER: &str = "$ID";

/// One property of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Check {
    /// Opening and closing repeatedly leaves the endpoint working.
    OpenClose,
    /// A nacked message comes back, `message_id` unchanged where the endpoint promises it.
    Nack,
    /// A message held by a consumer that died comes back.
    Crash,
    /// Nacks in between acks come back, earlier acks do not.
    MixedAcks,
    /// A metadata entry survives the hop.
    Metadata,
    /// With half a backlog held unacked, the rest still arrives once it is acked.
    Backpressure,
    /// A route on the source delivers a message and stops in time.
    RouteStop,
    /// Messages sent one at a time arrive in that order.
    Order,
    /// A message the sink fails as retryable stays in the source.
    SinkRefuses,
    /// Sending and receiving at the same time loses nothing.
    Parallel,
    /// Every tenth message is nacked during a concurrent run and comes back.
    ParallelWithRejects,
    /// Rejected messages come back to a consumer that reconnected.
    RejectsThroughReconnect,
    /// Batches of different sizes arrive whole, and a read respects its maximum.
    BatchSizes,
    /// Messages written while no consumer runs are there when one starts.
    WriteBeforeConsumer,
    /// Binary, Unicode, structural, large and empty payloads arrive byte for byte.
    PayloadFidelity,
    /// A backlog of a thousand messages arrives complete.
    LargeBacklog,
}

impl Check {
    pub const ALL: [Check; 16] = [
        Check::OpenClose,
        Check::Nack,
        Check::Crash,
        Check::MixedAcks,
        Check::Metadata,
        Check::Backpressure,
        Check::RouteStop,
        Check::Order,
        Check::SinkRefuses,
        Check::Parallel,
        Check::ParallelWithRejects,
        Check::RejectsThroughReconnect,
        Check::BatchSizes,
        Check::WriteBeforeConsumer,
        Check::PayloadFidelity,
        Check::LargeBacklog,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Check::OpenClose => "open_close",
            Check::Nack => "nack",
            Check::Crash => "crash",
            Check::MixedAcks => "mixed_acks",
            Check::Metadata => "metadata",
            Check::Backpressure => "backpressure",
            Check::RouteStop => "route_stop",
            Check::Order => "order",
            Check::SinkRefuses => "sink_refuses",
            Check::Parallel => "parallel",
            Check::ParallelWithRejects => "parallel_with_rejects",
            Check::RejectsThroughReconnect => "rejects_through_reconnect",
            Check::BatchSizes => "batch_sizes",
            Check::WriteBeforeConsumer => "write_before_consumer",
            Check::PayloadFidelity => "payload_fidelity",
            Check::LargeBacklog => "large_backlog",
        }
    }

    fn works_on_a_sink(self) -> bool {
        matches!(
            self,
            Check::OpenClose
                | Check::Parallel
                | Check::BatchSizes
                | Check::PayloadFidelity
                | Check::LargeBacklog
        )
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.name())
    }
}

/// How a check ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Held, with what is worth knowing about how, such as a payload the endpoint refused.
    Passed(Option<String>),
    Failed(String),
    Skipped(String),
}

/// The result of one check.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CheckResult {
    pub check: Check,
    pub outcome: Outcome,
    pub elapsed: Duration,
}

impl fmt::Display for CheckResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let check = self.check;
        match &self.outcome {
            Outcome::Passed(None) => write!(f, "{check:<26} ok        {:.1?}", self.elapsed),
            Outcome::Passed(Some(note)) => {
                write!(f, "{check:<26} ok        {:.1?} ({note})", self.elapsed)
            }
            Outcome::Failed(reason) => write!(f, "{check:<26} FAILED    {reason}"),
            Outcome::Skipped(reason) => write!(f, "{check:<26} skipped   {reason}"),
        }
    }
}

/// Every check of one run, in catalog order.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Report {
    pub name: String,
    pub results: Vec<CheckResult>,
}

impl Report {
    /// The checks that failed.
    pub fn failed(&self) -> Vec<Check> {
        self.results
            .iter()
            .filter(|result| matches!(result.outcome, Outcome::Failed(_)))
            .map(|result| result.check)
            .collect()
    }

    /// At least one check ran and none failed.
    pub fn passed(&self) -> bool {
        self.failed().is_empty()
            && self
                .results
                .iter()
                .any(|result| matches!(result.outcome, Outcome::Passed(_)))
    }

    /// Panics with the whole report unless [`Report::passed`].
    #[track_caller]
    pub fn assert_passed(&self) {
        assert!(self.passed(), "delivery contract failed:\n{self}");
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for result in &self.results {
            writeln!(f, "{}  {result}", self.name)?;
        }
        Ok(())
    }
}

type Hook = Arc<dyn Fn(String) -> BoxFuture<'static, ()> + Send + Sync>;
type ReadBack = Arc<dyn Fn(String) -> BoxFuture<'static, Result<Vec<Vec<u8>>>> + Send + Sync>;

#[derive(Clone, Copy)]
struct Limits {
    wait: Duration,
    idle: Duration,
}

/// One endpoint under test. Everything it does not promise is declared with a builder method.
#[non_exhaustive]
pub struct Contract {
    name: String,
    input: Option<serde_json::Value>,
    output: serde_json::Value,
    stable_id: bool,
    hop_keeps_id: bool,
    keeps_metadata: bool,
    ordered: bool,
    json: bool,
    parallelism: usize,
    limits: Limits,
    setup: Option<Hook>,
    expire_locks: Option<Hook>,
    read_back: Option<ReadBack>,
    disposable_input: bool,
    skipped: Vec<(Check, String)>,
}

impl Contract {
    pub fn new(
        name: impl Into<String>,
        input: serde_json::Value,
        output: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            input: Some(input),
            output,
            stable_id: true,
            hop_keeps_id: true,
            keeps_metadata: true,
            ordered: true,
            json: false,
            parallelism: 4,
            limits: Limits {
                wait: Duration::from_secs(90),
                idle: Duration::from_secs(5),
            },
            setup: None,
            expire_locks: None,
            read_back: None,
            disposable_input: false,
            skipped: Vec::new(),
        }
    }

    /// A sink without a matching source. `read_back` gets the queue id and returns every
    /// payload the sink holds for it.
    pub fn sink_only<F, Fut>(
        name: impl Into<String>,
        output: serde_json::Value,
        read_back: F,
    ) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<Vec<u8>>>> + Send + 'static,
    {
        let mut contract = Self::new(name, json!(null), output);
        contract.input = None;
        contract.read_back = Some(Arc::new(move |id| Box::pin(read_back(id))));
        contract
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The source does not hand messages out in the order they were sent.
    pub fn unordered(mut self) -> Self {
        self.ordered = false;
        self
    }

    /// `message_id` may change when a message is redelivered.
    pub fn unstable_id(mut self) -> Self {
        self.stable_id = false;
        self
    }

    /// The consumer reports another id than the one the publisher was given.
    pub fn id_not_kept(mut self) -> Self {
        self.hop_keeps_id = false;
        self
    }

    /// Metadata set on the publisher does not reach the consumer.
    pub fn no_metadata(mut self) -> Self {
        self.keeps_metadata = false;
        self
    }

    /// The output takes only JSON: payloads go out in the row form the source reads back.
    pub fn json_payloads(mut self) -> Self {
        self.json = true;
        self
    }

    /// How many isolated checks run at once. Default 4.
    pub fn parallelism(mut self, checks: usize) -> Self {
        self.parallelism = checks.max(1);
        self
    }

    /// How long a step waits for what it expects. Default 90 s.
    pub fn timeout(mut self, wait: Duration) -> Self {
        self.limits = Limits {
            wait,
            idle: (wait / 6).min(Duration::from_secs(5)),
        };
        self
    }

    /// Runs with the queue id once the service is up, e.g. to create the queue. With `$ID`
    /// once per check, without it once per run with an empty id.
    pub fn setup<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.setup = Some(Arc::new(move |id| Box::pin(hook(id))));
        self
    }

    /// Lets the lock a dead consumer left lapse now instead of after its lease.
    pub fn expire_locks<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.expire_locks = Some(Arc::new(move |id| Box::pin(hook(id))));
        self
    }

    /// The input without `$ID` is a queue owned by this test: a check may read and ack
    /// whatever it holds. Never set this on a queue with data you want to keep.
    pub fn disposable_input(mut self) -> Self {
        self.disposable_input = true;
        self
    }

    /// Leaves a check out. The reason appears in the report; link the finding that explains it.
    pub fn skip(mut self, check: Check, reason: impl Into<String>) -> Self {
        self.skipped.push((check, reason.into()));
        self
    }

    fn isolated(&self) -> bool {
        let has_marker = |value: &serde_json::Value| value.to_string().contains(QUEUE_MARKER);
        has_marker(&self.output) && self.input.as_ref().is_none_or(has_marker)
    }

    fn skip_reason(&self, check: Check) -> Option<String> {
        if let Some((_, reason)) = self.skipped.iter().find(|(c, _)| *c == check) {
            return Some(reason.clone());
        }
        if self.input.is_none() && !check.works_on_a_sink() {
            return Some("sink only".into());
        }
        match check {
            Check::Metadata if !self.keeps_metadata => Some("declared without metadata".into()),
            Check::Order if !self.ordered => Some("declared unordered".into()),
            _ => None,
        }
    }

    /// Runs every check and reports all of them; a failing check does not stop the others.
    pub async fn run(self) -> Report {
        super::setup_logging();
        let target = Arc::new(self);
        let isolated = target.isolated();
        let mut results = Vec::with_capacity(Check::ALL.len());
        let mut runnable = Vec::with_capacity(Check::ALL.len());
        for check in Check::ALL {
            match target.skip_reason(check) {
                Some(reason) => results.push(CheckResult {
                    check,
                    outcome: Outcome::Skipped(reason),
                    elapsed: Duration::ZERO,
                }),
                None => runnable.push(check),
            }
        }

        let setup_failure = match (&target.setup, isolated) {
            (Some(setup), false) => {
                let setup = Arc::clone(setup);
                let limit = target.limits.wait * 4;
                let run = async move { setup(String::new()).await };
                match tokio::spawn(tokio::time::timeout(limit, run)).await {
                    Ok(Ok(())) => None,
                    Ok(Err(_)) => Some(format!("no result within {limit:?}")),
                    Err(error) => Some(panic_text(error)),
                }
            }
            _ => None,
        };
        if let Some(reason) = setup_failure {
            results.extend(runnable.drain(..).map(|check| CheckResult {
                check,
                outcome: Outcome::Failed(format!("setup failed: {reason}")),
                elapsed: Duration::ZERO,
            }));
        }

        let width = if isolated { target.parallelism } else { 1 };
        let ran: Vec<CheckResult> = futures::stream::iter(runnable)
            .map(|check| run_check(Arc::clone(&target), check, isolated))
            .buffered(width)
            .collect()
            .await;
        results.extend(ran);
        results.sort_by_key(|result| result.check);
        Report {
            name: target.name.clone(),
            results,
        }
    }
}

fn panic_text(error: tokio::task::JoinError) -> String {
    if !error.is_panic() {
        return error.to_string();
    }
    let panic = error.into_panic();
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "panicked".into())
}

async fn run_check(target: Arc<Contract>, check: Check, isolated: bool) -> CheckResult {
    let started = Instant::now();
    let limit = target.limits.wait * 4;
    let name = target.name.clone();
    let body = async move {
        let mut ctx = Ctx::open(target, isolated).await?;
        run_body(check, &mut ctx).await?;
        Ok::<_, anyhow::Error>(ctx.note.take())
    };
    let task = tokio::spawn(tokio::time::timeout(limit, body));
    let outcome = match task.await {
        Ok(Ok(Ok(note))) => Outcome::Passed(note),
        Ok(Ok(Err(error))) => Outcome::Failed(format!("{error:#}")),
        Ok(Err(_)) => Outcome::Failed(format!("no result within {limit:?}")),
        Err(error) => Outcome::Failed(format!("panicked: {}", panic_text(error))),
    };
    let result = CheckResult {
        check,
        outcome,
        elapsed: started.elapsed(),
    };
    println!("{name}  {result}");
    result
}

async fn run_body(check: Check, ctx: &mut Ctx) -> Result<()> {
    match check {
        Check::OpenClose => check_open_close(ctx).await,
        Check::Nack => check_nack(ctx).await,
        Check::Crash => check_crash(ctx).await,
        Check::MixedAcks => check_mixed_acks(ctx).await,
        Check::Metadata => check_metadata(ctx).await,
        Check::Backpressure => check_backpressure(ctx).await,
        Check::RouteStop => check_route_stops(ctx).await,
        Check::Order => check_order(ctx).await,
        Check::SinkRefuses => check_sink_refuses(ctx).await,
        Check::Parallel => check_parallel(ctx).await,
        Check::ParallelWithRejects => check_parallel_with_rejects(ctx).await,
        Check::RejectsThroughReconnect => check_rejects_through_reconnect(ctx).await,
        Check::BatchSizes => check_batch_sizes(ctx).await,
        Check::WriteBeforeConsumer => check_write_before_consumer(ctx).await,
        Check::PayloadFidelity => check_payload_fidelity(ctx).await,
        Check::LargeBacklog => check_large_backlog(ctx).await,
    }
}

/// The payload as sent. A change-capture source carries it as the `payload` column of a row,
/// so any JSON object with a `payload` string, or one under `value` as a Kafka record envelope
/// has it, is read that way; contract payloads are never JSON.
fn bytes(message: &CanonicalMessage) -> Vec<u8> {
    use base64::Engine;
    let raw: &[u8] = &message.payload;
    let row: Option<serde_json::Value> = raw
        .starts_with(b"{")
        .then(|| serde_json::from_slice(raw).ok())
        .flatten();
    let encoded = row
        .as_ref()
        .and_then(|row| row["payload"].as_str().or(row["value"]["payload"].as_str()));
    match encoded {
        // Malformed base64 stays as received, so it can never count as an expected payload.
        Some(encoded) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap_or_else(|_| raw.to_vec()),
        None => raw.to_vec(),
    }
}

fn text(message: &CanonicalMessage) -> String {
    String::from_utf8_lossy(&bytes(message)).into_owned()
}

fn wire(json_payloads: bool, mut message: CanonicalMessage) -> CanonicalMessage {
    use base64::Engine;
    if json_payloads {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&message.payload);
        message.set_payload_str(json!({ "payload": encoded }).to_string());
    }
    message
}

fn resolve(value: &serde_json::Value, id: &str) -> Result<Endpoint> {
    let resolved = value.to_string().replace(QUEUE_MARKER, id);
    serde_json::from_str(&resolved).map_err(|error| anyhow!("endpoint config: {error}"))
}

struct Source {
    endpoint: Endpoint,
    consumer: Option<Box<dyn MessageConsumer>>,
    limits: Limits,
}

impl Source {
    async fn open(&mut self) -> Result<&mut Box<dyn MessageConsumer>> {
        let deadline = Instant::now() + self.limits.wait.min(CONNECT);
        while self.consumer.is_none() {
            match create_consumer_from_route("contract", &self.endpoint).await {
                Ok(consumer) => self.consumer = Some(consumer),
                Err(error) => {
                    ensure!(Instant::now() < deadline, "no consumer: {error:#}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        Ok(self.consumer.as_mut().expect("just opened"))
    }

    /// Closes the consumer as a route does before it connects again.
    async fn reconnect(&mut self) {
        if let Some(mut consumer) = self.consumer.take() {
            match tokio::time::timeout(STOP, consumer.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("contract: close failed: {error:#}"),
                Err(_) => tracing::warn!("contract: close took over {STOP:?}"),
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
    ) -> Result<HashMap<String, u128>> {
        let deadline = Instant::now() + self.limits.wait;
        let idle_limit = self.limits.idle;
        let mut seen = HashMap::new();
        let mut idle = 0;
        while seen.len() < payloads.len() {
            ensure!(
                Instant::now() < deadline,
                "{step}: {} of {} arrived",
                seen.len(),
                payloads.len()
            );
            let consumer = self.open().await?;
            let batch = match tokio::time::timeout(idle_limit, consumer.receive_batch(16)).await {
                Ok(Ok(batch)) => batch,
                // What a route does with a failed read: connect again.
                Ok(Err(_)) => {
                    self.reconnect().await;
                    continue;
                }
                Err(_) => {
                    idle += 1;
                    if idle == 2 {
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
                return Ok(seen);
            }
            let dispositions = dispositions.into_iter().flatten().collect();
            if (batch.commit)(dispositions).await.is_err() {
                self.reconnect().await;
            }
            // A log replays from the nacked batch, so nacking on would never get past it.
            if matches!(disposition, Some(MessageDisposition::Nack)) && !seen.is_empty() {
                return Ok(seen);
            }
        }
        Ok(seen)
    }

    /// Reads batches of at most `max`, settling each message as `settle` says, until it
    /// reports done.
    async fn read(
        &mut self,
        step: &str,
        max: usize,
        mut settle: impl FnMut(&CanonicalMessage) -> (MessageDisposition, bool) + Send,
    ) -> Result<()> {
        let deadline = Instant::now() + self.limits.wait;
        let idle_limit = self.limits.idle;
        let mut done = false;
        let mut idle = 0;
        while !done {
            ensure!(Instant::now() < deadline, "{step}: timed out");
            let consumer = self.open().await?;
            let batch = match tokio::time::timeout(idle_limit, consumer.receive_batch(max)).await {
                Ok(Ok(batch)) => batch,
                Ok(Err(_)) => {
                    self.reconnect().await;
                    continue;
                }
                Err(_) => {
                    idle += 1;
                    if idle == 2 {
                        self.reconnect().await;
                        idle = 0;
                    }
                    continue;
                }
            };
            idle = 0;
            ensure!(
                batch.messages.len() <= max,
                "{step}: asked for at most {max}, got {}",
                batch.messages.len()
            );
            let mut dispositions = Vec::with_capacity(batch.messages.len());
            for message in &batch.messages {
                let (disposition, finished) = settle(message);
                dispositions.push(disposition);
                done |= finished;
            }
            if (batch.commit)(dispositions).await.is_err() {
                self.reconnect().await;
                // The batch is unsettled, so the step is not done with it.
                done = false;
            }
        }
        Ok(())
    }

    /// Reads for `window`, settling each message as `settle` says.
    async fn linger(
        &mut self,
        window: Duration,
        mut settle: impl FnMut(&CanonicalMessage) -> MessageDisposition + Send,
    ) -> Result<()> {
        let until = Instant::now() + window;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            let consumer = self.open().await?;
            match tokio::time::timeout(left, consumer.receive_batch(16)).await {
                Ok(Ok(batch)) => {
                    let dispositions = batch.messages.iter().map(&mut settle).collect();
                    if (batch.commit)(dispositions).await.is_err() {
                        self.reconnect().await;
                    }
                }
                Ok(Err(_)) => self.reconnect().await,
                Err(_) => {}
            }
        }
        Ok(())
    }
}

/// What one check works with: its own queue id, source and publisher.
struct Ctx {
    target: Arc<Contract>,
    id: String,
    run: u128,
    source: Option<Source>,
    publisher: Arc<dyn MessagePublisher>,
    output: Endpoint,
    note: Option<String>,
}

impl Ctx {
    async fn open(target: Arc<Contract>, isolated: bool) -> Result<Self> {
        let run = fast_uuid_v7::gen_id();
        let id = if isolated {
            format!("c{:012x}", run & 0xffff_ffff_ffff)
        } else {
            String::new()
        };
        if isolated {
            if let Some(setup) = &target.setup {
                setup(id.clone()).await;
            }
        }
        let output = resolve(&target.output, &id)?;
        let mut source = match &target.input {
            Some(input) => Some(Source {
                endpoint: resolve(input, &id)?,
                consumer: None,
                limits: target.limits,
            }),
            None => None,
        };
        // A subscription has to exist before the messages do.
        if let Some(source) = &mut source {
            source.open().await?;
        }
        let publisher = connect_publisher(&output, target.limits.wait).await?;
        Ok(Self {
            target,
            id,
            run,
            source,
            publisher,
            output,
            note: None,
        })
    }

    fn source(&mut self) -> &mut Source {
        self.source.as_mut().expect("this check needs a source")
    }

    fn labels(&self, label: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|i| format!("contract-{:x}-{label}-{i}", self.run))
            .collect()
    }

    fn message(&self, payload: &str) -> CanonicalMessage {
        CanonicalMessage::new(payload.as_bytes().to_vec(), None)
    }

    /// Publishes `messages` and fails the step if the publisher refused any of them.
    async fn publish(&self, messages: Vec<CanonicalMessage>) -> Result<()> {
        let json_payloads = self.target.json;
        let messages = messages
            .into_iter()
            .map(|message| wire(json_payloads, message))
            .collect();
        let sent = self
            .publisher
            .send_batch(messages)
            .await
            .map_err(|error| anyhow!("publish: {error}"))?;
        if let SentBatch::Partial { failed, .. } = sent {
            if let Some((_, error)) = failed.first() {
                bail!(
                    "publish: {} message(s) failed, first: {error}",
                    failed.len()
                );
            }
        }
        Ok(())
    }

    /// Sends one at a time under one `contract_key`, the record key of a partitioned output,
    /// so a log holds them in this order.
    async fn send_in_order(&self, payloads: &[String]) -> Result<()> {
        for payload in payloads {
            let message = self
                .message(payload)
                .with_metadata_kv("contract_key", format!("{:x}", self.run));
            self.publisher
                .send(wire(self.target.json, message))
                .await
                .map_err(|error| anyhow!("publish: {error}"))?;
        }
        Ok(())
    }

    /// Starts sending `payloads` concurrently, so a reader can overlap with the writers.
    fn send_concurrently(&self, payloads: &[String]) -> Sending {
        let publisher = Arc::clone(&self.publisher);
        let json_payloads = self.target.json;
        let payloads = payloads.to_vec();
        Sending(tokio::spawn(async move {
            let mut sends = futures::stream::iter(payloads)
                .map(|payload| {
                    let publisher = Arc::clone(&publisher);
                    let message = wire(
                        json_payloads,
                        CanonicalMessage::new(payload.into_bytes(), None),
                    );
                    async move { publisher.send(message).await }
                })
                .buffer_unordered(16);
            while let Some(sent) = sends.next().await {
                sent.map_err(|error| anyhow!("publish: {error}"))?;
            }
            Ok(())
        }))
    }

    /// Hands each payload the endpoint holds to `visit`, acking it, until `visit` is done.
    async fn consume(
        &mut self,
        step: &str,
        max: usize,
        mut visit: impl FnMut(&[u8]) -> bool + Send,
    ) -> Result<()> {
        let Some(read_back) = self.target.read_back.clone() else {
            return self
                .source()
                .read(step, max, |message| {
                    (MessageDisposition::Ack, visit(&bytes(message)))
                })
                .await;
        };
        let deadline = Instant::now() + self.target.limits.wait;
        loop {
            let held = read_back(self.id.clone()).await?;
            let mut done = false;
            for payload in &held {
                done |= visit(payload);
            }
            if done {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "{step}: timed out");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Reads until every payload arrived, naming what is missing if some never does.
    async fn collect(&mut self, step: &str, payloads: &[String], max: usize) -> Result<()> {
        let wanted: HashSet<&[u8]> = payloads.iter().map(|p| p.as_bytes()).collect();
        let mut seen: HashSet<Vec<u8>> = HashSet::with_capacity(wanted.len());
        let result = self
            .consume(step, max, |payload| {
                if wanted.contains(payload) {
                    seen.insert(payload.to_vec());
                }
                seen.len() == wanted.len()
            })
            .await;
        if seen.len() == wanted.len() {
            return result;
        }
        let first = payloads.iter().find(|p| !seen.contains(p.as_bytes()));
        let cause = result
            .err()
            .map(|e| format!(" ({e:#})"))
            .unwrap_or_default();
        bail!(
            "{step}: {} of {} missing, first `{}`{cause}",
            wanted.len() - seen.len(),
            wanted.len(),
            first.map(String::as_str).unwrap_or_default()
        )
    }
}

/// A running sender, stopped if its check ends first, so it cannot publish into the next one.
struct Sending(tokio::task::JoinHandle<Result<()>>);

impl Future for Sending {
    type Output = Result<Result<()>, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn connect_publisher(output: &Endpoint, wait: Duration) -> Result<Arc<dyn MessagePublisher>> {
    let deadline = Instant::now() + wait.min(CONNECT);
    loop {
        match create_publisher_from_route("contract", output).await {
            Ok(publisher) => return Ok(publisher),
            Err(error) => {
                ensure!(Instant::now() < deadline, "no publisher: {error:#}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Stands in while the real publisher is closed.
struct ClosedPublisher;

#[async_trait::async_trait]
impl MessagePublisher for ClosedPublisher {
    async fn send_batch(&self, _: Vec<CanonicalMessage>) -> Result<SentBatch, PublisherError> {
        Err(PublisherError::NonRetryable(anyhow!("publisher is closed")))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Opening and closing three times leaves source and sink working.
async fn check_open_close(ctx: &mut Ctx) -> Result<()> {
    for _ in 0..3 {
        if let Some(source) = &mut ctx.source {
            source.open().await?;
            source.reconnect().await;
        }
        if let Some(hook) = ctx.publisher.on_disconnect_hook() {
            hook.await?;
        }
        // A single-writer sink refuses the next publisher while this one is alive.
        ctx.publisher = Arc::new(ClosedPublisher);
        ctx.publisher = connect_publisher(&ctx.output, ctx.target.limits.wait).await?;
    }
    if let Some(source) = &mut ctx.source {
        source.open().await?;
    }
    let payloads = ctx.labels("reopened", 1);
    ctx.publish(vec![ctx.message(&payloads[0])]).await?;
    ctx.collect("after reopening", &payloads, 16).await
}

async fn check_nack(ctx: &mut Ctx) -> Result<()> {
    let run = ctx.run;
    let sent: HashMap<String, u128> = ctx
        .labels("nack", 3)
        .into_iter()
        .zip(1u128..)
        .map(|(payload, index)| (payload, run + index))
        .collect();
    let payloads: Vec<String> = sent.keys().cloned().collect();
    let messages = sent
        .iter()
        .map(|(payload, id)| CanonicalMessage::new(payload.clone().into_bytes(), Some(*id)))
        .collect();
    ctx.publish(messages).await?;

    let source = ctx.source();
    let first = source
        .deliveries("nack", &payloads, Some(MessageDisposition::Nack))
        .await?;
    let second = source
        .deliveries("after the nack", &payloads, Some(MessageDisposition::Ack))
        .await?;
    ensure!(!first.is_empty(), "nothing was nacked");
    if ctx.target.stable_id {
        for (payload, id) in &first {
            ensure!(
                second.get(payload) == Some(id),
                "the id changed on redelivery"
            );
        }
    }
    if ctx.target.hop_keeps_id {
        ensure!(second == sent, "the id did not survive the hop");
    }
    Ok(())
}

async fn check_crash(ctx: &mut Ctx) -> Result<()> {
    let crashed = ctx.labels("crash", 1);
    let message = CanonicalMessage::new(crashed[0].clone().into_bytes(), Some(ctx.run + 9));
    ctx.publish(vec![message]).await?;
    let before = ctx.source().deliveries("crash", &crashed, None).await?;
    if let Some(expire_locks) = ctx.target.expire_locks.clone() {
        expire_locks(ctx.id.clone()).await;
    }
    let after = ctx
        .source()
        .deliveries("after the crash", &crashed, Some(MessageDisposition::Ack))
        .await?;
    if ctx.target.stable_id {
        ensure!(before == after, "the id changed after a crash");
    }
    Ok(())
}

/// Acks A, B and D, nacks C and E, then restarts the consumer: C and E come back, and A
/// and B, acked before the first nack, do not. Catches a commit that skips past a nack.
/// An unordered source may hand A back after C and E, so it is read on for a while.
async fn check_mixed_acks(ctx: &mut Ctx) -> Result<()> {
    let ids = ctx.labels("mixed", 5);
    ctx.send_in_order(&ids).await?;
    let nacked = |payload: &str| payload == ids[2] || payload == ids[4];

    let mut seen = HashSet::new();
    let source = ctx.source();
    source
        .read("mixed acks", 16, |message| {
            let payload = text(message);
            if !ids.contains(&payload) {
                return (MessageDisposition::Ack, false);
            }
            // A log replays from the nack, so a second sighting ends the step.
            let replay = !seen.insert(payload.clone());
            let done = replay || seen.len() == ids.len();
            if replay || nacked(&payload) {
                (MessageDisposition::Nack, done)
            } else {
                (MessageDisposition::Ack, done)
            }
        })
        .await?;

    source.reconnect().await;
    let mut back = HashSet::new();
    let mut acked_back = Vec::new();
    source
        .read("after mixed acks", 16, |message| {
            let payload = text(message);
            if payload == ids[0] || payload == ids[1] {
                acked_back.push(payload);
            } else if nacked(&payload) {
                back.insert(payload);
            }
            (MessageDisposition::Ack, back.len() == 2)
        })
        .await?;
    if !ctx.target.ordered {
        let idle = ctx.target.limits.idle;
        ctx.source()
            .linger(idle, |message| {
                let payload = text(message);
                if payload == ids[0] || payload == ids[1] {
                    acked_back.push(payload);
                }
                MessageDisposition::Ack
            })
            .await?;
    }
    ensure!(
        acked_back.is_empty(),
        "acked messages came back: {acked_back:?}"
    );
    Ok(())
}

/// A metadata entry set on the publisher arrives at the consumer.
async fn check_metadata(ctx: &mut Ctx) -> Result<()> {
    let payload = ctx.labels("metadata", 1).remove(0);
    let tag = format!("{:x}", ctx.run);
    let message = ctx
        .message(&payload)
        .with_metadata_kv("contract_tag", tag.clone());
    ctx.publish(vec![message]).await?;
    let mut received = None;
    ctx.source()
        .read("metadata", 16, |message| {
            let done = text(message) == payload;
            if done {
                received = Some(message.get_metadata("contract_tag").map(str::to_string));
            }
            (MessageDisposition::Ack, done)
        })
        .await?;
    ensure!(
        received == Some(Some(tag)),
        "metadata did not survive the hop"
    );
    Ok(())
}

/// Holds about half of a 40-message backlog unacked (fewer if the broker stops handing
/// out), pauses, acks it, then reads on: every message arrives, duplicates allowed.
async fn check_backpressure(ctx: &mut Ctx) -> Result<()> {
    let payloads = ctx.labels("held", 40);
    let messages = payloads.iter().map(|p| ctx.message(p)).collect();
    ctx.publish(messages).await?;
    let Limits { wait, idle } = ctx.target.limits;

    let mut seen = HashSet::new();
    let mut held = Vec::new();
    let deadline = Instant::now() + wait;
    let source = ctx.source();
    while seen.len() < payloads.len() / 2 {
        ensure!(Instant::now() < deadline, "nothing was read while holding");
        let consumer = source.open().await?;
        match tokio::time::timeout(idle, consumer.receive_batch(8)).await {
            // An empty poll after a delivery: the broker stopped handing out.
            Ok(Ok(batch)) if batch.messages.is_empty() => {
                if !held.is_empty() {
                    break;
                }
            }
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
            Ok(Err(_)) => {
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
    tokio::time::sleep(Duration::from_millis(500).min(idle)).await;
    for batch in held {
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        if (batch.commit)(acks).await.is_err() {
            source.reconnect().await;
            break;
        }
    }

    source
        .read("after the backlog", 16, |message| {
            let payload = text(message);
            if payloads.contains(&payload) {
                seen.insert(payload);
            }
            (MessageDisposition::Ack, seen.len() == payloads.len())
        })
        .await
        .map_err(|error| anyhow!("{error:#}: {} of {} arrived", seen.len(), payloads.len()))?;
    ctx.note = Some(format!("held {held_count} of {}", payloads.len()));
    Ok(())
}

/// Runs a route from the source to memory, waits for one message to pass, then stops it.
async fn check_route_stops(ctx: &mut Ctx) -> Result<()> {
    let label = format!("contract-route-{}-{:x}", ctx.target.name, ctx.run);
    let wait = ctx.target.limits.wait;
    let idle = ctx.target.limits.idle;
    // A second reader would compete with the route for the message.
    ctx.source().reconnect().await;
    let sink: Endpoint = serde_json::from_value(json!({ "memory": { "topic": label } }))?;
    let mut out = create_consumer_from_route("contract-out", &sink).await?;
    let handle = Route::new(ctx.source().endpoint.clone(), sink)
        .run(&label)
        .await
        .map_err(|error| anyhow!("route start: {error:?}"))?;

    let payload = ctx.labels("route", 1).remove(0);
    let delivered = async {
        ctx.publish(vec![ctx.message(&payload)]).await?;
        let deadline = Instant::now() + wait;
        loop {
            ensure!(Instant::now() < deadline, "the route delivered nothing");
            let Ok(Ok(batch)) = tokio::time::timeout(idle, out.receive_batch(16)).await else {
                continue;
            };
            let found = batch.messages.iter().any(|m| text(m) == payload);
            let acks = vec![MessageDisposition::Ack; batch.messages.len()];
            (batch.commit)(acks).await?;
            if found {
                return Ok(());
            }
        }
    }
    .await;
    let stopped = stop_route(handle).await;
    delivered.and(stopped)
}

async fn stop_route(handle: RouteHandle) -> Result<()> {
    handle.stop().await;
    tokio::time::timeout(STOP, handle.join())
        .await
        .map_err(|_| anyhow!("the route did not stop within {STOP:?}"))?
        .map_err(|error| anyhow!("route task: {error:?}"))?;
    Ok(())
}

/// Ten messages sent one at a time arrive in that order; a replayed duplicate is ignored.
async fn check_order(ctx: &mut Ctx) -> Result<()> {
    let ids = ctx.labels("order", 10);
    ctx.send_in_order(&ids).await?;
    let mut arrived: Vec<String> = Vec::new();
    ctx.source()
        .read("order", 16, |message| {
            let payload = text(message);
            if ids.contains(&payload) && !arrived.contains(&payload) {
                arrived.push(payload);
            }
            (MessageDisposition::Ack, arrived.len() == ids.len())
        })
        .await?;
    ensure!(arrived == ids, "messages arrived out of order: {arrived:?}");
    Ok(())
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
        let refuse = || PublisherError::Retryable(anyhow!("contract sink refuses"));
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
        bail!("the refusing sink is publish-only")
    }

    async fn create_publisher(
        &self,
        _: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let tag = config["tag"].as_str().unwrap_or_default();
        let seen = sinks().lock().unwrap().get(tag).cloned();
        let seen = seen.ok_or_else(|| anyhow!("no refusing sink `{tag}`"))?;
        Ok(Box::new(RefusingSink(seen)))
    }
}

fn refusing_sink(tag: &str) -> Result<(Endpoint, Payloads)> {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    if REGISTERED.get().is_none() {
        // A concurrent check may have won the registration; that one serves both.
        let _ = register_endpoint_factory(SINK_FACTORY, Arc::new(RefusingSinkFactory));
        let _ = REGISTERED.set(());
    }
    let seen = Payloads::default();
    sinks()
        .lock()
        .unwrap()
        .insert(tag.to_string(), Arc::clone(&seen));
    let config = json!({ "custom": { "name": SINK_FACTORY, "config": { "tag": tag } } });
    Ok((serde_json::from_value(config)?, seen))
}

/// A message the sink fails as retryable is nacked, not acked: it is still in the source
/// once the route is stopped.
async fn check_sink_refuses(ctx: &mut Ctx) -> Result<()> {
    // On a shared log, a leftover at the head would be refused forever and hide the message.
    if ctx.id.is_empty() && ctx.target.disposable_input {
        let idle = ctx.target.limits.idle;
        ctx.source()
            .linger(idle, |_| MessageDisposition::Ack)
            .await?;
    }
    // A second reader would compete with the route for the message.
    ctx.source().reconnect().await;
    let label = format!("contract-refuse-{}-{:x}", ctx.target.name, ctx.run);
    let (sink, seen) = refusing_sink(&label)?;
    let handle = Route::new(ctx.source().endpoint.clone(), sink)
        .run(&label)
        .await
        .map_err(|error| anyhow!("route start: {error:?}"))?;
    let payload = ctx.labels("refused", 1).remove(0);
    let refused = async {
        ctx.send_in_order(std::slice::from_ref(&payload)).await?;
        let deadline = Instant::now() + ctx.target.limits.wait;
        while !seen.lock().unwrap().contains(&payload) {
            ensure!(Instant::now() < deadline, "the sink was never handed it");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
    .await;
    let stopped = stop_route(handle).await;
    sinks().lock().unwrap().remove(&label);
    refused.and(stopped)?;
    ctx.source()
        .read("after the refusal", 16, |message| {
            (MessageDisposition::Ack, text(message) == payload)
        })
        .await
}

/// Writers and a reader overlap, and everything arrives.
async fn check_parallel(ctx: &mut Ctx) -> Result<()> {
    let payloads = ctx.labels("parallel", CONCURRENT);
    let sending = ctx.send_concurrently(&payloads);
    let collected = ctx.collect("parallel", &payloads, 32).await;
    sending.await??;
    collected
}

fn is_rejected(index: usize) -> bool {
    index % REJECTED_OUT_OF == 1
}

/// Every tenth message is nacked at first sight while the writers are still running; each
/// one comes back and everything else arrives.
async fn check_parallel_with_rejects(ctx: &mut Ctx) -> Result<()> {
    let payloads = ctx.labels("rejects", CONCURRENT);
    let rejected: HashSet<&String> = payloads
        .iter()
        .enumerate()
        .filter(|(index, _)| is_rejected(*index))
        .map(|(_, payload)| payload)
        .collect();
    let all: HashSet<&String> = payloads.iter().collect();
    let sending = ctx.send_concurrently(&payloads);

    let mut seen: HashSet<String> = HashSet::new();
    let mut back: HashSet<String> = HashSet::new();
    let result = ctx
        .source()
        .read("parallel with rejects", 32, |message| {
            let payload = text(message);
            if !all.contains(&payload) {
                return (MessageDisposition::Ack, false);
            }
            let again = !seen.insert(payload.clone());
            let disposition = if !rejected.contains(&payload) {
                MessageDisposition::Ack
            } else if again {
                back.insert(payload);
                MessageDisposition::Ack
            } else {
                MessageDisposition::Nack
            };
            let done = seen.len() == all.len() && back.len() == rejected.len();
            (disposition, done)
        })
        .await;
    sending.await??;
    result.map_err(|error| {
        anyhow!(
            "{error:#}: {} of {} arrived, {} of {} rejected came back",
            seen.len(),
            all.len(),
            back.len(),
            rejected.len()
        )
    })
}

/// Rejects a tenth of the messages, closes the consumer, and a new one gets them.
async fn check_rejects_through_reconnect(ctx: &mut Ctx) -> Result<()> {
    let payloads = ctx.labels("reconnect", CONCURRENT);
    let rejected: HashSet<&String> = payloads
        .iter()
        .enumerate()
        .filter(|(index, _)| is_rejected(*index))
        .map(|(_, payload)| payload)
        .collect();
    let all: HashSet<&String> = payloads.iter().collect();
    ctx.send_concurrently(&payloads).await??;

    let mut seen: HashSet<String> = HashSet::new();
    let source = ctx.source();
    source
        .read("before the reconnect", 32, |message| {
            let payload = text(message);
            if !all.contains(&payload) {
                return (MessageDisposition::Ack, false);
            }
            // A log replays from the first nack, so a second sighting ends the pass.
            let replay = !seen.insert(payload.clone());
            let disposition = if replay || rejected.contains(&payload) {
                MessageDisposition::Nack
            } else {
                MessageDisposition::Ack
            };
            (disposition, replay || seen.len() == all.len())
        })
        .await?;
    source.reconnect().await;

    let remaining: Vec<String> = payloads
        .iter()
        .filter(|payload| rejected.contains(payload) || !seen.contains(*payload))
        .cloned()
        .collect();
    ctx.collect("after the reconnect", &remaining, 32).await
}

/// Each batch size arrives whole, read with a maximum below, at and above it.
async fn check_batch_sizes(ctx: &mut Ctx) -> Result<()> {
    for (send, max) in [(1, 1), (2, 16), (10, 5), (64, 16)] {
        let payloads = ctx.labels(&format!("batch{send}"), send);
        let messages = payloads.iter().map(|p| ctx.message(p)).collect();
        ctx.publish(messages).await?;
        ctx.collect(&format!("batch of {send}"), &payloads, max)
            .await?;
    }
    Ok(())
}

/// Messages written while no consumer runs are read by the next one.
async fn check_write_before_consumer(ctx: &mut Ctx) -> Result<()> {
    ctx.source().reconnect().await;
    let payloads = ctx.labels("offline", 20);
    let messages = payloads.iter().map(|p| ctx.message(p)).collect();
    ctx.publish(messages).await?;
    ctx.collect("after the consumer started", &payloads, 16)
        .await
}

/// Binary, Unicode, structural, large and empty payloads arrive byte for byte. An endpoint
/// may refuse a binary, large or empty one at the send, loudly; it may not lose or change it.
async fn check_payload_fidelity(ctx: &mut Ctx) -> Result<()> {
    let prefix = format!("contract-{:x}-fidelity-", ctx.run).into_bytes();
    let tagged = |index: usize, body: &[u8]| {
        let mut payload = prefix.clone();
        payload.extend_from_slice(format!("{index}:").as_bytes());
        payload.extend_from_slice(body);
        payload
    };
    let binary: Vec<u8> = (0..=255).collect();
    let large: Vec<u8> = (0..LARGE_PAYLOAD).map(|i| (i % 251) as u8).collect();
    let mut cases: Vec<(&str, Vec<u8>, bool)> = vec![
        ("binary", tagged(0, &binary), true),
        (
            "unicode",
            tagged(1, "äöü 日本語 🦀 \u{2028}".as_bytes()),
            false,
        ),
        ("structural", tagged(2, b"{\"a\":1}\n\r\t\"'\\"), false),
        ("large", tagged(3, &large), true),
    ];
    // An empty payload carries no tag, so only a queue of its own can tell it apart.
    if !ctx.id.is_empty() {
        cases.push(("empty", Vec::new(), true));
    }

    let mut expected: Vec<Vec<u8>> = Vec::with_capacity(cases.len());
    let mut refused = Vec::new();
    for (label, payload, may_refuse) in cases {
        let message = wire(
            ctx.target.json,
            CanonicalMessage::new(payload.clone(), None),
        );
        match ctx.publisher.send(message).await {
            Ok(_) => expected.push(payload),
            Err(_) if may_refuse => refused.push(label),
            Err(error) => bail!("publish {label}: {error}"),
        }
    }
    if !refused.is_empty() {
        ctx.note = Some(format!("refused at send: {}", refused.join(", ")));
    }

    let mut pending = expected.clone();
    let mut changed = 0;
    let result = ctx
        .consume("payload fidelity", 16, |payload| {
            if let Some(found) = pending.iter().position(|p| p == payload) {
                pending.remove(found);
            } else if payload.starts_with(&prefix) && !expected.iter().any(|p| p == payload) {
                changed += 1;
            }
            pending.is_empty() || changed > 0
        })
        .await;
    ensure!(changed == 0, "{changed} payload(s) arrived changed");
    if !pending.is_empty() {
        let cause = result
            .err()
            .map(|e| format!(" ({e:#})"))
            .unwrap_or_default();
        bail!(
            "{} of {} never arrived{cause}",
            pending.len(),
            expected.len()
        );
    }
    result
}

/// A thousand messages, published in batches, all arrive.
async fn check_large_backlog(ctx: &mut Ctx) -> Result<()> {
    let payloads = ctx.labels("backlog", BACKLOG);
    for chunk in payloads.chunks(100) {
        let messages = chunk.iter().map(|p| ctx.message(p)).collect();
        ctx.publish(messages).await?;
    }
    ctx.collect("large backlog", &payloads, 64).await
}

#[cfg(test)]
mod tests;
