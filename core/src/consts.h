#ifndef CRSQLITE_CONSTS_H
#define CRSQLITE_CONSTS_H

// db version is a signed 64bit int since sqlite doesn't support saving and
// retrieving unsigned 64bit ints. (2^64 / 2) is a big enough number to write 1
// million entries per second for 3,000 centuries.
#define MIN_POSSIBLE_DB_VERSION 0L

#define ROWID_SLAB_SIZE 10000000000000

// Note: CLOCK_TABLES_SELECT uses GLOB (not LIKE) so the underscores are matched
// literally rather than as single-character wildcards. The pattern matches
// both V1 clock tables (suffix "__crsql_clock") and V2 clock tables (suffix
// "__crsql_v2_clock"). The consuming C code path (pSelectClockTablesStmt /
// crsql_recreate_db_version_stmt) is currently dead -- the statement is
// prepared and finalized but never stepped -- so this is kept for correctness
// should it be revived.
#define CLOCK_TABLES_SELECT                                                    \
  "SELECT tbl_name FROM sqlite_master WHERE type='table' AND "                \
  "(tbl_name GLOB '*__crsql_clock' OR tbl_name GLOB '*__crsql_v2_clock')"

#define SET_SYNC_BIT "SELECT crsql_internal_sync_bit(1)"
#define CLEAR_SYNC_BIT "SELECT crsql_internal_sync_bit(0)"

#define TBL_SITE_ID "crsql_site_id"
#define TBL_DB_VERSION "crsql_db_versions"

#define MAX_TBL_NAME_LEN 2048
#define SITE_ID_LEN 16

// Version int:
// M - major
// m - minor
// p - patch
// b - build
// MM.mm.pp.bb
// 00 00 00 00
// Given we can't prefix an int with 0s, read from right to left.
// Rightmost is always `bb`
#define CRSQLITE_VERSION 180000

#endif
