#!/usr/bin/env python3
"""Backfill missing V1 clock entries by delete+reinserting offending rows.

For each CRR table, finds rows that need repair:
  1. Tracked but wrong: rows in __crsql_pks with wrong clock entry count
     (fewer or more than expected — one per non-PK column, or 1 for pk-only)
  2. Untracked: rows in the base table with no __crsql_pks entry at all

For each such table, processes offending rows in batches of 500.
Each batch is its own committed transaction:
  1. Copy batch rows to a temp table
  2. DELETE them (triggers fire → tombstone with even CL)
  3. INSERT them back (triggers fire → alive with odd CL, all clock entries)
  4. Drop the temp table
  5. Commit

Both delete and insert in a batch share the same db_version (same transaction),
so peers fetching changes at that db_version see: delete absorbed by insert.
Net effect: row is alive with all columns properly clocked at col_version=1.
The higher CL ensures this write wins over stale state on peers.

Batching keeps changesets small so corrosion doesn't need to process
one huge transaction per table.

Writes go through the Corrosion Postgres wire-protocol API so they
replicate properly to peers. Direct SQLite writes would not replicate.

Uses psycopg3 (the `psycopg` package) with its transaction() context
manager for robust, well-tested transaction handling.

    pip install psycopg[binary]

The script is interactive and requires confirmation before:
  - Pinging the Corrosion API
  - Starting the scan
  - Repairing each table (shows batch count)

Usage:
  python3 backfill_v1_clock_entries.py <config_path>

  config_path: path to corrosion config.toml (e.g., /etc/corrosion2/config.toml)
"""
import sys
import os

try:
    import psycopg
except ImportError:
    print("Error: psycopg not installed.")
    print("Install: pip install 'psycopg[binary]'")
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
    """Connect to Postgres wire protocol endpoint using psycopg3.

    Returns a connection object.
    """
    host, port = parse_host_port(addr)

    conn = psycopg.connect(
        host=host,
        port=port,
        dbname="corrosion",
        user="corrosion",
        connect_timeout=10,
        autocommit=True,  # We'll use explicit transaction() for batches
    )
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
    """Execute and fetch all rows. Uses autocommit connection (read-only queries)."""
    with conn.cursor() as cur:
        if params:
            cur.execute(sql, params)
        else:
            cur.execute(sql)
        return cur.fetchall()


def exec_fetchone(conn, sql, params=None):
    """Execute and fetch one row. Uses autocommit connection (read-only queries)."""
    with conn.cursor() as cur:
        if params:
            cur.execute(sql, params)
        else:
            cur.execute(sql)
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
    row = exec_fetchone(conn, "SELECT value FROM crsql_master WHERE key=%s", (f"v2_pks_{table}",))
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
    pk_cols = [(r[5], r[1]) for r in rows if r[5] > 0]
    pk_cols.sort()
    return [c[1] for c in pk_cols]


def get_expected_clock_count(conn, table):
    """Get expected clock entries per row: count of non-PK columns."""
    try:
        row = exec_fetchone(conn, f'SELECT count(*) FROM "{table}__crsql_v2_col_map" WHERE col_name != \'\'')
        count = row[0]
        if count > 0:
            return count
    except Exception:
        pass

    rows = exec_fetchall(conn, f'PRAGMA table_info("{table}")')
    non_pk = [r for r in rows if r[5] == 0]
    return len(non_pk)


def get_all_columns(conn, table):
    """Get all column names for a table."""
    rows = exec_fetchall(conn, f'PRAGMA table_info("{table}")')
    return [r[1] for r in rows]


def find_offending_count(conn, table, expected_count, pk_cols):
    """Count rows that need repair.

    Starts from the base table, LEFT JOINs to pks + clock:
      - Untracked: no pks entry (p.__crsql_key IS NULL)
      - Tracked-wrong: pks entry exists but clock count is wrong or missing

    Returns (total_count, tracked_wrong_count, untracked_count).
    """
    pk_join = " AND ".join(f'p."{c}" = b."{c}"' for c in pk_cols)

    # Untracked: base rows with no pks entry
    untracked_sql = f'''
        SELECT count(*)
        FROM "{table}" b
        LEFT JOIN "{table}__crsql_pks" p ON {pk_join}
        WHERE p.__crsql_key IS NULL
    '''
    untracked = int(exec_fetchone(conn, untracked_sql)[0])

    # Tracked-wrong: base rows with pks entry but wrong clock count
    tracked_sql = f'''
        SELECT count(*)
        FROM "{table}" b
        JOIN "{table}__crsql_pks" p ON {pk_join}
        LEFT JOIN (
            SELECT c.key, count(*) AS cnt
            FROM "{table}__crsql_clock" c
            WHERE c.col_name != '-1'
            GROUP BY c.key
        ) clk ON clk.key = p.__crsql_key
        WHERE clk.cnt IS NULL OR clk.cnt != {expected_count}
    '''
    tracked_wrong = int(exec_fetchone(conn, tracked_sql)[0])

    return (tracked_wrong + untracked, tracked_wrong, untracked)


