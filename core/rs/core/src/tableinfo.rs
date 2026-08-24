use crate::alloc::string::ToString;
use crate::c::crsql_ExtData;
use crate::c::crsql_fetchPragmaSchemaVersion;
use crate::c::TABLE_INFO_SCHEMA_VERSION;
use crate::consts;
use crate::pack_columns::bind_package_to_stmt;
use crate::pack_columns::ColumnValue;
use crate::stmt_cache::reset_cached_stmt;
use crate::util::Countable;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::Ref;
use core::cell::RefCell;
use core::cell::RefMut;
use core::ffi::c_char;
use core::ffi::c_int;
use core::ffi::c_void;
use core::mem::forget;
use num_traits::ToPrimitive;
use sqlite::sqlite3;
use sqlite::value;
use sqlite_nostd as sqlite;
use sqlite_nostd::Connection;
use sqlite_nostd::ManagedStmt;
use sqlite_nostd::ResultCode;
use sqlite_nostd::Stmt;
use sqlite_nostd::StrRef;

// TODO: make this configurable with a crsql_config_set.
const MAX_CL_CACHE_SIZE: usize = 1500;

/// Which metadata schema version is active for a table.
/// Set at TableInfo creation time based on config flags and which tables physically exist.
#[derive(PartialEq, Debug, Copy, Clone)]
pub enum SchemaVersion {
    V1,
    V2,
    V2AndV1,
}

pub struct TableInfo {
    pub tbl_name: String,
    pub pks: Vec<ColumnInfo>,
    pub non_pks: Vec<ColumnInfo>,
    pub schema_version: SchemaVersion,
    /// True when the main table is a rowid table with accessible rowid.
    /// True when __crsql_key in v2_pks is the SQLite rowid of the base table.
    /// This means v2_pks uses the compact schema (__crsql_key, [pk_cols], hashed_pk, cl)
    /// where __crsql_key = rowid, and PK values are fetched from the base table via
    /// SELECT pk_cols WHERE rowid = ?.
    ///
    /// When false (WITHOUT ROWID tables), __crsql_key is an auto-incremented integer
    /// and PK columns are stored directly in v2_pks.
    ///
    /// Relationship with has_integer_pk:
    ///   key_is_rowid = true  → __crsql_key = rowid
    ///   has_integer_pk = true → PK value = rowid
    ///   Both true            → __crsql_key = PK value (can use PK directly, no JOIN)
    ///   key_is_rowid only    → need JOIN to map PK → rowid → __crsql_key
    pub key_is_rowid: bool,
    /// True when the table has an INTEGER PRIMARY KEY column.
    /// In SQLite, `INTEGER PRIMARY KEY` is a rowid alias — the PK value IS the rowid.
    /// When combined with key_is_rowid, this means __crsql_key = PK value, so
    /// unpacked_pks[0] can be used directly as __crsql_key without any JOIN.
    /// See key_is_rowid doc for the full relationship matrix.
    pub has_integer_pk: bool,
    /// The column name to use as the rowid alias for JOINs/ad-hoc queries.
    /// For INTEGER PRIMARY KEY tables: the PK column name (e.g. "id").
    /// For plain rowid tables: "rowid" (or first unshadowed built-in alias).
    /// Only valid when key_is_rowid is true.
    pub rowid_alias: String,
    /// True when hashing is skipped for this table's PK.
    /// Tombstones store the PK value directly (no hashed_pk BLOB column).
    /// v2_tombstone_pks table is not created. Lookups use the PK value
    /// directly instead of a hash. Independent of key_is_rowid.
    /// Auto-qualified for single integer-affinity PKs; can be manually enabled
    /// for other single-column PKs via schema directive or as_crr option.
    /// Requires single-column PK — composite PKs fall back to hash mode.
    pub skip_hash: bool,
    /// Pre-computed escaped single PK column name for skip_hash mode.
    /// Only valid when skip_hash is true (which requires pks.len() == 1).
    pub skip_hash_pk_col: String,

    // Lookaside --
    // insert returning?
    // select?
    // insert or ignore returning followed by select?
    // or selecet first?
    select_key_stmt: RefCell<Option<ManagedStmt>>,
    insert_key_stmt: RefCell<Option<ManagedStmt>>,
    insert_or_ignore_returning_key_stmt: RefCell<Option<ManagedStmt>>,

    // For merges --
    set_winner_clock_stmt: RefCell<Option<ManagedStmt>>,
    local_cl_stmt: RefCell<Option<ManagedStmt>>,
    col_version_stmt: RefCell<Option<ManagedStmt>>,
    col_site_id_stmt: RefCell<Option<ManagedStmt>>,
    merge_pk_only_insert_stmt: RefCell<Option<ManagedStmt>>,
    merge_delete_stmt: RefCell<Option<ManagedStmt>>,
    merge_delete_drop_clocks_stmt: RefCell<Option<ManagedStmt>>,
    // We zero clocks, rather than going to 1, because
    // the current values should be totally ignored at all sites.
    // This is because the current values would not exist had the current node
    // processed the intervening delete.
    // This also means that col_version is not always >= 1. A resurrected column,
    // which missed a delete event, will have a 0 version.
    zero_clocks_on_resurrect_stmt: RefCell<Option<ManagedStmt>>,

    // For local writes --
    combo_insert_clock_stmt: RefCell<Option<ManagedStmt>>,
    select_clock_stmt: RefCell<Option<ManagedStmt>>,
    insert_clock_stmt: RefCell<Option<ManagedStmt>>,
    update_clock_stmt: RefCell<Option<ManagedStmt>>,
    mark_locally_deleted_stmt: RefCell<Option<ManagedStmt>>,
    move_non_sentinels_stmt: RefCell<Option<ManagedStmt>>,
    mark_locally_created_stmt: RefCell<Option<ManagedStmt>>,
    maybe_mark_locally_reinserted_stmt: RefCell<Option<ManagedStmt>>,
    cl_cache: BTreeMap<i64, i64>,
    /// Cached V2 prepared statements. None if table is not V2-enabled,
    /// or if statements haven't been prepared yet.
    v2_stmts: RefCell<Option<crate::v2_stmts::V2Stmts>>,
}

