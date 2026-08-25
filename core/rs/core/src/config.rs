use alloc::format;
use alloc::vec::Vec;

use core::ffi::c_int;
use sqlite::{Connection, Context};
use sqlite_nostd as sqlite;
use sqlite_nostd::{ManagedStmt, ResultCode, Value};

use crate::c::crsql_ExtData;

pub const MERGE_EQUAL_VALUES: &str = "merge-equal-values";
pub const METADATA_WRITE_VERSION: &str = "metadata-write-version";
pub const METADATA_USE_VERSION: &str = "metadata-use-version";
pub const SYNC_LOG_VERSION: &str = "sync-log-version";

/// Default metadata write version: 1 = V1 (legacy), 2 = V2&V1 (dual write), 3 = V2 only
pub const METADATA_WRITE_VERSION_DEFAULT: c_int = 1;
/// Default metadata use version: 1 = V1 (legacy), 2 = V2
pub const METADATA_USE_VERSION_DEFAULT: c_int = 1;
/// Default sync log version: 1 = V1 (per-column rows), 2 = V2 (packed)
pub const SYNC_LOG_VERSION_DEFAULT: c_int = 1;

// Integer values for metadata-write-version config option
// Migration order: 1 -> 2 -> 3 (forward only, except 2 -> 1 rollback)
pub const METADATA_VERSION_V1: c_int = 1;
pub const METADATA_VERSION_V2_AND_V1: c_int = 2;  // dual write, migration in progress
pub const METADATA_VERSION_V2: c_int = 3;          // V2 only, V1 tables dropped

