//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The stable C ABI between an mq-bridge host process and a dynamically loaded
//! endpoint plugin.
//!
//! Deliberately tiny and self-contained: no logic beyond version checking, only
//! `#[repr(C)]` types, status codes, and the shape of the exported function
//! table. Hosts use [`MqbPluginVTable`](crate::support::plugin_abi::MqbPluginVTable)
//! through [`crate::plugin`]; plugin
//! authors should use `mq-bridge-plugin-sdk` instead of implementing these
//! functions by hand.
//!
//! It is versioned by
//! [`MQB_PLUGIN_ABI_MAJOR`](crate::support::plugin_abi::MQB_PLUGIN_ABI_MAJOR) /
//! [`MQB_PLUGIN_ABI_MINOR`](crate::support::plugin_abi::MQB_PLUGIN_ABI_MINOR),
//! independently of the mq-bridge release it ships in — those constants, not
//! the crate version, are what a host checks before calling a plugin.
//!
//! # What may cross this boundary
//!
//! Only C-compatible data: integers, raw pointers, `#[repr(C)]` structs and
//! `extern "C"` function pointers. No Rust trait object, future, `String`,
//! `Vec`, closure, or error type is ever passed across it, and a plugin must
//! never let a panic unwind out of an ABI function (return
//! [`MQB_ERR_PANIC`](crate::support::plugin_abi::MQB_ERR_PANIC) instead).
//!
//! # Ownership rules
//!
//! * **Host → plugin data**
//!   ([`MqbSlice`](crate::support::plugin_abi::MqbSlice),
//!   [`MqbMessage`](crate::support::plugin_abi::MqbMessage) arrays passed as
//!   arguments) is owned by the host and is only valid for the duration of the
//!   call. A plugin that needs it later must copy it.
//! * **Plugin → host data**
//!   ([`MqbMessage`](crate::support::plugin_abi::MqbMessage) arrays returned from
//!   [`MqbPluginVTable::consumer_receive_batch`](crate::support::plugin_abi::MqbPluginVTable::consumer_receive_batch)) is owned by the plugin and
//!   stays valid until the batch handle is committed or freed.
//! * **Error text** is returned as an
//!   [`MqbBuffer`](crate::support::plugin_abi::MqbBuffer) allocated by the plugin.
//!   The host must hand every non-empty buffer back to
//!   [`MqbPluginVTable::buffer_free`](crate::support::plugin_abi::MqbPluginVTable::buffer_free).
//! * **Handles** are opaque plugin-owned pointers. Each has exactly one
//!   `*_free` function, and freeing is the host's responsibility. A handle must
//!   be safe to use and free from any thread (`Send`), and consumer/publisher
//!   handles must tolerate concurrent calls from different threads (`Sync`).
//!
//! # Versioning
//!
//! [`MQB_PLUGIN_ABI_MAJOR`](crate::support::plugin_abi::MQB_PLUGIN_ABI_MAJOR)
//! changes for any incompatible change; a host rejects
//! a plugin whose major differs. Within a major version, fields may only be
//! *appended* to
//! [`MqbPluginVTable`](crate::support::plugin_abi::MqbPluginVTable), and both
//! sides use
//! [`MqbPluginVTable::struct_size`](crate::support::plugin_abi::MqbPluginVTable::struct_size)
//! to discover which fields exist.

#![allow(clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::fmt;

/// Incompatible-change counter. A host refuses a plugin with a different major.
pub const MQB_PLUGIN_ABI_MAJOR: u32 = 1;
/// Additive-change counter. A host accepts any minor, old or new.
///
/// * **1.1** appended [`MqbPluginVTable::publisher_requires_ordered_publish`],
///   [`MqbPluginVTable::publisher_send_batch_outcomes`] and
///   [`MqbPluginVTable::factory_config_schema`].
/// * **1.2** appended request/reply ([`MqbPluginVTable::publisher_send_batch_responses`],
///   [`MqbPluginVTable::responses_free`], [`MqbPluginVTable::batch_commit_replies`])
///   status ([`MqbPluginVTable::consumer_status`], [`MqbPluginVTable::publisher_status`])
///   non-blocking twins of the hot-path calls ([`MqbCompletion`]) and host
///   services for logs, metrics and crash handlers ([`MqbHostVTable`]).
pub const MQB_PLUGIN_ABI_MINOR: u32 = 2;

/// Name of the discovery symbol a plugin shared library must export.
///
/// Its type is [`MqbPluginEntry`]. The trailing NUL is included so the value
/// can be passed straight to a dynamic loader.
pub const MQB_PLUGIN_ENTRY_SYMBOL: &[u8] = b"mq_bridge_plugin_v1\0";

/// Signature of the exported discovery symbol.
///
/// Returns a pointer to a table with `'static` lifetime inside the plugin
/// library. It must never return null and must be callable before any other
/// plugin function.
pub type MqbPluginEntry = unsafe extern "C" fn() -> *const MqbPluginVTable;

/// Name of the optional symbol through which one library exports several
/// tables (1.2). Its type is [`MqbPluginListEntry`].
pub const MQB_PLUGIN_LIST_SYMBOL: &[u8] = b"mq_bridge_plugin_v1_at\0";

/// Returns the table at `index`, or null past the last one. Index 0 must be the
/// table [`MQB_PLUGIN_ENTRY_SYMBOL`] returns, so a 1.0/1.1 host loads that one.
pub type MqbPluginListEntry = unsafe extern "C" fn(index: usize) -> *const MqbPluginVTable;

/// Result of an ABI call. `0` is success; every other value is a failure whose
/// class the host maps onto its own error types.
pub type MqbStatus = i32;

/// The call succeeded.
pub const MQB_OK: MqbStatus = 0;
/// Transient failure. The host may retry the operation.
pub const MQB_ERR_RETRYABLE: MqbStatus = 1;
/// Permanent failure. Retrying cannot help.
pub const MQB_ERR_PERMANENT: MqbStatus = 2;
/// The endpoint configuration is invalid. Never retried.
pub const MQB_ERR_INVALID_CONFIG: MqbStatus = 3;
/// The source is exhausted and will produce no further messages.
pub const MQB_END_OF_STREAM: MqbStatus = 4;
/// A panic was caught inside the plugin. Treated as permanent.
pub const MQB_ERR_PANIC: MqbStatus = 5;
/// The plugin does not implement this operation (e.g. it is output-only).
pub const MQB_ERR_UNSUPPORTED: MqbStatus = 6;
/// Connection-level failure. The host reconnects the endpoint.
pub const MQB_ERR_CONNECTION: MqbStatus = 7;

/// Acknowledge the message: it was processed successfully.
pub const MQB_DISPOSITION_ACK: u8 = 0;
/// Negatively acknowledge the message so the broker can redeliver it.
pub const MQB_DISPOSITION_NACK: u8 = 1;
/// Acknowledge the message and send the parallel reply (ABI 1.2,
/// [`MqbPluginVTable::batch_commit_replies`] only).
pub const MQB_DISPOSITION_REPLY: u8 = 2;

