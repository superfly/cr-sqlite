extern crate crsql_bundle;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

// If sync bit is on, nothing gets written to clock tables for that connection.
//
// This test includes a negative control: DML WITHOUT the sync bit MUST create
// clock entries, proving the triggers are functional. Only then does the sync
// bit test (DML WITH sync bit → no new clock entries) become meaningful —
// otherwise a "no clock entries" result could just mean the triggers are broken.
fn sync_bit_honored() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    let conn = &db.db;
    conn.exec_safe("CREATE TABLE foo (a primary key not null, b);")?;
    conn.exec_safe("SELECT crsql_set_ts('1700000000')")?;
    conn.exec_safe("SELECT crsql_as_crr('foo');")?;

    // --- Negative control: DML WITHOUT sync bit MUST create clock entries ---
    conn.exec_safe("INSERT INTO foo VALUES (1, 2);")?;
    let stmt = conn.prepare_v2("SELECT count(*) FROM foo__crsql_clock")?;
    stmt.step()?;
    let neg_count = stmt.column_int(0);
    assert!(
        neg_count > 0,
        "negative control: clock entries should be created without sync bit, got {}",
        neg_count
    );
    // Verify base table state after the control DML.
    let stmt = conn.prepare_v2("SELECT count(*) FROM foo")?;
    stmt.step()?;
    assert!(
        stmt.column_int(0) == 1,
        "negative control: base table should have 1 row, got {}",
        stmt.column_int(0)
    );

    // --- Positive test: DML WITH sync bit should NOT create clock entries ---
    conn.exec_safe("SELECT crsql_internal_sync_bit(1)")?;
    conn.exec_safe("INSERT INTO foo VALUES (3, 4);")?;
    conn.exec_safe("UPDATE foo SET b = 5 WHERE a = 1;")?;
    conn.exec_safe("INSERT INTO foo VALUES (5, 6);")?;
    conn.exec_safe("DELETE FROM foo WHERE a = 5;")?;
    conn.exec_safe("SELECT crsql_internal_sync_bit(0)")?;

    // Verify the base table has the expected rows after sync-bit DML.
    let stmt = conn.prepare_v2("SELECT count(*) FROM foo")?;
    stmt.step()?;
    assert!(
        stmt.column_int(0) == 2,
        "base table should have 2 rows after sync-bit DML, got {}",
        stmt.column_int(0)
    );
    let stmt = conn.prepare_v2("SELECT b FROM foo WHERE a = 1")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 5, "foo(1).b should be 5 after update");
    let stmt = conn.prepare_v2("SELECT b FROM foo WHERE a = 3")?;
    stmt.step()?;
    assert!(stmt.column_int(0) == 4, "foo(3).b should be 4");

    // The V1 clock table should still only have entries from the negative control
    // (no new entries from the sync-bit DML).
    // Note: this assertion checks the V1 clock table (foo__crsql_clock). If the
    // default metadata-write-version changes to V2, this test should also check
    // foo__crsql_v2_clock for the same invariant.
    let stmt = conn.prepare_v2("SELECT count(*) FROM foo__crsql_clock")?;
    stmt.step()?;
    let pos_count = stmt.column_int(0);
    assert!(
        pos_count == neg_count,
        "sync bit on: no new clock entries should be created, expected {} got {}",
        neg_count,
        pos_count
    );

    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    sync_bit_honored()
}
