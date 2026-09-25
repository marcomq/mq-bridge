/*
 * mq-bridge plugin wrapping two existing C libraries.
 *
 * Middleware: each fixed-width record becomes a JSON payload (legacy_parser);
 * a record it rejects is dropped and logged. Output: each message is appended
 * to a ledger file (legacy_ledger), configured as { "path": "..." }.
 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "legacy_ledger.h"
#include "legacy_parser.h"
#include "mq_bridge_plugin_helpers.h"

#define PLUGIN_NAME "legacy_payments"
#define JSON_CAPACITY 128

/* One allocation per call: output messages, then JSON payloads, then keep flags. */
static MqbStatus apply(MqbMiddlewareHandle middleware, const MqbMessage *messages, size_t len,
                       MqbFilterHandle *out_result, const MqbMessage **out_messages,
                       const uint8_t **out_kept, MqbBuffer *err) {
    uint8_t *block = malloc(len * (sizeof(MqbMessage) + JSON_CAPACITY + 1) + 1);
    if (block == NULL) {
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    MqbMessage *out = (MqbMessage *)block;
    char *json = (char *)(block + len * sizeof(MqbMessage));
    uint8_t *kept = (uint8_t *)(json + len * JSON_CAPACITY);

    for (size_t i = 0; i < len; i++) {
        legacy_record record;
        const char *problem =
            legacy_parse(messages[i].payload.ptr, messages[i].payload.len, &record);
        if (problem != NULL) {
            char text[128];
            snprintf(text, sizeof(text), "dropping record: %s", problem);
            mqb_log(MQB_LOG_WARN, PLUGIN_NAME, text);
            kept[i] = MQB_MESSAGE_DROPPED;
            continue;
        }
        char *payload = json + i * JSON_CAPACITY;
        int n = snprintf(payload, JSON_CAPACITY,
                         "{\"account\":\"%s\",\"amount_minor\":%llu,\"currency\":\"%s\"}",
                         record.account, (unsigned long long)record.amount_minor,
                         record.currency);
        out[i] = messages[i]; /* id and metadata point into the input, which is allowed */
        out[i].payload.ptr = (const uint8_t *)payload;
        out[i].payload.len = (size_t)n;
        kept[i] = MQB_MESSAGE_KEPT;
    }

    *out_result = block;
    *out_messages = out;
    *out_kept = kept;
    return MQB_OK;
}

/* The ledger is not thread-safe, and the host may send from several threads. */
typedef struct {
    pthread_mutex_t lock;
    legacy_ledger *ledger;
} ledger_publisher;

/* Reads "path" from the config. Minimal on purpose (no escapes): use a JSON library in real code. */
static int config_path(MqbSlice config, char path[256]) {
    char json[1024];
    if (config.len >= sizeof(json)) {
        return -1;
    }
    memcpy(json, config.ptr, config.len);
    json[config.len] = '\0';
    const char *at = strstr(json, "\"path\"");
    return at != NULL && sscanf(at, "\"path\" : \"%255[^\"]\"", path) == 1 ? 0 : -1;
}

static MqbStatus publisher_create(MqbFactoryHandle factory, MqbSlice route_name,
                                  MqbSlice config_json, MqbPublisherHandle *out, MqbBuffer *err) {
    char path[256];
    if (config_path(config_json, path) != 0) {
        mqb_set_error(err, "config needs a \"path\"");
        return MQB_ERR_INVALID_CONFIG;
    }
    ledger_publisher *publisher = malloc(sizeof(*publisher));
    if (publisher == NULL || (publisher->ledger = legacy_ledger_open(path)) == NULL) {
        free(publisher);
        mqb_set_error(err, "could not open the ledger");
        return MQB_ERR_RETRYABLE;
    }
    pthread_mutex_init(&publisher->lock, NULL);
    *out = publisher;
    return MQB_OK;
}

static MqbStatus publisher_send(MqbPublisherHandle handle, const MqbMessage *messages,
                                size_t len, MqbBuffer *err) {
    ledger_publisher *publisher = handle;
    int failed = 0;
    pthread_mutex_lock(&publisher->lock);
    for (size_t i = 0; i < len && !failed; i++) {
        failed = legacy_ledger_append(publisher->ledger, messages[i].payload.ptr,
                                      messages[i].payload.len);
    }
    pthread_mutex_unlock(&publisher->lock);
    if (failed) {
        mqb_set_error(err, "could not append to the ledger");
        return MQB_ERR_RETRYABLE; /* the whole batch is retried: at-least-once */
    }
    return MQB_OK;
}

static MqbStatus publisher_flush(MqbPublisherHandle handle, MqbBuffer *err) {
    ledger_publisher *publisher = handle;
    pthread_mutex_lock(&publisher->lock);
    int failed = legacy_ledger_flush(publisher->ledger);
    pthread_mutex_unlock(&publisher->lock);
    if (failed) {
        mqb_set_error(err, "could not flush the ledger");
        return MQB_ERR_RETRYABLE;
    }
    return MQB_OK;
}

static void publisher_free(MqbPublisherHandle handle) {
    ledger_publisher *publisher = handle;
    if (publisher == NULL) {
        return;
    }
    legacy_ledger_close(publisher->ledger);
    pthread_mutex_destroy(&publisher->lock);
    free(publisher);
}

/* Runs inside the host's signal handler, after its crash dump: async-signal-safe calls only. */
static void on_crash(void *user_data, const MqbCrashInfo *info) {
    static const char text[] = PLUGIN_NAME ": crashed; send this output to the plugin vendor\n";
    (void)user_data;
    (void)info;
    (void)!write(2, text, sizeof(text) - 1);
}

static void init(const MqbHostVTable *host) {
    mqb_stub_init(host);
    mqb_register_crash_handler(on_crash, NULL);
}

static const MqbPluginVTable table = {
    MQB_TABLE_HEADER(PLUGIN_NAME, "0.1.0", MQB_CAP_MIDDLEWARE | MQB_CAP_PUBLISHER),
    MQB_FACTORY_WITHOUT_INIT,
    .plugin_init = init,
    MQB_NO_CONSUMER,
    MQB_STATELESS_MIDDLEWARE,
    .middleware_apply = apply,
    .middleware_result_free = free,
    MQB_BLOCKING_PUBLISHER,
    .publisher_create = publisher_create,
    .publisher_send_batch = publisher_send,
    .publisher_flush = publisher_flush,
    .publisher_close = publisher_flush,
    .publisher_free = publisher_free,
};

const MqbPluginVTable *mq_bridge_plugin_v1(void) { return &table; }
