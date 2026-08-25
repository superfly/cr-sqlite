extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use sqlite_nostd as sqlite;
use sqlite_nostd::{sqlite3, Connection, ManagedStmt, ResultCode};

use crate::consts;
use crate::tableinfo::{ColumnInfo, TableInfo};

/// RAII guard that resets + clears bindings on drop.
/// Prevents "forgot to reset" bugs — the compiler does it for you.
pub struct StmtGuard<'a> {
    stmt: &'a mut ManagedStmt,
}

impl<'a> StmtGuard<'a> {
    pub fn new(stmt: &'a mut ManagedStmt) -> Self {
        StmtGuard { stmt }
    }
}

impl<'a> core::ops::Deref for StmtGuard<'a> {
    type Target = ManagedStmt;
    fn deref(&self) -> &ManagedStmt {
        self.stmt
    }
}

impl<'a> core::ops::DerefMut for StmtGuard<'a> {
    fn deref_mut(&mut self) -> &mut ManagedStmt {
        self.stmt
    }
}

impl<'a> Drop for StmtGuard<'a> {
    fn drop(&mut self) {
        let _ = self.stmt.clear_bindings();
        let _ = self.stmt.reset();
    }
}

/// All cached V2 prepared statements for a single table.
/// Pre-prepared at first use — fails early on bad SQL.
/// Statement count is bounded: fixed set per table + one per column for clock_merge_upsert.
///
/// `merge_equal` is baked into the SQL at prepare time. When it changes,
/// `TableInfo::get_v2_stmts` detects the mismatch and re-prepares automatically.
pub struct V2Stmts {
    // --- Lookups ---
    /// UNION ALL lookup: alive (__crsql_key, cl) from v2_pks + dead (NULL, cl) from v2_tombstones.
    /// Bind the same PK value/hashed_pk to both slots 1 and 2.
    /// If column 0 is NULL → row is dead (in tombstones); otherwise it's alive (in v2_pks).
    lookup_row_state: ManagedStmt,

    // --- v2_pks mutations ---
    /// INSERT ... cl=? with RETURNING __crsql_key (used for new rows, resurrections, hydration, merge)
    pks_insert: ManagedStmt,
    /// DELETE FROM v2_pks WHERE __crsql_key = ?
    pks_delete: ManagedStmt,

    // --- v2_tombstones mutations ---
    /// DELETE FROM v2_tombstones WHERE ... (local writes)
    tomb_delete: ManagedStmt,
    /// INSERT OR REPLACE INTO v2_tombstones ... (site_id is always a bind param)
    tomb_insert: ManagedStmt,
    /// Upsert with ON CONFLICT + merge_equal WHERE clause (merge path)
    tomb_upsert: ManagedStmt,
    /// hash mode only
    tomb_pks_delete: Option<ManagedStmt>,
    /// hash mode only
    tomb_pks_insert: Option<ManagedStmt>,

    // --- v2_clock mutations ---
    /// INSERT OR REPLACE ... VALUES (?, 1, ...) (local insert path)
    clock_insert: ManagedStmt,
    /// INSERT OR REPLACE ... VALUES (?, COALESCE(...), ...) (local update path)
    clock_upsert: ManagedStmt,
    /// DELETE FROM v2_clock WHERE cell_key >= ? AND <= ?
    clock_delete_range: ManagedStmt,
    /// INSERT INTO v2_clock SELECT ? + col_id ... FROM v2_col_map (ensure alive row)
    clock_zero_fill: ManagedStmt,
    /// Per-column merge upsert with crsql_change_wins. Keyed by column name.
    clock_merge_upserts: alloc::collections::BTreeMap<String, ManagedStmt>,

    // --- v2_col_map ---
    /// SELECT col_id FROM v2_col_map WHERE col_name = ?
    col_id_lookup: ManagedStmt,
    /// SELECT col_id FROM v2_col_map (all columns, for clock entry creation)
    col_ids_all: ManagedStmt,

