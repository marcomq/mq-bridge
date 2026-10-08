use std::collections::BTreeMap;
use std::ffi::c_char;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::anyhow;
use mq_bridge_bindings_common as common;
use mqb::errors::ConsumerError;
use mqb::models::Endpoint;
use mqb::support::plugin_abi::MqbStatus;
use mqb::traits::{BatchCommitFunc, MessageConsumer, MessageDisposition};
use tokio::runtime::Runtime;

use crate::message::mqb_message_t;
use crate::{guard, guard_new, handle, into_c_string, optional_name, parse_config, text};

type Pending = BTreeMap<u32, (BatchCommitFunc, usize)>;

/// A pull-based consumer over one input endpoint. `mqb_consumer_poll` does not
/// acknowledge: each batch stays outstanding until it is acked, nacked or committed.
pub struct mqb_consumer_t {
    runtime: Runtime,
    // `None` once closed.
    consumer: tokio::sync::Mutex<Option<Box<dyn MessageConsumer>>>,
    // Outstanding batches by token; ordered so commit acks oldest-first.
    pending: Mutex<Pending>,
    next_token: AtomicU32,
    exhausted: AtomicBool,
    // Cumulative-ack transports (Kafka, ...): token acks must stay oldest-first.
    requires_order: bool,
}

/// The messages of one `mqb_consumer_poll`, owned by the batch.
pub struct mqb_batch_t {
    messages: Vec<mqb_message_t>,
    token: u32,
}

fn build(name: Option<&str>, endpoint: Endpoint) -> anyhow::Result<mqb_consumer_t> {
    let name = name.map_or_else(common::default_route_name, str::to_string);
    let runtime = common::build_runtime()?;
    let consumer =
        runtime.block_on(mqb::endpoints::create_consumer_from_route(&name, &endpoint))?;
    let requires_order = consumer.commit_requires_order();
    Ok(mqb_consumer_t {
        runtime,
        consumer: tokio::sync::Mutex::new(Some(consumer)),
        pending: Mutex::new(BTreeMap::new()),
        next_token: AtomicU32::new(0),
        exhausted: AtomicBool::new(false),
        requires_order,
    })
}

impl mqb_consumer_t {
    fn lock_pending(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Pending>> {
        self.pending
            .lock()
            .map_err(|_| anyhow!("consumer commit lock poisoned"))
    }

    fn receive(&self, max: u32, timeout_ms: i64) -> anyhow::Result<Option<mqb_batch_t>> {
        self.runtime.block_on(async {
            // The token is taken under the consumer lock, so token order is receive order.
            let mut guard = self.consumer.lock().await;
            let consumer = guard.as_mut().ok_or_else(|| anyhow!("consumer is closed"))?;
            let recv = consumer.receive_batch(max.max(1) as usize);
            let batch = if timeout_ms >= 0 {
                match tokio::time::timeout(Duration::from_millis(timeout_ms as u64), recv).await {
                    Ok(result) => result,
                    Err(_) => return Ok(None),
                }
            } else {
                recv.await
            };
            match batch {
                Ok(batch) if batch.messages.is_empty() => Ok(None),
                Ok(batch) => {
                    let token = self.next_token.fetch_add(1, Ordering::SeqCst);
                    let mut pending = self.lock_pending()?;
                    if pending.contains_key(&token) {
                        return Err(anyhow!(
                            "batch token space exhausted (counter wrapped with batches still outstanding)"
                        ));
                    }
                    pending.insert(token, (batch.commit, batch.messages.len()));
                    Ok(Some(mqb_batch_t {
                        messages: batch.messages.into_iter().map(mqb_message_t::wrap).collect(),
                        token,
                    }))
                }
                Err(ConsumerError::EndOfStream) => {
                    self.exhausted.store(true, Ordering::SeqCst);
                    Ok(None)
                }
                Err(err) => Err(err.into()),
            }
        })
    }

    /// Settles every outstanding batch, oldest first. Batches after a failure stay outstanding.
    fn settle_all(&self, disposition: MessageDisposition) -> anyhow::Result<()> {
        let mut batches = std::mem::take(&mut *self.lock_pending()?).into_iter();
        let mut failure = None;
        for (_token, (commit, len)) in batches.by_ref() {
            if let Err(err) = self
                .runtime
                .block_on(commit(vec![disposition.clone(); len]))
            {
                failure = Some(err);
                break;
            }
        }
        self.lock_pending()?.extend(batches);
        failure.map_or(Ok(()), Err)
    }

    fn settle_one(&self, token: u32, disposition: MessageDisposition) -> anyhow::Result<()> {
        let entry = {
            let mut pending = self.lock_pending()?;
            if matches!(disposition, MessageDisposition::Ack) && self.requires_order {
                if let Some((&oldest, _)) = pending.iter().next() {
                    if token != oldest && pending.contains_key(&token) {
                        return Err(anyhow!(
                            "cannot ack batch token {token} before older outstanding token {oldest}: \
                             this transport commits cumulatively, so acks must follow receive order"
                        ));
                    }
                }
            }
            pending.remove(&token)
        };
        let (commit, len) = entry.ok_or_else(|| {
            anyhow!("unknown batch token {token} (already settled, or never polled)")
        })?;
        self.runtime.block_on(commit(vec![disposition; len]))
    }
}

/// Builds a consumer from a YAML or JSON config file. `name` selects an entry of a
/// `consumers:` document; null or `""` for a single bare endpoint. Null on error.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_from_file(
    path: *const c_char,
    name: *const c_char,
) -> *mut mqb_consumer_t {
    guard_new(|| {
        let path = unsafe { text(path, "path") }?;
        let name = unsafe { optional_name(name) }?;
        build(name, common::load_named_consumer(Path::new(path), name)?)
    })
}

