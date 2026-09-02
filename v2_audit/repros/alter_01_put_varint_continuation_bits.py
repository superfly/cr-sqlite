"""
put_varint() sets SQLite varint continuation bits on the WRONG bytes.

pack_columns.rs:176-202 `put_varint`:
    for i in 1..n { bytes[i - 1] |= 0x80; }   // sets cont bit on bytes[0..n-2]
    for i in (0..n).rev() { buf.put_u8(bytes[i]) }  // emits bytes[n-1] FIRST

bytes[0] is the LOW 7-bit group and is emitted LAST; bytes[n-1] is the HIGH
group and is emitted FIRST.  A SQLite varint must set the continuation bit on
every byte EXCEPT the last one emitted.  The code sets it on every byte except
the FIRST one emitted -- exactly backwards.

Consequence: every value >= 128 is encoded so that the decoder (get_varint,
which is a correct SQLite varint reader) stops on the very first byte and
returns only the high 7-bit group.

EXPECTED: 130 == put/get round trip; crsql_pack_columns(130 args) unpacks to
          130 cells; col_version 200 replicates to a peer as 200.
ACTUAL:   130 encodes as 01 82 and decodes as 1; 130-arg pack unpacks to 1
          cell; a cell with col_version=200 arrives at the peer with
          col_version=1, so a peer write with col_version=2 beats 200 updates
          -> silent, permanent divergence on the v2 wire.
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, sync, dump, section, fail, ok, run


def main():
    section("A. crsql_pack_columns / crsql_unpack_columns with 130 columns")
    c = connect()
    n = 130
    args = ",".join(str(i) for i in range(n))
    blob = c.execute("SELECT crsql_pack_columns(%s)" % args).fetchone()[0]
    print("     header bytes: %s (correct SQLite varint for 130 is 81 02)" % blob[:2].hex())
    cnt = c.execute(
        "SELECT count(*) FROM crsql_unpack_columns WHERE package = ?", (blob,)
    ).fetchone()[0]
    print("     unpacked cell count: %d (expected %d)" % (cnt, n))
    if cnt != n:
        fail("pack/unpack of %d columns round-trips to %d cells" % (n, cnt))
    else:
        ok("130-column pack round-trips")

    section("B. col_version >= 128 on the v2 packed wire")
    a = connect()
    b = connect()
    for x in (a, b):
        x.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v INTEGER)")
        x.execute("SELECT crsql_as_crr('t')")
    a.execute("INSERT INTO t VALUES (1, 0)")
    for i in range(1, 200):
        a.execute("UPDATE t SET v = ? WHERE id = 1", (i,))
    src_cv = a.execute(
        'SELECT col_version FROM "t__crsql_v2_clock"').fetchone()[0]
    row = a.execute("SELECT * FROM crsql_changes").fetchall()[0]
    print("     A local col_version = %d" % src_cv)
    print("     wire col_vrsn blob  = %r  (correct varint for 200 is 81 48)" % (row[4],))
    sync(a, b)
    dst_cv = b.execute(
        'SELECT col_version FROM "t__crsql_v2_clock"').fetchone()[0]
    print("     B col_version after merge = %d (expected %d)" % (dst_cv, src_cv))
    if dst_cv != src_cv:
        fail("col_version %d replicated as %d" % (src_cv, dst_cv))
    else:
        ok("col_version replicated intact")

    section("C. divergence: a single peer write now beats 199 updates")
    # B writes twice locally -> col_version 2 there (it thinks A's was 1)
    b.execute("UPDATE t SET v = 999 WHERE id = 1")
    sync(b, a)
    av = a.execute("SELECT v FROM t").fetchone()[0]
    bv = b.execute("SELECT v FROM t").fetchone()[0]
    dump(a, 'SELECT * FROM "t__crsql_v2_clock"', label="A clock after merging B")
    print("     A.v=%r  B.v=%r" % (av, bv))
    if av != bv:
        fail("nodes diverged: A.v=%r != B.v=%r" % (av, bv))
    else:
        ok("converged (values equal) -- but note B's single write overwrote "
           "A's 199-version cell: %r" % (av,))


run(main)
