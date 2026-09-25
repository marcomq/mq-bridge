/*
 * mq_bridge_plugin.h - the C ABI of an mq-bridge endpoint/middleware plugin.
 *
 * A plugin is a shared library exporting `mq_bridge_plugin_v1`, which returns a
 * static MqbPluginVTable. The host checks `abi_major` and `struct_size`, then
 * calls through the table. See docs/PLUGINS.md and examples/c-plugin/;
 * mq_bridge_plugin_helpers.h fills in the entries a plugin does not need.
 *
 * Rules a C or C++ plugin must follow:
 *  - Every function pointer in the table must be non-null, including ones the
 *    plugin does not support: point those at a stub returning
 *    MQB_ERR_UNSUPPORTED (or doing nothing, for the *_free entries).
 *  - Host -> plugin data (arguments) is borrowed for the call only; copy what
 *    you keep. Plugin -> host data stays valid until its handle is freed.
 *    middleware_apply's output may point into its input.
 *  - Error text goes into the `err` MqbBuffer, allocated by the plugin and
 *    released through `buffer_free`; leave it empty on MQB_OK.
 *  - Handles may be used and freed from any thread, and may be called
 *    concurrently.
 *  - Never let a C++ exception escape an ABI function: catch it and return
 *    MQB_ERR_PERMANENT.
 *  - The layout is defined for 64-bit targets.
 */

#ifndef MQ_BRIDGE_PLUGIN_H
#define MQ_BRIDGE_PLUGIN_H

/* Generated from src/support/plugin_abi.rs by cbindgen. Do not edit.
 * Regenerate: MQB_BLESS=1 cargo test -p mq-bridge-plugin-fixture --test plugin_c_header */

#include <stddef.h>
#include <stdint.h>

// Incompatible-change counter. A host refuses a plugin with a different major.
#define MQB_PLUGIN_ABI_MAJOR 1

// Additive-change counter. A host accepts any minor, old or new.
//
// * **1.1** appended `MqbPluginVTable::publisher_requires_ordered_publish`,
//   `MqbPluginVTable::publisher_send_batch_outcomes` and
//   `MqbPluginVTable::factory_config_schema`.
// * **1.2** appended request/reply (`MqbPluginVTable::publisher_send_batch_responses`,
//   `MqbPluginVTable::responses_free`, `MqbPluginVTable::batch_commit_replies`)
//   status (`MqbPluginVTable::consumer_status`, `MqbPluginVTable::publisher_status`)
//   non-blocking twins of the hot-path calls (`MqbCompletion`) and host
//   services for logs and metrics (`MqbHostVTable`).
#define MQB_PLUGIN_ABI_MINOR 2

// Acknowledge the message: it was processed successfully.
#define MQB_DISPOSITION_ACK 0

// Negatively acknowledge the message so the broker can redeliver it.
#define MQB_DISPOSITION_NACK 1

// Acknowledge the message and send the parallel reply (ABI 1.2,
// `MqbPluginVTable::batch_commit_replies` only).
#define MQB_DISPOSITION_REPLY 2

// The message was published. Per-message counterpart of `MQB_OK`, written by
// `MqbPluginVTable::publisher_send_batch_outcomes`.
#define MQB_OUTCOME_OK 0

// This message failed transiently; the host may send it again.
#define MQB_OUTCOME_RETRYABLE 1

// This message failed permanently. Sending it again cannot help.
#define MQB_OUTCOME_PERMANENT 2

// The plugin can create consumers (input endpoints).
#define MQB_CAP_CONSUMER (1 << 0)

// The plugin can create publishers (output endpoints).
#define MQB_CAP_PUBLISHER (1 << 1)

// The plugin provides a middleware under the same name.
#define MQB_CAP_MIDDLEWARE (1 << 2)

// Asks `MqbPluginVTable::factory_config_schema` for the endpoint's
// configuration object.
#define MQB_SCHEMA_ENDPOINT 0

