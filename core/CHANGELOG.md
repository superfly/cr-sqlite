# @vlcn.io/crsqlite

## 0.18.0

### Breaking Changes

- Removed `crsql_automigrate` function. Schema management should be handled externally by the application or migration framework. Corrosion handles schema migrations externally and uses `crsql_as_crr()` to register new tables and `crsql_begin_alter()` / `crsql_commit_alter()` to reconcile cr-sqlite metadata tables after schema changes.
- `crsql_set_ts()` is now mandatory before any write operation (crsql_as_crr, crsql_begin_alter/commit_alter, crsql_incremental_maintenance, INSERT/UPDATE/DELETE on CRR tables, INSERT INTO crsql_changes). Operations will fail with an error if the timestamp is not set.
- Requires SQLite 3.44.0 or later.

### New Features

- **V2 metadata format**: New compact metadata tables (`__crsql_v2_pks`, `__crsql_v2_clock`, `__crsql_v2_col_map`, `__crsql_v2_tombstones`, `__crsql_v2_tombstone_pks`) with packed integer `cell_key` primary keys. Replaces V1's `__crsql_clock` + `__crsql_pks` schema. Opt-in via `crsql_config_set('metadata-write-version', 2)`. Note: the V2 schema may receive breaking changes during the 0.18.x series. V1 support will be removed in 0.19.0.
- **V2 wire format**: Coalesces all column changes per row per db_version into a single `crsql_changes` row with packed `cid` and `cval` fields. Opt-in via `crsql_config_set('sync-log-version', 2)`.
- **Hashed primary keys**: V2 hashes PK values with `xxh3_128` (truncated to 10 bytes) to limit tombstone size. Tombstones moved to a dedicated `v2_tombstones` table, reducing clock table bloat.
- **Incremental V1→V2 migration**: `crsql_incremental_maintenance(batch_size)` migrates tables in batches without long lock times. Supports dual-write mode (V1+V2) and rollback to V1 while in dual-write.
- **Simplified PK change detection**: PK column info is now stored in `crsql_master` at table creation time and compared via a canonical signature (rowid-key flag + sorted PK column names + types). Eliminates complex schema introspection queries.

### Improvements

- Simplified `sync_col_map_v2`: linear col_id assignment, removed dead sentinel-creation code, removed unused parameters.
- Fixed teardown: removed dead per-PK-column trigger drop loop (only one `{table}__crsql_utrig` trigger is ever created per table).

## 0.17.0

### Breaking Changes

- Forked from upstream cr-sqlite v0.15.0. Databases created with cr-sqlite < 0.17.0 are not supported and must be migrated.
- `crsql_next_db_version()` no longer accepts a `merging_version` argument. The db_version is committed to storage immediately when computed.
- Clock table schema changed: `ts TEXT NOT NULL DEFAULT '0'` column added to all `__crsql_clock` tables. `PRIMARY KEY (key, col_name)` with `WITHOUT ROWID, STRICT`.
- db_version index changed from `(db_version)` to `(site_id, db_version)`.
- Removed `crsql_tracked_peers` built-in peer tracking. Applications are now responsible for gap detection, seq tracking, and buffering.

### New Features

- **Per-site DB version tracking**: New `crsql_db_versions` table tracks the latest `db_version` seen from each site. New functions `crsql_peek_next_db_version()` and `crsql_set_db_version(site_id, db_version)`.
- **Timestamps**: Every change record now carries a `ts` column (NTP64 timestamp string). Set per-transaction via `crsql_set_ts()`, read via `crsql_get_ts()`. Enables time-based retention policies.
- **In-memory caching**: Site ordinal cache, causal length cache (max 1500 entries per TableInfo), and last db_versions map — all transaction-scoped with proper commit/rollback lifecycle.
- **Debug logging**: `crsql_set_debug(1)` enables libc_print-based debug output.
- **ASAN support**: `make asan` target with proper Rust sanitizer flags.

