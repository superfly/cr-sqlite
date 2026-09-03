extern crate alloc;
use crate::c::CrsqlChangesColumn;
use crate::tableinfo::{TableInfo, SchemaVersion};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_char;
use sqlite::ResultCode;

use sqlite_nostd as sqlite;

use crate::consts;
use crate::changes_vtab::{read_idx_plan, PlanConstraint};

/// A constraint that will be pushed into subqueries:
/// (column, operator, param_index). e.g., (Ts, ">=", 2) means `ts >= ?2`.
/// Uses CrsqlChangesColumn directly — Pk and Cval won't appear here because
/// they're filtered out by the partition in changes_union_query.
struct PushedConstraint {
    col: CrsqlChangesColumn,
    op: String,
    param_index: i32,
}

/// Convert a SQLITE_INDEX_CONSTRAINT_* value to its SQL operator string.
/// Returns None for unknown operators — callers should skip such constraints
/// rather than silently defaulting to "=".
fn op_id_to_str(op_id: u8) -> Option<&'static str> {
    match op_id as u32 {
        sqlite::INDEX_CONSTRAINT_EQ => Some("="),
        sqlite::INDEX_CONSTRAINT_GT => Some(">"),
        sqlite::INDEX_CONSTRAINT_LE => Some("<="),
        sqlite::INDEX_CONSTRAINT_LT => Some("<"),
        sqlite::INDEX_CONSTRAINT_GE => Some(">="),
        sqlite::INDEX_CONSTRAINT_MATCH => Some("MATCH"),
        sqlite::INDEX_CONSTRAINT_LIKE => Some("LIKE"),
        sqlite::INDEX_CONSTRAINT_GLOB => Some("GLOB"),
        sqlite::INDEX_CONSTRAINT_REGEXP => Some("REGEXP"),
        sqlite::INDEX_CONSTRAINT_NE => Some("!="),
        sqlite::INDEX_CONSTRAINT_ISNOT => Some("IS NOT"),
        sqlite::INDEX_CONSTRAINT_ISNOTNULL => Some("IS NOT NULL"),
        sqlite::INDEX_CONSTRAINT_ISNULL => Some("IS NULL"),
        sqlite::INDEX_CONSTRAINT_IS => Some("IS"),
        _ => None,
    }
}

/// Returns true if the operator is LIKE/MATCH/GLOB/REGEXP.
fn is_pattern_op_id(op_id: u8) -> bool {
    matches!(op_id as u32,
        sqlite::INDEX_CONSTRAINT_LIKE
        | sqlite::INDEX_CONSTRAINT_MATCH
        | sqlite::INDEX_CONSTRAINT_GLOB
        | sqlite::INDEX_CONSTRAINT_REGEXP
    )
}

/// Convert a CrsqlChangesColumn to the column name used in SQL.
fn col_to_name(col: CrsqlChangesColumn) -> Option<&'static str> {
    match col {
        CrsqlChangesColumn::Tbl => Some("tbl"),
        CrsqlChangesColumn::Cid => Some("cid"),
        CrsqlChangesColumn::ColVrsn => Some("col_vrsn"),
        CrsqlChangesColumn::DbVrsn => Some("db_vrsn"),
        CrsqlChangesColumn::SiteId => Some("site_id"),
        CrsqlChangesColumn::Cl => Some("cl"),
        CrsqlChangesColumn::Seq => Some("seq"),
        CrsqlChangesColumn::Ts => Some("ts"),
        // Pk and Cval are not usable as constraints or order-by columns
        CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval => None,
    }
}

/// Build the SQL WHERE clause text for a constraint (for non-packed mode
/// or for the outer WHERE in packed mode).
fn constraint_to_sql(c: &PlanConstraint) -> Option<String> {
    let col_name = col_to_name(c.col)?;
    let op_str = op_id_to_str(c.op_id)?;
    if c.param_idx == 0 {
        // IS NULL / IS NOT NULL — no parameter
        Some(format!("{} {}", col_name, op_str))
    } else {
        Some(format!("{} {} ?{}", col_name, op_str, c.param_idx))
    }
}

