extern crate alloc;
use crate::alloc::collections::BTreeMap;
use crate::changes_vtab_write::crsql_merge_insert;
use crate::stmt_cache::reset_cached_stmt;
use crate::tableinfo::{crsql_ensure_table_infos_are_up_to_date, TableInfo};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int};
use core::mem::{self, forget};
use core::ptr::null_mut;

use alloc::ffi::CString;
#[cfg(not(feature = "std"))]
use num_traits::FromPrimitive;
use sqlite::{ColumnType, Connection, Context, Stmt, Value};
use sqlite_nostd as sqlite;
use sqlite_nostd::ResultCode;

/// Magic prefix for the binary idxStr format. Starts with a null byte so
/// any stray `printf("%s", idxStr)` prints "Rust magic" (the null terminates
/// it for C, but the full magic is visible in a debugger).
pub const IDX_MAGIC: [u8; 11] = *b"Rust magic\0";

/// A single WHERE constraint, packed into 3 bytes.
/// `col` is the constrained column (CrsqlChangesColumn, 1 byte via repr(u8)).
/// `op_id` is a `SQLITE_INDEX_CONSTRAINT_*` value (2-71).
/// `param_idx` is the 1-based argv index (0 for IS NULL / IS NOT NULL).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PlanConstraint {
    pub col: crate::c::CrsqlChangesColumn,
    pub op_id: u8,
    pub param_idx: u8,
}

/// Variable-size header for the binary idxStr. Followed by:
///   [PlanConstraint; num_constraints]
///   [u8; num_order_by]  (column IDs for ORDER BY)
///
/// Allocated with sqlite3_malloc, freed by SQLite via sqlite3_free.
/// No heap pointers, no Drop — pure POD.
#[repr(C, packed)]
pub struct ChangesIdxHeader {
    pub magic: [u8; 11],         // "Rust magic\0"
    pub num_constraints: u8,
    pub num_order_by: u8,
    pub order_by_desc: u8,       // 1 for DESC, 0 for ASC
    pub has_order_by: u8,        // 1 if user provided ORDER BY, 0 if default
}

/// Read the constraints from a ChangesIdxHeader pointer.
/// Returns (constraints, order_by_cols, order_by_desc, has_order_by).
pub unsafe fn read_idx_plan(
    ptr: *const c_char,
) -> (
    Vec<PlanConstraint>,
    Vec<crate::c::CrsqlChangesColumn>,
    bool,
    bool,
) {
    if ptr.is_null() {
        return (vec![], vec![], false, false);
    }
    let header = &*(ptr as *const ChangesIdxHeader);
    if header.magic != IDX_MAGIC {
        return (vec![], vec![], false, false);
    }
    let nc = header.num_constraints as usize;
    let no = header.num_order_by as usize;

    let constraints_ptr = (ptr as *const u8).add(core::mem::size_of::<ChangesIdxHeader>())
        as *const PlanConstraint;
    let constraints = core::slice::from_raw_parts(constraints_ptr, nc).to_vec();

    let order_ptr = (constraints_ptr as *const u8).add(nc * core::mem::size_of::<PlanConstraint>());
    // CrsqlChangesColumn is #[repr(u8)], so we can transmute the raw bytes
    let order_by: Vec<crate::c::CrsqlChangesColumn> =
        core::slice::from_raw_parts(order_ptr as *const crate::c::CrsqlChangesColumn, no)
            .to_vec();

    (constraints, order_by, header.order_by_desc != 0, header.has_order_by != 0)
}

