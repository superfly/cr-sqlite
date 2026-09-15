extern crate crsql_bundle;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

/*
Test:
- create crr
- destroy crr
- use crr that was created
- create if not exist vtab
-
*/

fn create_crr_via_vtab() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    let conn = &db.db;

    conn.exec_safe("CREATE VIRTUAL TABLE foo_schema USING CLSet (a primary key not null, b);")?;
    conn.exec_safe("INSERT INTO foo VALUES (1, 2);")?;
    let stmt = conn.prepare_v2("SELECT count(*) FROM crsql_changes")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 1);
    Ok(())
}

fn destroy_crr_via_vtab() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    let conn = &db.db;

    conn.exec_safe("CREATE VIRTUAL TABLE foo_schema USING CLSet (a primary key not null, b);")?;
    conn.exec_safe("DROP TABLE foo_schema")?;
    let stmt = conn.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name LIKE '%foo%'")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 0);
    // Also verify no remaining entries in crsql_master for the table.
    let stmt = conn.prepare_v2("SELECT count(*) FROM crsql_master WHERE key LIKE '%foo%'")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 0);
    Ok(())
}

fn create_invalid_crr() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    let conn = &db.db;

    let result = conn.exec_safe("CREATE VIRTUAL TABLE foo_schema USING CLSet (a, b);");
    assert_eq!(result, Err(ResultCode::ERROR));
    let msg = conn.errmsg().unwrap();
    assert!(
        msg.contains("has no primary key or primary key is nullable"),
        "unexpected error message: {}",
        msg
    );
    // Verify no CRR metadata remains after the failed creation. The base table
    // `foo` may still exist (it is created before the PK validation check),
    // but the clock table and crsql_master entries should not.
    let stmt = conn.prepare_v2("SELECT count(*) FROM sqlite_master WHERE name='foo__crsql_clock'")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 0, "no clock table should remain after failed creation");
    Ok(())
}

fn create_if_not_exists() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    let conn = &db.db;

    conn.exec_safe(
        "CREATE VIRTUAL TABLE IF NOT EXISTS foo_schema USING CLSet (a primary key not null, b);",
    )?;
    conn.exec_safe("INSERT INTO foo VALUES (1, 2);")?;
    let stmt = conn.prepare_v2("SELECT count(*) FROM crsql_changes")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 1);
    drop(stmt);
    // second create is a no-op
    conn.exec_safe(
        "CREATE VIRTUAL TABLE IF NOT EXISTS foo_schema USING CLSet (a primary key not null, b);",
    )?;
    let stmt = conn.prepare_v2("SELECT count(*) FROM crsql_changes")?;
    stmt.step()?;
    let count = stmt.column_int(0);
    assert_eq!(count, 1);
    Ok(())
}

// and later migration tests
// UPDATE foo SET schema = '...';
// INSERT INTO foo (alter) VALUES ('...');
// and auto-migrate tests for whole schema.
// auto-migrate would...
// - re-write `create vtab` things as `update foo set schema = ...` where those vtabs did not exist.

pub fn run_suite() -> Result<(), ResultCode> {
    create_crr_via_vtab()?;
    destroy_crr_via_vtab()?;
    create_invalid_crr()?;
    create_if_not_exists()
}
