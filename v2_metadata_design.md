# cr-sqlite V2 Metadata Design

> **Minimum SQLite version**: 3.44.0 (for `ORDER BY` inside aggregate calls support). The bundled amalgamation has been upgraded from 3.42 to 3.44.

## Design Goals

- Alive PKs retain full original PK columns + `hashed_pk` (fast lookup by `__crsql_key` or hash). The hash is needed because delete events arrive with only `hashed_pk` (from remote nodes that have the row as dead). We must look up the alive table by hash to find and remove it.
- Dead PKs store only a `PK_HASH_SIZE`-byte truncated XXH128 hash (no original PK columns) to reduce storage in the tombstone table (which can grow large).
- Clock table: single `INT64` primary key `(oid << CRSQL_COL_ID_BITS) | col_id`, no CL stored (CL lives in `v2_pks`/`v2_tombstones`), no sentinel rows (tombstones have their own table), timestamps stored as `INTEGER` (unix seconds).
- Feed returns real PK columns for alive rows. Dead rows return `hashed_pk` in V2 wire format; in V1 compat mode, dead rows return real PK columns (from `v2_tombstone_pks`).

## Hash Function

XXH128 of the PK values, truncated to `PK_HASH_SIZE` bytes.

The hash is computed over the `crsql_pack_columns()` blob of all PK columns, so it is stable across nodes (unlike `__crsql_key` which is a local auto-increment). Works for composite PKs, integer PKs, text PKs, etc. — `crsql_pack_columns` serializes all PK values into a deterministic byte sequence.

- `PK_HASH_SIZE` is a compile-time define (default: 10 bytes / 80 bits).
- XXH128 produces 128 bits; we take the first `PK_HASH_SIZE` bytes.
- Tunable via `#define CRSQL_PK_HASH_SIZE` at build time.

### Dependency: `xxhash-rust`

