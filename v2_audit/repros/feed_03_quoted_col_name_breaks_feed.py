"""feed_03: a column name containing a single quote makes the whole crsql_changes
feed unusable (SQL logic error), for every table in the database.

Design: "Feed (Changes Since db_version for a Site)" / "Value extraction via
compiled CASE statement" -- the feed compiles
    CASE cm.col_id WHEN 0 THEN mt.col_a WHEN 1 THEN mt.col_b ... END

EXPECTED: the compiled CASE is valid SQL for any legal SQLite column name.

ACTUAL:   changes_vtab_read.rs:203-221 (`build_col_val_case`) emits
            WHEN '{col_name}' THEN mt."{col_name}"
          and escapes BOTH occurrences with `crate::util::escape_ident`, which
          only doubles double-quotes (util.rs:118-120).  The first occurrence is
          a SQL *string literal*, so it needs `escape_ident_as_value` (doubling
          single quotes, util.rs:122-124).  A column named  o'brien  produces
            WHEN 'o'brien' THEN mt."o'brien"
          which fails to parse.  The feed query is one UNION over ALL crr
          tables, so preparation fails and `SELECT * FROM crsql_changes` errors
          out for every table, not just the offending one.

          This is also an identifier->SQL-literal injection point: a column name
          is attacker/schema controlled text spliced unescaped into the feed SQL.

CONSEQUENCE: a single legally-named column bricks change replication for the
          entire database -- no changes can be read out at all, in either V2
          wire mode or V1-compat wire mode (both go through build_col_val_case).
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run, LOG_V1


def probe(c, label):
    section(label)
    c.execute("CREATE TABLE ok_tbl (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")
    c.execute("SELECT crsql_as_crr('ok_tbl')")
    c.execute("INSERT INTO ok_tbl VALUES (1,'fine')")
    dump(c, "SELECT quote(cid), quote(val) FROM crsql_changes", label="feed with only the healthy table")

    c.execute("""CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, "o'brien" TEXT)""")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1,'v')")
    dump(c, "SELECT name FROM pragma_table_info('t')", label="t columns")

    err = None
    try:
        rows = c.execute("SELECT quote(cid), quote(val) FROM crsql_changes").fetchall()
        print("     %r" % (rows,))
    except Exception as e:
        err = e
        print("     <error: %s: %s>" % (type(e).__name__, e))
    return err


def main():
    e2 = probe(connect(), "V2 wire (sync-log-version = 2)")
    e1 = probe(connect(log=LOG_V1), "V1-compat wire (sync-log-version = 1, metadata v2)")

    if e2 is not None or e1 is not None:
        fail("crsql_changes fails to prepare when a crr column name contains a "
             "single quote (v2wire err=%r, v1wire err=%r); the healthy table's "
             "changes become unreadable too" % (e2, e1))
    else:
        ok("feed handled quoted column names")


run(main)
