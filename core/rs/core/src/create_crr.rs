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

    create_crr_inner(db, _schema, table, is_commit_alter, no_tx, use_rowid, skip_hash_flag, err)
}

fn create_crr_inner(
    db: *mut sqlite::sqlite3,
    _schema: &str,
    table: &str,
    is_commit_alter: bool,
    no_tx: bool,
    use_rowid: Option<bool>,
    skip_hash_flag: bool,
    err: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    if is_crr(db, table)? {
        return Ok(ResultCode::OK);
    }

    // If V2 metadata tables don't exist for this table, clear any stale
    // crsql_master flags from a previous incarnation (e.g., table was dropped
    // and re-created with a different schema).
    // If stale V2 tables do exist (orphaned by a DROP TABLE), drop them too —
    // they carry data from the old incarnation and would corrupt the new one.
    // Skip this check during crsql_commit_alter (is_commit_alter=true) because
    // crsql_begin_alter deliberately drops triggers, which would make the stale
    // detection false-positive and destroy valid V2 metadata.
    // This is safe for corrosion when triggers get accidentally dropped
    // because corrosion only calls crsql_as_crr once per table (during schema apply),
    // never re-calling it on an existing CRR.
    if !is_commit_alter {
        let has_v2 = crate::bootstrap_v2::has_v2_tables(db, table)?;
        if !has_v2 {
            unsafe { crate::util::clear_crr_mode_flags(db, table); }
        } else {
            // V2 tables exist — check if they're stale (base table was dropped and
            // re-created). If no crsql triggers exist, the table was likely dropped
            // and re-created — drop the stale V2 tables and clear flags.
            let has_triggers = db.prepare_v2(&format!(
                "SELECT 1 FROM sqlite_master WHERE type='trigger' AND tbl_name='{}' AND name LIKE '%crsql%'",
                crate::util::escape_ident_as_value(table)
            ))?;
            let has_triggers = has_triggers.step()? == ResultCode::ROW;
            if !has_triggers {
                crate::teardown_v2::remove_crr_v2_tables(db, table)?;
                unsafe { crate::util::clear_crr_mode_flags(db, table); }
            }
        }
    }

    // We do not / can not pull this from the cached set of table infos
    // since nothing would exist in it for a table not yet made into a crr.
    // TODO: Note: we can optimize out our `ensureTableInfosAreUpToDate` by mutating our ext data
    // when upgrading stuff to CRRs
    let mut table_info = pull_table_info(db, table, err)?;

    let metadata_write_version = get_metadata_write_version(db, err)?;

    // Resolve use_rowid: as_crr arg takes precedence, then schema directive, then auto.
    // Some(true)  = force rowid-key mode (caller guarantees rowids < MAX_ROWID_KEY)
    // Some(false) = force non-rowid-key mode
    // None        = auto-detect (default for INTEGER PK is non-rowid)
    let use_rowid_directive = crate::schema_directive::read_use_rowid_directive_opt(db, table)
        .map_err(|e| {
            err.set(&format!("directive read error: {}", e));
            e
        })?;
    let use_rowid_resolved = use_rowid.or(use_rowid_directive);

    // Override key_is_rowid based on the resolved use_rowid preference.
    // This only matters on first registration — subsequent pull_table_info calls
    // will infer from the persisted flag.
    match use_rowid_resolved {
        Some(true) => {
            // Force rowid-key mode. Only allowed for INTEGER PRIMARY KEY tables
            // that are not WITHOUT ROWID — implicit rowids (tables without
            // INTEGER PK) are unstable and can be renumbered by VACUUM, and
            // WITHOUT ROWID tables have no rowid at all.
            if !table_info.has_integer_pk {
                err.set(&format!(
                    "use_rowid=1 is only allowed on INTEGER PRIMARY KEY tables. \
                    Table '{table}' does not have an INTEGER PRIMARY KEY — \
                    its implicit rowid is unstable under VACUUM and cannot be used as a persistent key."
                ));
                return Err(ResultCode::ERROR);
            }
            if table_info.is_without_rowid {
                err.set(&format!(
                    "use_rowid=1 is not allowed on WITHOUT ROWID tables. \
                    Table '{table}' is WITHOUT ROWID — it has no stable rowid to use as a key."
                ));
                return Err(ResultCode::ERROR);
            }
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
    // Composite PKs silently ignore the flag (design: "silently ignored,
    // falls back to hash mode"). Single-column PKs opt in.
    if skip_hash_flag && !table_info.skip_hash {
        if table_info.pks.len() == 1 {
            table_info.skip_hash = true;
            // Recompute skip_hash_pk_col — it was empty because pull_table_info
            // ran before the flag override.
            table_info.skip_hash_pk_col = crate::util::escape_ident(&table_info.pks[0].name);
        } else {
            // Composite PK: skip_hash is not supported — return an error
            // rather than silently ignoring the flag.
            err.set(&format!(
                "skip_hash is only supported on tables with a single primary key column. \
                Table '{table}' has {} primary key columns.",
                table_info.pks.len()
            ));
            return Err(ResultCode::ERROR);
        }
    }

    // Persist skip_hash preference so migration path and subsequent pull_table_info
    // calls can read it when v2_pks doesn't exist yet.
    // Always persist (0 or 1) so the value is deterministic — auto-qualification
    // alone isn't persisted, but explicit directives are.
    let skip_hash_val: i32 = if table_info.skip_hash { 1 } else { 0 };
    // Only persist if there was an explicit directive or flag (not just auto-qualified).
    // For auto-qualified tables, the auto rule will re-apply on reload.
    // For explicitly enabled/disabled tables, we need to persist.
    let directive = crate::schema_directive::read_skip_hash_directive_opt(db, table)
        .map_err(|e| {
            err.set(&format!("directive read error: {}", e));
            e
        })?;
    if directive.is_some() || skip_hash_flag {
        unsafe { crate::util::set_master_value(db, &format!("skip_hash_{}", table), skip_hash_val as i64) }?;
    }

    // Validate rowid range BEFORE creating any metadata tables or triggers.
    // If validation fails, the table is not partially registered.
    if table_info.key_is_rowid {
        validate_rowid_range(db, table, &table_info.rowid_alias, err)?;
    }

    // Create V2 tables if metadata write mode is dual-write (2) or V2-only (3)
    if metadata_write_version >= config::METADATA_VERSION_V2_AND_V1 {
        if let Err(rc) = crate::bootstrap_v2::create_v2_tables(db, &table_info) {
            err.set(&format!(
                "create_v2_tables failed for {table} (key_is_rowid={}, skip_hash={}): {:?}",
                table_info.key_is_rowid, table_info.skip_hash, rc
            ));
            return Err(rc);
        }
    }

    // Create V1 clock tables unless mode is V2-only (3)
    if metadata_write_version != config::METADATA_VERSION_V2 {
        create_clock_table(db, &table_info, err)?;
    }

    remove_crr_triggers_if_exist(db, table)?;
    if let Err(rc) = create_triggers(db, &table_info, err) {
        err.set(&format!("create_triggers failed for {table}: {:?}", rc));
        return Err(rc);
    }

    // For rowid tables (not converted to without_rowid), validate rowid range.
    // Enforcement is done within the existing triggers, not separate ones.
    // (Validation was already done above before table/trigger creation.)

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
        if let Err(rc) = crate::backfill_v2::backfill_table_v2(
            db,
            table,
            &table_info.pks,
            &table_info.non_pks,
            table_info.key_is_rowid,
            &table_info.rowid_alias,
            table_info.skip_hash,
            no_tx,
        ) {
            err.set(&format!(
                "backfill_table_v2 failed for {table} (key_is_rowid={}, skip_hash={}): {:?}",
                table_info.key_is_rowid, table_info.skip_hash, rc
            ));
            return Err(rc);
        }
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
/// Propagates errors from the underlying read instead of silently defaulting.
fn get_metadata_write_version(
    db: *mut sqlite::sqlite3,
    err: *mut *mut c_char,
) -> Result<core::ffi::c_int, ResultCode> {
    match unsafe { crate::util::get_master_value(db, "config.metadata-write-version") } {
        Ok(Some(v)) => Ok(v as core::ffi::c_int),
        Ok(None) => Ok(config::METADATA_WRITE_VERSION_DEFAULT),
        Err(e) => {
            err.set(&format!("metadata write version read error: {}", e));
            Err(e)
        }
    }
}
