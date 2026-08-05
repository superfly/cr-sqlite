use sqlite_nostd as sqlite;
use sqlite_nostd::{Connection, ResultCode};
extern crate alloc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub fn remove_crr_clock_table_if_exists(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<ResultCode, ResultCode> {
    let escaped_table = crate::util::escape_ident(table);
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{table}__crsql_clock\"",
        table = escaped_table
    ))?;
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{table}__crsql_pks\"",
        table = escaped_table
    ))
}

pub fn remove_crr_triggers_if_exist(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<ResultCode, ResultCode> {
    let escaped_table = crate::util::escape_ident(table);

    db.exec_safe(&format!(
        "DROP TRIGGER IF EXISTS \"{table}__crsql_itrig\"",
        table = escaped_table
    ))?;

    db.exec_safe(&format!(
        "DROP TRIGGER IF EXISTS \"{table}__crsql_utrig\"",
        table = escaped_table
    ))?;

    // Collect pk col names first, then drop triggers after statement is finalized
    // to avoid LOCKED errors from schema modifications while a stmt is active.
    let pk_cols: Vec<String> = {
        let stmt = db.prepare_v2("SELECT name FROM pragma_table_info(?) WHERE pk > 0")?;
        stmt.bind_text(1, table, sqlite::Destructor::STATIC)?;
        let mut cols = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            cols.push(String::from(stmt.column_text(0)?));
        }
        cols
    };

    for col_name in &pk_cols {
        db.exec_safe(&format!(
            "DROP TRIGGER IF EXISTS \"{tbl_name}_{col_name}__crsql_utrig\"",
            tbl_name = crate::util::escape_ident(table),
            col_name = crate::util::escape_ident(col_name),
        ))?;
    }

    db.exec_safe(&format!(
        "DROP TRIGGER IF EXISTS \"{table}__crsql_dtrig\"",
        table = escaped_table
    ))
}
