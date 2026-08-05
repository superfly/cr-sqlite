use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use core::mem;
use sqlite::Stmt;
use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, ResultCode, Value};

use crate::c::crsql_ExtData;
use crate::c::{crsql_Changes_vtab, CrsqlChangesColumn};
use crate::compare_values::crsql_compare_sqlite_values;
use crate::config;
use crate::db_version::{get_or_set_site_ordinal, insert_db_version};
use crate::pack_columns::bind_package_to_stmt;
use crate::pack_columns::{unpack_columns, ColumnValue};
use crate::stmt_cache::reset_cached_stmt;
use crate::tableinfo::{crsql_ensure_table_infos_are_up_to_date, TableInfo, SchemaVersion};
use crate::util::slab_rowid;
use crate::consts;

/**
 * did_cid_win does not take into account the causal length.
 * The expectation is that all causal length concerns have already been handle
 * via:
 * - early return because insert_cl < local_cl
 * - automatic win because insert_cl > local_cl
 * - come here to did_cid_win if insert_cl = local_cl
 */
fn did_cid_win(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    insert_tbl: &str,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    key: sqlite::int64,
    insert_val: *mut sqlite::value,
    insert_site_id: &[u8],
    col_name: &str,
    col_version: sqlite::int64,
    errmsg: *mut *mut c_char,
) -> Result<bool, ResultCode> {
    let col_vrsn_stmt_ref = tbl_info.get_col_version_stmt(db)?;
    let col_vrsn_stmt = col_vrsn_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let bind_result = col_vrsn_stmt
        .bind_int64(1, key)
        .and_then(|_| col_vrsn_stmt.bind_text(2, col_name, sqlite::Destructor::STATIC));
    if let Err(rc) = bind_result {
        reset_cached_stmt(col_vrsn_stmt.stmt)?;
        return Err(rc);
    }

    match col_vrsn_stmt.step() {
        Ok(ResultCode::ROW) => {
            let local_version = col_vrsn_stmt.column_int64(0);
            reset_cached_stmt(col_vrsn_stmt.stmt)?;
            // causal lengths are the same. Fall back to original algorithm.
            if col_version > local_version {
                return Ok(true);
            } else if col_version < local_version {
                return Ok(false);
            }
        }
        Ok(ResultCode::DONE) => {
            reset_cached_stmt(col_vrsn_stmt.stmt)?;
            // no rows returned
            // of course the incoming change wins if there's nothing there locally.
            return Ok(true);
        }
        Ok(rc) | Err(rc) => {
            reset_cached_stmt(col_vrsn_stmt.stmt)?;
            let err = CString::new("Bad return code when selecting local column version")?;
            unsafe { *errmsg = err.into_raw() };
            return Err(rc);
        }
    }

    // versions are equal
    // need to compare values
    let col_val_stmt_ref = tbl_info.get_col_value_stmt(db, col_name)?;
    let col_val_stmt = col_val_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let bind_result = bind_package_to_stmt(col_val_stmt.stmt, &unpacked_pks, 0);
    if let Err(rc) = bind_result {
        reset_cached_stmt(col_val_stmt.stmt)?;
        return Err(rc);
    }

    let step_result = col_val_stmt.step();
    match step_result {
        Ok(ResultCode::ROW) => {
            let local_value = col_val_stmt.column_value(0)?;
            let ret = crsql_compare_sqlite_values(insert_val, local_value);
            reset_cached_stmt(col_val_stmt.stmt)?;
            if ret == 0 && unsafe { (*ext_data).mergeEqualValues == 1 } {
                // values are the same (ret == 0) and the option to tie break on site_id is true
                let won = did_site_id_win(
                    db,
                    insert_tbl,
                    tbl_info,
                    key,
                    col_name,
                    insert_site_id,
                    errmsg,
                )?;
                return Ok(won);
            }
            return Ok(ret > 0);
        }
        _ => {
            // ResultCode::DONE would happen if clock values exist but actual values are missing.
            // should we just allow the insert anyway?
            reset_cached_stmt(col_val_stmt.stmt)?;
            let err = CString::new(format!(
                "could not find row to merge with for tbl {}",
                insert_tbl
            ))?;
            unsafe { *errmsg = err.into_raw() };
            return Err(ResultCode::ERROR);
        }
    }
}

fn did_site_id_win(
    db: *mut sqlite3,
    insert_tbl: &str,
    tbl_info: &TableInfo,
    key: sqlite::int64,
    col_name: &str,
    insert_site_id: &[u8],
    errmsg: *mut *mut c_char,
) -> Result<bool, ResultCode> {
    let col_site_id_stmt_ref = tbl_info.get_col_site_id_stmt(db)?;
    let col_site_id_stmt = col_site_id_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let bind_result = col_site_id_stmt
        .bind_int64(1, key)
        .and_then(|_| col_site_id_stmt.bind_text(2, col_name, sqlite::Destructor::STATIC));
    if let Err(rc) = bind_result {
        reset_cached_stmt(col_site_id_stmt.stmt)?;
        return Err(rc);
    }

    match col_site_id_stmt.step() {
        Ok(ResultCode::ROW) => {
            let local_site_id = col_site_id_stmt.column_blob(0)?;
            let ret = insert_site_id.cmp(local_site_id) as c_int;
            reset_cached_stmt(col_site_id_stmt.stmt)?;
            Ok(ret > 0)
        }
        Ok(ResultCode::DONE) => {
            reset_cached_stmt(col_site_id_stmt.stmt)?;
            let err = CString::new(format!(
                "could not find site_id for previous change, cr-sqlite clock table might be corrupt for tbl {}",
                insert_tbl
            ))?;
            unsafe { *errmsg = err.into_raw() };
            return Err(ResultCode::ERROR);
        }
        Ok(rc) | Err(rc) => {
            reset_cached_stmt(col_site_id_stmt.stmt)?;
            let err = CString::new("Bad return code when selecting local column site_id")?;
            unsafe { *errmsg = err.into_raw() };
            return Err(rc);
        }
    }
}

fn set_winner_clock(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    key: sqlite::int64,
    insert_col_name: &str,
    insert_col_vrsn: sqlite::int64,
    insert_db_vrsn: sqlite::int64,
    insert_site_id: &[u8],
    insert_seq: sqlite::int64,
    insert_ts: sqlite::int64,
) -> Result<sqlite::int64, ResultCode> {
    // set the site_id ordinal
    // get the returned ordinal
    // use that in place of insert_site_id in the metadata table(s)

    // on changes read, join to gather the proper site id.
    let ordinal = unsafe {
        if insert_site_id.is_empty() {
            None
        } else {
            Some(get_or_set_site_ordinal(ext_data, insert_site_id)?)
        }
    };

    let set_stmt_ref = tbl_info.get_set_winner_clock_stmt(db)?;
    let set_stmt = set_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let bind_result = set_stmt
        .bind_int64(1, key)
        .and_then(|_| set_stmt.bind_text(2, insert_col_name, sqlite::Destructor::STATIC))
        .and_then(|_| set_stmt.bind_int64(3, insert_col_vrsn))
        .and_then(|_| set_stmt.bind_int64(4, insert_db_vrsn))
        .and_then(|_| set_stmt.bind_int64(5, insert_seq))
        .and_then(|_| match ordinal {
            Some(ordinal) => set_stmt.bind_int64(6, ordinal),
            None => set_stmt.bind_null(6),
        })
        .and_then(|_| {
            let ts_str = format!("{}", insert_ts);
            set_stmt.bind_text(7, &ts_str, sqlite::Destructor::TRANSIENT)
        });

    if let Err(rc) = bind_result {
        reset_cached_stmt(set_stmt.stmt)?;
        return Err(rc);
    }

    let rowid = match set_stmt.step() {
        Ok(ResultCode::ROW) => {
            let rowid = set_stmt.column_int64(0);
            reset_cached_stmt(set_stmt.stmt)?;
            rowid
        }
        _ => {
            reset_cached_stmt(set_stmt.stmt)?;
            return Err(ResultCode::ERROR);
        }
    };

    Ok(rowid)
}

fn merge_sentinel_only_insert(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    key: sqlite::int64,
    remote_col_vrsn: sqlite::int64,
    remote_db_vsn: sqlite::int64,
    remote_site_id: &[u8],
    remote_seq: sqlite::int64,
    remote_ts: sqlite::int64,
) -> Result<sqlite::int64, ResultCode> {
    let merge_stmt_ref = tbl_info.get_merge_pk_only_insert_stmt(db)?;
    let merge_stmt = merge_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let rc = bind_package_to_stmt(merge_stmt.stmt, unpacked_pks, 0);
    if let Err(rc) = rc {
        reset_cached_stmt(merge_stmt.stmt)?;
        return Err(rc);
    }
    let rc = unsafe {
        (*ext_data)
            .pSetSyncBitStmt
            .step()
            .and_then(|_| merge_stmt.step())
    };

    unsafe { (*ext_data).pSetSyncBitStmt.reset()? };
    reset_cached_stmt(merge_stmt.stmt)?;

    let sync_rc = unsafe {
        let rc = (*ext_data).pClearSyncBitStmt.step();
        (*ext_data).pClearSyncBitStmt.reset()?;
        rc
    };

    if let Err(sync_rc) = sync_rc {
        return Err(sync_rc);
    }
    if let Err(rc) = rc {
        return Err(rc);
    }

    if rc.is_ok() {
        zero_clocks_on_resurrect(db, tbl_info, key)?;
        return set_winner_clock(
            db,
            ext_data,
            tbl_info,
            key,
            crate::c::INSERT_SENTINEL,
            remote_col_vrsn,
            remote_db_vsn,
            remote_site_id,
            remote_seq,
            remote_ts,
        );
    }

    Ok(-1)
}

