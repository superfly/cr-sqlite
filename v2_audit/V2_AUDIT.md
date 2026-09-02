# cr-sqlite V2 Metadata — Audit Findings

Audit of the V2 metadata implementation in `core/rs/core/src/` against
`v2_metadata_design.md`, on branch `somtochi/v2-bugs` @ `3ee6c141`.

18 findings, all with an executable reproduction in `v2_audit/repros/`.
Each script exits **1** when the bug is present and **0** when it is not, so the
whole set doubles as a regression suite:

```sh
cd ~/workspace/superfly/cr-sqlite
v2_audit/run_all.sh              # runs every repro, prints a pass/fail table
/opt/homebrew/bin/python3 v2_audit/repros/<script>.py    # one at a time
```

Scripts use `/opt/homebrew/bin/python3` — the macOS system python is built
without `enable_load_extension` and cannot load `core/dist/crsqlite.dylib`.
Rebuild the extension with `cd core && make loadable` after any fix.

**Both existing test suites pass against this build**, so none of the findings
below are caught today:

```sh
cd core && make test                                   # Rust integration_check: all suites Success
cd py/correctness && ./my-venv/bin/python -m pytest tests -q   # 156 passed
```

(The `py/correctness` suite needs the project venv — a bare `pytest` fails
collection with `ModuleNotFoundError: crsql_correctness`, and still exits 0, so
it is easy to mistake for a pass.)

Coverage context: the Python suite never sets `metadata-write-version` /
`metadata-use-version` / `sync-log-version` at all, so it exercises only the V1
paths. The Rust suite does cover V2 (`v2_tests.rs`, `v2_compat_tests.rs`,
`skip_hash_tests.rs`, `rowid_check.rs`, `seeded_snapshot.rs`) and still misses
every finding here — the gaps are values ≥ 128, multi-column ALTER windows,
multi-node delete/resurrect races, and the packed feed's filter/order pushdown.

---

## Severity summary

| # | Severity | Finding | Repro |
|---|---|---|---|
| 1 | **CRITICAL** | `put_varint` writes continuation bits on the wrong bytes — every value ≥ 128 is corrupted on the V2 wire | `alter_01` |
| 2 | **CRITICAL** | A malformed blob from a peer **aborts the process** — uncatchable, and the poisoned change replays on every retry | `merge_01` |
| 3 | **CRITICAL** | Dropping the last two non-PK columns in one ALTER window fails and leaves the table with **no triggers** | `alter_02` |
| 4 | **CRITICAL** | `/*/` anywhere in a `CREATE TABLE` aborts the process (panic in the directive parser) | `schema_01` |
| 5 | **CRITICAL** | `WHERE seq BETWEEN …` returns zero rows in packed mode — corrosion partial-replay deadlock | `feed_01` |
| 6 | **HIGH** | `db_version` reused after a statement/savepoint rollback — changes become unreachable | `writes_01` |
| 7 | **HIGH** | Feed is not ordered by `(db_version, seq)` in packed mode, and claims it is | `feed_02` |
| 8 | **HIGH** | Hash-only tombstones are silently dropped from the V1-compat and PK-only feeds | `e2e_05` |
| 9 | **HIGH** | Merge path has no rowid bound check — `cell_key` overflows and rows vanish | `schema_02` |
| 10 | **HIGH** | `crsql_as_table` leaves all V2 metadata behind; `remove_crr_v2_tables` is dead code | `schema_03` |
| 11 | **HIGH** | A column name containing `'` breaks `crsql_changes` for the whole database | `feed_03` |
| 12 | **HIGH** | `crsql_as_crr(tbl,'skip_hash')` is broken — always errors | `schema_04` |
| 13 | **HIGH** | `ALTER TABLE … RENAME COLUMN` destroys the column's clock history — permanent divergence | `alter_03` |
| 14 | MEDIUM | Orphaned V2 metadata is silently swallowed by the feed's INNER JOIN | `feed_04` |
| 15 | MEDIUM | Single-column packed groups carry no `char(0)`, contradicting the documented detection rule | `feed_05` |
| 16 | MEDIUM | V2-wire reception is gated on `metadata-use-version`, not on the documented condition | `e2e_03` |
| 17 | MEDIUM | Retired `col_id`s are recycled, contradicting the col_id reuse policy | `alter_04` |
| 18 | LOW/DOC | Six design-vs-implementation divergences (config types, rowid reuse, CHECK constraints, directive keys) | `design_01` |

`e2e_04_metadata_invariants.py` is a structural invariant checker rather than a
finding of its own; it currently trips on finding 8 and should go green once
that is fixed. Verified **not** broken (regression tests, currently passing):
`e2e_01`, `e2e_02`, `e2e_06`, `design_02`.

Current state of the full suite (`v2_audit/run_all.sh`):

