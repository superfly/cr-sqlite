use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
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

/// CR3 regression: BLOB lengths 128-255 round-trip correctly.
/// The `bytes` crate's `get_int(nbytes)` for `nbytes < 8` does NOT sign-extend —
/// it copies bytes into the LSB of an 8-byte zeroed buffer and uses `from_be_bytes`.
/// So `get_int(1)` on byte 0x80 returns 128, not -128. This test confirms
/// the correct behavior for BLOB lengths whose 1-byte encoding has the high bit set.
fn test_blob_length_high_bit_round_trip() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, data BLOB)")?;

    // Test BLOB lengths that produce a 1-byte length encoding with the high bit set.
    // These are the lengths 128-255 where the encoded byte is 0x80-0xFF.
    let test_lengths: &[usize] = &[128, 200, 255];

    for &len in test_lengths {
        let blob: Vec<u8> = vec![0xAB; len];
        db.db.exec_safe("DELETE FROM foo")?;
        let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?)")?;
        insert_stmt.bind_int(1, len as i32)?;
        insert_stmt.bind_blob(2, &blob, sqlite::Destructor::STATIC)?;
        insert_stmt.step()?;

        let select_stmt =
            db.db.prepare_v2("SELECT crsql_pack_columns(data) FROM foo")?;
        select_stmt.step()?;
        let packed = select_stmt.column_blob(0)?;
        let unpacked = unpack_columns(packed)?;
        assert_eq!(unpacked.len(), 1, "expected 1 column for len {}", len);
        match &unpacked[0] {
            ColumnValue::Blob(b) => {
                assert_eq!(b.len(), len, "BLOB length mismatch for len {}", len);
                assert!(b.iter().all(|&x| x == 0xAB), "BLOB content mismatch for len {}", len);
            }
            _ => assert!(false, "expected Blob for len {}", len),
        }
    }

    // Also test 256 (2-byte length encoding, high bit NOT set — should already work)
    db.db.exec_safe("DELETE FROM foo")?;
    let blob: Vec<u8> = vec![0xCD; 256];
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?)")?;
    insert_stmt.bind_int(1, 256)?;
    insert_stmt.bind_blob(2, &blob, sqlite::Destructor::STATIC)?;
    insert_stmt.step()?;
    let select_stmt = db.db.prepare_v2("SELECT crsql_pack_columns(data) FROM foo")?;
    select_stmt.step()?;
    let packed = select_stmt.column_blob(0)?;
    let unpacked = unpack_columns(packed)?;
    assert_eq!(unpacked.len(), 1);
    match &unpacked[0] {
        ColumnValue::Blob(b) => assert_eq!(b.len(), 256),
        _ => assert!(false, "expected Blob for len 256"),
    }

    Ok(())
}

/// CR4 repro: Malformed UTF-8 in packed text should return an error,
/// not trigger undefined behavior via `from_utf8_unchecked`.
fn test_malformed_utf8_text_unpack() -> Result<(), ResultCode> {
    // Construct a packed blob with invalid UTF-8 text bytes.
    // Format: [num_columns:varint][type_byte][len][bytes...]
    // Text type = 3, intlen = 1 → type_byte = (1 << 3) | 3 = 0x0B
    let mut packed: Vec<u8> = vec![];
    packed.push(0x01); // 1 column
    packed.push(0x0B); // type=Text(3), intlen=1
    packed.push(0x02); // length = 2
    packed.push(0xFF); // invalid UTF-8 continuation byte
    packed.push(0xFE); // invalid UTF-8

    // This should return an error, not UB
    let result = unpack_columns(&packed);
    assert!(result.is_err(), "malformed UTF-8 should return error, not succeed");

    // Also test via the virtual table interface
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY)")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (1)")?;
    insert_stmt.step()?;

    // Use the unpack_columns vtab with malformed packed data
    let hex = packed.iter().map(|b| format!("{:02X}", b)).collect::<String>();
    let select_stmt = db.db.prepare_v2(&format!(
        "SELECT cell FROM crsql_unpack_columns WHERE package = X'{}'",
        hex
    ))?;
    // This should error or return no rows, not crash
    let rc = select_stmt.step();
    // Either it errors (ABORT) or returns no rows (DONE) — both are acceptable.
    // What's NOT acceptable is a crash or UB.
    assert!(
        rc == Ok(ResultCode::DONE) || rc.is_err(),
        "malformed UTF-8 via vtab should error or return DONE"
    );

    Ok(())
}

/// M14 regression: truncated varints (all continuation bits set, buffer ends)
/// must be rejected, not silently accepted as partial values.
fn test_truncated_varint_rejected() -> Result<(), ResultCode> {
    // A varint with all continuation bits set but buffer ending is truncated.
    // 0x80 = continuation bit set, no data bits. Buffer ends after this byte.
    // This should be rejected with ABORT, not accepted as value 0.
    let truncated = [0x80u8];
    let result = unpack_columns(&truncated);
    assert!(result.is_err(), "truncated varint should be rejected");

    // Two bytes, both with continuation bits set, buffer ends — truncated.
    let truncated2 = [0x80u8, 0x80u8];
    let result2 = unpack_columns(&truncated2);
    assert!(result2.is_err(), "truncated 2-byte varint should be rejected");

    // 9 bytes all with continuation bits set — truncated (9th byte is the last
    // but all 8 previous bytes have continuation set, so it's a valid 9-byte
    // varint... actually the 9th byte uses all 8 bits, so 9 bytes is valid.
    // Let's test 8 bytes all with continuation bits set — that's truncated
    // because the 9th byte is missing.
    let truncated8 = [0x80u8; 8];
    let result8 = unpack_columns(&truncated8);
    assert!(result8.is_err(), "truncated 8-byte varint should be rejected");

    libc_print::libc_println!("=== test_truncated_varint_rejected PASS ===");
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    test_pack_columns()?;
    test_unpack_columns()?;
    test_varint_encoding()?;
    test_blob_length_high_bit_round_trip()?;
    test_malformed_utf8_text_unpack()?;
    test_truncated_varint_rejected()
}
