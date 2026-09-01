#!/usr/bin/env python3
"""Fix untracked rows in Corrosion CRR tables.

Untracked rows are rows that exist in the base table but have no __crsql_pks
entry — cr-sqlite has no metadata for them. This script finds and repairs them
by inserting a pks entry and a sentinel clock entry (col_name='-1',
col_version=1) to mark the row as alive. It does NOT touch the base table data
(no DELETE/INSERT), so it doesn't flood replication with spurious changes.

Tracked-but-wrong rows (rows in __crsql_pks with wrong clock entry counts) are
reported but NOT repaired — they are safe to leave as-is:
  - They don't appear in crsql_changes (invisible to replication)
  - V2 migration handles them correctly (is_alive=1, cl defaults to 1)
  - Future mutations will create proper clock entries
  - Repairing them (DELETE+INSERT) would flood replication with spurious changes

For each table with untracked rows, processes them in batches of 500.
Each batch is its own committed transaction.

Writes go through the Corrosion Postgres wire-protocol API so they
replicate properly to peers. Direct SQLite writes would not replicate.

Uses pg8000 (pure Python PG driver) to avoid C extension segfaults.

    pip install pg8000

The script is interactive and requires confirmation before:
  - Pinging the Corrosion API
  - Starting the scan
  - Repairing each table (shows batch count)

Usage:
  python3 fix_corrosion_untracked_rows.py <config_path>

  config_path: path to corrosion config.toml (e.g., /etc/corrosion2/config.toml)
"""
import sys
import os

try:
    import pg8000
except ImportError:
    print("Error: pg8000 not installed.")
    print("Install: pip install pg8000")
    sys.exit(1)

try:
    import tomllib  # Python 3.11+
except ImportError:
    try:
        import tomli as tomllib  # pip install tomli (backport for < 3.11)
    except ImportError:
        print("Error: no TOML parser available.")
        print("Python 3.11+ has tomllib built-in.")
        print("For older Python: pip install tomli")
        sys.exit(1)


def load_config(config_path):
    """Load corrosion config.toml and extract api.pg.addr and db.path."""
    if not os.path.exists(config_path):
        print(f"Error: config not found: {config_path}")
        sys.exit(1)

    with open(config_path, "rb") as f:
        config = tomllib.load(f)

    api = config.get("api", {})
    pg = api.get("pg", {})
    pg_addr = None
    if isinstance(pg, dict):
        pg_addr = pg.get("addr")
    elif isinstance(pg, list) and len(pg) > 0:
        for listener in pg:
            if not listener.get("readonly", False):
                pg_addr = listener.get("addr")
                break
        if not pg_addr and pg:
            pg_addr = pg[0].get("addr")

    db_path = config.get("db", {}).get("path")
    return pg_addr, db_path


def parse_host_port(addr):
    """Parse a host:port address, handling IPv6 brackets.

    '[fc01::1]:5487' → ('fc01::1', 5487)
    '127.0.0.1:5487' → ('127.0.0.1', 5487)
    'localhost'      → ('localhost', 5432)
    """
    if addr.startswith("["):
        # IPv6: [host]:port
        end = addr.find("]")
        if end == -1:
            raise ValueError(f"Malformed IPv6 address: {addr}")
        host = addr[1:end]
        rest = addr[end+1:]
        if rest.startswith(":"):
            port = int(rest[1:])
        else:
            port = 5432
    elif ":" in addr:
        host, port = addr.rsplit(":", 1)
        port = int(port)
    else:
        host = addr
        port = 5432
    return host, port


def connect_postgres(addr):
    """Connect to Postgres wire protocol endpoint using pg8000 (pure Python).

    Uses TCP keepalives to detect dead connections and a 60s timeout
    per socket operation.
    """
    host, port = parse_host_port(addr)

    conn = pg8000.connect(
        host=host,
        port=port,
        database="state",
        user="postgres",
        timeout=180,          # 3 min — queries take 12-19s on sqlite directly, PG API adds overhead
        tcp_keepalive=True,
    )
    conn.autocommit = True
    return conn


def confirm(prompt, default=False):
    """Ask user for yes/no confirmation. Returns True for yes."""
    suffix = " [Y/n] " if default else " [y/N] "
    try:
        answer = input(prompt + suffix).strip().lower()
    except EOFError:
        return default
    if not answer:
        return default
    return answer in ("y", "yes")


def exec_fetchall(conn, sql, params=None):
    """Execute and fetch all rows."""
    cur = conn.cursor()
    cur.execute(sql, params or ())
    return cur.fetchall()


