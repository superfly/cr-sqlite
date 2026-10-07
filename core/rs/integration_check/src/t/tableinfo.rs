extern crate alloc;
use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use core::{ffi::c_char, ffi::c_int, mem};
use crsql_bundle::test_exports;
use crsql_bundle::test_exports::tableinfo::TableInfo;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

// Unfortunate circumstance that we still have some C code that requires this argument
fn make_err_ptr() -> *mut *mut c_char {
    let boxed = Box::new(core::ptr::null_mut() as *mut c_char);
    return Box::into_raw(boxed);
}

fn drop_err_ptr(err: *mut *mut c_char) {
    unsafe {
        let ptr = Box::from_raw(err);
        if ptr.is_null() {
            return;
        }
        let _ = CString::from_raw(*ptr);
    }
}

fn make_site() -> *mut c_char {
    let inner_ptr: *mut c_char = CString::new("0000000000000000").unwrap().into_raw();
    inner_ptr
}

fn test_ensure_table_infos_are_up_to_date() {
    let db = crate::opendb().expect("Opened DB");
    let c = &db.db;
    let raw_db = db.db.db;
    let err = make_err_ptr();

    // manually create some clock tables w/o using the extension
    // pull table info and ensure it is what we expect
    c.exec_safe("CREATE TABLE foo (a PRIMARY KEY NOT NULL, b);")
        .expect("made foo");
    c.exec_safe(
        "CREATE TABLE foo__crsql_clock (
      id,
      col_name,
      col_version,
      db_version,
      site_id,
      seq,
      ts
    )",
    )
    .expect("made foo clock");

    let ext_data = unsafe { test_exports::c::crsql_newExtData(raw_db) };
    assert!(!ext_data.is_null(), "crsql_newExtData returned null");
    let rc = unsafe { test_exports::c::crsql_initSiteIdExt(raw_db, ext_data, make_site() as *mut core::ffi::c_uchar) };
    assert_eq!(rc, 0);
    let rc = test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err);
    assert_eq!(rc, ResultCode::OK as c_int);

    let mut table_infos = unsafe {
        mem::ManuallyDrop::new(Box::from_raw((*ext_data).tableInfos as *mut Vec<TableInfo>))
    };

    assert_eq!(table_infos.len(), 1);
    assert_eq!(table_infos[0].tbl_name, "foo");

    // we're going to change table infos so we can check that it does not get filled again since no schema changes happened
    table_infos[0].tbl_name = "bar".to_string();

    unsafe {
        (*ext_data).updatedTableInfosThisTx = 0;
    }
    let rc = test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err);
    assert_eq!(rc, ResultCode::OK as c_int);

    assert_eq!(table_infos.len(), 1);
    assert_eq!(table_infos[0].tbl_name, "bar");

    c.exec_safe("CREATE TABLE boo (a PRIMARY KEY NOT NULL, b);")
        .expect("made boo");
    c.exec_safe(
        "CREATE TABLE boo__crsql_clock (
      id,
      col_name,
      col_version,
      db_version,
      site_id,
      seq,
      ts
    )",
    )
    .expect("made boo clock");

    unsafe {
        (*ext_data).updatedTableInfosThisTx = 0;
    }
    let rc = test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err);
    assert_eq!(rc, ResultCode::OK as c_int);

    assert_eq!(table_infos.len(), 2);
    assert_eq!(table_infos[0].tbl_name, "foo");
    assert_eq!(table_infos[1].tbl_name, "boo");

    c.exec_safe("DROP TABLE foo").expect("dropped foo");
    c.exec_safe("DROP TABLE boo").expect("dropped boo");
    c.exec_safe("DROP TABLE boo__crsql_clock")
        .expect("dropped boo clock");
    c.exec_safe("DROP TABLE foo__crsql_clock")
        .expect("dropped foo clock");

    unsafe {
        (*ext_data).updatedTableInfosThisTx = 0;
    }
    let rc = test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err);
    assert_eq!(rc, ResultCode::OK as c_int);
    drop_err_ptr(err);

    assert_eq!(table_infos.len(), 0);

    unsafe {
        test_exports::c::crsql_freeExtData(ext_data);
    };
}

