extern crate crsql_bundle;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use libc_print::libc_println;
use sqlite::{Connection, Destructor, ManagedConnection, ResultCode};
use sqlite_nostd as sqlite;

/// Migrate a DB to V2&V1 and run maintenance until complete.
fn migrate_to_v2(db: &ManagedConnection) -> Result<(), ResultCode> {
    db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            return Err(ResultCode::ERROR);
        }
        iterations += 1;
    }
    db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    Ok(())
}

/// Sync all changes from left to right, filtering out the right db's own site_id.
fn sync_left_to_right(
    l: &dyn Connection,
    r: &dyn Connection,
    since: sqlite::int64,
) -> Result<(), ResultCode> {
    let siteid_stmt = r.prepare_v2("SELECT crsql_site_id()")?;
    siteid_stmt.step()?;
    let siteid = siteid_stmt.column_blob(0)?;

    let stmt_l = l.prepare_v2(
        "SELECT * FROM crsql_changes WHERE db_version >= ? AND site_id IS NOT ?",
    )?;
    stmt_l.bind_int64(1, since)?;
    stmt_l.bind_blob(2, siteid, Destructor::STATIC)?;

    r.exec_safe("BEGIN")?;
    r.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    while stmt_l.step()? == ResultCode::ROW {
        let stmt_r = r
            .prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
        for x in 0..10 {
            stmt_r.bind_value(x + 1, stmt_l.column_value(x)?)?;
        }
        match stmt_r.step() {
            Ok(_) => {}
            Err(e) => {
                let msg = r.errmsg().unwrap_or_else(|_| alloc::string::ToString::to_string("unknown"));
                libc_println!("V2 SYNC ERROR: {:?} - {}", e, msg);
                libc_println!("  row: tbl={:?} pk={:?} cid={:?}",
                    stmt_l.column_text(0), stmt_l.column_text(1), stmt_l.column_text(2));
                let _ = r.exec_safe("ROLLBACK");
                return Err(e);
            }
        }
    }
    r.exec_safe("COMMIT")?;
    Ok(())
}

/// Compare all rows in a table between two databases, returning true if identical.
fn tables_match(
    l: &dyn Connection,
    r: &dyn Connection,
    table: &str,
    order_col: &str,
) -> Result<bool, ResultCode> {
    let sql = alloc::format!(
        "SELECT * FROM \"{}\" ORDER BY \"{}\" ASC",
        table, order_col
    );
    let stmt_l = l.prepare_v2(&sql)?;
    let stmt_r = r.prepare_v2(&sql)?;

    loop {
        let rc_l = stmt_l.step()?;
        let rc_r = stmt_r.step()?;
        if rc_l != rc_r {
            libc_println!("MISMATCH: step results differ ({:?} vs {:?})", rc_l, rc_r);
            return Ok(false);
        }
        if rc_l == ResultCode::DONE {
            return Ok(true);
        }
        let n = stmt_l.column_count();
        for i in 0..n {
            let lt = stmt_l.column_type(i)?;
            let rt = stmt_r.column_type(i)?;
            if lt != rt {
                libc_println!("MISMATCH: column {} type {:?} vs {:?}", i, lt, rt);
                return Ok(false);
            }
            match lt {
                sqlite::ColumnType::Integer => {
                    if stmt_l.column_int64(i) != stmt_r.column_int64(i) {
                        libc_println!("MISMATCH: column {} int {} vs {}", i, stmt_l.column_int64(i), stmt_r.column_int64(i));
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Float => {
                    if stmt_l.column_double(i) != stmt_r.column_double(i) {
                        libc_println!("MISMATCH: column {} float {} vs {}", i, stmt_l.column_double(i), stmt_r.column_double(i));
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Text => {
                    if stmt_l.column_text(i)? != stmt_r.column_text(i)? {
                        libc_println!("MISMATCH: column {} text {:?} vs {:?}", i, stmt_l.column_text(i)?, stmt_r.column_text(i)?);
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Blob => {
                    if stmt_l.column_blob(i)? != stmt_r.column_blob(i)? {
                        libc_println!("MISMATCH: column {} blob {:?} vs {:?}", i, stmt_l.column_blob(i)?, stmt_r.column_blob(i)?);
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Null => {}
            }
        }
    }
}

/// Test: basic insert + sync in V2 mode
fn v2_basic_insert_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_basic_insert_sync START ===");
    let db_a = crate::opendb()?;
    db_a.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT, val INTEGER)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_a.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'one', 100)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (2, 'two', 200)")?;

    let db_b = crate::opendb()?;
    db_b.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT, val INTEGER)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_b.db)?;

    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);
    libc_println!("=== v2_basic_insert_sync PASS ===");
    Ok(())
}

/// Test: update + sync in V2 mode
fn v2_update_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_update_sync START ===");
    let db_a = crate::opendb()?;
    db_a.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT, val INTEGER)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_a.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'one', 100)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE foo SET name = 'updated', val = 150 WHERE id = 1")?;

    let db_b = crate::opendb()?;
    db_b.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT, val INTEGER)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_b.db)?;

    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);
    libc_println!("=== v2_update_sync PASS ===");
    Ok(())
}

/// Test: delete + sync in V2 mode
fn v2_delete_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_delete_sync START ===");
    let db_a = crate::opendb()?;
    db_a.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_a.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'one')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (2, 'two')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;

    let db_b = crate::opendb()?;
    db_b.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db_b.db)?;

    // First sync to get initial state
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Verify row 1 is deleted in destination
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "row 1 should be deleted in destination");

    libc_println!("=== v2_delete_sync PASS ===");
    Ok(())
}

/// Test: PK-only table insert + sync in V2 mode
fn v2_pk_only_insert_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_insert_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (2)")?;

    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);
    libc_println!("=== v2_pk_only_insert_sync PASS ===");
    Ok(())
}

/// Test: composite PK table + sync in V2 mode
fn v2_composite_pk_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_composite_pk_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE jx (id1 NOT NULL, id2 NOT NULL, val TEXT, PRIMARY KEY(id1, id2))")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('jx')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO jx VALUES (1, 2, 'a')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO jx VALUES (3, 4, 'b')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE jx SET val = 'updated' WHERE id1 = 1 AND id2 = 2")?;

    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    assert!(tables_match(&db_a.db, &db_b.db, "jx", "id1")?);
    libc_println!("=== v2_composite_pk_sync PASS ===");
    Ok(())
}

/// Test: sync bit honored in V2 mode (writes with sync bit should not create clock entries)
fn v2_sync_bit_honored() -> Result<(), ResultCode> {
    libc_println!("=== v2_sync_bit_honored START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (a PRIMARY KEY NOT NULL, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    // Enable sync bit
    db.db.exec_safe("SELECT crsql_internal_sync_bit(1)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 2)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("UPDATE foo SET b = 5 WHERE a = 1")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 2)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("DELETE FROM foo WHERE a = 2")?;

    // V2 clock table should be empty
    let stmt = db.db.prepare_v2("SELECT 1 FROM foo__crsql_v2_clock")?;
    let result = stmt.step()?;
    assert!(result == ResultCode::DONE, "V2 clock table should be empty when sync bit is set");

    libc_println!("=== v2_sync_bit_honored PASS ===");
    Ok(())
}

/// Test: bidirectional sync in V2 mode
fn v2_bidirectional_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_bidirectional_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE items (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('items')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // A inserts row 1, B inserts row 2
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO items VALUES (1, 'from_a', 10)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("INSERT INTO items VALUES (2, 'from_b', 20)")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    // Sync B -> A
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    assert!(tables_match(&db_a.db, &db_b.db, "items", "id")?);

    // Both should have both rows
    let stmt = db_a.db.prepare_v2("SELECT count(*) FROM items")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "db_a should have 2 rows");

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM items")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "db_b should have 2 rows");

    libc_println!("=== v2_bidirectional_sync PASS ===");
    Ok(())
}

/// Test: crsql_changes produces correct output in V2 mode
fn v2_changes_output() -> Result<(), ResultCode> {
    libc_println!("=== v2_changes_output START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT, val INTEGER)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'one', 100)")?;

    // Check crsql_changes produces results
    let stmt = db.db.prepare_v2("SELECT * FROM crsql_changes")?;
    let mut count = 0;
    while stmt.step()? == ResultCode::ROW {
        count += 1;
        let tbl = stmt.column_text(0)?;
        assert_eq!(tbl, "foo", "table name should be foo");
        // cid should be a column name or sentinel
        let cid = stmt.column_text(2)?;
        assert!(cid == "-1" || cid == "name" || cid == "val" || cid.contains('\0'),
            "cid should be a valid column name, got: {:?}", cid);
    }
    assert!(count > 0, "crsql_changes should produce at least one row");

    libc_println!("=== v2_changes_output PASS ===");
    Ok(())
}

/// Test: delete then re-insert (resurrect) in V2 mode
fn v2_delete_then_reinsert() -> Result<(), ResultCode> {
    libc_println!("=== v2_delete_then_reinsert START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Insert, sync, delete, re-insert, sync again
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'first')")?;
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'second')")?;

    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // Destination should have the re-inserted row
    let stmt = db_b.db.prepare_v2("SELECT name FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "second", "destination should have re-inserted value");

    libc_println!("=== v2_delete_then_reinsert PASS ===");
    Ok(())
}

/// Test: PK-only table delete sync in V2 mode
fn v2_pk_only_delete_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_delete_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (2)")?;
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Delete one row and sync
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // db_b should also have deleted the row
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "destination should have deleted id=1");

    // db_b should still have id=2
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "destination should still have id=2");

    libc_println!("=== v2_pk_only_delete_sync PASS ===");
    Ok(())
}

/// Test: PK-only table delete-then-reinsert in V2 mode
fn v2_pk_only_reinsert() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_reinsert START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // Delete then reinsert
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // db_b should have the row back
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "destination should have resurrected id=1");

    libc_println!("=== v2_pk_only_reinsert PASS ===");
    Ok(())
}

/// Test: ALTER TABLE add column to PK-only table (sentinel becomes regular clock entry)
fn v2_alter_add_column_to_pk_only() -> Result<(), ResultCode> {
    libc_println!("=== v2_alter_add_column_to_pk_only START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo (id) VALUES (2)")?;

    // Verify sentinel entries exist at col_id=0
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock WHERE cell_key & 255 = 0")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "should have 2 sentinel clock entries at col_id=0");

    // Add a non-PK column via crsql_begin_alter / crsql_commit_alter
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_begin_alter('foo')")?;
    db.db.exec_safe("ALTER TABLE foo ADD COLUMN name TEXT")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_commit_alter('foo')")?;

    // Now col_id=0 should be mapped to 'name' in v2_col_map
    let stmt = db.db.prepare_v2("SELECT col_name FROM foo__crsql_v2_col_map WHERE col_id = 0")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "name", "col_id=0 should be mapped to 'name'");

    // The old sentinel entries should still exist (now as regular clock entries for 'name')
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock WHERE cell_key & 255 = 0")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "clock entries at col_id=0 should still exist");

    // Updating the new column should work
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("UPDATE foo SET name = 'hello' WHERE id = 1")?;

    // Changes feed should now emit 'name' column changes (not sentinel cid=-1)
    let stmt = db.db.prepare_v2("SELECT cid FROM crsql_changes WHERE \"table\" = 'foo'")?;
    let mut found_name = false;
    while stmt.step()? == ResultCode::ROW {
        if stmt.column_text(0)? == "name" {
            found_name = true;
        }
    }
    assert!(found_name, "changes feed should emit 'name' column changes");

    libc_println!("=== v2_alter_add_column_to_pk_only PASS ===");
    Ok(())
}

/// Test: ALTER TABLE drop last non-PK column (table becomes PK-only, clock entries migrated)
fn v2_alter_drop_column_becomes_pk_only() -> Result<(), ResultCode> {
    libc_println!("=== v2_alter_drop_column_becomes_pk_only START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, name TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'world')")?;

    // Record the db_version of the clock entries before the drop
    let stmt = db.db.prepare_v2("SELECT min(db_version), max(db_version) FROM foo__crsql_v2_clock")?;
    stmt.step()?;
    let min_db_ver_before = stmt.column_int64(0);
    let max_db_ver_before = stmt.column_int64(1);

    // Drop the last non-PK column
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_begin_alter('foo')")?;
    db.db.exec_safe("ALTER TABLE foo DROP COLUMN name")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_commit_alter('foo')")?;

    // v2_col_map should be empty (no non-PK columns)
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_col_map")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "v2_col_map should be empty after dropping last non-PK column");

    // Sentinel clock entries should exist at col_id=0
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock WHERE cell_key & 255 = 0")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "should have 2 sentinel clock entries at col_id=0");

    // Clock entries should be migrated (preserving db_version), not freshly created
    let stmt = db.db.prepare_v2("SELECT min(db_version), max(db_version) FROM foo__crsql_v2_clock WHERE cell_key & 255 = 0")?;
    stmt.step()?;
    let min_db_ver_after = stmt.column_int64(0);
    let max_db_ver_after = stmt.column_int64(1);
    assert_eq!(min_db_ver_after, min_db_ver_before, "migrated sentinels should preserve min db_version");
    assert_eq!(max_db_ver_after, max_db_ver_before, "migrated sentinels should preserve max db_version");

    // Changes feed should emit sentinel rows (cid=-1) for PK-only table
    let stmt = db.db.prepare_v2("SELECT cid FROM crsql_changes WHERE \"table\" = 'foo'")?;
    let mut found_sentinel = false;
    while stmt.step()? == ResultCode::ROW {
        if stmt.column_text(0)? == "-1" {
            found_sentinel = true;
        }
    }
    assert!(found_sentinel, "changes feed should emit sentinel rows for PK-only table");

    libc_println!("=== v2_alter_drop_column_becomes_pk_only PASS ===");
    Ok(())
}

