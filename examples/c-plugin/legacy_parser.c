#include "legacy_parser.h"

#include <ctype.h>
#include <string.h>

const char *legacy_parse(const uint8_t *data, size_t len, legacy_record *out) {
    if (len != LEGACY_RECORD_LEN) {
        return "record must be exactly 25 bytes";
    }

    size_t account_len = 10;
    while (account_len > 0 && data[account_len - 1] == ' ') {
        account_len--;
    }
    if (account_len == 0) {
        return "account is empty";
    }
    for (size_t i = 0; i < account_len; i++) {
        if (!isalnum(data[i])) {
            return "account must be alphanumeric";
        }
    }
    memcpy(out->account, data, account_len);
    out->account[account_len] = '\0';

    out->amount_minor = 0;
    for (size_t i = 10; i < 22; i++) {
        if (!isdigit(data[i])) {
            return "amount must be 12 digits";
        }
        out->amount_minor = out->amount_minor * 10 + (uint64_t)(data[i] - '0');
    }

    for (size_t i = 22; i < 25; i++) {
        if (!isupper(data[i])) {
            return "currency must be 3 uppercase letters";
        }
        out->currency[i - 22] = (char)data[i];
    }
    out->currency[3] = '\0';
    return NULL;
}
