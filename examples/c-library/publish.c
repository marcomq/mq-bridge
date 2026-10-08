/* Publishes one message to the endpoint in a config file.
 *
 *   ./publish publisher.yaml [name] < payload
 *
 * The file holds one endpoint, e.g. `nats: { url: "nats://localhost:4222", subject: orders }`,
 * or a `publishers:` document, in which case `name` picks one. */
#include <stdio.h>

#include "mq_bridge.h"

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s publisher.yaml [name] < payload\n", argv[0]);
        return 2;
    }
    if (mqb_api_version() != MQB_API_VERSION) {
        fprintf(stderr, "mq_bridge.h and the mq_bridge library are from different releases\n");
        return 2;
    }

    uint8_t payload[65536];
    size_t len = fread(payload, 1, sizeof(payload), stdin);

    mqb_publisher_t *publisher = mqb_publisher_from_file(argv[1], argc > 2 ? argv[2] : NULL);
    if (publisher == NULL) {
        fprintf(stderr, "could not open the publisher: %s\n", mqb_last_error());
        return 1;
    }

    mqb_message_t *message = mqb_message_new(payload, len);
    mqb_message_set_metadata(message, "kind", "example");
    MqbStatus status = mqb_publisher_send(publisher, message);
    if (status != MQB_OK) {
        fprintf(stderr, "send failed: %s\n", mqb_last_error());
    }

    mqb_message_free(message);
    mqb_publisher_free(publisher);
    return status == MQB_OK ? 0 : 1;
}
