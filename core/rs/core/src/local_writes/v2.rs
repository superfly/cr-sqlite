extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, Destructor, ResultCode};

use crate::c::crsql_ExtData;
use crate::consts;
use crate::hash_pk::hash_pk_values;
use crate::tableinfo::TableInfo;
use super::bump_seq;

/// Build WHERE clause for PK lookup in v2_pks or v2_tombstones.
/// `is_pks_table` = true for v2_pks, false for v2_tombstones.
/// Returns (where_clause, use_hash_bind).
fn v2_pk_lookup_where(tbl_info: &TableInfo, is_pks_table: bool) -> (String, bool) {
    if tbl_info.skip_hash {
        if is_pks_table && tbl_info.key_is_rowid {
            // v2_pks stores __crsql_key = rowid = PK value for INTEGER PRIMARY KEY tables
            ("__crsql_key = ?".to_string(), false)
        } else {
            // v2_tombstones always stores the PK column directly (no __crsql_key)
            (format!("\"{}\" = ?", tbl_info.skip_hash_pk_col), false)
        }
    } else {
        ("hashed_pk = ?".to_string(), true)
    }
}

/// Build SELECT query for v2_pks or v2_tombstones PK lookup.
/// `is_pks_table` = true for v2_pks, false for v2_tombstones.
/// `select_cols` is e.g. "__crsql_key, cl" or "__crsql_key" or "cl".
/// Returns (sql, use_hash_bind).
fn v2_pk_lookup_sql(
    tbl_info: &TableInfo,
    escaped: &str,
    select_cols: &str,
    is_pks_table: bool,
) -> (String, bool) {
    let suffix = if is_pks_table { consts::V2_PKS_SUFFIX } else { consts::V2_TOMBSTONES_SUFFIX };
    let (where_clause, use_hash) = v2_pk_lookup_where(tbl_info, is_pks_table);
    (
        format!(
            "SELECT {} FROM \"{}{}\" WHERE {}",
            select_cols, escaped, suffix, where_clause
        ),
        use_hash,
    )
}

/// Bind the PK lookup value to a statement prepared by v2_pk_lookup_sql.
fn v2_pk_bind_lookup(
    stmt: &sqlite::ManagedStmt,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<Vec<u8>>,
    use_hash: bool,
) -> Result<(), String> {
    if use_hash {
        stmt.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
            .map(|_| ())
            .map_err(|e| format!("bind: {:?}", e))
    } else {
        stmt.bind_value(1, pks[0])
            .map(|_| ())
            .map_err(|e| format!("bind: {:?}", e))
    }
}

/// Build INSERT INTO v2_pks SQL with RETURNING __crsql_key.
/// `cl_expr` is "1" for new rows or "?" for resurrection (cl bound as parameter).
fn v2_pks_insert_sql(
    tbl_info: &TableInfo,
    escaped: &str,
    pk_cols: &str,
    pk_values: &str,
    cl_expr: &str,
) -> String {
    let suffix = consts::V2_PKS_SUFFIX;
    if tbl_info.skip_hash && tbl_info.key_is_rowid {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, cl) VALUES (?, {cl_expr}) RETURNING __crsql_key"
        )
    } else if tbl_info.skip_hash {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, cl) VALUES ({pk_values}, {cl_expr}) RETURNING __crsql_key"
        )
    } else if tbl_info.key_is_rowid {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, {cl_expr}) RETURNING __crsql_key"
        )
    } else {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, hashed_pk, cl) VALUES ({pk_values}, ?, {cl_expr}) RETURNING __crsql_key"
        )
    }
}

