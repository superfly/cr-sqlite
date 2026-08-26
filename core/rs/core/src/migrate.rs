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
    if tbl_info.skip_hash {
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
    for tbl_info in table_infos.iter() {
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

        // Migrate a chunk of rows from V1 to V2
        match migrate_v1_to_v2_chunk(db, ext_data, tbl_info, chunk_size as i64) {
            Ok(remaining) => {
                crate::debug::debug_log(&format!("migration: {} remaining={}", tbl_info.tbl_name, remaining));
                total_remaining += remaining as c_int;
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
        let total_key = format!("cleanup_total_{}", tbl_name);
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
) -> Result<i64, ResultCode> {
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let ts_fallback = unsafe { (*ext_data).timestamp as i64 };

    db.exec_safe("SAVEPOINT migration_chunk")?;

    // Get progress marker from crsql_master
    let progress_key = format!("migration_v1_to_v2_migration_{}", tbl_info.tbl_name);
    let progress = crate::util::get_master_value(db, &progress_key)?;
    let start_key = progress.unwrap_or(0);

    // One-time count of remaining rows to migrate (cached in crsql_master).
    // Counts only unprocessed rows (key > start_key) so resume from interruption is correct.
    let total_key = format!("migration_v1_to_v2_total_{}", tbl_info.tbl_name);
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

    let sentinel = crate::c::INSERT_SENTINEL;
    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let key_is_rowid = tbl_info.key_is_rowid;
    let skip_hash = tbl_info.skip_hash;

    // Build PK join condition between pks table and backing table.
    // The backing table is aliased as `t` in rowid-key queries and un-aliased in non-rowid queries.
    // For rowid-key tables: use t."col" = p."col"
    // For non-rowid tables: use "{escaped}"."col" = p."col"
    let pk_join_conds: Vec<String> = tbl_info.pks.iter().map(|c| {
        if key_is_rowid {
            format!("t.\"{col}\" = p.\"{col}\"", col = crate::util::escape_ident(&c.name))
        } else {
            format!("\"{escaped}\".\"{col}\" = p.\"{col}\"", escaped = escaped, col = crate::util::escape_ident(&c.name))
        }
    }).collect();
    let pk_join_cond = pk_join_conds.join(" AND ");

    let result = (|| {
        // Step 1: Batch insert alive PKs into v2_pks.
        // Build column list and SELECT expressions conditionally based on skip_hash / key_is_rowid.
        let rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        let (pks_cols, pks_select) = if skip_hash && key_is_rowid {
            (
                "__crsql_key, cl".to_string(),
                format!("t.\"{}\"", rowid_alias),
            )
        } else if skip_hash {
            (
                format!("{}, cl", pk_cols_list),
                pk_cols_p_list.clone(),
            )
        } else if key_is_rowid {
            (
                "__crsql_key, hashed_pk, cl".to_string(),
                format!("t.\"{}\", crsql_hash_pk({})", rowid_alias, pk_cols_t_list),
            )
        } else {
            (
                format!("{}, hashed_pk, cl", pk_cols_list),
                format!("{}, crsql_hash_pk({})", pk_cols_p_list, pk_cols_p_list),
            )
        };
        // Backing table JOIN: aliased as `t` for rowid-key, un-aliased for non-rowid.
        let pks_join = if key_is_rowid {
            format!("JOIN \"{escaped}\" t ON {pk_join_cond}", escaped = escaped, pk_join_cond = pk_join_cond)
        } else {
            format!("JOIN \"{escaped}\" ON {pk_join_cond}", escaped = escaped, pk_join_cond = pk_join_cond)
        };
        db.exec_safe(&format!(
            "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" ({pks_cols})
             SELECT {pks_select}, COALESCE(s.col_version, 1)
             FROM \"{escaped}__crsql_pks\" p
             JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
               ON p.__crsql_key = chunk.__crsql_key
             {pks_join}
             LEFT JOIN \"{escaped}__crsql_clock\" s
               ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
             WHERE s.col_version IS NULL OR s.col_version % 2 != 0",
            escaped = escaped,
            v2_pks = consts::V2_PKS_SUFFIX,
            pks_cols = pks_cols,
            pks_select = pks_select,
            pks_join = pks_join,
            sentinel = sentinel,
            start_key = start_key,
            chunk_size = chunk_size,
        ))?;
        // Capture actual rows inserted into v2_pks (Step 1) for progress tracking.
        // Subsequent steps (tombstones, clock) may insert different row counts.
        let pks_inserted = db.changes64();

        // Step 2: Batch insert tombstones (dead rows).
        // skip_hash: PK column replaces hashed_pk. No v2_tombstone_pks needed.
        let (tomb_pk_col, tomb_pk_select) = if skip_hash {
            (format!("\"{}\"", tbl_info.skip_hash_pk_col), pk_cols_p_list.clone())
        } else {
            ("hashed_pk".to_string(), format!("crsql_hash_pk({})", pk_cols_p_list))
        };
        db.exec_safe(&format!(
            "INSERT OR REPLACE INTO \"{escaped}{v2_tomb}\"
             (site_id, db_version, seq, {tomb_pk_col}, cl, ts)
             SELECT s.site_id, s.db_version, s.seq, {tomb_pk_select}, s.col_version,
               CASE WHEN s.ts > 0 THEN s.ts ELSE {ts_fallback} END
             FROM \"{escaped}__crsql_pks\" p
             JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
               ON p.__crsql_key = chunk.__crsql_key
             JOIN \"{escaped}__crsql_clock\" s
               ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
             WHERE s.col_version % 2 = 0",
            escaped = escaped,
            v2_tomb = consts::V2_TOMBSTONES_SUFFIX,
            tomb_pk_col = tomb_pk_col,
            tomb_pk_select = tomb_pk_select,
            sentinel = sentinel,
            start_key = start_key,
            chunk_size = chunk_size,
            ts_fallback = ts_fallback,
        ))?;

        // Step 3: Batch insert tombstone PKs (hash mode only — skip_hash stores PK directly in tombstone)
        if !skip_hash {
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_tomb_pks}\" ({pk_cols}, hashed_pk)
                 SELECT {pk_cols_p}, crsql_hash_pk({pk_cols_p})
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version % 2 = 0",
                escaped = escaped,
                v2_tomb_pks = consts::V2_TOMBSTONE_PKS_SUFFIX,
                pk_cols = pk_cols_list,
                pk_cols_p = pk_cols_p_list,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
            ))?;
        }

        // Step 4: Batch migrate clock entries — INNER JOIN on pks chunk + v2_col_map.
        // Step 4b: For PK-only tables, migrate V1 sentinel clock entries to V2 sentinel at col_id=0.
        // Both steps share the same structure; only the col_id source, col_version, WHERE, and
        // the v2_col_map JOIN differ. We build conditionally and run both in a loop.
        let clock_joins = if key_is_rowid {
            let base_join = format!(
                "JOIN \"{escaped}\" t ON {pk_join_cond}",
                escaped = escaped, pk_join_cond = pk_join_cond
            );
            let cell_key_base = format!("(t.\"{}\" << {col_id_bits})", rowid_alias, col_id_bits = col_id_bits);
            (cell_key_base, base_join)
        } else {
            let v2_pks_join = v2_pks_join_clause(tbl_info, &escaped, &pk_cols_p_list);
            let cell_key_base = format!("(vp.__crsql_key << {col_id_bits})", col_id_bits = col_id_bits);
            (cell_key_base, v2_pks_join)
        };
        let (ref cell_key_base, ref base_join) = clock_joins;

        // Step 4: non-sentinel clock entries (join v2_col_map for col_id)
        db.exec_safe(&format!(
            "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
             (cell_key, col_version, site_id, db_version, seq, ts)
             SELECT {cell_key_base} | m.col_id,
               c.col_version, c.site_id, c.db_version, c.seq,
               CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
             FROM \"{escaped}__crsql_clock\" c
             JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
               ON c.key = chunk.__crsql_key
             JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
             {base_join}
             JOIN \"{escaped}{v2_col_map}\" m ON c.col_name = m.col_name
             WHERE c.col_name != '{sentinel}'",
            escaped = escaped,
            v2_clock = consts::V2_CLOCK_SUFFIX,
            v2_col_map = consts::V2_COL_MAP_SUFFIX,
            cell_key_base = cell_key_base,
            base_join = base_join,
            sentinel = sentinel,
            start_key = start_key,
            chunk_size = chunk_size,
            ts_fallback = ts_fallback,
        ))?;

        // Step 4b: For PK-only tables, migrate sentinel clock entries at col_id=0.
        // The normal clock migration (step 4) skips sentinels. For PK-only tables, the sentinel
        // is the only clock entry, so we need to migrate it separately.
        if tbl_info.non_pks.is_empty() {
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
                 (cell_key, col_version, site_id, db_version, seq, ts)
                 SELECT {cell_key_base} | 0,
                   1, c.site_id, c.db_version, c.seq,
                   CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_clock\" c
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON c.key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
                 {base_join}
                 WHERE c.col_name = '{sentinel}'",
                escaped = escaped,
                v2_clock = consts::V2_CLOCK_SUFFIX,
                cell_key_base = cell_key_base,
                base_join = base_join,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
                ts_fallback = ts_fallback,
            ))?;
        }

        // Step 5: Get max key from chunk to update cursor.
        // NULL means the chunk was empty — migration is done.
        let max_key_sql = format!(
            "SELECT max(__crsql_key) FROM (
                SELECT __crsql_key FROM \"{escaped}__crsql_pks\"
                WHERE __crsql_key > {start_key}
                ORDER BY __crsql_key
                LIMIT {chunk_size}
            )\0",
            escaped = escaped,
            start_key = start_key,
            chunk_size = chunk_size,
        );
        let max_key_stmt = db.prepare_v2(&max_key_sql)?;
        max_key_stmt.step()?;
        let last_key = max_key_stmt.column_int64(0);

        if last_key == 0 {
            // Chunk was empty — migration complete
            Ok(0i64)
        } else {
            // Update progress marker to last key processed
            crate::util::set_master_value(db, &progress_key, last_key)?;
            // Use actual rows inserted into v2_pks (Step 1) as processed count.
            // This is more accurate than chunk_size which over-counts due to orphans/IGNORE.
            Ok(pks_inserted)
        }
    })();

    match result {
        Ok(processed) => {
            remaining_estimate -= processed;
            crate::debug::debug_log(&format!("migrate_chunk: processed={} remaining_estimate={}", processed, remaining_estimate));
            if processed == 0 {
                // Chunk was empty — migration complete for this table
                crate::util::clear_master_key(db, &progress_key)?;
                crate::util::clear_master_key(db, &total_key)?;
                db.exec_safe("RELEASE migration_chunk")?;
                Ok(0)
            } else if remaining_estimate <= 0 {
                // Estimate exhausted — re-count to verify (may have been over-count due to orphans/IGNORE).
                // Clear cached total first so get_or_count actually runs the count query.
                crate::util::clear_master_key(db, &total_key)?;
                let verify_sql = format!(
                    "SELECT count(*) FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {last_key}\0",
                    escaped = escaped,
                    last_key = crate::util::get_master_value(db, &progress_key)?.unwrap_or(0),
                );
                let actual = crate::util::get_or_count(db, &total_key, &verify_sql)?;
                if actual == 0 {
                    crate::util::clear_master_key(db, &progress_key)?;
                    crate::util::clear_master_key(db, &total_key)?;
                    db.exec_safe("RELEASE migration_chunk")?;
                    Ok(0)
                } else {
                    db.exec_safe("RELEASE migration_chunk")?;
                    Ok(actual)
                }
            } else {
                // Update cached estimate
                crate::util::set_master_value(db, &total_key, remaining_estimate)?;
                db.exec_safe("RELEASE migration_chunk")?;
                crate::debug::debug_log(&format!("migrate_chunk: returning remaining={}", remaining_estimate));
                Ok(remaining_estimate)
            }
        }
        Err(e) => {
            let errmsg = match db.errmsg() {
                Ok(s) => s,
                Err(_) => alloc::string::String::from("unknown"),
            };
            crate::debug::debug_log(&format!("migrate_v1_to_v2_chunk FAILED: {:?} errmsg={}", e, errmsg));
            db.exec_safe("ROLLBACK")?;
            Err(e)
        }
    }
}