// Asks `MqbPluginVTable::factory_config_schema` for the middleware's
// configuration object.
#define MQB_SCHEMA_MIDDLEWARE 1

// Middleware sitting on an input endpoint: it sees each batch after the source
// produced it.
#define MQB_MIDDLEWARE_RECEIVE 0

// Middleware sitting on an output endpoint: it sees each batch before the sink
// does.
#define MQB_MIDDLEWARE_SEND 1

// The middleware dropped this message: the corresponding entry of the message
// array is unspecified and must not be read.
#define MQB_MESSAGE_DROPPED 0

// The middleware kept this message, possibly rewritten.
#define MQB_MESSAGE_KEPT 1

// `MqbPluginVTable::factory_delivery` flag: a publisher built from the config absorbs replays.
#define MQB_DELIVERY_IDEMPOTENT_SINK (1 << 0)

// `MqbPluginVTable::factory_delivery` flag: a consumer built from the config acknowledges.
#define MQB_DELIVERY_ACKNOWLEDGES (1 << 1)

// `MQB_LOG_*`: severity of an event passed to `MqbHostVTable::log` (ABI 1.2).
#define MQB_LOG_ERROR 1

#define MQB_LOG_WARN 2

#define MQB_LOG_INFO 3

#define MQB_LOG_DEBUG 4

#define MQB_LOG_TRACE 5

// `MQB_METRIC_*`: what `MqbHostVTable::metric` does with its value (ABI 1.2).
#define MQB_METRIC_COUNTER 0

#define MQB_METRIC_COUNTER_ABSOLUTE 1

#define MQB_METRIC_GAUGE_SET 2

// Adds to a gauge; a negative value decrements it.
#define MQB_METRIC_GAUGE_ADD 3

#define MQB_METRIC_HISTOGRAM 4

// Result of an ABI call. `0` is success; every other value is a failure whose
// class the host maps onto its own error types.
typedef int32_t MqbStatus;

// A borrowed, non-owning view of bytes. Lifetime is defined by whichever side
// produced it; see the crate-level ownership rules.
typedef struct MqbSlice {
  const uint8_t *ptr;
  size_t len;
} MqbSlice;

// A buffer allocated by the plugin and returned to the host, used for error
// text. The host must return it to `MqbPluginVTable::buffer_free` exactly
// once; a buffer with a null `ptr` or zero `len` carries no message and needs
// no release.
typedef struct MqbBuffer {
  uint8_t *ptr;
  size_t len;
  size_t cap;
} MqbBuffer;

// One metadata entry of a message. Both halves are UTF-8.
typedef struct MqbKeyValue {
  struct MqbSlice key;
  struct MqbSlice value;
} MqbKeyValue;

// A message in transit across the ABI.
//
// `message_id` is a big-endian 128-bit id (mq-bridge uses UUIDv7). All-zero
// means "no id"; the receiving side then generates one.
typedef struct MqbMessage {
  uint8_t message_id[16];
  struct MqbSlice payload;
  // Pointer to `metadata_len` entries; may be null when `metadata_len` is 0.
  const struct MqbKeyValue *metadata;
  size_t metadata_len;
} MqbMessage;

// Opaque handle to a plugin's endpoint factory.
typedef void *MqbFactoryHandle;

// Opaque handle to a plugin consumer (input endpoint).
typedef void *MqbConsumerHandle;

// Opaque handle to a plugin publisher (output endpoint).
typedef void *MqbPublisherHandle;

// Opaque handle to one received batch, holding the broker-side state needed to
// acknowledge it later.
typedef void *MqbBatchHandle;

// Opaque handle to a middleware instance, bound to one route and side.
typedef void *MqbMiddlewareHandle;

// Opaque handle to the result of one middleware call, owning the arrays it
// handed back.
typedef void *MqbFilterHandle;

// Plugin-owned publish responses, released with
// `MqbPluginVTable::responses_free` (ABI 1.2).
typedef void *MqbResponsesHandle;

