extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, Destructor, ResultCode};

use crate::c::crsql_ExtData;
use crate::consts;
use crate::hash_pk::hash_pk_values;
use crate::tableinfo::TableInfo;
use crate::v2_stmts::StmtGuard;
use super::bump_seq;

/// Bind the PK lookup value to a lookup_row_state statement (both slots 1 and 2).
/// `use_hash` = true binds hashed_pk blob; false binds the raw PK value.
fn bind_row_state_lookup(
    stmt: &mut StmtGuard,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<alloc::vec::Vec<u8>>,
    use_hash: bool,
) -> Result<(), String> {
    if use_hash {
        let blob = hashed_pk.as_ref().unwrap();
        stmt.bind_blob(1, blob, Destructor::STATIC)
            .map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_blob(2, blob, Destructor::STATIC)
            .map_err(|e| format!("bind: {:?}", e))?;
    } else {
        stmt.bind_value(1, pks[0])
            .map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_value(2, pks[0])
            .map_err(|e| format!("bind: {:?}", e))?;
    }
    Ok(())
}

/// Bind PK values (and hashed_pk if hash mode) to a v2_pks INSERT statement.
/// Returns the next bind slot index (for optional cl bind in resurrection).
fn bind_pks_insert(
    stmt: &mut StmtGuard,
    tbl_info: &TableInfo,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<alloc::vec::Vec<u8>>,
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
) -> Result<Option<alloc::vec::Vec<u8>>, String> {
    if tbl_info.skip_hash {
        Ok(None)
    } else {
        hash_pk_values(pks)
            .map(Some)
            .map_err(|_| "failed to hash PK values".to_string())
    }
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

    let skip_hash = tbl_info.skip_hash;
    let hashed_pk = compute_hashed_pk(tbl_info, pks_new)?;
    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    // Get cached statements (lazily prepared on first use)
    let mut v2_stmts_ref = tbl_info.get_v2_stmts(db, ext_data)
        .map_err(|e| format!("failed to get v2 stmts: {:?}", e))?;
    let v2_stmts = v2_stmts_ref.as_mut().unwrap();

    // Single lookup: alive (key, cl) from v2_pks or dead (NULL, cl) from v2_tombstones.
    let use_hash = !skip_hash;
    let (key_opt, cl) = {
        let mut stmt = v2_stmts.lookup_row_state();
        bind_row_state_lookup(&mut stmt, pks_new, &hashed_pk, use_hash)?;
        match stmt.step().map_err(|e| format!("step: {:?}", e))? {
            ResultCode::ROW => {
                if stmt.column_type(0).map_err(|e| format!("column_type: {:?}", e))? == sqlite::ColumnType::Null {
                    (None, stmt.column_int64(1)) // dead — in tombstones
                } else {
                    (Some(stmt.column_int64(0)), stmt.column_int64(1)) // alive — in v2_pks
                }
            }
            ResultCode::DONE => (None, 0), // truly new row
            _ => return Err("unexpected result from lookup_row_state".to_string()),
        }
        // guard drops here → auto reset + clear_bindings
    };

    if let Some(k) = key_opt {
        // Row exists in v2_pks — update CL if it was previously dead (even CL)
        if cl % 2 == 0 {
            let _ = bump_seq(ext_data);
            let new_cl = cl + 1;
            let mut upd = v2_stmts.pks_update_cl();
            upd.bind_int64(1, new_cl).map_err(|e| format!("bind: {:?}", e))?;
            upd.bind_int64(2, k).map_err(|e| format!("bind: {:?}", e))?;
            upd.step().map_err(|e| format!("step: {:?}", e))?;
        }
        return finish_insert(db, ext_data, tbl_info, v2_stmts, k, db_version, ts_val);
    }

    // Row is either dead (in tombstones) or truly new.
    let key = if cl > 0 {
        // Resurrection: row was in tombstones (cl > 0 means we found a tombstone entry)
        let new_cl = cl + 1; // even→odd = resurrection
        let _ = bump_seq(ext_data);

        // Remove from v2_tombstones
        {
            let mut del = v2_stmts.tomb_delete();
            if use_hash {
                del.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
                    .map_err(|e| format!("bind: {:?}", e))?;
            } else {
                del.bind_value(1, pks_new[0])
                    .map_err(|e| format!("bind: {:?}", e))?;
            }
            del.step().map_err(|e| format!("step: {:?}", e))?;
        }

        // Remove from v2_tombstone_pks (hash mode only)
        if !skip_hash {
            let mut del = v2_stmts.tomb_pks_delete()
                .map_err(|e| format!("tomb_pks_delete: {:?}", e))?;
            del.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
                .map_err(|e| format!("bind: {:?}", e))?;
            del.step().map_err(|e| format!("step: {:?}", e))?;
        }

        // Re-insert into v2_pks with resurrected CL
        let mut ins = v2_stmts.pks_insert();
        let cl_slot = bind_pks_insert(&mut ins, tbl_info, pks_new, &hashed_pk)?;
        ins.bind_int64(cl_slot, new_cl).map_err(|e| format!("bind: {:?}", e))?;
        ins.step().map_err(|e| format!("step: {:?}", e))?;
        ins.column_int64(0)
    } else {
        // Truly new row — insert into v2_pks with cl=1
        let mut ins = v2_stmts.pks_insert();
        let cl_slot = bind_pks_insert(&mut ins, tbl_info, pks_new, &hashed_pk)?;
        ins.bind_int64(cl_slot, 1).map_err(|e| format!("bind: {:?}", e))?;
        ins.step().map_err(|e| {
            let errmsg = db.errmsg().unwrap_or_else(|_| "unknown".to_string());
            format!("step: {:?} - {}", e, errmsg)
        })?;
        ins.column_int64(0)
    };

    finish_insert(db, ext_data, tbl_info, v2_stmts, key, db_version, ts_val)
}

