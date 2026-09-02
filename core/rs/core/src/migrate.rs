extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use sqlite_nostd::{sqlite3, Connection, ResultCode};

use crate::c::crsql_ExtData;
use crate::consts;
use crate::tableinfo::{TableInfo, SchemaVersion};
use core::mem;

/// Build JOIN clause for v2_pks in non-rowid migration queries.
/// skip_hash: JOIN on PK column directly. hash mode: JOIN on hashed_pk.
fn v2_pks_join_clause(tbl_info: &TableInfo, escaped: &str, pk_cols_p: &str) -> String {
    if tbl_info.skip_hash && tbl_info.key_is_rowid {
        // v2_pks has only (__crsql_key, cl) — __crsql_key = rowid.
        // JOIN base table to get rowid from PK, then JOIN v2_pks on __crsql_key = rowid.
        let rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        format!(
            "JOIN \"{escaped}\" b ON b.\"{pk_col}\" = {pk_cols_p}
             JOIN \"{escaped}{v2_pks}\" vp ON vp.__crsql_key = b.\"{rowid_alias}\"",
            escaped = escaped,
            v2_pks = consts::V2_PKS_SUFFIX,
            pk_col = tbl_info.skip_hash_pk_col,
            pk_cols_p = pk_cols_p,
            rowid_alias = rowid_alias,
        )
    } else if tbl_info.skip_hash {
        format!(
            "JOIN \"{escaped}{v2_pks}\" vp ON vp.\"{pk_col}\" = {pk_cols_p}",
            escaped = escaped,
            v2_pks = consts::V2_PKS_SUFFIX,
            pk_col = tbl_info.skip_hash_pk_col,
            pk_cols_p = pk_cols_p
        )
    } else {
        format!(
            "JOIN \"{escaped}{v2_pks}\" vp ON vp.hashed_pk = crsql_hash_pk({pk_cols_p})",
            escaped = escaped,
            v2_pks = consts::V2_PKS_SUFFIX,
            pk_cols_p = pk_cols_p
        )
    }
}

/// crsql_incremental_maintenance(chunk_size) -> INTEGER
/// Dispatches to pending maintenance tasks across all CRR tables.
/// Does up to `chunk_size` units of work per call, returns total remaining.
/// When it returns 0, all maintenance is complete.
#[no_mangle]
pub unsafe extern "C" fn crsql_incremental_maintenance(
    db: *mut sqlite3,
    chunk_size: c_int,
    ext_data: *mut crsql_ExtData,
) -> c_int {
    match incremental_maintenance(db, chunk_size, ext_data) {
        Ok(remaining) => remaining,
        Err(_) => -1,
    }
}