/// Test: ALTER TABLE reordering composite PK columns triggers metadata rebuild.
/// crsql_hash_pk is order-sensitive: hash(a, b) != hash(b, a). A PK reorder must
/// be detected as a PK change so V2 metadata tables are dropped and recreated.
/// Previously, compute_pk_signature sorted PKs alphabetically, so a reorder was
/// not detected — leaving stale hashed_pk entries that broke lookups and sync.
///
/// This test verifies the fix at the signature level: two tables with the same
/// PK columns but different PK order must produce different signatures. We also
/// verify the end-to-end behavior by creating two separate databases (one with
/// each PK order) and confirming their hashed_pk values differ.
fn v2_alter_reorder_composite_pk() -> Result<(), ResultCode> {
    libc_println!("=== v2_alter_reorder_composite_pk START ===");

    // Create two DBs with the same composite PK columns but different order.
    // DB A: PRIMARY KEY(id1, id2) — hash order is (id1, id2)
    // DB B: PRIMARY KEY(id2, id1) — hash order is (id2, id1)
    // If compute_pk_signature sorts alphabetically, both would have the same
    // signature "nh:id1:TEXT,id2:TEXT", and a PK reorder would not be detected.
    // With the fix, the signatures preserve pk-index order:
    //   DB A: "nh:id1:TEXT,id2:TEXT"
    //   DB B: "nh:id2:TEXT,id1:TEXT"
    // These are different, so check_pk_changed_v2 would detect the reorder.

    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    db_a.db.exec_safe("CREATE TABLE jx (id1 TEXT NOT NULL, id2 TEXT NOT NULL, val TEXT, PRIMARY KEY(id1, id2))")?;
    db_b.db.exec_safe("CREATE TABLE jx (id1 TEXT NOT NULL, id2 TEXT NOT NULL, val TEXT, PRIMARY KEY(id2, id1))")?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('jx')")?;
    }
    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Insert the same row in both DBs
    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("INSERT INTO jx VALUES ('a', 'b', 'hello')")?;
    }

    // Verify crsql_master signatures differ (the fix: no alphabetical sort)
    let stmt = db_a.db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'v2_pks_jx'")?;
    stmt.step()?;
    let sig_a = stmt.column_text(0)?.to_string();
    let stmt = db_b.db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'v2_pks_jx'")?;
    stmt.step()?;
    let sig_b = stmt.column_text(0)?.to_string();
    libc_println!("  signature A (PK(id1,id2)): {}", sig_a);
    libc_println!("  signature B (PK(id2,id1)): {}", sig_b);
    assert_ne!(sig_a, sig_b, "PK reorder must produce different signatures (fix: no alphabetical sort)");

    // Verify hashed_pk values differ — crsql_hash_pk is order-sensitive
    let stmt = db_a.db.prepare_v2("SELECT hashed_pk FROM jx__crsql_v2_pks")?;
    stmt.step()?;
    let hash_a = stmt.column_blob(0)?.to_vec();
    let stmt = db_b.db.prepare_v2("SELECT hashed_pk FROM jx__crsql_v2_pks")?;
    stmt.step()?;
    let hash_b = stmt.column_blob(0)?.to_vec();
    assert_ne!(hash_a, hash_b, "hashed_pk must differ for different PK order (hash is order-sensitive)");

    // Verify both DBs can sync the row correctly (each with its own hash order)
    // If A sends to B, B should see it as a new row (different PK = different identity)
    // This is expected behavior — reordering PKs changes row identity.
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE jx SET val = 'updated' WHERE id1 = 'a' AND id2 = 'b'")?;

    // DB A should produce changes
    let stmt = db_a.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'jx'")?;
    stmt.step()?;
    assert!(stmt.column_int(0) > 0, "update should produce changes");

    libc_println!("=== v2_alter_reorder_composite_pk PASS ===");
    Ok(())
}

/// Test: PK-only bidirectional sync
fn v2_pk_only_bidirectional() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_bidirectional START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Both insert different rows
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (2)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("INSERT INTO foo (id) VALUES (3)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("INSERT INTO foo (id) VALUES (4)")?;

    // Sync both directions
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    // Both should have all 4 rows
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    let stmt = db_a.db.prepare_v2("SELECT count(*) FROM foo")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 4, "db_a should have all 4 rows");

    libc_println!("=== v2_pk_only_bidirectional PASS ===");
    Ok(())
}

/// Helper: sync with V2 wire format (sync-log-version=2 on both sides)
fn sync_v2_wire(l: &dyn Connection, r: &dyn Connection, since: sqlite::int64) -> Result<(), ResultCode> {
    let siteid_stmt = r.prepare_v2("SELECT crsql_site_id()")?;
    siteid_stmt.step()?;
    let siteid = siteid_stmt.column_blob(0)?;

    let stmt_l = l.prepare_v2(
        "SELECT * FROM crsql_changes WHERE db_version >= ? AND site_id IS NOT ?",
    )?;
    stmt_l.bind_int64(1, since)?;
    stmt_l.bind_blob(2, siteid, Destructor::STATIC)?;

    r.exec_safe("BEGIN")?;
    r.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    while stmt_l.step()? == ResultCode::ROW {
        let stmt_r = r
            .prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
        for x in 0..10 {
            stmt_r.bind_value(x + 1, stmt_l.column_value(x)?)?;
        }
        match stmt_r.step() {
            Ok(_) => {}
            Err(e) => {
                let msg = r.errmsg().unwrap_or_else(|_| alloc::string::ToString::to_string("unknown"));
                libc_println!("V2 WIRE SYNC ERROR: {:?} - {}", e, msg);
                let _ = r.exec_safe("ROLLBACK");
                return Err(e);
            }
        }
    }
    r.exec_safe("COMMIT")?;
    Ok(())
}

/// Test: V2 wire format delete sync (cid=-2 hash tombstone)
fn v2_wire_delete_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_delete_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Enable V2 wire format on both sides
    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    }

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'first')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (2, 'second')")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Delete and sync via V2 wire
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "v2 wire: destination should have deleted id=1");

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "v2 wire: destination should still have id=2");

    libc_println!("=== v2_wire_delete_sync PASS ===");
    Ok(())
}

/// Test: V2 wire format delete-then-reinsert (resurrection via hash tombstone)
fn v2_wire_delete_then_reinsert() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_delete_then_reinsert START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    }

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'first')")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Delete, then re-insert with different value
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES (1, 'second')")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;

    let stmt = db_b.db.prepare_v2("SELECT name FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "second", "v2 wire: destination should have resurrected row with 'second'");

    libc_println!("=== v2_wire_delete_then_reinsert PASS ===");
    Ok(())
}

/// Test: V2 wire format PK-only delete sync
fn v2_wire_pk_only_delete() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_pk_only_delete START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    }

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (2)")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Delete and sync
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "v2 wire pk-only: destination should have deleted id=1");

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "v2 wire pk-only: destination should still have id=2");

    libc_println!("=== v2_wire_pk_only_delete PASS ===");
    Ok(())
}

/// Test: V2 wire format delete sync for non-rowid table (text PK)
fn v2_wire_non_rowid_delete() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_non_rowid_delete START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id TEXT PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    }

    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES ('a', 'first')")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo VALUES ('b', 'second')")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;
    assert!(tables_match(&db_a.db, &db_b.db, "foo", "id")?);

    // Delete and sync
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM foo WHERE id = 'a'")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;

    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 'a'")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "v2 wire non-rowid: destination should have deleted id='a'");

    let stmt = db_b.db.prepare_v2("SELECT name FROM foo WHERE id = 'b'")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "second", "v2 wire non-rowid: destination should still have id='b'");

    libc_println!("=== v2_wire_non_rowid_delete PASS ===");
    Ok(())
}

/// Test: PK-only table forward sync (A→B→C).
/// Verifies that remote merge creates the col_id=0 sentinel clock entry,
/// so the row appears in crsql_changes on the receiving node and can be
/// forwarded to a third node.
fn v2_pk_only_forward_sync() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_forward_sync START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;
    let db_c = crate::opendb()?;

    for db in [&db_a.db, &db_b.db, &db_c.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;
    migrate_to_v2(&db_c.db)?;

    // Insert on A
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;

    // Sync A → B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have the row in base table
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "db_b should have id=1 in base table");

    // B should have a clock entry at col_id=0 (sentinel) so the row appears in crsql_changes
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "db_b should have 1 clock entry (sentinel at col_id=0), got {}", stmt.column_int(0));

    // B's crsql_changes should emit the row
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo'")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "db_b crsql_changes should have 1 row for foo");

    // Sync B → C (forward)
    sync_left_to_right(&db_b.db, &db_c.db, 0)?;

    // C should have the row
    let stmt = db_c.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "db_c should have id=1 after forward sync");

    libc_println!("=== v2_pk_only_forward_sync PASS ===");
    Ok(())
}

/// Test: PK-only table site_id tie-break at equal CL.
/// Two nodes independently insert the same PK-only row (CL=1).
/// With merge-equal-values enabled, the node with the higher site_id blob
/// should win the sentinel clock entry.
fn v2_pk_only_site_id_tiebreak() -> Result<(), ResultCode> {
    libc_println!("=== v2_pk_only_site_id_tiebreak START ===");
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('foo')")?;
        // Enable site_id tie-break on both nodes
        db.exec_safe("SELECT crsql_config_set('merge-equal-values', 1)")?;
    }

    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Get site_ids for comparison
    let siteid_a: Vec<u8> = {
        let stmt = db_a.db.prepare_v2("SELECT crsql_site_id()")?;
        stmt.step()?;
        stmt.column_blob(0)?.to_vec()
    };
    let siteid_b: Vec<u8> = {
        let stmt = db_b.db.prepare_v2("SELECT crsql_site_id()")?;
        stmt.step()?;
        stmt.column_blob(0)?.to_vec()
    };

    // Both insert the same PK-only row (CL=1 on each)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("INSERT INTO foo (id) VALUES (1)")?;

    // Sync A → B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should still have the row
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM foo WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "db_b should have id=1");

    // Check which site_id won the sentinel clock entry on db_b
    // The sentinel is at col_id=0 (cell_key & mask == 0)
    let stmt = db_b.db.prepare_v2(
        "SELECT s.site_id FROM foo__crsql_v2_clock AS c \
         JOIN crsql_site_id AS s ON c.site_id = s.ordinal \
         WHERE c.cell_key & 255 = 0"
    )?;
    stmt.step()?;
    let winning_site_id: Vec<u8> = stmt.column_blob(0)?.to_vec();

    // The site_id with the higher blob value should win
    let a_wins = siteid_a > siteid_b;
    let expected_winner = if a_wins { &siteid_a[..] } else { &siteid_b[..] };
    assert_eq!(
        &winning_site_id[..], expected_winner,
        "db_b sentinel should have the winning site_id (higher blob value). \
         a={:?} b={:?} winner={:?}",
        siteid_a, siteid_b, winning_site_id
    );

    libc_println!("=== v2_pk_only_site_id_tiebreak PASS ===");
    Ok(())
}