// Where an asynchronous 1.2 call reports that it finished.
//
// If the starting call returns `MQB_OK`, the plugin invokes `callback(ctx,
// status)` exactly once, from any thread, possibly before the starting call
// returns; any other return means it never does. Out-parameters stay writable
// until the callback, which must not block.
//
// A plugin that returns `MQB_ERR_UNSUPPORTED` from a non-blocking entry gets
// its blocking twin instead, from then on for that endpoint.
typedef struct MqbCompletion {
  void (*callback)(void *ctx, MqbStatus status);
  void *ctx;
} MqbCompletion;

// Services the host offers a plugin, handed over once through
// `MqbPluginVTable::plugin_init` (ABI 1.2).
//
// Lives as long as the process. Every function may be called from any thread,
// never blocks, and borrows its arguments only for the call.
typedef struct MqbHostVTable {
  // `size_of::<MqbHostVTable>()` as compiled into the host; fields may be appended.
  size_t struct_size;
  // Non-zero if the host records events at `MQB_LOG_*` `level`.
  uint8_t (*log_enabled)(uint8_t level);
  // Records one event; `target` is the plugin's module path, `message` the
  // rendered text including its fields.
  void (*log)(uint8_t level, struct MqbSlice target, struct MqbSlice message);
  // Records one metric sample; `kind` is an `MQB_METRIC_*` code.
  void (*metric)(uint8_t kind,
                 struct MqbSlice name,
                 const struct MqbKeyValue *labels,
                 size_t labels_len,
                 double value);
} MqbHostVTable;

// Signature of `MqbPluginVTable::publisher_send_batch_outcomes`, named so the
// field and the accessor cannot drift apart.
typedef MqbStatus (*MqbPublisherSendBatchOutcomes)(MqbPublisherHandle publisher,
                                                   const struct MqbMessage *messages,
                                                   size_t len,
                                                   uint8_t *out_outcomes,
                                                   struct MqbBuffer *err);

// Signature of `MqbPluginVTable::factory_config_schema`, named so the field
// and the accessor cannot drift apart.
typedef MqbStatus (*MqbConfigSchema)(MqbFactoryHandle factory,
                                     uint32_t kind,
                                     struct MqbBuffer *out,
                                     struct MqbBuffer *err);

// Signature of `MqbPluginVTable::publisher_send_batch_responses`.
typedef MqbStatus (*MqbPublisherSendBatchResponses)(MqbPublisherHandle publisher,
                                                    const struct MqbMessage *messages,
                                                    size_t len,
                                                    uint8_t *out_outcomes,
                                                    MqbResponsesHandle *out_result,
                                                    const struct MqbMessage **out_responses,
                                                    size_t *out_responses_len,
                                                    struct MqbBuffer *err);

// Signature of `MqbPluginVTable::consumer_receive_batch_async`.
typedef MqbStatus (*MqbReceiveBatchAsync)(MqbConsumerHandle consumer,
                                          size_t max_messages,
                                          MqbBatchHandle *out_batch,
                                          const struct MqbMessage **out_messages,
                                          size_t *out_len,
                                          struct MqbBuffer *err,
                                          struct MqbCompletion completion);

// Signature of `MqbPluginVTable::batch_commit_async`.
typedef MqbStatus (*MqbBatchCommitAsync)(MqbBatchHandle batch,
                                         const uint8_t *dispositions,
                                         const struct MqbMessage *replies,
                                         size_t len,
                                         struct MqbBuffer *err,
                                         struct MqbCompletion completion);

// Signature of `MqbPluginVTable::publisher_send_batch_async`.
typedef MqbStatus (*MqbPublisherSendBatchAsync)(MqbPublisherHandle publisher,
                                                const struct MqbMessage *messages,
                                                size_t len,
                                                uint8_t *out_outcomes,
                                                MqbResponsesHandle *out_result,
                                                const struct MqbMessage **out_responses,
                                                size_t *out_responses_len,
                                                struct MqbBuffer *err,
                                                struct MqbCompletion completion);

