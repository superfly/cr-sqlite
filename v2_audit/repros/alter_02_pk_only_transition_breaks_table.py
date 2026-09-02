"""Dropping the last TWO non-PK columns in one ALTER window fails, leaves the
table with NO TRIGGERS, and silently stops tracking every later write.

Mechanism
---------
alter_v2.rs::sync_col_map_v2 handles the normal -> PK-only transition by
migrating one dropped column's clock entries down to col_id=0:

    if will_be_pk_only && !dropped_col_ids.is_empty() {
        let migrate_col_id = dropped_col_ids.pop().unwrap();
        UPDATE v2_clock SET cell_key = cell_key & ~mask WHERE cell_key & mask = ?
    }

When BOTH remaining columns are dropped in the same crsql_commit_alter window,
dropped_col_ids = [0, 1]. It pops col_id 1 and rewrites those rows to
cell_key = key<<12 | 0 -- but rows for col_id 0 ALREADY occupy exactly those
cell_key values. v2_clock.cell_key is INTEGER PRIMARY KEY, so the UPDATE dies on
a uniqueness violation and crsql_compact_post_alter_v2 returns an error.

Three separate failures follow from that one error:

1. crsql_commit_alter errors out.
2. crsql_begin_alter already dropped the table's triggers and nothing puts them
   back on the error path, so the table is left as a plain SQLite table with
   cr-sqlite metadata attached. Every subsequent INSERT/UPDATE/DELETE is
   invisible to cr-sqlite: no v2_pks row, no clock row, nothing in the feed, and
   NO ERROR anywhere. The row exists locally and can never replicate.
3. The alter is left half-applied: v2_col_map was already emptied by the
   preceding DELETE while the clock rows survive, so the PK-only feed query
   (which no longer joins v2_col_map) emits ONE DUPLICATE CHANGE EVENT PER
   LEFTOVER col_id for the same row.

Design references
-----------------
"PK-Only Tables" -> "ALTER TABLE: Dropping the Last Non-PK Column": step 2 says
"If multiple columns are dropped in the same crsql_commit_alter call, the last
dropped column's entries are migrated; other dropped columns' entries are
deleted normally" -- the deletes are ordered AFTER the migration, so the
migration collides. Step 3 ("Create missing sentinels") is not implemented at
all, so rows in v2_pks that had no clock entry stay invisible in PK-only mode.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, fail, ok, run, section


def main():
    c = connect()
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a, b)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1, 'x', 'y')")

    section("before the alter")
    dump(c, "SELECT * FROM t__crsql_v2_col_map", (), "col_map")
    dump(c, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id FROM t__crsql_v2_clock",
         (), "clock")
    trigs_before = [r[0] for r in c.execute(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 't__%'").fetchall()]
    print("     triggers: %r" % trigs_before)

    section("drop BOTH non-PK columns inside one begin/commit_alter window")
    c.execute("SELECT crsql_begin_alter('t')")
    c.execute("ALTER TABLE t DROP COLUMN a")
    c.execute("ALTER TABLE t DROP COLUMN b")
    commit_err = None
    try:
        c.execute("SELECT crsql_commit_alter('t')")
    except Exception as e:
        commit_err = e
        print("     crsql_commit_alter raised: %s" % e)

    section("state left behind")
    dump(c, "SELECT * FROM t__crsql_v2_col_map", (), "col_map (emptied)")
    dump(c, "SELECT cell_key >> 12 AS key, cell_key & 4095 AS col_id FROM t__crsql_v2_clock",
         (), "clock (NOT migrated -- alter half-applied)")
    trigs_after = [r[0] for r in c.execute(
        "SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 't__%'").fetchall()]
    print("     triggers: %r" % trigs_after)

    section("consequence 1: later writes are silently untracked")
    c.execute("INSERT INTO t VALUES (2)")
    rows = dump(c, "SELECT * FROM t", (), "base table")
    pks = dump(c, "SELECT * FROM t__crsql_v2_pks", (), "v2_pks")
    feed = dump(c, 'SELECT "table", cid, cl, db_version FROM crsql_changes', (), "feed")

    section("verdict")
    if commit_err is None:
        ok("crsql_commit_alter succeeded")
    else:
        fail("crsql_commit_alter failed dropping the last two non-PK columns: %s" % commit_err)

    if not trigs_after:
        fail("table left with NO triggers (had %r) -- every later local write is "
             "invisible to cr-sqlite with no error" % trigs_before)
    else:
        ok("triggers survived the failed alter")

    if len(rows) != len(pks):
        fail("base table has %d rows but v2_pks has %d -- row(s) %r will never replicate"
             % (len(rows), len(pks),
                sorted(set(r[0] for r in rows) - set(p[1] for p in pks))))
    else:
        ok("every base row has a v2_pks entry")

    keys = [r[3] for r in c.execute(
        'SELECT "table", cid, cl, db_version FROM crsql_changes').fetchall()] if feed else []
    if len(feed) > len(pks):
        fail("feed emits %d events for %d tracked row(s) -- leftover col_ids each "
             "produce a duplicate PK-only sentinel event" % (len(feed), len(pks)))
    else:
        ok("feed event count matches the tracked rows")

    c.close()


run(main)
