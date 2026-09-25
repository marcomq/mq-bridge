/* Stand-in for an existing C library: appends lines to a ledger file. Not thread-safe. */
#ifndef LEGACY_LEDGER_H
#define LEGACY_LEDGER_H

#include <stddef.h>

typedef struct legacy_ledger legacy_ledger;

legacy_ledger *legacy_ledger_open(const char *path);
int legacy_ledger_append(legacy_ledger *ledger, const void *line, size_t len);
int legacy_ledger_flush(legacy_ledger *ledger);
void legacy_ledger_close(legacy_ledger *ledger);

#endif
