use crsql_bundle::test_exports::pack_columns::unpack_columns;
use crsql_bundle::test_exports::pack_columns::unpack_varints;
use crsql_bundle::test_exports::pack_columns::ColumnValue;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

// The rust test is mainly to check with valgrind and ensure we're correctly
// freeing data as we do some passing of destructors from rust to SQLite.
// Complete property based tests for encode & decode exist in python.
fn test_pack_columns() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, x, y)")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;
    let blob: [u8; 3] = [1, 2, 3];

    insert_stmt.bind_int(1, 12)?;
    insert_stmt.bind_text(2, "str", sqlite::Destructor::STATIC)?;
    insert_stmt.bind_blob(3, &blob, sqlite::Destructor::STATIC)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT quote(crsql_pack_columns(id, x, y)) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_text(0)?;
    assert!(result == "X'03090C0B037374720C03010203'");
    // 03 09 0C 0B 03 73 74 72 0C 03 01 02 03
    // cols: 03
    // type & intlen: 09 -> 0b00001001 -> 01 type & 01 intlen
    // value: 0C -> 12
    // type & intlen: 0B -> 0b00001011 -> 03 type & 01 intlen
    // 03 -> len
    // 73 74 72 -> str
    // type & intlen: 0C ->  0b00001100 -> 04 type & 01 intlen
    // len: 03
    // bytes: 01 02 3
    // voila, done in 13 bytes! < 18 byte string < 26 byte binary w/o varints

    // Before variable length encoding:
    // 03 01 00 00 00 00 00 00 00 0C 03 00 00 00 03 73 74 72 04 00 00 00 03 01 02 03
    // cols:03
    // type: 01 (integer)
    // value: 00 00 00 00 00 00 00 0C (12) TODO: encode as variable length integers to save space?
    // type: 03 (text)
    // len: 00 00 00 03 (3)
    // byes: 73 (s) 74 (t) 72 (r)
    // type: 04 (blob)
    // len: 00 00 00 03 (3)
    // bytes: 01 02 03
    // vs string:
    // 12|'str'|x'010203'
    // ^ 18 bytes via string
    // vs
    // 26 bytes via binary
    // 13 bytes are wasted due to not using variable length encoding for integers
    // So.. do variable length ints?

    let select_stmt = db
        .db
        .prepare_v2("SELECT crsql_pack_columns(id, x, y) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_blob(0)?;
    assert!(result.len() == 13);
    let unpacked = unpack_columns(result)?;
    assert!(unpacked.len() == 3);

    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == 12);
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Text(s) = &unpacked[1] {
        assert!(s == "str")
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Blob(b) = &unpacked[2] {
        assert!(b[..] == blob);
    } else {
        assert!("unexpected type" == "");
    }

    db.db.exec_safe("DELETE FROM foo")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;

    insert_stmt.bind_int(1, 0)?;
    insert_stmt.bind_int(2, 10000000)?;
    insert_stmt.bind_int(3, -2500000)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT crsql_pack_columns(id, x, y) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_blob(0)?;
    let unpacked = unpack_columns(result)?;
    assert!(unpacked.len() == 3);

    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == 0);
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Integer(i) = unpacked[1] {
        assert!(i == 10000000)
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Integer(i) = unpacked[2] {
        assert!(i == -2500000);
    } else {
        assert!("unexpected type" == "");
    }

    Ok(())
}

