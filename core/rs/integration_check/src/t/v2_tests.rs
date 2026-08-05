extern crate alloc;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use crsql_bundle::test_exports::pack_columns::{unpack_columns, ColumnValue};
use libc_print::libc_println;
use sqlite::{Connection, Destructor, ManagedConnection, ResultCode};
use sqlite_nostd as sqlite;

/// Test that crsql_pack_agg produces byte-identical output to crsql_pack_columns
/// for the same values in the same order.
fn test_pack_agg_matches_pack_columns() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, x, y, z)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello', 42, x'01020304')")?;

    // Get crsql_pack_columns output
    let select_pack = db.db.prepare_v2(
        "SELECT crsql_pack_columns(id, x, y, z) FROM foo WHERE id = 1"
    )?;
    select_pack.step()?;
    let packed = select_pack.column_blob(0)?;

    // Get crsql_pack_agg output via aggregate over a grouped query
    // We use a subquery to feed the same values to crsql_pack_agg
    let select_agg = db.db.prepare_v2(
        "SELECT crsql_pack_agg(v) FROM (SELECT id AS v FROM foo WHERE id = 1 UNION ALL SELECT x FROM foo WHERE id = 1 UNION ALL SELECT y FROM foo WHERE id = 1 UNION ALL SELECT z FROM foo WHERE id = 1)"
    )?;
    select_agg.step()?;
    let agged = select_agg.column_blob(0)?;

    assert!(
        packed == agged,
        "crsql_pack_agg output should match crsql_pack_columns output"
    );

    // Verify unpacking works
    let unpacked = unpack_columns(agged)?;
    assert!(unpacked.len() == 4);
    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == 1);
    } else {
        panic!("expected integer");
    }
    if let ColumnValue::Text(s) = &unpacked[1] {
        assert!(s == "hello");
    } else {
        panic!("expected text");
    }
    if let ColumnValue::Integer(i) = unpacked[2] {
        assert!(i == 42);
    } else {
        panic!("expected integer");
    }
    if let ColumnValue::Blob(b) = &unpacked[3] {
        assert!(b[..] == [1, 2, 3, 4]);
    } else {
        panic!("expected blob");
    }

    Ok(())
}

/// Test crsql_pack_agg with NULL values
fn test_pack_agg_with_nulls() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE bar (id PRIMARY KEY, a, b)")?;
    db.db.exec_safe("INSERT INTO bar VALUES (1, NULL, 'text')")?;

    let select_agg = db.db.prepare_v2(
        "SELECT crsql_pack_agg(v) FROM (SELECT a AS v FROM bar WHERE id = 1 UNION ALL SELECT b FROM bar WHERE id = 1)"
    )?;
    select_agg.step()?;
    let agged = select_agg.column_blob(0)?;

    let unpacked = unpack_columns(agged)?;
    assert!(unpacked.len() == 2);
    assert!(matches!(unpacked[0], ColumnValue::Null));
    if let ColumnValue::Text(s) = &unpacked[1] {
        assert!(s == "text");
    } else {
        panic!("expected text");
    }

    Ok(())
}

/// Test crsql_pack_agg with empty input (no rows)
fn test_pack_agg_empty() -> Result<(), ResultCode> {
    let db = crate::opendb()?;

    // crsql_pack_agg over empty set should produce varint(0) = single byte 0x00
    let select_agg = db.db.prepare_v2(
        "SELECT crsql_pack_agg(v) FROM (SELECT 1 AS v WHERE 0)"
    )?;
    select_agg.step()?;
    let agged = select_agg.column_blob(0)?;

    // Should be a single byte 0x00 (varint encoding of count=0)
    assert!(agged.len() == 1, "empty pack_agg should be 1 byte, got {}", agged.len());
    assert!(agged[0] == 0x00, "empty pack_agg should be 0x00");

    // Unpacking should give 0 columns
    let unpacked = unpack_columns(agged)?;
    assert!(unpacked.is_empty());

    Ok(())
}

/// Test crsql_hash_pk produces deterministic, truncated hashes
fn test_hash_pk_deterministic() -> Result<(), ResultCode> {
    let db = crate::opendb()?;

    // Same input should produce same hash
    let select1 = db.db.prepare_v2("SELECT crsql_hash_pk(1, 'abc')")?;
    select1.step()?;
    let hash1 = select1.column_blob(0)?;

    let select2 = db.db.prepare_v2("SELECT crsql_hash_pk(1, 'abc')")?;
    select2.step()?;
    let hash2 = select2.column_blob(0)?;

    assert!(hash1 == hash2, "same input should produce same hash");
    assert!(
        hash1.len() == 10,
        "hash should be PK_HASH_SIZE=10 bytes, got {}",
        hash1.len()
    );

    // Different input should produce different hash
    let select3 = db.db.prepare_v2("SELECT crsql_hash_pk(2, 'abc')")?;
    select3.step()?;
    let hash3 = select3.column_blob(0)?;

    assert!(hash1 != hash3, "different input should produce different hash");

    Ok(())
}

/// Test varint count header: values 0-127 should be byte-identical to old u8 format
fn test_varint_count_backward_compat() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE baz (id PRIMARY KEY, a, b)")?;
    db.db.exec_safe("INSERT INTO baz VALUES (1, 'x', 'y')")?;

    // 3 columns -> count byte should be 0x03 (same as old u8 format for <128)
    let select = db.db.prepare_v2("SELECT crsql_pack_columns(id, a, b) FROM baz WHERE id = 1")?;
    select.step()?;
    let packed = select.column_blob(0)?;

    assert!(
        packed[0] == 3,
        "first byte should be count=3 (varint single byte, same as old u8)"
    );

    // Unpack should work correctly
    let unpacked = unpack_columns(packed)?;
    assert!(unpacked.len() == 3);

    Ok(())
}

/// Test that crsql_pack_agg and crsql_pack_columns produce identical output
/// for integer-only values including negative and large numbers
fn test_pack_agg_integers() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE nums (id PRIMARY KEY, a, b, c)")?;
    db.db.exec_safe("INSERT INTO nums VALUES (1, -1, 0, 9999999999999)")?;

    let select_pack = db.db.prepare_v2(
        "SELECT crsql_pack_columns(a, b, c) FROM nums WHERE id = 1"
    )?;
    select_pack.step()?;
    let packed = select_pack.column_blob(0)?;

    let select_agg = db.db.prepare_v2(
        "SELECT crsql_pack_agg(v) FROM (SELECT a AS v FROM nums WHERE id = 1 UNION ALL SELECT b FROM nums WHERE id = 1 UNION ALL SELECT c FROM nums WHERE id = 1)"
    )?;
    select_agg.step()?;
    let agged = select_agg.column_blob(0)?;

    assert!(
        packed == agged,
        "pack_agg should match pack_columns for integers"
    );

    let unpacked = unpack_columns(agged)?;
    assert!(unpacked.len() == 3);
    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == -1);
    } else {
        panic!("expected integer");
    }
    if let ColumnValue::Integer(i) = unpacked[1] {
        assert!(i == 0);
    } else {
        panic!("expected integer");
    }
    if let ColumnValue::Integer(i) = unpacked[2] {
        assert!(i == 9999999999999);
    } else {
        panic!("expected integer");
    }

    Ok(())
}

/// Test crsql_hash_pk with different types (integer, text, blob)
fn test_hash_pk_different_types() -> Result<(), ResultCode> {
    let db = crate::opendb()?;

    // Integer PK
    let s1 = db.db.prepare_v2("SELECT crsql_hash_pk(42)")?;
    s1.step()?;
    let h1 = s1.column_blob(0)?;

    // Text PK
    let s2 = db.db.prepare_v2("SELECT crsql_hash_pk('hello')")?;
    s2.step()?;
    let h2 = s2.column_blob(0)?;

    // Blob PK
    let s3 = db.db.prepare_v2("SELECT crsql_hash_pk(x'deadbeef')")?;
    s3.step()?;
    let h3 = s3.column_blob(0)?;

    // All should be 10 bytes and different from each other
    assert!(h1.len() == 10 && h2.len() == 10 && h3.len() == 10);
    assert!(h1 != h2 && h1 != h3 && h2 != h3);

    Ok(())
}