pub extern "C" fn crsql_config_set(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    let name = args[0].text();
    let ext_data = ctx.user_data() as *mut crsql_ExtData;

    let value = match name {
        MERGE_EQUAL_VALUES => {
            let value = args[1];
            unsafe { (*ext_data).mergeEqualValues = value.int() };
            value
        }
        METADATA_WRITE_VERSION => {
            let new_val = args[1].int();
            let old_val = unsafe { (*ext_data).metadataWriteVersion };
            if !validate_write_version_transition(old_val, new_val) {
                ctx.result_error("Invalid metadata-write-version transition");
                ctx.result_error_code(ResultCode::ERROR);
                return;
            }
            let db = ctx.db_handle();
            // Direct 1->3 transition: skip migration/cleanup, just verify no CRR tables exist
            if old_val == METADATA_VERSION_V1 && new_val == METADATA_VERSION_V2 {
                match has_no_crr_tables(db) {
                    Ok(true) => {
                        // No CRR tables — safe to go directly to V2-only
                    },
                    Ok(false) => {
                        ctx.result_error("Cannot set metadata-write-version to v2 directly: existing CRR tables found. Migrate via v2&v1 first.");
                        ctx.result_error_code(ResultCode::ERROR);
                        return;
                    },
                    Err(rc) => {
                        ctx.result_error("Failed to check for existing CRR tables");
                        ctx.result_error_code(rc);
                        return;
                    }
                }
            } else {
                // Any other transition requires prior cleanup tasks to be done
                match is_cleanup_complete(db) {
                    Ok(true) => {},
                    Ok(false) => {
                        ctx.result_error("Cannot transition metadata-write-version: cleanup tasks still pending");
                        ctx.result_error_code(ResultCode::ERROR);
                        return;
                    },
                    Err(rc) => {
                        ctx.result_error("Failed to check cleanup status");
                        ctx.result_error_code(rc);
                        return;
                    }
                }
                // Setting to v2&v1 queues migration tasks for all V1 CRR tables
                if new_val == METADATA_VERSION_V2_AND_V1 && old_val == METADATA_VERSION_V1 {
                    // Create V2 tables for all existing CRR tables so dual-write
                    // triggers have somewhere to write immediately.
                    if let Err(rc) = create_v2_tables_for_existing_crrs(db) {
                        ctx.result_error("Failed to create V2 tables during transition");
                        ctx.result_error_code(rc);
                        return;
                    }
                    if let Err(rc) = queue_migration_tasks(db) {
                        ctx.result_error("Failed to queue migration tasks");
                        ctx.result_error_code(rc);
                        return;
                    }
                }
                // Transitioning to v2 (dropping V1 tables) requires migration to be complete
                if new_val == METADATA_VERSION_V2 {
                    match is_migration_complete(db) {
                        Ok(true) => {
                            // Queue V1 table cleanup tasks
                            if let Err(rc) = queue_v1_cleanup_tasks(db) {
                                ctx.result_error("Failed to queue V1 cleanup tasks");
                                ctx.result_error_code(rc);
                                return;
                            }
                        },
                        Ok(false) => {
                            ctx.result_error("Cannot set metadata-write-version to v2: migration not complete for all tables");
                            ctx.result_error_code(ResultCode::ERROR);
                            return;
                        },
                        Err(rc) => {
                            ctx.result_error("Failed to check migration status");
                            ctx.result_error_code(rc);
                            return;
                        }
                    }
                }
                // Rolling back to v1 queues V2 table cleanup tasks and aborts migration
                if new_val == METADATA_VERSION_V1 && old_val == METADATA_VERSION_V2_AND_V1 {
                    // Clear any pending migration markers since we're aborting migration
                    if let Err(rc) = clear_migration_markers(db) {
                        ctx.result_error("Failed to clear migration markers");
                        ctx.result_error_code(rc);
                        return;
                    }
                    if let Err(rc) = queue_v2_cleanup_tasks(db) {
                        ctx.result_error("Failed to queue V2 cleanup tasks");
                        ctx.result_error_code(rc);
                        return;
                    }
                }
            }
            // Auto-cascade dependent config values to prevent invalid states
            if new_val == METADATA_VERSION_V1 {
                // Rolling back to V1: force use-version and sync-log-version to V1
                unsafe { (*ext_data).metadataUseVersion = 1; }
                unsafe { (*ext_data).syncLogVersion = 1; }
            } else if new_val == METADATA_VERSION_V2 {
                // Moving to V2-only: force use-version to V2
                unsafe { (*ext_data).metadataUseVersion = 2; }
            }
            unsafe { (*ext_data).metadataWriteVersion = new_val };
            args[1]
        }
        METADATA_USE_VERSION => {
            let new_val = args[1].int();
            let old_val = unsafe { (*ext_data).metadataUseVersion };
            if !validate_use_version_transition(old_val, new_val, unsafe { (*ext_data).metadataWriteVersion }) {
                ctx.result_error("Invalid metadata-use-version transition");
                ctx.result_error_code(ResultCode::ERROR);
                return;
            }
            // Setting to v2 requires all migrations to be complete
            if new_val == 2 {
                let db = ctx.db_handle();
                if check_migration_complete_or_error(ctx, db, "metadata-use-version").is_err() {
                    return;
                }
            }
            unsafe { (*ext_data).metadataUseVersion = new_val };
            args[1]
        }
        SYNC_LOG_VERSION => {
            let new_val = args[1].int();
            let old_val = unsafe { (*ext_data).syncLogVersion };
            if !validate_sync_log_transition(old_val, new_val, unsafe { (*ext_data).metadataUseVersion }, unsafe { (*ext_data).metadataWriteVersion }) {
                ctx.result_error("Invalid sync-log-version transition");
                ctx.result_error_code(ResultCode::ERROR);
                return;
            }
            // Setting to v2 requires all migrations to be complete
            if new_val == 2 {
                let db = ctx.db_handle();
                if check_migration_complete_or_error(ctx, db, "sync-log-version").is_err() {
                    return;
                }
            }
            unsafe { (*ext_data).syncLogVersion = new_val };
            args[1]
        }
        _ => {
            ctx.result_error("Unknown setting name");
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    };

    let db = ctx.db_handle();
    match insert_config_setting(db, name, value) {
        Ok((_stmt, value)) => {
            ctx.result_value(value);
        }
        Err(rc) => {
            ctx.result_error("Could not persist config in database");
            ctx.result_error_code(rc);
            return;
        }
    }
}

fn insert_config_setting(
    db: *mut sqlite_nostd::sqlite3,
    name: &str,
    value: *mut sqlite::value,
) -> Result<(ManagedStmt, *mut sqlite::value), ResultCode> {
    let stmt =
        db.prepare_v2("INSERT OR REPLACE INTO crsql_master VALUES (?, ?) RETURNING value")?;

    stmt.bind_text(1, &format!("config.{name}"), sqlite::Destructor::TRANSIENT)?;
    stmt.bind_value(2, value)?;

    if let ResultCode::ROW = stmt.step()? {
        let res = stmt.column_value(0)?;
        // Res will get invalidated when stmt gets dropped
        // The lifetime of res is not currently checked by the compiler
        Ok((stmt, res))
    } else {
        Err(ResultCode::ERROR)
    }
}

pub extern "C" fn crsql_config_get(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    let name = args[0].text();

    match name {
        MERGE_EQUAL_VALUES => {
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            ctx.result_int(unsafe { (*ext_data).mergeEqualValues });
        }
        METADATA_WRITE_VERSION => {
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            ctx.result_int(unsafe { (*ext_data).metadataWriteVersion });
        }
        METADATA_USE_VERSION => {
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            ctx.result_int(unsafe { (*ext_data).metadataUseVersion });
        }
        SYNC_LOG_VERSION => {
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            ctx.result_int(unsafe { (*ext_data).syncLogVersion });
        }
        _ => {
            ctx.result_error("Unknown setting name");
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    }
}

/// Validate metadata-write-version transitions.
/// Allowed: 1->2, 2->3, 2->1 (rollback), 1->3 (only when no CRR tables exist)
/// Forbidden: 3->2, 3->1
fn validate_write_version_transition(old: c_int, new: c_int) -> bool {
    match (old, new) {
        // v1 -> v2&v1: forward, starts migration
        (METADATA_VERSION_V1, METADATA_VERSION_V2_AND_V1) => true,
        // v2&v1 -> v2: forward, V1 tables will be dropped
        (METADATA_VERSION_V2_AND_V1, METADATA_VERSION_V2) => true,
        // v2&v1 -> v1: rollback, V1 tables were kept in sync
        (METADATA_VERSION_V2_AND_V1, METADATA_VERSION_V1) => true,
        // v1 -> v2: direct V2-only, only allowed when no CRR tables exist
        // The actual check for empty DB is done in crsql_config_set
        (METADATA_VERSION_V1, METADATA_VERSION_V2) => true,
        // no-op
        _ if old == new => true,
        // everything else forbidden
        _ => false,
    }
}

/// Validate metadata-use-version transitions.
/// v1->v2: forward (guarded by write version being v2 or v2&v1)
/// v2->v1: only if write version is v1 or v2&v1 (V1 tables still active)
fn validate_use_version_transition(old: c_int, new: c_int, write_version: c_int) -> bool {
    match (old, new) {
        // v1 -> v2: forward, requires V2 tables being written
        (1, 2) => write_version == METADATA_VERSION_V2 || write_version == METADATA_VERSION_V2_AND_V1,
        // v2 -> v1: rollback, only if V1 tables still active
        (2, 1) => write_version == METADATA_VERSION_V1 || write_version == METADATA_VERSION_V2_AND_V1,
        // no-op
        _ if old == new => true,
        _ => false,
    }
}

/// Validate sync-log-version transitions.
/// v1->v2: forward, requires use-version=v2 and write-version=v2 or v2&v1
/// v2->v1: rollback, requires use-version=v1
fn validate_sync_log_transition(old: c_int, new: c_int, use_version: c_int, write_version: c_int) -> bool {
    match (old, new) {
        // v1 -> v2: forward, requires use=v2 and write=v2/v2&v1
        (1, 2) => use_version == 2 && (write_version == METADATA_VERSION_V2 || write_version == METADATA_VERSION_V2_AND_V1),
        // v2 -> v1: rollback, requires use=v1
        (2, 1) => use_version == 1,
        // no-op
        _ if old == new => true,
        _ => false,
    }
}

/// Find all tables in sqlite_master whose name ends with `suffix`,
/// returning the base table names (with the suffix stripped).
pub fn find_tables_with_suffix(
    db: *mut sqlite_nostd::sqlite3,
    suffix: &str,
) -> Result<Vec<alloc::string::String>, ResultCode> {
    let sql = format!(
        "SELECT DISTINCT name FROM sqlite_master WHERE name LIKE '%{}'\0",
        suffix
    );
    let stmt = db.prepare_v2(&sql)?;
    let mut table_names: Vec<alloc::string::String> = Vec::new();
    while stmt.step()? == ResultCode::ROW {
        let name = stmt.column_text(0)?;
        if let Some(base) = name.strip_suffix(suffix) {
            table_names.push(alloc::string::String::from(base));
        }
    }
    Ok(table_names)
}

/// Queue a task in crsql_master by inserting a key with value 0.
/// The key is formed as `{key_prefix}_{table_name}`.
fn queue_task(
    db: *mut sqlite_nostd::sqlite3,
    key_prefix: &str,
    table_name: &str,
) -> Result<(), ResultCode> {
    let key = format!("{}_{}", key_prefix, table_name);
    unsafe { crate::util::set_master_value(db, &key, 0) }
}

/// Check that all migration tasks are complete, setting an error on the
/// context if not. Returns `Ok(())` if migration is complete, or `Err(())`
/// if an error was set on the context (caller should return immediately).
fn check_migration_complete_or_error(
    ctx: *mut sqlite::context,
    db: *mut sqlite_nostd::sqlite3,
    config_name: &str,
) -> Result<(), ()> {
    match is_migration_complete(db) {
        Ok(true) => Ok(()),
        Ok(false) => {
            ctx.result_error(&format!(
                "Cannot set {} to v2: migration not complete for all tables",
                config_name
            ));
            ctx.result_error_code(ResultCode::ERROR);
            Err(())
        }
        Err(rc) => {
            ctx.result_error("Failed to check migration status");
            ctx.result_error_code(rc);
            Err(())
        }
    }
}

/// Create V2 metadata tables for all existing CRR tables that don't have them yet.
/// Called during the V1→V2AndV1 transition so dual-write triggers have V2 tables ready.
fn create_v2_tables_for_existing_crrs(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let table_names = find_tables_with_suffix(db, "__crsql_clock")?;

    for tbl_name in &table_names {
        // Check if V2 tables already exist (e.g., table was created in dual-write mode)
        let check_sql = format!(
            "SELECT 1 FROM sqlite_master WHERE name = '{}__crsql_v2_pks'\0",
            tbl_name
        );
        let check = db.prepare_v2(&check_sql)?;
        if check.step()? == ResultCode::ROW {
            continue; // V2 tables already exist
        }

        // Pull table info and create V2 tables
        let mut err: *mut core::ffi::c_char = core::ptr::null_mut();
        let tbl_info = crate::tableinfo::pull_table_info(db, tbl_name, &mut err)?;
        crate::bootstrap_v2::create_v2_tables(db, &tbl_info)?;
    }

    Ok(())
}

/// Queue migration tasks for all V1 CRR tables.
/// Sets a progress marker of 0 for each table that has __crsql_clock (V1) tables.
fn queue_migration_tasks(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let table_names = find_tables_with_suffix(db, "__crsql_clock")?;
    for tbl_name in &table_names {
        queue_task(db, "migration_v1_to_v2_migration", tbl_name)?;
    }
    Ok(())
}

/// Check if all migration tasks are complete (no pending migration markers in crsql_master).
fn is_migration_complete(db: *mut sqlite_nostd::sqlite3) -> Result<bool, ResultCode> {
    let sql = "SELECT count(*) FROM crsql_master WHERE key LIKE 'migration_v1_to_v2_migration_%'\0";
    let stmt = db.prepare_v2(sql)?;
    stmt.step()?;
    let count = stmt.column_int64(0);
    Ok(count == 0)
}

/// Clear all pending migration markers. Called when aborting migration (2->1 rollback).
fn clear_migration_markers(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let sql = "DELETE FROM crsql_master WHERE key LIKE 'migration_v1_to_v2_migration_%'\0";
    db.exec_safe(sql)?;
    Ok(())
}

/// Queue V1 table cleanup tasks for all CRR tables that have V1 clock tables.
/// Called when transitioning from v2&v1 to v2 (V1 tables no longer needed).
fn queue_v1_cleanup_tasks(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let table_names = find_tables_with_suffix(db, "__crsql_clock")?;
    for tbl_name in &table_names {
        queue_task(db, "cleanup_v1_tables", tbl_name)?;
    }
    Ok(())
}

/// Queue V2 table cleanup tasks for all CRR tables that have V2 tables.
/// Called when rolling back from v2&v1 to v1 (V2 tables no longer needed).
fn queue_v2_cleanup_tasks(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let table_names = find_tables_with_suffix(db, "__crsql_v2_clock")?;
    for tbl_name in &table_names {
        queue_task(db, "cleanup_v2_tables", tbl_name)?;
    }
    Ok(())
}

/// Check if all cleanup tasks are complete (no pending cleanup markers in crsql_master).
fn is_cleanup_complete(db: *mut sqlite_nostd::sqlite3) -> Result<bool, ResultCode> {
    let sql = "SELECT count(*) FROM crsql_master WHERE key LIKE 'cleanup_v1_tables_%' OR key LIKE 'cleanup_v2_tables_%'\0";
    let stmt = db.prepare_v2(sql)?;
    stmt.step()?;
    let count = stmt.column_int64(0);
    Ok(count == 0)
}

/// Check if the database has no existing CRR tables.
/// Used to determine if a direct 1->3 (V2-only) transition is safe.
fn has_no_crr_tables(db: *mut sqlite_nostd::sqlite3) -> Result<bool, ResultCode> {
    let sql = "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE '%__crsql_itrig'\0";
    let stmt = db.prepare_v2(sql)?;
    stmt.step()?;
    let count = stmt.column_int64(0);
    Ok(count == 0)
}
