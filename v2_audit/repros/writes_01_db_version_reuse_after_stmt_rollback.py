"""writes_01: db_version is REUSED after a statement/savepoint rollback.

FILE: core/rs/core/src/db_version.rs:91  (`next_db_version`)

    // update db_version in db if it changed
    if ret != unsafe { (*ext_data).pendingDbVersion } { ...write crsql_db_versions... }

`next_db_version` writes the `crsql_db_versions` row ONLY when the value it just
computed differs from the in-memory `pendingDbVersion`. `pendingDbVersion` lives in
`crsql_ExtData` and is reset only by the commit/rollback hooks
(`commit.rs::commit_or_rollback_reset`), which fire on a *transaction* rollback --
NOT on a statement-level abort (constraint violation) and NOT on `ROLLBACK TO
<savepoint>`.

So the sequence:
    BEGIN
      INSERT ... -- trigger -> next_db_version(): ret=1 != pending(-1) -> WRITES
                 --                                pendingDbVersion = 1
      <statement aborts on a UNIQUE violation; SQLite's statement journal undoes
       the crsql_db_versions write, but pendingDbVersion stays 1 in memory>
      INSERT ... -- trigger -> next_db_version(): ret=1 == pending(1) -> SKIPS the write
    COMMIT
leaves clock rows stamped db_version=1 while `crsql_db_versions` has NO row.

EXPECTED: after COMMIT, `crsql_db_versions` holds the site's max db_version (1), so a
          later connection continues from 2 and every change gets a unique db_version.
ACTUAL:   `crsql_db_versions` is empty. A fresh connection reads db_version = 0 from
          storage and hands out db_version = 1 AGAIN to the next local write. Two
          distinct changes now carry the same db_version.

CONSEQUENCE: a peer that already pulled up to db_version=1 syncs with
`WHERE db_version > 1` and will never see the second change -- silent, permanent
divergence with no error anywhere. `ROLLBACK TO SAVEPOINT` (used by ORMs / nested
transactions, and by cr-sqlite's own `crsql_as_crr`) reproduces it identically.
"""
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, close, dump, section, fail, ok, run


def scenario_statement_abort(path):
    c = connect(path)
    c.execute("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, a)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("BEGIN")
    try:
        # row 1 succeeds (writes crsql_db_versions), row 2 violates the PK ->
        # the whole statement is rolled back by SQLite's statement journal.
        c.execute("INSERT INTO t VALUES ('a', 1), ('a', 2)")
    except Exception as e:
        print("     (expected statement abort: %s)" % e)
    c.execute("INSERT INTO t VALUES ('b', 2)")
    c.execute("COMMIT")
    return c


def scenario_savepoint(path):
    c = connect(path)
    c.execute("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, a)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("BEGIN")
    c.execute("SAVEPOINT s1")
    c.execute("INSERT INTO t VALUES ('a', 1)")
    c.execute("ROLLBACK TO s1")
    c.execute("INSERT INTO t VALUES ('b', 2)")
    c.execute("COMMIT")
    return c


def main():
    for name, setup in (("statement abort (UNIQUE violation)", scenario_statement_abort),
                        ("ROLLBACK TO SAVEPOINT", scenario_savepoint)):
        section(name)
        path = os.path.join(tempfile.mkdtemp(), "db.sqlite")
        c = setup(path)

        rows = dump(c, "SELECT * FROM crsql_db_versions", label="crsql_db_versions after COMMIT")
        dump(c, "SELECT * FROM t__crsql_v2_clock", label="v2_clock (row 'b' is stamped db_version=1)")
        v_mem = c.execute("SELECT crsql_db_version()").fetchone()[0]
        print("     crsql_db_version() on the writing connection: %d" % v_mem)
        close(c)

        if rows:
            ok("crsql_db_versions row survived")
            continue
        fail("crsql_db_versions is EMPTY after COMMIT even though clock rows carry db_version=1")

        # Now show the actual reuse: reopen and write again.
        c2 = connect(path, write=None, use=None, log=None)
        v_reopen = c2.execute("SELECT crsql_db_version()").fetchone()[0]
        print("     crsql_db_version() after reopen: %d  (expected %d)" % (v_reopen, v_mem))
        c2.execute("INSERT INTO t VALUES ('c', 3)")
        clock = dump(c2, "SELECT cell_key, db_version, seq FROM t__crsql_v2_clock ORDER BY cell_key",
                     label="v2_clock after the post-reopen insert")
        versions = [r[1] for r in clock]
        if len(versions) != len(set(versions)):
            fail("db_version %r REUSED for two independent changes ('b' and 'c') -- "
                 "a peer synced past db_version=%d will never receive the other one"
                 % (versions, max(versions)))
        else:
            ok("db_versions are distinct")

        dump(c2, "SELECT tbl_name, pk, db_version, seq FROM crsql_changes ORDER BY db_version, seq",
             label="crsql_changes (both rows share one db_version)")
        close(c2)


run(main)