/// Like `mqb_consumer_from_file`, from YAML or JSON text.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_from_str(
    config: *const c_char,
    name: *const c_char,
) -> *mut mqb_consumer_t {
    guard_new(|| {
        let value = parse_config(unsafe { text(config, "config") }?)?;
        let name = unsafe { optional_name(name) }?;
        build(name, common::named_consumer_from_value(value, name)?)
    })
}

/// Receives up to `max` messages into `*batch`. `timeout_ms < 0` blocks until
/// something arrives, and `mqb_consumer_close` / `mqb_consumer_status_json` on
/// another thread wait for it. `*batch` is null when the timeout passed or the source is
/// exhausted (see `mqb_consumer_exhausted`); otherwise free it with `mqb_batch_free`.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_poll(
    consumer: *const mqb_consumer_t,
    max: u32,
    timeout_ms: i64,
    batch: *mut *mut mqb_batch_t,
) -> MqbStatus {
    guard(|| {
        let consumer = unsafe { handle(consumer) }?;
        if batch.is_null() {
            return Err(anyhow!("null output pointer"));
        }
        unsafe { *batch = std::ptr::null_mut() };
        if let Some(received) = consumer.receive(max, timeout_ms)? {
            unsafe { *batch = Box::into_raw(Box::new(received)) };
        }
        Ok(())
    })
}

#[no_mangle]
pub unsafe extern "C" fn mqb_batch_count(batch: *const mqb_batch_t) -> usize {
    unsafe { handle(batch) }.map_or(0, |batch| batch.messages.len())
}

/// The message at `index`, owned by the batch; null when out of range.
#[no_mangle]
pub unsafe extern "C" fn mqb_batch_at(
    batch: *const mqb_batch_t,
    index: usize,
) -> *const mqb_message_t {
    unsafe { handle(batch) }
        .ok()
        .and_then(|batch| batch.messages.get(index))
        .map_or(std::ptr::null(), std::ptr::from_ref)
}

/// The token for `mqb_consumer_ack` / `mqb_consumer_nack`.
#[no_mangle]
pub unsafe extern "C" fn mqb_batch_token(batch: *const mqb_batch_t) -> u32 {
    unsafe { handle(batch) }.map_or(0, |batch| batch.token)
}

/// Frees the batch and its messages. Does not acknowledge them.
#[no_mangle]
pub unsafe extern "C" fn mqb_batch_free(batch: *mut mqb_batch_t) {
    if !batch.is_null() {
        drop(unsafe { Box::from_raw(batch) });
    }
}

/// Acknowledges every outstanding batch, oldest first. Without an ack or commit
/// the source redelivers and most brokers eventually stall.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_commit(consumer: *const mqb_consumer_t) -> MqbStatus {
    guard(|| unsafe { handle(consumer) }?.settle_all(MessageDisposition::Ack))
}

/// Acknowledges one batch by its token.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_ack(
    consumer: *const mqb_consumer_t,
    token: u32,
) -> MqbStatus {
    guard(|| unsafe { handle(consumer) }?.settle_one(token, MessageDisposition::Ack))
}

/// Negatively acknowledges one batch so the broker can redeliver it.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_nack(
    consumer: *const mqb_consumer_t,
    token: u32,
) -> MqbStatus {
    guard(|| unsafe { handle(consumer) }?.settle_one(token, MessageDisposition::Nack))
}

/// Negatively acknowledges every outstanding batch, oldest first.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_nack_all(consumer: *const mqb_consumer_t) -> MqbStatus {
    guard(|| unsafe { handle(consumer) }?.settle_all(MessageDisposition::Nack))
}

/// Status snapshot of the endpoint as JSON (`healthy`, `target`, optional
/// `pending` backlog, ...), or null on error. Free with `mqb_string_free`.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_status_json(consumer: *const mqb_consumer_t) -> *mut c_char {
    let mut out = std::ptr::null_mut();
    guard(|| {
        let consumer = unsafe { handle(consumer) }?;
        let status = consumer.runtime.block_on(async {
            let guard = consumer.consumer.lock().await;
            let inner = guard
                .as_ref()
                .ok_or_else(|| anyhow!("consumer is closed"))?;
            anyhow::Ok(serde_json::to_string(&inner.status().await)?)
        })?;
        out = into_c_string(status);
        Ok(())
    });
    out
}

/// True once the source signalled end-of-stream (e.g. a drained file).
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_exhausted(consumer: *const mqb_consumer_t) -> bool {
    unsafe { handle(consumer) }.is_ok_and(|consumer| consumer.exhausted.load(Ordering::SeqCst))
}

/// Releases the endpoint connection. Idempotent; polling fails afterwards.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_close(consumer: *const mqb_consumer_t) -> MqbStatus {
    guard(|| {
        let consumer = unsafe { handle(consumer) }?;
        consumer.runtime.block_on(async {
            if let Some(mut inner) = consumer.consumer.lock().await.take() {
                inner.close().await?;
            }
            Ok(())
        })
    })
}

/// Closes and frees the consumer. Outstanding batches are left unacknowledged.
#[no_mangle]
pub unsafe extern "C" fn mqb_consumer_free(consumer: *mut mqb_consumer_t) {
    if !consumer.is_null() {
        unsafe { mqb_consumer_close(consumer) };
        drop(unsafe { Box::from_raw(consumer) });
    }
}
