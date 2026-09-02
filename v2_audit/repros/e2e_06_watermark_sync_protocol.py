"""Convergence under the REAL sync protocol: per-origin-site watermarks.

e2e_01 re-sends every change on every round, which hides any bug where a change
is emitted under the wrong (site_id, db_version) or is skipped by a watermark
query. Production (corrosion) instead keeps, per peer site, the highest
db_version it has seen from that site, and pulls:

    SELECT * FROM crsql_changes WHERE site_id = ? AND db_version > ?

Every change must be reachable exactly once through that query, under its true
origin site_id, with a db_version that never goes backwards for that site. If a
change is emitted under a db_version at or below a watermark the receiver has
already passed, it is lost forever.

Property: 3 nodes, random ops, watermark-driven all-pairs sync to quiescence ->
identical base tables.
"""
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, fail, ok, run, section

SCHEMAS = [
    ("t_txt", "CREATE TABLE t_txt (id TEXT PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_ipk", "CREATE TABLE t_ipk (id INTEGER PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_comp", "CREATE TABLE t_comp (k1 TEXT NOT NULL, k2 INTEGER NOT NULL, a, PRIMARY KEY (k1, k2))", ["k1", "k2"], ["a"]),
    ("t_pkonly", "CREATE TABLE t_pkonly (id INTEGER PRIMARY KEY NOT NULL)", ["id"], []),
]

MODES = {"v1": dict(write=1, use=1, log=1),
         "v2&v1": dict(write=2, use=2, log=2),
         "v2": dict(write=3, use=2, log=2)}


class Node(object):
    def __init__(self, mode):
        self.db = connect(**MODES[mode])
        for _, ddl, _, _ in SCHEMAS:
            self.db.execute(ddl)
        for n, _, _, _ in SCHEMAS:
            self.db.execute("SELECT crsql_as_crr(?)", (n,))
        self.site = self.db.execute("SELECT crsql_site_id()").fetchone()[0]
        self.marks = {}          # origin site_id blob -> highest db_version applied

    def known_sites(self):
        # every origin site this node can emit changes for
        rows = self.db.execute(
            "SELECT DISTINCT site_id FROM crsql_changes").fetchall()
        return [r[0] for r in rows]

    def pull_from(self, other):
        """Apply everything `other` holds that this node has not seen, per origin site."""
        applied = 0
        for site in other.known_sites():
            key = bytes(site) if site is not None else None
            since = self.marks.get(key, -1)
            rows = other.db.execute(
                "SELECT * FROM crsql_changes WHERE site_id = ? AND db_version > ? "
                "ORDER BY db_version, seq", (site, since)).fetchall()
            hi = since
            for r in rows:
                self.db.execute("SELECT crsql_set_ts('1700000000')")
                self.db.execute("INSERT INTO crsql_changes VALUES (%s)"
                                % ",".join("?" * len(r)), r)
                if r[5] is not None and r[5] > hi:
                    hi = r[5]
                applied += 1
            self.marks[key] = hi
        return applied


def make_pk(name, n):
    if name == "t_txt":
        return ["k%d" % n]
    if name in ("t_ipk", "t_pkonly"):
        return [n]
    return ["g%d" % (n % 2), n]


def apply_op(node, rnd, spec):
    name, _, pkcols, cols = spec
    pk = make_pk(name, rnd.randrange(5))
    where = " AND ".join('"%s" = ?' % p for p in pkcols)
    op = rnd.choice(["ins", "ins", "upd", "upd", "del"] if cols else ["ins", "del"])
    if op == "del":
        node.db.execute('DELETE FROM "%s" WHERE %s' % (name, where), pk)
    elif op == "ins":
        allc = pkcols + cols
        try:
            node.db.execute('INSERT INTO "%s" (%s) VALUES (%s)'
                            % (name, ",".join('"%s"' % x for x in allc), ",".join("?" * len(allc))),
                            tuple(pk) + tuple(rnd.randrange(1000) for _ in cols))
        except Exception:
            pass
    else:
        node.db.execute('UPDATE "%s" SET "%s" = ? WHERE %s' % (name, rnd.choice(cols), where),
                        (rnd.randrange(1000),) + tuple(pk))


def snapshot(node):
    return {n: sorted(node.db.execute('SELECT * FROM "%s"' % n).fetchall())
            for n, _, _, _ in SCHEMAS}


def trial(seed, mode, nodes_n=3, ops=80):
    rnd = random.Random(seed)
    nodes = [Node(mode) for _ in range(nodes_n)]
    for _ in range(ops):
        apply_op(nodes[rnd.randrange(nodes_n)], rnd, rnd.choice(SCHEMAS))
        if rnd.random() < 0.35:
            a, b = rnd.sample(range(nodes_n), 2)
            nodes[b].pull_from(nodes[a])
    for _ in range(nodes_n * 3):
        moved = 0
        for a in range(nodes_n):
            for b in range(nodes_n):
                if a != b:
                    moved += nodes[b].pull_from(nodes[a])
        if moved == 0:
            break
    snaps = [snapshot(n) for n in nodes]
    conv = all(s == snaps[0] for s in snaps)
    for n in nodes:
        n.db.close()
    return conv, snaps


def main():
    for mode in ("v1", "v2&v1", "v2"):
        section("watermark sync, mode = %s" % mode)
        bad = []
        for seed in range(20):
            conv, snaps = trial(seed, mode)
            if not conv:
                bad.append(seed)
                for name, _, _, _ in SCHEMAS:
                    base = snaps[0][name]
                    for i in range(1, len(snaps)):
                        if snaps[i][name] != base:
                            sa, sb = set(map(tuple, base)), set(map(tuple, snaps[i][name]))
                            print("  seed %d %s: only-node0=%r only-node%d=%r"
                                  % (seed, name, sorted(sa - sb)[:4], i, sorted(sb - sa)[:4]))
                break
        if bad:
            fail("mode %s lost changes under watermark-driven sync (seeds %r)" % (mode, bad))
        else:
            ok("mode %s: 20 seeds converged under per-site watermarks" % mode)


run(main)
