pub const TBL_SITE_ID: &'static str = "crsql_site_id";
pub const TBL_SCHEMA: &'static str = "crsql_master";
// pub const CRSQLITE_VERSION_0_15_0: i32 = 15_00_00;
// pub const CRSQLITE_VERSION_0_13_0: i32 = 13_00_00;
// MM_mm_pp_xx
// so a 1.0.0 release is:
// 01_00_00_00 -> 1000000
// a 0.5 release is:
// 00_05_00_00 ->   50000
// a 0.5.1 is:
// 00_05_01_00
// and, if we ever need it, we can track individual builds of a patch release
// 00_05_01_01
pub const CRSQLITE_VERSION: i32 = 18_00_00;
pub const _CRSQLITE_VERSION_STR: &'static str = "0.18.0";
pub const CRSQLITE_VERSION_0_17_0: i32 = 17_00_00;

/// Minimum SQLite version required: 3.44.0 (ORDER BY in aggregates).
/// Format: XYYZZ00 -> 3.44.0 = 3044000
pub const MIN_SQLITE_VERSION: i32 = 3044000;

pub const SITE_ID_LEN: i32 = 16;
pub const ROWID_SLAB_SIZE: i64 = 10000000000000;
// db version is a signed 64bit int since sqlite doesn't support saving and
// retrieving unsigned 64bit ints. (2^64 / 2) is a big enough number to write 1
// million entries per second for 3,000 centuries.
pub const MIN_POSSIBLE_DB_VERSION: i64 = 0;
pub const MAX_TBL_NAME_LEN: i32 = 2048;

// V2 metadata constants
/// Number of bits used for col_id in the packed cell_key.
/// Default: 12 bits = up to 4096 columns per table.
/// cell_key = (pk_key << CRSQL_COL_ID_BITS) | col_id
pub const CRSQL_COL_ID_BITS: u32 = 12;
/// Mask to extract col_id from cell_key: (1 << CRSQL_COL_ID_BITS) - 1
pub const CRSQL_COL_ID_MASK: i64 = (1i64 << CRSQL_COL_ID_BITS) - 1;
/// Number of bytes for truncated XXH128 hash of PK values.
/// Default: 10 bytes / 80 bits.
pub const PK_HASH_SIZE: usize = 10;
/// Maximum __crsql_key value for rowid tables to keep cell_key positive in signed INT64.
/// 2^(63 - CRSQL_COL_ID_BITS)
pub const MAX_ROWID_KEY: i64 = 1i64 << (63 - CRSQL_COL_ID_BITS);

// V2 table name suffixes
pub const V2_COL_MAP_SUFFIX: &str = "__crsql_v2_col_map";
pub const V2_CLOCK_SUFFIX: &str = "__crsql_v2_clock";
pub const V2_PKS_SUFFIX: &str = "__crsql_v2_pks";
pub const V2_TOMBSTONES_SUFFIX: &str = "__crsql_v2_tombstones";
pub const V2_TOMBSTONE_PKS_SUFFIX: &str = "__crsql_v2_tombstone_pks";

// V2 wire format sentinels
pub static V2_HASH_TOMBSTONE_CID: &str = "-2";

// Metadata and sync log version constants
pub const META_USE_V1: i32 = 1;
pub const META_USE_V2: i32 = 2;
pub const SYNC_LOG_V1: i32 = 1;
pub const SYNC_LOG_V2: i32 = 2;
