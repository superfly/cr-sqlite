"""schema_02: the rowid bound is never enforced on the merge path -> cell_key
overflows to a negative/wrapped value and the row silently stops replicating.

Design doc, "3. Alive PKs" / "Large Rowid Handling":

    cell_key = (__crsql_key << CRSQL_COL_ID_BITS) | col_id must fit in a
    *signed* INT64 ... CHECK (rowid >= 0 AND rowid < 2251799813685248)

The implementation never adds that CHECK to the main table (SQLite cannot
ALTER TABLE ADD CHECK, so it cannot). Instead create_crr.rs::validate_rowid_range
scans *existing* rows once at as_crr time, and local_writes/after_insert.rs:56 /
after_update.rs:94 guard *local* writes. The merge path
(changes_vtab_write.rs) has no such guard.

So a node running in `use_rowid` mode happily accepts a replicated row whose
INTEGER PRIMARY KEY is >= 2^51. (1 << 60) << 12 wraps to 0, so the clock row is
written under cell_key 0. The feed joins cell_key >> 12 back to
v2_pks.__crsql_key, finds nothing, and the row disappears from crsql_changes
forever: the receiving node holds data it can never forward, and two such rows
collide on the same cell_key and overwrite each other's clocks.

Expected: the merge either rejects the out-of-range key (same as the local
          write path) or stores a correct cell_key.
Actual:   the row is accepted, cell_key wraps, the change vanishes from the feed.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run, sync

BIG_A = 1 << 60
BIG_B = 1 << 61


def main():
    section("node A: default (non-rowid key) mode, two huge INTEGER PKs")
    a = connect()
    a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    a.execute("SELECT crsql_as_crr('t')")
    a.execute("INSERT INTO t VALUES (?, 'from-a-1')", (BIG_A,))
    a.execute("INSERT INTO t VALUES (?, 'from-a-2')", (BIG_B,))
    dump(a, "SELECT * FROM t", label="A rows")
    dump(a, "SELECT cell_key FROM t__crsql_v2_clock ORDER BY cell_key", label="A clock (sane)")
    a_changes = dump(a, 'SELECT "table", cid, val FROM crsql_changes', label="A feed")

    section("node B: same table registered with use_rowid=1 (__crsql_key = rowid = PK)")
    b = connect()
    b.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    b.execute("SELECT crsql_as_crr('t','use_rowid')")

    section("local write of a huge PK on B is correctly rejected")
    try:
        b.execute("INSERT INTO t VALUES (?, 'local')", (BIG_A,))
        print("     local insert SUCCEEDED (unexpected)")
        local_guarded = False
    except Exception as e:
        print("     local insert rejected: %s" % e)
        local_guarded = True

    section("but the same value arriving over the merge path is accepted")
    rows = sync(a, b)
    print("     applied %d change rows" % len(rows))
    dump(b, "SELECT * FROM t", label="B rows (data is there)")
    dump(b, "SELECT cell_key, cell_key >> 12 AS key_back FROM t__crsql_v2_clock",
         label="B clock (cell_key wrapped)")
    dump(b, "SELECT * FROM t__crsql_v2_pks", label="B v2_pks")
    b_changes = dump(b, 'SELECT "table", cid, val FROM crsql_changes', label="B feed")

    bad_cells = b.execute(
        "SELECT count(*) FROM t__crsql_v2_clock WHERE cell_key >> 12 <> "
        "(SELECT __crsql_key FROM t__crsql_v2_pks p WHERE (p.__crsql_key << 12) | 0 = cell_key)"
    ).fetchone()
    n_rows_b = b.execute("SELECT count(*) FROM t").fetchone()[0]
    n_clock_b = b.execute("SELECT count(*) FROM t__crsql_v2_clock").fetchone()[0]

    section("verdict")
    if not local_guarded:
        fail("local write of an out-of-range rowid was not rejected either")
    if n_rows_b == 2 and len(b_changes) < len(a_changes):
        fail("B holds %d rows but its feed emits %d of A's %d changes -- the "
             "replicated rows are invisible to downstream nodes"
             % (n_rows_b, len(b_changes), len(a_changes)))
    if n_clock_b < 2:
        fail("B's clock table has only %d entries for 2 replicated rows -- "
             "cell_keys collided after the shift overflowed" % n_clock_b)
    if n_rows_b == 2 and len(b_changes) == len(a_changes) and n_clock_b >= 2:
        ok("merge path preserved cell_key integrity")
    print("     (bad_cells probe: %r)" % (bad_cells,))


run(main)
