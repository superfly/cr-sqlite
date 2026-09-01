extern crate crsql_bundle;
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
    v2_wire_delete_sync()?;
    v2_wire_delete_then_reinsert()?;
    v2_wire_pk_only_delete()?;
    v2_wire_non_rowid_delete()?;
    Ok(())
}