def exec_fetchone(conn, sql, params=None):
    """Execute and fetch one row."""
    cur = conn.cursor()
    cur.execute(sql, params or ())
    return cur.fetchone()


def get_crr_tables(conn):
    """Discover all tables with V1 crsql tables."""
    rows = exec_fetchall(conn, """
        SELECT name FROM sqlite_master
        WHERE type='table' AND name LIKE '%__crsql_clock'
    """)
    v1_tables = [row[0] for row in rows]

    result = []
    for v1_table in v1_tables:
        base_table = v1_table.replace("__crsql_clock", "")
        if base_table.startswith("crsql_"):
            continue
        result.append(base_table)
    return result


def get_pk_info(conn, table):
    """Get PK columns from crsql_master v2_pks_{table} key, or from pragma."""
    # Corrosion's PG API doesn't support parameterized queries, so inline the key.
    # Table name comes from our own table scan, not user input.
    key = f"v2_pks_{table}".replace("'", "''")
    row = exec_fetchone(conn, f"SELECT value FROM crsql_master WHERE key = '{key}'")
    if row:
        value = row[0]
        parts = value.split(":", 1)
        pk_str = parts[1]
        pk_cols = []
        for pk_part in pk_str.split(","):
            col_info = pk_part.split(":")
            pk_cols.append(col_info[0])
        return pk_cols

    rows = exec_fetchall(conn, f'PRAGMA table_info("{table}")')
    pk_cols = [(int(r[5]), r[1]) for r in rows if int(r[5]) > 0]
    pk_cols.sort()
    return [c[1] for c in pk_cols]


def get_expected_clock_count(conn, table):
    """Get expected clock entries per row: count of non-PK columns."""
    try:
        row = exec_fetchone(conn, f'SELECT count(*) FROM "{table}__crsql_v2_col_map" WHERE col_name != \'\'')
        count = int(row[0])
        if count > 0:
            return count
    except Exception:
        pass

    rows = exec_fetchall(conn, f'PRAGMA table_info("{table}")')
    non_pk = [r for r in rows if int(r[5]) == 0]
    return len(non_pk)


def get_all_columns(conn, table):
    """Get all column names for a table."""
    rows = exec_fetchall(conn, f'PRAGMA table_info("{table}")')
    return [r[1] for r in rows]


def scan_table(conn, table, expected_count, pk_cols):
    """Scan a table for clock entry issues using separate efficient queries.

    V1 clock table has PRIMARY KEY (key, col_name) WITHOUT ROWID — so
    the `key` column is indexed. V1 pks table has:
      - __crsql_key INTEGER PRIMARY KEY (indexed)
      - UNIQUE INDEX on the actual PK columns (indexed)

    Strategy:
    1. Total row count (simple, fast)
    2. Clock count distribution from pks+clock (uses clock PK index for GROUP BY key)
    3. Untracked: skip for very large tables (the NOT EXISTS scan is too slow
       through corrosion's PG API). Can be computed separately if needed.

    Returns dict with: total_rows, offending, tracked_wrong, untracked,
    distribution (list of (count, num_rows)), max_clock, pks_count.
    """
    # 1. Total row count
    total_rows = int(exec_fetchone(conn, f'SELECT count(*) FROM "{table}"')[0])

    # 2. Clock count distribution — base JOIN pks, filtered to alive keys only
    pk_join = " AND ".join(f'b."{c}" = p."{c}"' for c in pk_cols)
    dist_sql = f'''
        SELECT cnt, count(*) as num_rows
        FROM (
            SELECT (
                SELECT count(*) FROM "{table}__crsql_clock" c
                WHERE c.key = p.__crsql_key AND c.col_name != '-1'
            ) as cnt
            FROM "{table}" b
            JOIN "{table}__crsql_pks" p ON {pk_join}
            WHERE p.__crsql_key IN (
                SELECT key FROM "{table}__crsql_clock" WHERE col_name != '-1' GROUP BY key
            )
        )
        GROUP BY cnt
        ORDER BY cnt
    '''
    dist_rows = exec_fetchall(conn, dist_sql)
    dist = [(int(r[0]), int(r[1])) for r in dist_rows]

    # 3. Untracked: base rows with no pks entry
    if len(pk_cols) == 1:
        c = pk_cols[0]
        untracked_sql = f'''
            SELECT count(*)
            FROM "{table}" b
            WHERE b."{c}" NOT IN (
                SELECT "{c}" FROM "{table}__crsql_pks" WHERE "{c}" IS NOT NULL
            )
        '''
    else:
        pk_where = " AND ".join(f'b."{c}" = p."{c}"' for c in pk_cols)
        untracked_sql = f'''
            SELECT count(*)
            FROM "{table}" b
            WHERE NOT EXISTS (
                SELECT 1 FROM "{table}__crsql_pks" p WHERE {pk_where}
            )
        '''
    try:
        untracked = int(exec_fetchone(conn, untracked_sql)[0])
    except Exception:
        untracked = -1

    # Derive zero-clock: tracked rows (base - untracked) minus rows in distribution
    dist_total = sum(num for _, num in dist)
    if untracked >= 0:
        zero_clock = (total_rows - untracked) - dist_total
        if zero_clock < 0:
            zero_clock = 0
    else:
        zero_clock = 0

    # Build full distribution
    distribution = list(dist)
    if zero_clock > 0:
        distribution.append((0, zero_clock))
    if untracked > 0:
        distribution.append((-1, untracked))
    distribution.sort(key=lambda x: x[0])

    tracked_total = dist_total + zero_clock
    tracked_wrong = sum(num for cnt, num in dist if cnt != expected_count) + zero_clock
    max_clock = max((cnt for cnt, _ in distribution), default=0)

    if untracked < 0:
        offending = tracked_wrong
    else:
        offending = tracked_wrong + untracked

    return {
        "total_rows": total_rows,
        "offending": offending,
        "tracked_wrong": tracked_wrong,
        "untracked": untracked,
        "distribution": distribution,
        "max_clock": max_clock,
        "pks_count": tracked_total,
    }