### Improvements

- **Optimized local write path**: Insert uses UPDATE-then-INSERT fallback with combo-insert fast path. Update uses `peek_next_db_version()` to skip version increment when nothing changed. Delete returns new causal length via `RETURNING`. PK change moves clock rows via `UPDATE OR REPLACE` preserving col_version.
- **Merge write path**: `set_winner_clock` takes `insert_ts` parameter. `zero_clocks_on_resurrect` no longer sets db_version during resurrect. Unified merge path for sentinel-only, resurrect, and normal cases.
- **Config lifetime fix**: `crsql_config_set` properly manages statement lifetime to prevent use-after-free.
- **`crsql_changes` schema**: Added `ts` column (column index 9).
- **`crsql_version()`**: Re-enabled (was commented out upstream).

## 0.16.3

### Patch Changes

- fix windows breakage, fix wasm oom

## 0.16.1

### Patch Changes

- fix install script for nodejs based projects

## 0.16.0

### Minor Changes

- e4a0a42: v0.16.0-next

### Patch Changes

- d485812: prepare `tables_used` query, correctly unzip native library from pre-builds
- 6f0ccac: fix error where separate connections would not report the correct db version

## 0.16.0-next.2

### Patch Changes

- fix error where separate connections would not report the correct db version

## 0.16.0-next.1

### Patch Changes

- prepare `tables_used` query, correctly unzip native library from pre-builds

## 0.16.0-next.0

### Minor Changes

- v0.16.0-next

## 0.15.1

### Patch Changes

- c113d8c: ensure statements are finalized when closing db, allow automigrating fractindex tables, fractindex w/o list columns fix

## 0.15.1-next.0

### Patch Changes

- ensure statements are finalized when closing db, allow automigrating fractindex tables, fractindex w/o list columns fix

## 0.15.0

### Minor Changes

- 56df096: re-insertion, api naming consistencies, metadata size reduction, websocket server, websocket client, websocket demo

### Patch Changes

- 4022bd6: litefs support
- 08f13fb: react strict mode fiex, migrator fixes, typed-sql basic support, ws replication, db provider hooks
- f327068: rebuild

## 0.15.0-next.2

### Patch Changes

- litefs support

## 0.15.0-next.1

### Patch Changes

- react strict mode fiex, migrator fixes, typed-sql basic support, ws replication, db provider hooks

## 0.15.0-next.0

### Minor Changes

- re-insertion, api naming consistencies, metadata size reduction, websocket server, websocket client, websocket demo

## 0.14.0

### Minor Changes

- 68deb1c: binary encoded primary keys, no string encoding on values, cache prepared statements on merge, fix webkit JIT crash

## 0.14.0-next.0

### Minor Changes

- binary encoded primary keys, no string encoding on values, cache prepared statements on merge, fix webkit JIT crash

## 0.13.0

### Minor Changes

- 62912ad: split up large transactions, compact out unneeded delete records, coordinate dedicated workers for android, null merge fix

## 0.13.0-next.0

### Minor Changes

- split up large transactions, compact out unneeded delete records, coordinate dedicated workers for android, null merge fix

## 0.12.0

### Minor Changes

- 7885afd: 50x perf boost when pulling changesets

## 0.12.0-next.0

### Minor Changes

- 15c8e04: 50x perf boost when pulling changesets

## 0.11.0

### Minor Changes

- automigrate fixes for WASM, react fixes for referential equality, direct-connect networking implementations, sync in shared worker, dbProvider hooks for React

### Patch Changes

- 4e737a0: better error reporting on migration failure, handle schema swap

## 0.10.2-next.0

### Patch Changes

- better error reporting on migration failure, handle schema swap

## 0.10.1

### Patch Changes

- fts5, sqlite 3.42.1, direct-connect packages

## 0.10.0

### Minor Changes

