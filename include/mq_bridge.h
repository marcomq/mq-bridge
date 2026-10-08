/*
 * mq_bridge.h - the C API of mq-bridge: publishers, consumers and routes over
 * any supported endpoint, configured with the same YAML/JSON as the CLI.
 *
 * Rules:
 *  - Every call blocks the calling thread until it is done.
 *  - A fallible call returns an MqbStatus (MQB_OK is 0) or a null handle; the
 *    text is in mqb_last_error(), which is per thread.
 *  - Handles are opaque and may be shared between threads, except a message,
 *    which must not be changed while another thread reads it.
 *  - Every `*_new`, `*_from_*` and out-parameter handle has a matching `*_free`.
 *  - There is no ABI stability: this header and the library ship together.
 *    Compare MQB_API_VERSION with mqb_api_version() to catch a mismatch.
 *  - 64-bit targets only (see mq_bridge_plugin.h, which also defines MqbSlice,
 *    MqbStatus and the MQB_* status codes used here).
 */

#ifndef MQ_BRIDGE_H
#define MQ_BRIDGE_H

/* Generated from c/mq-bridge-c/src by cbindgen. Do not edit.
 * Regenerate: MQB_BLESS=1 cargo test -p mq-bridge-c --no-default-features --test c_header */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "mq_bridge_plugin.h"

// Bumped when a change to `mq_bridge.h` can break a program built against the
// old one; compare with `mqb_api_version()`. `tests/c_header.rs` asks on each change.
#define MQB_API_VERSION 1

// The messages of one `mqb_consumer_poll`, owned by the batch.
typedef struct mqb_batch_t mqb_batch_t;

// A pull-based consumer over one input endpoint. `mqb_consumer_poll` does not
// acknowledge: each batch stays outstanding until it is acked, nacked or committed.
typedef struct mqb_consumer_t mqb_consumer_t;

// A message: payload bytes, string metadata and an id.
typedef struct mqb_message_t mqb_message_t;

// Publishes to one output endpoint. Safe to use from several threads.
typedef struct mqb_publisher_t mqb_publisher_t;

// One route: an input endpoint, optional handlers, an output endpoint.
typedef struct mqb_route_t mqb_route_t;

// Receives one library log event. `level` is `error`, `warn`, `info`, `debug` or
// `trace`; the strings are valid for the call only.
typedef void (*mqb_log_fn)(const char *level,
                           const char *target,
                           const char *message,
                           void *user_data);

// Handles one message of a route. `message` is valid for the call only. Leave
// `*out` null to acknowledge, or set it to a new message to publish that instead
// (the library frees it). Return `MQB_OK`, `MQB_ERR_RETRYABLE` to have the
// message redelivered, or any other status to drop it as failed.
//
// Called from the library's worker threads, possibly several at once.
typedef MqbStatus (*mqb_handler_fn)(const struct mqb_message_t *message,
                                    struct mqb_message_t **out,
                                    void *user_data);

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Version of the library, e.g. `"0.4.20"`. Static; do not free.
const char *mqb_version(void);

// The `MQB_API_VERSION` this library was built with. A mismatch with the header's
// value means header and library come from different releases.
uint32_t mqb_api_version(void);

// Text of the last failed call on this thread. Valid until the next failing call
// on the same thread; do not free.
const char *mqb_last_error(void);

// Frees a string returned by this library (`mqb_config_schema`, `*_status_json`).
void mqb_string_free(char *text);

// Requests a graceful shutdown of every route. Returns true only for the first
// request; it cannot be undone.
bool mqb_request_shutdown(void);

bool mqb_is_shutdown_requested(void);

// JSON Schema of the config document, or null if the library was built without
// the `schema` feature. Free with `mqb_string_free`.
char *mqb_config_schema(void);

// Loads a native plugin library and registers the endpoints and middleware it
// exports. Call before starting a route that names them.
MqbStatus mqb_load_plugin(const char *path);

// Registers a custom endpoint or middleware implemented in this program. `table`
// is the one a plugin library would export (see `mq_bridge_plugin.h`) and must
// stay valid for the life of the process.
MqbStatus mqb_register_plugin(const MqbPluginVTable *table);

// Routes the library's log events into `callback`, which may be called from any
// thread. `level` (null for `warn`) seeds the filter; `MQ_BRIDGE_LOG` / `RUST_LOG`
// override it. Fails if logging was already initialized.
MqbStatus mqb_init_logging(mqb_log_fn callback, void *user_data, const char *level);

// Builds a consumer from a YAML or JSON config file. `name` selects an entry of a
// `consumers:` document; null or `""` for a single bare endpoint. Null on error.
struct mqb_consumer_t *mqb_consumer_from_file(const char *path, const char *name);

// Like `mqb_consumer_from_file`, from YAML or JSON text.
struct mqb_consumer_t *mqb_consumer_from_str(const char *config, const char *name);

// Receives up to `max` messages into `*batch`. `timeout_ms < 0` blocks until
// something arrives, and `mqb_consumer_close` / `mqb_consumer_status_json` on
// another thread wait for it. `*batch` is null when the timeout passed or the source is
// exhausted (see `mqb_consumer_exhausted`); otherwise free it with `mqb_batch_free`.
MqbStatus mqb_consumer_poll(const struct mqb_consumer_t *consumer,
                            uint32_t max,
                            int64_t timeout_ms,
                            struct mqb_batch_t **batch);

size_t mqb_batch_count(const struct mqb_batch_t *batch);

// The message at `index`, owned by the batch; null when out of range.
const struct mqb_message_t *mqb_batch_at(const struct mqb_batch_t *batch, size_t index);