/// The message was published. Per-message counterpart of [`MQB_OK`], written by
/// [`MqbPluginVTable::publisher_send_batch_outcomes`].
pub const MQB_OUTCOME_OK: u8 = 0;
/// This message failed transiently; the host may send it again.
pub const MQB_OUTCOME_RETRYABLE: u8 = 1;
/// This message failed permanently. Sending it again cannot help.
pub const MQB_OUTCOME_PERMANENT: u8 = 2;

/// The plugin can create consumers (input endpoints).
pub const MQB_CAP_CONSUMER: u64 = 1 << 0;
/// The plugin can create publishers (output endpoints).
pub const MQB_CAP_PUBLISHER: u64 = 1 << 1;
/// The plugin provides a middleware under the same name.
pub const MQB_CAP_MIDDLEWARE: u64 = 1 << 2;

/// Asks [`MqbPluginVTable::factory_config_schema`] for the endpoint's
/// configuration object.
pub const MQB_SCHEMA_ENDPOINT: u32 = 0;
/// Asks [`MqbPluginVTable::factory_config_schema`] for the middleware's
/// configuration object.
pub const MQB_SCHEMA_MIDDLEWARE: u32 = 1;

/// Middleware sitting on an input endpoint: it sees each batch after the source
/// produced it.
pub const MQB_MIDDLEWARE_RECEIVE: u8 = 0;
/// Middleware sitting on an output endpoint: it sees each batch before the sink
/// does.
pub const MQB_MIDDLEWARE_SEND: u8 = 1;

/// The middleware dropped this message: the corresponding entry of the message
/// array is unspecified and must not be read.
pub const MQB_MESSAGE_DROPPED: u8 = 0;
/// The middleware kept this message, possibly rewritten.
pub const MQB_MESSAGE_KEPT: u8 = 1;

/// [`MqbPluginVTable::factory_delivery`] flag: a publisher built from the config absorbs replays.
pub const MQB_DELIVERY_IDEMPOTENT_SINK: u8 = 1 << 0;
/// [`MqbPluginVTable::factory_delivery`] flag: a consumer built from the config acknowledges.
pub const MQB_DELIVERY_ACKNOWLEDGES: u8 = 1 << 1;

/// A borrowed, non-owning view of bytes. Lifetime is defined by whichever side
/// produced it; see the crate-level ownership rules.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MqbSlice {
    pub ptr: *const u8,
    pub len: usize,
}

impl MqbSlice {
    /// An empty slice. `ptr` is dangling-but-aligned, never dereferenced.
    pub const EMPTY: MqbSlice = MqbSlice {
        ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(),
        len: 0,
    };

    /// Borrows `bytes`. The caller keeps responsibility for outliving the slice.
    pub const fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }

    /// Borrows `text` as UTF-8 bytes.
    pub const fn from_str(text: &str) -> Self {
        Self::from_bytes(text.as_bytes())
    }

    /// # Safety
    /// `ptr`/`len` must describe an initialised region that outlives `'a`.
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        if self.len == 0 {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// A buffer allocated by the plugin and returned to the host, used for error
/// text. The host must return it to [`MqbPluginVTable::buffer_free`] exactly
/// once; a buffer with a null `ptr` or zero `len` carries no message and needs
/// no release.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MqbBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl MqbBuffer {
    pub const EMPTY: MqbBuffer = MqbBuffer {
        ptr: core::ptr::null_mut(),
        len: 0,
        cap: 0,
    };

    pub fn is_empty(&self) -> bool {
        self.ptr.is_null() || self.len == 0
    }

    /// # Safety
    /// The buffer must not have been freed yet.
    pub unsafe fn as_bytes<'a>(&self) -> &'a [u8] {
        if self.is_empty() {
            return &[];
        }
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// One metadata entry of a message. Both halves are UTF-8.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MqbKeyValue {
    pub key: MqbSlice,
    pub value: MqbSlice,
}

/// A message in transit across the ABI.
///
/// `message_id` is a big-endian 128-bit id (mq-bridge uses UUIDv7). All-zero
/// means "no id"; the receiving side then generates one.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MqbMessage {
    pub message_id: [u8; 16],
    pub payload: MqbSlice,
    /// Pointer to `metadata_len` entries; may be null when `metadata_len` is 0.
    pub metadata: *const MqbKeyValue,
    pub metadata_len: usize,
}

/// Opaque handle to a plugin's endpoint factory.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbFactoryHandle(pub *mut c_void);

/// Opaque handle to a plugin consumer (input endpoint).
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbConsumerHandle(pub *mut c_void);

/// Opaque handle to a plugin publisher (output endpoint).
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbPublisherHandle(pub *mut c_void);

/// Opaque handle to one received batch, holding the broker-side state needed to
/// acknowledge it later.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbBatchHandle(pub *mut c_void);

/// Opaque handle to a middleware instance, bound to one route and side.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbMiddlewareHandle(pub *mut c_void);

/// Opaque handle to the result of one middleware call, owning the arrays it
/// handed back.
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbFilterHandle(pub *mut c_void);

/// Plugin-owned publish responses, released with
/// [`MqbPluginVTable::responses_free`] (ABI 1.2).
#[repr(transparent)]
#[derive(Copy, Clone, Debug)]
pub struct MqbResponsesHandle(pub *mut c_void);

/// `MQB_LOG_*`: severity of an event passed to [`MqbHostVTable::log`] (ABI 1.2).
pub const MQB_LOG_ERROR: u8 = 1;
pub const MQB_LOG_WARN: u8 = 2;
pub const MQB_LOG_INFO: u8 = 3;
pub const MQB_LOG_DEBUG: u8 = 4;
pub const MQB_LOG_TRACE: u8 = 5;

/// `MQB_METRIC_*`: what [`MqbHostVTable::metric`] does with its value (ABI 1.2).
pub const MQB_METRIC_COUNTER: u8 = 0;
pub const MQB_METRIC_COUNTER_ABSOLUTE: u8 = 1;
pub const MQB_METRIC_GAUGE_SET: u8 = 2;
/// Adds to a gauge; a negative value decrements it.
pub const MQB_METRIC_GAUGE_ADD: u8 = 3;
pub const MQB_METRIC_HISTOGRAM: u8 = 4;

/// What a crash handler learns about the fatal signal (ABI 1.2).
#[repr(C)]
pub struct MqbCrashInfo {
    /// `size_of::<MqbCrashInfo>()` as compiled into the host; fields may be appended.
    pub struct_size: usize,
    /// The signal number: `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE` or `SIGABRT`.
    pub signal: i32,
    /// `siginfo_t::si_code`.
    pub code: i32,
    /// `siginfo_t::si_addr`: the faulting address, for the signals that have one.
    pub fault_address: *const c_void,
    /// The interrupted instruction, or null where the host cannot read it.
    pub pc: *const c_void,
    /// The raw `siginfo_t *` the host's signal handler received.
    pub siginfo: *const c_void,
    /// The raw `ucontext_t *` the host's signal handler received.
    pub ucontext: *const c_void,
}