```
SCRIPT                                               RESULT
---------------------------------------------------- ------
alter_01_put_varint_continuation_bits                BUG
alter_02_pk_only_transition_breaks_table             BUG
alter_03_rename_column_loses_history                 BUG
alter_04_col_id_reuse                                BUG
design_01_doc_divergences                            BUG
design_02_known_good                                 ok
e2e_01_convergence_fuzz                              ok
e2e_02_mixed_mode_cluster                            ok
e2e_03_v2_wire_reception_gate                        BUG
e2e_04_metadata_invariants                           BUG   (trips on finding 8)
e2e_05_tombstone_pks_gap_drops_deletes               BUG
e2e_06_watermark_sync_protocol                       ok
feed_01_packed_seq_range_filter                      BUG
feed_02_packed_feed_ordering                         BUG
feed_03_quoted_col_name_breaks_feed                  BUG
feed_04_orphan_metadata_silently_dropped             BUG
feed_05_single_col_packed_missing_char0              BUG
merge_01_unpack_panics_abort_process                 BUG
schema_01_directive_comment_panic                    BUG
schema_02_merge_rowid_overflow                       BUG
schema_03_drop_table_leaves_v2_metadata              BUG
schema_04_as_crr_skip_hash_flag_broken               BUG
writes_01_db_version_reuse_after_stmt_rollback       BUG

reproduced: 19   correct: 4   harness errors: 0
```

---

## 1. CRITICAL — `put_varint` sets continuation bits on the wrong bytes

**Where:** `core/rs/core/src/pack_columns.rs:176-201` (the loop at `:194-196`)
**Design:** "Packed Wire Format" → *Count header: u8 → varint*
**Repro:** `v2_audit/repros/alter_01_put_varint_continuation_bits.py`

```rust
while v > 0 && n < 9 {
    bytes[n] = (v & 0x7F) as u8;   // bytes[0] = least-significant group
    v >>= 7;
    n += 1;
}
for i in 1..n {
    bytes[i - 1] |= 0x80;          // <-- sets the bit on bytes[0..n-2]
}
for i in (0..n).rev() {            // emitted most-significant group FIRST
    buf.put_u8(bytes[i]);
}
```

Output is most-significant-group-first, so the continuation bit must be set on
every byte **except** `bytes[0]`. The loop sets it on every byte except
`bytes[n-1]` — exactly inverted.

Observed vs. correct SQLite varints:

| value | emitted | correct |
|---|---|---|
| 128 | `01 80` | `81 00` |
| 200 | `01 c8` | `81 48` |
| 16384 | `01 80 80` | `81 80 00` |
| 1048576 | `40 80 80` | `c0 80 00` |

`get_varint` reads the first byte, sees no continuation bit, and returns the
low group as the whole value — **200 decodes as 1** — then resumes parsing
mid-value, so every subsequent field in the blob is garbage too.

**Blast radius.** `crsql_pack_varint_agg` encodes `col_version` and `seq` for
every packed V2-wire change (`changes_vtab_read.rs:251,254`). A column reaches
`col_version = 128` after 128 updates, which is routine. From that point on the
receiver reads a tiny bogus version, loses every subsequent merge comparison,
and the two nodes diverge silently and permanently. `seq ≥ 128` (a transaction
touching ≥ 128 cells) and any `crsql_pack_columns` blob with ≥ 128 columns are
corrupted the same way.

The repro drives one column to `col_version = 200` on node A, syncs to B, and
shows A holding `199` while B holds `999`, with no error on either side.

**Fix:** `for i in 1..n { bytes[i] |= 0x80; }`. Add a round-trip property test
over `put_varint`/`get_varint` across `0 ..= u64::MAX` boundaries (127/128,
16383/16384, 2^21, 2^28, …, 2^63) and a byte-for-byte comparison against
`sqlite3PutVarint`. The dead `if v == 0` branch at `:184-187` (unreachable
inside the `value >= 0x80` arm) should go at the same time.

---

## 2. CRITICAL — a malformed blob from a peer aborts the process

**Where:** `core/rs/core/src/pack_columns.rs:241-244` (`unpack_columns`) and
`core/rs/core/src/pack_columns.rs:467-476` (`unpack_varints`)
**Repro:** `v2_audit/repros/merge_01_unpack_panics_abort_process.py`

Two unchecked reads of attacker-controlled fields. The extension is built
`no_std` with `panic = "abort"`, so neither is catchable — the host process dies
with SIGTRAP. No SQL error, no rollback, no chance for the application to skip
the bad change.

**A — `unpack_columns` never bounds-checks the per-value `intlen`:**

```rust
let column_type_and_maybe_intlen = buf.get_u8();
let column_type = ColumnType::from_u8(column_type_and_maybe_intlen & 0x07);
let intlen = (column_type_and_maybe_intlen >> 3 & 0xFF) as usize;
...
if buf.remaining() < intlen { return Err(ResultCode::ABORT); }
let len = buf.get_int(intlen) as usize;
```

`intlen` comes straight off the wire and ranges 0..=31. Every guard checks that
enough **bytes remain**, which a padded blob trivially satisfies — but
`bytes::Buf::get_int(n)` **panics for `n > 8`**. `intlen = 8` is fine;
`intlen = 9` kills the process. Measured across the sweep 8/9/10/15/31: only 8
survives.

**B — `unpack_varints` sizes an allocation from the wire:**

```rust
let (count, header_len) = get_varint(data)?;
let mut out = Vec::with_capacity(count as usize);
```