fn test_reinitializes_null_table_info_cache() {
    let db = crate::opendb().expect("Opened DB");
    let c = &db.db;
    let raw_db = db.db.db;
    c.exec_safe("CREATE TABLE foo (id PRIMARY KEY NOT NULL, value)")
        .expect("made foo");
    c.exec_safe(
        "CREATE TABLE foo__crsql_clock (key, col_name, col_version, db_version, site_id, seq, ts)",
    )
    .expect("made foo clock");

    let ext_data = unsafe { test_exports::c::crsql_newExtData(raw_db) };
    assert!(!ext_data.is_null(), "crsql_newExtData returned null");
    assert_eq!(
        unsafe {
            test_exports::c::crsql_initSiteIdExt(
                raw_db,
                ext_data,
                make_site() as *mut core::ffi::c_uchar,
            )
        },
        0
    );

    let err = make_err_ptr();
    assert_eq!(
        test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err),
        ResultCode::OK as c_int
    );
    unsafe { test_exports::tableinfo::crsql_drop_table_info_vec(ext_data) };
    assert!(unsafe { (*ext_data).tableInfos.is_null() });
    unsafe { (*ext_data).updatedTableInfosThisTx = 0 };

    // A remote merge can arrive through a connection whose cache has not been
    // initialized. The refresh path must recreate the empty cache instead of
    // returning a generic table-info error.
    assert_eq!(
        test_exports::tableinfo::crsql_ensure_table_infos_are_up_to_date(raw_db, ext_data, err),
        ResultCode::OK as c_int
    );
    let table_infos = unsafe {
        &*( (*ext_data).tableInfos as *const Vec<TableInfo>)
    };
    assert_eq!(table_infos.len(), 1);

    drop_err_ptr(err);
    unsafe { test_exports::c::crsql_freeExtData(ext_data) };
}

fn test_pull_table_info() {
    let db = crate::opendb().expect("Opened DB");
    let c = &db.db;
    let raw_db = db.db.db;
    let err = make_err_ptr();
    // test that we receive the expected values in column info and such.
    // pks are ordered
    // pks and non pks split
    // cids filled

    c.exec_safe(
        "CREATE TABLE foo (a INTEGER PRIMARY KEY NOT NULL, b TEXT NOT NULL, c NUMBER, d FLOAT, e);",
    )
    .expect("made foo");

    let tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "foo", err)
        .expect("pulled table info for foo");
    assert_eq!(tbl_info.pks.len(), 1);
    assert_eq!(tbl_info.pks[0].name, "a");
    assert_eq!(tbl_info.pks[0].cid, 0);
    assert_eq!(tbl_info.pks[0].pk, 1);
    assert_eq!(tbl_info.non_pks.len(), 4);
    assert_eq!(tbl_info.non_pks[0].name, "b");
    assert_eq!(tbl_info.non_pks[0].cid, 1);
    assert_eq!(tbl_info.non_pks[1].name, "c");
    assert_eq!(tbl_info.non_pks[1].cid, 2);
    assert_eq!(tbl_info.non_pks[2].name, "d");
    assert_eq!(tbl_info.non_pks[2].cid, 3);
    assert_eq!(tbl_info.non_pks[3].name, "e");
    assert_eq!(tbl_info.non_pks[3].cid, 4);

    c.exec_safe("CREATE TABLE boo (a INTEGER, b TEXT NOT NULL, c NUMBER NOT NULL, d FLOAT NOT NULL, e NOT NULL, PRIMARY KEY(b, c, d, e));")
        .expect("made boo");
    let tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "boo", err)
        .expect("pulled table info for boo");
    assert_eq!(tbl_info.pks.len(), 4);
    assert_eq!(tbl_info.pks[0].name, "b");
    assert_eq!(tbl_info.pks[0].cid, 1);
    assert_eq!(tbl_info.pks[0].pk, 1);
    assert_eq!(tbl_info.pks[1].name, "c");
    assert_eq!(tbl_info.pks[1].cid, 2);
    assert_eq!(tbl_info.pks[1].pk, 2);
    assert_eq!(tbl_info.pks[2].name, "d");
    assert_eq!(tbl_info.pks[2].cid, 3);
    assert_eq!(tbl_info.pks[2].pk, 3);
    assert_eq!(tbl_info.pks[3].name, "e");
    assert_eq!(tbl_info.pks[3].cid, 4);
    assert_eq!(tbl_info.pks[3].pk, 4);
    assert_eq!(tbl_info.non_pks.len(), 1);
    assert_eq!(tbl_info.non_pks[0].name, "a");
    assert_eq!(tbl_info.non_pks[0].cid, 0);
    assert_eq!(tbl_info.non_pks[0].pk, 0);
    drop_err_ptr(err);
}