impl TableInfo {
    pub fn get_cl(&self, key: i64) -> Option<&i64> {
        self.cl_cache.get(&key)
    }

    pub fn set_cl(&mut self, key: i64, cl: i64) {
        // clear the cache if we are over limit
        if self.cl_cache.len() >= MAX_CL_CACHE_SIZE {
            self.cl_cache.clear();
        }
        self.cl_cache.insert(key, cl);
    }

    pub fn clear_cl_cache(&mut self) {
        if !self.cl_cache.is_empty() {
            self.cl_cache.clear();
        }
    }

    fn find_non_pk_col(&self, col_name: &str) -> Result<&ColumnInfo, ResultCode> {
        for col in &self.non_pks {
            if col.name == col_name {
                return Ok(col);
            }
        }
        Err(ResultCode::ERROR)
    }

    pub fn get_or_create_key(
        &self,
        db: *mut sqlite3,
        pks: &Vec<ColumnValue>,
    ) -> Result<sqlite::int64, ResultCode> {
        let stmt_ref = self.get_select_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        bind_package_to_stmt(stmt.stmt, pks, 0)?;
        match stmt.step() {
            Ok(ResultCode::DONE) => {
                // create it
                reset_cached_stmt(stmt.stmt)?;
                let ret = self.create_key(db, pks)?;
                return Ok(ret);
            }
            Ok(ResultCode::ROW) => {
                // return it
                let ret = stmt.column_int64(0);
                reset_cached_stmt(stmt.stmt)?;
                return Ok(ret);
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(stmt.stmt)?;
                return Err(rc);
            }
        }
    }

    pub fn get_or_create_key_via_raw_values(
        &self,
        db: *mut sqlite3,
        pks: &[*mut value],
    ) -> Result<sqlite::int64, ResultCode> {
        let stmt_ref = self.get_select_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        for (i, pk) in pks.iter().enumerate() {
            if let Err(rc) = stmt.bind_value(i as i32 + 1, *pk) {
                stmt.clear_bindings()?;
                return Err(rc);
            }
        }
        match stmt.step() {
            Ok(ResultCode::DONE) => {
                // create it
                reset_cached_stmt(stmt.stmt)?;
                let ret = self.create_key_via_raw_values(db, pks)?;
                return Ok(ret);
            }
            Ok(ResultCode::ROW) => {
                // return it
                let ret = stmt.column_int64(0);
                reset_cached_stmt(stmt.stmt)?;
                return Ok(ret);
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(stmt.stmt)?;
                return Err(rc);
            }
        }
    }

    pub fn get_or_create_key_for_insert(
        &self,
        db: *mut sqlite3,
        pks: &[*mut value],
    ) -> Result<(bool, sqlite::int64), ResultCode> {
        let stmt_ref = self.get_insert_or_ignore_returning_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        for (i, pk) in pks.iter().enumerate() {
            if let Err(rc) = stmt.bind_value(i as i32 + 1, *pk) {
                stmt.clear_bindings()?;
                return Err(rc);
            }
        }
        match stmt.step() {
            Ok(ResultCode::DONE) => {
                // already exists, get it
                reset_cached_stmt(stmt.stmt)?;
                let stmt_ref = self.get_select_key_stmt(db)?;
                let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
                for (i, pk) in pks.iter().enumerate() {
                    stmt.bind_value(i as i32 + 1, *pk)?;
                }
                let ret = stmt.step()?;
                match ret {
                    ResultCode::ROW => {
                        let ret = stmt.column_int64(0);
                        reset_cached_stmt(stmt.stmt)?;
                        Ok((true, ret))
                    }
                    _ => {
                        reset_cached_stmt(stmt.stmt)?;
                        Err(ret)
                    }
                }
            }
            Ok(ResultCode::ROW) => {
                // return it
                let ret = stmt.column_int64(0);
                reset_cached_stmt(stmt.stmt)?;
                Ok((false, ret))
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(stmt.stmt)?;
                Err(rc)
            }
        }
    }

    fn create_key(
        &self,
        db: *mut sqlite3,
        pks: &Vec<ColumnValue>,
    ) -> Result<sqlite::int64, ResultCode> {
        let stmt_ref = self.get_insert_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        bind_package_to_stmt(stmt.stmt, pks, 0)?;
        match stmt.step() {
            Ok(ResultCode::ROW) => {
                // return it
                let ret = stmt.column_int64(0);
                reset_cached_stmt(stmt.stmt)?;
                return Ok(ret);
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(stmt.stmt)?;
                return Err(rc);
            }
        }
    }

    fn create_key_via_raw_values(
        &self,
        db: *mut sqlite3,
        pks: &[*mut value],
    ) -> Result<sqlite::int64, ResultCode> {
        let stmt_ref = self.get_insert_key_stmt(db)?;
        let stmt = stmt_ref.as_ref().ok_or(ResultCode::ERROR)?;
        for (i, pk) in pks.iter().enumerate() {
            if let Err(rc) = stmt.bind_value(i as i32 + 1, *pk) {
                stmt.clear_bindings()?;
                return Err(rc);
            }
        }
        match stmt.step() {
            Ok(ResultCode::ROW) => {
                // return it
                let ret = stmt.column_int64(0);
                reset_cached_stmt(stmt.stmt)?;
                return Ok(ret);
            }
            Ok(rc) | Err(rc) => {
                reset_cached_stmt(stmt.stmt)?;
                return Err(rc);
            }
        }
    }