`count` is an attacker-controlled u64 varint with no sanity check against the
remaining buffer length, even though it can never legitimately exceed
`data.len()`. A header of `FF FF FF FF FF FF FF FF 7F` requests a ~2^63-element
`Vec` and the allocation failure aborts.

**Reachability — this is the ordinary sync path, not just the debug vtab:**

| entry point | field | result |
|---|---|---|
| `INSERT INTO crsql_changes` (V1 wire) | `pk` → `unpack_columns` | **abort** |
| `INSERT INTO crsql_changes` (packed V2 wire) | `col_version`, `seq` → `unpack_varints` | **abort** |
| `SELECT … FROM crsql_unpack_columns WHERE package = ?` | `package` | **abort** |

**Consequence.** Any peer that can push a change — or any corruption of those
bytes in transit or on disk — takes the receiving node down. It is a crash
**loop**, not a single outage: the poisoned change is still in the sender's
feed, so the node dies again on every retry, and no operator-visible error ever
names the offending row.

**Fix:** reject `intlen > 8` in `unpack_columns` before calling `get_int`; in
`unpack_varints`, drop the `with_capacity` (or cap it against
`data.len() - header_len`, since one varint is at least one byte). Then fuzz
both functions over arbitrary byte strings — neither may ever panic. Given
`panic = "abort"`, it is worth auditing every remaining slice index, `get_int`,
and `with_capacity` in `pack_columns.rs` the same way; finding 4 is the same
class of defect in a different file.

---

## 3. CRITICAL — dropping the last two non-PK columns leaves the table trigger-less

**Where:** `core/rs/core/src/alter_v2.rs:135-147` (`sync_col_map_v2`)
**Design:** "PK-Only Tables" → *ALTER TABLE: Dropping the Last Non-PK Column*
**Repro:** `v2_audit/repros/alter_02_pk_only_transition_breaks_table.py`

On a normal → PK-only transition the code migrates one dropped column's clock
entries down to `col_id = 0`:

```rust
let migrate_col_id = dropped_col_ids.pop().unwrap();
UPDATE v2_clock SET cell_key = cell_key & ~mask WHERE cell_key & mask = ?
```

Drop **both** remaining columns in one `crsql_commit_alter` window and
`dropped_col_ids = [0, 1]`. It pops `1` and rewrites those rows to
`cell_key = key<<12 | 0` — where the `col_id = 0` rows already sit.
`v2_clock.cell_key` is `INTEGER PRIMARY KEY`, so the UPDATE dies on a
uniqueness violation. Three failures cascade:

1. `crsql_commit_alter` errors.
2. `crsql_begin_alter` already dropped the triggers and **nothing restores them
   on the error path**. The table is now a plain SQLite table with cr-sqlite
   metadata attached. Every later `INSERT`/`UPDATE`/`DELETE` writes no `v2_pks`
   row, no clock row, and never reaches the feed — with **no error anywhere**.
   The repro inserts a row after the failed alter and shows it present in the
   base table, absent from `v2_pks`, and invisible to `crsql_changes`.
3. The alter is left half-applied: `v2_col_map` was already emptied by the
   preceding DELETE while the clock rows survive, so the PK-only feed query
   (which no longer joins `v2_col_map`) emits **one duplicate sentinel event per
   leftover col_id** for the same row.

Design step 3 for this transition ("Create missing sentinels: for any rows in
`v2_pks` that had no clock entry at all … a sentinel entry is created at
`col_id=0`") is **not implemented at all**, so rows that were never modified
after insert stay invisible in PK-only mode even when the alter succeeds.

**Fix:** delete the other dropped col_ids' clock rows *before* the migrate
UPDATE (or `INSERT OR REPLACE`-style migrate); implement the missing-sentinel
backfill; and make `crsql_commit_alter` restore triggers on every error path —
ideally wrap begin/commit_alter in a savepoint that rolls back the whole alter
on failure.

---

## 4. CRITICAL — `/*/` in a `CREATE TABLE` aborts the process

**Where:** `core/rs/core/src/schema_directive.rs:66-70`
**Design:** "Schema-Embedded Configuration Directives" → *Reading the Directive*
**Repro:** `v2_audit/repros/schema_01_directive_comment_panic.py`

```rust
while let Some(comment_start) = create_sql[search_pos..].find("/*") {
    let abs_start = search_pos + comment_start;
    if let Some(comment_end_rel) = create_sql[abs_start..].find("*/") {
        let comment_body = &create_sql[abs_start + 2 .. abs_start + comment_end_rel];
```

For the three-character sequence `/*/`, `find("/*")` returns 0 and
`find("*/")` returns **1**, so the slice is `[2..1]` — start > end — and Rust
panics. The extension is built `no_std` with `panic = "abort"`, so this is not a
catchable error: **the host process dies**. The repro runs it in a subprocess
and records exit code `-5` (SIGTRAP) with empty stdout/stderr.

`/*/` is legal SQL (it is simply an unterminated comment start, or appears
inside a string literal / another comment). `crsql_as_crr` reads
`sqlite_master.sql` for every table it registers, so any application whose
schema contains that byte sequence — including inside a `DEFAULT '…/*/…'`
string — crashes on startup. There is no bounds check anywhere in the loop.

**Fix:** guard `comment_end_rel >= 2` before slicing, and search for `*/`
starting at `abs_start + 2` rather than `abs_start`. Fuzz `parse_directives`
against arbitrary strings — it must never panic.

---

## 5. CRITICAL — `WHERE seq BETWEEN …` returns nothing in packed mode

**Where:** `core/rs/core/src/changes_vtab_read.rs:254` (`seq` is a BLOB) +
`core/rs/core/src/changes_vtab.rs:80-110` / `changes_vtab_read.rs:394-439`
**Design:** "Seq Handling" — explicitly claims this works
**Repro:** `v2_audit/repros/feed_01_packed_seq_range_filter.py`

The design says:

> Partial replays (`SyncNeedV1::Partial`) query `WHERE seq BETWEEN :start AND
> :end` on the `crsql_changes` vtable. This works correctly because the vtable
> filters on the underlying clock table's `seq` column (via
> `xBestIndex`/`xFilter`), **not on the packed output**.

