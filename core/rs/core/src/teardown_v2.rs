extern crate alloc;

use sqlite_nostd as sqlite;
use sqlite_nostd::ResultCode;

use crate::bootstrap_v2;

/// Remove all V2 metadata tables for a CRR table.
pub fn remove_crr_v2_tables(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<ResultCode, ResultCode> {
    bootstrap_v2::drop_v2_tables(db, table)
}
