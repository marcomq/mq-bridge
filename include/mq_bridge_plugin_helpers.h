/*
 * mq_bridge_plugin_helpers.h - optional shortcuts for a plugin written in C.
 *
 * Every entry of MqbPluginVTable must be non-null. These macros fill the ones a
 * plugin does not implement, so a table lists only what it actually does:
 *
 *   static const MqbPluginVTable table = {
 *       MQB_TABLE_HEADER("my_filter", "0.1.0", MQB_CAP_MIDDLEWARE),
 *       MQB_DEFAULT_FACTORY,
 *       MQB_NO_CONSUMER,
 *       MQB_NO_PUBLISHER,
 *       MQB_STATELESS_MIDDLEWARE,
 *       .middleware_apply = apply,
 *       .middleware_result_free = result_free,
 *   };
 *
 * Use each group at most once and don't set an entry a group already sets.
 * C99 designated initializers only: C++ fills the table in declaration order.
 */
#ifndef MQ_BRIDGE_PLUGIN_HELPERS_H
#define MQ_BRIDGE_PLUGIN_HELPERS_H

#include <stdlib.h>
#include <string.h>

#include "mq_bridge_plugin.h"

/* The host's log/metric services, set by MQB_DEFAULT_FACTORY's plugin_init. */
static const MqbHostVTable *mqb_host;
static char mqb_token;

static inline MqbSlice mqb_str(const char *text) {
    MqbSlice slice = {(const uint8_t *)text, strlen(text)};
    return slice;
}

/* Writes `text` as the error of a failing call; freed by the default buffer_free. */
static inline void mqb_set_error(MqbBuffer *err, const char *text) {
    size_t len = strlen(text);
    uint8_t *ptr = (uint8_t *)malloc(len);
    if (err == NULL || ptr == NULL) {
        free(ptr);
        return;
    }
    memcpy(ptr, text, len);
    err->ptr = ptr;
    err->len = len;
    err->cap = len;
}

/* Logs through the host; a no-op before plugin_init or when the level is off. */
static inline void mqb_log(uint8_t level, const char *target, const char *text) {
    if (mqb_host != NULL && mqb_host->struct_size >= MQB_HOST_VTABLE_SIZE_V1_2 &&
        mqb_host->log_enabled(level)) {
        mqb_host->log(level, mqb_str(target), mqb_str(text));
    }
}

#if defined(__GNUC__)
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"
#endif

static inline void mqb_stub_init(const MqbHostVTable *host) { mqb_host = host; }
static inline MqbStatus mqb_stub_factory_create(MqbFactoryHandle *out, MqbBuffer *err) {
    *out = &mqb_token;
    return MQB_OK;
}
static inline void mqb_stub_buffer_free(MqbBuffer buffer) { free(buffer.ptr); }
static inline MqbStatus mqb_stub_config_schema(MqbFactoryHandle factory, uint32_t kind,
                                               MqbBuffer *out, MqbBuffer *err) {
    return MQB_OK;
}
static inline MqbStatus mqb_stub_delivery(MqbFactoryHandle factory, MqbSlice config_json,
                                          uint8_t *out_flags, MqbBuffer *err) {
    *out_flags = 0;
    return MQB_OK;
}
static inline MqbStatus mqb_stub_middleware_create(MqbFactoryHandle factory,
                                                   MqbSlice route_name, MqbSlice config_json,
                                                   uint8_t side, MqbMiddlewareHandle *out,
                                                   MqbBuffer *err) {
    *out = &mqb_token;
    return MQB_OK;
}

