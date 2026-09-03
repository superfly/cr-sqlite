extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bytes::{Buf, BufMut};
#[cfg(not(feature = "std"))]
use num_traits::FromPrimitive;
use sqlite_nostd as sqlite;
use sqlite_nostd::{aggregate_context, ColumnType, Context, ResultCode, Stmt, Value};
use core::ffi::c_int;

pub extern "C" fn crsql_pack_columns(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    match pack_columns(args) {
        Err(code) => {
            ctx.result_error("Failed to pack columns");
            ctx.result_error_code(code);
        }
        Ok(blob) => {
            // TODO: pass a destructor so we don't have to copy the blob
            ctx.result_blob_owned(blob);
        }
    }
}

pub fn pack_columns(args: &[*mut sqlite::value]) -> Result<Vec<u8>, ResultCode> {
    let mut buf = vec![];
    /*
     * Format:
     * [num_columns:u8,...[(type(0-3),num_bytes?(3-7)):u8, length?:i32, ...bytes:u8[]]]
     *
     * The byte used for column type also encodes the number of bytes used for the integer.
     * e.g.: (type(0-3),num_bytes?(3-7)):u8
     * first 3 bits are type
     * last 5 encode how long the following integer, if there is a following integer, is. 1, 2, 3, ... 8 bytes.
     *
     * Not packing an integer into the minimal number of bytes required is rather wasteful.
     * E.g., the number `0` would take 8 bytes rather than 1 byte.
     */
    let len_result: Result<u64, _> = args.len().try_into();
    if let Ok(len) = len_result {
        put_varint(&mut buf, len);
        for value in args {
            match value.value_type() {
                ColumnType::Blob => {
                    let len = value.bytes();
                    let num_bytes_for_len = num_bytes_needed_i32(len);
                    let type_byte = num_bytes_for_len << 3 | (ColumnType::Blob as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(len as i64, num_bytes_for_len as usize);
                    buf.put_slice(value.blob());
                }
                ColumnType::Null => {
                    buf.put_u8(ColumnType::Null as u8);
                }
                ColumnType::Float => {
                    buf.put_u8(ColumnType::Float as u8);
                    buf.put_f64(value.double());
                }
                ColumnType::Integer => {
                    let val = value.int64();
                    let num_bytes_for_int = num_bytes_needed_i64(val);
                    let type_byte = num_bytes_for_int << 3 | (ColumnType::Integer as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(val, num_bytes_for_int as usize);
                }
                ColumnType::Text => {
                    let len = value.bytes();
                    let num_bytes_for_len = num_bytes_needed_i32(len);
                    let type_byte = num_bytes_for_len << 3 | (ColumnType::Text as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(len as i64, num_bytes_for_len as usize);
                    buf.put_slice(value.blob());
                }
            }
        }
        Ok(buf)
    } else {
        Err(ResultCode::ABORT)
    }
}

/// Pack a slice of ColumnValue into the same wire format as pack_columns.
/// Used by V2 code paths that work with unpacked values.
pub fn pack_column_values(values: &[ColumnValue]) -> Result<Vec<u8>, ResultCode> {
    let mut buf = vec![];
    let len_result: Result<u64, _> = values.len().try_into();
    if let Ok(len) = len_result {
        put_varint(&mut buf, len);
        for value in values {
            match value {
                ColumnValue::Blob(b) => {
                    let len = b.len() as i32;
                    let num_bytes_for_len = num_bytes_needed_i32(len);
                    let type_byte = num_bytes_for_len << 3 | (ColumnType::Blob as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(len as i64, num_bytes_for_len as usize);
                    buf.put_slice(b);
                }
                ColumnValue::Null => {
                    buf.put_u8(ColumnType::Null as u8);
                }
                ColumnValue::Float(f) => {
                    buf.put_u8(ColumnType::Float as u8);
                    buf.put_f64(*f);
                }
                ColumnValue::Integer(val) => {
                    let num_bytes_for_int = num_bytes_needed_i64(*val);
                    let type_byte = num_bytes_for_int << 3 | (ColumnType::Integer as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(*val, num_bytes_for_int as usize);
                }
                ColumnValue::Text(t) => {
                    let len = t.len() as i32;
                    let num_bytes_for_len = num_bytes_needed_i32(len);
                    let type_byte = num_bytes_for_len << 3 | (ColumnType::Text as u8);
                    buf.put_u8(type_byte);
                    buf.put_int(len as i64, num_bytes_for_len as usize);
                    buf.put_slice(t.as_bytes());
                }
            }
        }
        Ok(buf)
    } else {
        Err(ResultCode::ABORT)
    }
}

fn num_bytes_needed_i32(val: i32) -> u8 {
    if val & 0xFF000000u32 as i32 != 0 {
        return 4;
    } else if val & 0x00FF0000 != 0 {
        return 3;
    } else if val & 0x0000FF00 != 0 {
        return 2;
    } else if val * 0x000000FF != 0 {
        return 1;
    } else {
        return 0;
    }
}

fn num_bytes_needed_i64(val: i64) -> u8 {
    if val & 0xFF00000000000000u64 as i64 != 0 {
        return 8;
    } else if val & 0x00FF000000000000 != 0 {
        return 7;
    } else if val & 0x0000FF0000000000 != 0 {
        return 6;
    } else if val & 0x000000FF00000000 != 0 {
        return 5;
    } else {
        return num_bytes_needed_i32(val as i32);
    }
}

#[derive(Clone)]
pub enum ColumnValue {
    Blob(Vec<u8>),
    Float(f64),
    Integer(i64),
    Null,
    Text(String),
}

/// Encode a value as a SQLite varint into the buffer.
/// Values 0-127 encode as a single byte (0x00-0x7F), byte-identical
/// to the old u8 format. Larger values use multi-byte encoding.
///
/// SQLite varint format (MSB-first / big-endian):
///   1-8 bytes: each byte has 7 data bits (high bit = continuation).
///             The first byte has the most significant bits.
///             The last byte has the least significant 7 bits (no continuation).
///   9 bytes:  p[0..7] have 7 data bits each (all with continuation bit set),
///             p[8] has 8 data bits (the least significant byte, no continuation).
/// Total capacity: 7*8 + 8 = 64 bits.
///
/// This matches SQLite's sqlite3PutVarint exactly.
fn put_varint(buf: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        buf.put_u8(value as u8);
        return;
    }

    // 9-byte case (value >= 2^56): p[8] = lowest 8 bits,
    // p[0..7] = 7-bit groups from the remaining bits, all with continuation.
    if value & (0xff000000u64 << 32) != 0 {
        let mut v = value >> 8;
        let mut bytes = [0u8; 9];
        bytes[8] = (value & 0xFF) as u8;
        for i in (0..8).rev() {
            bytes[i] = ((v & 0x7F) as u8) | 0x80;
            v >>= 7;
        }
        buf.extend_from_slice(&bytes);
        return;
    }

    // 2-8 byte case: extract 7-bit groups LSB-first into a stack buffer,
    // clear the continuation bit on the LSB, reverse in place, bulk-write.
    let mut tmp = [0u8; 8];
    let mut n = 0;
    let mut v = value;
    loop {
        tmp[n] = ((v & 0x7F) as u8) | 0x80;
        v >>= 7;
        n += 1;
        if v == 0 {
            break;
        }
    }
    tmp[0] &= 0x7F;
    tmp[..n].reverse();
    buf.extend_from_slice(&tmp[..n]);
}

/// Read a SQLite varint from the buffer. Returns the value and number of bytes consumed.
fn get_varint(buf: &[u8]) -> Result<(u64, usize), ResultCode> {
    if buf.is_empty() {
        return Err(ResultCode::ABORT);
    }
    let mut result: u64 = 0;
    let mut i = 0;
    while i < buf.len() && i < 9 {
        let byte = buf[i];
        if i == 8 {
            // 9th byte uses all 8 bits
            result = (result << 8) | byte as u64;
            i += 1;
            break;
        }
        result = (result << 7) | (byte & 0x7F) as u64;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
    }
    if i > buf.len() {
        return Err(ResultCode::ABORT);
    }
    Ok((result, i))
}

// TODO: make a table valued function that can be used to extract a row per packed column?
pub fn unpack_columns(data: &[u8]) -> Result<Vec<ColumnValue>, ResultCode> {
    let mut ret = vec![];
    let (num_columns, varint_len) = get_varint(data)?;
    let mut buf = &data[varint_len..];
    let num_columns = num_columns as usize;

    for _i in 0..num_columns {
        if !buf.has_remaining() {
            return Err(ResultCode::ABORT);
        }
        let column_type_and_maybe_intlen = buf.get_u8();
        let column_type = ColumnType::from_u8(column_type_and_maybe_intlen & 0x07);
        let intlen = (column_type_and_maybe_intlen >> 3 & 0xFF) as usize;

        match column_type {
            Some(ColumnType::Blob) => {
                if buf.remaining() < intlen {
                    return Err(ResultCode::ABORT);
                }
                let len = buf.get_int(intlen) as usize;
                if buf.remaining() < len {
                    return Err(ResultCode::ABORT);
                }
                let bytes = buf.copy_to_bytes(len);
                ret.push(ColumnValue::Blob(bytes.to_vec()));
            }
            Some(ColumnType::Float) => {
                if buf.remaining() < 8 {
                    return Err(ResultCode::ABORT);
                }
                ret.push(ColumnValue::Float(buf.get_f64()));
            }
            Some(ColumnType::Integer) => {
                if buf.remaining() < intlen {
                    return Err(ResultCode::ABORT);
                }
                let unsigned = buf.get_uint(intlen);
                ret.push(ColumnValue::Integer(unsigned as i64));
            }
            Some(ColumnType::Null) => {
                ret.push(ColumnValue::Null);
            }
            Some(ColumnType::Text) => {
                if buf.remaining() < intlen {
                    return Err(ResultCode::ABORT);
                }
                let len = buf.get_uint(intlen) as usize;
                if buf.remaining() < len {
                    return Err(ResultCode::ABORT);
                }
                let bytes = buf.copy_to_bytes(len);
                ret.push(ColumnValue::Text(unsafe {
                    String::from_utf8_unchecked(bytes.to_vec())
                }))
            }
            None => return Err(ResultCode::MISUSE),
        }
    }

    Ok(ret)
}

pub fn bind_package_to_stmt(
    stmt: *mut sqlite::stmt,
    values: &Vec<ColumnValue>,
    offset: usize,
) -> Result<ResultCode, ResultCode> {
    for (i, val) in values.iter().enumerate() {
        bind_slot(i + 1 + offset, val, stmt)?;
    }
    Ok(ResultCode::OK)
}

pub fn bind_slot(
    slot_num: usize,
    val: &ColumnValue,
    stmt: *mut sqlite::stmt,
) -> Result<ResultCode, ResultCode> {
    match val {
        ColumnValue::Blob(b) => stmt.bind_blob(slot_num as i32, b, sqlite::Destructor::STATIC),
        ColumnValue::Float(f) => stmt.bind_double(slot_num as i32, *f),
        ColumnValue::Integer(i) => stmt.bind_int64(slot_num as i32, *i),
        ColumnValue::Null => stmt.bind_null(slot_num as i32),
        ColumnValue::Text(t) => stmt.bind_text(slot_num as i32, t, sqlite::Destructor::STATIC),
    }
}

/// Accumulator state for crsql_pack_agg aggregate function.
/// Stored via sqlite3_aggregate_context.
#[repr(C)]
struct PackAggAcc {
    buf: *mut Vec<u8>,
    count: u64,
}

impl PackAggAcc {
    const SIZE: c_int = core::mem::size_of::<PackAggAcc>() as c_int;
}

/// xStep callback for crsql_pack_agg.
/// Encodes a single SQLite value into the accumulator buffer using the same
/// TLV encoding as crsql_pack_columns.
pub unsafe extern "C" fn crsql_pack_agg_step(
    ctx: *mut sqlite::context,
    argc: c_int,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);
    if args.is_empty() {
        return;
    }
    let value = args[0];

    let acc_ptr = aggregate_context(ctx, PackAggAcc::SIZE) as *mut PackAggAcc;
    if acc_ptr.is_null() {
        ctx.result_error("crsql_pack_agg: failed to allocate aggregate context");
        ctx.result_error_code(ResultCode::NOMEM);
        return;
    }

    // First call: initialize the buffer
    if (*acc_ptr).buf.is_null() {
        (*acc_ptr).buf = Box::into_raw(Box::new(Vec::new()));
        (*acc_ptr).count = 0;
    }

    let buf = &mut *(*acc_ptr).buf;
    encode_value(buf, value);
    (*acc_ptr).count += 1;
}

/// xFinal callback for crsql_pack_agg.
/// Prepends the varint count header and returns the packed blob.
pub unsafe extern "C" fn crsql_pack_agg_final(ctx: *mut sqlite::context) {
    let acc_ptr = aggregate_context(ctx, 0) as *mut PackAggAcc;

    if acc_ptr.is_null() || (*acc_ptr).buf.is_null() {
        // No rows were processed (xStep never called).
        // Return an empty packed blob: just varint(0).
        let mut empty = Vec::new();
        put_varint(&mut empty, 0);
        ctx.result_blob_owned(empty);
        return;
    }

    let buf_ptr = (*acc_ptr).buf;
    let count = (*acc_ptr).count;
    let buf = Box::from_raw(buf_ptr);

    // Prepend varint count header
    let mut result = Vec::with_capacity(9 + buf.len());
    put_varint(&mut result, count);
    result.extend_from_slice(&buf);

    ctx.result_blob_owned(result);
}

/// Accumulator state for crsql_pack_varint_agg aggregate function.
/// Collects a sequence of integers as SQLite varints, with a varint count
/// header prepended in xFinal. Used by the V2 packed feed query for `seq`
/// and `col_vrsn` (both arrays of integers), replacing the old
/// `GROUP_CONCAT(..., char(0))` text encoding.
#[repr(C)]
struct PackVarintAcc {
    buf: *mut Vec<u8>,
    count: u64,
}

impl PackVarintAcc {
    const SIZE: c_int = core::mem::size_of::<PackVarintAcc>() as c_int;
}

/// xStep callback for crsql_pack_varint_agg.
/// Appends the integer argument as a SQLite varint to the accumulator buffer.
/// Non-integer arguments are coerced to int64 first (matching SQLite semantics).
pub unsafe extern "C" fn crsql_pack_varint_agg_step(
    ctx: *mut sqlite::context,
    argc: c_int,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);
    if args.is_empty() {
        return;
    }
    let value = args[0];

    let acc_ptr = aggregate_context(ctx, PackVarintAcc::SIZE) as *mut PackVarintAcc;
    if acc_ptr.is_null() {
        ctx.result_error("crsql_pack_varint_agg: failed to allocate aggregate context");
        ctx.result_error_code(ResultCode::NOMEM);
        return;
    }

    if (*acc_ptr).buf.is_null() {
        (*acc_ptr).buf = Box::into_raw(Box::new(Vec::new()));
        (*acc_ptr).count = 0;
    }

    // SQLite varints are unsigned (u64). Reinterpret the int64 bits so
    // negative values round-trip correctly (same bit pattern on decode).
    let val = value.int64() as u64;
    let buf = &mut *(*acc_ptr).buf;
    put_varint(buf, val);
    (*acc_ptr).count += 1;
}

/// xFinal callback for crsql_pack_varint_agg.
/// Prepends the varint count header and returns the packed blob.
/// Format: [count:varint, ...varint(value_i)] — matches the envelope shape
/// of crsql_pack_columns / crsql_pack_agg (count header + payload).
pub unsafe extern "C" fn crsql_pack_varint_agg_final(ctx: *mut sqlite::context) {
    let acc_ptr = aggregate_context(ctx, 0) as *mut PackVarintAcc;

    if acc_ptr.is_null() || (*acc_ptr).buf.is_null() {
        // No rows: emit varint(0) so the blob is self-describing.
        let mut empty = Vec::new();
        put_varint(&mut empty, 0);
        ctx.result_blob_owned(empty);
        return;
    }

    let buf_ptr = (*acc_ptr).buf;
    let count = (*acc_ptr).count;
    let buf = Box::from_raw(buf_ptr);

    let mut result = Vec::with_capacity(9 + buf.len());
    put_varint(&mut result, count);
    result.extend_from_slice(&buf);

    ctx.result_blob_owned(result);
}

/// Unpack a blob produced by crsql_pack_varint_agg into a Vec<i64>.
/// Format: [count:varint, ...varint(value_i)]. Values are reinterpreted from
/// u64 to i64 to recover negative numbers (see crsql_pack_varint_agg_step).
pub fn unpack_varints(data: &[u8]) -> Result<Vec<i64>, ResultCode> {
    let (count, header_len) = get_varint(data)?;
    let mut buf = &data[header_len..];
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (val, n) = get_varint(buf)?;
        out.push(val as i64);
        buf = &buf[n..];
    }
    Ok(out)
}

/// Encode a single SQLite value into the buffer using the same TLV encoding
/// as pack_columns.
fn encode_value(buf: &mut Vec<u8>, value: *mut sqlite::value) {
    match value.value_type() {
        ColumnType::Blob => {
            let len = value.bytes();
            let num_bytes_for_len = num_bytes_needed_i32(len);
            let type_byte = num_bytes_for_len << 3 | (ColumnType::Blob as u8);
            buf.put_u8(type_byte);
            buf.put_int(len as i64, num_bytes_for_len as usize);
            buf.put_slice(value.blob());
        }
        ColumnType::Null => {
            buf.put_u8(ColumnType::Null as u8);
        }
        ColumnType::Float => {
            buf.put_u8(ColumnType::Float as u8);
            buf.put_f64(value.double());
        }
        ColumnType::Integer => {
            let val = value.int64();
            let num_bytes_for_int = num_bytes_needed_i64(val);
            let type_byte = num_bytes_for_int << 3 | (ColumnType::Integer as u8);
            buf.put_u8(type_byte);
            buf.put_int(val, num_bytes_for_int as usize);
        }
        ColumnType::Text => {
            let len = value.bytes();
            let num_bytes_for_len = num_bytes_needed_i32(len);
            let type_byte = num_bytes_for_len << 3 | (ColumnType::Text as u8);
            buf.put_u8(type_byte);
            buf.put_int(len as i64, num_bytes_for_len as usize);
            buf.put_slice(value.blob());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_single_byte() {
        for v in 0..0x80 {
            let mut buf = vec![];
            put_varint(&mut buf, v);
            assert_eq!(buf.len(), 1);
            assert_eq!(buf[0], v as u8);
            let (decoded, n) = get_varint(&buf).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(n, 1);
        }
    }

    #[test]
    fn test_varint_two_bytes() {
        // 128 = 0x81 0x00, 200 = 0x81 0x48, 16383 = 0xFF 0x7F
        let cases: &[(u64, &[u8])] = &[
            (128, &[0x81, 0x00]),
            (200, &[0x81, 0x48]),
            (16383, &[0xFF, 0x7F]),
        ];
        for &(val, expected) in cases {
            let mut buf = vec![];
            put_varint(&mut buf, val);
            assert_eq!(buf.as_slice(), expected, "encoding mismatch for {}", val);
            let (decoded, n) = get_varint(&buf).unwrap();
            assert_eq!(decoded, val);
            assert_eq!(n, expected.len());
        }
    }

    #[test]
    fn test_varint_three_bytes() {
        // 16384 = 0x81 0x80 0x00, 1048576 = 0xC0 0x80 0x00
        let cases: &[(u64, &[u8])] = &[
            (16384, &[0x81, 0x80, 0x00]),
            (1048576, &[0xC0, 0x80, 0x00]),
        ];
        for &(val, expected) in cases {
            let mut buf = vec![];
            put_varint(&mut buf, val);
            assert_eq!(buf.as_slice(), expected, "encoding mismatch for {}", val);
            let (decoded, n) = get_varint(&buf).unwrap();
            assert_eq!(decoded, val);
            assert_eq!(n, expected.len());
        }
    }

    #[test]
    fn test_varint_round_trip_boundaries() {
        // Test all power-of-2 boundaries and values just below/above them
        let mut values = vec![0u64, 1, 127, 128, 129];
        let mut shift = 7;
        while shift < 64 {
            values.push(1u64 << shift);
            values.push((1u64 << shift) - 1);
            values.push((1u64 << shift) + 1);
            shift += 7;
        }
        values.push(u64::MAX);
        values.push(i64::MAX as u64);
        values.push(i64::MIN as u64); // reinterpret for negative round-trip

        for &val in &values {
            let mut buf = vec![];
            put_varint(&mut buf, val);
            let (decoded, n) = get_varint(&buf).unwrap();
            assert_eq!(decoded, val, "round-trip failed for {} (0x{:x})", val, val);
            assert_eq!(n, buf.len());
        }
    }

    #[test]
    fn test_varint_sequential_decode() {
        // Multiple varints packed back-to-back should decode independently
        let values = [0u64, 1, 127, 128, 200, 16384, 1048576, 42];
        let mut buf = vec![];
        for &v in &values {
            put_varint(&mut buf, v);
        }
        let mut offset = 0;
        for &expected in &values {
            let (decoded, n) = get_varint(&buf[offset..]).unwrap();
            assert_eq!(decoded, expected);
            offset += n;
        }
        assert_eq!(offset, buf.len());
    }

    #[test]
    fn test_varint_nine_bytes() {
        // 9-byte varints: values that require all 9 bytes.
        // SQLite varint format (MSB-first / big-endian):
        //   p[0..7] = 7 data bits each (high bit = continuation, always set)
        //   p[8]    = 8 data bits (least significant byte, no continuation)
        // Total capacity: 7*8 + 8 = 64 bits.
        //
        // Encoder: p[8] = v & 0xFF, then extract 7-bit groups from v>>8
        // into p[7..0] (MSB-first).
        let cases: &[(u64, &[u8])] = &[
            // 2^56 = 0x0100000000000000
            // p[8] = 0x00, v>>8 = 2^48
            // 7-bit groups from 2^48: [0, 0, 0, 0, 0, 0, 64, 0] (LSB-first)
            // Emitted MSB-first: p[0]=0x80, p[1]=0xC0, p[2..7]=0x80, p[8]=0x00
            (
                1u64 << 56,
                &[0x80, 0xC0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
            ),
            // u64::MAX = 0xFFFFFFFFFFFFFFFF — all bits set
            // p[8] = 0xFF, all 7-bit groups = 0x7F → 0xFF with continuation
            (
                u64::MAX,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            ),
            // i64::MAX = 0x7FFFFFFFFFFFFFFF
            // p[8] = 0xFF, top group = 0x3F → 0xBF, rest = 0xFF
            (
                i64::MAX as u64,
                &[0xBF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            ),
        ];
        for &(val, expected) in cases {
            let mut buf = vec![];
            put_varint(&mut buf, val);
            assert_eq!(buf.len(), 9, "9-byte varint for 0x{:x} should be 9 bytes", val);
            assert_eq!(
                buf.as_slice(),
                expected,
                "9-byte encoding mismatch for 0x{:x}",
                val
            );
            let (decoded, n) = get_varint(&buf).unwrap();
            assert_eq!(decoded, val, "9-byte round-trip failed for 0x{:x}", val);
            assert_eq!(n, 9);
        }
    }

    #[test]
    fn test_varint_continuation_bit_placement() {
        // Explicitly verify that the continuation bit (0x80) is set on every
        // byte EXCEPT the last emitted byte. The old bug set it on bytes 0..n-2
        // (missing the second-to-last byte) instead of 1..n-1.
        //
        // For a 2-byte varint: byte[0] has continuation, byte[1] doesn't.
        // For a 3-byte varint: byte[0] and byte[1] have continuation, byte[2] doesn't.
        // For a 9-byte varint: bytes[0..7] have continuation, byte[8] may or may not
        // (it uses all 8 bits for data, so the high bit can be set as a data bit).
        let mut buf = vec![];
        put_varint(&mut buf, 200); // 2 bytes
        assert!(buf[0] & 0x80 != 0, "first byte must have continuation bit");
        assert!(buf[1] & 0x80 == 0, "last byte must NOT have continuation bit");

        buf.clear();
        put_varint(&mut buf, 16384); // 3 bytes
        assert!(buf[0] & 0x80 != 0, "byte 0 must have continuation bit");
        assert!(buf[1] & 0x80 != 0, "byte 1 must have continuation bit");
        assert!(buf[2] & 0x80 == 0, "last byte must NOT have continuation bit");

        buf.clear();
        put_varint(&mut buf, 1u64 << 56); // 9 bytes
        // Bytes 0-7 must have continuation bit set (they're 7-bit groups)
        for i in 0..8 {
            assert!(
                buf[i] & 0x80 != 0,
                "byte {} of 9-byte varint must have continuation bit",
                i
            );
        }
        // Byte 8 uses all 8 bits for data — high bit may or may not be set
        // For 2^56, byte 8 = 0x00 (no high bit)
        assert_eq!(buf[8], 0x00, "byte 8 of 2^56 should be 0x00");
    }
}
