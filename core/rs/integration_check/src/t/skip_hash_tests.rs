extern crate alloc;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use libc_print::libc_println;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

/// Helper: run incremental maintenance until V2 migration is complete.
fn migrate_to_v2(db: &sqlite::ManagedConnection) -> Result<(), ResultCode> {
    db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0);
        if remaining < 0 {
            return Err(ResultCode::ERROR);
        }
        iterations += 1;
    }
    Ok(())
}

/// Helper: count columns in v2_pks table.
fn v2_pks_col_count(db: &sqlite::ManagedConnection, table: &str) -> i32 {
    let stmt = db.prepare_v2(&format!(
        "SELECT count(*) FROM pragma_table_info('{table}__crsql_v2_pks')",
        table = table
    ));
    match stmt {
        Ok(s) => {
            s.step().unwrap_or(ResultCode::DONE);
            s.column_int(0)
        }
        Err(_) => 0,
    }
}

/// Helper: check if v2_tombstone_pks table exists.
fn has_v2_tombstone_pks(db: &sqlite::ManagedConnection, table: &str) -> bool {
    let stmt = db.prepare_v2(&format!(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='{table}__crsql_v2_tombstone_pks'",
        table = table
    ));
    match stmt {
        Ok(s) => s.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW,
        Err(_) => false,
    }
}

/// Helper: check if v2_pks has a hashed_pk column.
fn v2_pks_has_hashed_pk(db: &sqlite::ManagedConnection, table: &str) -> bool {
    let stmt = db.prepare_v2(&format!(
        "SELECT count(*) FROM pragma_table_info('{table}__crsql_v2_pks') WHERE name = 'hashed_pk'",
        table = table
    ));
    match stmt {
        Ok(s) => {
            s.step().unwrap_or(ResultCode::DONE);
            s.column_int(0) == 1
        }
        Err(_) => false,
    }
}

/// Helper: get the pk signature from crsql_master (e.g. "ns:id:INTEGER", "rh:id:TEXT").
fn get_pk_signature(db: &sqlite::ManagedConnection, table: &str) -> String {
    let stmt = db.prepare_v2(&format!(
        "SELECT value FROM crsql_master WHERE key = 'v2_pks_{table}'",
        table = table
    ));
    match stmt {
        Ok(s) => {
            s.step().unwrap_or(ResultCode::DONE);
            s.column_text(0).unwrap_or("").to_string()
        }
        Err(_) => String::new(),
    }
}

// =============================================================================
// Detection tests
// =============================================================================

/// Single INTEGER PRIMARY KEY → auto-qualified for skip_hash.
/// v2_pks should have 3 columns (__crsql_key, "id", cl), no hashed_pk.
/// INTEGER PK → skip_hash + !key_is_rowid (avoids rowid overflow for large PK values).
fn test_auto_qualified_int_pk() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    let col_count = v2_pks_col_count(&db.db, "foo");
    assert!(col_count == 3, "int PK: expected 3 cols, got {}", col_count);
    assert!(!v2_pks_has_hashed_pk(&db.db, "foo"), "int PK: should not have hashed_pk");
    assert!(!has_v2_tombstone_pks(&db.db, "foo"), "int PK: should not have v2_tombstone_pks");
    libc_println!("  int PK: 3 cols, no hashed_pk, no tombstone_pks — PASS");
    Ok(())
}

/// TEXT PRIMARY KEY → not auto-qualified (no INT in type).
/// Non-rowid (implicit rowid is unstable under VACUUM) →
/// hash + non-rowid → 4 columns (__crsql_key, "id", hashed_pk, cl).
/// __crsql_key is auto-assigned, "id" stores the TEXT PK, hashed_pk stores its hash.
fn test_text_pk_not_auto_qualified() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    let col_count = v2_pks_col_count(&db.db, "foo");
    // hash + non-rowid: __crsql_key, "id", hashed_pk, cl = 4 cols
    assert!(col_count == 4, "text PK: expected 4 cols, got {}", col_count);
    assert!(v2_pks_has_hashed_pk(&db.db, "foo"), "text PK: should have hashed_pk");
    libc_println!("  text PK: 4 cols, has hashed_pk — PASS");
    Ok(())
}

/// Composite PK → not auto-qualified (more than 1 PK column).
/// v2_pks should have hashed_pk.
fn test_composite_pk_not_auto_qualified() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (a INTEGER NOT NULL, b INTEGER NOT NULL, x TEXT, PRIMARY KEY(a, b))")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    assert!(v2_pks_has_hashed_pk(&db.db, "foo"), "composite PK: should have hashed_pk");
    libc_println!("  composite PK: has hashed_pk — PASS");
    Ok(())
}

