use core::ffi::c_void;
use core::mem;
use core::ptr;

use crate::alloc::string::ToString;
use crate::alloc::{boxed::Box, vec::Vec};
use alloc::collections::BTreeMap;
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
        let site_id_slice = core::slice::from_raw_parts((*ext_data).siteId, SITE_ID_LEN as usize);

        let db_version_stmt = (*ext_data).pDbVersionStmt;

        let bind_result = (*ext_data).pDbVersionStmt.bind_blob(
            1,
            site_id_slice,
            sqlite_nostd::Destructor::STATIC,
        );

        if bind_result.is_err() {
            return Err("failed binding to db_version_stmt".into());
        }
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

#[no_mangle]
pub extern "C" fn crsql_init_last_db_versions_map(ext_data: *mut crsql_ExtData) {
    let map: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    unsafe { (*ext_data).lastDbVersions = Box::into_raw(Box::new(map)) as *mut c_void }
}

#[no_mangle]
pub extern "C" fn crsql_drop_last_db_versions_map(ext_data: *mut crsql_ExtData) {
    unsafe {
        drop(Box::from_raw(
            (*ext_data).lastDbVersions as *mut BTreeMap<Vec<u8>, i64>,
        ));
    }
}

pub fn insert_db_version(
    ext_data: *mut crsql_ExtData,
    insert_site_id: &[u8],
    insert_db_vrsn: i64,
) -> Result<(), ResultCode> {
    unsafe {
        let mut last_db_versions: mem::ManuallyDrop<Box<BTreeMap<Vec<u8>, i64>>> =
            mem::ManuallyDrop::new(Box::from_raw(
                (*ext_data).lastDbVersions as *mut BTreeMap<Vec<u8>, i64>,
            ));

        if let Some(db_v) = last_db_versions.get(insert_site_id) {
            if *db_v >= insert_db_vrsn {
                // already inserted this site version!
                return Ok(());
            }
        }

        let bind_result = (*ext_data)
            .pSetDbVersionStmt
            .bind_blob(1, insert_site_id, sqlite::Destructor::STATIC)
            .and_then(|_| (*ext_data).pSetDbVersionStmt.bind_int64(2, insert_db_vrsn));

        if let Err(rc) = bind_result {
            reset_cached_stmt((*ext_data).pSetDbVersionStmt)?;
            return Err(rc);
        }
        match (*ext_data).pSetDbVersionStmt.step() {
            Ok(ResultCode::ROW) => {
                last_db_versions.insert(
                    insert_site_id.to_vec(),
                    (*ext_data).pSetDbVersionStmt.column_int64(0),
                );
            }
            Ok(_) => {}
            Err(rc) => {
                reset_cached_stmt((*ext_data).pSetDbVersionStmt)?;
                return Err(rc);
            }
        }
        reset_cached_stmt((*ext_data).pSetDbVersionStmt)?;
    }
    Ok(())
}