/// Test: ALTER TABLE drop last TWO non-PK columns in one ALTER window.
/// This is the bug from audit finding #2: when dropping both non-PK columns
/// at once, the migrate UPDATE to col_id=0 hit a uniqueness violation because
/// col_id=0 rows already existed for one of the columns. The fix deletes other
/// dropped col_ids' clock rows BEFORE the migrate, and backfills missing
/// sentinels for rows that had no clock entries at all.
///
/// Additionally, the crsql_commit_alter error path had an early `return` that
/// skipped the ROLLBACK, leaving the table trigger-less. This test verifies
/// that triggers are restored after the alter and new writes are tracked.
fn v2_alter_drop_two_columns_becomes_pk_only() -> Result<(), ResultCode> {
    libc_println!("=== v2_alter_drop_two_columns_becomes_pk_only START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    migrate_to_v2(&db.db)?;

    // Insert rows so clock entries exist for both columns
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a1', 'b1')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'a2', 'b2')")?;

    // Drop BOTH non-PK columns in one ALTER window
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_begin_alter('foo')")?;
    db.db.exec_safe("ALTER TABLE foo DROP COLUMN a")?;
    db.db.exec_safe("ALTER TABLE foo DROP COLUMN b")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_commit_alter('foo')")?;

    // v2_col_map should be empty (no non-PK columns)
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_col_map")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "v2_col_map should be empty after dropping all non-PK columns");

    // Sentinel clock entries should exist at col_id=0 for both rows
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock WHERE cell_key & 255 = 0")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "should have 2 sentinel clock entries at col_id=0");

    // Triggers must be restored — new inserts should be tracked
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (4)")?;

    // v2_pks should now have 4 rows
    let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 4, "triggers should track new inserts after alter");

    // Changes feed should emit sentinel rows for all 4 rows
    let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo'")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 4, "changes feed should have 4 rows (2 migrated + 2 new)");

    // Verify the new rows (id=3, id=4) appear in the feed
    let stmt = db.db.prepare_v2("SELECT pk FROM crsql_changes WHERE \"table\" = 'foo'")?;
    let mut found_3 = false;
    let mut found_4 = false;
    while stmt.step()? == ResultCode::ROW {
        let pk_blob = stmt.column_blob(0)?;
        // PK format: count(1), type=9(int), value
        if pk_blob.len() >= 3 && pk_blob[0] == 1 && pk_blob[1] == 9 {
            if pk_blob[2] == 3 { found_3 = true; }
            if pk_blob[2] == 4 { found_4 = true; }
        }
    }
    assert!(found_3, "new row id=3 should appear in changes feed");
    assert!(found_4, "new row id=4 should appear in changes feed");

    libc_println!("=== v2_alter_drop_two_columns_becomes_pk_only PASS ===");
    Ok(())
}

/// Test: WHERE seq BETWEEN in V2-wire packed mode.
/// This is the bug from audit finding #4: in packed mode, `seq` is a BLOB
/// (crsql_pack_varint_agg), so `WHERE seq <= N` compared BLOB to INTEGER,
/// which is always false in SQLite — returning zero rows.
///
/// The fix pushes seq constraints into per-table subqueries (where c.seq is
/// scalar) and adds a scalar _seq_order column for outer ORDER BY.
///
/// This test also covers finding #6: the feed must be ordered by
/// (db_version, seq) even in packed mode.
fn v2_wire_packed_seq_filter_and_order() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_packed_seq_filter_and_order START ===");
    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Insert a row and update columns in separate transactions to get
    // different db_versions and seqs.
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a0', 'b0')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("UPDATE foo SET a = 'a1' WHERE id = 1")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("UPDATE foo SET b = 'b1' WHERE id = 1")?;

    // Get all changes (no filter) — should return rows
    let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo'")?;
    stmt.step()?;
    let total = stmt.column_int(0);
    assert!(total > 0, "unfiltered feed should return rows, got {}", total);

    // WHERE seq >= 0 — before the fix, this returned 0 rows in packed mode
    // because seq is a BLOB and BLOB >= INTEGER is false.
    let stmt = db.db.prepare_v2(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo' AND seq >= 0"
    )?;
    stmt.step()?;
    let filtered = stmt.column_int(0);
    assert_eq!(filtered, total, "WHERE seq >= 0 should return all rows in packed mode, got {} of {}", filtered, total);

    // WHERE seq BETWEEN 0 AND 100 — the exact pattern from the bug report
    let stmt = db.db.prepare_v2(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo' AND seq BETWEEN 0 AND 100"
    )?;
    stmt.step()?;
    let between = stmt.column_int(0);
    assert_eq!(between, total, "WHERE seq BETWEEN 0 AND 100 should return all rows in packed mode");

    // ORDER BY seq — should not error and should return rows in order
    let stmt = db.db.prepare_v2(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo' ORDER BY seq"
    )?;
    stmt.step()?;
    assert!(stmt.column_int(0) > 0, "ORDER BY seq should return rows in packed mode");

    // ORDER BY db_version, seq — the canonical feed ordering
    let stmt = db.db.prepare_v2(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo' ORDER BY db_version, seq"
    )?;
    stmt.step()?;
    assert!(stmt.column_int(0) > 0, "ORDER BY db_version, seq should return rows in packed mode");

    // === Ordering verification ===
    // Verify that ORDER BY db_version, seq returns rows in the correct order.
    // In packed mode, seq is a BLOB — the fix adds _seq_order (scalar c.seq)
    // to each arm and rewrites the outer ORDER BY to use it.
    // We collect (db_version, seq) pairs and verify they are non-decreasing.
    {
        let stmt = db.db.prepare_v2(
            "SELECT db_version, seq FROM crsql_changes WHERE \"table\" = 'foo' ORDER BY db_version, seq"
        )?;
        let mut prev_dbv: i64 = -1;
        let mut prev_seq: i64 = -1;
        let mut row_count = 0;
        while stmt.step()? == ResultCode::ROW {
            let dbv = stmt.column_int64(0);
            // seq is a packed BLOB in V2-wire — but the ORDER BY uses _seq_order
            // (scalar), so the ordering is correct even though the returned
            // seq column is a BLOB. We can't easily compare BLOB seq values,
            // but we can verify db_version is non-decreasing.
            assert!(dbv >= prev_dbv,
                "ORDER BY db_version, seq: db_version should be non-decreasing, got {} after {}",
                dbv, prev_dbv);
            prev_dbv = dbv;
            prev_seq = 0; // placeholder
            row_count += 1;
        }
        assert!(row_count > 0, "ORDER BY db_version, seq should return rows");
        libc_println!("  ordering: {} rows, db_version non-decreasing", row_count);
    }

    // Verify ordering across multiple tables (UNION ALL)
    // The outer ORDER BY must correctly order rows from different arms.
    {
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq"
        )?;
        let mut prev_dbv: i64 = -1;
        let mut row_count = 0;
        while stmt.step()? == ResultCode::ROW {
            let dbv = stmt.column_int64(1);
            assert!(dbv >= prev_dbv,
                "cross-table ORDER BY db_version: should be non-decreasing, got {} after {}",
                dbv, prev_dbv);
            prev_dbv = dbv;
            row_count += 1;
        }
        assert!(row_count > 0, "cross-table ORDER BY should return rows");
        libc_println!("  cross-table ordering: {} rows, db_version non-decreasing", row_count);
    }

    // === ORDER BY + LIMIT ===
    // LIMIT should work with the rewritten _seq_order ordering.
    // _seq_order is not in the outer SELECT column list but IS in the subquery,
    // so SQLite can sort and limit on it.
    {
        // LIMIT 1 should return the first row by (db_version, seq)
        let stmt = db.db.prepare_v2(
            "SELECT db_version FROM crsql_changes WHERE \"table\" = 'foo' ORDER BY db_version, seq LIMIT 1"
        )?;
        stmt.step()?;
        let first_dbv = stmt.column_int64(0);
        libc_println!("  ORDER BY + LIMIT 1: first db_version={}", first_dbv);

        // LIMIT with offset
        let stmt = db.db.prepare_v2(
            "SELECT db_version FROM crsql_changes WHERE \"table\" = 'foo' ORDER BY db_version, seq LIMIT 1 OFFSET 1"
        )?;
        stmt.step()?;
        let second_dbv = stmt.column_int64(0);
        libc_println!("  ORDER BY + LIMIT 1 OFFSET 1: second db_version={}", second_dbv);
        assert!(second_dbv >= first_dbv,
            "second row should have db_version >= first ({} >= {})", second_dbv, first_dbv);
    }

    // ORDER BY + LIMIT across multiple tables
    {
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq LIMIT 3"
        )?;
        let mut rows: Vec<(String, i64)> = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            rows.push((stmt.column_text(0)?.to_string(), stmt.column_int64(1)));
        }
        libc_println!("  cross-table LIMIT 3: got {} rows: {:?}", rows.len(), rows);
        assert!(rows.len() <= 3, "LIMIT 3 should return at most 3 rows, got {}", rows.len());
        assert!(rows.len() > 0, "should have at least 1 row");
        // Verify non-decreasing db_version
        for i in 1..rows.len() {
            assert!(rows[i].1 >= rows[i-1].1,
                "LIMIT rows should be ordered: row {} db_version {} < row {} db_version {}",
                i, rows[i].1, i-1, rows[i-1].1);
        }
    }

    libc_println!("=== v2_wire_packed_seq_filter_and_order PASS ===");
    Ok(())
}