/// Resolve a pushed constraint column to its source-column reference.
/// Clock arms alias the clock table as `c`, tombstone arms as `t`;
/// `site_id` comes from the `site_tbl` join present in both arm shapes.
fn pushed_col_ref(col: CrsqlChangesColumn, clock: bool) -> Option<&'static str> {
    match col {
        CrsqlChangesColumn::Seq => {
            if clock { Some("c.seq") } else { Some("t.seq") }
        }
        CrsqlChangesColumn::DbVrsn => {
            if clock { Some("c.db_version") } else { Some("t.db_version") }
        }
        CrsqlChangesColumn::SiteId => Some("site_tbl.site_id"),
        CrsqlChangesColumn::Ts => {
            if clock { Some("c.ts") } else { Some("t.ts") }
        }
        CrsqlChangesColumn::Cl => {
            if clock { Some("pk_tbl.cl") } else { Some("t.cl") }
        }
        // Cid, ColVrsn, Tbl are handled directly in build_pushed_where
        // since their source expression depends on arm-specific context.
        // Pk, Cval are never pushed.
        CrsqlChangesColumn::Cid | CrsqlChangesColumn::ColVrsn | CrsqlChangesColumn::Tbl
        | CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval => None,
    }
}

/// Build a WHERE fragment from pushed constraints for one arm kind.
/// Returns e.g. "c.seq >= ?2 AND c.db_version >= ?1" or empty if none.
/// `cid_expr` / `col_vrsn_expr` / `tbl_expr` provide the source expressions
/// for cid, col_vrsn, and tbl in this specific arm. For tbl, each arm emits
/// a literal table name (e.g. "'foo'") so the comparison is constant-foldable.
/// When None, the constraint is skipped — the outer WHERE handles it.
/// When "NULL", the expression is emitted as-is so NULL <op> ?N evaluates
/// to false, pruning rows that don't have a meaningful value for this column
/// (e.g. col_vrsn in tombstone arms).
fn build_pushed_where(
    constraints: &[PushedConstraint],
    clock: bool,
    cid_expr: Option<&str>,
    col_vrsn_expr: Option<&str>,
    tbl_expr: Option<&str>,
) -> String {
    constraints
        .iter()
        .filter_map(|c| {
            let col_ref = match c.col {
                CrsqlChangesColumn::Cid => cid_expr.map(|s| s.to_string()),
                CrsqlChangesColumn::ColVrsn => col_vrsn_expr.map(|s| s.to_string()),
                CrsqlChangesColumn::Tbl => tbl_expr.map(|s| s.to_string()),
                _ => pushed_col_ref(c.col, clock).map(|s| s.to_string()),
            };
            col_ref.map(|col| format!("{} {} ?{}", col, c.op, c.param_index))
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Rebuild the outer idx_str with non-pushed WHERE clauses and _seq_order in ORDER BY.
fn rebuild_outer_idx_str(
    other_clauses: &[&PlanConstraint],
    order_by_cols: &[CrsqlChangesColumn],
    order_by_desc: bool,
    has_order_by: bool,
) -> String {
    let mut str = String::new();

    if !other_clauses.is_empty() {
        let clauses: Vec<String> = other_clauses
            .iter()
            .filter_map(|c| constraint_to_sql(c))
            .collect();
        if !clauses.is_empty() {
            str.push_str("WHERE ");
            str.push_str(&clauses.join(" AND "));
        }
    }

    // ORDER BY: replace seq with _seq_order for packed mode
    if has_order_by && !order_by_cols.is_empty() {
        str.push_str(" ORDER BY ");
        let suffix = if order_by_desc { " DESC" } else { " ASC" };
        let cols: Vec<String> = order_by_cols
            .iter()
            .map(|&c| {
                let name = if c == CrsqlChangesColumn::Seq {
                    "_seq_order"
                } else {
                    col_to_name(c).unwrap_or("db_vrsn")
                };
                format!("{}{}", name, suffix)
            })
            .collect();
        str.push_str(&cols.join(", "));
    } else {
        // Default ordering
        str.push_str(" ORDER BY db_vrsn ASC, _seq_order ASC");
    }

    str
}

/// Build the skip_hash tombstone SELECT query for feed operations.
/// `col_vrsn_expr` is "t.cl" for v1wire/pkonly or "NULL" for v2wire.
/// `pushed_where` is an optional WHERE fragment on scalar t.* columns
/// (seq / db_version / site_id / ts) pushed down for packed-mode filtering.
/// `need_seq_order` adds a scalar _seq_order column for outer ORDER BY.
fn skip_hash_tombstone_query(
    table_info: &TableInfo,
    escaped: &str,
    table_name_val: &str,
    col_vrsn_expr: &str,
    pushed_where: &str,
    need_seq_order: bool,
) -> String {
    let seq_order_col = if need_seq_order { ", t.seq as _seq_order" } else { "" };
    let where_clause = if pushed_where.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", pushed_where)
    };
    format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns(t.\"{pk_col}\") as pks,
          '{delete_sentinel}' as cid,
          {col_vrsn} as col_vrsn,
          t.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          (1 << 62) | (t.site_id << 46) | (t.db_version << 22) | t.seq as key,
          t.seq as seq,
          t.cl as cl,
          t.ts as ts,
          NULL as cval{seq_order_col}
        FROM \"{escaped}{tomb_suffix}\" AS t
        LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal{where_clause}",
        table_name_val = table_name_val,
        pk_col = table_info.skip_hash_pk_col,
        delete_sentinel = crate::c::DELETE_SENTINEL,
        col_vrsn = col_vrsn_expr,
        seq_order_col = seq_order_col,
        where_clause = where_clause,
        escaped = escaped,
        tomb_suffix = consts::V2_TOMBSTONES_SUFFIX,
    )
}

// Metadata version constants are in consts.rs

/// Build the PK expression and main table JOIN clause for V2 feed queries.
/// Returns (pk_expr, main_join) where:
/// - pk_expr: the PK column list for crsql_pack_columns (e.g., "mt.col1, mt.col2" or "pk_tbl.col1, ...")
/// - main_join: the JOIN clause to the base table (needed for cval fetch via CASE)
///
/// For rowid-key tables: join on rowid alias = pk_tbl.__crsql_key.
/// For non-rowid tables: join on PK columns.
fn build_pk_expr_and_join(table_info: &TableInfo, escaped: &str) -> Result<(String, String), ResultCode> {
    if table_info.key_is_rowid {
        let mt_pk_list = crate::util::as_identifier_list(&table_info.pks, Some("mt."))?;
        let alias = crate::util::escape_ident(&table_info.rowid_alias);
        Ok((mt_pk_list, format!("JOIN \"{escaped}\" AS mt ON mt.\"{alias}\" = pk_tbl.__crsql_key", alias = alias)))
    } else {
        let pk_list = crate::util::as_identifier_list(&table_info.pks, Some("pk_tbl."))?;
        let pk_join_conds: Vec<String> = table_info.pks.iter().map(|c| {
            format!("mt.\"{col}\" = pk_tbl.\"{col}\"", col = crate::util::escape_ident(&c.name))
        }).collect();
        Ok((pk_list, format!("JOIN \"{escaped}\" AS mt ON {conds}", conds = pk_join_conds.join(" AND "))))
    }
}

/// Build the hash-mode tombstone SELECT for V2 feed queries.
/// Used by v1wire and pkonly query builders (skip_hash uses skip_hash_tombstone_query instead).
/// `pushed_where` is an optional WHERE fragment on scalar t.* columns
/// (seq / db_version / site_id / ts) pushed down for packed-mode filtering.
fn hash_tombstone_query(
    table_info: &TableInfo,
    escaped: &str,
    table_name_val: &str,
    pk_list_tomb: &str,
    col_vrsn_expr: &str,
    pushed_where: &str,
    need_seq_order: bool,
) -> String {
    let seq_order_col = if need_seq_order { ", t.seq as _seq_order" } else { "" };
    let where_clause = if pushed_where.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", pushed_where)
    };
    format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns({pk_list_tomb}) as pks,
          '{delete_sentinel}' as cid,
          {col_vrsn} as col_vrsn,
          t.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          (1 << 62) | (t.site_id << 46) | (t.db_version << 22) | t.seq as key,
          t.seq as seq,
          t.cl as cl,
          t.ts as ts,
          NULL as cval{seq_order_col}
        FROM \"{escaped}{tomb_suffix}\" AS t
        JOIN \"{escaped}{tomb_pks_suffix}\" AS tpk_tbl ON t.hashed_pk = tpk_tbl.hashed_pk
        LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal{where_clause}",
        table_name_val = table_name_val,
        pk_list_tomb = pk_list_tomb,
        delete_sentinel = crate::c::DELETE_SENTINEL,
        col_vrsn = col_vrsn_expr,
        seq_order_col = seq_order_col,
        where_clause = where_clause,
        escaped = escaped,
        tomb_suffix = consts::V2_TOMBSTONES_SUFFIX,
        tomb_pks_suffix = consts::V2_TOMBSTONE_PKS_SUFFIX,
    )
}