    // TODO: macro-ify all these
    pub fn get_select_key_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.select_key_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "SELECT __crsql_key FROM \"{table_name}__crsql_pks\" WHERE {pk_where_list}",
                table_name = crate::util::escape_ident(&self.tbl_name),
                pk_where_list = crate::util::where_list(&self.pks, None)?,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.select_key_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.select_key_stmt.try_borrow()?)
    }

    pub fn get_insert_key_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.insert_key_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT INTO \"{table_name}__crsql_pks\" ({pk_list}) VALUES ({pk_bindings}) RETURNING __crsql_key",
                table_name = crate::util::escape_ident(&self.tbl_name),
                pk_list = crate::util::as_identifier_list(&self.pks, None)?,
                pk_bindings = crate::util::binding_list(self.pks.len()),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.insert_key_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.insert_key_stmt.try_borrow()?)
    }

    pub fn get_insert_or_ignore_returning_key_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self
            .insert_or_ignore_returning_key_stmt
            .try_borrow()?
            .is_none()
        {
            let sql = format!(
                "INSERT OR IGNORE INTO \"{table_name}__crsql_pks\" ({pk_list}) VALUES ({pk_bindings}) RETURNING __crsql_key",
                table_name = crate::util::escape_ident(&self.tbl_name),
                pk_list = crate::util::as_identifier_list(&self.pks, None)?,
                pk_bindings = crate::util::binding_list(self.pks.len()),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.insert_or_ignore_returning_key_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.insert_or_ignore_returning_key_stmt.try_borrow()?)
    }

    pub fn get_set_winner_clock_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.set_winner_clock_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT OR REPLACE INTO \"{table_name}__crsql_clock\"
              (key, col_name, col_version, db_version, seq, site_id, ts)
              VALUES (
                ?,
                ?,
                ?,
                ?,
                ?,
                ?,
                ?
              ) RETURNING key",
                table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.set_winner_clock_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.set_winner_clock_stmt.try_borrow()?)
    }

    pub fn get_local_cl_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.local_cl_stmt.try_borrow()?.is_none() {
            // prepare it
            let sql = format!(
              "SELECT COALESCE(
                (SELECT col_version FROM \"{table_name}__crsql_clock\" WHERE key = ? AND col_name = '{delete_sentinel}'),
                (SELECT 1 FROM \"{table_name}__crsql_clock\" WHERE key = ?)
              )",
              table_name = crate::util::escape_ident(&self.tbl_name),
              delete_sentinel = crate::c::DELETE_SENTINEL,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.local_cl_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.local_cl_stmt.try_borrow()?)
    }

    pub fn get_col_version_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.col_version_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "SELECT col_version FROM \"{table_name}__crsql_clock\" WHERE key = ? AND col_name = ?",
              table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.col_version_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.col_version_stmt.try_borrow()?)
    }

    pub fn get_col_site_id_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.col_site_id_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "SELECT site_id FROM crsql_site_id WHERE ordinal = (SELECT site_id FROM \"{table_name}__crsql_clock\" WHERE key = ? AND col_name = ?)",
              table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.col_site_id_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.col_site_id_stmt.try_borrow()?)
    }

    pub fn get_merge_pk_only_insert_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.merge_pk_only_insert_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT OR IGNORE INTO \"{table_name}\" ({pk_idents}) VALUES ({pk_bindings})",
                table_name = crate::util::escape_ident(&self.tbl_name),
                pk_idents = crate::util::as_identifier_list(&self.pks, None)?,
                pk_bindings = crate::util::binding_list(self.pks.len()),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.merge_pk_only_insert_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.merge_pk_only_insert_stmt.try_borrow()?)
    }

    pub fn get_merge_delete_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.merge_delete_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "DELETE FROM \"{table_name}\" WHERE {pk_where_list}",
                table_name = crate::util::escape_ident(&self.tbl_name),
                pk_where_list = crate::util::where_list(&self.pks, None)?,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.merge_delete_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.merge_delete_stmt.try_borrow()?)
    }

    pub fn get_merge_delete_drop_clocks_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.merge_delete_drop_clocks_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "DELETE FROM \"{table_name}__crsql_clock\" WHERE key = ? AND col_name IS NOT '{sentinel}'",
              table_name = crate::util::escape_ident(&self.tbl_name),
              sentinel = crate::c::DELETE_SENTINEL
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.merge_delete_drop_clocks_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.merge_delete_drop_clocks_stmt.try_borrow()?)
    }

    pub fn get_zero_clocks_on_resurrect_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.zero_clocks_on_resurrect_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "UPDATE \"{table_name}__crsql_clock\" SET col_version = 0 WHERE key = ? AND col_name IS NOT '{sentinel}'",
              table_name = crate::util::escape_ident(&self.tbl_name),
              sentinel = crate::c::INSERT_SENTINEL
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.zero_clocks_on_resurrect_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.zero_clocks_on_resurrect_stmt.try_borrow()?)
    }

    pub fn get_mark_locally_deleted_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.mark_locally_deleted_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT INTO \"{table_name}__crsql_clock\" (
            key,
            col_name,
            col_version,
            db_version,
            seq,
            site_id,
            ts
          ) SELECT
            ?,
            '{sentinel}',
            2,
            ?,
            ?,
            0,
            ? WHERE true
          ON CONFLICT DO UPDATE SET
            col_version = 1 + col_version,
            db_version = excluded.db_version,
            seq = excluded.seq,
            site_id = 0,
            ts = excluded.ts
          RETURNING col_version",
                table_name = crate::util::escape_ident(&self.tbl_name),
                sentinel = crate::c::DELETE_SENTINEL,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.mark_locally_deleted_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.mark_locally_deleted_stmt.try_borrow()?)
    }

    #[allow(dead_code)]
    pub fn get_move_non_sentinels_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.move_non_sentinels_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "UPDATE OR REPLACE \"{table_name}__crsql_clock\" SET key = ? WHERE key = ? AND col_name != '{sentinel}'",
              table_name = crate::util::escape_ident(&self.tbl_name),
              sentinel = crate::c::DELETE_SENTINEL,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.move_non_sentinels_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.move_non_sentinels_stmt.try_borrow()?)
    }

    pub fn get_move_non_pk_col_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.move_non_sentinels_stmt.try_borrow()?.is_none() {
            // Incrementing col_version is especially important for the case where we
            // are updating to a currently existing pk, so that the columns
            // from the old pk can override the ones from the new at a node
            // following our changes.
            let sql = format!(
                "UPDATE OR REPLACE \"{table_name}__crsql_clock\" SET
                key = ?,
                db_version = ?,
                seq = ?,
                col_version = col_version + 1,
                site_id = 0,
                ts = ?
            WHERE
                key = ? AND col_name = ?",
                table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.move_non_sentinels_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.move_non_sentinels_stmt.try_borrow()?)
    }

    pub fn get_mark_locally_created_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.mark_locally_created_stmt.try_borrow()?.is_none() {
            let sql = format!(
              "INSERT INTO \"{table_name}__crsql_clock\" (
                key,
                col_name,
                col_version,
                db_version,
                seq,
                site_id,
                ts
              ) SELECT
                ?,
                '{sentinel}',
                1,
                ?,
                ?,
                0,
                ? WHERE true
                ON CONFLICT DO UPDATE SET
                  col_version = CASE col_version % 2 WHEN 0 THEN col_version + 1 ELSE col_version + 2 END,
                  db_version = excluded.db_version,
                  seq = excluded.seq,
                  site_id = 0,
                  ts = excluded.ts
                  RETURNING col_version",
              table_name = crate::util::escape_ident(&self.tbl_name),
              sentinel = crate::c::INSERT_SENTINEL,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.mark_locally_created_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.mark_locally_created_stmt.try_borrow()?)
    }

    pub fn get_combo_insert_clock_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.combo_insert_clock_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT OR IGNORE INTO \"{table_name}__crsql_clock\" (
                    key, col_name, col_version, db_version, seq, site_id, ts
                ) VALUES {values};",
                values = self
                    .non_pks
                    .iter()
                    .map(|_col| "(?, ?, 1, ?, ?, 0, ?)")
                    .collect::<Vec<_>>()
                    .join(", "),
                table_name = crate::util::escape_ident(&self.tbl_name)
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.combo_insert_clock_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.combo_insert_clock_stmt.try_borrow()?)
    }

    #[allow(dead_code)]
    pub fn get_select_clock_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.select_clock_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "SELECT 1 FROM \"{table_name}__crsql_clock\"
                    WHERE key = ? AND col_name = ? LIMIT 1;",
                table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.select_clock_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.select_clock_stmt.try_borrow()?)
    }

    pub fn get_insert_clock_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.insert_clock_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT INTO \"{table_name}__crsql_clock\" (
                    key, col_name, col_version, db_version, seq, site_id, ts
                ) VALUES (?, ?, 1, ?, ?, 0, ?);",
                table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.insert_clock_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.insert_clock_stmt.try_borrow()?)
    }

    pub fn get_update_clock_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.update_clock_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "UPDATE \"{table_name}__crsql_clock\"
                SET
                    col_version = col_version + 1,
                    db_version = ?,
                    seq = ?,
                    site_id = 0,
                    ts = ?
                WHERE key = ? AND col_name = ?;",
                table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.update_clock_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.update_clock_stmt.try_borrow()?)
    }

    pub fn get_maybe_mark_locally_reinserted_stmt(
        &self,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self
            .maybe_mark_locally_reinserted_stmt
            .try_borrow()?
            .is_none()
        {
            let sql = format!(
              "UPDATE \"{table_name}__crsql_clock\" SET
                col_version = CASE col_version % 2 WHEN 0 THEN col_version + 1 ELSE col_version + 2 END,
                db_version = ?,
                seq = ?,
                site_id = 0,
                ts = ?
              WHERE key = ? AND col_name = ?
              RETURNING col_version",
              table_name = crate::util::escape_ident(&self.tbl_name),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.maybe_mark_locally_reinserted_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.maybe_mark_locally_reinserted_stmt.try_borrow()?)
    }

    pub fn get_col_value_stmt(
        &self,
        db: *mut sqlite3,
        col_name: &str,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        let col_info = self.find_non_pk_col(col_name)?;
        col_info.get_curr_value_stmt(self, db)
    }

    pub fn get_merge_insert_stmt(
        &self,
        db: *mut sqlite3,
        col_name: &str,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        let col_info = self.find_non_pk_col(col_name)?;
        col_info.get_merge_insert_stmt(self, db)
    }

    pub fn get_row_patch_data_stmt(
        &self,
        db: *mut sqlite3,
        col_name: &str,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        let col_info = self.find_non_pk_col(col_name)?;
        col_info.get_row_patch_data_stmt(self, db)
    }

    /// Get or lazily prepare V2 cached statements.
    /// If merge_equal has changed since last prepare, re-prepares.
    /// The caller must hold the borrow for the duration of statement use.
    pub fn get_v2_stmts(
        &self,
        db: *mut sqlite3,
        ext_data: *mut crate::c::crsql_ExtData,
    ) -> Result<RefMut<Option<crate::v2_stmts::V2Stmts>>, ResultCode> {
        let merge_equal = unsafe { (*ext_data).mergeEqualValues };
        let needs_prepare = match self.v2_stmts.try_borrow()?.as_ref() {
            None => true,
            Some(s) => s.merge_equal() != merge_equal,
        };
        if needs_prepare {
            let stmts = crate::v2_stmts::V2Stmts::prepare(db, self, merge_equal)?;
            *self.v2_stmts.try_borrow_mut()? = Some(stmts);
        }
        Ok(self.v2_stmts.try_borrow_mut()?)
    }

    pub fn clear_stmts(&self) -> Result<ResultCode, ResultCode> {
        // finalize all stmts
        let mut stmt = self.set_winner_clock_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.local_cl_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.col_version_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.merge_pk_only_insert_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.merge_delete_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.merge_delete_drop_clocks_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.zero_clocks_on_resurrect_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.mark_locally_deleted_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.move_non_sentinels_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.mark_locally_created_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.select_clock_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.combo_insert_clock_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.insert_clock_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.update_clock_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.maybe_mark_locally_reinserted_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.insert_key_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.insert_or_ignore_returning_key_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.select_key_stmt.try_borrow_mut()?;
        stmt.take();

        // V2 cached statements
        let mut v2 = self.v2_stmts.try_borrow_mut()?;
        v2.take();

        // primary key columns shouldn't have statements? right?
        for col in &self.non_pks {
            col.clear_stmts()?;
        }

        Ok(ResultCode::OK)
    }
}

