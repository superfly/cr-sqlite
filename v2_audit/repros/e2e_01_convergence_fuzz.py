"""Randomized N-node convergence fuzz across table shapes and metadata modes.

Property under test (the core CRDT guarantee): once every node has exchanged
all of its changes with every other node and the cluster is quiescent, all
nodes must hold identical base-table contents.

Covers, per trial: plain rowid table, INTEGER PRIMARY KEY, TEXT PK, composite
PK, WITHOUT ROWID, and a PK-only table. Ops are insert / update / delete /
resurrect, with random pairwise syncs interleaved so deletes race with writes.

Modes exercised (a cluster is homogeneous per trial here; mixed-mode clusters
are covered by e2e_02):
  v1     - metadata-write/use/sync-log all v1  (control/baseline)
  v2&v1  - dual write, v2 read + v2 wire
  v2     - v2 only

Exits 1 on the first non-converging seed.
"""
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, converge, sync_all, fail, ok, run, section

SCHEMAS = [
    ("rowid_tbl", "CREATE TABLE rowid_tbl (id TEXT PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("ipk", "CREATE TABLE ipk (id INTEGER PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("comp", "CREATE TABLE comp (k1 TEXT NOT NULL, k2 INTEGER NOT NULL, a, b, PRIMARY KEY (k1, k2))", ["k1", "k2"], ["a", "b"]),
    ("worid", "CREATE TABLE worid (id TEXT NOT NULL PRIMARY KEY, a, b) WITHOUT ROWID", ["id"], ["a", "b"]),
    ("pkonly", "CREATE TABLE pkonly (id INTEGER PRIMARY KEY NOT NULL)", ["id"], []),
]

MODES = {
    "v1": dict(write=1, use=1, log=1),
    "v2&v1": dict(write=2, use=2, log=2),
    "v2": dict(write=3, use=2, log=2),
}


def mk(mode):
    c = connect(**MODES[mode])
    for name, ddl, _, _ in SCHEMAS:
        c.execute(ddl)
    for name, _, _, _ in SCHEMAS:
        c.execute("SELECT crsql_as_crr(?)", (name,))
    return c


def pk_vals(pkcols, n):
    out = []
    for i, col in enumerate(pkcols):
        out.append(n if col in ("k2", "id") and col != "k1" else "g%d" % n)
    if pkcols == ["id"] and "TEXT" not in "":
        pass
    return out


def make_pk(name, n):
    if name in ("rowid_tbl", "worid"):
        return ["k%d" % n]
    if name in ("ipk", "pkonly"):
        return [n]
    return ["g%d" % (n % 2), n]


def apply_op(c, rnd, spec):
    name, _, pkcols, cols = spec
    n = rnd.randrange(5)
    pk = make_pk(name, n)
    where = " AND ".join('"%s" = ?' % p for p in pkcols)
    op = rnd.choice(["ins", "ins", "upd", "upd", "del"] if cols else ["ins", "ins", "del"])
    if op == "del":
        c.execute('DELETE FROM "%s" WHERE %s' % (name, where), pk)
    elif op == "ins":
        vals = [rnd.randrange(1000) for _ in cols]
        allc = pkcols + cols
        try:
            c.execute('INSERT INTO "%s" (%s) VALUES (%s)'
                      % (name, ",".join('"%s"' % x for x in allc), ",".join("?" * len(allc))),
                      tuple(pk) + tuple(vals))
        except Exception:
            pass  # already present
    else:
        col = rnd.choice(cols)
        c.execute('UPDATE "%s" SET "%s" = ? WHERE %s' % (name, col, where),
                  (rnd.randrange(1000),) + tuple(pk))


def snapshot(c):
    return {n: sorted(c.execute('SELECT * FROM "%s"' % n).fetchall()) for n, _, _, _ in SCHEMAS}


def trial(seed, mode, nodes_n=3, ops=80):
    rnd = random.Random(seed)
    nodes = [mk(mode) for _ in range(nodes_n)]
    for _ in range(ops):
        apply_op(nodes[rnd.randrange(nodes_n)], rnd, rnd.choice(SCHEMAS))
        if rnd.random() < 0.35:
            a, b = rnd.sample(range(nodes_n), 2)
            sync_all(nodes[a], nodes[b])
    _, conv = converge(nodes)
    snaps = [snapshot(c) for c in nodes]
    for c in nodes:
        c.close()
    return conv, snaps


def main():
    for mode in ("v1", "v2&v1", "v2"):
        section("mode = %s" % mode)
        bad = []
        for seed in range(25):
            conv, snaps = trial(seed, mode)
            if not conv:
                bad.append(seed)
                for name, _, _, _ in SCHEMAS:
                    base = snaps[0][name]
                    for i in range(1, len(snaps)):
                        if snaps[i][name] != base:
                            sa, sb = set(map(tuple, base)), set(map(tuple, snaps[i][name]))
                            print("  seed %d table %s: only-node0=%r only-node%d=%r"
                                  % (seed, name, sorted(sa - sb)[:4], i, sorted(sb - sa)[:4]))
                break
        if bad:
            fail("mode %s did not converge (seeds %r)" % (mode, bad))
        else:
            ok("mode %s: 25 seeds converged" % mode)


run(main)
