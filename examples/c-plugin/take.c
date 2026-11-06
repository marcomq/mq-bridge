/* A middleware that wraps the endpoint the host lends it (ABI 1.5): an input that ends
 * after `limit` messages. A map/drop middleware cannot end a stream. */
#include <stdlib.h>
#include <string.h>

#include "mq_bridge_plugin_helpers.h"

typedef struct {
    size_t limit;
} Take;

typedef struct {
    const MqbPluginVTable *table;
    MqbConsumerHandle inner;
    size_t left;
} TakeConsumer;

/* A batch of the lent endpoint, with the table its entries go to. */
typedef struct {
    const MqbPluginVTable *table;
    MqbBatchHandle inner;
} TakeBatch;

static MqbStatus take_create(MqbFactoryHandle factory, MqbSlice route_name, MqbSlice config_json,
                             uint8_t side, MqbMiddlewareHandle *out, MqbBuffer *err) {
    if (side != MQB_MIDDLEWARE_RECEIVE) {
        mqb_set_error(err, "take is an input middleware");
        return MQB_ERR_INVALID_CONFIG;
    }
    /* Crude on purpose: the config is {"limit": N}. */
    char config[64] = {0};
    memcpy(config, config_json.ptr, config_json.len < 63 ? config_json.len : 63);
    const char *limit = strstr(config, "\"limit\"");
    Take *take = malloc(sizeof *take);
    if (limit == NULL || take == NULL) {
        free(take);
        mqb_set_error(err, "take needs {\"limit\": N}");
        return MQB_ERR_INVALID_CONFIG;
    }
    take->limit = strtoul(strchr(limit, ':') + 1, NULL, 10);
    *out = take;
    return MQB_OK;
}

static MqbStatus take_wrap(MqbMiddlewareHandle middleware, const MqbPluginVTable *inner_table,
                           MqbConsumerHandle inner, MqbConsumerHandle *out, MqbBuffer *err) {
    TakeConsumer *consumer = malloc(sizeof *consumer);
    if (consumer == NULL) {
        mqb_inner_consumer_free(inner_table, inner);
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    consumer->table = inner_table;
    consumer->inner = inner;
    consumer->left = ((Take *)middleware)->limit;
    *out = consumer;
    return MQB_OK;
}

static MqbStatus receive(MqbConsumerHandle handle, size_t max_messages, MqbBatchHandle *out_batch,
                         const MqbMessage **out_messages, size_t *out_len, MqbBuffer *err) {
    TakeConsumer *consumer = handle;
    if (consumer->left == 0) {
        return MQB_END_OF_STREAM;
    }
    TakeBatch *batch = malloc(sizeof *batch);
    if (batch == NULL) {
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    MqbBuffer inner_err = {0};
    size_t max = max_messages < consumer->left ? max_messages : consumer->left;
    MqbStatus status = consumer->table->consumer_receive_batch(
        consumer->inner, max, &batch->inner, out_messages, out_len, &inner_err);
    if (status != MQB_OK) {
        free(batch);
        return mqb_inner_status(consumer->table, status, inner_err, err);
    }
    batch->table = consumer->table;
    consumer->left -= *out_len;
    *out_batch = batch;
    return MQB_OK;
}

static MqbStatus commit(MqbBatchHandle handle, const uint8_t *dispositions, size_t len,
                        MqbBuffer *err) {
    TakeBatch *batch = handle;
    MqbBuffer inner_err = {0};
    MqbStatus status = batch->table->batch_commit(batch->inner, dispositions, len, &inner_err);
    return mqb_inner_status(batch->table, status, inner_err, err);
}

static MqbStatus commit_replies(MqbBatchHandle handle, const uint8_t *dispositions,
                                const MqbMessage *replies, size_t len, MqbBuffer *err) {
    TakeBatch *batch = handle;
    MqbBuffer inner_err = {0};
    MqbStatus status = batch->table->batch_commit_replies(batch->inner, dispositions, replies,
                                                          len, &inner_err);
    return mqb_inner_status(batch->table, status, inner_err, err);
}

static void batch_free(MqbBatchHandle handle) {
    TakeBatch *batch = handle;
    batch->table->batch_free(batch->inner);
    free(batch);
}

static uint8_t requires_order(MqbConsumerHandle handle) {
    TakeConsumer *consumer = handle;
    return consumer->table->consumer_commit_requires_order(consumer->inner);
}

static void set_exit_on_empty(MqbConsumerHandle handle, uint8_t exit_on_empty) {
    TakeConsumer *consumer = handle;
    consumer->table->consumer_set_exit_on_empty(consumer->inner, exit_on_empty);
}

static MqbStatus close_consumer(MqbConsumerHandle handle, MqbBuffer *err) {
    TakeConsumer *consumer = handle;
    MqbBuffer inner_err = {0};
    MqbStatus status = consumer->table->consumer_close(consumer->inner, &inner_err);
    return mqb_inner_status(consumer->table, status, inner_err, err);
}

static void free_consumer(MqbConsumerHandle handle) {
    TakeConsumer *consumer = handle;
    mqb_inner_consumer_free(consumer->table, consumer->inner);
    free(consumer);
}

static const MqbPluginVTable table = {
    MQB_TABLE_HEADER("take", "0.1.0", MQB_CAP_MIDDLEWARE | MQB_CAP_MIDDLEWARE_WRAP),
    MQB_DEFAULT_FACTORY,
    MQB_NO_PUBLISHER,
    MQB_WRAPPING_MIDDLEWARE,
    .middleware_create = take_create,
    .middleware_free = free,
    .middleware_wrap_consumer = take_wrap,
    .middleware_wrap_publisher = mqb_stub_wrap_publisher,
    /* The consumer handed back by take_wrap. */
    .consumer_create = mqb_stub_create,
    .consumer_receive_batch = receive,
    .consumer_commit_requires_order = requires_order,
    .consumer_set_exit_on_empty = set_exit_on_empty,
    .consumer_close = close_consumer,
    .consumer_free = free_consumer,
    .consumer_status = mqb_stub_status,
    .batch_commit = commit,
    .batch_commit_replies = commit_replies,
    .batch_free = batch_free,
    .consumer_receive_batch_async = mqb_stub_receive_async,
    .batch_commit_async = mqb_stub_commit_async,
};

const MqbPluginVTable *mq_bridge_plugin_v1(void) { return &table; }