impl Drop for TableInfo {
    fn drop(&mut self) {
        // we'll leak rather than panic
        let _ = self.clear_stmts();
    }
}

pub struct ColumnInfo {
    pub cid: i32,
    pub name: String,
    pub col_type: String,
    // > 0 if it is a primary key columns
    // the value refers to the position in the `PRIMARY KEY (cols...)` statement
    pub pk: i32,
    // can we one day delete this and use site id for ties?
    // if we do, how does that impact the backup and restore story?
    // e.g., restoring a database snapshot on a new machine with a new siteid but
    // bootstrapped from a backup?
    // If we track that "we've seen this restored node since the backup point with the old site_id"
    // then site_id comparisons could change merge results after restore for nodes that
    // have different "seen since" records for the old site_id.
    curr_value_stmt: RefCell<Option<ManagedStmt>>,
    merge_insert_stmt: RefCell<Option<ManagedStmt>>,
    row_patch_data_stmt: RefCell<Option<ManagedStmt>>,
}

impl ColumnInfo {
    fn get_curr_value_stmt(
        &self,
        tbl_info: &TableInfo,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.curr_value_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "SELECT \"{col_name}\" FROM \"{table_name}\" WHERE {pk_where_list}",
                col_name = crate::util::escape_ident(&self.name),
                table_name = crate::util::escape_ident(&tbl_info.tbl_name),
                pk_where_list = crate::util::where_list(&tbl_info.pks, None)?,
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.curr_value_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.curr_value_stmt.try_borrow()?)
    }

    fn get_merge_insert_stmt(
        &self,
        tbl_info: &TableInfo,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.merge_insert_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "INSERT INTO \"{table_name}\" ({pk_list}, \"{col_name}\")
                VALUES ({pk_bind_list}, ?)
                ON CONFLICT DO UPDATE
                SET \"{col_name}\" = ?",
                table_name = crate::util::escape_ident(&tbl_info.tbl_name),
                pk_list = crate::util::as_identifier_list(&tbl_info.pks, None)?,
                col_name = crate::util::escape_ident(&self.name),
                pk_bind_list = crate::util::binding_list(tbl_info.pks.len()),
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.merge_insert_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.merge_insert_stmt.try_borrow()?)
    }

    fn get_row_patch_data_stmt(
        &self,
        tbl_info: &TableInfo,
        db: *mut sqlite3,
    ) -> Result<Ref<Option<ManagedStmt>>, ResultCode> {
        if self.row_patch_data_stmt.try_borrow()?.is_none() {
            let sql = format!(
                "SELECT \"{col_name}\" FROM \"{table_name}\" WHERE {where_list}\0",
                col_name = crate::util::escape_ident(&self.name),
                table_name = crate::util::escape_ident(&tbl_info.tbl_name),
                where_list = crate::util::where_list(&tbl_info.pks, None)?
            );
            let ret = db.prepare_v3(&sql, sqlite::PREPARE_PERSISTENT)?;
            *self.row_patch_data_stmt.try_borrow_mut()? = Some(ret);
        }
        Ok(self.row_patch_data_stmt.try_borrow()?)
    }

    pub fn clear_stmts(&self) -> Result<ResultCode, ResultCode> {
        let mut stmt = self.curr_value_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.merge_insert_stmt.try_borrow_mut()?;
        stmt.take();
        let mut stmt = self.row_patch_data_stmt.try_borrow_mut()?;
        stmt.take();

        Ok(ResultCode::OK)
    }
}

