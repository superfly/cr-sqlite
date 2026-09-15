/*
 * These tests are very similar to `pk_only_tables` tests.
 * What we want to test here is that rows whose primary keys get changed get
 * replicated correctly.
 *
 * Example:
 * ```
 * CREATE TABLE foo (id primary key, value);
 * ```
 *
 * | id | value |
 * | -- | ----- |
 * | 1  |  abc  |
 *
 * Now we:
 * ```
 * UPDATE foo SET id = 2 WHERE id = 1;
 * ```
 *
 * This should be a _delete_ of row id 1 and a _create_ of
 * row id 2, bringing all the values from row 1 to row 2.
 *
 * pk_only_tables.rs tested this for table that _only_
 * had primary key columns but not for tables that have
 * primary key columns + other columns.
 */
extern crate crsql_bundle;
use sqlite::Destructor;
use sqlite::ManagedConnection;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

fn sync_left_to_right(l: &dyn Connection, r: &dyn Connection, since: sqlite::int64) {
    let siteid_stmt = r.prepare_v2("SELECT crsql_site_id()").expect("prepared");
    siteid_stmt.step().expect("stepped");
    let siteid = siteid_stmt.column_blob(0).expect("got site id");

    let stmt_l = l
        .prepare_v2("SELECT * FROM crsql_changes WHERE db_version > ? AND site_id IS NOT ?")
        .expect("prepared select changes");
    stmt_l.bind_int64(1, since).expect("bound db version");
    stmt_l
        .bind_blob(2, siteid, Destructor::STATIC)
        .expect("bound site id");

    r.exec_safe("BEGIN").expect("begin");
    r.exec_safe("SELECT crsql_set_ts('1700000000')").expect("set ts");

    while stmt_l.step().expect("pulled change set") == ResultCode::ROW {
        let stmt_r = r
            .prepare_v2("INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .expect("prepared insert changes");
        for x in 0..10 {
            stmt_r
                .bind_value(x + 1, stmt_l.column_value(x).expect("got changeset value"))
                .expect("bound value");
        }
        stmt_r.step().expect("inserted change");
    }
    r.exec_safe("COMMIT").expect("commit");
}

fn setup_schema(db: &ManagedConnection) {
    db.exec_safe("CREATE TABLE foo (id INTEGER PRIMARY KEY NOT NULL, value);")
        .expect("created foo");
    db.exec_safe("SELECT crsql_set_ts('1700000000')")
        .expect("set ts");
    db.exec_safe("SELECT crsql_as_crr('foo');")
        .expect("converted to crr");
}

/// Test that a primary key update on a table with PK + data columns
/// replicates as a delete of the old PK and a create of the new PK,
/// carrying the data column values over to the new row.
fn pk_update_with_data_column() -> Result<(), ResultCode> {
    let db_a = crate::opendb()?;
    let db_b = crate::opendb()?;

    setup_schema(&db_a.db);
    setup_schema(&db_b.db);

    // Insert a row with a data column value.
    let stmt = db_a
        .db
        .prepare_v2("INSERT INTO foo (id, value) VALUES (1, 'abc');")
        .expect("prepare insert to foo");
    stmt.step().expect("inserted row");

    // Sync the initial insert to db_b.
    sync_left_to_right(&db_a.db, &db_b.db, -1);

    // Verify the row arrived on db_b.
    let stmt = db_b
        .db
        .prepare_v2("SELECT id, value FROM foo WHERE id = 1;")
        .expect("prepare select from foo");
    let result = stmt.step().expect("stepped");
    assert_eq!(result, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 1);
    assert_eq!(stmt.column_text(1)?, "abc");

    // Update the primary key on db_a.
    let stmt = db_a
        .db
        .prepare_v2("UPDATE foo SET id = 2 WHERE id = 1;")
        .expect("prepare update pk");
    let result = stmt.step();
    assert_eq!(result, Ok(ResultCode::DONE), "failed to update pk");

    // Sync the PK update to db_b.
    sync_left_to_right(&db_a.db, &db_b.db, 0);

    // The old PK row (id = 1) should be gone on db_b.
    let stmt = db_b
        .db
        .prepare_v2("SELECT id, value FROM foo WHERE id = 1;")
        .expect("prepare select old pk");
    let result = stmt.step().expect("stepped");
    assert_eq!(
        result,
        ResultCode::DONE,
        "old PK row should be deleted after PK update replication"
    );

    // The new PK row (id = 2) should exist on db_b with the carried-over value.
    let stmt = db_b
        .db
        .prepare_v2("SELECT id, value FROM foo WHERE id = 2;")
        .expect("prepare select new pk");
    let result = stmt.step().expect("stepped");
    assert_eq!(result, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 2);
    assert_eq!(stmt.column_text(1)?, "abc");

    // Ensure only one row remains on db_b.
    let stmt = db_b
        .db
        .prepare_v2("SELECT COUNT(*) FROM foo;")
        .expect("prepare count");
    let result = stmt.step().expect("stepped");
    assert_eq!(result, ResultCode::ROW);
    assert_eq!(stmt.column_int(0), 1);

    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    pk_update_with_data_column()?;
    Ok(())
}