fn zero_clocks_on_resurrect(
    db: *mut sqlite3,
    tbl_info: &TableInfo,
    key: sqlite::int64,
) -> Result<ResultCode, ResultCode> {
    let zero_stmt_ref = tbl_info.get_zero_clocks_on_resurrect_stmt(db)?;
    let zero_stmt = zero_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let ret = zero_stmt.bind_int64(1, key).and_then(|_| zero_stmt.step());
    reset_cached_stmt(zero_stmt.stmt)?;
    return ret;
}

unsafe fn merge_delete(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    key: sqlite::int64,
    remote_col_vrsn: sqlite::int64,
    remote_db_vrsn: sqlite::int64,
    remote_site_id: &[u8],
    remote_seq: sqlite::int64,
    remote_ts: sqlite::int64,
) -> Result<sqlite::int64, ResultCode> {
    let delete_stmt_ref = tbl_info.get_merge_delete_stmt(db)?;
    let delete_stmt = delete_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    if let Err(rc) = bind_package_to_stmt(delete_stmt.stmt, unpacked_pks, 0) {
        reset_cached_stmt(delete_stmt.stmt)?;
        return Err(rc);
    }
    let rc = (*ext_data)
        .pSetSyncBitStmt
        .step()
        .and_then(|_| delete_stmt.step());

    (*ext_data).pSetSyncBitStmt.reset()?;
    reset_cached_stmt(delete_stmt.stmt)?;

    let sync_rc = (*ext_data).pClearSyncBitStmt.step();

    (*ext_data).pClearSyncBitStmt.reset()?;
    if let Err(sync_rc) = sync_rc {
        return Err(sync_rc);
    }
    if let Err(rc) = rc {
        return Err(rc);
    }

    let ret = set_winner_clock(
        db,
        ext_data,
        tbl_info,
        key,
        crate::c::DELETE_SENTINEL,
        remote_col_vrsn,
        remote_db_vrsn,
        remote_site_id,
        remote_seq,
        remote_ts,
    )?;

    // Drop clocks _after_ setting the winner clock so we don't lose track of the max db_version!!
    // This must never come before `set_winner_clock`
    let drop_clocks_stmt_ref = tbl_info.get_merge_delete_drop_clocks_stmt(db)?;
    let drop_clocks_stmt = drop_clocks_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let rc = drop_clocks_stmt
        .bind_int64(1, key)
        .and_then(|_| drop_clocks_stmt.step());
    reset_cached_stmt(drop_clocks_stmt.stmt)?;
    rc?;

    return Ok(ret);
}

#[no_mangle]
pub unsafe extern "C" fn crsql_merge_insert(
    vtab: *mut sqlite::vtab,
    argc: c_int,
    argv: *mut *mut sqlite::value,
    rowid: *mut sqlite::int64,
    errmsg: *mut *mut c_char,
) -> c_int {
    match merge_insert(vtab, argc, argv, rowid, errmsg) {
        Err(rc) | Ok(rc) => rc as c_int,
    }
}

fn get_local_cl(
    db: *mut sqlite::sqlite3,
    tbl_info: &mut TableInfo,
    key: sqlite::int64,
) -> Result<sqlite::int64, ResultCode> {
    if let Some(cl) = tbl_info.get_cl(key) {
        return Ok(*cl);
    }

    let cl = {
        let local_cl_stmt_ref = tbl_info.get_local_cl_stmt(db)?;
        let local_cl_stmt = local_cl_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

        let rc = local_cl_stmt
            .bind_int64(1, key)
            .and_then(|_| local_cl_stmt.bind_int64(2, key));
        if let Err(rc) = rc {
            reset_cached_stmt(local_cl_stmt.stmt)?;
            return Err(rc);
        }

        let step_result = local_cl_stmt.step();
        match step_result {
            Ok(ResultCode::ROW) => {
                let ret = local_cl_stmt.column_int64(0);
                reset_cached_stmt(local_cl_stmt.stmt)?;
                ret
            }
            Ok(ResultCode::DONE) => {
                reset_cached_stmt(local_cl_stmt.stmt)?;
                0
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(local_cl_stmt.stmt)?;
                return Err(rc);
            }
        }
    };

    tbl_info.set_cl(key, cl);
    Ok(cl)
}

/// Post-merge processing shared by all V2 merge paths:
/// 1. Update db_version tracking (if site_id is present)
/// 2. Dual-write: copy V2 metadata to V1 metadata tables (if in dual-write mode)
unsafe fn post_v2_merge(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: Option<&Vec<ColumnValue>>,
    hashed_pk: &[u8],
    insert_site_id: &[u8],
    insert_db_vrsn: sqlite::int64,
) -> Result<(), ResultCode> {
    // Update db_version tracking
    if !insert_site_id.is_empty() {
        let _ = insert_db_version(ext_data, insert_site_id, insert_db_vrsn);
    }
    // Dual-write: copy V2 metadata to V1 metadata tables
    let mwv = unsafe { (*ext_data).metadataWriteVersion };
    if mwv == config::METADATA_VERSION_V2_AND_V1 {
        let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
        let (v2_key_opt, v2_cl) =
            v2_lookup_key_and_cl(db, &escaped, hashed_pk).unwrap_or((None, 0));
        let _ = v2_to_v1_mirror_metadata(
            db,
            ext_data,
            tbl_info,
            unpacked_pks,
            hashed_pk,
            v2_key_opt,
            v2_cl,
        );
    }
    Ok(())
}

