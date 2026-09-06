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
pub const MQB_PLUGIN_ABI_MINOR: u32 = 0;

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

/// The plugin can create consumers (input endpoints).
pub const MQB_CAP_CONSUMER: u64 = 1 << 0;
/// The plugin can create publishers (output endpoints).
pub const MQB_CAP_PUBLISHER: u64 = 1 << 1;
/// The plugin provides a middleware under the same name.
pub const MQB_CAP_MIDDLEWARE: u64 = 1 << 2;

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
    MqbFilterHandle
);

/// The function table a plugin exports through [`MQB_PLUGIN_ENTRY_SYMBOL`].
///
/// Every fallible function takes an `err` out-parameter. On a non-[`MQB_OK`]
/// return the plugin may write an owned [`MqbBuffer`] holding UTF-8 error text;
/// on [`MQB_OK`] it must leave the buffer empty. All calls are blocking: the
/// host invokes them off its async executor, and the plugin drives its own
/// runtime internally.
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
        }
    }
}