/// Schema directive /* crsql: skip_hash=1 */ on a TEXT PK → manually enabled.
fn test_schema_directive_enables_skip_hash() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo /* crsql: skip_hash=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    assert!(!v2_pks_has_hashed_pk(&db.db, "foo"), "directive: should not have hashed_pk");
    assert!(!has_v2_tombstone_pks(&db.db, "foo"), "directive: should not have tombstone_pks");
    libc_println!("  schema directive on text PK: no hashed_pk — PASS");
    Ok(())
}

/// Schema directive skip_hash=0 on an INT PK → explicitly disabled.
fn test_schema_directive_disables_skip_hash() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    assert!(v2_pks_has_hashed_pk(&db.db, "foo"), "directive skip_hash=0: should have hashed_pk");
    libc_println!("  schema directive skip_hash=0 on int PK: has hashed_pk — PASS");
    Ok(())
}

// =============================================================================
// Local write path tests
// =============================================================================

/// INSERT on skip_hash table: v2_pks should have the row with correct CL.
fn test_skip_hash_insert() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;

    // v2_pks should have 1 row with cl=1
    let stmt = db.db.prepare_v2("SELECT cl FROM foo__crsql_v2_pks WHERE __crsql_key = 1")?;
    stmt.step()?;
    assert!(stmt.column_int64(0) == 1, "insert: cl should be 1");

    // v2_clock should have 1 entry for col_id=0 (x column)
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 1, "insert: should have 1 clock entry");
    libc_println!("  insert: cl=1, 1 clock entry — PASS");
    Ok(())
}

/// UPDATE on skip_hash table: clock entries should be updated.
fn test_skip_hash_update() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT, y TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a', 'b')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("UPDATE foo SET x = 'c' WHERE id = 1")?;

    // v2_clock should have 2 entries: one per non-PK column (x and y) created by the INSERT.
    // The subsequent UPDATE on x updates the existing x entry in place rather than adding a new row.
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 2, "update: should have 2 clock entries, got {}", stmt.column_int(0));
    libc_println!("  update: 2 clock entries — PASS");
    Ok(())
}

/// DELETE on skip_hash table: row should move to v2_tombstones, no v2_tombstone_pks.
fn test_skip_hash_delete() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("DELETE FROM foo WHERE id = 1")?;

    // v2_pks should be empty
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 0, "delete: v2_pks should be empty");

    // v2_tombstones should have 1 row
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_tombstones")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 1, "delete: v2_tombstones should have 1 row");

    // v2_tombstone_pks should NOT exist
    assert!(!has_v2_tombstone_pks(&db.db, "foo"), "delete: should not have v2_tombstone_pks");
    libc_println!("  delete: v2_pks empty, 1 tombstone, no tombstone_pks — PASS");
    Ok(())
}

/// DELETE then INSERT (resurrection) on skip_hash table.
fn test_skip_hash_resurrect() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'b')")?;

    // v2_pks should have 1 row with cl=3 (1 insert + 1 delete + 1 resurrect)
    let stmt = db.db.prepare_v2("SELECT cl FROM foo__crsql_v2_pks WHERE __crsql_key = 1")?;
    stmt.step()?;
    assert!(stmt.column_int64(0) == 3, "resurrect: cl should be 3, got {}", stmt.column_int64(0));

    // v2_tombstones should be empty
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_tombstones")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 0, "resurrect: tombstones should be empty");
    libc_println!("  resurrect: cl=3, no tombstones — PASS");
    Ok(())
}

// =============================================================================
// Feed query tests
// =============================================================================

/// Feed query on skip_hash table: alive rows should produce correct changes.
fn test_skip_hash_feed_alive() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;

    let stmt = db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [col_version], [db_version], [site_id], [seq], [cl], [ts] FROM crsql_changes"
    )?;
    let mut rows = vec![];
    while stmt.step()? == ResultCode::ROW {
        rows.push((
            stmt.column_text(0)?.to_string(),
            stmt.column_text(2)?.to_string(),
        ));
    }
    // Should have 1 change (x column)
    assert!(rows.len() == 1, "feed alive: expected 1 row, got {}", rows.len());
    assert!(rows[0].0 == "foo", "feed alive: table should be foo");
    assert!(rows[0].1 == "x", "feed alive: cid should be x");
    libc_println!("  feed alive: 1 change for x column — PASS");
    Ok(())
}

/// Feed query on skip_hash table: dead rows should produce delete changes.
fn test_skip_hash_feed_dead() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("DELETE FROM foo WHERE id = 1")?;

    let stmt = db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [col_version], [db_version], [site_id], [seq], [cl], [ts] FROM crsql_changes"
    )?;
    let mut rows = vec![];
    while stmt.step()? == ResultCode::ROW {
        rows.push((
            stmt.column_text(0)?.to_string(),
            stmt.column_text(2)?.to_string(),
            stmt.column_int64(7), // cl
        ));
    }
    // Should have 1 delete change with even CL
    assert!(rows.len() == 1, "feed dead: expected 1 row, got {}", rows.len());
    assert!(rows[0].1 == "-1", "feed dead: cid should be -1 (delete sentinel)");
    assert!(rows[0].2 % 2 == 0, "feed dead: cl should be even, got {}", rows[0].2);
    libc_println!("  feed dead: 1 delete with even CL — PASS");
    Ok(())
}

// =============================================================================
// Merge path tests
// =============================================================================

/// Sync roundtrip: source skip_hash → target skip_hash.
fn test_skip_hash_sync_roundtrip() -> Result<(), ResultCode> {
    // Source DB — single INT PK auto-qualifies for skip_hash
    let src = crate::opendb()?;
    src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    src.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("INSERT INTO foo VALUES (2, 'b')")?;

    // Target DB
    let tgt = crate::opendb()?;
    tgt.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    tgt.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    tgt.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    tgt.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    tgt.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Read changes from source and merge into target
    // Use SELECT * to get columns in vtab order, and bind_value directly
    let read_stmt = src.db.prepare_v2("SELECT * FROM crsql_changes")?;
    let mut count = 0;
    tgt.db.exec_safe("BEGIN")?;
    tgt.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    while read_stmt.step()? == ResultCode::ROW {
        let merge_stmt = tgt.db.prepare_v2(
            "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )?;
        for i in 0..10 {
            merge_stmt.bind_value(i + 1, read_stmt.column_value(i)?)?;
        }
        let rc = merge_stmt.step();
        if let Err(e) = rc {
            let errmsg = tgt.db.errmsg().unwrap_or_else(|_| "unknown".to_string());
            libc_println!("  roundtrip: merge FAILED: {:?} - {}", e, errmsg);
            let _ = tgt.db.exec_safe("ROLLBACK");
            return Err(e);
        }
        count += 1;
    }
    tgt.db.exec_safe("COMMIT")?;
    assert!(count > 0, "roundtrip: should have changes to merge");

    // Verify target has the data
    let stmt = tgt.db.prepare_v2("SELECT x FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert!(stmt.column_text(0)? == "a", "roundtrip: foo(1).x should be 'a'");

    let stmt = tgt.db.prepare_v2("SELECT x FROM foo WHERE id = 2")?;
    stmt.step()?;
    assert!(stmt.column_text(0)? == "b", "roundtrip: foo(2).x should be 'b'");
    libc_println!("  sync roundtrip: {} changes merged, data verified — PASS", count);
    Ok(())
}

/// Sync delete from source skip_hash → target skip_hash.
fn test_skip_hash_sync_delete() -> Result<(), ResultCode> {
    // Source DB
    let src = crate::opendb()?;
    src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    src.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;

    // Target DB — has the same row
    let tgt = crate::opendb()?;
    tgt.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    tgt.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    tgt.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    tgt.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    tgt.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    tgt.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    tgt.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;

    // Delete on source
    src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    src.db.exec_safe("DELETE FROM foo WHERE id = 1")?;

    // Sync delete to target
    let read_stmt = src.db.prepare_v2("SELECT * FROM crsql_changes")?;
    let mut count = 0;
    tgt.db.exec_safe("BEGIN")?;
    tgt.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    while read_stmt.step()? == ResultCode::ROW {
        let merge_stmt = tgt.db.prepare_v2(
            "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )?;
        for i in 0..10 {
            merge_stmt.bind_value(i + 1, read_stmt.column_value(i)?)?;
        }
        merge_stmt.step()?;
        count += 1;
    }
    tgt.db.exec_safe("COMMIT")?;
    assert!(count > 0, "sync delete: should have changes to merge");

    // Verify target row is deleted
    let stmt = tgt.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 0, "sync delete: foo(1) should be deleted");
    libc_println!("  sync delete: row deleted on target — PASS");
    Ok(())
}

// =============================================================================
// Orthogonality test: skip_hash × key_is_rowid
// =============================================================================

/// Test all 4 combinations of skip_hash × key_is_rowid:
/// 1. skip_hash + rowid-key (INTEGER PK + use_rowid arg, auto-qualified)
/// 2. skip_hash + non-rowid (TEXT PK + directive, without_rowid)
/// 3. hash + rowid-key (INTEGER PK + skip_hash=0 directive + use_rowid arg)
/// 4. hash + non-rowid (TEXT PK, without_rowid)
fn test_skip_hash_rowid_orthogonality() -> Result<(), ResultCode> {
    // 1. skip_hash + rowid-key: INTEGER PK, auto-qualified, use_rowid forces key_is_rowid=true
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t1 (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t1', 'use_rowid')")?;
        let sig = get_pk_signature(&db.db, "t1");
        let mode = sig.split(':').next().unwrap_or("");
        assert!(mode == "rs", "combo 1: expected mode 'rs' (rowid + skip_hash), got '{}'", mode);
        assert!(!v2_pks_has_hashed_pk(&db.db, "t1"), "combo 1: should not have hashed_pk");
        libc_println!("  combo 1 (skip_hash + rowid-key INTEGER PK): mode={}, no hashed_pk — PASS", mode);
    }

    // 2. skip_hash + non-rowid: TEXT PK + directive + without_rowid
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t2 /* crsql: skip_hash=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t2', 'without_rowid')")?;
        let sig = get_pk_signature(&db.db, "t2");
        let mode = sig.split(':').next().unwrap_or("");
        assert!(mode == "ns", "combo 2: expected mode 'ns' (non-rowid + skip_hash), got '{}'", mode);
        assert!(!v2_pks_has_hashed_pk(&db.db, "t2"), "combo 2: should not have hashed_pk");
        libc_println!("  combo 2 (skip_hash + non-rowid): mode={}, no hashed_pk — PASS", mode);
    }

    // 3. hash + rowid-key: INTEGER PK + skip_hash=0 directive + use_rowid arg
    //    use_rowid forces key_is_rowid=true, skip_hash=0 forces hash mode
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t3 /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t3', 'use_rowid')")?;
        let sig = get_pk_signature(&db.db, "t3");
        let mode = sig.split(':').next().unwrap_or("");
        assert!(mode == "rh", "combo 3: expected mode 'rh' (rowid + hash), got '{}'", mode);
        assert!(v2_pks_has_hashed_pk(&db.db, "t3"), "combo 3: should have hashed_pk");
        libc_println!("  combo 3 (hash + rowid-key INTEGER PK): mode={}, has hashed_pk — PASS", mode);
    }

    // 4. hash + non-rowid: TEXT PK + without_rowid
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t4 (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t4', 'without_rowid')")?;
        let sig = get_pk_signature(&db.db, "t4");
        let mode = sig.split(':').next().unwrap_or("");
        assert!(mode == "nh", "combo 4: expected mode 'nh' (non-rowid + hash), got '{}'", mode);
        assert!(v2_pks_has_hashed_pk(&db.db, "t4"), "combo 4: should have hashed_pk");
        libc_println!("  combo 4 (hash + non-rowid): mode={}, has hashed_pk — PASS", mode);
    }

    Ok(())
}

/// Test skip_hash with non-rowid (manually enabled via directive) local writes.
fn test_skip_hash_non_rowid_insert() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("CREATE TABLE foo /* crsql: skip_hash=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo', 'without_rowid')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES ('abc', 'a')")?;

    // v2_pks should have 1 row
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 1, "non-rowid insert: should have 1 row in v2_pks");

    // Verify the PK column is stored directly
    let stmt = db.db.prepare_v2("SELECT id, cl FROM foo__crsql_v2_pks")?;
    stmt.step()?;
    assert!(stmt.column_text(0)? == "abc", "non-rowid insert: pk should be 'abc'");
    assert!(stmt.column_int64(1) == 1, "non-rowid insert: cl should be 1");
    libc_println!("  non-rowid insert: pk='abc', cl=1 — PASS");
    Ok(())
}

/// Auto-detection matrix test.
/// Verifies that table classification (mode + key_is_rowid) is correct for various PK schemas.
///
/// Mode format: {r|n}{s|h} where r=rowid-key, n=non-rowid, s=skip_hash, h=hash
///
/// #   Schema                          Directive         as_crr arg          Expected  Why
/// 1   INTEGER PK                      —                 —                   ns        INTEGER PK is rowid alias → non-rowid (overflow safety), skip_hash auto (INT affinity)
/// 2   INT PK                          —                 —                   ns        INT is not rowid alias → implicit rowid unstable under VACUUM → non-rowid, skip_hash auto
/// 3   BIGINT PK                       —                 —                   ns        Same as INT
/// 4   TEXT PK                         —                 —                   nh        TEXT PK, implicit rowid unstable → non-rowid, hash mode
/// 5   (INTEGER, INTEGER) composite    —                 —                   nh        Composite PK → no skip_hash, implicit rowid unstable → non-rowid
/// 6   INTEGER PK WITHOUT ROWID        —                 —                   ns        Non-rowid, skip_hash auto
/// 7   INT PK WITHOUT ROWID            —                 —                   ns        Non-rowid, skip_hash auto
/// 8   TEXT PK WITHOUT ROWID           —                 —                   nh        Non-rowid, hash
/// 9   (TEXT, TEXT) WITHOUT ROWID      —                 —                   nh        Non-rowid, composite, hash
/// 10  INTEGER PK                      skip_hash=0       —                   nh        Explicit hash, non-rowid (INTEGER PK default)
/// 11  TEXT PK                         skip_hash=1,      without_rowid       ns        Explicit skip_hash + non-rowid (without_rowid arg = use_rowid=0)
/// 12  INTEGER PK                      —                 use_rowid           rs        Explicit use_rowid overrides non-rowid default, skip_hash auto
/// 13  INTEGER PK                      skip_hash=0       use_rowid           rh        Explicit use_rowid + explicit hash
/// 14  (INTEGER, INTEGER) composite    skip_hash=1       —                   nh        skip_hash=1 rejected on composite PK → hash, non-rowid
/// 15  (TEXT, TEXT) WITHOUT ROWID      skip_hash=1       —                   nh        skip_hash=1 rejected on composite PK → hash, non-rowid
/// 16  INTEGER PK                      use_rowid=1       —                   rs        use_rowid=1 directive forces rowid-key
/// 17  INT PK                          use_rowid=0       —                   ns        use_rowid=0 directive forces non-rowid-key
/// 18  INTEGER PK                      use_rowid=0,      —                   nh        use_rowid=0 + skip_hash=0 → hash + non-rowid
///                                    skip_hash=0
/// 19  INT PK                          —                 use_rowid           ERROR     use_rowid rejected on non-INTEGER PK (implicit rowid unstable)
/// 20  TEXT PK                         use_rowid=1       —                   ERROR     use_rowid=1 directive rejected on non-INTEGER PK
fn test_auto_detection_matrix() -> Result<(), ResultCode> {
    libc_println!("=== test_auto_detection_matrix START ===");

    // (create_sql, as_crr_args, expected_mode_prefix, label)
    let cases: &[(&str, &str, &str, &str)] = &[
        ("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "ns", "INTEGER PK non-rowid"),
        ("CREATE TABLE t (id INT PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "ns", "INT PK non-rowid"),
        ("CREATE TABLE t (id BIGINT PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "ns", "BIGINT PK non-rowid"),
        ("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "nh", "TEXT PK non-rowid"),
        ("CREATE TABLE t (a INTEGER NOT NULL, b INTEGER NOT NULL, x TEXT, PRIMARY KEY (a, b))",
         "'t'", "nh", "composite INTEGER PK non-rowid"),
        ("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, x TEXT) WITHOUT ROWID",
         "'t'", "ns", "INTEGER PK WITHOUT ROWID"),
        ("CREATE TABLE t (id INT PRIMARY KEY NOT NULL, x TEXT) WITHOUT ROWID",
         "'t'", "ns", "INT PK WITHOUT ROWID"),
        ("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, x TEXT) WITHOUT ROWID",
         "'t'", "nh", "TEXT PK WITHOUT ROWID"),
        ("CREATE TABLE t (a TEXT NOT NULL, b TEXT NOT NULL, x TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
         "'t'", "nh", "composite TEXT PK WITHOUT ROWID"),
        ("CREATE TABLE t /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "nh", "INTEGER PK + skip_hash=0"),
        ("CREATE TABLE t /* crsql: skip_hash=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)",
         "'t', 'without_rowid'", "ns", "TEXT PK + skip_hash=1 + without_rowid"),
        ("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t', 'use_rowid'", "rs", "INTEGER PK + use_rowid"),
        ("CREATE TABLE t /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t', 'use_rowid'", "rh", "INTEGER PK + skip_hash=0 + use_rowid"),
        ("CREATE TABLE t /* crsql: skip_hash=1 */ (a INTEGER NOT NULL, b INTEGER NOT NULL, x TEXT, PRIMARY KEY (a, b))",
         "'t'", "nh", "composite INTEGER PK + skip_hash=1 (rejected)"),
        ("CREATE TABLE t /* crsql: skip_hash=1 */ (a TEXT NOT NULL, b TEXT NOT NULL, x TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID",
         "'t'", "nh", "composite TEXT PK WITHOUT ROWID + skip_hash=1 (rejected)"),
        // use_rowid directive (tri-state): =1 forces rowid, =0 forces non-rowid
        ("CREATE TABLE t /* crsql: use_rowid=1 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "rs", "INTEGER PK + use_rowid=1 directive"),
        ("CREATE TABLE t /* crsql: use_rowid=0 */ (id INT PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "ns", "INT PK + use_rowid=0 directive (force non-rowid)"),
        ("CREATE TABLE t /* crsql: use_rowid=0, skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "nh", "INTEGER PK + use_rowid=0 + skip_hash=0"),
        // use_rowid=1 on non-INTEGER PK → ERROR (implicit rowid unstable under VACUUM)
        ("CREATE TABLE t (id INT PRIMARY KEY NOT NULL, x TEXT)",
         "'t', 'use_rowid'", "ERROR", "INT PK + use_rowid (should fail)"),
        ("CREATE TABLE t /* crsql: use_rowid=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)",
         "'t'", "ERROR", "TEXT PK + use_rowid=1 directive (should fail)"),
    ];

    for (i, (create_sql, as_crr_args, expected, label)) in cases.iter().enumerate() {
        libc_println!("  [{:>2}/{}] {} — running...", i + 1, cases.len(), label);
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")
            .map_err(|_| { libc_println!("  [{:>2}] {}: config_set FAILED", i + 1, label); ResultCode::ERROR })?;
        db.db.exec_safe(create_sql)
            .map_err(|_| { libc_println!("  [{:>2}] {}: CREATE TABLE FAILED", i + 1, label); ResultCode::ERROR })?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")
            .map_err(|_| { libc_println!("  [{:>2}] {}: set_ts FAILED", i + 1, label); ResultCode::ERROR })?;
        let as_crr_rc = db.db.exec_safe(&format!("SELECT crsql_as_crr({})", as_crr_args));

        if *expected == "ERROR" {
            assert!(
                as_crr_rc.is_err(),
                "[{}] {}: expected as_crr to FAIL, but it succeeded",
                i + 1, label
            );
            libc_println!("  [{:>2}] {}: correctly rejected — PASS", i + 1, label);
            continue;
        }

        as_crr_rc
            .map_err(|_| { libc_println!("  [{:>2}] {}: crsql_as_crr({}) FAILED", i + 1, label, as_crr_args); ResultCode::ERROR })?;
        let sig = get_pk_signature(&db.db, "t");
        let mode = sig.split(':').next().unwrap_or("");
        assert!(
            mode == *expected,
            "[{}] {}: expected '{}', got '{}' (full: '{}')",
            i + 1, label, expected, mode, sig
        );
        libc_println!("  [{:>2}] {}: {} — PASS", i + 1, label, mode);
    }

    libc_println!("=== test_auto_detection_matrix PASS ({} cases) ===", cases.len());
    Ok(())
}

/// Helper: return the declared type of a column in a table, or "" if absent.
fn column_type(db: &sqlite::ManagedConnection, table: &str, col: &str) -> String {
    let stmt = db.prepare_v2(&format!(
        "SELECT type FROM pragma_table_info('{table}') WHERE name = '{col}'",
        table = table,
        col = col,
    ));
    match stmt {
        Ok(s) => {
            if s.step().unwrap_or(ResultCode::DONE) == ResultCode::ROW {
                s.column_text(0).unwrap_or("").to_string()
            } else {
                String::new()
            }
        }
        Err(_) => String::new(),
    }
}

/// Non-STRICT source table with INTEGER PRIMARY KEY: type affinity allows
/// storing a TEXT value in the PK column. The V2 mirror tables (v2_pks,
/// v2_tombstones) must accept that value too, so their PK columns use ANY
/// rather than the declared INTEGER type. Regression test for the bug where
/// skip_hash mirror tables hardcoded INTEGER and rejected valid TEXT PKs.
fn test_non_strict_int_pk_accepts_text_value() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    // Non-STRICT WITHOUT ROWID table with INTEGER PRIMARY KEY.
    db.db.exec_safe(
        "CREATE TABLE tests (id INTEGER NOT NULL PRIMARY KEY, text TEXT) WITHOUT ROWID",
    )?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('tests')")?;

    // The v2_pks PK column must be ANY (non-STRICT source).
    let pk_type = column_type(&db.db, "tests__crsql_v2_pks", "id");
    assert!(
        pk_type == "ANY",
        "non-strict: v2_pks.id should be ANY, got '{}'",
        pk_type
    );
    // The v2_tombstones PK column must be ANY (non-STRICT source).
    let tomb_type = column_type(&db.db, "tests__crsql_v2_tombstones", "id");
    assert!(
        tomb_type == "ANY",
        "non-strict: v2_tombstones.id should be ANY, got '{}'",
        tomb_type
    );

    // Inserting a TEXT value into the INTEGER PK is allowed by affinity.
    // Before the fix this failed with CONSTRAINT_DATATYPE from the v2 trigger.
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES ('service-id-0', 'hello')")?;

    // The PK value should be stored verbatim in v2_pks.
    let stmt = db.db.prepare_v2("SELECT id, cl FROM tests__crsql_v2_pks")?;
    stmt.step()?;
    assert!(
        stmt.column_text(0)? == "service-id-0",
        "non-strict: v2_pks should store text pk verbatim"
    );
    assert!(stmt.column_int64(1) == 1, "non-strict: cl should be 1");

    // Deleting the row must also work — the tombstone stores the text PK.
    db.db.exec_safe("SELECT crsql_set_ts('1700000002')")?;
    db.db.exec_safe("DELETE FROM tests WHERE id = 'service-id-0'")?;
    let stmt = db.db.prepare_v2("SELECT count(*) FROM tests__crsql_v2_tombstones")?;
    stmt.step()?;
    assert!(
        stmt.column_int(0) == 1,
        "non-strict: delete should produce 1 tombstone"
    );

    libc_println!("  non-strict int PK accepts text value (insert + delete) — PASS");
    Ok(())
}

