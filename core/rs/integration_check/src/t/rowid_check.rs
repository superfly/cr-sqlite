extern crate alloc;
use alloc::format;
use libc_print::std_name::println;
use sqlite::{Connection, ManagedConnection, ResultCode};
use sqlite_nostd as sqlite;

/// Run incremental maintenance until V2 migration is complete.
fn migrate_to_v2(db: &ManagedConnection) -> Result<(), ResultCode> {
    db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0);
        iterations += 1;
    }
    Ok(())
}

/// Count columns in v2_pks table for a given table name.
/// Returns 0 if v2_pks doesn't exist.
fn v2_pks_col_count(db: &ManagedConnection, table: &str) -> i32 {
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

/// Test that as_crr on a rowid table with safe rowids succeeds.
/// After V2 migration, v2_pks should have rowid-key schema (3 columns).
fn test_safe_rowids_get_triggers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a'), (100, 'b'), (1000000, 'c')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Migrate to V2 and verify v2_pks has rowid-key schema (3 columns)
    migrate_to_v2(&db.db)?;
    let col_count = v2_pks_col_count(&db.db, "foo");
    assert!(col_count == 3, "expected 3 columns in v2_pks (rowid-key schema), got {}", col_count);
    Ok(())
}

/// Test that as_crr on an empty rowid table succeeds (rowid check is enforced at runtime in Rust).
fn test_empty_rowid_table_gets_triggers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    // Verify auto-generated rowid insert works
    db.db.exec_safe("INSERT INTO foo (data) VALUES ('auto')")?;
    Ok(())
}

/// Test that as_crr rejects a table with rowids exceeding MAX_ROWID_KEY.
fn test_rowid_too_large_rejected() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    // Insert a rowid that exceeds MAX_ROWID_KEY (2^51)
    db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ))?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('foo')");
    assert!(
        rc.is_err(),
        "expected as_crr to fail for table with oversized rowid"
    );
    Ok(())
}

/// Test that as_crr rejects a table with negative rowids.
fn test_negative_rowid_rejected() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (-1, 'negative')")?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('foo')");
    assert!(
        rc.is_err(),
        "expected as_crr to fail for table with negative rowid"
    );
    Ok(())
}

/// Test that without_rowid flag allows as_crr even with oversized rowids,
/// and after migration uses the non-rowid V2 schema (v2_pks has PK columns).
fn test_without_rowid_allows_oversized() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ))?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('foo', 'without_rowid')");
    assert!(
        rc.is_ok(),
        "expected as_crr with without_rowid to succeed"
    );

    // Migrate to V2 and verify v2_pks has non-rowid schema (4 columns for 1 PK)
    migrate_to_v2(&db.db)?;
    let col_count = v2_pks_col_count(&db.db, "foo");
    assert!(col_count == 4, "expected 4 columns in v2_pks (non-rowid schema), got {}", col_count);
    Ok(())
}

/// Test that the rowid range check blocks inserts of oversized rowids.
fn test_trigger_blocks_oversized_insert() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Insert within range should work
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'ok')")?;

    // Insert above MAX_ROWID_KEY should fail
    let rc = db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ));
    assert!(
        rc.is_err(),
        "expected insert with oversized rowid to be blocked by trigger"
    );
    Ok(())
}

/// Test that the rowid range check blocks updates to oversized rowids.
fn test_trigger_blocks_oversized_update() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'ok')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Update to oversized rowid should fail
    let rc = db.db.exec_safe(&format!(
        "UPDATE foo SET id = {} WHERE id = 1",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ));
    assert!(
        rc.is_err(),
        "expected update with oversized rowid to be blocked by trigger"
    );
    Ok(())
}

/// Test that the rowid range check blocks negative rowid inserts.
fn test_trigger_blocks_negative_insert() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    let rc = db.db.exec_safe("INSERT INTO foo VALUES (-1, 'negative')");
    assert!(
        rc.is_err(),
        "expected insert with negative rowid to be blocked by trigger"
    );
    Ok(())
}

/// Test that non-rowid tables (WITHOUT ROWID or non-integer PK) are not subject to rowid range checks.
fn test_non_rowid_table_no_check_triggers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    // Text PK — not a rowid alias
    db.db.exec_safe("CREATE TABLE foo (id TEXT PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES ('a', 'b')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    // Non-rowid table should allow inserts without rowid range issues
    db.db.exec_safe("INSERT INTO foo VALUES ('b', 'c')")?;
    Ok(())
}

/// Test that downgrade (crsql_as_table) removes rowid range enforcement.
fn test_downgrade_drops_check_triggers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // While CRR, oversized insert should fail
    let rc = db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ));
    assert!(rc.is_err(), "expected oversized insert to be blocked while CRR");

    // Downgrade
    let rc = db.db.exec_safe("SELECT crsql_as_table('foo')");
    if rc.is_err() {
        println!("test_downgrade: crsql_as_table failed!");
        return Err(ResultCode::ERROR);
    }

    // After downgrade, oversized insert should succeed (no rowid check)
    let rc = db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ));
    assert!(rc.is_ok(), "expected oversized insert to succeed after downgrade");
    Ok(())
}