/// Allocate a ChangesIdxHeader + trailing arrays with sqlite3_malloc.
/// Returns the pointer (SQLite will free it via sqlite3_free).
pub fn alloc_idx_plan(
    constraints: &[PlanConstraint],
    order_by_cols: &[crate::c::CrsqlChangesColumn],
    order_by_desc: bool,
    has_order_by: bool,
) -> *mut c_char {
    let header_size = core::mem::size_of::<ChangesIdxHeader>();
    let constraint_size = constraints.len() * core::mem::size_of::<PlanConstraint>();
    let order_size = order_by_cols.len();
    let total = header_size + constraint_size + order_size;

    let ptr = unsafe { sqlite::malloc(total) } as *mut u8;
    if ptr.is_null() {
        return core::ptr::null_mut();
    }

    unsafe {
        let header = ptr as *mut ChangesIdxHeader;
        (*header).magic = IDX_MAGIC;
        (*header).num_constraints = constraints.len() as u8;
        (*header).num_order_by = order_by_cols.len() as u8;
        (*header).order_by_desc = if order_by_desc { 1 } else { 0 };
        (*header).has_order_by = if has_order_by { 1 } else { 0 };

        let c_ptr = ptr.add(header_size) as *mut PlanConstraint;
        for (i, c) in constraints.iter().enumerate() {
            *c_ptr.add(i) = PlanConstraint {
                col: c.col,
                op_id: c.op_id,
                param_idx: c.param_idx,
            };
        }

        let o_ptr = (c_ptr as *mut u8).add(constraint_size);
        for (i, &col) in order_by_cols.iter().enumerate() {
            *o_ptr.add(i) = col as u8;
        }
    }

    ptr as *mut c_char
}

use crate::c::{
    crsql_Changes_cursor, crsql_Changes_vtab, ChangeRowType, ClockUnionColumn, CrsqlChangesColumn,
};
use crate::consts;
use crate::changes_vtab_read::changes_union_query;
use crate::pack_columns::bind_package_to_stmt;
use crate::pack_columns::unpack_columns;

fn changes_crsr_finalize(crsr: *mut crsql_Changes_cursor) -> c_int {
    // Assign pointers to null after freeing
    // since we can get into this twice for the same cursor object.
    unsafe {
        let mut rc = 0;
        rc += match (*crsr).pChangesStmt.finalize() {
            Ok(rc) => rc as c_int,
            Err(rc) => rc as c_int,
        };
        (*crsr).pChangesStmt = null_mut();
        // Also finalize the cached statement if any
        rc += match (*crsr).cached_pChangesStmt.finalize() {
            Ok(rc) => rc as c_int,
            Err(rc) => rc as c_int,
        };
        (*crsr).cached_pChangesStmt = null_mut();
        let reset_rc = reset_cached_stmt((*crsr).pRowStmt);
        match reset_rc {
            Ok(r) | Err(r) => rc += r as c_int,
        }
        (*crsr).pRowStmt = null_mut();
        (*crsr).dbVersion = crate::consts::MIN_POSSIBLE_DB_VERSION;

        // Clear cached idx_str reference (SQLite owns the memory)
        (*crsr).cached_idx_str = null_mut();

        rc
    }
}

// A very c-style port. We can get more idiomatic once we finish the rust port and have test and perf parity
#[no_mangle]
pub unsafe extern "C" fn crsql_changes_best_index(
    vtab: *mut sqlite::vtab,
    index_info: *mut sqlite::index_info,
) -> c_int {
    match changes_best_index(vtab, index_info) {
        Ok(rc) => rc as c_int,
        Err(rc) => rc as c_int,
    }
}

