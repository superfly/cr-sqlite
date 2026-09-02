"""A V2 delete for a row the receiver never saw is recorded hash-only, and is
then SILENTLY DROPPED from that node's V1-compat feed forever.

Mechanism
---------
1. Node A inserts a row and deletes it before ever syncing. Its feed contains
   only the V2 tombstone event (cid='-2', pk = hashed_pk; the insert's clock
   rows were deleted by the delete).
2. Node C receives that '-2' event. It has never seen the row, so it cannot
   resolve hash -> real PK. Per the design ("Codepath Separation", V2->V1
   translation: "If the hash is unknown and it's a delete (cid='-2') it can be
   ignored ... The tombstone is still recorded in v2_tombstones") C records the
   tombstone with the hash only, and v2_tombstone_pks gets NO row.
3. C is now in the documented rollout window between step 3 (use=v2) and
   step 4 (log=v2) of "Recommended Rollout Sequence", so it emits the V1-compat
   dead-row feed, which is:

       FROM v2_tombstones AS d
       JOIN v2_tombstone_pks AS tp ON d.hashed_pk = tp.hashed_pk

   an INNER JOIN. The tombstone has no tp row, so the delete vanishes from C's
   feed. No error, no warning, no row.

Consequence
-----------
C never forwards that delete to any V1-wire peer. In a gossip topology where C
is the only path between A and a V1 peer, the peer keeps the row alive forever
while A and C have it deleted — permanent, silent divergence. The same gap also
means a later `metadata-use-version` rollback or any V1-compat resync from C is
lossy. Delete-before-first-sync and delete-arriving-before-insert are both
ordinary events in a real cluster, not corner cases.

This is also a DESIGN GAP: the design acknowledges the hash-only tombstone but
its V1-compat feed query has no fallback for a tombstone with no PK mapping.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, sync_all, dump, fail, ok, run, section

DDL = "CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, a)"


def mk(**cfg):
    c = connect(**cfg)
    c.execute(DDL)
    c.execute("SELECT crsql_as_crr('t')")
    return c


def main():
    section("node A: insert then delete, never synced in between")
    a = mk(write=2, use=2, log=2)
    a.execute("INSERT INTO t VALUES ('x', 1)")
    a.execute("DELETE FROM t WHERE id = 'x'")
    dump(a, 'SELECT hex(hashed_pk), cl FROM t__crsql_v2_tombstones', (), "A v2_tombstones")
    dump(a, 'SELECT hex(hashed_pk), id FROM t__crsql_v2_tombstone_pks', (), "A v2_tombstone_pks")
    dump(a, 'SELECT "table", cid, hex(pk), cl FROM crsql_changes', (), "A feed (V2 wire)")

    section("node C: rollout window use=v2 / log=v1 (between step 3 and step 4)")
    c = mk(write=2, use=2, log=1)
    sync_all(a, c)
    dump(c, 'SELECT hex(hashed_pk), cl FROM t__crsql_v2_tombstones', (), "C v2_tombstones")
    tp = dump(c, 'SELECT hex(hashed_pk), id FROM t__crsql_v2_tombstone_pks', (), "C v2_tombstone_pks")

    section("C's V1-compat feed — the delete must appear here")
    feed = dump(c, 'SELECT "table", cid, hex(pk), cl, db_version FROM crsql_changes', (),
                "C feed (V1 compat wire)")

    section("downstream V1 node D receives C's feed")
    d = mk(write=1, use=1, log=1)
    d.execute("INSERT INTO t VALUES ('x', 1)")   # D already has the row
    sync_all(c, d)
    rows = dump(d, "SELECT * FROM t", (), "D rows after receiving C's feed")

    if not tp:
        print("\n  -> C holds a tombstone with no PK mapping")
    if not feed:
        print("  -> C's V1-compat feed is EMPTY: the delete was dropped by the "
              "INNER JOIN on v2_tombstone_pks")
    if rows:
        fail("delete never reached the V1 peer: D still has %r while A and C have it deleted"
             % (rows,))
    else:
        ok("delete propagated through the V1-compat feed")

    a.close()
    c.close()
    d.close()


run(main)
