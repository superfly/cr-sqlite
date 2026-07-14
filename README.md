# cr-sqlite (Fly.io Fork)

A [run-time loadable extension](https://www.sqlite.org/loadext.html) for [SQLite](https://www.sqlite.org/index.html) that adds multi-master replication via conflict-free replicated data types (CRDTs). This is a fork of [vlcn-io/cr-sqlite](https://github.com/vlcn-io/cr-sqlite), maintained by [Fly.io](https://fly.io) and used as the replication engine for [Corrosion](https://github.com/superfly/corrosion), a distributed SQLite database.

## Relationship to Upstream

This fork diverges from upstream cr-sqlite v0.15.0 and introduces significant, breaking changes to the change bookkeeping model. The current version is **0.17.0**. Databases created with cr-sqlite < 0.17.0 are not supported and must be migrated.

The core CRDT approach (history-free, last-write-wins per column, causal length sets) remains the same. The differences are in **how changes are tracked, timestamped, and replicated**.

## Key Changes from Upstream

### Per-Site DB Version Tracking

Upstream cr-sqlite maintains a single global `db_version` counter. This fork introduces a `crsql_db_versions` table that tracks the latest `db_version` seen from **each site** (actor):

```sql
CREATE TABLE crsql_db_versions (site_id BLOB NOT NULL PRIMARY KEY, db_version INTEGER NOT NULL);
```

Unlike upstream cr-sqlite, which provides some built-in peer tracking via `crsql_tracked_peers`, this fork expects the **application** to handle all bookkeeping — gap detection, seq tracking, and buffering — outside the extension. The extension only tracks the latest `db_version` seen per site in `crsql_db_versions`. Corrosion implements this bookkeeping with:
- `__corro_bookkeeping_gaps` — tracks missing db_version ranges per peer
- `__corro_seq_bookkeeping` — tracks which seq ranges have been received within each db_version
- `__corro_buffered_changes` — buffers partial transactions (individual changes from a db_version whose seqs haven't all arrived yet) until the full transaction can be applied

New SQL functions:

- **`crsql_peek_next_db_version()`** — Returns the next db_version without incrementing it. Used to inspect what the next version will be before writes happen.
- **`crsql_set_db_version(site_id, db_version)`** — Sets the db_version for a specific remote site. Used when applying changes from a peer to record how far we've synced from that peer, even if no changes won the merge.

The `crsql_next_db_version()` function no longer accepts a `merging_version` argument (breaking change). The db_version is now committed to storage immediately when computed, rather than only at transaction commit.

#### Important: seqs can disappear from a db_version

`db_version`s are monotonic per peer, and site_id ordinal 0 is always the local node. The clock table uses `PRIMARY KEY (key, col_name)`, so when a newer write updates the same column of the same row, it **replaces** the old clock entry — the old `db_version` and `seq` for that column are gone.

This means that when querying `crsql_changes WHERE db_version = X AND site_id = Y`, some seqs may be missing even though the version was fully written. For example, if db_version X originally produced seqs 0-10 for site_id Y, but a newer write superseded the value at (X, 5), querying for db_version X will return seqs 0-4 and 6-10 — seq 5 is gone because it got supersceded by a newer write (the extension is history-free, so old clock entries are not retained).

Applications must treat a version as **complete** even when some seqs are missing, since those seqs were superseded by newer writes. In the extreme case, all seqs in a db_version can be missing — the version is effectively empty because every column it touched has since been overwritten. Corrosion handles this by sending metadata indicating whether a given version is fully complete (no missing seqs due to network fragmentation).

### Timestamps (`ts` column)

Every change record now carries a timestamp. A `ts TEXT NOT NULL DEFAULT '0'` column has been added to all `__crsql_clock` tables and to the `crsql_changes` virtual table. The timestamp is set per-transaction via a new SQL function:

```sql
SELECT crsql_set_ts('1719878400000');
-- subsequent writes in this transaction will record this timestamp
INSERT INTO foo VALUES (1, 'bar');
```

The timestamp is stored as a string representation of a `u64`. The value itself is an NTP64 timestamp — in Corrosion, this is the physical-time component extracted from a [Hybrid Logical Clock](https://www.cse.iitb.ac.in/~br/publications/2014-saurabh-mtech-thesis.pdf) (`uhlc::HLC`), which combines wall-clock time with a logical counter to maintain causal ordering while staying close to real time. This enables time-based retention policies — the Corrosion reaper uses `ts` to garbage-collect tombstones (delete sentinel rows) older than a configurable retention period.

The current timestamp for a transaction can be read with `crsql_get_ts()`.

### In-Memory Caching

This fork introduces several in-memory caches to avoid repeated lookups during merges and local writes. All caches are scoped to a transaction and cleared on commit/rollback (including savepoint rollback via `xRollbackTo`).

- **Site ordinal cache**: A `BTreeMap<site_id, ordinal>` on `ExtData` avoids repeated lookups to `crsql_site_id` during merge operations. Triggers on the `crsql_site_id` table keep this cache in sync when rows are inserted, updated, or deleted directly — a new `crsql_update_site_id(site_id, ordinal)` function updates the `BTreeMap` in-memory (it does not write to persistent storage; the `crsql_site_id` table is the source of truth). This allows external tooling (e.g., Corrosion's `corro-admin`) to manipulate site IDs directly in the table without reloading the extension.
- **Causal length cache**: Causal lengths (cl) are cached per-transaction in a `BTreeMap` on each `TableInfo`, with a configurable max size (currently 1500 entries). This avoids repeated lookups to the clock table for the sentinel row during merges and local writes.
- **Last db versions map**: A `BTreeMap<site_id, db_version>` on `ExtData` tracks the highest db_version inserted per site during merge transactions, avoiding redundant writes to `crsql_db_versions`.

Proper SQLite commit and rollback hooks are registered to manage the cache lifecycle. On commit, `pendingDbVersion` becomes `dbVersion`. On rollback, all caches are cleared.

### Clock Table Schema Changes

The `__crsql_clock` table schema is now:

```sql
CREATE TABLE "table_name__crsql_clock" (
  key INTEGER NOT NULL,
  col_name TEXT NOT NULL,
  col_version INTEGER NOT NULL,
  db_version INTEGER NOT NULL,
  site_id INTEGER NOT NULL DEFAULT 0,
  seq INTEGER NOT NULL,
  ts TEXT NOT NULL DEFAULT '0',
  PRIMARY KEY (key, col_name)
) WITHOUT ROWID, STRICT;
```

The db_version index has changed from `(db_version)` to `(site_id, db_version)` to optimize per-site change queries.

### Optimized Local Write Path

Local writes (insert/update/delete triggers) have been significantly reworked:

- **Insert**: Uses a new `mark_locally_inserted` function that tries an `UPDATE` on clock rows first, then falls back to `INSERT` only for columns where the update was a no-op (detected via `sqlite3_changes64()`). A combo-insert fast path batches all column inserts into a single statement when none of the updates hit existing rows.
- **Update**: Uses `peek_next_db_version()` to avoid incrementing the version if nothing actually changed. Only calls `next_db_version()` (which writes to storage) if at least one column value differed.
- **Delete**: The `mark_locally_deleted` statement now returns the new causal length via `RETURNING`, which is cached in the cl cache.
- **PK change**: When a primary key changes during an update, non-sentinel clock rows are moved from the old key to the new key via `UPDATE OR REPLACE`, preserving their col_version (so they can override values at downstream nodes).

### Merge Write Path Changes

- `set_winner_clock` now takes an `insert_ts` parameter and binds it to the clock table.
- `zero_clocks_on_resurrect` no longer sets `db_version` during resurrect (only zeroes `col_version`).
- `merge_sentinel_only_insert` now accepts and binds `remote_ts`.
- After any merge (win or lose), `insert_db_version` is called to update `crsql_db_versions` for the remote site.
- The merge code is restructured to handle all three cases (sentinel-only, resurrect, normal) in a unified path.

### New SQL Functions

| Function | Description |
|---|---|
| `crsql_set_ts(ts)` | Set the timestamp for the current transaction (string u64) |
| `crsql_get_ts()` | Get the current transaction's timestamp |
| `crsql_peek_next_db_version()` | Peek at the next db_version without incrementing |
| `crsql_set_db_version(site_id, db_version)` | Set the db_version for a specific site |
| `crsql_set_debug(enabled)` | Enable/disable debug logging |
| `crsql_version()` | Return the cr-sqlite version integer (re-enabled; was commented out upstream) |

### Other Changes

- **Debug logging**: A `crsql_set_debug(1)` function enables `libc_print`-based debug output.
- **ASAN support**: Added `make asan` target with proper Rust sanitizer flags.
- **Config lifetime fix**: `crsql_config_set` now properly manages the statement lifetime to prevent use-after-free of returned values.
- **`crsql_changes` schema**: The `crsql_changes` virtual table now includes a `ts` column (column index 9).

## Usage

```sql
-- load the extension
.load crsqlite
.mode qbox

-- create tables as normal
CREATE TABLE foo (a PRIMARY KEY NOT NULL, b);
CREATE TABLE baz (a PRIMARY KEY NOT NULL, b, c, d);

-- upgrade tables to be CRRs (conflict-free replicated relations)
SELECT crsql_as_crr('foo');
SELECT crsql_as_crr('baz');

-- optionally set a timestamp for this transaction
SELECT crsql_set_ts('1719878400000');

-- insert data as normal
INSERT INTO foo (a, b) VALUES (1, 2);
INSERT INTO baz (a, b, c, d) VALUES ('a', 'woo', 'doo', 'daa');

-- fetch changes (note the ts column)
SELECT "table", "pk", "cid", "val", "col_version", "db_version", "site_id", "cl", "seq", "ts"
  FROM crsql_changes
  WHERE db_version > 0 AND site_id = crsql_site_id();

-- apply changes from a peer
INSERT INTO crsql_changes
  ("table", "pk", "cid", "val", "col_version", "db_version", "site_id", "cl", "seq", "ts")
  VALUES
  ('foo', x'010905', 'b', 'thing', 5, 5, X'7096E2D505314699A59C95FABA14ABB5', 1, 0, '1719878400000');

-- tear down before closing the connection
SELECT crsql_finalize();
```

### Altering CRR Tables

```sql
SELECT crsql_begin_alter('table_name');
-- 1 or more alterations
ALTER TABLE table_name ...;
SELECT crsql_commit_alter('table_name');
```

## How It Is Used in Corrosion

[Corrosion](https://github.com/superfly/corrosion) is a distributed, eventually-consistent SQLite database. It embeds cr-sqlite as a loadable extension (with pre-built binaries bundled for darwin-aarch64, linux-x86_64, and linux-aarch64) and builds a full clustering layer on top:

- **Actor identity**: Corrosion uses `crsql_site_id()` as the actor ID in its cluster. The `crsql_site_id` table (with ordinals) maps to Corrosion's actor tracking.
- **Change extraction**: Corrosion queries `crsql_changes` filtered by `db_version` and `site_id` to extract changesets for broadcast to peers. It uses `crsql_peek_next_db_version()` to determine the version before writes are committed, then queries `MAX(seq)` and `MAX(ts)` to track the last sequence and timestamp per version.
- **Change application**: Remote changesets are inserted into `crsql_changes` in bulk (via `unnest` for batch inserts). Corrosion uses `crsql_rows_impacted()` to verify that merges actually affected rows.
- **Timestamps**: Corrosion uses a Hybrid Logical Clock (HLC) and calls `crsql_set_ts()` before each transaction to stamp changes with an NTP64 timestamp. This enables the reaper to garbage-collect tombstones based on age.
- **Per-site version tracking**: Corrosion uses the `crsql_db_versions` table to track the latest `db_version` received from each peer. When processing incomplete or buffered changes, it calls `crsql_set_db_version(site_id, version)` to advance the tracked version even when no changes won the merge.
- **Schema management**: Corrosion uses `crsql_as_crr()` to register tables and `crsql_begin_alter()` / `crsql_commit_alter()` for schema migrations. When migrating from older cr-sqlite versions that lacked the `(site_id, db_version)` index on clock tables, it creates `corro_{table}__crsql_clock_site_id_dbv` indexes.
- **Buffered changes & gap tracking**: Corrosion handles two kinds of gaps. Missing **db_versions** from a peer are tracked in `__corro_bookkeeping_gaps`. Partial transactions (a db_version split across multiple messages with missing **seqs**) are buffered in `__corro_buffered_changes` (same schema as `crsql_changes`), with received seq ranges tracked in `__corro_seq_bookkeeping`. Once all seqs for a db_version are received, the changes are applied to `crsql_changes` in bulk.
- **Reaper**: A background reaper process uses the `ts` column to find tombstones (delete sentinel rows where `col_name = -1 AND col_version % 2 = 0 AND ts < cutoff`) and cleans them up along with orphaned entries in `__crsql_clock` and `__crsql_pks`.
- **Config**: Corrosion enables `crsql_config_set('merge-equal-values', 1)` to optimize merges where values are equal.
- **Migration**: Corrosion includes a `crsqlite_v0_17_migration` that adds the `ts` column to existing clock tables and recreates indexes.

## Building

You'll need Rust (nightly toolchain required).

### Run Time Loadable Extension

```bash
rustup toolchain install nightly
git clone --recurse-submodules git@github.com:superfly/cr-sqlite.git
cd cr-sqlite/core
make loadable
```

This creates a shared library at `dist/crsqlite.[so|dylib|dll]`.

> Note: loading the extension should be the _first_ operation after opening a connection. The extension must be loaded on every connection.

### CLI (statically linked sqlite3)

```bash
cd core
make sqlite3
```

Creates `dist/sqlite3` with cr-sqlite statically linked and pre-loaded.

### Tests

C tests:

```bash
cd core
make test
```

Python integration tests:

```bash
cd core
make loadable
cd ../py/correctness
./install-and-test.sh
```

### Performance Benchmarking

A Rust-based benchmarking tool is in `tools/`:

```bash
cd tools
cargo run -- ../core/dist/crsqlite
```

## API Reference

### Core Functions

| Function | Description |
|---|---|
| `crsql_as_crr('table')` | Upgrade a table to a conflict-free replicated relation |
| `crsql_as_table('table')` | Alias for `crsql_as_crr` |
| `crsql_site_id()` | Return this database's 16-byte site ID |
| `crsql_db_version()` | Return the current db_version |
| `crsql_next_db_version()` | Return and persist the next db_version |
| `crsql_peek_next_db_version()` | Peek at the next db_version without persisting |
| `crsql_set_db_version(site_id, version)` | Set the db_version for a specific site |
| `crsql_set_ts(ts)` | Set the timestamp for the current transaction |
| `crsql_get_ts()` | Get the current transaction's timestamp |
| `crsql_rows_impacted()` | Return rows impacted by the last merge insert |
| `crsql_finalize()` | Tear down the extension (call before closing connection) |
| `crsql_version()` | Return the cr-sqlite version integer |
| `crsql_config_set(name, value)` | Set a configuration option |
| `crsql_set_debug(enabled)` | Enable/disable debug logging |

### Schema Alter Functions

| Function | Description |
|---|---|
| `crsql_begin_alter('table')` | Begin an alter session on a CRR table |
| `crsql_commit_alter('table')` | Commit alterations to a CRR table |

### Virtual Tables

- **`crsql_changes`** — Query and apply changesets. Columns: `table, pk, cid, val, col_version, db_version, site_id, cl, seq, ts`
- **`crsql_site_id`** — Maps site IDs to ordinals (ordinal 0 is the local site)

### Internal Tables (per CRR table)

- **`{table}__crsql_clock`** — Per-column clock metadata (col_version, db_version, site_id, seq, ts)
- **`{table}__crsql_pks`** — Maps primary key values to integer keys for clock table lookups

### Global Tables

- **`crsql_db_versions`** — Per-site db_version tracking
- **`crsql_master`** — Extension metadata and config key-value store
- **`crsql_tracked_peers`** — Peer tracking table (site_id, version, seq, tag, event). Created by the extension but bookkeeping is expected to be handled by the application.

## How It Works

CR-SQLite uses history-free CRDTs based on [causal length sets](https://dl.acm.org/doi/pdf/10.1145/3380787.3393678). Each table upgraded to a CRR gets:

1. **Clock tables** (`__crsql_clock`) that track per-column version metadata (col_version, db_version, site_id, seq, ts)
2. **PK lookaside tables** (`__crsql_pks`) that map composite primary keys to integer keys for efficient clock table joins
3. **Triggers** (insert, update, delete) that automatically record changes to the clock tables

Merging works by comparing column versions and causal lengths. For each incoming change:
- If the incoming `col_version` is greater than the local one, the change wins
- If versions are equal, the column values are compared
- If values are equal, the `site_id` is used as a tiebreaker
- Deletes are tracked via sentinel rows (tombstones) with even `col_version` values; the causal length determines whether a delete or a create wins

The `crsql_changes` virtual table provides a unified view across all CRR clock tables, allowing you to extract and apply changesets without knowing the underlying schema.

## Research & Prior Art

- [Towards a General Database Management System of Conflict-Free Replicated Relations](https://munin.uit.no/bitstream/handle/10037/22344/thesis.pdf?sequence=2)
- [Conflict-Free Replicated Relations for Multi-Synchronous Database Management at Edge](https://hal.inria.fr/hal-02983557/document)
- [Merkle-CRDTs](https://arxiv.org/pdf/2004.00107.pdf)
- [Time, Clocks, and the Ordering of Events in a Distributed System](https://lamport.azurewebsites.net/pubs/time-clocks.pdf)
- [Replicated abstract data types: Building blocks for collaborative applications](http://csl.skku.edu/papers/jpdc11.pdf)
- [CRDTs for Brrr](https://josephg.com/blog/crdts-go-brrr/)
