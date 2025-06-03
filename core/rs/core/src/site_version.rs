use alloc::collections::BTreeMap;
use core::ffi::c_void;
use core::{mem, ptr};

use crate::alloc::{boxed::Box, string::ToString, vec::Vec};
use crate::consts;
use crate::consts::MIN_POSSIBLE_SITE_VERSION;
use crate::stmt_cache::reset_cached_stmt;
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
// use crate::ext_data::recreate_site_version_stmt;

// #[no_mangle]
// pub extern "C" fn crsql_fill_site_version_if_needed(
//     db: *mut sqlite3,
//     ext_data: *mut crsql_ExtData,
//     errmsg: *mut *mut c_char,
// ) -> c_int {
//     match fill_site_version_if_needed(db, ext_data) {
//         Ok(rc) => rc as c_int,
//         Err(msg) => {
//             errmsg.set(&msg);
//             ResultCode::ERROR as c_int
//         }
//     }
// }

// #[no_mangle]
// pub extern "C" fn crsql_next_site_version(
//     db: *mut sqlite3,
//     ext_data: *mut crsql_ExtData,
//     errmsg: *mut *mut c_char,
// ) -> sqlite::int64 {
//     match next_site_version(db, ext_data) {
//         Ok(version) => version,
//         Err(msg) => {
//             errmsg.set(&msg);
//             -1
//         }
//     }
// }

/**
 * Given this needs to do a pragma check, invoke it as little as possible.
 * TODO: We could optimize to only do a pragma check once per transaction.
 * Need to save some bit that states we checked the pragma already and reset on tx commit or rollback.
 */
// pub fn next_site_version(db: *mut sqlite3, ext_data: *mut crsql_ExtData) -> Result<i64, String> {
//     fill_site_version_if_needed(db, ext_data)?;

//     // libc_print::libc_println!("got site_version = {}", unsafe { (*ext_data).siteVersion });

//     let ret = unsafe { (*ext_data).siteVersion + 1 };
//     // libc_print::libc_println!("next site_version = {}", ret);
//     unsafe {
//         (*ext_data).pendingSiteVersion = ret;

//         if (*ext_data).nextSiteVersionSet == 1 {
//             // libc_print::libc_println!("already inserted in DB! returning.");
//             return Ok(ret);
//         }

//         let site_id_slice =
//             core::slice::from_raw_parts((*ext_data).siteId, consts::SITE_ID_LEN as usize);
// libc_print::libc_println!(
//     "next_site_version: setting into DB! {:?} => {}",
//     site_id_slice,
//     ret
// );
// next site id is not set in the DB yet, do this now.
//         let bind_result = (*ext_data)
//             .pSetSiteVersionStmt
//             .bind_blob(1, site_id_slice, sqlite_nostd::Destructor::STATIC)
//             .and_then(|_| (*ext_data).pSetSiteVersionStmt.bind_int64(2, ret));

//         if bind_result.is_err() {
//             return Err("failed binding to set_site_version_stmt".into());
//         }

//         if (*ext_data).pSetSiteVersionStmt.step().is_err() {
//             reset_cached_stmt((*ext_data).pSetSiteVersionStmt)
//                 .map_err(|_| "failed to reset cached set_site_version_stmt")?;
//             return Err("failed to insert site_version for current site ID".into());
//         }

//         reset_cached_stmt((*ext_data).pSetSiteVersionStmt)
//             .map_err(|_| "failed to reset cached set_site_version_stmt")?;

//         (*ext_data).nextSiteVersionSet = 1;
//     }

//     Ok(ret)
// }