// Signature of `MqbPluginVTable::publisher_flush_async`.
typedef MqbStatus (*MqbPublisherFlushAsync)(MqbPublisherHandle publisher,
                                            struct MqbBuffer *err,
                                            struct MqbCompletion completion);

// Signature of `MqbPluginVTable::plugin_init`.
typedef void (*MqbPluginInit)(const struct MqbHostVTable *host);

// Signature of `MqbPluginVTable::factory_delivery`.
typedef MqbStatus (*MqbFactoryDelivery)(MqbFactoryHandle factory,
                                        struct MqbSlice config_json,
                                        uint8_t *out_flags,
                                        struct MqbBuffer *err);

// The function table a plugin exports through `MQB_PLUGIN_ENTRY_SYMBOL`.
//
// Every fallible function takes an `err` out-parameter. On a non-`MQB_OK`
// return the plugin may write an owned `MqbBuffer` holding UTF-8 error text;
// on `MQB_OK` it must leave the buffer empty. All calls are blocking: the
// host invokes them off its async executor, and the plugin drives its own
// runtime internally. The `*_async` entries (1.2) are the exception.
//
// Fields may only be appended in later minor versions. Readers must check
// `struct_size` before touching a field added after 1.0.
typedef struct MqbPluginVTable {
  // `size_of::<MqbPluginVTable>()` as compiled into the plugin.
  size_t struct_size;
  // Must equal `MQB_PLUGIN_ABI_MAJOR` for the host to accept the plugin.
  uint32_t abi_major;
  // Highest minor version the plugin was built against.
  uint32_t abi_minor;
  // Bit set of `MQB_CAP_*` flags.
  uint64_t capabilities;
  // Endpoint name to register under, e.g. `pulsar`. UTF-8, `'static`.
  struct MqbSlice name;
  // Human-readable plugin version, e.g. its crate version. UTF-8, `'static`.
  struct MqbSlice version;
  // Creates the factory. Called once per loaded library.
  MqbStatus (*factory_create)(MqbFactoryHandle *out, struct MqbBuffer *err);
  // Releases a factory handle. Null is a no-op.
  void (*factory_free)(MqbFactoryHandle factory);
  // Releases a buffer previously handed to the host. Empty is a no-op.
  void (*buffer_free)(struct MqbBuffer buffer);
  // Opens a consumer. `config_json` is the endpoint's configuration object
  // encoded as UTF-8 JSON.
  MqbStatus (*consumer_create)(MqbFactoryHandle factory,
                               struct MqbSlice route_name,
                               struct MqbSlice config_json,
                               MqbConsumerHandle *out,
                               struct MqbBuffer *err);
  // Receives up to `max_messages` messages.
  //
  // On `MQB_OK` the plugin writes a batch handle plus a pointer to
  // `*out_len` messages. Both stay valid until the batch is committed or
  // freed. `*out_len == 0` means "idle, nothing available" and the host
  // still receives (and must release) a batch handle.
  MqbStatus (*consumer_receive_batch)(MqbConsumerHandle consumer,
                                      size_t max_messages,
                                      MqbBatchHandle *out_batch,
                                      const struct MqbMessage **out_messages,
                                      size_t *out_len,
                                      struct MqbBuffer *err);
  // Non-zero if this consumer's commits must be applied in receive order
  // (cumulative-offset transports such as Kafka).
  uint8_t (*consumer_commit_requires_order)(MqbConsumerHandle consumer);
  // Tells the consumer whether the route terminates on an empty batch.
  void (*consumer_set_exit_on_empty)(MqbConsumerHandle consumer, uint8_t exit_on_empty);
  // Releases broker-side resources. The handle stays valid until freed.
  MqbStatus (*consumer_close)(MqbConsumerHandle consumer, struct MqbBuffer *err);
  // Frees a consumer handle. Null is a no-op.
  void (*consumer_free)(MqbConsumerHandle consumer);
  // Applies one disposition per message of the batch, in receive order, and
  // consumes the handle: it must not be used or freed afterwards.
  //
  // `dispositions` points to `len` `MQB_DISPOSITION_*` bytes; `len` always
  // equals the batch's message count.
  MqbStatus (*batch_commit)(MqbBatchHandle batch,
                            const uint8_t *dispositions,
                            size_t len,
                            struct MqbBuffer *err);
  // Discards an uncommitted batch without acknowledging anything. Null is a
  // no-op. Never called after `batch_commit` on the same handle.
  void (*batch_free)(MqbBatchHandle batch);
  // Opens a publisher. `config_json` is as for `consumer_create`.
  MqbStatus (*publisher_create)(MqbFactoryHandle factory,
                                struct MqbSlice route_name,
                                struct MqbSlice config_json,
                                MqbPublisherHandle *out,
                                struct MqbBuffer *err);
  // Publishes `len` messages, which are borrowed for the duration of the
  // call. Success means every message was accepted; a failure status applies
  // to the whole batch.
  MqbStatus (*publisher_send_batch)(MqbPublisherHandle publisher,
                                    const struct MqbMessage *messages,
                                    size_t len,
                                    struct MqbBuffer *err);
  // Flushes anything the publisher has buffered.
  MqbStatus (*publisher_flush)(MqbPublisherHandle publisher, struct MqbBuffer *err);
  // Releases broker-side resources. The handle stays valid until freed.
  MqbStatus (*publisher_close)(MqbPublisherHandle publisher, struct MqbBuffer *err);
  // Frees a publisher handle. Null is a no-op.
  void (*publisher_free)(MqbPublisherHandle publisher);
  // Opens a middleware instance for one route and one `MQB_MIDDLEWARE_*`
  // side. Only called when `MQB_CAP_MIDDLEWARE` is set.
  MqbStatus (*middleware_create)(MqbFactoryHandle factory,
                                 struct MqbSlice route_name,
                                 struct MqbSlice config_json,
                                 uint8_t side,
                                 MqbMiddlewareHandle *out,
                                 struct MqbBuffer *err);
  // Passes a batch through the middleware.
  //
  // The input is borrowed for the call. On `MQB_OK` the plugin writes a
  // result handle plus **two arrays of exactly `len` entries**: the messages,
  // and one `MQB_MESSAGE_KEPT` / `MQB_MESSAGE_DROPPED` flag each. A dropped
  // entry's message is unspecified. Both arrays stay valid until the result
  // is freed.
  //
  // The output may point into the input: the host reads the result before it
  // releases the input, so an unchanged message (or its id and metadata) can
  // be passed back without copying.
  //
  // Keeping the arrays parallel to the input is what lets the host map the
  // route's dispositions back onto the source messages and acknowledge the
  // ones that were dropped.
  MqbStatus (*middleware_apply)(MqbMiddlewareHandle middleware,
                                const struct MqbMessage *messages,
                                size_t len,
                                MqbFilterHandle *out_result,
                                const struct MqbMessage **out_messages,
                                const uint8_t **out_kept,
                                struct MqbBuffer *err);
  // Releases one middleware result. Null is a no-op.
  void (*middleware_result_free)(MqbFilterHandle result);
  // Frees a middleware handle. Null is a no-op.
  void (*middleware_free)(MqbMiddlewareHandle middleware);
  // Non-zero if whole batches must reach this publisher in the order the
  // source produced them, the publisher-side counterpart of
  // `MqbPluginVTable::consumer_commit_requires_order`.
  //
  // Only present when
  // `struct_size` reaches
  // `MQB_VTABLE_SIZE_V1_1`; read it through
  // `MqbPluginVTable::publisher_ordering_hook`, never directly.
  uint8_t (*publisher_requires_ordered_publish)(MqbPublisherHandle publisher);
  // Publishes a batch like
  // `publisher_send_batch`, but says
  // which messages failed.
  //
  // `out_outcomes` is **host-allocated** and exactly `len` bytes long. On a
  // non-`MQB_OK` return the plugin writes one `MQB_OUTCOME_*` byte per
  // message in the order they were passed, and `err` carries one batch-level
  // message for the whole failure — no per-message text, so nothing is
  // allocated per failure. On `MQB_OK` every message was accepted and the
  // buffer is left untouched.
  //
  // Marking a subset is what stops the host re-sending the part that already
  // landed. A batch where *nothing* landed needs no marks: the return status
  // alone says so, which is what keeps `MQB_ERR_CONNECTION` meaning
  // "reconnect this endpoint".
  //
  // Only present when
  // `struct_size` reaches
  // `MQB_VTABLE_SIZE_V1_1`; read it through
  // `MqbPluginVTable::publisher_outcomes_hook`, never directly.
  // `MQB_ERR_UNSUPPORTED` falls back to `publisher_send_batch`.
  MqbPublisherSendBatchOutcomes publisher_send_batch_outcomes;
  // Describes one of the plugin's configuration objects as a JSON Schema.
  //
  // `kind` is an `MQB_SCHEMA_*` selector. On `MQB_OK` the plugin either
  // writes an owned `MqbBuffer` holding a UTF-8 JSON Schema document, or
  // leaves it empty to say it describes nothing — an empty buffer is the
  // answer for a kind the plugin does not implement, so a host may ask for
  // any selector without checking first.
  //
  // The document is read once at load time and outlives the call, so the
  // plugin may build it on demand rather than keeping it resident.
  //
  // Only present when
  // `struct_size` reaches
  // `MQB_VTABLE_SIZE_V1_1`; read it through
  // `MqbPluginVTable::config_schema_hook`, never directly.
  MqbConfigSchema factory_config_schema;
  // Publishes like
  // `publisher_send_batch_outcomes`
  // and also returns the responses the sink produced.
  //
  // `out_result`, `out_responses` and `out_responses_len` start as null/0.
  // Whatever the status, the plugin may write a result handle plus a compact
  // array of responses in input order (only messages that produced one). The
  // array lives until the host passes the handle to `responses_free`.
  // `MQB_ERR_UNSUPPORTED` falls back to `publisher_send_batch_outcomes`.
  MqbPublisherSendBatchResponses publisher_send_batch_responses;
  // Releases a result written by `publisher_send_batch_responses`. Null is a no-op.
  void (*responses_free)(MqbResponsesHandle result);
  // Like `batch_commit`, and accepts
  // `MQB_DISPOSITION_REPLY`. `replies` is parallel to `dispositions`,
  // borrowed for the call, and read only where the disposition is a reply.
  MqbStatus (*batch_commit_replies)(MqbBatchHandle batch,
                                    const uint8_t *dispositions,
                                    const struct MqbMessage *replies,
                                    size_t len,
                                    struct MqbBuffer *err);
  // Writes the consumer's `EndpointStatus` as an owned UTF-8 JSON buffer;
  // `MQB_ERR_UNSUPPORTED` reports a healthy default.
  MqbStatus (*consumer_status)(MqbConsumerHandle consumer,
                               struct MqbBuffer *out,
                               struct MqbBuffer *err);
  // Writes the publisher's `EndpointStatus` as an owned UTF-8 JSON buffer;
  // `MQB_ERR_UNSUPPORTED` reports a healthy default.
  MqbStatus (*publisher_status)(MqbPublisherHandle publisher,
                                struct MqbBuffer *out,
                                struct MqbBuffer *err);
  // Non-blocking `consumer_receive_batch`;
  // see `MqbCompletion`. Read through `async_hooks`.
  MqbReceiveBatchAsync consumer_receive_batch_async;
  // Non-blocking `batch_commit_replies`.
  // The inputs are copied before it returns. The handle is consumed unless it
  // returns `MQB_ERR_UNSUPPORTED`, which falls back to the blocking commit.
  MqbBatchCommitAsync batch_commit_async;
  // Non-blocking `publisher_send_batch_responses`.
  // `messages` is copied before it returns.
  MqbPublisherSendBatchAsync publisher_send_batch_async;
  // Non-blocking `publisher_flush`.
  MqbPublisherFlushAsync publisher_flush_async;
  // Hands the plugin the host's services before `factory_create`. Called
  // for every table a library exports, so it must tolerate repeats.
  // Read through `host_init_hook`.
  MqbPluginInit plugin_init;
  // Writes the `MQB_DELIVERY_*` flags for an endpoint built from `config_json`.
  // Read through `delivery_hook`.
  MqbFactoryDelivery factory_delivery;
} MqbPluginVTable;