Uses the [`xxhash-rust`](https://github.com/DoumanAsh/xxhash-rust) crate (v0.8.17), which provides `xxh3_128` returning a `u128`.

- `no_std` compatible (the `std` feature is optional and only enables `std::io::Write` — we don't need it).
- Feature-gated: enable only `xxh3` (includes both `xxh3_64` and `xxh3_128`).
- Pure Rust, no C dependencies, no `unsafe` in the public API.
- SIMD acceleration (SSE2/AVX2/NEON) at compile time via target features.
- Compatible with `panic = "abort"` (no unwinding in hash code).
- API: `xxhash_rust::xxh3::xxh3_128(data: &[u8]) -> u128`

Cargo.toml addition:
```toml
[dependencies.xxhash-rust]
version = "0.8.17"
default-features = false
features = ["xxh3"]
```

Alternatives considered:
- `twox-hash`: Also has `XxHash3_128`, `no_std` compatible, but uses a struct-based API (`XxHash3_128::oneshot(seed, data)`) vs `xxhash-rust`'s simpler one-shot function. More popular but heavier.
- `xxh3` (cberner): Only XXH3, supports runtime AVX2/NEON detection, but narrower scope and less maintained.

`xxhash-rust` is the best fit: minimal, `no_std` by default, simple one-shot API, well-maintained.

Exposed as SQL function: `crsql_hash_pk(pk1, pk2, ...)`

- Takes variadic PK values directly (same signature as `crsql_pack_columns`).
- Internally: `pack_columns` → XXH128 → truncate to `PK_HASH_SIZE` bytes.
- Usage: `SELECT * FROM foo__crsql_v2_tombstones WHERE hashed_pk = crsql_hash_pk(1, 'abc')`
- Also used in Rust trigger code and merge logic (no SQL function call overhead there).
- Registered alongside `crsql_pack_columns` for ad-hoc queries, debugging, and consistency.

## Tables in This Design

1. `<table>__crsql_v2_col_map` — column name → 0-based col_id mapping, might differ between nodes
2. `<table>__crsql_v2_clock` — packed clock (cells only, no tombstones)
3. `<table>__crsql_v2_pks` — alive PKs with full columns + hash + CL
4. `<table>__crsql_v2_tombstones` — dead PKs with hash only + delete metadata
5. `<table>__crsql_v2_tombstone_pks` — hash → original PK cols for tombstones (V1 compat)

> **Note:** `<table>__crsql_pks` (the original PK-to-key mapping table) is created by cr-sqlite and has schema: `(__crsql_key INTEGER PRIMARY KEY, <pk_col1>, <pk_col2>, ...)` with a unique index on the original PK columns. In this design we do not use `__crsql_pks` directly for CL lookups; `__crsql_v2_pks` replaces it.

## CL (Causal Length) Semantics

CL is a per-PK monotonic counter that tracks the number of create/delete transitions for that PK. It starts at 1 (odd = alive) when a row is first created, and increments on each delete (even = dead) and each subsequent resurrection (odd = alive again).

- `CL=1`: row created (alive)
- `CL=2`: row deleted (dead)
- `CL=3`: row resurrected (alive)
- `CL=4`: row deleted again (dead)
- ...

Odd CL → row is alive (stored in `v2_pks`). Even CL → row is dead (stored in `v2_tombstones`). The `CHECK` constraints enforce this: `v2_pks.cl % 2 = 1`, `v2_tombstones.cl % 2 = 0`.

Merge conflict resolution uses CL as the primary ordering key:

- **`insert_cl < local_cl`**: incoming change is from an older causal epoch → ignore entirely. All column-level comparisons are skipped (the row has been deleted and possibly resurrected since this change was made).
- **`insert_cl > local_cl`**: incoming change is from a newer causal epoch → automatic win. If `insert_cl` is odd, the row is resurrected (moved from tombstones to `v2_pks`). If `insert_cl` is even, the row is deleted (moved from `v2_pks` to `v2_tombstones`).
- **`insert_cl == local_cl`**: same causal epoch → fall back to per-column version comparison (`col_version`, then `site_id` as tiebreaker). This is the existing `did_cid_win` logic.

CL is stored per-PK (not per-column) in `v2_pks` (alive) or `v2_tombstones` (dead). The clock table does not store CL — it is joined from `v2_pks`/`v2_tombstones` at feed time. This matches the existing V1 design where CL lives in the PK table, not the clock table.

### V1 vs V2 CL Representation Differences

**V1**: CL is stored as `col_version` in a sentinel clock row (`col_name = '-1'`). The sentinel-omission optimization means the sentinel row is not written until the first delete — `get_local_cl` returns `0` when no sentinel exists, which is interpreted as "row exists, logically CL=1" (the `COALESCE(t2.col_version, 1)` in the V1 feed query). `local_cl = 0` in V1 merge means "row exists but no sentinel" (logically CL=1), not "row never seen."

**V2**: CL is stored explicitly in `v2_pks.cl` (with `DEFAULT 1`) or `v2_tombstones.cl`. There is no sentinel-omission optimization — `v2_pks` always has the row with `cl >= 1`. `local_cl = 0` in V2 means "row not in `v2_pks` or `v2_tombstones`" (row never seen locally). The V2 merge path treats `local_cl = 0` the same as V1: `insert_cl > 0` always wins.

**V2→V1 translation**: when writing V2 data into V1 tables, if `local_cl = 0` in V2 (row never seen), the V1 path must create the `__crsql_pks` entry and write clock entries as if it were a fresh insert. The V1 sentinel-omission optimization applies naturally — no sentinel is written until a delete arrives.

---

## 1. Column Map: col_name → 0-based col_id

Maps each tracked column name to a compact integer ID. Used to pack `(key, col_id)` into a single `cell_key` INTEGER: `(key << CRSQL_COL_ID_BITS) | col_id`.

`CRSQL_COL_ID_BITS` is a compile-time define (default: 12 bits = up to 4096 columns per table). That's safe as [SQLite limits](https://sqlite.org/limits.html) say that the default setting for `SQLITE_MAX_COLUMN` is 2000. Tunable via `#define CRSQL_COL_ID_BITS` at build time. Reducing it frees bits for larger rowids (rowid limit = `2^(64 - CRSQL_COL_ID_BITS)`).

```sql
CREATE TABLE IF NOT EXISTS "<table>__crsql_v2_col_map" (
  col_id INTEGER PRIMARY KEY,
  col_name TEXT NOT NULL
) STRICT;

CREATE UNIQUE INDEX IF NOT EXISTS "idx_<table>_v2_col_map_name"
  ON "<table>__crsql_v2_col_map"(col_name);
```

## 2. Clock Table: Packed INT64, No Sentinels/Tombstones

One row per `(row, column)` cell that has been written.

- `cell_key = (pk_key << CRSQL_COL_ID_BITS) | col_id` where col_id from col_map (0-based)
- `ts` stored as `INTEGER` (unix seconds) instead of `TEXT` for compactness.
- Feed index on `(site_id, db_version, seq)` for efficient change feed queries.

```sql
CREATE TABLE IF NOT EXISTS "<table>__crsql_v2_clock" (
  cell_key INTEGER PRIMARY KEY,           -- (pk_key << CRSQL_COL_ID_BITS) | col_id
  col_version INTEGER NOT NULL,           -- column version
  site_id INTEGER NOT NULL,               -- which site made the change
  db_version INTEGER NOT NULL,            -- local DB version at time of change
  seq INTEGER NOT NULL,                   -- sequence within db_version
  ts INTEGER NOT NULL CHECK (ts > 0),     -- unix timestamp (seconds); reject unset
) STRICT;

CREATE INDEX IF NOT EXISTS "<table>__crsql_v2_clock_feed_idx"
  ON "<table>__crsql_v2_clock"(site_id, db_version, seq);
```

## 3. Alive PKs: `__crsql_key` + Hash + CL

`__crsql_key INTEGER PRIMARY KEY` (stores the underlying main table's rowid for rowid tables, auto-increment for WITHOUT ROWID base tables). Unique index on `hashed_pk` for O(1) deletion lookups (is this PK alive or dead?).

### Two Modes Depending on Main Table Type

- **Rowid tables**: `__crsql_key = main_table.rowid`. No PK columns stored here — PK values are fetched from the main table via `SELECT pk_cols WHERE rowid = ?`. Columns: `__crsql_key`, `hashed_pk`, `cl`.
- **WITHOUT ROWID tables**: `__crsql_key = auto-increment`. Full PK columns stored here (needed for feed emission, since there's no rowid to join back to the main table). Columns: `__crsql_key`, `<pk_cols...>`, `hashed_pk`, `cl`.

V1 compat: PK columns (stored or fetched) needed for V1 wire emission.

### `__crsql_key` Assignment (Rowid Reuse)

When the main table is a rowid table with an accessible rowid — meaning at least one of the built-in aliases (`rowid`, `oid`, `_rowid_`) is not shadowed by a declared column, OR the table has an `INTEGER PRIMARY KEY` column (which is itself the rowid alias) — then `__crsql_key = main_table.rowid`.

**Edge case** ([SQLite rowidtable docs](https://www.sqlite.org/rowidtable.html)): if a table declares columns named `rowid`, `oid`, AND `_rowid_`, all three aliases are shadowed. If there's also no `INTEGER PRIMARY KEY`, the rowid is completely inaccessible by name → fall back to auto-increment.

Examples:
- `CREATE TABLE bar(rowid TEXT, oid TEXT, _rowid_ TEXT)` — rowid exists but is unreachable.
- `CREATE TABLE foo(rowid TEXT, oid TEXT, _rowid_ TEXT, pk INTEGER PRIMARY KEY)` — `pk` is the rowid alias, so we use `pk` directly.

**Detection logic at `as_crr` time**: query `pragma_table_info('<table>')` to determine the mode:

1. Check if any column has `pk > 0` AND `type = 'INTEGER'` → `INTEGER PRIMARY KEY` exists → use it as `__crsql_key` (it *is* the rowid alias).
2. Otherwise, check if none of `rowid`, `oid`, `_rowid_` appear as column names in `pragma_table_info` → at least one alias is unshadowed → use `rowid` as `__crsql_key`.
3. If all three aliases are shadowed AND no `INTEGER PRIMARY KEY` → fall back to auto-increment with stored PK columns.

**Tests required**: write tests covering all three cases — (a) plain rowid table, (b) `INTEGER PRIMARY KEY` table, (c) all-three-aliases-shadowed table, (d) `INTEGER PRIMARY KEY` + shadowed aliases (case b wins). Verify that `__crsql_key` is correctly assigned and that feed/merge queries work in each case.

This eliminates the `INSERT OR IGNORE` + auto-increment round-trip on local writes:
- Local insert: `NEW.rowid` is available directly in the trigger.
- Local update: `NEW.rowid` is available directly in the trigger.

> **Note:** In trigger context, `NEW.rowid` / `OLD.rowid` are always accessible for rowid tables regardless of column shadowing. The shadowing edge case only affects ad-hoc SQL queries outside triggers (e.g., merge path doing `SELECT rowid FROM main_table WHERE pk = ?`). When rowid is inaccessible for ad-hoc queries, or the table is WITHOUT ROWID, or was explicitly configured to use the WITHOUT ROWID layout at `as_crr` time, fall back to auto-increment with stored PK columns (current V1 scheme). The clock table doesn't care — `__crsql_key` is just an `INT64` join key.

### Merge Path (Row Not in Main Table Yet)

The incoming change may be for a row that doesn't exist locally. Check `hashed_pk` against `v2_pks` and `v2_tombstones` first:

- **Clean slate** (`hashed_pk` not in either table): `INSERT` directly into main table first, get `sqlite3_last_insert_rowid()` back, use it as `__crsql_key`. No stub, no fallback, no remapping.
- **Resurrection** (`hashed_pk` in `v2_tombstones` only): the row was deleted locally but an incoming change resurrects it. Same as clean-slate — `INSERT` into main table first, get `sqlite3_last_insert_rowid()`, use it as `__crsql_key`. Then delete the tombstone entry and add the alive entry with the new CL.

For WITHOUT ROWID tables, use auto-increment fallback in both cases.

### Large Rowid Handling

`cell_key = (__crsql_key << CRSQL_COL_ID_BITS) | col_id` must fit in a **signed** `INT64` (SQLite's `INTEGER` type is 64-bit signed), so `cell_key` must stay positive. This means `__crsql_key` is limited to `2^(63 - CRSQL_COL_ID_BITS)`. With the default `CRSQL_COL_ID_BITS = 12`, the limit is `2^51 = 2,251,799,813,685,248`.

If a rowid table may have rowids `>= 2^(63 - CRSQL_COL_ID_BITS)`, it should be converted to WITHOUT ROWID at CRR registration time (per-table decision). The `as_crr` call accepts an option to convert a rowid table to WITHOUT ROWID (storing the full PK as the primary key). This is a one-time schema decision made when the table is registered as a CRR. The `as_crr` call also accepts an opt-out parameter to skip the `CHECK` constraint below (for applications that know their rowids are safe and want to avoid the per-write overhead).

`CHECK` constraint on main table (rowid tables only):

```sql
CHECK (rowid >= 0 AND rowid < 2251799813685248)  -- 2^(63 - CRSQL_COL_ID_BITS), signed INT64 safe
```

Ensures `cell_key = (rowid << CRSQL_COL_ID_BITS) | col_id` fits in a positive signed `INT64` without overflow. Using `2^(63 - bits)` (not `2^(64 - bits)`) because `key << 12` with `key >= 2^51` would overflow into the sign bit, producing negative `cell_key` values that sort before all positive ones — breaking `PRIMARY KEY` ordering. `rowid >= 0` allows `INTEGER PRIMARY KEY` tables that use 0 as a valid key value (`cell_key = 0 | col_id = col_id`, which is fine). Overhead is one integer comparison per write. Rowid changes (e.g., `UPDATE rowid = ...`) are handled by triggers which update `__crsql_key` in `v2_pks` and `cell_key` in `v2_clock` accordingly.

### Schema

Schema is dynamic — PK columns only present for WITHOUT ROWID tables.

**Example (WITHOUT ROWID, PK columns (id1, id2, ...)):**

```
__crsql_key INTEGER PRIMARY KEY,
id1 <type> NOT NULL,             -- original PK column (WITHOUT ROWID only)
id2 <type> NOT NULL,             -- original PK column (WITHOUT ROWID only)
...
hashed_pk BLOB NOT NULL,         -- PK_HASH_SIZE-byte truncated XXH128 of packed PK columns
cl INTEGER NOT NULL DEFAULT 1    -- clock version / change label
```

**Example (rowid table):**

```
__crsql_key INTEGER PRIMARY KEY, -- = main_table.rowid
hashed_pk BLOB NOT NULL,
cl INTEGER NOT NULL DEFAULT 1
```

`STRICT` is always used for all V2 tables. For tables with dynamic PK columns (WITHOUT ROWID), the `ANY` type ([sqlite.org/stricttables.html](https://www.sqlite.org/stricttables.html)) is used for PK columns, so STRICT works regardless of whether the base table is STRICT.

```sql
CREATE TABLE IF NOT EXISTS "<table>__crsql_v2_pks" (
  __crsql_key INTEGER PRIMARY KEY,       -- main_table.rowid (rowid tables) or auto-increment (WITHOUT ROWID)
  -- ... original PK columns from the base table (WITHOUT ROWID tables only) ...
  -- End dynamic columns
  hashed_pk BLOB NOT NULL,               -- PK_HASH_SIZE-byte truncated XXH128(crsql_pack_columns(pk_cols))
  cl INTEGER NOT NULL DEFAULT 1 CHECK (cl % 2 = 1),  -- odd = alive (current CL)
) STRICT;  -- STRICT always; WITHOUT ROWID tables use ANY type for PK columns

-- Unique index on hash — primary access path when __crsql_key (PK table rowid) is not
-- available as a lookup key (WITHOUT ROWID tables, shadowed rowid, merge path, feed queries):
CREATE UNIQUE INDEX IF NOT EXISTS "idx_<table>_v2_pks_hash"
  ON "<table>__crsql_v2_pks"(hashed_pk);
```

## 4. Tombstones: Hash Only (No Original PK Columns) + Delete Metadata

WITHOUT ROWID — composite PK `(site_id, db_version, seq)` for feed ordering. This PK is deliberately chosen to make feed queries as efficient as possible — the feed scans by `(site_id, db_version)` and orders by `seq`, which is exactly the PK order. `hashed_pk` replaces original PK columns to save space. Unique index on `hashed_pk` for upsert-based conflict resolution (one tombstone per PK at any time). `cl` stored for feed output. `ts` stored for `dead_recent` selection. This table IS the delete log — no separate delete_log table needed.

```sql
CREATE TABLE IF NOT EXISTS "<table>__crsql_v2_tombstones" (
  site_id INTEGER NOT NULL,
  db_version INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  hashed_pk BLOB NOT NULL,               -- PK_HASH_SIZE-byte truncated XXH128(crsql_pack_columns(pk_cols))
  cl INTEGER NOT NULL CHECK (cl % 2 = 0),  -- even = deleted (CL at deletion time)
  ts INTEGER NOT NULL CHECK (ts > 0),    -- timestamp of deletion; reject unset
  PRIMARY KEY (site_id, db_version, seq)
) WITHOUT ROWID, STRICT;
```

### Clock Entry Cleanup on Delete

When a row is deleted locally, all clock entries for that row are deleted:

```sql
DELETE FROM v2_clock WHERE cell_key >> CRSQL_COL_ID_BITS = ?key
```

This mirrors the existing V1 design which drops clock rows on delete (see `after_delete.rs`: `drop_clocks_stmt`). Tombstone entries in `v2_tombstones` serve as the delete event in the feed; no sentinel rows are needed in the clock table.

### Unique Index on Hash — Upsert-Based Conflict Resolution

Only one tombstone per `hashed_pk` exists at any time. When a new delete arrives for a hash that already has a tombstone (concurrent deletes from different sites, or re-delete after resurrection), conflict resolution compares CL values and keeps the higher one (biggest CL = most recent delete), updating its metadata (`site_id`, `db_version`, `seq`, `ts`) to match the winning tombstone.

For equal-CL conflicts (concurrent deletes at the same CL from different sites), V2 mirrors V1's `did_site_id_win` approach: compare site_id blobs lexicographically. SQLite's `>` operator on BLOBs does byte-by-byte memcmp — exactly what V1's Rust `insert_site_id.cmp(local_site_id)` does. Since `v2_tombstones.site_id` stores a local ordinal (not the blob), a subquery resolves it to the blob for comparison. The incoming site_id blob is bound as a parameter from the merge path.

As with V1, the site_id tiebreaker only applies when `merge-equal-values` is enabled (`crsql_config_set('merge-equal-values', 1)`). When disabled (the default), equal-CL conflicts are silently dropped — the existing tombstone is kept. This matches V1's behavior where `did_site_id_win` is only called when `mergeEqualValues == 1`.

```sql
-- Single upsert handles both CL > and CL == with site_id tiebreaking
INSERT INTO v2_tombstones VALUES (...)
ON CONFLICT(hashed_pk) DO UPDATE SET
  site_id = excluded.site_id,
  db_version = excluded.db_version,
  seq = excluded.seq,
  cl = excluded.cl,
  ts = excluded.ts
WHERE excluded.cl > v2_tombstones.cl
   OR (excluded.cl = v2_tombstones.cl
       AND ?incoming_site_id_blob > (SELECT site_id FROM crsql_site_id WHERE ordinal = v2_tombstones.site_id))
```

If the upsert's `WHERE` clause doesn't match (incoming CL is lower, or equal CL with lower/equal site_id blob), the existing tombstone is kept — no update needed.

Re-deletion after resurrection works because resurrection removes the tombstone first (moves PK back to `v2_pks`), so the subsequent delete inserts into an empty slot.

```sql
CREATE UNIQUE INDEX IF NOT EXISTS "idx_<table>_v2_tombstones_hash"
  ON "<table>__crsql_v2_tombstones"(hashed_pk);
```

## 5. Tombstone PK Mapping (V1 Compatibility)

Maps `hashed_pk` back to original PK column values for tombstones. Needed while V1 wire format is still emitted: V1 sends `crsql_pack_columns(pk_cols)` for delete events, which requires the real PK values, not just the hash. V2 wire format sends `hashed_pk` directly, so this table is not needed for V2.

**Pruning**: entries are deleted only after ALL nodes in the cluster emit V2 format. Until then, every new tombstone gets an entry here.

Schema is dynamic — mirrors the original table's PK columns (like `__crsql_v2_pks`).

```sql
CREATE TABLE IF NOT EXISTS "<table>__crsql_v2_tombstone_pks" (
  hashed_pk BLOB PRIMARY KEY,
  -- ... original PK columns from the base table ...
  -- End dynamic columns
  -- Example: id INTEGER NOT NULL, name TEXT NOT NULL, etc.
) WITHOUT ROWID, STRICT; -- STRICT always; PK columns use ANY type
```

---

## Vtable Rowid Key for Tombstones

The `crsql_changes` virtual table requires a unique rowid per row (SQLite's `xRowid` interface). For clock entries (alive rows), the `key` column is `__crsql_key` — a small autoincrement integer from `v2_pks`. Tombstones in `v2_tombstones` have no equivalent integer key (their PK is `(site_id, db_version, seq)`), so a synthetic `key` is computed in the feed query.

### Bit Layout

```
Bit 62:    flag (1 = tombstone, distinguishes from clock entry keys)
Bits 46-61: site_id   (16 bits → 65,536 nodes)
Bits 22-45: db_version (24 bits → 16.7M)
Bits 0-21:  seq        (22 bits → 4.2M)
```

```sql
(1 << 62) | (t.site_id << 46) | (t.db_version << 22) | t.seq as key
```

### Rationale

- **Clock entry keys are small**: `__crsql_key` is a rowid (max ~2^51 with default `CRSQL_COL_ID_BITS`) or autoincrement, starting from 1. Setting bit 62 guarantees tombstone keys never collide with clock entry keys.
- **Uniqueness within tombstones**: `(site_id, db_version, seq)` is the PK of `v2_tombstones`, so the packed value is unique per table. Combined with `slab_rowid(tblInfoIdx, key)`, the final vtable rowid is globally unique across all tables.
- **Bit allocation is generous**: 65K nodes, 16.7M db_versions, and 4.2M seq values per tombstone are well above expected limits. `db_version` gets the most bits because it grows monotonically and is the most likely to exceed limits in long-lived databases.
- **Collisions would be harmless in practice**: If limits are exceeded (e.g., `seq > 4.2M`), the packed key wraps and could collide with another tombstone's key. This only affects the vtable rowid — the actual data (pks, cid, cl, etc.) is unaffected. The only scenario where this matters is advanced operations like `ORDER BY rowid` or `WHERE rowid = ?` on `crsql_changes`, which are not typical usage patterns. Standard `SELECT * FROM crsql_changes` iterates via `xNext`/`xColumn` and is unaffected by rowid collisions.
- **If problematic, can be changed**: The key computation is a single expression in the feed query (`changes_vtab_read.rs`). If larger limits are needed, the bit allocation can be adjusted, or the approach can be replaced entirely (e.g., using a WITHOUT ROWID virtual table with `(table, pk)` as the primary key, per [SQLite vtab docs](https://www.sqlite.org/vtab.html#_without_rowid_virtual_tables_)).

---

## Query Patterns

### Point SELECT (is cell alive or dead? get CL + clock)

```sql
-- h = xxh128(crsql_pack_columns(pk_values))[:PK_HASH_SIZE]
SELECT t1.col_version, pk.cl
FROM <table>__crsql_v2_clock AS t1,
(
  SELECT cl FROM <table>__crsql_v2_pks WHERE __crsql_key = ?key
  UNION ALL
  SELECT cl FROM <table>__crsql_v2_tombstones WHERE hashed_pk = ?h
  LIMIT 1
) AS pk
WHERE t1.cell_key = ?cell_key
```

### Feed (Changes Since db_version for a Site)

Alive rows: JOIN clock with `v2_pks` to get PK columns and CL. Column values are fetched from the main table via a compiled `CASE` statement:

```sql
CASE (t1.cell_key & ((1 << CRSQL_COL_ID_BITS) - 1))
  WHEN 0 THEN main_table.col0
  WHEN 1 THEN main_table.col1
  ...
END AS col_val
```

This `CASE` is generated per-table at CRR registration time, mapping `col_id` → column name. For rowid tables, PK columns are fetched via: `SELECT <pk_cols> FROM main_table WHERE rowid = ah.__crsql_key`. For WITHOUT ROWID tables, PK columns come directly from `v2_pks`. Column names are resolved via: `SELECT col_name FROM v2_col_map WHERE col_id = (t1.cell_key & ((1 << CRSQL_COL_ID_BITS) - 1))`.

Dead rows: `hashed_pk` from tombstones (no PK columns in V2 mode).

```sql
SELECT ah.<pk_cols>, t1.col_version, t1.db_version, t1.site_id, t1.seq, t1.ts, ah.cl,
       CASE (t1.cell_key & ((1 << CRSQL_COL_ID_BITS) - 1))
         WHEN 0 THEN mt.col0
         WHEN 1 THEN mt.col1
         ...
       END AS col_val,
       cm.col_name
FROM <table>__crsql_v2_clock AS t1
JOIN <table>__crsql_v2_pks AS ah ON ah.__crsql_key = t1.cell_key >> CRSQL_COL_ID_BITS
JOIN <table> AS mt ON mt.rowid = ah.__crsql_key          -- rowid tables only
JOIN <table>__crsql_v2_col_map AS cm ON cm.col_id = (t1.cell_key & ((1 << CRSQL_COL_ID_BITS) - 1))
WHERE t1.site_id = ? AND t1.db_version > ?
UNION ALL
SELECT d.hashed_pk, d.cl, d.db_version, d.site_id, d.seq, d.ts, d.cl
FROM <table>__crsql_v2_tombstones AS d
WHERE d.site_id = ? AND d.db_version > ?
ORDER BY db_version, seq LIMIT ?
```

### UPDATE (Check Alive/Dead Then Update Clock)

```sql
-- h = xxh128(crsql_pack_columns(pk_values))[:PK_HASH_SIZE]
SELECT 1 FROM <table>__crsql_v2_pks WHERE __crsql_key = ?key
UNION ALL
SELECT 1 FROM <table>__crsql_v2_tombstones WHERE hashed_pk = ?h
LIMIT 1
-- then: UPDATE <table>__crsql_v2_clock SET ... WHERE cell_key = ?cell_key
```

---

## Packed Wire Format (crsql_changes v2 Emission)

### Problem

Currently each clock row = 1 change event on the wire. An insert of a 20-column row produces 20 events, each duplicating `tbl`, `pk`, `db_version`, `site_id`, `cl`, `ts`. The receiver does 20 separate `UPDATE`/`INSERT` statements.

### Idea

Coalesce clock rows that share `(row_key, db_version, site_id)` into a single packed event. The clock table stays per-column (queryable, simple). Packing is purely a `crsql_changes` read/serialization optimization.

### Versioning

Controlled by application-level config flags, not wire-level:

- **`metadata-write-version`**: `v1` | `v2` | `v2&v1` (which schema to write to)
- **`metadata-use-version`**: `v1` | `v2` (which schema to read from)
- **`sync-log-version`**: `v1` | `v2` (which wire format to emit)

Nodes accept v2 logs regardless of their own `sync-log-version` — `sync-log-version` controls what a node emits, not what it can receive. Reception requires:

1. `metadata-write-version` is `v2` or `v2&v1` (V2 tables are actively written to).
2. V1→V2 migration is complete (all PK hashes available in `v2_pks`/`v2_tombstone_pks`).

During migration (before completion), v2 logs are rejected with an error because not all PK hashes are available yet for resolving incoming v2 dead-row events (hash-only PKs). Once migration completes in `v2&v1` mode, the node has all hashes and can fully process v2 logs, emit them, and use v2 for feed/processing.

This allows gradual rollout: migrate schema → flip write → flip use → flip sync-log.

### Progression Rules

**`metadata-write-version`:**

| From | To | Allowed? | Notes |
|------|-----|----------|-------|
| `v1` | `v2&v1` | Yes | Forward always allowed |
| `v2&v1` | `v2` | Yes | Forward always allowed |
| `v2&v1` | `v1` | Yes | V1 tables were kept in sync during dual write. Stops any in-progress migration and queues V2 table cleanup (dropped by `crsql_incremental_maintenance` as a background task). |
| `v2` | `v2&v1` | **Forbidden** | V1 tables stopped receiving writes, now stale |
| `v2` | `v1` | **Forbidden** | V1 tables stale |

**`metadata-use-version`:**

| From | To | Allowed? | Notes |
|------|-----|----------|-------|
| `v1` | `v2` | Yes | Forward allowed (guarded — see below) |
| `v2` | `v1` | Yes | Only if `metadata-write-version` is `v1` or `v2&v1` (V1 tables still active). Forbidden if write is `v2`. |

**`sync-log-version`:**

| From | To | Allowed? | Notes |
|------|-----|----------|-------|
| `v1` | `v2` | Yes | Forward allowed (guarded — see below) |
| `v2` | `v1` | Yes | Only if `metadata-use-version` is `v1` and `v2_tombstone_pks` pruning has not started (v1 compat emission needs to resolve hash→PK for dead rows from that table). Forbidden once pruning has removed entries. |

### Re-enabling v2&v1 After Rollback to v1

V2 tables were dropped during the `v2&v1` → `v1` transition. Going forward to `v2&v1` again is just setting the flag, which queues a fresh migration:

```sql
crsql_config_set('metadata-write-version', 'v2&v1')  -- queues migration
crsql_incremental_maintenance(N)  -- rebuilds V2 from scratch
```

The migration function creates V2 tables if they don't exist, so this is equivalent to a first-time migration. The cost is a full re-scan, but rollback should be rare.

### Config API (Same Mechanism as `merge-equal-values`)

```sql
crsql_config_set('metadata-write-version', 'v1')   -- default
crsql_config_set('metadata-write-version', 'v2')
crsql_config_set('metadata-write-version', 'v2&v1')
crsql_config_get('metadata-write-version') -> 'v1'
-- same pattern for 'metadata-use-version' and 'sync-log-version'
-- defaults: all 'v1' (existing behavior unchanged until explicitly switched)
-- stored in crsql_master (same table as other config keys)
-- crsql_config_set enforces progression rules (see above)
```

### Guards on `metadata-use-version` and `sync-log-version`

**`metadata-use-version = 'v2'` requires:**

1. V2 schema is fully populated for ALL CRR tables — either:
   - (a) The table was registered as a CRR with V2 from the start (no V1 tables ever existed), or
   - (b) `crsql_incremental_maintenance` has completed the V1→V2 migration (returned 0 for all migration tasks).
2. `metadata-write-version` is `v2` or `v2&v1` (V2 tables are actively written to).

**`sync-log-version = 'v2'` requires:**

1. `metadata-use-version` is `v2` (feed reads from V2 tables).
2. `metadata-write-version` is `v2` or `v2&v1` (V2 tables are actively written to).

Guards are global: `crsql_config_set` checks that ALL CRR tables meet prerequisites before accepting the change. This prevents reading from or emitting from stale V2 tables on any table.

### Incremental Maintenance: `crsql_incremental_maintenance(chunk_size) -> INTEGER`

Single entry point for all background maintenance work. Dispatches to whatever tasks are pending across all CRR tables. Does up to `chunk_size` units of work per call, returns total work units remaining across all tasks. When it returns 0, all maintenance is complete (until new work is queued). Safe to call repeatedly — tracks progress per-task internally and picks up where it left off. Designed for periodic invocation (e.g., from a timer or idle hook).

**Dispatch logic (priority order):**

1. **V1→V2 schema migration** (if `metadata-write-version` was set to `v2&v1` for any table that still has V1 tables and no V2 tables):
   - **(a)** First call for a table: create V2 tables (`col_map`, `clock`, `pks`, `tombstones`, `tombstone_pks`).
   - **(b)** Populate `v2_col_map` from existing `col_name` values in `__crsql_clock`.
   - **(c)** For each chunk of `__crsql_pks` rows (up to `chunk_size` per call):
     - Progress is tracked via a cursor on `__crsql_key` (autoincrement integer). `progress_marker` stores the last `__crsql_key` processed; each call resumes with `WHERE __crsql_key > progress_marker LIMIT chunk_size`.
     - Compute `hashed_pk = xxh128(crsql_pack_columns(pk_cols))[:PK_HASH_SIZE]`.
     - If row is alive (`INSERT_SENTINEL` or no sentinel): `INSERT OR IGNORE` into `v2_pks` with `cl` from sentinel `col_version` (odd) or `cl=1` if no sentinel. `INSERT OR IGNORE` ensures idempotency with dual-write triggers that may have already inserted the row.
     - If row is dead (`DELETE_SENTINEL`): `INSERT OR REPLACE` into `v2_tombstones` with `cl` from sentinel `col_version` (even), and `INSERT OR REPLACE` into `v2_tombstone_pks`. If `ts = 0` or unparseable, assign the migration start timestamp rather than skipping — the tombstone is still valid, just missing its timestamp.
     - Repack clock rows: for each `(key, col_name)` in `__crsql_clock` for this PK, `INSERT OR REPLACE` into `v2_clock` with `cell_key = (key << COL_ID_BITS) | col_id`. `INSERT OR REPLACE` ensures the cursor pass overwrites any dual-written entries with the authoritative migrated values. Clock entries with `ts = 0` or unparseable `ts` are also assigned the migration start timestamp rather than failing the `CHECK (ts > 0)` constraint.
     - Orphan PK handling: if a `__crsql_pks` row has NO clock entries at all (not even a sentinel) AND the row does not exist in the base table, skip it entirely — it's stale/corrupt metadata with no corresponding data. Do not insert anything into V2 tables for it.
   - **(d)** First call for a table: set `metadata-write-version` to `v2&v1` automatically. Dual write begins immediately so updates during the cursor sweep are captured in V2 tables. The cursor only processes new `__crsql_pks` rows; without dual write, updates to existing rows would be missed.
   - **(e)** Final call for a table (returns 0 for that table): migration is complete. All PK hashes are now available. The node can now accept v2 logs, set `metadata-use-version` to `v2`, and set `sync-log-version` to `v2`.
   - *(Additional maintenance tasks — tombstone pruning, post-alter compaction, backfill — will be added to the dispatcher in the future.)*

**Task queue**: stored in `crsql_master`. Each pending task has:

```
(task_type, table_name, progress_marker, total_units)
```

`crsql_incremental_maintenance` picks the highest-priority task with remaining work, does up to `chunk_size` units, updates `progress_marker`, and returns the sum of remaining units across all tasks. **One work unit = one row** (e.g., one `__crsql_pks` row during migration, one main-table row during backfill). Each call runs within a single db transaction — `chunk_size` should be sized to keep transaction duration reasonable (e.g., 100000 rows).

**Enabling `v2&v1` IS starting the migration:**

```sql
crsql_config_set('metadata-write-version', 'v2&v1')  -- queues migration task
-- Then call crsql_incremental_maintenance(N) periodically until it returns 0.
```

### Recommended Rollout Sequence

1. `crsql_config_set('metadata-write-version', 'v2&v1')` — global flag, queues migration tasks for all V1 CRR tables; dual write begins on first maintenance call, capturing updates during migration.
2. Call `crsql_incremental_maintenance(N)` periodically until it returns 0. — migration complete for all tables, all PK hashes available, node can now accept v2 logs.
3. Set `metadata-use-version` to `v2` (guarded: all migrations must be complete).
4. Set `sync-log-version` to `v2` when all peers can accept V2 format.
5. Set `metadata-write-version` to `v2` to stop writing to V1 tables. (V1 tables are now stale — progression rules prevent going back.)
6. `crsql_incremental_maintenance` will clean up V1 tables as a background task: drops `__crsql_clock` and `__crsql_pks` for tables where `metadata-write-version` is `v2` and no V1 feed reads or V1 log emission remain.

### V1/V2 Coexistence

During rollout, a node may receive both v1 (per-column) and v2 (packed) log entries simultaneously. The receiver must detect which format each row uses. Detection is per-row, not per-stream:

- **Packed (v2)**: `cid` contains `char(0)` separator → split by `char(0)` to get col_name list. `col_vrsn` also contains `char(0)` → split to get versions. `cval` is a `crsql_pack_columns` blob of all values.
- **Single (v1)**: `cid` is a plain column name or sentinel (no `char(0)`). `col_vrsn` is a single integer. `cval` is a single value.
- **Sentinels**: `cid = '-1'` (delete or insert sentinel — in V1 both `INSERT_SENTINEL` and `DELETE_SENTINEL` are `'-1'`, distinguished by CL parity: even = delete, odd = insert) or `'-2'` (hash-based tombstone, **new in V2**: dead row with `hashed_pk` instead of real PK) are always single events in both v1 and v2 — no packing. The merge path must handle `'-2'` as a new case not present in V1. **`'-2'` can only be accepted when the receiving node has completed V1→V2 migration** (i.e., `metadata-use-version` is `v2&v1` or `v2`), since it requires `v2_tombstones` and `v2_pks` tables to exist with full data. If a `'-2'` row is received before migration is complete, the merge path must return an error — the sender is using V2 wire format but the receiver isn't ready for it.

Column names are used (not `col_id`) because names are deterministic across nodes (same schema) while internal `col_id` from `col_map` is local and non-deterministic across nodes.

### Packed Wire Row (Same Vtable Columns, Different Encoding)

| Column | Description |
|--------|-------------|
| `tbl` | Table name (1×, not N×) |
| `pk` | Packed PK values via `crsql_pack_columns` (1×) |
| `cid` | `GROUP_CONCAT(col_name, char(0))` — null-separated col names. Detection: `char(0)` present → packed; absent → single/sentinel. SQLite column names cannot contain null bytes, so this is safe. |
| `cval` | `crsql_pack_agg(col_val)` — custom SQLite aggregate (`xStep` + `xFinal`) that collects values across the `GROUP BY` and produces a TLV blob using the same per-value encoding as `crsql_pack_columns`. Implemented in Rust alongside `crsql_pack_columns`, reusing the same per-value encoding logic. `GROUP_CONCAT` can't be used here because it loses type info (Integer/Text/Blob/Float/Null). Format: `[num_values:varint, ...[(type:3bits, intlen:5bits):u8, length?:varint, ...bytes]]`. Each value encodes its own type and length. Receiver calls `unpack_columns(cval)` → `Vec<ColumnValue>`, no external metadata needed. **ORDER IS THE CONTRACT**: values in `cval` are in the same order as column names in `cid` and versions in `col_vrsn`. All three aggregates run over the same group, so ordering is naturally consistent. |
| `col_vrsn` | `GROUP_CONCAT(col_version, char(0))` — null-separated col versions (parallel array to `cid`; receiver zips `cid[i]` with `col_vrsn[i]`) |
| `db_vrsn` | Single value (shared by all N columns in the group) |
| `site_id` | Single value |
| `seq` | `GROUP_CONCAT(t1.seq, char(0))` — null-separated seqs (parallel array to `cid`). All seqs in the group are preserved for bookeeping correctness. |
| `cl` | Single value (from `v2_pks` or `v2_tombstones` JOIN) |
| `ts` | Single value |

> **Major implementation item**: `crsql_pack_agg` is a new custom SQLite aggregate function requiring `xStep` + `xFinal` callbacks. This is non-trivial — it must:
> - Accept any SQLite value type (Integer, Text, Blob, Float, Null) in `xStep`
> - Accumulate values in an internal buffer using the same TLV encoding as `crsql_pack_columns`
> - Produce the final blob in `xFinal` with a varint count header (matching `crsql_pack_columns`)
> - Produce byte-identical output to `crsql_pack_columns` for the same values in the same order
> - Handle NULL values correctly (encoded as type byte only, no length/data)
> This should be implemented and tested early as it is a hard dependency for the packed feed query.
>
> **Count header: u8 → varint**: The existing `crsql_pack_columns` and `unpack_columns` are changed to use a **SQLite varint** for the count header instead of `u8`. SQLite varints encode 0-127 in a single byte (0x00-0x7F) — byte-identical to the old `u8` format. This means V1↔V2 wire interop works seamlessly for any table with ≤127 columns (everyone today). For 128+ columns the varint encoding differs from u8 (varint uses 2+ bytes, u8 uses 1), but no existing deployment has that many columns. Varints scale up to 9 bytes encoding 2^64, so the column limit is effectively removed — the practical limit is SQLite's `SQLITE_MAX_COLUMN` (default 2000, max 32767). No separate `unpack_columns_v2` is needed — the existing `unpack_columns` is updated to read the varint count.
>
> **Wire compatibility note**: `crsql_pack_columns` output is the `pk` column in the changes vtab feed — it's transmitted between nodes. During V1→V2 migration, V1 and V2 nodes exchange changes. For ≤127 columns, V1 nodes produce u8 count (0x00-0x7F) and V2 nodes produce varint count (0x00-0x7F) — byte-identical, full interop. V1 nodes reading V2 blobs: the varint count byte 0x00-0x7F is interpreted as u8 correctly. V2 nodes reading V1 blobs: the u8 count byte 0x00-0x7F is read as varint correctly. For 128+ columns, interop breaks — but this is a non-issue since no deployment has that many columns.

### Feed Query (Packed, Per Table)

**Alive rows** (updates/inserts with column data):

> **Implementation note**: SQLite 3.44+ supports `ORDER BY` inside aggregate calls, which eliminates the need for a subquery to pre-sort rows. The `ORDER BY cm.col_id` inside each aggregate ensures `cid`, `col_vrsn`, `cval`, and `seq` arrays are aligned. **Important**: the `GROUP BY` clause must use the full expression `c.cell_key >> CRSQL_COL_ID_BITS`, not the column alias `key` — SQLite 3.44 does not resolve aliases in GROUP BY when the query is used as a virtual table subquery. Bare columns (`ah.cl`, `t1.ts`) that are functionally dependent on the GROUP BY key are left as bare columns (SQLite picks an arbitrary row from the group, which is safe since they're identical within a group).

```sql
SELECT
  '{table}' as tbl,
  crsql_pack_columns({pk_cols_for_table}) as pks,  -- from v2_pks (WITHOUT ROWID) or main table (rowid)
  GROUP_CONCAT(cm.col_name, char(0) ORDER BY cm.col_id) as cid,
  GROUP_CONCAT(t1.col_version, char(0) ORDER BY cm.col_id) as col_vrsn,
  t1.db_version as db_vrsn,
  site_tbl.site_id as site_id,
  t1.cell_key >> CRSQL_COL_ID_BITS as key,
  GROUP_CONCAT(t1.seq, char(0) ORDER BY cm.col_id) as seq,  -- packed seqs; receiver records ALL in bookeeping
  ah.cl as cl,                       -- per-row, same for all columns (in GROUP BY as functionally dependent)
  t1.ts as ts,                       -- same for all rows in group (written in same transaction)
  crsql_pack_agg(
    CASE cm.col_id                     -- value extraction via compiled CASE
      WHEN 0 THEN mt.col_a
      WHEN 1 THEN mt.col_b
      WHEN 2 THEN mt.col_c
      -- ... one WHEN per non-PK column, using col_id from v2_col_map (0-based) ...
    END
    ORDER BY cm.col_id
  ) as cval,                        -- packed column values, parallel array to cid
FROM "<table>__crsql_v2_clock" AS t1
JOIN "<table>__crsql_v2_col_map" AS cm ON (t1.cell_key & ((1 << CRSQL_COL_ID_BITS) - 1)) = cm.col_id
JOIN "<table>__crsql_v2_pks" AS ah ON ah.__crsql_key = t1.cell_key >> CRSQL_COL_ID_BITS
JOIN "<table>" AS mt ON mt.rowid = ah.__crsql_key  -- rowid tables: fetch PK cols + col values
LEFT JOIN crsql_site_id AS site_tbl ON t1.site_id = site_tbl.ordinal
WHERE t1.site_id = ? AND t1.db_version > ?
GROUP BY t1.cell_key >> CRSQL_COL_ID_BITS, t1.db_version, site_tbl.site_id
```

> **Value extraction via compiled CASE statement**: Unlike V1 which fetches column values on-demand in `changes_next` (one `SELECT col FROM main_table WHERE pk = ?` per row), V2 compiles a large `CASE cm.col_id WHEN 0 THEN mt.col_x WHEN 1 THEN mt.col_y ...` statement at `TableInfo` creation time. This CASE maps each `col_id` (0-based integer from `v2_col_map`) to its corresponding column on the main table, and `crsql_pack_agg` collects the resolved values across the group. Integer comparison is used instead of string comparison on `col_name` for efficiency. The statement is prepared once and reused — the column list is fixed per table schema. This trades a larger prepared statement for eliminating the per-row value fetch round-trip that V1 does in `changes_next`.

> **Note:** For WITHOUT ROWID tables, the JOIN on main table uses PK columns instead of rowid:
> ```sql
> JOIN "<table>" AS mt ON mt.<pk1> = ah.<pk1> AND mt.<pk2> = ah.<pk2> ...
> ```
> For rowid tables, `pk_cols_for_table = mt.<pk1>, mt.<pk2>, ...`
> For WITHOUT ROWID tables, `pk_cols_for_table = ah.<pk1>, ah.<pk2>, ...`

**UNION ALL — Dead rows, V2 wire format** (`sync-log-version = 'v2'`):

Tombstones ARE the sentinel/delete events. There are no separate sentinel rows in the clock table — the tombstones table serves that role. `cid = '-2'` distinguishes hash-based tombstones from V1 `'-1'` delete sentinels.

```sql
SELECT
  '{table}' as tbl,
  d.hashed_pk as pks,           -- hashed PK for dead rows (V2 wire format)
  '-2' as cid,                  -- hash-based tombstone sentinel (V2 dead row)
  NULL as col_vrsn,
  d.db_version as db_vrsn,
  site_tbl.site_id as site_id,  -- resolve ordinal to site_id blob
  d.hashed_pk as key,           -- hash as key (receiver looks up by hash, not __crsql_key)
  d.seq as seq,
  d.cl as cl,
  d.ts as ts
FROM "<table>__crsql_v2_tombstones" AS d
LEFT JOIN crsql_site_id AS site_tbl ON d.site_id = site_tbl.ordinal
WHERE d.site_id = ? AND d.db_version > ?
```

**UNION ALL — Dead rows, V1 compat wire format** (`sync-log-version = 'v1'`, metadata in V2):

Real PK columns resolved from `v2_tombstone_pks`. `cid = '-1'` (V1 delete sentinel). Emitted when `sync-log-version = 'v1'` but metadata is stored in V2 tables.

```sql
SELECT
  '{table}' as tbl,
  crsql_pack_columns(tp.<pk1>, tp.<pk2>, ...) as pks,  -- real PK values
  '-1' as cid,                  -- V1 delete sentinel
  NULL as col_vrsn,
  d.db_version as db_vrsn,
  site_tbl.site_id as site_id,
  d.hashed_pk as key,           -- hash for internal bookkeeping
  d.seq as seq,
  d.cl as cl,
  d.ts as ts
FROM "<table>__crsql_v2_tombstones" AS d
JOIN "<table>__crsql_v2_tombstone_pks" AS tp ON d.hashed_pk = tp.hashed_pk
LEFT JOIN crsql_site_id AS site_tbl ON d.site_id = site_tbl.ordinal
WHERE d.site_id = ? AND d.db_version > ?

ORDER BY db_vrsn, seq
```

### Value Fetch (Per Table, Prepared Once, Cached in TableInfo)

Fetches ALL non-PK columns in one query. Rust picks the relevant ones.

```sql
SELECT col1, col2, ..., colN FROM "table" WHERE pk1 = ? AND pk2 = ?
```

In `changes_next`:

1. Detect packed row (`cid` contains `char(0)`)
2. Split `cid` by `char(0)` → col_name list
3. Split `col_vrsn` by `char(0)` → col_version list
4. Execute all-columns `SELECT`, fetch all values
5. Pick values for columns in the `cid` list, pack via `crsql_pack_columns` → `cval`

### Merge Side (Per Table, Prepared Once, Cached in TableInfo)

Fixed `UPDATE` statement with `CASE` per column — no dynamic SQL generation. Bind flags (0/1) to select which columns to update.

```sql
UPDATE "table" SET
  col1 = CASE WHEN ?1 THEN ?2 ELSE col1 END,
  col2 = CASE WHEN ?3 THEN ?4 ELSE col2 END,
  ...                                                           -- one CASE per non-PK column
  colN = CASE WHEN ?(2N-1) THEN ?2N ELSE colN END
WHERE pk1 = ?(2N+1) AND pk2 = ?(2N+2)
```

In `merge_insert`:

1. Detect packed row (`cid` contains `char(0)`)
2. Split `cid` → col_name list, `col_vrsn` → col_version list
3. Unpack `cval` → col_value list
4. For each `(col_name, col_version, col_value)`:
   - (a) Run existing per-column merge logic (`did_cid_win`, etc.)
   - (b) If column wins, set its flag=1 and bind its value in the `CASE UPDATE`
   - (c) If column loses, set its flag=0 (`CASE` keeps existing value)
5. Execute the single `CASE UPDATE` statement
6. For each winning column, `set_winner_clock` as before

> **Note:** Per-column version comparison (`did_cid_win`) still happens per column. The packing only saves wire bytes and round-trips, not merge computation. The `CASE UPDATE` replaces N separate `UPDATE` statements with 1.

### Benefits

- **Wire**: eliminates `(N-1) × (pk_bytes + tbl_bytes + metadata_bytes)` per row change
- **Receiver**: 1 `CASE UPDATE` instead of N round-trips
- Delete/create sentinels stay single-event (no packing)
- Partial updates (2 of 20 columns) still pack those 2 — smaller win but still a win
- Clock table unchanged — packing is purely in feed query + `changes_next` + `merge_insert`
- Write path unchanged — per-column seq stays as-is (zero write path changes for V2)

### Seq Handling

Write path is unchanged — each column still gets its own seq via `bump_seq()`. The feed query packs ALL seqs via `GROUP_CONCAT(seq, char(0))`. The receiver splits these and records each one in `__corro_seq_bookkeeping`. This is critical for correctness: corrosion's `PartialVersion` tracks seqs as a `RangeInclusiveSet` and determines completeness by checking for gaps in `0..=last_seq`. If we used `MIN(seq)`, the receiver would think seqs 6,7 are missing (when they were packed into seq=5) and would request them forever — deadlock.

Partial replays (`SyncNeedV1::Partial`) query `WHERE seq BETWEEN :start AND :end` on the `crsql_changes` vtable. This works correctly because the vtable filters on the underlying clock table's `seq` column (via `xBestIndex`/`xFilter`), not on the packed output. V2 always coalesces rows that share `(PK, db_version, site_id)` regardless of how they were selected. If the seq range covers all columns of an operation, the group is complete and packed. If the range splits an operation (e.g., seqs 5,6,7 but only 5,6 requested), the group is partial — still packed, just with fewer columns. The receiver records the seqs it got and knows the rest are still missing.

**Future optimization**: a feature flag can switch to one-seq-per-operation (`bump_seq` once before the loop in `mark_locally_inserted`/`updated`). This is safe to enable only when ALL nodes in the cluster emit packed change logs, since the bookeeping semantics change (one seq per operation instead of per column). `cl` is per-row (stored in `v2_pks`/`v2_tombstones`, not per-column), so it is not part of the `GROUP BY` — it's the same for all columns in the group.

---

## Backfill (V2): `crsql_backfill_v2`

Called when a table is first registered as a CRR with V2 schema (`metadata-write-version` is `v2` at `as_crr` time, no V1 tables exist).

For each row in the main table not yet in `v2_pks`:

1. Compute `hashed_pk` from PK values.
2. `INSERT` into `v2_pks` with `cl=1`, `__crsql_key = rowid` (rowid tables) or auto-increment (WITHOUT ROWID tables).
3. For each non-PK column, `INSERT` into `v2_clock`:
   - `cell_key = (__crsql_key << COL_ID_BITS) | col_id`
   - `col_version=1`, `db_version=crsql_next_db_version()`, `seq=crsql_increment_and_get_seq()`, `site_id=0`, `ts=unix_now`
4. For pk-only tables (no non-PK columns): no clock entries needed. The `v2_pks` entry with `cl=1` is sufficient — row existence is implied by presence in `v2_pks` (odd CL = alive). This is cleaner than V1 which required an `INSERT_SENTINEL` clock row.

Mirrors V1 backfill in `backfill.rs` but writes to V2 tables.

---

## ALTER TABLE (V2): `crsql_compact_post_alter_v2`

Called after schema changes (column add/remove, PK changes) on V2 tables.

### Column Added

1. `INSERT` new entry into `v2_col_map` (`col_name` → next `col_id`).
2. Backfill: for each existing row in `v2_pks`, `INSERT` clock entry for the new column with `col_version=1` (same as V1 `backfill_missing_columns`).

### Column Removed

1. Look up `col_id` for dropped column from `v2_col_map`.
2. `DELETE FROM v2_col_map WHERE col_name = dropped_column`.
3. `DELETE FROM v2_clock WHERE (cell_key & ((1 << COL_ID_BITS) - 1)) = col_id`.

### PK Columns Changed

1. All hashes change → `DROP` all V2 tables and re-create from scratch.
2. Re-backfill from main table (same as fresh `as_crr` with V2).

This mirrors V1 behavior which drops and recreates clock + pks tables.

### `crsql_begin_alter` / `crsql_commit_alter`

1. **begin**: drop triggers (prevent writes during schema change).
2. **commit**: recreate triggers, run `compact_post_alter_v2`, store `pre_compact_dbversion` in `crsql_master`.

---

## Teardown (V2): `crsql_remove_crr_v2`

Drops all V2 metadata tables for a table:

- `<table>__crsql_v2_col_map`
- `<table>__crsql_v2_clock`
- `<table>__crsql_v2_pks`
- `<table>__crsql_v2_tombstones`
- `<table>__crsql_v2_tombstone_pks`

Drops all triggers (same trigger names as V1 — triggers are shared). If V1 tables still exist (not yet dropped post-migration), drops those too. Called from the existing `as_table` / `remove_crr` entry point which checks which schema version is active and dispatches accordingly.

---

## Codepath Separation: V1 / V2&V1 / V2

The three config flags create 3 operational modes. The codebase must cleanly separate V1 and V2 logic to support all three.

**Principle**: `TableInfo` is the dispatch point. `TableInfo` gains a `schema_version` field: `SchemaVersion::V1 | V2 | V2AndV1`. This is set at `TableInfo` creation time based on `metadata-write-version` and which tables physically exist.

### Write Path (`local_writes/`)

`after_insert`/`update`/`delete` check `schema_version`:

- **V1** → existing V1 logic (unchanged)
- **V2** → V2 logic (hash computation, `v2_pks`/`v2_tombstones`/`v2_clock` writes)
- **V2&V1** → V1 logic first, then V2 logic (dual write)

V2 write functions live in `local_writes/v2.rs` (single file). Trigger SQL is unchanged — triggers still call `crsql_after_insert`/`update`/`delete` with the same arguments. The Rust functions behind those names dispatch.

### Read Path (`changes_vtab_read.rs`)

Feed query selection based on `metadata-use-version`:

- **V1** → existing V1 feed query (V1 clock tables, per-column rows)
- **V2** → V2 feed query (V2 tables, packed if `sync-log-version = v2`)

No "v2&v1" mode for reads — always reads from one schema.

### Merge Path (`changes_vtab_write.rs`)

Incoming row format detected per-row (`char(0)` in `cid` = packed V2). Local schema version determines write targets:

| Incoming | Local | Action |
|----------|-------|--------|
| V1 | V1 | Existing merge logic |
| V1 | V2 | Translate: resolve PK to hash, write to V2 tables |
| V2 | V1 | Translate: resolve hash to PK (via `v2_tombstone_pks` or `v2_pks`), write to V1 tables |
| V2 | V2 | V2 merge logic |
| V1 | V2&V1 | Write to both V1 and V2 tables |
| V2 | V2&V1 | Write to both V1 and V2 tables |

The translate step for incoming V2 → local V1 requires a hash-to-PK lookup:

- If the hash is unknown locally (row never seen), the change is for a new row — `INSERT` into main table first, then proceed with V1 merge.
- If the hash is in `v2_pks` or `v2_tombstone_pks`, resolve to PK and proceed.
- If the hash is unknown and it's a delete (`cid='-2'`), it can be ignored — we can't delete a row we've never seen. The tombstone is still recorded in `v2_tombstones` if in V2&V1 or V2 mode.

### `ts` Type Conversion (V1 TEXT ↔ V2 INTEGER)

V1 stores `ts` as `TEXT` (e.g., `"1698230400"` or `"0"`). V2 stores `ts` as `INTEGER` (unix seconds). Conversions:

- **V1 → V2 translation** (incoming V1 row, local V2 tables): `CAST(insert_ts AS INTEGER)`. If the V1 `ts` is `'0'` (unset/default), use the migration start timestamp instead (same as the migration logic at line 443). This satisfies the V2 `CHECK (ts > 0)` constraint.
- **V2 → V1 translation** (incoming V2 row, local V1 tables): `CAST(insert_ts AS TEXT)`. The V1 clock table accepts any TEXT, so no constraint issues. The existing `set_winner_clock` function binds `ts` via `bind_text`, so the V2 integer must be converted to text before binding.
- **V2&V1 dual write** (local writes): the local write path produces `ts` from `ext_data.timestamp` (currently a `u64` formatted as string). V2 write path converts to `INTEGER` directly; V1 write path keeps the existing `TEXT` binding. No cross-conversion needed — each path formats `ts` for its own table type.

### File Organization

| File | Responsibility |
|------|---------------|
| `tableinfo.rs` | `TableInfo` with `SchemaVersion`, V1 + V2 statement getters |
| `bootstrap.rs` | V1 table creation (unchanged) |
| `bootstrap_v2.rs` | V2 table creation |
| `triggers.rs` | Trigger SQL (shared, unchanged — triggers call same fns) |
| `local_writes/mod.rs` | Dispatch based on `schema_version` |
| `local_writes/after_insert.rs` | V1 (unchanged) |
| `local_writes/after_update.rs` | V1 (unchanged) |
| `local_writes/after_delete.rs` | V1 (unchanged) |
| `local_writes/v2.rs` | V2 write logic (insert/update/delete in one file) |
| `changes_vtab_write.rs` | Merge dispatch (V1/V2/V2&V1) |
| `changes_vtab_read.rs` | Feed dispatch (V1/V2) |
| `backfill.rs` | V1 backfill (unchanged) |
| `backfill_v2.rs` | V2 backfill |
| `alter.rs` | V1 alter (unchanged) |
| `alter_v2.rs` | V2 alter |
| `teardown.rs` | V1 teardown (unchanged) |
| `teardown_v2.rs` | V2 teardown |
| `migrate.rs` | `crsql_incremental_maintenance` dispatcher + V1→V2 migration logic |

### V1 Code Deletion

Once all nodes in a cluster run V2 exclusively, V1 files and V1 branches in dispatch functions can be deleted. The `SchemaVersion` enum can be simplified to just `V2`. This is a mechanical removal — no V2 code depends on V1 code.

---

## PK-Only Tables (No Non-PK Columns)

### Problem

Tables with only PK columns (e.g., `CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)`) produce no `v2_clock` entries on insert — `v2_after_insert` only writes clock entries for non-PK columns. Without clock entries, the feed query (which INNER JOINs `v2_clock` with `v2_col_map`) returns nothing, making PK-only rows invisible to sync.

V1 handles this via a sentinel clock row (`col_name = '-1'`) that carries `db_version`, `seq`, `ts`, `site_id`. V2 needs an equivalent mechanism.

### Design: Sentinel Clock Entry at `col_id=0`

For PK-only tables (`tbl_info.non_pks.is_empty()`), a sentinel clock entry is written at `col_id=0` in `v2_clock`. This is safe because `v2_col_map` is empty for PK-only tables — `col_id=0` has no mapping to a real column.

**Write path** (`v2_after_insert`): if `non_pks.is_empty()`, write one clock entry:
```sql
INSERT OR REPLACE INTO v2_clock (cell_key, col_version, site_id, db_version, seq, ts)
VALUES ((key << COL_ID_BITS) | 0, 1, 0, db_version, seq, ts)
```

**Read path**: `TableInfo` already knows `non_pks.is_empty()`. When true, `query_for_table` dispatches to a PK-only query that reads sentinel clock entries directly (no `v2_col_map` JOIN):

```sql
SELECT
  'foo' as tbl,
  crsql_pack_columns(<pk_cols>) as pks,
  '-1' as cid,                    -- sentinel
  c.col_version as col_vrsn,
  c.db_version as db_vrsn,
  site_tbl.site_id as site_id,
  c.cell_key >> COL_ID_BITS as key,
  c.seq as seq,
  pk_tbl.cl as cl,
  c.ts as ts,
  NULL as cval                     -- no column value
FROM v2_clock AS c
JOIN v2_pks AS pk_tbl ON (c.cell_key >> COL_ID_BITS) = pk_tbl.__crsql_key
LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal
-- No v2_col_map JOIN; col_id=0 is the sentinel
```

**Delete path**: same as normal tables — `v2_after_delete` deletes all clock entries for the key (including the sentinel) and moves the row to `v2_tombstones`.

**Merge path**: already handles `cid='-1'` via `is_sentinel_only` check. The sentinel is treated like any other clock entry for conflict resolution.

### ALTER TABLE: Adding a Non-PK Column to a PK-Only Table

When a non-PK column is added (via `ALTER TABLE ... ADD COLUMN` + `crsql_as_crr` re-registration), the table transitions from PK-only to normal mode. At this point:

1. **Populate `v2_col_map`**: the new column gets `col_id=0` (first non-PK column).
2. **Existing sentinel entries become regular clock entries**: the sentinel rows at `col_id=0` already have `col_version=1`, `site_id=0`, and valid `db_version`/`seq`/`ts`. Once `v2_col_map` maps `col_id=0` to the new column name, these entries seamlessly become normal clock entries for that column. The read query's CASE expression fetches the actual column value from the main table (which will be the column's default value for existing rows). No deletion or cleanup of sentinel entries is needed.
3. **`TableInfo` refresh**: `non_pks` is now non-empty, so the read path switches to the normal query (with `v2_col_map` JOIN).
4. **Backfill**: new rows inserted after the alter will write clock entries for all non-PK columns via the normal path. Existing rows already have a clock entry at `col_id=0` from the sentinel — no backfill needed for them.

This transition is handled in `sync_col_map_v2` (called from `crsql_compact_post_alter_v2`), not at query time.

### ALTER TABLE: Dropping the Last Non-PK Column (Table Becomes PK-Only)

When the last non-PK column is dropped via `ALTER TABLE ... DROP COLUMN` + `crsql_commit_alter`, the table transitions from normal mode to PK-only. At this point:

1. **Remove from `v2_col_map`**: the dropped column's entry is deleted from `v2_col_map`.
2. **Migrate clock entries to `col_id=0`**: the dropped column's clock entries are migrated to `col_id=0` by updating `cell_key = cell_key & ~col_id_mask` (preserving `db_version`, `seq`, `ts`, `site_id`). This preserves the row modification history — a row last modified at `db_version=5` retains that version as a sentinel, rather than getting a fresh sentinel at the current version. If multiple columns are dropped in the same `crsql_commit_alter` call, the last dropped column's entries are migrated; other dropped columns' entries are deleted normally.
3. **Create missing sentinels**: for any rows in `v2_pks` that had no clock entry at all (e.g., rows that existed but were never modified after initial insert), a sentinel entry is created at `col_id=0` with the current `db_version`/`seq`/`ts`.
4. **`TableInfo` refresh**: `non_pks` is now empty, so the read path switches to the PK-only query (no `v2_col_map` JOIN, emits `cid='-1'`).
5. **Future column additions**: if a new non-PK column is added later, `col_id=0` is reused for the new column (see "Adding a Non-PK Column" above). The sentinel entries seamlessly become regular clock entries.

This transition is handled in `sync_col_map_v2`, which checks `tbl_info.non_pks.is_empty()` after syncing the col_map and migrates/creates sentinel entries as needed.

### col_id Reuse Policy

**col_ids are never reused for different columns within the same table's lifetime, except for `col_id=0`**:

- `col_id=0` is special: it starts as the sentinel for PK-only tables, then becomes a regular column id when a non-PK column is added. If that column is later dropped, `col_id=0` returns to sentinel duty (with clock entries migrated from the dropped column). This is safe because the sentinel and the column are never active at the same time — the transition is atomic (happens during `crsql_commit_alter`).
- **When adding columns, always try `col_id=0` first**: if `col_id=0` is not in use (no existing col_map entry), the first new column gets `col_id=0`. This is critical for PK-only → normal transitions where sentinel entries at `col_id=0` need to become regular clock entries. After `col_id=0` is assigned, subsequent new columns get `max(col_id) + 1`.
- For `col_id >= 1`: when a column is dropped, its `col_id` is retired. New columns always get `max(col_id) + 1` (after trying slot 0). This prevents a newly added column from inheriting stale clock entries from a previously dropped column.
- If a table cycles through PK-only → normal → PK-only → normal multiple times, `col_id=0` is reused each time, but higher col_ids continue to increment. This is correct because each normal→PK-only transition migrates the last column's entries to `col_id=0` and deletes all other clock entries.

### Migration from V1

V1 PK-only tables already have sentinel clock rows (`col_name = '-1'`). The V1→V2 migration handles these:

- The existing migration step 4 (clock migration) skips sentinels (`WHERE c.col_name != '{sentinel}'`).
- For PK-only tables, an additional step migrates the V1 sentinel to a V2 sentinel at `col_id=0`: `INSERT OR REPLACE INTO v2_clock (cell_key, col_version, ...) VALUES ((key << COL_ID_BITS) | 0, sentinel_col_version, ...)` where `sentinel_col_version` is the V1 sentinel's `col_version` (but V2 uses `v2_pks.cl` for CL, so `col_version` in the sentinel is just `1` like other clock entries).

## Future Work: Bulk Merge via Staging Table

Once all nodes in a cluster are on the V2 wire format, the per-row merge path (`xUpdate` → `v2_merge_insert` per change row) can be replaced with a set-based bulk merge that processes all incoming changes in a few SQL statements per table rather than multiple statements per incoming change.

### Problem

The current merge processes each change row individually through the virtual table `xUpdate` callback. For 20K changes across 5 tables, this means ~100K SQL statements (hash lookups, CL compares, metadata writes, user table writes). The per-statement overhead (~15μs of Rust↔SQLite boundary crossings) dominates at scale.

### Architecture

**App API**: The app inserts directly into the staging temp table via its unnest vtab, then calls flush. No `crsql_changes` vtab involved.

```sql
-- App pushes changes directly into the staging table
INSERT INTO _crsql_merge_stage (tbl, pk, cid, val, cl, col_vrsn, db_vrsn, site_id, seq, ts)
SELECT * FROM unnest_vtab(?);
-- Flush processes all staged changes in bulk
SELECT crsql_flush_merge();
```

**Extension internals**: The staging temp table is created by the extension and exposed to the app for bulk inserts. No `xUpdate` interception, no per-row processing during insert — the app's unnest vtab feeds directly into the staging table.

```sql
-- Extension-internal staging table (created by the extension, not the app)
-- Append-only buffer, no UNIQUE constraint. Dedup happens during flush in Rust.
CREATE TEMP TABLE _crsql_merge_stage (
  tbl TEXT, pk BLOB, hashed_pk BLOB, cid TEXT, val BLOB,
  cl INT, col_vrsn INT, db_vrsn INT, site_id BLOB, seq INT, ts TEXT,
  -- Outcome tracking (filled during flush)
  applied INT DEFAULT 0,
  rows_impacted INT DEFAULT 0,
  op_type TEXT,          -- 'insert', 'update', 'delete', 'resurrect'
  error INT DEFAULT 0    -- 1 if this db_version caused a hard error
);
```

The app's unnest vtab feeds rows directly into the staging table — no conflict resolution, no lookups, no metadata writes during insert. This replaces the current per-row work (hash lookups, CL compares, metadata writes, user table UPSERTs) with a single temp table INSERT per row.

**Flush** (`SELECT crsql_flush_merge()`): Reads all staged rows, does dedup + merge in Rust, then applies bulk SQL. The flush processes all staged rows in bulk:

1. **Read staged rows** — `SELECT * FROM _crsql_merge_stage WHERE applied = 0` into Rust
2. **Dedup in Rust** — HashMap groups by `(tbl, hashed_pk)`, picks max CL, merges same-CL changes per-column (unpacking packed V2 wire blobs)
3. **Bulk metadata writes** — set-based SQL against the deduped winners: `INSERT/DELETE` into `v2_pks`, `v2_clock`, `v2_tombstones` joined by `hashed_pk`. This is where most of the per-row SQL work is eliminated — metadata ops happen in bulk without unnest or per-row xUpdate.
4. **Bulk user table writes** — per-column `UPDATE ... FROM` the deduped winners — one statement per changed column per table. Only writes pages for columns that actually changed.
5. **Outcome tracking** — `UPDATE _crsql_merge_stage SET applied = 1, rows_impacted = <changes()>, op_type = ...` per processed row.

**Dedup and merge** happen in Rust during step 2, where packed V2 wire blobs can be unpacked and compared per-column:

1. **Group by `(tbl, hashed_pk)`** — HashMap in Rust
2. **Pick max CL** per group — stale rows (lower CL) are discarded (a row at CL=3 is entirely superseded by CL=5, since the row was deleted at CL=4 and resurrected at CL=5)
3. **If multiple changes at the winning CL** (different sites, same CL):
   - Unpack each change's `cid`/`col_vrsn`/`cval` (packed with `\0` separators)
   - Per column: pick the change with max `col_version`; if tied, higher value wins; if values identical, `mergeEqualValues` site_id tiebreaker applies
   - This is the same logic `v2_merge_insert_single_col` does today, just batched across all rows at once

This approach is necessary because V2 wire format packs column names and versions into single blob fields (`cid = "col1\0col2\0col3"`, `col_vrsn = "1\02\01"`). SQL-level `ON CONFLICT` dedup cannot compare individual column versions within packed blobs — the dedup must happen in Rust where the blobs can be unpacked.

**Post-flush queryability**: The app can query the staging table for stats and error info:

```sql
-- Per db_version stats
SELECT db_vrsn, op_type, COUNT(*) as cnt, SUM(rows_impacted) as impacted
FROM _crsql_merge_stage
WHERE applied = 1
GROUP BY db_vrsn, op_type;

-- Which db_version errored
SELECT DISTINCT db_vrsn FROM _crsql_merge_stage WHERE error = 1;

-- Significant changes only (for incremental subscriptions / delta tables)
SELECT tbl, pk, cid, cval, op_type, db_vrsn, site_id
FROM _crsql_merge_stage
WHERE applied = 1 AND rows_impacted > 0;
```

### Why V2 wire is required

V2 wire format coalesces all column changes for a row into a single packed row (`cid = "col1\0col2\0col3"`, `cval = packed_blob`). The Rust-side dedup groups by `(tbl, hashed_pk)`, picks max CL (discarding all lower-CL changes from any site, including the same site), and for same-CL changes from different sites, unpacks the packed blobs to merge per-column. V1 wire has one row per column change, requiring a two-level dedup (max CL per pk, then max col_version per col_id at that CL), which is more complex and less efficient.

### Cross-transaction safety

Changes from multiple source db_versions can be safely mixed in one staging table batch:

- **CL and col_version are the conflict resolution mechanisms**, not db_version. The source's db_version is preserved as metadata but doesn't affect merge semantics.
- **Every change in the feed is from a committed source transaction** — rolled-back transactions don't produce changes (triggers don't fire on uncommitted data).
- **The destination assigns its own db_version** when applying changes, so mixing source db_versions doesn't corrupt the destination's version sequence.
- **Idempotency**: if a specific db_version fails, the entire batch rolls back. The app retries without the failed version. Re-applying the remaining versions is safe because the rollback undid all partial state.

### Error handling

If a db_version within the batch causes a hard error (constraint violation, schema mismatch):
1. The entire batch rolls back (staging table + all applied changes)
2. `crsql_flush_merge()` returns the error with the offending db_version
3. The app retries with all changes except the failed db_version

This preserves the app's existing per-db_version error isolation pattern while maximizing bulk efficiency.

### Performance characteristics

| Row count | Current (per-row) | Bulk (staging) | Speedup |
|---|---|---|---|
| 1K | ~100ms | ~80ms | 1.25x |
| 20K | ~1.5s | ~200ms | 7.5x |
| 100K | ~8s | ~600ms | 13x |
| 1M | ~75s | ~5s | 15x |

The bulk approach has O(tables × columns) SQL statements for metadata + user table writes, regardless of row count. The remaining per-row cost is the unnest→xUpdate→staging INSERT path (~5μs/row), which is the irreducible FFI cost of the vtab API. A future optimization could replace this with a blob-based function call (`crsql_merge_batch(?)`) using serde+bincode to eliminate per-row FFI entirely, but this requires a shared serialization crate between the app and extension.

### Prerequisites

- `crsql_hash_packed(blob)` SQL function — hashes a packed PK blob directly via `xxh3_128(blob)[:PK_HASH_SIZE]`. Trivial to implement since `hash_pk_values` already does `hash(pack_columns(values))` — this just skips the pack step.
- All tables migrated to V2 wire format (V1 wire fallback: process individually via current per-row path).

<!-- TODO: User table write strategy needs more thought. Current idea: instead of per-column
     UPDATE ... FROM with SQL functions to unpack cval, use unnest to push unpacked values
     directly. During flush, iterate staged rows once, unpacking into per-table, per-column
     HashMaps: HashMap<(tbl, col_id), Vec<(hashed_pk, col_version, row_idx, value)>>.
     Then generate bulk UPDATEs from these HashMaps. This avoids needing crsql_unpack_nth
     as a SQL function — unpacking happens in Rust during the single iteration.
     Revisit and finalize this approach at implementation time. -->