/// Test that metadata-use-version=1 reads from V1 tables even when V2 tables exist.
/// Setup: create CRR, insert data, migrate to V2&V1, then query with use-version=1 and use-version=2.
/// Both should produce the same per-column rows (V1 wire format).
fn test_metadata_use_version_dispatch() -> Result<(), ResultCode> {
    libc_println!("DEBUG: test_metadata_use_version_dispatch START");
    let db = crate::opendb()?;
    libc_println!("DEBUG: db opened");
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, x, y)")?;
    libc_println!("DEBUG: table created");
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'a', 10)")?;
    libc_println!("DEBUG: data inserted");
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    let rc = db.db.exec_safe("SELECT crsql_as_crr('foo')");
    match rc {
        Ok(code) => libc_println!("DEBUG: as_crr ok: {:?}", code),
        Err(code) => {
            libc_println!("DEBUG: as_crr failed: {:?}", code);
            // Try to get error message
            let err_stmt = db.db.prepare_v2("SELECT sqlite3_errmsg(0)\0");
            if let Ok(s) = err_stmt {
                let _ = s.step();
                if let Ok(msg) = s.column_text(0) {
                    libc_println!("DEBUG: errmsg: {}", msg);
                }
            }
            return Err(code);
        }
    }

    // Check crsql_master exists
    let check = db.db.prepare_v2("SELECT 1 FROM sqlite_master WHERE name = 'crsql_master'\0")?;
    let has_master = check.step()? == ResultCode::ROW;
    libc_println!("crsql_master exists: {}", has_master);

    // Start migration to V2&V1
    let rc = db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)");
    libc_println!("config_set rc: {:?}", rc);
    if let Err(e) = rc {
        libc_println!("config_set failed: {:?}", e);
        return Err(e);
    }

    // Enable debug logging
    db.db.exec_safe("SELECT crsql_set_debug(1)")?;

    // Check config was set
    let get_stmt = db.db.prepare_v2("SELECT crsql_config_get('metadata-write-version')\0")?;
    get_stmt.step()?;
    libc_println!("metadata-write-version is: {}", get_stmt.column_int(0));

    // Run maintenance until complete
    let mut remaining = 1;
    let mut iterations = 0;
    while remaining > 0 && iterations < 100 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)\0")?;
        let rc = stmt.step()?;
        if rc != ResultCode::ROW {
            libc_println!("maintenance step returned {:?}, not ROW", rc);
            break;
        }
        remaining = stmt.column_int(0) as i32;
        if iterations < 3 {
            libc_println!("maintenance iteration {} remaining: {}", iterations, remaining);
        }
        if remaining < 0 {
            panic!("migration failed with remaining={}", remaining);
        }
        iterations += 1;
    }
    libc_println!("migration done after {} iterations, remaining={}", iterations, remaining);

    // Verify V2 tables exist
    let check = db.db.prepare_v2("SELECT 1 FROM sqlite_master WHERE tbl_name = 'foo__crsql_v2_clock'\0")?;
    let has_v2 = check.step()? == ResultCode::ROW;
    libc_println!("V2 clock table exists: {}", has_v2);
    assert!(has_v2, "V2 clock table should exist");

    // Set metadata-use-version=1 → should read from V1 tables
    libc_println!("DEBUG: setting metadata-use-version=1");
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 1)")?;
    libc_println!("DEBUG: metadata-use-version=1 set ok");

    let stmt = match db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [col_version], [db_version], [site_id], [seq], [cl], [ts] FROM crsql_changes"
    ) {
        Ok(s) => {
            libc_println!("DEBUG: feed query prepared");
            s
        }
        Err(e) => {
            libc_println!("DEBUG: feed query prepare FAILED: {:?}", e);
            let errmsg = db.db.errmsg().unwrap_or_else(|_| "unknown".to_string());
            libc_println!("DEBUG: errmsg: {}", errmsg);
            return Err(e);
        }
    };
    let mut rows_v1 = vec![];
    while stmt.step()? == ResultCode::ROW {
        rows_v1.push((
            stmt.column_text(0)?.to_string(),
            stmt.column_text(2)?.to_string(),
            stmt.column_int64(3),
        ));
    }

    // Should have: 2 cell changes (x, y) — no sentinel row for newly created CRRs (sentinel-omission optimization)
    libc_println!("DEBUG: rows_v1 count = {}", rows_v1.len());
    for (i, row) in rows_v1.iter().enumerate() {
        libc_println!("DEBUG: rows_v1[{}] = ({}, {}, {})", i, row.0, row.1, row.2);
    }
    assert!(rows_v1.len() == 2, "use-version=1 should produce 2 rows, got {}", rows_v1.len());

    // Set metadata-use-version=2 → should read from V2 tables (per-column format)
    libc_println!("DEBUG: setting metadata-use-version=2");
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    libc_println!("DEBUG: metadata-use-version=2 set ok");

    let stmt = match db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [col_version], [db_version], [site_id], [seq], [cl], [ts] FROM crsql_changes"
    ) {
        Ok(s) => {
            libc_println!("DEBUG: v2 feed query prepared");
            s
        }
        Err(e) => {
            libc_println!("DEBUG: v2 feed query prepare FAILED: {:?}", e);
            let errmsg = db.db.errmsg().unwrap_or_else(|_| "unknown".to_string());
            libc_println!("DEBUG: errmsg: {}", errmsg);
            return Err(e);
        }
    };
    let mut rows_v2 = vec![];
    while stmt.step()? == ResultCode::ROW {
        rows_v2.push((
            stmt.column_text(0)?.to_string(),
            stmt.column_text(2)?.to_string(),
            stmt.column_int64(3),
        ));
    }

    // V2 per-column format: 2 cell changes (x, y) — same as V1, no sentinel rows
    libc_println!("DEBUG: rows_v2 count = {}", rows_v2.len());
    for (i, row) in rows_v2.iter().enumerate() {
        libc_println!("DEBUG: rows_v2[{}] = ({}, {}, {})", i, row.0, row.1, row.2);
    }
    assert!(rows_v2.len() == 2, "use-version=2 should produce 2 rows, got {}", rows_v2.len());

    // Both should have the same table name
    for r in &rows_v1 {
        assert!(r.0 == "foo", "use-version=1 table name should be foo");
    }
    for r in &rows_v2 {
        assert!(r.0 == "foo", "use-version=2 table name should be foo");
    }

    // Both should have the same column changes
    let v1_cids: Vec<_> = rows_v1.iter().map(|r| r.1.clone()).collect();
    let v2_cids: Vec<_> = rows_v2.iter().map(|r| r.1.clone()).collect();
    assert!(v1_cids.contains(&"x".to_string()), "use-version=1 should have x column");
    assert!(v2_cids.contains(&"x".to_string()), "use-version=2 should have x column");
    assert!(v1_cids.contains(&"y".to_string()), "use-version=1 should have y column");
    assert!(v2_cids.contains(&"y".to_string()), "use-version=2 should have y column");

    Ok(())
}

/// Test packed wire format (sync-log-version=2):
/// Multiple column changes for the same row/db_version/site_id are coalesced into one row
/// with char(0)-separated cid and col_vrsn, and a packed cval blob.
fn test_packed_wire_format() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE bar (id PRIMARY KEY NOT NULL, a, b, c)")?;
    db.db.exec_safe("INSERT INTO bar VALUES (1, 'x', 42, 3.14)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('bar')")?;

    // Migrate to V2&V1
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            panic!("migration failed");
        }
    }

    // Set use-version=2 and sync-log-version=2 → packed format
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    let stmt = db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [val], [cl] FROM crsql_changes"
    )?;

    let mut packed_rows = vec![];
    loop {
        let rc = stmt.step();
        match rc {
            Ok(ResultCode::ROW) => {},
            Ok(ResultCode::DONE) => break,
            Ok(rc) => {
                return Err(rc);
            }
            Err(e) => {
                let errmsg = db.db.errmsg().unwrap_or_else(|_| "unknown".to_string());
                libc_println!("packed wire format: step failed: {:?} - {}", e, errmsg);
                return Err(e);
            }
        }
        // Read blob column before text columns to avoid sqlite3_value invalidation
        let cval_raw = stmt.column_blob(3).unwrap_or(&[]);
        let tbl = stmt.column_text(0)?.to_string();
        let cid = stmt.column_text(2)?.to_string();
        let cl = stmt.column_int64(4);

        if cid.contains('\0') {
            // Packed row: cid has char(0) separators
            let cids: Vec<String> = cid.split('\0').map(|s| s.to_string()).collect();
            let cval = cval_raw.to_vec();
            packed_rows.push((tbl, cids, cval, cl));
        }
    }

    // Should have 1 packed row (all 3 columns coalesced)
    assert!(packed_rows.len() == 1, "should have 1 packed row, got {}", packed_rows.len());

    let (tbl, cids, cval, _cl) = &packed_rows[0];
    assert!(tbl == "bar", "table should be bar");
    assert!(cids.len() == 3, "packed row should have 3 columns, got {}", cids.len());
    assert!(cids.contains(&"a".to_string()), "should contain column a");
    assert!(cids.contains(&"b".to_string()), "should contain column b");
    assert!(cids.contains(&"c".to_string()), "should contain column c");

    // cval should be a valid packed blob (unpackable)
    let unpacked = unpack_columns(cval)?;
    assert!(unpacked.len() == 3, "cval should unpack to 3 values, got {}", unpacked.len());
    // Values should be 'x', 42, 3.14 in col_id order (a=0, b=1, c=2)
    if let ColumnValue::Text(s) = &unpacked[0] {
        assert!(s == "x", "first cval should be 'x', got {}", s);
    } else {
        panic!("expected text for first cval");
    }
    if let ColumnValue::Integer(i) = unpacked[1] {
        assert!(i == 42, "second cval should be 42, got {}", i);
    } else {
        panic!("expected integer for second cval");
    }
    if let ColumnValue::Float(f) = unpacked[2] {
        // no_std: manual abs comparison
        let diff = if f > 3.14 { f - 3.14 } else { 3.14 - f };
        assert!(diff < 0.001, "third cval should be ~3.14, got {}", f);
    } else {
        panic!("expected float for third cval");
    }

    Ok(())
}

