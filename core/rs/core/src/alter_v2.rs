extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, CStr};
use sqlite_nostd::{sqlite3, Connection, ResultCode};

use crate::c::crsql_ExtData;
use crate::consts;
use crate::tableinfo::{crsql_ensure_table_infos_are_up_to_date, TableInfo, SchemaVersion};
use core::mem;

/// Compact V2 metadata tables after an ALTER TABLE operation.
/// Handles:
/// - New columns: add to v2_col_map
/// - Removed columns: remove from v2_col_map, delete clock entries
/// - PK changes: drop and recreate all V2 metadata tables + backfill
#[no_mangle]
pub unsafe extern "C" fn crsql_compact_post_alter_v2(
    db: *mut sqlite3,
    tbl_name: *const c_char,
    ext_data: *mut crsql_ExtData,
    errmsg: *mut *mut c_char,
) -> c_int {
    match compact_post_alter_v2(db, tbl_name, ext_data, errmsg) {
        Ok(rc) | Err(rc) => rc as c_int,
    }
}

unsafe fn compact_post_alter_v2(
    db: *mut sqlite3,
    tbl_name: *const c_char,
    ext_data: *mut crsql_ExtData,
    errmsg: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    let tbl_name_str = CStr::from_ptr(tbl_name).to_str()?;
    let escaped = crate::util::escape_ident(tbl_name_str);

    // Ensure table infos are up to date so we can detect schema changes
    let c_rc = crsql_ensure_table_infos_are_up_to_date(db, ext_data, errmsg);
    if c_rc != ResultCode::OK as c_int {
        return Err(ResultCode::ERROR);
    }

    let table_infos =
        mem::ManuallyDrop::new(Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>));
    let tbl_info = table_infos.iter().find(|x| x.tbl_name == tbl_name_str);
    if tbl_info.is_none() {
        return Err(ResultCode::ERROR);
    }
    let tbl_info = tbl_info.unwrap();

    // Only handle V2 tables
    if tbl_info.schema_version != SchemaVersion::V2 && tbl_info.schema_version != SchemaVersion::V2AndV1 {
        return Ok(ResultCode::OK);
    }

    // Check if PK columns changed by comparing current schema with v2_pks columns
    let pk_changed = check_pk_changed_v2(db, tbl_name_str, tbl_info)?;

    if pk_changed {
        // PK change: drop and recreate all V2 metadata tables, then backfill
        crate::bootstrap_v2::drop_v2_tables(db, tbl_name_str)?;
        crate::bootstrap_v2::create_v2_tables(db, tbl_info)?;
        crate::backfill_v2::backfill_table_v2(db, tbl_name_str, &tbl_info.pks, &tbl_info.non_pks, tbl_info.key_is_rowid, &tbl_info.rowid_alias, tbl_info.skip_hash, false)?;
    } else {
        // Sync col_map with current schema
        sync_col_map_v2(db, &escaped, tbl_info)?
    }

    Ok(ResultCode::OK)
}

/// Check if PK columns changed by comparing current tableinfo with stored PK info.
/// PK info is stored in crsql_master at create_v2_tables time.
unsafe fn check_pk_changed_v2(
    db: *mut sqlite3,
    tbl_name: &str,
    tbl_info: &TableInfo,
) -> Result<bool, ResultCode> {
    let pk_key = format!("v2_pks_{}", tbl_name);
    let stmt = db.prepare_v2("SELECT value FROM crsql_master WHERE key = ?\0")?;
    stmt.bind_text(1, &pk_key, sqlite_nostd::Destructor::TRANSIENT)?;
    let stored = match stmt.step()? {
        ResultCode::ROW => stmt.column_text(0)?.to_string(),
        _ => return Ok(false),
    };
    drop(stmt);

    Ok(stored != crate::bootstrap_v2::compute_pk_signature(tbl_info))
}

