use alloc::format;
use sqlite::Connection;
use sqlite_nostd as sqlite;
use sqlite_nostd::ResultCode;

/**
* Given a table name, returns whether or not it has already
* been upgraded to a CRR.
*/
pub fn is_crr(db: *mut sqlite::sqlite3, table: &str) -> Result<bool, ResultCode> {
    // Check for all three CRR trigger types. A table is only a CRR if all
    // triggers exist — partial trigger state (e.g. insert trigger created
    // but update/delete failed) should not be treated as a registered CRR.
    let trigger_suffixes = ["__crsql_itrig", "__crsql_utrig", "__crsql_dtrig"];
    for suffix in &trigger_suffixes {
        let stmt =
            db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?")?;
        stmt.bind_text(
            1,
            &format!("{}{}", table, suffix),
            sqlite::Destructor::TRANSIENT,
        )?;
        stmt.step()?;
        let count = stmt.column_int(0);
        if count == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}
