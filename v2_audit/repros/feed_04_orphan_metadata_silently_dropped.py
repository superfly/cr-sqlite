"""feed_04: V2 feed silently drops every change for a row whose base-table row is
gone, leaving live metadata that can never be synced (V1 did not do this).

Design: "Feed Query (Packed, Per Table)" specifies
    JOIN "<table>" AS mt ON mt.rowid = ah.__crsql_key
i.e. an INNER JOIN to the base table, used both for the PK columns and for the
compiled-CASE value fetch (changes_vtab_read.rs:58 / :64, used by the v2wire
query at :260, the v1wire query at :169 and the pk-only query at :342).

EXPECTED: metadata in v2_pks / v2_clock is either kept consistent with the base
          table, or, if it goes stale, the feed surfaces it (V1's read path
          falls back to emitting DELETE_SENTINEL when the base row is missing --
          changes_vtab.rs:572-579).

ACTUAL:   the INNER JOIN drops the row.  Deleting a base row while the crr
          triggers are down (`crsql_begin_alter` ... `crsql_commit_alter`, the
          documented way to run a schema migration) leaves the v2_pks row and its
          v2_clock cells in place.  V2's commit_alter does not prune them (V1's
          does -- see the V1 leg of this script), and the feed then hides them:
          no update rows, no tombstone, nothing.

CONSEQUENCE: a migration that deletes rows inside a begin_alter/commit_alter
          window produces permanent, silent divergence.  Peers keep the deleted
          row forever because no tombstone is ever emitted, and the local node
          cannot re-emit it either.  The stale cells also stay live: if the base
          table later reuses that rowid, the old clock entries (with their old
          db_versions/col_versions) resurface and are attributed to the new row.
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run, WRITE_V1, USE_V1, LOG_V1


def build(c):
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a TEXT)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1,'a1')")
    c.execute("INSERT INTO t VALUES (2,'a2')")
    # a schema-migration window: triggers are dropped between these two calls
    c.execute("SELECT crsql_begin_alter('t')")
    c.execute("DELETE FROM t WHERE id=2")
    c.execute("SELECT crsql_commit_alter('t')")


def main():
    section("V2 (metadata-write/use/log = v2)")
    c = connect()
    build(c)
    dump(c, "SELECT * FROM t", label="base table (row 2 gone)")
    pks = dump(c, "SELECT * FROM t__crsql_v2_pks", label="v2_pks (row 2 metadata still live)")
    clk = dump(c, "SELECT cell_key>>12 AS key, cell_key&4095 AS col_id, col_version, db_version, seq "
                  "FROM t__crsql_v2_clock", label="v2_clock")
    dump(c, "SELECT * FROM t__crsql_v2_tombstones", label="v2_tombstones (no tombstone for row 2)")
    feed = dump(c, "SELECT quote(pk), quote(cid), quote(val), cl, db_version FROM crsql_changes",
                label="V2 feed")

    section("V1 (metadata-write/use/log = v1) -- same sequence, for comparison")
    c1 = connect(write=WRITE_V1, use=USE_V1, log=LOG_V1)
    build(c1)
    dump(c1, "SELECT * FROM t__crsql_pks", label="v1 __crsql_pks (row 2 pruned by commit_alter)")
    dump(c1, "SELECT key, col_name, col_version, db_version FROM t__crsql_clock", label="v1 __crsql_clock")
    dump(c1, "SELECT quote(pk), quote(cid), quote(val), cl FROM crsql_changes", label="V1 feed")

    orphan_keys = {r[0] for r in clk} - {1}
    print()
    print("  v2_pks rows: %d, v2_clock keys: %r, feed rows: %d"
          % (len(pks), sorted({r[0] for r in clk}), len(feed)))

    if orphan_keys and len(feed) == 1:
        fail("v2 metadata still holds key(s) %r (rows in v2_pks + v2_clock) but the "
             "feed emits nothing for them -- no update, no tombstone.  The INNER "
             "JOIN to the base table swallows them silently." % (sorted(orphan_keys),))
    else:
        ok("no orphaned-and-hidden metadata")


run(main)