fn changes_best_index(
    _vtab: *mut sqlite::vtab,
    index_info: *mut sqlite::index_info,
) -> Result<ResultCode, ResultCode> {
    let mut idx_num: i32 = 0;

    let mut plan_constraints: Vec<PlanConstraint> = Vec::new();
    let constraints = sqlite::args!((*index_info).nConstraint, (*index_info).aConstraint);
    let constraint_usage =
        sqlite::args_mut!((*index_info).nConstraint, (*index_info).aConstraintUsage);
    let mut arg_v_index = 1;
    for (i, constraint) in constraints.iter().enumerate() {
        if !constraint_is_usable(constraint) {
            continue;
        }
        let col = CrsqlChangesColumn::from_i32(constraint.iColumn);
        if let Some(col_enum) = col {
            if is_supported_op(constraint.op) {
                if constraint.op == sqlite::INDEX_CONSTRAINT_ISNOTNULL as u8
                    || constraint.op == sqlite::INDEX_CONSTRAINT_ISNULL as u8
                {
                    constraint_usage[i].argvIndex = 0;
                    constraint_usage[i].omit = 1;
                    plan_constraints.push(PlanConstraint {
                        col: col_enum,
                        op_id: constraint.op,
                        param_idx: 0,
                    });
                } else {
                    constraint_usage[i].argvIndex = arg_v_index;
                    constraint_usage[i].omit = 1;
                    plan_constraints.push(PlanConstraint {
                        col: col_enum,
                        op_id: constraint.op,
                        param_idx: arg_v_index as u8,
                    });
                    arg_v_index += 1;
                }
            }
        }

        // idx bit mask
        match col {
            Some(CrsqlChangesColumn::DbVrsn) => idx_num |= 2,
            Some(CrsqlChangesColumn::SiteId) => idx_num |= 4,
            _ => {}
        }
    }

    let mut desc = false;
    let order_bys = sqlite::args!((*index_info).nOrderBy, (*index_info).aOrderBy);
    let mut order_by_consumed = true;
    let mut order_by_cols: Vec<CrsqlChangesColumn> = Vec::new();
    let has_order_by = !order_bys.is_empty();
    for order_by in order_bys {
        desc = order_by.desc != 0;
        let col = CrsqlChangesColumn::from_i32(order_by.iColumn);
        if let Some(col_enum) = col {
            // Only include columns we recognize (skip pk, cval)
            if !matches!(col_enum, CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval) {
                order_by_cols.push(col_enum);
            } else {
                order_by_consumed = false;
            }
        } else {
            order_by_consumed = false;
        }
    }

    // If no user ORDER BY, default to db_vrsn, seq ASC
    if !has_order_by {
        order_by_cols.push(CrsqlChangesColumn::DbVrsn);
        order_by_cols.push(CrsqlChangesColumn::Seq);
    }

    // TODO: update your order by py test to explain query plans to ensure correct indices are selected
    // both constraints are present. Also to check that order by is consumed.
    if idx_num & 6 == 6 {
        unsafe {
            (*index_info).estimatedCost = 1.0;
            (*index_info).estimatedRows = 1;
        }
    }
    // only the version constraint is present
    else if idx_num & 2 == 2 {
        unsafe {
            (*index_info).estimatedCost = 10.0;
            (*index_info).estimatedRows = 10;
        }
    }
    // no constraints are present or only the requestor constraint is present
    else {
        unsafe {
            (*index_info).estimatedCost = 2147483647.0;
            (*index_info).estimatedRows = 2147483647;
        }
    }

    let ptr = alloc_idx_plan(&plan_constraints, &order_by_cols, desc, has_order_by);
    unsafe {
        (*index_info).idxNum = idx_num;
        (*index_info).orderByConsumed = if order_by_consumed { 1 } else { 0 };
        (*index_info).idxStr = ptr;
        (*index_info).needToFreeIdxStr = 1;
    }

    Ok(ResultCode::OK)
}

fn constraint_is_usable(constraint: &sqlite::index_constraint) -> bool {
    if constraint.usable == 0 {
        return false;
    }
    if let Some(col) = CrsqlChangesColumn::from_i32(constraint.iColumn) {
        // Pk (packed blob) and Cval (no backing column) are not usable
        // as index constraints. Tbl is accepted — in V2-wire packed mode it
        // is pushed into arms as a literal comparison for branch pruning;
        // in other modes it is enforced by the outer WHERE.
        !matches!(col, CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval)
    } else {
        false
    }
}

