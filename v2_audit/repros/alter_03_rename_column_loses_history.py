"""ALTER TABLE ... RENAME COLUMN destroys the column's replication history.

alter_v2.rs::sync_col_map_v2 reconciles v2_col_map by NAME. A rename looks
exactly like "drop the old name, add a new name", so it:

  1. deletes the old name's v2_col_map row,
  2. DELETEs every v2_clock row carrying that col_id, and
  3. inserts the new name with a recycled col_id and NO clock rows.

After the rename the column has no clock entry on any row, so it is absent from
crsql_changes entirely. Its current value stops replicating until something
writes to it again.

CONSEQUENCE: if two nodes hold different values for that column and both apply
the same rename (which is what happens when a schema change is rolled out to a
cluster), the column's clock history is destroyed on both sides and the values
can never reconcile — the divergence becomes permanent instead of being
resolved on the next sync. The design doc does not cover RENAME COLUMN at all
("ALTER TABLE (V2)" only lists Column Added / Column Removed / PK Changed).

A correct implementation would detect the rename (col_id survives, name
changes) and UPDATE v2_col_map.col_name in place, leaving the clock rows alone.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, sync_all, dump, fail, ok, run, section


def mk():
    c = connect()
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a, payload)")
    c.execute("SELECT crsql_as_crr('t')")
    return c


def rename(c):
    c.execute("SELECT crsql_begin_alter('t')")
    c.execute("ALTER TABLE t RENAME COLUMN payload TO payload2")
    c.execute("SELECT crsql_commit_alter('t')")


def main():
    section("two nodes hold different values for `payload`, then both rename it")
    a, b = mk(), mk()
    a.execute("INSERT INTO t VALUES (1, 'x', 'from-A')")
    sync_all(a, b)
    b.execute("UPDATE t SET payload = 'from-B' WHERE id = 1")

    dump(a, "SELECT * FROM t", (), "A before rename")
    dump(b, "SELECT * FROM t", (), "B before rename")
    dump(b, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, col_version "
            "FROM t__crsql_v2_clock", (), "B clock before rename")

    rename(a)
    rename(b)

    section("after the rename")
    dump(a, "SELECT * FROM t__crsql_v2_col_map", (), "A col_map")
    dump(a, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, col_version "
            "FROM t__crsql_v2_clock", (), "A clock (payload's rows are gone)")
    dump(b, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, col_version "
            "FROM t__crsql_v2_clock", (), "B clock")
    dump(a, 'SELECT "table", cid FROM crsql_changes', (), "A feed")
    dump(b, 'SELECT "table", cid FROM crsql_changes', (), "B feed")

    section("sync both ways — B's newer value must reach A")
    for _ in range(3):
        sync_all(a, b)
        sync_all(b, a)
    ra = dump(a, "SELECT * FROM t", (), "A after sync")
    rb = dump(b, "SELECT * FROM t", (), "B after sync")

    a_map = a.execute("SELECT col_id FROM t__crsql_v2_col_map WHERE col_name='payload2'").fetchall()
    a_clock = a.execute(
        "SELECT count(*) FROM t__crsql_v2_clock WHERE cell_key & 4095 = ?",
        (a_map[0][0],)).fetchone()[0] if a_map else -1
    if a_clock == 0:
        print("\n  -> the renamed column has %d clock rows on A: it cannot appear in the feed"
              % a_clock)

    if ra != rb:
        fail("nodes cannot converge after the rename: A=%r B=%r "
             "(the renamed column's clock history was deleted on both sides)" % (ra, rb))
    else:
        ok("nodes converged after the rename")

    a.close()
    b.close()


run(main)