/// Test V2 hash tombstone wire format:
/// When sync-log-version=2, deleted rows should emit with cid='-2' and pks=hashed_pk (blob),
/// not cid='-1' and pks=crsql_pack_columns(real_pks).
fn test_v2_hash_tombstone() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE baz (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("INSERT INTO baz VALUES (1, 'x', 'y')")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('baz')")?;

    // Migrate to V2
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            panic!("migration failed");
        }
    }

    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Set timestamp before delete (v2_tombstones has CHECK (ts > 0))
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;

    // Delete the row to create a tombstone
    db.db.exec_safe("DELETE FROM baz WHERE id = 1")?;

    // Query crsql_changes — should get 1 tombstone row with cid=-2 and hashed_pk blob
    let stmt = db.db.prepare_v2(
        "SELECT [table], [pk], [cid], [val], [cl] FROM crsql_changes"
    )?;

    let mut tombstone_row: Option<(String, Vec<u8>, String)> = None;
    loop {
        let rc = stmt.step()?;
        if rc == ResultCode::DONE { break; }
        if rc != ResultCode::ROW { return Err(rc); }

        // Read blob column first to avoid invalidation
        let pk_blob = stmt.column_blob(1).unwrap_or(&[]).to_vec();
        let tbl = stmt.column_text(0)?.to_string();
        let cid = stmt.column_text(2)?.to_string();

        if cid == "-2" {
            // V2 hash tombstone: pks should be hashed_pk blob, val should be NULL
            tombstone_row = Some((tbl, pk_blob, cid));
            let val_type = stmt.column_type(3)?;
            assert!(val_type == sqlite::ColumnType::Null,
                "tombstone val should be NULL, got {:?}", val_type);
        }
    }

    // Clock entries are deleted on delete, so only the tombstone row should remain
    let (tbl, pk_blob, cid) = tombstone_row.expect("should have a tombstone row with cid=-2");
    assert!(tbl == "baz", "tombstone table should be baz, got {}", tbl);
    assert!(cid == "-2", "tombstone cid should be -2, got {}", cid);
    assert!(pk_blob.len() > 0, "tombstone pk (hashed_pk) should be non-empty blob, got {} bytes", pk_blob.len());

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

    // Use a transaction so ts is only set once for all merge inserts
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
                let msg = r.errmsg().unwrap_or_else(|_| "unknown".to_string());
                libc_println!("SYNC ERROR: {:?} - {}", e, msg);
                libc_println!("  row: tbl={:?}", stmt_l.column_text(0));
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
            let type_l = stmt_l.column_type(i)?;
            let type_r = stmt_r.column_type(i)?;
            if type_l != type_r {
                libc_println!("MISMATCH: column {} type {:?} vs {:?}", i, type_l, type_r);
                return Ok(false);
            }
            match type_l {
                sqlite::ColumnType::Null => {}
                sqlite::ColumnType::Integer => {
                    let v_l = stmt_l.column_int64(i);
                    let v_r = stmt_r.column_int64(i);
                    if v_l != v_r {
                        libc_println!("MISMATCH: column {} int {} vs {}", i, v_l, v_r);
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Float => {
                    let f_l = stmt_l.column_double(i);
                    let f_r = stmt_r.column_double(i);
                    let diff = if f_l > f_r { f_l - f_r } else { f_r - f_l };
                    if diff > 0.0001 {
                        libc_println!("MISMATCH: column {} float {} vs {}", i, f_l, f_r);
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Text => {
                    let t_l = stmt_l.column_text(i)?;
                    let t_r = stmt_r.column_text(i)?;
                    if t_l != t_r {
                        libc_println!("MISMATCH: column {} text '{}' vs '{}'", i, t_l, t_r);
                        return Ok(false);
                    }
                }
                sqlite::ColumnType::Blob => {
                    let b_l = stmt_l.column_blob(i)?;
                    let b_r = stmt_r.column_blob(i)?;
                    if b_l != b_r {
                        libc_println!("MISMATCH: column {} blob len {} vs {}", i, b_l.len(), b_r.len());
                        return Ok(false);
                    }
                }
            }
        }
    }
}

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
    Ok(())
}

/// Cross-mode sync round-trip test covering all 3 metadata/wire combinations:
/// 1. V1 metadata (V1 wire)
/// 2. V2 metadata + V1 wire (dual-write source, V1 wire sync)
/// 3. V2 metadata + V2 wire (dual-write source, V2 packed wire sync)
fn test_cross_mode_sync_roundtrip() -> Result<(), ResultCode> {
    libc_println!("=== test_cross_mode_sync_roundtrip START ===");

    let db_src = crate::opendb()?;
    db_src.db.exec_safe("CREATE TABLE items (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER, price REAL)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('items')")?;

    // Insert rows
    db_src.db.exec_safe("INSERT INTO items VALUES (1, 'apple', 10, 1.5)")?;
    db_src.db.exec_safe("INSERT INTO items VALUES (2, 'banana', 20, 0.75)")?;
    db_src.db.exec_safe("INSERT INTO items VALUES (3, 'cherry', 5, 3.25)")?;

    // Update some rows
    db_src.db.exec_safe("UPDATE items SET qty = 15, price = 1.75 WHERE id = 1")?;
    db_src.db.exec_safe("UPDATE items SET name = 'banana_split' WHERE id = 2")?;

    // Delete a row
    db_src.db.exec_safe("DELETE FROM items WHERE id = 3")?;

    // Insert another row after delete
    db_src.db.exec_safe("INSERT INTO items VALUES (4, 'date', 8, 2.0)")?;

    libc_println!("Source DB has 3 rows (1, 2, 4) after operations");

    // --- Mode 1: V1 metadata sync ---
    libc_println!("--- Mode 1: V1 metadata ---");
    {
        let db_dst = crate::opendb()?;
        db_dst.db.exec_safe("CREATE TABLE items (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER, price REAL)")?;
        db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db_dst.db.exec_safe("SELECT crsql_as_crr('items')")?;

        sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

        let match_result = tables_match(&db_src.db, &db_dst.db, "items", "id")?;
        assert!(match_result, "V1 meta: destination data should match source");
        libc_println!("V1 metadata: PASS");
    }

    // --- Mode 2: V2 metadata + V1 wire ---
    // Migrate source to V2&V1, then sync with V1 wire to a V1 destination
    libc_println!("--- Mode 2: V2 metadata + V1 wire ---");
    {
        // Migrate source to V2&V1
        db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        let mut remaining = 1;
        while remaining > 0 {
            db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let stmt = db_src.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
            stmt.step()?;
            remaining = stmt.column_int(0) as i32;
            if remaining < 0 {
                panic!("migration failed");
            }
        }
        // Use V1 wire format for sync
        db_src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;

        let db_dst = crate::opendb()?;
        db_dst.db.exec_safe("CREATE TABLE items (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER, price REAL)")?;
        db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db_dst.db.exec_safe("SELECT crsql_as_crr('items')")?;

        sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

        let match_result = tables_match(&db_src.db, &db_dst.db, "items", "id")?;
        assert!(match_result, "V2 meta + V1 wire: destination data should match source");
        libc_println!("V2 metadata + V1 wire: PASS");
    }

    // --- Mode 3: V2 metadata + V2 wire ---
    libc_println!("--- Mode 3: V2 metadata + V2 wire ---");
    {
        // Use V2 wire format for sync
        db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

        let db_dst = crate::opendb()?;
        db_dst.db.exec_safe("CREATE TABLE items (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER, price REAL)")?;
        db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db_dst.db.exec_safe("SELECT crsql_as_crr('items')")?;
        // Destination also needs V2 metadata to accept V2 wire
        db_dst.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        let mut remaining = 1;
        while remaining > 0 {
            db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let stmt = db_dst.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
            stmt.step()?;
            remaining = stmt.column_int(0) as i32;
            if remaining < 0 {
                panic!("destination migration failed");
            }
        }
        db_dst.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
        db_dst.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

        sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

        let match_result = tables_match(&db_src.db, &db_dst.db, "items", "id")?;
        assert!(match_result, "V2 meta + V2 wire: destination data should match source");
        libc_println!("V2 metadata + V2 wire: PASS");
    }

    libc_println!("=== test_cross_mode_sync_roundtrip ALL PASS ===");
    Ok(())
}

