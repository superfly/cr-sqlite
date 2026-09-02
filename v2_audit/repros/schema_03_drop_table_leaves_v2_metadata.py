"""schema_03: DROP TABLE / crsql_as_table leave every V2 metadata table (and the
crsql_master mode flags) behind; re-creating the table then adopts the stale
metadata and the CRR is broken.

bootstrap_v2.rs::drop_v2_tables and teardown_v2.rs::remove_crr_v2_tables exist,
but remove_crr_v2_tables is never called from anywhere (rustc reports
"function `remove_crr_v2_tables` is never used"). lib.rs::crsql_as_table_impl
only calls remove_crr_clock_table_if_exists (V1 tables) + trigger removal, and
nothing at all reacts to a plain DROP TABLE.

Consequences after `DROP TABLE t; CREATE TABLE t (...); crsql_as_crr('t')`:
  * t__crsql_v2_pks / v2_clock / v2_tombstones still hold the *old* table's rows.
    bootstrap_v2 uses CREATE TABLE IF NOT EXISTS, so they are silently adopted.
  * tableinfo.rs:1162 infers skip_hash from the leftover v2_pks schema and
    tableinfo.rs:1222 reads the leftover `use_rowid_<t>` / `skip_hash_<t>` keys
    from crsql_master, so the new table inherits the *old* table's modes --
    including key_is_rowid = true on a table with no INTEGER PRIMARY KEY, which
    create_crr.rs:57 would otherwise reject outright.
  * Inserting a PK that the old incarnation used aborts the statement with an
    internal consistency error.

Expected: dropping the base table (or downgrading it with crsql_as_table)
          removes its V2 metadata and mode flags.
Actual:   everything survives and poisons the next table with the same name.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run


def v2_tables(c, base):
    return [r[0] for r in c.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE ?",
        (base + "__crsql_v2%",)).fetchall()]


def main():
    section("part 1 -- DROP TABLE leaves the V2 metadata behind")
    c = connect()
    c.execute("CREATE TABLE z (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    c.execute("SELECT crsql_as_crr('z')")
    c.execute("INSERT INTO z VALUES (1,'old1')")
    c.execute("INSERT INTO z VALUES (2,'old2')")
    c.execute("DELETE FROM z WHERE id = 2")
    c.execute("DROP TABLE z")
    leftovers = v2_tables(c, "z")
    print("     tables still present: %r" % leftovers)
    dump(c, "SELECT * FROM z__crsql_v2_pks", label="stale v2_pks (id=1 still 'alive')")
    dump(c, "SELECT * FROM z__crsql_v2_tombstones", label="stale tombstone for id=2")
    dump(c, "SELECT key, value FROM crsql_master WHERE key LIKE '%\\_z' ESCAPE '\\'",
         label="stale crsql_master flags")
    if leftovers:
        fail("DROP TABLE z left %d V2 metadata tables: %r" % (len(leftovers), leftovers))
    else:
        ok("DROP TABLE cleaned up the V2 metadata")

    section("part 2 -- re-creating z adopts the stale metadata and breaks")
    c.execute("CREATE TABLE z (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    c.execute("SELECT crsql_as_crr('z')")
    dump(c, "SELECT * FROM z__crsql_v2_pks", label="v2_pks of the *new* z")
    broke = False
    try:
        c.execute("INSERT INTO z VALUES (1,'new1')")
        print("     insert id=1 succeeded")
    except Exception as e:
        broke = True
        print("     insert id=1 FAILED: %s" % e)
    dump(c, "SELECT * FROM z", label="new z contents")
    if broke:
        fail("a plain INSERT into the freshly created table fails because the "
             "old incarnation's v2_pks row was inherited")
    else:
        ok("new table is usable")

    section("part 3 -- stale use_rowid_<t> flag bypasses the create_crr guard")
    c2 = connect()
    c2.execute("CREATE TABLE q (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    c2.execute("SELECT crsql_as_crr('q','use_rowid')")
    c2.execute("DROP TABLE q")
    c2.execute("DROP TABLE q__crsql_v2_pks")
    c2.execute("DROP TABLE q__crsql_v2_clock")
    c2.execute("DROP TABLE q__crsql_v2_tombstones")
    c2.execute("DROP TABLE q__crsql_v2_col_map")
    # only the crsql_master flags are left now
    dump(c2, "SELECT key, value FROM crsql_master WHERE key LIKE '%\\_q' ESCAPE '\\'",
         label="surviving flags")
    c2.execute("CREATE TABLE q (id TEXT PRIMARY KEY NOT NULL, v TEXT)")
    c2.execute("SELECT crsql_as_crr('q')")   # no use_rowid flag this time
    sql = c2.execute("SELECT sql FROM sqlite_master WHERE name='q__crsql_v2_pks'").fetchone()[0]
    print("     new q__crsql_v2_pks: %s" % " ".join(sql.split()))
    sig = c2.execute("SELECT value FROM crsql_master WHERE key = 'v2_pks_q'").fetchone()[0]
    print("     recorded mode signature: %r  ('r' prefix = key_is_rowid)" % sig)
    # rowid-key mode stores no PK column in v2_pks; __crsql_key is the implicit
    # rowid of a TEXT-PK table, which VACUUM is free to renumber.
    if sig.startswith("r") and '"id"' not in sql:
        fail("stale use_rowid_q=1 forced key_is_rowid on a TEXT-PK table; a direct "
             "crsql_as_crr('q','use_rowid') is rejected by create_crr.rs:57, but the "
             "leftover crsql_master flag reaches key_is_rowid without any check")
    else:
        ok("stale flag did not leak into the new registration")

    section("part 4 -- crsql_as_table() also leaves the V2 tables")
    c3 = connect()
    c3.execute("CREATE TABLE y (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    c3.execute("SELECT crsql_as_crr('y')")
    c3.execute("INSERT INTO y VALUES (1,'a')")
    c3.execute("SELECT crsql_as_table('y')")
    left = v2_tables(c3, "y")
    print("     after crsql_as_table('y'): %r" % left)
    if left:
        fail("crsql_as_table left %d V2 metadata tables behind" % len(left))
    else:
        ok("crsql_as_table cleaned up")


run(main)
