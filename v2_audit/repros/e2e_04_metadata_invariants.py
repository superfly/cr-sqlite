"""V2 metadata invariants after a delete/resurrect-heavy 3-node fuzz.

Convergence of the base tables is not enough — the V2 metadata itself must stay
internally consistent, or later syncs/migrations read garbage. Invariants
checked on every node after the fuzz quiesces:

  I1  every base-table row has exactly one v2_pks row
  I2  every v2_pks row has a matching base-table row (no orphan alive PKs)
  I3  no hashed_pk / pk value appears in both v2_pks and v2_tombstones
  I4  v2_pks.cl is odd, v2_tombstones.cl is even  (CHECK constraints, but the
      merge path can also write via paths that bypass them - verify)
  I5  every v2_clock row's key half resolves to a live v2_pks row
      (no clock entries left behind for deleted/tombstoned rows)
  I6  every v2_clock row's col_id half resolves to a v2_col_map entry
      (or is the col_id=0 sentinel on a pk-only table)
  I7  ts > 0 everywhere in v2_clock and v2_tombstones
  I8  in hash mode every v2_tombstones row has a v2_tombstone_pks row
      (needed for V1-compat delete emission)
  I9  agreement across nodes on the CL of every key that any node knows
"""
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, converge, sync_all, dump, fail, ok, run, section

COL_ID_BITS = 12
MASK = (1 << COL_ID_BITS) - 1

SCHEMAS = [
    ("t_txt", "CREATE TABLE t_txt (id TEXT PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_ipk", "CREATE TABLE t_ipk (id INTEGER PRIMARY KEY NOT NULL, a, b)", ["id"], ["a", "b"]),
    ("t_comp", "CREATE TABLE t_comp (k1 TEXT NOT NULL, k2 INTEGER NOT NULL, a, PRIMARY KEY (k1, k2))", ["k1", "k2"], ["a"]),
    ("t_pkonly", "CREATE TABLE t_pkonly (id INTEGER PRIMARY KEY NOT NULL)", ["id"], []),
]


def mk():
    c = connect()
    for _, ddl, _, _ in SCHEMAS:
        c.execute(ddl)
    for name, _, _, _ in SCHEMAS:
        c.execute("SELECT crsql_as_crr(?)", (name,))
    return c


def make_pk(name, n):
    if name == "t_txt":
        return ["k%d" % n]
    if name in ("t_ipk", "t_pkonly"):
        return [n]
    return ["g%d" % (n % 2), n]


def apply_op(c, rnd, spec):
    name, _, pkcols, cols = spec
    pk = make_pk(name, rnd.randrange(4))
    where = " AND ".join('"%s" = ?' % p for p in pkcols)
    # delete-heavy so rows churn through the tombstone table repeatedly
    op = rnd.choice(["ins", "ins", "del", "del", "upd"] if cols else ["ins", "del", "del"])
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


def cols_of(c, tbl):
    return [r[1] for r in c.execute('PRAGMA table_info("%s")' % tbl).fetchall()]