/// Test that ALTER preserves rowid range enforcement (it's in the Rust trigger code, not separate triggers).
fn test_alter_preserves_check_triggers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Alter: add a column
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_begin_alter('foo')");
    if rc.is_err() {
        println!("test_alter_preserves_check: crsql_begin_alter failed!");
        return Err(ResultCode::ERROR);
    }
    let rc = db.db.exec_safe("ALTER TABLE foo ADD COLUMN extra TEXT");
    if rc.is_err() {
        println!("test_alter_preserves_check: ALTER TABLE failed!");
        return Err(ResultCode::ERROR);
    }
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_commit_alter('foo')");
    if rc.is_err() {
        println!("test_alter_preserves_check: crsql_commit_alter failed!");
        return Err(ResultCode::ERROR);
    }

    // After alter, rowid range check should still be enforced
    let rc = db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'too_big', 'extra')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ));
    assert!(rc.is_err(), "expected oversized insert to be blocked after ALTER");
    Ok(())
}

/// Test that ALTER preserves the without_rowid preference (no triggers, non-rowid v2_pks schema).
fn test_alter_preserves_without_rowid() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo', 'without_rowid')")?;

    // Alter: add a column
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_begin_alter('foo')")?;
    db.db.exec_safe("ALTER TABLE foo ADD COLUMN extra TEXT")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_commit_alter('foo')")?;

    // Migrate to V2 and verify v2_pks still has non-rowid schema (4 columns for 1 PK)
    migrate_to_v2(&db.db)?;
    let col_count = v2_pks_col_count(&db.db, "foo");
    assert!(col_count == 4, "expected 4 columns in v2_pks after ALTER (non-rowid preserved), got {}", col_count);
    Ok(())
}

/// Test that schema, table name form works with without_rowid.
fn test_schema_table_without_rowid_form() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe(&format!(
        "INSERT INTO foo VALUES ({}, 'big')",
        crsql_bundle::test_exports::consts::MAX_ROWID_KEY as i64
    ))?;

    // (schema, table, flag) form
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('main', 'foo', 'without_rowid')");
    assert!(rc.is_ok(), "expected (schema, table, flag) form to work");
    Ok(())
}

/// Test that direct V2 mode (metadata-write-version=3 on empty DB) creates V2 tables
/// directly without V1 clock tables.
fn test_direct_v2_mode_creates_v2_only() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    // Set metadata-write-version to 3 (V2-only) on empty DB with no CRR tables
    let rc = db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)");
    if rc.is_err() {
        println!("test_direct_v2: config_set failed");
        return Err(ResultCode::ERROR);
    }
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a'), (2, 'b')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('foo')");
    if rc.is_err() {
        println!("test_direct_v2: crsql_as_crr failed");
        return Err(ResultCode::ERROR);
    }

    // V2 tables should exist
    {
        let v2_clock = db.db.prepare_v2(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='foo__crsql_v2_clock'"
        )?;
        v2_clock.step()?;
        let count = v2_clock.column_int(0);
        if count != 1 {
            println!("test_direct_v2: v2_clock table missing, count={}", count);
            return Err(ResultCode::CONSTRAINT);
        }
    }

    // V1 clock table should NOT exist
    {
        let v1_clock = db.db.prepare_v2(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='foo__crsql_clock'"
        )?;
        v1_clock.step()?;
        let count = v1_clock.column_int(0);
        if count != 0 {
            println!("test_direct_v2: V1 clock table exists, count={}", count);
            return Err(ResultCode::CONSTRAINT);
        }
    }

    // v2_pks should have rowid-key schema (3 columns)
    let col_count = v2_pks_col_count(&db.db, "foo");
    if col_count != 3 {
        println!("test_direct_v2: v2_pks col_count={}, expected 3", col_count);
        return Err(ResultCode::CONSTRAINT);
    }

    // Verify data was backfilled into v2_pks
    {
        let pks_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
        pks_stmt.step()?;
        let count = pks_stmt.column_int(0);
        if count != 2 {
            println!("test_direct_v2: v2_pks rows={}, expected 2", count);
            return Err(ResultCode::CONSTRAINT);
        }
    }

    // Verify a write goes to V2 tables (insert)
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'c')")?;
    {
        let pks_after = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
        pks_after.step()?;
        let count = pks_after.column_int(0);
        if count != 3 {
            println!("test_direct_v2: v2_pks rows after insert={}, expected 3", count);
            return Err(ResultCode::CONSTRAINT);
        }
    }
    Ok(())
}