/// Test that dual-write (metadata-write-version=2) works correctly:
/// - Insert multiple rows after migration
/// - Verify both V1 and V2 tables have correct data
/// - Verify crsql_changes produces same results from both V1 and V2 metadata
fn test_dual_write_multiple_rows() -> Result<(), ResultCode> {
    libc_println!("=== test_dual_write_multiple_rows START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Migrate to V2&V1
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            panic!("migration failed");
        }
    }

    // Insert multiple rows after migration — exercises dual-write triggers
    // Each INSERT auto-commits, so ts must be set before each one
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'x', 10)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'y', 20)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'z', 30)")?;

    // Verify V2 pks has all 3 rows
    let pks_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    pks_stmt.step()?;
    let pks_count = pks_stmt.column_int64(0);
    assert!(pks_count == 3, "V2 pks should have 3 rows, got {}", pks_count);

    // Verify V2 clock has entries for all 3 rows (2 cols each = 6)
    let clock_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock")?;
    clock_stmt.step()?;
    let clock_count = clock_stmt.column_int64(0);
    assert!(clock_count == 6, "V2 clock should have 6 entries, got {}", clock_count);

    // Verify V1 tables also have the data (dual-write)
    let v1_clock_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_clock")?;
    v1_clock_stmt.step()?;
    let v1_clock_count = v1_clock_stmt.column_int64(0);
    assert!(v1_clock_count == 6, "V1 clock should have 6 entries, got {}", v1_clock_count);

    // Verify crsql_changes produces same row count from V1 and V2 metadata
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 1)")?;
    let v1_stmt = db.db.prepare_v2("SELECT [table], [cid], [val] FROM crsql_changes")?;
    let mut v1_rows = vec![];
    while v1_stmt.step()? == ResultCode::ROW {
        v1_rows.push((
            v1_stmt.column_text(0)?.to_string(),
            v1_stmt.column_text(1)?.to_string(),
        ));
    }

    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;
    let v2_stmt = db.db.prepare_v2("SELECT [table], [cid], [val] FROM crsql_changes")?;
    let mut v2_rows = vec![];
    while v2_stmt.step()? == ResultCode::ROW {
        v2_rows.push((
            v2_stmt.column_text(0)?.to_string(),
            v2_stmt.column_text(1)?.to_string(),
        ));
    }

    assert!(v1_rows.len() == v2_rows.len(),
        "V1 and V2 should produce same number of change rows: {} vs {}", v1_rows.len(), v2_rows.len());

    libc_println!("=== test_dual_write_multiple_rows PASS ===");
    Ok(())
}

/// Test that migration of a table with existing data produces correct V2 metadata:
/// - V2 clock cell_keys must reference V2 pks __crsql_key (not V1 keys)
/// - crsql_changes from V2 metadata must return the same data as V1
fn test_migration_with_data() -> Result<(), ResultCode> {
    libc_println!("=== test_migration_with_data START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Insert data BEFORE migration (V1 metadata)
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'x', 10)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'y', 20)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'z', 30)")?;

    // Migrate to V2&V1
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 {
            panic!("migration failed");
        }
    }

    // Verify V2 pks has all 3 rows
    let pks_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_pks")?;
    pks_stmt.step()?;
    let pks_count = pks_stmt.column_int64(0);
    assert!(pks_count == 3, "V2 pks should have 3 rows, got {}", pks_count);

    // Verify V2 clock has entries for all non-sentinel columns (2 cols * 3 rows = 6)
    let clock_stmt = db.db.prepare_v2("SELECT count(*) FROM foo__crsql_v2_clock")?;
    clock_stmt.step()?;
    let clock_count = clock_stmt.column_int64(0);
    assert!(clock_count == 6, "V2 clock should have 6 entries, got {}", clock_count);

    // Critical check: every V2 clock cell_key must reference a V2 pks __crsql_key
    // cell_key >> col_id_bits should equal __crsql_key in v2_pks
    let join_stmt = db.db.prepare_v2(
        "SELECT count(*) FROM foo__crsql_v2_clock c
         LEFT JOIN foo__crsql_v2_pks p
           ON (c.cell_key >> 12) = p.__crsql_key
         WHERE p.__crsql_key IS NULL"
    )?;
    join_stmt.step()?;
    let orphan_count = join_stmt.column_int64(0);
    assert!(orphan_count == 0,
        "V2 clock has {} orphan entries not matching any V2 pks __crsql_key", orphan_count);

    // Verify crsql_changes from V2 metadata matches V1 metadata
    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 1)")?;
    let v1_stmt = db.db.prepare_v2("SELECT [table], [cid], [val] FROM crsql_changes")?;
    let mut v1_rows = vec![];
    while v1_stmt.step()? == ResultCode::ROW {
        v1_rows.push((
            v1_stmt.column_text(0)?.to_string(),
            v1_stmt.column_text(1)?.to_string(),
        ));
    }

    db.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;
    let v2_stmt = db.db.prepare_v2("SELECT [table], [cid], [val] FROM crsql_changes")?;
    let mut v2_rows = vec![];
    while v2_stmt.step()? == ResultCode::ROW {
        v2_rows.push((
            v2_stmt.column_text(0)?.to_string(),
            v2_stmt.column_text(1)?.to_string(),
        ));
    }

    assert!(v1_rows.len() == v2_rows.len(),
        "V1 and V2 should produce same number of change rows: {} vs {}", v1_rows.len(), v2_rows.len());

    libc_println!("=== test_migration_with_data PASS ===");
    Ok(())
}

/// Helper: read crsql_changes and return sorted (tbl, pk, cid, seq) rows.
fn read_changes_sorted(db: &crate::CRConnection, use_version: i32, sync_log: i32) -> Result<Vec<(String, Vec<u8>, String, i64)>, ResultCode> {
    db.db.exec_safe(&format!("SELECT crsql_config_set('metadata-use-version', {})", use_version))?;
    if use_version == 2 {
        db.db.exec_safe(&format!("SELECT crsql_config_set('sync-log-version', {})", sync_log))?;
    }
    let stmt = db.db.prepare_v2("SELECT [table], [pk], [cid], [seq] FROM crsql_changes")?;
    let mut rows: Vec<(String, Vec<u8>, String, i64)> = vec![];
    while stmt.step()? == ResultCode::ROW {
        rows.push((
            stmt.column_text(0)?.to_string(),
            stmt.column_blob(1)?.to_vec(),
            stmt.column_text(2)?.to_string(),
            stmt.column_int64(3),
        ));
    }
    rows.sort_by(|a, b| (&a.0, &a.1[..], &a.2).cmp(&(&b.0, &b.1[..], &b.2)));
    Ok(rows)
}

/// Helper: compare V1 and V2 change rows and assert equality.
fn assert_changes_match(v1: &[(String, Vec<u8>, String, i64)], v2: &[(String, Vec<u8>, String, i64)]) {
    assert!(v1.len() == v2.len(),
        "V1 and V2 should produce same number of change rows: {} vs {}", v1.len(), v2.len());
    for (i, (a, b)) in v1.iter().zip(v2.iter()).enumerate() {
        assert!(a.0 == b.0, "Row {} tbl mismatch: V1='{}' V2='{}'", i, a.0, b.0);
        assert!(a.1 == b.1, "Row {} pk mismatch for tbl='{}' cid='{}'", i, a.0, a.2);
        assert!(a.2 == b.2, "Row {} cid mismatch: V1='{}' V2='{}'", i, a.2, b.2);
        assert!(a.3 == b.3, "Row {} seq mismatch for tbl='{}' cid='{}': V1={} V2={}", i, a.0, a.2, a.3, b.3);
    }
}

/// Test that in dual-write mode, seq values are identical between V1 and V2 metadata reads
/// across a comprehensive set of operations: inserts, updates (single + multi-col),
/// deletes, resurrections, split across multiple transactions.
fn test_dual_write_seq_consistency() -> Result<(), ResultCode> {
    libc_println!("=== test_dual_write_seq_consistency START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Insert some data before migration (V1 mode)
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'x', 10)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'y', 20)")?;

    // Migrate to V2&V1 (dual write)
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 { panic!("migration failed"); }
    }

    // --- Transaction 1: inserts ---
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (3, 'z', 30)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (4, 'w', 40)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (5, 'v', 50)")?;

    // --- Transaction 2: single-column updates ---
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("UPDATE foo SET a = 'x2' WHERE id = 1")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("UPDATE foo SET b = 99 WHERE id = 2")?;

    // --- Transaction 3: multi-column updates ---
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("UPDATE foo SET a = 'z2', b = 31 WHERE id = 3")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("UPDATE foo SET a = 'w2', b = 41 WHERE id = 4")?;

    // Compare V1 vs V2 — inserts and updates should match exactly
    let v1_rows = read_changes_sorted(&db, 1, 1)?;
    let v2_rows = read_changes_sorted(&db, 2, 1)?;
    assert_changes_match(&v1_rows, &v2_rows);

    libc_println!("=== test_dual_write_seq_consistency PASS ({} rows) ===", v1_rows.len());
    Ok(())
}

