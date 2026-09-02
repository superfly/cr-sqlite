"""feed_02: crsql_changes is NOT ordered by (db_version, seq) in V2 packed mode.

Design: "Feed Query (Packed, Per Table)" ends the UNION ALL with
    ORDER BY db_vrsn, seq
and changes_vtab.rs:changes_best_index appends the same default ordering
("ORDER BY db_vrsn, seq ASC", changes_vtab.rs:128).

EXPECTED: within one db_version, feed rows come out in ascending seq order,
          interleaving tombstones and alive-row groups by their seq.

ACTUAL:   the ordering is applied to the outer projection where, for alive rows,
          `seq` is `crsql_pack_varint_agg(c.seq ORDER BY cm.col_id)` -- a BLOB
          (changes_vtab_read.rs:254) -- while tombstone rows keep an INTEGER seq
          (changes_vtab_read.rs:30 / :291).  SQLite sorts INTEGER before BLOB, so:
            * every tombstone in a db_version sorts before every alive-row group,
              regardless of its actual seq; and
            * alive-row groups sort by the raw varint bytes, whose first byte is
              the *column count*, so a 1-column group always sorts before a
              3-column group no matter what their seqs are.

CONSEQUENCE: any consumer that streams `SELECT * FROM crsql_changes ORDER BY
          db_version, seq` (corrosion does, and it is the vtab's own default
          ordering) sees a transaction's events in an arbitrary order.  A delete
          emitted at seq 4 is handed to the receiver before the updates at seq
          0..3 that precede it; combined with `LIMIT`, the first N rows returned
          are not the N lowest-seq changes, so resumable/chunked feed readers
          skip changes.  orderByConsumed is set to 1, so SQLite trusts the
          vtab's claim and does not re-sort.
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run


def main():
    c = connect()
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT, cc TEXT)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1,'a1','b1','c1')")
    c.execute("INSERT INTO t VALUES (2,'a2','b2','c2')")
    c.execute("INSERT INTO t VALUES (3,'a3','b3','c3')")

    # one transaction -> one db_version, seqs 0..4
    c.execute("BEGIN")
    c.execute("UPDATE t SET a='X', b='X', cc='X' WHERE id=2")   # seqs 0,1,2
    c.execute("UPDATE t SET a='Y' WHERE id=3")                  # seq 3
    c.execute("DELETE FROM t WHERE id=1")                       # seq 4
    c.execute("COMMIT")

    section("ground truth for db_version 4")
    dump(c, "SELECT cell_key>>12 AS key, cell_key&4095 AS col_id, seq "
            "FROM t__crsql_v2_clock WHERE db_version=4 ORDER BY seq", label="clock (alive)")
    dump(c, "SELECT db_version, seq, cl FROM t__crsql_v2_tombstones", label="tombstone")
    print("     => correct emission order for db_version 4 is:")
    print("        key=2 group (seqs 0,1,2), key=3 group (seq 3), tombstone key=1 (seq 4)")

    section("actual feed order (vtab default ORDER BY db_vrsn, seq)")
    rows = dump(c, "SELECT quote(pk), quote(cid), db_version, quote(seq), typeof(seq) "
                   "FROM crsql_changes WHERE db_version > 3")

    section("explicit ORDER BY db_version, seq")
    rows2 = dump(c, "SELECT quote(cid), quote(seq) FROM crsql_changes "
                    "WHERE db_version > 3 ORDER BY db_version, seq")

    # min seq actually carried by each emitted row, in emission order
    def min_seq(row_seq_quoted, cid_quoted):
        # tombstone rows carry a plain integer seq; packed rows carry X'<count><seqs...>'
        if row_seq_quoted.startswith("X'"):
            b = bytes.fromhex(row_seq_quoted[2:-1])
            return min(b[1:])          # skip the varint count header
        return int(row_seq_quoted)

    order = [min_seq(r[3], r[1]) for r in rows]
    print()
    print("  first seq of each emitted row, in emission order: %r" % (order,))
    print("  expected (ascending):                             %r" % (sorted(order),))

    if order != sorted(order):
        fail("feed rows are not ordered by seq within a db_version: got %r, expected %r"
             % (order, sorted(order)))
    else:
        ok("feed ordering is correct")


run(main)
