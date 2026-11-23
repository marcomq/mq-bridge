//! The contract checked against itself: a correct in-memory broker passes, and each injected
//! fault fails exactly the checks responsible for it.

use super::*;
use crate::traits::{BatchCommitFunc, ConsumerError, ReceivedBatch};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

const FACTORY: &str = "contract_fault_broker";
const LOST_EVERY: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    LosesMessages,
    NackActsAsAck,
    ReplaysAcked,
    ChangesIdOnRedelivery,
    DropsMetadata,
    SwapsOrder,
    CorruptsLargePayloads,
}

const FAULTS: [Fault; 8] = [
    Fault::None,
    Fault::LosesMessages,
    Fault::NackActsAsAck,
    Fault::ReplaysAcked,
    Fault::ChangesIdOnRedelivery,
    Fault::DropsMetadata,
    Fault::SwapsOrder,
    Fault::CorruptsLargePayloads,
];

struct Broker {
    fault: Fault,
    queue: Mutex<VecDeque<CanonicalMessage>>,
    sent: AtomicUsize,
    replayed: Mutex<HashSet<u128>>,
}

impl Broker {
    fn redeliver(&self, mut message: CanonicalMessage) {
        if self.fault == Fault::ChangesIdOnRedelivery {
            message.message_id = message.message_id.wrapping_add(1);
        }
        self.queue.lock().unwrap().push_front(message);
    }

    fn accept(&self, mut message: CanonicalMessage) {
        let number = self.sent.fetch_add(1, Ordering::SeqCst) + 1;
        match self.fault {
            Fault::LosesMessages if number.is_multiple_of(LOST_EVERY) => return,
            #[allow(deprecated)]
            Fault::DropsMetadata => message.metadata = Default::default(),
            Fault::CorruptsLargePayloads if message.payload.len() > 1024 => {
                let mut payload = message.payload.to_vec();
                *payload.last_mut().expect("large payload") ^= 0xff;
                message.payload = payload.into();
            }
            _ => {}
        }
        self.queue.lock().unwrap().push_back(message);
    }
}

type BrokerRegistry = Mutex<HashMap<(String, String), Arc<Broker>>>;

fn brokers() -> &'static BrokerRegistry {
    static BROKERS: OnceLock<BrokerRegistry> = OnceLock::new();
    BROKERS.get_or_init(Default::default)
}

fn broker(config: &serde_json::Value) -> Arc<Broker> {
    let fault_name = config["fault"].as_str().unwrap_or_default().to_string();
    let queue = config["queue"].as_str().unwrap_or_default().to_string();
    let fault = FAULTS
        .into_iter()
        .find(|fault| format!("{fault:?}") == fault_name)
        .expect("known fault");
    let mut all = brokers().lock().unwrap();
    Arc::clone(all.entry((fault_name, queue)).or_insert_with(|| {
        Arc::new(Broker {
            fault,
            queue: Mutex::default(),
            sent: AtomicUsize::new(0),
            replayed: Mutex::default(),
        })
    }))
}

struct FaultConsumer {
    broker: Arc<Broker>,
    in_flight: Arc<Mutex<BTreeMap<u64, CanonicalMessage>>>,
    acked: Arc<Mutex<Vec<CanonicalMessage>>>,
    next_tag: u64,
}

