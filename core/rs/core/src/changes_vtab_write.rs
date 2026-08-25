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
use crate::pack_columns::{unpack_columns, unpack_varints, ColumnValue};
use crate::stmt_cache::reset_cached_stmt;
use crate::tableinfo::{crsql_ensure_table_infos_are_up_to_date, TableInfo, SchemaVersion};
use crate::util::slab_rowid;
use crate::consts;

/// Set the sync bit, run `f`, then clear the sync bit.
/// Ensures the clear always runs even if `f` returns an error.
unsafe fn with_sync_bit<F, T>(ext_data: *mut crsql_ExtData, f: F) -> Result<T, ResultCode>
where
    F: FnOnce() -> Result<T, ResultCode>,
{
    (*ext_data).pSetSyncBitStmt.step()?;
    (*ext_data).pSetSyncBitStmt.reset()?;
    let result = f();
    (*ext_data).pClearSyncBitStmt.step()?;
    (*ext_data).pClearSyncBitStmt.reset()?;
    result
}

/// Get site ordinal, or 0 if site_id is empty (local site).
unsafe fn get_site_ordinal_or_zero(
    ext_data: *mut crsql_ExtData,
    site_id: &[u8],
) -> Result<i64, ResultCode> {
    if site_id.is_empty() {
        Ok(0)
    } else {
        get_or_set_site_ordinal(ext_data, site_id)
    }
}