fn crsql_changes_query_for_table(table_info: &TableInfo) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let pk_list = crate::util::as_identifier_list(&table_info.pks, Some("pk_tbl."))?;

    Ok(format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns({pk_list}) as pks,
          t1.col_name as cid,
          t1.col_version as col_vrsn,
          t1.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          t1.key,
          t1.seq as seq,
          COALESCE(t2.col_version, 1) as cl,
          t1.ts as ts
      FROM \"{table_name_ident}__crsql_clock\" AS t1
      JOIN \"{table_name_ident}__crsql_pks\" AS pk_tbl ON t1.key = pk_tbl.__crsql_key
      LEFT JOIN crsql_site_id AS site_tbl ON t1.site_id = site_tbl.ordinal
      LEFT JOIN \"{table_name_ident}__crsql_clock\" AS t2 ON
      t1.key = t2.key AND t2.col_name = '{sentinel}'",
        table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name),
        pk_list = pk_list,
        table_name_ident = crate::util::escape_ident(&table_info.tbl_name),
        sentinel = crate::c::INSERT_SENTINEL,
    ))
}

/// V2 feed query (per-column / V1 wire format): reads from V2 metadata tables
/// and produces the same per-column output schema as V1.
/// Two parts UNIONed together:
/// 1. Cell changes from v2_clock (column-level changes for alive rows)
/// 2. Tombstone rows from v2_tombstones (delete events)
fn crsql_changes_query_for_table_v2_v1wire(table_info: &TableInfo) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let pk_list_tomb = crate::util::as_identifier_list(&table_info.pks, Some("tpk_tbl."))?;
    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;

    let (pk_expr, main_join) = build_pk_expr_and_join(table_info, &escaped)?;
    let col_val_case = build_col_val_case(table_info)?;

    // Part 1: Cell changes from v2_clock joined with v2_pks and v2_col_map
    // cval is fetched inline via CASE expression — no lazy per-row fetch needed.
    let cell_changes = format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns({pk_expr}) as pks,
          cm.col_name as cid,
          c.col_version as col_vrsn,
          c.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          c.cell_key >> {col_id_bits} as key,
          c.seq as seq,
          pk_tbl.cl as cl,
          c.ts as ts,
          {col_val_case} as cval
        FROM \"{escaped}{clock_suffix}\" AS c
        JOIN \"{escaped}{pks_suffix}\" AS pk_tbl ON (c.cell_key >> {col_id_bits}) = pk_tbl.__crsql_key
        {main_join}
        JOIN \"{escaped}{col_map_suffix}\" AS cm ON (c.cell_key & {col_id_mask}) = cm.col_id
        LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal",
        table_name_val = table_name_val,
        pk_expr = pk_expr,
        main_join = main_join,
        col_val_case = col_val_case,
        escaped = escaped,
        clock_suffix = consts::V2_CLOCK_SUFFIX,
        pks_suffix = consts::V2_PKS_SUFFIX,
        col_map_suffix = consts::V2_COL_MAP_SUFFIX,
        col_id_bits = col_id_bits,
        col_id_mask = consts::CRSQL_COL_ID_MASK,
    );

    // Part 2: Tombstone rows
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "t.cl", "", false)
    } else {
        hash_tombstone_query(table_info, &escaped, &table_name_val, &pk_list_tomb, "t.cl", "", false)
    };

    Ok(format!(
        "{cell_changes} UNION ALL {tombstone_rows}",
        cell_changes = cell_changes,
        tombstone_rows = tombstone_rows,
    ))
}

