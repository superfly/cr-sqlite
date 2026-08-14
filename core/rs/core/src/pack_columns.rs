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
fn put_varint(buf: &mut Vec<u8>, value: u64) {
    if value < 0x80 {
        buf.put_u8(value as u8);
        return;
    }
    // SQLite varint: up to 9 bytes, high bit = continuation
    let mut bytes = [0u8; 9];
    let mut n = 0;
    let mut v = value;
    if v == 0 {
        buf.put_u8(0);
        return;
    }
    while v > 0 && n < 9 {
        bytes[n] = (v & 0x7F) as u8;
        v >>= 7;
        n += 1;
    }
    // Set continuation bits on all but the last byte
    for i in 1..n {
        bytes[i - 1] |= 0x80;
    }
    // Bytes are stored MSB first (reverse of how we filled them)
    for i in (0..n).rev() {
        buf.put_u8(bytes[i]);
    }
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

fn bind_slot(
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
