"""schema_04: the `skip_hash` argument to crsql_as_crr() is unusable -- it
always leaves the v2 tables with an empty-string PK column name and the
registration fails.

pull_table_info (tableinfo.rs:1216) computes

    skip_hash_pk_col = if skip_hash && !pks.is_empty() { escape_ident(&pks[0].name) }
                       else { String::new() }

create_crr.rs:80 then flips the flag *after* that:

    if skip_hash_flag && !table_info.skip_hash { table_info.skip_hash = true; }

without recomputing skip_hash_pk_col. bootstrap_v2.rs:106/118/180/189 emit
`"" <type> NOT NULL` for the PK column, and backfill/local writes then fail.

Design doc, "Skip Hash Optimization / Eligibility":
    Manually enabled via schema directive (`/* crsql: skip_hash=1 */`) *or the
    `as_crr` option*: any single-column PK ...
Design doc, "Eligibility" (composite PKs):
    If a `skip_hash=1` directive is present on a table with multiple PK columns,
    it is silently ignored and the table falls back to hash mode.

Expected: crsql_as_crr('t','skip_hash') behaves exactly like the
          `/* crsql: skip_hash=1 */` directive, and is silently ignored on a
          composite PK.
Actual:   every use of the flag that is not already auto-qualified errors out
          ("backfill_table_v2 failed ... CONSTRAINT/ERROR"), including the
          composite-PK case which the design says must be a silent no-op.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run


def attempt(title, ddl, tbl, call):
    section(title)
    c = connect()
    c.execute(ddl)
    print("     %s" % call)
    try:
        c.execute("SELECT %s" % call)
        print("     -> OK")
        err = None
    except Exception as e:
        err = str(e)
        print("     -> ERROR: %s" % err)
    dump(c, "SELECT name, sql FROM sqlite_master WHERE type='table' AND name LIKE ?",
         (tbl + "__crsql_v2_pks",), label="v2_pks")
    return err


def main():
    e_blob = attempt(
        "single BLOB PK + as_crr 'skip_hash' (design: allowed, manual opt-in)",
        "CREATE TABLE t5 (id BLOB PRIMARY KEY NOT NULL, v TEXT)", "t5",
        "crsql_as_crr('t5','skip_hash')")

    section("control: the SAME table via the schema directive works")
    c = connect()
    c.execute("CREATE TABLE t5b /* crsql: skip_hash=1 */ (id BLOB PRIMARY KEY NOT NULL, v TEXT)")
    c.execute("SELECT crsql_as_crr('t5b')")
    dump(c, "SELECT sql FROM sqlite_master WHERE name='t5b__crsql_v2_pks'")
    print("     -> the directive path sets skip_hash_pk_col, the flag path does not")

    e_dir = attempt(
        "directive skip_hash=0 + as_crr 'skip_hash'",
        "CREATE TABLE t4 /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, v TEXT)",
        "t4", "crsql_as_crr('t4','skip_hash')")

    e_comp = attempt(
        "composite PK + as_crr 'skip_hash' (design: silently ignored)",
        "CREATE TABLE t6 (a TEXT NOT NULL, b TEXT NOT NULL, v TEXT, PRIMARY KEY(a,b))",
        "t6", "crsql_as_crr('t6','skip_hash')")

    section("verdict")
    if e_blob:
        fail("as_crr('t5','skip_hash') on a single BLOB PK failed: %s" % e_blob)
    else:
        ok("manual skip_hash via the as_crr flag works for a BLOB PK")
    if e_dir:
        fail("as_crr('t4','skip_hash') on an auto-qualified table with "
             "skip_hash=0 in the schema failed: %s" % e_dir)
    else:
        ok("flag/directive combination handled")
    if e_comp:
        fail("as_crr('t6','skip_hash') on a composite PK errored instead of "
             "being silently ignored: %s" % e_comp)
    else:
        ok("composite PK + skip_hash flag silently ignored")


run(main)