/// Build a CASE expression that maps col_id → main_table column value.
/// Used in packed (v2 sync-log) format to fetch all column values in one query.
/// Example: CASE (c.cell_key & mask) WHEN 0 THEN mt."col0" WHEN 1 THEN mt."col1" END
fn build_col_val_case(table_info: &TableInfo) -> Result<String, ResultCode> {
    let mut when_clauses = vec![];
    for (_, col) in table_info.non_pks.iter().enumerate() {
        when_clauses.push(format!(
            "WHEN '{col_name}' THEN mt.\"{col_name}\"",
            col_name = crate::util::escape_ident(&col.name)
        ));
    }

    if when_clauses.is_empty() {
        // No non-PK columns — return NULL
        Ok("NULL".to_string())
    } else {
        Ok(format!(
            "CASE cm.col_name {whens} END",
            whens = when_clauses.join(" ")
        ))
    }
}

/// V2 packed feed query (V2 wire format): coalesces clock rows that share
/// (row_key, db_version, site_id) into a single packed event.
/// - cid = GROUP_CONCAT(col_name, char(0))  (text — column names are strings)
/// - col_vrsn = crsql_pack_varint_agg(col_version)  (binary varint array)
/// - seq = crsql_pack_varint_agg(c.seq)  (binary varint array)
/// - cval = crsql_pack_agg(col_val)  (column values fetched from main table via CASE)
/// Sentinels and tombstones are always single events (no packing).
fn crsql_changes_query_for_table_v2_v2wire(
    table_info: &TableInfo,
    pushed: &[PushedConstraint],
    need_seq_order: bool,
) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;
    let col_val_case = build_col_val_case(table_info)?;

    let (pk_expr, main_join) = build_pk_expr_and_join(table_info, &escaped)?;

    // Pushed constraints are applied to scalar source rows before GROUP BY.
    // tbl is a literal per-arm constant — SQLite constant-folds the comparison
    // and skips non-matching arms entirely.
    let tbl_expr = format!("'{}'", table_name_val);
    let cell_pushed_where = build_pushed_where(pushed, true, Some("cm.col_name"), Some("c.col_version"), Some(&tbl_expr));
    let cell_where = if cell_pushed_where.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", cell_pushed_where)
    };
    // _seq_order = MIN(c.seq) gives a scalar for outer ORDER BY.
    let seq_order_col = if need_seq_order { ", MIN(c.seq) as _seq_order" } else { "" };

    // Part 1: Packed cell changes — GROUP BY (key expression, db_version, site_id)
    // No subquery needed with SQLite 3.44+ — ORDER BY inside aggregates ensures alignment.
    // GROUP BY must use the full expression, not the column alias, for SQLite 3.44 compatibility.
    let cell_changes = format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns({pk_expr}) as pks,
          cast(group_concat(cm.col_name, char(0) ORDER BY cm.col_id) as blob) as cid,
          crsql_pack_varint_agg(c.col_version ORDER BY cm.col_id) as col_vrsn,
          c.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          c.cell_key >> {col_id_bits} as key,
          crsql_pack_varint_agg(c.seq ORDER BY cm.col_id) as seq,
          pk_tbl.cl as cl,
          c.ts as ts,
          crsql_pack_agg(({col_val_case}) ORDER BY cm.col_id) as cval{seq_order_col}
        FROM \"{escaped}{clock_suffix}\" AS c
        JOIN \"{escaped}{pks_suffix}\" AS pk_tbl ON (c.cell_key >> {col_id_bits}) = pk_tbl.__crsql_key
        {main_join}
        JOIN \"{escaped}{col_map_suffix}\" AS cm ON (c.cell_key & {col_id_mask}) = cm.col_id
        LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal
        {cell_where}
        GROUP BY c.cell_key >> {col_id_bits}, c.db_version, site_tbl.site_id",
        table_name_val = table_name_val,
        pk_expr = pk_expr,
        main_join = main_join,
        col_val_case = col_val_case,
        seq_order_col = seq_order_col,
        cell_where = cell_where,
        escaped = escaped,
        clock_suffix = consts::V2_CLOCK_SUFFIX,
        pks_suffix = consts::V2_PKS_SUFFIX,
        col_map_suffix = consts::V2_COL_MAP_SUFFIX,
        col_id_bits = col_id_bits,
        col_id_mask = consts::CRSQL_COL_ID_MASK,
    );

    // Part 2: Tombstone rows — V2 wire format
    // skip_hash tombstones use cid='-1' (DELETE_SENTINEL), hash tombstones use cid='-2'.
    // col_vrsn is always NULL in v2wire tombstone arms.
    let tomb_cid = if table_info.skip_hash {
        crate::c::DELETE_SENTINEL
    } else {
        consts::V2_HASH_TOMBSTONE_CID
    };
    let tomb_cid_expr = format!("'{}'", tomb_cid);
    let tomb_pushed_where = build_pushed_where(pushed, false, Some(&tomb_cid_expr), Some("NULL"), Some(&tbl_expr));
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "NULL", &tomb_pushed_where, need_seq_order)
    } else {
        let seq_order_col = if need_seq_order { ", t.seq as _seq_order" } else { "" };
        let where_clause = if tomb_pushed_where.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", tomb_pushed_where)
        };
        format!(
            "SELECT
              '{table_name_val}' as tbl,
              t.hashed_pk as pks,
              '{hash_tombstone_cid}' as cid,
              NULL as col_vrsn,
              t.db_version as db_vrsn,
              site_tbl.site_id as site_id,
              (1 << 62) | (t.site_id << 46) | (t.db_version << 22) | t.seq as key,
              t.seq as seq,
              t.cl as cl,
              t.ts as ts,
              NULL as cval{seq_order_col}
            FROM \"{escaped}{tomb_suffix}\" AS t
            LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal{where_clause}",
            table_name_val = table_name_val,
            hash_tombstone_cid = consts::V2_HASH_TOMBSTONE_CID,
            seq_order_col = seq_order_col,
            where_clause = where_clause,
            escaped = escaped,
            tomb_suffix = consts::V2_TOMBSTONES_SUFFIX,
        )
    };

    Ok(format!(
        "{cell_changes} UNION ALL {tombstone_rows}",
        cell_changes = cell_changes,
        tombstone_rows = tombstone_rows,
    ))
}