/// Returns true if the operator is one we accept as a vtab constraint.
/// LIKE/MATCH/GLOB/REGEXP are accepted with omit=1 so SQLite trusts the vtab
/// to handle them. We then error in changes_union_query (xFilter) — this
/// prevents SQLite from falling back to a plan that evaluates them externally
/// on packed BLOB outputs (which would silently produce wrong results).
fn is_supported_op(op: u8) -> bool {
    matches!(op as u32,
        sqlite::INDEX_CONSTRAINT_EQ
        | sqlite::INDEX_CONSTRAINT_GT
        | sqlite::INDEX_CONSTRAINT_LE
        | sqlite::INDEX_CONSTRAINT_LT
        | sqlite::INDEX_CONSTRAINT_GE
        | sqlite::INDEX_CONSTRAINT_MATCH
        | sqlite::INDEX_CONSTRAINT_LIKE
        | sqlite::INDEX_CONSTRAINT_GLOB
        | sqlite::INDEX_CONSTRAINT_REGEXP
        | sqlite::INDEX_CONSTRAINT_NE
        | sqlite::INDEX_CONSTRAINT_ISNOT
        | sqlite::INDEX_CONSTRAINT_ISNOTNULL
        | sqlite::INDEX_CONSTRAINT_ISNULL
        | sqlite::INDEX_CONSTRAINT_IS
    )
}

// This'll become safe once more code is moved over to Rust
#[no_mangle]
pub unsafe extern "C" fn crsql_changes_filter(
    cursor: *mut sqlite::vtab_cursor,
    _idx_num: c_int,
    idx_str: *const c_char,
    argc: c_int,
    argv: *mut *mut sqlite::value,
) -> c_int {
    let args = sqlite::args!(argc, argv);
    let cursor = cursor.cast::<crsql_Changes_cursor>();
    match changes_filter(cursor, idx_str, args) {
        Err(rc) | Ok(rc) => rc as c_int,
    }
}

unsafe fn changes_filter(
    cursor: *mut crsql_Changes_cursor,
    idx_str: *const c_char,
    args: &[*mut sqlite::value],
) -> Result<ResultCode, ResultCode> {
    let tab = (*cursor).pTab;
    let db = (*tab).db;

    let c_rc = crsql_ensure_table_infos_are_up_to_date(
        db,
        (*tab).pExtData,
        &mut (*tab).base.zErrMsg as *mut _,
    );
    if c_rc != 0 {
        if let Some(rc) = ResultCode::from_i32(c_rc) {
            return Err(rc);
        } else {
            return Err(ResultCode::ERROR);
        }
    }

    // nothing to fetch, no crrs exist.
    let tbl_infos = mem::ManuallyDrop::new(Box::from_raw(
        (*(*tab).pExtData).tableInfos as *mut Vec<TableInfo>,
    ));
    if tbl_infos.len() == 0 {
        return Ok(ResultCode::OK);
    }

    let metadata_use_version = unsafe { (*(*tab).pExtData).metadataUseVersion };
    let sync_log_version = unsafe { (*(*tab).pExtData).syncLogVersion };
    let schema_version = unsafe { (*(*tab).pExtData).pragmaSchemaVersionForTableInfos };

    // Check if we can reuse the cached prepared statement.
    // Cache hit: cached stmt exists + same idx_str + same config + same schema version.
    let cache_hit = !(*cursor).cached_pChangesStmt.is_null()
        && !(*cursor).cached_idx_str.is_null()
        && idx_str == (*cursor).cached_idx_str as *const u8 as *const _
        && (*cursor).cached_meta_use_version == metadata_use_version
        && (*cursor).cached_sync_log_version == sync_log_version
        && (*cursor).cached_schema_version == schema_version;

    if cache_hit {
        // Reuse cached statement — move it back to pChangesStmt, reset + rebind args
        let stmt = (*cursor).cached_pChangesStmt;
        (*cursor).cached_pChangesStmt = null_mut();
        (*cursor).pChangesStmt = stmt;
        stmt.clear_bindings()?;
        for (i, arg) in args.iter().enumerate() {
            stmt.bind_value(i as i32 + 1, *arg)?;
        }
    } else {
        // Cache miss — finalize old cached statement, build new SQL, prepare
        if !(*cursor).cached_pChangesStmt.is_null() {
            (*cursor).cached_pChangesStmt.finalize()?;
            (*cursor).cached_pChangesStmt = null_mut();
        }
        (*cursor).cached_idx_str = null_mut();

        let tbl_refs: Vec<&TableInfo> = tbl_infos.iter().collect();
        let sql = changes_union_query(&tbl_refs, idx_str, metadata_use_version, sync_log_version)?;
        let stmt = db.prepare_v2(&sql)?;
        for (i, arg) in args.iter().enumerate() {
            stmt.bind_value(i as i32 + 1, *arg)?;
        }
        (*cursor).pChangesStmt = stmt.stmt;
        forget(stmt);

        // Cache the idx_str pointer (owned by SQLite, stable for the prepared stmt's lifetime)
        (*cursor).cached_idx_str = idx_str as *const c_char;
        (*cursor).cached_meta_use_version = metadata_use_version;
        (*cursor).cached_sync_log_version = sync_log_version;
        (*cursor).cached_schema_version = schema_version;
    }

    changes_next(cursor, (*cursor).pTab.cast::<sqlite::vtab>())
}