unsafe fn incremental_maintenance(
    db: *mut sqlite3,
    chunk_size: c_int,
    ext_data: *mut crsql_ExtData,
) -> Result<c_int, ResultCode> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { (*ext_data).timestamp } == 0 {
        crate::debug::debug_log("incremental_maintenance: timestamp not set — call crsql_set_ts() first");
        return Err(ResultCode::ERROR);
    }

    // Refresh table infos cache — new CRRs may have been created since last refresh
    let mut err: *mut c_char = core::ptr::null_mut();
    let rc = crate::tableinfo::crsql_ensure_table_infos_are_up_to_date(
        db,
        ext_data,
        &mut err as *mut *mut c_char,
    );
    if rc != ResultCode::OK as c_int {
        crate::debug::debug_log("incremental_maintenance: ensure_table_infos failed");
        return Err(ResultCode::ERROR);
    }
    crate::debug::debug_log("incremental_maintenance: table infos refreshed");

    let table_infos =
        mem::ManuallyDrop::new(Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>));
    crate::debug::debug_log(&format!("incremental_maintenance: {} table infos", table_infos.len()));

    let mut total_remaining: c_int = 0;

    // Priority 1: V1 table cleanup tasks (from v2&v1 -> v2 transition)
    process_cleanup_tasks(
        db,
        chunk_size,
        &mut total_remaining,
        "cleanup_v1_tables",
        &["__crsql_clock", "__crsql_pks"],
    )?;

    // Priority 2: V2 table cleanup tasks (from v2&v1 -> v1 rollback)
    process_cleanup_tasks(
        db,
        chunk_size,
        &mut total_remaining,
        "cleanup_v2_tables",
        &[
            consts::V2_COL_MAP_SUFFIX,
            consts::V2_CLOCK_SUFFIX,
            consts::V2_PKS_SUFFIX,
            consts::V2_TOMBSTONES_SUFFIX,
            consts::V2_TOMBSTONE_PKS_SUFFIX,
        ],
    )?;

    // Priority 3: V1→V2 migration tasks
    // Budget is shared across tables — if a table finishes with budget left over,
    // the remaining budget is used for the next table. This avoids spikes where
    // a small table wastes a large chunk_size budget.
    let mut budget: i64 = chunk_size as i64;
    for tbl_info in table_infos.iter() {
        if budget <= 0 {
            // Still need to count remaining for the return value.
            // Use cached total key (set by get_or_count in the main path) to avoid
            // full COUNT(*) scans on every maintenance call.
            if tbl_info.schema_version != SchemaVersion::V2 {
                let has_v2 = crate::bootstrap_v2::has_v2_tables(db, &tbl_info.tbl_name)?;
                if has_v2 {
                    let progress_key = format!("migration_v1_to_v2_migration_{}", tbl_info.tbl_name);
                    let progress = crate::util::get_master_value(db, &progress_key)?;
                    if progress.is_some() {
                        let total_key = format!("migration_v1_to_v2_remaining_{}", tbl_info.tbl_name);
                        let cached = crate::util::get_master_value(db, &total_key)?;
                        if let Some(v) = cached {
                            total_remaining += v as c_int;
                        } else {
                            // No cached value — fall back to COUNT(*)
                            let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
                            let start_key = progress.unwrap_or(0);
                            let count_sql = format!(
                                "SELECT count(*) FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key}\0",
                                escaped = escaped,
                                start_key = start_key,
                            );
                            let stmt = db.prepare_v2(&count_sql)?;
                            stmt.step()?;
                            total_remaining += stmt.column_int64(0) as c_int;
                        }
                    }
                }
            }
            continue;
        }

        crate::debug::debug_log(&format!("migration: checking table {} schema_version={:?}", tbl_info.tbl_name, tbl_info.schema_version));
        // Only migrate tables that have V1 tables (V1 or V2AndV1 schema)
        if tbl_info.schema_version == SchemaVersion::V2 {
            continue;
        }

        // Check if V2 tables already exist
        let has_v2 = crate::bootstrap_v2::has_v2_tables(db, &tbl_info.tbl_name)?;
        crate::debug::debug_log(&format!("migration: {} has_v2={}", tbl_info.tbl_name, has_v2));
        if !has_v2 {
            // First call for this table: create V2 tables
            match crate::bootstrap_v2::create_v2_tables(db, tbl_info) {
                Ok(_) => crate::debug::debug_log(&format!("migration: created v2 tables for {}", tbl_info.tbl_name)),
                Err(e) => {
                    crate::debug::debug_log(&format!("migration: create_v2_tables failed for {}: {:?}", tbl_info.tbl_name, e));
                    return Err(e);
                }
            }
        }

        // Migrate a chunk of rows from V1 to V2, using remaining budget
        match migrate_v1_to_v2_chunk(db, ext_data, tbl_info, budget) {
            Ok((processed, remaining)) => {
                crate::debug::debug_log(&format!("migration: {} processed={} remaining={}", tbl_info.tbl_name, processed, remaining));
                total_remaining += remaining as c_int;
                budget -= processed;
            }
            Err(e) => {
                crate::debug::debug_log(&format!("migration: FAILED for {}: {:?}", tbl_info.tbl_name, e));
                // Don't abort the entire migration — skip this table and continue with others
                // The error will be retried on the next maintenance call
            }
        }
    }

    Ok(total_remaining)
}

