extern crate alloc;

use crate::{alloc::string::ToString, tableinfo::ColumnInfo};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::str::Utf8Error;
use sqlite::{sqlite3, ColumnType, Connection, ResultCode};
use sqlite_nostd as sqlite;
use sqlite_nostd::Destructor;

pub fn get_dflt_value(
    db: *mut sqlite3,
    table: &str,
    col: &str,
) -> Result<Option<String>, ResultCode> {
    let sql = "SELECT [dflt_value], [notnull] FROM pragma_table_info(?) WHERE name = ?";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, table, sqlite_nostd::Destructor::STATIC)?;
    stmt.bind_text(2, col, sqlite_nostd::Destructor::STATIC)?;
    let rc = stmt.step()?;
    if rc == ResultCode::DONE {
        // There should always be a row for a column in pragma_table_info
        return Err(ResultCode::DONE);
    }

    let notnull = stmt.column_int(1);
    let dflt_column_type = stmt.column_type(0)?;

    // if the column is nullable and no default value is specified
    // then the default value is null.
    if notnull == 0 && dflt_column_type == ColumnType::Null {
        return Ok(Some(String::from("NULL")));
    }

    if dflt_column_type == ColumnType::Null {
        // no default value specified
        // and the column is not nullable
        return Ok(None);
    }

    let raw = String::from(stmt.column_text(0)?);
    // pragma_table_info returns string defaults with surrounding quotes
    // (e.g., "'2018-01-01'" for DEFAULT '2018-01-01'). Strip them so the
    // value matches what IS stored in the column for comparison purposes.
    let stripped = if raw.len() >= 2
        && ((raw.starts_with('\'') && raw.ends_with('\''))
            || (raw.starts_with('"') && raw.ends_with('"')))
    {
        // Unescape doubled quote characters inside the default value
        // (e.g., DEFAULT 'O''Brien' produces raw "'O''Brien'" which
        // should become O'Brien, not O''Brien).
        let quote = raw.chars().next().unwrap();
        let inner = &raw[1..raw.len() - 1];
        let doubled = format!("{}{}", quote, quote);
        inner.replace(&doubled, &quote.to_string())
    } else {
        raw
    };
    Ok(Some(stripped))
}