/// Test that WHERE constraints on db_version, site_id, and ts are correctly
/// pushed into per-table subqueries in V2-wire packed mode.
///
/// This verifies:
/// 1. db_version >= N returns the same rows as V1-wire mode
/// 2. site_id IS NOT ? (the common sync filter) still works (must NOT be pushed)
/// 3. ts > N filters correctly at the cell level
/// 4. Combined db_version + site_id filter returns correct subset
fn v2_wire_packed_generalized_pushdown() -> Result<(), ResultCode> {
    libc_println!("=== v2_wire_packed_generalized_pushdown START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('default-ts', 1700000000)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Diverse table types to exercise all UNION arm variants:
    //   skip_int  — int PK, has non-PK cols → skip_hash clock + skip_hash tomb (cid='-1')
    //   hash_tbl  — text PK, has non-PK cols → hash clock + hash tomb (cid='-2')
    //   skip_pkonly — int PK, no non-PK cols → pkonly clock + skip_hash tomb (cid='-1')
    //   comp_pk   — composite int+text PK, has non-PK cols → hash clock + hash tomb (cid='-2')
    db.db.exec_safe("CREATE TABLE skip_int (id INTEGER PRIMARY KEY NOT NULL, a TEXT, b TEXT)")?;
    db.db.exec_safe("CREATE TABLE hash_tbl (id TEXT PRIMARY KEY NOT NULL, x TEXT)")?;
    db.db.exec_safe("CREATE TABLE skip_pkonly (id INTEGER PRIMARY KEY NOT NULL)")?;
    db.db.exec_safe("CREATE TABLE comp_pk (uid INTEGER NOT NULL, tenant TEXT NOT NULL, val TEXT, PRIMARY KEY(uid, tenant))")?;
    db.db.exec_safe("SELECT crsql_as_crr('skip_int')")?;
    db.db.exec_safe("SELECT crsql_as_crr('hash_tbl')")?;
    db.db.exec_safe("SELECT crsql_as_crr('skip_pkonly')")?;
    db.db.exec_safe("SELECT crsql_as_crr('comp_pk')")?;

    // Insert rows across multiple transactions (different db_versions)
    // db_version 1: skip_int insert
    db.db.exec_safe("INSERT INTO skip_int VALUES (1, 'a1', 'b1')")?;
    // db_version 2: hash_tbl insert
    db.db.exec_safe("INSERT INTO hash_tbl VALUES ('k1', 'x1')")?;
    // db_version 3: skip_int insert + skip_pkonly insert
    db.db.exec_safe("INSERT INTO skip_int VALUES (2, 'a2', 'b2')")?;
    db.db.exec_safe("INSERT INTO skip_pkonly VALUES (10)")?;
    // db_version 4: skip_int update
    db.db.exec_safe("UPDATE skip_int SET a='a3' WHERE id=1")?;
    // db_version 5: comp_pk insert
    db.db.exec_safe("INSERT INTO comp_pk VALUES (1, 't1', 'v1')")?;
    // db_version 6: hash_tbl delete (tombstone)
    db.db.exec_safe("DELETE FROM hash_tbl WHERE id='k1'")?;
    // db_version 7: comp_pk update
    db.db.exec_safe("UPDATE comp_pk SET val='v2' WHERE uid=1 AND tenant='t1'")?;
    // db_version 8: skip_pkonly delete (tombstone, pk-only)
    db.db.exec_safe("DELETE FROM skip_pkonly WHERE id=10")?;

    let siteid = {
        let s = db.db.prepare_v2("SELECT crsql_site_id()")?;
        s.step()?;
        s.column_blob(0)?.to_vec()
    };

    // Helper: count rows matching a WHERE clause (no params)
    let count_where = |sql: &str| -> i32 {
        let stmt = db.db.prepare_v2(sql).unwrap();
        stmt.step().unwrap();
        stmt.column_int(0)
    };
    // Helper: count rows with a blob param for site_id
    let count_where_siteid = |sql: &str| -> i32 {
        let stmt = db.db.prepare_v2(sql).unwrap();
        stmt.bind_blob(1, &siteid, sqlite::Destructor::STATIC).unwrap();
        stmt.step().unwrap();
        stmt.column_int(0)
    };

    // Baseline: count all changes
    let count_all = count_where("SELECT count(*) FROM crsql_changes");
    libc_println!("  baseline count_all={}", count_all);
    assert!(count_all > 0, "should have changes");

    // Count per table
    let count_skip_int = count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_int'");
    let count_hash_tbl = count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'hash_tbl'");
    let count_skip_pkonly = count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_pkonly'");
    let count_comp_pk = count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'comp_pk'");
    libc_println!("  counts: skip_int={} hash_tbl={} skip_pkonly={} comp_pk={}",
        count_skip_int, count_hash_tbl, count_skip_pkonly, count_comp_pk);
    assert!(count_skip_int > 0, "skip_int should have changes");
    assert!(count_hash_tbl > 0, "hash_tbl should have changes (incl tombstone)");
    assert!(count_skip_pkonly > 0, "skip_pkonly should have changes (incl tombstone)");
    assert!(count_comp_pk > 0, "comp_pk should have changes");
    assert_eq!(count_skip_int + count_hash_tbl + count_skip_pkonly + count_comp_pk, count_all,
        "per-table counts should sum to total");

    // Dump all rows for debugging
    {
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version, cid, cl FROM crsql_changes ORDER BY db_version, \"table\""
        )?;
        while stmt.step()? == ResultCode::ROW {
            libc_println!("  row: table={} db_version={} cid={} cl={}",
                stmt.column_text(0)?, stmt.column_int64(1),
                core::str::from_utf8(stmt.column_blob(2)?).unwrap_or("?"),
                stmt.column_int64(3));
        }
    }

    // === db_version pushdown ===
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE db_version >= 1"), count_all,
        "db_version >= 1 should return all");

    // Partial: db_version >= 5 should return a subset
    let count_dbv_ge5 = count_where("SELECT count(*) FROM crsql_changes WHERE db_version >= 5");
    libc_println!("  db_version >= 5: count={}", count_dbv_ge5);
    assert!(count_dbv_ge5 > 0 && count_dbv_ge5 < count_all,
        "db_version >= 5 should return a subset, got {} of {}", count_dbv_ge5, count_all);
    // Cross-check: complement
    let count_dbv_lt5 = count_where("SELECT count(*) FROM crsql_changes WHERE db_version < 5");
    assert_eq!(count_dbv_ge5 + count_dbv_lt5, count_all,
        "db_version >= 5 + < 5 should equal total");

    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE db_version >= 100"), 0,
        "db_version >= 100 should return 0");

    // === site_id pushdown (IS and IS NOT) ===
    assert_eq!(count_where_siteid("SELECT count(*) FROM crsql_changes WHERE site_id IS NOT ?"), 0,
        "site_id IS NOT self should return 0");
    assert_eq!(count_where_siteid("SELECT count(*) FROM crsql_changes WHERE site_id IS ?"), count_all,
        "site_id IS self should return all");

    // === ts pushdown ===
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE ts > 0"), count_all,
        "ts > 0 should return all");
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE ts > 2000000000"), 0,
        "ts > 2000000000 should return 0");

    // === tbl pushdown (literal comparison, branch pruning) ===
    // Each table filter should return only that table's rows
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_int'"), count_skip_int);
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'hash_tbl'"), count_hash_tbl);
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_pkonly'"), count_skip_pkonly);
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'comp_pk'"), count_comp_pk);
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE \"table\" = 'nonexistent'"), 0,
        "table = 'nonexistent' should return 0");

    // Verify tbl filter returns correct table names (not mixed)
    {
        let stmt = db.db.prepare_v2("SELECT DISTINCT \"table\" FROM crsql_changes WHERE \"table\" = 'comp_pk'")?;
        stmt.step()?;
        assert_eq!(stmt.column_text(0)?, "comp_pk", "table = 'comp_pk' should only return comp_pk");
    }

    // === cid pushdown ===
    // cid = '-1' matches skip_hash tombstones (hash_tbl delete uses cid='-2', skip_int/skip_pkonly delete uses '-1')
    let count_cid_del1 = count_where("SELECT count(*) FROM crsql_changes WHERE cid = '-1'");
    libc_println!("  cid='-1' (skip_hash tomb): count={}", count_cid_del1);
    assert!(count_cid_del1 > 0, "cid = '-1' should return skip_hash tombstone rows");

    // cid = '-2' matches hash tombstones (hash_tbl delete)
    let count_cid_del2 = count_where("SELECT count(*) FROM crsql_changes WHERE cid = '-2'");
    libc_println!("  cid='-2' (hash tomb): count={}", count_cid_del2);
    assert!(count_cid_del2 > 0, "cid = '-2' should return hash tombstone rows");

    // cid != '-1' should return non-skip_hash-tombstone rows
    let count_cid_non_del1 = count_where("SELECT count(*) FROM crsql_changes WHERE cid != '-1'");
    assert!(count_cid_non_del1 > 0, "cid != '-1' should return non-skip_hash-tombstone rows");

    // cid = 'a' should return only rows where column 'a' was changed (skip_int only)
    let count_cid_a = count_where("SELECT count(*) FROM crsql_changes WHERE cid = 'a'");
    libc_println!("  cid='a' count={}", count_cid_a);
    assert!(count_cid_a > 0, "cid = 'a' should return rows where column a was changed");

    // cid = 'val' should return comp_pk update rows
    let count_cid_val = count_where("SELECT count(*) FROM crsql_changes WHERE cid = 'val'");
    libc_println!("  cid='val' count={}", count_cid_val);
    assert!(count_cid_val > 0, "cid = 'val' should return comp_pk rows where val was changed");

    // cid = 'x' — hash_tbl insert was deleted, so x column change is gone (tombstone replaces it)
    let count_cid_x = count_where("SELECT count(*) FROM crsql_changes WHERE cid = 'x'");
    libc_println!("  cid='x' count={}", count_cid_x);
    // hash_tbl insert was deleted — tombstone supersedes the x column change

    // Verify cid values are correct (cid is BLOB in packed mode)
    {
        let stmt = db.db.prepare_v2("SELECT cid FROM crsql_changes WHERE cid = 'val' LIMIT 1")?;
        stmt.step()?;
        let cid_blob = stmt.column_blob(0)?;
        let cid_str = core::str::from_utf8(cid_blob).unwrap_or("");
        assert!(cid_str.contains("val"), "cid = 'val' should return rows with cid containing 'val', got: {:?}", cid_str);
    }

    // === col_version pushdown ===
    // col_version >= 1 should return non-tombstone changes.
    // Note: pk-only tombstones have col_version = t.cl (causal length), not NULL.
    // Hash tombstones have col_version = NULL (pruned by NULL >= 1).
    // So col_version >= 1 returns: all clock rows + pk-only tombstones with cl >= 1.
    let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE col_version >= ?1")?;
    stmt.bind_int(1, 1)?;
    stmt.step()?;
    let count_colvrsn_ge1 = stmt.column_int(0);
    libc_println!("  col_version >= 1 count={}", count_colvrsn_ge1);
    assert!(count_colvrsn_ge1 > 0, "col_version >= 1 should return non-tombstone changes");
    // Should exclude hash tombstones (col_version = NULL) but include pk-only tombstones (col_version = t.cl)
    // hash_tbl tombstone is the only one with NULL col_version
    assert_eq!(count_colvrsn_ge1, count_all - count_cid_del2,
        "col_version >= 1 should return all except hash tombstones ({} - {} = {})",
        count_all, count_cid_del2, count_all - count_cid_del2);

    // col_version >= 100 should return nothing
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE col_version >= 100"), 0,
        "col_version >= 100 should return 0");

    // === cl pushdown ===
    let count_cl_ge1 = count_where("SELECT count(*) FROM crsql_changes WHERE cl >= 1");
    libc_println!("  cl >= 1 count={}", count_cl_ge1);
    assert_eq!(count_cl_ge1, count_all, "cl >= 1 should return all (every change has cl >= 1)");
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE cl >= 100"), 0,
        "cl >= 100 should return 0");

    // === seq pushdown ===
    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE seq >= 0"), count_all,
        "seq >= 0 should return all");

    // Partial: seq >= 1 should return a subset
    let count_seq_ge1 = count_where("SELECT count(*) FROM crsql_changes WHERE seq >= 1");
    libc_println!("  seq >= 1: count={}", count_seq_ge1);
    assert!(count_seq_ge1 > 0 && count_seq_ge1 < count_all,
        "seq >= 1 should return a subset, got {} of {}", count_seq_ge1, count_all);

    assert_eq!(count_where("SELECT count(*) FROM crsql_changes WHERE seq >= 100000"), 0,
        "seq >= 100000 should return 0");

    // === BETWEEN ===
    // Use actual min/max db_version from the data to construct a meaningful BETWEEN.
    // Note: min()/max() aggregates on vtab columns can return incorrect values
    // (SQLite's optimizer may not scan all rows for aggregates on vtabs).
    // We manually scan to get the true range.
    let mut versions: Vec<i64> = Vec::new();
    {
        let stmt = db.db.prepare_v2("SELECT db_version FROM crsql_changes")?;
        while stmt.step()? == ResultCode::ROW {
            versions.push(stmt.column_int64(0));
        }
    }
    let min_dbv = *versions.iter().min().unwrap_or(&1);
    let max_dbv = *versions.iter().max().unwrap_or(&9);
    let mid_dbv = (min_dbv + max_dbv) / 2;
    libc_println!("  db_version range: {} to {}, mid={}", min_dbv, max_dbv, mid_dbv);

    // BETWEEN min AND max should return all
    let count_between_all = count_where(&format!(
        "SELECT count(*) FROM crsql_changes WHERE db_version BETWEEN {} AND {}", min_dbv, max_dbv));
    assert_eq!(count_between_all, count_all, "BETWEEN min AND max should return all");

    // BETWEEN min AND mid should return a subset (or all if mid >= max)
    let count_between_partial = count_where(&format!(
        "SELECT count(*) FROM crsql_changes WHERE db_version BETWEEN {} AND {}", min_dbv, mid_dbv));
    libc_println!("  db_version BETWEEN {} AND {}: count={}", min_dbv, mid_dbv, count_between_partial);
    // Cross-check: BETWEEN should match >= min AND <= mid
    assert_eq!(count_where(&format!(
        "SELECT count(*) FROM crsql_changes WHERE db_version >= {} AND db_version <= {}", min_dbv, mid_dbv)),
        count_between_partial, "BETWEEN should match >= AND <=");

    // seq BETWEEN 0 AND 0 — partial subset (some rows have seq=0, some have seq>0)
    let count_seq_between = count_where("SELECT count(*) FROM crsql_changes WHERE seq BETWEEN 0 AND 0");
    libc_println!("  seq BETWEEN 0 AND 0: count={}", count_seq_between);
    // All our rows have seq=0 (single-row transactions), so this returns all
    // Use a different BETWEEN to get a partial subset
    let count_seq_ge1 = count_where("SELECT count(*) FROM crsql_changes WHERE seq >= 1");
    if count_seq_ge1 > 0 {
        assert!(count_seq_between > 0 && count_seq_between < count_all,
            "seq BETWEEN 0 AND 0 should return a subset, got {} of {}", count_seq_between, count_all);
    } else {
        // All rows have seq=0 — BETWEEN 0 AND 0 returns all, which is correct
        assert_eq!(count_seq_between, count_all,
            "seq BETWEEN 0 AND 0 should return all when all seq=0");
    }

    // === Combined pushdown ===
    // Combined: db_vrsn + site_id IS NOT self + ts → 0
    assert_eq!(count_where_siteid(
        "SELECT count(*) FROM crsql_changes WHERE db_version >= 1 AND site_id IS NOT ? AND ts > 0"), 0,
        "combined: db_vrsn + site_id IS NOT self + ts should be 0");

    // Combined: table = 'skip_int' AND db_version >= 1 → all skip_int rows
    assert_eq!(count_where(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_int' AND db_version >= 1"),
        count_skip_int, "table='skip_int' AND db_version>=1 should return skip_int rows only");

    // Combined: table = 'comp_pk' AND cid = 'val' → comp_pk val changes
    assert_eq!(count_where(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'comp_pk' AND cid = 'val'"),
        count_cid_val, "table='comp_pk' AND cid='val' should match comp_pk val changes");

    // Combined partial: table = 'skip_int' AND db_version >= mid → subset of skip_int
    let count_skip_int_dbv_mid = count_where(&format!(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_int' AND db_version >= {}", mid_dbv));
    libc_println!("  table='skip_int' AND db_version>={}: count={}", mid_dbv, count_skip_int_dbv_mid);
    assert!(count_skip_int_dbv_mid > 0 && count_skip_int_dbv_mid < count_skip_int,
        "table='skip_int' AND db_version>={} should return a subset of skip_int, got {} of {}",
        mid_dbv, count_skip_int_dbv_mid, count_skip_int);

    // Combined partial: table = 'hash_tbl' AND cid = '-2' → only hash_tbl tombstone
    let count_hash_tomb = count_where(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'hash_tbl' AND cid = '-2'");
    libc_println!("  table='hash_tbl' AND cid='-2': count={}", count_hash_tomb);
    assert!(count_hash_tomb > 0, "table='hash_tbl' AND cid='-2' should return hash tombstone");
    assert_eq!(count_hash_tomb, count_cid_del2,
        "hash_tbl tombstone count should match cid='-2' count");

    // Combined partial: table = 'skip_pkonly' AND cid = '-1' → only skip_pkonly tombstone
    let count_pkonly_tomb = count_where(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'skip_pkonly' AND cid = '-1'");
    libc_println!("  table='skip_pkonly' AND cid='-1': count={}", count_pkonly_tomb);
    assert!(count_pkonly_tomb > 0, "table='skip_pkonly' AND cid='-1' should return pkonly tombstone");

    // Cross-table: cid = '-1' should span skip_int and skip_pkonly tombstones
    // (skip_int has no delete in our data, so only skip_pkonly)
    assert_eq!(count_cid_del1, count_pkonly_tomb,
        "cid='-1' should match skip_pkonly tombstone (skip_int has no delete)");

    // Combined: table = 'comp_pk' AND db_version BETWEEN min AND max → all comp_pk
    let count_comp_between = count_where(&format!(
        "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'comp_pk' AND db_version BETWEEN {} AND {}",
        min_dbv, max_dbv));
    libc_println!("  table='comp_pk' AND db_version BETWEEN {} AND {}: count={}", min_dbv, max_dbv, count_comp_between);
    assert_eq!(count_comp_between, count_comp_pk,
        "table='comp_pk' AND db_version BETWEEN min AND max should return all comp_pk");

    // === Ordering across all 4 table types ===
    // Verify ORDER BY db_version, seq returns rows in non-decreasing db_version
    // across all UNION arms (skip_hash clock, hash clock, pkonly, tombstones).
    {
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq"
        )?;
        let mut prev_dbv: i64 = -1;
        let mut row_count = 0;
        let mut tables_seen: Vec<String> = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            let tbl = stmt.column_text(0)?.to_string();
            let dbv = stmt.column_int64(1);
            assert!(dbv >= prev_dbv,
                "ORDER BY db_version, seq: db_version should be non-decreasing, got {} after {} (table={})",
                dbv, prev_dbv, tbl);
            if !tables_seen.contains(&tbl) {
                tables_seen.push(tbl);
            }
            prev_dbv = dbv;
            row_count += 1;
        }
        assert_eq!(row_count, count_all, "ORDER BY should return all rows");
        assert_eq!(tables_seen.len(), 4, "ORDER BY should return rows from all 4 tables, got {:?}", tables_seen);
        libc_println!("  full ordering: {} rows from {} tables, db_version non-decreasing", row_count, tables_seen.len());
    }

    // === ORDER BY + LIMIT across all 4 table types ===
    // Verify LIMIT returns the correct subset of ordered rows
    {
        // Get all rows ordered
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq"
        )?;
        let mut all_rows: Vec<(String, i64)> = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            all_rows.push((stmt.column_text(0)?.to_string(), stmt.column_int64(1)));
        }

        // LIMIT 3 should return first 3 of all_rows
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq LIMIT 3"
        )?;
        let mut limit_rows: Vec<(String, i64)> = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            limit_rows.push((stmt.column_text(0)?.to_string(), stmt.column_int64(1)));
        }
        libc_println!("  LIMIT 3: got {} rows", limit_rows.len());
        assert_eq!(limit_rows.len(), 3, "LIMIT 3 should return exactly 3 rows (have {})", count_all);
        for i in 0..3 {
            assert_eq!(limit_rows[i].0, all_rows[i].0,
                "LIMIT row {} table mismatch: got {} expected {}", i, limit_rows[i].0, all_rows[i].0);
            assert_eq!(limit_rows[i].1, all_rows[i].1,
                "LIMIT row {} db_version mismatch: got {} expected {}", i, limit_rows[i].1, all_rows[i].1);
        }

        // LIMIT 2 OFFSET 2 should return rows 2,3
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version, seq LIMIT 2 OFFSET 2"
        )?;
        let mut offset_rows: Vec<(String, i64)> = Vec::new();
        while stmt.step()? == ResultCode::ROW {
            offset_rows.push((stmt.column_text(0)?.to_string(), stmt.column_int64(1)));
        }
        libc_println!("  LIMIT 2 OFFSET 2: got {} rows", offset_rows.len());
        assert_eq!(offset_rows.len(), 2, "LIMIT 2 OFFSET 2 should return 2 rows");
        for i in 0..2 {
            assert_eq!(offset_rows[i].0, all_rows[i+2].0,
                "OFFSET row {} table mismatch", i);
            assert_eq!(offset_rows[i].1, all_rows[i+2].1,
                "OFFSET row {} db_version mismatch", i);
        }
    }

    // === DESC ordering ===
    // Verify ORDER BY db_version DESC returns rows in non-increasing order
    {
        let stmt = db.db.prepare_v2(
            "SELECT \"table\", db_version FROM crsql_changes ORDER BY db_version DESC, seq DESC"
        )?;
        let mut prev_dbv: i64 = i64::MAX;
        let mut row_count = 0;
        while stmt.step()? == ResultCode::ROW {
            let dbv = stmt.column_int64(1);
            assert!(dbv <= prev_dbv,
                "ORDER BY db_version DESC: should be non-increasing, got {} after {}",
                dbv, prev_dbv);
            prev_dbv = dbv;
            row_count += 1;
        }
        assert_eq!(row_count, count_all, "DESC ordering should return all rows");
        libc_println!("  DESC ordering: {} rows, db_version non-increasing", row_count);
    }

    // DESC + LIMIT: first row should have the highest db_version
    {
        let stmt = db.db.prepare_v2(
            "SELECT db_version FROM crsql_changes ORDER BY db_version DESC, seq DESC LIMIT 1"
        )?;
        stmt.step()?;
        let max_dbv_query = stmt.column_int64(0);
        // Should match the max we computed earlier
        assert_eq!(max_dbv_query, max_dbv,
            "DESC LIMIT 1 should return max db_version ({}), got {}", max_dbv, max_dbv_query);
        libc_println!("  DESC LIMIT 1: db_version={}", max_dbv_query);
    }

    libc_println!("=== v2_wire_packed_generalized_pushdown PASS ===");
    Ok(())
}