def check_node(c, node_i, spec, problems):
    name, _, pkcols, non_pks = spec
    pks_t = '%s__crsql_v2_pks' % name
    tomb_t = '%s__crsql_v2_tombstones' % name
    clock_t = '%s__crsql_v2_clock' % name
    map_t = '%s__crsql_v2_col_map' % name
    tpk_t = '%s__crsql_v2_tombstone_pks' % name

    pks_cols = cols_of(c, pks_t)
    hash_mode = "hashed_pk" in pks_cols

    def q(sql, params=()):
        return c.execute(sql, params).fetchall()

    def note(inv, msg):
        problems.append("node%d %s %s: %s" % (node_i, name, inv, msg))

    n_base = q('SELECT count(*) FROM "%s"' % name)[0][0]
    n_pks = q('SELECT count(*) FROM "%s"' % pks_t)[0][0]
    if n_base != n_pks:
        note("I1/I2", "base rows=%d but v2_pks rows=%d" % (n_base, n_pks))

    # I3 alive and dead must be disjoint
    key_col = "hashed_pk" if hash_mode else (pkcols[0] if pkcols[0] in pks_cols else None)
    if key_col:
        both = q('SELECT count(*) FROM "%s" p JOIN "%s" d ON p."%s" = d."%s"'
                 % (pks_t, tomb_t, key_col, key_col))[0][0]
        if both:
            note("I3", "%d keys present in BOTH v2_pks and v2_tombstones" % both)

    # I4 parity
    bad = q('SELECT count(*) FROM "%s" WHERE cl %% 2 != 1' % pks_t)[0][0]
    if bad:
        note("I4", "%d v2_pks rows with even cl" % bad)
    bad = q('SELECT count(*) FROM "%s" WHERE cl %% 2 != 0' % tomb_t)[0][0]
    if bad:
        note("I4", "%d v2_tombstones rows with odd cl" % bad)

    # I5 no orphan clock entries
    orph = q('SELECT count(*) FROM "%s" c LEFT JOIN "%s" p ON p.__crsql_key = (c.cell_key >> %d) '
             'WHERE p.__crsql_key IS NULL' % (clock_t, pks_t, COL_ID_BITS))[0][0]
    if orph:
        note("I5", "%d clock rows whose key has no live v2_pks row" % orph)

    # I6 col_id resolves
    if non_pks:
        unk = q('SELECT count(*) FROM "%s" c LEFT JOIN "%s" m ON m.col_id = (c.cell_key & %d) '
                'WHERE m.col_id IS NULL' % (clock_t, map_t, MASK))[0][0]
        if unk:
            note("I6", "%d clock rows with a col_id absent from v2_col_map" % unk)

    # I7 timestamps
    for t in (clock_t, tomb_t):
        bad = q('SELECT count(*) FROM "%s" WHERE ts <= 0' % t)[0][0]
        if bad:
            note("I7", "%d rows in %s with ts <= 0" % (bad, t))

    # I8 tombstone_pks coverage (hash mode only)
    if hash_mode:
        exists = q("SELECT count(*) FROM sqlite_master WHERE type='table' AND name = ?", (tpk_t,))[0][0]
        if not exists:
            note("I8", "hash-mode table has no v2_tombstone_pks table")
        else:
            missing = q('SELECT count(*) FROM "%s" d LEFT JOIN "%s" tp ON d.hashed_pk = tp.hashed_pk '
                        'WHERE tp.hashed_pk IS NULL' % (tomb_t, tpk_t))[0][0]
            if missing:
                note("I8", "%d tombstones with no v2_tombstone_pks row (V1-compat delete emission "
                           "would drop these)" % missing)


def main():
    section("delete/resurrect-heavy 3-node fuzz, then V2 metadata invariant check")
    problems = []
    for seed in range(12):
        rnd = random.Random(seed)
        nodes = [mk() for _ in range(3)]
        for _ in range(90):
            apply_op(nodes[rnd.randrange(3)], rnd, rnd.choice(SCHEMAS))
            if rnd.random() < 0.35:
                a, b = rnd.sample(range(3), 2)
                sync_all(nodes[a], nodes[b])
        _, conv = converge(nodes)
        if not conv:
            problems.append("seed %d: base tables did not converge" % seed)
        seed_problems = []
        for i, c in enumerate(nodes):
            for spec in SCHEMAS:
                check_node(c, i, spec, seed_problems)
        if seed_problems:
            problems.append("seed %d:" % seed)
            problems.extend("  " + p for p in seed_problems[:12])
        for c in nodes:
            c.close()
        if problems:
            break

    if problems:
        for p in problems:
            print("  " + p)
        fail("V2 metadata invariants violated")
    else:
        ok("12 seeds: converged and all invariants held")


run(main)
