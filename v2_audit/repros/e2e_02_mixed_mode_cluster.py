"""Mixed-metadata cluster on the V1 wire — the actual rollout scenario.

Rollout steps 1-4 of the design leave a cluster where nodes store metadata at
different levels but ALL still emit the V1 wire format (sync-log-version is
only flipped once every peer can accept V2). Every such node must interoperate:

  node0: write=v1,    use=v1, log=v1   (not migrated)
  node1: write=v2&v1, use=v2, log=v1   (migrated metadata, V1 emission from V2 tables)
  node2: write=v2,    use=v2, log=v1   (cut over, V1-compat emission from V2 tables)

This exercises the "Dead rows, V1 compat wire format" feed path (real PK cols
resolved from v2_tombstone_pks) and the V1-wire -> V2-metadata merge translation.

Property: after all-pairs sync to quiescence every node holds identical base
table contents.
"""
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, converge, sync_all, dump, fail, ok, run, section

SCHEMAS = [
    ("t_txt", "CREATE TABLE t_txt (id TEXT PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_ipk", "CREATE TABLE t_ipk (id INTEGER PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_comp", "CREATE TABLE t_comp (k1 TEXT NOT NULL, k2 INTEGER NOT NULL, a, PRIMARY KEY (k1, k2))", ["k1", "k2"], ["a"]),
]

PROFILES = [
    ("v1-only", dict(write=1, use=1, log=1)),
    ("v2&v1/logv1", dict(write=2, use=2, log=1)),
    ("v2-only/logv1", dict(write=3, use=2, log=1)),
]


def mk(cfg):
    c = connect(**cfg)
    for name, ddl, _, _ in SCHEMAS:
        c.execute(ddl)
    for name, _, _, _ in SCHEMAS:
        c.execute("SELECT crsql_as_crr(?)", (name,))
    return c


def make_pk(name, n):
    if name == "t_txt":
        return ["k%d" % n]
    if name == "t_ipk":
        return [n]
    return ["g%d" % (n % 2), n]


def apply_op(c, rnd, spec):
    name, _, pkcols, cols = spec
    pk = make_pk(name, rnd.randrange(5))
    where = " AND ".join('"%s" = ?' % p for p in pkcols)
    op = rnd.choice(["ins", "ins", "upd", "upd", "del"])
    if op == "del":
        c.execute('DELETE FROM "%s" WHERE %s' % (name, where), pk)
    elif op == "ins":
        allc = pkcols + cols
        try:
            c.execute('INSERT INTO "%s" (%s) VALUES (%s)'
                      % (name, ",".join('"%s"' % x for x in allc), ",".join("?" * len(allc))),
                      tuple(pk) + tuple(rnd.randrange(1000) for _ in cols))
        except Exception:
            pass
    else:
        c.execute('UPDATE "%s" SET "%s" = ? WHERE %s' % (name, rnd.choice(cols), where),
                  (rnd.randrange(1000),) + tuple(pk))


def snapshot(c):
    return {n: sorted(c.execute('SELECT * FROM "%s"' % n).fetchall()) for n, _, _, _ in SCHEMAS}


def trial(seed, ops=80):
    rnd = random.Random(seed)
    nodes = [mk(cfg) for _, cfg in PROFILES]
    for _ in range(ops):
        apply_op(nodes[rnd.randrange(len(nodes))], rnd, rnd.choice(SCHEMAS))
        if rnd.random() < 0.35:
            a, b = rnd.sample(range(len(nodes)), 2)
            sync_all(nodes[a], nodes[b])
    _, conv = converge(nodes)
    snaps = [snapshot(c) for c in nodes]
    for c in nodes:
        c.close()
    return conv, snaps


def main():
    section("mixed-mode cluster: %s" % ", ".join(n for n, _ in PROFILES))
    bad = []
    for seed in range(25):
        try:
            conv, snaps = trial(seed)
        except Exception as e:
            fail("seed %d raised during sync: %s" % (seed, e))
            raise
        if not conv:
            bad.append(seed)
            for name, _, _, _ in SCHEMAS:
                base = snaps[0][name]
                for i in range(1, len(snaps)):
                    if snaps[i][name] != base:
                        sa, sb = set(map(tuple, base)), set(map(tuple, snaps[i][name]))
                        print("  seed %d %s: only-%s=%r  only-%s=%r"
                              % (seed, name, PROFILES[0][0], sorted(sa - sb)[:4],
                                 PROFILES[i][0], sorted(sb - sa)[:4]))
            break
    if bad:
        fail("mixed-mode cluster did not converge (seeds %r)" % bad)
    else:
        ok("25 seeds converged across all three config profiles")


run(main)
