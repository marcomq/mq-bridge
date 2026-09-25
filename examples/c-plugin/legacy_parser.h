/*
 * Stand-in for an existing C library: it knows nothing about mq-bridge.
 *
 * Parses a fixed-width payment record of exactly 25 bytes:
 *   ACCOUNT   10  alphanumeric, right-padded with spaces
 *   AMOUNT    12  digits, minor units (cents)
 *   CURRENCY   3  uppercase ISO 4217 code
 */
#ifndef LEGACY_PARSER_H
#define LEGACY_PARSER_H

#include <stddef.h>
#include <stdint.h>

#define LEGACY_RECORD_LEN 25

typedef struct {
    char account[11];
    uint64_t amount_minor;
    char currency[4];
} legacy_record;

/* Returns NULL on success, otherwise a static description of the problem. */
const char *legacy_parse(const uint8_t *data, size_t len, legacy_record *out);

#endif
