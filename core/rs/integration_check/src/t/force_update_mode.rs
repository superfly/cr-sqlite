extern crate alloc;
use alloc::string::String;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

fn test_force_update_mode_basic() -> Result<(), ResultCode> {
    let c = crate::opendb().expect("db opened");
    let db = &c.db;

    db.db.exec_safe("CREATE TABLE foo (a INTEGER PRIMARY KEY NOT NULL, b INTEGER);")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo');")?;

    // Insert a row normally
    db.db.exec_safe("INSERT INTO foo VALUES (1, 100);")?;

    // Check initial CL
    let stmt = db.db.prepare_v2("SELECT cl FROM crsql_changes WHERE cid = 'b'")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let initial_cl = stmt.column_int64(0);
    assert_eq!(initial_cl, 1);
    drop(stmt);

    // Enable force update mode within a transaction
    db.db.exec_safe("BEGIN;")?;
    db.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;

    // Update the row - should force higher CL
    db.db.exec_safe("UPDATE foo SET b = 200 WHERE a = 1;")?;
    db.db.exec_safe("COMMIT;")?;

    // Check that CL increased (should be 3: delete at 2, recreate at 3)
    let stmt = db.db.prepare_v2("SELECT cl FROM crsql_changes WHERE cid = 'b'")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let forced_cl = stmt.column_int64(0);
    assert_eq!(forced_cl, 3);
    drop(stmt);
    Ok(())
}

fn test_missing_triggers_recovery() -> Result<(), ResultCode> {
    let c = crate::opendb().expect("db opened");
    let db = &c.db;

    // Step 1: Create a working crsqlite system
    db.db.exec_safe("CREATE TABLE products (id INTEGER PRIMARY KEY NOT NULL, name TEXT, price INTEGER);")?;
    db.db.exec_safe("SELECT crsql_as_crr('products');")?;

    // Insert some initial data
    db.db.exec_safe("INSERT INTO products VALUES (1, 'Widget', 100);")?;
    db.db.exec_safe("INSERT INTO products VALUES (2, 'Gadget', 200);")?;

    // Verify initial state
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM crsql_changes;")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let initial_changes = stmt.column_int(0);
    assert!(initial_changes > 0);
    drop(stmt);

    // Step 2: Simulate triggers disappearing (drop all triggers)
    db.db.exec_safe("DROP TRIGGER products__crsql_itrig;")?;
    db.db.exec_safe("DROP TRIGGER products__crsql_utrig;")?;
    db.db.exec_safe("DROP TRIGGER products__crsql_dtrig;")?;

    // Step 3: Make updates/inserts without triggers (these won't be tracked!)
    db.db.exec_safe("UPDATE products SET price = 150 WHERE id = 1;")?;
    db.db.exec_safe("UPDATE products SET name = 'Super Widget' WHERE id = 1;")?;
    db.db.exec_safe("INSERT INTO products VALUES (3, 'Doohickey', 300);")?;
    db.db.exec_safe("DELETE FROM products WHERE id = 2;")?;

    // Verify that changes were NOT tracked
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM crsql_changes;")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let untracked_changes = stmt.column_int(0);
    assert_eq!(untracked_changes, initial_changes);
    drop(stmt);

    // Verify actual data state
    let stmt = db.db.prepare_v2("SELECT id, name, price FROM products ORDER BY id;")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 1);
    assert_eq!(stmt.column_text(1)?, "Super Widget");
    assert_eq!(stmt.column_int(2), 150);
    assert_eq!(stmt.step()?, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 3);
    assert_eq!(stmt.column_text(1)?, "Doohickey");
    assert_eq!(stmt.step()?, ResultCode::DONE);
    drop(stmt);

    // Step 4: Restore triggers manually using crsql_as_crr
    db.db.exec_safe("SELECT crsql_as_crr('products');")?;

    // Step 5: Enable force update mode within a transaction
    db.db.exec_safe("BEGIN;")?;
    db.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;

    // Step 6: Update existing rows to force them to be tracked with higher CL
    db.db.exec_safe("UPDATE products SET price = price WHERE id = 1;")?;
    db.db.exec_safe("UPDATE products SET price = price WHERE id = 3;")?;
    db.db.exec_safe("COMMIT;")?;

    // Step 7: Verify crsql_changes now has the forced updates
    // Query CL for product 1 (price column)
    let stmt = db.db.prepare_v2(
        "SELECT cl FROM crsql_changes WHERE pk = crsql_pack_columns(1) AND cid = 'price';"
    )?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let cl_product_1 = stmt.column_int64(0);
    assert!(cl_product_1 >= 3, "Product 1 CL should be >= 3 after force update");
    drop(stmt);

    // Query CL for product 3 (price column)
    let stmt = db.db.prepare_v2(
        "SELECT cl FROM crsql_changes WHERE pk = crsql_pack_columns(3) AND cid = 'price';"
    )?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let cl_product_3 = stmt.column_int64(0);
    assert!(cl_product_3 >= 3, "Product 3 CL should be >= 3 after force update");
    drop(stmt);
    Ok(())
}

