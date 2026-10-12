# Vendored from faucet-stream — Postgres CDC / WAL decoding

The pgoutput logical-decoding stack and the LSN/replication helpers in this
directory were **ported from [faucet-stream]**, not written from scratch. This
file pins the exact upstream revision we synced against and records what we
changed, so an upstream fix can be diffed in later.

[faucet-stream]: https://github.com/PawanSikawat/faucet-stream

## Upstream pin

| | |
|---|---|
| Repo | `github.com/PawanSikawat/faucet-stream` |
| License | dual **Apache-2.0 OR MIT** (preserved) |
| Synced commit | **`3aad80a416e68778b43457aa98075d451d262b64`** (repo HEAD as of 2026-07-11, our copy date) |
| Upstream path | `crates/source/postgres-cdc/src/` |
| Last upstream change to `pgoutput/` at the pin | `19c4ee1de0201364dcaef9e11e3087305e12bd57` (2026-06-10) |

> The pin is the repo-wide HEAD at copy time. The `pgoutput/` subtree itself was
> unchanged upstream since `19c4ee1`, so either sha identifies the same tree for
> those files.

## File map

| ours (`src/endpoints/postgres/`) | upstream (`crates/source/postgres-cdc/src/`) | delta vs. upstream¹ |
|---|---|---|
| `pgoutput/decoder.rs`  | `pgoutput/decoder.rs`  | +58 / −41  — mechanical |
| `pgoutput/messages.rs` | `pgoutput/messages.rs` | +16 / −9   — mechanical |
| `pgoutput/registry.rs` | `pgoutput/registry.rs` | +11 / −7   — small feature add |
| `pgoutput/values.rs`   | `pgoutput/values.rs`   | +117 / −55 — behavioral adaptations |
| `pgoutput/mod.rs`      | `pgoutput/mod.rs`      | +100 / −1  — mostly new local code |
| `replication.rs`       | `replication.rs`       | +155 / −696 — heavy rewrite |
| `state.rs`             | `state.rs`             | +30 / −123  — thinned to LSN helpers |

¹ `diff -u` add/remove line counts at the pin, including doc-comment edits.

## Changes we performed

**All files:** upstream's `faucet_core::FaucetError` was replaced with
`anyhow`, and the module doc headers were rewritten to attribute the port. These
are the bulk of the mechanical diffs.

- **`pgoutput/decoder.rs`** — `FaucetError` → `anyhow` only; decode logic is
  byte-for-byte the upstream algorithm. `XLogDataHeader` and the keepalive
  decoder are marked `#[allow(dead_code)]`: `pgwire-replication` strips the
  XLogData framing and answers keepalives itself, so those paths are unused but
  retained to keep the port a complete, fixture-tested pgoutput decoder.
- **`pgoutput/messages.rs`** — error type only; message structs unchanged.
- **`pgoutput/registry.rs`** — error type, plus a **local addition**: a re-sent
  `Relation` with a changed column set (mid-stream `ALTER TABLE`) now logs a
  warning so a same-arity rename/type change can be correlated downstream.
- **`pgoutput/values.rs`** — error type, plus **behavioral adaptations** to the
  OID→JSON text mapping: a `bytea_unescape` fallback for the non-`\x` bytea text
  form, and let-else → `match` rewrites (control flow only). Type coverage
  otherwise follows upstream.
- **`pgoutput/mod.rs`** — upstream is only module declarations (7 lines). We
  added `tuple_to_json_object`, the flat-row JSON projection that keys cells by
  column name and **omits** unchanged-TOAST cells. This file is essentially our
  own code on top of the upstream module layout.
- **`replication.rs`** — we kept upstream's **approach** for URL parsing, slot
  lifecycle (`CREATE`/`ADVANCE`/`DROP`), and TLS mapping, but replaced its
  streaming engine with `pgwire-replication` (wire protocol) + `sqlx` (slot
  control plane). ~696 upstream lines dropped.
- **`state.rs`** — reduced to `format_lsn` / `parse_lsn` (ported verbatim in
  spirit); upstream's broader checkpoint/state machinery was dropped in favor of
  mq-bridge's `CheckpointStore`.

## Upstream audit findings applied after the pin

Upstream fixed these in `70ca00b2`, `db56d8c6` and `f2f7942c` (audits #321, #654, #789).
They were applied by hand on 2026-10-10; the pin above is unchanged because the files were
not re-synced.

| Upstream finding | Here |
|---|---|
| Control connections ignore the TLS policy (#321 M5, SQL-87) | Already in `control_conn`. The reverse gap was ours: the stream ignored the URL's `sslmode`; fixed in `map_tls`. |
| Errors classified by message text (#654 H8, SQL-137) | `is_missing_slot_error` matches the `(SQLSTATE 42704)` suffix. |
| URL not percent-decoded, query overrides ignored (SQL-86) | `parse_url`. |
| Slot advance to a position behind the slot fails (SQL-154) | `advance_slot` skips it. |
| Temporary slot is gone before the stream starts (SQL-47) | Not affected: `temporary_slot` is drop-on-stop. |
| Slow sink lets `wal_sender_timeout` end the stream (SQL-88) | Not affected: `pgwire-replication` 0.4 sends status updates while its event channel is full. |
| Cycle bound, commit batching (SQL-08, SQL-118) | Not applicable: the consumer confirms continuously. Empty commits and keepalives confirm while idle. |
| Temporal values depend on `DateStyle` (SQL-119) | Not taken: the text stays as Postgres prints it. The stream sets `datestyle=ISO` on its own session (`STREAM_SESSION_OPTIONS`), so the role's setting cannot change it. |
| PostgreSQL 13 rejects the `messages` option (SQL-153) | Documented: 14 or newer. |

## Re-syncing an upstream fix

1. Note the current pin above (`OLD`) and pick the upstream commit with the fix
   (`NEW`).
2. `git clone` faucet-stream (or use the GitHub raw API) and diff the relevant
   file between `OLD..NEW`:
   `git diff OLD NEW -- crates/source/postgres-cdc/src/pgoutput/<file>.rs`
3. Apply the hunks by hand, re-mapping `FaucetError` → `anyhow` and skipping any
   dead-code paths we don't use (XLogData framing / keepalives).
4. Update the **Synced commit** in this file to `NEW`.

Mechanical files (`decoder.rs`, `messages.rs`) re-sync cleanly. `replication.rs`
and `state.rs` diverge structurally — port fixes by intent, not by patch.
