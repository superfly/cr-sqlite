extern crate crsql_bundle;
use alloc::string::ToString;
use libc_print::libc_println;
use sqlite::{Connection, Destructor, ResultCode};
use sqlite_nostd as sqlite;

#[cfg(not(target_os = "windows"))]
extern "C" {
    fn unlink(pathname: *const core::ffi::c_char) -> core::ffi::c_int;
    fn system(cmd: *const core::ffi::c_char) -> core::ffi::c_int;
}
#[cfg(target_os = "windows")]
extern "C" {
    fn _unlink(pathname: *const core::ffi::c_char) -> core::ffi::c_int;
    fn system(cmd: *const core::ffi::c_char) -> core::ffi::c_int;
}

/// Sync all changes from `l` to `r` since the given db_version.
fn sync_left_to_right(
    l: &dyn Connection,
    r: &dyn Connection,
    since: sqlite::int64,
) -> Result<(), ResultCode> {
    let stmt_l = l.prepare_v2("SELECT * FROM crsql_changes WHERE db_version >= ?")?;
    stmt_l.bind_int64(1, since)?;

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
                let msg = r
                    .errmsg()
                    .unwrap_or_else(|_| alloc::string::ToString::to_string("unknown"));
                libc_println!("SYNC ERROR: {:?} - {}", e, msg);
                let _ = r.exec_safe("ROLLBACK");
                return Err(e);
            }
        }
    }
    r.exec_safe("COMMIT")?;
    Ok(())
}

fn cleanup_files(paths: &[&str]) {
    for path in paths {
        let p = alloc::format!("{}\0", path);
        unsafe {
            #[cfg(not(target_os = "windows"))]
            unlink(p.as_ptr() as *const core::ffi::c_char);
            #[cfg(target_os = "windows")]
            _unlink(p.as_ptr() as *const core::ffi::c_char);
        }
    }
}

fn copy_file(src: &str, dst: &str) {
    let cmd_cstr = alloc::format!("cp {} {}\0", src, dst);
    unsafe {
        system(cmd_cstr.as_ptr() as *const core::ffi::c_char);
    }
}

/// Create a seed DB with data, migrate to V2, then clear clock entries.
/// This simulates the "snapshot cleanup" workflow:
/// 1. Create tables and insert data (creates clock entries)
/// 2. Migrate to V2 (creates V2 clock tables)
/// 3. Delete clock entries + tombstones, reset CL to 1 in v2_pks (snapshot cleanup)
/// 4. Reset pre_compact_dbversion so changes start fresh
///
/// The base table data and v2_pks (row identity + CL=1) are preserved.
/// Clock entries are regenerated on the first write to each row.
fn create_seed_db(path: &str) -> Result<(), ResultCode> {
    cleanup_files(&[path]);
    let db = crate::opendb_file(path)?;

    // Create tables: regular, composite PK, and PK-only
    db.db.exec_safe("CREATE TABLE items (id INTEGER PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
    db.db.exec_safe("CREATE TABLE connections (src NOT NULL, dst NOT NULL, weight INTEGER, PRIMARY KEY(src, dst))")?;
    db.db.exec_safe("CREATE TABLE pk_only_tab (id INTEGER PRIMARY KEY NOT NULL)")?;

    // Register as CRRs (crsql_set_ts must be called before each crsql_as_crr)
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('items')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('connections')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('pk_only_tab')")?;

    // Insert data
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO items VALUES (1, 'widget', 10)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO items VALUES (2, 'gadget', 20)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO items VALUES (3, 'gizmo', 30)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO connections VALUES ('a', 'b', 5)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO connections VALUES ('b', 'c', 10)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO pk_only_tab VALUES (42)")?;

    // Migrate to V2
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            return Err(ResultCode::ERROR);
        }
        iterations += 1;
    }
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;

    // Verify clock entries exist before nuking
    let stmt = db.db.prepare_v2("SELECT count(*) FROM items__crsql_v2_clock")?;
    stmt.step()?;
    assert!(stmt.column_int(0) > 0, "items should have clock entries before nuke");

    // Nuke clock entries and tombstones (the "snapshot cleanup").
    // Keep v2_pks intact (row identity + CL) but reset CL to 1 for all rows.
    // This ensures proper conflict resolution and efficient local writes
    // (only changed columns get new clock entries, not all columns).
    let _ = db.db.exec_safe("DELETE FROM items__crsql_v2_clock");
    let _ = db.db.exec_safe("DELETE FROM connections__crsql_v2_clock");
    let _ = db.db.exec_safe("DELETE FROM pk_only_tab__crsql_v2_clock");
    // Reset CL to 1 (alive, first version) in v2_pks
    let _ = db.db.exec_safe("UPDATE items__crsql_v2_pks SET cl = 1");
    let _ = db.db.exec_safe("UPDATE connections__crsql_v2_pks SET cl = 1");
    let _ = db.db.exec_safe("UPDATE pk_only_tab__crsql_v2_pks SET cl = 1");
    // Tombstones (may not exist if no deletes happened)
    let _ = db.db.exec_safe("DELETE FROM items__crsql_v2_tombstones");
    let _ = db.db.exec_safe("DELETE FROM connections__crsql_v2_tombstones");
    let _ = db.db.exec_safe("DELETE FROM pk_only_tab__crsql_v2_tombstones");
    // Tombstone PKs (hash mode, may not exist)
    let _ = db.db.exec_safe("DELETE FROM items__crsql_v2_tombstones_pks");
    let _ = db.db.exec_safe("DELETE FROM connections__crsql_v2_tombstones_pks");
    let _ = db.db.exec_safe("DELETE FROM pk_only_tab__crsql_v2_tombstones_pks");
    // V1 tables (exist in dual-write mode, may not exist in V2-only)
    let _ = db.db.exec_safe("DELETE FROM items__crsql_clock");
    let _ = db.db.exec_safe("DELETE FROM items__crsql_pks");
    let _ = db.db.exec_safe("DELETE FROM connections__crsql_clock");
    let _ = db.db.exec_safe("DELETE FROM connections__crsql_pks");
    let _ = db.db.exec_safe("DELETE FROM pk_only_tab__crsql_clock");
    let _ = db.db.exec_safe("DELETE FROM pk_only_tab__crsql_pks");
    // Reset pre_compact_dbversion so changes start from 1
    let _ = db.db.exec_safe("DELETE FROM crsql_master WHERE key = 'pre_compact_dbversion'");

    // Verify clock entries are gone but v2_pks is preserved
    let stmt = db.db.prepare_v2("SELECT count(*) FROM items__crsql_v2_clock")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "items clock should be empty after nuke");
    let stmt = db.db.prepare_v2("SELECT count(*), max(cl) FROM items__crsql_v2_pks")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 3, "items v2_pks should have 3 rows preserved");
    assert_eq!(stmt.column_int64(1), 1, "items v2_pks CL should be reset to 1");

    // Verify base table data is intact
    let stmt = db.db.prepare_v2("SELECT count(*) FROM items")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 3, "items base data should be intact");

    let stmt = db.db.prepare_v2("SELECT count(*) FROM pk_only_tab")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "pk_only_tab base data should be intact");

    Ok(())
}

