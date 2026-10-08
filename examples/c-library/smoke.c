/* End-to-end check of mq_bridge.h, run by c/mq-bridge-c/tests/c_api.rs.
 * Uses in-memory endpoints and files in the directory given as the first
 * argument (default /tmp), so it needs no broker. */
#define _POSIX_C_SOURCE 200809L

#include <ctype.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "mq_bridge.h"
#include "mq_bridge_plugin_helpers.h"

#define CHECK(cond)                                                                         \
    do {                                                                                    \
        if (!(cond)) {                                                                      \
            fprintf(stderr, "%s:%d: CHECK(%s) failed: %s\n", __FILE__, __LINE__, #cond,     \
                    mqb_last_error());                                                      \
            exit(1);                                                                        \
        }                                                                                   \
    } while (0)

static int slice_is(MqbSlice slice, const char *text) {
    return slice.ptr != NULL && slice.len == strlen(text) && memcmp(slice.ptr, text, slice.len) == 0;
}

static mqb_message_t *text_message(const char *text) {
    mqb_message_t *message = mqb_message_new((const uint8_t *)text, strlen(text));
    CHECK(message != NULL);
    return message;
}

static void send_text(const mqb_publisher_t *publisher, const char *text) {
    mqb_message_t *message = text_message(text);
    CHECK(mqb_publisher_send(publisher, message) == MQB_OK);
    mqb_message_free(message);
}

static void sleep_ms(long ms) {
    struct timespec pause = {ms / 1000, (ms % 1000) * 1000000L};
    nanosleep(&pause, NULL);
}

static void test_versions(void) {
    CHECK(mqb_api_version() == MQB_API_VERSION);
    CHECK(strlen(mqb_version()) > 0);
}

static void test_bad_config(void) {
    CHECK(mqb_publisher_from_str("no_such_endpoint_type: {}", NULL) == NULL);
    CHECK(strlen(mqb_last_error()) > 0);
    CHECK(mqb_route_from_file("/no/such/file.yaml", NULL) == NULL);
    CHECK(mqb_publisher_send(NULL, NULL) == MQB_ERR_PERMANENT);
    CHECK(mqb_message_new(NULL, 5) == NULL);
    CHECK(mqb_init_logging(NULL, NULL, NULL) != MQB_OK);

    mqb_route_t *route = mqb_route_from_str(
        "input: { memory: { topic: smoke_null_in } }\n"
        "output: { memory: { topic: smoke_null_out } }\n",
        NULL);
    CHECK(route != NULL);
    CHECK(mqb_route_set_handler(route, NULL, NULL) != MQB_OK);
    CHECK(mqb_route_add_handler(route, "kind", NULL, NULL) != MQB_OK);
    mqb_route_free(route);

    /* A failed request leaves no stale response behind. */
    mqb_message_t *response = (mqb_message_t *)&route;
    CHECK(mqb_publisher_request(NULL, NULL, &response) != MQB_OK);
    CHECK(response == NULL);
}

static void test_message(void) {
    mqb_message_t *message = text_message("hello");
    MqbSlice key, value;
    CHECK(mqb_message_set_metadata(message, "kind", "greeting") == MQB_OK);
    CHECK(mqb_message_set_id(message, "0x2a") == MQB_OK);
    CHECK(slice_is(mqb_message_payload(message), "hello"));
    CHECK(slice_is(mqb_message_metadata(message, "kind"), "greeting"));
    CHECK(mqb_message_metadata(message, "absent").ptr == NULL);
    CHECK(mqb_message_metadata_count(message) == 1);
    CHECK(mqb_message_metadata_at(message, 0, &key, &value) == MQB_OK);
    CHECK(slice_is(key, "kind") && slice_is(value, "greeting"));
    CHECK(mqb_message_metadata_at(message, 1, &key, &value) != MQB_OK);
    CHECK(mqb_message_id(message).len > 0);
    mqb_message_free(message);
}