/**
 * Advances our Changes_cursor to its next row of output.
 * TODO: this'll get more idiomatic as we move dependencies to Rust
 */
#[no_mangle]
pub unsafe extern "C" fn crsql_changes_next(cursor: *mut sqlite::vtab_cursor) -> c_int {
    let cursor = cursor.cast::<crsql_Changes_cursor>();
    let vtab = (*cursor).pTab.cast::<sqlite::vtab>();
    match changes_next(cursor, vtab) {
        Ok(rc) => rc as c_int,
        Err(rc) => {
            changes_crsr_finalize(cursor);
            rc as c_int
        }
    }
}

// We'll get more idiomatic once we have more Rust and less C
unsafe fn changes_next(
    cursor: *mut crsql_Changes_cursor,
    vtab: *mut sqlite::vtab,
) -> Result<ResultCode, ResultCode> {
    if (*cursor).pChangesStmt.is_null() {
        let err = CString::new("pChangesStmt is null in changes_next")?;
        (*vtab).zErrMsg = err.into_raw();
        return Err(ResultCode::ABORT);
    }

    if !(*cursor).pRowStmt.is_null() {
        let rc = reset_cached_stmt((*cursor).pRowStmt);
        (*cursor).pRowStmt = null_mut();
        rc?;
    }

    let rc = (*cursor).pChangesStmt.step()?;
    if rc == ResultCode::DONE {
        // Reset the statement and move it to cache for potential reuse on next xFilter.
        // Set pChangesStmt to null so changes_eof sees EOF.
        let stmt = (*cursor).pChangesStmt;
        stmt.reset()?;
        // Finalize any previously cached statement before caching this one
        if !(*cursor).cached_pChangesStmt.is_null() {
            (*cursor).cached_pChangesStmt.finalize()?;
        }
        (*cursor).cached_pChangesStmt = stmt;
        (*cursor).pChangesStmt = null_mut();
        if !(*cursor).pRowStmt.is_null() {
            let reset_rc = reset_cached_stmt((*cursor).pRowStmt);
            (*cursor).pRowStmt = null_mut();
            reset_rc?;
        }
        (*cursor).dbVersion = crate::consts::MIN_POSSIBLE_DB_VERSION;
        return Ok(ResultCode::OK);
    }

    // we had a row... we can do the rest
    let tbl = (*cursor)
        .pChangesStmt
        .column_text(ClockUnionColumn::Tbl as i32);
    let pks = (*cursor)
        .pChangesStmt
        .column_value(ClockUnionColumn::Pks as i32);
    let cid = (*cursor)
        .pChangesStmt
        .column_text(ClockUnionColumn::Cid as i32);
    let db_version = (*cursor)
        .pChangesStmt
        .column_int64(ClockUnionColumn::DbVrsn as i32);
    let changes_rowid = (*cursor)
        .pChangesStmt
        .column_int64(ClockUnionColumn::RowId as i32);
    (*cursor).dbVersion = db_version;

    let tbl_infos = mem::ManuallyDrop::new(Box::from_raw(
        (*(*(*cursor).pTab).pExtData).tableInfos as *mut Vec<TableInfo>,
    ));
    // TODO: will this work given `insert_tbl` is null termed?
    let tbl_info_index = tbl_infos.iter().position(|x| x.tbl_name == tbl);

    if tbl_info_index.is_none() {
        let err = CString::new(format!("could not find schema for table {}", tbl))?;
        (*vtab).zErrMsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }
    // TODO: technically safe since we checked `is_none` but this should be more idiomatic
    let tbl_info_index = tbl_info_index.unwrap();

    let tbl_info = &tbl_infos[tbl_info_index];
    (*cursor).changesRowid = changes_rowid;
    (*cursor).tblInfoIdx = tbl_info_index as i32;

    if tbl_info.pks.is_empty() {
        let err = CString::new(format!("crr {} is missing primary keys", tbl))?;
        (*vtab).zErrMsg = err.into_raw();
        return Err(ResultCode::ERROR);
    }

    if cid == crate::c::DELETE_SENTINEL || cid == consts::V2_HASH_TOMBSTONE_CID {
        (*cursor).rowType = ChangeRowType::Delete as c_int;
        return Ok(ResultCode::OK);
    } else if cid == crate::c::INSERT_SENTINEL {
        (*cursor).rowType = ChangeRowType::PkOnly as c_int;
        return Ok(ResultCode::OK);
    } else {
        let sync_log_version = (*(*(*cursor).pTab).pExtData).syncLogVersion;
        if sync_log_version == crate::consts::SYNC_LOG_V2 {
            // Packed (v2 sync-log) row: all update rows are packed in V2 wire format.
            // cval is already in the query result (ClockUnionColumn::Cval), no lazy fetch needed.
            (*cursor).rowType = ChangeRowType::PackedUpdate as c_int;
            return Ok(ResultCode::OK);
        }
        (*cursor).rowType = ChangeRowType::Update as c_int;
    }

    // V2 metadata fetches cval inline in the query — no lazy fetch needed.
    let metadata_use_version = (*(*(*cursor).pTab).pExtData).metadataUseVersion;
    if metadata_use_version == crate::consts::META_USE_V2 {
        return Ok(ResultCode::OK);
    }

    let row_stmt_ref = tbl_info.get_row_patch_data_stmt((*(*cursor).pTab).db, cid)?;
    let row_stmt = row_stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;

    let packed_pks = pks.blob();
    let unpacked_pks = unpack_columns(packed_pks)?;
    bind_package_to_stmt(row_stmt.stmt, &unpacked_pks, 0)?;

    match row_stmt.step() {
        Ok(ResultCode::DONE) => {
            reset_cached_stmt(row_stmt.stmt)?;
        }
        Ok(_) => {}
        Err(rc) => {
            reset_cached_stmt(row_stmt.stmt)?;
            return Err(rc);
        }
    }

    (*cursor).pRowStmt = row_stmt.stmt;
    Ok(ResultCode::OK)
}

