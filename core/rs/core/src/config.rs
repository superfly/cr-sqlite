use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::ffi::c_int;
use num_traits::FromPrimitive;
use sqlite::{Connection, Context};
use sqlite_nostd as sqlite;
use sqlite_nostd::{ManagedStmt, ResultCode, Value};

use crate::c::crsql_ExtData;

pub const MERGE_EQUAL_VALUES: &str = "merge-equal-values";
pub const METADATA_WRITE_VERSION: &str = "metadata-write-version";
pub const METADATA_USE_VERSION: &str = "metadata-use-version";
pub const SYNC_LOG_VERSION: &str = "sync-log-version";
pub const DEFAULT_TS: &str = "default-ts";

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
pub const METADATA_VERSION_V2: c_int = 3; // V2 only, V1 tables dropped

/// Refresh connection-local configuration after another connection commits.
///
/// Configuration is persisted in `crsql_master`, but the hot paths read the
/// cached fields in `crsql_ExtData`. Check PRAGMA data_version once per
/// transaction so an existing connection cannot continue using stale metadata
/// mode or merge settings. The current transaction's snapshot remains pinned;
/// the next transaction will perform the next check.
unsafe fn config_refresh_error(
    db: *mut sqlite_nostd::sqlite3,
    ext_data: *mut crsql_ExtData,
    operation: &str,
    key: Option<&str>,
) -> String {
    let sqlite_rc = db.errcode();
    let sqlite_error = db
        .errmsg()
        .map(|msg| msg.to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string());
    let master_exists = match db.prepare_v2(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'crsql_master'",
    ) {
        Ok(stmt) => match stmt.step() {
            Ok(ResultCode::ROW) => "yes",
            Ok(_) => "no",
            Err(_) => "error",
        },
        Err(_) => "error",
    };
    format!(
        "cr-sqlite config refresh failed: operation={}, key={}, rc={}, sqlite_error={}, autocommit={}, data_version={}, checked_this_tx={}, crsql_master={}, table_infos={:p}, write_version={}, use_version={}, sync_log_version={}, merge_equal_values={}",
        operation,
        key.unwrap_or("<none>"),
        sqlite_rc,
        sqlite_error,
        db.get_autocommit(),
        (*ext_data).pragmaDataVersion,
        (*ext_data).checkedConfigThisTx,
        master_exists,
        (*ext_data).tableInfos,
        (*ext_data).metadataWriteVersion,
        (*ext_data).metadataUseVersion,
        (*ext_data).syncLogVersion,
        (*ext_data).mergeEqualValues,
    )
}