/// Non-STRICT WITHOUT ROWID table with INTEGER PRIMARY KEY: the PK has INTEGER
/// affinity but is NOT a rowid alias, so it can hold non-integer values (text,
/// real, blob) that can't be losslessly converted. The mirror uses ANY, which
/// has no affinity — values must pass through verbatim with no implicit cast.
/// This test pins that property: for each storage class, the mirror stores the
/// value with the same storage class as the source.
fn test_non_strict_without_rowid_int_pk_preserves_storage_class() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    // Non-STRICT WITHOUT ROWID: INTEGER PK is NOT a rowid alias.
    db.db
        .exec_safe("CREATE TABLE tests (id INTEGER NOT NULL PRIMARY KEY, text TEXT) WITHOUT ROWID")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('tests')")?;

    // Mirror PK columns must be ANY (non-STRICT, non-rowid).
    let pk_type = column_type(&db.db, "tests__crsql_v2_pks", "id");
    assert!(
        pk_type == "ANY",
        "v2_pks.id should be ANY, got '{}'",
        pk_type
    );

    // Insert values that exercise different storage classes under INTEGER
    // affinity:
    //   5              → integer (already integer)
    //   5.5            → real (cannot losslessly convert to integer)
    //   'service-id-0' → text (not a well-formed integer literal)
    //   x'deadbeef'    → blob (affinity never converts blobs)
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES (5, 'a')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES (5.5, 'b')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES ('service-id-0', 'c')")?;
    db.db
        .exec_safe("INSERT INTO tests (id, text) VALUES (x'deadbeef', 'd')")?;

    // For each row, verify the source and mirror agree on the storage class.
    // This proves no implicit cast happens when the trigger writes NEW.id
    // into the STRICT ANY column.
    let cases: &[(&str, &str)] = &[
        ("5", "integer"),
        ("5.5", "real"),
        ("'service-id-0'", "text"),
        ("x'deadbeef'", "blob"),
    ];
    for (literal, expected_type) in cases {
        // Source table
        let stmt = db.db
            .prepare_v2(&format!("SELECT typeof(id) FROM tests WHERE id = {}", literal))?;
        stmt.step()?;
        let src_type = stmt.column_text(0)?;
        assert!(
            src_type == *expected_type,
            "source: typeof(id) for {} should be '{}', got '{}'",
            literal,
            expected_type,
            src_type
        );

        // Mirror table (STRICT ANY column — no affinity, no cast)
        let stmt = db.db
            .prepare_v2(&format!("SELECT typeof(id) FROM tests__crsql_v2_pks WHERE id = {}", literal))?;
        stmt.step()?;
        let mirror_type = stmt.column_text(0)?;
        assert!(
            mirror_type == *expected_type,
            "mirror: typeof(id) for {} should be '{}', got '{}' (implicit cast detected)",
            literal,
            expected_type,
            mirror_type
        );
    }

    libc_println!("  non-strict without rowid int PK preserves storage class — PASS");
    Ok(())
}