fn test_is_table_compatible() {
    let db = crate::opendb().expect("Opened DB");
    let c = &db.db;
    let raw_db = db.db.db;
    let err = make_err_ptr();
    // convert the commented out test below into a format that resembles the tests above
    // and then run it

    // no pks
    c.exec_safe("CREATE TABLE foo (a);").expect("made foo");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "foo", err)
        .expect("checked if foo is compatible");
    assert_eq!(is_compatible, false);

    // pks
    c.exec_safe("CREATE TABLE bar (a PRIMARY KEY NOT NULL);")
        .expect("made bar");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "bar", err)
        .expect("checked if bar is compatible");
    assert_eq!(is_compatible, true);

    // nullable pks
    c.exec_safe("CREATE TABLE bal (a PRIMARY KEY);")
        .expect("made bal");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "bal", err)
        .expect("checked if bal is compatible");
    assert_eq!(is_compatible, false);

    // nullable composite pks
    c.exec_safe("CREATE TABLE baf (a NOT NULL, b, PRIMARY KEY(a, b));")
        .expect("made baf");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "baf", err)
        .expect("checked if baf is compatible");
    assert_eq!(is_compatible, false);

    // pks + other non unique indices
    c.exec_safe("CREATE TABLE baz (a PRIMARY KEY NOT NULL, b);")
        .expect("made baz");
    c.exec_safe("CREATE INDEX bar_i ON baz (b);")
        .expect("made index");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "baz", err)
        .expect("checked if baz is compatible");
    assert_eq!(is_compatible, true);

    // pks + other unique indices
    c.exec_safe("CREATE TABLE booz (a PRIMARY KEY NOT NULL, b);")
        .expect("made booz");
    c.exec_safe("CREATE UNIQUE INDEX booz_b ON booz (b);")
        .expect("made index");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "booz", err)
        .expect("checked if booz is compatible");
    assert_eq!(is_compatible, false);

    // not null and no dflt
    c.exec_safe("CREATE TABLE buzz (a PRIMARY KEY NOT NULL, b NOT NULL);")
        .expect("made buzz");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "buzz", err)
        .expect("checked if buzz is compatible");
    assert_eq!(is_compatible, false);

    // not null and dflt
    c.exec_safe("CREATE TABLE boom (a PRIMARY KEY NOT NULL, b NOT NULL DEFAULT 1);")
        .expect("made boom");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "boom", err)
        .expect("checked if boom is compatible");
    assert_eq!(is_compatible, true);

    // fk constraint
    c.exec_safe("CREATE TABLE zoom (a PRIMARY KEY NOT NULL, b, FOREIGN KEY(b) REFERENCES foo(a));")
        .expect("made zoom");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "zoom", err)
        .expect("checked if zoom is compatible");
    assert_eq!(is_compatible, false);

    // strict mode should be ok
    c.exec_safe("CREATE TABLE atable (\"id\" TEXT PRIMARY KEY) STRICT;")
        .expect("made atable");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "atable", err)
        .expect("checked if atable is compatible");
    assert_eq!(is_compatible, true);

    // no autoincrement
    c.exec_safe("CREATE TABLE woom (a integer primary key autoincrement not null);")
        .expect("made woom");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "woom", err)
        .expect("checked if woom is compatible");
    assert_eq!(is_compatible, false);

    // aliased rowid
    c.exec_safe("CREATE TABLE loom (a integer primary key not null);")
        .expect("made loom");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "loom", err)
        .expect("checked if loom is compatible");
    assert_eq!(is_compatible, true);

    c.exec_safe("CREATE TABLE atable2 (\"id\" TEXT PRIMARY KEY NOT NULL, x TEXT) STRICT;")
        .expect("made atable2");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "atable2", err)
        .expect("checked if atable2 is compatible");
    assert_eq!(is_compatible, true);

    c.exec_safe(
        "CREATE TABLE ydoc (\
        doc_id TEXT NOT NULL,\
        yhash BLOB NOT NULL,\
        yval BLOB,\
        primary key (doc_id, yhash)\
      ) STRICT;",
    )
    .expect("made ydoc");
    let is_compatible = test_exports::tableinfo::is_table_compatible(raw_db, "ydoc", err)
        .expect("checked if ydoc is compatible");
    assert_eq!(is_compatible, true);
    drop_err_ptr(err);
}

