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
    let cleanup_v1_sql = "SELECT key FROM crsql_master WHERE key LIKE 'cleanup_v1_tables_%'\0";
    let cleanup_stmt = db.prepare_v2(cleanup_v1_sql)?;
    let mut v1_cleanup_tables: Vec<String> = Vec::new();
    while cleanup_stmt.step()? == ResultCode::ROW {
        let key = cleanup_stmt.column_text(0)?;
        if let Some(tbl) = key.strip_prefix("cleanup_v1_tables_") {
            v1_cleanup_tables.push(String::from(tbl));
        }
    }
    for tbl_name in &v1_cleanup_tables {
        let remaining = cleanup_v1_tables_chunk(db, tbl_name, chunk_size as i64)?;
        if remaining == 0 {
            clear_cleanup_marker(db, &format!("cleanup_v1_tables_{}", tbl_name))?;
        } else {
            total_remaining += remaining as c_int;
        }
    }

    // Priority 2: V2 table cleanup tasks (from v2&v1 -> v1 rollback)
    let cleanup_v2_sql = "SELECT key FROM crsql_master WHERE key LIKE 'cleanup_v2_tables_%'\0";
    let cleanup_stmt = db.prepare_v2(cleanup_v2_sql)?;
    let mut v2_cleanup_tables: Vec<String> = Vec::new();
    while cleanup_stmt.step()? == ResultCode::ROW {
        let key = cleanup_stmt.column_text(0)?;
        if let Some(tbl) = key.strip_prefix("cleanup_v2_tables_") {
            v2_cleanup_tables.push(String::from(tbl));
        }
    }
    for tbl_name in &v2_cleanup_tables {
        let remaining = cleanup_v2_tables_chunk(db, tbl_name, chunk_size as i64)?;
        if remaining == 0 {
            clear_cleanup_marker(db, &format!("cleanup_v2_tables_{}", tbl_name))?;
        } else {
            total_remaining += remaining as c_int;
        }
    }

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
        let remaining = migrate_v1_to_v2_chunk(db, ext_data, tbl_info, chunk_size as i64)?;
        crate::debug::debug_log(&format!("migration: {} remaining={}", tbl_info.tbl_name, remaining));
        total_remaining += remaining as c_int;
    }

    Ok(total_remaining)
}

/// Chunked V1 table cleanup: DELETE rows in batches, then DROP empty tables.
/// Returns estimated remaining rows across both tables.
unsafe fn cleanup_v1_tables_chunk(
    db: *mut sqlite3,
    tbl_name: &str,
    chunk_size: i64,
) -> Result<i64, ResultCode> {
    let escaped = crate::util::escape_ident(tbl_name);
    db.exec_safe("SAVEPOINT cleanup_v1_chunk")?;

    let result = (|| {
        // Delete a chunk from __crsql_clock
        db.exec_safe(&format!(
            "DELETE FROM \"{escaped}__crsql_clock\" LIMIT {chunk_size}",
            escaped = escaped,
            chunk_size = chunk_size,
        ))?;
        let clock_remaining = count_rows(db, &format!("\"{escaped}__crsql_clock\"", escaped = escaped))?;

        // Delete a chunk from __crsql_pks
        db.exec_safe(&format!(
            "DELETE FROM \"{escaped}__crsql_pks\" LIMIT {chunk_size}",
            escaped = escaped,
            chunk_size = chunk_size,
        ))?;
        let pks_remaining = count_rows(db, &format!("\"{escaped}__crsql_pks\"", escaped = escaped))?;

        // When both empty, drop the tables
        if clock_remaining == 0 && pks_remaining == 0 {
            db.exec_safe(&format!(
                "DROP TABLE IF EXISTS \"{escaped}__crsql_clock\";\
                 DROP TABLE IF EXISTS \"{escaped}__crsql_pks\";",
                escaped = escaped,
            ))?;
        }

        Ok(clock_remaining + pks_remaining)
    })();

    match result {
        Ok(remaining) => {
            db.exec_safe("RELEASE cleanup_v1_chunk")?;
            Ok(remaining)
        }
        Err(e) => {
            db.exec_safe("ROLLBACK")?;
            Err(e)
        }
    }
}