/// Process chunked cleanup tasks for tables registered in crsql_master.
/// Looks up keys matching `{marker_prefix}_*`, deletes rows in batches from each
/// suffixed table, and drops the tables when empty. Clears the marker when done.
unsafe fn process_cleanup_tasks(
    db: *mut sqlite3,
    chunk_size: c_int,
    total_remaining: &mut c_int,
    marker_prefix: &str,
    suffixes: &[&str],
) -> Result<(), ResultCode> {
    let like_pattern = format!("{}_%", marker_prefix);
    let stmt = db.prepare_v2("SELECT key FROM crsql_master WHERE key LIKE ?\0")?;
    stmt.bind_text(1, &like_pattern, sqlite_nostd::Destructor::TRANSIENT)?;
    let strip = format!("{}_", marker_prefix);
    let mut tables: Vec<String> = Vec::new();
    while stmt.step()? == ResultCode::ROW {
        let key = stmt.column_text(0)?;
        if let Some(tbl) = key.strip_prefix(&strip) {
            tables.push(String::from(tbl));
        }
    }
    for tbl_name in &tables {
        let remaining = cleanup_tables_chunk(db, tbl_name, chunk_size as i64, suffixes)?;
        if remaining == 0 {
            crate::util::clear_master_key(db, &format!("{}_{}", marker_prefix, tbl_name))?;
        } else {
            *total_remaining += remaining as c_int;
        }
    }
    Ok(())
}

/// Chunked table cleanup: DELETE rows in batches from each suffixed table, then
/// DROP all tables when empty. Returns estimated remaining rows across all tables.
///
/// Uses `changes64()` to track deleted rows without running `count(*)` on every call.
/// The total row count is cached in crsql_master on the first call. When a DELETE
/// removes 0 rows, the table is empty. If the estimate goes negative (possible if
/// rows were added concurrently), we re-count to correct it.
unsafe fn cleanup_tables_chunk(
    db: *mut sqlite3,
    tbl_name: &str,
    chunk_size: i64,
    suffixes: &[&str],
) -> Result<i64, ResultCode> {
    let escaped = crate::util::escape_ident(tbl_name);
    db.exec_safe("SAVEPOINT cleanup_chunk")?;

    let result = (|| {
        let mut all_empty = true;
        let mut total_deleted: i64 = 0;

        // Load or initialize cached total row count across all suffixes
        let total_key = format!("cleanup_remaining_{}", tbl_name);
        let cached_total: i64 = match crate::util::get_master_value(db, &total_key)? {
            Some(v) => v,
            None => {
                // First call: count all rows across suffixes
                let mut total: i64 = 0;
                for suffix in suffixes {
                    let count_sql = format!(
                        "SELECT count(*) FROM \"{escaped}{suffix}\"\0",
                        escaped = escaped, suffix = suffix,
                    );
                    let stmt = db.prepare_v2(&count_sql)?;
                    stmt.step()?;
                    total += stmt.column_int64(0);
                }
                crate::util::set_master_value(db, &total_key, total)?;
                total
            }
        };

        for suffix in suffixes {
            let table_name = format!("\"{escaped}{suffix}\"", escaped = escaped, suffix = suffix);
            db.exec_safe(&format!(
                "DELETE FROM {table_name} LIMIT {chunk_size}",
                table_name = table_name,
                chunk_size = chunk_size,
            ))?;
            let deleted = db.changes64();
            total_deleted += deleted;
            if deleted > 0 {
                // Deleted something → table might still have rows
                all_empty = false;
            }
        }

        // If no rows were deleted in any table, all are empty → drop them
        if all_empty {
            for suffix in suffixes {
                db.exec_safe(&format!(
                    "DROP TABLE IF EXISTS \"{escaped}{suffix}\";",
                    escaped = escaped,
                    suffix = suffix,
                ))?;
            }
            crate::util::clear_master_key(db, &total_key)?;
            return Ok(0i64);
        }

        // Estimate remaining from cached total minus cumulative deleted.
        // If negative (concurrent inserts or over-estimate), re-count to correct.
        let remaining = cached_total - total_deleted;
        if remaining <= 0 {
            // Re-count to verify
            let mut actual: i64 = 0;
            for suffix in suffixes {
                let count_sql = format!(
                    "SELECT count(*) FROM \"{escaped}{suffix}\"\0",
                    escaped = escaped, suffix = suffix,
                );
                let stmt = db.prepare_v2(&count_sql)?;
                stmt.step()?;
                actual += stmt.column_int64(0);
            }
            if actual == 0 {
                for suffix in suffixes {
                    db.exec_safe(&format!(
                        "DROP TABLE IF EXISTS \"{escaped}{suffix}\";",
                        escaped = escaped,
                        suffix = suffix,
                    ))?;
                }
                crate::util::clear_master_key(db, &total_key)?;
                return Ok(0i64);
            }
            // Correct the cache and return actual count
            crate::util::set_master_value(db, &total_key, actual + total_deleted)?;
            Ok(actual)
        } else {
            Ok(remaining)
        }
    })();

    match result {
        Ok(remaining) => {
            db.exec_safe("RELEASE cleanup_chunk")?;
            Ok(remaining)
        }
        Err(e) => {
            db.exec_safe("ROLLBACK")?;
            Err(e)
        }
    }
}

