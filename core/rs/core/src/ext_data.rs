extern crate alloc;
use crate::alloc::string::ToString;
use alloc::vec;
use core::ffi::c_int;
use core::ptr::null_mut;

use alloc::string::String;
use sqlite::{sqlite3, Connection, ResultCode, Stmt};
use sqlite_nostd as sqlite;

use crate::{
    c::{crsql_ExtData, crsql_fetchPragmaDataVersion},
    util::get_db_version_union_query,
};

/**
 * Refresh connection-local state cached in `crsql_ExtData` after another
 * connection committed changes to the database.
 *
 * `PRAGMA data_version` only changes for commits made by other connections
 * and is pinned while this connection holds a transaction open, so this is a
 * cheap no-op in the common case. When the data version did change, every
 * cached value is reloaded through its own reload_* function so independent
 * caches cannot observe the same change inconsistently.
 */
pub fn refresh_connection_state_if_needed(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<(), String> {
    let (previous_data_version, previous_db_version) = unsafe {
        ((*ext_data).pragmaDataVersion, (*ext_data).dbVersion)
    };
    let rc = unsafe { crsql_fetchPragmaDataVersion(db, ext_data) };
    if rc == -1 {
        unsafe {
            (*ext_data).pragmaDataVersion = previous_data_version;
            (*ext_data).dbVersion = previous_db_version;
        }
        return Err("failed to fetch PRAGMA data_version".to_string());
    }
    // `data_version` cannot distinguish "unchanged" from "never loaded":
    // dbVersion is the one lazily-initialized cache, so its -1 sentinel
    // doubles as the first-load check for all of them.
    if rc == 0 && unsafe { (*ext_data).dbVersion } != -1 {
        return Ok(());
    }
    let result = crate::db_version::reload_db_version(db, ext_data)
        .map(|_| ())
        .and_then(|_| crate::config::reload_connection_config(db, ext_data));
    if result.is_err() {
        unsafe {
            (*ext_data).pragmaDataVersion = previous_data_version;
            (*ext_data).dbVersion = previous_db_version;
        }
    }
    result
}

#[no_mangle]
pub extern "C" fn crsql_recreate_db_version_stmt(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> c_int {
    match recreate_db_version_stmt(db, ext_data) {
        Ok(ResultCode::DONE) => -1, // negative 1 means no clock tables exist and there is nothing to fetch
        Ok(rc) | Err(rc) => rc as c_int,
    }
}

pub fn recreate_db_version_stmt(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<ResultCode, ResultCode> {
    let clock_tables_stmt = unsafe { (*ext_data).pSelectClockTablesStmt };
    let db_version_stmt = unsafe { (*ext_data).pDbVersionStmt };

    db_version_stmt.finalize()?;
    unsafe {
        (*ext_data).pDbVersionStmt = null_mut();
    }

    let mut clock_tbl_names = vec![];
    loop {
        match clock_tables_stmt.step() {
            Ok(ResultCode::DONE) => {
                clock_tables_stmt.reset()?;
                if clock_tbl_names.len() == 0 {
                    return Ok(ResultCode::DONE);
                }
                break;
            }
            Ok(ResultCode::ROW) => {
                clock_tbl_names.push(clock_tables_stmt.column_text(0).to_string());
            }
            Ok(rc) | Err(rc) => {
                clock_tables_stmt.reset()?;
                return Err(rc);
            }
        }
    }

    let union = get_db_version_union_query(&clock_tbl_names);

    let db_version_stmt = db.prepare_v3(&union, sqlite::PREPARE_PERSISTENT)?;
    unsafe {
        (*ext_data).pDbVersionStmt = db_version_stmt.into_raw();
    }

    Ok(ResultCode::OK)
}
