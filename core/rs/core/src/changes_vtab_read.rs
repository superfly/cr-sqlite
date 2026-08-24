extern crate alloc;
use crate::tableinfo::{TableInfo, SchemaVersion};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use sqlite::ResultCode;

use sqlite_nostd as sqlite;

use crate::consts;

/// Build the skip_hash tombstone SELECT query for feed operations.
/// `col_vrsn_expr` is "t.cl" for v1wire/pkonly or "NULL" for v2wire.
fn skip_hash_tombstone_query(
    table_info: &TableInfo,
    escaped: &str,
    table_name_val: &str,
    col_vrsn_expr: &str,
) -> String {
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
          NULL as cval
        FROM \"{escaped}{tomb_suffix}\" AS t
        LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal",
        table_name_val = table_name_val,
        pk_col = table_info.skip_hash_pk_col,
        delete_sentinel = crate::c::DELETE_SENTINEL,
        col_vrsn = col_vrsn_expr,
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
fn hash_tombstone_query(
    table_info: &TableInfo,
    escaped: &str,
    table_name_val: &str,
    pk_list_tomb: &str,
    col_vrsn_expr: &str,
) -> String {
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
          NULL as cval
        FROM \"{escaped}{tomb_suffix}\" AS t
        JOIN \"{escaped}{tomb_pks_suffix}\" AS tpk_tbl ON t.hashed_pk = tpk_tbl.hashed_pk
        LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal",
        table_name_val = table_name_val,
        pk_list_tomb = pk_list_tomb,
        delete_sentinel = crate::c::DELETE_SENTINEL,
        col_vrsn = col_vrsn_expr,
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
        sentinel = crate::c::INSERT_SENTINEL
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
    // skip_hash: PK value stored directly in v2_tombstones, no v2_tombstone_pks JOIN needed.
    // hash mode: JOIN v2_tombstone_pks to get PK values from hashed_pk.
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "t.cl")
    } else {
        hash_tombstone_query(table_info, &escaped, &table_name_val, &pk_list_tomb, "t.cl")
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
    let mask = consts::CRSQL_COL_ID_MASK;

    let mut when_clauses = vec![];
    for (col_id, col) in table_info.non_pks.iter().enumerate() {
        when_clauses.push(format!(
            "WHEN {col_id} THEN mt.\"{col_name}\"",
            col_id = col_id,
            col_name = crate::util::escape_ident(&col.name)
        ));
    }

    if when_clauses.is_empty() {
        // No non-PK columns — return NULL
        Ok("NULL".to_string())
    } else {
        Ok(format!(
            "CASE (c.cell_key & {mask}) {whens} END",
            mask = mask,
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
fn crsql_changes_query_for_table_v2_v2wire(table_info: &TableInfo) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;
    let col_val_case = build_col_val_case(table_info)?;

    let (pk_expr, main_join) = build_pk_expr_and_join(table_info, &escaped)?;

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
          crsql_pack_agg(({col_val_case}) ORDER BY cm.col_id) as cval
        FROM \"{escaped}{clock_suffix}\" AS c
        JOIN \"{escaped}{pks_suffix}\" AS pk_tbl ON (c.cell_key >> {col_id_bits}) = pk_tbl.__crsql_key
        {main_join}
        JOIN \"{escaped}{col_map_suffix}\" AS cm ON (c.cell_key & {col_id_mask}) = cm.col_id
        LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal
        GROUP BY c.cell_key >> {col_id_bits}, c.db_version, site_tbl.site_id",
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

    // Part 2: Tombstone rows — V2 wire format
    // skip_hash: crsql_pack_columns(pk_col) as pks, '-1' as cid (real PK value, not hash)
    // hash mode: hashed_pk as pks, '-2' as cid (hash tombstone)
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "NULL")
    } else {
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
              NULL as cval
            FROM \"{escaped}{tomb_suffix}\" AS t
            LEFT JOIN crsql_site_id AS site_tbl ON t.site_id = site_tbl.ordinal",
            table_name_val = table_name_val,
            hash_tombstone_cid = consts::V2_HASH_TOMBSTONE_CID,
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
fn crsql_changes_query_for_table_v2_pkonly(table_info: &TableInfo) -> Result<String, ResultCode> {
    if table_info.pks.is_empty() {
        return Err(ResultCode::ABORT);
    }

    let pk_list_tomb = crate::util::as_identifier_list(&table_info.pks, Some("tpk_tbl."))?;
    let escaped = crate::util::escape_ident(&table_info.tbl_name);
    let table_name_val = crate::util::escape_ident_as_value(&table_info.tbl_name);
    let col_id_bits = consts::CRSQL_COL_ID_BITS;

    let (pk_expr, main_join) = build_pk_expr_and_join(table_info, &escaped)?;

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
          NULL as cval
        FROM \"{escaped}{clock_suffix}\" AS c
        JOIN \"{escaped}{pks_suffix}\" AS pk_tbl ON (c.cell_key >> {col_id_bits}) = pk_tbl.__crsql_key
        {main_join}
        LEFT JOIN crsql_site_id AS site_tbl ON c.site_id = site_tbl.ordinal",
        table_name_val = table_name_val,
        pk_expr = pk_expr,
        sentinel = crate::c::INSERT_SENTINEL,
        main_join = main_join,
        escaped = escaped,
        clock_suffix = consts::V2_CLOCK_SUFFIX,
        pks_suffix = consts::V2_PKS_SUFFIX,
        col_id_bits = col_id_bits,
    );

    // Tombstone rows (same pattern as v1wire — skip_hash reads PK directly)
    let tombstone_rows = if table_info.skip_hash {
        skip_hash_tombstone_query(table_info, &escaped, &table_name_val, "t.cl")
    } else {
        hash_tombstone_query(table_info, &escaped, &table_name_val, &pk_list_tomb, "t.cl")
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
                return crsql_changes_query_for_table_v2_pkonly(table_info);
            }
            if sync_log_version == consts::SYNC_LOG_V2 {
                crsql_changes_query_for_table_v2_v2wire(table_info)
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
    table_infos: &Vec<TableInfo>,
    idx_str: &str,
    metadata_use_version: i32,
    sync_log_version: i32,
) -> Result<String, ResultCode> {
    let mut sub_queries = vec![];
    let has_cval = query_has_cval(metadata_use_version);

    for table_info in table_infos {
        let query_part = query_for_table(table_info, metadata_use_version, sync_log_version)?;
        sub_queries.push(query_part);
    }

    if has_cval {
        Ok(format!(
            "SELECT tbl, pks, cid, col_vrsn, db_vrsn, site_id, key, seq, cl, ts, cval FROM ({unions}) {idx_str}\0",
            unions = sub_queries.join(" UNION ALL "),
            idx_str = idx_str,
        ))
    } else {
        Ok(format!(
            "SELECT tbl, pks, cid, col_vrsn, db_vrsn, site_id, key, seq, cl, ts FROM ({unions}) {idx_str}\0",
            unions = sub_queries.join(" UNION ALL "),
            idx_str = idx_str,
        ))
    }
}