/// Open a file-based DB and ensure V2 mode is enabled.
fn open_node(path: &str) -> Result<crate::CRConnection, ResultCode> {
    let db = crate::opendb_file(path)?;
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    Ok(db)
}

/// Test: After clearing clock entries, crsql_changes should be empty until a write happens.
/// After a write, only the changed column appears (v2_pks is preserved with CL=1,
/// so the row is already tracked as alive — it's a normal update, not an insert).
fn seeded_no_spurious_changes() -> Result<(), ResultCode> {
    libc_println!("=== seeded_no_spurious_changes START ===");
    let seed_path = "seed_spurious_test.db";
    let node_a_path = "seed_spurious_node_a.db";

    cleanup_files(&[seed_path, node_a_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);

    let db_a = open_node(node_a_path)?;

    // crsql_changes should be empty (no clock entries = no changes)
    let stmt = db_a.db.prepare_v2("SELECT count(*) FROM crsql_changes")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "crsql_changes should be empty after snapshot cleanup");

    // After a write, all non-PK columns appear (first write = like insert)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE items SET qty = 999 WHERE id = 1")?;

    // With v2_pks preserved (CL=1), only the changed column (qty) gets a clock entry.
    // name is unchanged so it doesn't appear in crsql_changes.
    let stmt = db_a.db.prepare_v2("SELECT count(*) FROM crsql_changes")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "should have 1 change (only changed col) for update to seeded row");

    libc_println!("=== seeded_no_spurious_changes PASS ===");
    cleanup_files(&[seed_path, node_a_path]);
    Ok(())
}

