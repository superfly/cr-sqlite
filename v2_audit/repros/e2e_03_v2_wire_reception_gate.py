"""V2-wire reception is gated on metadata-use-version, not on the documented condition.

Design ("Versioning", Packed Wire Format section):

  "Nodes accept v2 logs regardless of their own sync-log-version -
   sync-log-version controls what a node emits, not what it can receive.
   Reception requires:
     1. metadata-write-version is v2 or v2&v1 (V2 tables are actively written to).
     2. V1->V2 migration is complete."

metadata-use-version is NOT in that list. But the implementation rejects the
change unless metadata-use-version == 2:

  changes_vtab_write.rs: "received V2 wire format change but
  metadata-use-version is not 2"

A node at write=v2&v1 with migration complete and use=v1 is a legitimate state
in the documented rollout (it is exactly the state between step 2 and step 3 of
"Recommended Rollout Sequence"). Per the doc such a node can accept V2 logs; in
practice every V2-wire change it receives errors out. An operator who follows
the doc and flips a peer to sync-log-version=v2 once "all peers can accept V2
format" will hard-fail sync against every peer still at use=v1.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, fail, ok, run, section

DDL = "CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a, b)"


def mk(**cfg):
    c = connect(**cfg)
    c.execute(DDL)
    c.execute("SELECT crsql_as_crr('t')")
    return c


def main():
    section("sender: write=v2&v1, use=v2, log=v2 (emits V2 packed wire)")
    src = mk(write=2, use=2, log=2)
    src.execute("INSERT INTO t VALUES (1, 'x', 'y')")
    rows = dump(src, 'SELECT "table", cid, cl, db_version FROM crsql_changes', (), "V2 wire emitted")

    section("receiver: write=v2&v1 (V2 tables written), use=v1 — per the doc this MUST accept")
    dst = mk(write=2, use=1, log=1)
    print("  receiver config: write=%r use=%r log=%r" % (
        dst.execute("SELECT crsql_config_get('metadata-write-version')").fetchone()[0],
        dst.execute("SELECT crsql_config_get('metadata-use-version')").fetchone()[0],
        dst.execute("SELECT crsql_config_get('sync-log-version')").fetchone()[0]))

    all_rows = src.execute("SELECT * FROM crsql_changes").fetchall()
    err = None
    for r in all_rows:
        try:
            dst.execute("SELECT crsql_set_ts('1700000000')")
            dst.execute("INSERT INTO crsql_changes VALUES (%s)" % ",".join("?" * len(r)), r)
        except Exception as e:
            err = e
            break

    if err is not None:
        print("  receiver raised: %s" % err)
        fail("V2 wire rejected by a node that the design says must accept it "
             "(write=v2&v1, migration complete, use=v1)")
    else:
        dump(dst, "SELECT * FROM t", (), "receiver rows")
        ok("receiver accepted the V2 wire change")

    src.close()
    dst.close()


run(main)