// The token for `mqb_consumer_ack` / `mqb_consumer_nack`.
uint32_t mqb_batch_token(const struct mqb_batch_t *batch);

// Frees the batch and its messages. Does not acknowledge them.
void mqb_batch_free(struct mqb_batch_t *batch);

// Acknowledges every outstanding batch, oldest first. Without an ack or commit
// the source redelivers and most brokers eventually stall.
MqbStatus mqb_consumer_commit(const struct mqb_consumer_t *consumer);

// Acknowledges one batch by its token.
MqbStatus mqb_consumer_ack(const struct mqb_consumer_t *consumer, uint32_t token);

// Negatively acknowledges one batch so the broker can redeliver it.
MqbStatus mqb_consumer_nack(const struct mqb_consumer_t *consumer, uint32_t token);

// Negatively acknowledges every outstanding batch, oldest first.
MqbStatus mqb_consumer_nack_all(const struct mqb_consumer_t *consumer);

// Status snapshot of the endpoint as JSON (`healthy`, `target`, optional
// `pending` backlog, ...), or null on error. Free with `mqb_string_free`.
char *mqb_consumer_status_json(const struct mqb_consumer_t *consumer);

// True once the source signalled end-of-stream (e.g. a drained file).
bool mqb_consumer_exhausted(const struct mqb_consumer_t *consumer);

// Releases the endpoint connection. Idempotent; polling fails afterwards.
MqbStatus mqb_consumer_close(const struct mqb_consumer_t *consumer);

// Closes and frees the consumer. Outstanding batches are left unacknowledged.
void mqb_consumer_free(struct mqb_consumer_t *consumer);

// Creates a message with a copy of `payload` and a generated id. Free it with
// `mqb_message_free`; sending does not consume it.
struct mqb_message_t *mqb_message_new(const uint8_t *payload, size_t len);

// Sets the id: a UUID, a `0x` hex or decimal integer, or any text (hashed).
MqbStatus mqb_message_set_id(struct mqb_message_t *message, const char *id);

MqbStatus mqb_message_set_metadata(struct mqb_message_t *message,
                                   const char *key,
                                   const char *value);

// The payload. Like every slice a message returns, it is not NUL-terminated and
// stays valid until the message is changed or freed.
MqbSlice mqb_message_payload(const struct mqb_message_t *message);

MqbSlice mqb_message_id(const struct mqb_message_t *message);

// The metadata value of `key`; `ptr` is null when there is none.
MqbSlice mqb_message_metadata(const struct mqb_message_t *message, const char *key);

size_t mqb_message_metadata_count(const struct mqb_message_t *message);

// The metadata entry at `index` (below `mqb_message_metadata_count`), in no
// particular order.
MqbStatus mqb_message_metadata_at(const struct mqb_message_t *message,
                                  size_t index,
                                  MqbSlice *key,
                                  MqbSlice *value);

void mqb_message_free(struct mqb_message_t *message);

// Builds a publisher from a YAML or JSON config file. `name` selects an entry of
// a `publishers:` document; null or `""` for a single bare endpoint. Null on error.
struct mqb_publisher_t *mqb_publisher_from_file(const char *path, const char *name);

// Like `mqb_publisher_from_file`, from YAML or JSON text.
struct mqb_publisher_t *mqb_publisher_from_str(const char *config, const char *name);

// Publishes one message and waits for the endpoint to accept it.
MqbStatus mqb_publisher_send(const struct mqb_publisher_t *publisher,
                             const struct mqb_message_t *message);

// Publishes `count` messages in one call. Fails if any of them was rejected.
MqbStatus mqb_publisher_send_batch(const struct mqb_publisher_t *publisher,
                                   const struct mqb_message_t *const *messages,
                                   size_t count);

// Sends a request and stores the reply in `*response` (free it with
// `mqb_message_free`; null on failure). Needs an endpoint that supports request-reply.
MqbStatus mqb_publisher_request(const struct mqb_publisher_t *publisher,
                                const struct mqb_message_t *message,
                                struct mqb_message_t **response);

void mqb_publisher_free(struct mqb_publisher_t *publisher);

// Builds a route from a YAML or JSON config file: a `routes:` document, a
// `{name: route}` map, or a single route body (`name` null or `""`). Null on error.
struct mqb_route_t *mqb_route_from_file(const char *path, const char *name);

// Like `mqb_route_from_file`, from YAML or JSON text.
struct mqb_route_t *mqb_route_from_str(const char *config, const char *name);

// Runs every message through `handler` before the output. Set before `mqb_route_start`.
MqbStatus mqb_route_set_handler(const struct mqb_route_t *route,
                                mqb_handler_fn handler,
                                void *user_data);

// Runs messages whose `kind` metadata equals `kind` through `handler`. May be
// called once per kind. Set before `mqb_route_start`.
MqbStatus mqb_route_add_handler(const struct mqb_route_t *route,
                                const char *kind,
                                mqb_handler_fn handler,
                                void *user_data);

// Connects both endpoints and starts moving messages in the background.
MqbStatus mqb_route_start(const struct mqb_route_t *route);

// Asks a running route to stop; `mqb_route_join` waits for it.
MqbStatus mqb_route_stop(const struct mqb_route_t *route);

// Blocks until the route has stopped, by `mqb_route_stop` or on its own (a
// drained source under `exit_on_empty`). Fails if it ended on a permanent error.
MqbStatus mqb_route_join(const struct mqb_route_t *route);

// Stops the route if it is running, waits for it, and frees it.
void mqb_route_free(struct mqb_route_t *route);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif /* MQ_BRIDGE_H */
