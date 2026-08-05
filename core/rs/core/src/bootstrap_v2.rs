extern crate alloc;

use alloc::format;
use alloc::string::String;

use sqlite_nostd as sqlite;
use sqlite_nostd::{Connection, ResultCode};

use crate::tableinfo::TableInfo;
use crate::consts;

/// Create all V2 metadata tables for a given CRR table.
/// Tables created:
/// 1. <table>__crsql_v2_col_map — col_name → col_id mapping
/// 2. <table>__crsql_v2_clock — packed clock (cells only)
/// 3. <table>__crsql_v2_pks — alive PKs with hash + CL
/// 4. <table>__crsql_v2_tombstones — dead PKs with hash only
/// 5. <table>__crsql_v2_tombstone_pks — hash → original PK cols (V1 compat)
pub fn create_v2_tables(
    db: *mut sqlite::sqlite3,
    table_info: &TableInfo,
) -> Result<ResultCode, ResultCode> {
    let table_name = &table_info.tbl_name;
    let escaped = crate::util::escape_ident(table_name);

    // 1. Column map
    db.exec_safe(&format!(
        "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
          col_id INTEGER PRIMARY KEY,
          col_name TEXT NOT NULL
        ) STRICT;",
        escaped = escaped,
        suffix = consts::V2_COL_MAP_SUFFIX
    ))?;

    db.exec_safe(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS \"idx_{escaped}_v2_col_map_name\"
          ON \"{escaped}{suffix}\"(col_name);",
        escaped = escaped,
        suffix = consts::V2_COL_MAP_SUFFIX
    ))?;

    // 2. Clock table
    db.exec_safe(&format!(
        "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
          cell_key INTEGER PRIMARY KEY,
          col_version INTEGER NOT NULL,
          site_id INTEGER NOT NULL,
          db_version INTEGER NOT NULL,
          seq INTEGER NOT NULL,
          ts INTEGER NOT NULL CHECK (ts > 0)
        ) STRICT;",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    ))?;

    db.exec_safe(&format!(
        "CREATE INDEX IF NOT EXISTS \"{escaped}{suffix}_feed_idx\"
          ON \"{escaped}{suffix}\"(site_id, db_version, seq);",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    ))?;

    // 3. Alive PKs
    // If uses_rowid_key (single INTEGER PRIMARY KEY on rowid table), __crsql_key = rowid.
    // No need to store PK columns separately since rowid IS the PK.
    // STRICT always used. For non-rowid tables, dynamic PK columns use ANY type
    // (https://www.sqlite.org/stricttables.html) so STRICT works regardless of base table type.
    if table_info.uses_rowid_key {
        db.exec_safe(&format!(
            "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
              __crsql_key INTEGER PRIMARY KEY,
              hashed_pk BLOB NOT NULL,
              cl INTEGER NOT NULL DEFAULT 1 CHECK (cl % 2 = 1)
            ) STRICT;",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
        ))?;
    } else {
        let mut pk_cols_sql = String::new();
        for (i, pk) in table_info.pks.iter().enumerate() {
            if i > 0 { pk_cols_sql.push_str(", "); }
            pk_cols_sql.push_str(&format!(
                "\"{}\" ANY NOT NULL",
                crate::util::escape_ident(&pk.name)
            ));
        }

        let create_sql = format!(
            "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
              __crsql_key INTEGER PRIMARY KEY,
              {pk_cols},
              hashed_pk BLOB NOT NULL,
              cl INTEGER NOT NULL DEFAULT 1 CHECK (cl % 2 = 1)
            ) STRICT;",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
            pk_cols = pk_cols_sql
        );
        db.exec_safe(&create_sql)?;
    }

    let index_sql = format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS \"idx_{escaped}_v2_pks_hash\"
          ON \"{escaped}{suffix}\"(hashed_pk);",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    );
    db.exec_safe(&index_sql)?;

    // 4. Tombstones
    db.exec_safe(&format!(
        "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
          site_id INTEGER NOT NULL,
          db_version INTEGER NOT NULL,
          seq INTEGER NOT NULL,
          hashed_pk BLOB NOT NULL,
          cl INTEGER NOT NULL CHECK (cl % 2 = 0),
          ts INTEGER NOT NULL CHECK (ts > 0),
          PRIMARY KEY (site_id, db_version, seq)
        ) WITHOUT ROWID, STRICT;",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONES_SUFFIX
    ))?;

    db.exec_safe(&format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS \"idx_{escaped}_v2_tombstones_hash\"
          ON \"{escaped}{suffix}\"(hashed_pk);",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONES_SUFFIX
    ))?;

    // 5. Tombstone PKs (V1 compat)
    // STRICT always; dynamic PK columns use ANY type.
    let mut tombstone_pk_cols_sql = String::new();
    for (i, pk) in table_info.pks.iter().enumerate() {
        if i > 0 { tombstone_pk_cols_sql.push_str(", "); }
        tombstone_pk_cols_sql.push_str(&format!(
            "\"{}\" ANY NOT NULL",
            crate::util::escape_ident(&pk.name)
        ));
    }

    let tombstone_pks_sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{escaped}{suffix}\" (
          hashed_pk BLOB PRIMARY KEY,
          {pk_cols}
        ) WITHOUT ROWID, STRICT;",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONE_PKS_SUFFIX,
        pk_cols = tombstone_pk_cols_sql
    );
    db.exec_safe(&tombstone_pks_sql)?;

    // Populate col_map with existing non-PK columns
    populate_col_map(db, table_info)?;

    Ok(ResultCode::OK)
}

