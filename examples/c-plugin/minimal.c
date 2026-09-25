/* The smallest useful plugin: a middleware that drops "ping" heartbeats. */
#include <stdlib.h>
#include <string.h>

#include "mq_bridge_plugin_helpers.h"

static MqbStatus apply(MqbMiddlewareHandle middleware, const MqbMessage *messages, size_t len,
                       MqbFilterHandle *out_result, const MqbMessage **out_messages,
                       const uint8_t **out_kept, MqbBuffer *err) {
    uint8_t *kept = malloc(len + 1);
    if (kept == NULL) {
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    for (size_t i = 0; i < len; i++) {
        MqbSlice p = messages[i].payload;
        int ping = p.len == 4 && memcmp(p.ptr, "ping", 4) == 0;
        kept[i] = ping ? MQB_MESSAGE_DROPPED : MQB_MESSAGE_KEPT;
    }
    *out_result = kept;
    *out_messages = messages; /* kept messages pass through unchanged */
    *out_kept = kept;
    return MQB_OK;
}

static const MqbPluginVTable table = {
    MQB_TABLE_HEADER("drop_heartbeats", "0.1.0", MQB_CAP_MIDDLEWARE),
    MQB_DEFAULT_FACTORY,
    MQB_NO_CONSUMER,
    MQB_NO_PUBLISHER,
    MQB_STATELESS_MIDDLEWARE,
    .middleware_apply = apply,
    .middleware_result_free = free,
};

const MqbPluginVTable *mq_bridge_plugin_v1(void) { return &table; }