/// A plugin's crash handler, registered through
/// [`MqbHostVTable::register_crash_handler`] (ABI 1.2).
///
/// It runs inside the host's signal handler, possibly on a small alternate
/// stack: only async-signal-safe calls (`write`, `backtrace_symbols_fd`), no
/// `malloc`, `printf` or locks. The process dies after it returns; recovering
/// by `siglongjmp` is unsupported.
pub type MqbCrashHandler =
    Option<unsafe extern "C" fn(user_data: *mut c_void, info: *const MqbCrashInfo)>;

/// Services the host offers a plugin, handed over once through
/// [`MqbPluginVTable::plugin_init`] (ABI 1.2).
///
/// Lives as long as the process. Every function may be called from any thread,
/// never blocks, and borrows its arguments only for the call.
#[repr(C)]
pub struct MqbHostVTable {
    /// `size_of::<MqbHostVTable>()` as compiled into the host; fields may be appended.
    pub struct_size: usize,
    /// Non-zero if the host records events at `MQB_LOG_*` `level`.
    pub log_enabled: unsafe extern "C" fn(level: u8) -> u8,
    /// Records one event; `target` is the plugin's module path, `message` the
    /// rendered text including its fields.
    pub log: unsafe extern "C" fn(level: u8, target: MqbSlice, message: MqbSlice),
    /// Records one metric sample; `kind` is an `MQB_METRIC_*` code.
    pub metric: unsafe extern "C" fn(
        kind: u8,
        name: MqbSlice,
        labels: *const MqbKeyValue,
        labels_len: usize,
        value: f64,
    ),
    /// Runs `handler` with `user_data` when the process gets a fatal signal,
    /// before it dies. `MQB_ERR_UNSUPPORTED` where the host has no crash
    /// handling (Windows), `MQB_ERR_PERMANENT` for a null handler or once all
    /// slots are taken.
    pub register_crash_handler:
        unsafe extern "C" fn(handler: MqbCrashHandler, user_data: *mut c_void) -> MqbStatus,
}

/// Size of the 1.2 [`MqbHostVTable`]; a plugin reads no field past a host's
/// `struct_size`.
pub const MQB_HOST_VTABLE_SIZE_V1_2: usize = 5 * core::mem::size_of::<usize>();

/// Where an asynchronous 1.2 call reports that it finished.
///
/// If the starting call returns [`MQB_OK`], the plugin invokes `callback(ctx,
/// status)` exactly once, from any thread, possibly before the starting call
/// returns; any other return means it never does. Out-parameters stay writable
/// until the callback, which must not block. The host sets no deadline: a
/// callback that never comes holds the call and its buffers until shutdown.
///
/// A plugin that returns [`MQB_ERR_UNSUPPORTED`] from a non-blocking entry gets
/// its blocking twin instead, from then on for that endpoint.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MqbCompletion {
    pub callback: unsafe extern "C" fn(ctx: *mut c_void, status: MqbStatus),
    pub ctx: *mut c_void,
}

macro_rules! handle_helpers {
    ($($ty:ident),+ $(,)?) => {$(
        impl $ty {
            pub const NULL: $ty = $ty(core::ptr::null_mut());

            pub fn is_null(&self) -> bool {
                self.0.is_null()
            }
        }
    )+};
}
handle_helpers!(
    MqbFactoryHandle,
    MqbConsumerHandle,
    MqbPublisherHandle,
    MqbBatchHandle,
    MqbMiddlewareHandle,
    MqbFilterHandle,
    MqbResponsesHandle,
);

/// The function table a plugin exports through [`MQB_PLUGIN_ENTRY_SYMBOL`].
///
/// Every fallible function takes an `err` out-parameter. On a non-[`MQB_OK`]
/// return the plugin may write an owned [`MqbBuffer`] holding UTF-8 error text;
/// on [`MQB_OK`] it must leave the buffer empty. All calls are blocking: the
/// host invokes them off its async executor, and the plugin drives its own
/// runtime internally. The `*_async` entries (1.2) are the exception.
///
/// Fields may only be appended in later minor versions. Readers must check
/// [`struct_size`](Self::struct_size) before touching a field added after 1.0.
#[repr(C)]
pub struct MqbPluginVTable {
    /// `size_of::<MqbPluginVTable>()` as compiled into the plugin.
    pub struct_size: usize,
    /// Must equal [`MQB_PLUGIN_ABI_MAJOR`] for the host to accept the plugin.
    pub abi_major: u32,
    /// Highest minor version the plugin was built against.
    pub abi_minor: u32,
    /// Bit set of `MQB_CAP_*` flags.
    pub capabilities: u64,
    /// Endpoint name to register under, e.g. `pulsar`. UTF-8, `'static`.
    pub name: MqbSlice,
    /// Human-readable plugin version, e.g. its crate version. UTF-8, `'static`.
    pub version: MqbSlice,

    /// Creates the factory. Called once per loaded library.
    pub factory_create:
        unsafe extern "C" fn(out: *mut MqbFactoryHandle, err: *mut MqbBuffer) -> MqbStatus,
    /// Releases a factory handle. Null is a no-op.
    pub factory_free: unsafe extern "C" fn(factory: MqbFactoryHandle),
    /// Releases a buffer previously handed to the host. Empty is a no-op.
    pub buffer_free: unsafe extern "C" fn(buffer: MqbBuffer),