/// Bind PK values (and hashed_pk if hash mode) to a v2_pks INSERT statement.
/// Returns the next bind slot index (for optional cl bind in resurrection).
fn v2_pks_bind_insert(
    stmt: &sqlite::ManagedStmt,
    tbl_info: &TableInfo,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<Vec<u8>>,
) -> Result<i32, String> {
    let bind_err = |e: sqlite::ResultCode| format!("bind: {:?}", e);
    if tbl_info.skip_hash && tbl_info.key_is_rowid {
        stmt.bind_value(1, pks[0]).map_err(bind_err)?;
        Ok(2)
    } else if tbl_info.skip_hash {
        for (i, pk) in pks.iter().enumerate() {
            stmt.bind_value(i as i32 + 1, *pk).map_err(bind_err)?;
        }
        Ok(pks.len() as i32 + 1)
    } else if tbl_info.key_is_rowid {
        stmt.bind_value(1, pks[0]).map_err(bind_err)?;
        stmt.bind_blob(2, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
            .map_err(bind_err)?;
        Ok(3)
    } else {
        for (i, pk) in pks.iter().enumerate() {
            stmt.bind_value(i as i32 + 1, *pk).map_err(bind_err)?;
        }
        let next = pks.len() as i32 + 1;
        stmt.bind_blob(next, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
            .map_err(bind_err)?;
        Ok(next + 1)
    }
}

/// Compute hashed_pk for hash mode (returns None for skip_hash).
fn compute_hashed_pk(
    tbl_info: &TableInfo,
    pks: &[*mut sqlite::value],
) -> Result<Option<Vec<u8>>, String> {
    if tbl_info.skip_hash {
        Ok(None)
    } else {
        hash_pk_values(pks)
            .map(Some)
            .map_err(|_| "failed to hash PK values".to_string())
    }
}

/// Build comma-separated escaped PK column list and matching "?" placeholders.
/// E.g. ("\"id\"", "?") or ("\"a\", \"b\"", "?, ?").
fn pk_cols_and_values(tbl_info: &TableInfo) -> (String, String) {
    let pk_cols = tbl_info.pks.iter()
        .map(|p| format!("\"{}\"", crate::util::escape_ident(&p.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let pk_values = tbl_info.pks.iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    (pk_cols, pk_values)
}

/// V2 after_insert: write to v2_pks and v2_clock tables.
pub fn v2_after_insert(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    pks_new: &[*mut sqlite::value],
) -> Result<ResultCode, String> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { (*ext_data).timestamp } == 0 {
        return Err("v2_after_insert: timestamp not set — call crsql_set_ts() first".to_string());
    }
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let skip_hash = tbl_info.skip_hash;
    let (pk_cols, pk_values) = pk_cols_and_values(tbl_info);

    let hashed_pk = compute_hashed_pk(tbl_info, pks_new)?;

    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    let (check_sql, check_binds_hash) = v2_pk_lookup_sql(
        tbl_info, &escaped, "__crsql_key, cl", true,
    );
    let check_stmt = db.prepare_v2(&check_sql)
        .map_err(|e| format!("failed to prepare check stmt: {:?}", e))?;
    v2_pk_bind_lookup(&check_stmt, pks_new, &hashed_pk, check_binds_hash)?;

    let (key, cl) = match check_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => {
            (Some(check_stmt.column_int64(0)), check_stmt.column_int64(1))
        }
        ResultCode::DONE => {
            (None, 0)
        }
        _ => return Err("unexpected result from check stmt".to_string()),
    };

    let key = if let Some(k) = key {
        // Row exists in v2_pks — update CL if it was previously dead (even CL)
        if cl % 2 == 0 {
            // Resurrection: CL was even (dead), now odd (alive)
            let _ = bump_seq(ext_data);
            let new_cl = cl + 1;
            let update_stmt = db.prepare_v2(&format!(
                "UPDATE \"{escaped}{suffix}\" SET cl = ? WHERE __crsql_key = ?",
                escaped = escaped,
                suffix = consts::V2_PKS_SUFFIX
            )).map_err(|e| format!("failed to prepare update stmt: {:?}", e))?;
            update_stmt.bind_int64(1, new_cl).map_err(|e| format!("bind: {:?}", e))?;
            update_stmt.bind_int64(2, k).map_err(|e| format!("bind: {:?}", e))?;
            update_stmt.step().map_err(|e| format!("step: {:?}", e))?;
        }
        k
    } else {
        // Row not in v2_pks — check v2_tombstones for resurrection
        let (tomb_check_sql, tomb_binds_hash) = v2_pk_lookup_sql(
            tbl_info, &escaped, "cl", false,
        );
        let tomb_check = db.prepare_v2(&tomb_check_sql)
            .map_err(|e| format!("failed to prepare tomb check: {:?}", e))?;
        v2_pk_bind_lookup(&tomb_check, pks_new, &hashed_pk, tomb_binds_hash)?;

        match tomb_check.step().map_err(|e| format!("step: {:?}", e))? {
            ResultCode::ROW => {
                let tomb_cl = tomb_check.column_int64(0);
                let new_cl = tomb_cl + 1; // even→odd = resurrection

                let _ = bump_seq(ext_data);

                // Remove from v2_tombstones
                let (del_where, del_hash) = v2_pk_lookup_where(tbl_info, false);
                let del_tomb_sql = format!(
                    "DELETE FROM \"{escaped}{suffix}\" WHERE {where}",
                    escaped = escaped,
                    suffix = consts::V2_TOMBSTONES_SUFFIX,
                    where = del_where,
                );
                let del_tomb = db.prepare_v2(&del_tomb_sql)
                    .map_err(|e| format!("failed to prepare del tomb: {:?}", e))?;
                v2_pk_bind_lookup(&del_tomb, pks_new, &hashed_pk, del_hash)?;
                del_tomb.step().map_err(|e| format!("step: {:?}", e))?;

                // Remove from v2_tombstone_pks (only in hash mode)
                if !skip_hash {
                    let del_tpk = db.prepare_v2(&format!(
                        "DELETE FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
                        escaped = escaped,
                        suffix = consts::V2_TOMBSTONE_PKS_SUFFIX
                    )).map_err(|e| format!("failed to prepare del tpk: {:?}", e))?;
                    del_tpk.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                    del_tpk.step().map_err(|e| format!("step: {:?}", e))?;
                }

                // Re-insert into v2_pks with resurrected CL
                let insert_sql = v2_pks_insert_sql(tbl_info, &escaped, &pk_cols, &pk_values, "?");
                let insert_stmt = db.prepare_v2(&insert_sql)
                    .map_err(|e| format!("failed to prepare resurrect insert: {:?}", e))?;
                let cl_slot = v2_pks_bind_insert(&insert_stmt, tbl_info, pks_new, &hashed_pk)?;
                insert_stmt.bind_int64(cl_slot, new_cl).map_err(|e| format!("bind: {:?}", e))?;
                insert_stmt.step().map_err(|e| format!("step: {:?}", e))?;
                insert_stmt.column_int64(0)
            }
            _ => {
                // Truly new row — insert into v2_pks with cl=1
                let insert_sql = v2_pks_insert_sql(tbl_info, &escaped, &pk_cols, &pk_values, "1");
                let insert_stmt = db.prepare_v2(&insert_sql)
                    .map_err(|e| format!("failed to prepare insert stmt: {:?}", e))?;
                v2_pks_bind_insert(&insert_stmt, tbl_info, pks_new, &hashed_pk)?;
                insert_stmt.step().map_err(|e| {
                    let errmsg = db.errmsg().unwrap_or_else(|_| "unknown".to_string());
                    format!("step: {:?} - {}", e, errmsg)
                })?;
                insert_stmt.column_int64(0)
            }
        }
    };

    // Write clock entries for each non-PK column (or sentinel for pk-only tables).
    let clock_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, 1, 0, ?, ?, ?)",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    );
    let clock_stmt = db.prepare_v2(&clock_sql)
        .map_err(|e| format!("failed to prepare clock stmt: {:?}", e))?;

    let col_ids: Vec<usize> = if tbl_info.non_pks.is_empty() {
        vec![0] // sentinel for pk-only tables
    } else {
        (0..tbl_info.non_pks.len()).collect()
    };
    for col_id in col_ids {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id as i64;
        clock_stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(4, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;
        clock_stmt.reset().map_err(|e| format!("reset: {:?}", e))?;
    }

    Ok(ResultCode::OK)
}

/// V2 after_update: update v2_clock entries for changed columns.
pub fn v2_after_update(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    pks_new: &[*mut sqlite::value],
    changed_col_indices: &[usize],
) -> Result<ResultCode, String> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { (*ext_data).timestamp } == 0 {
        return Err("v2_after_update: timestamp not set — call crsql_set_ts() first".to_string());
    }
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    let hashed_pk = compute_hashed_pk(tbl_info, pks_new)?;

    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    let (key_sql, key_binds_hash) = v2_pk_lookup_sql(
        tbl_info, &escaped, "__crsql_key", true,
    );
    let key_stmt = db.prepare_v2(&key_sql)
        .map_err(|e| format!("failed to prepare key stmt: {:?}", e))?;
    v2_pk_bind_lookup(&key_stmt, pks_new, &hashed_pk, key_binds_hash)?;
    let key = match key_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => key_stmt.column_int64(0),
        _ => return Err("row not found in v2_pks for update".to_string()),
    };

    let clock_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, COALESCE((SELECT col_version + 1 FROM \"{escaped}{suffix}\" WHERE cell_key = ?), 1), 0, ?, ?, ?)",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    );
    let clock_stmt = db.prepare_v2(&clock_sql)
        .map_err(|e| format!("failed to prepare clock stmt: {:?}", e))?;

    for &col_idx in changed_col_indices {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_idx as i64;
        clock_stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(2, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(3, db_version).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int(4, seq).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(5, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;
        clock_stmt.reset().map_err(|e| format!("reset: {:?}", e))?;
    }

    Ok(ResultCode::OK)
}

/// V2 after_delete: move row from v2_pks to v2_tombstones, delete clock entries.
pub fn v2_after_delete(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    pks_old: &[*mut sqlite::value],
) -> Result<ResultCode, String> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { (*ext_data).timestamp } == 0 {
        return Err("v2_after_delete: timestamp not set — call crsql_set_ts() first".to_string());
    }
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let skip_hash = tbl_info.skip_hash;

    let hashed_pk = compute_hashed_pk(tbl_info, pks_old)?;

    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    let (key_sql, key_binds_hash) = v2_pk_lookup_sql(
        tbl_info, &escaped, "__crsql_key, cl", true,
    );
    let key_stmt = db.prepare_v2(&key_sql)
        .map_err(|e| format!("failed to prepare key stmt: {:?}", e))?;
    v2_pk_bind_lookup(&key_stmt, pks_old, &hashed_pk, key_binds_hash)?;

    let (key, cl) = match key_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => (key_stmt.column_int64(0), key_stmt.column_int64(1)),
        // Not in v2_pks — row is either already tombstoned or never tracked. No-op.
        _ => return Ok(ResultCode::OK),
    };

    let new_cl = cl + 1;
    let seq = bump_seq(ext_data);

    let del_stmt = db.prepare_v2(&format!(
        "DELETE FROM \"{escaped}{suffix}\" WHERE __crsql_key = ?",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    )).map_err(|e| format!("failed to prepare del stmt: {:?}", e))?;
    del_stmt.bind_int64(1, key).map_err(|e| format!("bind: {:?}", e))?;
    del_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    let del_clock_stmt = db.prepare_v2(&format!(
        "DELETE FROM \"{escaped}{suffix}\" WHERE cell_key >= ? AND cell_key <= ?",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    )).map_err(|e| format!("failed to prepare del clock stmt: {:?}", e))?;
    let base = key << consts::CRSQL_COL_ID_BITS as i64;
    del_clock_stmt.bind_int64(1, base).map_err(|e| format!("bind: {:?}", e))?;
    del_clock_stmt.bind_int64(2, base | consts::CRSQL_COL_ID_MASK as i64).map_err(|e| format!("bind: {:?}", e))?;
    del_clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    // Insert tombstone. Column name is PK col (skip_hash) or hashed_pk (hash mode).
    let pk_col_name = if skip_hash { &tbl_info.skip_hash_pk_col } else { "hashed_pk" };
    let tomb_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (site_id, db_version, seq, \"{pk_col}\", cl, ts) VALUES (0, ?, ?, ?, ?, ?)",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONES_SUFFIX,
        pk_col = pk_col_name,
    );
    let tomb_stmt = db.prepare_v2(&tomb_sql)
        .map_err(|e| format!("failed to prepare tomb insert stmt: {:?}", e))?;
    tomb_stmt.bind_int64(1, db_version).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.bind_int(2, seq).map_err(|e| format!("bind: {:?}", e))?;
    if skip_hash {
        tomb_stmt.bind_value(3, pks_old[0]).map_err(|e| format!("bind: {:?}", e))?;
    } else {
        tomb_stmt.bind_blob(3, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
    }
    tomb_stmt.bind_int64(4, new_cl).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.bind_int64(5, ts_val).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    // Insert tombstone PKs (only in hash mode — skip_hash stores PK directly in tombstone)
    if !skip_hash {
        let (pk_cols, pk_values) = pk_cols_and_values(tbl_info);
        let tpk_sql = format!(
            "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (hashed_pk, {pk_cols}) VALUES (?, {pk_values})",
            escaped = escaped,
            suffix = consts::V2_TOMBSTONE_PKS_SUFFIX,
            pk_cols = pk_cols,
            pk_values = pk_values,
        );
        let tpk_stmt = db.prepare_v2(&tpk_sql)
            .map_err(|e| format!("failed to prepare tpk stmt: {:?}", e))?;
        tpk_stmt.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
        for (i, pk) in pks_old.iter().enumerate() {
            tpk_stmt.bind_value(i as i32 + 2, *pk).map_err(|e| format!("bind: {:?}", e))?;
        }
        tpk_stmt.step().map_err(|e| format!("step: {:?}", e))?;
    }

    Ok(ResultCode::OK)
}