/// PK-only table query: reads sentinel clock entries at col_id=0.
/// No v2_col_map JOIN needed. Emits cid='-1', cval=NULL.
/// Tombstone part is the same as the normal v1wire query.
fn crsql_changes_query_for_table_v2_pkonly(
    table_info: &TableInfo,
    pushed: &[PushedConstraint],
    need_seq_order: bool,
) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let pk_list_tomb = crate::util::as_identifier_list(&table_info.pks, Some("tpk_tbl."))?;
    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;

    let (pk_expr, main_join) = build_pk_expr_and_join(table_info, &escaped)?;

    let seq_order_col = if need_seq_order { ", c.seq as _seq_order" } else { "" };
    // PK-only clock arm has no cm join — cid is a literal sentinel,
    // col_vrsn is c.col_version (scalar, non-aggregate).
    let tbl_expr = format!("'{}'", table_name_val);
    let pkonly_cid_expr = format!("'{}'", crate::c::INSERT_SENTINEL);
    let cell_pushed_where = build_pushed_where(pushed, true, Some(&pkonly_cid_expr), Some("c.col_version"), Some(&tbl_expr));
    let cell_where = if cell_pushed_where.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", cell_pushed_where)
    };

    // Sentinel clock entries at col_id=0
    let cell_changes = format!(
        "SELECT
          '{table_name_val}' as tbl,
          crsql_pack_columns({pk_expr}) as pks,
          '{sentinel}' as cid,
          c.col_version as col_vrsn,
          c.db_version as db_vrsn,
          site_tbl.site_id as site_id,
          c.cell_key >> {col_id_bits} as key,
          c.seq as seq,
          pk_tbl.cl as cl,
          c.ts as ts,
          NULL as cval{seq_order_col}
        FROM \"{escaped}{clock_suffix}\" AS c
        JOIN \"{escaped}{pks_suffix}\" AS pk_tbl ON (c.cell_key >> {col_id_bits}) = pk_tbl.__crsql_key
        {main_join}
        LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal{cell_where}",
        table_name_val = table_name_val,
        pk_expr = pk_expr,
        sentinel = crate::c::INSERT_SENTINEL,
        main_join = main_join,
        seq_order_col = seq_order_col,
        cell_where = cell_where,
        escaped = escaped,
        clock_suffix = consts::V2_CLOCK_SUFFIX,
        pks_suffix = consts::V2_PKS_SUFFIX,
        col_id_bits = col_id_bits,
    );

    // Tombstone rows (same pattern as v1wire — skip_hash reads PK directly)
    // cid = DELETE_SENTINEL (-1), col_vrsn = t.cl for both tombstone types.
    let tomb_cid_expr = format!("'{}'", crate::c::DELETE_SENTINEL);
    let tomb_pushed_where = build_pushed_where(pushed, false, Some(&tomb_cid_expr), Some("t.cl"), Some(&tbl_expr));
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "t.cl", &tomb_pushed_where, need_seq_order)
    } else {
        hash_tombstone_query(table_info, &escaped, &table_name_val, &pk_list_tomb, "t.cl", &tomb_pushed_where, need_seq_order)
    };

    Ok(format!(
        "{cell_changes} UNION ALL {tombstone_rows}",
        cell_changes = cell_changes,
        tombstone_rows = tombstone_rows,
    ))
}