impl Drop for FaultConsumer {
    fn drop(&mut self) {
        let unsettled = std::mem::take(&mut *self.in_flight.lock().unwrap());
        for (_, message) in unsettled.into_iter().rev() {
            self.broker.redeliver(message);
        }
        if self.broker.fault == Fault::ReplaysAcked {
            let acked = std::mem::take(&mut *self.acked.lock().unwrap());
            let mut replayed = self.broker.replayed.lock().unwrap();
            for message in acked.into_iter().rev() {
                if replayed.insert(message.message_id) {
                    self.broker.queue.lock().unwrap().push_front(message);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl MessageConsumer for FaultConsumer {
    async fn receive_batch(&mut self, max: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut messages = loop {
            let taken: Vec<CanonicalMessage> = {
                let mut queue = self.broker.queue.lock().unwrap();
                let count = max.min(queue.len());
                queue.drain(..count).collect()
            };
            if !taken.is_empty() {
                break taken;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        if self.broker.fault == Fault::SwapsOrder && messages.len() >= 2 {
            messages.swap(0, 1);
        }
        let first_tag = self.next_tag;
        self.next_tag += messages.len() as u64;
        {
            let mut in_flight = self.in_flight.lock().unwrap();
            for (tag, message) in (first_tag..).zip(&messages) {
                in_flight.insert(tag, message.clone());
            }
        }
        let broker = Arc::clone(&self.broker);
        let in_flight = Arc::clone(&self.in_flight);
        let acked = Arc::clone(&self.acked);
        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                // From the back, so nacked messages keep their order at the head of the queue.
                let settled: Vec<_> = (first_tag..).zip(dispositions).collect();
                for (tag, disposition) in settled.into_iter().rev() {
                    let Some(message) = in_flight.lock().unwrap().remove(&tag) else {
                        continue;
                    };
                    let nacked = matches!(disposition, MessageDisposition::Nack);
                    if nacked && broker.fault != Fault::NackActsAsAck {
                        broker.redeliver(message);
                    } else {
                        acked.lock().unwrap().push(message);
                    }
                }
                Ok(())
            })
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct FaultPublisher(Arc<Broker>);

#[async_trait::async_trait]
impl MessagePublisher for FaultPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        for message in messages {
            self.0.accept(message);
        }
        Ok(SentBatch::Ack)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Debug)]
struct FaultFactory;

#[async_trait::async_trait]
impl CustomEndpointFactory for FaultFactory {
    async fn create_consumer(
        &self,
        _: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Ok(Box::new(FaultConsumer {
            broker: broker(config),
            in_flight: Arc::default(),
            acked: Arc::default(),
            next_tag: 0,
        }))
    }

    async fn create_publisher(
        &self,
        _: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(FaultPublisher(broker(config))))
    }
}

fn endpoint(fault: Fault) -> serde_json::Value {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        register_endpoint_factory(FACTORY, Arc::new(FaultFactory)).expect("register");
    });
    json!({ "custom": { "name": FACTORY, "config": {
        "fault": format!("{fault:?}"), "queue": "$ID"
    } } })
}

async fn failed_checks(fault: Fault) -> Vec<Check> {
    let config = endpoint(fault);
    Contract::new(format!("{fault:?}"), config.clone(), config)
        .timeout(Duration::from_secs(3))
        .run()
        .await
        .failed()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_correct_broker_passes_every_check() {
    let config = endpoint(Fault::None);
    let report = Contract::new("correct", config.clone(), config)
        .timeout(Duration::from_secs(10))
        .run()
        .await;
    report.assert_passed();
    assert_eq!(report.results.len(), Check::ALL.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn every_fault_fails_exactly_its_checks() {
    let cases = [
        (
            Fault::LosesMessages,
            vec![
                Check::Parallel,
                Check::ParallelWithRejects,
                Check::RejectsThroughReconnect,
                Check::BatchSizes,
                Check::LargeBacklog,
            ],
        ),
        (
            Fault::NackActsAsAck,
            vec![
                Check::Nack,
                Check::MixedAcks,
                Check::SinkRefuses,
                Check::ParallelWithRejects,
                Check::RejectsThroughReconnect,
            ],
        ),
        (Fault::ReplaysAcked, vec![Check::MixedAcks]),
        (
            Fault::ChangesIdOnRedelivery,
            vec![Check::Nack, Check::Crash],
        ),
        (Fault::DropsMetadata, vec![Check::Metadata]),
        (Fault::SwapsOrder, vec![Check::Order]),
        (Fault::CorruptsLargePayloads, vec![Check::PayloadFidelity]),
    ];
    let mut wrong = Vec::new();
    for (fault, expected) in cases {
        let failed = failed_checks(fault).await;
        if failed != expected {
            wrong.push(format!(
                "{fault:?}: failed {failed:?}, expected {expected:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sink_only_target_runs_the_sink_checks() {
    let config = endpoint(Fault::None);
    let queues = config.clone();
    let report = Contract::sink_only("sink", config, move |id| {
        let inner = &queues["custom"]["config"];
        let target = broker(&json!({ "fault": inner["fault"], "queue": id }));
        async move {
            let queue = target.queue.lock().unwrap();
            Ok(queue.iter().map(|m| m.payload.to_vec()).collect())
        }
    })
    .timeout(Duration::from_secs(10))
    .run()
    .await;
    report.assert_passed();
    let ran = report
        .results
        .iter()
        .filter(|result| matches!(result.outcome, Outcome::Passed(_)))
        .count();
    assert_eq!(ran, 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_check_does_not_hide_the_others() {
    let config = endpoint(Fault::DropsMetadata);
    let report = Contract::new("reporting", config.clone(), config)
        .skip(Check::Order, "not under test")
        .timeout(Duration::from_secs(3))
        .run()
        .await;
    assert_eq!(report.failed(), vec![Check::Metadata]);
    assert!(!report.passed());
    let order = report
        .results
        .iter()
        .find(|result| result.check == Check::Order)
        .expect("order is reported");
    assert_eq!(order.outcome, Outcome::Skipped("not under test".into()));
}
