use alloc::string::String;
use crate::c::crsql_ExtData;
use crate::tableinfo::TableInfo;
use sqlite_nostd as sqlite;

/// Check if force update mode is enabled
pub fn is_force_update_mode_enabled(ext_data: *mut crsql_ExtData) -> bool {
    unsafe { (*ext_data).forceUpdateMode != 0 }
}

/// Get the forced CL for a row in force update mode.
/// This returns the current CL + 2 to ensure the forced update wins.
/// If current CL is odd (alive), we return CL + 2 (next alive state).
/// If current CL is even (deleted), we return CL + 1 (resurrect).
/// If no CL exists (0), we return 1 (initial insert).
pub fn get_forced_cl(current_cl: i64) -> i64 {
    if current_cl == 0 {
        // No existing CL, start at 1
        1
    } else if current_cl % 2 == 0 {
        // Current CL is even (deleted), resurrect with odd CL
        current_cl + 1
    } else {
        // Current CL is odd (alive), force to next alive state (skip delete)
        current_cl + 2
    }
}

/// For force update mode, we need to ensure all operations result in a higher CL.
/// This function calculates what the "delete" CL should be for a force delete operation.
pub fn get_forced_delete_cl(current_cl: i64) -> i64 {
    if current_cl == 0 {
        // No existing CL, delete at CL 2
        2
    } else if current_cl % 2 == 0 {
        // Already deleted, bump to next delete state
        current_cl + 2
    } else {
        // Currently alive, delete at next even CL
        current_cl + 1
    }
}

/// Get the current CL for a key from the table info cache or database
pub fn get_current_cl_for_key(
    db: *mut sqlite::sqlite3,
    tbl_info: &TableInfo,
    key: sqlite::int64,
) -> Result<i64, String> {
    // First check the cache
    if let Some(&cl) = tbl_info.get_cl(key) {
        return Ok(cl);
    }

    // If not in cache, query from database
    let local_cl_stmt_ref = tbl_info
        .get_local_cl_stmt(db)
        .map_err(|_| "failed to get local_cl_stmt")?;
    let local_cl_stmt = local_cl_stmt_ref
        .as_ref()
        .ok_or("Failed to deref local_cl_stmt")?;

    local_cl_stmt
        .bind_int64(1, key)
        .and_then(|_| local_cl_stmt.bind_int64(2, key))
        .map_err(|_| "failed to bind to local_cl_stmt")?;

    let cl = if local_cl_stmt.step().map_err(|_| "failed to step local_cl_stmt")? == sqlite::ResultCode::ROW {
        local_cl_stmt.column_int64(0)
    } else {
        0
    };

    local_cl_stmt
        .reset()
        .map_err(|_| "failed to reset local_cl_stmt")?;

    Ok(cl)
}