/// Migrate a chunk of V1 PK rows to V2 tables.
/// Returns the number of remaining rows to migrate.
unsafe fn migrate_v1_to_v2_chunk(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    chunk_size: i64,
) -> Result<(i64, i64), ResultCode> {
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let ts_fallback = unsafe { (*ext_data).timestamp as i64 };

    // Get progress marker from crsql_master.
    // No progress marker = table not queued for migration (either not started or already done).
    // Migration is only queued by queue_migration_tasks() during the V1→V2&V1 config change.
    let progress_key = format!("migration_v1_to_v2_migration_{}", tbl_info.tbl_name);
    let progress = crate::util::get_master_value(db, &progress_key)?;
    if progress.is_none() {
        // Not queued — skip
        return Ok((0, 0));
    }
    let start_key = progress.unwrap_or(0);

    db.exec_safe("SAVEPOINT migration_chunk")?;

    // One-time count of remaining rows to migrate (cached in crsql_master).
    let total_key = format!("migration_v1_to_v2_remaining_{}", tbl_info.tbl_name);
    let count_sql = format!(
        "SELECT count(*) FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key}\0",
        escaped = escaped,
        start_key = start_key,
    );
    let mut remaining_estimate = crate::util::get_or_count(db, &total_key, &count_sql)?;

    // Process a chunk of rows
    let (pk_cols_list, _) = crate::v2_stmts::pk_cols_and_values(&tbl_info.pks);
    let pk_cols_p_list = crate::util::as_identifier_list(&tbl_info.pks, Some("p."))?;
    // PK columns qualified with t. (backing table alias) for rowid-key tables
    let pk_cols_t_list = crate::util::as_identifier_list(&tbl_info.pks, Some("t."))?;
    // PK columns from p. with alias to column name (for temp table SELECT)
    let pk_cols_p_aliased: Vec<String> = tbl_info.pks.iter()
        .map(|c| format!("p.\"{}\" as \"{}\"", crate::util::escape_ident(&c.name), crate::util::escape_ident(&c.name)))
        .collect();
    let pk_cols_p_aliased_str = pk_cols_p_aliased.join(", ");
    // PK columns prefixed with chunk. (for reading from temp table)
    let pk_cols_chunk_list = crate::util::as_identifier_list(&tbl_info.pks, Some("chunk."))?;

    let sentinel = crate::c::INSERT_SENTINEL;
    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let key_is_rowid = tbl_info.key_is_rowid;
    let skip_hash = tbl_info.skip_hash;

    let result = (|| {        // Step 0: Select the chunk ONCE into a temp table, enriched with precomputed:
        // - PK column values (from __crsql_pks)
        // - hashed_pk (hash mode only — avoids computing it 2-3 times per dead row)
        // - sentinel clock data (col_version, site_id, db_version, seq, ts)
        // - is_alive flag (avoids re-evaluating the sentinel JOIN + WHERE in each step)
        //
        // With PK values cached, steps 1-3 no longer need to JOIN __crsql_pks at all.
        // Orphan filtering: INNER JOIN to base table in Step 1 (only for alive rows).
        // Step 4/4b use v2_pks JOIN for cell_key — orphans are filtered because v2_pks
        // only has alive rows (from Step 1).
        db.exec_safe("DROP TABLE IF EXISTS temp.migration_chunk")?;

        // Use CREATE (empty schema) + INSERT ... SELECT so changes64() reports the row count.
        // CREATE TABLE AS SELECT is DDL — changes64() does not reflect rows inserted.
        let pk_cols_schema: Vec<String> = tbl_info.pks.iter()
            .map(|c| format!("\"{}\"", crate::util::escape_ident(&c.name)))
            .collect();
        let pk_cols_schema_str = pk_cols_schema.join(", ");
        let hash_schema = if skip_hash { "" } else { ", hashed_pk BLOB" };
        db.exec_safe(&format!(
            "CREATE TEMP TABLE migration_chunk (
               __crsql_key INTEGER PRIMARY KEY,
               {pk_cols_schema},
               sentinel_version INTEGER,
               sentinel_site_id INTEGER,
               sentinel_db_version INTEGER,
               sentinel_seq INTEGER,
               sentinel_ts TEXT,
               is_alive INTEGER{hash_schema}
             )",
            pk_cols_schema = pk_cols_schema_str,
            hash_schema = hash_schema,
        ))?;
        let hash_select = if skip_hash { "".to_string() } else { format!(", crsql_hash_pk({}) as hashed_pk", pk_cols_p_list) };
        // No base table JOIN here — only alive rows need it, and that's done in Step 1.
        // Step 4 gets __crsql_key from v2_pks (populated in Step 1) for cell_key computation.
        db.exec_safe(&format!(
            "INSERT INTO temp.migration_chunk
             SELECT p.__crsql_key, {pk_cols_aliased},
               s.col_version as sentinel_version, s.site_id as sentinel_site_id,
               s.db_version as sentinel_db_version, s.seq as sentinel_seq,
               s.ts as sentinel_ts,
               CASE WHEN s.col_version IS NULL OR s.col_version % 2 != 0 THEN 1 ELSE 0 END as is_alive
               {hash_select}
             FROM \"{escaped}__crsql_pks\" p
             LEFT JOIN \"{escaped}__crsql_clock\" s
               ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
             WHERE p.__crsql_key > {start_key}
             ORDER BY p.__crsql_key LIMIT {chunk_size}",
            pk_cols_aliased = pk_cols_p_aliased_str,
            hash_select = hash_select,
            escaped = escaped,
            start_key = start_key,
            chunk_size = chunk_size,
            sentinel = sentinel,
        ))?;
        let chunk_rows = db.changes64();
        if chunk_rows == 0 {
            // No rows to migrate — done
            return Ok(0i64);
        }

        // Step 1: Batch insert alive PKs into v2_pks.
        // INNER JOIN base table for all tables:
        // - rowid tables: get b.rowid for v2_pks.__crsql_key
        // - non-rowid tables: filter orphans (only alive rows hit the base table)
        let (pks_cols, pks_select) = if skip_hash && key_is_rowid {
            (
                "__crsql_key, cl".to_string(),
                "b.rowid".to_string(),
            )
        } else if skip_hash {
            (
                format!("{}, cl", pk_cols_list),
                pk_cols_chunk_list.clone(),
            )
        } else if key_is_rowid {
            (
                "__crsql_key, hashed_pk, cl".to_string(),
                "b.rowid, chunk.hashed_pk".to_string(),
            )
        } else {
            (
                format!("{}, hashed_pk, cl", pk_cols_list),
                format!("{}, chunk.hashed_pk", pk_cols_chunk_list),
            )
        };
        // INNER JOIN base table for all tables:
        // - rowid tables: get b.rowid for v2_pks.__crsql_key
        // - non-rowid tables: filter orphans (only alive rows hit the base table)
        let join_conds: Vec<String> = tbl_info.pks.iter().map(|c| {
            format!("b.\"{col}\" = chunk.\"{col}\"", col = crate::util::escape_ident(&c.name))
        }).collect();
        let pks_join = format!("JOIN \"{escaped}\" b ON {}", join_conds.join(" AND "));
        let pks_where = "WHERE chunk.is_alive = 1";
        db.exec_safe(&format!(
            "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" ({pks_cols})
             SELECT {pks_select}, COALESCE(chunk.sentinel_version, 1)
             FROM temp.migration_chunk chunk
             {pks_join}
             {pks_where}",
            escaped = escaped,
            v2_pks = consts::V2_PKS_SUFFIX,
            pks_cols = pks_cols,
            pks_select = pks_select,
            pks_join = pks_join,
            pks_where = pks_where,
        ))?;

        // Step 2: Batch insert tombstones (dead rows).
        // Uses precomputed hashed_pk, PK values, and sentinel data from temp table.
        // No JOIN to __crsql_pks or base table needed — dead rows don't need orphan filtering.
        let (tomb_pk_col, tomb_pk_select) = if skip_hash {
            (format!("\"{}\"", tbl_info.skip_hash_pk_col), pk_cols_chunk_list.clone())
        } else {
            ("hashed_pk".to_string(), "chunk.hashed_pk".to_string())
        };
        db.exec_safe(&format!(
            "INSERT OR IGNORE INTO \"{escaped}{v2_tomb}\"
             (site_id, db_version, seq, {tomb_pk_col}, cl, ts)
             SELECT chunk.sentinel_site_id, chunk.sentinel_db_version, chunk.sentinel_seq,
               {tomb_pk_select}, chunk.sentinel_version,
               CASE WHEN CAST(chunk.sentinel_ts AS INTEGER) > 0 THEN CAST(chunk.sentinel_ts AS INTEGER) ELSE {ts_fallback} END
             FROM temp.migration_chunk chunk
             WHERE chunk.is_alive = 0",
            escaped = escaped,
            v2_tomb = consts::V2_TOMBSTONES_SUFFIX,
            tomb_pk_col = tomb_pk_col,
            tomb_pk_select = tomb_pk_select,
            ts_fallback = ts_fallback,
        ))?;

        // Step 3: Batch insert tombstone PKs (hash mode only — skip_hash stores PK directly in tombstone)
        // Uses precomputed hashed_pk and PK values from temp table.
        if !skip_hash {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_tomb_pks}\" ({pk_cols}, hashed_pk)
                 SELECT {pk_cols_chunk}, chunk.hashed_pk
                 FROM temp.migration_chunk chunk
                 WHERE chunk.is_alive = 0",
                escaped = escaped,
                v2_tomb_pks = consts::V2_TOMBSTONE_PKS_SUFFIX,
                pk_cols = pk_cols_list,
                pk_cols_chunk = pk_cols_chunk_list,
            ))?;
        }

        // Step 4: Batch migrate clock entries — INNER JOIN on pks chunk + v2_col_map.
        // Step 4b: For PK-only tables, migrate V1 sentinel clock entries to V2 sentinel at col_id=0.
        // Both steps share the same structure; only the col_id source, col_version, WHERE, and
        // the v2_col_map JOIN differ. We build conditionally and run both in a loop.
        // All tables: JOIN v2_pks to get __crsql_key for cell_key computation.
        // For rowid tables, v2_pks.__crsql_key IS the rowid (set in Step 1).
        // For non-rowid tables, v2_pks.__crsql_key is the auto-assigned key.
        let v2_pks_join = v2_pks_join_clause(tbl_info, &escaped, &pk_cols_chunk_list);
        let cell_key_base = format!("(vp.__crsql_key << {col_id_bits})", col_id_bits = col_id_bits);
        let base_join = v2_pks_join;
        // Step 4a: non-sentinel clock entries (only for tables with non-PK columns).
        // PK-only tables have no non-sentinel clock entries — skip this entirely.
        if !tbl_info.non_pks.is_empty() {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_clock}\"
                 (cell_key, col_version, site_id, db_version, seq, ts)
                 SELECT {cell_key_base} | m.col_id,
                   c.col_version, c.site_id, c.db_version, c.seq,
                   CASE WHEN CAST(c.ts AS INTEGER) > 0 THEN CAST(c.ts AS INTEGER) ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_clock\" c
                 JOIN temp.migration_chunk chunk
                   ON c.key = chunk.__crsql_key
                 {base_join}
                 JOIN \"{escaped}{v2_col_map}\" m ON c.col_name = m.col_name
                 WHERE c.col_name != '{sentinel}' AND chunk.is_alive = 1",
                escaped = escaped,
                v2_clock = consts::V2_CLOCK_SUFFIX,
                v2_col_map = consts::V2_COL_MAP_SUFFIX,
                cell_key_base = cell_key_base,
                base_join = base_join,
                sentinel = sentinel,
                ts_fallback = ts_fallback,
            ))?;
        }

        // Step 4b: For PK-only tables, migrate/create sentinel clock entries at col_id=0.
        // The sentinel is the only clock entry for PK-only tables.
        // v2_pks JOIN (base_join) filters orphans — only alive rows were inserted in Step 1.
        if tbl_info.non_pks.is_empty() {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_clock}\"
                 (cell_key, col_version, site_id, db_version, seq, ts)
                 SELECT {cell_key_base} | 0,
                   COALESCE(chunk.sentinel_version, 1), COALESCE(chunk.sentinel_site_id, 0),
                   COALESCE(chunk.sentinel_db_version, 0), COALESCE(chunk.sentinel_seq, 0),
                   CASE WHEN CAST(COALESCE(chunk.sentinel_ts, '0') AS INTEGER) > 0 THEN CAST(chunk.sentinel_ts AS INTEGER) ELSE {ts_fallback} END
                 FROM temp.migration_chunk chunk
                 {base_join}
                 WHERE chunk.is_alive = 1",
                escaped = escaped,
                v2_clock = consts::V2_CLOCK_SUFFIX,
                cell_key_base = cell_key_base,
                base_join = base_join,
                ts_fallback = ts_fallback,
            ))?;
        }

        // Step 5: Get max key from chunk to update cursor.
        // Chunk is guaranteed non-empty (checked above via changes64()).
        let max_key_sql = "SELECT max(__crsql_key) FROM temp.migration_chunk\0";
        let max_key_stmt = db.prepare_v2(&max_key_sql)?;
        max_key_stmt.step()?;
        let last_key = max_key_stmt.column_int64(0);
        drop(max_key_stmt);

        // Update progress marker to last key processed
        crate::util::set_master_value(db, &progress_key, last_key)?;
        // Use chunk_rows (total __crsql_pks rows processed, alive + dead) as processed count.
        // This accurately reflects how many rows we consumed from the cursor.
        // Return (processed, remaining) — remaining is computed in the match below.
        Ok(chunk_rows)
    })();
    // result is Ok(processed) from the closure — the match below wraps it into (processed, remaining)

    // Clean up temp table
    let _ = db.exec_safe("DROP TABLE IF EXISTS temp.migration_chunk");

    match result {
        Ok(processed) => {
            remaining_estimate -= processed;
            crate::debug::debug_log(&format!("migrate_chunk: processed={} remaining_estimate={}", processed, remaining_estimate));
            if processed == 0 {
                // Chunk was empty — migration complete for this table.
                // Backfill v2_pks for untracked rows: base table rows that have no
                // __crsql_pks entry (and thus no v2_pks entry after migration).
                // These are inserted with CL=1 (alive, first version).
                backfill_untracked_v2_pks(db, tbl_info, &escaped)?;
                crate::util::clear_master_key(db, &progress_key)?;
                crate::util::clear_master_key(db, &total_key)?;
                db.exec_safe("RELEASE migration_chunk")?;
                Ok((0, 0))
            } else if remaining_estimate <= 0 {
                // Estimate exhausted — re-count to verify (may have been over-count due to orphans/IGNORE).
                // Clear cached total first so get_or_count actually runs the count query.
                crate::util::clear_master_key(db, &total_key)?;
                let verify_sql = format!(
                    "SELECT count(*) FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {last_key}\0",
                    escaped = escaped,
                    last_key = crate::util::get_master_value(db, &progress_key)?.unwrap_or(0),
                );
                // Run count directly — do NOT cache via get_or_count, since the cached
                // value would be stale on the next call (start_key will have advanced).
                let verify_stmt = db.prepare_v2(&verify_sql)?;
                verify_stmt.step()?;
                let actual = verify_stmt.column_int64(0);
                drop(verify_stmt);
                if actual == 0 {
                    // Migration complete — backfill untracked rows
                    backfill_untracked_v2_pks(db, tbl_info, &escaped)?;
                    crate::util::clear_master_key(db, &progress_key)?;
                    crate::util::clear_master_key(db, &total_key)?;
                    db.exec_safe("RELEASE migration_chunk")?;
                    Ok((processed, 0))
                } else {
                    // Do NOT cache — the next call will count with the new start_key via get_or_count.
                    // total_key was already cleared above, so get_or_count will run the count query.
                    db.exec_safe("RELEASE migration_chunk")?;
                    Ok((processed, actual))
                }
            } else {
                // Update cached estimate
                crate::util::set_master_value(db, &total_key, remaining_estimate)?;
                db.exec_safe("RELEASE migration_chunk")?;
                crate::debug::debug_log(&format!("migrate_chunk: returning remaining={}", remaining_estimate));
                Ok((processed, remaining_estimate))
            }
        }
        Err(e) => {
            let errmsg = match db.errmsg() {
                Ok(s) => s,
                Err(_) => alloc::string::String::from("unknown"),
            };
            crate::debug::debug_log(&format!("migrate_v1_to_v2_chunk FAILED: {:?} errmsg={}", e, errmsg));
            // Rollback the savepoint — ignore errors in case the savepoint is already gone
            let _ = db.exec_safe("ROLLBACK TO migration_chunk");
            let _ = db.exec_safe("RELEASE migration_chunk");
            Err(e)
        }
    }
}