static inline void mqb_stub_free(void *handle) {}
static inline uint8_t mqb_stub_false(void *handle) { return 0; }
static inline void mqb_stub_set_exit_on_empty(MqbConsumerHandle consumer, uint8_t exit_on_empty) {}
static inline MqbStatus mqb_stub_handle(void *handle, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_status(void *handle, MqbBuffer *out, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_create(MqbFactoryHandle factory, MqbSlice route_name,
                                        MqbSlice config_json, void **out, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_receive(MqbConsumerHandle consumer, size_t max_messages,
                                         MqbBatchHandle *out_batch,
                                         const MqbMessage **out_messages, size_t *out_len,
                                         MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_commit(MqbBatchHandle batch, const uint8_t *dispositions,
                                        size_t len, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_commit_replies(MqbBatchHandle batch,
                                                const uint8_t *dispositions,
                                                const MqbMessage *replies, size_t len,
                                                MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_receive_async(MqbConsumerHandle consumer, size_t max_messages,
                                               MqbBatchHandle *out_batch,
                                               const MqbMessage **out_messages, size_t *out_len,
                                               MqbBuffer *err, MqbCompletion completion) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_commit_async(MqbBatchHandle batch, const uint8_t *dispositions,
                                              const MqbMessage *replies, size_t len,
                                              MqbBuffer *err, MqbCompletion completion) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_send(MqbPublisherHandle publisher, const MqbMessage *messages,
                                      size_t len, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_send_outcomes(MqbPublisherHandle publisher,
                                               const MqbMessage *messages, size_t len,
                                               uint8_t *out_outcomes, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_send_responses(MqbPublisherHandle publisher,
                                                const MqbMessage *messages, size_t len,
                                                uint8_t *out_outcomes,
                                                MqbResponsesHandle *out_result,
                                                const MqbMessage **out_responses,
                                                size_t *out_responses_len, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_send_async(MqbPublisherHandle publisher,
                                            const MqbMessage *messages, size_t len,
                                            uint8_t *out_outcomes, MqbResponsesHandle *out_result,
                                            const MqbMessage **out_responses,
                                            size_t *out_responses_len, MqbBuffer *err,
                                            MqbCompletion completion) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_flush_async(MqbPublisherHandle publisher, MqbBuffer *err,
                                             MqbCompletion completion) {
    return MQB_ERR_UNSUPPORTED;
}
static inline MqbStatus mqb_stub_middleware_apply(MqbMiddlewareHandle middleware,
                                                  const MqbMessage *messages, size_t len,
                                                  MqbFilterHandle *out_result,
                                                  const MqbMessage **out_messages,
                                                  const uint8_t **out_kept, MqbBuffer *err) {
    return MQB_ERR_UNSUPPORTED;
}

#if defined(__GNUC__)
#pragma GCC diagnostic pop
#endif

#define MQB_TABLE_HEADER(name_, version_, capabilities_)                                    \
    .struct_size = sizeof(MqbPluginVTable), .abi_major = MQB_PLUGIN_ABI_MAJOR,              \
    .abi_minor = MQB_PLUGIN_ABI_MINOR, .capabilities = (capabilities_),                     \
    .name = {(const uint8_t *)(name_), sizeof(name_) - 1},                                  \
    .version = {(const uint8_t *)(version_), sizeof(version_) - 1}

/* A stateless factory with no config schema and no delivery guarantees. */
#define MQB_DEFAULT_FACTORY                                                                 \
    .factory_create = mqb_stub_factory_create, .factory_free = mqb_stub_free,               \
    .buffer_free = mqb_stub_buffer_free, .factory_config_schema = mqb_stub_config_schema,   \
    .factory_delivery = mqb_stub_delivery, .plugin_init = mqb_stub_init

#define MQB_NO_CONSUMER                                                                     \
    .consumer_create = mqb_stub_create, .consumer_receive_batch = mqb_stub_receive,         \
    .consumer_commit_requires_order = mqb_stub_false,                                       \
    .consumer_set_exit_on_empty = mqb_stub_set_exit_on_empty,                               \
    .consumer_close = mqb_stub_handle, .consumer_free = mqb_stub_free,                      \
    .batch_commit = mqb_stub_commit, .batch_free = mqb_stub_free,                           \
    .batch_commit_replies = mqb_stub_commit_replies, .consumer_status = mqb_stub_status,    \
    .consumer_receive_batch_async = mqb_stub_receive_async,                                 \
    .batch_commit_async = mqb_stub_commit_async

#define MQB_NO_PUBLISHER                                                                    \
    .publisher_create = mqb_stub_create, .publisher_send_batch = mqb_stub_send,             \
    .publisher_flush = mqb_stub_handle, .publisher_close = mqb_stub_handle,                 \
    .publisher_free = mqb_stub_free, .publisher_requires_ordered_publish = mqb_stub_false,  \
    .publisher_send_batch_outcomes = mqb_stub_send_outcomes,                                \
    .publisher_send_batch_responses = mqb_stub_send_responses,                              \
    .responses_free = mqb_stub_free, .publisher_status = mqb_stub_status,                   \
    .publisher_send_batch_async = mqb_stub_send_async,                                      \
    .publisher_flush_async = mqb_stub_flush_async

/* The optional publisher entries: the host then calls publisher_send_batch and
 * publisher_flush on a blocking thread. Set create, the two, close and free. */
#define MQB_BLOCKING_PUBLISHER                                                              \
    .publisher_requires_ordered_publish = mqb_stub_false,                                   \
    .publisher_send_batch_outcomes = mqb_stub_send_outcomes,                                \
    .publisher_send_batch_responses = mqb_stub_send_responses,                              \
    .responses_free = mqb_stub_free, .publisher_status = mqb_stub_status,                   \
    .publisher_send_batch_async = mqb_stub_send_async,                                      \
    .publisher_flush_async = mqb_stub_flush_async

#define MQB_NO_MIDDLEWARE                                                                   \
    .middleware_create = mqb_stub_middleware_create,                                        \
    .middleware_apply = mqb_stub_middleware_apply,                                          \
    .middleware_result_free = mqb_stub_free, .middleware_free = mqb_stub_free

/* middleware_create/_free for a middleware that keeps no per-route state. */
#define MQB_STATELESS_MIDDLEWARE                                                            \
    .middleware_create = mqb_stub_middleware_create, .middleware_free = mqb_stub_free

#endif /* MQ_BRIDGE_PLUGIN_HELPERS_H */
