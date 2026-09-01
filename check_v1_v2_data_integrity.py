#!/usr/bin/env python3
"""Validate V1↔V2 data integrity for all CRR tables in a SQLite database.

Discovers all tables with both V1 (__crsql_clock) and V2 (__crsql_v2_clock)
tables, then validates in parallel (one connection per table):
  1. Alive rows: v2_pks count == base table count
  2. Dead rows: v2_tombstones count == V1 dead sentinels
  3. Tombstone PKs: v2_tombstone_pks count == V1 dead (hash tables only)
  4. Clock entries: v2_clock count == V1 clock for alive rows
  5. Data integrity: all clock values match between V1 and V2

Usage:
  python3 check_v1_v2_data_integrity.py <database> [extension_path]

If extension_path is omitted, tries ./crsqlite.so, ./crsqlite.dylib, crsqlite.
The extension is optional - validation works without it.
"""
import sqlite3
import sys
import os
import threading
from concurrent.futures import ThreadPoolExecutor, as_completed


def get_crr_tables(conn):
    """Discover all tables with both V1 and V2 crsql tables."""
    cursor = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE '%__crsql_clock'"
    )
    v1_tables = [row[0] for row in cursor.fetchall()]

    result = []
    for v1_table in v1_tables:
        base_table = v1_table.replace("__crsql_clock", "")
        # Skip crsql internal tables
        if base_table.startswith("crsql_"):
            continue
        v2_clock = f"{base_table}__crsql_v2_clock"
        cursor = conn.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
            (v2_clock,),
        )
        if cursor.fetchone():
            result.append(base_table)
    return result


def get_pk_info(conn, table):
    """Get PK columns and mode from crsql_master v2_pks_{table} key.

    Format: rh:id:TEXT,node:TEXT (hash) or rs:id:INTEGER (skip_hash)
    Modes: rs=rowid+skip_hash, ns=non-rowid+skip_hash, rh=rowid+hash, nh=non-rowid+hash
    Returns (mode, [pk_cols]) or (None, None) if not found.
    """
    cursor = conn.execute(
        "SELECT value FROM crsql_master WHERE key=?", (f"v2_pks_{table}",)
    )
    row = cursor.fetchone()
    if not row:
        return None, None

    value = row[0]
    parts = value.split(":", 1)
    mode = parts[0]  # rh = hash, rs = skip_hash
    pk_str = parts[1]

    pk_cols = []
    for pk_part in pk_str.split(","):
        col_info = pk_part.split(":")
        pk_cols.append(col_info[0])

    return mode, pk_cols


def get_col_id_bits(conn):
    """Get col_id_bits from crsql_master (default 12)."""
    try:
        cursor = conn.execute(
            "SELECT value FROM crsql_master WHERE key='crsql_col_id_bits'"
        )
        row = cursor.fetchone()
        return int(row[0]) if row else 12
    except Exception:
        return 12


def get_v2_pks_columns(conn, table):
    """Get column names from v2_pks table."""
    cursor = conn.execute(f'PRAGMA table_info("{table}__crsql_v2_pks")')
    return [row[1] for row in cursor.fetchall()]


def is_pk_only_table(conn, table):
    """Check if table has non-PK columns by looking at v2_col_map."""
    cursor = conn.execute(
        f'SELECT count(*) FROM "{table}__crsql_v2_col_map" WHERE col_name != \'\''
    )
    return cursor.fetchone()[0] == 0


def escape_ident(name):
    return f'"{name}"'


def build_base_join(pk_cols, alias_p="p", alias_b="b"):
    """Build JOIN condition: b."pk1" = p."pk1" AND b."pk2" = p."pk2\""""
    conditions = []
    for pk in pk_cols:
        conditions.append(f"{alias_b}.{escape_ident(pk)} = {alias_p}.{escape_ident(pk)}")
    return " AND ".join(conditions)


def build_v2_pks_join(pk_cols, v2_pks_cols):
    """Build JOIN condition for v2_pks.

    If PK columns are stored in v2_pks: JOIN on PK columns directly.
    Otherwise (key_is_rowid): JOIN on vp.__crsql_key = b.rowid
    """
    pk_in_v2_pks = all(col in v2_pks_cols for col in pk_cols)
    if pk_in_v2_pks:
        conditions = []
        for pk in pk_cols:
            conditions.append(f"vp.{escape_ident(pk)} = p.{escape_ident(pk)}")
        return " AND ".join(conditions)
    else:
        return "vp.__crsql_key = b.rowid"


