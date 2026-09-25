#include "legacy_ledger.h"

#include <stdio.h>
#include <stdlib.h>

struct legacy_ledger {
    FILE *file;
};

legacy_ledger *legacy_ledger_open(const char *path) {
    legacy_ledger *ledger = malloc(sizeof(*ledger));
    if (ledger == NULL) {
        return NULL;
    }
    ledger->file = fopen(path, "ab");
    if (ledger->file == NULL) {
        free(ledger);
        return NULL;
    }
    return ledger;
}

int legacy_ledger_append(legacy_ledger *ledger, const void *line, size_t len) {
    if (fwrite(line, 1, len, ledger->file) != len || fputc('\n', ledger->file) == EOF) {
        return -1;
    }
    return 0;
}

int legacy_ledger_flush(legacy_ledger *ledger) { return fflush(ledger->file) == 0 ? 0 : -1; }

void legacy_ledger_close(legacy_ledger *ledger) {
    fclose(ledger->file);
    free(ledger);
}
