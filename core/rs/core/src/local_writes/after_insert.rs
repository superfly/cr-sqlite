use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use core::ffi::c_int;
use sqlite::sqlite3;
use sqlite::value;
use sqlite::Context;
use sqlite::ResultCode;
use sqlite_nostd as sqlite;
use sqlite_nostd::Value;

use crate::{c::crsql_ExtData, tableinfo::TableInfo, config, consts};

use super::bump_seq;
use super::trigger_fn_preamble;

/**
 * crsql_after_insert("table", pk_values..., [rowid_value])
 * The rowid_value is appended when the table key_is_rowid.
 */
pub unsafe extern "C" fn x_crsql_after_insert(
    ctx: *mut sqlite::context,
    argc: c_int,
    argv: *mut *mut sqlite::value,
) {
    let result = trigger_fn_preamble(ctx, argc, argv, |table_info, values, ext_data| {
        let (pks_new, rowid_val) = if table_info.key_is_rowid {
            let len = values.len();
            (&values[1..len - 1], Some(values[len - 1]))
        } else {
            (&values[1..], None)
        };
        after_insert(ctx.db_handle(), ext_data, table_info, pks_new, rowid_val)
    });

    match result {
        Ok(_) => {
            ctx.result_int64(0);
        }
        Err(msg) => {
            ctx.result_error(&msg);
        }
    }
}

fn after_insert(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    tbl_info: &mut TableInfo,
    pks_new: &[*mut value],
    rowid_val: Option<*mut value>,
) -> Result<ResultCode, String> {
    // Enforce rowid range for rowid-key tables
    if tbl_info.key_is_rowid {
        let rowid = rowid_val.ok_or("rowid-key table missing rowid value")?.int64();
        if rowid < 0 || rowid >= consts::MAX_ROWID_KEY {
            return Err(format!(
                "rowid out of cr-sqlite safe range [0, {})",
                consts::MAX_ROWID_KEY
            ));
        }
    }

    let mwv = unsafe { (*ext_data).metadataWriteVersion };

    // Mode 3 (V2-only): write to V2 only
    if mwv == config::METADATA_VERSION_V2 {
        let rowid = if tbl_info.key_is_rowid {
            Some(rowid_val.ok_or("rowid-key table missing rowid value")?.int64())
        } else {
            None
        };
        return super::v2::v2_after_insert(db, ext_data, tbl_info, pks_new, rowid);
    }

    if mwv == config::METADATA_VERSION_V2_AND_V1 {
        // Mode 2 (Dual-write): hydrate V2 from V1 on-demand, then write to V2 first
        let saved_seq = unsafe { (*ext_data).seq };
        let rowid = if tbl_info.key_is_rowid {
            Some(rowid_val.ok_or("rowid-key table missing rowid value")?.int64())
        } else {
            None
        };
        unsafe { crate::changes_vtab_write::v1_to_v2_hydrate_row_from_values(db, ext_data, tbl_info, pks_new) }
            .map_err(|_| "V1 to V2 hydration failed".to_string())?;
        super::v2::v2_after_insert(db, ext_data, tbl_info, pks_new, rowid)?;
        // Restore seq so V1 reuses the same values V2 just bumped.
        unsafe { (*ext_data).seq = saved_seq; }
    }

    // V1 code path (write to V1)
    let ts = unsafe { (*ext_data).timestamp.to_string() };

    let db_version = crate::db_version::next_db_version(db, ext_data)?;
    let (create_record_existed, key_new) = tbl_info
        .get_or_create_key_for_insert(db, pks_new)
        .map_err(|_| "failed getting or creating lookaside key")?;

    let cl = if tbl_info.non_pks.is_empty() {
        let seq = bump_seq(ext_data);
        // just a sentinel record
        let cl = super::mark_new_pk_row_created(db, tbl_info, key_new, db_version, seq, &ts)?;
        Some(cl)
    } else {
        let cl = if create_record_existed {
            // update the create record since it already exists.
            let seq = bump_seq(ext_data);
            update_create_record(db, tbl_info, key_new, db_version, seq, &ts)?
        } else {
            None
        };
        super::mark_locally_inserted(db, ext_data, tbl_info, key_new, db_version, &ts)?;
        cl
    };

    if let Some(cl) = cl {
        tbl_info.set_cl(key_new, cl);
    }

    Ok(ResultCode::OK)
}

fn update_create_record(
    db: *mut sqlite3,
    tbl_info: &TableInfo,
    new_key: sqlite::int64,
    db_version: sqlite::int64,
    seq: i32,
    ts: &str,
) -> Result<Option<i64>, String> {
    let update_create_record_stmt_ref = tbl_info
        .get_maybe_mark_locally_reinserted_stmt(db)
        .map_err(|_e| "failed to get update_create_record_stmt")?;
    let update_create_record_stmt = update_create_record_stmt_ref
        .as_ref()
        .ok_or("Failed to deref update_create_record_stmt")?;

    update_create_record_stmt
        .bind_int64(1, db_version)
        .and_then(|_| update_create_record_stmt.bind_int(2, seq))
        .and_then(|_| update_create_record_stmt.bind_text(3, ts, sqlite::Destructor::STATIC))
        .and_then(|_| update_create_record_stmt.bind_int64(4, new_key))
        .and_then(|_| {
            update_create_record_stmt.bind_text(
                5,
                crate::c::INSERT_SENTINEL,
                sqlite::Destructor::STATIC,
            )
        })
        .map_err(|_e| "failed binding to update_create_record_stmt")?;

    let res = update_create_record_stmt.step();
    let result = match res {
        Ok(ResultCode::ROW) => {
            let col_version = update_create_record_stmt.column_int64(0);
            Ok(Some(col_version))
        }
        Ok(ResultCode::DONE) => Ok(None),
        _ => Err("failed to step update_create_record_stmt".to_string()),
    };
    super::reset_cached_stmt(update_create_record_stmt.stmt)
        .map_err(|_e| "failed to reset cached stmt")?;
    result
}