/// STRICT source table with INTEGER PRIMARY KEY: the V2 mirror tables use the
/// declared PK type (INTEGER) to preserve type safety. A non-integer value is
/// rejected by the source table itself, so the mirror never sees one.
fn test_strict_int_pk_uses_declared_type() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe(
        "CREATE TABLE tests (id INTEGER NOT NULL PRIMARY KEY, text TEXT) WITHOUT ROWID, STRICT",
    )?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('tests')")?;

    // STRICT source → mirror PK columns use the declared type (INTEGER).
    let pk_type = column_type(&db.db, "tests__crsql_v2_pks", "id");
    assert!(
        pk_type == "INTEGER",
        "strict: v2_pks.id should be INTEGER, got '{}'",
        pk_type
    );
    let tomb_type = column_type(&db.db, "tests__crsql_v2_tombstones", "id");
    assert!(
        tomb_type == "INTEGER",
        "strict: v2_tombstones.id should be INTEGER, got '{}'",
        tomb_type
    );

    // Valid integer insert works end-to-end.
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES (42, 'hello')")?;
    let stmt = db.db.prepare_v2("SELECT id FROM tests__crsql_v2_pks")?;
    stmt.step()?;
    assert!(stmt.column_int64(0) == 42, "strict: v2_pks should store 42");

    libc_println!("  strict int PK uses declared type INTEGER — PASS");
    Ok(())
}