/// Decide which query to use for a table based on metadata-use-version and sync-log-version.
/// - metadata_use_version=1: always use V1 query (reads from V1 tables)
/// - metadata_use_version=2 + sync_log_version=1: V2 per-column query (reads from V2 tables, V1 wire)
/// - metadata_use_version=2 + sync_log_version=2: V2 packed query (reads from V2 tables, V2 wire)
/// - PK-only tables (non_pks empty): use PK-only query with sentinel at col_id=0
fn query_for_table(
    table_info: &TableInfo,
    metadata_use_version: i32,
    sync_log_version: i32,
    pushed: &[PushedConstraint],
    need_seq_order: bool,
) -> Result<String, ResultCode> {
    // If metadata-use-version is V1, always read from V1 tables (even if V2 tables exist)
    if metadata_use_version == consts::META_USE_V1 {
        // Only use V1 query if V1 tables exist
        if table_info.schema_version == SchemaVersion::V1
            || table_info.schema_version == SchemaVersion::V2AndV1
        {
            return crsql_changes_query_for_table(table_info);
        }
        // V2-only tables but use-version=1 shouldn't happen (config guards prevent it)
        // Fall through to V2 query as safety
    }

    // metadata-use-version is V2 (or safety fallback)
    match table_info.schema_version {
        SchemaVersion::V2 | SchemaVersion::V2AndV1 => {
            // PK-only tables use a dedicated query with sentinel at col_id=0
            if table_info.non_pks.is_empty() {
                return crsql_changes_query_for_table_v2_pkonly(table_info, pushed, need_seq_order);
            }
            if sync_log_version == consts::SYNC_LOG_V2 {
                crsql_changes_query_for_table_v2_v2wire(table_info, pushed, need_seq_order)
            } else {
                crsql_changes_query_for_table_v2_v1wire(table_info)
            }
        }
        SchemaVersion::V1 => crsql_changes_query_for_table(table_info),
    }
}

