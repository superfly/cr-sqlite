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
        // In skip_hash mode, we can only have one PK column
        stmt.bind_value(1, pks[0])
            .map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_value(2, pks[0])
            .map_err(|e| format!("bind: {:?}", e))?;
    }
    Ok(())
}

/// Result of looking up a row's state in v2_pks / v2_tombstones.
enum RowState {
    Alive(i64, i64),    // (key, cl) — row is in v2_pks
    Dead(i64),          // cl — row is in v2_tombstones (key is NULL)
    NotFound,           // row doesn't exist in either table
}

/// Bind PKs to lookup_row_state and return the row state.
fn lookup_row_state(
    v2_stmts: &mut crate::v2_stmts::V2Stmts,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<alloc::vec::Vec<u8>>,
    use_hash: bool,
) -> Result<RowState, String> {
    let mut stmt = v2_stmts.lookup_row_state();
    bind_row_state_lookup(&mut stmt, pks, hashed_pk, use_hash)?;
    match stmt.step().map_err(|e| format!("step: {:?}", e))? {
        ResultCode::ROW => {
            if stmt.column_type(0).map_err(|e| format!("column_type: {:?}", e))? == sqlite::ColumnType::Null {
                Ok(RowState::Dead(stmt.column_int64(1)))
            } else {
                Ok(RowState::Alive(stmt.column_int64(0), stmt.column_int64(1)))
            }
        }
        ResultCode::DONE => Ok(RowState::NotFound),
        _ => Err("unexpected result from lookup_row_state".to_string()),
    }
}