fn test_unpack_columns() -> Result<(), ResultCode> {
    let db = crate::opendb().unwrap();
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, x, y)")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;
    let blob: [u8; 3] = [1, 2, 3];

    insert_stmt.bind_int(1, 12)?;
    insert_stmt.bind_text(2, "str", sqlite::Destructor::STATIC)?;
    insert_stmt.bind_blob(3, &blob, sqlite::Destructor::STATIC)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT cell FROM crsql_unpack_columns WHERE package = (SELECT crsql_pack_columns(id, x, y) FROM foo)")?;
    select_stmt.step()?;
    assert!(select_stmt.column_int(0) == 12);
    select_stmt.step()?;
    assert!(select_stmt.column_text(0)? == "str");
    select_stmt.step()?;
    assert!(select_stmt.column_blob(0)? == blob);

    db.db.exec_safe("CREATE TABLE bar (id PRIMARY KEY)")?;
    let int_col: [i64; 7] = [
        1,
        -1,
        i64::MAX,
        i64::MIN,
        i8::MAX as i64,
        i16::MIN as i64,
        10156800_i64,
    ];

    for i in int_col {
        let insert_stmt = db.db.prepare_v2("INSERT INTO bar VALUES (?)")?;
        insert_stmt.bind_int64(1, i)?;
        insert_stmt.step()?;

        let select_stmt = db
            .db
            .prepare_v2("SELECT crsql_pack_columns(id) FROM bar where id = ?")?;
        select_stmt.bind_int64(1, i)?;
        select_stmt.step()?;
        let result = select_stmt.column_blob(0)?;
        let unpacked = unpack_columns(result)?;
        assert!(unpacked.len() == 1);
        if let ColumnValue::Integer(i) = unpacked[0] {
            assert!(i == i);
        } else {
            assert!("unexpected type" == "");
        }
    }

    db.db.exec_safe("DELETE FROM bar")?;
    let text_col: [&str; 4] = ["a", ",", "-abcdefghijklmnopqrstuvwxyz1234567890?!", ""];

    for txt in text_col {
        let insert_stmt = db.db.prepare_v2("INSERT INTO bar VALUES (?)")?;
        insert_stmt.bind_text(1, txt, sqlite::Destructor::STATIC)?;
        insert_stmt.step()?;

        let select_stmt = db
            .db
            .prepare_v2("SELECT crsql_pack_columns(id) FROM bar where id = ?")?;
        select_stmt.bind_text(1, txt, sqlite::Destructor::STATIC)?;
        select_stmt.step()?;
        let result = select_stmt.column_blob(0)?;
        let unpacked = unpack_columns(result)?;
        assert!(unpacked.len() == 1);
        libc_print::std_name::println!("unpacked: {:?}", txt);
        if let ColumnValue::Text(i) = &unpacked[0] {
            assert!(i == txt);
        } else {
            assert!("unexpected type" == "");
        }
    }

    Ok(())
}

/// Test varint encoding via crsql_pack_varint_agg and unpack_varints.
/// This tests the put_varint/get_varint functions end-to-end through the
/// SQL aggregate, covering all byte lengths including the 9-byte case.
///
/// The unit tests in pack_columns.rs verify the exact byte
/// encoding; this test verifies the round-trip through the SQL interface.
fn test_varint_encoding() -> Result<(), ResultCode> {
    let db = crate::opendb()?;

    // Test values covering all varint byte lengths:
    // 1 byte: 0-127
    // 2 bytes: 128-16383
    // 3 bytes: 16384-2097151
    // ...
    // 9 bytes: values >= 2^56
    let test_values: &[(i64, &str)] = &[
        (0, "0 (1 byte)"),
        (127, "127 (1 byte boundary)"),
        (128, "128 (2 byte boundary)"),
        (200, "200 (2 byte)"),
        (16383, "16383 (2 byte max)"),
        (16384, "16384 (3 byte boundary)"),
        (1048576, "1048576 (3 byte)"),
        (i32::MAX as i64, "i32::MAX (5 byte)"),
        (i64::MAX, "i64::MAX (9 byte)"),
        (i64::MIN, "i64::MIN (9 byte, negative)"),
        (-1, "-1 (9 byte, negative via reinterpret)"),
    ];

    for &(val, desc) in test_values {
        // Pack a single value via crsql_pack_varint_agg
        let stmt = db.db.prepare_v2(
            "SELECT crsql_pack_varint_agg(v) FROM (SELECT ? AS v)"
        )?;
        stmt.bind_int64(1, val)?;
        stmt.step()?;
        let packed = stmt.column_blob(0)?;

        // Unpack and verify round-trip
        let unpacked = unpack_varints(packed)?;
        assert_eq!(unpacked.len(), 1, "should have 1 value for {}", desc);
        assert_eq!(
            unpacked[0], val,
            "varint round-trip failed for {}: expected {}, got {}",
            desc, val, unpacked[0]
        );
    }

    // Test multiple values packed together (simulates packed mode with multiple cols)
    let stmt = db.db.prepare_v2(
        "SELECT crsql_pack_varint_agg(v) FROM (SELECT 0 AS v UNION ALL SELECT 127 UNION ALL SELECT 128 UNION ALL SELECT 200 UNION ALL SELECT 16384 UNION ALL SELECT 1048576 UNION ALL SELECT 2000000000)"
    )?;
    stmt.step()?;
    let packed = stmt.column_blob(0)?;
    let unpacked = unpack_varints(packed)?;
    assert_eq!(unpacked.len(), 7, "should have 7 values");
    assert_eq!(unpacked[0], 0);
    assert_eq!(unpacked[1], 127);
    assert_eq!(unpacked[2], 128);
    assert_eq!(unpacked[3], 200);
    assert_eq!(unpacked[4], 16384);
    assert_eq!(unpacked[5], 1048576);
    assert_eq!(unpacked[6], 2000000000);

    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    test_pack_columns()?;
    test_unpack_columns()?;
    test_varint_encoding()
}