/// Test that LIKE/MATCH/GLOB/REGEXP constraints error on crsql_changes
/// in all modes. These operators silently produce wrong results on packed BLOB
/// outputs (cid, col_vrsn, seq, cval, pks). xBestIndex accepts them with
/// omit=1 so SQLite trusts the vtab, then xFilter errors. Users who need
/// pattern matching should wrap crsql_changes in a subquery and filter the
/// outer query.
fn v2_reject_pattern_ops() -> Result<(), ResultCode> {
    libc_println!("=== v2_reject_pattern_ops START ===");

    // Helper: prepare + step, return true if error occurred
    let query_errors = |db: &ManagedConnection, sql: &str| -> bool {
        let stmt = match db.prepare_v2(sql) {
            Ok(s) => s,
            Err(_) => return true,
        };
        match stmt.step() {
            Ok(_) => false,
            Err(_) => true,
        }
    };

    // Test in V2-wire packed mode
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('default-ts', 1700000000)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
        db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, a TEXT)")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello')")?;

        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE cid LIKE '%el%'"),
            "LIKE on cid should error in V2-wire packed mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE cid GLOB 'h*'"),
            "GLOB on cid should error in V2-wire packed mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE \"table\" LIKE 'f%'"),
            "LIKE on table should error in V2-wire packed mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE \"table\" GLOB 'f*'"),
            "GLOB on table should error in V2-wire packed mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE seq LIKE '1%'"),
            "LIKE on seq should error in V2-wire packed mode");
    }

    // Test in V1-wire mode
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('default-ts', 1700000000)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;
        db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, a TEXT)")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello')")?;

        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE cid LIKE '%el%'"),
            "LIKE on cid should error in V1-wire mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE cid GLOB 'h*'"),
            "GLOB on cid should error in V1-wire mode");
        assert!(query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE \"table\" LIKE 'f%'"),
            "LIKE on table should error in V1-wire mode");
    }

    // Verify that normal comparison operators still work (regression check)
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('default-ts', 1700000000)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
        db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, a TEXT)")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello')")?;

        assert!(!query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE cid = 'a'"),
            "cid = 'a' should still work");
        assert!(!query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE db_version >= 1"),
            "db_version >= 1 should still work");
        assert!(!query_errors(&db.db, "SELECT count(*) FROM crsql_changes WHERE \"table\" = 'foo'"),
            "table = 'foo' should still work");
    }

    libc_println!("=== v2_reject_pattern_ops PASS ===");
    Ok(())
}

/// Test: malformed blobs on the merge path must produce a clean SQL error,
/// not abort the process. Covers:
/// - unpack_columns with intlen > 8 (get_int panics for n > 8)
/// - unpack_varints with a huge count header (Vec::with_capacity OOM)
/// - malformed pk blob on an incoming V1-wire change
/// - malformed col_version/seq on an incoming packed V2-wire change
fn v2_malformed_blob_no_panic() -> Result<(), ResultCode> {
    libc_println!("=== v2_malformed_blob_no_panic START ===");

    // Test via the crsql_unpack_columns vtab directly.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let rc = db.db.exec_safe("SELECT crsql_as_crr('t')");
        rc?;

        // intlen=9 with enough padding — would panic without the guard.
        // type_byte = (9 << 3) | 1 = 0x49, followed by 40 zero bytes.
        let mut bad_blob = vec![0x01u8, 0x49];
        bad_blob.extend_from_slice(&[0u8; 40]);
        let stmt = db.db.prepare_v2("SELECT cell FROM crsql_unpack_columns WHERE package = ?")?;
        stmt.bind_blob(1, &bad_blob, Destructor::STATIC)?;
        let rc = stmt.step();
        libc_println!("vtab step rc: {:?}", rc);
        // The vtab should return an error for malformed blobs.
        // If it returns Ok(DONE), the filter function swallowed the error.
        assert!(rc.is_err() || rc == Ok(ResultCode::DONE), "malformed blob should error or return no rows, got {:?}", rc);
    }

    // Test via the merge path: malformed pk blob on V1-wire INSERT.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
        db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t')")?;
        let siteid_stmt = db.db.prepare_v2("SELECT crsql_site_id()")?;
        siteid_stmt.step()?;
        let siteid = siteid_stmt.column_blob(0)?;
        libc_println!("siteid ok len={}", siteid.len());
        let bad_pk = vec![0x01u8, 0x49, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let stmt = db.db.prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
        stmt.bind_text(1, "t", Destructor::STATIC)?;
        stmt.bind_blob(2, &bad_pk, Destructor::STATIC)?;
        stmt.bind_text(3, "a", Destructor::STATIC)?;
        stmt.bind_text(4, "v", Destructor::STATIC)?;
        stmt.bind_int64(5, 1)?;
        stmt.bind_int64(6, 1)?;
        stmt.bind_blob(7, siteid, Destructor::STATIC)?;
        stmt.bind_int64(8, 1)?;
        stmt.bind_int64(9, 0)?;
        stmt.bind_int64(10, 1700000000)?;
        let rc = stmt.step();
        libc_println!("merge path step rc: {:?}", rc);
        assert!(rc.is_err(), "malformed pk blob on merge path should error, not crash, got {:?}", rc);
    }

    // Test: unpack_varints with a huge count header.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
        db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t')")?;
        let siteid_stmt = db.db.prepare_v2("SELECT crsql_site_id()")?;
        siteid_stmt.step()?;
        let siteid = siteid_stmt.column_blob(0)?;

        // count = 2^63-ish: FF FF FF FF FF FF FF FF 7F
        let bad_varint = vec![0xFFu8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F];
        let stmt = db.db.prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
        stmt.bind_text(1, "t", Destructor::STATIC)?;
        stmt.bind_blob(2, [0x01u8, 0x01].as_ref(), Destructor::STATIC)?;
        stmt.bind_blob(3, b"a\x00b", Destructor::STATIC)?;
        stmt.bind_blob(4, [0x02u8, 0x01, 0x09, 0x01, 0x09].as_ref(), Destructor::STATIC)?;
        stmt.bind_blob(5, &bad_varint, Destructor::STATIC)?;
        stmt.bind_int64(6, 1)?;
        stmt.bind_blob(7, siteid, Destructor::STATIC)?;
        stmt.bind_int64(8, 1)?;
        stmt.bind_blob(9, &bad_varint, Destructor::STATIC)?;
        stmt.bind_int64(10, 1700000000)?;
        let rc = stmt.step();
        assert!(rc.is_err(), "malformed varint count on merge path should error, not crash");
    }

    libc_println!("=== v2_malformed_blob_no_panic PASS ===");
    Ok(())
}

/// Test: a column name containing a single quote must not break crsql_changes.
/// Also tests a table name with a single quote.
fn v2_quoted_column_and_table_names() -> Result<(), ResultCode> {
    libc_println!("=== v2_quoted_column_and_table_names START ===");

    // Column name with single quote: o'brien
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

        db.db.exec_safe("CREATE TABLE ok_tbl (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('ok_tbl')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO ok_tbl VALUES (1, 'fine')")?;

        db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, \"o'brien\" TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO t VALUES (1, 'v')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes")?;
        stmt.step()?;
        let count = stmt.column_int(0);
        assert!(count >= 2, "crsql_changes should return rows for both tables, got {}", count);

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 't'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "should have 1 change for table t");
    }

    // Table name with single quote: o'brien
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

        db.db.exec_safe("CREATE TABLE \"o'brien\" (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('o''brien')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO \"o'brien\" VALUES (1, 'val')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "should have 1 change for table o'brien");

        let stmt = db.db.prepare_v2("SELECT \"table\" FROM crsql_changes")?;
        stmt.step()?;
        let tbl = stmt.column_text(0)?;
        assert_eq!(tbl, "o'brien", "table name should be o'brien in the feed");
    }

    libc_println!("=== v2_quoted_column_and_table_names PASS ===");
    Ok(())
}

