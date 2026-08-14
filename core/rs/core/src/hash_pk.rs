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

// Stability tests with hardcoded hash vectors.
// These hashes are computed from the exact packed blob format (varint count header
// + TLV-encoded values) and truncated XXH128. If the packing format or hash
// computation changes, these tests will fail — preventing silent hash changes
// that would break existing V2 metadata tables (tombstones, v2_pks lookups, etc.).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack_columns::{ColumnValue, pack_column_values};

    fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn compute_hash(values: &[ColumnValue]) -> Vec<u8> {
        let packed = pack_column_values(values).unwrap();
        hash_packed_blob(&packed)
    }

    fn assert_hash(values: &[ColumnValue], expected_hex: &str) {
        let hash = compute_hash(values);
        let expected = hex_to_bytes(expected_hex);
        assert_eq!(hash, expected, "hash mismatch");
    }

    #[test]
    fn test_single_integer_pk() {
        assert_hash(&[ColumnValue::Integer(1)], "cbbe68493d0f89fd6eff");
        assert_hash(&[ColumnValue::Integer(42)], "b47bb9d09c233e761427");
        assert_hash(&[ColumnValue::Integer(0)], "50e58cbd6ada00a1070d");
    }

    #[test]
    fn test_single_text_pk() {
        assert_hash(&[ColumnValue::Text("hello".to_string())], "ec40da9a164df00c981f");
        assert_hash(&[ColumnValue::Text("".to_string())], "15ca8e4527acb64342f1");
    }

    #[test]
    fn test_composite_pk() {
        assert_hash(
            &[ColumnValue::Integer(1), ColumnValue::Text("abc".to_string())],
            "d17dbd291f0b1cd9e1d5",
        );
    }

    #[test]
    fn test_null_pk() {
        assert_hash(&[ColumnValue::Null], "2220638bc95eeae33b08");
    }

    #[test]
    fn test_blob_pk() {
        assert_hash(
            &[ColumnValue::Blob(vec![0x01, 0x02, 0x03])],
            "290b910dddb1869e845d",
        );
    }

    #[test]
    fn test_float_pk() {
        assert_hash(&[ColumnValue::Float(3.14)], "d61f1c470f6bcc19d56a");
    }

    #[test]
    fn test_hash_packed_blob_direct() {
        // Known packed blob: varint(1) + integer(1) = [0x01, 0x08, 0x01]
        let packed = vec![0x01, 0x08, 0x01];
        let hash = hash_packed_blob(&packed);
        assert_eq!(hash.len(), crate::consts::PK_HASH_SIZE);
        // Verify determinism: same input always produces same output
        let hash2 = hash_packed_blob(&packed);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_different_inputs_produce_different_hashes() {
        let packed1 = pack_column_values(&[ColumnValue::Integer(1)]).unwrap();
        let packed2 = pack_column_values(&[ColumnValue::Integer(2)]).unwrap();
        let hash1 = hash_packed_blob(&packed1);
        let hash2 = hash_packed_blob(&packed2);
        assert_ne!(hash1, hash2);
    }
}
