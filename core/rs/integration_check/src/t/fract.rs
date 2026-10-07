extern crate crsql_bundle;
use sqlite::Connection;
use sqlite_nostd as sqlite;

fn sort_no_list_col() {
    let w = crate::opendb().expect("db opened");
    let db = &w.db;

    db.exec_safe("CREATE TABLE todo (id primary key, position)")
        .expect("table created");
    db.exec_safe("SELECT crsql_fract_as_ordered('todo', 'position')")
        .expect("as ordered");
    db.exec_safe(
        // Use distinct fractional indices for each id so the test exercises
        // the intended reposition path rather than the conflict-resolution path.
        "INSERT INTO todo VALUES (1, 'Zm'), (2, 'ZmG'), (3, 'ZmM'), (4, 'ZmV'), (5, 'Zn')",
    )
    .expect("inserted initial values");
    db.exec_safe("UPDATE todo_fractindex SET after_id = 2 WHERE id = 5")
        .expect("repositioned id 5");

    // Query the resulting ordering and assert the expected order.
    // After moving id 5 to be immediately after id 2, the order should be:
    // 1, 2, 5, 3, 4
    let stmt = db
        .prepare_v2("SELECT id FROM todo ORDER BY position")
        .expect("prepared select ordered ids");
    let expected = [1, 2, 5, 3, 4];
    let mut idx = 0;
    while stmt.step().expect("stepped") == sqlite::ResultCode::ROW {
        let id = stmt.column_int(0);
        assert_eq!(
            id, expected[idx],
            "id at position {} was {} but expected {}",
            idx, id, expected[idx]
        );
        idx += 1;
    }
    assert_eq!(idx, expected.len(), "expected {} rows", expected.len());
}

pub fn run_suite() {
    sort_no_list_col();
}