static void test_publish_and_poll(void) {
    mqb_publisher_t *publisher = mqb_publisher_from_str("memory: { topic: smoke_poll }", NULL);
    mqb_consumer_t *consumer = mqb_consumer_from_str("memory: { topic: smoke_poll }", NULL);
    mqb_batch_t *batch = NULL;
    CHECK(publisher != NULL && consumer != NULL);

    mqb_message_t *first = text_message("one");
    CHECK(mqb_message_set_metadata(first, "kind", "number") == MQB_OK);
    mqb_message_t *second = text_message("two");
    const mqb_message_t *pair[] = {first, second};
    CHECK(mqb_publisher_send_batch(publisher, pair, 2) == MQB_OK);
    mqb_message_free(first);
    mqb_message_free(second);

    size_t seen = 0;
    while (seen < 2) {
        CHECK(mqb_consumer_poll(consumer, 10, 5000, &batch) == MQB_OK);
        CHECK(batch != NULL);
        if (seen == 0) {
            const mqb_message_t *message = mqb_batch_at(batch, 0);
            CHECK(slice_is(mqb_message_payload(message), "one"));
            CHECK(slice_is(mqb_message_metadata(message, "kind"), "number"));
        }
        seen += mqb_batch_count(batch);
        CHECK(mqb_batch_at(batch, mqb_batch_count(batch)) == NULL);
        mqb_batch_free(batch);
    }
    CHECK(mqb_consumer_commit(consumer) == MQB_OK);

    /* Nothing left: the poll times out with no batch. */
    CHECK(mqb_consumer_poll(consumer, 10, 50, &batch) == MQB_OK);
    CHECK(batch == NULL);

    /* A batch is settled once, by its token. */
    send_text(publisher, "three");
    CHECK(mqb_consumer_poll(consumer, 10, 5000, &batch) == MQB_OK);
    CHECK(batch != NULL);
    uint32_t token = mqb_batch_token(batch);
    mqb_batch_free(batch);
    CHECK(mqb_consumer_ack(consumer, token) == MQB_OK);
    CHECK(mqb_consumer_ack(consumer, token) != MQB_OK);
    CHECK(mqb_consumer_nack(consumer, token + 1) != MQB_OK);

    char *status = mqb_consumer_status_json(consumer);
    CHECK(status != NULL && status[0] == '{');
    mqb_string_free(status);

    CHECK(mqb_consumer_close(consumer) == MQB_OK);
    CHECK(mqb_consumer_poll(consumer, 1, 0, &batch) != MQB_OK);
    mqb_consumer_free(consumer);
    mqb_publisher_free(publisher);
}

/* Publishes the payload in upper case; `user_data` counts the calls. */
static MqbStatus upper_case(const mqb_message_t *message, mqb_message_t **out, void *user_data) {
    MqbSlice payload = mqb_message_payload(message);
    uint8_t *upper = malloc(payload.len + 1);
    if (upper == NULL) {
        return MQB_ERR_RETRYABLE;
    }
    for (size_t i = 0; i < payload.len; i++) {
        upper[i] = (uint8_t)toupper(payload.ptr[i]);
    }
    *out = mqb_message_new(upper, payload.len);
    free(upper);
    atomic_fetch_add((atomic_int *)user_data, 1);
    return *out != NULL ? MQB_OK : MQB_ERR_PERMANENT;
}

static void test_route_with_handler(void) {
    atomic_int calls = 0;
    mqb_route_t *route = mqb_route_from_str(
        "input: { memory: { topic: smoke_route_in } }\n"
        "output: { memory: { topic: smoke_route_out } }\n",
        NULL);
    CHECK(route != NULL);
    CHECK(mqb_route_set_handler(route, upper_case, &calls) == MQB_OK);
    CHECK(mqb_route_start(route) == MQB_OK);
    CHECK(mqb_route_start(route) != MQB_OK);

    mqb_publisher_t *publisher =
        mqb_publisher_from_str("memory: { topic: smoke_route_in }", NULL);
    mqb_consumer_t *consumer = mqb_consumer_from_str("memory: { topic: smoke_route_out }", NULL);
    mqb_batch_t *batch = NULL;
    CHECK(publisher != NULL && consumer != NULL);
    send_text(publisher, "shout");

    CHECK(mqb_consumer_poll(consumer, 1, 5000, &batch) == MQB_OK);
    CHECK(batch != NULL && mqb_batch_count(batch) == 1);
    CHECK(slice_is(mqb_message_payload(mqb_batch_at(batch, 0)), "SHOUT"));
    CHECK(atomic_load(&calls) == 1);
    mqb_batch_free(batch);
    CHECK(mqb_consumer_commit(consumer) == MQB_OK);

    CHECK(mqb_route_stop(route) == MQB_OK);
    CHECK(mqb_route_join(route) == MQB_OK);
    mqb_consumer_free(consumer);
    mqb_publisher_free(publisher);
    mqb_route_free(route);
}