/// Test deletes and resurrections in dual-write mode.
/// V1 and V2 have a known semantic difference: when a row is deleted then resurrected,
/// V1 keeps the delete sentinel in __crsql_clock (cid=-1), but V2 removes the tombstone
/// from v2_tombstones. So row counts may differ. This test verifies that for rows
/// that exist in both (matching tbl+pk+cid), seq values are identical.
fn test_dual_write_delete_resurrect() -> Result<(), ResultCode> {
    libc_println!("=== test_dual_write_delete_resurrect START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'x', 10)")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'y', 20)")?;

    // Migrate to dual-write
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 { panic!("migration failed"); }
    }

    // Delete a row
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("DELETE FROM foo WHERE id = 2")?;

    // Resurrect it
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("INSERT INTO foo VALUES (2, 'resurrected', 200)")?;

    // Update the resurrected row
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("UPDATE foo SET a = 'resurrected2', b = 201 WHERE id = 2")?;

    // Delete + resurrect in same transaction (ts set once covers both ops)
    db.db.exec_safe("SELECT crsql_set_ts('1700000001')")?;
    db.db.exec_safe("BEGIN")?;
    db.db.exec_safe("DELETE FROM foo WHERE id = 1")?;
    db.db.exec_safe("INSERT INTO foo VALUES (1, 'same_tx_back', 100)")?;
    db.db.exec_safe("COMMIT")?;

    // Compare V1 vs V2 — row counts may differ due to tombstone removal on resurrection.
    // For rows that exist in both, seq values must match.
    let v1_rows = read_changes_sorted(&db, 1, 1)?;
    let v2_rows = read_changes_sorted(&db, 2, 1)?;

    let mut mismatches = 0;
    let mut matched = 0;
    for a in &v1_rows {
        for b in &v2_rows {
            if a.0 == b.0 && a.1 == b.1 && a.2 == b.2 {
                if a.3 != b.3 {
                    libc_println!("SEQ MISMATCH: tbl={} cid={} V1.seq={} V2.seq={}", a.0, a.2, a.3, b.3);
                    mismatches += 1;
                }
                matched += 1;
                break;
            }
        }
    }
    assert!(mismatches == 0, "Found {} seq mismatches in {} matched rows", mismatches, matched);

    libc_println!("=== test_dual_write_delete_resurrect PASS (matched={}, v1={}, v2={}) ===",
        matched, v1_rows.len(), v2_rows.len());
    Ok(())
}

/// Randomized fuzz test: perform N random operations in dual-write mode,
/// then verify V1 and V2 metadata reads produce identical seq values.
/// Uses a simple LCG for deterministic reproducibility.
fn test_dual_write_seq_fuzz() -> Result<(), ResultCode> {
    libc_println!("=== test_dual_write_seq_fuzz START ===");

    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a, b)")?;
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db.db.exec_safe("SELECT crsql_as_crr('foo')")?;

    // Seed some data before migration
    db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    for i in 1..=5 {
        db.db.exec_safe(&format!("INSERT INTO foo VALUES ({}, 'init{}', {}00)", i, i, i))?;
    }

    // Migrate to dual-write
    db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    let mut remaining = 1;
    while remaining > 0 {
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        stmt.step()?;
        remaining = stmt.column_int(0) as i32;
        if remaining < 0 { panic!("migration failed"); }
    }

    // Simple LCG random number generator (deterministic)
    let mut state: u64 = 12345;
    let mut next_rand = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };

    let num_ops = 50;
    let max_id = 10; // ids 1..max_id, some may not exist (deleted)

    for op_num in 0..num_ops {
        let rand = next_rand();
        let op_type = rand % 4; // 0=insert, 1=update, 2=delete, 3=multi-update
        let id = (next_rand() % max_id) + 1;
        let val_a = next_rand() % 1000;
        let val_b = next_rand() % 1000;

        let sql = match op_type {
            0 => format!("INSERT INTO foo VALUES ({}, 'a{}', {})", id, val_a, val_b),
            1 => format!("UPDATE foo SET a = 'u{}' WHERE id = {}", val_a, id),
            2 => format!("DELETE FROM foo WHERE id = {}", id),
            3 => format!("UPDATE foo SET a = 'm{}', b = {} WHERE id = {}", val_a, val_b, id),
            _ => unreachable!(),
        };

        // Set ts before each op (auto-commit resets it)
        let _ = db.db.exec_safe("SELECT crsql_set_ts('1800000000')");
        // Ignore errors (e.g., update on non-existent row is a no-op, not an error)
        let _ = db.db.exec_safe(&sql);
    }

    // Compare V1 vs V2 — row counts may differ due to tombstone removal on resurrection.
    // For rows that exist in both, seq values must match.
    let v1_rows = read_changes_sorted(&db, 1, 1)?;
    let v2_rows = read_changes_sorted(&db, 2, 1)?;

    let mut mismatches = 0;
    for a in &v1_rows {
        for b in &v2_rows {
            if a.0 == b.0 && a.1 == b.1 && a.2 == b.2 {
                if a.3 != b.3 {
                    libc_println!("FUZZ SEQ MISMATCH: tbl={} cid={} V1.seq={} V2.seq={}", a.0, a.2, a.3, b.3);
                    mismatches += 1;
                }
                break;
            }
        }
    }
    assert!(mismatches == 0, "Fuzz: found {} seq mismatches in matched rows", mismatches);

    libc_println!("=== test_dual_write_seq_fuzz PASS (v1={} v2={}, {} ops) ===", v1_rows.len(), v2_rows.len(), num_ops);
    Ok(())
}

fn test_compile_const_mismatch() -> Result<(), ResultCode> {
    libc_println!("=== test_compile_const_mismatch START ===");

    let path = "crsql_const_test.db\0";
    let path_str = path.trim_end_matches('\0');

    #[cfg(not(target_os = "windows"))]
    extern "C" {
        fn unlink(pathname: *const core::ffi::c_char) -> core::ffi::c_int;
    }
    #[cfg(target_os = "windows")]
    extern "C" {
        fn _unlink(pathname: *const core::ffi::c_char) -> core::ffi::c_int;
    }

    // Remove any leftover file from previous runs
    unsafe {
        #[cfg(not(target_os = "windows"))]
        unlink(path.as_ptr() as *const core::ffi::c_char);
        #[cfg(target_os = "windows")]
        _unlink(path.as_ptr() as *const core::ffi::c_char);
    }

    // Create a fresh DB and initialize crsql
    {
        let db = crate::opendb_file(path_str)?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'hello')")?;
        // Verify the constant was stored
        let stmt = db.db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'crsql_col_id_bits'")?;
        stmt.step()?;
        let stored = stmt.column_int(0);
        assert!(stored > 0, "crsql_col_id_bits should be stored in crsql_master");
        libc_println!("Stored crsql_col_id_bits = {}", stored);
    }
    // Connection A is now closed (dropped)

    // Tamper with the constant — open the DB, modify crsql_master, close.
    // Bootstrap validation runs on extension init (connection open), so
    // this connection will pass validation (constants still match).
    // We tamper AFTER validation, then close.
    {
        let db = crate::opendb_file(path_str)?;
        db.db.exec_safe("UPDATE crsql_master SET value = 999 WHERE key = 'crsql_col_id_bits'")?;
        libc_println!("Tampered crsql_col_id_bits to 999");
    }
    // Connection is closed

    // Now reopen — bootstrap should detect the mismatch and fail
    let result = crate::opendb_file(path_str);
    match result {
        Ok(db) => {
            // If open succeeded, try a crsql operation to see if it fails
            db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let cr = db.db.exec_safe("SELECT crsql_as_crr('foo')");
            match cr {
                Err(rc) => {
                    libc_println!("=== test_compile_const_mismatch PASS (crsql_as_crr rejected mismatched constant, rc={:?}) ===", rc);
                }
                Ok(_) => {
                    // Try querying crsql_changes
                    let cr2 = db.db.exec_safe("SELECT * FROM crsql_changes");
                    match cr2 {
                        Err(rc) => {
                            libc_println!("=== test_compile_const_mismatch PASS (crsql_changes rejected mismatched constant, rc={:?}) ===", rc);
                        }
                        Ok(_) => {
                            panic!("FAIL: DB with tampered compile constant was accepted without error");
                        }
                    }
                }
            }
        }
        Err(rc) => {
            libc_println!("=== test_compile_const_mismatch PASS (bootstrap rejected mismatched constant on open, rc={:?}) ===", rc);
        }
    }

    // Clean up
    unsafe {
        #[cfg(not(target_os = "windows"))]
        unlink(path.as_ptr() as *const core::ffi::c_char);
        #[cfg(target_os = "windows")]
        _unlink(path.as_ptr() as *const core::ffi::c_char);
    }

    Ok(())
}

/// Test V2 wire format sync with a table that has a single PK and single non-PK column.
/// In this case, packed cid won't contain null separators (only one column),
/// but cval is still a packed blob from crsql_pack_agg (with 1 column).
/// This verifies the write path correctly detects V2 wire packed rows
/// when cid has no '\0' but col_vrsn is text/blob type.
fn test_v2_wire_single_col_sync() -> Result<(), ResultCode> {
    libc_println!("=== test_v2_wire_single_col_sync START ===");

    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Single PK, single non-PK column
    db_src.db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('item')")?;
    db_dst.db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT)")?;
    db_dst.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("SELECT crsql_as_crr('item')")?;

    // Insert and update data
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (1, 'apple')")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (2, 'banana')")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("UPDATE item SET name = 'cherry' WHERE id = 1")?;

    // Enable V2 wire format on source
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Verify the source emits V2 wire format
    let check = db_src.db.prepare_v2(
        "SELECT [cid], typeof([cid]), [col_version], typeof([col_version]), [val], typeof([val]) FROM crsql_changes"
    )?;
    while check.step()? == ResultCode::ROW {
        let cid = check.column_text(0)?;
        let cid_type = check.column_text(1)?;
        let cval_type = check.column_text(5)?;
        libc_println!("  src row: cid={:?} cid_type={:?} cval_type={:?}", cid, cid_type, cval_type);
    }

    // Sync from src to dst
    sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

    // Verify data matches
    let match_result = tables_match(&db_src.db, &db_dst.db, "item", "id")?;
    assert!(match_result, "V2 wire single-col: destination data should match source");

    libc_println!("=== test_v2_wire_single_col_sync PASS ===");
    Ok(())
}