fn test_create_clock_table_from_table_info() {
    let db = crate::opendb().expect("Opened DB");
    let c = &db.db;
    let raw_db = db.db.db;
    let err = make_err_ptr();

    c.exec_safe("CREATE TABLE foo (a not null, b not null, primary key (a, b));")
        .expect("made foo");
    c.exec_safe("CREATE TABLE bar (a primary key not null);")
        .expect("made bar");
    c.exec_safe("CREATE TABLE baz (a primary key not null, b);")
        .expect("made baz");
    c.exec_safe("CREATE TABLE boo (a primary key not null, b, c);")
        .expect("made boo");

    let foo_tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "foo", err)
        .expect("pulled table info for foo");
    let bar_tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "bar", err)
        .expect("pulled table info for bar");
    let baz_tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "baz", err)
        .expect("pulled table info for baz");
    let boo_tbl_info = test_exports::tableinfo::pull_table_info(raw_db, "boo", err)
        .expect("pulled table info for boo");

    test_exports::bootstrap::create_clock_table(raw_db, &foo_tbl_info, err)
        .expect("created clock table for foo");
    test_exports::bootstrap::create_clock_table(raw_db, &bar_tbl_info, err)
        .expect("created clock table for bar");
    test_exports::bootstrap::create_clock_table(raw_db, &baz_tbl_info, err)
        .expect("created clock table for baz");
    test_exports::bootstrap::create_clock_table(raw_db, &boo_tbl_info, err)
        .expect("created clock table for boo");

    // Verify the clock table schema for each table.
    // The __crsql_clock table should have columns:
    // key, col_name, col_version, db_version, site_id, seq, ts
    // with PRIMARY KEY (key, col_name).
    for tbl in &["foo", "bar", "baz", "boo"] {
        assert_clock_table_schema(raw_db, tbl);
    }

    drop_err_ptr(err);
}

/// Assert that the `__crsql_clock` table for `tbl` has the expected columns
/// and primary key. Queries `pragma_table_info` and verifies the dbv index exists.
fn assert_clock_table_schema(db: *mut sqlite::sqlite3, tbl: &str) {
    let clock_tbl = format!("{}__crsql_clock", tbl);

    // Collect (name, type, notnull, pk) from pragma_table_info
    let stmt = db
        .prepare_v2(&format!(
            "SELECT \"name\", \"type\", \"notnull\", \"pk\" FROM pragma_table_info('{clock_tbl}') ORDER BY cid"
        ))
        .expect("prepared pragma_table_info for clock table");
    let mut cols: Vec<(String, String, i32, i32)> = vec![];
    let mut s = stmt;
    while s.step().expect("stepped pragma_table_info") == ResultCode::ROW {
        let name = s.column_text(0).expect("col name").to_string();
        let ty = s.column_text(1).expect("col type").to_string();
        let notnull = s.column_int(2);
        let pk = s.column_int(3);
        cols.push((name, ty, notnull, pk));
    }

    let expected_cols = [
        ("key", "INTEGER", 1, 1),
        ("col_name", "TEXT", 1, 2),
        ("col_version", "INTEGER", 1, 0),
        ("db_version", "INTEGER", 1, 0),
        ("site_id", "INTEGER", 1, 0),
        ("seq", "INTEGER", 1, 0),
        ("ts", "TEXT", 1, 0),
    ];
    assert_eq!(
        cols.len(),
        expected_cols.len(),
        "clock table {} has unexpected column count",
        clock_tbl
    );
    for (i, (name, ty, notnull, pk)) in expected_cols.iter().enumerate() {
        assert_eq!(&cols[i].0, name, "clock table {} column {} name", clock_tbl, i);
        assert_eq!(
            &cols[i].1, ty,
            "clock table {} column {} type",
            clock_tbl,
            name
        );
        assert_eq!(
            cols[i].2, *notnull,
            "clock table {} column {} notnull",
            clock_tbl,
            name
        );
        assert_eq!(
            cols[i].3, *pk,
            "clock table {} column {} pk",
            clock_tbl,
            name
        );
    }

    // The table should have a dbv index (site_id, db_version).
    let idx_stmt = db
        .prepare_v2(&format!(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='{tbl}__crsql_clock_dbv_idx'"
        ))
        .expect("prepared index count query");
    let mut idx_s = idx_stmt;
    assert_eq!(
        idx_s.step().expect("stepped index count"),
        ResultCode::ROW
    );
    let idx_count = idx_s.column_int(0);
    assert_eq!(
        idx_count, 1,
        "clock table {} should have a dbv index",
        clock_tbl
    );
}