/// Write clock entries for each non-PK column (or sentinel for pk-only tables).
fn finish_insert(
    _db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    v2_stmts: &mut crate::v2_stmts::V2Stmts,
    key: i64,
    db_version: i64,
    ts_val: i64,
) -> Result<ResultCode, String> {
    let col_ids: vec::Vec<usize> = if tbl_info.non_pks.is_empty() {
        vec![0] // sentinel for pk-only tables
    } else {
        (0..tbl_info.non_pks.len()).collect()
    };
    let mut stmt = v2_stmts.clock_insert();
    for col_id in col_ids {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id as i64;
        stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(4, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        stmt.step().map_err(|e| format!("step: {:?}", e))?;
        // clear_bindings + reset for next iteration
        stmt.clear_bindings().map_err(|e| format!("clear_bindings: {:?}", e))?;
        stmt.reset().map_err(|e| format!("reset: {:?}", e))?;
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

    let skip_hash = tbl_info.skip_hash;
    let hashed_pk = compute_hashed_pk(tbl_info, pks_new)?;
    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    let mut v2_stmts_ref = tbl_info.get_v2_stmts(db, ext_data)
        .map_err(|e| format!("failed to get v2 stmts: {:?}", e))?;
    let v2_stmts = v2_stmts_ref.as_mut().unwrap();

    // Lookup __crsql_key — row must be alive (in v2_pks). Dead or missing = error.
    let key = {
        let mut stmt = v2_stmts.lookup_row_state();
        bind_row_state_lookup(&mut stmt, pks_new, &hashed_pk, !skip_hash)?;
        match stmt.step().map_err(|e| format!("step: {:?}", e))? {
            ResultCode::ROW => {
                if stmt.column_type(0).map_err(|e| format!("column_type: {:?}", e))? == sqlite::ColumnType::Null {
                    return Err("row is dead (in tombstones) — cannot update a deleted row".to_string());
                }
                stmt.column_int64(0)
            }
            _ => return Err("row not found in v2_pks for update".to_string()),
        }
    };

    // Update clock entries for each changed column
    let mut stmt = v2_stmts.clock_upsert();
    for &col_idx in changed_col_indices {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_idx as i64;
        stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(2, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(3, db_version).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int(4, seq).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(5, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        stmt.step().map_err(|e| format!("step: {:?}", e))?;
        stmt.clear_bindings().map_err(|e| format!("clear_bindings: {:?}", e))?;
        stmt.reset().map_err(|e| format!("reset: {:?}", e))?;
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

    let skip_hash = tbl_info.skip_hash;
    let hashed_pk = compute_hashed_pk(tbl_info, pks_old)?;
    let ts_val = unsafe { (*ext_data).timestamp as i64 };

    let mut v2_stmts_ref = tbl_info.get_v2_stmts(db, ext_data)
        .map_err(|e| format!("failed to get v2 stmts: {:?}", e))?;
    let v2_stmts = v2_stmts_ref.as_mut().unwrap();

    // Lookup __crsql_key and cl — row must be alive (in v2_pks). Dead or missing = no-op.
    let (key, cl) = {
        let mut stmt = v2_stmts.lookup_row_state();
        bind_row_state_lookup(&mut stmt, pks_old, &hashed_pk, !skip_hash)?;
        match stmt.step().map_err(|e| format!("step: {:?}", e))? {
            ResultCode::ROW => {
                if stmt.column_type(0).map_err(|e| format!("column_type: {:?}", e))? == sqlite::ColumnType::Null {
                    // Row is already dead (in tombstones) — nothing to do.
                    return Ok(ResultCode::OK);
                }
                (stmt.column_int64(0), stmt.column_int64(1))
            }
            // Not in v2_pks or v2_tombstones — never tracked. No-op.
            _ => return Ok(ResultCode::OK),
        }
    };

    let new_cl = cl + 1;
    let seq = bump_seq(ext_data);

    // Delete from v2_pks
    {
        let mut del = v2_stmts.pks_delete();
        del.bind_int64(1, key).map_err(|e| format!("bind: {:?}", e))?;
        del.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Delete clock entries for this key
    {
        let mut del = v2_stmts.clock_delete_range();
        let base = key << consts::CRSQL_COL_ID_BITS as i64;
        del.bind_int64(1, base).map_err(|e| format!("bind: {:?}", e))?;
        del.bind_int64(2, base | consts::CRSQL_COL_ID_MASK as i64).map_err(|e| format!("bind: {:?}", e))?;
        del.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Insert tombstone
    {
        let mut ins = v2_stmts.tomb_insert();
        ins.bind_int(1, 0).map_err(|e| format!("bind: {:?}", e))?;
        ins.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        ins.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        if skip_hash {
            ins.bind_value(4, pks_old[0]).map_err(|e| format!("bind: {:?}", e))?;
        } else {
            ins.bind_blob(4, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(|e| format!("bind: {:?}", e))?;
        }
        ins.bind_int64(5, new_cl).map_err(|e| format!("bind: {:?}", e))?;
        ins.bind_int64(6, ts_val).map_err(|e| format!("bind: {:?}", e))?;
        ins.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Insert tombstone PKs (hash mode only — skip_hash stores PK directly in tombstone)
    if !skip_hash {
        let mut ins = v2_stmts.tomb_pks_insert()
            .map_err(|e| format!("tomb_pks_insert: {:?}", e))?;
        ins.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
            .map_err(|e| format!("bind: {:?}", e))?;
        for (i, pk) in pks_old.iter().enumerate() {
            ins.bind_value(i as i32 + 2, *pk).map_err(|e| format!("bind: {:?}", e))?;
        }
        ins.step().map_err(|e| format!("step: {:?}", e))?;
    }

    Ok(ResultCode::OK)
}
