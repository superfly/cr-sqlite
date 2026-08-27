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
    use_rowid: Option<bool>,
    skip_hash_flag: bool,
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

    // Resolve use_rowid: as_crr arg takes precedence, then schema directive, then auto.
    // Some(true)  = force rowid-key mode (caller guarantees rowids < MAX_ROWID_KEY)
    // Some(false) = force non-rowid-key mode
    // None        = auto-detect (default for INTEGER PK is non-rowid)
    let use_rowid_resolved = use_rowid.or_else(|| {
        crate::schema_directive::read_use_rowid_directive_opt(db, table).unwrap_or(None)
    });

    // Override key_is_rowid based on the resolved use_rowid preference.
    // This only matters on first registration — subsequent pull_table_info calls
    // will infer from the persisted flag.
    match use_rowid_resolved {
        Some(true) => {
            // Force rowid-key mode even for INTEGER PK tables (which default to
            // non-rowid for overflow safety). Only valid for rowid tables.
            table_info.key_is_rowid = true;
            unsafe { crate::util::set_master_value(db, &format!("use_rowid_{}", table), 1) }?;
        }
        Some(false) => {
            // Force non-rowid-key mode.
            table_info.key_is_rowid = false;
            unsafe { crate::util::set_master_value(db, &format!("use_rowid_{}", table), 0) }?;
        }
        None => {} // auto-detect — table_info already has the right value
    }

    // Override skip_hash if explicitly requested via flag.
    // The flag forces skip_hash on (even for non-integer PKs, though that would
    // be unusual). To force skip_hash off on an auto-qualified table, use the
    // schema comment `/* crsql: skip_hash=0 */` instead.
    if skip_hash_flag && !table_info.skip_hash {
        table_info.skip_hash = true;
    }

    // Persist skip_hash preference so migration path and subsequent pull_table_info
    // calls can read it when v2_pks doesn't exist yet.
    // Always persist (0 or 1) so the value is deterministic — auto-qualification
    // alone isn't persisted, but explicit directives are.
    let skip_hash_val: i32 = if table_info.skip_hash { 1 } else { 0 };
    // Only persist if there was an explicit directive or flag (not just auto-qualified).
    // For auto-qualified tables, the auto rule will re-apply on reload.
    // For explicitly enabled/disabled tables, we need to persist.
    let directive = crate::schema_directive::read_skip_hash_directive_opt(db, table).unwrap_or(None);
    if directive.is_some() || skip_hash_flag {
        unsafe { crate::util::set_master_value(db, &format!("skip_hash_{}", table), skip_hash_val as i64) }?;
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

    // For rowid tables (not converted to without_rowid), validate rowid range.
    // Enforcement is done within the existing triggers, not separate ones.
    if table_info.key_is_rowid {
        validate_rowid_range(db, table, &table_info.rowid_alias, err)?;
    }

    // Backfill appropriate metadata tables based on write mode.
    // V1=1, V2_AND_V1=2, V2=3. Dual-write (2) backfills both.
    if metadata_write_version <= config::METADATA_VERSION_V2_AND_V1 {
        backfill_table(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            is_commit_alter,
            no_tx,
        )?;
    }
    if metadata_write_version >= config::METADATA_VERSION_V2_AND_V1 {
        crate::backfill_v2::backfill_table_v2(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            table_info.key_is_rowid,
            &table_info.rowid_alias,
            table_info.skip_hash,
            no_tx,
        )?;
    }

    Ok(ResultCode::OK)
}

/// Validate that existing rowids are within the safe range for cell_key packing.
/// cell_key = (rowid << CRSQL_COL_ID_BITS) | col_id must fit in a signed INT64,
/// so rowid must be >= 0 and < 2^(63 - CRSQL_COL_ID_BITS).
/// Runtime enforcement for new writes is done in the after_insert/after_update
/// trigger handlers in Rust, gated by tbl_info.key_is_rowid.
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
            Pass 'use_rowid=0' via schema directive or use a non-rowid key strategy.",
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
    match unsafe { crate::util::get_master_value(db, "config.metadata-write-version") } {
        Ok(Some(v)) => v as core::ffi::c_int,
        _ => config::METADATA_WRITE_VERSION_DEFAULT,
    }
}
