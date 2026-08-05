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
    let tbl_name_val = crate::util::escape_ident_as_value(tbl_name_str);

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
    let pk_changed = check_pk_changed_v2(db, tbl_name_str, &escaped)?;

    if pk_changed {
        // PK change: drop and recreate all V2 metadata tables, then backfill
        crate::bootstrap_v2::drop_v2_tables(db, tbl_name_str)?;
        crate::bootstrap_v2::create_v2_tables(db, tbl_info)?;
        crate::backfill_v2::backfill_table_v2(db, tbl_name_str, &tbl_info.pks, &tbl_info.non_pks, tbl_info.uses_rowid_key, &tbl_info.rowid_alias, false)?;
    } else {
        // Sync col_map with current schema
        sync_col_map_v2(db, &escaped, tbl_name_str, tbl_info, ext_data)?
    }

    Ok(ResultCode::OK)
}

/// Check if PK columns changed by comparing pragma_table_info with v2_pks schema
unsafe fn check_pk_changed_v2(
    db: *mut sqlite3,
    tbl_name: &str,
    escaped: &str,
) -> Result<bool, ResultCode> {
    // Check if v2_pks table exists
    let stmt = db.prepare_v2(&format!(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='{}{}'\0",
        escaped, consts::V2_PKS_SUFFIX
    ))?;
    stmt.step()?;
    let exists = stmt.column_int(0);
    drop(stmt);

    if exists == 0 {
        // No V2 tables yet, nothing to compare
        return Ok(false);
    }

    // For rowid-key tables, v2_pks uses __crsql_key (not the actual PK column name).
    // Check if v2_pks has __crsql_key as a column — if so, it's a rowid-key table
    // and PK changes are detected by comparing the rowid alias, not column names.
    let rowid_check = db.prepare_v2(&format!(
        "SELECT count(*) FROM pragma_table_info('{escaped}{pks_suffix}') WHERE name = '__crsql_key'\0",
        escaped = escaped,
        pks_suffix = consts::V2_PKS_SUFFIX,
    ))?;
    rowid_check.step()?;
    let is_rowid_key = rowid_check.column_int(0) > 0;
    drop(rowid_check);

    if is_rowid_key {
        // Rowid-key table: v2_pks only has __crsql_key, hashed_pk, cl.
        // PK change = the rowid alias column changed.
        // We detect this by checking if the current PK column count is still 1
        // and it's an INTEGER PRIMARY KEY (rowid alias).
        // For simplicity, just check PK column count hasn't changed.
        let stmt = db.prepare_v2(&format!(
            "SELECT count(*) FROM pragma_table_info('{tbl_name_val}') WHERE pk > 0\0",
            tbl_name_val = crate::util::escape_ident_as_value(tbl_name),
        ))?;
        stmt.step()?;
        let pk_count = stmt.column_int(0);
        // Rowid-key tables always have exactly 1 PK column
        return Ok(pk_count != 1);
    }

    // Non-rowid tables: compare PK column names directly
    let stmt = db.prepare_v2(&format!(
        "SELECT count(*) FROM (
            SELECT name FROM pragma_table_info('{tbl_name_val}') WHERE pk > 0
            EXCEPT
            SELECT name FROM pragma_table_info('{escaped}{pks_suffix}')
            WHERE name NOT IN ('__crsql_key', 'hashed_pk', 'cl')
        ) UNION ALL
        SELECT count(*) FROM (
            SELECT name FROM pragma_table_info('{escaped}{pks_suffix}')
            WHERE name NOT IN ('__crsql_key', 'hashed_pk', 'cl')
            EXCEPT
            SELECT name FROM pragma_table_info('{tbl_name_val}') WHERE pk > 0
        )\0",
        tbl_name_val = crate::util::escape_ident_as_value(tbl_name),
        escaped = escaped,
        pks_suffix = consts::V2_PKS_SUFFIX,
    ))?;
    stmt.step()?;
    let diff = stmt.column_int(0);
    Ok(diff > 0)
}

