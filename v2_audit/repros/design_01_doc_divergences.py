"""Design-doc vs implementation divergences (finding 17).

None of these is a correctness bug on its own, but each one means a reader who
follows v2_metadata_design.md gets behaviour the code does not implement. Each
check below prints what the doc says and what the build actually does.

  17.1  config values are integers, the doc uses strings ('v1' / 'v2' / 'v2&v1')
  17.2  key_is_rowid defaults to false for every table; the doc's
        "Rowid Reuse: Opt-in vs Opt-out" table says plain rowid tables default on
  17.3  directive keys: doc says use_rowid_key / without_rowid; the code reads
        use_rowid and never reads without_rowid
  17.4  the rowid-bounds and typeof() CHECK constraints the doc mandates are not
        on the main table (SQLite has no ALTER TABLE ADD CONSTRAINT)
  17.5  the value-extraction CASE compares col_name strings, not col_id integers,
        contrary to the doc's stated efficiency rationale (and the direct cause
        of feed_03)
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, fail, ok, run, section

BIG = 1 << 60


def main():
    section("17.1 -- config takes integers, the doc writes strings")
    c = connect(write=None, use=None, log=None, ts="1700000000")
    try:
        c.execute("SELECT crsql_config_set('metadata-write-version', 'v2&v1')")
        ok("the documented string form was accepted")
    except Exception as e:
        print("     crsql_config_set(..., 'v2&v1')  ->  %s" % e)
        fail("the doc's string config values are rejected; the build takes "
             "integers (1=v1, 2=v2&v1, 3=v2)")
    c.execute("SELECT crsql_config_set('metadata-write-version', 2)")
    print("     integer form works: metadata-write-version = %r"
          % c.execute("SELECT crsql_config_get('metadata-write-version')").fetchone()[0])
    c.close()

    section("17.2 / 17.3 -- key_is_rowid default and the use_rowid_key directive")
    d = connect()
    d.execute("CREATE TABLE plain (id TEXT PRIMARY KEY NOT NULL, a)")
    d.execute("SELECT crsql_as_crr('plain')")
    d.execute("INSERT INTO plain (rowid, id, a) VALUES (?, 'z', 1)", (BIG,))
    row = d.execute("SELECT rowid FROM plain").fetchone()[0]
    key = d.execute("SELECT __crsql_key FROM plain__crsql_v2_pks").fetchone()[0]
    print("     plain rowid table: base rowid=%d, __crsql_key=%d" % (row, key))
    if key != row:
        fail("plain rowid table has key_is_rowid=false (doc section 3 says it "
             "defaults to TRUE / opt-out). The code's reasoning (VACUUM renumbers "
             "implicit rowids) is sound -- the DOC is stale here, not the code")
    else:
        ok("plain rowid table reuses the rowid as documented")

    d.execute("CREATE TABLE withdir /* crsql: use_rowid_key=1 */ "
              "(id INTEGER PRIMARY KEY NOT NULL, a)")
    d.execute("SELECT crsql_as_crr('withdir')")
    d.execute("INSERT INTO withdir VALUES (7, 'x')")
    k = d.execute("SELECT __crsql_key FROM withdir__crsql_v2_pks").fetchone()[0]
    print("     /* crsql: use_rowid_key=1 */ on INTEGER PK 7 -> __crsql_key=%d" % k)
    if k != 7:
        fail("the documented directive key `use_rowid_key` is ignored; "
             "schema_directive.rs reads `use_rowid`")
    else:
        ok("use_rowid_key honoured")

    d.execute("CREATE TABLE withdir2 /* crsql: use_rowid=1 */ "
              "(id INTEGER PRIMARY KEY NOT NULL, a)")
    d.execute("SELECT crsql_as_crr('withdir2')")
    d.execute("INSERT INTO withdir2 VALUES (7, 'x')")
    k2 = d.execute("SELECT __crsql_key FROM withdir2__crsql_v2_pks").fetchone()[0]
    print("     /* crsql: use_rowid=1   */ on INTEGER PK 7 -> __crsql_key=%d "
          "(this is the key the code actually reads)" % k2)

    section("17.4 -- the CHECK constraints the design mandates are absent")
    for t in ("plain", "withdir2"):
        sql = d.execute("SELECT sql FROM sqlite_master WHERE name = ?", (t,)).fetchone()[0]
        print("     %s: %s" % (t, sql.replace("\n", " ")))
    have_check = any("CHECK" in (d.execute(
        "SELECT sql FROM sqlite_master WHERE name = ?", (t,)).fetchone()[0] or "")
        for t in ("plain", "withdir2"))
    if not have_check:
        fail("no rowid-bounds CHECK and no typeof(pk)='integer' CHECK on the main "
             "tables (design 3 'Large Rowid Handling' and 'Runtime Guard'); SQLite "
             "has no ALTER TABLE ADD CONSTRAINT so as_crr cannot add them -- this is "
             "why the merge path is unguarded (finding 8)")
    else:
        ok("CHECK constraints present")

    section("17.5 -- the feed's value CASE compares col_name strings, not col_ids")
    e = connect()
    e.execute("CREATE TABLE q (id INTEGER PRIMARY KEY NOT NULL, alpha, beta)")
    e.execute("SELECT crsql_as_crr('q')")
    e.execute("INSERT INTO q VALUES (1, 'x', 'y')")
    plan = e.execute("EXPLAIN QUERY PLAN SELECT * FROM crsql_changes").fetchall()
    for p in plan:
        print("     %r" % (p,))
    print("     (changes_vtab_read.rs::build_col_val_case emits "
          "`CASE cm.col_name WHEN 'alpha' THEN mt.\"alpha\" ...`;")
    print("      the design specifies `CASE cm.col_id WHEN 0 THEN ...` and says "
          "\"Integer comparison is used instead of")
    print("      string comparison on col_name for efficiency\")")
    fail("the compiled CASE uses per-cell string comparison on col_name instead of "
         "the documented integer col_id comparison; this is also the direct cause of "
         "feed_03 (a quote in a column name breaks the whole feed)")

    d.close()
    e.close()


run(main)
