"""feed_05: a single-column packed group is emitted with NO char(0) in `cid`,
breaking the documented V1/V2 packed-row detection contract.

Design: "V1/V2 Coexistence" and the "Packed Wire Row" table both define detection
as a property of `cid`:
    "Packed (v2): `cid` contains `char(0)` separator -> split by `char(0)` ...
     Single (v1): `cid` is a plain column name or sentinel (no `char(0)`)."
    "`cid` | `GROUP_CONCAT(col_name, char(0))` ... Detection: `char(0)` present
     -> packed; absent -> single/sentinel."

EXPECTED: every packed row emitted in V2 wire mode is recognisable as packed by
          the documented rule.

ACTUAL:   changes_vtab_read.rs:249 emits
              cast(group_concat(cm.col_name, char(0) ORDER BY cm.col_id) as blob)
          group_concat of a ONE-element group produces just the column name with
          no separator, so a single-column change (the most common change shape:
          one column updated) is emitted with `cid` = b"a" -- no char(0) -- while
          `col_vrsn`, `seq` and `val` are still packed binary arrays.

          cr-sqlite's own receiver happens to survive because it ignores the
          documented rule and sniffs the *type* of col_vrsn instead
          (changes_vtab_write.rs:559-565).  Any other implementation of the wire
          format -- and the design doc is the only spec such an implementation
          has -- classifies the row as a V1 single change and then reads
          col_vrsn (a varint-array BLOB) as a column version and val (a
          crsql_pack_agg TLV BLOB) as the raw column value.

CONSEQUENCE: cross-implementation interop is silently wrong rather than loudly
          broken: a conforming receiver writes the TLV blob itself into the user
          column and a nonsense integer/blob into col_version.  Either the doc's
          detection rule or the emission (e.g. always append a trailing char(0),
          or make detection type-based in the spec) has to change; today they
          disagree.
"""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run


def main():
    c = connect()
    c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("INSERT INTO t VALUES (1,'a1','b1')")   # 2-column group
    c.execute("UPDATE t SET a='a2' WHERE id=1")       # 1-column group

    section("feed rows (typeof of each packed field)")
    dump(c, "SELECT quote(cid), typeof(cid), quote(col_version), typeof(col_version), "
            "quote(val), typeof(val), quote(seq), db_version FROM crsql_changes")

    rows = c.execute("SELECT cid, col_version, val, db_version FROM crsql_changes "
                     "ORDER BY db_version").fetchall()

    bad = []
    for cid, cvrsn, val, dbv in rows:
        cid_b = cid if isinstance(cid, bytes) else str(cid).encode()
        has_nul = b"\x00" in cid_b
        packed_by_type = isinstance(cvrsn, (bytes, bytearray))
        print("     db_version=%d cid=%r char(0)?=%s col_vrsn is blob?=%s val=%r"
              % (dbv, cid_b, has_nul, packed_by_type, val))
        if packed_by_type and not has_nul:
            bad.append((dbv, cid_b, cvrsn, val))

    section("interpretation by the documented rule ('char(0) in cid -> packed')")
    for dbv, cid_b, cvrsn, val in bad:
        print("     db_version=%d -> classified SINGLE (v1) change:" % dbv)
        print("        col_name    = %r" % cid_b.decode())
        print("        col_version = %r   <- actually a crsql_pack_varint_agg array" % (cvrsn,))
        print("        value       = %r   <- actually a crsql_pack_agg TLV blob" % (val,))

    if bad:
        fail("%d packed row(s) carry no char(0) in cid yet pack col_vrsn/seq/val; "
             "a receiver following the documented detection rule misparses them"
             % len(bad))
    else:
        ok("every packed row is detectable via char(0)")


run(main)