    // --- Base table ---
    /// INSERT INTO base table (pk_cols) VALUES (?, ...) — for merge new row creation
    base_insert: ManagedStmt,
    /// DELETE FROM base table WHERE rowid_alias = ? (rowid-key tables)
    base_delete_rowid: ManagedStmt,
    /// DELETE FROM base table WHERE pk_cols = ? (non-rowid tables)
    base_delete_nonrowid: Option<ManagedStmt>,
    /// SELECT rowid_alias FROM base table WHERE pk_cols = ? — hydration rowid lookup
    base_lookup_rowid: Option<ManagedStmt>,
    /// SELECT pk_cols FROM base table WHERE rowid_alias = ? (rowid-key) or
    /// SELECT pk_cols FROM v2_pks WHERE __crsql_key = ? (non-rowid) — merge PK lookup
    pk_lookup_by_key: ManagedStmt,
    /// Per-column base table UPDATE statements. Keyed by column name.
    /// For rowid-key tables: UPDATE base SET "col" = ? WHERE rowid_alias = ?
    /// For non-rowid tables: UPDATE base SET "col" = ? WHERE pk1 = ? AND pk2 = ? ...
    /// V2 guarantees the row exists (created by v2_ensure_alive_row_at_cl),
    /// so a plain UPDATE is more efficient than INSERT ... ON CONFLICT DO UPDATE.
    base_updates: alloc::collections::BTreeMap<String, ManagedStmt>,

    // --- V1 interop (hydration + mirror) ---
    // --- V1 interop (only prepared when SchemaVersion::V2AndV1) ---
    /// SELECT col_version FROM V1 clock WHERE key = ? AND col_name = DELETE_SENTINEL
    v1_sentinel_lookup: Option<ManagedStmt>,
    /// SELECT 1 FROM V1 clock WHERE key = ? LIMIT 1
    v1_any_clock_lookup: Option<ManagedStmt>,
    /// SELECT site_id, db_version, seq, ts FROM V1 clock WHERE key = ? AND col_name = DELETE_SENTINEL
    v1_sentinel_detail: Option<ManagedStmt>,
    /// DELETE FROM V1 clock WHERE key = ?
    v1_clock_delete: Option<ManagedStmt>,
    /// INSERT INTO V1 clock SELECT ... FROM v2_clock WHERE cell_key = ? (alive sentinel)
    v1_sentinel_insert_alive: Option<ManagedStmt>,
    /// INSERT INTO V1 clock SELECT ... FROM v2_tombstones WHERE ... (dead sentinel)
    /// skip_hash: WHERE pk_col = ?, hash: WHERE hashed_pk = ?
    v1_sentinel_insert_dead: Option<ManagedStmt>,
    /// INSERT INTO V1 clock SELECT ... FROM v2_clock JOIN v2_col_map (clock copy)
    v1_clock_copy: Option<ManagedStmt>,
    /// INSERT INTO v2_clock SELECT ... FROM V1 clock JOIN v2_col_map (hydration clock copy)
    hydrate_clock_copy: Option<ManagedStmt>,

    // --- PK lookup by hashed_pk (hash mode only, for v1 copy) ---
    /// SELECT pk_cols FROM v2_tombstone_pks WHERE hashed_pk = ?
    lookup_pks_tomb: Option<ManagedStmt>,
    /// SELECT pk_cols FROM v2_pks [JOIN base] WHERE hashed_pk = ?
    lookup_pks_alive: Option<ManagedStmt>,

    /// The merge_equal value baked into the SQL at prepare time.
    merge_equal: i32,
}