impl Drop for ColumnInfo {
    fn drop(&mut self) {
        // we'll leak rather than panic
        let _ = self.clear_stmts();
    }
}

#[no_mangle]
pub extern "C" fn crsql_init_table_info_vec(ext_data: *mut crsql_ExtData) {
    let vec: Vec<TableInfo> = vec![];
    unsafe { (*ext_data).tableInfos = Box::into_raw(Box::new(vec)) as *mut c_void }
}

#[no_mangle]
pub extern "C" fn crsql_drop_table_info_vec(ext_data: *mut crsql_ExtData) {
    unsafe {
        drop(Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>));
    }
}

#[no_mangle]
pub extern "C" fn crsql_ensure_table_infos_are_up_to_date(
    db: *mut sqlite::sqlite3,
    ext_data: *mut crsql_ExtData,
    err: *mut *mut c_char,
) -> c_int {
    let already_updated = unsafe { (*ext_data).updatedTableInfosThisTx == 1 };
    if already_updated {
        return ResultCode::OK as c_int;
    }

    let schema_changed =
        unsafe { crsql_fetchPragmaSchemaVersion(db, ext_data, TABLE_INFO_SCHEMA_VERSION) };

    if schema_changed < 0 {
        return ResultCode::ERROR as c_int;
    }

    let mut table_infos: Box<Vec<TableInfo>> =
        unsafe { Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>) };

    if schema_changed > 0 || table_infos.len() == 0 {
        match pull_all_table_infos(db, ext_data, err) {
            Ok(new_table_infos) => {
                *table_infos = new_table_infos;
                forget(table_infos);
                unsafe {
                    (*ext_data).updatedTableInfosThisTx = 1;
                }
                return ResultCode::OK as c_int;
            }
            Err(e) => {
                forget(table_infos);
                return e as c_int;
            }
        }
    }

    forget(table_infos);
    unsafe {
        (*ext_data).updatedTableInfosThisTx = 1;
    }
    return ResultCode::OK as c_int;
}