fn test_force_update_wins_in_sync() -> Result<(), ResultCode> {
    let c1 = crate::opendb().expect("db1 opened");
    let c2 = crate::opendb().expect("db2 opened");
    let db1 = &c1.db;
    let db2 = &c2.db;

    // Setup both databases
    db1.db.exec_safe("CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, value INTEGER);")?;
    db1.db.exec_safe("SELECT crsql_as_crr('items');")?;

    db2.db.exec_safe("CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, value INTEGER);")?;
    db2.db.exec_safe("SELECT crsql_as_crr('items');")?;

    // db1: Insert and update multiple times
    db1.db.exec_safe("INSERT INTO items VALUES (1, 100);")?;
    db1.db.exec_safe("UPDATE items SET value = 200 WHERE id = 1;")?;
    db1.db.exec_safe("UPDATE items SET value = 300 WHERE id = 1;")?;

    // Get db1's CL
    let stmt = db1.db.prepare_v2("SELECT cl FROM crsql_changes WHERE cid = 'value'")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let db1_cl = stmt.column_int64(0);
    drop(stmt);

    // db2: Insert with different value
    db2.db.exec_safe("INSERT INTO items VALUES (1, 999);")?;

    // db2: Use force update mode to override
    db2.db.exec_safe("BEGIN;")?;
    db2.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;
    db2.db.exec_safe("UPDATE items SET value = 777 WHERE id = 1;")?;
    db2.db.exec_safe("COMMIT;")?;

    // Get db2's forced CL
    let stmt = db2.db.prepare_v2("SELECT cl FROM crsql_changes WHERE cid = 'value'")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let db2_forced_cl = stmt.column_int64(0);
    drop(stmt);

    // Verify db2's CL is higher than db1's
    assert!(db2_forced_cl > db1_cl);

    // Sync db2 to db1 - db2's forced update should win
    let stmt_read = db2.db.prepare_v2("SELECT * FROM crsql_changes")?;
    let stmt_write = db1.db.prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")?;

    while stmt_read.step()? == ResultCode::ROW {
        for i in 0..9 {
            stmt_write.bind_value(i + 1, stmt_read.column_value(i)?)?;
        }
        assert_eq!(stmt_write.step()?, ResultCode::DONE);
        stmt_write.reset()?;
    }
    drop(stmt_read);
    drop(stmt_write);

    // Verify db1 now has db2's value
    let stmt = db1.db.prepare_v2("SELECT value FROM items WHERE id = 1")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let db1_value = stmt.column_int(0);
    assert_eq!(db1_value, 777);
    drop(stmt);

    Ok(())
}