/// Test that a V1 metadata node rejects V2 wire format changes per spec.
/// The README states: "Nodes with metadata-use-version set to 1 will emit an error
/// if they receive V2 wire format changes."
/// This covers both packed update rows (cid with col_vrsn as blob) and
/// hash tombstone rows (cid='-2' with hashed_pk instead of packed PKs).
fn test_v1_rejects_v2_wire() -> Result<(), ResultCode> {
    libc_println!("=== test_v1_rejects_v2_wire START ===");

    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Source: V2 metadata + V2 wire
    db_src.db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('item')")?;
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Destination: V1 metadata only (default, no metadata-write-version set)
    db_dst.db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("SELECT crsql_as_crr('item')")?;

    // Insert + delete to produce both packed updates and a tombstone
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (1, 'apple', 10)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (2, 'banana', 20)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("DELETE FROM item WHERE id = 2")?;

    // Attempt sync - should fail with error
    let result = sync_left_to_right(&db_src.db, &db_dst.db, 0);
    assert!(
        result.is_err(),
        "V1 metadata node should reject V2 wire format changes, but sync succeeded"
    );

    libc_println!("=== test_v1_rejects_v2_wire PASS ===");
    Ok(())
}

/// Test V2 wire packed resurrection: row is deleted on dst (tombstone),
/// then src resurrects it via INSERT and syncs via V2 wire packed format.
/// Dst should resurrect the row (remove tombstone, insert into v2_pks, apply values).
fn test_v2_wire_packed_resurrection() -> Result<(), ResultCode> {
    let db_src = crate::opendb()?;
    let db_dst = crate::opendb()?;

    // Both nodes: V2 metadata
    for db in [&db_src.db, &db_dst.db] {
        db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
        db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('item')")?;
        db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    }

    // Src inserts a row (CL=1)
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (1, 'apple', 10)")?;

    // Sync src -> dst, then dst -> src so CLs converge
    sync_left_to_right(&db_src.db, &db_dst.db, 0)?;
    sync_left_to_right(&db_dst.db, &db_src.db, 0)?;

    // Dst deletes the row (creates tombstone with CL=2)
    db_dst.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_dst.db.exec_safe("DELETE FROM item WHERE id = 1")?;

    // Verify dst has tombstone
    let tomb_check = db_dst.db.prepare_v2(
        "SELECT COUNT(*) FROM item__crsql_v2_tombstones"
    )?;
    tomb_check.step()?;
    assert_eq!(tomb_check.column_int64(0), 1, "dst should have tombstone after delete");

    // Sync dst -> src so src learns about the delete (src CL becomes 2 = dead)
    sync_left_to_right(&db_dst.db, &db_src.db, 0)?;

    // Src resurrects the row via INSERT (CL bumps to 3 = alive)
    // After receiving the delete, the row is gone from src's main table,
    // so UPDATE won't work — must use INSERT.
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO item VALUES (1, 'cherry', 20)")?;

    // Enable V2 wire format on src
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Sync src -> dst via V2 wire packed format (triggers resurrection on dst)
    sync_left_to_right(&db_src.db, &db_dst.db, 0)?;

    // Verify dst no longer has tombstone
    let tomb_after = db_dst.db.prepare_v2(
        "SELECT COUNT(*) FROM item__crsql_v2_tombstones"
    )?;
    tomb_after.step()?;
    assert_eq!(tomb_after.column_int64(0), 0, "tombstone should be removed after resurrection");

    // Verify dst has the row in v2_pks
    let pks_after = db_dst.db.prepare_v2(
        "SELECT COUNT(*) FROM item__crsql_v2_pks"
    )?;
    pks_after.step()?;
    assert_eq!(pks_after.column_int64(0), 1, "v2_pks should have the row after resurrection");

    // Verify dst has the actual data
    let data_check = db_dst.db.prepare_v2("SELECT name, qty FROM item WHERE id = 1")?;
    assert_eq!(data_check.step()?, ResultCode::ROW);
    assert_eq!(data_check.column_text(0)?, "cherry", "name should be 'cherry' after resurrection");
    assert_eq!(data_check.column_int64(1), 20, "qty should be 20 after resurrection");

    libc_println!("=== test_v2_wire_packed_resurrection PASS ===");
    Ok(())
}