unsafe fn merge_insert(
    vtab: *mut sqlite::vtab,
    argc: c_int,
    argv: *mut *mut sqlite::value,
    rowid: *mut sqlite::int64,
    errmsg: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    let tab = vtab.cast::<crsql_Changes_vtab>();
    let db = (*tab).db;

    let rc = crsql_ensure_table_infos_are_up_to_date(db, (*tab).pExtData, errmsg);
    if rc != ResultCode::OK as i32 {
        let err = CString::new("Failed to update CRR table information")?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    let args = sqlite::args!(argc, argv);
    let insert_tbl = args[2 + CrsqlChangesColumn::Tbl as usize];
    if insert_tbl.bytes() > crate::consts::MAX_TBL_NAME_LEN {
        let err = CString::new("crsql - table name exceeded max length")?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    let insert_tbl = insert_tbl.text();
    let insert_pks = args[2 + CrsqlChangesColumn::Pk as usize];
    let insert_col = args[2 + CrsqlChangesColumn::Cid as usize];
    let insert_col_vrsn_raw = args[2 + CrsqlChangesColumn::ColVrsn as usize];
    // V2 wire packed format: col_version is Text/Blob (packed), and col names
    // are null-separated — skip the single-column length check for those.
    let col_vrsn_type = insert_col_vrsn_raw.value_type();

    let insert_col = insert_col.text();
    let insert_val = args[2 + CrsqlChangesColumn::Cval as usize];
    let insert_db_vrsn = args[2 + CrsqlChangesColumn::DbVrsn as usize].int64();
    let insert_site_id = args[2 + CrsqlChangesColumn::SiteId as usize];
    let insert_cl = args[2 + CrsqlChangesColumn::Cl as usize].int64();
    let insert_seq_raw = args[2 + CrsqlChangesColumn::Seq as usize];
    let insert_ts_raw = args[2 + CrsqlChangesColumn::Ts as usize].int64();

    if insert_site_id.bytes() > crate::consts::SITE_ID_LEN {
        let err = CString::new("crsql - site id exceeded max length")?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    let insert_site_id = insert_site_id.blob();

    // Detect V2 wire packed row from the incoming data, not from local syncLogVersion config.
    // syncLogVersion controls what we emit, not what we accept. A node with sync-log-version=1
    // can receive V2 wire format changes from a peer with sync-log-version=2.
    // V1 wire always has INTEGER col_vrsn (raw c.col_version).
    // V2 wire always has BLOB col_vrsn (cast(group_concat(...) as blob)).
    // So col_vrsn type alone distinguishes the two formats.
    // Tombstone rows (cid='-1' or cid='-2') are never packed regardless of wire format.
    let is_v2_hash_tombstone = insert_col == crate::consts::V2_HASH_TOMBSTONE_CID;
    let is_tombstone = insert_col == crate::c::DELETE_SENTINEL || is_v2_hash_tombstone;
    let is_v2_wire_packed = !is_tombstone
        && (col_vrsn_type == sqlite::ColumnType::Text
            || col_vrsn_type == sqlite::ColumnType::Blob);

    // Skip column name length check for V2 packed rows (col names are null-separated)
    if !is_v2_wire_packed && insert_col.len() > crate::consts::MAX_TBL_NAME_LEN as usize {
        let err = CString::new("crsql - column name exceeded max length")?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    // Reject V2 wire format changes if this node is not on V2 metadata.
    // Per spec: "Nodes with metadata-use-version set to 1 will emit an error
    // if they receive V2 wire format changes."
    if (is_v2_wire_packed || is_v2_hash_tombstone)
        && (*(*tab).pExtData).metadataUseVersion != consts::META_USE_V2
    {
        let err = CString::new(
            "crsql - received V2 wire format change but metadata-use-version is not 2. \
             Set metadata-use-version=2 before accepting V2 wire format changes.",
        )?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    // Look up table info — needed by all branches below.
    let mut tbl_infos = mem::ManuallyDrop::new(Box::from_raw(
        (*(*tab).pExtData).tableInfos as *mut Vec<TableInfo>,
    ));
    let tbl_info_index = tbl_infos.iter().position(|x| x.tbl_name == insert_tbl);
    if tbl_info_index.is_none() {
        let err = CString::new(format!(
            "crsql - could not find the schema information for table {}",
            insert_tbl
        ))?;
        *errmsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }
    let tbl_info_index = tbl_info_index.unwrap();
    let tbl_info = &mut tbl_infos[tbl_info_index];

    // V2 clock tables require a non-zero ts. If we get a legacy row with
    // ts=0 then we fall back to the current ts. Error early if current ts is not set.
    if unsafe { (*(*tab).pExtData).timestamp } == 0 {
        let err = CString::new("crsql - timestamp not set — call crsql_set_ts() before syncing changes")?;
        unsafe { *errmsg = err.into_raw() };
        return Err(ResultCode::ERROR);
    }

    // If incoming ts is 0, fall back to current set timestamp.
    let insert_ts = if insert_ts_raw > 0 {
        insert_ts_raw
    } else {
        unsafe { (*(*tab).pExtData).timestamp as i64 }
    };

    // V1-only write mode: use the old V1 merge path
    let mwv = unsafe { (*(*tab).pExtData).metadataWriteVersion };
    if mwv == config::METADATA_VERSION_V1 {
        return v1_merge_insert(
            db,
            (*tab).pExtData,
            tbl_info,
            insert_tbl,
            insert_pks,
            insert_col,
            insert_col_vrsn_raw,
            insert_val,
            insert_db_vrsn,
            insert_site_id,
            insert_cl,
            insert_seq_raw,
            insert_ts_raw,
            rowid,
            tbl_info_index,
            errmsg,
        );
    }

    // --- V2 or V2AndV1 mode ---
    // Convert any V1 wire format to V2 wire format on the fly, then dispatch
    // to v2_packed_merge or v2_merge_insert_hash_tombstone.

    // Compute common PK data.
    // V2 hash tombstone: pks blob IS the hashed_pk, no unpacked pks needed.
    let (unpacked_pks_opt, hashed_pk): (Option<Vec<ColumnValue>>, Vec<u8>) = if is_v2_hash_tombstone {
        (None, insert_pks.blob().to_vec())
    } else {
        let packed_pks = insert_pks.blob();
        let unpacked_pks = unpack_columns(&packed_pks)?;
        let hashed_pk = crate::hash_pk::hash_packed_blob(&packed_pks);
        (Some(unpacked_pks), hashed_pk)
    };

    // Hydrate V2 from V1 metadata in dual-write mode for V1 wire format
    if !is_v2_wire_packed && !is_v2_hash_tombstone && mwv == config::METADATA_VERSION_V2_AND_V1 {
        if let Some(ref unpacked_pks) = unpacked_pks_opt {
            v1_to_v2_hydrate_row(
                db,
                (*tab).pExtData,
                tbl_info,
                unpacked_pks,
                &hashed_pk,
            )?;
        }
    }

    // Delete if V2 hash tombstone, or V1 wire with even CL.
    let is_delete = is_v2_hash_tombstone || (!is_v2_wire_packed && insert_cl % 2 == 0);

    let result = if is_delete {
        let col_vrsn = insert_col_vrsn_raw.int64();
        let seq = insert_seq_raw.int64();

        v2_merge_insert_hash_tombstone(
            db,
            (*tab).pExtData,
            tbl_info,
            &hashed_pk,
            insert_tbl,
            insert_col,
            insert_val,
            col_vrsn,
            insert_db_vrsn,
            insert_site_id,
            insert_cl,
            seq,
            insert_ts,
            rowid,
            tbl_info_index,
            errmsg,
        )
    } else {
        // Packed merge: compute vectors based on wire format
        let (col_names, col_vrsns, seqs, unpacked_vals) = if is_v2_wire_packed {
            let col_names: Vec<&str> = insert_col.split('\0').collect();
            let col_vrsns: Vec<i64> = insert_col_vrsn_raw.text().split('\0').map(|s| s.parse::<i64>().unwrap_or(0)).collect();
            let seqs: Vec<i64> = insert_seq_raw.text().split('\0').map(|s| s.parse::<i64>().unwrap_or(0)).collect();
            let unpacked_vals = unpack_columns(insert_val.blob())?;

            let n_cols = col_names.len();
            if col_vrsns.len() != n_cols || seqs.len() != n_cols || unpacked_vals.len() != n_cols {
                let err = CString::new(format!(
                    "crsql - V2 wire packed row has mismatched lengths: col_names={}, col_vrsns={}, seqs={}, vals={}",
                    n_cols, col_vrsns.len(), seqs.len(), unpacked_vals.len()
                ))?;
                *errmsg = err.into_raw();
                return Err(ResultCode::ERROR);
            }
            (col_names, col_vrsns, seqs, unpacked_vals)
        } else {
            // V1 wire format: convert to single-element (or empty) vectors
            let insert_col_vrsn = insert_col_vrsn_raw.int64();
            let insert_seq = insert_seq_raw.int64();

            if insert_col == crate::c::INSERT_SENTINEL {
                (Vec::new(), Vec::new(), Vec::new(), Vec::new())
            } else {
                let col_val = sqlite_value_to_column_value(insert_val);
                (
                    vec![insert_col],
                    vec![insert_col_vrsn],
                    vec![insert_seq],
                    vec![col_val],
                )
            }
        };

        v2_packed_merge(
            db,
            (*tab).pExtData,
            tbl_info,
            unpacked_pks_opt.as_ref().unwrap(),
            &hashed_pk,
            &col_names,
            &col_vrsns,
            &seqs,
            &unpacked_vals,
            insert_db_vrsn,
            insert_site_id,
            insert_cl,
            insert_ts,
        )
    };

    // Post-merge: db_version + dual-write V1 metadata
    if result.is_ok() {
        let _ = post_v2_merge(
            db,
            (*tab).pExtData,
            tbl_info,
            unpacked_pks_opt.as_ref(),
            &hashed_pk,
            insert_site_id,
            insert_db_vrsn,
        );
    }

    return result;
}

/// V1 merge path: handles incoming changes for tables using V1 metadata only.
#[allow(clippy::too_many_arguments)]
unsafe fn v1_merge_insert(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &mut TableInfo,
    insert_tbl: &str,
    insert_pks: *mut sqlite::value,
    insert_col: &str,
    insert_col_vrsn_raw: *mut sqlite::value,
    insert_val: *mut sqlite::value,
    insert_db_vrsn: sqlite::int64,
    insert_site_id: &[u8],
    insert_cl: sqlite::int64,
    insert_seq_raw: *mut sqlite::value,
    insert_ts: sqlite::int64,
    rowid: *mut sqlite::int64,
    tbl_info_index: usize,
    errmsg: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    let insert_col_vrsn = insert_col_vrsn_raw.int64();
    let insert_seq = insert_seq_raw.int64();
    let unpacked_pks = unpack_columns(insert_pks.blob())?;

    // Get or create key as the first thing we do.
    // We'll need the key for all later operations.
    let key = tbl_info.get_or_create_key(db, &unpacked_pks)?;

    let local_cl = get_local_cl(db, tbl_info, key)?;

    // We can ignore all updates from older causal lengths.
    // They won't win at anything.
    let res = (|| {
        if insert_cl < local_cl {
            return Ok(ResultCode::OK);
        }

        let is_delete = insert_cl % 2 == 0;
        // Resurrect or update to latest cl.
        // The current node might have missed the delete preceeding this causal length
        // in out-of-order delivery setups but we still call it a resurrect as special
        // handling needs to happen in the "alive -> missed_delete -> alive" case.
        let needs_resurrect = insert_cl > local_cl && insert_cl % 2 == 1;
        let row_exists_locally = local_cl != 0;
        let is_sentinel_only = crate::c::INSERT_SENTINEL == insert_col;

        if is_delete {
            // We got a delete event but we've already processed a delete at that version.
            // Just bail.
            if insert_cl == local_cl {
                if unsafe { (*ext_data).mergeEqualValues == 1 }
                    && did_site_id_win(
                        db,
                        insert_tbl,
                        &tbl_info,
                        key,
                        insert_col,
                        insert_site_id,
                        errmsg,
                    )?
                {
                    // here we set the same winner for the clock if incoming site_id won
                    set_winner_clock(
                        db,
                        ext_data,
                        &tbl_info,
                        key,
                        insert_col,
                        insert_col_vrsn,
                        insert_db_vrsn,
                        insert_site_id,
                        insert_seq,
                        insert_ts,
                    )?;
                }
                return Ok(ResultCode::OK);
            }
            // else, it is a delete and the cl is > than ours. Drop the row.
            let merge_result = merge_delete(
                db,
                ext_data,
                &tbl_info,
                &unpacked_pks,
                key,
                insert_col_vrsn,
                insert_db_vrsn,
                insert_site_id,
                insert_seq,
                insert_ts,
            );
            match merge_result {
                Err(rc) => {
                    return Err(rc);
                }
                Ok(inner_rowid) => {
                    (*ext_data).rowsImpacted += 1;
                    *rowid = slab_rowid(tbl_info_index as i32, inner_rowid);
                    return Ok(ResultCode::OK);
                }
            }
        }

        /*
        || crsql_columnExists(
                // TODO: only safe because we _know_ this is actually a cstr
                insert_col.as_ptr() as *const c_char,
                (*tbl_info).nonPks,
                (*tbl_info).nonPksLen,
            ) == 0
         */
        if is_sentinel_only {
            // If it is a sentinel but the local_cl already matches, nothing to do
            // as the local sentinel already has the same data!
            if insert_cl == local_cl {
                return Ok(ResultCode::OK);
            }
            let inner_rowid = merge_sentinel_only_insert(
                db,
                ext_data,
                &tbl_info,
                &unpacked_pks,
                key,
                insert_col_vrsn,
                insert_db_vrsn,
                insert_site_id,
                insert_seq,
                insert_ts,
            )?;
            // a success & rowid of -1 means the merge was a no-op
            if inner_rowid != -1 {
                (*ext_data).rowsImpacted += 1;
                *rowid = slab_rowid(tbl_info_index as i32, inner_rowid);
            }
            return Ok(ResultCode::OK);
        }

        // we got a causal length which would resurrect the row.
        // In an in-order delivery situation then `sentinel_only` would have already resurrected the row
        // In out-of-order delivery, we need to resurrect the row as soon as we get a value
        // which should resurrect the row. I.e., don't wait on the sentinel value to resurrect the row!
        // If the row does not exist locally and the insert_cl is > 1 then we need to create a sentinel to record the insert cl.
        // Not doing so will cause us to assume a cl of 1.
        if needs_resurrect && (row_exists_locally || (!row_exists_locally && insert_cl > 1)) {
            // this should work -- same as `merge_sentinel_only_insert` except we're not done once we do it
            // and the version to set to is the cl not col_vrsn of current insert
            let inner_rowid = merge_sentinel_only_insert(
                db,
                ext_data,
                &tbl_info,
                &unpacked_pks,
                key,
                insert_cl,
                insert_db_vrsn,
                insert_site_id,
                insert_seq,
                insert_ts,
            )?;
            // a success & rowid of -1 means the merge was a no-op
            if inner_rowid != -1 {
                (*ext_data).rowsImpacted += 1;
                *rowid = slab_rowid(tbl_info_index as i32, inner_rowid);
            }
        }

        // we can short-circuit via needs_resurrect
        // given the greater cl automatically means a win.
        // or if we realize that the row does not exist locally at all.
        let does_cid_win = needs_resurrect
            || !row_exists_locally
            || did_cid_win(
                db,
                ext_data,
                insert_tbl,
                &tbl_info,
                &unpacked_pks,
                key,
                insert_val,
                insert_site_id,
                insert_col,
                insert_col_vrsn,
                errmsg,
            )?;

        if !does_cid_win {
            // doesCidWin == 0? compared against our clocks, nothing wins. OK and
            // Done.
            return Ok(ResultCode::OK);
        }

        // TODO: this is all almost identical between all three merge cases!
        let merge_stmt_ref = tbl_info.get_merge_insert_stmt(db, insert_col)?;
        let merge_stmt = merge_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

        let bind_result = bind_package_to_stmt(merge_stmt.stmt, &unpacked_pks, 0)
            .and_then(|_| merge_stmt.bind_value(unpacked_pks.len() as i32 + 1, insert_val))
            .and_then(|_| merge_stmt.bind_value(unpacked_pks.len() as i32 + 2, insert_val));
        if let Err(rc) = bind_result {
            reset_cached_stmt(merge_stmt.stmt)?;
            return Err(rc);
        }

        let rc = (*ext_data)
            .pSetSyncBitStmt
            .step()
            .and_then(|_| (*ext_data).pSetSyncBitStmt.reset())
            .and_then(|_| merge_stmt.step());

        reset_cached_stmt(merge_stmt.stmt)?;

        let sync_rc = (*ext_data)
            .pClearSyncBitStmt
            .step()
            .and_then(|_| (*ext_data).pClearSyncBitStmt.reset());

        rc?;
        sync_rc?;

        let merge_result = set_winner_clock(
            db,
            ext_data,
            &tbl_info,
            key,
            insert_col,
            insert_col_vrsn,
            insert_db_vrsn,
            insert_site_id,
            insert_seq,
            insert_ts,
        );
        match merge_result {
            Err(rc) => {
                return Err(rc);
            }
            Ok(inner_rowid) => {
                (*ext_data).rowsImpacted += 1;
                *rowid = slab_rowid(tbl_info_index as i32, inner_rowid);
                return Ok(ResultCode::OK);
            }
        }
    })();

    // Update the received db_version whether the change won or not.
    if res.is_ok() && !insert_site_id.is_empty() {
        if let Err(rc) = insert_db_version(ext_data, insert_site_id, insert_db_vrsn) {
            let err = CString::new(format!(
                "Unable to insert db version {} for site id {:?}: {:?}",
                insert_db_vrsn, insert_site_id, rc
            ))?;
            *errmsg = err.into_raw();
            return Err(rc);
        }

        // a bigger cl always wins
        if insert_cl > local_cl {
            tbl_info.set_cl(key, insert_cl);
        }
    }

    res
}

/// Ensure an alive row exists in v2_pks at incoming_cl.
/// Handles: stale CL bail, skipped-delete cleanup, resurrection cleanup, new row creation.
/// Returns (local_key, local_cl) where local_cl is the CL *before* any modification.
/// Returns None if incoming_cl < local_cl (stale, caller should bail).
unsafe fn v2_ensure_alive_row_at_cl(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    hashed_pk: &[u8],
    incoming_cl: i64,
    incoming_db_version: i64,
    incoming_site_id: i64,
) -> Result<Option<(i64, i64)>, ResultCode> {
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    let (local_key_opt, local_cl) = v2_lookup_key_and_cl(db, &escaped, hashed_pk)?;

    if incoming_cl < local_cl {
        return Ok(None);
    }

    let local_key = if incoming_cl > local_cl {
        if let Some(key) = local_key_opt {
            // Row exists in v2_pks but CL jumped — we skipped a delete.
            // Nuke local clocks, v2_pks, and base table row before re-creating.
            v2_nuke_local_row(db, ext_data, &escaped, key, unpacked_pks, tbl_info)?;
        } else if local_cl > 0 {
            // Was in tombstones — resurrection. Clean up tombstone entries.
            v2_nuke_tombstone(db, &escaped, hashed_pk)?;
        }
        // Create fresh v2_pks entry with new CL
        let new_key = v2_insert_pk_row(db, ext_data, &escaped, tbl_info, unpacked_pks, hashed_pk, incoming_cl)?;
        // Create zero-version clock entries for all mapped columns so they appear in sync logs.
        // Use the incoming change's site_id and db_version since it created this row.
        // seq=0 for placeholders; the packed column updates will overwrite specific entries.
        let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
        let base = new_key << col_id_bits;
        let clock_sql = format!(
            "INSERT INTO \"{}{}\" (cell_key, col_version, site_id, db_version, seq, ts)\n             SELECT ? + col_id, 0, ?, ?, 0, crsql_get_ts()\n             FROM \"{}{}\"\0",
            escaped, consts::V2_CLOCK_SUFFIX,
            escaped, consts::V2_COL_MAP_SUFFIX
        );
        let clock_stmt = db.prepare_v2(&clock_sql)?;
        clock_stmt.bind_int64(1, base)?;
        clock_stmt.bind_int64(2, incoming_site_id)?;
        clock_stmt.bind_int64(3, incoming_db_version)?;
        clock_stmt.step()?;
        new_key
    } else {
        // incoming_cl == local_cl — row must already exist
        local_key_opt.unwrap_or(0)
    };

    Ok(Some((local_key, local_cl)))
}

/// Look up key and CL from v2_pks or v2_tombstones by hashed_pk.
/// A row is in exactly one of v2_pks (alive) or v2_tombstones (dead), never both.
unsafe fn v2_lookup_key_and_cl(
    db: *mut sqlite3,
    escaped: &str,
    hashed_pk: &[u8],
) -> Result<(Option<i64>, i64), ResultCode> {
    let sql = format!(
        "SELECT __crsql_key, cl FROM \"{}{}\" WHERE hashed_pk = ? \
         UNION ALL \
         SELECT NULL, cl FROM \"{}{}\" WHERE hashed_pk = ? \
         LIMIT 1\0",
        escaped, consts::V2_PKS_SUFFIX,
        escaped, consts::V2_TOMBSTONES_SUFFIX
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
    stmt.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        let key = stmt.column_int64(0);
        let cl = stmt.column_int64(1);
        // key is NULL if the row was found in v2_tombstones
        if stmt.column_type(0)? == sqlite::ColumnType::Null {
            return Ok((None, cl));
        }
        return Ok((Some(key), cl));
    }

    // No existing entry
    Ok((None, 0))
}

/// On-demand V1→V2 hydration for a single row.
/// In dual-write mode with incomplete migration, a row may exist in V1 tables
/// but not yet in V2. This function migrates that row's V1 metadata to V2
/// so the V2 merge/local-write paths can see the correct CL and clock state.
///
/// `unpacked_pks` is used to look up the V1 key. For rowid-key integer PK tables, the
/// first PK column should be the rowid alias value.
/// `hashed_pk` is the pre-computed hash of the PKs for V2 table insertion.
pub unsafe fn v1_to_v2_hydrate_row(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    hashed_pk: &[u8],
) -> Result<(), ResultCode> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    if unsafe { (*ext_data).timestamp } == 0 {
        crate::debug::debug_log("v1_to_v2_hydrate_row: timestamp not set — call crsql_set_ts() first");
        return Err(ResultCode::ERROR);
    }
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let ts_fallback = unsafe { (*ext_data).timestamp as i64 };

    // Guard: if V2 already has an entry for this row, skip hydration.
    // The row was either already migrated or written via dual-write triggers.
    let (existing_v2_key, existing_v2_cl) = v2_lookup_key_and_cl(db, &escaped, hashed_pk)?;
    if existing_v2_key.is_some() || existing_v2_cl != 0 {
        return Ok(());
    }

    // 1. Look up V1 key (SELECT only — do not create)
    let v1_key = {
        let stmt_ref = tbl_info.get_select_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
        let result = stmt.step();
        let v1_key = match result {
            Ok(ResultCode::ROW) => Some(stmt.column_int64(0)),
            _ => None,
        };
        reset_cached_stmt(stmt.stmt)?;
        match v1_key {
            Some(k) => k,
            None => return Ok(()), // No V1 entry — nothing to hydrate
        }
    };

    // 2. Look up V1 CL from sentinel clock entry
    let (v1_cl, is_dead) = {
        let sql = format!(
            "SELECT col_version FROM \"{}__crsql_clock\" WHERE key = ? AND col_name = '{}'\0",
            escaped, crate::c::DELETE_SENTINEL
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_int64(1, v1_key)?;
        if stmt.step()? == ResultCode::ROW {
            let cl = stmt.column_int64(0);
            (cl, cl % 2 == 0)
        } else {
            // No sentinel — row is alive at CL=1 (or whatever clocks exist)
            // Check if any clock entries exist at all
            let sql = format!(
                "SELECT 1 FROM \"{}__crsql_clock\" WHERE key = ? LIMIT 1\0",
                escaped
            );
            let stmt = db.prepare_v2(&sql)?;
            stmt.bind_int64(1, v1_key)?;
            if stmt.step()? == ResultCode::ROW {
                (1, false) // Alive with no explicit sentinel → CL=1
            } else {
                return Ok(()); // No V1 metadata at all — nothing to hydrate
            }
        }
    };

    if is_dead {
        // 3a. Row is dead — insert into v2_tombstones + v2_tombstone_pks
        // Get tombstone metadata from V1 sentinel
        let sql = format!(
            "SELECT site_id, db_version, seq, ts FROM \"{}__crsql_clock\" WHERE key = ? AND col_name = '{}'\0",
            escaped, crate::c::DELETE_SENTINEL
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_int64(1, v1_key)?;
        if stmt.step()? == ResultCode::ROW {
            let site_id = stmt.column_int64(0);
            let db_version = stmt.column_int64(1);
            let seq = stmt.column_int64(2);
            let ts = stmt.column_int64(3);
            let ts = if ts > 0 { ts } else { ts_fallback };

            // Insert tombstone
            let sql = format!(
                "INSERT OR REPLACE INTO \"{}{}\" (site_id, db_version, seq, hashed_pk, cl, ts) VALUES (?, ?, ?, ?, ?, ?)\0",
                escaped, consts::V2_TOMBSTONES_SUFFIX
            );
            let ins_stmt = db.prepare_v2(&sql)?;
            ins_stmt.bind_int64(1, site_id)?;
            ins_stmt.bind_int64(2, db_version)?;
            ins_stmt.bind_int64(3, seq)?;
            ins_stmt.bind_blob(4, hashed_pk, sqlite::Destructor::STATIC)?;
            ins_stmt.bind_int64(5, v1_cl)?;
            ins_stmt.bind_int64(6, ts)?;
            ins_stmt.step()?;

            // Insert tombstone PKs
            let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
            let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
            let sql = format!(
                "INSERT OR REPLACE INTO \"{}{}\" ({}, hashed_pk) VALUES ({}, ?)\0",
                escaped, consts::V2_TOMBSTONE_PKS_SUFFIX,
                pk_cols.join(", "),
                placeholders.join(", ")
            );
            let ins_stmt = db.prepare_v2(&sql)?;
            bind_package_to_stmt(ins_stmt.stmt, unpacked_pks, 0)?;
            ins_stmt.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
            ins_stmt.step()?;
        }
    } else {
        // 3b. Row is alive — insert into v2_pks, then copy clock entries to v2_clock
        let v2_key = if tbl_info.uses_rowid_key {
            let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
            let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
            let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
            let sql = format!(
                "SELECT \"{}\" FROM \"{}\" WHERE {}\0",
                alias, escaped,
                pk_cols.iter().zip(placeholders.iter())
                    .map(|(c, p)| format!("\"{}\" = {}", c, p))
                    .collect::<Vec<_>>().join(" AND ")
            );
            let stmt = db.prepare_v2(&sql)?;
            bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
            if stmt.step()? != ResultCode::ROW {
                return Ok(()); // Row not found in base table — skip hydration
            }
            let rowid_val = stmt.column_int64(0);
            let sql = format!(
                "INSERT INTO \"{}{}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, ?)\0",
                escaped, consts::V2_PKS_SUFFIX
            );
            let stmt = db.prepare_v2(&sql)?;
            stmt.bind_int64(1, rowid_val)?;
            stmt.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
            stmt.bind_int64(3, v1_cl)?;
            stmt.step()?;
            rowid_val
        } else {
            // Non-rowid table: insert PK columns into v2_pks
            let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
            let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
            let sql = format!(
                "INSERT INTO \"{}{}\" ({}, hashed_pk, cl) VALUES ({}, ?, ?) RETURNING __crsql_key\0",
                escaped, consts::V2_PKS_SUFFIX,
                pk_cols.join(", "),
                placeholders.join(", ")
            );
            let stmt = db.prepare_v2(&sql)?;
            bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
            stmt.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
            stmt.bind_int64(unpacked_pks.len() as i32 + 2, v1_cl)?;
            stmt.step()?;
            stmt.column_int64(0)
        };

        // Copy clock entries from V1 to V2
        let sql = format!(
            "INSERT INTO \"{}{}\" (cell_key, col_version, site_id, db_version, seq, ts)
             SELECT (? << {} | m.col_id), c.col_version, c.site_id, c.db_version, c.seq,
               CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END
             FROM \"{}__crsql_clock\" c
             JOIN \"{}{}\" m ON c.col_name = m.col_name
             WHERE c.key = ?\0",
            escaped, consts::V2_CLOCK_SUFFIX,
            col_id_bits,
            escaped,
            escaped, consts::V2_COL_MAP_SUFFIX
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_int64(1, v2_key)?;
        stmt.bind_int64(2, v1_key)?;
        stmt.step()?;
    }

    Ok(())
}

/// Convenience wrapper for local write paths that have raw sqlite::value pointers.
/// Converts PKs to ColumnValue, computes hashed_pk, and calls v1_to_v2_hydrate_row.
pub unsafe fn v1_to_v2_hydrate_row_from_values(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    pks: &[*mut sqlite::value],
) -> Result<(), ResultCode> {
    let unpacked_pks: Vec<ColumnValue> = pks.iter().map(|v| sqlite_value_to_column_value(*v)).collect();
    let packed = crate::pack_columns::pack_column_values(&unpacked_pks)?;
    let hashed_pk = crate::hash_pk::hash_packed_blob(&packed);
    v1_to_v2_hydrate_row(db, ext_data, tbl_info, &unpacked_pks, &hashed_pk)
}

/// Insert a row into the base table with PK columns only (OR IGNORE).
/// Sync bit is set to suppress after_insert triggers.
unsafe fn v2_insert_base_row(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    escaped: &str,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
) -> Result<(), ResultCode> {
    let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
    let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
    let base_sql = format!(
        "INSERT INTO \"{}\" ({}) VALUES ({})\0",
        escaped,
        pk_cols.join(", "),
        placeholders.join(", ")
    );
    (*ext_data).pSetSyncBitStmt.step()?;
    (*ext_data).pSetSyncBitStmt.reset()?;
    let base_stmt = db.prepare_v2(&base_sql)?;
    bind_package_to_stmt(base_stmt.stmt, unpacked_pks, 0)?;
    base_stmt.step()?;
    reset_cached_stmt(base_stmt.stmt)?;
    (*ext_data).pClearSyncBitStmt.step()?;
    (*ext_data).pClearSyncBitStmt.reset()?;
    Ok(())
}

/// Insert a new PK row into v2_pks and return the __crsql_key.
/// For rowid-key tables: INSERT into base table first, get the rowid,
/// then use it as __crsql_key.
/// For non-rowid tables: INSERT into base table, then INSERT PK columns +
/// hashed_pk + cl into v2_pks (auto-increment __crsql_key) with RETURNING.
unsafe fn v2_insert_pk_row(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    escaped: &str,
    tbl_info: &TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    hashed_pk: &[u8],
    cl: i64,
) -> Result<i64, ResultCode> {
    // First create the row in the base table
    v2_insert_base_row(db, ext_data, escaped, tbl_info, unpacked_pks)?;

    // Then insert the PK row into v2_pks
    if tbl_info.uses_rowid_key {
        let rowid = sqlite::last_insert_rowid(db);
        let sql = format!(
            "INSERT INTO \"{}{}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, ?)\0",
            escaped,
            consts::V2_PKS_SUFFIX,
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_int64(1, rowid)?;
        stmt.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
        stmt.bind_int64(3, cl)?;
        stmt.step()?;
        Ok(rowid)
    } else {
        let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
        let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
        let sql = format!(
            "INSERT INTO \"{}{}\" ({}, hashed_pk, cl) VALUES ({}, ?, ?) RETURNING __crsql_key\0",
            escaped,
            consts::V2_PKS_SUFFIX,
            pk_cols.join(", "),
            placeholders.join(", ")
        );
        let stmt = db.prepare_v2(&sql)?;
        bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
        stmt.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
        stmt.bind_int64(unpacked_pks.len() as i32 + 2, cl)?;
        stmt.step()?;
        Ok(stmt.column_int64(0))
    }
}

/// Get col_id from v2_col_map by col_name
unsafe fn v2_get_col_id(
    db: *mut sqlite3,
    escaped: &str,
    col_name: &str,
) -> Result<Option<i64>, ResultCode> {
    let sql = format!(
        "SELECT col_id FROM \"{}{}\" WHERE col_name = ?\0",
        escaped, consts::V2_COL_MAP_SUFFIX
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_text(1, col_name, sqlite::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        return Ok(Some(stmt.column_int64(0)));
    }
    Ok(None)
}

/// Handle V2 wire tombstone (cid=-2): we only have hashed_pk, not packed PK values.
/// Look up PK values from local v2_pks if the row exists there.
/// If the row doesn't exist locally, insert a bare tombstone with hash only.
#[allow(clippy::too_many_arguments)]
unsafe fn v2_merge_insert_hash_tombstone(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &mut TableInfo,
    hashed_pk: &[u8],
    insert_tbl: &str,
    _insert_col: &str,
    _insert_val: *mut sqlite::value,
    _insert_col_vrsn: sqlite::int64,
    insert_db_vrsn: sqlite::int64,
    insert_site_id: &[u8],
    insert_cl: sqlite::int64,
    insert_seq: sqlite::int64,
    insert_ts: sqlite::int64,
    _rowid: *mut sqlite::int64,
    _tbl_info_index: usize,
    _errmsg: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    // ts check is done at the top of merge_insert
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);

    // Look up key and CL from v2_pks (alive) or v2_tombstones (dead)
    let (local_key, local_cl) = v2_lookup_key_and_cl(db, &escaped, hashed_pk)?;

    // Bail early if incoming CL can't beat local CL
    if insert_cl < local_cl {
        return Ok(ResultCode::OK);
    }

    // If insert_cl == local_cl then the row should be already deleted
    // If insert_cl > local_cl then this deletion always wins
    // Therefore we can always run this upsert
    let merge_equal = unsafe { (*ext_data).mergeEqualValues };
    let where_clause = if merge_equal == 1 {
        format!(
            "WHERE excluded.cl > \"{}{}\".cl \
             OR (excluded.cl = \"{}{}\".cl \
                 AND ? > (SELECT site_id FROM crsql_site_id WHERE ordinal = \"{}{}\".site_id))",
            escaped, consts::V2_TOMBSTONES_SUFFIX,
            escaped, consts::V2_TOMBSTONES_SUFFIX,
            escaped, consts::V2_TOMBSTONES_SUFFIX
        )
    } else {
        format!(
            "WHERE excluded.cl > \"{}{}\".cl",
            escaped, consts::V2_TOMBSTONES_SUFFIX
        )
    };
    let sql = format!(
        "INSERT INTO \"{}{}\" (site_id, db_version, seq, hashed_pk, cl, ts) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(hashed_pk) DO UPDATE SET \
         site_id = excluded.site_id, \
         db_version = excluded.db_version, \
         seq = excluded.seq, \
         cl = excluded.cl, \
         ts = excluded.ts \
         {}\0",
        escaped, consts::V2_TOMBSTONES_SUFFIX,
        where_clause
    );
    let stmt = db.prepare_v2(&sql)?;
    let site_ordinal = if insert_site_id.is_empty() {
        0
    } else {
        get_or_set_site_ordinal(ext_data, insert_site_id)?
    };
    stmt.bind_int64(1, site_ordinal as i64)?;
    stmt.bind_int64(2, insert_db_vrsn)?;
    stmt.bind_int64(3, insert_seq)?;
    stmt.bind_blob(4, hashed_pk, sqlite::Destructor::STATIC)?;
    stmt.bind_int64(5, insert_cl)?;
    stmt.bind_int64(6, insert_ts)?;
    if merge_equal == 1 {
        stmt.bind_blob(7, insert_site_id, sqlite::Destructor::STATIC)?;
    }
    stmt.step()?;

    // If the row was alive, nuke its local state (clocks, v2_pks, base table row).
    // Also save PK values into v2_tombstone_pks for future lookups.
    if let Some(local_key) = local_key {
        let pk_col_names: Vec<String> = tbl_info.pks.iter().map(|c| c.name.clone()).collect();
        let pk_cols: Vec<&str> = pk_col_names.iter().map(|s| s.as_str()).collect();

        // Look up PK values once — used for both tombstone_pks insert and v2_nuke_local_row.
        // For rowid-key tables, PKs live in the base table (v2_pks only has __crsql_key).
        // For non-rowid tables, PKs are stored directly in v2_pks.
        let lookup_sql = if tbl_info.uses_rowid_key {
            format!(
                "SELECT {} FROM \"{}\" WHERE \"{}\" = ?\0",
                pk_cols.join(", "),
                escaped,
                crate::util::escape_ident(&tbl_info.rowid_alias)
            )
        } else {
            format!(
                "SELECT {} FROM \"{}{}\" WHERE __crsql_key = ?\0",
                pk_cols.join(", "),
                escaped, consts::V2_PKS_SUFFIX
            )
        };
        let stmt = db.prepare_v2(&lookup_sql)?;
        stmt.bind_int64(1, local_key)?;
        let mut unpacked_pks: Vec<ColumnValue> = Vec::new();
        if stmt.step()? == ResultCode::ROW {
            for i in 0..pk_cols.len() {
                let val = stmt.column_value(i as i32)?;
                unpacked_pks.push(sqlite_value_to_column_value(val));
            }
        }

        // Save PK values into v2_tombstone_pks for future lookups
        if !unpacked_pks.is_empty() {
            let placeholders: Vec<&str> = unpacked_pks.iter().map(|_| "?").collect();
            let sql = format!(
                "INSERT INTO \"{}{}\" ({}, hashed_pk) VALUES ({}, ?)\0",
                escaped,
                consts::V2_TOMBSTONE_PKS_SUFFIX,
                pk_cols.join(", "),
                placeholders.join(", ")
            );
            let stmt = db.prepare_v2(&sql)?;
            bind_package_to_stmt(stmt.stmt, &unpacked_pks, 0)?;
            stmt.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
            stmt.step()?;
        }

        // Nuke clocks, v2_pks, and base table row
        v2_nuke_local_row(db, ext_data, &escaped, local_key, &unpacked_pks, tbl_info)?;
    }

    Ok(ResultCode::OK)
}

/// Convert a sqlite value pointer to a ColumnValue for binding.
pub unsafe fn sqlite_value_to_column_value(val: *mut sqlite::value) -> ColumnValue {
    use sqlite_nostd::Value;
    let val_type = val.value_type();
    match val_type {
        sqlite::ColumnType::Integer => ColumnValue::Integer(val.int64()),
        sqlite::ColumnType::Float => ColumnValue::Float(val.double()),
        sqlite::ColumnType::Text => {
            ColumnValue::Text(alloc::string::ToString::to_string(val.text()))
        }
        sqlite::ColumnType::Blob => {
            ColumnValue::Blob(val.blob().to_vec())
        }
        sqlite::ColumnType::Null => ColumnValue::Null,
    }
}

/// V2 packed merge: process a full V2 wire packed row in one pass.
/// Looks up local key/CL once, handles resurrection and skipped-delete cleanup,
/// then applies each column change.
#[allow(clippy::too_many_arguments)]
unsafe fn v2_packed_merge(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &mut TableInfo,
    unpacked_pks: &Vec<ColumnValue>,
    hashed_pk: &[u8],
    col_names: &[&str],
    col_vrsns: &[i64],
    seqs: &[i64],
    unpacked_vals: &[ColumnValue],
    db_vrsn: i64,
    site_id: &[u8],
    incoming_cl: i64,
    ts: i64,
) -> Result<ResultCode, ResultCode> {
    // ts check is done at the top of merge_insert
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;

    // Ensure alive row exists at incoming_cl. Handles stale bail, resurrection,
    // skipped-delete cleanup, and new row creation in one shot.
    let site_ordinal = if site_id.is_empty() {
        0
    } else {
        get_or_set_site_ordinal(ext_data, site_id)?
    };
    let (local_key, local_cl) = match v2_ensure_alive_row_at_cl(
        db, ext_data, tbl_info, unpacked_pks, hashed_pk, incoming_cl,
        db_vrsn, site_ordinal,
    )? {
        Some(result) => result,
        None => return Ok(ResultCode::OK), // stale CL
    };

    // Apply each column change.
    // The upsert in v2_apply_value_change_colval handles conflict resolution:
    // - When CL won, local clocks are at col_version=0, so incoming always wins.
    // - When CL is equal, the WHERE clause compares col_version and values.
    for i in 0..col_names.len() {
        v2_apply_value_change_colval(
            db, ext_data, tbl_info, &escaped, local_key, col_names[i],
            &unpacked_vals[i], col_vrsns[i], db_vrsn, site_id, seqs[i], ts,
            unpacked_pks, col_id_bits,
        )?;
    }

    Ok(ResultCode::OK)
}

/// Remove all local state for a row: clocks, v2_pks entry, and base table row.
unsafe fn v2_nuke_local_row(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    escaped: &str,
    key: i64,
    unpacked_pks: &Vec<ColumnValue>,
    tbl_info: &TableInfo,
) -> Result<(), ResultCode> {
    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let col_id_mask = consts::CRSQL_COL_ID_MASK as i64;

    // Delete clocks for this key (range scan on INTEGER PRIMARY KEY index)
    let base = key << col_id_bits;
    let sql = format!(
        "DELETE FROM \"{}{}\" WHERE cell_key >= ? AND cell_key <= ?\0",
        escaped, consts::V2_CLOCK_SUFFIX
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_int64(1, base)?;
    stmt.bind_int64(2, base | col_id_mask)?;
    stmt.step()?;

    // Delete from v2_pks
    let sql = format!(
        "DELETE FROM \"{}{}\" WHERE __crsql_key = ?\0",
        escaped, consts::V2_PKS_SUFFIX
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_int64(1, key)?;
    stmt.step()?;

    // Delete from base table (with sync bit to prevent trigger recursion)
    let sql = if tbl_info.uses_rowid_key {
        format!(
            "DELETE FROM \"{}\" WHERE \"{}\" = ?\0",
            escaped,
            crate::util::escape_ident(&tbl_info.rowid_alias)
        )
    } else {
        let pk_cols: Vec<&str> = tbl_info.pks.iter().map(|c| c.name.as_str()).collect();
        let where_conds: Vec<String> = pk_cols.iter().map(|c| {
            format!("\"{}\" = ?", crate::util::escape_ident(c))
        }).collect();
        format!(
            "DELETE FROM \"{}\" WHERE {}\0",
            escaped,
            where_conds.join(" AND ")
        )
    };
    // Set sync bit to suppress after_delete trigger
    (*ext_data).pSetSyncBitStmt.step()?;
    (*ext_data).pSetSyncBitStmt.reset()?;
    let stmt = db.prepare_v2(&sql)?;
    if tbl_info.uses_rowid_key {
        stmt.bind_int64(1, key)?;
    } else {
        bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
    }
    stmt.step()?;
    // Clear sync bit
    (*ext_data).pClearSyncBitStmt.step()?;
    (*ext_data).pClearSyncBitStmt.reset()?;

    Ok(())
}

/// Remove tombstone entries for a hashed_pk (resurrection cleanup).
unsafe fn v2_nuke_tombstone(
    db: *mut sqlite3,
    escaped: &str,
    hashed_pk: &[u8],
) -> Result<(), ResultCode> {
    let del_tomb = db.prepare_v2(&format!(
        "DELETE FROM \"{}{}\" WHERE hashed_pk = ?\0",
        escaped, consts::V2_TOMBSTONES_SUFFIX
    ))?;
    del_tomb.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
    del_tomb.step()?;

    let del_tpk = db.prepare_v2(&format!(
        "DELETE FROM \"{}{}\" WHERE hashed_pk = ?\0",
        escaped, consts::V2_TOMBSTONE_PKS_SUFFIX
    ))?;
    del_tpk.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
    del_tpk.step()?;

    Ok(())
}

/// Look up PK values from V2 metadata tables (v2_pks or v2_tombstone_pks) by hashed_pk.
/// Used when unpacked PKs are not available from the wire (e.g., hash tombstone case).
unsafe fn v2_lookup_pks_for_v1_copy(
    db: *mut sqlite3,
    escaped: &str,
    tbl_info: &TableInfo,
    hashed_pk: &[u8],
) -> Result<Vec<ColumnValue>, ResultCode> {
    let pk_col_names: Vec<String> = tbl_info.pks.iter().map(|c| crate::util::escape_ident(&c.name)).collect();
    let pk_list = pk_col_names.join(", ");

    // Try v2_tombstone_pks first (row might have been deleted)
    let sql = format!(
        "SELECT {} FROM \"{}{}\" WHERE hashed_pk = ?\0",
        pk_list, escaped, consts::V2_TOMBSTONE_PKS_SUFFIX
    );
    let stmt = db.prepare_v2(&sql)?;
    stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        let mut result = Vec::new();
        for i in 0..tbl_info.pks.len() {
            result.push(sqlite_value_to_column_value(stmt.column_value(i as i32)?));
        }
        return Ok(result);
    }

    // Try v2_pks (row might still be alive)
    if tbl_info.uses_rowid_key {
        let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        let t_pk_list: Vec<String> = pk_col_names.iter().map(|c| format!("t.{}", c)).collect();
        let sql = format!(
            "SELECT {} FROM \"{}{}\" p JOIN \"{}\" t ON t.\"{}\" = p.__crsql_key WHERE p.hashed_pk = ?\0",
            t_pk_list.join(", "), escaped, consts::V2_PKS_SUFFIX,
            escaped, alias
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        if stmt.step()? == ResultCode::ROW {
            let mut result = Vec::new();
            for i in 0..tbl_info.pks.len() {
                result.push(sqlite_value_to_column_value(stmt.column_value(i as i32)?));
            }
            return Ok(result);
        }
    } else {
        let sql = format!(
            "SELECT {} FROM \"{}{}\" WHERE hashed_pk = ?\0",
            pk_list, escaped, consts::V2_PKS_SUFFIX
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        if stmt.step()? == ResultCode::ROW {
            let mut result = Vec::new();
            for i in 0..tbl_info.pks.len() {
                result.push(sqlite_value_to_column_value(stmt.column_value(i as i32)?));
            }
            return Ok(result);
        }
    }

    Ok(Vec::new())
}

/// Copy V2 metadata state to V1 metadata tables for dual-write mode.
/// Called after V2 merge has completed for SchemaVersion::V2AndV1 tables.
/// Receives the post-merge V2 key/CL from the caller to avoid a redundant lookup.
/// Reads the current V2 clock entries and mirrors them to V1 tables
/// (__crsql_pks + __crsql_clock), ensuring semantic equivalence
/// without relying on trigger-based V1 metadata population.
unsafe fn v2_to_v1_mirror_metadata(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    unpacked_pks: Option<&Vec<ColumnValue>>,
    hashed_pk: &[u8],
    v2_key_opt: Option<i64>,
    v2_cl: i64,
) -> Result<(), ResultCode> {
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let ts_fallback = unsafe { (*ext_data).timestamp as i64 };
    if v2_cl == 0 {
        return Ok(());
    }

    // 1. Get unpacked PKs — from parameter or look up from V2 tables
    let looked_up_pks;
    let pks: &Vec<ColumnValue> = match unpacked_pks {
        Some(p) => p,
        None => {
            looked_up_pks = v2_lookup_pks_for_v1_copy(db, &escaped, tbl_info, hashed_pk)?;
            &looked_up_pks
        }
    };

    if pks.is_empty() {
        return Ok(());
    }

    // 3. Get or create V1 PK entry
    let v1_key = tbl_info.get_or_create_key(db, pks)?;

    // 4. Delete all existing V1 clock entries and sentinels for this v1 key
    let del_sql = format!(
        "DELETE FROM \"{}__crsql_clock\" WHERE key = ?\0",
        escaped
    );
    let del_stmt = db.prepare_v2(&del_sql)?;
    del_stmt.bind_int64(1, v1_key)?;
    del_stmt.step()?;

    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let col_id_mask = consts::CRSQL_COL_ID_MASK as i64;

    // Set the V1 sentinel if needed
    // V1 omits the sentinel for CL=1 (alive, default), so only write it for CL > 1.
    // Besides the CL V1 requires (site_id, db_version, seq, ts)
    // In V2 we have this extra data for dead rows but we don't have it for alive rows
    // For alive rows we improvise by taking the values from col_id=0
    if v2_cl > 1 {
        let (source_sql, bind_cell_key) = if let Some(k) = v2_key_opt {
            (
                format!(
                    "SELECT site_id, db_version, seq, ts FROM \"{}{}\" WHERE cell_key = ?\0",
                    escaped, consts::V2_CLOCK_SUFFIX
                ),
                (k << col_id_bits) | 0,
            )
        } else {
            (
                format!(
                    "SELECT site_id, db_version, seq, ts FROM \"{}{}\" WHERE hashed_pk = ?\0",
                    escaped, consts::V2_TOMBSTONES_SUFFIX
                ),
                0,
            )
        };
        let ins_sql = format!(
            "INSERT INTO \"{}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts)
             SELECT ?, '-1', {cl}, site_id, db_version, seq,
               CASE WHEN ts > 0 THEN ts ELSE {ts_fallback} END
             FROM ({src}) LIMIT 1\0",
            escaped, cl = v2_cl, ts_fallback = ts_fallback, src = source_sql.trim_end_matches('\0')
        );
        let ins_stmt = db.prepare_v2(&ins_sql)?;
        ins_stmt.bind_int64(1, v1_key)?;
        if v2_key_opt.is_some() {
            ins_stmt.bind_int64(2, bind_cell_key)?;
        } else {
            ins_stmt.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
        }
        ins_stmt.step()?;
    }

    // Copy clocks from V2 to V1. For PK only tables it's an no op as the
    // insert sentinel was created already and the V2_COL_MAP_SUFFIX join will filter it out 
    if let Some(v2_key) = v2_key_opt {
        // cell_key = (v2_key << col_id_bits) | col_id
        // All entries for this v2_key are in range [base, base | col_id_mask].
        // Use range scan to leverage the INTEGER PRIMARY KEY index.
        let base = v2_key << col_id_bits;
        let cell_sql = format!(
            "INSERT INTO \"{}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts)
             SELECT ?, m.col_name, c.col_version, c.db_version, c.seq,
               CASE WHEN c.ts > 0 THEN c.ts ELSE {ts_fallback} END, c.site_id
             FROM \"{}{}\" c
             JOIN \"{}{}\" m ON (c.cell_key & ?) = m.col_id
             WHERE c.cell_key >= ? AND c.cell_key <= ?\0",
            escaped,
            escaped, consts::V2_CLOCK_SUFFIX,
            escaped, consts::V2_COL_MAP_SUFFIX,
            ts_fallback = ts_fallback,
        );
        let cell_stmt = db.prepare_v2(&cell_sql)?;
        cell_stmt.bind_int64(1, v1_key)?;
        cell_stmt.bind_int64(2, col_id_mask)?;
        cell_stmt.bind_int64(3, base)?;
        cell_stmt.bind_int64(4, base | col_id_mask)?;
        cell_stmt.step()?;
    }

    Ok(())
}

/// Apply a value change using ColumnValue instead of *mut sqlite::value.
/// The clock upsert decides if the change wins via WHERE + RETURNING.
/// The base table is only updated if the clock upsert applied (RETURNING yielded a row).
#[allow(clippy::too_many_arguments)]
unsafe fn v2_apply_value_change_colval(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &TableInfo,
    escaped: &str,
    key: i64,
    col_name: &str,
    val: &ColumnValue,
    col_vrsn: i64,
    db_version: i64,
    site_id: &[u8],
    seq: i64,
    ts: i64,
    unpacked_pks: &Vec<ColumnValue>,
    col_id_bits: u32,
) -> Result<(), ResultCode> {
    // Get site ordinal
    let site_ordinal = if site_id.is_empty() {
        0
    } else {
        get_or_set_site_ordinal(ext_data, site_id)?
    };

    // Get col_id
    let col_id = v2_get_col_id(db, escaped, col_name)?;
    if col_id.is_none() {
        return Ok(());
    }
    let col_id = col_id.unwrap();
    let cell_key = (key << col_id_bits) | (col_id as i64);

    // Build base table subquery for value comparison.
    // For rowid-key tables, use rowid alias directly (1 param).
    // For non-rowid tables, use PK columns (n params).
    let escaped_col = crate::util::escape_ident(col_name);
    let merge_equal = unsafe { (*ext_data).mergeEqualValues };
    let pk_len = unpacked_pks.len() as i32;

    let (subquery, subquery_param_count) = if tbl_info.uses_rowid_key {
        let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        (
            format!("SELECT \"{}\" FROM \"{}\" WHERE \"{}\" = ?", escaped_col, escaped, alias),
            1
        )
    } else {
        let pk_where: String = tbl_info.pks
            .iter()
            .map(|c| format!("\"{}\" = ?", crate::util::escape_ident(&c.name)))
            .collect::<Vec<_>>()
            .join(" AND ");
        (
            format!("SELECT \"{}\" FROM \"{}\" WHERE {}", escaped_col, escaped, pk_where),
            pk_len
        )
    };

    // Upsert v2_clock entry with conflict resolution in WHERE clause.
    // RETURNING tells us if the change won — only then do we update the base table.
    // site_id comparison uses blob comparison via crsql_site_id join, matching V1 semantics.
    let sql = format!(
        "INSERT INTO \"{}{}\" (cell_key, col_version, site_id, db_version, seq, ts)\n
         VALUES (?, ?, ?, ?, ?, ?)\n
         ON CONFLICT(cell_key) DO UPDATE SET\n
         col_version = excluded.col_version,\n
         site_id = excluded.site_id,\n
         db_version = excluded.db_version,\n
         seq = excluded.seq,\n
         ts = excluded.ts\n
         WHERE excluded.col_version > col_version\n
         OR (excluded.col_version = col_version AND\n
         crsql_change_wins(?, ({}),\n
         ? > (SELECT site_id FROM crsql_site_id WHERE ordinal = site_id), ?))\n
         RETURNING cell_key\0",
        escaped, consts::V2_CLOCK_SUFFIX,
        subquery
    );
    let stmt = db.prepare_v2(&sql)?;
    // Bind clock values (params 1-6)
    stmt.bind_int64(1, cell_key)?;
    stmt.bind_int64(2, col_vrsn)?;
    stmt.bind_int64(3, site_ordinal)?;
    stmt.bind_int64(4, db_version)?;
    stmt.bind_int64(5, seq)?;
    stmt.bind_int64(6, ts)?;
    // Bind incoming value for crsql_change_wins (param 7)
    match val {
        ColumnValue::Integer(i) => { stmt.bind_int64(7, *i)?; }
        ColumnValue::Float(f) => { stmt.bind_double(7, *f)?; }
        ColumnValue::Text(t) => { stmt.bind_text(7, t, sqlite::Destructor::STATIC)?; }
        ColumnValue::Blob(b) => { stmt.bind_blob(7, b, sqlite::Destructor::STATIC)?; }
        ColumnValue::Null => { stmt.bind_null(7)?; }
    }
    // Bind subquery params (params 8..8+subquery_param_count)
    if tbl_info.uses_rowid_key {
        stmt.bind_int64(8, key)?;
    } else {
        bind_package_to_stmt(stmt.stmt, unpacked_pks, 7)?;
    }
    // Bind incoming site_id blob for comparison (param 8+subquery_param_count)
    stmt.bind_blob(8 + subquery_param_count, site_id, sqlite::Destructor::STATIC)?;
    // Bind mergeEqualValues flag (param 9+subquery_param_count)
    stmt.bind_int(9 + subquery_param_count, merge_equal)?;

    let won = stmt.step()? == ResultCode::ROW;
    reset_cached_stmt(stmt.stmt)?;

    if !won {
        return Ok(());
    }

    // Change won — update the actual user table.
    // Set sync bit to suppress triggers that would overwrite V2 clock with local site_id.
    (*ext_data).pSetSyncBitStmt.step()?;
    (*ext_data).pSetSyncBitStmt.reset()?;

    let merge_stmt_ref = tbl_info.get_merge_insert_stmt(db, col_name)?;
    let merge_stmt = merge_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
    bind_package_to_stmt(merge_stmt.stmt, unpacked_pks, 0)?;
    let raw_stmt = merge_stmt.stmt;
    match val {
        ColumnValue::Integer(i) => { raw_stmt.bind_int64(pk_len + 1, *i)?; }
        ColumnValue::Float(f) => { raw_stmt.bind_double(pk_len + 1, *f)?; }
        ColumnValue::Text(t) => { raw_stmt.bind_text(pk_len + 1, t, sqlite::Destructor::STATIC)?; }
        ColumnValue::Blob(b) => { raw_stmt.bind_blob(pk_len + 1, b, sqlite::Destructor::STATIC)?; }
        ColumnValue::Null => { raw_stmt.bind_null(pk_len + 1)?; }
    }
    match val {
        ColumnValue::Integer(i) => { raw_stmt.bind_int64(pk_len + 2, *i)?; }
        ColumnValue::Float(f) => { raw_stmt.bind_double(pk_len + 2, *f)?; }
        ColumnValue::Text(t) => { raw_stmt.bind_text(pk_len + 2, t, sqlite::Destructor::STATIC)?; }
        ColumnValue::Blob(b) => { raw_stmt.bind_blob(pk_len + 2, b, sqlite::Destructor::STATIC)?; }
        ColumnValue::Null => { raw_stmt.bind_null(pk_len + 2)?; }
    }
    raw_stmt.step()?;
    raw_stmt.reset()?;

    (*ext_data).pClearSyncBitStmt.step()?;
    (*ext_data).pClearSyncBitStmt.reset()?;

    Ok(())
}
