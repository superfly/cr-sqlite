pub use crate::bootstrap;
pub use crate::c;
pub use crate::consts;
pub use crate::db_version;
pub fn refresh_connection_state_if_needed(
    db: *mut sqlite_nostd::sqlite3,
    ext_data: *mut crate::c::crsql_ExtData,
) -> Result<(), alloc::string::String> {
    crate::ext_data::refresh_connection_state_if_needed(db, ext_data)
}
pub use crate::pack_columns;
pub use crate::tableinfo;