pub fn get_db_version_union_query(tbl_names: &[String]) -> String {
    if tbl_names.is_empty() {
        // Avoid producing invalid SQL like "SELECT max(version) FROM ( UNION SELECT ...)".
        // Filter by site_id (bind param 1) to match the per-table union path, which
        // uses `WHERE site_id = 0` (ordinal 0 = local site). Without this filter we'd
        // return the max db_version across ALL sites, not just our own.
        return "SELECT max(db_version) as version FROM crsql_db_versions WHERE site_id = ?".to_string();
    }
    let unions_str = tbl_names
        .iter()
        .map(|tbl_name| {
            format!(
                "SELECT max(db_version) as version FROM \"{}\" WHERE site_id = 0",
                escape_ident(tbl_name),
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");

    format!(
        "SELECT max(version) as version FROM ({} UNION SELECT value as
        version FROM crsql_master WHERE key = 'pre_compact_dbversion')",
        unions_str
    )
}

pub fn slab_rowid(idx: i32, rowid: sqlite::int64) -> sqlite::int64 {
    if idx < 0 {
        return -1;
    }

    // Use Euclidean remainder to ensure non-negative modulo even for negative rowids.
    let modulo = rowid.rem_euclid(crate::consts::ROWID_SLAB_SIZE);
    // Use checked arithmetic to detect overflow rather than wrapping silently.
    match (idx as i64).checked_mul(crate::consts::ROWID_SLAB_SIZE) {
        Some(product) => match product.checked_add(modulo) {
            Some(result) => result,
            None => -1,
        },
        None => -1,
    }
}

pub fn where_list(columns: &Vec<ColumnInfo>, prefix: Option<&str>) -> Result<String, Utf8Error> {
    let mut result = vec![];
    for c in columns {
        let name = &c.name;
        if let Some(prefix) = prefix {
            result.push(format!(
                "{prefix}\"{col_name}\" IS ?",
                prefix = prefix,
                col_name = crate::util::escape_ident(name)
            ));
        } else {
            result.push(format!(
                "\"{col_name}\" IS ?",
                col_name = crate::util::escape_ident(name)
            ));
        }
    }

    Ok(result.join(" AND "))
}

pub fn binding_list(num_slots: usize) -> String {
    core::iter::repeat('?')
        .take(num_slots)
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn as_identifier_list(
    columns: &Vec<ColumnInfo>,
    prefix: Option<&str>,
) -> Result<String, Utf8Error> {
    let mut result = vec![];
    for c in columns {
        result.push(if let Some(prefix) = prefix {
            format!("{}\"{}\"", prefix, crate::util::escape_ident(&c.name))
        } else {
            format!("\"{}\"", crate::util::escape_ident(&c.name))
        })
    }
    Ok(result.join(","))
}

pub fn escape_ident(ident: &str) -> String {
    // NUL bytes would truncate the identifier when passed to SQLite as a C string,
    // enabling identifier injection. Reject them.
    if ident.contains('\0') {
        return String::new();
    }
    ident.replace("\"", "\"\"")
}

pub fn escape_ident_as_value(ident: &str) -> String {
    // NUL bytes would truncate the value when passed to SQLite as a C string,
    // enabling injection. Reject them, matching the behavior of escape_ident.
    if ident.contains('\0') {
        return String::new();
    }
    ident.replace("'", "''")
}

pub trait Countable {
    fn count(self, sql: &str) -> Result<i32, ResultCode>;
}

impl Countable for *mut sqlite::sqlite3 {
    fn count(self, sql: &str) -> Result<i32, ResultCode> {
        let stmt = self.prepare_v2(sql)?;
        if stmt.step()? == ResultCode::ROW {
            Ok(stmt.column_int(0))
        } else {
            // No row was produced; return 0 rather than reading an invalid column.
            Ok(0)
        }
    }
}

/// Get an integer value from crsql_master by exact key.
/// Returns None if the key does not exist.
pub unsafe fn get_master_value(db: *mut sqlite3, key: &str) -> Result<Option<i64>, ResultCode> {
    let sql = "SELECT value FROM crsql_master WHERE key = ?";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        return Ok(Some(stmt.column_int64(0)));
    }
    Ok(None)
}

/// Get a cached count from crsql_master, or run a count query and cache it.
/// Used by migration/cleanup to avoid expensive `count(*)` on every chunk.
/// The count SQL should count only remaining rows (e.g. with a WHERE clause).
pub unsafe fn get_or_count(
    db: *mut sqlite3,
    cache_key: &str,
    count_sql: &str,
) -> Result<i64, ResultCode> {
    match get_master_value(db, cache_key)? {
        Some(v) => Ok(v),
        None => {
            let stmt = db.prepare_v2(count_sql)?;
            stmt.step()?;
            let total = stmt.column_int64(0);
            set_master_value(db, cache_key, total)?;
            Ok(total)
        }
    }
}

/// Set an integer value in crsql_master by exact key (insert or replace).
pub unsafe fn set_master_value(db: *mut sqlite3, key: &str, value: i64) -> Result<(), ResultCode> {
    let sql = "INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, ?)";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, Destructor::STATIC)?;
    stmt.bind_int64(2, value)?;
    stmt.step()?;
    Ok(())
}

/// Delete a key from crsql_master by exact key.
pub unsafe fn clear_master_key(db: *mut sqlite3, key: &str) -> Result<(), ResultCode> {
    let sql = "DELETE FROM crsql_master WHERE key = ?";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, Destructor::STATIC)?;
    stmt.step()?;
    Ok(())
}

/// Clear all crsql_master mode flags for a table (use_rowid, skip_hash, v2_pks).
pub unsafe fn clear_crr_mode_flags(db: *mut sqlite3, table: &str) {
    let _ = clear_master_key(db, &format!("use_rowid_{}", table));
    let _ = clear_master_key(db, &format!("skip_hash_{}", table));
    let _ = clear_master_key(db, &format!("v2_pks_{}", table));
}