fn pull_all_table_infos(
    db: *mut sqlite::sqlite3,
    ext_data: *mut crsql_ExtData,
    err: *mut *mut c_char,
) -> Result<Vec<TableInfo>, ResultCode> {
    let mut clock_table_names = vec![];
    let stmt = unsafe { (*ext_data).pSelectClockTablesStmt };
    loop {
        match stmt.step() {
            Ok(ResultCode::ROW) => {
                clock_table_names.push(stmt.column_text(0).to_string());
            }
            Ok(ResultCode::DONE) => {
                stmt.reset()?;
                break;
            }
            Ok(rc) | Err(rc) => {
                stmt.reset()?;
                return Err(rc);
            }
        }
    }

    // Also discover V2-only CRR tables (mode 3 creates no V1 clock tables)
    let v2_stmt = db.prepare_v2(
        "SELECT tbl_name FROM sqlite_master WHERE type='table' AND tbl_name LIKE '%__crsql_v2_clock'",
    )?;
    loop {
        match v2_stmt.step() {
            Ok(ResultCode::ROW) => {
                clock_table_names.push(v2_stmt.column_text(0)?.to_string());
            }
            Ok(ResultCode::DONE) => {
                break;
            }
            Ok(rc) | Err(rc) => {
                return Err(rc);
            }
        }
    }

    let mut seen = alloc::collections::BTreeSet::new();
    let mut ret = vec![];
    for name in clock_table_names {
        // Strip either __crsql_clock (V1) or __crsql_v2_clock (V2) suffix
        let base_name = if name.ends_with("__crsql_v2_clock") {
            name[0..(name.len() - "__crsql_v2_clock".len())].to_string()
        } else if name.ends_with("__crsql_clock") {
            name[0..(name.len() - "__crsql_clock".len())].to_string()
        } else {
            continue;
        };
        if seen.contains(base_name.as_str()) {
            continue;
        }
        seen.insert(base_name.clone());
        ret.push(pull_table_info(db, &base_name, err)?);
    }

    Ok(ret)
}

/**
 * Given a table name, return the table info that describes that table.
 * TableInfo is a struct that represents the results
 * of pragma_table_info, pragma_index_list, pragma_index_info on a given table
 * and its indices as well as some extra fields to facilitate crr creation.
 */
