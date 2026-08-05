extern crate alloc;
use alloc::format;
use core::ffi::c_char;
use sqlite_nostd as sqlite;
use sqlite_nostd::{Connection, ResultCode, StrRef};

use crate::bootstrap::create_clock_table;
use crate::consts;
use crate::tableinfo::{is_table_compatible, pull_table_info};
use crate::triggers::create_triggers;
use crate::{backfill_table, is_crr, remove_crr_triggers_if_exist};
use crate::config;

/**
 * Create a new crr --
 * all triggers, views, tables
 */
pub fn create_crr(
    db: *mut sqlite::sqlite3,
    _schema: &str,
    table: &str,
    is_commit_alter: bool,
    no_tx: bool,
    without_rowid: bool,
    err: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    if !is_table_compatible(db, table, err)? {
        return Err(ResultCode::ERROR);
    }
    if is_crr(db, table)? {
        return Ok(ResultCode::OK);
    }

    // We do not / can not pull this from the cached set of table infos
    // since nothing would exist in it for a table not yet made into a crr.
    // TODO: Note: we can optimize out our `ensureTableInfosAreUpToDate` by mutating our ext data
    // when upgrading stuff to CRRs
    let mut table_info = pull_table_info(db, table, err)?;

    // Override uses_rowid_key if without_rowid was requested.
    // This only matters on first registration — subsequent pull_table_info calls
    // will infer from v2_pks schema.
    if without_rowid && table_info.uses_rowid_key {
        table_info.uses_rowid_key = false;
        // Persist the without_rowid preference so migration path can read it
        // when creating v2_tables before v2_pks exists.
        let key = format!("without_rowid_{}\0", table);
        let stmt = db.prepare_v2("INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, 1)\0")?;
        stmt.bind_text(1, &key, sqlite::Destructor::TRANSIENT)?;
        stmt.step()?;
    }

    let metadata_write_version = get_metadata_write_version(db);

    // Create V2 tables if metadata write mode is dual-write (2) or V2-only (3)
    if metadata_write_version >= config::METADATA_VERSION_V2_AND_V1 {
        crate::bootstrap_v2::create_v2_tables(db, &table_info)?;
    }

    // Create V1 clock tables unless mode is V2-only (3)
    if metadata_write_version != config::METADATA_VERSION_V2 {
        create_clock_table(db, &table_info, err)?;
    }

    remove_crr_triggers_if_exist(db, table)?;
    create_triggers(db, &table_info, err)?;

    // For rowid tables (not converted to without_rowid), validate rowid range
    // and add enforcement triggers.
    if table_info.uses_rowid_key {
        validate_rowid_range(db, table, &table_info.rowid_alias, err)?;
    }

    // Backfill appropriate metadata tables based on write mode
    if metadata_write_version == config::METADATA_VERSION_V2 {
        crate::backfill_v2::backfill_table_v2(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            table_info.uses_rowid_key,
            &table_info.rowid_alias,
            no_tx,
        )?;
    } else if metadata_write_version == config::METADATA_VERSION_V2_AND_V1 {
        // Dual-write: backfill both V1 and V2
        backfill_table(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            is_commit_alter,
            no_tx,
        )?;
        crate::backfill_v2::backfill_table_v2(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            table_info.uses_rowid_key,
            &table_info.rowid_alias,
            no_tx,
        )?;
    } else {
        // V1-only: backfill V1 tables only
        backfill_table(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            is_commit_alter,
            no_tx,
        )?;
    }

    Ok(ResultCode::OK)
}

/// Validate that existing rowids are within the safe range for cell_key packing.
/// cell_key = (rowid << CRSQL_COL_ID_BITS) | col_id must fit in a signed INT64,
/// so rowid must be >= 0 and < 2^(63 - CRSQL_COL_ID_BITS).
/// Runtime enforcement for new writes is done in the after_insert/after_update
/// trigger handlers in Rust, gated by tbl_info.uses_rowid_key.
fn validate_rowid_range(
    db: *mut sqlite::sqlite3,
    table: &str,
    rowid_alias: &str,
    err: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    let escaped = crate::util::escape_ident(table);
    let alias = crate::util::escape_ident(rowid_alias);

    // Scan existing rowids for violations
    let stmt = db.prepare_v2(&format!(
        "SELECT max(\"{alias}\"), min(\"{alias}\") FROM \"{escaped}\"",
        alias = alias,
        escaped = escaped,
    ))?;
    stmt.step()?;
    // column_int64 returns 0 for NULL (empty table), which is in range — safe to skip
    let max_rowid = stmt.column_int64(0);
    let min_rowid = stmt.column_int64(1);

    if max_rowid >= consts::MAX_ROWID_KEY || min_rowid < 0 {
        err.set(&format!(
            "Table {table} has rowids outside the safe range [0, {max_key}). \
            Found range [{min_rowid}, {max_rowid}]. \
            cell_key = (rowid << {bits}) | col_id must fit in a signed INT64. \
            Pass 'without_rowid' to crsql_as_crr to use the table PK columns instead of rowid as the internal key.",
            table = table,
            max_key = consts::MAX_ROWID_KEY,
            min_rowid = min_rowid,
            max_rowid = max_rowid,
            bits = consts::CRSQL_COL_ID_BITS,
        ));
        return Err(ResultCode::ERROR);
    }

    Ok(ResultCode::OK)
}

/// Read the persisted metadata-write-version config from crsql_master.
/// Returns METADATA_WRITE_VERSION_DEFAULT (1) if not set or table doesn't exist.
fn get_metadata_write_version(db: *mut sqlite::sqlite3) -> core::ffi::c_int {
    let stmt = db.prepare_v2(
        "SELECT value FROM crsql_master WHERE key = 'config.metadata-write-version'"
    );
    match stmt {
        Ok(stmt) => {
            if stmt.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                stmt.column_int(0)
            } else {
                config::METADATA_WRITE_VERSION_DEFAULT
            }
        }
        Err(_) => config::METADATA_WRITE_VERSION_DEFAULT,
    }
}