#[no_mangle]
pub extern "C" fn crsql_changes_eof(cursor: *mut sqlite::vtab_cursor) -> c_int {
    let cursor = cursor.cast::<crsql_Changes_cursor>();
    if unsafe { (*cursor).pChangesStmt.is_null() } {
        1
    } else {
        0
    }
}

#[no_mangle]
pub extern "C" fn crsql_changes_column(
    cursor: *mut sqlite::vtab_cursor, /* The cursor */
    ctx: *mut sqlite::context,        /* First argument to sqlite3_result_...() */
    i: c_int,                         /* Which column to return */
) -> c_int {
    match column_impl(cursor, ctx, i) {
        Ok(code) | Err(code) => code as c_int,
    }
}

fn column_impl(
    cursor: *mut sqlite::vtab_cursor,
    ctx: *mut sqlite::context,
    i: c_int,
) -> Result<ResultCode, ResultCode> {
    let cursor = cursor.cast::<crsql_Changes_cursor>();
    let column = CrsqlChangesColumn::from_i32(i);
    // TODO: only de-reference where needed?
    let changes_stmt = unsafe { (*cursor).pChangesStmt };
    match column {
        Some(CrsqlChangesColumn::Tbl) => {
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Tbl as i32));
        }
        Some(CrsqlChangesColumn::Pk) => {
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Pks as i32));
        }
        Some(CrsqlChangesColumn::Cval) => unsafe {
            let row_type = ChangeRowType::from_i32((*cursor).rowType);
            match row_type {
                Some(ChangeRowType::PackedUpdate) => {
                    // Packed format: cval is in the query result directly.
                    // Copy blob data to a local Vec before passing to result_blob_transient
                    // to avoid use-after-free when other columns are read from the underlying stmt.
                    let val = changes_stmt.column_value(ClockUnionColumn::Cval as i32);
                    let blob_copy: Vec<u8> = val.blob().to_vec();
                    ctx.result_blob_transient(&blob_copy);
                }
                Some(ChangeRowType::Update) => {
                    // V2 metadata: cval is inline in the query (pRowStmt is null, lazy fetch skipped).
                    // Use result_value to preserve the original type (text, int, float, blob).
                    // V1 metadata: cval is fetched lazily via pRowStmt.
                    if (*cursor).pRowStmt.is_null() {
                        let val = changes_stmt.column_value(ClockUnionColumn::Cval as i32);
                        ctx.result_value(val);
                    } else {
                        ctx.result_value((*cursor).pRowStmt.column_value(0));
                    }
                }
                _ => {
                    // V1 metadata: lazy fetch from main table via pRowStmt
                    if (*cursor).pRowStmt.is_null() {
                        ctx.result_null();
                    } else {
                        ctx.result_value((*cursor).pRowStmt.column_value(0));
                    }
                }
            }
        },
        Some(CrsqlChangesColumn::Cid) => unsafe {
            let row_type = ChangeRowType::from_i32((*cursor).rowType);
            match row_type {
                Some(ChangeRowType::PkOnly) => ctx.result_text_static(crate::c::INSERT_SENTINEL),
                Some(ChangeRowType::Delete) => {
                    // Could be -1 (V1 delete sentinel) or -2 (V2 hash tombstone)
                    ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Cid as i32));
                }
                Some(ChangeRowType::PackedUpdate) => {
                    // Packed format: cid is group_concat with char(0) separators, cast to blob.
                    // Read as blob explicitly to preserve null-byte separators.
                    let blob_copy: Vec<u8> = changes_stmt.column_blob(ClockUnionColumn::Cid as i32).to_vec();
                    ctx.result_blob_transient(&blob_copy);
                }
                Some(ChangeRowType::Update) => {
                    // V2 metadata: pRowStmt is null (no lazy fetch), cid is in the query result.
                    // V1 metadata: pRowStmt is non-null if the row exists in the main table.
                    //   If pRowStmt is null in V1, the row was deleted — return DELETE_SENTINEL.
                    if (*cursor).pRowStmt.is_null() {
                        let metadata_use_version = (*(*(*cursor).pTab).pExtData).metadataUseVersion;
                        if metadata_use_version == crate::consts::META_USE_V2 {
                            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Cid as i32));
                        } else {
                            ctx.result_text_static(crate::c::DELETE_SENTINEL);
                        }
                    } else {
                        ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Cid as i32));
                    }
                }
                None => return Err(ResultCode::ABORT),
            }
        },
        Some(CrsqlChangesColumn::ColVrsn) => unsafe {
            let row_type = ChangeRowType::from_i32((*cursor).rowType);
            if row_type == Some(ChangeRowType::PackedUpdate) {
                let blob_copy: Vec<u8> = changes_stmt.column_blob(ClockUnionColumn::ColVrsn as i32).to_vec();
                ctx.result_blob_transient(&blob_copy);
            } else {
                ctx.result_value(changes_stmt.column_value(ClockUnionColumn::ColVrsn as i32));
            }
        }
        Some(CrsqlChangesColumn::DbVrsn) => {
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::DbVrsn as i32));
        }
        Some(CrsqlChangesColumn::SiteId) => {
            // todo: short circuit null? if col type null bind null rather than value?
            // sholdn't matter..
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::SiteId as i32));
        }
        Some(CrsqlChangesColumn::Seq) => unsafe {
            let row_type = ChangeRowType::from_i32((*cursor).rowType);
            if row_type == Some(ChangeRowType::PackedUpdate) {
                let blob_copy: Vec<u8> = changes_stmt.column_blob(ClockUnionColumn::Seq as i32).to_vec();
                ctx.result_blob_transient(&blob_copy);
            } else {
                ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Seq as i32));
            }
        }
        Some(CrsqlChangesColumn::Cl) => {
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Cl as i32))
        }
        Some(CrsqlChangesColumn::Ts) => {
            ctx.result_value(changes_stmt.column_value(ClockUnionColumn::Ts as i32));
        }
        None => return Err(ResultCode::MISUSE),
    }

    Ok(ResultCode::OK)
}