/// Test: Updates to existing (seeded) rows propagate to other nodes.
fn seeded_update_propagates() -> Result<(), ResultCode> {
    libc_println!("=== seeded_update_propagates START ===");
    let seed_path = "seed_update_test.db";
    let node_a_path = "seed_update_node_a.db";
    let node_b_path = "seed_update_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // A updates an existing row (id=1, which has no clock entries)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE items SET name = 'updated_widget', qty = 99 WHERE id = 1")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have the updated values
    let stmt = db_b.db.prepare_v2("SELECT name, qty FROM items WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "updated_widget", "B should have updated name");
    assert_eq!(stmt.column_int(1), 99, "B should have updated qty");

    // Other rows should be unchanged
    let stmt = db_b.db.prepare_v2("SELECT name, qty FROM items WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "gadget", "B should still have original name for id=2");
    assert_eq!(stmt.column_int(1), 20, "B should still have original qty for id=2");

    libc_println!("=== seeded_update_propagates PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

/// Test: New inserts propagate and can be forwarded to a third node.
fn seeded_new_insert_propagates() -> Result<(), ResultCode> {
    libc_println!("=== seeded_new_insert_propagates START ===");
    let seed_path = "seed_insert_test.db";
    let node_a_path = "seed_insert_node_a.db";
    let node_b_path = "seed_insert_node_b.db";
    let node_c_path = "seed_insert_node_c.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path, node_c_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);
    copy_file(seed_path, node_c_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;
    let db_c = open_node(node_c_path)?;

    // A inserts a new row
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO items VALUES (4, 'new_item', 40)")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have the new row
    let stmt = db_b.db.prepare_v2("SELECT name, qty FROM items WHERE id = 4")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "new_item", "B should have new item name");
    assert_eq!(stmt.column_int(1), 40, "B should have new item qty");

    // Forward sync B -> C
    sync_left_to_right(&db_b.db, &db_c.db, 0)?;

    let stmt = db_c.db.prepare_v2("SELECT name, qty FROM items WHERE id = 4")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "new_item", "C should have new item via forward sync");
    assert_eq!(stmt.column_int(1), 40, "C should have new item qty via forward sync");

    libc_println!("=== seeded_new_insert_propagates PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path, node_c_path]);
    Ok(())
}

/// Test: Deletes of existing (seeded) rows propagate to other nodes.
fn seeded_delete_propagates() -> Result<(), ResultCode> {
    libc_println!("=== seeded_delete_propagates START ===");
    let seed_path = "seed_delete_test.db";
    let node_a_path = "seed_delete_node_a.db";
    let node_b_path = "seed_delete_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // A deletes an existing row
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM items WHERE id = 2")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have deleted the row
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM items WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 0, "B should have deleted id=2");

    // Other rows should be intact
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM items")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 2, "B should have 2 items remaining");

    libc_println!("=== seeded_delete_propagates PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

/// Test: PK-only table resurrection propagates and can be forwarded.
fn seeded_pk_only_propagates() -> Result<(), ResultCode> {
    libc_println!("=== seeded_pk_only_propagates START ===");
    let seed_path = "seed_pkonly_test.db";
    let node_a_path = "seed_pkonly_node_a.db";
    let node_b_path = "seed_pkonly_node_b.db";
    let node_c_path = "seed_pkonly_node_c.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path, node_c_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);
    copy_file(seed_path, node_c_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;
    let db_c = open_node(node_c_path)?;

    // A deletes and re-inserts the PK-only row (resurrection)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM pk_only_tab WHERE id = 42")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO pk_only_tab VALUES (42)")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should still have the row
    let stmt = db_b.db.prepare_v2("SELECT count(*) FROM pk_only_tab WHERE id = 42")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "B should have pk_only_tab row after resurrection sync");

    // Forward sync B -> C
    sync_left_to_right(&db_b.db, &db_c.db, 0)?;

    let stmt = db_c.db.prepare_v2("SELECT count(*) FROM pk_only_tab WHERE id = 42")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 1, "C should have pk_only_tab row via forward sync");

    libc_println!("=== seeded_pk_only_propagates PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path, node_c_path]);
    Ok(())
}

/// Test: Composite PK table updates propagate.
fn seeded_composite_pk_propagates() -> Result<(), ResultCode> {
    libc_println!("=== seeded_composite_pk_propagates START ===");
    let seed_path = "seed_composite_test.db";
    let node_a_path = "seed_composite_node_a.db";
    let node_b_path = "seed_composite_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // A updates a connection weight
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE connections SET weight = 99 WHERE src = 'a' AND dst = 'b'")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have the updated weight
    let stmt = db_b.db.prepare_v2("SELECT weight FROM connections WHERE src = 'a' AND dst = 'b'")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 99, "B should have updated edge weight");

    // Other edge should be unchanged
    let stmt = db_b.db.prepare_v2("SELECT weight FROM connections WHERE src = 'b' AND dst = 'c'")?;
    stmt.step()?;
    assert_eq!(stmt.column_int(0), 10, "B should have unchanged weight for other edge");

    libc_println!("=== seeded_composite_pk_propagates PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

/// Test: Bidirectional sync — both nodes modify different rows, then sync both ways.
fn seeded_bidirectional_sync() -> Result<(), ResultCode> {
    libc_println!("=== seeded_bidirectional_sync START ===");
    let seed_path = "seed_bidir_test.db";
    let node_a_path = "seed_bidir_node_a.db";
    let node_b_path = "seed_bidir_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // A updates row 1, B updates row 2
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE items SET name = 'from_a' WHERE id = 1")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("UPDATE items SET name = 'from_b' WHERE id = 2")?;

    // Sync both directions
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    // Both should have both updates
    let stmt = db_a.db.prepare_v2("SELECT name FROM items WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "from_a", "A should have its own update for id=1");

    let stmt = db_a.db.prepare_v2("SELECT name FROM items WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "from_b", "A should have B's update for id=2");

    let stmt = db_b.db.prepare_v2("SELECT name FROM items WHERE id = 1")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "from_a", "B should have A's update for id=1");

    let stmt = db_b.db.prepare_v2("SELECT name FROM items WHERE id = 2")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "from_b", "B should have its own update for id=2");

    // Row 3 should be unchanged on both
    let stmt = db_a.db.prepare_v2("SELECT name FROM items WHERE id = 3")?;
    stmt.step()?;
    let a_row3 = stmt.column_text(0)?.to_string();
    let stmt = db_b.db.prepare_v2("SELECT name FROM items WHERE id = 3")?;
    stmt.step()?;
    let b_row3 = stmt.column_text(0)?.to_string();
    assert_eq!(a_row3, b_row3, "Row 3 should match between A and B");
    assert_eq!(a_row3, "gizmo", "Row 3 should be unchanged");

    libc_println!("=== seeded_bidirectional_sync PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

/// Test: Delete then reinsert (resurrection) of a seeded row with new values.
fn seeded_delete_reinsert() -> Result<(), ResultCode> {
    libc_println!("=== seeded_delete_reinsert START ===");
    let seed_path = "seed_delreins_test.db";
    let node_a_path = "seed_delreins_node_a.db";
    let node_b_path = "seed_delreins_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // A deletes and reinserts with different values
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM items WHERE id = 3")?;
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO items VALUES (3, 'resurrected', 333)")?;

    // Sync A -> B
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // B should have the resurrected row with new values
    let stmt = db_b.db.prepare_v2("SELECT name, qty FROM items WHERE id = 3")?;
    stmt.step()?;
    assert_eq!(stmt.column_text(0)?, "resurrected", "B should have resurrected name");
    assert_eq!(stmt.column_int(1), 333, "B should have resurrected qty");

    libc_println!("=== seeded_delete_reinsert PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

/// Test: Two nodes update the SAME seeded row, then sync both ways.
/// With v2_pks preserved (CL=1 on both), this exercises equal-CL conflict
/// resolution — the whole point of keeping v2_pks instead of deleting it.
/// The node with the lower site_id should win on conflicting columns.
fn seeded_conflict_resolution() -> Result<(), ResultCode> {
    libc_println!("=== seeded_conflict_resolution START ===");
    let seed_path = "seed_conflict_test.db";
    let node_a_path = "seed_conflict_node_a.db";
    let node_b_path = "seed_conflict_node_b.db";

    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    create_seed_db(seed_path)?;
    copy_file(seed_path, node_a_path);
    copy_file(seed_path, node_b_path);

    let db_a = open_node(node_a_path)?;
    let db_b = open_node(node_b_path)?;

    // Both nodes update the SAME row's SAME column with different values
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("UPDATE items SET name = 'from_a' WHERE id = 1")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("UPDATE items SET name = 'from_b' WHERE id = 1")?;

    // Sync both directions
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    // Both nodes should converge to the same value for the conflicting column.
    // With equal CL (1) and equal col_version (1), site_id tie-break decides.
    // The exact winner depends on site_id ordering, but both nodes MUST agree.
    let stmt = db_a.db.prepare_v2("SELECT name FROM items WHERE id = 1")?;
    stmt.step()?;
    let a_name = stmt.column_text(0)?.to_string();

    let stmt = db_b.db.prepare_v2("SELECT name FROM items WHERE id = 1")?;
    stmt.step()?;
    let b_name = stmt.column_text(0)?.to_string();

    assert_eq!(a_name, b_name, "both nodes must converge on the same value for conflicting column (A={}, B={})", a_name, b_name);
    assert!(a_name == "from_a" || a_name == "from_b", "winner should be one of the two values, got {}", a_name);

    libc_println!("  converged on name='{}'", a_name);
    libc_println!("=== seeded_conflict_resolution PASS ===");
    cleanup_files(&[seed_path, node_a_path, node_b_path]);
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    seeded_no_spurious_changes()?;
    seeded_update_propagates()?;
    seeded_new_insert_propagates()?;
    seeded_delete_propagates()?;
    seeded_pk_only_propagates()?;
    seeded_composite_pk_propagates()?;
    seeded_bidirectional_sync()?;
    seeded_delete_reinsert()?;
    seeded_conflict_resolution()?;
    Ok(())
}