/// Non-STRICT rowid table with INTEGER PRIMARY KEY: the PK is a rowid alias,
/// so the value is always a 64-bit integer regardless of type affinity. The
/// mirror tables use INTEGER (not ANY). A non-integer text value is rejected
/// by the source table itself (rowid must be a valid integer), so the mirror
/// never sees one.
fn test_non_strict_rowid_int_pk_uses_integer() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    // Non-STRICT rowid table (NOT WITHOUT ROWID) with INTEGER PRIMARY KEY.
    db.db.exec_safe("CREATE TABLE tests (id INTEGER NOT NULL PRIMARY KEY, text TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('tests')")?;

    // Non-STRICT rowid table + INTEGER PK (rowid alias) → mirror uses INTEGER.
    let pk_type = column_type(&db.db, "tests__crsql_v2_pks", "id");
    assert!(
        pk_type == "INTEGER",
        "non-strict rowid: v2_pks.id should be INTEGER, got '{}'",
        pk_type
    );
    let tomb_type = column_type(&db.db, "tests__crsql_v2_tombstones", "id");
    assert!(
        tomb_type == "INTEGER",
        "non-strict rowid: v2_tombstones.id should be INTEGER, got '{}'",
        tomb_type
    );

    // Valid integer insert works end-to-end.
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO tests (id, text) VALUES (42, 'hello')")?;
    let stmt = db.db.prepare_v2("SELECT id FROM tests__crsql_v2_pks")?;
    stmt.step()?;
    assert!(stmt.column_int64(0) == 42, "non-strict rowid: v2_pks should store 42");

    libc_println!("  non-strict rowid int PK uses INTEGER — PASS");
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    libc_println!("=== skip_hash detection tests ===");
    test_auto_qualified_int_pk().map_err(|e| { libc_println!("test_auto_qualified_int_pk FAILED: {:?}", e); e })?;
    test_text_pk_not_auto_qualified().map_err(|e| { libc_println!("test_text_pk_not_auto_qualified FAILED: {:?}", e); e })?;
    test_composite_pk_not_auto_qualified().map_err(|e| { libc_println!("test_composite_pk_not_auto_qualified FAILED: {:?}", e); e })?;
    test_schema_directive_enables_skip_hash().map_err(|e| { libc_println!("test_schema_directive_enables_skip_hash FAILED: {:?}", e); e })?;
    test_schema_directive_disables_skip_hash().map_err(|e| { libc_println!("test_schema_directive_disables_skip_hash FAILED: {:?}", e); e })?;

    libc_println!("=== skip_hash local write tests ===");
    test_skip_hash_insert().map_err(|e| { libc_println!("test_skip_hash_insert FAILED: {:?}", e); e })?;
    test_skip_hash_update().map_err(|e| { libc_println!("test_skip_hash_update FAILED: {:?}", e); e })?;
    test_skip_hash_delete().map_err(|e| { libc_println!("test_skip_hash_delete FAILED: {:?}", e); e })?;
    test_skip_hash_resurrect().map_err(|e| { libc_println!("test_skip_hash_resurrect FAILED: {:?}", e); e })?;
    test_skip_hash_non_rowid_insert().map_err(|e| { libc_println!("test_skip_hash_non_rowid_insert FAILED: {:?}", e); e })?;

    libc_println!("=== skip_hash feed query tests ===");
    test_skip_hash_feed_alive().map_err(|e| { libc_println!("test_skip_hash_feed_alive FAILED: {:?}", e); e })?;
    test_skip_hash_feed_dead().map_err(|e| { libc_println!("test_skip_hash_feed_dead FAILED: {:?}", e); e })?;

    libc_println!("=== skip_hash merge path tests ===");
    test_skip_hash_sync_roundtrip().map_err(|e| { libc_println!("test_skip_hash_sync_roundtrip FAILED: {:?}", e); e })?;
    test_skip_hash_sync_delete().map_err(|e| { libc_println!("test_skip_hash_sync_delete FAILED: {:?}", e); e })?;

    libc_println!("=== skip_hash orthogonality test ===");
    test_skip_hash_rowid_orthogonality().map_err(|e| { libc_println!("test_skip_hash_rowid_orthogonality FAILED: {:?}", e); e })?;

    libc_println!("=== auto-detection matrix test ===");
    test_auto_detection_matrix().map_err(|e| { libc_println!("test_auto_detection_matrix FAILED: {:?}", e); e })?;

    libc_println!("=== non-strict/strict PK type tests ===");
    test_non_strict_int_pk_accepts_text_value().map_err(|e| { libc_println!("test_non_strict_int_pk_accepts_text_value FAILED: {:?}", e); e })?;
    test_non_strict_without_rowid_int_pk_preserves_storage_class().map_err(|e| { libc_println!("test_non_strict_without_rowid_int_pk_preserves_storage_class FAILED: {:?}", e); e })?;
    test_strict_int_pk_uses_declared_type().map_err(|e| { libc_println!("test_strict_int_pk_uses_declared_type FAILED: {:?}", e); e })?;
    test_non_strict_rowid_int_pk_uses_integer().map_err(|e| { libc_println!("test_non_strict_rowid_int_pk_uses_integer FAILED: {:?}", e); e })?;

    libc_println!("=== ALL skip_hash tests PASS ===");
    Ok(())
}