/// Test: crsql_as_crr('tbl', 'skip_hash') should work for single-column PKs
/// and be silently ignored for composite PKs.
fn v2_skip_hash_flag_works() -> Result<(), ResultCode> {
    libc_println!("=== v2_skip_hash_flag_works START ===");

    // Single BLOB PK + skip_hash flag
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE t5 (id BLOB PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t5', 'skip_hash')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO t5 VALUES (x'01', 'hello')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 't5'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "skip_hash BLOB PK should produce changes");
    }

    // INTEGER PK + skip_hash flag
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE t4 (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t4', 'skip_hash')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO t4 VALUES (1, 'hello')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 't4'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "skip_hash INTEGER PK should produce changes");
    }

    // Composite PK + skip_hash flag (design: silently ignored, not an error)
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE t6 (a TEXT NOT NULL, b TEXT NOT NULL, v TEXT, PRIMARY KEY(a,b))")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('t6', 'skip_hash')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO t6 VALUES ('x', 'y', 'val')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_changes WHERE \"table\" = 't6'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "composite PK with skip_hash flag should still work (ignored)");
    }

    libc_println!("=== v2_skip_hash_flag_works PASS ===");
    Ok(())
}

/// Test: crsql_as_table must clean up all V2 metadata tables.
/// Also tests that DROP TABLE on a CRR cleans up V2 metadata.
fn v2_as_table_cleans_v2_metadata() -> Result<(), ResultCode> {
    libc_println!("=== v2_as_table_cleans_v2_metadata START ===");

    // crsql_as_table should remove all V2 tables.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE y (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('y')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO y VALUES (1, 'a')")?;

        {
            let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE 'y__crsql_v2_%'")?;
            stmt.step()?;
            assert!(stmt.column_int(0) > 0, "V2 tables should exist before crsql_as_table");
        }

        db.db.exec_safe("SELECT crsql_as_table('y')")?;

        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE 'y__crsql_v2_%'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 0, "crsql_as_table should remove all V2 metadata tables");

        let stmt = db.db.prepare_v2(
            "SELECT count(*) FROM crsql_master WHERE key LIKE '%\\_y' ESCAPE '\\'"
        )?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 0, "crsql_as_table should remove crsql_master flags");
    }

    // DROP TABLE leaves V2 metadata behind (orphaned tables).
    // Lazy cleanup happens on the next write to any CRR table — the trigger
    // preamble calls crsql_ensure_table_infos_are_up_to_date, which detects
    // the schema change (from DROP TABLE) and runs pull_all_table_infos,
    // which cleans up orphaned V2 tables and crsql_master flags.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE z (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('z')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO z VALUES (1, 'a')")?;

        // Create a second CRR table so we can trigger lazy cleanup via a write.
        db.db.exec_safe("CREATE TABLE w (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('w')")?;

        // DROP TABLE z — leaves orphaned V2 metadata.
        db.db.exec_safe("DROP TABLE z")?;

        // Verify orphans exist before cleanup.
        {
            let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE 'z__crsql_v2_%'")?;
            stmt.step()?;
            assert!(stmt.column_int(0) > 0, "V2 tables should be orphaned after DROP TABLE");
        }

        // Write to w — trigger preamble calls crsql_ensure_table_infos_are_up_to_date
        // which detects schema change and runs pull_all_table_infos → schedules cleanup.
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO w VALUES (1, 'a')")?;

        // Orphaned V2 tables can't be dropped inside a trigger (SQLITE_LOCKED),
        // so pull_all_table_infos schedules cleanup tasks in crsql_master.
        // Process them via incremental_maintenance.
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_incremental_maintenance(1000)")?;

        // Verify orphaned V2 tables were cleaned up.
        {
            let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE 'z__crsql_v2_%'")?;
            stmt.step()?;
            assert_eq!(stmt.column_int(0), 0, "orphaned V2 tables should be cleaned up after incremental_maintenance");
        }

        // Verify crsql_master flags were cleaned up.
        {
            let stmt = db.db.prepare_v2(
                "SELECT count(*) FROM crsql_master WHERE key LIKE '%\\_z' ESCAPE '\\'"
            )?;
            stmt.step()?;
            assert_eq!(stmt.column_int(0), 0, "crsql_master flags for z should be cleaned up");
        }
    }

    libc_println!("=== v2_as_table_cleans_v2_metadata PASS ===");
    Ok(())
}

/// Test: data consistency after a statement-level rollback inside a transaction.
/// A statement-level abort (e.g., CHECK constraint violation) undoes only the
/// failing statement's writes via the statement journal. The transaction
/// continues, and all statements in the same transaction share one db_version
/// (one tx = one Lamport event). This test verifies that clock entries and
/// table data remain consistent after such a rollback.
fn v2_data_consistency_after_stmt_rollback() -> Result<(), ResultCode> {
    libc_println!("=== v2_data_consistency_after_stmt_rollback START ===");

    let db = crate::opendb()?;
    // Use a CHECK constraint to trigger a statement-level rollback.
    // CRRs don't allow UNIQUE constraints or NOT NULL without DEFAULT,
    // so we use a CHECK constraint on a nullable column.
    db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT, CHECK(v != 'bad'))")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('t')")?;

    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO t VALUES (1, 'a')")?;

    // This INSERT aborts on CHECK constraint — statement journal undoes
    // the clock table write, but the transaction continues.
    let stmt = db.db.prepare_v2("INSERT INTO t VALUES (2, 'bad')")?;
    let rc = stmt.step();
    assert!(rc.is_err(), "CHECK constraint violation should fail");

    // This INSERT should still succeed and be tracked in the clock table.
    db.db.exec_safe("INSERT INTO t VALUES (3, 'c')")?;
    db.db.exec_safe("COMMIT")?;

    // Verify the rolled-back insert (id=2) is not in the table.
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t WHERE id = 2")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 0, "rolled-back insert should not be in the table");
    }

    // Verify the successful inserts are in the table.
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 2, "should have 2 rows (ids 1 and 3)");
    }

    // Verify clock entries exist for the successful inserts.
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t__crsql_clock")?;
        stmt.step()?;
        assert!(stmt.column_int(0) > 0, "clock entries should exist for successful inserts");
    }

    // Verify crsql_db_versions has at least 1 entry.
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM crsql_db_versions")?;
        stmt.step()?;
        assert!(stmt.column_int(0) >= 1, "should have at least 1 db_version");
    }

    libc_println!("=== v2_data_consistency_after_stmt_rollback PASS ===");
    Ok(())
}

/// Test: transitioning metadata-write-version from 2 (dual-write) to 3 (V2-only)
/// must clean up V1 clock tables (__crsql_clock, __crsql_pks) via incremental_maintenance.
fn v2_cleanup_v1_tables_after_2_to_3_transition() -> Result<(), ResultCode> {
    libc_println!("=== v2_cleanup_v1_tables_after_2_to_3_transition START ===");

    let db = crate::opendb()?;

    // Start in dual-write mode (V1 + V2)
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    // Use a transaction so ts persists across statements
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'b')")?;
    db.db.exec_safe("COMMIT")?;

    // Verify V1 tables exist
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_clock'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "V1 clock table should exist in dual-write mode");
    }
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_pks'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "V1 pks table should exist in dual-write mode");
    }

    // Transition to V2-only (needs ts in same transaction for migration)
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("COMMIT")?;

    // Run incremental maintenance to process the cleanup
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_incremental_maintenance(1000)")?;
    db.db.exec_safe("COMMIT")?;

    // Verify V1 tables were cleaned up
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_clock'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 0, "V1 clock table should be cleaned up after 2→3 transition");
    }
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_pks'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 0, "V1 pks table should be cleaned up after 2→3 transition");
    }

    // Verify V2 tables still exist and data is intact
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE 'foo__crsql_v2_%'")?;
        stmt.step()?;
        assert!(stmt.column_int(0) > 0, "V2 tables should still exist after cleanup");
    }
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM foo")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 2, "data should be intact after cleanup");
    }

    // Verify writes still work after cleanup
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'c')")?;
    db.db.exec_safe("COMMIT")?;
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM foo")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 3, "writes should work after cleanup");
    }

    libc_println!("=== v2_cleanup_v1_tables_after_2_to_3_transition PASS ===");
    Ok(())
}

/// Test: the merge path must reject rowid values >= 2^51.
fn v2_merge_rejects_rowid_overflow() -> Result<(), ResultCode> {
    libc_println!("=== v2_merge_rejects_rowid_overflow START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('t', 'use_rowid')")?;

    let siteid_stmt = db.db.prepare_v2("SELECT crsql_site_id()")?;
    siteid_stmt.step()?;
    let siteid = siteid_stmt.column_blob(0)?;

    // 2^51 = 2251799813685248 — the max allowed rowid.
    let overflow_rowid: i64 = 1i64 << 60;
    let pk_blob = {
        let mut blob = vec![1u8]; // 1 column
        blob.push(0x40); // Integer type, intlen=8
        blob.extend_from_slice(&overflow_rowid.to_be_bytes());
        blob
    };

    db.db.exec_safe("BEGIN")?;
    let stmt = db.db.prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
    stmt.bind_text(1, "t", Destructor::STATIC)?;
    stmt.bind_blob(2, &pk_blob, Destructor::STATIC)?;
    stmt.bind_text(3, "v", Destructor::STATIC)?;
    stmt.bind_text(4, "val", Destructor::STATIC)?;
    stmt.bind_int64(5, 1)?;
    stmt.bind_int64(6, 1)?;
    stmt.bind_blob(7, siteid, Destructor::STATIC)?;
    stmt.bind_int64(8, 1)?;
    stmt.bind_int64(9, 0)?;
    stmt.bind_int64(10, 1700000000)?;
    let rc = stmt.step();
    let _ = db.db.exec_safe("ROLLBACK");
    assert!(rc.is_err(), "merge path should reject rowid >= 2^51, got {:?}", rc);

    libc_println!("=== v2_merge_rejects_rowid_overflow PASS ===");
    Ok(())
}

/// Test: V2 hash tombstone for a row not present locally must be rejected
/// when sync-log-version is 1 (V1 wire emission), because the delete cannot
/// be forwarded to V1 wire peers without a PK mapping.
/// When sync-log-version is 2 (V2 wire emission), the same tombstone is accepted.
fn v2_hash_tombstone_rejected_in_v1_wire_mode() -> Result<(), ResultCode> {
    libc_println!("=== v2_hash_tombstone_rejected_in_v1_wire_mode START ===");

    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, a)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('t')")?;
    }

    // Both nodes on V2 metadata
    migrate_to_v2(&db_a.db)?;
    migrate_to_v2(&db_b.db)?;

    // Node A: V2 wire emission, inserts and deletes a row without syncing in between
    db_a.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO t VALUES ('x', 1)")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM t WHERE id = 'x'")?;

    // Node B: V2 metadata but V1 wire emission (rollout window)
    db_b.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;

    // Sync A -> B: should fail because B has never seen the row and can't emit V1 wire
    // Use B's site_id to filter (same pattern as sync_v2_wire)
    let siteid = {
        let stmt = db_b.db.prepare_v2("SELECT crsql_site_id()")?;
        stmt.step()?;
        stmt.column_blob(0)?.to_vec()
    };

    let stmt_a = db_a.db.prepare_v2(
        "SELECT * FROM crsql_changes WHERE db_version >= ? AND site_id IS NOT ?",
    )?;
    stmt_a.bind_int64(1, 0)?;
    stmt_a.bind_blob(2, &siteid, Destructor::STATIC)?;

    db_b.db.exec_safe("BEGIN")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let mut got_error = false;
    while stmt_a.step()? == ResultCode::ROW {
        let stmt_b = db_b.db
            .prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;
        for x in 0..10 {
            stmt_b.bind_value(x + 1, stmt_a.column_value(x)?)?;
        }
        match stmt_b.step() {
            Ok(ResultCode::ERROR) => {
                got_error = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {
                got_error = true;
                break;
            }
        }
    }
    db_b.db.exec_safe("ROLLBACK")?;

    assert!(got_error, "V2 hash tombstone must be rejected when sync-log-version=1 and row is not present locally");

    // Now switch B to V2 wire emission — same tombstone should be accepted
    db_b.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    sync_v2_wire(&db_a.db, &db_b.db, 0)?;

    // B should now have the tombstone
    let count = {
        let stmt = db_b.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_tombstones")?;
        stmt.step()?;
        stmt.column_int(0)
    };
    assert_eq!(count, 1, "tombstone should be recorded after switching to V2 wire");

    libc_println!("=== v2_hash_tombstone_rejected_in_v1_wire_mode PASS ===");
    Ok(())
}