// Signature of the exported discovery symbol.
//
// Returns a pointer to a table with `'static` lifetime inside the plugin
// library. It must never return null and must be callable before any other
// plugin function.
typedef const struct MqbPluginVTable *(*MqbPluginEntry)(void);

// Returns the table at `index`, or null past the last one. Index 0 must be the
// table `MQB_PLUGIN_ENTRY_SYMBOL` returns, so a 1.0/1.1 host loads that one.
typedef const struct MqbPluginVTable *(*MqbPluginListEntry)(size_t index);

// The call succeeded.
#define MQB_OK 0

// Transient failure. The host may retry the operation.
#define MQB_ERR_RETRYABLE 1

// Permanent failure. Retrying cannot help.
#define MQB_ERR_PERMANENT 2

// The endpoint configuration is invalid. Never retried.
#define MQB_ERR_INVALID_CONFIG 3

// The source is exhausted and will produce no further messages.
#define MQB_END_OF_STREAM 4

// A panic was caught inside the plugin. Treated as permanent.
#define MQB_ERR_PANIC 5

// The plugin does not implement this operation (e.g. it is output-only).
#define MQB_ERR_UNSUPPORTED 6

// Connection-level failure. The host reconnects the endpoint.
#define MQB_ERR_CONNECTION 7

