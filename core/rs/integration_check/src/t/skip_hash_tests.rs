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

// =============================================================================
// Detection tests
// =============================================================================

/// Single INTEGER PRIMARY KEY → auto-qualified for skip_hash.
/// v2_pks should have 2 columns (__crsql_key, cl), no hashed_pk.
fn test_auto_qualified_int_pk() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    let col_count = v2_pks_col_count(&db.db, "foo");
    assert!(col_count == 2, "int PK: expected 2 cols, got {}", col_count);
    assert!(!v2_pks_has_hashed_pk(&db.db, "foo"), "int PK: should not have hashed_pk");
    assert!(!has_v2_tombstone_pks(&db.db, "foo"), "int PK: should not have v2_tombstone_pks");
    libc_println!("  int PK: 2 cols, no hashed_pk, no tombstone_pks — PASS");
    Ok(())
}

/// TEXT PRIMARY KEY → not auto-qualified (no INT in type).
/// Rowid table (TEXT PK can't be rowid alias, but table still has rowid) →
/// hash + rowid → 3 columns (__crsql_key, hashed_pk, cl).
/// __crsql_key = rowid, hashed_pk stores the hash of the TEXT PK.
fn test_text_pk_not_auto_qualified() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    let col_count = v2_pks_col_count(&db.db, "foo");
    // hash + rowid: __crsql_key, hashed_pk, cl = 3 cols
    assert!(col_count == 3, "text PK: expected 3 cols, got {}", col_count);
    assert!(v2_pks_has_hashed_pk(&db.db, "foo"), "text PK: should have hashed_pk");
    libc_println!("  text PK: 3 cols, has hashed_pk — PASS");
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

    // v2_clock should have 2 entries (insert sentinel + x update)
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
/// 1. skip_hash + rowid-key (INT PK, auto-qualified)
/// 2. skip_hash + non-rowid (TEXT PK + directive, without_rowid)
/// 3. hash + rowid-key (INT PK + skip_hash=0 directive)
/// 4. hash + non-rowid (TEXT PK, without_rowid)
fn test_skip_hash_rowid_orthogonality() -> Result<(), ResultCode> {
    // 1. skip_hash + rowid-key: INT PK, auto-qualified
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t1 (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t1')")?;
        assert!(!v2_pks_has_hashed_pk(&db.db, "t1"), "combo 1: should not have hashed_pk");
        assert!(v2_pks_col_count(&db.db, "t1") == 2, "combo 1: should have 2 cols");
        libc_println!("  combo 1 (skip_hash + rowid): 2 cols, no hashed_pk — PASS");
    }

    // 2. skip_hash + non-rowid: TEXT PK + directive + without_rowid
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t2 /* crsql: skip_hash=1 */ (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t2', 'without_rowid')")?;
        assert!(!v2_pks_has_hashed_pk(&db.db, "t2"), "combo 2: should not have hashed_pk");
        // skip_hash + non-rowid: __crsql_key, id, cl = 3 cols
        assert!(v2_pks_col_count(&db.db, "t2") == 3, "combo 2: should have 3 cols, got {}", v2_pks_col_count(&db.db, "t2"));
        libc_println!("  combo 2 (skip_hash + non-rowid): 3 cols, no hashed_pk — PASS");
    }

    // 3. hash + rowid-key: INT PK + skip_hash=0 directive
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t3 /* crsql: skip_hash=0 */ (id INTEGER PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t3')")?;
        assert!(v2_pks_has_hashed_pk(&db.db, "t3"), "combo 3: should have hashed_pk");
        assert!(v2_pks_col_count(&db.db, "t3") == 3, "combo 3: should have 3 cols");
        libc_println!("  combo 3 (hash + rowid): 3 cols, has hashed_pk — PASS");
    }

    // 4. hash + non-rowid: TEXT PK + without_rowid
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE t4 (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t4', 'without_rowid')")?;
        assert!(v2_pks_has_hashed_pk(&db.db, "t4"), "combo 4: should have hashed_pk");
        // hash + non-rowid: __crsql_key, id, hashed_pk, cl = 4 cols
        assert!(v2_pks_col_count(&db.db, "t4") == 4, "combo 4: should have 4 cols, got {}", v2_pks_col_count(&db.db, "t4"));
        libc_println!("  combo 4 (hash + non-rowid): 4 cols, has hashed_pk — PASS");
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

    libc_println!("=== ALL skip_hash tests PASS ===");
    Ok(())
}