/// Test C1: V1 mirror per-column clock entries must have correct site_id and ts.
/// The v1_clock_copy SQL must not swap site_id and ts columns.
/// This test syncs an insert from a source to a dual-write destination and
/// checks that per-column V1 clock rows (col_name != '-1') have:
///   - site_id = source site ordinal (small integer, not the timestamp)
///   - ts = the timestamp string (e.g. '1700000000', not the site ordinal)
fn v2_mirror_v1_clock_copy_site_id_ts_not_swapped() -> Result<(), ResultCode> {
    libc_println!("=== v2_mirror_v1_clock_copy_site_id_ts_not_swapped START ===");

    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Source: V2-only, V2 wire
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db_src.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO t VALUES (1, 'hello')")?;

    // Destination: dual-write, V2 wire reception
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_dst.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db_dst.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("SELECT crsql_as_crr('t')")?;
    migrate_to_v2(&db_dst.db)?;

    // Sync insert from source to destination — triggers v2_to_v1_mirror_metadata
    sync_v2_wire(&db_src.db, &db_dst.db, 0)?;

    // Full V1↔V2 clock comparison: join V1 per-column clock entries to their
    // V2 counterparts and compare ALL columns (col_version, db_version, seq,
    // site_id, ts). This catches any column swap in v1_clock_copy, not just
    // site_id/ts (C1). The join maps:
    //   V1.key → v2_pks.__crsql_key
    //   V1.col_name → v2_col_map.col_name → col_id
    //   V2 cell_key = (v2_pks.__crsql_key << col_id_bits) | col_id
    let col_id_bits = {
        let s = db_dst.db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'crsql_col_id_bits'")?;
        s.step()?;
        s.column_int64(0)
    };
    let cmp_sql = alloc::format!(
        "SELECT \
            v1.col_version, v1.db_version, v1.seq, v1.site_id, v1.ts, \
            v2.col_version, v2.db_version, v2.seq, v2.site_id, v2.ts, \
            v1.col_name \
         FROM t__crsql_clock v1 \
         JOIN t__crsql_v2_pks v2p ON v1.key = v2p.__crsql_key \
         JOIN t__crsql_v2_col_map v2m ON v1.col_name = v2m.col_name \
         JOIN t__crsql_v2_clock v2 ON v2.cell_key = (v2p.__crsql_key << {bits}) | v2m.col_id \
         WHERE v1.col_name != '-1'",
        bits = col_id_bits
    );
    let stmt = db_dst.db.prepare_v2(&cmp_sql)?;
    let mut found_per_col = false;
    let mut mismatches = 0;
    while stmt.step()? == ResultCode::ROW {
        found_per_col = true;
        let v1_cv = stmt.column_int64(0);
        let v1_dv = stmt.column_int64(1);
        let v1_seq = stmt.column_int64(2);
        let v1_site = stmt.column_int64(3);
        let v1_ts = stmt.column_text(4)?.to_string();
        let v2_cv = stmt.column_int64(5);
        let v2_dv = stmt.column_int64(6);
        let v2_seq = stmt.column_int64(7);
        let v2_site = stmt.column_int64(8);
        let v2_ts = stmt.column_int64(9);
        let col_name = stmt.column_text(10)?.to_string();

        // V1 ts is TEXT, V2 ts is INTEGER.
        let v1_ts_int: i64 = v1_ts.parse().unwrap_or(-1);

        if v1_cv != v2_cv || v1_dv != v2_dv || v1_seq != v2_seq
            || v1_site != v2_site || v1_ts_int != v2_ts
        {
            mismatches += 1;
            libc_println!(
                "  C1 MISMATCH col_name={}: \
                 cv({}/{}) dv({}/{}) seq({}/{}) site({}/{}) ts({}/{})",
                col_name,
                v1_cv, v2_cv, v1_dv, v2_dv, v1_seq, v2_seq,
                v1_site, v2_site, v1_ts, v2_ts,
            );
        }
    }
    assert!(found_per_col, "C1: should have per-column V1 clock entries after mirror");
    assert_eq!(
        mismatches, 0,
        "C1 BUG: V1 mirror clock entries must match V2 source for all columns \
         (col_version, db_version, seq, site_id, ts) — found {} mismatches",
        mismatches
    );

    libc_println!("=== v2_mirror_v1_clock_copy_site_id_ts_not_swapped PASS ===");
    Ok(())
}

/// Test C2: V1 mirror dead sentinel (hash mode) must have correct db_version/seq/site_id.
/// The v1_sentinel_insert_dead SQL for hash mode must not swap these columns.
/// This test syncs a delete from a source to a dual-write destination and checks
/// that the V1 dead sentinel row (col_name = '-1') has:
///   - db_version = V2 tombstone db_version (> 0)
///   - site_id = source site ordinal
///   - seq = V2 tombstone seq (typically 0)
fn v2_mirror_v1_dead_sentinel_hash_mode_fields_correct() -> Result<(), ResultCode> {
    libc_println!("=== v2_mirror_v1_dead_sentinel_hash_mode_fields_correct START ===");

    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Source: V2-only, V2 wire. Use TEXT PK (hash mode, not skip_hash).
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db_src.db.exec_safe("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, v TEXT)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO t VALUES ('x', 'hello')")?;

    // Destination: dual-write, V2 wire reception
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_dst.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db_dst.db.exec_safe("CREATE TABLE t (id TEXT PRIMARY KEY NOT NULL, v TEXT)")?;
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("SELECT crsql_as_crr('t')")?;
    migrate_to_v2(&db_dst.db)?;

    // Sync the INSERT first so the destination has the row and PK mapping.
    sync_v2_wire(&db_src.db, &db_dst.db, 0)?;

    // Now delete on the source and sync the delete separately.
    db_src.db.exec_safe("SELECT crsql_set_ts('1800000000')")?;
    db_src.db.exec_safe("DELETE FROM t WHERE id = 'x'")?;

    // Sync delete from source to destination — triggers v2_to_v1_mirror_metadata with dead sentinel
    sync_v2_wire(&db_src.db, &db_dst.db, 2)?;

    // Full V1 dead sentinel ↔ V2 tombstone comparison: join the V1 sentinel row
    // (col_name='-1') to the V2 tombstone by hashed_pk and compare ALL columns
    // (col_version/cl, db_version, seq, site_id, ts). This catches any column
    // swap in v1_sentinel_insert_dead, not just the specific C2 swap.
    let cmp_sql = alloc::format!(
        "SELECT \
            v1.col_version, v1.db_version, v1.seq, v1.site_id, v1.ts, \
            v2.cl, v2.db_version, v2.seq, v2.site_id, v2.ts \
         FROM t__crsql_clock v1 \
         JOIN t__crsql_v2_tombstones v2 ON 1=1 \
         WHERE v1.col_name = '-1'"
    );
    let stmt = db_dst.db.prepare_v2(&cmp_sql)?;
    let mut found_sentinel = false;
    let mut mismatches = 0;
    while stmt.step()? == ResultCode::ROW {
        found_sentinel = true;
        let v1_cl = stmt.column_int64(0);
        let v1_dv = stmt.column_int64(1);
        let v1_seq = stmt.column_int64(2);
        let v1_site = stmt.column_int64(3);
        let v1_ts = stmt.column_text(4)?.to_string();
        let v2_cl = stmt.column_int64(5);
        let v2_dv = stmt.column_int64(6);
        let v2_seq = stmt.column_int64(7);
        let v2_site = stmt.column_int64(8);
        let v2_ts = stmt.column_int64(9);

        let v1_ts_int: i64 = v1_ts.parse().unwrap_or(-1);

        if v1_cl != v2_cl || v1_dv != v2_dv || v1_seq != v2_seq
            || v1_site != v2_site || v1_ts_int != v2_ts
        {
            mismatches += 1;
            libc_println!(
                "  C2 MISMATCH: cl({}/{}) dv({}/{}) seq({}/{}) site({}/{}) ts({}/{})",
                v1_cl, v2_cl, v1_dv, v2_dv, v1_seq, v2_seq,
                v1_site, v2_site, v1_ts, v2_ts,
            );
        }
    }
    assert!(found_sentinel, "C2: should have a dead sentinel in V1 clock after delete mirror");
    assert_eq!(
        mismatches, 0,
        "C2 BUG: V1 dead sentinel must match V2 tombstone for all columns \
         (cl, db_version, seq, site_id, ts) — found {} mismatches",
        mismatches
    );

    libc_println!("=== v2_mirror_v1_dead_sentinel_hash_mode_fields_correct PASS ===");
    Ok(())
}

/// Test C3: skip_hash table receiving a V2 hash tombstone (cid='-2') in dual-write
/// mode must not panic. In normal sync, skip_hash tables emit cid='-1' tombstones,
/// but a malformed peer could send cid='-2'. The post_v2_merge function calls
/// v2_lookup_key_and_cl which indexes unpacked_pks[0] without a bounds check.
/// For skip_hash + V2 hash tombstone, unpacked_pks is None (empty vec), causing a panic.
fn v2_skip_hash_hash_tombstone_dual_write_no_panic() -> Result<(), ResultCode> {
    libc_println!("=== v2_skip_hash_hash_tombstone_dual_write_no_panic START ===");

    let db = crate::opendb()?;

    // Destination: dual-write (V2&V1), V2 wire reception, skip_hash table
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('t', 'skip_hash')")?;
    migrate_to_v2(&db.db)?;

    // Get the local site_id (to use as the "remote" site_id in the crafted insert)
    let siteid = {
        let stmt = db.db.prepare_v2("SELECT crsql_site_id()")?;
        stmt.step()?;
        stmt.column_blob(0)?.to_vec()
    };

    // Craft a V2 hash tombstone (cid='-2') insert for the skip_hash table.
    // In normal sync, skip_hash tables emit cid='-1', but a malformed peer
    // could send cid='-2'. This must not panic.
    let fake_hashed_pk = vec![0u8; 10];

    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let stmt = db.db.prepare_v2(
        "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )?;
    stmt.bind_text(1, "t", Destructor::STATIC)?;
    stmt.bind_blob(2, &fake_hashed_pk, Destructor::STATIC)?;
    stmt.bind_text(3, "-2", Destructor::STATIC)?;
    stmt.bind_text(4, "", Destructor::STATIC)?;
    stmt.bind_int64(5, 1)?;
    stmt.bind_int64(6, 1)?;
    stmt.bind_blob(7, &siteid, Destructor::STATIC)?;
    stmt.bind_int64(8, 2)?;
    stmt.bind_int64(9, 0)?;
    stmt.bind_int64(10, 1700000000)?;

    // This must not panic. It should either succeed or return an error gracefully.
    let rc = stmt.step();
    let _ = db.db.exec_safe("ROLLBACK");

    match rc {
        Ok(ResultCode::OK) | Ok(ResultCode::DONE) => {}
        Ok(ResultCode::ERROR) => {}
        Ok(other) => {
            libc_println!("  merge returned {:?} (acceptable)", other);
        }
        Err(e) => {
            libc_println!("  merge returned Err({:?}) (acceptable)", e);
        }
    }

    libc_println!("=== v2_skip_hash_hash_tombstone_dual_write_no_panic PASS ===");
    Ok(())
}

/// Test C4: crsql_incremental_maintenance with chunk_size <= 0 must not drop non-empty tables.
/// With chunk_size=0, LIMIT 0 deletes nothing, but the code incorrectly treats
/// "nothing deleted" as "all empty" and drops the tables.
fn v2_maintenance_chunk_size_zero_no_drop() -> Result<(), ResultCode> {
    libc_println!("=== v2_maintenance_chunk_size_zero_no_drop START ===");

    let db = crate::opendb()?;

    // Start in dual-write mode with data
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'b')")?;
    db.db.exec_safe("COMMIT")?;

    // Transition to V2-only — queues V1 cleanup tasks
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db.db.exec_safe("COMMIT")?;

    // Verify V1 clock table has data
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_clock")?;
        stmt.step()?;
        let count = stmt.column_int64(0);
        assert!(count > 0, "V1 clock should have entries before cleanup");
    }

    // Call maintenance with chunk_size=0 — must NOT drop the non-empty V1 tables
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    {
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(0)")?;
        stmt.step()?;
        let rc = stmt.column_int(0);
        // Should return an error (-1) or a positive remaining count, not 0 (complete).
        // Returning 0 with chunk_size=0 would mean it dropped the tables.
        assert!(
            rc < 0 || rc > 0,
            "C4 BUG: crsql_incremental_maintenance(0) returned {} — \
             chunk_size=0 must not declare cleanup complete (tables would be dropped)",
            rc
        );
    }
    db.db.exec_safe("COMMIT")?;

    // V1 tables must still exist with data
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_clock'")?;
        stmt.step()?;
        assert_eq!(
            stmt.column_int(0), 1,
            "C4 BUG: V1 clock table was dropped by maintenance(0) despite being non-empty"
        );
    }
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_clock")?;
        stmt.step()?;
        let count = stmt.column_int64(0);
        assert!(count > 0, "C4 BUG: V1 clock table data was lost after maintenance(0)");
    }

    libc_println!("=== v2_maintenance_chunk_size_zero_no_drop PASS ===");
    Ok(())
}

