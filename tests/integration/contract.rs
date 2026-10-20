//! The source rows of `docs/DELIVERY.md` against real brokers: a nacked message comes
//! back, a message read by a consumer that died comes back, and `message_id` is the
//! same on the second delivery where the table says so.

#![allow(dead_code, unused_imports)]

use mq_bridge::endpoints::{create_consumer_from_route, create_publisher_from_route};
use mq_bridge::models::Endpoint;
use mq_bridge::test_utils::{run_test_with_docker, setup_logging};
use mq_bridge::traits::{MessageConsumer, MessageDisposition};
use mq_bridge::CanonicalMessage;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(90);
const IDLE: Duration = Duration::from_secs(5);

pub struct Contract {
    pub name: &'static str,
    pub compose: &'static str,
    pub input: serde_json::Value,
    pub output: serde_json::Value,
    /// "`message_id` stable across replay" in the sources table.
    pub stable_id: bool,
    /// The id the publisher was given is the one the consumer reports.
    pub hop_keeps_id: bool,
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
                    self.consumer = None;
                    continue;
                }
                Err(_) => {
                    idle += 1;
                    if idle == 2 {
                        println!("[{name}] {step}: nothing for 10 s, reconnecting");
                        self.consumer = None;
                        idle = 0;
                    }
                    continue;
                }
            };
            idle = 0;
            let mut dispositions = Vec::with_capacity(batch.messages.len());
            for message in &batch.messages {
                let payload = message.get_payload_str().to_string();
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
                self.consumer = None;
            }
            // A log replays from the nacked batch, so nacking on would never get past it.
            if matches!(disposition, Some(MessageDisposition::Nack)) && !seen.is_empty() {
                return seen;
            }
        }
        seen
    }
}

pub async fn check(contract: Contract) {
    if !mq_bridge::test_utils::should_run(contract.name) {
        return;
    }
    setup_logging();
    let name = contract.name;
    run_test_with_docker(contract.compose, || async move {
        let endpoint = |value| serde_json::from_value::<Endpoint>(value).expect("endpoint config");
        let mut source = Source {
            name,
            endpoint: endpoint(contract.input),
            consumer: None,
        };
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
        let after = source
            .deliveries("after the crash", &crashed, Some(MessageDisposition::Ack))
            .await;
        if contract.stable_id {
            assert_eq!(before, after, "[{name}] the id changed after a crash");
        }
        println!("[{name}] delivery contract holds");
    })
    .await;
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
    check(Contract {
        name: "nats",
        compose: compose("nats"),
        input: json!({ "nats": nats }),
        output: json!({ "nats": nats }),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
}

#[cfg(feature = "amqp")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_amqp_contract() {
    let amqp = json!({ "url": "amqp://guest:guest@localhost:5672/%2f", "queue": "contract" });
    check(Contract {
        name: "amqp",
        compose: compose("amqp"),
        input: json!({ "amqp": amqp }),
        output: json!({ "amqp": amqp }),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
}

#[cfg(feature = "kafka")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_kafka_contract() {
    check(Contract {
        name: "kafka",
        compose: compose("kafka"),
        input: json!({ "kafka": { "url": "localhost:9092", "topic": "contract", "group_id": "contract" } }),
        output: json!({ "kafka": { "url": "localhost:9092", "topic": "contract" } }),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
}

#[cfg(feature = "redis-streams")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_redis_streams_contract() {
    check(Contract {
        name: "redis_streams",
        compose: compose("redis"),
        // The 60 s default would make each redelivery step wait a minute.
        input: json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract", "group": "contract", "redelivery_timeout_ms": 1000 } }),
        output: json!({ "redis_streams": { "url": "redis://localhost:6379", "stream": "contract" } }),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
}

#[cfg(feature = "mongodb")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_mongodb_contract() {
    let mongodb = json!({ "url": "mongodb://localhost:27017", "database": "contract", "collection": "contract" });
    let mut input = mongodb.clone();
    input["consume"] = json!("consumer");
    check(Contract {
        name: "mongodb",
        compose: compose("mongodb"),
        input: json!({ "mongodb": input }),
        output: json!({ "mongodb": mongodb }),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
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
    check(Contract {
        name: "mqtt",
        compose: compose("mqtt"),
        input: mqtt("contract-in"),
        output: mqtt("contract-out"),
        stable_id: true,
        hop_keeps_id: true,
    })
    .await;
}

#[cfg(feature = "sqlx")]
#[tokio::test]
#[ignore = "requires docker compose"]
async fn test_postgres_contract() {
    let url = "postgres://testuser:testpass@localhost:5432/testdb";
    check(Contract {
        name: "postgres",
        compose: compose("postgres"),
        input: json!({ "sqlx": { "url": url, "table": "contract", "delete_after_read": true, "polling_interval_ms": 50 } }),
        output: json!({ "sqlx": { "url": url, "table": "contract", "auto_create_table": true } }),
        // The table row has no id column the reader would take one from.
        stable_id: false,
        hop_keeps_id: false,
    })
    .await;
}