/// Populate the col_map table with column names from the table info.
/// Assigns 0-based col_id to each non-PK column.
fn populate_col_map(
    db: *mut sqlite::sqlite3,
    table_info: &TableInfo,
) -> Result<ResultCode, ResultCode> {
    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let stmt = db.prepare_v2(&format!(
        "INSERT OR IGNORE INTO \"{escaped}{suffix}\" (col_id, col_name) VALUES (?, ?)",
        escaped = escaped,
        suffix = consts::V2_COL_MAP_SUFFIX
    ))?;

    for (i, col) in table_info.non_pks.iter().enumerate() {
        stmt.bind_int(1, i as i32)?;
        stmt.bind_text(2, &col.name, sqlite::Destructor::STATIC)?;
        stmt.step()?;
        stmt.reset()?;
    }

    Ok(ResultCode::OK)
}

/// Drop all V2 metadata tables for a given table.
pub fn drop_v2_tables(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<ResultCode, ResultCode> {
    let escaped = crate::util::escape_ident(table);

    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{escaped}{suffix}\"",
        escaped = escaped,
        suffix = consts::V2_COL_MAP_SUFFIX
    ))?;
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{escaped}{suffix}\"",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    ))?;
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{escaped}{suffix}\"",
        escaped = escaped,
        suffix = consts::V2_PKS_SUFFIX
    ))?;
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{escaped}{suffix}\"",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONES_SUFFIX
    ))?;
    db.exec_safe(&format!(
        "DROP TABLE IF EXISTS \"{escaped}{suffix}\"",
        escaped = escaped,
        suffix = consts::V2_TOMBSTONE_PKS_SUFFIX
    ))
}

/// Check if V2 tables exist for a given table.
pub fn has_v2_tables(
    db: *mut sqlite::sqlite3,
    table: &str,
) -> Result<bool, ResultCode> {
    let stmt = db.prepare_v2(&format!(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND tbl_name = '{escaped}{suffix}'",
        escaped = crate::util::escape_ident_as_value(table),
        suffix = consts::V2_CLOCK_SUFFIX
    ))?;
    let rc = stmt.step()?;
    Ok(rc == ResultCode::ROW)
}