#[no_mangle]
pub extern "C" fn crsql_changes_rowid(
    cursor: *mut sqlite::vtab_cursor,
    rowid: *mut sqlite::int64,
) -> c_int {
    let cursor = cursor.cast::<crsql_Changes_cursor>();
    unsafe {
        *rowid = crate::util::slab_rowid((*cursor).tblInfoIdx, (*cursor).changesRowid);
        if *rowid < 0 {
            return ResultCode::ERROR as c_int;
        }
    }
    ResultCode::OK as c_int
}

#[no_mangle]
pub extern "C" fn crsql_changes_update(
    vtab: *mut sqlite::vtab,
    argc: c_int,
    argv: *mut *mut sqlite::value,
    row_id: *mut sqlite::int64,
) -> c_int {
    let args = sqlite::args!(argc, argv);
    let arg = args[0];
    if args.len() > 1 && arg.value_type() == ColumnType::Null {
        // insert statement
        // argv[1] is the rowid.. but why would it ever be filled for us?
        let mut err_msg = null_mut();
        let rc = unsafe { crsql_merge_insert(vtab, argc, argv, row_id, &mut err_msg as *mut _) };
        if rc != ResultCode::OK as c_int {
            unsafe {
                (*vtab).zErrMsg = err_msg;
            }
        }
        rc
    } else if let Ok(err) = CString::new(
        "Only INSERT and SELECT statements are allowed against the crsql changes table",
    ) {
        unsafe {
            (*vtab).zErrMsg = err.into_raw();
        }
        ResultCode::MISUSE as c_int
    } else {
        ResultCode::NOMEM as c_int
    }
}