impl V2Stmts {
    /// Pre-prepare all V2 statements for a table.
    /// All SQL lives here — one place to read, one place to fail.
    pub fn prepare(db: *mut sqlite3, tbl_info: &TableInfo, merge_equal: i32) -> Result<Self, ResultCode> {
        let escaped = crate::util::escape_ident(&tbl_info.tbl_name);
        let (pk_cols, pk_values) = pk_cols_and_values(&tbl_info.pks);
        let (where_tomb, _use_hash_tomb) = v2_pk_lookup_where(tbl_info, false);
        let pk_col_name = if tbl_info.skip_hash { &tbl_info.skip_hash_pk_col } else { "hashed_pk" };

        // --- Lookups ---

        // lookup_row_state: UNION ALL alive + dead, with 2 bind slots (same value bound twice)
        let lookup_row_state = if tbl_info.skip_hash {
            let pk_col = &tbl_info.skip_hash_pk_col;
            let (alive, dead) = if tbl_info.has_integer_pk && tbl_info.key_is_rowid {
                (
                    format!("SELECT __crsql_key, cl FROM \"{escaped}{}\" WHERE __crsql_key = ?",
                        consts::V2_PKS_SUFFIX),
                    format!("SELECT NULL, cl FROM \"{escaped}{}\" WHERE \"{pk_col}\" = ?",
                        consts::V2_TOMBSTONES_SUFFIX),
                )
            } else if tbl_info.key_is_rowid {
                let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
                (
                    format!("SELECT v2p.__crsql_key, v2p.cl FROM \"{escaped}{}\" v2p \
                        JOIN \"{escaped}\" mt ON mt.\"{alias}\" = v2p.__crsql_key \
                        WHERE mt.\"{pk_col}\" = ?", consts::V2_PKS_SUFFIX),
                    format!("SELECT NULL, t.cl FROM \"{escaped}{}\" t WHERE t.\"{pk_col}\" = ?",
                        consts::V2_TOMBSTONES_SUFFIX),
                )
            } else {
                (
                    format!("SELECT __crsql_key, cl FROM \"{escaped}{}\" WHERE \"{pk_col}\" = ?",
                        consts::V2_PKS_SUFFIX),
                    format!("SELECT NULL, cl FROM \"{escaped}{}\" WHERE \"{pk_col}\" = ?",
                        consts::V2_TOMBSTONES_SUFFIX),
                )
            };
            db.prepare_v3(&format!("{alive} UNION ALL {dead} LIMIT 1"), sqlite::PREPARE_PERSISTENT)?
        } else {
            db.prepare_v3(&format!(
                "SELECT __crsql_key, cl FROM \"{escaped}{}\" WHERE hashed_pk = ? \
                UNION ALL \
                SELECT NULL, cl FROM \"{escaped}{}\" WHERE hashed_pk = ? \
                LIMIT 1",
                consts::V2_PKS_SUFFIX, consts::V2_TOMBSTONES_SUFFIX
            ), sqlite::PREPARE_PERSISTENT)?
        };

        // --- v2_pks mutations ---

        let pks_insert = db.prepare_v3(
            &v2_pks_insert_sql(tbl_info.skip_hash, tbl_info.key_is_rowid, &escaped, &pk_cols, &pk_values, "?"),
            sqlite::PREPARE_PERSISTENT)?;

        let pks_delete = db.prepare_v3(&format!(
            "DELETE FROM \"{escaped}{}\" WHERE __crsql_key = ?",
            consts::V2_PKS_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        // --- v2_tombstones mutations ---

        let tomb_delete = db.prepare_v3(&format!(
            "DELETE FROM \"{escaped}{}\" WHERE {where_tomb}",
            consts::V2_TOMBSTONES_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        // site_id is always a bind param (0 for local writes, actual site for hydration/merge)
        let tomb_insert = db.prepare_v3(&format!(
            "INSERT OR REPLACE INTO \"{escaped}{}\" (site_id, db_version, seq, \"{pk_col}\", cl, ts) \
             VALUES (?, ?, ?, ?, ?, ?)",
            consts::V2_TOMBSTONES_SUFFIX,
            pk_col = pk_col_name,
        ), sqlite::PREPARE_PERSISTENT)?;

        // Merge upsert with ON CONFLICT + merge_equal WHERE clause
        let merge_where = if merge_equal == 1 {
            format!(
                "WHERE excluded.cl > \"{escaped}{}\".cl \
                OR (excluded.cl = \"{escaped}{}\".cl \
                    AND ? > (SELECT site_id FROM crsql_site_id WHERE ordinal = \"{escaped}{}\".site_id))",
                consts::V2_TOMBSTONES_SUFFIX, consts::V2_TOMBSTONES_SUFFIX, consts::V2_TOMBSTONES_SUFFIX
            )
        } else {
            format!("WHERE excluded.cl > \"{escaped}{}\".cl", consts::V2_TOMBSTONES_SUFFIX)
        };
        let tomb_upsert = if tbl_info.skip_hash {
            db.prepare_v3(&format!(
                "INSERT INTO \"{escaped}{}\" (site_id, db_version, seq, \"{pk_col}\", cl, ts) \
                VALUES (?, ?, ?, ?, ?, ?) \
                ON CONFLICT(\"{pk_col}\") DO UPDATE SET \
                site_id = excluded.site_id, db_version = excluded.db_version, \
                seq = excluded.seq, cl = excluded.cl, ts = excluded.ts \
                {merge_where}",
                consts::V2_TOMBSTONES_SUFFIX,
                pk_col = pk_col_name,
            ), sqlite::PREPARE_PERSISTENT)?
        } else {
            db.prepare_v3(&format!(
                "INSERT INTO \"{escaped}{}\" (site_id, db_version, seq, hashed_pk, cl, ts) \
                VALUES (?, ?, ?, ?, ?, ?) \
                ON CONFLICT(hashed_pk) DO UPDATE SET \
                site_id = excluded.site_id, db_version = excluded.db_version, \
                seq = excluded.seq, cl = excluded.cl, ts = excluded.ts \
                {merge_where}",
                consts::V2_TOMBSTONES_SUFFIX,
            ), sqlite::PREPARE_PERSISTENT)?
        };

        let (tomb_pks_delete, tomb_pks_insert) = if !tbl_info.skip_hash {
            let del = db.prepare_v3(&format!(
                "DELETE FROM \"{escaped}{}\" WHERE hashed_pk = ?",
                consts::V2_TOMBSTONE_PKS_SUFFIX
            ), sqlite::PREPARE_PERSISTENT)?;
            let ins = db.prepare_v3(&format!(
                "INSERT OR REPLACE INTO \"{escaped}{}\" (hashed_pk, {pk_cols}) VALUES (?, {pk_values})",
                consts::V2_TOMBSTONE_PKS_SUFFIX,
            ), sqlite::PREPARE_PERSISTENT)?;
            (Some(del), Some(ins))
        } else {
            (None, None)
        };

        // --- v2_clock mutations ---

        let clock_insert = db.prepare_v3(&format!(
            "INSERT OR REPLACE INTO \"{escaped}{}\" (cell_key, col_version, site_id, db_version, seq, ts) \
             VALUES (?, 1, 0, ?, ?, ?)",
            consts::V2_CLOCK_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        let clock_upsert = db.prepare_v3(&format!(
            "INSERT OR REPLACE INTO \"{escaped}{}\" (cell_key, col_version, site_id, db_version, seq, ts) \
             VALUES (?, COALESCE((SELECT col_version + 1 FROM \"{escaped}{}\" WHERE cell_key = ?), 1), 0, ?, ?, ?)",
            consts::V2_CLOCK_SUFFIX, consts::V2_CLOCK_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        let clock_delete_range = db.prepare_v3(&format!(
            "DELETE FROM \"{escaped}{}\" WHERE cell_key >= ? AND cell_key <= ?",
            consts::V2_CLOCK_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        let clock_zero_fill = db.prepare_v3(&format!(
            "INSERT INTO \"{escaped}{}\" (cell_key, col_version, site_id, db_version, seq, ts) \
             SELECT ? + col_id, 0, ?, ?, 0, crsql_get_ts() \
             FROM \"{escaped}{}\"",
            consts::V2_CLOCK_SUFFIX, consts::V2_COL_MAP_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        // --- v2_col_map ---

        let col_id_lookup = db.prepare_v3(&format!(
            "SELECT col_id FROM \"{escaped}{}\" WHERE col_name = ?",
            consts::V2_COL_MAP_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        let col_ids_all = db.prepare_v3(&format!(
            "SELECT col_id FROM \"{escaped}{}\" ORDER BY col_id",
            consts::V2_COL_MAP_SUFFIX
        ), sqlite::PREPARE_PERSISTENT)?;

        // --- Base table ---

        let base_insert = db.prepare_v3(&format!(
            "INSERT INTO \"{escaped}\" ({pk_cols}) VALUES ({pk_values})",
        ), sqlite::PREPARE_PERSISTENT)?;

        let base_delete_rowid = db.prepare_v3(&format!(
            "DELETE FROM \"{escaped}\" WHERE \"{alias}\" = ?",
            alias = crate::util::escape_ident(&tbl_info.rowid_alias)
        ), sqlite::PREPARE_PERSISTENT)?;

        let base_delete_nonrowid = if !tbl_info.key_is_rowid {
            let where_conds: Vec<String> = tbl_info.pks.iter()
                .map(|c| format!("\"{}\" = ?", crate::util::escape_ident(&c.name)))
                .collect();
            Some(db.prepare_v3(&format!(
                "DELETE FROM \"{escaped}\" WHERE {}",
                where_conds.join(" AND ")
            ), sqlite::PREPARE_PERSISTENT)?)
        } else {
            None
        };

        let base_lookup_rowid = if tbl_info.key_is_rowid {
            let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
            let pk_where: Vec<String> = tbl_info.pks.iter()
                .map(|c| format!("\"{}\" = ?", crate::util::escape_ident(&c.name)))
                .collect();
            Some(db.prepare_v3(&format!(
                "SELECT \"{alias}\" FROM \"{escaped}\" WHERE {}",
                pk_where.join(" AND ")
            ), sqlite::PREPARE_PERSISTENT)?)
        } else {
            None
        };

        // pk_lookup_by_key: used in merge to look up PK values from v2_pks or base table
        let pk_lookup_by_key = if tbl_info.key_is_rowid {
            let pk_list: Vec<String> = tbl_info.pks.iter()
                .map(|c| crate::util::escape_ident(&c.name))
                .collect();
            db.prepare_v3(&format!(
                "SELECT {pk_list} FROM \"{escaped}\" WHERE \"{alias}\" = ?",
                pk_list = pk_list.join(", "),
                alias = crate::util::escape_ident(&tbl_info.rowid_alias)
            ), sqlite::PREPARE_PERSISTENT)?
        } else {
            let pk_list: Vec<String> = tbl_info.pks.iter()
                .map(|c| crate::util::escape_ident(&c.name))
                .collect();
            db.prepare_v3(&format!(
                "SELECT {pk_list} FROM \"{escaped}{}\" WHERE __crsql_key = ?",
                consts::V2_PKS_SUFFIX,
                pk_list = pk_list.join(", ")
            ), sqlite::PREPARE_PERSISTENT)?
        };

        // --- V1 interop (only when V2AndV1 — V1 clock tables exist) ---

        let has_v1 = matches!(tbl_info.schema_version, crate::tableinfo::SchemaVersion::V2AndV1);

        let v1_sentinel_lookup = if has_v1 {
            Some(db.prepare_v3(&format!(
                "SELECT col_version FROM \"{escaped}__crsql_clock\" WHERE key = ? AND col_name = '{}'",
                crate::c::DELETE_SENTINEL
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        let v1_any_clock_lookup = if has_v1 {
            Some(db.prepare_v3(&format!(
                "SELECT 1 FROM \"{escaped}__crsql_clock\" WHERE key = ? LIMIT 1"
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        let v1_sentinel_detail = if has_v1 {
            Some(db.prepare_v3(&format!(
                "SELECT site_id, db_version, seq, ts FROM \"{escaped}__crsql_clock\" WHERE key = ? AND col_name = '{}'",
                crate::c::DELETE_SENTINEL
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        let v1_clock_delete = if has_v1 {
            Some(db.prepare_v3(&format!(
                "DELETE FROM \"{escaped}__crsql_clock\" WHERE key = ?"
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        // v1_sentinel_insert_alive: INSERT INTO V1 clock SELECT ... FROM v2_clock WHERE cell_key = ?
        // Bind order: 1=key, 2=cl, 3=ts_fallback, 4=cell_key
        let v1_sentinel_insert_alive = if has_v1 {
            Some(db.prepare_v3(&format!(
                "INSERT INTO \"{escaped}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts) \
                 SELECT ?, '-1', ?, site_id, db_version, seq, \
                 CASE WHEN ts > 0 THEN ts ELSE ? END \
                 FROM (SELECT site_id, db_version, seq, ts FROM \"{escaped}{}\" WHERE cell_key = ?) LIMIT 1",
                consts::V2_CLOCK_SUFFIX
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        // v1_sentinel_insert_dead: depends on skip_hash vs hash
        // Bind order: 1=key, 2=cl, 3=ts_fallback, 4=pk lookup value
        let v1_sentinel_insert_dead = if has_v1 {
            if tbl_info.skip_hash {
                Some(db.prepare_v3(&format!(
                    "INSERT INTO \"{escaped}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts) \
                    SELECT ?, '-1', ?, site_id, db_version, seq, \
                    CASE WHEN ts > 0 THEN ts ELSE ? END \
                    FROM (SELECT site_id, db_version, seq, ts FROM \"{escaped}{}\" WHERE \"{pk_col}\" = ?) LIMIT 1",
                    consts::V2_TOMBSTONES_SUFFIX,
                    pk_col = tbl_info.skip_hash_pk_col,
                ), sqlite::PREPARE_PERSISTENT)?)
            } else {
                Some(db.prepare_v3(&format!(
                    "INSERT INTO \"{escaped}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts) \
                    SELECT ?, '-1', ?, site_id, db_version, seq, \
                    CASE WHEN ts > 0 THEN ts ELSE ? END \
                    FROM (SELECT site_id, db_version, seq, ts FROM \"{escaped}{}\" WHERE hashed_pk = ?) LIMIT 1",
                    consts::V2_TOMBSTONES_SUFFIX,
                ), sqlite::PREPARE_PERSISTENT)?)
            }
        } else { None };

        // v1_clock_copy: bind order: 1=key, 2=ts_fallback, 3=col_id_mask, 4=cell_key_base, 5=cell_key_end
        let v1_clock_copy = if has_v1 {
            Some(db.prepare_v3(&format!(
                "INSERT INTO \"{escaped}__crsql_clock\" (key, col_name, col_version, db_version, seq, site_id, ts) \
                 SELECT ?, m.col_name, c.col_version, c.db_version, c.seq, \
                 CASE WHEN c.ts > 0 THEN c.ts ELSE ? END, c.site_id \
                 FROM \"{escaped}{}\" c \
                 JOIN \"{escaped}{}\" m ON (c.cell_key & ?) = m.col_id \
                 WHERE c.cell_key >= ? AND c.cell_key <= ?",
                consts::V2_CLOCK_SUFFIX, consts::V2_COL_MAP_SUFFIX
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        // hydrate_clock_copy: bind order: 1=v2_key, 2=ts_fallback, 3=v1_key
        let hydrate_clock_copy = if has_v1 {
            Some(db.prepare_v3(&format!(
                "INSERT INTO \"{escaped}{}\" (cell_key, col_version, site_id, db_version, seq, ts) \
                 SELECT (? << {col_id_bits} | m.col_id), c.col_version, c.site_id, c.db_version, c.seq, \
                 CASE WHEN c.ts > 0 THEN c.ts ELSE ? END \
                 FROM \"{escaped}__crsql_clock\" c \
                 JOIN \"{escaped}{}\" m ON c.col_name = m.col_name \
                 WHERE c.key = ?",
                consts::V2_CLOCK_SUFFIX, consts::V2_COL_MAP_SUFFIX,
                col_id_bits = consts::CRSQL_COL_ID_BITS
            ), sqlite::PREPARE_PERSISTENT)?)
        } else { None };

        // --- PK lookup by hashed_pk (hash mode only) ---

        let (lookup_pks_tomb, lookup_pks_alive) = if !tbl_info.skip_hash {
            let pk_list: Vec<String> = tbl_info.pks.iter()
                .map(|c| crate::util::escape_ident(&c.name))
                .collect();

            let tomb = db.prepare_v3(&format!(
                "SELECT {pk_list} FROM \"{escaped}{}\" WHERE hashed_pk = ?",
                consts::V2_TOMBSTONE_PKS_SUFFIX,
                pk_list = pk_list.join(", ")
            ), sqlite::PREPARE_PERSISTENT)?;

            let alive = if tbl_info.key_is_rowid {
                let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
                let t_pk_list: Vec<String> = pk_list.iter().map(|c| format!("t.{c}")).collect();
                db.prepare_v3(&format!(
                    "SELECT {t_pk_list} FROM \"{escaped}{}\" p \
                     JOIN \"{escaped}\" t ON t.\"{alias}\" = p.__crsql_key \
                     WHERE p.hashed_pk = ?",
                    consts::V2_PKS_SUFFIX,
                    t_pk_list = t_pk_list.join(", ")
                ), sqlite::PREPARE_PERSISTENT)?
            } else {
                db.prepare_v3(&format!(
                    "SELECT {pk_list} FROM \"{escaped}{}\" WHERE hashed_pk = ?",
                    consts::V2_PKS_SUFFIX,
                    pk_list = pk_list.join(", ")
                ), sqlite::PREPARE_PERSISTENT)?
            };
            (Some(tomb), Some(alive))
        } else {
            (None, None)
        };

        Ok(Self {
            lookup_row_state,
            pks_insert,
            pks_delete,
            tomb_delete,
            tomb_insert,
            tomb_upsert,
            tomb_pks_delete,
            tomb_pks_insert,
            clock_insert,
            clock_upsert,
            clock_delete_range,
            clock_zero_fill,
            clock_merge_upserts: {
                let mut map = alloc::collections::BTreeMap::new();
                for col in &tbl_info.non_pks {
                    let sql = clock_merge_upsert_sql(tbl_info, &escaped, &col.name);
                    map.insert(col.name.clone(), db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?);
                }
                map
            },
            col_id_lookup,
            col_ids_all,
            base_insert,
            base_delete_rowid,
            base_delete_nonrowid,
            base_lookup_rowid,
            pk_lookup_by_key,
            base_updates: {
                let mut map = alloc::collections::BTreeMap::new();
                for col in &tbl_info.non_pks {
                    let sql = base_update_sql(tbl_info, &escaped, &col.name);
                    map.insert(col.name.clone(), db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?);
                }
                map
            },
            v1_sentinel_lookup,
            v1_any_clock_lookup,
            v1_sentinel_detail,
            v1_clock_delete,
            v1_sentinel_insert_alive,
            v1_sentinel_insert_dead,
            v1_clock_copy,
            hydrate_clock_copy,
            lookup_pks_tomb,
            lookup_pks_alive,
            merge_equal,
        })
    }

    /// Get a per-column clock merge upsert.
    /// Each column needs its own prepared statement because the subquery
    /// embeds the column name to fetch the current value for crsql_change_wins.
    /// All statements are pre-prepared at V2Stmts construction time.
    pub fn clock_merge_upsert(
        &mut self,
        _db: *mut sqlite3,
        _tbl_info: &TableInfo,
        _escaped: &str,
        col_name: &str,
    ) -> Result<StmtGuard, ResultCode> {
        Ok(StmtGuard::new(self.clock_merge_upserts.get_mut(col_name).ok_or(ResultCode::ERROR)?))
    }

    // --- Getters ---

    pub fn lookup_row_state(&mut self) -> StmtGuard { StmtGuard::new(&mut self.lookup_row_state) }
    pub fn pks_insert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.pks_insert) }
    pub fn pks_delete(&mut self) -> StmtGuard { StmtGuard::new(&mut self.pks_delete) }
    pub fn tomb_delete(&mut self) -> StmtGuard { StmtGuard::new(&mut self.tomb_delete) }
    pub fn tomb_insert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.tomb_insert) }
    pub fn tomb_upsert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.tomb_upsert) }
    pub fn clock_insert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.clock_insert) }
    pub fn clock_upsert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.clock_upsert) }
    pub fn clock_delete_range(&mut self) -> StmtGuard { StmtGuard::new(&mut self.clock_delete_range) }
    pub fn clock_zero_fill(&mut self) -> StmtGuard { StmtGuard::new(&mut self.clock_zero_fill) }
    pub fn col_id_lookup(&mut self) -> StmtGuard { StmtGuard::new(&mut self.col_id_lookup) }
    pub fn col_ids_all(&mut self) -> StmtGuard { StmtGuard::new(&mut self.col_ids_all) }
    pub fn base_insert(&mut self) -> StmtGuard { StmtGuard::new(&mut self.base_insert) }
    pub fn base_delete_rowid(&mut self) -> StmtGuard { StmtGuard::new(&mut self.base_delete_rowid) }
    pub fn pk_lookup_by_key(&mut self) -> StmtGuard { StmtGuard::new(&mut self.pk_lookup_by_key) }
    /// Get a per-column base table UPDATE statement.
    /// All statements are pre-prepared at V2Stmts construction time.
    pub fn base_update(&mut self, col_name: &str) -> Result<StmtGuard, ResultCode> {
        Ok(StmtGuard::new(self.base_updates.get_mut(col_name).ok_or(ResultCode::ERROR)?))
    }
    pub fn v1_sentinel_lookup(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_sentinel_lookup.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_any_clock_lookup(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_any_clock_lookup.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_sentinel_detail(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_sentinel_detail.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_clock_delete(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_clock_delete.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_sentinel_insert_alive(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_sentinel_insert_alive.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_sentinel_insert_dead(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_sentinel_insert_dead.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn v1_clock_copy(&mut self) -> Result<StmtGuard, ResultCode> {
        self.v1_clock_copy.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn hydrate_clock_copy(&mut self) -> Result<StmtGuard, ResultCode> {
        self.hydrate_clock_copy.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }

    pub fn tomb_pks_delete(&mut self) -> Result<StmtGuard, ResultCode> {
        self.tomb_pks_delete.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn tomb_pks_insert(&mut self) -> Result<StmtGuard, ResultCode> {
        self.tomb_pks_insert.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn base_delete_nonrowid(&mut self) -> Result<StmtGuard, ResultCode> {
        self.base_delete_nonrowid.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn base_lookup_rowid(&mut self) -> Result<StmtGuard, ResultCode> {
        self.base_lookup_rowid.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn lookup_pks_tomb(&mut self) -> Result<StmtGuard, ResultCode> {
        self.lookup_pks_tomb.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }
    pub fn lookup_pks_alive(&mut self) -> Result<StmtGuard, ResultCode> {
        self.lookup_pks_alive.as_mut().map(StmtGuard::new).ok_or(ResultCode::ERROR)
    }

    /// The merge_equal value baked into these statements.
    pub fn merge_equal(&self) -> i32 {
        self.merge_equal
    }
}

// --- SQL builder helpers ---

/// Build the per-column clock merge upsert SQL.
/// Each column needs its own statement because the subquery embeds the column name
/// to fetch the current value for crsql_change_wins.
fn clock_merge_upsert_sql(tbl_info: &TableInfo, escaped: &str, col_name: &str) -> String {
    let escaped_col = crate::util::escape_ident(col_name);
    let subquery = if tbl_info.key_is_rowid {
        let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        format!("SELECT \"{escaped_col}\" FROM \"{escaped}\" WHERE \"{alias}\" = ?")
    } else {
        let pk_where: Vec<String> = tbl_info.pks.iter()
            .map(|c| format!("\"{}\" = ?", crate::util::escape_ident(&c.name)))
            .collect();
        format!("SELECT \"{escaped_col}\" FROM \"{escaped}\" WHERE {}", pk_where.join(" AND "))
    };
    format!(
        "INSERT INTO \"{escaped}{suffix}\" (cell_key, col_version, site_id, db_version, seq, ts) \
        VALUES (?, ?, ?, ?, ?, ?) \
        ON CONFLICT(cell_key) DO UPDATE SET \
        col_version = excluded.col_version, site_id = excluded.site_id, \
        db_version = excluded.db_version, seq = excluded.seq, ts = excluded.ts \
        WHERE excluded.col_version > col_version \
        OR (excluded.col_version = col_version AND \
        crsql_change_wins(?, ({subquery}), \
        ? > (SELECT site_id FROM crsql_site_id WHERE ordinal = site_id), ?)) \
        RETURNING cell_key",
        escaped = escaped,
        suffix = consts::V2_CLOCK_SUFFIX,
        subquery = subquery,
    )
}

/// Build a per-column base table UPDATE statement.
/// V2 guarantees the row exists, so a plain UPDATE is more efficient than
/// INSERT ... ON CONFLICT DO UPDATE (no failed INSERT attempt).
fn base_update_sql(tbl_info: &TableInfo, escaped: &str, col_name: &str) -> String {
    let escaped_col = crate::util::escape_ident(col_name);
    if tbl_info.key_is_rowid {
        let alias = crate::util::escape_ident(&tbl_info.rowid_alias);
        format!("UPDATE \"{escaped}\" SET \"{escaped_col}\" = ? WHERE \"{alias}\" = ?")
    } else {
        let pk_where: Vec<String> = tbl_info.pks.iter()
            .map(|c| format!("\"{}\" = ?", crate::util::escape_ident(&c.name)))
            .collect();
        format!("UPDATE \"{escaped}\" SET \"{escaped_col}\" = ? WHERE {}", pk_where.join(" AND "))
    }
}

fn v2_pk_lookup_where(tbl_info: &TableInfo, is_pks_table: bool) -> (String, bool) {
    if tbl_info.skip_hash {
        if is_pks_table && tbl_info.key_is_rowid {
            ("__crsql_key = ?".to_string(), false)
        } else {
            (format!("\"{}\" = ?", tbl_info.skip_hash_pk_col), false)
        }
    } else {
        ("hashed_pk = ?".to_string(), true)
    }
}

pub fn v2_pks_insert_sql(
    skip_hash: bool,
    key_is_rowid: bool,
    escaped: &str,
    pk_cols: &str,
    pk_values: &str,
    cl_expr: &str,
) -> String {
    let suffix = consts::V2_PKS_SUFFIX;
    if skip_hash && key_is_rowid {
        format!("INSERT INTO \"{escaped}{suffix}\" (__crsql_key, cl) VALUES (?, {cl_expr}) RETURNING __crsql_key")
    } else if skip_hash {
        format!("INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, cl) VALUES ({pk_values}, {cl_expr}) RETURNING __crsql_key")
    } else if key_is_rowid {
        format!("INSERT INTO \"{escaped}{suffix}\" (__crsql_key, hashed_pk, cl) VALUES (?, ?, {cl_expr}) RETURNING __crsql_key")
    } else {
        format!("INSERT INTO \"{escaped}{suffix}\" ({pk_cols}, hashed_pk, cl) VALUES ({pk_values}, ?, {cl_expr}) RETURNING __crsql_key")
    }
}

pub fn pk_cols_and_values(pks: &[ColumnInfo]) -> (String, String) {
    let pk_cols = pks.iter()
        .map(|p| format!("\"{}\"", crate::util::escape_ident(&p.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let pk_values = pks.iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    (pk_cols, pk_values)
}