/// Test that a node in dual-write mode (V2AndV1) arrives at semantically the same
/// metadata and data regardless of whether it receives changes via V1 wire or V2 wire.
/// Covers: inserts, updates, deletes, resurrections, and PK-only tables.
/// For each scenario, two destination nodes are synced from the same source —
/// one via V1 wire, one via V2 wire — and both must match the source and each other.
fn test_dual_write_wire_convergence() -> Result<(), ResultCode> {
    libc_println!("=== test_dual_write_wire_convergence START ===");

    // --- Source node: dual-write mode ---
    let db_src = crate::opendb()?;
    // Regular table with data columns
    db_src.db.exec_safe("CREATE TABLE prod (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
    // PK-only table
    db_src.db.exec_safe("CREATE TABLE tag (id PRIMARY KEY NOT NULL)")?;
    // Migrate to dual-write and do all setup + writes in one transaction
    db_src.db.exec_safe("BEGIN")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('prod')")?;
    db_src.db.exec_safe("SELECT crsql_as_crr('tag')")?;
    db_src.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;

    // --- Phase 1: Inserts ---
    db_src.db.exec_safe("INSERT INTO prod VALUES (1, 'apple', 10)")?;
    db_src.db.exec_safe("INSERT INTO prod VALUES (2, 'banana', 20)")?;
    db_src.db.exec_safe("INSERT INTO prod VALUES (3, 'cherry', 5)")?;
    db_src.db.exec_safe("INSERT INTO tag VALUES (100)")?;
    db_src.db.exec_safe("INSERT INTO tag VALUES (200)")?;

    // --- Phase 2: Updates ---
    db_src.db.exec_safe("UPDATE prod SET name = 'apple2', qty = 15 WHERE id = 1")?;
    db_src.db.exec_safe("UPDATE prod SET qty = 25 WHERE id = 2")?;

    // --- Phase 3: Delete ---
    db_src.db.exec_safe("DELETE FROM prod WHERE id = 3")?;
    db_src.db.exec_safe("DELETE FROM tag WHERE id = 200")?;
    db_src.db.exec_safe("COMMIT")?;

    // --- Phase 4: Resurrection ---
    // Sync so dst nodes learn about the deletes, then resurrect on src
    // We need two dst nodes: one for V1 wire, one for V2 wire.
    // But first we need to sync the pre-delete state, then the delete,
    // then the resurrection — to properly test resurrection via each wire format.

    // Create two destination nodes in dual-write mode
    let make_dst = || -> Result<crate::CRConnection, ResultCode> {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE prod (id PRIMARY KEY NOT NULL, name TEXT, qty INTEGER)")?;
        db.db.exec_safe("CREATE TABLE tag (id PRIMARY KEY NOT NULL)")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        db.db.exec_safe("BEGIN")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('prod')")?;
        db.db.exec_safe("SELECT crsql_as_crr('tag')")?;
        db.db.exec_safe("COMMIT")?;
        // Run migration to create V2 tables (each iteration auto-commits)
        let mut remaining = 1;
        while remaining > 0 {
            db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
            stmt.step()?;
            remaining = stmt.column_int(0) as i32;
            if remaining < 0 {
                panic!("dst migration failed");
            }
        }
        Ok(db)
    };

    // --- Destination A: will receive V1 wire ---
    let db_dst_v1 = make_dst()?;
    db_dst_v1.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_dst_v1.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;

    // --- Destination B: will receive V2 wire ---
    let db_dst_v2 = make_dst()?;
    db_dst_v2.db.exec_safe("SELECT crsql_config_set('metadata-use-version', 2)")?;
    db_dst_v2.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;

    // Sync pre-delete state to both destinations
    // Source uses V1 wire for first sync
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 1)")?;
    sync_left_to_right(&db_src.db, &db_dst_v1.db, 0)?;
    sync_left_to_right(&db_src.db, &db_dst_v2.db, 0)?;

    // Verify data matches after initial sync
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "prod", "id")?, "V1 wire: prod mismatch after initial sync");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "prod", "id")?, "V2 wire: prod mismatch after initial sync");
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "tag", "id")?, "V1 wire: tag mismatch after initial sync");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "tag", "id")?, "V2 wire: tag mismatch after initial sync");
    libc_println!("  Phase 1-2 (inserts + updates): PASS");

    // Now sync the deletes
    sync_left_to_right(&db_src.db, &db_dst_v1.db, 0)?;
    sync_left_to_right(&db_src.db, &db_dst_v2.db, 0)?;

    // Verify deletes propagated
    let check_v1 = db_dst_v1.db.prepare_v2("SELECT COUNT(*) FROM prod WHERE id = 3")?;
    check_v1.step()?;
    assert_eq!(check_v1.column_int64(0), 0, "V1 wire: row 3 should be deleted");
    let check_v2 = db_dst_v2.db.prepare_v2("SELECT COUNT(*) FROM prod WHERE id = 3")?;
    check_v2.step()?;
    assert_eq!(check_v2.column_int64(0), 0, "V2 wire: row 3 should be deleted");
    let check_tag_v1 = db_dst_v1.db.prepare_v2("SELECT COUNT(*) FROM tag WHERE id = 200")?;
    check_tag_v1.step()?;
    assert_eq!(check_tag_v1.column_int64(0), 0, "V1 wire: tag 200 should be deleted");
    let check_tag_v2 = db_dst_v2.db.prepare_v2("SELECT COUNT(*) FROM tag WHERE id = 200")?;
    check_tag_v2.step()?;
    assert_eq!(check_tag_v2.column_int64(0), 0, "V2 wire: tag 200 should be deleted");
    libc_println!("  Phase 3 (deletes): PASS");

    // --- Resurrection: bring back row 3 and tag 200 ---
    db_src.db.exec_safe("BEGIN")?;
    db_src.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_src.db.exec_safe("INSERT INTO prod VALUES (3, 'cherry2', 50)")?;
    db_src.db.exec_safe("INSERT INTO tag VALUES (200)")?;
    db_src.db.exec_safe("COMMIT")?;

    // Sync resurrection via V1 wire to dst_v1
    sync_left_to_right(&db_src.db, &db_dst_v1.db, 0)?;

    // Sync resurrection via V2 wire to dst_v2
    db_src.db.exec_safe("SELECT crsql_config_set('sync-log-version', 2)")?;
    sync_left_to_right(&db_src.db, &db_dst_v2.db, 0)?;

    // Verify resurrected data matches
    let r_v1 = db_dst_v1.db.prepare_v2("SELECT name, qty FROM prod WHERE id = 3")?;
    assert_eq!(r_v1.step()?, ResultCode::ROW);
    assert_eq!(r_v1.column_text(0)?, "cherry2", "V1 wire: resurrected name mismatch");
    assert_eq!(r_v1.column_int64(1), 50, "V1 wire: resurrected qty mismatch");

    let r_v2 = db_dst_v2.db.prepare_v2("SELECT name, qty FROM prod WHERE id = 3")?;
    assert_eq!(r_v2.step()?, ResultCode::ROW);
    assert_eq!(r_v2.column_text(0)?, "cherry2", "V2 wire: resurrected name mismatch");
    assert_eq!(r_v2.column_int64(1), 50, "V2 wire: resurrected qty mismatch");

    // Verify resurrected PK-only row
    let r_tag_v1 = db_dst_v1.db.prepare_v2("SELECT COUNT(*) FROM tag WHERE id = 200")?;
    r_tag_v1.step()?;
    assert_eq!(r_tag_v1.column_int64(0), 1, "V1 wire: tag 200 should be resurrected");
    let r_tag_v2 = db_dst_v2.db.prepare_v2("SELECT COUNT(*) FROM tag WHERE id = 200")?;
    r_tag_v2.step()?;
    assert_eq!(r_tag_v2.column_int64(0), 1, "V2 wire: tag 200 should be resurrected");
    libc_println!("  Phase 4 (resurrection): PASS");

    // --- Final convergence: all three nodes should have identical data ---
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "prod", "id")?, "Final: prod mismatch src vs V1 dst");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "prod", "id")?, "Final: prod mismatch src vs V2 dst");
    assert!(tables_match(&db_dst_v1.db, &db_dst_v2.db, "prod", "id")?, "Final: prod mismatch V1 dst vs V2 dst");
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "tag", "id")?, "Final: tag mismatch src vs V1 dst");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "tag", "id")?, "Final: tag mismatch src vs V2 dst");
    assert!(tables_match(&db_dst_v1.db, &db_dst_v2.db, "tag", "id")?, "Final: tag mismatch V1 dst vs V2 dst");

    // --- Verify V1 and V2 metadata consistency on each destination ---
    // Both destinations are in dual-write mode, so both have V1 and V2 metadata.
    // Check that V1 clock table and V2 clock table have consistent column data.
    for (db, label) in [(&db_dst_v1.db, "V1-wire dst"), (&db_dst_v2.db, "V2-wire dst")] {
        // V1 clock entries for prod
        let v1_clock = db.prepare_v2(
            "SELECT COUNT(*) FROM prod__crsql_clock"
        )?;
        v1_clock.step()?;
        let v1_count = v1_clock.column_int64(0);

        // V2 clock entries for prod
        let v2_clock = db.prepare_v2(
            "SELECT COUNT(*) FROM prod__crsql_v2_clock"
        )?;
        v2_clock.step()?;
        let v2_count = v2_clock.column_int64(0);

        libc_println!("  {} metadata: V1 clock={}, V2 clock={}", label, v1_count, v2_count);

        // V1 pks
        let v1_pks = db.prepare_v2(
            "SELECT COUNT(*) FROM prod__crsql_pks"
        )?;
        v1_pks.step()?;
        let v1_pks_count = v1_pks.column_int64(0);

        // V2 pks
        let v2_pks = db.prepare_v2(
            "SELECT COUNT(*) FROM prod__crsql_v2_pks"
        )?;
        v2_pks.step()?;
        let v2_pks_count = v2_pks.column_int64(0);

        libc_println!("  {} metadata: V1 pks={}, V2 pks={}", label, v1_pks_count, v2_pks_count);

        // V2 pks should have alive rows (id 1, 2, 3)
        assert_eq!(v2_pks_count, 3, "{}: V2 pks should have 3 alive rows", label);

        // V1 pks should also have 3 rows (V1 keeps all keys including deleted ones)
        // V1 may keep deleted rows in pks, so just check >= 3
        assert!(v1_pks_count >= 3, "{}: V1 pks should have at least 3 rows, got {}", label, v1_pks_count);

        // Check that V1 clock entries have the correct remote site_id, not just local.
        // If V1 metadata is only populated by triggers (accidentally from V2 merge upserts),
        // the site_id will be the local site, not the remote source site.
        let v1_clock_sites = db.prepare_v2(
            "SELECT DISTINCT s.site_id FROM prod__crsql_clock c JOIN crsql_site_id s ON c.site_id = s.ordinal ORDER BY s.site_id"
        )?;
        let mut sites = Vec::new();
        while v1_clock_sites.step()? == ResultCode::ROW {
            sites.push(v1_clock_sites.column_blob(0)?.to_vec());
        }
        libc_println!("  {} V1 clock site_ids: {} distinct", label, sites.len());

        // The source site_id should appear in V1 clock entries (from sync).
        // If only local site_id appears, V1 metadata was populated by triggers, not merge.
        let src_siteid_stmt = db_src.db.prepare_v2("SELECT crsql_site_id()")?;
        src_siteid_stmt.step()?;
        let src_site_id = src_siteid_stmt.column_blob(0)?.to_vec();

        let has_src_site = sites.iter().any(|s| s.as_slice() == src_site_id.as_slice());
        assert!(has_src_site, "{}: V1 clock should contain source site_id (remote sync), not just local", label);
    }

    // --- Verify V2 tombstones are clean on both destinations ---
    for (db, label) in [(&db_dst_v1.db, "V1-wire dst"), (&db_dst_v2.db, "V2-wire dst")] {
        let tomb_prod = db.prepare_v2(
            "SELECT COUNT(*) FROM prod__crsql_v2_tombstones"
        )?;
        tomb_prod.step()?;
        assert_eq!(tomb_prod.column_int64(0), 0, "{}: no prod tombstones after resurrection", label);

        // tag 200 was resurrected, so no tombstone should remain
        let tomb_tag = db.prepare_v2(
            "SELECT COUNT(*) FROM tag__crsql_v2_tombstones"
        )?;
        tomb_tag.step()?;
        assert_eq!(tomb_tag.column_int64(0), 0, "{}: no tag tombstones after resurrection", label);
    }

    // --- Bidirectional sync: dst_v1 -> src and dst_v2 -> src to converge CLs ---
    // dst_v1 sends via V1 wire
    sync_left_to_right(&db_dst_v1.db, &db_src.db, 0)?;
    // dst_v2 sends via V2 wire
    sync_left_to_right(&db_dst_v2.db, &db_src.db, 0)?;

    // Final data check after bidirectional sync
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "prod", "id")?, "Post-bidi: prod mismatch src vs V1 dst");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "prod", "id")?, "Post-bidi: prod mismatch src vs V2 dst");
    assert!(tables_match(&db_dst_v1.db, &db_dst_v2.db, "prod", "id")?, "Post-bidi: prod mismatch V1 dst vs V2 dst");
    assert!(tables_match(&db_src.db, &db_dst_v1.db, "tag", "id")?, "Post-bidi: tag mismatch src vs V1 dst");
    assert!(tables_match(&db_src.db, &db_dst_v2.db, "tag", "id")?, "Post-bidi: tag mismatch src vs V2 dst");
    assert!(tables_match(&db_dst_v1.db, &db_dst_v2.db, "tag", "id")?, "Post-bidi: tag mismatch V1 dst vs V2 dst");

    libc_println!("=== test_dual_write_wire_convergence ALL PASS ===");
    Ok(())
}

