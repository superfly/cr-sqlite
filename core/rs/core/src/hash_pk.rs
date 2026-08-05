extern crate alloc;

use alloc::vec::Vec;
use sqlite_nostd as sqlite;
use sqlite_nostd::{Context, ResultCode};

use crate::pack_columns::pack_columns;

/// Compute a truncated XXH128 hash of packed PK values.
/// Returns PK_HASH_SIZE bytes.
pub fn hash_pk_values(values: &[*mut sqlite::value]) -> Result<Vec<u8>, ResultCode> {
    let packed = pack_columns(values)?;
    let hash = xxhash_rust::xxh3::xxh3_128(&packed);
    let bytes = hash.to_be_bytes();
    Ok(bytes[..crate::consts::PK_HASH_SIZE].to_vec())
}

/// Hash PK values from unpacked ColumnValue list.
/// Packs the values and hashes with XXH128, truncated to PK_HASH_SIZE bytes.
pub fn hash_pk_values_from_column_values(
    values: &[crate::pack_columns::ColumnValue],
) -> Result<Vec<u8>, ResultCode> {
    let packed = crate::pack_columns::pack_column_values(values)?;
    let hash = xxhash_rust::xxh3::xxh3_128(&packed);
    let bytes = hash.to_be_bytes();
    Ok(bytes[..crate::consts::PK_HASH_SIZE].to_vec())
}

/// Hash a pre-packed PK blob directly (e.g., from crsql_changes pk column).
/// Avoids the unpack→repack cycle when the packed blob is already available.
pub fn hash_packed_blob(packed: &[u8]) -> Vec<u8> {
    let hash = xxhash_rust::xxh3::xxh3_128(packed);
    let bytes = hash.to_be_bytes();
    bytes[..crate::consts::PK_HASH_SIZE].to_vec()
}

/// SQL function: crsql_hash_pk(pk1, pk2, ...)
/// Takes variadic PK values, packs them, hashes with XXH128, truncates to PK_HASH_SIZE bytes.
pub extern "C" fn crsql_hash_pk(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    match hash_pk_values(args) {
        Ok(hash) => {
            ctx.result_blob_owned(hash);
        }
        Err(code) => {
            ctx.result_error("crsql_hash_pk: failed to hash PK values");
            ctx.result_error_code(code);
        }
    }
}