def validate_table(db_path, ext_path, table, col_id_bits):
    """Run all validation checks for a table using a dedicated connection.
    Returns (table, ok, errors, stats)."""
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=300)
    try:
        # Load extension if provided
        if ext_path:
            try:
                conn.enable_load_extension(True)
                conn.load_extension(ext_path)
            except Exception:
                pass

        mode, pk_cols = get_pk_info(conn, table)
        if not mode:
            return (table, True, [], {"skip": True})

        v2_pks_cols = get_v2_pks_columns(conn, table)
        pk_only = is_pk_only_table(conn, table)
        base_join = build_base_join(pk_cols)
        v2_pks_join = build_v2_pks_join(pk_cols, v2_pks_cols)
        all_ok = True
        errors = []

        # 1. Alive rows: v2_pks count == base table count
        v2_pks_count = conn.execute(
            f'SELECT count(*) FROM "{table}__crsql_v2_pks"'
        ).fetchone()[0]
        base_count = conn.execute(f'SELECT count(*) FROM "{table}"').fetchone()[0]
        if v2_pks_count != base_count:
            errors.append(f"alive rows mismatch (v2_pks={v2_pks_count}, base={base_count})")
            all_ok = False

        # 2. Dead rows: v2_tombstones count == V1 dead sentinels
        v2_tomb_count = conn.execute(
            f'SELECT count(*) FROM "{table}__crsql_v2_tombstones"'
        ).fetchone()[0]
        v1_dead_count = conn.execute(
            f"SELECT count(*) FROM \"{table}__crsql_clock\" "
            f"WHERE col_name = '-1' AND col_version % 2 = 0"
        ).fetchone()[0]
        if v2_tomb_count != v1_dead_count:
            errors.append(
                f"dead rows mismatch (v2_tombstones={v2_tomb_count}, v1_dead={v1_dead_count})"
            )
            all_ok = False

        # 3. Tombstone PKs (hash mode only — both rh and nh)
        if mode.endswith("h"):
            v2_tomb_pks_count = conn.execute(
                f'SELECT count(*) FROM "{table}__crsql_v2_tombstone_pks"'
            ).fetchone()[0]
            if v2_tomb_pks_count != v1_dead_count:
                errors.append(
                    f"tombstone PKs mismatch (v2={v2_tomb_pks_count}, v1_dead={v1_dead_count})"
                )
                all_ok = False

        # 4. Clock entries: v2_clock count == V1 clock for alive rows
        v2_clock_count = conn.execute(
            f'SELECT count(*) FROM "{table}__crsql_v2_clock"'
        ).fetchone()[0]

        if pk_only:
            v1_clock_filter = "c.col_name = '-1'"
        else:
            v1_clock_filter = "c.col_name != '-1'"

        v1_clock_alive = conn.execute(
            f'''SELECT count(*) FROM "{table}__crsql_clock" c
                JOIN "{table}__crsql_pks" p ON c.key = p.__crsql_key
                JOIN "{table}" b ON {base_join}
                WHERE {v1_clock_filter}'''
        ).fetchone()[0]

        if v2_clock_count != v1_clock_alive:
            errors.append(
                f"clock entries mismatch (v2={v2_clock_count}, v1_alive={v1_clock_alive})"
            )
            all_ok = False

        # 5. Data integrity: compare all clock values between V1 and V2
        if pk_only:
            integrity_sql = f'''
                SELECT status, count(*) as cnt FROM (
                  SELECT
                    CASE
                      WHEN v.cell_key IS NULL THEN 'MISSING'
                      WHEN c.col_version = v.col_version
                        AND c.site_id = v.site_id
                        AND c.db_version = v.db_version
                        AND c.seq = v.seq
                        AND CASE WHEN CAST(c.ts AS INTEGER) > 0
                                 THEN CAST(c.ts AS INTEGER) ELSE 1 END = v.ts
                      THEN 'MATCH'
                      ELSE 'MISMATCH'
                    END as status
                  FROM "{table}__crsql_clock" c
                  JOIN "{table}__crsql_pks" p ON c.key = p.__crsql_key
                  JOIN "{table}" b ON {base_join}
                  JOIN "{table}__crsql_v2_pks" vp ON {v2_pks_join}
                  LEFT JOIN "{table}__crsql_v2_clock" v
                    ON v.cell_key = ((vp.__crsql_key << {col_id_bits}) | 0)
                  WHERE c.col_name = '-1'
                ) GROUP BY status
            '''
        else:
            integrity_sql = f'''
                SELECT status, count(*) as cnt FROM (
                  SELECT
                    CASE
                      WHEN v.cell_key IS NULL THEN 'MISSING'
                      WHEN c.col_version = v.col_version
                        AND c.site_id = v.site_id
                        AND c.db_version = v.db_version
                        AND c.seq = v.seq
                        AND CASE WHEN CAST(c.ts AS INTEGER) > 0
                                 THEN CAST(c.ts AS INTEGER) ELSE 1 END = v.ts
                      THEN 'MATCH'
                      ELSE 'MISMATCH'
                    END as status
                  FROM "{table}__crsql_clock" c
                  JOIN "{table}__crsql_pks" p ON c.key = p.__crsql_key
                  JOIN "{table}" b ON {base_join}
                  JOIN "{table}__crsql_v2_col_map" m ON c.col_name = m.col_name
                  JOIN "{table}__crsql_v2_pks" vp ON {v2_pks_join}
                  LEFT JOIN "{table}__crsql_v2_clock" v
                    ON v.cell_key = ((vp.__crsql_key << {col_id_bits}) | m.col_id)
                  WHERE c.col_name != '-1'
                ) GROUP BY status
            '''

        try:
            cursor = conn.execute(integrity_sql)
            results = cursor.fetchall()
            for status, cnt in results:
                if status != "MATCH":
                    errors.append(f"integrity {status}={cnt}")
                    all_ok = False
        except Exception as e:
            errors.append(f"integrity query failed: {e}")
            all_ok = False

        # 6. Per-row clock entry count: every alive row must have exactly C
        #    clock entries, where C is the number of non-PK columns (regular
        #    tables) or 1 (pk-only tables, the '-1' sentinel). This catches
        #    missing/extra clock entries on individual rows that a global
        #    count comparison (check #4) would miss.
        if pk_only:
            expected_per_row = 1
            clock_filter = "c.col_name = '-1'"
        else:
            expected_per_row = conn.execute(
                f'SELECT count(*) FROM "{table}__crsql_v2_col_map" WHERE col_name != \'\''
            ).fetchone()[0]
            clock_filter = "c.col_name != '-1'"

        per_row_sql = f'''
            SELECT status, count(*) as cnt FROM (
              SELECT
                CASE
                  WHEN clk.clock_cnt IS NULL THEN 'NO_CLOCK_ENTRIES'
                  WHEN clk.clock_cnt = {expected_per_row} THEN 'OK'
                  ELSE 'WRONG_COUNT'
                END as status
              FROM "{table}__crsql_pks" p
              JOIN "{table}" b ON {base_join}
              LEFT JOIN (
                SELECT c.key, count(*) AS clock_cnt
                FROM "{table}__crsql_clock" c
                WHERE {clock_filter}
                GROUP BY c.key
              ) clk ON clk.key = p.__crsql_key
            ) GROUP BY status
        '''

        try:
            cursor = conn.execute(per_row_sql)
            results = cursor.fetchall()
            for status, cnt in results:
                if status != "OK":
                    errors.append(
                        f"per-row clock count {status}={cnt} (expected {expected_per_row} per alive row)"
                    )
                    all_ok = False
        except Exception as e:
            errors.append(f"per-row clock count query failed: {e}")
            all_ok = False

        stats = {
            "alive": v2_pks_count,
            "dead": v2_tomb_count,
            "clock": v2_clock_count,
            "pk_only": pk_only,
            "mode": mode,
        }
        return (table, all_ok, errors, stats)
    finally:
        conn.close()