def get_clock_count_distribution(conn, table, pk_cols):
    """Get distribution of clock entry counts for a table.

    Starts from the base table, LEFT JOINs to pks + clock.
    count=-1 means untracked (no __crsql_pks entry).

    Returns list of (count, num_rows) sorted by count.
    """
    pk_join = " AND ".join(f'p."{c}" = b."{c}"' for c in pk_cols)

    # All base rows, classified by clock count or untracked
    sql = f'''
        SELECT
            CASE
                WHEN p.__crsql_key IS NULL THEN -1
                WHEN clk.cnt IS NULL THEN 0
                ELSE clk.cnt
            END as cnt,
            count(*) as num_rows
        FROM "{table}" b
        LEFT JOIN "{table}__crsql_pks" p ON {pk_join}
        LEFT JOIN (
            SELECT c.key, count(*) AS cnt
            FROM "{table}__crsql_clock" c
            WHERE c.col_name != '-1'
            GROUP BY c.key
        ) clk ON clk.key = p.__crsql_key
        GROUP BY cnt
        ORDER BY cnt
    '''
    rows = exec_fetchall(conn, sql)
    return [(int(r[0]), int(r[1])) for r in rows]


def repair_table(conn, table, pk_cols, expected_count, batch_size=500):
    """Repair a table in batches, entirely in SQL.

    Each batch:
      1. Finds 500 offending rows (LIMIT) — copy to temp table
      2. DELETE from base (triggers fire → tombstone)
      3. INSERT back from temp (triggers fire → alive with all clock entries)
      4. Drop temp

    No queue table — each batch re-runs the offending query with LIMIT.
    This avoids a full table scan upfront but re-scans per batch. The
    LIMIT stops early once 500 are found, so each scan is proportional
    to how far into the table the offending rows are.

    Returns (total_repaired, error).
    """
    all_cols = get_all_columns(conn, table)
    col_list_escaped = ", ".join(f'"{c}"' for c in all_cols)
    pk_cols_escaped = ", ".join(f'"{c}"' for c in pk_cols)
    pk_join = " AND ".join(f'p."{c}" = b."{c}"' for c in pk_cols)
    temp_pk_match = " AND ".join(
        f'"{table}"."{c}" = "{temp_name}"."{c}"' for c in pk_cols
    )

    temp_name = f"_backfill_temp_{table}"

    # Clean up any leftover temp table
    try:
        with conn.transaction():
            with conn.cursor() as cur:
                cur.execute(f'DROP TABLE IF EXISTS "{temp_name}"')
    except Exception:
        pass

    # Subquery to find offending rows — reused per batch with LIMIT
    # Starts from base table, LEFT JOINs to pks + clock:
    #   - Untracked: no pks entry (p.__crsql_key IS NULL)
    #   - Tracked-wrong: pks entry exists but clock count wrong or missing
    offending_subquery = f'''
        SELECT b.{pk_cols_escaped}
        FROM "{table}" b
        LEFT JOIN "{table}__crsql_pks" p ON {pk_join}
        LEFT JOIN (
            SELECT c.key, count(*) AS cnt
            FROM "{table}__crsql_clock" c
            WHERE c.col_name != '-1'
            GROUP BY c.key
        ) clk ON clk.key = p.__crsql_key
        WHERE p.__crsql_key IS NULL
           OR clk.cnt IS NULL
           OR clk.cnt != {expected_count}
        LIMIT {batch_size}
    '''

    # Join condition for matching temp table PKs back to base
    temp_join = " AND ".join(f'b."{c}" = q."{c}"' for c in pk_cols)

    total_repaired = 0
    batch_num = 0

    while True:
        batch_num += 1
        print(f"\r  batch {batch_num}...", end="", flush=True)

        try:
            with conn.transaction():
                with conn.cursor() as cur:
                    # 1. Copy 500 offending rows to temp
                    cur.execute(f'DROP TABLE IF EXISTS "{temp_name}"')
                    cur.execute(
                        f'CREATE TEMP TABLE "{temp_name}" AS '
                        f'SELECT b.* FROM "{table}" b '
                        f'JOIN ({offending_subquery}) q ON {temp_join}'
                    )

                    # Check if we got any rows
                    cur.execute(f'SELECT count(*) FROM "{temp_name}"')
                    batch_count = cur.fetchone()[0]
                    if batch_count == 0:
                        cur.execute(f'DROP TABLE "{temp_name}"')
                        break  # No more offending rows

                    # 2. Delete from base (triggers fire → tombstone)
                    # Safety: only delete rows that match a PK in temp (the 500 we copied)
                    cur.execute(
                        f'DELETE FROM "{table}" WHERE EXISTS '
                        f'(SELECT 1 FROM "{temp_name}" WHERE {temp_pk_match})'
                    )
                    deleted_count = cur.rowcount
                    if deleted_count != batch_count:
                        raise RuntimeError(
                            f"DELETE mismatch: expected {batch_count}, got {deleted_count}. "
                            f"Aborting to prevent data loss."
                        )

                    # 3. Reinsert from temp (triggers fire → alive with all clock entries)
                    cur.execute(
                        f'INSERT INTO "{table}" ({col_list_escaped}) '
                        f'SELECT {col_list_escaped} FROM "{temp_name}"'
                    )
                    inserted_count = cur.rowcount
                    if inserted_count != batch_count:
                        raise RuntimeError(
                            f"INSERT mismatch: expected {batch_count}, got {inserted_count}. "
                            f"Aborting to prevent data loss."
                        )

                    # 4. Drop temp
                    cur.execute(f'DROP TABLE "{temp_name}"')

            total_repaired += batch_count
        except Exception as e:
            print(f" FAIL: {e}")
            try:
                with conn.transaction():
                    with conn.cursor() as cur:
                        cur.execute(f'DROP TABLE IF EXISTS "{temp_name}"')
            except Exception:
                pass
            return (total_repaired, str(e))

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
    print("connected (psycopg3)")

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
    try:
        row = exec_fetchone(conn, "SELECT name FROM sqlite_master WHERE type='table' AND name='crsql_master'")
        if not row:
            print("Error: crsql_master table not found. Is this a crsql database?")
            sys.exit(1)
    except Exception as e:
        print(f"Error checking crsql_master: {e}")
        sys.exit(1)

    tables = get_crr_tables(conn)
    if not tables:
        print("No CRR tables found")
        sys.exit(0)

    print(f"\nFound {len(tables)} CRR tables:")
    for t in tables:
        pk_cols = get_pk_info(conn, t)
        pk_str = ", ".join(pk_cols) if pk_cols else "?"
        print(f"  - {t} (pk: {pk_str})")
    print()

    # Step 1: Confirm scan
    if not confirm("Scan all tables for missing clock entries?"):
        print("Aborted.")
        sys.exit(0)

    print("\nScanning...\n")

    scan_results = []
    for table in tables:
        pk_cols = get_pk_info(conn, table)
        if not pk_cols:
            print(f"  {table}: SKIP (could not determine PK columns)")
            continue

        expected = get_expected_clock_count(conn, table)
        if expected == 0:
            expected = 1  # pk-only

        total, tracked_wrong, untracked = find_offending_count(conn, table, expected, pk_cols)
        distribution = get_clock_count_distribution(conn, table, pk_cols)

        scan_results.append({
            "table": table,
            "pk_cols": pk_cols,
            "expected": expected,
            "offending_count": total,
            "tracked_wrong": tracked_wrong,
            "untracked": untracked,
            "distribution": distribution,
        })

    # Print scan summary
    print("Scan results:\n")
    needs_repair = []
    for r in scan_results:
        pk_str = ", ".join(r["pk_cols"])
        if r["offending_count"] == 0:
            print(f"  {r['table']} (pk: {pk_str}): OK (all rows have {r['expected']} clock entries)")
        else:
            print(f"  {r['table']} (pk: {pk_str}): {r['offending_count']} rows need repair")
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

    print(f"\n{len(needs_repair)} tables need repair, {sum(r['offending_count'] for r in needs_repair)} rows total.")
    print()

    # Step 2: Confirm each table repair individually
    repaired_tables = []
    for r in needs_repair:
        table = r["table"]
        count = r["offending_count"]
        expected = r["expected"]
        tracked_wrong = r["tracked_wrong"]
        untracked_count = r["untracked"]
        num_batches = (count + 499) // 500

        print(f"Table: {table} (pk: {', '.join(r['pk_cols'])})")
        if tracked_wrong > 0:
            print(f"  {tracked_wrong} tracked rows with wrong clock entry count (expected {expected} per row)")
        if untracked_count > 0:
            print(f"  {untracked_count} untracked rows (in base table but no __crsql_pks entry)")
        est_batches = (count + 499) // 500
        print(f"  This will DELETE and re-INSERT ~{count} rows in ~{est_batches} batches of 500.")
        print(f"  Each batch is its own committed transaction (smaller changesets for replication).")
        print(f"  Triggers will fire, creating proper clock entries for all columns.")
        print(f"  Writes go through Corrosion PG API for replication.")
        print()

        if not confirm(f"  Repair {table} (~{count} rows, ~{est_batches} batches)?"):
            print(f"  Skipped {table}.\n")
            continue

        repaired, error = repair_table(conn, table, r["pk_cols"], expected)
        if error:
            print(f"  FAIL: {error}")
        else:
            repaired_tables.append((table, repaired))
        print()

    if not repaired_tables:
        print("No tables were repaired.")
        sys.exit(0)

    # Summary (batches were committed individually)
    print(f"Repair summary:")
    for table, count in repaired_tables:
        print(f"  {table}: {count} rows repaired")
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
        total, _, _ = find_offending_count(conn, table, expected, pk_cols)
        if total > 0:
            print(f"  {table}: STILL HAS {total} offending rows")
            all_ok = False
        else:
            print(f"  {table}: OK")

    if all_ok:
        print("\nAll repairs verified successfully.")
    else:
        print("\nSome tables still have issues — see above.")

    conn.close()


if __name__ == "__main__":
    main()
