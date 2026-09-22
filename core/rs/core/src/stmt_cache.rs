extern crate alloc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;

use alloc::boxed::Box;
use sqlite::Stmt;
use sqlite_nostd as sqlite;
use sqlite_nostd::ResultCode;

use crate::c::crsql_ExtData;
use crate::tableinfo::TableInfo;

// Finalize prepared statements attached to table infos.
// Do not drop the table infos.
// We do this explicitly since `drop` cannot return an error and we want to
// return the error / not panic.
#[no_mangle]
pub extern "C" fn crsql_clear_stmt_cache(ext_data: *mut crsql_ExtData) {
    if ext_data.is_null() {
        return;
    }
    unsafe {
        if (*ext_data).tableInfos.is_null() {
            return;
        }
        let tbl_infos =
            ManuallyDrop::new(Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>));
        for tbl_info in tbl_infos.iter() {
            // TODO: return an error.
            if let Err(_) = tbl_info.clear_stmts() {
                // TODO: log or propagate the error.
            }
        }
    }
}

pub fn reset_cached_stmt(stmt: *mut sqlite::stmt) -> Result<ResultCode, ResultCode> {
    if stmt.is_null() {
        return Ok(ResultCode::OK);
    }
    let _ = stmt.clear_bindings();
    stmt.reset()
}
