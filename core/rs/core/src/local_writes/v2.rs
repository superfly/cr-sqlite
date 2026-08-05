extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, Destructor, ResultCode};

use crate::c::crsql_ExtData;
use crate::consts;
use crate::hash_pk::hash_pk_values;
use crate::tableinfo::TableInfo;
use super::bump_seq;

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
    let ts = unsafe { (*ext_data).timestamp.to_string() };
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    // Compute hashed_pk
    let hashed_pk = hash_pk_values(pks_new)
        .map_err(|_| "failed to hash PK values".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    // Insert or update v2_pks entry
    let uses_rowid_key = tbl_info.uses_rowid_key;
    let pk_cols = tbl_info.pks.iter()
        .map(|p| format!("\"{}\"", crate::util::escape_ident(&p.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let pk_values = tbl_info.pks.iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");

    // Check if row already exists in v2_pks by hashed_pk
    let check_stmt = db.prepare_v2(&format!(
        "SELECT __crsql_key, cl FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    )).map_err(|e| format!("failed to prepare check stmt: {:?}", e))?;
    check_stmt.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
    let (key, existing_cl) = match check_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => {
            let k = check_stmt.column_int64(0);
            let cl = check_stmt.column_int64(1);
            (Some(k), Some(cl))
        }
        ResultCode::DONE => {
            (None, None)
        }
        _ => return Err("unexpected result from check stmt".to_string()),
    };

    let key = if let Some(k) = key {
        // Row exists in v2_pks — update CL if it was previously dead (even CL)
        if let Some(cl) = existing_cl {
            if cl % 2 == 0 {
                // Resurrection: CL was even (dead), now odd (alive)
                // Bump seq once for the CL change, matching V1's create record update.
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
        }
        k
    } else {
        // Row not in v2_pks — check v2_tombstones for resurrection
        let tomb_check = db.prepare_v2(&format!(
            "SELECT cl FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
            escaped = escaped,
            suffix = consts::V2_TOMBSTONES_SUFFIX
        )).map_err(|e| format!("failed to prepare tomb check: {:?}", e))?;
        tomb_check.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
        match tomb_check.step().map_err(|e| format!("step: {:?}", e))? {
            ResultCode::ROW => {
                let tomb_cl = tomb_check.column_int64(0);
                let new_cl = tomb_cl + 1; // even→odd = resurrection

                // Bump seq once for the CL change, matching V1's create record update.
                let _ = bump_seq(ext_data);

                // Remove from v2_tombstones and v2_tombstone_pks
                let del_tomb = db.prepare_v2(&format!(
                    "DELETE FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
                    escaped = escaped,
                    suffix = consts::V2_TOMBSTONES_SUFFIX
                )).map_err(|e| format!("failed to prepare del tomb: {:?}", e))?;
                del_tomb.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                del_tomb.step().map_err(|e| format!("step: {:?}", e))?;

                let del_tpk = db.prepare_v2(&format!(
                    "DELETE FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
                    escaped = escaped,
                    suffix = consts::V2_TOMBSTONE_PKS_SUFFIX
                )).map_err(|e| format!("failed to prepare del tpk: {:?}", e))?;
                del_tpk.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                del_tpk.step().map_err(|e| format!("step: {:?}", e))?;

                // Re-insert into v2_pks with resurrected CL
                let insert_sql = if uses_rowid_key {
                    format!(
                        "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, ?) RETURNING __crsql_key",
                        escaped = escaped,
                        suffix = consts::V2_PKS_SUFFIX,
                    )
                } else {
                    format!(
                        "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, hashed_pk, cl) VALUES ({pk_values}, ?, ?) RETURNING __crsql_key",
                        escaped = escaped,
                        suffix = consts::V2_PKS_SUFFIX,
                        pk_cols = pk_cols,
                        pk_values = pk_values,
                    )
                };
                let insert_stmt = db.prepare_v2(&insert_sql)
                    .map_err(|e| format!("failed to prepare resurrect insert: {:?}", e))?;
                if uses_rowid_key {
                    insert_stmt.bind_value(1, pks_new[0]).map_err(|e| format!("bind: {:?}", e))?;
                    insert_stmt.bind_blob(2, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                    insert_stmt.bind_int64(3, new_cl).map_err(|e| format!("bind: {:?}", e))?;
                } else {
                    for (i, pk) in pks_new.iter().enumerate() {
                        insert_stmt.bind_value(i as i32 + 1, *pk).map_err(|e| format!("bind: {:?}", e))?;
                    }
                    insert_stmt.bind_blob(pks_new.len() as i32 + 1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                    insert_stmt.bind_int64(pks_new.len() as i32 + 2, new_cl).map_err(|e| format!("bind: {:?}", e))?;
                }
                insert_stmt.step().map_err(|e| format!("step: {:?}", e))?;
                insert_stmt.column_int64(0)
            }
            _ => {
                // Truly new row — insert into v2_pks with cl=1
                let insert_sql = if uses_rowid_key {
                    format!(
                        "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, 1) RETURNING __crsql_key",
                        escaped = escaped,
                        suffix = consts::V2_PKS_SUFFIX,
                    )
                } else {
                    format!(
                        "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, hashed_pk, cl) VALUES ({pk_values}, ?, 1) RETURNING __crsql_key",
                        escaped = escaped,
                        suffix = consts::V2_PKS_SUFFIX,
                        pk_cols = pk_cols,
                        pk_values = pk_values,
                    )
                };
                let insert_stmt = db.prepare_v2(&insert_sql)
                    .map_err(|e| format!("failed to prepare insert stmt: {:?}", e))?;
                if uses_rowid_key {
                    insert_stmt.bind_value(1, pks_new[0]).map_err(|e| format!("bind: {:?}", e))?;
                    insert_stmt.bind_blob(2, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                } else {
                    for (i, pk) in pks_new.iter().enumerate() {
                        insert_stmt.bind_value(i as i32 + 1, *pk).map_err(|e| format!("bind: {:?}", e))?;
                    }
                    insert_stmt.bind_blob(pks_new.len() as i32 + 1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
                }
                insert_stmt.step().map_err(|e| {
                    let errmsg = db.errmsg().unwrap_or_else(|_| "unknown".to_string());
                    format!("step: {:?} - {}", e, errmsg)
                })?;
                insert_stmt.column_int64(0)
            }
        }
    };

    // Write clock entries for each non-PK column
    for (col_id, _col) in tbl_info.non_pks.iter().enumerate() {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id as i64;
        let clock_sql = format!(
            "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, 1, 0, ?, ?, ?)",
            escaped = escaped,
            suffix = consts::V2_CLOCK_SUFFIX
        );
        let clock_stmt = db.prepare_v2(&clock_sql)
            .map_err(|e| format!("failed to prepare clock stmt: {:?}", e))?;
        clock_stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        let ts_val = ts.parse::<i64>().map_err(|_| "invalid ts".to_string())?;
        if ts_val == 0 { return Err("zero ts".to_string()); }
        clock_stmt.bind_int64(4, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // For pk-only tables (no non-PK columns), write a sentinel clock entry at col_id=0.
    // This carries db_version, seq, ts, site_id so the row is visible in the feed.
    if tbl_info.non_pks.is_empty() {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | 0;
        let clock_sql = format!(
            "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, 1, 0, ?, ?, ?)",
            escaped = escaped,
            suffix = consts::V2_CLOCK_SUFFIX
        );
        let clock_stmt = db.prepare_v2(&clock_sql)
            .map_err(|e| format!("failed to prepare sentinel clock stmt: {:?}", e))?;
        clock_stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        let ts_val = ts.parse::<i64>().map_err(|_| "invalid ts".to_string())?;
        if ts_val == 0 { return Err("zero ts".to_string()); }
        clock_stmt.bind_int64(4, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;
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
    let ts = unsafe { (*ext_data).timestamp.to_string() };
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    let hashed_pk = hash_pk_values(pks_new)
        .map_err(|_| "failed to hash PK values".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    let key_stmt = db.prepare_v2(&format!(
        "SELECT __crsql_key FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    )).map_err(|e| format!("failed to prepare key stmt: {:?}", e))?;
    key_stmt.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
    let key = match key_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => key_stmt.column_int64(0),
        _ => return Err("row not found in v2_pks for update".to_string()),
    };

    for (i, &col_idx) in changed_col_indices.iter().enumerate() {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_idx as i64;
        let clock_sql = format!(
            "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, COALESCE((SELECT col_version + 1 FROM \"{escaped}{suffix}\" WHERE cell_key = ?), 1), 0, ?, ?, ?)",
            escaped = escaped,
            suffix = consts::V2_CLOCK_SUFFIX
        );
        let clock_stmt = db.prepare_v2(&clock_sql)
            .map_err(|e| format!("failed to prepare clock stmt: {:?}", e))?;
        clock_stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(2, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int64(3, db_version).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.bind_int(4, seq).map_err(|e| format!("bind: {:?}", e))?;
        let ts_val = ts.parse::<i64>().map_err(|_| "invalid ts".to_string())?;
        if ts_val == 0 { return Err("zero ts".to_string()); }
        clock_stmt.bind_int64(5, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;
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
    let ts = unsafe { (*ext_data).timestamp.to_string() };
    let db_version = crate::db_version::next_db_version(db, ext_data)
        .map_err(|_| "failed to get next db_version".to_string())?;

    let hashed_pk = hash_pk_values(pks_old)
        .map_err(|_| "failed to hash PK values".to_string())?;

    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    let key_stmt = db.prepare_v2(&format!(
        "SELECT __crsql_key, cl FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    )).map_err(|e| format!("failed to prepare key stmt: {:?}", e))?;
    key_stmt.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;

    let (key, cl) = match key_stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => (key_stmt.column_int64(0), key_stmt.column_int64(1)),
        _ => {
            let tomb_stmt = db.prepare_v2(&format!(
                "SELECT cl FROM \"{escaped}{suffix}\" WHERE hashed_pk = ?",
                escaped = escaped,
                suffix = consts::V2_TOMBSTONES_SUFFIX
            )).map_err(|e| format!("failed to prepare tomb stmt: {:?}", e))?;
            tomb_stmt.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
            match tomb_stmt.step().map_err(|e| format!("step: {:?}", e))? {
                ResultCode::ROW => return Ok(ResultCode::OK),
                _ => return Ok(ResultCode::OK),
            }
        }
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
        "DELETE FROM \"{escaped}{suffix}\" WHERE cell_key >> {bits} = ?",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX,
        bits = consts::CRSQL_COL_ID_BITS
    )).map_err(|e| format!("failed to prepare del clock stmt: {:?}", e))?;
    del_clock_stmt.bind_int64(1, key).map_err(|e| format!("bind: {:?}", e))?;
    del_clock_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    let tomb_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (site_id, db_version, seq, hashed_pk, cl, ts) VALUES (0, ?, ?, ?, ?, ?)",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONES_SUFFIX
    );
    let tomb_stmt = db.prepare_v2(&tomb_sql)
        .map_err(|e| format!("failed to prepare tomb insert stmt: {:?}", e))?;
    tomb_stmt.bind_int64(1, db_version).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.bind_int(2, seq).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.bind_blob(3, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.bind_int64(4, new_cl).map_err(|e| format!("bind: {:?}", e))?;
    let ts_val = ts.parse::<i64>().map_err(|_| "invalid ts".to_string())?;
    if ts_val == 0 { return Err("zero ts".to_string()); }
    tomb_stmt.bind_int64(5, ts_val).map_err(|e| format!("bind: {:?}", e))?;
    tomb_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    let pk_cols = tbl_info.pks.iter()
        .map(|p| format!("\"{}\"", crate::util::escape_ident(&p.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let pk_values = tbl_info.pks.iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let tpk_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (hashed_pk, {pk_cols}) VALUES (?, {pk_values})",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONE_PKS_SUFFIX,
        pk_cols = pk_cols,
        pk_values = pk_values,
    );
    let tpk_stmt = db.prepare_v2(&tpk_sql)
        .map_err(|e| format!("failed to prepare tpk stmt: {:?}", e))?;
    tpk_stmt.bind_blob(1, &hashed_pk, Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
    for (i, pk) in pks_old.iter().enumerate() {
        tpk_stmt.bind_value(i as i32 + 2, *pk).map_err(|e| format!("bind: {:?}", e))?;
    }
    tpk_stmt.step().map_err(|e| format!("step: {:?}", e))?;

    Ok(ResultCode::OK)
}