It does not. `changes_best_index` appends `seq >= ? AND seq <= ?` to `idx_str`,
and `changes_union_query` splices `idx_str` onto the **outer** SELECT over the
UNION of the already-`GROUP BY`ed subqueries. In V2-wire mode that outer `seq`
is `crsql_pack_varint_agg(c.seq ORDER BY cm.col_id)` — a BLOB. SQLite orders
INTEGER before BLOB unconditionally, so `blob <= 100` is always false.

Every upper bound (`BETWEEN`, `<=`, `<`, `=`) silently drops **all** alive-row
changes; every lower bound (`>=`, `>`) silently matches all of them. The
constraint is marked `omit = 1`, so SQLite trusts the vtab and does not
re-check.

**Consequence:** corrosion's `SyncNeedV1::Partial` replay fetches nothing for
the requested window. The requesting peer's `PartialVersion` never fills its
seq gaps, so it re-requests the same range forever — a permanent sync stall
plus unbounded request traffic.

**Fix:** push `seq` constraints down into the per-table subqueries against
`c.seq` (before the `GROUP BY`), not onto the packed outer column. Same for
`ORDER BY` (finding 6).

---

## 6. HIGH — `db_version` reused after a statement or savepoint rollback

**Where:** `core/rs/core/src/db_version.rs:88-91`, `core/rs/core/src/commit.rs:33-40`
**Repro:** `v2_audit/repros/writes_01_db_version_reuse_after_stmt_rollback.py`

`next_db_version` persists the new version to `crsql_db_versions` only when it
differs from the in-memory `pendingDbVersion`:

```rust
if ret != unsafe { (*ext_data).pendingDbVersion } { …write crsql_db_versions… }
```

`pendingDbVersion` is reset only by the commit and rollback **hooks**, which
fire on a transaction boundary — not on a statement-level abort (constraint
violation) and not on `ROLLBACK TO <savepoint>`. So:

```
BEGIN
  INSERT …          -- next_db_version: 1 != pending(-1) -> writes row, pending = 1
  INSERT …          -- aborts on a constraint violation; SQLite's statement journal
                    --   undoes the crsql_db_versions write, pendingDbVersion stays 1
  INSERT …          -- next_db_version: 1 == pending(1) -> skips the write
COMMIT
```

leaves clock rows stamped `db_version = 1` with **no row in
`crsql_db_versions`**. A fresh connection reads 0 from storage and hands out
`db_version = 1` again for the next local write. Two distinct changes now share
one version, and a peer that has already pulled past `db_version = 1` will never
see the second — silent, permanent divergence.

`ROLLBACK TO SAVEPOINT` reproduces it identically, which matters because
`crsql_as_crr` itself uses savepoints.

This code is shared with V1, so it is **not a V2 regression** — but V2 inherits
it and the packed feed makes the lost-change window larger.

**Fix:** reset `pendingDbVersion` from a statement-abort-aware point, or make
the write unconditional (it is an upsert of one row), or track the persisted
value separately from the in-memory pending value.

---

## 7. HIGH — the feed is not ordered by `(db_version, seq)` but says it is

**Where:** `core/rs/core/src/changes_vtab.rs:124-183` (`orderByConsumed = 1`) +
`changes_vtab_read.rs:254` / `:30` / `:291`
**Design:** "Feed Query (Packed, Per Table)" ends with `ORDER BY db_vrsn, seq`
**Repro:** `v2_audit/repros/feed_02_packed_feed_ordering.py`

The ordering is applied to the outer projection, where alive rows carry a BLOB
`seq` (`crsql_pack_varint_agg`) and tombstone rows carry an INTEGER `seq`. Two
consequences:

* every tombstone in a `db_version` sorts before every alive-row group,
  regardless of its actual seq;
* alive-row groups sort by raw varint bytes, whose **first byte is the element
  count** — so a 1-column group always sorts before a 3-column group no matter
  what their seqs are.

The repro emits three groups whose first seqs are `0, 3, 4` and gets them back
in the order `4, 3, 0`.