fn test_force_update_with_missing_clock_entries() -> Result<(), ResultCode> {
    let c = crate::opendb().expect("db opened");
    let db = &c.db;

    // Create table and make it a CRR
    db.db.exec_safe("CREATE TABLE data (id INTEGER PRIMARY KEY NOT NULL, a INTEGER, b INTEGER, c INTEGER);")?;
    db.db.exec_safe("SELECT crsql_as_crr('data');")?;

    // Insert a row normally
    db.db.exec_safe("INSERT INTO data VALUES (1, 10, 20, 30);")?;

    // Verify initial crsql_changes has entries for a, b, c
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM crsql_changes WHERE cid IN ('a', 'b', 'c');")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let initial_count = stmt.column_int(0);
    assert_eq!(initial_count, 3);
    drop(stmt);

    // Simulate corruption: delete some clock entries for columns b and c
    db.db.exec_safe("DELETE FROM data__crsql_clock WHERE col_name = 'b';")?;
    db.db.exec_safe("DELETE FROM data__crsql_clock WHERE col_name = 'c';")?;

    // Verify clock entries are missing (only 'a' and sentinel '-1' should remain)
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM data__crsql_clock WHERE col_name IN ('b', 'c');")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 0);
    drop(stmt);

    // crsql_changes should now only have 'a' (b and c clock entries are gone)
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM crsql_changes WHERE cid IN ('a', 'b', 'c');")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let missing_count = stmt.column_int(0);
    assert_eq!(missing_count, 1); // Only 'a' remains
    drop(stmt);

    // Enable force update mode and update the row
    db.db.exec_safe("BEGIN;")?;
    db.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;
    db.db.exec_safe("UPDATE data SET a = a, b = b, c = c WHERE id = 1;")?;
    db.db.exec_safe("COMMIT;")?;

    // Verify crsql_changes now has all columns again
    let stmt = db.db.prepare_v2("SELECT COUNT(*) FROM crsql_changes WHERE cid IN ('a', 'b', 'c');")?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let restored_count = stmt.column_int(0);
    assert_eq!(restored_count, 3); // All three columns should be back
    drop(stmt);

    // Verify the CL is >= 3 (force update does delete+recreate)
    let stmt = db.db.prepare_v2(
        "SELECT cl FROM crsql_changes WHERE pk = crsql_pack_columns(1) AND cid = 'a';"
    )?;
    assert_eq!(stmt.step()?, ResultCode::ROW);
    let cl = stmt.column_int64(0);
    assert!(cl >= 3);
    drop(stmt);

    Ok(())
}

fn test_force_update_requires_transaction() -> Result<(), ResultCode> {
    let c = crate::opendb().expect("db opened");
    let db = &c.db;

    db.db.exec_safe("CREATE TABLE test (id INTEGER PRIMARY KEY NOT NULL, value INTEGER);")?;
    db.db.exec_safe("SELECT crsql_as_crr('test');")?;

    // Try to enable force update mode outside a transaction - should fail
    let result = db.db.exec_safe("SELECT crsql_enable_force_update_mode();");
    assert!(result.is_err(), "Should fail when enabling force update mode outside transaction");

    // Now try within a transaction - should succeed
    db.db.exec_safe("BEGIN;")?;
    db.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;

    // Verify mode is enabled
    db.db.exec_safe("INSERT INTO test VALUES (1, 100);")?;
    db.db.exec_safe("COMMIT;")?;

    // After commit, mode should be auto-disabled
    let result = db.db.exec_safe("SELECT crsql_enable_force_update_mode();");
    assert!(result.is_err(), "Should fail after transaction commit");

    // Test rollback also disables it
    db.db.exec_safe("BEGIN;")?;
    db.db.exec_safe("SELECT crsql_enable_force_update_mode();")?;
    db.db.exec_safe("ROLLBACK;")?;

    let result = db.db.exec_safe("SELECT crsql_enable_force_update_mode();");
    assert!(result.is_err(), "Should fail after transaction rollback");

    Ok(())
}

pub fn run_suite() -> Result<(), String> {
    test_force_update_requires_transaction().map_err(|e| alloc::format!("test_force_update_requires_transaction failed: {:?}", e))?;
    test_force_update_mode_basic().map_err(|e| alloc::format!("test_force_update_mode_basic failed: {:?}", e))?;
    test_missing_triggers_recovery().map_err(|e| alloc::format!("test_missing_triggers_recovery failed: {:?}", e))?;
    test_force_update_wins_in_sync().map_err(|e| alloc::format!("test_force_update_wins_in_sync failed: {:?}", e))?;
    test_force_update_with_missing_clock_entries().map_err(|e| alloc::format!("test_force_update_with_missing_clock_entries failed: {:?}", e))?;
    Ok(())
}