    /// Opens a consumer. `config_json` is the endpoint's configuration object
    /// encoded as UTF-8 JSON.
    pub consumer_create: unsafe extern "C" fn(
        factory: MqbFactoryHandle,
        route_name: MqbSlice,
        config_json: MqbSlice,
        out: *mut MqbConsumerHandle,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Receives up to `max_messages` messages.
    ///
    /// On [`MQB_OK`] the plugin writes a batch handle plus a pointer to
    /// `*out_len` messages. Both stay valid until the batch is committed or
    /// freed. `*out_len == 0` means "idle, nothing available" and the host
    /// still receives (and must release) a batch handle.
    pub consumer_receive_batch: unsafe extern "C" fn(
        consumer: MqbConsumerHandle,
        max_messages: usize,
        out_batch: *mut MqbBatchHandle,
        out_messages: *mut *const MqbMessage,
        out_len: *mut usize,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Non-zero if this consumer's commits must be applied in receive order
    /// (cumulative-offset transports such as Kafka).
    pub consumer_commit_requires_order: unsafe extern "C" fn(consumer: MqbConsumerHandle) -> u8,
    /// Tells the consumer whether the route terminates on an empty batch.
    pub consumer_set_exit_on_empty:
        unsafe extern "C" fn(consumer: MqbConsumerHandle, exit_on_empty: u8),
    /// Releases broker-side resources. The handle stays valid until freed.
    pub consumer_close:
        unsafe extern "C" fn(consumer: MqbConsumerHandle, err: *mut MqbBuffer) -> MqbStatus,
    /// Frees a consumer handle. Null is a no-op.
    pub consumer_free: unsafe extern "C" fn(consumer: MqbConsumerHandle),

    /// Applies one disposition per message of the batch, in receive order, and
    /// consumes the handle: it must not be used or freed afterwards.
    ///
    /// `dispositions` points to `len` `MQB_DISPOSITION_*` bytes; `len` always
    /// equals the batch's message count.
    pub batch_commit: unsafe extern "C" fn(
        batch: MqbBatchHandle,
        dispositions: *const u8,
        len: usize,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Discards an uncommitted batch without acknowledging anything. Null is a
    /// no-op. Never called after `batch_commit` on the same handle.
    pub batch_free: unsafe extern "C" fn(batch: MqbBatchHandle),

    /// Opens a publisher. `config_json` is as for `consumer_create`.
    pub publisher_create: unsafe extern "C" fn(
        factory: MqbFactoryHandle,
        route_name: MqbSlice,
        config_json: MqbSlice,
        out: *mut MqbPublisherHandle,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Publishes `len` messages, which are borrowed for the duration of the
    /// call. Success means every message was accepted; a failure status applies
    /// to the whole batch.
    pub publisher_send_batch: unsafe extern "C" fn(
        publisher: MqbPublisherHandle,
        messages: *const MqbMessage,
        len: usize,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Flushes anything the publisher has buffered.
    pub publisher_flush:
        unsafe extern "C" fn(publisher: MqbPublisherHandle, err: *mut MqbBuffer) -> MqbStatus,
    /// Releases broker-side resources. The handle stays valid until freed.
    pub publisher_close:
        unsafe extern "C" fn(publisher: MqbPublisherHandle, err: *mut MqbBuffer) -> MqbStatus,
    /// Frees a publisher handle. Null is a no-op.
    pub publisher_free: unsafe extern "C" fn(publisher: MqbPublisherHandle),

    /// Opens a middleware instance for one route and one `MQB_MIDDLEWARE_*`
    /// side. Only called when [`MQB_CAP_MIDDLEWARE`] is set.
    pub middleware_create: unsafe extern "C" fn(
        factory: MqbFactoryHandle,
        route_name: MqbSlice,
        config_json: MqbSlice,
        side: u8,
        out: *mut MqbMiddlewareHandle,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Passes a batch through the middleware.
    ///
    /// The input is borrowed for the call. On [`MQB_OK`] the plugin writes a
    /// result handle plus **two arrays of exactly `len` entries**: the messages,
    /// and one `MQB_MESSAGE_KEPT` / `MQB_MESSAGE_DROPPED` flag each. A dropped
    /// entry's message is unspecified. Both arrays stay valid until the result
    /// is freed.
    ///
    /// The output may point into the input: the host reads the result before it
    /// releases the input, so an unchanged message (or its id and metadata) can
    /// be passed back without copying.
    ///
    /// Keeping the arrays parallel to the input is what lets the host map the
    /// route's dispositions back onto the source messages and acknowledge the
    /// ones that were dropped.
    pub middleware_apply: unsafe extern "C" fn(
        middleware: MqbMiddlewareHandle,
        messages: *const MqbMessage,
        len: usize,
        out_result: *mut MqbFilterHandle,
        out_messages: *mut *const MqbMessage,
        out_kept: *mut *const u8,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Releases one middleware result. Null is a no-op.
    pub middleware_result_free: unsafe extern "C" fn(result: MqbFilterHandle),
    /// Frees a middleware handle. Null is a no-op.
    pub middleware_free: unsafe extern "C" fn(middleware: MqbMiddlewareHandle),

    // --- Added in ABI 1.1. Appended here rather than beside the other
    // publisher entries because the layout up to `middleware_free` is frozen:
    // moving a field would break every plugin compiled against 1.0.
    /// Non-zero if whole batches must reach this publisher in the order the
    /// source produced them, the publisher-side counterpart of
    /// [`MqbPluginVTable::consumer_commit_requires_order`].
    ///
    /// Only present when
    /// [`struct_size`](MqbPluginVTable::struct_size) reaches
    /// [`MQB_VTABLE_SIZE_V1_1`]; read it through
    /// [`MqbPluginVTable::publisher_ordering_hook`], never directly.
    pub publisher_requires_ordered_publish:
        unsafe extern "C" fn(publisher: MqbPublisherHandle) -> u8,
    /// Publishes a batch like
    /// [`publisher_send_batch`](MqbPluginVTable::publisher_send_batch), but says
    /// which messages failed.
    ///
    /// `out_outcomes` is **host-allocated** and exactly `len` bytes long. On a
    /// non-[`MQB_OK`] return the plugin writes one `MQB_OUTCOME_*` byte per
    /// message in the order they were passed, and `err` carries one batch-level
    /// message for the whole failure — no per-message text, so nothing is
    /// allocated per failure. On [`MQB_OK`] every message was accepted and the
    /// buffer is left untouched.
    ///
    /// Marking a subset is what stops the host re-sending the part that already
    /// landed. A batch where *nothing* landed needs no marks: the return status
    /// alone says so, which is what keeps [`MQB_ERR_CONNECTION`] meaning
    /// "reconnect this endpoint".
    ///
    /// Only present when
    /// [`struct_size`](MqbPluginVTable::struct_size) reaches
    /// [`MQB_VTABLE_SIZE_V1_1`]; read it through
    /// [`MqbPluginVTable::publisher_outcomes_hook`], never directly.
    /// [`MQB_ERR_UNSUPPORTED`] falls back to `publisher_send_batch`.
    pub publisher_send_batch_outcomes: MqbPublisherSendBatchOutcomes,
    /// Describes one of the plugin's configuration objects as a JSON Schema.
    ///
    /// `kind` is an `MQB_SCHEMA_*` selector. On [`MQB_OK`] the plugin either
    /// writes an owned [`MqbBuffer`] holding a UTF-8 JSON Schema document, or
    /// leaves it empty to say it describes nothing — an empty buffer is the
    /// answer for a kind the plugin does not implement, so a host may ask for
    /// any selector without checking first.
    ///
    /// The document is read once at load time and outlives the call, so the
    /// plugin may build it on demand rather than keeping it resident.
    ///
    /// Only present when
    /// [`struct_size`](MqbPluginVTable::struct_size) reaches
    /// [`MQB_VTABLE_SIZE_V1_1`]; read it through
    /// [`MqbPluginVTable::config_schema_hook`], never directly.
    pub factory_config_schema: MqbConfigSchema,

    // --- Added in ABI 1.2. Read through `request_reply_hooks` / `status_hooks`.
    /// Publishes like
    /// [`publisher_send_batch_outcomes`](MqbPluginVTable::publisher_send_batch_outcomes)
    /// and also returns the responses the sink produced.
    ///
    /// `out_result`, `out_responses` and `out_responses_len` start as null/0.
    /// Whatever the status, the plugin may write a result handle plus a compact
    /// array of responses in input order (only messages that produced one). The
    /// array lives until the host passes the handle to `responses_free`.
    /// [`MQB_ERR_UNSUPPORTED`] falls back to `publisher_send_batch_outcomes`.
    pub publisher_send_batch_responses: MqbPublisherSendBatchResponses,
    /// Releases a result written by `publisher_send_batch_responses`. Null is a no-op.
    pub responses_free: unsafe extern "C" fn(result: MqbResponsesHandle),
    /// Like [`batch_commit`](MqbPluginVTable::batch_commit), and accepts
    /// [`MQB_DISPOSITION_REPLY`]. `replies` is parallel to `dispositions`,
    /// borrowed for the call, and read only where the disposition is a reply.
    pub batch_commit_replies: unsafe extern "C" fn(
        batch: MqbBatchHandle,
        dispositions: *const u8,
        replies: *const MqbMessage,
        len: usize,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Writes the consumer's `EndpointStatus` as an owned UTF-8 JSON buffer;
    /// [`MQB_ERR_UNSUPPORTED`] reports a healthy default.
    pub consumer_status: unsafe extern "C" fn(
        consumer: MqbConsumerHandle,
        out: *mut MqbBuffer,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Writes the publisher's `EndpointStatus` as an owned UTF-8 JSON buffer;
    /// [`MQB_ERR_UNSUPPORTED`] reports a healthy default.
    pub publisher_status: unsafe extern "C" fn(
        publisher: MqbPublisherHandle,
        out: *mut MqbBuffer,
        err: *mut MqbBuffer,
    ) -> MqbStatus,
    /// Non-blocking [`consumer_receive_batch`](MqbPluginVTable::consumer_receive_batch);
    /// see [`MqbCompletion`]. Read through `async_hooks`.
    pub consumer_receive_batch_async: MqbReceiveBatchAsync,
    /// Non-blocking [`batch_commit_replies`](MqbPluginVTable::batch_commit_replies).
    /// The inputs are copied before it returns. The handle is consumed unless it
    /// returns [`MQB_ERR_UNSUPPORTED`], which falls back to the blocking commit.
    pub batch_commit_async: MqbBatchCommitAsync,
    /// Non-blocking [`publisher_send_batch_responses`](MqbPluginVTable::publisher_send_batch_responses).
    /// `messages` is copied before it returns.
    pub publisher_send_batch_async: MqbPublisherSendBatchAsync,
    /// Non-blocking [`publisher_flush`](MqbPluginVTable::publisher_flush).
    pub publisher_flush_async: MqbPublisherFlushAsync,
    /// Hands the plugin the host's services before `factory_create`. Called
    /// for every table a library exports, so it must tolerate repeats.
    /// Read through `host_init_hook`.
    pub plugin_init: MqbPluginInit,
    /// Writes the `MQB_DELIVERY_*` flags for an endpoint built from `config_json`.
    /// Read through `delivery_hook`.
    pub factory_delivery: MqbFactoryDelivery,
}

/// Signature of [`MqbPluginVTable::factory_delivery`].
pub type MqbFactoryDelivery = unsafe extern "C" fn(
    factory: MqbFactoryHandle,
    config_json: MqbSlice,
    out_flags: *mut u8,
    err: *mut MqbBuffer,
) -> MqbStatus;

/// Signature of [`MqbPluginVTable::plugin_init`].
pub type MqbPluginInit = unsafe extern "C" fn(host: *const MqbHostVTable);

/// Signature of [`MqbPluginVTable::consumer_receive_batch_async`].
pub type MqbReceiveBatchAsync = unsafe extern "C" fn(
    consumer: MqbConsumerHandle,
    max_messages: usize,
    out_batch: *mut MqbBatchHandle,
    out_messages: *mut *const MqbMessage,
    out_len: *mut usize,
    err: *mut MqbBuffer,
    completion: MqbCompletion,
) -> MqbStatus;

/// Signature of [`MqbPluginVTable::batch_commit_async`].
pub type MqbBatchCommitAsync = unsafe extern "C" fn(
    batch: MqbBatchHandle,
    dispositions: *const u8,
    replies: *const MqbMessage,
    len: usize,
    err: *mut MqbBuffer,
    completion: MqbCompletion,
) -> MqbStatus;

/// Signature of [`MqbPluginVTable::publisher_send_batch_async`].
pub type MqbPublisherSendBatchAsync = unsafe extern "C" fn(
    publisher: MqbPublisherHandle,
    messages: *const MqbMessage,
    len: usize,
    out_outcomes: *mut u8,
    out_result: *mut MqbResponsesHandle,
    out_responses: *mut *const MqbMessage,
    out_responses_len: *mut usize,
    err: *mut MqbBuffer,
    completion: MqbCompletion,
) -> MqbStatus;

/// Signature of [`MqbPluginVTable::publisher_flush_async`].
pub type MqbPublisherFlushAsync = unsafe extern "C" fn(
    publisher: MqbPublisherHandle,
    err: *mut MqbBuffer,
    completion: MqbCompletion,
) -> MqbStatus;

/// The 1.2 non-blocking entries.
#[derive(Copy, Clone)]
pub struct MqbAsyncHooks {
    pub receive_batch: MqbReceiveBatchAsync,
    pub batch_commit: MqbBatchCommitAsync,
    pub send_batch: MqbPublisherSendBatchAsync,
    pub flush: MqbPublisherFlushAsync,
}

/// Signature of [`MqbPluginVTable::publisher_send_batch_responses`].
pub type MqbPublisherSendBatchResponses = unsafe extern "C" fn(
    publisher: MqbPublisherHandle,
    messages: *const MqbMessage,
    len: usize,
    out_outcomes: *mut u8,
    out_result: *mut MqbResponsesHandle,
    out_responses: *mut *const MqbMessage,
    out_responses_len: *mut usize,
    err: *mut MqbBuffer,
) -> MqbStatus;

/// The 1.2 request/reply entries, present together or not at all.
#[derive(Copy, Clone)]
pub struct MqbRequestReplyHooks {
    pub send_batch_responses: MqbPublisherSendBatchResponses,
    pub responses_free: unsafe extern "C" fn(MqbResponsesHandle),
    pub batch_commit_replies: unsafe extern "C" fn(
        MqbBatchHandle,
        *const u8,
        *const MqbMessage,
        usize,
        *mut MqbBuffer,
    ) -> MqbStatus,
}

/// The 1.2 status entries.
#[derive(Copy, Clone)]
pub struct MqbStatusHooks {
    pub consumer_status:
        unsafe extern "C" fn(MqbConsumerHandle, *mut MqbBuffer, *mut MqbBuffer) -> MqbStatus,
    pub publisher_status:
        unsafe extern "C" fn(MqbPublisherHandle, *mut MqbBuffer, *mut MqbBuffer) -> MqbStatus,
}

/// Signature of [`MqbPluginVTable::factory_config_schema`], named so the field
/// and the accessor cannot drift apart.
pub type MqbConfigSchema = unsafe extern "C" fn(
    factory: MqbFactoryHandle,
    kind: u32,
    out: *mut MqbBuffer,
    err: *mut MqbBuffer,
) -> MqbStatus;

/// Signature of [`MqbPluginVTable::publisher_send_batch_outcomes`], named so the
/// field and the accessor cannot drift apart.
pub type MqbPublisherSendBatchOutcomes = unsafe extern "C" fn(
    publisher: MqbPublisherHandle,
    messages: *const MqbMessage,
    len: usize,
    out_outcomes: *mut u8,
    err: *mut MqbBuffer,
) -> MqbStatus;

impl MqbPluginVTable {
    /// The 1.1 publisher-ordering hook, or `None` when the plugin predates it.
    ///
    /// A 1.0 plugin's table really is only [`MQB_VTABLE_SIZE_V1_0`] bytes long,
    /// so the size is checked *before* the field is touched — reading it first
    /// reads past the end of the table. `None` means "unknown", which callers
    /// treat as unordered — exactly how 1.0 plugins already behave.
    pub fn publisher_ordering_hook(
        &self,
    ) -> Option<unsafe extern "C" fn(MqbPublisherHandle) -> u8> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_1 {
            return None;
        }
        Some(self.publisher_requires_ordered_publish)
    }

    /// The 1.1 per-message publish hook, or `None` when the plugin predates it.
    ///
    /// Gated for the same reason as
    /// [`publisher_ordering_hook`](Self::publisher_ordering_hook). `None` means
    /// the caller must fall back to
    /// [`publisher_send_batch`](Self::publisher_send_batch), whose failures are
    /// whole-batch.
    pub fn publisher_outcomes_hook(&self) -> Option<MqbPublisherSendBatchOutcomes> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_1 {
            return None;
        }
        Some(self.publisher_send_batch_outcomes)
    }

    /// The 1.1 configuration-schema hook, or `None` when the plugin predates it.
    ///
    /// Gated for the same reason as
    /// [`publisher_ordering_hook`](Self::publisher_ordering_hook). `None` and an
    /// empty answer mean the same thing to a caller: the plugin describes no
    /// configuration, so the host validates and maps nothing on its behalf.
    pub fn config_schema_hook(&self) -> Option<MqbConfigSchema> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_1 {
            return None;
        }
        Some(self.factory_config_schema)
    }

    /// The 1.2 request/reply entries, or `None` for an older plugin, which
    /// then publishes without responses and acks a reply disposition.
    pub fn request_reply_hooks(&self) -> Option<MqbRequestReplyHooks> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_2 {
            return None;
        }
        Some(MqbRequestReplyHooks {
            send_batch_responses: self.publisher_send_batch_responses,
            responses_free: self.responses_free,
            batch_commit_replies: self.batch_commit_replies,
        })
    }

    /// The 1.2 status entries, or `None` for an older plugin.
    pub fn status_hooks(&self) -> Option<MqbStatusHooks> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_2 {
            return None;
        }
        Some(MqbStatusHooks {
            consumer_status: self.consumer_status,
            publisher_status: self.publisher_status,
        })
    }

    /// The 1.2 host-services entry, or `None` for an older plugin, which then
    /// logs and records metrics only to its own globals.
    pub fn host_init_hook(&self) -> Option<MqbPluginInit> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_2 {
            return None;
        }
        Some(self.plugin_init)
    }

    /// The 1.2 delivery-flags entry, or `None` for an older plugin, whose
    /// guarantees then come from its config schema alone.
    pub fn delivery_hook(&self) -> Option<MqbFactoryDelivery> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_2 {
            return None;
        }
        Some(self.factory_delivery)
    }

    /// The 1.2 non-blocking entries, or `None` for an older plugin, whose calls
    /// the host then runs on its blocking pool.
    pub fn async_hooks(&self) -> Option<MqbAsyncHooks> {
        if self.struct_size < MQB_VTABLE_SIZE_V1_2 {
            return None;
        }
        Some(MqbAsyncHooks {
            receive_batch: self.consumer_receive_batch_async,
            batch_commit: self.batch_commit_async,
            send_batch: self.publisher_send_batch_async,
            flush: self.publisher_flush_async,
        })
    }
}

/// Size of the **1.0** field set: 7 header words (`struct_size`, the packed
/// `abi_major`/`abi_minor` pair, `capabilities`, and the two slices) plus 20
/// function pointers.
///
/// Frozen deliberately rather than derived from [`MqbPluginVTable`]: appending a
/// 1.1 field would otherwise grow the minimum and reject every 1.0 plugin, which
/// is exactly what the additive-minor promise rules out. A newer field must be
/// gated on the caller's [`MqbPluginVTable::struct_size`], not on this constant.
/// This word-count formula assumes a 64-bit target; a 32-bit port needs its own
/// frozen constant because `u64` alignment changes the table layout.
///
/// `the_1_0_table_size_is_frozen` checks it against the declared struct, so a
/// target whose padding differs fails the build's tests rather than silently
/// rejecting valid plugins.
pub const MQB_VTABLE_SIZE_V1_0: usize = 27 * core::mem::size_of::<usize>();

/// Size of the **1.1** field set: 1.0 plus
/// [`MqbPluginVTable::publisher_requires_ordered_publish`],
/// [`MqbPluginVTable::publisher_send_batch_outcomes`] and
/// [`MqbPluginVTable::factory_config_schema`].
///
/// This is a *feature gate*, not a minimum: [`check_compatibility`] still
/// admits anything at or above [`MQB_VTABLE_SIZE_V1_0`], and a table smaller
/// than this one simply has neither 1.1 hook. Both are gated on the one
/// constant because 1.1 ships as a unit; nothing in between was ever released.
pub const MQB_VTABLE_SIZE_V1_1: usize = MQB_VTABLE_SIZE_V1_0 + 3 * core::mem::size_of::<usize>();

/// Size of the **1.2** field set: 1.1 plus request/reply, status, the
/// non-blocking entries, host services and delivery flags. A feature gate like
/// [`MQB_VTABLE_SIZE_V1_1`].
pub const MQB_VTABLE_SIZE_V1_2: usize = MQB_VTABLE_SIZE_V1_1 + 11 * core::mem::size_of::<usize>();

/// Why a plugin was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiMismatch {
    /// The plugin was built against a different, incompatible major version.
    Major { plugin: u32, host: u32 },
    /// The table is smaller than the fields the host needs.
    TableTooSmall { plugin: usize, required: usize },
}

impl fmt::Display for AbiMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AbiMismatch::Major { plugin, host } => write!(
                f,
                "plugin ABI major version {plugin} is incompatible with host ABI major version \
                 {host}; rebuild the plugin against mq-bridge-plugin-abi {host}.x"
            ),
            AbiMismatch::TableTooSmall { plugin, required } => write!(
                f,
                "plugin function table is {plugin} bytes but this host requires at least \
                 {required}; the plugin was built against an older ABI revision"
            ),
        }
    }
}