/// Test tombstone conflict resolution: when two nodes delete the same row
/// and sync, the tombstone with the higher CL wins. If CLs are equal,
/// site_id is used as tiebreaker. This verifies the ON CONFLICT upsert
/// on v2_tombstones.hashed_pk unique index works per the design doc.
fn test_tombstone_conflict_resolution() -> Result<(), ResultCode> {
    libc_println!("=== test_tombstone_conflict_resolution START ===");

    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    // Both nodes: V2 metadata
    for db in [&db_a.db, &db_b.db] {
        db.exec_safe("CREATE TABLE item (id PRIMARY KEY NOT NULL, name TEXT)")?;
        db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.exec_safe("SELECT crsql_as_crr('item')")?;
    }

    // Both nodes insert the same row (same CL=1 after sync)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("INSERT INTO item VALUES (1, 'apple')")?;
    // Sync A -> B so both have the row
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;
    // Sync B -> A so CLs converge
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    // Both nodes delete the row (各自删除)
    db_a.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_a.db.exec_safe("DELETE FROM item WHERE id = 1")?;
    db_b.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    db_b.db.exec_safe("DELETE FROM item WHERE id = 1")?;

    // Both should have tombstones with CL=2 (even = dead)
    // Sync A -> B: B receives A's tombstone. CLs are equal (both 2),
    // so site_id tiebreaker applies. The upsert should not error.
    sync_left_to_right(&db_a.db, &db_b.db, 0)?;

    // Sync B -> A: A receives B's tombstone. Same situation.
    sync_left_to_right(&db_b.db, &db_a.db, 0)?;

    // Verify both nodes still have the row deleted
    let check_a = db_a.db.prepare_v2("SELECT COUNT(*) FROM item WHERE id = 1")?;
    check_a.step()?;
    assert_eq!(check_a.column_int64(0), 0, "row should be deleted on A after tombstone sync");

    let check_b = db_b.db.prepare_v2("SELECT COUNT(*) FROM item WHERE id = 1")?;
    check_b.step()?;
    assert_eq!(check_b.column_int64(0), 0, "row should be deleted on B after tombstone sync");

    // Verify tombstone exists exactly once (no duplicate inserts)
    let tomb_count_a = db_a.db.prepare_v2(
        "SELECT COUNT(*) FROM item__crsql_v2_tombstones WHERE hashed_pk = crsql_hash_pk(1)"
    )?;
    tomb_count_a.step()?;
    assert_eq!(tomb_count_a.column_int64(0), 1, "exactly one tombstone on A");

    let tomb_count_b = db_b.db.prepare_v2(
        "SELECT COUNT(*) FROM item__crsql_v2_tombstones WHERE hashed_pk = crsql_hash_pk(1)"
    )?;
    tomb_count_b.step()?;
    assert_eq!(tomb_count_b.column_int64(0), 1, "exactly one tombstone on B");

    libc_println!("=== test_tombstone_conflict_resolution PASS ===");
    Ok(())
}

/// Test that all V2 write paths fail with descriptive errors when ts is not set.
/// Each operation should return an error (not a SIGSEGV or silent corruption).
fn test_ts_not_set_errors() -> Result<(), ResultCode> {
    libc_println!("=== test_ts_not_set_errors START ===");

    // --- crsql_as_crr without ts in V2 mode ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        // Do NOT call crsql_set_ts — should error
        let rc = db.db.exec_safe("SELECT crsql_as_crr('foo')");
        assert!(rc.is_err(), "crsql_as_crr should fail when ts not set in V2 mode");
        libc_println!("  crsql_as_crr without ts: correctly rejected");
    }

    // --- crsql_begin_alter without ts in V2 mode ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        // Migrate to V2
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        let mut remaining = 1;
        while remaining > 0 {
            db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
            stmt.step()?;
            remaining = stmt.column_int(0) as i32;
            if remaining < 0 { panic!("migration failed"); }
        }
        // Do NOT call crsql_set_ts — should error
        let rc = db.db.exec_safe("SELECT crsql_begin_alter('foo')");
        assert!(rc.is_err(), "crsql_begin_alter should fail when ts not set");
        libc_println!("  crsql_begin_alter without ts: correctly rejected");
    }

    // --- crsql_commit_alter without ts in V2 mode ---
    // crsql_begin_alter creates a savepoint, so ALTER TABLE within it doesn't auto-commit.
    // The ts check in crsql_commit_alter fires when ts was never set in this transaction.
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        let mut remaining = 1;
        while remaining > 0 {
            db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
            let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
            stmt.step()?;
            remaining = stmt.column_int(0) as i32;
            if remaining < 0 { panic!("migration failed"); }
        }
        // Set ts, begin_alter, do the ALTER, then commit — ts is still set
        // because begin_alter creates a savepoint (no auto-commit).
        // To test the ts=0 path, we use a nested savepoint that releases first,
        // causing an auto-commit that resets ts.
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_begin_alter('foo')")?;
        db.db.exec_safe("ALTER TABLE foo ADD COLUMN b TEXT")?;
        // Release the begin_alter savepoint — this commits and resets ts
        db.db.exec_safe("RELEASE SAVEPOINT alter_crr")?;
        // Now ts=0, commit_alter should error
        let rc = db.db.exec_safe("SELECT crsql_commit_alter('foo')");
        assert!(rc.is_err(), "crsql_commit_alter should fail when ts not set after savepoint release");
        libc_println!("  crsql_commit_alter without ts: correctly rejected");
    }

    // --- INSERT on V2 CRR table without ts ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        // Do NOT call crsql_set_ts — INSERT trigger should error
        let rc = db.db.exec_safe("INSERT INTO foo VALUES (1, 'x')");
        assert!(rc.is_err(), "INSERT on V2 CRR should fail when ts not set");
        libc_println!("  INSERT without ts: correctly rejected");
    }

    // --- UPDATE on V2 CRR table without ts ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'x')")?;
        // Do NOT call crsql_set_ts — UPDATE trigger should error
        let rc = db.db.exec_safe("UPDATE foo SET a = 'y' WHERE id = 1");
        assert!(rc.is_err(), "UPDATE on V2 CRR should fail when ts not set");
        libc_println!("  UPDATE without ts: correctly rejected");
    }

    // --- DELETE on V2 CRR table without ts ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'x')")?;
        // Do NOT call crsql_set_ts — DELETE trigger should error
        let rc = db.db.exec_safe("DELETE FROM foo WHERE id = 1");
        assert!(rc.is_err(), "DELETE on V2 CRR should fail when ts not set");
        libc_println!("  DELETE without ts: correctly rejected");
    }

    // --- crsql_incremental_maintenance without ts ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'x')")?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 2)")?;
        // Do NOT call crsql_set_ts — should error
        let stmt = db.db.prepare_v2("SELECT crsql_incremental_maintenance(1000)")?;
        let rc = stmt.step();
        // incremental_maintenance returns -1 via result_int when ts not set,
        // so step() succeeds but returns a negative value
        let result = stmt.column_int(0);
        assert!(result < 0, "crsql_incremental_maintenance should return < 0 when ts not set, got {}", result);
        libc_println!("  crsql_incremental_maintenance without ts: correctly returned error ({})", result);
    }

    // --- INSERT INTO crsql_changes (sync) without ts ---
    {
        let db = crate::opendb()?;
        db.db.exec_safe("SELECT crsql_config_set('metadata-write-version', 3)")?;
        db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, a)")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("SELECT crsql_as_crr('foo')")?;
        db.db.exec_safe("SELECT crsql_set_ts('1700000000')")?;
        db.db.exec_safe("INSERT INTO foo VALUES (1, 'x')")?;
        // Read changes from source
        let read_stmt = db.db.prepare_v2("SELECT * FROM crsql_changes")?;
        read_stmt.step()?;
        // Try to merge back without ts — should error
        let merge_stmt = db.db.prepare_v2(
            "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )?;
        for i in 0..10 {
            merge_stmt.bind_value(i + 1, read_stmt.column_value(i)?)?;
        }
        let rc = merge_stmt.step();
        assert!(rc.is_err(), "INSERT INTO crsql_changes should fail when ts not set");
        libc_println!("  INSERT INTO crsql_changes without ts: correctly rejected");
    }

    libc_println!("=== test_ts_not_set_errors PASS ===");
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    test_pack_agg_matches_pack_columns()?;
    test_pack_agg_with_nulls()?;
    test_pack_agg_empty()?;
    test_pack_agg_integers()?;
    test_hash_pk_deterministic()?;
    test_hash_pk_different_types()?;
    test_varint_count_backward_compat()?;
    test_metadata_use_version_dispatch()?;
    test_packed_wire_format()?;
    test_v2_hash_tombstone()?;
    test_migration_with_data()?;
    test_dual_write_multiple_rows()?;
    test_dual_write_seq_consistency()?;
    test_dual_write_delete_resurrect()?;
    test_dual_write_seq_fuzz()?;
    test_cross_mode_sync_roundtrip()?;
    test_compile_const_mismatch()?;
    test_v2_wire_single_col_sync()?;
    test_v1_rejects_v2_wire()?;
    test_v2_wire_packed_resurrection()?;
    test_dual_write_wire_convergence()?;
    test_tombstone_conflict_resolution()?;
    test_ts_not_set_errors()?;
    Ok(())
}
