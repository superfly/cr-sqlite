extern crate alloc;
use crate::alloc::string::ToString;
use alloc::vec;
use core::ffi::c_int;
use core::ptr::null_mut;

use alloc::string::String;
use sqlite::{sqlite3, Connection, ResultCode, Stmt};
use sqlite_nostd as sqlite;

use crate::{
    c::{crsql_ExtData, crsql_readPragmaDataVersion},
    util::get_db_version_union_query,
};

/**
 * Refresh connection-local state cached in `crsql_ExtData` after another
 * connection committed changes to the database.
 *
 * Read the data version on every call rather than tracking a per-transaction
 * flag; stepping the cached PRAGMA statement is cheaper than accounting for
 * read-only commits and savepoint rollbacks. When the data version changes,
 * load each cache into temporary state and apply it only after every load
 * succeeds.
 */
pub fn refresh_connection_state_if_needed(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<(), String> {
    let mut data_version = 0;
    let rc = unsafe { crsql_readPragmaDataVersion(ext_data, &mut data_version) };
    if rc != ResultCode::OK as c_int {
        return Err("failed to fetch PRAGMA data_version".to_string());
    }
    if data_version == unsafe { (*ext_data).pragmaDataVersion }
        && unsafe { (*ext_data).dbVersion } != -1
    {
        return Ok(());
    }

    let config = crate::config::load_connection_config(db, ext_data)?;
    let db_version = crate::db_version::load_db_version(ext_data)?;
    unsafe {
        (*ext_data).mergeEqualValues = config.merge_equal_values;
        (*ext_data).metadataWriteVersion = config.metadata_write_version;
        (*ext_data).metadataUseVersion = config.metadata_use_version;
        (*ext_data).syncLogVersion = config.sync_log_version;
        (*ext_data).dbVersion = db_version;
        (*ext_data).pragmaDataVersion = data_version;
    }
    Ok(())
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
    if ext_data.is_null() {
        return Err(ResultCode::ERROR);
    }
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