/// Collect PK values from the first N columns of a stmt into a Vec<ColumnValue>.
unsafe fn collect_pks_from_stmt(
    stmt: *mut sqlite::stmt,
    n_pks: usize,
) -> Result<Vec<ColumnValue>, ResultCode> {
    let mut result = Vec::with_capacity(n_pks);
    for i in 0..n_pks {
        let val = <_ as sqlite::Stmt>::column_value(&stmt, i as i32);
        result.push(sqlite_value_to_column_value(val));
    }
    Ok(result)
}

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
            v2_lookup_key_and_cl(db, &escaped, tbl_info, hashed_pk, unpacked_pks.unwrap_or(&Vec::new()), ext_data).unwrap_or((None, 0));
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
    // V2 wire always has BLOB col_vrsn (crsql_pack_varint_agg — varint count header + payload).
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
    // skip_hash mode: no hashed_pk — lookups use PK column directly.
    let skip_hash = tbl_info.skip_hash;
    let (unpacked_pks_opt, hashed_pk): (Option<Vec<ColumnValue>>, Vec<u8>) = if is_v2_hash_tombstone {
        (None, insert_pks.blob().to_vec())
    } else {
        let packed_pks = insert_pks.blob();
        let unpacked_pks = unpack_columns(&packed_pks)?;
        let hashed_pk = if skip_hash {
            Vec::new() // not used in skip_hash mode
        } else {
            crate::hash_pk::hash_packed_blob(&packed_pks)
        };
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

        v2_merge_insert_tombstone(
            db,
            (*tab).pExtData,
            tbl_info,
            &hashed_pk,
            unpacked_pks_opt.as_ref(),
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
            let col_vrsns: Vec<i64> = unpack_varints(insert_col_vrsn_raw.blob())?;
            let seqs: Vec<i64> = unpack_varints(insert_seq_raw.blob())?;
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

    let (local_key_opt, local_cl) = v2_lookup_key_and_cl(db, &escaped, tbl_info, hashed_pk, unpacked_pks, ext_data)?;

    if incoming_cl < local_cl {
        return Ok(None);
    }

    let local_key = if incoming_cl > local_cl {
        if let Some(key) = local_key_opt {
            v2_nuke_local_row(db, ext_data, &escaped, key, unpacked_pks, tbl_info)?;
        } else if local_cl > 0 {
            v2_nuke_tombstone(db, &escaped, tbl_info, hashed_pk, unpacked_pks, ext_data)?;
        }
        // Create fresh v2_pks entry with new CL
        let new_key = v2_insert_pk_row(db, ext_data, &escaped, tbl_info, unpacked_pks, hashed_pk, incoming_cl)?;
        // Create zero-version clock entries for all mapped columns so they appear in sync logs.
        // Use the incoming change's site_id and db_version since it created this row.
        // seq=0 for placeholders; the packed column updates will overwrite specific entries.
        //
        // col_version=0 here (not 1 like local inserts) because these are placeholders
        // for columns we haven't received yet. The actual column changes arrive as
        // separate change rows and overwrite specific entries with col_version=1+.
        // With V1 wire format, col_version=0 entries appear in the feed but lose to
        // any local col_version > 0 on merge — safe but potentially wasteful.
        //
        // TODO(0.19): In V2-wire-only mode, we can skip zero-fill entirely and only
        // create clock entries for columns that were actually received in the change.
        let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
        let base = new_key << col_id_bits;
        let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
        let v2 = v2_ref.as_mut().unwrap();
        let mut clock_stmt = v2.clock_zero_fill();
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

/// Look up key and CL from v2_pks or v2_tombstones.
/// A row is in exactly one of v2_pks (alive) or v2_tombstones (dead), never both.
///
/// In hash mode: lookup by hashed_pk blob.
/// In skip_hash mode: lookup by PK column value.
///   - has_integer_pk && key_is_rowid: PK value == rowid == __crsql_key, so
///     `WHERE __crsql_key = ?` directly on v2_pks, `WHERE "pk_col" = ?` on v2_tombstones.
///   - key_is_rowid only (non-integer PK): JOIN main table to map PK value → rowid → __crsql_key.
///   - non-rowid: `WHERE "pk_col" = ?` directly on v2_pks/v2_tombstones.
unsafe fn v2_lookup_key_and_cl(
    db: *mut sqlite3,
    escaped: &str,
    tbl_info: &TableInfo,
    hashed_pk: &[u8],
    unpacked_pks: &[ColumnValue],
    ext_data: *mut crsql_ExtData,
) -> Result<(Option<i64>, i64), ResultCode> {
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();
    let mut stmt = v2.lookup_row_state();
    if tbl_info.skip_hash {
        // We enforce that in skip_hash mode, there is only one PK column
        let pk_val = &unpacked_pks[0];
        crate::pack_columns::bind_slot(1, pk_val, stmt.stmt)?;
        crate::pack_columns::bind_slot(2, pk_val, stmt.stmt)?;
    } else {
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        stmt.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
    }
    if stmt.step()? == ResultCode::ROW {
        let key = stmt.column_int64(0);
        let cl = stmt.column_int64(1);
        if stmt.column_type(0)? == sqlite::ColumnType::Null {
            return Ok((None, cl));
        }
        return Ok((Some(key), cl));
    }
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
    let (existing_v2_key, existing_v2_cl) = v2_lookup_key_and_cl(db, &escaped, tbl_info, hashed_pk, unpacked_pks, ext_data)?;
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
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    let (v1_cl, is_dead) = {
        let mut stmt = v2.v1_sentinel_lookup()?;
        stmt.bind_int64(1, v1_key)?;
        if stmt.step()? == ResultCode::ROW {
            let cl = stmt.column_int64(0);
            (cl, cl % 2 == 0)
        } else {
            // sentinel guard dropped here
            (-1i64, false) // sentinel: try any_clock_lookup
        }
    };

    // If sentinel lookup found nothing, check if any clock entries exist
    let (v1_cl, is_dead) = if v1_cl == -1 {
        let mut stmt = v2.v1_any_clock_lookup()?;
        stmt.bind_int64(1, v1_key)?;
        if stmt.step()? == ResultCode::ROW {
            (1, false) // Alive with no explicit sentinel → CL=1
        } else {
            return Ok(()); // No V1 metadata at all — nothing to hydrate
        }
    } else {
        (v1_cl, is_dead)
    };

    if is_dead {
        // 3a. Row is dead — insert into v2_tombstones (+ v2_tombstone_pks in hash mode)
        let (site_id, db_version, seq, ts) = {
            let mut stmt = v2.v1_sentinel_detail()?;
            stmt.bind_int64(1, v1_key)?;
            if stmt.step()? == ResultCode::ROW {
                let ts = stmt.column_int64(3);
                (stmt.column_int64(0), stmt.column_int64(1), stmt.column_int64(2), if ts > 0 { ts } else { ts_fallback })
            } else {
                return Ok(()); // No sentinel detail — shouldn't happen but bail
            }
        };

        // Insert tombstone (site_id is a bind param — 0 for local writes, actual site for hydration)
        {
            let mut ins = v2.tomb_insert();
            ins.bind_int64(1, site_id)?;
            ins.bind_int64(2, db_version)?;
            ins.bind_int64(3, seq)?;
            if tbl_info.skip_hash {
                crate::pack_columns::bind_slot(4, &unpacked_pks[0], ins.stmt)?;
            } else {
                ins.bind_blob(4, hashed_pk, sqlite::Destructor::STATIC)?;
            }
            ins.bind_int64(5, v1_cl)?;
            ins.bind_int64(6, ts)?;
            ins.step()?;
        }

        // Insert tombstone PKs (hash mode only)
        if !tbl_info.skip_hash {
            let mut ins = v2.tomb_pks_insert()?;
            // Cached SQL: (hashed_pk, pk_cols) VALUES (?, pk_values)
            ins.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
            bind_package_to_stmt(ins.stmt, unpacked_pks, 1)?;
            ins.step()?;
        }
    } else {
        // 3b. Row is alive — insert into v2_pks, then copy clock entries to v2_clock
        let v2_key = if tbl_info.key_is_rowid {
            // Look up rowid from base table
            let rowid_val = {
                let mut stmt = v2.base_lookup_rowid()?;
                bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
                if stmt.step()? != ResultCode::ROW {
                    return Ok(()); // Row not found in base table — skip hydration
                }
                stmt.column_int64(0)
            };

            // Insert into v2_pks with RETURNING (unified path for rowid and non-rowid)
            {
                let mut ins = v2.pks_insert();
                if tbl_info.skip_hash {
                    ins.bind_int64(1, rowid_val)?;
                    ins.bind_int64(2, v1_cl)?;
                } else {
                    ins.bind_int64(1, rowid_val)?;
                    ins.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
                    ins.bind_int64(3, v1_cl)?;
                }
                ins.step()?;
                ins.column_int64(0)
            }
        } else {
            // Non-rowid table: insert PK columns into v2_pks with RETURNING
            let mut ins = v2.pks_insert();
            bind_package_to_stmt(ins.stmt, unpacked_pks, 0)?;
            let next_slot = if tbl_info.skip_hash {
                unpacked_pks.len() as i32 + 1
            } else {
                ins.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
                unpacked_pks.len() as i32 + 2
            };
            ins.bind_int64(next_slot, v1_cl)?;
            ins.step()?;
            ins.column_int64(0)
        };

        // Copy clock entries from V1 to V2
        // Bind order: 1=v2_key, 2=ts_fallback, 3=v1_key
        let mut stmt = v2.hydrate_clock_copy()?;
        stmt.bind_int64(1, v2_key)?;
        stmt.bind_int64(2, ts_fallback)?;
        stmt.bind_int64(3, v1_key)?;
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
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();
    with_sync_bit(ext_data, || {
        let mut base_stmt = v2.base_insert();
        bind_package_to_stmt(base_stmt.stmt, unpacked_pks, 0)?;
        base_stmt.step()?;
        Ok(())
    })
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

    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // Then insert the PK row into v2_pks (unified path with RETURNING)
    if tbl_info.key_is_rowid {
        let rowid = sqlite::last_insert_rowid(db);
        let mut ins = v2.pks_insert();
        if tbl_info.skip_hash {
            ins.bind_int64(1, rowid)?;
            ins.bind_int64(2, cl)?;
        } else {
            ins.bind_int64(1, rowid)?;
            ins.bind_blob(2, hashed_pk, sqlite::Destructor::STATIC)?;
            ins.bind_int64(3, cl)?;
        }
        ins.step()?;
        Ok(ins.column_int64(0))
    } else {
        // Non-rowid: use pks_insert (has RETURNING)
        let mut ins = v2.pks_insert();
        bind_package_to_stmt(ins.stmt, unpacked_pks, 0)?;
        let next_slot = if tbl_info.skip_hash {
            unpacked_pks.len() as i32 + 1
        } else {
            ins.bind_blob(unpacked_pks.len() as i32 + 1, hashed_pk, sqlite::Destructor::STATIC)?;
            unpacked_pks.len() as i32 + 2
        };
        ins.bind_int64(next_slot, cl)?;
        ins.step()?;
        Ok(ins.column_int64(0))
    }
}

/// Get col_id from v2_col_map by col_name
unsafe fn v2_get_col_id(
    db: *mut sqlite3,
    tbl_info: &TableInfo,
    ext_data: *mut crsql_ExtData,
    col_name: &str,
) -> Result<Option<i64>, ResultCode> {
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();
    let mut stmt = v2.col_id_lookup();
    stmt.bind_text(1, col_name, sqlite::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        return Ok(Some(stmt.column_int64(0)));
    }
    Ok(None)
}

/// Handle tombstone merge (cid=-2 hash tombstone or cid='-1' V1-wire delete).
///
/// Hash mode (cid=-2): we only have hashed_pk, not packed PK values.
///   Look up PK values from local v2_pks if the row exists there.
///   If the row doesn't exist locally, insert a bare tombstone with hash only.
///
/// skip_hash mode (cid='-1' V1-wire delete): we have unpacked PK values.
///   Insert tombstone with PK column directly. No v2_tombstone_pks needed.
///
/// skip_hash mode (cid=-2 hash tombstone): cannot process — bail.
#[allow(clippy::too_many_arguments)]
unsafe fn v2_merge_insert_tombstone(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &mut TableInfo,
    hashed_pk: &[u8],
    unpacked_pks_opt: Option<&Vec<ColumnValue>>,
    insert_tbl: &str,
    insert_col: &str,
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
    let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
    let is_v2_hash_tombstone = insert_col == crate::consts::V2_HASH_TOMBSTONE_CID;

    // skip_hash + hash tombstone (cid=-2): cannot process — no hash→PK mapping exists.
    if tbl_info.skip_hash && is_v2_hash_tombstone {
        return Ok(ResultCode::OK);
    }

    // For skip_hash mode, we need unpacked PKs for lookups and tombstone insert.
    // For hash mode with cid=-2, we don't have unpacked PKs (only hashed_pk blob).
    let unpacked_pks: Vec<ColumnValue> = if tbl_info.skip_hash {
        unpacked_pks_opt.cloned().unwrap_or_default()
    } else {
        Vec::new()
    };

    // Look up key and CL from v2_pks (alive) or v2_tombstones (dead)
    let (local_key, local_cl) = v2_lookup_key_and_cl(db, &escaped, tbl_info, hashed_pk, &unpacked_pks, ext_data)?;

    // Bail early if incoming CL can't beat local CL
    if insert_cl < local_cl {
        return Ok(ResultCode::OK);
    }

    let merge_equal = unsafe { (*ext_data).mergeEqualValues };
    let site_ordinal = get_site_ordinal_or_zero(ext_data, insert_site_id)?;

    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // Upsert tombstone with conflict resolution (merge_equal baked into SQL at prep time)
    {
        let mut stmt = v2.tomb_upsert();
        stmt.bind_int64(1, site_ordinal as i64)?;
        stmt.bind_int64(2, insert_db_vrsn)?;
        stmt.bind_int64(3, insert_seq)?;
        if tbl_info.skip_hash {
            crate::pack_columns::bind_slot(4, &unpacked_pks[0], stmt.stmt)?;
        } else {
            stmt.bind_blob(4, hashed_pk, sqlite::Destructor::STATIC)?;
        }
        stmt.bind_int64(5, insert_cl)?;
        stmt.bind_int64(6, insert_ts)?;
        if merge_equal == 1 {
            stmt.bind_blob(7, insert_site_id, sqlite::Destructor::STATIC)?;
        }
        stmt.step()?;
    }

    // If the row was alive, nuke its local state (clocks, v2_pks, base table row).
    // Also save PK values into v2_tombstone_pks for future lookups (hash mode only).
    if let Some(local_key) = local_key {
        // Look up PK values once — used for both tombstone_pks insert and v2_nuke_local_row.
        let mut local_pks: Vec<ColumnValue> = Vec::new();
        {
            let mut stmt = v2.pk_lookup_by_key();
            stmt.bind_int64(1, local_key)?;
            if stmt.step()? == ResultCode::ROW {
                local_pks = collect_pks_from_stmt(stmt.stmt, tbl_info.pks.len())?;
            }
        }

        // Save PK values into v2_tombstone_pks for future lookups (hash mode only)
        if !tbl_info.skip_hash && !local_pks.is_empty() {
            let mut ins = v2.tomb_pks_insert()?;
            // Cached SQL: (hashed_pk, pk_cols) VALUES (?, pk_values)
            ins.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
            bind_package_to_stmt(ins.stmt, &local_pks, 1)?;
            ins.step()?;
        }

        // Drop v2_ref before calling v2_nuke_local_row which needs its own borrow
        drop(v2_ref);

        // Nuke clocks, v2_pks, and base table row
        v2_nuke_local_row(db, ext_data, &escaped, local_key, &local_pks, tbl_info)?;
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
    let site_ordinal = get_site_ordinal_or_zero(ext_data, site_id)?;
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
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // Delete clocks for this key (range scan on INTEGER PRIMARY KEY index)
    let base = key << col_id_bits;
    {
        let mut stmt = v2.clock_delete_range();
        stmt.bind_int64(1, base)?;
        stmt.bind_int64(2, base | col_id_mask)?;
        stmt.step()?;
    }

    // Delete from v2_pks
    {
        let mut stmt = v2.pks_delete();
        stmt.bind_int64(1, key)?;
        stmt.step()?;
    }

    // Delete from base table (with sync bit to prevent trigger recursion)
    with_sync_bit(ext_data, || {
        if tbl_info.key_is_rowid {
            let mut stmt = v2.base_delete_rowid();
            stmt.bind_int64(1, key)?;
            stmt.step()?;
        } else {
            let mut stmt = v2.base_delete_nonrowid()?;
            bind_package_to_stmt(stmt.stmt, unpacked_pks, 0)?;
            stmt.step()?;
        }
        Ok(())
    })?;

    Ok(())
}

/// Remove tombstone entries (resurrection cleanup).
/// In hash mode: delete by hashed_pk from v2_tombstones and v2_tombstone_pks.
/// In skip_hash mode: delete by PK column from v2_tombstones (no v2_tombstone_pks).
unsafe fn v2_nuke_tombstone(
    db: *mut sqlite3,
    escaped: &str,
    tbl_info: &TableInfo,
    hashed_pk: &[u8],
    unpacked_pks: &[ColumnValue],
    ext_data: *mut crsql_ExtData,
) -> Result<(), ResultCode> {
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // Delete from v2_tombstones
    {
        let mut stmt = v2.tomb_delete();
        if tbl_info.skip_hash {
            crate::pack_columns::bind_slot(1, &unpacked_pks[0], stmt.stmt)?;
        } else {
            stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        }
        stmt.step()?;
    }

    // Delete from v2_tombstone_pks (hash mode only)
    if !tbl_info.skip_hash {
        let mut stmt = v2.tomb_pks_delete()?;
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        stmt.step()?;
    }

    Ok(())
}

/// Look up PK values from V2 metadata tables (v2_pks or v2_tombstone_pks) by hashed_pk.
/// Used when unpacked PKs are not available from the wire (e.g., hash tombstone case).
/// Only called in hash mode — skip_hash mode always has unpacked PKs from the wire.
unsafe fn v2_lookup_pks_for_v1_copy(
    db: *mut sqlite3,
    escaped: &str,
    tbl_info: &TableInfo,
    hashed_pk: &[u8],
    ext_data: *mut crsql_ExtData,
) -> Result<Vec<ColumnValue>, ResultCode> {
    // skip_hash tables don't have hashed_pk columns — return empty.
    if tbl_info.skip_hash {
        return Ok(Vec::new());
    }
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // Try v2_tombstone_pks first (row might have been deleted)
    {
        let mut stmt = v2.lookup_pks_tomb()?;
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        if stmt.step()? == ResultCode::ROW {
            return Ok(collect_pks_from_stmt(stmt.stmt, tbl_info.pks.len())?);
        }
    }

    // Try v2_pks (row might still be alive)
    {
        let mut stmt = v2.lookup_pks_alive()?;
        stmt.bind_blob(1, hashed_pk, sqlite::Destructor::STATIC)?;
        if stmt.step()? == ResultCode::ROW {
            return Ok(collect_pks_from_stmt(stmt.stmt, tbl_info.pks.len())?);
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
            looked_up_pks = v2_lookup_pks_for_v1_copy(db, &escaped, tbl_info, hashed_pk, ext_data)?;
            &looked_up_pks
        }
    };

    if pks.is_empty() {
        return Ok(());
    }

    // 3. Get or create V1 PK entry
    let v1_key = tbl_info.get_or_create_key(db, pks)?;

    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();

    // 4. Delete all existing V1 clock entries and sentinels for this v1 key
    {
        let mut del = v2.v1_clock_delete()?;
        del.bind_int64(1, v1_key)?;
        del.step()?;
    }

    let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
    let col_id_mask = consts::CRSQL_COL_ID_MASK as i64;

    // Set the V1 sentinel if needed (CL > 1 only).
    // Bind order for cached stmts: 1=key, 2=cl, 3=ts_fallback, 4=lookup_param
    if v2_cl > 1 {
        if let Some(k) = v2_key_opt {
            // Alive: look up from v2_clock by cell_key
            let mut ins = v2.v1_sentinel_insert_alive()?;
            ins.bind_int64(1, v1_key)?;
            ins.bind_int64(2, v2_cl)?;
            ins.bind_int64(3, ts_fallback)?;
            ins.bind_int64(4, (k << col_id_bits) | 0)?;
            ins.step()?;
        } else {
            // Dead: look up from v2_tombstones
            let mut ins = v2.v1_sentinel_insert_dead()?;
            ins.bind_int64(1, v1_key)?;
            ins.bind_int64(2, v2_cl)?;
            ins.bind_int64(3, ts_fallback)?;
            if tbl_info.skip_hash {
                crate::pack_columns::bind_slot(4, &pks[0], ins.stmt)?;
            } else {
                ins.bind_blob(4, hashed_pk, sqlite::Destructor::STATIC)?;
            }
            ins.step()?;
        }
    }

    // Copy clocks from V2 to V1. For PK only tables it's a no-op as the
    // insert sentinel was created already and the V2_COL_MAP_SUFFIX join will filter it out
    if let Some(v2_key) = v2_key_opt {
        let base = v2_key << col_id_bits;
        // Bind order: 1=key, 2=ts_fallback, 3=col_id_mask, 4=cell_key_base, 5=cell_key_end
        let mut stmt = v2.v1_clock_copy()?;
        stmt.bind_int64(1, v1_key)?;
        stmt.bind_int64(2, ts_fallback)?;
        stmt.bind_int64(3, col_id_mask)?;
        stmt.bind_int64(4, base)?;
        stmt.bind_int64(5, base | col_id_mask)?;
        stmt.step()?;
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
    let site_ordinal = get_site_ordinal_or_zero(ext_data, site_id)?;

    // Get col_id
    let col_id = v2_get_col_id(db, tbl_info, ext_data, col_name)?;
    if col_id.is_none() {
        return Ok(());
    }
    let col_id = col_id.unwrap();
    let cell_key = (key << col_id_bits) | (col_id as i64);

    // Build base table subquery for value comparison.
    // For rowid-key tables, use rowid alias directly (1 param).
    // For non-rowid tables, use PK columns (n params).
    let merge_equal = unsafe { (*ext_data).mergeEqualValues };
    let pk_len = unpacked_pks.len() as i32;
    let subquery_param_count = if tbl_info.key_is_rowid { 1 } else { pk_len };

    // Get cached per-column clock merge upsert (lazily prepared on first use for this column)
    let mut v2_ref = tbl_info.get_v2_stmts(db, ext_data)?;
    let v2 = v2_ref.as_mut().unwrap();
    let mut stmt = v2.clock_merge_upsert(db, tbl_info, escaped, col_name)?;

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
    if tbl_info.key_is_rowid {
        stmt.bind_int64(8, key)?;
    } else {
        bind_package_to_stmt(stmt.stmt, unpacked_pks, 7)?;
    }
    // Bind incoming site_id blob for comparison (param 8+subquery_param_count)
    stmt.bind_blob(8 + subquery_param_count, site_id, sqlite::Destructor::STATIC)?;
    // Bind mergeEqualValues flag (param 9+subquery_param_count)
    stmt.bind_int(9 + subquery_param_count, merge_equal)?;

    let won = stmt.step()? == ResultCode::ROW;
    drop(stmt); // Release borrow on v2 before getting base_update

    if !won {
        return Ok(());
    }

    // Change won — update the actual user table.
    // Set sync bit to suppress triggers that would overwrite V2 clock with local site_id.
    // V2 guarantees the row exists (created by v2_ensure_alive_row_at_cl),
    // so we use a plain UPDATE instead of INSERT ... ON CONFLICT DO UPDATE.
    //
    // TODO(0.19): Batch multi-column changes into a single UPDATE statement
    // when all columns are available (V2 wire format coalesced changes).
    // Instead of N per-column UPDATEs, do one:
    //   UPDATE base SET col1 = ?, col2 = ?, ... WHERE rowid = ?
    // This requires hoisting the per-column conflict resolution to determine
    // all winning columns first, then issuing a single base table write.
    with_sync_bit(ext_data, || {
        let mut update_stmt = v2.base_update(col_name)?;
        // Bind value as param 1
        match val {
            ColumnValue::Integer(i) => { update_stmt.bind_int64(1, *i)?; }
            ColumnValue::Float(f) => { update_stmt.bind_double(1, *f)?; }
            ColumnValue::Text(t) => { update_stmt.bind_text(1, t, sqlite::Destructor::STATIC)?; }
            ColumnValue::Blob(b) => { update_stmt.bind_blob(1, b, sqlite::Destructor::STATIC)?; }
            ColumnValue::Null => { update_stmt.bind_null(1)?; }
        }
        // Bind rowid or PKs as params 2..
        if tbl_info.key_is_rowid {
            update_stmt.bind_int64(2, key)?;
        } else {
            bind_package_to_stmt(update_stmt.stmt, unpacked_pks, 1)?;
        }
        update_stmt.step()?;
        Ok(())
    })
}