/// Sync v2_col_map with the current table schema.
/// Adds new non-PK columns and removes deleted ones.
unsafe fn sync_col_map_v2(
    db: *mut sqlite3,
    escaped: &str,
    tbl_info: &TableInfo,
) -> Result<(), ResultCode> {
    // Get current columns in col_map
    let stmt = db.prepare_v2(&format!(
        "SELECT col_id, col_name FROM \"{}{}\"\0",
        escaped, consts::V2_COL_MAP_SUFFIX
    ))?;

    let mut existing_cols: Vec<(i64, String)> = vec![];
    while stmt.step()? == ResultCode::ROW {
        existing_cols.push((stmt.column_int64(0), stmt.column_text(1)?.to_string()));
    }
    drop(stmt);

    // Current non-PK column names from schema
    let current_names: Vec<String> = tbl_info.non_pks.iter().map(|c| c.name.clone()).collect();
    let current_set: Vec<&str> = current_names.iter().map(|s| s.as_str()).collect();

    // Remove deleted columns from col_map and collect their col_ids.
    let will_be_pk_only = tbl_info.non_pks.is_empty();
    let mut dropped_col_ids: Vec<i64> = vec![];
    for (col_id, col_name) in &existing_cols {
        if !current_set.contains(&col_name.as_str()) {
            dropped_col_ids.push(*col_id);
        }
    }

    // Batch delete dropped columns from col_map
    if !dropped_col_ids.is_empty() {
        let placeholders = dropped_col_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let stmt = db.prepare_v2(&format!(
            "DELETE FROM \"{}{}\" WHERE col_id IN ({})\0",
            escaped, consts::V2_COL_MAP_SUFFIX, placeholders
        ))?;
        for (i, col_id) in dropped_col_ids.iter().enumerate() {
            stmt.bind_int64(i as i32 + 1, *col_id)?;
        }
        stmt.step()?;
        drop(stmt);
    }

    // If the table will become PK-only, migrate one dropped column's clock
    // entries to col_id=0 to preserve row modification history.
    let col_id_mask = consts::CRSQL_COL_ID_MASK as i64;
    if will_be_pk_only && !dropped_col_ids.is_empty() {
        let migrate_col_id = dropped_col_ids.pop().unwrap();
        if migrate_col_id != 0 {
            let stmt = db.prepare_v2(&format!(
                "UPDATE \"{}{}\" SET cell_key = cell_key & ~{} WHERE cell_key & {} = ?\0",
                escaped, consts::V2_CLOCK_SUFFIX, col_id_mask, col_id_mask
            ))?;
            stmt.bind_int64(1, migrate_col_id)?;
            stmt.step()?;
            drop(stmt);
        }
    }

    // Batch delete clock entries for all remaining dropped columns
    if !dropped_col_ids.is_empty() {
        let placeholders = dropped_col_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let stmt = db.prepare_v2(&format!(
            "DELETE FROM \"{}{}\" WHERE cell_key & {} IN ({})\0",
            escaped, consts::V2_CLOCK_SUFFIX, col_id_mask, placeholders
        ))?;
        for (i, col_id) in dropped_col_ids.iter().enumerate() {
            stmt.bind_int64(i as i32 + 1, *col_id)?;
        }
        stmt.step()?;
        drop(stmt);
    }

    // Add new columns to col_map.
    // Find the next available col_id by checking against used ids.
    let existing_names: Vec<String> = existing_cols.iter().map(|(_, n)| n.clone()).collect();
    let used_col_ids: Vec<i64> = existing_cols.iter()
        .filter(|(_, n)| current_set.contains(&n.as_str()))
        .map(|(id, _)| *id)
        .collect();

    let mut next_col_id: i64 = 0;
    let mut new_col_rows: Vec<(i64, &str)> = vec![];
    for col in &tbl_info.non_pks {
        if !existing_names.contains(&col.name) {
            while used_col_ids.contains(&next_col_id) {
                next_col_id += 1;
            }
            new_col_rows.push((next_col_id, col.name.as_str()));
            next_col_id += 1;
        }
    }

    // Batch insert all new columns in a single statement
    if !new_col_rows.is_empty() {
        let placeholders = new_col_rows.iter().map(|_| "(?, ?)").collect::<Vec<_>>().join(", ");
        let stmt = db.prepare_v2(&format!(
            "INSERT INTO \"{}{}\" (col_id, col_name) VALUES {}\0",
            escaped, consts::V2_COL_MAP_SUFFIX, placeholders
        ))?;
        for (i, (col_id, col_name)) in new_col_rows.iter().enumerate() {
            let param = i as i32 * 2 + 1;
            stmt.bind_int64(param, *col_id)?;
            stmt.bind_text(param + 1, col_name, sqlite_nostd::Destructor::STATIC)?;
        }
        stmt.step()?;
        drop(stmt);
    }

    Ok(())
}