fn test_leak_condition() {
    // updating schemas prepares stements
    // re-pulling table infos should finalize those statements
    let c1w = crate::opendb_file("test_leak_condition").expect("Opened DB");
    let c2w = crate::opendb_file("test_leak_condition").expect("Opened DB");

    let c1 = &c1w.db;
    let c2 = &c2w.db;

    c1.exec_safe(
        "DROP TABLE IF EXISTS foo;
        DROP TABLE IF EXISTS bar;
        VACUUM;",
    )
    .expect("reset db");

    c1.exec_safe("CREATE TABLE foo (a not null, b not null, primary key (a, b));")
        .expect("made foo");
    c1.exec_safe("SELECT crsql_set_ts('1700000000')").expect("set ts");
    c1.exec_safe("SELECT crsql_as_crr('foo')")
        .expect("made foo a crr");
    c1.exec_safe("INSERT INTO foo VALUES (1, 2)")
        .expect("inserted into foo");
    c1.exec_safe("UPDATE FOO set b = 3").expect("updated foo");
    c2.exec_safe("INSERT INTO foo VALUES (2, 3)")
        .expect("inserted into foo");
    c2.exec_safe("CREATE TABLE bar (a)").expect("created bar");
    c1.exec_safe("INSERT INTO foo VALUES (3, 4)")
        .expect("inserted into foo");
    c2.exec_safe("INSERT INTO foo VALUES (4, 5)")
        .expect("inserted into foo");

    // Assert the resulting state: row counts and clock entries.
    // c1 inserted (1,2) then updated to (1,3), then inserted (3,4).
    // c2 inserted (2,3) and (4,5). All share the same file-based DB.
    let raw_db = c1w.db.db;
    let foo_count = count_rows(raw_db, "foo");
    assert_eq!(
        foo_count, 4,
        "foo should have 4 rows after inserts, got {}",
        foo_count
    );

    // The clock table should have entries for the CRR columns.
    let clock_count = count_rows(raw_db, "foo__crsql_clock");
    assert!(
        clock_count > 0,
        "foo__crsql_clock should have clock entries, got {}",
        clock_count
    );

    // bar was created but is not a CRR — it should exist with 0 rows.
    let bar_count = count_rows(raw_db, "bar");
    assert_eq!(bar_count, 0, "bar should have 0 rows, got {}", bar_count);
}

/// Helper: count rows in a table via SELECT count(*).
fn count_rows(db: *mut sqlite::sqlite3, table: &str) -> i32 {
    let stmt = db
        .prepare_v2(&format!("SELECT count(*) FROM {}", table))
        .expect("prepared count query");
    let mut s = stmt;
    assert_eq!(
        s.step().expect("stepped count query"),
        ResultCode::ROW
    );
    s.column_int(0)
}

