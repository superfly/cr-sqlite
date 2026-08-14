extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, Destructor, ResultCode};

use crate::consts;
use crate::hash_pk::hash_pk_values;
use crate::tableinfo::ColumnInfo;

/// Backfill V2 tables for a newly registered V2 CRR.
/// For each row in the main table not yet in v2_pks:
/// 1. Compute hashed_pk from PK values.
/// 2. INSERT into v2_pks with cl=1.
/// 3. For each non-PK column, INSERT into v2_clock.
pub fn backfill_table_v2(
    db: *mut sqlite3,
    table: &str,
    pk_cols: &Vec<ColumnInfo>,
    non_pk_cols: &Vec<ColumnInfo>,
    key_is_rowid: bool,
    rowid_alias: &str,
    skip_hash: bool,
    no_tx: bool,
) -> Result<ResultCode, ResultCode> {
    // V2 clock tables require a non-zero ts. Error early if not set.
    let ts_check = db.prepare_v2("SELECT crsql_get_ts()\0");
    if let Ok(stmt) = ts_check {
        if stmt.step().is_err() {
            crate::debug::debug_log("backfill_table_v2: timestamp not set — call crsql_set_ts() first");
            return Err(ResultCode::ERROR);
        }
    }

    if !no_tx {
        db.exec_safe("SAVEPOINT backfill_v2")?;
    }

    let escaped = crate::util::escape_ident(table);
    let pk_cols_list = pk_cols.iter()
        .map(|f| format!("\"{}\"", crate::util::escape_ident(&f.name)))
        .collect::<Vec<_>>()
        .join(", ");

    // Find rows in main table not yet in v2_pks
    // For rowid-key tables, compare rowid alias vs __crsql_key, but also select PK cols for hashing
    let sql = if key_is_rowid {
        format!(
            "SELECT t1.\"{alias}\", {pk_cols} FROM \"{table}\" AS t1
            WHERE t1.\"{alias}\" NOT IN (SELECT __crsql_key FROM \"{table}{pks_suffix}\")",
            alias = crate::util::escape_ident(rowid_alias),
            table = escaped,
            pk_cols = pk_cols_list,
            pks_suffix = consts::V2_PKS_SUFFIX,
        )
    } else {
        format!(
            "SELECT {pk_cols} FROM \"{table}\" AS t1
            EXCEPT SELECT {pk_cols} FROM \"{table}{pks_suffix}\" AS t2",
            table = escaped,
            pk_cols = pk_cols_list,
            pks_suffix = consts::V2_PKS_SUFFIX,
        )
    };
    let read_stmt = db.prepare_v2(&sql)?;

    // Prepare insert into v2_pks
    let insert_pks_sql = if skip_hash && key_is_rowid {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, cl) VALUES (?, 1) RETURNING __crsql_key",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
        )
    } else if skip_hash && !key_is_rowid {
        // skip_hash, non-rowid: store PK column, no hashed_pk
        format!(
            "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, cl) VALUES ({pk_values}, 1) RETURNING __crsql_key",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
            pk_cols = pk_cols_list,
            pk_values = pk_cols.iter().map(|_| "?").collect::<Vec<_>>().join(", "),
        )
    } else if key_is_rowid {
        format!(
            "INSERT INTO \"{escaped}{suffix}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, 1) RETURNING __crsql_key",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
        )
    } else {
        let pk_values = pk_cols.iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, hashed_pk, cl) VALUES ({pk_values}, ?, 1) RETURNING __crsql_key",
            escaped = escaped,
            suffix = consts::V2_PKS_SUFFIX,
            pk_cols = pk_cols_list,
            pk_values = pk_values,
        )
    };
    let insert_pks_stmt = db.prepare_v2(&insert_pks_sql)?;

    // Prepare insert into v2_clock
    let clock_sql = format!(
        "INSERT OR REPLACE INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) VALUES (?, 1, 0, crsql_next_db_version(), crsql_increment_and_get_seq(), crsql_get_ts())",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX
    );
    let clock_stmt = db.prepare_v2(&clock_sql)?;

    let result = (|| {
        while read_stmt.step()? == ResultCode::ROW {
            // For rowid-key tables: col 0 = rowid, cols 1..n = PK columns
            // For non-rowid tables: cols 0..n = PK columns
            let (rowid, pk_values) = if key_is_rowid {
                let r = read_stmt.column_int64(0);
                let pks: Vec<*mut sqlite::value> = (0..pk_cols.len())
                    .map(|i| read_stmt.column_value(i as i32 + 1))
                    .collect::<Result<Vec<_>, _>>()?;
                (r, pks)
            } else {
                let pks: Vec<*mut sqlite::value> = (0..pk_cols.len())
                    .map(|i| read_stmt.column_value(i as i32))
                    .collect::<Result<Vec<_>, _>>()?;
                (0i64, pks)
            };

            // Compute hashed_pk from PK values (only for hash mode)
            let hashed_pk = if !skip_hash {
                Some(hash_pk_values(&pk_values)?)
            } else {
                None
            };

            // Insert into v2_pks
            if skip_hash && key_is_rowid {
                // __crsql_key = rowid, no hashed_pk
                insert_pks_stmt.bind_int64(1, rowid)?;
            } else if skip_hash && !key_is_rowid {
                // PK column only, no hashed_pk
                for (i, val) in pk_values.iter().enumerate() {
                    insert_pks_stmt.bind_value(i as i32 + 1, *val)?;
                }
            } else if key_is_rowid {
                // __crsql_key = rowid, hashed_pk = hash(PK values)
                insert_pks_stmt.bind_int64(1, rowid)?;
                insert_pks_stmt.bind_blob(2, hashed_pk.as_ref().unwrap(), Destructor::STATIC)?;
            } else {
                for (i, val) in pk_values.iter().enumerate() {
                    insert_pks_stmt.bind_value(i as i32 + 1, *val)?;
                }
                insert_pks_stmt.bind_blob(pk_values.len() as i32 + 1, hashed_pk.as_ref().unwrap(), Destructor::STATIC)?;
            }

            match insert_pks_stmt.step()? {
                ResultCode::ROW => {
                    let key = insert_pks_stmt.column_int64(0);
                    insert_pks_stmt.reset()?;

                    // Insert clock entries for each non-PK column
                    for (col_id, _col) in non_pk_cols.iter().enumerate() {
                        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | col_id as i64;
                        clock_stmt.bind_int64(1, cell_key)?;
                        clock_stmt.step()?;
                        clock_stmt.reset()?;
                    }

                    // For PK-only tables, write a sentinel clock entry at col_id=0
                    if non_pk_cols.is_empty() {
                        let cell_key = (key << consts::CRSQL_COL_ID_BITS as i64) | 0;
                        clock_stmt.bind_int64(1, cell_key)?;
                        clock_stmt.step()?;
                        clock_stmt.reset()?;
                    }
                }
                _ => {
                    insert_pks_stmt.reset()?;
                }
            }
        }
        Ok(ResultCode::OK)
    })();

    read_stmt.reset()?;

    if let Err(e) = result {
        if !no_tx {
            db.exec_safe("ROLLBACK")?;
        }
        return Err(e);
    }

    if !no_tx {
        db.exec_safe("RELEASE backfill_v2")
    } else {
        Ok(ResultCode::OK)
    }
}
