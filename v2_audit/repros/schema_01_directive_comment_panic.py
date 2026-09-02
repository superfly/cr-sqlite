"""schema_01: `/*/` in a CREATE TABLE statement aborts the process.

crsql_as_crr() reads the `/* crsql: ... */` schema directive by scanning
sqlite_master.sql in schema_directive.rs::parse_directives(). The scanner finds
"/*" at `abs_start`, then searches for "*/" *starting at abs_start* rather than
at abs_start+2:

    let comment_end_rel = create_sql[abs_start..].find("*/")
    let comment_body = &create_sql[abs_start + 2 .. abs_start + comment_end_rel];

For the byte sequence "/*/", `find("*/")` returns 1, so the slice becomes
[abs_start+2 .. abs_start+1] — a reversed range. Rust panics; the extension is
built no_std with panic = "abort", so the whole host process dies (SIGTRAP /
exit 133). "/*/" is a perfectly legal way to open a SQLite comment
(`/*/ text */`), so any schema containing one takes down every process that
calls crsql_as_crr() or crsql_commit_alter() on that table.

Expected: as_crr either succeeds or returns a SQL error.
Actual:   the process is killed by an abort inside the extension.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from harness import connect, dump, section, fail, ok, run

CHILD = "--child"

DDL = "CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL /*/ hi */, v TEXT)"


def child():
    c = connect()
    c.execute(DDL)
    c.execute("SELECT crsql_as_crr('t')")
    print("child: as_crr returned normally")


def main():
    section("the CREATE TABLE statement under test")
    print("     %s" % DDL)

    section("sanity: sqlite itself is fine with the comment")
    c = connect()
    c.execute(DDL)
    dump(c, "SELECT sql FROM sqlite_master WHERE name = 't'")

    section("crsql_as_crr('t') in a child process")
    p = subprocess.run([sys.executable, os.path.abspath(__file__), CHILD],
                       capture_output=True, text=True)
    print("     child stdout: %r" % p.stdout.strip())
    print("     child stderr: %r" % p.stderr.strip()[-300:])
    print("     child exit code: %d" % p.returncode)

    if p.returncode < 0 or p.returncode == 133 or p.returncode > 128:
        fail("crsql_as_crr aborted the process (exit %d) parsing a legal `/*/` "
             "comment -- panic in schema_directive.rs::parse_directives" % p.returncode)
    elif p.returncode != 0:
        fail("crsql_as_crr failed unexpectedly (exit %d)" % p.returncode)
    else:
        ok("as_crr handled the comment without crashing")


if __name__ == "__main__" and CHILD in sys.argv:
    child()
    sys.exit(0)

run(main)