/// Chunked V2 table cleanup: DELETE rows in batches, then DROP empty tables.
/// Returns estimated remaining rows across all V2 tables.
unsafe fn cleanup_v2_tables_chunk(
    db: *mut sqlite3,
    tbl_name: &str,
    chunk_size: i64,
) -> Result<i64, ResultCode> {
    let escaped = crate::util::escape_ident(tbl_name);
    db.exec_safe("SAVEPOINT cleanup_v2_chunk")?;

    let v2_suffixes = [
        consts::V2_COL_MAP_SUFFIX,
        consts::V2_CLOCK_SUFFIX,
        consts::V2_PKS_SUFFIX,
        consts::V2_TOMBSTONES_SUFFIX,
        consts::V2_TOMBSTONE_PKS_SUFFIX,
    ];

    let result = (|| {
        let mut total_remaining: i64 = 0;
        let mut all_empty = true;

        for suffix in &v2_suffixes {
            let table_name = format!("\"{escaped}{suffix}\"", escaped = escaped, suffix = suffix);
            db.exec_safe(&format!(
                "DELETE FROM {table_name} LIMIT {chunk_size}",
                table_name = table_name,
                chunk_size = chunk_size,
            ))?;
            let remaining = count_rows(db, &table_name)?;
            total_remaining += remaining;
            if remaining > 0 {
                all_empty = false;
            }
        }

        // When all empty, drop the tables
        if all_empty {
            for suffix in &v2_suffixes {
                db.exec_safe(&format!(
                    "DROP TABLE IF EXISTS \"{escaped}{suffix}\";",
                    escaped = escaped,
                    suffix = suffix,
                ))?;
            }
        }

        Ok(total_remaining)
    })();

    match result {
        Ok(remaining) => {
            db.exec_safe("RELEASE cleanup_v2_chunk")?;
            Ok(remaining)
        }
        Err(e) => {
            db.exec_safe("ROLLBACK")?;
            Err(e)
        }
    }
}

/// Count rows in a table (quick check).
unsafe fn count_rows(db: *mut sqlite3, table_name: &str) -> Result<i64, ResultCode> {
    let sql = format!("SELECT count(*) FROM {table_name}\0", table_name = table_name);
    let stmt = db.prepare_v2(&sql)?;
    stmt.step()?;
    Ok(stmt.column_int64(0))
}