`orderByConsumed` is set to 1, so SQLite takes the vtab at its word and does not
re-sort. Any consumer streaming `SELECT * FROM crsql_changes ORDER BY
db_version, seq` — corrosion does, and it is also the vtab's own default —
receives a transaction's events in arbitrary order, and combined with `LIMIT`
the first N rows are not the N lowest-seq changes, so chunked/resumable readers
skip changes outright.

**Fix:** as with finding 4, order inside the per-table subqueries on the raw
`c.seq`, or expose a separate scalar ordering column. Do not claim
`orderByConsumed` unless the ordering is genuinely produced.

---

## 8. HIGH — hash-only tombstones are silently dropped from the feed

**Where:** `core/rs/core/src/changes_vtab_read.rs:91` (INNER `JOIN
v2_tombstone_pks`), reached from `:190` (V1-compat wire) and `:358` (PK-only)
**Design:** "Codepath Separation" (V2→V1 translation) + "5. Tombstone PK Mapping"
**Repro:** `v2_audit/repros/e2e_05_tombstone_pks_gap_drops_deletes.py`

When a `cid='-2'` delete arrives for a row the receiver has never seen, it
cannot resolve hash → PK, so it records the `v2_tombstones` row **without** a
`v2_tombstone_pks` row (`changes_vtab_write.rs:1476-1498` only writes the
mapping when `local_key` is `Some`). The design acknowledges this:

> If the hash is unknown and it's a delete (`cid='-2'`), it can be ignored …
> The tombstone is still recorded in `v2_tombstones`.

But the V1-compat dead-row query joins `v2_tombstone_pks` with an **INNER
JOIN**, so that tombstone can never be emitted again. No error, no warning, no
row.

Two ways this bites:

* **V1-compat emission** (`use = v2`, `log = v1`) — precisely the documented
  rollout window between step 3 and step 4 of "Recommended Rollout Sequence".
  The repro shows node C receiving the delete, holding the tombstone, emitting
  an empty feed, and a downstream V1 node keeping the row alive forever.
* **PK-only tables in hash mode** (composite or TEXT PK) — `query_for_table`
  routes every PK-only table to `crsql_changes_query_for_table_v2_pkonly`
  **regardless of `sync-log-version`** (`changes_vtab_read.rs:391-396`), so
  those deletes are dropped even in a pure V2 cluster.

Delete-before-first-sync and delete-arriving-before-insert are ordinary events
in a gossip topology, not corner cases.

**Fix:** `LEFT JOIN` and emit the hash when the mapping is absent (with the
`'-2'` sentinel), or make the merge path record a `v2_tombstone_pks` row from
the wire PK when one is available, or refuse to prune the mapping. The PK-only
path should also respect `sync-log-version`.

---

## 9. HIGH — the merge path never checks the rowid bound

**Where:** `core/rs/core/src/changes_vtab_write.rs` (no bound check;
`create_crr.rs::validate_rowid_range` only scans existing rows at `as_crr` time,
and `local_writes/after_insert.rs:56` / `after_update.rs:94` guard only local writes)
**Design:** "3. Alive PKs" → *Large Rowid Handling*
**Repro:** `v2_audit/repros/schema_02_merge_rowid_overflow.py`

The design mandates `CHECK (rowid >= 0 AND rowid < 2251799813685248)` on the
main table. SQLite cannot `ALTER TABLE ADD CHECK`, so the implementation cannot
add it and instead guards the local write paths. The **merge path has no guard
at all**, so a node in `use_rowid` mode accepts a replicated row whose
`INTEGER PRIMARY KEY` is ≥ 2^51. `(1 << 60) << 12` wraps to 0, the clock row is
written under `cell_key = 0`, the feed's join back to `v2_pks.__crsql_key` finds
nothing, and the change disappears from `crsql_changes` permanently. Two such
rows collide on the same `cell_key` and overwrite each other's clocks.

The repro shows node B holding 2 replicated rows, emitting 0 of them, with only
1 clock entry for the pair.

**Fix:** apply the same bound check in the merge path and return a clear error
(a peer sending out-of-range keys is a configuration error worth surfacing), and
document that the `CHECK` in the design is unimplementable on an existing table.

---

## 10. HIGH — `crsql_as_table` leaves every V2 table behind

**Where:** `core/rs/core/src/lib.rs:136-138`; `core/rs/core/src/teardown_v2.rs`
**Design:** "Teardown (V2): `crsql_remove_crr_v2`"
**Repro:** `v2_audit/repros/schema_03_drop_table_leaves_v2_metadata.py`

```rust
fn crsql_as_table_impl(db: *mut sqlite::sqlite3, table: &str) -> Result<ResultCode, ResultCode> {
    remove_crr_clock_table_if_exists(db, table)?;   // V1 clock + V1 pks only
    remove_crr_triggers_if_exist(db, table)
}
```

`remove_crr_v2_tables` (`teardown_v2.rs:9`) has **zero callers** — it is dead
code. Downgrading a CRR leaves `v2_col_map`, `v2_clock`, `v2_pks`,
`v2_tombstones` and `v2_tombstone_pks` in place, along with the table's
`crsql_master` rows (`use_rowid_<tbl>`, `skip_hash_<tbl>`, `v2_pks_<tbl>`) and
any queued maintenance task. Re-registering the table later picks up the stale
`v2_pks` schema through the `has_v2` inference in `tableinfo.rs:1148`, so the
old `skip_hash`/`key_is_rowid` decision silently overrides the new one, and a
pending migration task can still fire against a dropped table.