static void test_request_reply(void) {
    atomic_int calls = 0;
    mqb_route_t *route = mqb_route_from_str(
        "input: { memory: { topic: smoke_rpc, request_reply: true } }\n"
        "output: { response: {} }\n",
        NULL);
    CHECK(route != NULL);
    CHECK(mqb_route_set_handler(route, upper_case, &calls) == MQB_OK);
    CHECK(mqb_route_start(route) == MQB_OK);

    mqb_publisher_t *publisher =
        mqb_publisher_from_str("memory: { topic: smoke_rpc, request_reply: true }", NULL);
    CHECK(publisher != NULL);
    mqb_message_t *request = text_message("ping");
    mqb_message_t *response = NULL;
    CHECK(mqb_publisher_request(publisher, request, &response) == MQB_OK);
    CHECK(response != NULL && slice_is(mqb_message_payload(response), "PING"));
    mqb_message_free(response);
    mqb_message_free(request);
    mqb_publisher_free(publisher);
    mqb_route_free(route);
}

/* An in-process plugin: a middleware dropping "ping" and an output counting messages. */
static atomic_int delivered;

static MqbStatus drop_pings(MqbMiddlewareHandle middleware, const MqbMessage *messages,
                            size_t len, MqbFilterHandle *out_result,
                            const MqbMessage **out_messages, const uint8_t **out_kept,
                            MqbBuffer *err) {
    uint8_t *kept = malloc(len + 1);
    if (kept == NULL) {
        mqb_set_error(err, "out of memory");
        return MQB_ERR_RETRYABLE;
    }
    for (size_t i = 0; i < len; i++) {
        MqbSlice p = messages[i].payload;
        kept[i] = p.len == 4 && memcmp(p.ptr, "ping", 4) == 0 ? MQB_MESSAGE_DROPPED
                                                                : MQB_MESSAGE_KEPT;
    }
    *out_result = kept;
    *out_messages = messages;
    *out_kept = kept;
    return MQB_OK;
}

static MqbStatus counter_create(MqbFactoryHandle factory, MqbSlice route_name,
                                MqbSlice config_json, MqbPublisherHandle *out, MqbBuffer *err) {
    *out = &delivered;
    return MQB_OK;
}

static MqbStatus counter_send(MqbPublisherHandle publisher, const MqbMessage *messages,
                              size_t len, MqbBuffer *err) {
    atomic_fetch_add((atomic_int *)publisher, (int)len);
    return MQB_OK;
}

static MqbStatus counter_done(MqbPublisherHandle publisher, MqbBuffer *err) { return MQB_OK; }

static const MqbPluginVTable smoke_plugin = {
    MQB_TABLE_HEADER("c_smoke", "0.1.0", MQB_CAP_MIDDLEWARE | MQB_CAP_PUBLISHER),
    MQB_DEFAULT_FACTORY,
    MQB_NO_CONSUMER,
    MQB_STATELESS_MIDDLEWARE,
    .middleware_apply = drop_pings,
    .middleware_result_free = free,
    MQB_BLOCKING_PUBLISHER,
    .publisher_create = counter_create,
    .publisher_send_batch = counter_send,
    .publisher_flush = counter_done,
    .publisher_close = counter_done,
    .publisher_free = mqb_stub_free,
};