pub fn pull_table_info(
    db: *mut sqlite::sqlite3,
    table: &str,
    err: *mut *mut c_char,
) -> Result<TableInfo, ResultCode> {
    let sql = format!("SELECT count(*) FROM pragma_table_info('{table}')");
    let columns_len = match db.prepare_v2(&sql).and_then(|stmt| {
        stmt.step()?;
        stmt.column_int(0).to_usize().ok_or(ResultCode::ERROR)
    }) {
        Ok(count) => count,
        Err(code) => {
            err.set(&format!("Failed to find columns for crr -- {table}"));
            return Err(code);
        }
    };

    let sql = format!(
        "SELECT \"cid\", \"name\", \"type\", \"pk\"
         FROM pragma_table_info('{table}') ORDER BY cid ASC"
    );
    let column_infos = match db.prepare_v2(&sql) {
        Ok(stmt) => {
            let mut cols: Vec<ColumnInfo> = vec![];

            while stmt.step()? == ResultCode::ROW {
                cols.push(ColumnInfo {
                    name: stmt.column_text(1)?.to_string(),
                    col_type: stmt.column_text(2)?.to_string(),
                    cid: stmt.column_int(0),
                    pk: stmt.column_int(3),
                    curr_value_stmt: RefCell::new(None),
                    merge_insert_stmt: RefCell::new(None),
                    row_patch_data_stmt: RefCell::new(None),
                });
            }

            if cols.len() != columns_len {
                err.set("Number of fetched columns did not match expected number of columns");
                return Err(ResultCode::ERROR);
            }
            cols
        }
        Err(code) => {
            err.set(&format!("Failed to prepare select for crr -- {table}"));
            return Err(code);
        }
    };

    // Check alias shadowing before partition consumes column_infos
    let has_rowid_col = column_infos.iter().any(|c| c.name == "rowid");
    let has_oid_col = column_infos.iter().any(|c| c.name == "oid");
    let has_rowid_under_col = column_infos.iter().any(|c| c.name == "_rowid_");
    let all_aliases_shadowed = has_rowid_col && has_oid_col && has_rowid_under_col;

    let (mut pks, non_pks): (Vec<_>, Vec<_>) = column_infos.into_iter().partition(|x| x.pk > 0);
    pks.sort_by_key(|x| x.pk);

    // Detect rowid key optimization per design doc §3:
    // 1. INTEGER PRIMARY KEY exists (pk > 0 AND type = 'INTEGER') → it IS the rowid alias.
    // 2. No INTEGER PRIMARY KEY, but none of rowid/oid/_rowid_ are shadowed → rowid accessible.
    // 3. All three aliases shadowed AND no INTEGER PRIMARY KEY → auto-increment fallback.
    let integer_pk = pks.iter().find(|pk| {
        let type_sql = format!(
            "SELECT type FROM pragma_table_info('{table}') WHERE name = '{pk_name}'",
            table = crate::util::escape_ident_as_value(table),
            pk_name = crate::util::escape_ident_as_value(&pk.name),
        );
        db.prepare_v2(&type_sql).and_then(|stmt| {
            stmt.step()?;
            Ok(stmt.column_text(0)?.to_string() == "INTEGER")
        }).unwrap_or(false)
    });

    // Determine the rowid alias to use for ad-hoc queries
    let rowid_alias = if let Some(pk) = integer_pk {
        // Case 1: INTEGER PRIMARY KEY — the PK column IS the rowid alias
        pk.name.clone()
    } else if !all_aliases_shadowed {
        // Case 2: pick first unshadowed built-in alias
        if !has_rowid_col {
            "rowid".to_string()
        } else if !has_oid_col {
            "oid".to_string()
        } else {
            "_rowid_".to_string()
        }
    } else {
        // Case 3: fallback — no alias
        String::new()
    };

    let rowid_accessible = integer_pk.is_some() || !all_aliases_shadowed;
    let has_integer_pk = integer_pk.is_some();

    // Verify rowid is actually accessible (table is not WITHOUT ROWID)
    let mut key_is_rowid = if integer_pk.is_some() {
        // INTEGER PRIMARY KEY: the PK column IS the rowid alias.
        // Verify it's accessible (table is not WITHOUT ROWID).
        db.prepare_v2(&format!(
            "SELECT \"{alias}\" FROM \"{escaped}\" LIMIT 0",
            alias = crate::util::escape_ident(&rowid_alias),
            escaped = crate::util::escape_ident(table),
        )).is_ok()
    } else {
        // No INTEGER PRIMARY KEY — the PK is NOT the rowid.
        // We can still use the hidden rowid as __crsql_key, but we must store PK columns separately.
        false
    };

    // Detect V2 metadata tables
    let has_v2 = crate::bootstrap_v2::has_v2_tables(db, table).unwrap_or(false);

    // Detect skip_hash: auto-qualified for single integer-affinity PK,
    // or manually enabled via schema directive / crsql_master flag.
    // skip_hash requires a single-column PK — composite PKs are not supported.
    // Auto-qualification: pks.len() == 1 AND PK type contains "INT".
    let auto_skip_hash = pks.len() == 1 && {
        let pk_type = &pks[0].col_type;
        pk_type.to_uppercase().contains("INT")
    };

    // Check for schema directive or persisted flag
    // Returns Some(true) = explicitly enabled, Some(false) = explicitly disabled, None = not set
    let manual_skip_hash: Option<bool> = if has_v2 {
        // v2_pks exists — infer from its schema (presence/absence of hashed_pk column)
        let v2_pks_name = format!("{}{}", crate::util::escape_ident_as_value(table), consts::V2_PKS_SUFFIX);
        let has_hashed_pk_stmt = db.prepare_v2(&format!(
            "SELECT count(*) FROM pragma_table_info('{name}') WHERE name = 'hashed_pk'",
            name = v2_pks_name,
        ));
        if let Ok(stmt) = has_hashed_pk_stmt {
            if stmt.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                Some(stmt.column_int(0) == 0) // no hashed_pk column → skip_hash mode
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // v2_pks doesn't exist yet — check crsql_master for skip_hash flag
        // persisted by create_crr, or check schema directive in sqlite_master
        let skip_hash_key = format!("skip_hash_{}\0", table);
        let stmt = db.prepare_v2("SELECT value FROM crsql_master WHERE key = ?\0");
        let mut persisted: Option<bool> = None;
        if let Ok(stmt) = stmt {
            if stmt.bind_text(1, &skip_hash_key, sqlite::Destructor::TRANSIENT).is_ok() {
                if stmt.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                    persisted = Some(stmt.column_int(0) == 1);
                }
            }
        }
        if let Some(p) = persisted {
            Some(p)
        } else {
            // Check schema directive in sqlite_master (tri-state)
            crate::schema_directive::read_skip_hash_directive_opt(db, table).unwrap_or(None)
        }
    };

    // skip_hash resolution:
    // - If v2_pks exists: manual_skip_hash is the source of truth (persisted schema).
    // - If v2_pks doesn't exist yet:
    //   - Explicit directive (Some) overrides auto-qualification.
    //   - No directive (None): auto-qualification applies.
    // Enforcement: skip_hash requires a single-column PK. If a composite PK table
    // has skip_hash explicitly enabled via directive, ignore it (fall back to hash mode).
    let skip_hash = if has_v2 {
        manual_skip_hash.unwrap_or(false)
    } else {
        match manual_skip_hash {
            Some(explicit) => {
                if explicit && pks.len() != 1 {
                    // Reject skip_hash=1 on composite PK tables — not supported.
                    false
                } else {
                    explicit
                }
            }
            None => auto_skip_hash,
        }
    };

    // Pre-compute escaped PK column name for skip_hash mode (requires single PK)
    let skip_hash_pk_col = if skip_hash && !pks.is_empty() {
        crate::util::escape_ident(&pks[0].name)
    } else {
        String::new()
    };

    // If v2_pks table exists, infer key_is_rowid from its schema.
    // The inference must account for skip_hash mode:
    // - Hash mode, rowid-key: 3 cols (__crsql_key, hashed_pk, cl)
    // - Hash mode, non-rowid: 3+N cols (__crsql_key, [pk_cols...], hashed_pk, cl)
    // - Skip-hash, rowid-key: 2 cols (__crsql_key, cl)
    // - Skip-hash, non-rowid: 2+N cols (__crsql_key, [pk_col], cl)
    if has_v2 {
        let v2_pks_name = format!("{}{}", crate::util::escape_ident_as_value(table), consts::V2_PKS_SUFFIX);
        let pks_count_stmt = db.prepare_v2(&format!(
            "SELECT count(*) FROM pragma_table_info('{name}')",
            name = v2_pks_name,
        ));
        if let Ok(stmt) = pks_count_stmt {
            if stmt.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                let col_count = stmt.column_int(0);
                if skip_hash {
                    // Skip-hash: 2 cols = rowid-key, >2 = non-rowid
                    key_is_rowid = col_count == 2;
                } else {
                    // Hash mode: 3 cols = rowid-key, >3 = non-rowid
                    key_is_rowid = col_count == 3;
                }
            }
        }
    } else {
        // v2_pks doesn't exist yet — check crsql_master for without_rowid flag
        // persisted by create_crr when the without_rowid option was used.
        let without_rowid_key = format!("without_rowid_{}\0", table);
        let stmt = db.prepare_v2("SELECT value FROM crsql_master WHERE key = ?\0");
        if let Ok(stmt) = stmt {
            if stmt.bind_text(1, &without_rowid_key, sqlite::Destructor::TRANSIENT).is_ok() {
                if stmt.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                    key_is_rowid = false;
                }
            }
        }
    }
    let has_v1 = {
        let stmt = db.prepare_v2(&format!(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND tbl_name = '{escaped}{suffix}'",
            escaped = crate::util::escape_ident_as_value(table),
            suffix = "__crsql_clock"
        ))?;
        stmt.step()? == ResultCode::ROW
    };
    let schema_version = if has_v2 && has_v1 {
        SchemaVersion::V2AndV1
    } else if has_v2 {
        SchemaVersion::V2
    } else {
        SchemaVersion::V1
    };

    Ok(TableInfo {
        tbl_name: table.to_string(),
        pks,
        non_pks,
        schema_version,
        key_is_rowid,
        has_integer_pk,
        rowid_alias,
        skip_hash,
        skip_hash_pk_col,
        set_winner_clock_stmt: RefCell::new(None),
        local_cl_stmt: RefCell::new(None),
        col_version_stmt: RefCell::new(None),
        col_site_id_stmt: RefCell::new(None),

        select_key_stmt: RefCell::new(None),
        insert_key_stmt: RefCell::new(None),
        insert_or_ignore_returning_key_stmt: RefCell::new(None),

        merge_pk_only_insert_stmt: RefCell::new(None),
        merge_delete_stmt: RefCell::new(None),
        merge_delete_drop_clocks_stmt: RefCell::new(None),
        zero_clocks_on_resurrect_stmt: RefCell::new(None),

        mark_locally_deleted_stmt: RefCell::new(None),
        move_non_sentinels_stmt: RefCell::new(None),
        mark_locally_created_stmt: RefCell::new(None),
        maybe_mark_locally_reinserted_stmt: RefCell::new(None),
        combo_insert_clock_stmt: RefCell::new(None),
        select_clock_stmt: RefCell::new(None),
        insert_clock_stmt: RefCell::new(None),
        update_clock_stmt: RefCell::new(None),
        cl_cache: BTreeMap::new(),
        v2_stmts: RefCell::new(None),
    })
}