// If xBegin is not defined xCommit is not called.
#[no_mangle]
pub extern "C" fn crsql_changes_begin(_vtab: *mut sqlite::vtab) -> c_int {
    ResultCode::OK as c_int
}

#[no_mangle]
pub extern "C" fn crsql_changes_commit(vtab: *mut sqlite::vtab) -> c_int {
    let tab = vtab.cast::<crsql_Changes_vtab>();
    unsafe {
        (*(*tab).pExtData).rowsImpacted = 0;
    }
    ResultCode::OK as c_int
}

#[no_mangle]
pub extern "C" fn crsql_changes_savepoint(_vtab: *mut sqlite::vtab, _n: c_int) -> c_int {
    ResultCode::OK as c_int
}

#[no_mangle]
pub extern "C" fn crsql_changes_release(_vtab: *mut sqlite::vtab, _n: c_int) -> c_int {
    ResultCode::OK as c_int
}

// clear ordinal cache on rollback so we don't have wrong data in the cache.
#[no_mangle]
pub extern "C" fn crsql_changes_rollback_to(vtab: *mut sqlite::vtab, _: c_int) -> c_int {
    let tab = vtab.cast::<crsql_Changes_vtab>();

    let mut ordinals = unsafe {
        mem::ManuallyDrop::new(Box::from_raw(
            (*(*tab).pExtData).ordinalMap as *mut BTreeMap<Vec<u8>, i64>,
        ))
    };

    let mut table_infos = unsafe {
        mem::ManuallyDrop::new(Box::from_raw(
            (*(*tab).pExtData).tableInfos as *mut Vec<TableInfo>,
        ))
    };
    for tbl_info in table_infos.iter_mut() {
        tbl_info.clear_cl_cache();
    }

    ordinals.clear();
    ResultCode::OK as c_int
}