A second bug sits in the same function: on failure `crsql_as_table`
(`lib.rs:125-127`) issues a bare `ROLLBACK` rather than `ROLLBACK TO as_table`,
tearing down the **caller's entire transaction** instead of just its own
savepoint.

**Fix:** call `remove_crr_v2_tables` from `crsql_as_table_impl`, clear the
table's `crsql_master` keys and task-queue rows, and change the error path to
`ROLLBACK TO as_table` + `RELEASE`.

---

## 11. HIGH — a `'` in a column name breaks `crsql_changes` database-wide

**Where:** `core/rs/core/src/changes_vtab_read.rs:200-215` (`build_col_val_case`)
**Repro:** `v2_audit/repros/feed_03_quoted_col_name_breaks_feed.py`

```rust
when_clauses.push(format!(
    "WHEN '{col_name}' THEN mt.\"{col_name}\"",
    col_name = crate::util::escape_ident(&col.name)
));
```

`escape_ident` doubles `"` for the identifier position. The same escaped string
is then interpolated into a **single-quoted string literal**, where `'` — not
`"` — is the character that needs doubling. A column named `o'brien` produces
`WHEN 'o'brien' THEN …` and the whole query fails to prepare.

Because `changes_union_query` builds one `UNION ALL` across **all** CRR tables,
a single bad column name makes `SELECT * FROM crsql_changes` fail for every
table in the database — the healthy tables' changes become unreadable too. The
repro shows both the v2-wire and v1-wire builders failing with `SQL logic error`.

**Fix:** use `escape_ident_as_value` (or a dedicated literal escaper) for the
`WHEN '…'` position and `escape_ident` for the `mt."…"` position. Better: use
the design's own recommendation and switch the CASE to compare `cm.col_id`
integers (see finding 18.5), which removes the string literal entirely.

---

## 12. HIGH — `crsql_as_crr(tbl, 'skip_hash')` always errors

**Where:** `core/rs/core/src/create_crr.rs:80-82` vs `tableinfo.rs:1215-1220`
**Design:** "Skip Hash Optimization" → *Eligibility* (manually enabled via `as_crr` option)
**Repro:** `v2_audit/repros/schema_04_as_crr_skip_hash_flag_broken.py`

`pull_table_info` derives `skip_hash` and then computes the cached column name
from it:

```rust
let skip_hash_pk_col = if skip_hash && !pks.is_empty() {
    crate::util::escape_ident(&pks[0].name)
} else { String::new() };
```

`create_crr` flips the flag **afterwards** without recomputing the name:

```rust
if skip_hash_flag && !table_info.skip_hash {
    table_info.skip_hash = true;          // skip_hash_pk_col stays ""
}
```

Every generated statement then references an empty column name, and
registration dies with `backfill_table_v2 failed for … (key_is_rowid=false,
skip_hash=true): ERROR`. The repro shows all three documented use cases failing:

* a single `BLOB` PK with the manual opt-in (design says this is supported);
* an auto-qualified table with `skip_hash=0` in the schema plus the flag;
* a composite PK, which the design says must be **silently ignored** — it
  errors instead.

The schema-directive path works because it feeds into `pull_table_info` before
`skip_hash_pk_col` is computed, which is the control case in the repro.

**Fix:** resolve `skip_hash` (flag, directive, auto) *before* computing
`skip_hash_pk_col`, or recompute the name after the override. Add the
composite-PK "silently ignore" branch to the flag path as well as the directive
path.

---

## 13. HIGH — `RENAME COLUMN` destroys the column's clock history

**Where:** `core/rs/core/src/alter_v2.rs:104-200` (`sync_col_map_v2` reconciles by name)
**Design:** not covered — "ALTER TABLE (V2)" lists only Column Added / Removed / PK Changed
**Repro:** `v2_audit/repros/alter_03_rename_column_loses_history.py`

`sync_col_map_v2` matches `v2_col_map` against the current schema **by column
name**, so a rename is indistinguishable from "drop the old name, add a new
one". It deletes the old name's map row, `DELETE`s every `v2_clock` row for that
col_id, and inserts the new name with a recycled col_id and no clock rows. The
column then has no clock entry on any row and is absent from `crsql_changes`.

Rolling a rename out to a cluster — the normal way a schema change is applied —
destroys the history on **every** node simultaneously. The repro has node A
holding `payload='from-A'` and node B holding the newer `payload='from-B'`,
renames on both, syncs to quiescence, and the two values never reconcile.
Before the rename they would have converged on the next sync.

**Fix:** detect the rename (col_id survives, name changes — compare the set
difference in both directions when `|added| == |removed|`, or read
`sqlite_master` before/after) and `UPDATE v2_col_map SET col_name = ?` in place,
leaving the clock rows untouched.

---

## 14. MEDIUM — orphaned V2 metadata is silently swallowed by the feed