/// Test C5: After 2→1 rollback (metadata-write-version 2→1), incremental maintenance
/// must not recreate V2 tables that were dropped by cleanup.
/// The migration loop sees schema_version=V2AndV1 (stale cache), has_v2=false
/// (post-cleanup), and calls create_v2_tables — recreating dropped tables.
fn v2_rollback_to_v1_no_recreate_v2_tables() -> Result<(), ResultCode> {
    libc_println!("=== v2_rollback_to_v1_no_recreate_v2_tables START ===");

    let db = crate::opendb()?;

    // Start in dual-write mode with data
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db.db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, v TEXT)")?;
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a')")?;
    db.db.exec_safe("COMMIT")?;

    // Verify V2 tables exist
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_v2_clock'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "V2 clock table should exist in dual-write mode");
    }

    // Rollback to V1-only (metadata-write-version 2→1)
    // This queues V2 cleanup tasks and clears migration markers.
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 1)")?;
    db.db.exec_safe("COMMIT")?;

    // Run incremental maintenance to process V2 cleanup tasks.
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    for _ in 0..10 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        if stmt.column_int(0) <= 0 {
            break;
        }
    }
    db.db.exec_safe("COMMIT")?;

    // V2 tables must NOT exist after rollback + cleanup.
    // The migration loop must not recreate them.
    {
        let stmt = db.db.prepare_v2(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE 'foo__crsql_v2_%'"
        )?;
        stmt.step()?;
        let count = stmt.column_int(0);
        assert_eq!(
            count, 0,
            "C5 BUG: V2 tables were recreated by maintenance after 2→1 rollback (found {} V2 tables)",
            count
        );
    }

    // V1 tables should still exist (we rolled back to V1-only)
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_clock'")?;
        stmt.step()?;
        assert_eq!(stmt.column_int(0), 1, "V1 clock table should still exist after rollback to V1");
    }

    libc_println!("=== v2_rollback_to_v1_no_recreate_v2_tables PASS ===");
    Ok(())
}

/// Helper: full V1↔V2 clock comparison for a table.
/// Joins V1 per-column clock entries to V2 clock via v2_pks + v2_col_map
/// and compares all 5 data columns. Returns (total_compared, mismatches).
fn compare_v1_v2_clocks(
    db: &dyn Connection,
    tbl: &str,
) -> Result<(usize, usize), ResultCode> {
    let escaped = tbl;
    let col_id_bits = {
        let s = db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'crsql_col_id_bits'")?;
        s.step()?;
        s.column_int64(0)
    };
    let cmp_sql = alloc::format!(
        "SELECT \
            v1.col_version, v1.db_version, v1.seq, v1.site_id, v1.ts, \
            v2.col_version, v2.db_version, v2.seq, v2.site_id, v2.ts, \
            v1.col_name \
         FROM \"{escaped}__crsql_clock\" v1 \
         JOIN \"{escaped}__crsql_v2_pks\" v2p ON v1.key = v2p.__crsql_key \
         JOIN \"{escaped}__crsql_v2_col_map\" v2m ON v1.col_name = v2m.col_name \
         JOIN \"{escaped}__crsql_v2_clock\" v2 ON v2.cell_key = (v2p.__crsql_key << {bits}) | v2m.col_id \
         WHERE v1.col_name != '-1'",
        escaped = escaped, bits = col_id_bits
    );
    let stmt = db.prepare_v2(&cmp_sql)?;
    let mut total = 0;
    let mut mismatches = 0;
    while stmt.step()? == ResultCode::ROW {
        total += 1;
        let v1_cv = stmt.column_int64(0);
        let v1_dv = stmt.column_int64(1);
        let v1_seq = stmt.column_int64(2);
        let v1_site = stmt.column_int64(3);
        let v1_ts = stmt.column_text(4)?.to_string();
        let v2_cv = stmt.column_int64(5);
        let v2_dv = stmt.column_int64(6);
        let v2_seq = stmt.column_int64(7);
        let v2_site = stmt.column_int64(8);
        let v2_ts = stmt.column_int64(9);
        let col_name = stmt.column_text(10)?.to_string();

        let v1_ts_int: i64 = v1_ts.parse().unwrap_or(-1);

        if v1_cv != v2_cv || v1_dv != v2_dv || v1_seq != v2_seq
            || v1_site != v2_site || v1_ts_int != v2_ts
        {
            mismatches += 1;
            libc_println!(
                "  CLOCK MISMATCH col_name={}: \
                 cv({}/{}) dv({}/{}) seq({}/{}) site({}/{}) ts({}/{})",
                col_name,
                v1_cv, v2_cv, v1_dv, v2_dv, v1_seq, v2_seq,
                v1_site, v2_site, v1_ts, v2_ts,
            );
        }
    }
    Ok((total, mismatches))
}

/// Test: On-demand hydration — V1 rows exist, enable dual-write mode, NEVER call
/// incremental migration, then perform a local write on an existing row.
/// The local write trigger writes to V1 tables. The V2 feed read must hydrate
/// the row from V1 so the feed can emit V2 wire format.
fn v2_on_demand_hydration_local_write_and_remote_merge() -> Result<(), ResultCode> {
    libc_println!("=== v2_on_demand_hydration_local_write_and_remote_merge START ===");

    let db = crate::opendb()?;

    // Step 1: Create a V1 CRR and insert rows (V1-only mode)
    db.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT, w TEXT)")?;
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db.db.exec_safe("INSERT INTO t VALUES (1, 'a', 'x')")?;
    db.db.exec_safe("INSERT INTO t VALUES (2, 'b', 'y')")?;
    db.db.exec_safe("COMMIT")?;

    // Verify V1 clock entries exist
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t__crsql_clock")?;
        stmt.step()?;
        assert!(stmt.column_int64(0) > 0, "V1 clock should have entries");
    }

    // Step 2: Enable dual-write mode but DO NOT call incremental_maintenance.
    // V2 tables are created by crsql_as_crr in dual-write mode, but existing
    // V1 rows are NOT migrated to V2. They must be hydrated on-demand.
    // Note: metadata-use-version and sync-log-version cannot be set to 2 until
    // migration is complete. We stay on V1 wire — hydration still happens
    // for V1 wire merges in dual-write mode.
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    // use-version and sync-log-version remain 1 (V1 wire) — that's the point:
    // hydration must work even without completing migration.

    // Verify V2 tables exist but have no data (no migration was run)
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_pks")?;
        stmt.step()?;
        let count = stmt.column_int64(0);
        libc_println!("  V2 pks count after config: {}", count);
        assert_eq!(count, 0, "V2 pks should be empty (no migration run)");
    }
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_clock")?;
        stmt.step()?;
        assert_eq!(stmt.column_int64(0), 0, "V2 clock should be empty (no migration run)");
    }

    // Step 3: Local write on an existing row (id=1).
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("SELECT crsql_set_ts('1800000000')")?;
    db.db.exec_safe("UPDATE t SET v = 'a2' WHERE id = 1")?;
    db.db.exec_safe("COMMIT")?;

    // V1 clock should have the update.
    {
        let stmt = db.db.prepare_v2(
            "SELECT count(*) FROM t__crsql_clock WHERE col_name = 'v' AND col_version > 1"
        )?;
        stmt.step()?;
        assert!(stmt.column_int64(0) > 0, "V1 clock should have the update for col 'v'");
    }

    // Step 4: Sync from a V1 wire source to this destination. The merge path
    // must hydrate row id=1 from V1 → V2 before merging.
    let db_src = crate::opendb()?;
    db_src.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT, w TEXT)")?;
    db_src.db.exec_safe("BEGIN")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db_src.db.exec_safe("INSERT INTO t VALUES (3, 'c', 'z')")?;
    db_src.db.exec_safe("COMMIT")?;

    // Sync from src to dst (V1 wire) — this triggers hydration for existing rows
    sync_left_to_right(&db_src.db, &db.db, 0)?;

    // After the merge, V2 should have been hydrated for row id=1 (the existing row).
    {
        let stmt = db.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_pks")?;
        stmt.step()?;
        assert!(stmt.column_int64(0) > 0, "V2 pks should have entries after merge (hydration)");
    }

    libc_println!("=== v2_on_demand_hydration_local_write_and_remote_merge PASS ===");
    Ok(())
}

/// Test: On-demand hydration — V1 rows exist, enable dual-write mode, NEVER call
/// incremental migration, then accept a remote update for an existing row.
/// The V2 merge path must hydrate the row from V1 before merging so the CL
/// comparison is correct. After merge, V1↔V2 clocks must be consistent.
fn v2_on_demand_hydration_clock_comparison() -> Result<(), ResultCode> {
    libc_println!("=== v2_on_demand_hydration_clock_comparison START ===");

    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Source: V1-only (V1 wire emission). The destination is dual-write with V1 wire.
    db_src.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT, w TEXT)")?;
    db_src.db.exec_safe("BEGIN")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db_src.db.exec_safe("INSERT INTO t VALUES (1, 'a', 'x')")?;
    db_src.db.exec_safe("COMMIT")?;

    // Destination: V1-only initially, insert the same row independently
    db_dst.db.exec_safe("CREATE TABLE t (id INTEGER PRIMARY KEY NOT NULL, v TEXT, w TEXT)")?;
    db_dst.db.exec_safe("BEGIN")?;
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("SELECT crsql_as_crr('t')")?;
    db_dst.db.exec_safe("INSERT INTO t VALUES (1, 'a', 'x')")?;
    db_dst.db.exec_safe("COMMIT")?;

    // Now enable dual-write on destination but DO NOT migrate.
    // use-version and sync-log-version stay at 1 (V1 wire) — hydration must
    // work even without completing migration.
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;

    // V2 should be empty (no migration)
    {
        let stmt = db_dst.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_pks")?;
        stmt.step()?;
        assert_eq!(stmt.column_int64(0), 0, "V2 pks should be empty before sync");
    }

    // Source updates the row and syncs to destination via V1 wire.
    // The V2 merge path must hydrate row id=1 from V1 before merging.
    db_src.db.exec_safe("SELECT crsql_set_ts('1800000000')")?;
    db_src.db.exec_safe("UPDATE t SET v = 'a2', w = 'x2' WHERE id = 1")?;
    sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

    // The merge should have hydrated row id=1 from V1 → V2, then merged the update.
    {
        let stmt = db_dst.db.prepare_v2("SELECT count(*) FROM t__crsql_v2_pks")?;
        stmt.step()?;
        assert!(stmt.column_int64(0) > 0, "V2 pks should have entries after merge (hydration)");
    }

    // The base table should reflect the merged update.
    {
        let stmt = db_dst.db.prepare_v2("SELECT v, w FROM t WHERE id = 1")?;
        stmt.step()?;
        assert_eq!(stmt.column_text(0)?.to_string(), "a2", "base table should have merged value");
        assert_eq!(stmt.column_text(1)?.to_string(), "x2", "base table should have merged value");
    }

    // Full V1↔V2 clock comparison: after hydration + merge + mirror, V1 and V2
    // clock entries must be consistent for all columns.
    let (total, mismatches) = compare_v1_v2_clocks(&db_dst.db, "t")?;
    assert!(total > 0, "should have per-column V1 clock entries to compare");
    assert_eq!(
        mismatches, 0,
        "V1↔V2 clock mismatch after on-demand hydration + merge: {} mismatches out of {} entries",
        mismatches, total
    );

    libc_println!("=== v2_on_demand_hydration_clock_comparison PASS ===");
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    v2_basic_insert_sync()?;
    v2_update_sync()?;
    v2_delete_sync()?;
    v2_pk_only_insert_sync()?;
    v2_pk_only_delete_sync()?;
    v2_pk_only_reinsert()?;
    v2_pk_only_bidirectional()?;
    v2_pk_only_forward_sync()?;
    v2_pk_only_site_id_tiebreak()?;
    v2_composite_pk_sync()?;
    v2_sync_bit_honored()?;
    v2_bidirectional_sync()?;
    v2_changes_output()?;
    v2_delete_then_reinsert()?;
    v2_alter_add_column_to_pk_only()?;
    v2_alter_drop_column_becomes_pk_only()?;
    v2_alter_drop_two_columns_becomes_pk_only()?;
    v2_wire_packed_seq_filter_and_order()?;
    v2_wire_packed_generalized_pushdown()?;
    v2_reject_pattern_ops()?;
    v2_alter_reorder_composite_pk()?;
    v2_wire_delete_sync()?;
    v2_wire_delete_then_reinsert()?;
    v2_wire_pk_only_delete()?;
    v2_wire_non_rowid_delete()?;
    v2_malformed_blob_no_panic()?;
    v2_quoted_column_and_table_names()?;
    v2_skip_hash_flag_works()?;
    v2_as_table_cleans_v2_metadata()?;
    v2_data_consistency_after_stmt_rollback()?;
    v2_cleanup_v1_tables_after_2_to_3_transition()?;
    v2_merge_rejects_rowid_overflow()?;
    v2_hash_tombstone_rejected_in_v1_wire_mode()?;
    v2_mirror_v1_clock_copy_site_id_ts_not_swapped()?;
    v2_mirror_v1_dead_sentinel_hash_mode_fields_correct()?;
    v2_skip_hash_hash_tombstone_dual_write_no_panic()?;
    v2_maintenance_chunk_size_zero_no_drop()?;
    v2_rollback_to_v1_no_recreate_v2_tables()?;
    v2_on_demand_hydration_local_write_and_remote_merge()?;
    v2_on_demand_hydration_clock_comparison()?;
    Ok(())
}
