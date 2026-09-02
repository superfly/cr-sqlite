"""feed_01: partial replay by seq range returns ZERO alive rows in V2 packed mode.

Design: "Seq Handling" (v2_metadata_design.md) states:
    "Partial replays (SyncNeedV1::Partial) query `WHERE seq BETWEEN :start AND :end`
     on the `crsql_changes` vtable. This works correctly because the vtable filters
     on the underlying clock table's `seq` column (via xBestIndex/xFilter), not on
     the packed output."

EXPECTED: `SELECT * FROM crsql_changes WHERE seq BETWEEN 0 AND 100` returns every
          alive-row change whose seq falls in the range.

ACTUAL:   It returns ZERO alive-row changes.
          changes_vtab.rs:changes_best_index emits `WHERE seq >= ? AND seq <= ?`
          and changes_vtab_read.rs:changes_union_query splices that predicate onto
          the *outer* SELECT over the UNION of the already-GROUPed subqueries
          (changes_vtab_read.rs:429-439).  In v2-wire mode the outer `seq` is
          `crsql_pack_varint_agg(...)` -> a BLOB (changes_vtab_read.rs:254).
          In SQLite's type ordering INTEGER < BLOB always, so `blob <= 100` is
          always false and every packed row is filtered out.  Any upper bound on
          seq (BETWEEN, <=, <, =) silently drops all alive changes; lower bounds
          (>=, >) silently match everything.

CONSEQUENCE: corrosion's `SyncNeedV1::Partial` replay path fetches nothing for
          the requested seq window.  The requesting peer never receives the
          missing seqs, its `PartialVersion` never becomes complete, and it
          re-requests the same range forever -> permanent sync deadlock plus
          unbounded re-request traffic.
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run


def main():
    c = connect()  # write/use/log = v2 (packed wire)
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT, cc TEXT)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1,'a1','b1','c1')")
    c.execute("INSERT INTO t VALUES (2,'a2','b2','c2')")

    section("ground truth: clock rows (seqs 0..2 in each db_version)")
    dump(c, "SELECT cell_key>>12 AS key, cell_key&4095 AS col_id, db_version, seq "
            "FROM t__crsql_v2_clock ORDER BY db_version, seq")

    section("unfiltered feed")
    total = dump(c, "SELECT [table], quote(pk), quote(cid), db_version, quote(seq), typeof(seq) "
                    "FROM crsql_changes")

    section("what xBestIndex pushes down")
    dump(c, "EXPLAIN QUERY PLAN SELECT * FROM crsql_changes WHERE seq BETWEEN 0 AND 100")

    section("partial replay: WHERE seq BETWEEN 0 AND 100")
    ranged = dump(c, "SELECT [table], quote(cid), db_version, quote(seq) "
                     "FROM crsql_changes WHERE seq BETWEEN 0 AND 100")

    section("other seq shapes")
    dump(c, "SELECT count(*) FROM crsql_changes WHERE seq <= 100", label="seq <= 100")
    dump(c, "SELECT count(*) FROM crsql_changes WHERE seq = 0", label="seq = 0")
    dump(c, "SELECT count(*) FROM crsql_changes WHERE seq >= 0", label="seq >= 0 (matches all - blob > int)")
    dump(c, "SELECT count(*) FROM crsql_changes WHERE db_version = 2 AND seq BETWEEN 0 AND 2",
         label="db_version = 2 AND seq BETWEEN 0 AND 2")

    print()
    print("  expected rows from `seq BETWEEN 0 AND 100`: %d" % len(total))
    print("  actual   rows from `seq BETWEEN 0 AND 100`: %d" % len(ranged))

    if len(ranged) != len(total):
        fail("seq range filter dropped %d of %d alive-row changes "
             "(packed seq is a BLOB, so `seq <= <int>` is never true)"
             % (len(total) - len(ranged), len(total)))
    else:
        ok("seq range filter returned all rows")


run(main)