pub fn is_table_compatible(
    db: *mut sqlite::sqlite3,
    table: &str,
    err: *mut *mut c_char,
) -> Result<bool, ResultCode> {
    // No unique indices besides primary key
    if db.count(&format!(
        "SELECT count(*) FROM pragma_index_list('{table}')
            WHERE \"origin\" != 'pk' AND \"unique\" = 1"
    ))? != 0
    {
        err.set(&format!(
            "Table {table} has unique indices besides\
                        the primary key. This is not allowed for CRRs"
        ));
        return Ok(false);
    }

    // Must have a primary key
    let valid_pks = db.count(&format!(
        // pragma_index_list does not include primary keys that alias rowid...
        // hence why we cannot use
        // `select * from pragma_index_list where origin = pk`
        "SELECT count(*) FROM pragma_table_info('{table}')
        WHERE \"pk\" > 0 AND \"notnull\" > 0"
    ))?;
    if valid_pks == 0 {
        err.set(&format!(
            "Table {table} has no primary key or primary key is nullable. \
            CRRs must have a non nullable primary key"
        ));
        return Ok(false);
    }

    // All primary keys have to be non-nullable
    if db.count(&format!(
        "SELECT count(*) FROM pragma_table_info('{table}') WHERE \"pk\" > 0"
    ))? != valid_pks
    {
        err.set(&format!(
            "Table {table} has composite primary key part of which is nullable. \
            CRRs must have a non nullable primary key"
        ));
        return Ok(false);
    }

    // No auto-increment primary keys
    let stmt = db.prepare_v2(&format!(
        "SELECT 1 FROM sqlite_master WHERE name = ? AND type = 'table' AND sql
            LIKE '%autoincrement%' limit 1"
    ))?;
    stmt.bind_text(1, table, sqlite::Destructor::STATIC)?;
    if stmt.step()? == ResultCode::ROW {
        err.set(&format!(
            "{table} has auto-increment primary keys. This is likely a mistake as two \
                concurrent nodes will assign unrelated rows the same primary key. \
                Either use a primary key that represents the identity of your row or \
                use a database friendly UUID such as UUIDv7"
        ));
        return Ok(false);
    };

    // No checked foreign key constraints
    if db.count(&format!(
        "SELECT count(*) FROM pragma_foreign_key_list('{table}')"
    ))? != 0
    {
        err.set(&format!(
            "Table {table} has checked foreign key constraints. \
            CRRs may have foreign keys but must not have \
            checked foreign key constraints as they can be violated \
            by row level security or replication."
        ));
        return Ok(false);
    }

    // Check for default value or nullable
    if db.count(&format!(
        "SELECT count(*) FROM pragma_table_xinfo('{table}')
        WHERE \"notnull\" = 1 AND \"dflt_value\" IS NULL AND \"pk\" = 0"
    ))? != 0
    {
        err.set(&format!(
            "Table {table} has a NOT NULL column without a DEFAULT VALUE. \
            This is not allowed as it prevents forwards and backwards \
            compatibility between schema versions. Make the column \
            nullable or assign a default value to it."
        ));
        return Ok(false);
    }

    return Ok(true);
}