static void test_in_process_plugin(void) {
    CHECK(mqb_register_plugin(&smoke_plugin) == MQB_OK);
    CHECK(mqb_register_plugin(&smoke_plugin) != MQB_OK);
    CHECK(mqb_register_plugin(NULL) != MQB_OK);

    mqb_route_t *route = mqb_route_from_str(
        "input:\n"
        "  memory: { topic: smoke_plugin_in }\n"
        "  middlewares:\n"
        "    - custom: { name: c_smoke, config: {} }\n"
        "output:\n"
        "  custom: { name: c_smoke, config: {} }\n",
        NULL);
    CHECK(route != NULL);
    CHECK(mqb_route_start(route) == MQB_OK);

    mqb_publisher_t *publisher =
        mqb_publisher_from_str("memory: { topic: smoke_plugin_in }", NULL);
    CHECK(publisher != NULL);
    send_text(publisher, "ping");
    send_text(publisher, "data");
    send_text(publisher, "more");
    for (int waited = 0; atomic_load(&delivered) < 2 && waited < 5000; waited += 10) {
        sleep_ms(10);
    }
    CHECK(atomic_load(&delivered) == 2);

    mqb_publisher_free(publisher);
    mqb_route_free(route);
}

/* A CSV file through a `transform` middleware into JSON lines. */
static void test_csv_to_json(const char *dir) {
    char csv[512], json[512], config[2048], line[256];
    snprintf(csv, sizeof csv, "%s/smoke_in.csv", dir);
    snprintf(json, sizeof json, "%s/smoke_out.jsonl", dir);
    remove(json);
    FILE *file = fopen(csv, "w");
    CHECK(file != NULL);
    fputs("user_id,city,internal\n1,Berlin,x\n2,Lisbon,y\n", file);
    fclose(file);

    snprintf(config, sizeof config,
             "input:\n"
             "  file: { path: \"%s\", format: csv }\n"
             "  middlewares:\n"
             "    - transform:\n"
             "        mapping: { id: \"$.user_id\", city: \"$.city\" }\n"
             "        schema: { type: object, properties: { id: { type: integer } } }\n"
             "output:\n"
             "  file: { path: \"%s\", format: raw }\n",
             csv, json);
    mqb_route_t *route = mqb_route_from_str(config, NULL);
    CHECK(route != NULL);
    CHECK(mqb_route_start(route) == MQB_OK);

    int rows = 0;
    for (int waited = 0; rows < 2 && waited < 5000; waited += 10) {
        sleep_ms(10);
        rows = 0;
        file = fopen(json, "r");
        while (file != NULL && fgets(line, sizeof line, file) != NULL) {
            const char *city = rows == 0 ? "\"city\":\"Berlin\"" : "\"city\":\"Lisbon\"";
            const char *id = rows == 0 ? "\"id\":1" : "\"id\":2";
            if (strchr(line, '\n') == NULL) {
                break;
            }
            CHECK(strstr(line, city) != NULL && strstr(line, id) != NULL);
            CHECK(strstr(line, "internal") == NULL && strstr(line, "user_id") == NULL);
            rows++;
        }
        if (file != NULL) {
            fclose(file);
        }
    }
    CHECK(rows == 2);
    mqb_route_free(route);
    remove(csv);
    remove(json);
}

static atomic_int log_events;

static void count_log(const char *level, const char *target, const char *message,
                      void *user_data) {
    atomic_fetch_add((atomic_int *)user_data, level != NULL && target != NULL && message != NULL);
}

int main(int argc, char **argv) {
    CHECK(mqb_init_logging(count_log, &log_events, "debug") == MQB_OK);
    CHECK(mqb_init_logging(count_log, &log_events, "debug") != MQB_OK);
    test_versions();
    test_bad_config();
    test_message();
    test_publish_and_poll();
    test_route_with_handler();
    test_request_reply();
    test_in_process_plugin();
    test_csv_to_json(argc > 1 ? argv[1] : "/tmp");
    CHECK(atomic_load(&log_events) > 0);
    CHECK(!mqb_is_shutdown_requested());
    CHECK(mqb_request_shutdown());
    CHECK(mqb_is_shutdown_requested());
    puts("mq_bridge.h smoke test passed");
    return 0;
}