/// Returns true if the query includes a cval column.
/// V2 metadata always fetches cval inline (both V1 and V2 wire formats).
/// V1 metadata fetches column values lazily in changes_next.
pub fn query_has_cval(metadata_use_version: i32) -> bool {
    metadata_use_version == consts::META_USE_V2
}

pub fn changes_union_query(
    table_infos: &[&TableInfo],
    idx_str: *const c_char,
    metadata_use_version: i32,
    sync_log_version: i32,
) -> Result<String, ResultCode> {
    let mut sub_queries = vec![];
    let has_cval = query_has_cval(metadata_use_version);

    // Read the binary plan from idx_str (allocated by changes_best_index).
    let (constraints, order_by_col_ids, order_by_desc, has_order_by) =
        unsafe { read_idx_plan(idx_str) };

    // Reject LIKE/MATCH/GLOB/REGEXP on all crsql_changes columns. These ops
    // silently produce wrong results on packed BLOB outputs (cid, col_vrsn,
    // seq, cval, pks). xBestIndex accepts them with omit=1 so SQLite doesn't
    // evaluate them externally — we error here in xFilter instead.
    for c in &constraints {
        if is_pattern_op_id(c.op_id) {
            return Err(ResultCode::ERROR);
        }
    }

    let is_v2_packed = metadata_use_version == consts::META_USE_V2
        && sync_log_version == consts::SYNC_LOG_V2;

    // Columns that can be pushed into arms in V2-wire packed mode.
    const PUSHABLE_COLS: &[CrsqlChangesColumn] = &[
        CrsqlChangesColumn::Tbl,
        CrsqlChangesColumn::Cid,
        CrsqlChangesColumn::ColVrsn,
        CrsqlChangesColumn::DbVrsn,
        CrsqlChangesColumn::SiteId,
        CrsqlChangesColumn::Cl,
        CrsqlChangesColumn::Seq,
        CrsqlChangesColumn::Ts,
    ];

    let (pushed, outer_idx_str) = if is_v2_packed {
        // Partition constraints into pushable (go into arms) and other (stay
        // in outer WHERE). IS NULL / IS NOT NULL (param_idx == 0) stay in
        // outer WHERE since they have no parameter to bind inside arms.
        let (push_constraints, other_constraints): (Vec<&PlanConstraint>, Vec<&PlanConstraint>) =
            constraints
                .iter()
                .partition(|c| c.param_idx != 0 && PUSHABLE_COLS.contains(&c.col));

        // Convert pushable constraints to PushedConstraint.
        // No conversion needed — CrsqlChangesColumn is used directly.
        // Pk and Cval are filtered out by the partition above.
        let pushed: Vec<PushedConstraint> = push_constraints
            .iter()
            .filter_map(|&c| {
                if matches!(c.col, CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval) {
                    return None;
                }
                Some(PushedConstraint {
                    col: c.col,
                    op: op_id_to_str(c.op_id)?.to_string(),
                    param_index: c.param_idx as i32,
                })
            })
            .collect();

        let outer = rebuild_outer_idx_str(
            &other_constraints,
            &order_by_col_ids,
            order_by_desc,
            has_order_by,
        );
        (pushed, outer)
    } else {
        // Non-packed arms are non-aggregate: SQLite's push-down optimization
        // hoists outer WHERE terms into them automatically.
        // Reconstruct the SQL WHERE + ORDER BY from the binary plan.
        let mut outer = String::new();
        let clauses: Vec<String> = constraints
            .iter()
            .filter_map(|c| constraint_to_sql(c))
            .collect();
        if !clauses.is_empty() {
            outer.push_str("WHERE ");
            outer.push_str(&clauses.join(" AND "));
        }
        if has_order_by && !order_by_col_ids.is_empty() {
            outer.push_str(" ORDER BY ");
            let suffix = if order_by_desc { " DESC" } else { " ASC" };
            let cols: Vec<String> = order_by_col_ids
                .iter()
                .filter_map(|&c| col_to_name(c).map(|s| format!("{}{}", s, suffix)))
                .collect();
            outer.push_str(&cols.join(", "));
        } else {
            outer.push_str(" ORDER BY db_vrsn ASC, seq ASC");
        }
        (vec![], outer)
    };

    // In V2-wire packed mode we always need _seq_order for the outer ORDER BY.
    let need_seq_order = is_v2_packed;

    for table_info in table_infos {
        let query_part = query_for_table(
            table_info,
            metadata_use_version,
            sync_log_version,
            &pushed,
            need_seq_order,
        )?;
        sub_queries.push(query_part);
    }

    // Branch pruning can leave zero tables (e.g. `tbl = 'nope'` matches no
    // CRR). A UNION with no limbs is invalid SQL, so emit a typed empty limb.
    // It must expose every column the outer SELECT / ORDER BY reference,
    // including _seq_order in packed mode.
    if sub_queries.is_empty() {
        let mut cols = String::from(
            "NULL AS tbl, NULL AS pks, NULL AS cid, NULL AS col_vrsn, \
             NULL AS db_vrsn, NULL AS site_id, NULL AS key, NULL AS seq, \
             NULL AS cl, NULL AS ts",
        );
        if has_cval {
            cols.push_str(", NULL AS cval");
        }
        if need_seq_order {
            cols.push_str(", NULL AS _seq_order");
        }
        sub_queries.push(format!("SELECT {cols} WHERE 0", cols = cols));
    }

    if has_cval {
        Ok(format!(
            "SELECT tbl, pks, cid, col_vrsn, db_vrsn, site_id, key, seq, cl, ts, cval FROM ({unions}) {outer_idx_str}\0",
            unions = sub_queries.join(" UNION ALL "),
            outer_idx_str = outer_idx_str,
        ))
    } else {
        Ok(format!(
            "SELECT tbl, pks, cid, col_vrsn, db_vrsn, site_id, key, seq, cl, ts FROM ({unions}) {outer_idx_str}\0",
            unions = sub_queries.join(" UNION ALL "),
            outer_idx_str = outer_idx_str,
        ))
    }
}