**Where:** `core/rs/core/src/changes_vtab_read.rs` (`main_join` is an INNER JOIN
to the base table)
**Repro:** `v2_audit/repros/feed_04_orphan_metadata_silently_dropped.py`

When a `v2_pks` + `v2_clock` entry exists but the base-table row does not, the
feed's INNER JOIN to the main table drops the change entirely — no update event,
no tombstone, no error. The repro shows 2 keys in `v2_pks` and `v2_clock` but
only 1 row emitted.

This is reachable whenever base rows and metadata drift apart: a failed alter
(finding 2), an out-of-band `DELETE` with triggers dropped, a partially applied
merge, or the seeded-snapshot path. The node holds metadata claiming a row is
alive at some CL while never telling anyone about it, so peers can never correct
it.

**Fix:** surface the inconsistency — either emit a tombstone/repair event, or
have `crsql_incremental_maintenance` reconcile orphans (the repo already ships
`check_v1_v2_data_integrity.py` and `fix_corrosion_untracked_rows.py`, which is
evidence this drift happens in production).

---

## 15. MEDIUM — single-column packed groups carry no `char(0)`

**Where:** `core/rs/core/src/changes_vtab_read.rs:249`
**Design:** "V1/V2 Coexistence" and the "Packed Wire Row" table
**Repro:** `v2_audit/repros/feed_05_single_col_packed_missing_char0.py`

The design defines format detection as a property of `cid`:

> Packed (v2): `cid` contains `char(0)` separator → split by `char(0)`.
> Single (v1): `cid` is a plain column name or sentinel (no `char(0)`).

`group_concat(cm.col_name, char(0) ORDER BY cm.col_id)` over a **one**-element
group emits just the column name, no separator. A single-column update — the
single most common change shape — is therefore emitted with `cid = b"a"` while
`col_vrsn`, `seq` and `val` are still packed binary arrays.

cr-sqlite's own receiver survives because it ignores the documented rule and
sniffs the *type* of `col_vrsn` instead (`changes_vtab_write.rs:558-565`). Any
other implementation of the wire format — and the design doc is the only spec
such an implementation has — classifies the row as a V1 single change and then
writes the `crsql_pack_agg` TLV blob straight into the user column and a
nonsense value into `col_version`. Silently wrong rather than loudly broken.

**Fix:** pick one and make both sides agree — either always append a trailing
`char(0)`, or change the spec to the type-based detection the code actually
implements.

---

## 16. MEDIUM — V2-wire reception gated on the wrong config value

**Where:** `core/rs/core/src/changes_vtab_write.rs:576-587`
**Design:** "Versioning" — reception requirements
**Repro:** `v2_audit/repros/e2e_03_v2_wire_reception_gate.py`

The design says:

> Nodes accept v2 logs regardless of their own `sync-log-version` … Reception
> requires: 1. `metadata-write-version` is `v2` or `v2&v1`. 2. V1→V2 migration
> is complete.

`metadata-use-version` is not in that list. The implementation rejects the
change unless `metadataUseVersion == 2`. A node at `write = v2&v1` with
migration complete and `use = v1` — exactly the state between steps 2 and 3 of
"Recommended Rollout Sequence" — hard-fails on every V2-wire change it receives.
An operator who follows the doc and flips a peer to `sync-log-version = v2`
once "all peers can accept V2 format" breaks sync against every peer still at
`use = v1`.

**Fix:** either relax the guard to the documented condition, or update the
design doc and the rollout sequence to require `use = v2` cluster-wide before
any node emits V2 wire. The doc and the code must not disagree about this,
because the sender has no way to discover the receiver's state.

---

## 17. MEDIUM — retired `col_id`s are recycled

**Where:** `core/rs/core/src/alter_v2.rs:170-186`
**Design:** "PK-Only Tables" → *col_id Reuse Policy*
**Repro:** `v2_audit/repros/alter_04_col_id_reuse.py`

The design is explicit:

> For `col_id >= 1`: when a column is dropped, its `col_id` is retired. New
> columns always get `max(col_id) + 1` (after trying slot 0). This prevents a
> newly added column from inheriting stale clock entries from a previously
> dropped column.

The implementation computes `used_col_ids` from the columns that still exist and
then picks the **smallest unused id**, so with `a=0, b=1, c=2`, dropping `b` and
adding `d` gives `d` col_id **1** — `b`'s retired id.

Today the DELETE of `b`'s clock rows runs in the same call, so nothing is
inherited on the happy path. The hazard is real on any path that leaves clock
rows behind — finding 3 is exactly such a path: after the failed PK-only alter,
`v2_col_map` is empty while `col_id` 0 and 1 clock rows survive, so the next
`ADD COLUMN` takes col_id 0 and inherits a stale clock row with someone else's
`db_version`/`seq`.

**Fix:** track `max(col_id)` monotonically (persist a high-water mark, since the
map row is deleted), reserving only slot 0 for the PK-only sentinel handoff the
design describes.

---

## 18. LOW / DOC — design-vs-implementation divergences

**Repro:** `v2_audit/repros/design_01_doc_divergences.py` (documents all six; no
correctness impact on its own, but each one misleads a reader of the design doc)