- e0de95c: ANSI SQL compliance for crsql_changes, all filters available for crsql_changes, removal of tracked_peers, simplified crsql_master table

### Patch Changes

- 9b483aa: npm is not updating on package publish -- bump versions to try to force it

## 0.10.0-next.1

### Patch Changes

- npm is not updating on package publish -- bump versions to try to force it

## 0.10.0-next.0

### Minor Changes

- ANSI SQL compliance for crsql_changes, all filters available for crsql_changes, removal of tracked_peers, simplified crsql_master table

## 0.9.3

### Patch Changes

- release lock fix

## 0.9.2

### Patch Changes

- e5919ae: fix xcommit deadlock, bump versions on dependencies

## 0.9.2-next.0

### Patch Changes

- fix xcommit deadlock, bump versions on dependencies

## 0.9.1

### Patch Changes

- 419ee8f: include makefile in pkg
- aad733d: --

## 0.9.1-next.1

### Patch Changes

- include makefile in pkg

## 0.9.1-next.0

### Patch Changes

---

## 0.9.0

### Minor Changes

- 14c9f4e: useQuery perf updates, primary key only table fixes, sync in a background worker

## 0.9.0-next.0

### Minor Changes

- useQuery perf updates, primary key only table fixes, sync in a background worker

## 0.8.0

### Minor Changes

- 6316ec315: update to support prebuild binaries, include primary key only table fixes

### Patch Changes

- b7e0b21df: create dist dir on install
- 606060dbe: include install script

## 0.8.0-next.2

### Patch Changes

- create dist dir on install

## 0.8.0-next.1

### Patch Changes

- include install script

## 0.8.0-next.0

### Minor Changes

- update to support prebuild binaries, include primary key only table fixes

## 0.7.2

### Patch Changes

- 3d09cd595: preview all the hook improvements and multi db open fixes
- 567d8acba: auto-release prepared statements
- 54666261b: fractional indexing inclusion
- fractional indexing, better react hooks, many dbs opened concurrently
- fd9094220: fixup what is packed

## 0.7.2-next.3

### Patch Changes

- preview all the hook improvements and multi db open fixes

## 0.7.2-next.2

### Patch Changes

- auto-release prepared statements

## 0.7.2-next.1

### Patch Changes

- fixup what is packed

## 0.7.2-next.0

### Patch Changes

- fractional indexing inclusion

## 0.7.1

### Patch Changes

- 519bcfc2a: hooks, fixes to support examples, auto-determine tables queried
- hooks package, used_tables query, web only target for wa-sqlite

## 0.7.1-next.0

### Patch Changes

- hooks, fixes to support examples, auto-determine tables queried

## 0.7.0

### Minor Changes

- seen peers, binary encoding for network layer, retry on disconnect for server, auto-track peers

## 0.6.3

### Patch Changes

- deploy table validation fix

## 0.6.2

### Patch Changes

- cid winner selection bugfix

## 0.6.1

### Patch Changes

- rebuild all

## 0.6.0

### Minor Changes

- breaking change -- fix version recording problem that prevented convergence in p2p cases

## 0.5.2

### Patch Changes

- fix gh #108

## 0.5.1

### Patch Changes

- fix mem leak and cid win value selection bug

## 0.5.0

### Minor Changes

- fix tie breaking for merge, add example client-server sync

## 0.4.2

### Patch Changes

- fix bigint overflow in wasm, fix site_id not being returned with changesets

## 0.4.1

### Patch Changes

- fix memory leak when applying changesets

## 0.4.0

### Minor Changes

- fix multi-way merge

## 0.3.0

### Minor Changes

- incorporate schema fitness checks

## 0.2.0

### Minor Changes

- update to use `wa-sqlite`, fix site id forwarding, fix scientific notation replication, etc.

## 0.1.8

### Patch Changes

- fix linking issues on linux distros

## 0.1.7

### Patch Changes

- fixes site id not being passed during replication