/// Clear a cleanup marker from crsql_master by exact key.
unsafe fn clear_cleanup_marker(db: *mut sqlite3, key: &str) -> Result<(), ResultCode> {
    let sql = "DELETE FROM crsql_master WHERE key = ?\0";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, sqlite_nostd::Destructor::TRANSIENT)?;
    stmt.step()?;
    Ok(())
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
    let progress = get_progress_marker(db, &tbl_info.tbl_name, "v1_to_v2_migration")?;
    let start_key = progress.unwrap_or(0);

    // One-time count of total rows to migrate (stored for estimate)
    let total_key = format!("migration_v1_to_v2_total_{}\0", tbl_info.tbl_name);
    let total: i64 = {
        let stmt = db.prepare_v2("SELECT value FROM crsql_master WHERE key = ?\0")?;
        stmt.bind_text(1, &total_key, sqlite_nostd::Destructor::TRANSIENT)?;
        if stmt.step()? == ResultCode::ROW {
            stmt.column_int64(0)
        } else {
            // First call: count total rows
            let count_sql = format!(
                "SELECT count(*) FROM \"{escaped}__crsql_pks\"\0",
                escaped = escaped,
            );
            let count_stmt = db.prepare_v2(&count_sql)?;
            count_stmt.step()?;
            let total = count_stmt.column_int64(0);
            // Store it
            let insert_sql = "INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, ?)\0";
            let insert_stmt = db.prepare_v2(insert_sql)?;
            insert_stmt.bind_text(1, &total_key, sqlite_nostd::Destructor::TRANSIENT)?;
            insert_stmt.bind_int64(2, total)?;
            insert_stmt.step()?;
            total
        }
    };

    // Track cumulative processed count for estimate
    let done_key = format!("migration_v1_to_v2_done_{}\0", tbl_info.tbl_name);
    let mut cumulative_done: i64 = {
        let stmt = db.prepare_v2("SELECT value FROM crsql_master WHERE key = ?\0")?;
        stmt.bind_text(1, &done_key, sqlite_nostd::Destructor::TRANSIENT)?;
        if stmt.step()? == ResultCode::ROW {
            stmt.column_int64(0)
        } else {
            0
        }
    };

    // Process a chunk of rows
    let pk_cols: Vec<String> = tbl_info.pks.iter().map(|c| format!("\"{}\"", crate::util::escape_ident(&c.name))).collect();
    let pk_cols_list = pk_cols.join(", ");
    let pk_cols_no_quotes: Vec<String> = tbl_info.pks.iter().map(|c| format!("p.\"{}\"", crate::util::escape_ident(&c.name))).collect();
    let pk_cols_p_list = pk_cols_no_quotes.join(", ");
    // PK columns qualified with t. (backing table alias) for rowid-key tables
    let pk_cols_t: Vec<String> = tbl_info.pks.iter().map(|c| format!("t.\"{}\"", crate::util::escape_ident(&c.name))).collect();
    let pk_cols_t_list = pk_cols_t.join(", ");

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
        // Step 1: Batch insert alive PKs into v2_pks
        // skip_hash mode: no hashed_pk column. PK value stored directly (non-rowid) or as __crsql_key (rowid).
        if skip_hash && key_is_rowid {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" (__crsql_key, cl)
                 SELECT t.\"{rowid_alias}\",
                   CASE WHEN s.col_version IS NULL OR s.col_version % 2 != 0 THEN
                     CASE WHEN s.col_version IS NULL THEN 1 ELSE s.col_version END
                   ELSE 1 END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}\" t ON {pk_join_cond}
                 LEFT JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version IS NULL OR s.col_version % 2 != 0",
                escaped = escaped,
                v2_pks = consts::V2_PKS_SUFFIX,
                rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias),
                pk_join_cond = pk_join_cond,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
            ))?;
        } else if skip_hash && !key_is_rowid {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" ({pk_cols}, cl)
                 SELECT {pk_cols_p},
                   CASE WHEN s.col_version IS NULL OR s.col_version % 2 != 0 THEN
                     CASE WHEN s.col_version IS NULL THEN 1 ELSE s.col_version END
                   ELSE 1 END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}\" ON {pk_join_cond}
                 LEFT JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version IS NULL OR s.col_version % 2 != 0",
                escaped = escaped,
                v2_pks = consts::V2_PKS_SUFFIX,
                pk_cols = pk_cols_list,
                pk_cols_p = pk_cols_p_list,
                pk_join_cond = pk_join_cond,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
            ))?;
        } else if key_is_rowid {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" (__crsql_key, hashed_pk, cl)
                 SELECT t.\"{rowid_alias}\", crsql_hash_pk({pk_cols_t}),
                   CASE WHEN s.col_version IS NULL OR s.col_version % 2 != 0 THEN
                     CASE WHEN s.col_version IS NULL THEN 1 ELSE s.col_version END
                   ELSE 1 END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}\" t ON {pk_join_cond}
                 LEFT JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version IS NULL OR s.col_version % 2 != 0",
                escaped = escaped,
                v2_pks = consts::V2_PKS_SUFFIX,
                rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias),
                pk_cols_t = pk_cols_t_list,
                pk_join_cond = pk_join_cond,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
            ))?;
        } else {
            db.exec_safe(&format!(
                "INSERT OR IGNORE INTO \"{escaped}{v2_pks}\" ({pk_cols}, hashed_pk, cl)
                 SELECT {pk_cols_p}, crsql_hash_pk({pk_cols_p}),
                   CASE WHEN s.col_version IS NULL OR s.col_version % 2 != 0 THEN
                     CASE WHEN s.col_version IS NULL THEN 1 ELSE s.col_version END
                   ELSE 1 END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}\" ON {pk_join_cond}
                 LEFT JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version IS NULL OR s.col_version % 2 != 0",
                escaped = escaped,
                v2_pks = consts::V2_PKS_SUFFIX,
                pk_cols = pk_cols_list,
                pk_cols_p = pk_cols_p_list,
                pk_join_cond = pk_join_cond,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
            ))?;
        }

        // Step 2: Batch insert tombstones (dead rows)
        // skip_hash: PK column replaces hashed_pk. No v2_tombstone_pks needed.
        if skip_hash {
            let pk_col = &tbl_info.skip_hash_pk_col;
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_tomb}\"
                 (site_id, db_version, seq, \"{pk_col}\", cl, ts)
                 SELECT s.site_id, s.db_version, s.seq, {pk_cols_p}, s.col_version,
                   CASE WHEN s.ts > 0 THEN s.ts ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version % 2 = 0",
                escaped = escaped,
                v2_tomb = consts::V2_TOMBSTONES_SUFFIX,
                pk_col = pk_col,
                pk_cols_p = pk_cols_p_list,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
                ts_fallback = ts_fallback,
            ))?;
        } else {
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_tomb}\"
                 (site_id, db_version, seq, hashed_pk, cl, ts)
                 SELECT s.site_id, s.db_version, s.seq, crsql_hash_pk({pk_cols_p}), s.col_version,
                   CASE WHEN s.ts > 0 THEN s.ts ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_pks\" p
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON p.__crsql_key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_clock\" s
                   ON p.__crsql_key = s.key AND s.col_name = '{sentinel}'
                 WHERE s.col_version % 2 = 0",
                escaped = escaped,
                v2_tomb = consts::V2_TOMBSTONES_SUFFIX,
                pk_cols_p = pk_cols_p_list,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
                ts_fallback = ts_fallback,
            ))?;
        }

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

        // Step 4: Batch migrate clock entries — INNER JOIN on pks chunk + v2_col_map
        // For rowid-key tables: cell_key uses t.<rowid_alias> instead of c.key
        if key_is_rowid {
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
                 (cell_key, col_version, site_id, db_version, seq, ts)
                 SELECT (t.\"{rowid_alias}\" << {col_id_bits}) | m.col_id,
                   c.col_version, c.site_id, c.db_version, c.seq,
                   CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_clock\" c
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON c.key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
                 JOIN \"{escaped}\" t ON {pk_join_cond}
                 JOIN \"{escaped}{v2_col_map}\" m ON c.col_name = m.col_name
                 WHERE c.col_name != '{sentinel}'",
                escaped = escaped,
                v2_clock = consts::V2_CLOCK_SUFFIX,
                v2_col_map = consts::V2_COL_MAP_SUFFIX,
                col_id_bits = col_id_bits,
                rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias),
                pk_join_cond = pk_join_cond,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
                ts_fallback = ts_fallback,
            ))?;
        } else {
            // Non-rowid clock migration: JOIN v2_pks to get __crsql_key for cell_key computation.
            let v2_pks_join = v2_pks_join_clause(tbl_info, &escaped, &pk_cols_p_list);
            db.exec_safe(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
                 (cell_key, col_version, site_id, db_version, seq, ts)
                 SELECT (vp.__crsql_key << {col_id_bits}) | m.col_id,
                   c.col_version, c.site_id, c.db_version, c.seq,
                   CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
                 FROM \"{escaped}__crsql_clock\" c
                 JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                   ON c.key = chunk.__crsql_key
                 JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
                 {v2_pks_join}
                 JOIN \"{escaped}{v2_col_map}\" m ON c.col_name = m.col_name
                 WHERE c.col_name != '{sentinel}'",
                escaped = escaped,
                v2_clock = consts::V2_CLOCK_SUFFIX,
                v2_col_map = consts::V2_COL_MAP_SUFFIX,
                col_id_bits = col_id_bits,
                v2_pks_join = v2_pks_join,
                sentinel = sentinel,
                start_key = start_key,
                chunk_size = chunk_size,
                ts_fallback = ts_fallback,
            ))?;
        }

        // Step 4b: For PK-only tables, migrate V1 sentinel clock entries to V2 sentinel at col_id=0.
        // The normal clock migration (step 4) skips sentinels. For PK-only tables, the sentinel
        // is the only clock entry, so we need to migrate it separately.
        if tbl_info.non_pks.is_empty() {
            if key_is_rowid {
                db.exec_safe(&format!(
                    "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
                     (cell_key, col_version, site_id, db_version, seq, ts)
                     SELECT (t.\"{rowid_alias}\" << {col_id_bits}) | 0,
                       1, c.site_id, c.db_version, c.seq,
                       CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
                     FROM \"{escaped}__crsql_clock\" c
                     JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                       ON c.key = chunk.__crsql_key
                     JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
                     JOIN \"{escaped}\" t ON {pk_join_cond}
                     WHERE c.col_name = '{sentinel}'",
                    escaped = escaped,
                    v2_clock = consts::V2_CLOCK_SUFFIX,
                    col_id_bits = col_id_bits,
                    rowid_alias = crate::util::escape_ident(&tbl_info.rowid_alias),
                    pk_join_cond = pk_join_cond,
                    sentinel = sentinel,
                    start_key = start_key,
                    chunk_size = chunk_size,
                    ts_fallback = ts_fallback,
                ))?;
            } else {
                // Non-rowid PK-only sentinel migration: JOIN v2_pks to get __crsql_key.
                let v2_pks_join = v2_pks_join_clause(tbl_info, &escaped, &pk_cols_p_list);
                db.exec_safe(&format!(
                    "INSERT OR REPLACE INTO \"{escaped}{v2_clock}\"
                     (cell_key, col_version, site_id, db_version, seq, ts)
                     SELECT (vp.__crsql_key << {col_id_bits}) | 0,
                       1, c.site_id, c.db_version, c.seq,
                       CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
                     FROM \"{escaped}__crsql_clock\" c
                     JOIN (SELECT __crsql_key FROM \"{escaped}__crsql_pks\" WHERE __crsql_key > {start_key} ORDER BY __crsql_key LIMIT {chunk_size}) chunk
                       ON c.key = chunk.__crsql_key
                     JOIN \"{escaped}__crsql_pks\" p ON c.key = p.__crsql_key
                     {v2_pks_join}
                     WHERE c.col_name = '{sentinel}'",
                    escaped = escaped,
                    v2_clock = consts::V2_CLOCK_SUFFIX,
                    col_id_bits = col_id_bits,
                    v2_pks_join = v2_pks_join,
                    sentinel = sentinel,
                    start_key = start_key,
                    chunk_size = chunk_size,
                    ts_fallback = ts_fallback,
                ))?;
            }
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
            set_progress_marker(db, &tbl_info.tbl_name, "v1_to_v2_migration", last_key)?;
            // Use chunk_size as processed count for estimate (actual may be less due to orphans/IGNORE)
            Ok(chunk_size)
        }
    })();

    match result {
        Ok(processed) => {
            cumulative_done += processed;
            crate::debug::debug_log(&format!("migrate_chunk: processed={} cumulative_done={} total={}", processed, cumulative_done, total));
            if processed == 0 || cumulative_done >= total {
                // Migration complete for this table (no more rows, or all rows processed)
                clear_progress_marker(db, &tbl_info.tbl_name, "v1_to_v2_migration")?;
                // Clear total and done markers
                let clear_sql = "DELETE FROM crsql_master WHERE key = ?\0";
                let stmt = db.prepare_v2(clear_sql)?;
                stmt.bind_text(1, &total_key, sqlite_nostd::Destructor::TRANSIENT)?;
                stmt.step()?;
                let stmt = db.prepare_v2(clear_sql)?;
                stmt.bind_text(1, &done_key, sqlite_nostd::Destructor::TRANSIENT)?;
                stmt.step()?;
                db.exec_safe("RELEASE migration_chunk")?;
                Ok(0)
            } else {
                // Update done marker
                let update_sql = "INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, ?)\0";
                let stmt = db.prepare_v2(update_sql)?;
                stmt.bind_text(1, &done_key, sqlite_nostd::Destructor::TRANSIENT)?;
                stmt.bind_int64(2, cumulative_done)?;
                stmt.step()?;
                db.exec_safe("RELEASE migration_chunk")?;
                let remaining = total - cumulative_done;
                let remaining = if remaining < 0 { 0i64 } else { remaining };
                crate::debug::debug_log(&format!("migrate_chunk: returning remaining={} (total={} cum_done={})", remaining, total, cumulative_done));
                Ok(remaining)
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

/// Get progress marker from crsql_master
unsafe fn get_progress_marker(
    db: *mut sqlite3,
    tbl_name: &str,
    task_type: &str,
) -> Result<Option<i64>, ResultCode> {
    let key = format!("migration_{}_{}", task_type, tbl_name);
    let sql = format!(
        "SELECT value FROM crsql_master WHERE key = ?\0"
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_text(1, &key, sqlite_nostd::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        let val = stmt.column_int64(0);
        return Ok(Some(val));
    }
    Ok(None)
}

/// Set progress marker in crsql_master
unsafe fn set_progress_marker(
    db: *mut sqlite3,
    tbl_name: &str,
    task_type: &str,
    marker: i64,
) -> Result<(), ResultCode> {
    let key = format!("migration_{}_{}", task_type, tbl_name);
    let sql = format!(
        "INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, ?)\0"
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_text(1, &key, sqlite_nostd::Destructor::STATIC)?;
    stmt.bind_int64(2, marker)?;
    stmt.step()?;
    Ok(())
}

/// Clear progress marker from crsql_master
unsafe fn clear_progress_marker(
    db: *mut sqlite3,
    tbl_name: &str,
    task_type: &str,
) -> Result<(), ResultCode> {
    let key = format!("migration_{}_{}", task_type, tbl_name);
    let sql = format!(
        "DELETE FROM crsql_master WHERE key = ?\0"
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_text(1, &key, sqlite_nostd::Destructor::STATIC)?;
    stmt.step()?;
    Ok(())
}