1. **Config values are integers, the doc says strings.** The doc shows
   `crsql_config_set('metadata-write-version', 'v2&v1')`. The implementation
   reads `args[1].int()` (`config.rs:47`), so `1` = v1, `2` = v2&v1, `3` = v2
   (and `1`/`2` for the other two keys). Passing the documented string yields
   `.int() == 0` and the opaque error `Invalid metadata-write-version
   transition`. Also undocumented: `v1 → v2` **is** allowed directly when no CRR
   tables exist yet (`config.rs:56-70`), which the doc's transition table omits
   even though "Backfill (V2)" depends on it.

2. **`key_is_rowid` is never enabled by default.** The doc's entire "Rowid
   Reuse: Opt-in vs Opt-out" table says plain rowid tables default to
   `key_is_rowid = true`. `tableinfo.rs:1146,1232-1242` defaults it to `false`
   for **every** table and only `use_rowid=1` on an `INTEGER PRIMARY KEY` table
   turns it on. The code's reasoning (implicit rowids are renumbered by `VACUUM`)
   is sound and matches commit `8ffc07d2` — the **doc is stale**, not the code.
   Rewrite §3's table.

3. **Directive keys don't match.** The doc specifies `use_rowid_key`,
   `skip_hash` and `without_rowid`. `schema_directive.rs:34` reads `use_rowid`;
   `without_rowid` is not read at all. A schema carrying the documented
   `/* crsql: use_rowid_key=1 */` is silently ignored.

4. **The `CHECK` constraints in the design cannot exist.** §3 "Large Rowid
   Handling" and §"Runtime Guard" both call for `CHECK` constraints added to the
   *main* table at `as_crr` time. SQLite has no `ALTER TABLE ADD CONSTRAINT`, so
   the implementation substitutes a one-time scan
   (`create_crr.rs::validate_rowid_range`) plus per-write guards — which is why
   finding 8 exists. The doc should describe the guards that are actually
   possible.

5. **The value-extraction CASE compares strings, not `col_id`s.** §"Feed Query
   (Packed)" specifies `CASE cm.col_id WHEN 0 THEN … WHEN 1 THEN …` and
   justifies it: "Integer comparison is used instead of string comparison on
   `col_name` for efficiency." `build_col_val_case`
   (`changes_vtab_read.rs:200-215`) emits `CASE cm.col_name WHEN 'a' THEN …`.
   This is a per-cell string comparison on the hottest query in the system, and
   it is the direct cause of finding 11.

6. **A discarded `Result`.** `local_writes/v2.rs:92` calls
   `stmt.bind_int64(1, rowid…)` without `?` — a bind failure is silently
   ignored, leaving parameter 1 NULL. Every other bind in the file is checked.

---

## What is verified working

These are green today and are worth keeping as regression tests.
`e2e_04_metadata_invariants.py` belongs in this set too — it checks 9 structural
invariants (alive/dead disjointness, CL parity, no orphan clock rows, col_id
resolvability, `ts > 0`, tombstone-PK coverage) after a delete/resurrect-heavy
3-node fuzz. It is the script that **found finding 7**, so it reports BUG until
that is fixed; everything else it checks already holds.

| Script | Property |
|---|---|
| `e2e_01_convergence_fuzz.py` | 3-node convergence over 6 table shapes (plain rowid, INTEGER PK, TEXT PK, composite, WITHOUT ROWID, PK-only) × 25 seeds × modes v1 / v2&v1 / v2 |
| `e2e_02_mixed_mode_cluster.py` | mixed-metadata cluster on the V1 wire (v1-only, v2&v1+use=v2, v2-only), 25 seeds — exercises V1-compat emission and V1-wire → V2-metadata merge |
| `e2e_06_watermark_sync_protocol.py` | convergence under the real per-origin-site watermark protocol (`WHERE site_id = ? AND db_version > ?`), which the full-resend fuzz cannot catch |
| `design_02_known_good.py` | config persistence across connections, rollback leaves no metadata, `ts` edge cases error cleanly, PK-only insert/delete/resurrect, backfill on a pre-populated `as_crr`, PK-changing and rowid-changing `UPDATE`s, cross-node schema skew |

One caveat worth recording from the schema-skew case: a node that adds a column
*after* a peer has already sent changes for it only recovers the value because
the test re-sends every change. Under a real watermark the value is lost
permanently. That is an operational constraint (add columns before writing to
them), not a code defect, but it belongs in the rollout docs.

---

## Suggested fix order

1. **Finding 1** (`put_varint`) — one-line fix, corrupts data today, blocks any
   V2-wire rollout.
2. **Findings 5 and 7** (seq filter + ordering) — same root cause (predicates
   spliced onto the packed outer query); fix together.
3. **Finding 3** (ALTER leaves no triggers) plus the trigger-restore guard,
   which also mitigates findings 14 and 17.
4. **Finding 4** (parser panic) — trivial bounds check, but it is a process abort.
5. **Findings 8, 9, 10, 11, 12, 13** — independent, each self-contained.
6. **Finding 6** — shared with V1; schedule with the V1 owners.
7. **Findings 15, 16, 18** — reconcile the design doc and the code; several of
   these are the doc being stale rather than the code being wrong.
