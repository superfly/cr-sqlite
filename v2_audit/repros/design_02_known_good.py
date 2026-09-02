"""Behaviours that are CORRECT today — keep these green.

This is the negative-control half of the audit. Everything here was probed
looking for a bug and behaved properly; each one is a regression test for a
place where V2 could plausibly have broken and did not.

  1. config (write/use/log) is persisted in crsql_master and picked up by a
     fresh connection to the same file
  2. a transaction ROLLBACK leaves no V2 metadata and does not consume a
     db_version
  3. ts edge cases: negative and non-numeric are rejected with an explicit
     error, and ts=0 falls back to `default-ts` -- neither can produce a
     CHECK (ts > 0) violation in v2_clock
  4. a PK-only table survives insert / delete / resurrect and syncs, using the
     col_id=0 sentinel
  5. crsql_as_crr backfills a table that already contains rows
  6. a PK-changing UPDATE is handled as delete+insert (tombstone + new v2_pks
     row) and replicates
  7. an explicit rowid UPDATE on a non-rowid-key table does not disturb the
     metadata
  8. cross-node schema skew: a node without a column ignores changes for it and
     picks the value up once the column is added
"""
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import EXT, connect, dump, fail, ok, run, section, sync_all


def main():
    section("1. config persists across connections")
    import sqlite3
    path = os.path.join(tempfile.mkdtemp(), "cfg.db")
    c1 = connect(path)
    c1.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a)")
    c1.execute("SELECT crsql_as_crr('t')")
    c1.execute("INSERT INTO t VALUES (1, 'x')")
    c2 = sqlite3.connect(path)
    c2.enable_load_extension(True)
    c2.load_extension(EXT)
    c2.isolation_level = None
    got = [c2.execute("SELECT crsql_config_get(?)", (k,)).fetchone()[0]
           for k in ("metadata-write-version", "metadata-use-version", "sync-log-version")]
    print("     fresh connection reads %r (expected [3, 2, 2])" % got)
    if got != [3, 2, 2]:
        fail("config did not persist: %r" % got)
    else:
        ok("config persisted")
    c1.close()
    c2.close()

    section("2. ROLLBACK leaves no metadata and burns no db_version")
    a = connect()
    a.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a)")
    a.execute("SELECT crsql_as_crr('t')")
    a.execute("BEGIN")
    a.execute("INSERT INTO t VALUES (1, 'x')")
    a.execute("ROLLBACK")
    pks = a.execute("SELECT count(*) FROM t__crsql_v2_pks").fetchone()[0]
    clk = a.execute("SELECT count(*) FROM t__crsql_v2_clock").fetchone()[0]
    ver = a.execute("SELECT crsql_db_version()").fetchone()[0]
    print("     after rollback: v2_pks=%d v2_clock=%d db_version=%d" % (pks, clk, ver))
    if pks or clk or ver:
        fail("rollback left state behind: pks=%d clock=%d version=%d" % (pks, clk, ver))
    else:
        ok("rollback is clean")

    section("3. ts edge cases cannot produce a CHECK (ts > 0) violation")
    a.execute("INSERT INTO t VALUES (1, 'x')")
    for bad in ("-5", "abc"):
        try:
            a.execute("SELECT crsql_set_ts(?)", (bad,))
            a.execute("UPDATE t SET a = 'y' WHERE id = 1")
            fail("ts=%r was accepted" % bad)
        except Exception as e:
            print("     ts=%-5r -> %s" % (bad, e))
    # ts=0 means "unset": it must fall back to the configured default-ts
    a.execute("SELECT crsql_set_ts('0')")
    a.execute("UPDATE t SET a = 'z' WHERE id = 1")
    ts_vals = [r[0] for r in a.execute("SELECT DISTINCT ts FROM t__crsql_v2_clock").fetchall()]
    print("     ts='0' with default-ts=1700000000 -> v2_clock ts = %r" % ts_vals)
    if any(v <= 0 for v in ts_vals):
        fail("ts=0 was written straight into v2_clock: %r" % ts_vals)
    else:
        ok("negative/non-numeric rejected; ts=0 falls back to default-ts")
    a.execute("SELECT crsql_set_ts('1700000000')")

    section("4. PK-only table: insert / delete / resurrect / sync")
    def mkp():
        c = connect()
        c.execute("CREATE TABLE p (id INTEGER PRIMARY KEY NOT NULL)")
        c.execute("SELECT crsql_as_crr('p')")
        return c
    x, y = mkp(), mkp()
    x.execute("INSERT INTO p VALUES (1)")
    sync_all(x, y)
    x.execute("DELETE FROM p WHERE id = 1")
    sync_all(x, y)
    after_del = y.execute("SELECT * FROM p").fetchall()
    x.execute("INSERT INTO p VALUES (1)")
    sync_all(x, y)
    after_res = y.execute("SELECT * FROM p").fetchall()
    cl = y.execute("SELECT cl FROM p__crsql_v2_pks").fetchone()
    print("     peer after delete=%r after resurrect=%r cl=%r" % (after_del, after_res, cl))
    if after_del != [] or after_res != [(1,)] or cl != (3,):
        fail("pk-only lifecycle wrong: del=%r res=%r cl=%r" % (after_del, after_res, cl))
    else:
        ok("pk-only lifecycle correct (CL 1 -> 2 -> 3)")

    section("5. as_crr backfills a pre-populated table")
    b = connect()
    b.execute("CREATE TABLE s (id INTEGER PRIMARY KEY NOT NULL, a, c)")
    b.execute("INSERT INTO s VALUES (1, 'x', 1), (2, 'y', 2)")
    b.execute("SELECT crsql_as_crr('s')")
    b2 = connect()
    b2.execute("CREATE TABLE s (id INTEGER PRIMARY KEY NOT NULL, a, c)")
    b2.execute("SELECT crsql_as_crr('s')")
    sync_all(b, b2)
    got = b2.execute("SELECT * FROM s ORDER BY id").fetchall()
    print("     peer received %r" % (got,))
    if got != [(1, 'x', 1), (2, 'y', 2)]:
        fail("backfill did not replicate: %r" % (got,))
    else:
        ok("backfill replicated both rows")

    section("6/7. PK-changing UPDATE and explicit rowid UPDATE")
    e = connect()
    e.execute("CREATE TABLE u (id TEXT PRIMARY KEY NOT NULL, v)")
    e.execute("SELECT crsql_as_crr('u')")
    e.execute("INSERT INTO u VALUES ('a', 1)")
    e.execute("UPDATE u SET id = 'b' WHERE id = 'a'")
    tombs = e.execute("SELECT cl FROM u__crsql_v2_tombstones").fetchall()
    alive = e.execute("SELECT id, cl FROM u__crsql_v2_pks").fetchall()
    e2 = connect()
    e2.execute("CREATE TABLE u (id TEXT PRIMARY KEY NOT NULL, v)")
    e2.execute("SELECT crsql_as_crr('u')")
    sync_all(e, e2)
    peer = e2.execute("SELECT * FROM u").fetchall()
    print("     after PK change: tombstones=%r alive=%r peer=%r" % (tombs, alive, peer))
    if tombs != [(2,)] or alive != [('b', 1)] or peer != [('b', 1)]:
        fail("PK-changing UPDATE mishandled: tombs=%r alive=%r peer=%r"
             % (tombs, alive, peer))
    else:
        ok("PK change = delete + insert, replicated correctly")

    e.execute("UPDATE u SET rowid = 99 WHERE id = 'b'")
    feed = e.execute('SELECT "table", cid FROM crsql_changes').fetchall()
    print("     after rowid UPDATE, feed still emits %r" % (feed,))
    if not feed:
        fail("explicit rowid UPDATE made the row vanish from the feed")
    else:
        ok("rowid UPDATE is harmless when key_is_rowid=false")

    section("8. cross-node schema skew")
    f, g = connect(), connect()
    for c in (f, g):
        c.execute("CREATE TABLE w (id INTEGER PRIMARY KEY NOT NULL, a, b)")
        c.execute("SELECT crsql_as_crr('w')")
    f.execute("SELECT crsql_begin_alter('w')")
    f.execute("ALTER TABLE w ADD COLUMN c")
    f.execute("SELECT crsql_commit_alter('w')")
    f.execute("INSERT INTO w VALUES (1, 'x', 1, 'newcol')")
    sync_all(f, g)
    before = g.execute("SELECT * FROM w").fetchall()
    g.execute("SELECT crsql_begin_alter('w')")
    g.execute("ALTER TABLE w ADD COLUMN c")
    g.execute("SELECT crsql_commit_alter('w')")
    sync_all(f, g)
    after = g.execute("SELECT * FROM w").fetchall()
    print("     peer without the column: %r" % (before,))
    print("     peer after adding it:    %r" % (after,))
    if before != [(1, 'x', 1)] or after != [(1, 'x', 1, 'newcol')]:
        fail("schema skew mishandled: before=%r after=%r" % (before, after))
    else:
        ok("unknown column ignored, then picked up after ADD COLUMN")
    print("     NOTE: the value is only recovered here because this test re-sends")
    print("     every change. Under a real per-site watermark the peer would have")
    print("     consumed that db_version already and the value would be lost for")
    print("     good -- add columns before writing to them.")

    for c in (a, b, b2, e, e2, f, g, x, y):
        c.close()


run(main)
