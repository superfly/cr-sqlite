# cr-sqlite (Fly.io Fork)

A [run-time loadable extension](https://www.sqlite.org/loadext.html) for [SQLite](https://www.sqlite.org/index.html) that adds multi-master replication via conflict-free replicated data types (CRDTs). This is a fork of [vlcn-io/cr-sqlite](https://github.com/vlcn-io/cr-sqlite), maintained by [Fly.io](https://fly.io) and used as the replication engine for [Corrosion](https://github.com/superfly/corrosion), a distributed SQLite database.

## Relationship to Upstream

This fork diverges from upstream cr-sqlite v0.15.0 and introduces significant, breaking changes to the change bookkeeping model and underlying data format. The current version is **0.18.0**. Databases created with cr-sqlite < 0.17.0 are not supported and must be migrated. Databases on 0.17.0 can incrementally migrate to the new V2 metadata format (see [Migration Guide](#migration-guide-017--018)).

**0.19.0 will remove V1 support entirely. V2 schema may receive breaking changes during the 0.18.x series.**

The core CRDT approach (history-free, last-write-wins per column, causal length sets) remains the same. The differences are in **how changes are tracked, timestamped, and replicated**.

## Key Changes in 0.17.0

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

## What's New in 0.18.0

0.18.0 introduces a new V2 metadata format and V2 wire format for change tracking. These are opt-in in 0.18.0. The V2 schema may receive breaking changes during the 0.18.x series. V1 support will be removed in 0.19.0. For full design details, see [`v2_metadata_design.md`](./v2_metadata_design.md).

### Mandatory `crsql_set_ts()` Before All Write Operations

Starting in 0.18.0, **`crsql_set_ts()` must be called in the same transaction** before any operation that writes to clock tables or metadata. This includes:

- **`crsql_as_crr()`** — creating a new CRR
- **`crsql_begin_alter()` / `crsql_commit_alter()`** — altering CRR schema
- **`crsql_incremental_maintenance()`** — V1→V2 migration batches
- **`INSERT`/`UPDATE`/`DELETE` on CRR tables** — local writes (via triggers)
- **`INSERT INTO crsql_changes`** — applying remote changes (merges)

If the timestamp is not set (or was reset by a prior transaction commit), these operations will fail with an error:

```
crsql_as_crr: timestamp not set — call crsql_set_ts() first or set default-ts
```

The timestamp is **transaction-scoped**: it is set via `crsql_set_ts()` and resets to 0 on transaction commit or rollback. This means `crsql_set_ts()` and the operation that needs it must run in the same transaction. In autocommit mode, `SELECT crsql_set_ts(...)` does not trigger a commit (it's a read-only function call), so a subsequent `SELECT crsql_as_crr(...)` in a separate `exec` call will still see the timestamp — but the `crsql_as_crr` call itself writes to the database, which triggers an auto-commit and resets the timestamp for the next operation.

To skip calling `crsql_set_ts()` before every command, set a default:

```sql
SELECT crsql_config_set('default-ts', 1);
-- now as_crr / inserts / alter work without crsql_set_ts()
SELECT crsql_as_crr('foo');
INSERT INTO foo VALUES (1, 'bar');
```

`default-ts` is `0` by default (must still call `crsql_set_ts()`). Any value `> 0` is used for the current transaction when `crsql_set_ts()` was not called. An explicit `crsql_set_ts()` still wins for that transaction. The setting is persisted in `crsql_master`.

**Only plain `SELECT` queries from `crsql_changes`** (reading changes without merging) do not require a timestamp to be set.

Example:

```sql
-- Set timestamp before each transaction that writes to CRRs
SELECT crsql_set_ts('1719878400000');
SELECT crsql_as_crr('foo');
-- ts is now reset (crsql_as_crr triggered a commit)

-- Must set again for the next write operation
SELECT crsql_set_ts('1719878400000');
INSERT INTO foo VALUES (1, 'bar');
```

### V2 Metadata Format

The V1 metadata format uses two tables per CRR: `__crsql_clock` (with `key INTEGER, col_name TEXT` as composite primary key) and `__crsql_pks` (mapping PK blobs to integer keys). This works but suffers from performance degradation at scale: self-joins are needed to retrieve the causal length (`cl`) value, deletion tombstones in the clock table slow down update operations, and the `(crsql_key, col_name)` composite PK is less compact than V2's packed integer `cell_key`.

V2 replaces this with a more compact schema:

- **`{table}__crsql_v2_pks`** — Maps hashed PK values to a `pk_key` and stores the causal length (`cl`) directly. PK values are hashed with `xxh3_128` truncated to `PK_HASH_SIZE` bytes, primarily to limit the size of sentinel and tombstone entries which accumulate over time. This also helps when PKs are larger than `PK_HASH_SIZE` (e.g., large compound primary keys).
- **`{table}__crsql_v2_clock`** — Uses a packed `cell_key = (pk_key << COL_ID_BITS) | col_id` as the primary key. Column names are mapped to small integer `col_id`s via `__crsql_v2_col_map`. Tracks per cell (column value) metadata.
- **`{table}__crsql_v2_tombstones`** — Tracks deleted rows separately, keyed by `hashed_pk`, stores the deletion causal length (`cl`).
- **`{table}__crsql_v2_tombstone_pks`** — Maps tombstone hashed PKs back to original PK column values (for V1 wire format compatibility). Once in V2-only mode entries from this table are no longer needed and can be cleaned up.

A sentinel column (`col_id = 0`) replaces the old `-1` sentinel for PK-only tables and row existence tracking. If a column is later added to a previously PK-only table, it reuses `col_id = 0` for the new column.

### V2 Wire Format

The V1 wire format produces one `crsql_changes` row per column change. If a transaction updates 5 columns of a row in the same db_version, that's 5 change rows. The V2 wire format coalesces all column changes for a row within the same db_version into a single packed row:

- `cid` — `\0`-separated packed column names (e.g., `"col1\0col2\0col3"`)
- `cval` — packed column values (binary blob)
- `col_vrsn` — per-column version numbers

This speeds up processing of operations touching multiple columns at the same time, for example when inserting a new row into a table.

### Hashed Primary Keys

V2 hashes PK values with `xxh3_128` (truncated to `PK_HASH_SIZE` bytes, currently 10) and stores them as blobs. This is primarily to limit the size of tombstone entries, which accumulate over time. V2 also moves tombstones to a dedicated `v2_tombstones` table (separate from the clock table), reducing clock table bloat from row deletions.

### Incremental Migration

V2 migration is designed to be incremental — large tables can be migrated in batches without long lock times.

**Prerequisites:**
- SQLite 3.44.0 or later (see [Minimum SQLite Version](#minimum-sqlite-version))
- Load the 0.18.0 extension — it will detect the existing 0.17.0 schema and continue operating in V1 mode. No data is lost or changed.

```sql
-- Step 1: Enable dual-write mode (new changes go to both V1 and V2 tables)
SELECT crsql_config_set('metadata-write-version', 2);

-- Step 2: Run incremental maintenance in batches until complete
-- Returns remaining rows to migrate (0 = done, -1 = error)
-- Adjust batch size based on table size and lock tolerance
SELECT crsql_incremental_maintenance(1000);
-- ... repeat until the function returns 0

-- Step 3: Switch reads to V2
-- The V1 tables still exist and are maintained but not read from
-- This way you can check that everything works correctly with V2 before dropping V1
SELECT crsql_config_set('metadata-use-version', 2);

-- Optional: Enable V2 wire format for more compact change replication
-- This packs all column changes per row per db_version into a single crsql_changes row and crsqlite_changes will return pk hashes instead of full PK values for tombstones. Requires migration to be complete and metadata-use-version set to 2 on ALL NODES in the cluster.
-- Nodes with metadata-use-version set to 1 will emit an error if they receive V2 wire format changes.
SELECT crsql_config_set('sync-log-version', 2);

-- Step 4: Once confident, drop V1 tables by switching to V2-only mode
-- This is IRREVERSIBLE. If something went wrong, roll back to 1 while still in dual-write mode (step 2).
SELECT crsql_config_set('metadata-write-version', 3);

-- Step 5: Run incremental maintenance again to clean up any remaining V1 data and tables
SELECT crsql_incremental_maintenance(1000);
-- ... repeat until the function returns 0
```

The `metadata-write-version` config has three levels:
- **1** — V1 only (default, legacy)
- **2** — Dual write (V1 + V2, migration in progress, creates V2 tables)
- **3** — V2 only (V1 tables dropped, migration complete, irreversible)

For small databases, you can migrate in one shot with a large batch size. For large production databases, use smaller batches and run periodically (e.g., in a background task) to avoid long lock times.

### Rollback (from dual-write mode only)

To roll back to V1 while still in dual-write mode (`metadata-write-version = 2`):

```sql
SELECT crsql_config_set('metadata-write-version', 1);
```

This automatically resets `metadata-use-version` and `sync-log-version` to 1 as well. V2 tables are dropped via `crsql_incremental_maintenance`. V1 data is still intact (it was kept in sync during dual-write). Rollback is not possible once `metadata-write-version = 3` has been set.

> **Note**: Once 0.19.0 is released, V1 support will be removed and rollback will not be possible. Complete the migration before upgrading to 0.19.0.

### Minimum SQLite Version

0.18.0 requires **SQLite 3.44.0 or later** (for `ORDER BY` in aggregate functions). The extension checks at load time. If loading into an external SQLite, verify with `SELECT sqlite_version();`.

### `INSERT OR REPLACE` Behavior

Starting in 0.18.0, cr-sqlite enables `PRAGMA recursive_triggers = ON` at initialization. This changes the behavior of `INSERT OR REPLACE` on CRR tables:

- **DELETE trigger fires first** — moves the old row to tombstones (CL→even, emits a delete sentinel for replication)
- **INSERT trigger fires second** — resurrects the row (CL→odd, creates fresh clock entries)

This ensures metadata consistency and correct replication — peers see both the deletion and the re-insertion. Applications using `INSERT OR REPLACE` will see delete sentinels (`cid = -1`) in `crsql_changes` for replaced rows.

- Use `INSERT ... ON CONFLICT DO UPDATE` for true upsert behavior that does not create delete sentinels.
- Use `INSERT OR REPLACE` as a forced update/recreation mode when you need to replace a row globally across all nodes.

## Usage

```sql
-- load the extension
.load crsqlite
.mode qbox

-- create tables as normal
CREATE TABLE foo (a PRIMARY KEY NOT NULL, b);
CREATE TABLE baz (a PRIMARY KEY NOT NULL, b, c, d);

-- upgrade tables to be CRRs (conflict-free replicated relations)
-- crsql_set_ts must be called before crsql_as_crr in the same transaction
SELECT crsql_set_ts('1719878400000');
SELECT crsql_as_crr('foo');
SELECT crsql_set_ts('1719878400000');
SELECT crsql_as_crr('baz');

-- set a timestamp for this transaction's writes
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
SELECT crsql_set_ts('1719878400000');
SELECT crsql_begin_alter('table_name');
-- 1 or more alterations
ALTER TABLE table_name ...;
SELECT crsql_commit_alter('table_name');
```

### Schema Directives

cr-sqlite auto-detects the optimal key strategy for each table based on its primary key schema. You can override the auto-detection using **schema directives** — special comments in the `CREATE TABLE` SQL:

```sql
CREATE TABLE my_table /* crsql: skip_hash=0, use_rowid=1 */ (
  id INTEGER PRIMARY KEY NOT NULL,
  data TEXT
);
```

Directives are parsed from `/* crsql: key=value, ... */` comments. The comment must appear **after the table name** (SQLite strips comments that appear before it in `sqlite_master`).

#### `skip_hash`

Controls whether the primary key is hashed before storage in the internal `v2_pks` and `v2_tombstones` tables. It is highly recommended to keep keys hashed for large PKs (e.g. UUIDs, long strings) as it limits tombstone growth — unhashed tombstones store the full PK value, which can bloat storage significantly.

| Value | Behavior |
|---|---|
| `skip_hash=1` | Store the PK value directly (no hash). Faster lookups, but requires a single-column PK (any type). |
| `skip_hash=0` | Hash the PK before storage. Works with any PK type. |
| *(absent)* | **Auto-detect**: `skip_hash=1` if the table has a single-column PK with `INT` affinity, otherwise `skip_hash=0`. Manual `skip_hash=1` works with any single-column PK. |

`skip_hash=1` on a composite PK table is rejected (falls back to hash mode) — skip_hash requires a single-column PK.

#### `use_rowid`

Controls whether the SQLite `rowid` is used as the internal `__crsql_key` (the primary key for clock entries).

| Value | Behavior |
|---|---|
| `use_rowid=1` | Use `rowid` as `__crsql_key`. Most efficient, but requires `INTEGER PRIMARY KEY` and rowids within `[0, 2^51)`. |
| `use_rowid=0` | Use an auto-assigned `__crsql_key`; store the PK value(s) in `v2_pks` directly. |
| *(absent)* | **Auto-detect**: always `use_rowid=0` (non-rowid). See explanation below. |

The auto-detect default is always non-rowid. `use_rowid=1` is only allowed on `INTEGER PRIMARY KEY` tables — `INTEGER PRIMARY KEY` is a rowid alias, so the rowid IS the PK value and is stable. Other rowid tables (e.g. `INT PRIMARY KEY`, `TEXT PRIMARY KEY`) have **implicit rowids** that can be renumbered by `VACUUM`, making them unsafe as persistent keys. Attempting `use_rowid=1` on a table without `INTEGER PRIMARY KEY` will fail with an error.

Even for `INTEGER PRIMARY KEY` tables, the default is non-rowid because in distributed systems the common pattern is random 64-bit integer PKs (e.g. snowflake IDs), which routinely exceed `2^51` and would overflow the `cell_key = (rowid << 12) | col_id` computation. If you know your rowids are small (e.g. explicit 50-bit IDs, sequential auto-increment), use `use_rowid=1` for better performance.

#### `crsql_as_crr` Arguments

The same overrides can be passed as positional arguments to `crsql_as_crr`:

```sql
-- force rowid-key mode
SELECT crsql_as_crr('my_table', 'use_rowid');

-- force non-rowid-key mode
SELECT crsql_as_crr('my_table', 'without_rowid');

-- force skip_hash on
SELECT crsql_as_crr('my_table', 'skip_hash');
```

The `without_rowid` argument is equivalent to `use_rowid=0`.

## How It Is Used in Corrosion

[Corrosion](https://github.com/superfly/corrosion) is a distributed, eventually-consistent SQLite database. It embeds cr-sqlite as a loadable extension (with pre-built binaries bundled for darwin-aarch64, linux-x86_64, and linux-aarch64) and builds a full clustering layer on top:

- **Actor identity**: Corrosion uses `crsql_site_id()` as the actor ID in its cluster. The `crsql_site_id` table (with ordinals) maps to Corrosion's actor tracking.
- **Change extraction**: Corrosion queries `crsql_changes` filtered by `db_version` and `site_id` to extract changesets for broadcast to peers. It uses `crsql_peek_next_db_version()` to determine the version before writes are committed, then queries `MAX(seq)` and `MAX(ts)` to track the last sequence and timestamp per version.
- **Change application**: Remote changesets are inserted into `crsql_changes` in bulk (via `unnest` for batch inserts). Corrosion uses `crsql_rows_impacted()` to verify that merges actually affected rows.
- **Timestamps**: Corrosion uses a Hybrid Logical Clock (HLC) and calls `crsql_set_ts()` before each transaction to stamp changes with an NTP64 timestamp. This enables the reaper to garbage-collect tombstones based on age.
- **Per-site version tracking**: Corrosion uses the `crsql_db_versions` table to track the latest `db_version` received from each peer. When processing incomplete or buffered changes, it calls `crsql_set_db_version(site_id, version)` to advance the tracked version even when no changes won the merge.
- **Schema management**: Corrosion handles schema migrations externally and uses `crsql_as_crr()` to register new tables and `crsql_begin_alter()` / `crsql_commit_alter()` to reconcile cr-sqlite metadata after schema changes. The `crsql_automigrate()` function has been removed in 0.18.0 — schema management should be handled by the application or migration framework. When migrating from older cr-sqlite versions that lacked the `(site_id, db_version)` index on clock tables, it creates `corro_{table}__crsql_clock_site_id_dbv` indexes.
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
| `crsql_as_crr('table')` | Upgrade a table to a conflict-free replicated relation. Optional flags: `'use_rowid'`, `'without_rowid'`, `'skip_hash'`. See [Schema Directives](#schema-directives). |
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

**V1 (0.17.0, deprecated — removed in 0.19.0):**
- **`{table}__crsql_clock`** — Per-column clock metadata (col_version, db_version, site_id, seq, ts)
- **`{table}__crsql_pks`** — Maps primary key values to integer keys for clock table lookups

**V2 (0.18.0+):**
- **`{table}__crsql_v2_pks`** — Maps hashed PK values to `pk_key` and stores causal length (`cl`). For tables with a rowid or single INTEGER PRIMARY KEY, PK columns are not stored separately — the integer key is the rowid of the `v2_pks` table itself.
- **`{table}__crsql_v2_col_map`** — Maps column names to small integer `col_id`s
- **`{table}__crsql_v2_clock`** — Per-cell clock metadata keyed by packed `cell_key = (pk_key << COL_ID_BITS) | col_id`
- **`{table}__crsql_v2_tombstones`** — Deleted row tracking, keyed by `hashed_pk`, stores the deletion causal length (`cl`)
- **`{table}__crsql_v2_tombstone_pks`** — Maps tombstone hashed PKs back to original PK column values (V1 wire compat). Can be pruned when V2 wire format is enabled.

### Global Tables

- **`crsql_db_versions`** — Per-site db_version tracking
- **`crsql_master`** — Extension metadata and config key-value store
- **`crsql_tracked_peers`** — Peer tracking table (site_id, version, seq, tag, event). Created by the extension but bookkeeping is expected to be handled by the application.

## How It Works

CR-SQLite uses history-free CRDTs based on [causal length sets](https://dl.acm.org/doi/pdf/10.1145/3380787.3393678). Each table upgraded to a CRR gets:

1. **Clock tables** that track per-column version metadata (col_version, db_version, site_id, seq, ts). In V1 these are `__crsql_clock` + `__crsql_pks`; in V2 these are `__crsql_v2_clock` (packed cell keys) + `__crsql_v2_pks` (hashed PKs with causal length) + `__crsql_v2_tombstones` (deleted rows) + `__crsql_v2_col_map` (column name → col_id mapping) + `__crsql_v2_tombstone_pks` (tombstone PK values for V1 wire compat).
2. **Triggers** (insert, update, delete) that automatically record changes into the clock tables. The same triggers work for both V1 and V2 — the extension routes internally based on the configured metadata version.

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