def repair_untracked(conn, table, pk_cols, batch_size=500):
    """Fix untracked rows: base rows with no __crsql_pks entry.

    For each untracked row, inserts a pks entry and a sentinel clock entry
    (col_name='-1', col_version=1) to mark it as alive. Does NOT touch
    the base table data — no DELETE/INSERT, no trigger firing.

    This maintains the invariant that every base table row has a pks entry,
    without flooding the replication log with spurious changes.

    Returns (total_repaired, error).
    """
    pk_cols_escaped = ", ".join(f'"{c}"' for c in pk_cols)
    pk_cols_list = ", ".join(f'b."{c}"' for c in pk_cols)
    pk_where = " AND ".join(f'b."{c}" = p."{c}"' for c in pk_cols)

    # Find untracked rows: base rows with no pks entry
    if len(pk_cols) == 1:
        c = pk_cols[0]
        untracked_subquery = f'''
            SELECT b."{c}"
            FROM "{table}" b
            WHERE b."{c}" NOT IN (
                SELECT "{c}" FROM "{table}__crsql_pks" WHERE "{c}" IS NOT NULL
            )
            LIMIT {batch_size}
        '''
    else:
        untracked_subquery = f'''
            SELECT {pk_cols_list}
            FROM "{table}" b
            WHERE NOT EXISTS (
                SELECT 1 FROM "{table}__crsql_pks" p WHERE {pk_where}
            )
            LIMIT {batch_size}
        '''

    total_repaired = 0
    batch_num = 0

    while True:
        batch_num += 1
        print(f"\r  batch {batch_num}...", end="", flush=True)

        try:
            cur = conn.cursor()

            # 1. Insert pks entries for untracked rows
            # INSERT OR IGNORE to skip any that already exist (race safety)
            cur.execute(
                f'INSERT OR IGNORE INTO "{table}__crsql_pks" ({pk_cols_escaped}) '
                f'SELECT {pk_cols_list} FROM ({untracked_subquery}) q'
            )
            batch_count = cur.rowcount

            if batch_count == 0:
                conn.commit()
                break  # No more untracked rows

            # 2. Insert sentinel clock entries for the new pks entries
            # col_name='-1' (INSERT_SENTINEL), col_version=1 (alive), db_version=0, seq=0, site_id=0, ts='0'
            cur.execute(
                f'INSERT OR IGNORE INTO "{table}__crsql_clock" (key, col_name, col_version, db_version, seq, site_id, ts) '
                f'SELECT p.__crsql_key, \'-1\', 1, 0, 0, 0, \'0\' '
                f'FROM "{table}__crsql_pks" p '
                f'JOIN ({untracked_subquery}) q ON {(" AND ".join(f"p.\"{c}\" = q.\"{c}\"" for c in pk_cols))} '
                f'WHERE NOT EXISTS ('
                f'  SELECT 1 FROM "{table}__crsql_clock" c '
                f'  WHERE c.key = p.__crsql_key AND c.col_name = \'-1\''
                f')'
            )

            conn.commit()
            total_repaired += batch_count
        except Exception as e:
            conn.rollback()
            print(f" FAIL: {e}")
            return (total_repaired, str(e))

    return (total_repaired, None)

    print(f" done ({total_repaired} rows in {batch_num - 1} batches)")
    return (total_repaired, None)


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <config_path>")
        print(f"  config_path: path to corrosion config.toml")
        sys.exit(1)

    config_path = sys.argv[1]

    pg_addr, db_path = load_config(config_path)

    if not pg_addr:
        print(f"Error: no api.pg.addr found in {config_path}")
        print("Make sure the config has [api.pg] with addr set.")
        sys.exit(1)

    print(f"Config: {config_path}")
    print(f"Postgres endpoint: {pg_addr}")
    if db_path:
        print(f"Database path: {db_path}")
    print()

    if not confirm(f"Connect to Corrosion at {pg_addr}?"):
        print("Aborted.")
        sys.exit(0)

    # Connect
    print(f"Connecting...", end=" ", flush=True)
    try:
        conn = connect_postgres(pg_addr)
    except Exception as e:
        print(f"FAILED: {e}")
        sys.exit(1)
    print("connected (pg8000)")

    # Ping
    print("Pinging...", end=" ", flush=True)
    try:
        row = exec_fetchone(conn, "SELECT 1")
        if row and row[0] == 1:
            print("OK")
        else:
            print("UNEXPECTED RESPONSE")
            sys.exit(1)
    except Exception as e:
        print(f"FAILED: {e}")
        sys.exit(1)

    # Check crsql_master exists
    print("Checking crsql_master...", end=" ", flush=True)
    try:
        row = exec_fetchone(conn, "SELECT name FROM sqlite_master WHERE type='table' AND name='crsql_master'")
        if not row:
            print("NOT FOUND")
            print("Error: crsql_master table not found. Is this a crsql database?")
            sys.exit(1)
        print("OK")
    except Exception as e:
        print(f"FAILED: {e}")
        sys.exit(1)

    print("Discovering CRR tables...", end=" ", flush=True)
    tables = get_crr_tables(conn)
    print(f"found {len(tables)}")
    if not tables:
        print("No CRR tables found")
        sys.exit(0)

    print(f"\nFound {len(tables)} CRR tables:")
    for t in tables:
        print(f"  - {t}... ", end="", flush=True)
        pk_cols = get_pk_info(conn, t)
        pk_str = ", ".join(pk_cols) if pk_cols else "?"
        print(f"(pk: {pk_str})")
    print()

    # Step 1: Confirm scan
    if not confirm("Scan all tables for missing clock entries?"):
        print("Aborted.")
        sys.exit(0)

    print("\nScanning...\n")

    scan_results = []
    for i, table in enumerate(tables, 1):
        print(f"  [{i}/{len(tables)}] {table}...", end="", flush=True)

        # Reconnect if the previous query killed the connection
        try:
            exec_fetchone(conn, "SELECT 1")
        except Exception:
            print("(reconnecting)... ", end="", flush=True)
            try:
                conn.close()
            except Exception:
                pass
            conn = connect_postgres(pg_addr)
            print("OK ", end="", flush=True)

        pk_cols = get_pk_info(conn, table)
        if not pk_cols:
            print(" SKIP (could not determine PK columns)")
            continue

        expected = get_expected_clock_count(conn, table)
        if expected == 0:
            # PK-only table — no non-PK columns, no clock entries expected
            row_count = int(exec_fetchone(conn, f'SELECT count(*) FROM "{table}"')[0])
            print(f" OK (PK-only, {row_count} rows)")
            continue

        try:
            result = scan_table(conn, table, expected, pk_cols)
        except Exception as e:
            print(f" ERROR (scan): {e}")
            try:
                conn.close()
            except Exception:
                pass
            conn = connect_postgres(pg_addr)
            try:
                result = scan_table(conn, table, expected, pk_cols)
            except Exception as e2:
                print(f" RETRY FAILED: {e2}")
                continue

        pks_note = ""
        if result.get("pks_count", 0) != result["total_rows"]:
            pks_note = f" [pks: {result['pks_count']}, base: {result['total_rows']}]"

        untracked_str = str(result['untracked']) if result['untracked'] >= 0 else "?"

        if result["offending"] > 0:
            print(f" {result['offending']}/{result['total_rows']} offending (expected {expected} clk/row, max {result['max_clock']}, {result['tracked_wrong']} wrong + {untracked_str} untracked){pks_note}")
        else:
            print(f" OK ({result['total_rows']} rows){pks_note}")

        scan_results.append({
            "table": table,
            "pk_cols": pk_cols,
            "expected": expected,
            **result,
        })

    # Print scan summary
    print("Scan results:\n")
    needs_repair = []
    for r in scan_results:
        pk_str = ", ".join(r["pk_cols"])
        if r["offending"] == 0:
            print(f"  {r['table']} (pk: {pk_str}): OK (all rows have {r['expected']} clock entries)")
        else:
            print(f"  {r['table']} (pk: {pk_str}): {r['offending']} rows need repair")
            print(f"    expected {r['expected']} clock entries per row")
            print(f"    distribution:")
            for cnt, num in r["distribution"]:
                if cnt == -1:
                    label = "UNTRACKED (no __crsql_pks entry)"
                    marker = " <-- WRONG"
                else:
                    label = f"{cnt} entries"
                    marker = " <-- WRONG" if cnt != r["expected"] else ""
                print(f"      {label}: {num} rows{marker}")
            needs_repair.append(r)

    if not needs_repair:
        print("\nAll tables are fine — no repair needed.")
        sys.exit(0)

    # Filter to only tables with untracked rows
    needs_untracked_fix = [r for r in needs_repair if r["untracked"] > 0]
    tracked_wrong_total = sum(r["tracked_wrong"] for r in needs_repair)

    if tracked_wrong_total > 0:
        print(f"\nNote: {tracked_wrong_total} rows have pks entries but wrong clock entry counts.")
        print("These are NOT being repaired — they are safe to leave as-is:")
        print("  - They don't appear in crsql_changes (invisible to replication)")
        print("  - V2 migration handles them correctly (is_alive=1, cl defaults to 1)")
        print("  - Future mutations will create proper clock entries")
        print("  - Repairing them (DELETE+INSERT) would flood replication with spurious changes")
        print()

    if not needs_untracked_fix:
        print("\nNo untracked rows found — all base rows have pks entries. Nothing to repair.")
        sys.exit(0)

    print(f"\n{len(needs_untracked_fix)} tables have untracked rows, {sum(r['untracked'] for r in needs_untracked_fix)} total.")
    print("These rows exist in the base table but have no __crsql_pks entry.")
    print("Repair: INSERT pks entry + sentinel clock (col_version=1) — no base table changes.")
    print()

    # Step 2: Confirm each table repair individually
    repaired_tables = []
    for r in needs_untracked_fix:
        table = r["table"]
        untracked_count = r["untracked"]
        num_batches = (untracked_count + 499) // 500

        print(f"Table: {table} (pk: {', '.join(r['pk_cols'])})")
        print(f"  {untracked_count} untracked rows (~{num_batches} batches of 500)")
        print(f"  Repair: INSERT into __crsql_pks + sentinel clock entry (no base table changes)")
        print()

        if not confirm(f"  Fix untracked rows in {table} (~{untracked_count} rows)?"):
            print(f"  Skipped {table}.\n")
            continue

        repaired, error = repair_untracked(conn, table, r["pk_cols"])
        if error:
            print(f"  FAIL: {error}")
        else:
            repaired_tables.append((table, repaired))
        print()

    if not repaired_tables:
        print("No tables were repaired.")
        sys.exit(0)

    # Summary
    print(f"Repair summary:")
    for table, count in repaired_tables:
        print(f"  {table}: {count} untracked rows fixed")
    print(f"  Total: {sum(c for _, c in repaired_tables)} rows")
    print()

    # Verify
    print("Verifying repairs...")
    all_ok = True
    for table, _ in repaired_tables:
        pk_cols = get_pk_info(conn, table)
        expected = get_expected_clock_count(conn, table)
        if expected == 0:
            expected = 1
        result = scan_table(conn, table, expected, pk_cols)
        if result["untracked"] > 0:
            print(f"  {table}: STILL HAS {result['untracked']} untracked rows")
            all_ok = False
        else:
            print(f"  {table}: OK (0 untracked)")

    if all_ok:
        print("\nAll repairs verified successfully.")
    else:
        print("\nSome tables still have untracked rows — see above.")

    conn.close()


if __name__ == "__main__":
    main()