pub fn insert_site_version(
    ext_data: *mut crsql_ExtData,
    insert_site_id: &[u8],
    insert_site_vrsn: i64,
) -> Result<(), ResultCode> {
    unsafe {
        let mut last_site_versions: mem::ManuallyDrop<Box<BTreeMap<Vec<u8>, i64>>> =
            mem::ManuallyDrop::new(Box::from_raw(
                (*ext_data).lastDbVersions as *mut BTreeMap<Vec<u8>, i64>,
            ));

        if let Some(site_v) = last_site_versions.get(insert_site_id) {
            if *site_v >= insert_site_vrsn {
                // already inserted this site version!
                return Ok(());
            }
        }

        let bind_result = (*ext_data)
            .pSetDbVersionStmt
            .bind_blob(1, insert_site_id, sqlite::Destructor::STATIC)
            .and_then(|_| {
                (*ext_data)
                    .pSetDbVersionStmt
                    .bind_int64(2, insert_site_vrsn)
            });

        if let Err(rc) = bind_result {
            reset_cached_stmt((*ext_data).pSetDbVersionStmt)?;
            return Err(rc);
        }
        match (*ext_data).pSetDbVersionStmt.step() {
            Ok(ResultCode::ROW) => {
                last_site_versions.insert(
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

// pub fn fill_site_version_if_needed(
//     db: *mut sqlite3,
//     ext_data: *mut crsql_ExtData,
// ) -> Result<ResultCode, String> {
//     unsafe {
//         // maybe not needed
//         let rc = crsql_fetchPragmaDataVersion(db, ext_data);
//         if rc == -1 {
//             return Err("failed to fetch PRAGMA data_version".to_string());
//         }
//         // libc_print::libc_println!(
//         //     "fill_site_version_if_needed, currernt site version = {}, rc = {}",
//         //     (*ext_data).siteVersion,
//         //     rc
//         // );
//         if (*ext_data).siteVersion != -1 && rc == 0 {
//             // libc_print::libc_println!("fill_site_version_if_needed: no need to fetch from storage");
//             return Ok(ResultCode::OK);
//         }
//         fetch_site_version_from_storage(db, ext_data)
//     }
// }

// pub fn fetch_site_version_from_storage(
//     db: *mut sqlite3,
//     ext_data: *mut crsql_ExtData,
// ) -> Result<ResultCode, String> {
//     // libc_print::libc_println!("fetch_site_version_from_storage");
//     unsafe {
//         if (*ext_data).pSiteVersionStmt.is_null() {
//             // libc_print::libc_println!("null site version stmt");
//             match recreate_site_version_stmt(db, ext_data) {
//                 Ok(ResultCode::DONE) => {
//                     // libc_print::libc_println!("no clock tables means no site version!");
//                     // this means there are no clock tables / this is a clean db
//                     (*ext_data).siteVersion = 0;
//                     return Ok(ResultCode::OK);
//                 }
//                 Ok(_) => {}
//                 Err(rc) => return Err(format!("failed to recreate db version stmt: {}", rc)),
//             }
//         }

//         let site_version_stmt = (*ext_data).pSiteVersionStmt;
//         let rc = site_version_stmt.step();
//         match rc {
//             // no rows? We're a fresh db with the min starting version
//             Ok(ResultCode::DONE) => {
//                 // libc_print::libc_println!("fetch_site_version_from_storage: no rows...");
//                 site_version_stmt
//                     .reset()
//                     .map_err(|rc| format!("failed to reset db version stmt after DONE: {}", rc))?;
//                 (*ext_data).siteVersion = MIN_POSSIBLE_SITE_VERSION;
//                 Ok(ResultCode::OK)
//             }
//             // got a row? It is our db version.
//             Ok(ResultCode::ROW) => {
//                 // libc_print::libc_println!("fetch_site_version_from_storage: got a row!");
//                 (*ext_data).siteVersion = site_version_stmt.column_int64(0);
//                 site_version_stmt
//                     .reset()
//                     .map_err(|rc| format!("failed to reset db version stmt after ROW: {}", rc))
//             }
//             // Not row or done? Something went wrong.
//             Ok(rc) | Err(rc) => {
//                 site_version_stmt.reset().map_err(|rc| {
//                     format!("failed to reset db version stmt after UNKNOWN: {}", rc)
//                 })?;
//                 Err(format!("failed to step db version stmt: {}", rc))
//             }
//         }
//     }
// }

#[no_mangle]
pub extern "C" fn crsql_init_last_site_versions_map(ext_data: *mut crsql_ExtData) {
    let map: BTreeMap<Vec<u8>, i64> = BTreeMap::new();
    unsafe { (*ext_data).lastDbVersions = Box::into_raw(Box::new(map)) as *mut c_void }
}

#[no_mangle]
pub extern "C" fn crsql_drop_last_site_versions_map(ext_data: *mut crsql_ExtData) {
    unsafe {
        drop(Box::from_raw(
            (*ext_data).lastDbVersions as *mut BTreeMap<Vec<u8>, i64>,
        ));
    }
}
