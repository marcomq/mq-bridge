//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Running a [`Route`]: read batches from the input, send them to the output, commit them.
//!
//! # Lifecycle
//!
//! 1. **Check** (`checks.rs`): before anything connects, `check_with_resume` loads named
//!    plugins, rejects `random_panic` unless `allow_fault_injection` is set, and refuses replay setups that would
//!    duplicate or lose data. `replay.rs` decides where the source resumes and which
//!    [`DeliveryGuarantee`] the route has.
//! 2. **Start** (`run.rs`): [`Route::run`] spawns one task holding the reconnect loop and
//!    waits until the first connection is ready. Each attempt runs `run_until_err`, which
//!    picks a runner:
//!    - a fast path for endpoint pairs that copy without the batch loop
//!      (`endpoints::try_run_fast_path_route`);
//!    - `run_sequentially` for `concurrency: 1`: one send at a time in source order,
//!      while the next batch is read;
//!    - `run_concurrently` otherwise: a worker pool, with an `OrderTicket` when the sink
//!      must see batches in order.
//! 3. **Send and commit** (`send.rs`, `commit.rs`): `send_batch_and_commit` sends one batch,
//!    maps the responses to per-message dispositions, applies the failure policy (retry,
//!    drop with a record, stop the route), then commits through the `CommitRouter`:
//!    in order through a sequencer for cumulative-ack sources (Kafka, AMQP), concurrently
//!    otherwise. A publisher with deferred commits holds them in a `FlushGate` until
//!    its next flush.
//! 4. **Stop**: the shutdown channel ends the loop; in-flight sends finish, held commits
//!    flush, unpicked batches are nacked. `hooks.rs` runs the endpoints' connect and
//!    disconnect hooks around each attempt. [`RouteOutcome`] (`handle.rs`) says why it ended.
//!
//! An error inside an attempt reconnects after `reconnect_interval_ms`; an `exit_on_empty`
//! route gives up after a bounded number of reconnects.
//!
//! # State
//!
//! | What | Where | Holds |
//! |---|---|---|
//! | deployed routes | `ROUTE_REGISTRY` (`registry.rs`) | name → route + [`RouteHandle`], filled by [`Route::deploy`] |
//! | named endpoints | `ENDPOINT_REF_REGISTRY` (`registry.rs`) | name → endpoint for `ref:`, see [`register_endpoint`] |
//! | ref overlay | `ENDPOINT_REF_OVERLAY` (`registry.rs`, thread-local) | see [`with_endpoint_overrides`] |
//! | display names | `ROUTE_DISPLAY_NAMES` (`registry.rs`) | key → name shown in logs and errors |
//! | read positions | `RUN_POSITIONS` (`send.rs`, task-local) | per run: where a source without a checkpoint resumes after a reconnect |
//! | per-route status | [`RouteHandle`] | health, outcome, dropped messages, sends in flight |
//!
//! [`Route::run`] does not register the route; only [`Route::deploy`] does.
//!
//! # Files
//!
//! | File | Does |
//! |---|---|
//! | `builder.rs` | `with_*` options and handlers |
//! | `checks.rs`, `replay.rs` | pre-start checks; resume position and delivery guarantee |
//! | `run.rs` | reconnect loop, sequential and concurrent runners |
//! | `send.rs`, `commit.rs`, `flush_gate.rs` | one batch: send, failure policy, ordered or concurrent commit, deferred commits |
//! | `handle.rs`, `hooks.rs`, `registry.rs` | handle and outcome; connect hooks; process-wide registries |

use crate::endpoints::{
    check_source_position_available, create_consumer_from_route,
    create_consumer_from_route_with_policy, create_publisher_from_route_with_source_position,
    output_has_write_time_named_object_store, output_keys_on_source_position,
    output_passes_through_http_status, output_requires_source_metadata, relax_object_naming,
    supports_source_metadata,
};
use crate::errors::ProcessingError;
pub use crate::models::Route;
use crate::models::{DeliveryGuarantee, Endpoint, EndpointType, Middleware, NameBy, RouteOptions};
use crate::traits::{
    with_disconnect_outcome, BatchCommitFunc, ConsumerError, DisconnectOutcome, EndpointStatus,
    Handler, HandlerError, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError,
    SentBatch,
};
use async_channel::{bounded, Sender};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::{
    select,
    task::{JoinHandle, JoinSet},
};
use tracing::{debug, error, info, trace, warn};

mod builder;
mod checks;
mod commit;
mod flush_gate;
mod handle;
mod hooks;
mod registry;
mod replay;
mod run;
mod send;
#[cfg(test)]
mod tests;

use commit::*;
use flush_gate::FlushGate;
use replay::*;
use send::*;

use handle::OutcomeGuard;
pub(crate) use handle::{DropReport, PendingSends};
pub use handle::{RouteHandle, RouteOutcome};
pub(crate) use hooks::{
    disconnect_outcome, publisher_disconnect_result, run_consumer_connect_hook,
    run_consumer_disconnect_hook, run_publisher_connect_hook, run_publisher_disconnect_hook,
};
pub(crate) use registry::route_display_name;
use registry::*;
pub use registry::{
    get_endpoint, get_route, list_routes, register_endpoint, route_outcome, route_status,
    set_route_display_name, stop_route, unregister_endpoint, with_endpoint_overrides,
};
#[cfg(test)]
pub(crate) use replay::source_is_at_most_once;
pub(crate) use send::{RunPositions, RUN_POSITIONS};

// Re-export extensions for backward compatibility and internal usage
pub use crate::extensions::{
    get_endpoint_factory, get_middleware_factory, register_endpoint_factory,
    register_middleware_factory,
};
