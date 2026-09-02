"""Shared harness for v2 metadata audit repro scripts.

Usage from a repro script in v2_audit/repros/:

    import sys, os
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from harness import connect, sync, dump, section, fail, ok, run

Run with the homebrew python (system python lacks load_extension):
    /opt/homebrew/bin/python3 v2_audit/repros/<script>.py
"""
import os
import sqlite3
import sys
import traceback

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXT = os.path.join(REPO, "core", "dist", "crsqlite")

CHANGE_COLS = 10  # tbl, pk, cid, val, col_version, db_version, site_id, cl, seq, ts


# metadata-write-version integer values (config.rs)
WRITE_V1 = 1
WRITE_V2_AND_V1 = 2
WRITE_V2 = 3
USE_V1, USE_V2 = 1, 2
LOG_V1, LOG_V2 = 1, 2


def connect(path=":memory:", write=WRITE_V2, use=USE_V2, log=LOG_V2, ts="1700000000"):
    """Open a db with the crsqlite extension loaded and v2 config applied.

    write: 1 = v1, 2 = v2&v1 (dual write), 3 = v2 only. use/log: 1 = v1, 2 = v2.
    Pass None for any of them to leave the default in place.
    Config is set BEFORE any table is registered as a crr (direct 1->3 requires
    that no CRR tables exist yet).
    """
    if not hasattr(sqlite3.Connection, "enable_load_extension"):
        sys.exit("this python build cannot load sqlite extensions; use /opt/homebrew/bin/python3")
    c = sqlite3.connect(path)
    c.enable_load_extension(True)
    c.load_extension(EXT)
    c.isolation_level = None
    if write is not None:
        c.execute("SELECT crsql_config_set('metadata-write-version', ?)", (write,))
    if use is not None:
        c.execute("SELECT crsql_config_set('metadata-use-version', ?)", (use,))
    if log is not None:
        c.execute("SELECT crsql_config_set('sync-log-version', ?)", (log,))
    if ts is not None:
        # default-ts survives across statements; crsql_set_ts() is per-transaction
        c.execute("SELECT crsql_config_set('default-ts', ?)", (int(ts),))
        c.execute("SELECT crsql_set_ts(?)", (str(ts),))
    return c


def close(c):
    try:
        c.execute("SELECT crsql_finalize()")
    except Exception:
        pass
    c.close()


def changes(c, since=-1, site=None):
    if site is None:
        return c.execute(
            "SELECT * FROM crsql_changes WHERE db_version > ?", (since,)).fetchall()
    return c.execute(
        "SELECT * FROM crsql_changes WHERE db_version > ? AND site_id = ?",
        (since, site)).fetchall()


def sync(src, dst, since=-1, ts="1700000000"):
    """Apply all of src's changes since `since` into dst. Returns rows applied.

    NOTE: `since` filters on the db_version column, which is the ORIGIN site's
    version, not a global sequence. A single global watermark is therefore only
    correct for one-shot one-directional syncs. For multi-node / bidirectional
    tests use sync_all()/converge(), which re-send everything (merges are
    idempotent) instead of using a bogus watermark.
    """
    rows = changes(src, since)
    for r in rows:
        if ts is not None:
            dst.execute("SELECT crsql_set_ts(?)", (str(ts),))
        dst.execute(
            "INSERT INTO crsql_changes VALUES (%s)" % ",".join("?" * len(r)), r)
    return rows


def sync_all(src, dst, ts="1700000000"):
    """Push every change src has into dst. Idempotent; safe to repeat."""
    return sync(src, dst, since=-1, ts=ts)


def converge(nodes, max_rounds=20, ts="1700000000"):
    """Run all-pairs sync until every node's user tables stop changing.

    Returns (rounds_used, converged: bool).
    """
    def snap(c):
        names = [r[0] for r in c.execute(
            "SELECT name FROM sqlite_master WHERE type='table' "
            "AND name NOT LIKE 'crsql%' AND name NOT LIKE '%__crsql%' "
            "AND name NOT LIKE 'sqlite_%' ORDER BY name").fetchall()]
        return {n: sorted(c.execute('SELECT * FROM "%s"' % n).fetchall()) for n in names}

    prev = None
    for r in range(max_rounds):
        for a in nodes:
            for b in nodes:
                if a is not b:
                    sync_all(a, b, ts=ts)
        cur = [snap(c) for c in nodes]
        if cur == prev:
            return r + 1, all(s == cur[0] for s in cur)
        prev = cur
    return max_rounds, all(s == prev[0] for s in prev)


def dump(c, sql, params=(), label=None):
    if label:
        print("  -- %s" % label)
    try:
        rows = c.execute(sql, params).fetchall()
    except Exception as e:
        print("     <error: %s>" % e)
        return []
    for r in rows:
        print("     %r" % (r,))
    if not rows:
        print("     <no rows>")
    return rows


def tables(c, base):
    return [r[0] for r in c.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE ?",
        (base + "%",)).fetchall()]


def schema(c, name):
    r = c.execute("SELECT sql FROM sqlite_master WHERE name = ?", (name,)).fetchone()
    return r[0] if r else None


def section(title):
    print("\n=== %s ===" % title)


_state = {"failed": False}


def fail(msg):
    _state["failed"] = True
    print("  [BUG REPRODUCED] %s" % msg)


def ok(msg):
    print("  [ok] %s" % msg)


def run(main):
    """Entrypoint wrapper. Exits 1 when the bug reproduced, 0 when it did not."""
    try:
        main()
    except Exception:
        traceback.print_exc()
        print("\nRESULT: BUG REPRODUCED (uncaught exception above)")
        sys.exit(1)
    if _state["failed"]:
        print("\nRESULT: BUG REPRODUCED")
        sys.exit(1)
    print("\nRESULT: not reproduced (behaviour looks correct)")
    sys.exit(0)