/// Test that dual-write mode (metadata-write-version=2) creates both V1 and V2 tables.
fn test_dual_write_mode_creates_both() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    // Set metadata-write-version to 2 (dual-write) on empty DB
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a'), (2, 'b')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Both V1 and V2 tables should exist
    let v1_clock = db.db.prepare_v2(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='foo__crsql_clock'"
    )?;
    v1_clock.step()?;
    assert!(v1_clock.column_int(0) == 1, "expected V1 clock table in mode 2");

    let v2_clock = db.db.prepare_v2(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='foo__crsql_v2_clock'"
    )?;
    v2_clock.step()?;
    assert!(v2_clock.column_int(0) == 1, "expected V2 clock table in mode 2");

    // Both should have backfilled data
    let v1_pks = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_pks")?;
    v1_pks.step()?;
    assert!(v1_pks.column_int(0) == 2, "expected 2 rows in V1 pks after backfill");

    let v2_pks = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    v2_pks.step()?;
    assert!(v2_pks.column_int(0) == 2, "expected 2 rows in V2 pks after backfill");

    // A write should go to both V1 and V2
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'c')")?;
    let v1_after = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_pks")?;
    v1_after.step()?;
    assert!(v1_after.column_int(0) == 3, "expected 3 rows in V1 pks after insert");

    let v2_after = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    v2_after.step()?;
    assert!(v2_after.column_int(0) == 3, "expected 3 rows in V2 pks after insert");
    Ok(())
}

/// Test that mode 3 cannot be set when CRR tables already exist.
fn test_mode3_rejected_with_existing_crrs() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Now try to set mode 3 — should fail since CRR tables exist
    let rc = db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)");
    assert!(rc.is_err(), "expected mode 3 to be rejected when CRR tables exist");
    Ok(())
}

/// Test that in mode 3, triggers write only to V2 even if V1 tables exist.
/// This simulates the cleanup phase where V1 tables haven't been dropped yet.
fn test_mode3_writes_v2_only_with_v1_present() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    // Start in mode 1, create CRR with V1 tables
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, data TEXT)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Migrate to dual-write, then to V2-only
    let rc = db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)");
    if rc.is_err() {
        println!("test_mode3: failed to set mode 2");
        return Err(ResultCode::ERROR);
    }
    // Run migration to create V2 tables
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0);
        iterations += 1;
    }
    println!("test_mode3: migration done, iterations={}", iterations);

    // Now transition to V2-only (mode 3)
    // V1 tables still exist but won't be written to
    let rc = db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)");
    if rc.is_err() {
        println!("test_mode3: failed to set mode 3");
        return Err(ResultCode::ERROR);
    }
    println!("test_mode3: mode 3 set successfully");

    // Insert a new row — should only go to V2
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'b')")?;
    println!("test_mode3: insert done");

    // V2 pks should have the new row
    let v2_count = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    v2_count.step()?;
    let v2_rows = v2_count.column_int(0);
    assert!(v2_rows >= 2, "expected at least 2 rows in v2_pks, got {}", v2_rows);

    // V1 pks should NOT have the new row (mode 3 = V2-only writes)
    let v1_count = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_pks WHERE __crsql_key = 2")?;
    v1_count.step()?;
    assert!(v1_count.column_int(0) == 0, "expected V1 pks to NOT have new row in mode 3");
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    println!("rowid_check: test_safe_rowids_get_triggers");
    test_safe_rowids_get_triggers()?;
    println!("rowid_check: test_empty_rowid_table_gets_triggers");
    test_empty_rowid_table_gets_triggers()?;
    println!("rowid_check: test_rowid_too_large_rejected");
    test_rowid_too_large_rejected()?;
    println!("rowid_check: test_negative_rowid_rejected");
    test_negative_rowid_rejected()?;
    println!("rowid_check: test_without_rowid_allows_oversized");
    test_without_rowid_allows_oversized()?;
    println!("rowid_check: test_trigger_blocks_oversized_insert");
    test_trigger_blocks_oversized_insert()?;
    println!("rowid_check: test_trigger_blocks_oversized_update");
    test_trigger_blocks_oversized_update()?;
    println!("rowid_check: test_trigger_blocks_negative_insert");
    test_trigger_blocks_negative_insert()?;
    println!("rowid_check: test_non_rowid_table_no_check_triggers");
    test_non_rowid_table_no_check_triggers()?;
    println!("rowid_check: test_downgrade_drops_check_triggers");
    test_downgrade_drops_check_triggers()?;
    println!("rowid_check: test_alter_preserves_check_triggers");
    test_alter_preserves_check_triggers()?;
    println!("rowid_check: test_alter_preserves_without_rowid");
    test_alter_preserves_without_rowid()?;
    println!("rowid_check: test_schema_table_without_rowid_form");
    test_schema_table_without_rowid_form()?;
    println!("rowid_check: test_direct_v2_mode_creates_v2_only");
    test_direct_v2_mode_creates_v2_only()?;
    println!("rowid_check: test_dual_write_mode_creates_both");
    test_dual_write_mode_creates_both()?;
    println!("rowid_check: test_mode3_rejected_with_existing_crrs");
    test_mode3_rejected_with_existing_crrs()?;
    println!("rowid_check: test_mode3_writes_v2_only_with_v1_present");
    test_mode3_writes_v2_only_with_v1_present()?;
    Ok(())
}