/* Name of the discovery symbol a plugin must export; its type is MqbPluginEntry. */
#define MQB_PLUGIN_ENTRY_SYMBOL "mq_bridge_plugin_v1"
/* Optional symbol exporting several tables; its type is MqbPluginListEntry. */
#define MQB_PLUGIN_LIST_SYMBOL "mq_bridge_plugin_v1_at"

/* Table sizes that gate the fields each ABI minor appended. */
#define MQB_VTABLE_SIZE_V1_0 (27 * sizeof(size_t))
#define MQB_VTABLE_SIZE_V1_1 (MQB_VTABLE_SIZE_V1_0 + 3 * sizeof(size_t))
#define MQB_VTABLE_SIZE_V1_2 (MQB_VTABLE_SIZE_V1_1 + 11 * sizeof(size_t))
#define MQB_HOST_VTABLE_SIZE_V1_2 (4 * sizeof(size_t))

#if defined(_WIN32)
#define MQB_PLUGIN_EXPORT __declspec(dllexport)
#else
#define MQB_PLUGIN_EXPORT __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* The discovery function every plugin defines. */
MQB_PLUGIN_EXPORT const MqbPluginVTable *mq_bridge_plugin_v1(void);

#ifdef __cplusplus
}
#define MQB_STATIC_ASSERT(cond, msg) static_assert(cond, msg)
#else
#define MQB_STATIC_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif

MQB_STATIC_ASSERT(sizeof(size_t) == 8, "the plugin ABI layout is defined for 64-bit targets");
MQB_STATIC_ASSERT(sizeof(MqbMessage) == 16 + 4 * sizeof(size_t), "MqbMessage layout");
MQB_STATIC_ASSERT(sizeof(MqbHostVTable) == MQB_HOST_VTABLE_SIZE_V1_2, "MqbHostVTable layout");
MQB_STATIC_ASSERT(sizeof(MqbPluginVTable) == MQB_VTABLE_SIZE_V1_2,
                  "MqbPluginVTable grew: add the new MQB_VTABLE_SIZE_* to include/cbindgen.toml");

#endif /* MQ_BRIDGE_PLUGIN_H */