/// Sync v2_col_map with the current table schema.
/// Adds new non-PK columns and removes deleted ones.
unsafe fn sync_col_map_v2(
    db: *mut sqlite3,
    escaped: &str,
    tbl_name: &str,
    tbl_info: &TableInfo,
    ext_data: *mut crsql_ExtData,
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

    // Remove deleted columns from col_map.
    // If the table will become PK-only, migrate the last dropped column's clock
    // entries to col_id=0 (preserving db_version, seq, ts, site_id) instead of
    // deleting them. Other dropped columns' clock entries are deleted normally.
    let will_be_pk_only = tbl_info.non_pks.is_empty();
    let mut dropped_col_ids: Vec<i64> = vec![];
    for (col_id, col_name) in &existing_cols {
        if !current_set.contains(&col_name.as_str()) {
            dropped_col_ids.push(*col_id);
            // Delete from col_map
            let stmt = db.prepare_v2(&format!(
                "DELETE FROM \"{}{}\" WHERE col_id = ?\0",
                escaped, consts::V2_COL_MAP_SUFFIX
            ))?;
            stmt.bind_int64(1, *col_id)?;
            stmt.step()?;
            drop(stmt);
        }
    }

    if will_be_pk_only && !dropped_col_ids.is_empty() {
        // Migrate the last dropped column's clock entries to col_id=0.
        // This preserves the row modification history (db_version, seq, ts, site_id).
        let migrate_col_id = *dropped_col_ids.last().unwrap();
        let col_id_mask = consts::CRSQL_COL_ID_MASK as i64;

        if migrate_col_id != 0 {
            // Update cell_key: replace col_id part with 0 for only this column's entries
            // cell_key = (pk_key << COL_ID_BITS) | col_id
            // new_cell_key = (cell_key >> COL_ID_BITS) << COL_ID_BITS = cell_key & ~col_id_mask
            let stmt = db.prepare_v2(&format!(
                "UPDATE \"{}{}\" SET cell_key = cell_key & ~{} WHERE cell_key & {} = ?\0",
                escaped, consts::V2_CLOCK_SUFFIX, col_id_mask, col_id_mask
            ))?;
            stmt.bind_int64(1, migrate_col_id)?;
            stmt.step()?;
            drop(stmt);
        }

        // Delete clock entries for all other dropped columns (not the migrated one)
        for col_id in &dropped_col_ids {
            if *col_id != migrate_col_id {
                let stmt = db.prepare_v2(&format!(
                    "DELETE FROM \"{}{}\" WHERE cell_key & {} = ?\0",
                    escaped, consts::V2_CLOCK_SUFFIX, col_id_mask
                ))?;
                stmt.bind_int64(1, *col_id)?;
                stmt.step()?;
                drop(stmt);
            }
        }
    } else {
        // Normal case: delete clock entries for all dropped columns
        let col_id_mask = consts::CRSQL_COL_ID_MASK;
        for col_id in &dropped_col_ids {
            let stmt = db.prepare_v2(&format!(
                "DELETE FROM \"{}{}\" WHERE cell_key & {} = ?\0",
                escaped, consts::V2_CLOCK_SUFFIX, col_id_mask
            ))?;
            stmt.bind_int64(1, *col_id)?;
            stmt.step()?;
            drop(stmt);
        }
    }

    // Add new columns to col_map.
    // Always try col_id=0 first (important for PK-only → normal transition
    // where sentinel entries at col_id=0 become regular clock entries).
    let existing_names: Vec<String> = existing_cols.iter().map(|(_, n)| n.clone()).collect();
    let remaining_col_ids: Vec<i64> = existing_cols.iter()
        .filter(|(_, n)| current_set.contains(&n.as_str()))
        .map(|(id, _)| *id)
        .collect();
    let has_col_id_0 = remaining_col_ids.contains(&0);
    let max_col_id = remaining_col_ids.iter().max().copied().unwrap_or(-1);

    let mut next_col_id: i64 = if !has_col_id_0 { 0 } else { max_col_id + 1 };
    for col in &tbl_info.non_pks {
        if !existing_names.contains(&col.name) {
            let stmt = db.prepare_v2(&format!(
                "INSERT INTO \"{}{}\" (col_id, col_name) VALUES (?, ?)\0",
                escaped, consts::V2_COL_MAP_SUFFIX
            ))?;
            stmt.bind_int64(1, next_col_id)?;
            stmt.bind_text(2, &col.name, sqlite_nostd::Destructor::STATIC)?;
            stmt.step()?;
            drop(stmt);
            // After using slot 0, continue from max_col_id + 1
            if next_col_id == 0 && !has_col_id_0 {
                next_col_id = max_col_id + 1;
            } else {
                next_col_id += 1;
            }
        }
    }

    // If the table became PK-only, create sentinel clock entries at col_id=0
    // for any rows in v2_pks that don't already have one (e.g., rows that had
    // no clock entries at all before the column was dropped).
    if will_be_pk_only {
        let col_id_bits = consts::CRSQL_COL_ID_BITS as i64;
        let db_version = crate::db_version::next_db_version(db, ext_data)?;
        let ts_val = {
            let ts_str = (*ext_data).timestamp.to_string();
            let ts = ts_str.parse::<i64>().map_err(|_| ResultCode::ERROR)?;
            if ts == 0 {
                crate::debug::debug_log("commit_alter: timestamp not set — call crsql_set_ts() first");
                return Err(ResultCode::ERROR);
            }
            ts
        };
        let seq = (*ext_data).seq;

        let sql = format!(
            "INSERT OR IGNORE INTO \"{}{}\" (cell_key, col_version, site_id, db_version, seq, ts)
             SELECT (p.__crsql_key << {}) | 0, 1, 0, ?, ?, ?
             FROM \"{}{}\" p
             WHERE NOT EXISTS (
               SELECT 1 FROM \"{}{}\" c
               WHERE c.cell_key = (p.__crsql_key << {}) | 0
             )\0",
            escaped, consts::V2_CLOCK_SUFFIX,
            col_id_bits,
            escaped, consts::V2_PKS_SUFFIX,
            escaped, consts::V2_CLOCK_SUFFIX,
            col_id_bits,
        );
        let stmt = db.prepare_v2(&sql)?;
        stmt.bind_int64(1, db_version)?;
        stmt.bind_int(2, seq)?;
        stmt.bind_int64(3, ts_val)?;
        stmt.step()?;
    }

    Ok(())
}