pub unsafe fn ensure_config_current(
    db: *mut sqlite_nostd::sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<(), String> {
    // Read-only/autocommit statements do not invoke the commit hook, so a
    // transaction flag would otherwise remain set forever after the first
    // SELECT. In autocommit mode each operation is its own transaction and
    // must check data_version independently.
    let autocommit = db.get_autocommit();
    if (*ext_data).checkedConfigThisTx != 0 && !autocommit {
        return Ok(());
    }

    let data_version_changed = crate::c::crsql_fetchPragmaDataVersion(db, ext_data);
    if data_version_changed < 0 {
        return Err(config_refresh_error(db, ext_data, "PRAGMA data_version", None));
    }

    if data_version_changed != 0 {
        let config_values = [
            (MERGE_EQUAL_VALUES, &mut (*ext_data).mergeEqualValues),
            (METADATA_WRITE_VERSION, &mut (*ext_data).metadataWriteVersion),
            (METADATA_USE_VERSION, &mut (*ext_data).metadataUseVersion),
            (SYNC_LOG_VERSION, &mut (*ext_data).syncLogVersion),
        ];
        for (name, target) in config_values {
            let key = format!("config.{name}");
            match crate::util::get_master_value_cached(ext_data, &key) {
                Ok(Some(value)) => *target = value as c_int,
                Ok(None) => {}
                Err(_) => {
                    return Err(config_refresh_error(
                        db,
                        ext_data,
                        "read config (cached step)",
                        Some(name),
                    ));
                }
            }
        }
    }

    (*ext_data).checkedConfigThisTx = if autocommit { 0 } else { 1 };
    Ok(())
}

pub extern "C" fn crsql_config_set(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    let name = args[0].text();
    let ext_data = ctx.user_data() as *mut crsql_ExtData;

    // DEFAULT_TS is per-connection only — not persisted to crsql_master,
    // so no savepoint or persistence is needed.
    if name == DEFAULT_TS {
        let v = args[1].int64();
        if v < 0 {
            ctx.result_error("default-ts must be >= 0 (0 disables, >0 used when crsql_set_ts was not called)");
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
        unsafe { (*ext_data).defaultTimestamp = v as u64 };
        ctx.result_int64(v);
        return;
    }

    let db = ctx.db_handle();

    if let Err(message) = unsafe { ensure_config_current(db, ext_data) } {
        ctx.result_error(&message);
        ctx.result_error_code(ResultCode::ERROR);
        return;
    }

    // Wrap the entire transition in a savepoint so that schema changes
    // (V2 table creation, migration task queueing, cleanup task queueing)
    // are atomic with config persistence. If insert_config_setting fails,
    // the savepoint is rolled back, undoing the schema changes.
    // ext_data mutations are deferred until after persistence succeeds
    // to prevent in-memory state from diverging from persisted state.
    if db.exec_safe("SAVEPOINT config_set").is_err() {
        ctx.result_error("Failed to start savepoint for config_set");
        ctx.result_error_code(ResultCode::ERROR);
        return;
    }

    // Collect config changes as (config_name, int_value) pairs.
    // The first entry is always the explicitly-requested key; any subsequent
    // entries are cascaded side-effects. This single list drives BOTH
    // crsql_master persistence and ext_data updates, so the two can never
    // diverge — a cascaded in-memory change is impossible to add without also
    // persisting it, because both come from the same list.
    let mut config_changes: Vec<(&'static str, c_int)> = Vec::new();

    let value_result = (|| -> Result<*mut sqlite::value, ()> {
        let value = match name {
            MERGE_EQUAL_VALUES => {
                let value = args[1];
                let v = value.int();
                config_changes.push((MERGE_EQUAL_VALUES, v));
                value
            }
            METADATA_WRITE_VERSION => {
                let new_val = args[1].int();
                let old_val = unsafe { (*ext_data).metadataWriteVersion };
                if !validate_write_version_transition(old_val, new_val) {
                    ctx.result_error("Invalid metadata-write-version transition");
                    ctx.result_error_code(ResultCode::ERROR);
                    return Err(());
                }
                // Direct 1->3 transition: skip migration/cleanup, just verify no CRR tables exist
                if old_val == METADATA_VERSION_V1 && new_val == METADATA_VERSION_V2 {
                    match has_no_crr_tables(db) {
                        Ok(true) => {
                            // No CRR tables — safe to go directly to V2-only
                        },
                        Ok(false) => {
                            ctx.result_error("Cannot set metadata-write-version to v2 directly: existing CRR tables found. Migrate via v2&v1 first.");
                            ctx.result_error_code(ResultCode::ERROR);
                            return Err(());
                        },
                        Err(rc) => {
                            ctx.result_error("Failed to check for existing CRR tables");
                            ctx.result_error_code(rc);
                            return Err(());
                        }
                    }
                } else {
                    // Any other transition requires prior cleanup tasks to be done
                    match is_cleanup_complete(db) {
                        Ok(true) => {},
                        Ok(false) => {
                            ctx.result_error("Cannot transition metadata-write-version: cleanup tasks still pending");
                            ctx.result_error_code(ResultCode::ERROR);
                            return Err(());
                        },
                        Err(rc) => {
                            ctx.result_error("Failed to check cleanup status");
                            ctx.result_error_code(rc);
                            return Err(());
                        }
                    }
                    // Setting to v2&v1 queues migration tasks for all V1 CRR tables
                    if new_val == METADATA_VERSION_V2_AND_V1 && old_val == METADATA_VERSION_V1 {
                        // Create V2 tables for all existing CRR tables so dual-write
                        // triggers have somewhere to write immediately.
                        if let Err(rc) = create_v2_tables_for_existing_crrs(db, ext_data) {
                            ctx.result_error("Failed to create V2 tables during transition");
                            ctx.result_error_code(rc);
                            return Err(());
                        }
                        if let Err(rc) = queue_migration_tasks(db) {
                            ctx.result_error("Failed to queue migration tasks");
                            ctx.result_error_code(rc);
                            return Err(());
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
                                    return Err(());
                                }
                            },
                            Ok(false) => {
                                ctx.result_error("Cannot set metadata-write-version to v2: migration not complete for all tables");
                                ctx.result_error_code(ResultCode::ERROR);
                                return Err(());
                            },
                            Err(rc) => {
                                ctx.result_error("Failed to check migration status");
                                ctx.result_error_code(rc);
                                return Err(());
                            }
                        }
                    }
                    // Rolling back to v1 queues V2 table cleanup tasks and aborts migration
                    if new_val == METADATA_VERSION_V1 && old_val == METADATA_VERSION_V2_AND_V1 {
                        // Clear any pending migration markers since we're aborting migration
                        if let Err(rc) = clear_migration_markers(db) {
                            ctx.result_error("Failed to clear migration markers");
                            ctx.result_error_code(rc);
                            return Err(());
                        }
                        if let Err(rc) = queue_v2_cleanup_tasks(db) {
                            ctx.result_error("Failed to queue V2 cleanup tasks");
                            ctx.result_error_code(rc);
                            return Err(());
                        }
                    }
                }
                // Auto-cascade dependent config values to prevent invalid states.
                // Each cascaded value is appended to config_changes so it is BOTH
                // persisted to crsql_master AND applied to ext_data. Without
                // persisting cascades, new connections load stale values from
                // crsql_master.
                let new_use_version = if new_val == METADATA_VERSION_V1 {
                    Some(1)
                } else if new_val == METADATA_VERSION_V2 {
                    Some(2)
                } else {
                    None
                };
                let new_sync_log = if new_val == METADATA_VERSION_V1 { Some(1) } else { None };
                config_changes.push((METADATA_WRITE_VERSION, new_val));
                if let Some(uv) = new_use_version {
                    config_changes.push((METADATA_USE_VERSION, uv));
                }
                if let Some(sl) = new_sync_log {
                    config_changes.push((SYNC_LOG_VERSION, sl));
                }
                args[1]
            }
            METADATA_USE_VERSION => {
                let new_val = args[1].int();
                let old_val = unsafe { (*ext_data).metadataUseVersion };
                let write_version = unsafe { (*ext_data).metadataWriteVersion };
                if !validate_use_version_transition(old_val, new_val, write_version) {
                    let msg = if old_val == 1 && new_val == 2 {
                        "Cannot set metadata-use-version to v2: requires metadata-write-version to be v2&v1 or v2 first, and all V1→V2 migrations must be complete. Run crsql_incremental_maintenance() until it returns 0."
                    } else if old_val == 2 && new_val == 1 {
                        "Cannot set metadata-use-version to v1: requires metadata-write-version to be v1 or v2&v1 first."
                    } else {
                        "Invalid metadata-use-version transition"
                    };
                    ctx.result_error(msg);
                    ctx.result_error_code(ResultCode::ERROR);
                    return Err(());
                }
                // Setting to v2 requires all migrations to be complete
                if new_val == 2 {
                    if check_migration_complete_or_error(ctx, db, "metadata-use-version").is_err() {
                        return Err(());
                    }
                }
                config_changes.push((METADATA_USE_VERSION, new_val));
                args[1]
            }
            SYNC_LOG_VERSION => {
                let new_val = args[1].int();
                let old_val = unsafe { (*ext_data).syncLogVersion };
                let use_version = unsafe { (*ext_data).metadataUseVersion };
                let write_version = unsafe { (*ext_data).metadataWriteVersion };
                if !validate_sync_log_transition(old_val, new_val, use_version, write_version) {
                    let msg = if old_val == 1 && new_val == 2 {
                        if use_version != 2 {
                            "Cannot set sync-log-version to v2: requires metadata-use-version to be v2 first. Run crsql_incremental_maintenance() to complete V1→V2 migration, then set metadata-use-version to 2."
                        } else {
                            "Cannot set sync-log-version to v2: requires metadata-write-version to be v2 or v2&v1."
                        }
                    } else if old_val == 2 && new_val == 1 {
                        "Cannot set sync-log-version to v1: requires metadata-use-version to be v1 first."
                    } else {
                        "Invalid sync-log-version transition"
                    };
                    ctx.result_error(msg);
                    ctx.result_error_code(ResultCode::ERROR);
                    return Err(());
                }
                // Setting to v2 requires all migrations to be complete
                if new_val == 2 {
                    if check_migration_complete_or_error(ctx, db, "sync-log-version").is_err() {
                        return Err(());
                    }
                }
                config_changes.push((SYNC_LOG_VERSION, new_val));
                args[1]
            }
            _ => {
                ctx.result_error(&format!("Unknown setting name: {name}"));
                ctx.result_error_code(ResultCode::ERROR);
                return Err(());
            }
        };
        Ok(value)
    })();

    match value_result {
        Ok(value) => {
            // Persist the explicitly-requested key first. insert_config_setting
            // returns the stored value via RETURNING, but we use the input
            // value (args[1]) for the function result so the statement can be
            // dropped immediately — freeing the connection to persist cascaded
            // values without SQLite BUSY errors (a statement sitting on a ROW
            // blocks other writes on the same connection).
            match insert_config_setting(db, name, value) {
                Ok((stmt, _ret_value)) => {
                    drop(stmt);
                    // Persist any cascaded config changes (entries after the
                    // primary key). These are side-effects of the requested set
                    // (e.g. setting metadata-write-version cascades to
                    // metadata-use-version and sync-log-version). They MUST be
                    // persisted or new connections load stale values from
                    // crsql_master.
                    let mut cascade_err: Option<ResultCode> = None;
                    for (cname, cvalue) in config_changes.iter().skip(1) {
                        if let Err(rc) = persist_config_int(db, cname, *cvalue) {
                            cascade_err = Some(rc);
                            break;
                        }
                    }
                    if let Some(rc) = cascade_err {
                        rollback_config_set(db, ext_data);
                        ctx.result_error("Could not persist cascaded config in database");
                        ctx.result_error_code(rc);
                        return;
                    }
                    // All persistence succeeded — set the result, apply the
                    // ext_data updates (driven by the same config_changes list),
                    // and release the savepoint.
                    ctx.result_value(value);
                    apply_config_changes_to_ext_data(ext_data, config_changes);
                    let _ = db.exec_safe("RELEASE config_set");
                }
                Err(rc) => {
                    // Persistence failed — rollback schema changes, don't apply ext_data mutations.
                    rollback_config_set(db, ext_data);
                    ctx.result_error("Could not persist config in database");
                    ctx.result_error_code(rc);
                }
            }
        }
        Err(()) => {
            // Error already reported to ctx via result_error.
            // Rollback any schema changes made before the error.
            rollback_config_set(db, ext_data);
        }
    }
}

/// Rollback the config_set savepoint and invalidate any in-memory table info
/// cache that may have been populated by schema changes (e.g. V2 table
/// creation during the 1->2 transition) that are now being rolled back.
/// Without resetting these flags, the stale cache would persist for the
/// remainder of the transaction — `crsql_ensure_table_infos_are_up_to_date`
/// would see `updatedTableInfosThisTx == 1` and `schema_changed == 0` (the
/// DDL was rolled back so PRAGMA schema_version reverted) and return early
/// without re-pulling, leaving V2 TableInfo entries for tables that no
/// longer exist.
fn rollback_config_set(db: *mut sqlite_nostd::sqlite3, ext_data: *mut crsql_ExtData) {
    let _ = db.exec_safe("ROLLBACK TO config_set");
    let _ = db.exec_safe("RELEASE config_set");
    unsafe {
        (*ext_data).updatedTableInfosThisTx = 0;
        (*ext_data).pragmaSchemaVersionForTableInfos = -1;
    }
}

/// Persist an integer config setting to crsql_master as `config.{name}`.
/// Must be called within a transaction/savepoint so a failure can roll back
/// any preceding writes (including the primary config key and schema changes).
fn persist_config_int(
    db: *mut sqlite_nostd::sqlite3,
    name: &str,
    value: c_int,
) -> Result<(), ResultCode> {
    unsafe { crate::util::set_master_value(db, &format!("config.{name}"), value as i64) }
}

/// Apply a list of (config_name, value) changes to the in-memory ext_data
/// struct. This is the ext_data counterpart to persisting the same list to
/// crsql_master. Driving both persistence and ext_data updates from the same
/// list prevents the two from diverging — the bug where a cascaded in-memory
/// change was not persisted and thus lost on reconnect.
fn apply_config_changes_to_ext_data(
    ext_data: *mut crsql_ExtData,
    changes: Vec<(&'static str, c_int)>,
) {
    for (name, value) in changes {
        match name {
            MERGE_EQUAL_VALUES => unsafe { (*ext_data).mergeEqualValues = value; }
            METADATA_WRITE_VERSION => unsafe { (*ext_data).metadataWriteVersion = value; }
            METADATA_USE_VERSION => unsafe { (*ext_data).metadataUseVersion = value; }
            SYNC_LOG_VERSION => unsafe { (*ext_data).syncLogVersion = value; }
            _ => {}
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
    let ext_data = ctx.user_data() as *mut crsql_ExtData;

    if name != DEFAULT_TS {
        if let Err(message) = unsafe { ensure_config_current(ctx.db_handle(), ext_data) } {
            ctx.result_error(&message);
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    }

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
        DEFAULT_TS => {
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            ctx.result_int64(unsafe { (*ext_data).defaultTimestamp as i64 });
        }
        _ => {
            ctx.result_error(&format!("Unknown setting name: {name}"));
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    }
}

/// If the transaction timestamp is unset (0) and `default-ts` is configured
/// (>0), copy that default into the transaction timestamp.
/// Returns the timestamp to use, or `Err(())` if still unset.
pub unsafe fn ensure_timestamp(ext_data: *mut crsql_ExtData) -> Result<u64, ()> {
    if (*ext_data).timestamp != 0 {
        return Ok((*ext_data).timestamp);
    }
    if (*ext_data).defaultTimestamp != 0 {
        (*ext_data).timestamp = (*ext_data).defaultTimestamp;
        return Ok((*ext_data).timestamp);
    }
    Err(())
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
fn create_v2_tables_for_existing_crrs(
    db: *mut sqlite_nostd::sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<(), ResultCode> {
    let table_names = find_tables_with_suffix(db, "__crsql_clock")?;

    for tbl_name in &table_names {
        // Check if V2 tables already exist (e.g., table was created in dual-write mode)
        if crate::bootstrap_v2::has_v2_tables(db, tbl_name)? {
            continue; // V2 tables already exist
        }

        // Pull table info and create V2 tables
        let mut err: *mut core::ffi::c_char = core::ptr::null_mut();
        let tbl_info = crate::tableinfo::pull_table_info(db, tbl_name, &mut err)?;
        crate::bootstrap_v2::create_v2_tables(db, &tbl_info)?;
    }

    // Creating V2 tables bumps PRAGMA schema_version. Clear the per-tx flag so
    // ensure actually re-pulls instead of returning the pre-transition (V1) cache.
    unsafe {
        (*ext_data).updatedTableInfosThisTx = 0;
    }
    let mut err: *mut core::ffi::c_char = core::ptr::null_mut();
    let rc = crate::tableinfo::crsql_ensure_table_infos_are_up_to_date(
        db,
        ext_data,
        &mut err as *mut _,
    );
    if rc != ResultCode::OK as c_int {
        if let Some(code) = ResultCode::from_i32(rc) {
            return Err(code);
        }
        return Err(ResultCode::ERROR);
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
/// Clears both the task-existence markers and the cached remaining-count keys.
/// Without clearing `remaining_*`, a subsequent 1->2 re-transition would read
/// the stale cached count via get_or_count instead of recounting.
fn clear_migration_markers(db: *mut sqlite_nostd::sqlite3) -> Result<(), ResultCode> {
    let sql = "DELETE FROM crsql_master WHERE key LIKE 'migration_v1_to_v2_migration_%' OR key LIKE 'migration_v1_to_v2_remaining_%'";
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
    // Check for all three CRR trigger types. A table is a CRR if any of the
    // insert, update, or delete triggers exist — partial trigger state should
    // not be treated as "no CRR tables."
    let trigger_suffixes = ["__crsql_itrig", "__crsql_utrig", "__crsql_dtrig"];
    for suffix in &trigger_suffixes {
        let sql = "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name LIKE ?\0";
        let stmt = db.prepare_v2(sql)?;
        stmt.bind_text(
            1,
            &format!("%{}", suffix),
            sqlite::Destructor::TRANSIENT,
        )?;
        stmt.step()?;
        let count = stmt.column_int64(0);
        if count > 0 {
            return Ok(false);
        }
    }
    Ok(true)
}
