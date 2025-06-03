use core::ptr;

use crate::alloc::string::ToString;
use alloc::format;
use alloc::string::String;
use core::ffi::{c_char, c_int};
use sqlite::ResultCode;
use sqlite::StrRef;
use sqlite::{sqlite3, Stmt};
use sqlite_nostd as sqlite;

use crate::c::crsql_ExtData;
use crate::c::crsql_fetchPragmaDataVersion;
use crate::c::crsql_fetchPragmaSchemaVersion;
use crate::c::DB_VERSION_SCHEMA_VERSION;
use crate::consts::MIN_POSSIBLE_DB_VERSION;
use crate::consts::SITE_ID_LEN;
use crate::ext_data::recreate_db_version_stmt;
use crate::stmt_cache::reset_cached_stmt;
#[no_mangle]
pub extern "C" fn crsql_fill_db_version_if_needed(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    errmsg: *mut *mut c_char,
) -> c_int {
    match fill_db_version_if_needed(db, ext_data) {
        Ok(rc) => rc as c_int,
        Err(msg) => {
            errmsg.set(&msg);
            ResultCode::ERROR as c_int
        }
    }
}

#[no_mangle]
pub extern "C" fn crsql_next_db_version(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    errmsg: *mut *mut c_char,
) -> sqlite::int64 {
    match next_db_version(db, ext_data) {
        Ok(version) => version,
        Err(msg) => {
            errmsg.set(&msg);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn crsql_peek_next_db_version(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
    errmsg: *mut *mut c_char,
) -> sqlite::int64 {
    match peek_next_db_version(db, ext_data) {
        Ok(version) => version,
        Err(msg) => {
            errmsg.set(&msg);
            -1
        }
    }
}

pub fn peek_next_db_version(db: *mut sqlite3, ext_data: *mut crsql_ExtData) -> Result<i64, String> {
    fill_db_version_if_needed(db, ext_data)?;

    let mut ret = unsafe { (*ext_data).dbVersion + 1 };
    if ret < unsafe { (*ext_data).pendingDbVersion } {
        ret = unsafe { (*ext_data).pendingDbVersion };
    }
    Ok(ret)
}

/**
 * Given this needs to do a pragma check, invoke it as little as possible.
 * TODO: We could optimize to only do a pragma check once per transaction.
 * Need to save some bit that states we checked the pragma already and reset on tx commit or rollback.
 */
pub fn next_db_version(db: *mut sqlite3, ext_data: *mut crsql_ExtData) -> Result<i64, String> {
    fill_db_version_if_needed(db, ext_data)?;

    let mut ret = unsafe { (*ext_data).dbVersion + 1 };
    // libc_print::libc_println!(
    //     // "incrementing db_version: {} => {}, ret: {}",
    //     unsafe { (*ext_data).dbVersion },
    //     unsafe { (*ext_data).pendingDbVersion },
    //     ret
    // );
    if ret < unsafe { (*ext_data).pendingDbVersion } {
        ret = unsafe { (*ext_data).pendingDbVersion };
    }

    // update db_version in db if it changed
    if ret != unsafe { (*ext_data).pendingDbVersion } {
        // update site_version in db
        // libc_print::libc_println!(
        //     "next_site_version: setting into DB! => {}",
        //     ret
        // );
        // next site id is not set in the DB yet, do this now.
        unsafe {
            let site_id_slice =
                core::slice::from_raw_parts((*ext_data).siteId, SITE_ID_LEN as usize);

            let bind_result = (*ext_data)
                .pSetDbVersionStmt
                .bind_blob(1, site_id_slice, sqlite_nostd::Destructor::STATIC)
                .and_then(|_| (*ext_data).pSetDbVersionStmt.bind_int64(2, ret));

            if bind_result.is_err() {
                return Err("failed binding to set_site_version_stmt".into());
            }

            if (*ext_data).pSetDbVersionStmt.step().is_err() {
                reset_cached_stmt((*ext_data).pSetDbVersionStmt)
                    .map_err(|_| "failed to reset cached set_site_version_stmt")?;
                return Err("failed to insert site_version for current site ID".into());
            }

            reset_cached_stmt((*ext_data).pSetDbVersionStmt)
                .map_err(|_| "failed to reset cached set_site_version_stmt")?;

            //         (*ext_data).nextSiteVersionSet = 1;
        }
    }

    unsafe {
        (*ext_data).pendingDbVersion = ret;
    }
    Ok(ret)
}

pub fn fill_db_version_if_needed(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<ResultCode, String> {
    unsafe {
        let rc = crsql_fetchPragmaDataVersion(db, ext_data);
        if rc == -1 {
            return Err("failed to fetch PRAGMA data_version".to_string());
        }
        if (*ext_data).dbVersion != -1 && rc == 0 {
            return Ok(ResultCode::OK);
        }
        fetch_db_version_from_storage(db, ext_data)
    }
}

pub fn fetch_db_version_from_storage(
    db: *mut sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<ResultCode, String> {
    unsafe {
        let schema_changed = if (*ext_data).pDbVersionStmt == ptr::null_mut() {
            1 as c_int
        } else {
            crsql_fetchPragmaSchemaVersion(db, ext_data, DB_VERSION_SCHEMA_VERSION)
        };

        if schema_changed < 0 {
            return Err("failed to fetch the pragma schema version".to_string());
        }

        if schema_changed > 0 {
            match recreate_db_version_stmt(db, ext_data) {
                Ok(ResultCode::DONE) => {
                    // this means there are no clock tables / this is a clean db
                    (*ext_data).dbVersion = 0;
                    return Ok(ResultCode::OK);
                }
                Ok(_) => {}
                Err(rc) => return Err(format!("failed to recreate db version stmt: {}", rc)),
            }
        }

        let db_version_stmt = (*ext_data).pDbVersionStmt;
        let rc = db_version_stmt.step();
        match rc {
            // no rows? We're a fresh db with the min starting version
            Ok(ResultCode::DONE) => {
                db_version_stmt.reset().or_else(|rc| {
                    Err(format!(
                        "failed to reset db version stmt after DONE: {}",
                        rc
                    ))
                })?;
                (*ext_data).dbVersion = MIN_POSSIBLE_DB_VERSION;
                Ok(ResultCode::OK)
            }
            // got a row? It is our db version.
            Ok(ResultCode::ROW) => {
                (*ext_data).dbVersion = db_version_stmt.column_int64(0);
                db_version_stmt
                    .reset()
                    .or_else(|rc| Err(format!("failed to reset db version stmt after ROW: {}", rc)))
            }
            // Not row or done? Something went wrong.
            Ok(rc) | Err(rc) => {
                db_version_stmt.reset().or_else(|rc| {
                    Err(format!("failed to reset db version stmt after ROW: {}", rc))
                })?;
                Err(format!("failed to step db version stmt: {}", rc))
            }
        }
    }
}