fn test_site_id_initialization() {
    // Use a file-based DB so state persists across open/close cycles.
    // Each block opens the same file, so DELETE/DROP of crsql_site_id
    // in one block is visible to the next, actually testing re-initialization.
    // (With :memory: each block gets a fresh DB, so the DELETE/DROP has no
    // effect on subsequent blocks.)
    let db_file = "test_site_id_initialization";

    // Clean up any leftover state from a previous test run.
    {
        let db = crate::opendb_file(db_file).expect("Opened DB for cleanup");
        let raw_db = db.db.db;
        raw_db
            .exec_safe("DROP TABLE IF EXISTS crsql_site_id;")
            .expect("dropped crsql_site_id for cleanup");
    }

    // Block 1: site_id should be initialized on first open. Delete it.
    {
        let db = crate::opendb_file(db_file).expect("Opened DB");
        let raw_db = db.db.db;
        let site_id = select_site_id(raw_db).expect("selected site id");
        assert_eq!(site_id.len(), 16);
        raw_db
            .exec_safe("DELETE FROM crsql_site_id;")
            .expect("deleted site id");
    }

    // Block 2: site_id should be re-initialized after the DELETE.
    {
        let db = crate::opendb_file(db_file).expect("Opened DB");
        let raw_db = db.db.db;
        let site_id = select_site_id(raw_db).expect("selected site id");
        assert_eq!(site_id.len(), 16);
        raw_db
            .exec_safe("DROP TABLE crsql_site_id;")
            .expect("dropped crsql_site_id");
    }

    // Block 3: site_id should be re-initialized after the DROP TABLE.
    {
        let db = crate::opendb_file(db_file).expect("Opened DB");
        let raw_db = db.db.db;
        let site_id = select_site_id(raw_db).expect("selected site id");
        assert_eq!(site_id.len(), 16);
    }
}

fn select_site_id(db: *mut sqlite::sqlite3) -> Result<Vec<u8>, ResultCode> {
    let site_id_stmt = db.prepare_v2("SELECT crsql_site_id()")?;
    site_id_stmt.step()?;
    let site_id = site_id_stmt.column_blob(0)?.to_vec();
    Ok(site_id)
}

pub fn run_suite() {
    libc_print::libc_println!("Running tableinfo suite");
    test_ensure_table_infos_are_up_to_date();
    test_reinitializes_null_table_info_cache();
    test_pull_table_info();
    test_is_table_compatible();
    test_create_clock_table_from_table_info();
    test_leak_condition();
    test_site_id_initialization();
    test_integer_pk_case_insensitive();
}

/// H5 regression: SQLite treats INTEGER PRIMARY KEY as a rowid alias
/// case-insensitively. Our code must match — `integer PRIMARY KEY`
/// (lowercase) must be classified as a rowid-keyed table.
fn test_integer_pk_case_insensitive() {
    let db = crate::opendb().expect("Opened DB");
    let raw_db = db.db.db;
    // lowercase
    db.db.exec_safe("CREATE TABLE lower_int (id integer PRIMARY KEY NOT NULL, a)")
        .expect("created lower_int");
    // mixed case
    db.db.exec_safe("CREATE TABLE mixed_int (id Integer PRIMARY KEY NOT NULL, a)")
        .expect("created mixed_int");
    // uppercase (control)
    db.db.exec_safe("CREATE TABLE upper_int (id INTEGER PRIMARY KEY NOT NULL, a)")
        .expect("created upper_int");

    let err = make_err_ptr();
    // pull_table_info should classify all three as having an integer PK
    // (rowid alias), regardless of case.
    for tbl in &["lower_int", "mixed_int", "upper_int"] {
        let ti = test_exports::tableinfo::pull_table_info(raw_db, tbl, err);
        assert!(ti.is_ok(), "pull_table_info failed for {}: {:?}", tbl, ti.err());
        let ti = ti.unwrap();
        assert!(ti.has_integer_pk,
            "table {} should have has_integer_pk=true (case-insensitive INTEGER), got col_type={:?}",
            tbl, ti.pks[0].col_type);
        assert!(!ti.rowid_alias.is_empty(),
            "table {} should have a non-empty rowid_alias", tbl);
    }
    drop_err_ptr(err);
    libc_print::libc_println!("=== test_integer_pk_case_insensitive PASS ===");
}
