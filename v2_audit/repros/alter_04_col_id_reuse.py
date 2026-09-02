"""Retired col_ids are recycled, contradicting the col_id reuse policy.

Design, "PK-Only Tables" -> "col_id Reuse Policy":

    For `col_id >= 1`: when a column is dropped, its `col_id` is retired. New
    columns always get `max(col_id) + 1` (after trying slot 0). This prevents a
    newly added column from inheriting stale clock entries from a previously
    dropped column.

alter_v2.rs::sync_col_map_v2 instead computes `used_col_ids` from the columns
that still exist and picks the SMALLEST unused id:

    let mut next_col_id: i64 = 0;
    while used_col_ids.contains(&next_col_id) { next_col_id += 1; }

so with a=0, b=1, c=2, dropping b and adding d gives d col_id 1 -- b's retired
id.

On the happy path nothing is inherited, because b's clock rows are deleted in
the same call. The hazard is any path that leaves clock rows behind. The failed
PK-only alter of alter_02 is exactly such a path: it empties v2_col_map while
col_id 0 and 1 clock rows survive, so the next ADD COLUMN takes col_id 0 and
inherits a stale clock row carrying someone else's db_version and seq.

Part 1 shows the policy violation. Part 2 shows the actual inheritance.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, fail, ok, run, section


def alter(c, tbl, *stmts):
    c.execute("SELECT crsql_begin_alter(?)", (tbl,))
    for s in stmts:
        c.execute(s)
    c.execute("SELECT crsql_commit_alter(?)", (tbl,))


def main():
    section("part 1 -- a=0, b=1, c=2; drop b; add d")
    a = connect()
    a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a, b, c)")
    a.execute("SELECT crsql_as_crr('t')")
    a.execute("INSERT INTO t VALUES (1, 'A', 'B', 'C')")
    dump(a, "SELECT * FROM t__crsql_v2_col_map", (), "col_map")

    alter(a, "t", "ALTER TABLE t DROP COLUMN b")
    alter(a, "t", "ALTER TABLE t ADD COLUMN d")
    dump(a, "SELECT * FROM t__crsql_v2_col_map", (), "col_map after drop b / add d")

    d_id = a.execute(
        "SELECT col_id FROM t__crsql_v2_col_map WHERE col_name='d'").fetchone()[0]
    max_other = a.execute(
        "SELECT max(col_id) FROM t__crsql_v2_col_map WHERE col_name != 'd'").fetchone()[0]
    print("     d got col_id=%d; design requires max(col_id)+1 = %d" % (d_id, max_other + 1))
    if d_id <= max_other:
        fail("new column 'd' reused retired col_id %d instead of %d" % (d_id, max_other + 1))
    else:
        ok("new column got a fresh col_id")

    section("part 2 -- inheritance after the alter_02 failure leaves clock rows behind")
    b = connect()
    b.execute("CREATE TABLE u (id INTEGER PRIMARY KEY NOT NULL, a, b)")
    b.execute("SELECT crsql_as_crr('u')")
    b.execute("INSERT INTO u VALUES (1, 'A', 'B')")
    stale = dump(b, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, "
                    "col_version, db_version, seq FROM u__crsql_v2_clock", (), "clock before")

    b.execute("SELECT crsql_begin_alter('u')")
    b.execute("ALTER TABLE u DROP COLUMN a")
    b.execute("ALTER TABLE u DROP COLUMN b")
    try:
        b.execute("SELECT crsql_commit_alter('u')")
    except Exception as e:
        print("     commit_alter failed as in alter_02: %s" % e)

    leftover = dump(b, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, "
                       "col_version, db_version, seq FROM u__crsql_v2_clock", (),
                    "clock left behind (col_map is empty)")

    # triggers were dropped by begin_alter; re-register so ADD COLUMN goes through
    b.execute("SELECT crsql_as_crr('u')")
    try:
        b.execute("SELECT crsql_begin_alter('u')")
        b.execute("ALTER TABLE u ADD COLUMN fresh")
        b.execute("SELECT crsql_commit_alter('u')")
    except Exception as e:
        print("     add column failed: %s" % e)

    newmap = dump(b, "SELECT * FROM u__crsql_v2_col_map", (), "col_map after ADD COLUMN fresh")
    after = dump(b, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id, "
                    "col_version, db_version, seq FROM u__crsql_v2_clock", (),
                 "clock after -- 'fresh' now owns these rows")

    if newmap and after:
        fresh_id = [r[0] for r in newmap if r[1] == "fresh"]
        if fresh_id and any(r[1] == fresh_id[0] for r in leftover):
            fail("new column 'fresh' (col_id %d) inherited %d stale clock row(s) "
                 "left over from a dropped column -- they carry the dropped column's "
                 "db_version/seq and will be emitted as changes for 'fresh'"
                 % (fresh_id[0], sum(1 for r in leftover if r[1] == fresh_id[0])))
        else:
            ok("no stale clock rows were inherited")
    else:
        ok("no clock rows to inherit")

    a.close()
    b.close()


run(main)