/// Backfill v2_pks entries for untracked rows: base table rows that have no
/// v2_pks entry (because they had no __crsql_pks entry in V1). Inserts them
/// with CL=1 (alive, first version). This maintains the invariant that every
/// base table row has a v2_pks entry.
///
/// Uses INSERT OR IGNORE so rows already migrated from V1 are skipped.
/// For rowid-key tables, __crsql_key = rowid. For non-rowid tables, the
/// auto-increment __crsql_key is assigned by the INSERT.
/// For hash mode, hashed_pk is computed from the PK columns.
unsafe fn backfill_untracked_v2_pks(
    db: *mut sqlite3,
    tbl_info: &TableInfo,
    escaped: &str,
) -> Result<(), ResultCode> {
    let (pk_cols_list, _) = crate::v2_stmts::pk_cols_and_values(&tbl_info.pks);
    let pk_cols_base = crate::util::as_identifier_list(&tbl_info.pks, Some("b."))?;

    let (pks_cols, pks_select) = if tbl_info.skip_hash && tbl_info.key_is_rowid {
        ("__crsql_key, cl".to_string(), "b.rowid, 1".to_string())
    } else if tbl_info.skip_hash {
        (format!("{}, cl", pk_cols_list), format!("{}, 1", pk_cols_base))
    } else if tbl_info.key_is_rowid {
        (
            "__crsql_key, hashed_pk, cl".to_string(),
            format!("b.rowid, crsql_hash_pk({}), 1", pk_cols_base),
        )
    } else {
        (
            format!("{}, hashed_pk, cl", pk_cols_list),
            format!("{}, crsql_hash_pk({}), 1", pk_cols_base, pk_cols_base),
        )
    };

    let where_clause = if tbl_info.skip_hash && tbl_info.key_is_rowid {
        // Use rowid_alias instead of b.rowid to handle the edge case where
        // a column named "rowid" shadows the built-in alias.
        let rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        format!("vp.__crsql_key = b.\"{}\"", rowid_alias)
    } else if tbl_info.skip_hash {
        format!(
            "vp.\"{}\" = b.\"{}\"",
            crate::util::escape_ident(&tbl_info.skip_hash_pk_col),
            crate::util::escape_ident(&tbl_info.skip_hash_pk_col)
        )
    } else {
        format!("vp.hashed_pk = crsql_hash_pk({})", pk_cols_base)
    };

    let sql = format!(
        "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" ({pks_cols})
         SELECT {pks_select}
         FROM \"{escaped}\" b
         WHERE NOT EXISTS (
           SELECT 1 FROM \"{escaped}{v2_pks}\" vp
           WHERE {where_clause}
         )",
        escaped = escaped,
        v2_pks = consts::V2_PKS_SUFFIX,
        pks_cols = pks_cols,
        pks_select = pks_select,
        where_clause = where_clause,
    );

    db.exec_safe(&sql)?;
    let inserted = db.changes64();
    if inserted > 0 {
        crate::debug::debug_log(&format!(
            "backfill_untracked_v2_pks: {} inserted {} untracked rows",
            tbl_info.tbl_name, inserted
        ));
    }
    Ok(())
}