/// Clear ALL per-table crsql_master keys for a table: mode flags, cleanup task
/// markers, and migration task markers. This prevents stale markers from being
/// processed by incremental_maintenance after a teardown, which would drop
/// freshly re-created CRR metadata tables.
pub unsafe fn clear_all_per_table_master_keys(db: *mut sqlite3, table: &str) {
    // Mode flags
    let _ = clear_master_key(db, &format!("use_rowid_{}", table));
    let _ = clear_master_key(db, &format!("skip_hash_{}", table));
    let _ = clear_master_key(db, &format!("v2_pks_{}", table));
    // Cleanup task markers
    let _ = clear_master_key(db, &format!("cleanup_v1_tables_{}", table));
    let _ = clear_master_key(db, &format!("cleanup_v2_tables_{}", table));
    let _ = clear_master_key(db, &format!("cleanup_remaining_{}", table));
    // Migration task markers
    let _ = clear_master_key(db, &format!("migration_v1_to_v2_migration_{}", table));
    let _ = clear_master_key(db, &format!("migration_v1_to_v2_remaining_{}", table));
}

/// Get a text value from crsql_master by exact key.
/// Returns None if the key does not exist.
pub unsafe fn get_master_text_value(db: *mut sqlite3, key: &str) -> Result<Option<alloc::string::String>, ResultCode> {
    let sql = "SELECT value FROM crsql_master WHERE key = ?";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        return Ok(Some(stmt.column_text(0)?.to_string()));
    }
    Ok(None)
}

/// Set a text value in crsql_master by exact key (insert or replace).
pub unsafe fn set_master_text_value(db: *mut sqlite3, key: &str, value: &str) -> Result<(), ResultCode> {
    let sql = "INSERT OR REPLACE INTO crsql_master (key, value) VALUES (?, ?)";
    let stmt = db.prepare_v2(sql)?;
    stmt.bind_text(1, key, Destructor::TRANSIENT)?;
    stmt.bind_text(2, value, Destructor::TRANSIENT)?;
    stmt.step()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slab_rowid() {
        let foo_slab = slab_rowid(0, 1);
        let bar_slab = slab_rowid(1, 2);
        let baz_slab = slab_rowid(2, 3);

        assert_eq!(foo_slab, 1);
        assert_eq!(bar_slab, 2 + crate::consts::ROWID_SLAB_SIZE);
        assert_eq!(baz_slab, 3 + crate::consts::ROWID_SLAB_SIZE * 2);
        assert_eq!(slab_rowid(0, crate::consts::ROWID_SLAB_SIZE), 0);
        assert_eq!(slab_rowid(0, crate::consts::ROWID_SLAB_SIZE + 1), 1);

        let foo_slab = slab_rowid(0, crate::consts::ROWID_SLAB_SIZE + 1);
        let bar_slab = slab_rowid(1, crate::consts::ROWID_SLAB_SIZE + 2);
        let baz_slab = slab_rowid(2, crate::consts::ROWID_SLAB_SIZE * 2 + 3);

        assert_eq!(foo_slab, 1);
        assert_eq!(bar_slab, 2 + crate::consts::ROWID_SLAB_SIZE);
        assert_eq!(baz_slab, 3 + crate::consts::ROWID_SLAB_SIZE * 2);
    }

    #[test]
    fn test_get_db_version_union_query() {
        let tbl_names = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
        let union = get_db_version_union_query(&tbl_names);
        assert_eq!(
            union,
            "SELECT max(version) as version FROM (SELECT max(db_version) as version FROM \"foo\" WHERE site_id = 0 UNION ALL SELECT max(db_version) as version FROM \"bar\" WHERE site_id = 0 UNION ALL SELECT max(db_version) as version FROM \"baz\" WHERE site_id = 0 UNION SELECT value as\n        version FROM crsql_master WHERE key = 'pre_compact_dbversion')"
        );
    }

    #[test]
    fn test_get_db_version_union_query_empty() {
        // Empty table list: fallback to crsql_db_versions filtered by site_id
        let union = get_db_version_union_query(&[]);
        assert_eq!(
            union,
            "SELECT max(db_version) as version FROM crsql_db_versions WHERE site_id = ?"
        );
    }
}