/// Validates a table's version and size before any of its functions are called.
pub fn check_compatibility(table: &MqbPluginVTable) -> Result<(), AbiMismatch> {
    if table.abi_major != MQB_PLUGIN_ABI_MAJOR {
        return Err(AbiMismatch::Major {
            plugin: table.abi_major,
            host: MQB_PLUGIN_ABI_MAJOR,
        });
    }
    if table.struct_size < MQB_VTABLE_SIZE_V1_0 {
        return Err(AbiMismatch::TableTooSmall {
            plugin: table.struct_size,
            required: MQB_VTABLE_SIZE_V1_0,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, size_of};

    /// Layout of the data types is part of the contract; a change here is a
    /// major-version change, not a refactor.
    #[test]
    fn data_types_have_the_expected_c_layout() {
        assert_eq!(size_of::<MqbSlice>(), 2 * size_of::<usize>());
        assert_eq!(size_of::<MqbKeyValue>(), 2 * size_of::<MqbSlice>());
        assert_eq!(size_of::<MqbBuffer>(), 3 * size_of::<usize>());
        assert_eq!(align_of::<MqbSlice>(), align_of::<usize>());
        // id + payload slice + metadata pointer + metadata length, no padding
        // beyond the trailing pointer alignment.
        assert_eq!(
            size_of::<MqbMessage>(),
            16 + size_of::<MqbSlice>() + 2 * size_of::<usize>()
        );
        assert_eq!(size_of::<MqbFactoryHandle>(), size_of::<*mut c_void>());
        assert_eq!(size_of::<MqbConsumerHandle>(), size_of::<*mut c_void>());
        assert_eq!(size_of::<MqbPublisherHandle>(), size_of::<*mut c_void>());
        assert_eq!(size_of::<MqbBatchHandle>(), size_of::<*mut c_void>());
    }

    #[test]
    fn vtable_is_pointer_sized_fields_only() {
        // 7 header words on a 64-bit target — struct_size, the packed
        // abi_major/abi_minor pair, capabilities, and two two-word slices —
        // plus 20 function pointers, all pointer-aligned.
        assert_eq!(align_of::<MqbPluginVTable>(), align_of::<usize>());
        assert_eq!(MQB_VTABLE_SIZE_V1_0 % size_of::<usize>(), 0);
    }

    /// The 1.0 minimum must not move when a field is appended, or every plugin
    /// built against 1.0 would be rejected as "too small". Appending a field
    /// bumps [`MQB_PLUGIN_ABI_MINOR`]; this assertion then no longer applies and
    /// is what forces that to be a deliberate decision.
    #[test]
    fn the_1_0_table_size_is_frozen() {
        assert_eq!(MQB_VTABLE_SIZE_V1_0, 27 * size_of::<usize>());
        if MQB_PLUGIN_ABI_MINOR == 0 {
            assert_eq!(
                size_of::<MqbPluginVTable>(),
                MQB_VTABLE_SIZE_V1_0,
                "the declared table is still 1.0; update MQB_PLUGIN_ABI_MINOR, not this constant"
            );
        } else {
            assert!(size_of::<MqbPluginVTable>() > MQB_VTABLE_SIZE_V1_0);
        }
    }

    /// The 1.1 size is frozen for the same reason 1.0 is: it gates a field
    /// read, so a stale value would read past a 1.0 plugin's table.
    #[test]
    fn the_1_1_table_size_is_frozen() {
        assert_eq!(
            MQB_VTABLE_SIZE_V1_1,
            MQB_VTABLE_SIZE_V1_0 + 3 * size_of::<usize>()
        );
    }

    #[test]
    fn the_1_2_table_size_matches_the_declared_table() {
        assert_eq!(
            MQB_VTABLE_SIZE_V1_2,
            MQB_VTABLE_SIZE_V1_1 + 11 * size_of::<usize>()
        );
        assert_eq!(size_of::<MqbPluginVTable>(), MQB_VTABLE_SIZE_V1_2);
    }

    #[test]
    fn a_1_1_table_loads_but_offers_no_1_2_hooks() {
        let mut table = stub_table();
        table.struct_size = MQB_VTABLE_SIZE_V1_1;
        assert!(check_compatibility(&table).is_ok());
        assert!(table.config_schema_hook().is_some());
        assert!(table.request_reply_hooks().is_none());
        assert!(table.status_hooks().is_none());
        assert!(table.async_hooks().is_none());
        assert!(table.host_init_hook().is_none());
        assert!(table.delivery_hook().is_none());

        table.struct_size = MQB_VTABLE_SIZE_V1_2;
        assert!(table.request_reply_hooks().is_some());
        assert!(table.status_hooks().is_some());
        assert!(table.async_hooks().is_some());
        assert!(table.host_init_hook().is_some());
        assert!(table.delivery_hook().is_some());
    }

    #[test]
    fn the_1_2_host_table_size_is_frozen() {
        assert_eq!(size_of::<MqbHostVTable>(), MQB_HOST_VTABLE_SIZE_V1_2);
    }

    #[test]
    fn disposition_codes_are_stable() {
        assert_eq!(
            [
                MQB_DISPOSITION_ACK,
                MQB_DISPOSITION_NACK,
                MQB_DISPOSITION_REPLY
            ],
            [0, 1, 2]
        );
    }

    /// The whole point of an additive minor: a 1.0 plugin still loads, and the
    /// host discovers that it can ask it none of the 1.1 questions.
    #[test]
    fn a_1_0_table_loads_but_offers_no_1_1_hooks() {
        let mut table = stub_table();
        table.struct_size = MQB_VTABLE_SIZE_V1_0;
        assert!(check_compatibility(&table).is_ok());
        assert!(table.publisher_ordering_hook().is_none());
        assert!(table.publisher_outcomes_hook().is_none());
        assert!(table.config_schema_hook().is_none());

        table.struct_size = MQB_VTABLE_SIZE_V1_1;
        assert!(table.publisher_ordering_hook().is_some());
        assert!(table.publisher_outcomes_hook().is_some());
        assert!(table.config_schema_hook().is_some());
    }

    /// The three outcome codes share the byte the dispositions use, so they must
    /// stay distinct and stay put: a plugin compiled against one numbering and a
    /// host reading another would mis-class every failure.
    #[test]
    fn outcome_codes_are_stable() {
        assert_eq!(
            [MQB_OUTCOME_OK, MQB_OUTCOME_RETRYABLE, MQB_OUTCOME_PERMANENT],
            [0, 1, 2]
        );
    }

    fn table(abi_major: u32, struct_size: usize) -> MqbPluginVTable {
        let mut table = stub_table();
        table.abi_major = abi_major;
        table.struct_size = struct_size;
        table
    }

    #[test]
    fn compatible_table_is_accepted() {
        assert!(check_compatibility(&table(MQB_PLUGIN_ABI_MAJOR, MQB_VTABLE_SIZE_V1_0)).is_ok());
        // A newer plugin with appended fields is still accepted.
        assert!(
            check_compatibility(&table(MQB_PLUGIN_ABI_MAJOR, MQB_VTABLE_SIZE_V1_0 + 64)).is_ok()
        );
    }

    #[test]
    fn incompatible_tables_are_rejected_with_actionable_text() {
        let err = check_compatibility(&table(MQB_PLUGIN_ABI_MAJOR + 1, MQB_VTABLE_SIZE_V1_0))
            .unwrap_err();
        assert!(matches!(err, AbiMismatch::Major { .. }));
        assert!(format!("{err}").contains("rebuild the plugin"));

        let err = check_compatibility(&table(MQB_PLUGIN_ABI_MAJOR, MQB_VTABLE_SIZE_V1_0 - 8))
            .unwrap_err();
        assert!(matches!(err, AbiMismatch::TableTooSmall { .. }));
        assert!(format!("{err}").contains("older ABI revision"));
    }

    #[test]
    fn empty_slice_is_readable() {
        let slice = MqbSlice::EMPTY;
        assert!(unsafe { slice.as_bytes() }.is_empty());
        assert!(MqbBuffer::EMPTY.is_empty());
    }

    #[test]
    fn slices_borrow_without_copying() {
        let bytes = vec![1u8, 2, 3];
        let slice = MqbSlice::from_bytes(&bytes);
        assert_eq!(unsafe { slice.as_bytes() }, &bytes[..]);
        assert_eq!(unsafe { MqbSlice::from_str("ab").as_bytes() }, b"ab");
    }

    // A table of no-op functions, enough to exercise the version checks.
    fn stub_table() -> MqbPluginVTable {
        unsafe extern "C" fn factory_create(
            _out: *mut MqbFactoryHandle,
            _err: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn factory_free(_: MqbFactoryHandle) {}
        unsafe extern "C" fn buffer_free(_: MqbBuffer) {}
        unsafe extern "C" fn consumer_create(
            _: MqbFactoryHandle,
            _: MqbSlice,
            _: MqbSlice,
            _: *mut MqbConsumerHandle,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn consumer_receive_batch(
            _: MqbConsumerHandle,
            _: usize,
            _: *mut MqbBatchHandle,
            _: *mut *const MqbMessage,
            _: *mut usize,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn commit_requires_order(_: MqbConsumerHandle) -> u8 {
            1
        }
        unsafe extern "C" fn set_exit_on_empty(_: MqbConsumerHandle, _: u8) {}
        unsafe extern "C" fn consumer_close(_: MqbConsumerHandle, _: *mut MqbBuffer) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn consumer_free(_: MqbConsumerHandle) {}
        unsafe extern "C" fn batch_commit(
            _: MqbBatchHandle,
            _: *const u8,
            _: usize,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn batch_free(_: MqbBatchHandle) {}
        unsafe extern "C" fn publisher_create(
            _: MqbFactoryHandle,
            _: MqbSlice,
            _: MqbSlice,
            _: *mut MqbPublisherHandle,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn publisher_send_batch(
            _: MqbPublisherHandle,
            _: *const MqbMessage,
            _: usize,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn publisher_flush(
            _: MqbPublisherHandle,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn publisher_close(
            _: MqbPublisherHandle,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn publisher_free(_: MqbPublisherHandle) {}
        unsafe extern "C" fn middleware_create(
            _: MqbFactoryHandle,
            _: MqbSlice,
            _: MqbSlice,
            _: u8,
            _: *mut MqbMiddlewareHandle,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn middleware_apply(
            _: MqbMiddlewareHandle,
            _: *const MqbMessage,
            _: usize,
            _: *mut MqbFilterHandle,
            _: *mut *const MqbMessage,
            _: *mut *const u8,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn middleware_result_free(_: MqbFilterHandle) {}
        unsafe extern "C" fn middleware_free(_: MqbMiddlewareHandle) {}
        unsafe extern "C" fn requires_ordered_publish(_: MqbPublisherHandle) -> u8 {
            0
        }
        unsafe extern "C" fn send_batch_outcomes(
            _: MqbPublisherHandle,
            _: *const MqbMessage,
            _: usize,
            _: *mut u8,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn config_schema(
            _: MqbFactoryHandle,
            _: u32,
            _: *mut MqbBuffer,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn send_batch_responses(
            _: MqbPublisherHandle,
            _: *const MqbMessage,
            _: usize,
            _: *mut u8,
            _: *mut MqbResponsesHandle,
            _: *mut *const MqbMessage,
            _: *mut usize,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn responses_free(_: MqbResponsesHandle) {}
        unsafe extern "C" fn batch_commit_replies(
            _: MqbBatchHandle,
            _: *const u8,
            _: *const MqbMessage,
            _: usize,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn consumer_status(
            _: MqbConsumerHandle,
            _: *mut MqbBuffer,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn publisher_status(
            _: MqbPublisherHandle,
            _: *mut MqbBuffer,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }
        unsafe extern "C" fn receive_batch_async(
            _: MqbConsumerHandle,
            _: usize,
            _: *mut MqbBatchHandle,
            _: *mut *const MqbMessage,
            _: *mut usize,
            _: *mut MqbBuffer,
            _: MqbCompletion,
        ) -> MqbStatus {
            MQB_ERR_UNSUPPORTED
        }
        unsafe extern "C" fn batch_commit_async(
            _: MqbBatchHandle,
            _: *const u8,
            _: *const MqbMessage,
            _: usize,
            _: *mut MqbBuffer,
            _: MqbCompletion,
        ) -> MqbStatus {
            MQB_ERR_UNSUPPORTED
        }
        unsafe extern "C" fn send_batch_async(
            _: MqbPublisherHandle,
            _: *const MqbMessage,
            _: usize,
            _: *mut u8,
            _: *mut MqbResponsesHandle,
            _: *mut *const MqbMessage,
            _: *mut usize,
            _: *mut MqbBuffer,
            _: MqbCompletion,
        ) -> MqbStatus {
            MQB_ERR_UNSUPPORTED
        }
        unsafe extern "C" fn flush_async(
            _: MqbPublisherHandle,
            _: *mut MqbBuffer,
            _: MqbCompletion,
        ) -> MqbStatus {
            MQB_ERR_UNSUPPORTED
        }
        unsafe extern "C" fn plugin_init(_: *const MqbHostVTable) {}
        unsafe extern "C" fn factory_delivery(
            _: MqbFactoryHandle,
            _: MqbSlice,
            _: *mut u8,
            _: *mut MqbBuffer,
        ) -> MqbStatus {
            MQB_OK
        }

        MqbPluginVTable {
            struct_size: MQB_VTABLE_SIZE_V1_0,
            abi_major: MQB_PLUGIN_ABI_MAJOR,
            abi_minor: MQB_PLUGIN_ABI_MINOR,
            capabilities: MQB_CAP_CONSUMER | MQB_CAP_PUBLISHER,
            name: MqbSlice::from_str("stub"),
            version: MqbSlice::from_str("0.0.0"),
            factory_create,
            factory_free,
            buffer_free,
            consumer_create,
            consumer_receive_batch,
            consumer_commit_requires_order: commit_requires_order,
            consumer_set_exit_on_empty: set_exit_on_empty,
            consumer_close,
            consumer_free,
            batch_commit,
            batch_free,
            publisher_create,
            publisher_send_batch,
            publisher_flush,
            publisher_close,
            publisher_free,
            middleware_create,
            middleware_apply,
            middleware_result_free,
            middleware_free,
            publisher_requires_ordered_publish: requires_ordered_publish,
            publisher_send_batch_outcomes: send_batch_outcomes,
            factory_config_schema: config_schema,
            publisher_send_batch_responses: send_batch_responses,
            responses_free,
            batch_commit_replies,
            consumer_status,
            publisher_status,
            consumer_receive_batch_async: receive_batch_async,
            batch_commit_async,
            publisher_send_batch_async: send_batch_async,
            publisher_flush_async: flush_async,
            plugin_init,
            factory_delivery,
        }
    }
}