def main():
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <database> [extension_path]")
        sys.exit(1)

    db_path = sys.argv[1]
    ext_path = sys.argv[2] if len(sys.argv) > 2 else None

    if not os.path.exists(db_path):
        print(f"Error: database not found: {db_path}")
        sys.exit(1)

    # Main connection for discovery (read-only)
    conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)

    # Try to load extension (optional - works without it)
    try:
        conn.enable_load_extension(True)
        loaded = False
        if ext_path:
            try:
                conn.load_extension(ext_path)
                loaded = True
            except Exception:
                pass
        if not loaded:
            for path in ["./crsqlite.so", "./crsqlite.dylib", "crsqlite"]:
                try:
                    conn.load_extension(path)
                    loaded = True
                    break
                except Exception:
                    continue
        if loaded:
            print(f"Extension loaded")
        else:
            print(f"Extension not loaded (validation works without it)")
    except Exception:
        print(f"Extension loading not supported (validation works without it)")

    # Check crsql_master exists
    cursor = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='crsql_master'"
    )
    if not cursor.fetchone():
        print("Error: crsql_master table not found. Is this a crsql database?")
        sys.exit(1)

    col_id_bits = get_col_id_bits(conn)
    tables = get_crr_tables(conn)
    conn.close()

    if not tables:
        print("No CRR tables with both V1 and V2 found")
        sys.exit(0)

    print(f"\nFound {len(tables)} CRR tables to validate (col_id_bits={col_id_bits}):")
    print(f"Running {min(len(tables), 8)} tables in parallel\n")

    all_ok = True
    results = []

    # Run validation in parallel (one connection per table)
    with ThreadPoolExecutor(max_workers=min(len(tables), 8)) as executor:
        futures = {
            executor.submit(validate_table, db_path, ext_path, table, col_id_bits): table
            for table in tables
        }
        for future in as_completed(futures):
            table = futures[future]
            try:
                result = future.result()
                results.append(result)
            except Exception as e:
                results.append((table, False, [f"exception: {e}"], {}))

    # Sort results by table name for consistent output
    results.sort(key=lambda r: r[0])

    for table, ok, errors, stats in results:
        if stats.get("skip"):
            print(f"  {table}: SKIP (no v2_pks info in crsql_master)")
            continue
        if ok:
            pk_only_str = ", pk-only" if stats.get("pk_only") else ""
            mode_str = stats.get("mode", "?")
            print(
                f"  {table}: OK [{mode_str}] "
                f"(alive={stats['alive']}, dead={stats['dead']}, "
                f"clock={stats['clock']}{pk_only_str})"
            )
        else:
            print(f"  {table}: FAIL")
            for err in errors:
                print(f"    - {err}")
            all_ok = False

    print()
    if all_ok:
        print("ALL TABLES VALIDATED SUCCESSFULLY")
    else:
        print("VALIDATION FAILED - see errors above")
        sys.exit(1)


if __name__ == "__main__":
    main()