/// Bind PK values (and hashed_pk if hash mode) to a v2_pks INSERT statement.
/// Returns the next bind slot index (for optional cl bind in resurrection).
/// For key_is_rowid tables, `rowid` is used as __crsql_key (slot 1).
/// For has_integer_pk tables, pks[0] IS the rowid so rowid is redundant.
/// For key_is_rowid && !has_integer_pk, rowid must be provided explicitly
/// since pks[0] is the PK column value, not the rowid.
fn bind_pks_insert(
    stmt: &mut StmtGuard,
    tbl_info: &TableInfo,
    pks: &[*mut sqlite::value],
    hashed_pk: &Option<alloc::vec::Vec<u8>>,
    rowid: Option<i64>,
) -> Result<i32, String> {
    let bind_err = |e: sqlite::ResultCode| format!("bind: {:?}", e);
    if tbl_info.key_is_rowid {
        // __crsql_key = rowid. For has_integer_pk, pks[0] == rowid so either works.
        // For !has_integer_pk, we must use the explicit rowid.
        let key_val = if tbl_info.has_integer_pk {
            pks[0]
        } else {
            // Use the explicit rowid — bind as int64
            stmt.bind_int64(1, rowid.ok_or("rowid-key table missing rowid for pks_insert")?);
            // Still need to bind PK columns for the index (skip_hash) or hashed_pk
            if tbl_info.skip_hash {
                return Ok(2);
            }
            stmt.bind_blob(2, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
                .map_err(bind_err)?;
            return Ok(3);
        };
        if tbl_info.skip_hash {
            stmt.bind_value(1, key_val).map_err(bind_err)?;
            Ok(2)
        } else {
            stmt.bind_value(1, key_val).map_err(bind_err)?;
            stmt.bind_blob(2, hashed_pk.as_ref().unwrap(), Destructor::STATIC)
                .map_err(bind_err)?;
            Ok(3)
        }
    } else if tbl_info.skip_hash {
        for (i, pk) in pks.iter().enumerate() {
            stmt.bind_value(i as i32 + 1, *pk).map_err(bind_err)?;
        }
        Ok(pks.len() as i32 + 1)
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
/// `rowid` is the actual rowid for key_is_rowid tables (needed when
/// !has_integer_pk since pks[0] is the PK value, not the rowid).
pub fn v2_after_insert(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    pks_new: &[*mut sqlite::value],
    rowid: Option<i64>,
) -> Result<ResultCode, String> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { crate::config::ensure_timestamp(ext_data).is_err() } {
        return Err("v2_after_insert: timestamp not set — call crsql_set_ts() first or set default-ts".to_string());
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
    // Row must be Dead (resurrection) or NotFound (fresh insert). Alive is a
    // logic error: with recursive_triggers ON (which cr-sqlite enforces),
    // INSERT OR REPLACE fires DELETE then INSERT, so the row should be in
    // v2_tombstones by the time we get here. Reaching Alive means either
    // recursive_triggers was turned off or data is corrupt.
    let cl = match lookup_row_state(v2_stmts, pks_new, &hashed_pk, use_hash)? {
        RowState::Alive(..) => {
            return Err("v2_after_insert: row already alive in v2_pks — recursive_triggers may be disabled or data is corrupt".to_string());
        }
        RowState::Dead(cl) => cl,
        RowState::NotFound => 0,
    };

    // Row is either dead (in tombstones) or truly new.
    // cl=0 for NotFound (fresh insert → cl=1), cl=even for Dead (resurrection → cl+1).
    let new_cl = cl + 1;

    // Resurrection: remove tombstone entries before re-inserting into v2_pks.
    if cl > 0 {
        // Bump seq to stay in sync with V1, which bumps seq for the delete
        // trigger's tombstone insert before the insert trigger runs.
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
    }

    // Insert into v2_pks and get the assigned __crsql_key
    let mut ins = v2_stmts.pks_insert();
    let cl_slot = bind_pks_insert(&mut ins, tbl_info, pks_new, &hashed_pk, rowid)?;
    ins.bind_int64(cl_slot, new_cl).map_err(|e| format!("bind: {:?}", e))?;
    ins.step().map_err(|e| {
        let errmsg = db.errmsg().unwrap_or_else(|_| "unknown".to_string());
        format!("step: {:?} - {}", e, errmsg)
    })?;
    let key = ins.column_int64(0);
    drop(ins);

    write_clock_entries(db, ext_data, tbl_info, v2_stmts, key, db_version, ts_val)
}

/// Write clock entries for each non-PK column (or sentinel for pk-only tables).
/// Queries actual col_ids from v2_col_map to handle holes from dropped columns.
///
/// All columns get col_version=1 because the insert trigger only receives PK
/// values, not non-PK column values — we can't distinguish explicitly-set
/// columns from default-valued ones. Treating the entire row as "explicitly
/// written" is safe: peers either have no row (accept) or lower CL (accept).
///
/// This uses col_version=1 (not 0 like clock_zero_fill) because these are real
/// local writes, not placeholders. With V1 wire format, col_version=0 entries
/// would still appear in the feed and be sent to peers as if they were real
/// changes — wasteful and semantically wrong for a local insert.
///
/// TODO(0.19): In V2-wire-only mode, we can optimize this with a single
/// INSERT INTO ... SELECT FROM v2_col_map statement (like clock_zero_fill but
/// with col_version=1), avoiding the per-column Rust loop.
fn write_clock_entries(
    _db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    v2_stmts: &mut crate::v2_stmts::V2Stmts,
    key: i64,
    db_version: i64,
    ts_val: i64,
) -> Result<ResultCode, String> {
    // For pk-only tables, use col_id=0 as sentinel. Otherwise, query actual
    // col_ids from v2_col_map (may have holes from dropped columns).
    let col_ids: vec::Vec<i64> = if tbl_info.non_pks.is_empty() {
        vec![0]
    } else {
        let mut lookup = v2_stmts.col_ids_all();
        let mut ids = vec::Vec::new();
        while lookup.step().map_err(|e| format!("step: {:?}", e))? == ResultCode::ROW {
            ids.push(lookup.column_int64(0));
        }
        ids
    };
    let mut stmt = v2_stmts.clock_set_initial();
    for col_id in col_ids {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id;
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
    if unsafe { crate::config::ensure_timestamp(ext_data).is_err() } {
        return Err("v2_after_update: timestamp not set — call crsql_set_ts() first or set default-ts".to_string());
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
    let key = match lookup_row_state(v2_stmts, pks_new, &hashed_pk, !skip_hash)? {
        RowState::Alive(key, _) => key,
        RowState::Dead(_) => return Err("row is dead (in tombstones) — cannot update a deleted row".to_string()),
        RowState::NotFound => return Err("row not found in v2_pks for update".to_string()),
    };

    // Resolve non_pks indices through v2_col_map. After DROP COLUMN the map
    // has holes, so the array index is not the packed col_id.
    let mut col_ids: vec::Vec<i64> = vec![];
    {
        let mut lookup = v2_stmts.col_id_lookup();
        for &col_idx in changed_col_indices {
            let col_name = tbl_info
                .non_pks
                .get(col_idx)
                .map(|c| c.name.as_str())
                .ok_or_else(|| "changed column index out of range".to_string())?;
            lookup
                .bind_text(1, col_name, Destructor::STATIC)
                .map_err(|e| format!("bind col_id_lookup: {:?}", e))?;
            match lookup
                .step()
                .map_err(|e| format!("step col_id_lookup: {:?}", e))?
            {
                ResultCode::ROW => col_ids.push(lookup.column_int64(0)),
                _ => {
                    return Err(format!(
                        "no col_id in v2_col_map for column {}",
                        col_name
                    ))
                }
            }
            lookup
                .reset()
                .map_err(|e| format!("reset col_id_lookup: {:?}", e))?;
            lookup
                .clear_bindings()
                .map_err(|e| format!("clear col_id_lookup: {:?}", e))?;
        }
    }

    let mut stmt = v2_stmts.clock_bump_version();
    for col_id in col_ids {
        let seq = bump_seq(ext_data);
        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id;
        stmt.bind_int64(1, cell_key).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(2, db_version).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int(3, seq).map_err(|e| format!("bind: {:?}", e))?;
        stmt.bind_int64(4, ts_val).map_err(|e| format!("bind: {:?}", e))?;
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
    if unsafe { crate::config::ensure_timestamp(ext_data).is_err() } {
        return Err("v2_after_delete: timestamp not set — call crsql_set_ts() first or set default-ts".to_string());
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
    let (key, cl) = match lookup_row_state(v2_stmts, pks_old, &hashed_pk, !skip_hash)? {
        RowState::Alive(key, cl) => (key, cl),
        // Row is already dead (in tombstones) or never tracked — nothing to do.
        RowState::Dead(_) | RowState::NotFound => return Ok(ResultCode::OK),
    };

    let new_cl = cl + 1;
    let seq = bump_seq(ext_data);
    let bind_err = |e: sqlite::ResultCode| format!("bind: {:?}", e);

    // Delete from v2_pks
    {
        let mut del = v2_stmts.pks_delete();
        del.bind_int64(1, key).map_err(bind_err)?;
        del.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Delete clock entries for this key
    {
        let mut del = v2_stmts.clock_delete_range();
        let base = key << consts::CRSQL_COL_ID_BITS as i64;
        del.bind_int64(1, base).map_err(bind_err)?;
        del.bind_int64(2, base | consts::CRSQL_COL_ID_MASK as i64).map_err(bind_err)?;
        del.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Insert tombstone
    {
        let mut ins = v2_stmts.tomb_insert_local();
        ins.bind_int(1, 0).map_err(bind_err)?;
        ins.bind_int64(2, db_version).map_err(bind_err)?;
        ins.bind_int(3, seq).map_err(bind_err)?;
        if skip_hash {
            ins.bind_value(4, pks_old[0]).map_err(bind_err)?;
        } else {
            ins.bind_blob(4, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(bind_err)?;
        }
        ins.bind_int64(5, new_cl).map_err(bind_err)?;
        ins.bind_int64(6, ts_val).map_err(bind_err)?;
        ins.step().map_err(|e| format!("step: {:?}", e))?;
    }

    // Insert tombstone PKs (hash mode only — skip_hash stores PK directly in tombstone)
    if !skip_hash {
        let mut ins = v2_stmts.tomb_pks_insert()
            .map_err(|e| format!("tomb_pks_insert: {:?}", e))?;
        ins.bind_blob(1, hashed_pk.as_ref().unwrap(), Destructor::STATIC).map_err(bind_err)?;
        for (i, pk) in pks_old.iter().enumerate() {
            ins.bind_value(i as i32 + 2, *pk).map_err(bind_err)?;
        }
        ins.step().map_err(|e| format!("step: {:?}", e))?;
    }

    Ok(ResultCode::OK)
}
