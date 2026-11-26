/**
 * Test that:
 * 1. The rowid we return for a row on insert matches the rowid we get for it on
 * read
 * 2. That we can query the vtab by rowid??
 * 3. The returned rowid matches the rowid used in a point query by rowid
 * 4.
 */

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "consts.h"
#include "crsqlite.h"

int crsql_close(sqlite3 *db);

// static void testRowidForInsert() {
//   printf("RowidForInsert\n");

//   sqlite3 *db;
//   int rc;
//   rc = sqlite3_open(":memory:", &db);

//   rc = sqlite3_exec(db, "CREATE TABLE foo (a primary key not null, b);", 0,
//   0, 0); rc += sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
//   assert(rc == SQLITE_OK);

//   char *zSql =
//       "INSERT INTO crsql_changes ([table], pk, cid, val, col_version, "
//       "db_version) "
//       "VALUES "
//       "('foo', '1', 'b', '1', 1, 1) RETURNING _rowid_;";
//   sqlite3_stmt *pStmt;
//   rc = sqlite3_prepare_v2(db, zSql, -1, &pStmt, 0);
//   assert(rc == SQLITE_OK);
//   assert(sqlite3_step(pStmt) == SQLITE_ROW);
//   printf("rowid: %d\n", sqlite3_column_int64(pStmt, 0));
//   assert(sqlite3_column_int64(pStmt, 0) == 1);
//   sqlite3_finalize(pStmt);

//   // TODO: make extra crr tables and check their slab allotments and returned
//   // rowids

//   crsql_close(db);
//   printf("\t\e[0;32mSuccess\e[0m\n");
// }

static void testRowidsForReads() {
  printf("RowidForReads\n");

  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc = sqlite3_exec(db, "CREATE TABLE foo (a primary key not null, b);", 0, 0,
                    0);
  rc += sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000)", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "INSERT INTO foo VALUES (1,2);", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "INSERT INTO foo VALUES (2,3);", 0, 0, 0);
  assert(rc == SQLITE_OK);

  const char *zSql = "SELECT _rowid_ FROM crsql_changes ORDER BY _rowid_";
  sqlite3_stmt *pStmt;
  rc = sqlite3_prepare_v2(db, zSql, -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 2);
  sqlite3_finalize(pStmt);

  rc =
      sqlite3_exec(db, "CREATE TABLE bar (a primary key not null, b)", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000)", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_as_crr('bar');", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT INTO bar VALUES (1,2);", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT INTO bar VALUES (2,3);", 0, 0, 0);

  rc +=
      sqlite3_exec(db, "CREATE TABLE baz (a primary key not null, b)", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000)", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_as_crr('baz');", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT INTO baz VALUES (1,2);", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT INTO baz VALUES (2,3);", 0, 0, 0);

  assert(rc == SQLITE_OK);

  rc = sqlite3_prepare_v2(db, zSql, -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 2);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1 * ROWID_SLAB_SIZE + 1);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1 * ROWID_SLAB_SIZE + 2);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 2 * ROWID_SLAB_SIZE + 1);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 2 * ROWID_SLAB_SIZE + 2);
  sqlite3_finalize(pStmt);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

// static void testInsertRowidMatchesReadRowid() {
//   printf("RowidForInsertMatchesRowidForRead\n");
//   printf("\t\e[0;32mSuccess\e[0m\n");
// }

static void testRowidOperationEncoding() {
  printf("RowidOperationEncoding\n");

  sqlite3 *db;
  int rc = sqlite3_open(":memory:", &db);
  rc += sqlite3_exec(db, "SELECT crsql_set_ts('1700000000');", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000);", 0, 0, 0);
  rc += sqlite3_exec(db, "CREATE TABLE foo (a primary key not null, b);", 0, 0, 0);
  rc += sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
  assert(rc == SQLITE_OK);

  // 1. Insert (cl = 1, col_version = 1) -> op = 1
  rc = sqlite3_exec(
      db,
      "INSERT INTO crsql_changes VALUES ('foo', X'010901', 'b', 'v1', 1, 1, NULL, 1, 0, '0');",
      0, 0, 0);
  assert(rc == SQLITE_OK);
  sqlite3_int64 rowid = sqlite3_last_insert_rowid(db);
  assert(((rowid >> 60) & 0x7) == 1);
  assert((rowid & ((1LL << 60) - 1)) > 0);

  // 2. No-op (stale col_version = 1, same value) -> rowid = 0
  rc = sqlite3_exec(
      db,
      "INSERT INTO crsql_changes VALUES ('foo', X'010901', 'b', 'v1', 1, 2, NULL, 1, 0, '0');",
      0, 0, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_last_insert_rowid(db) == 0);

  // 3. Update (cl = 1, col_version = 2) -> op = 2
  rc = sqlite3_exec(
      db,
      "INSERT INTO crsql_changes VALUES ('foo', X'010901', 'b', 'v2', 2, 3, NULL, 1, 0, '0');",
      0, 0, 0);
  assert(rc == SQLITE_OK);
  rowid = sqlite3_last_insert_rowid(db);
  assert(((rowid >> 60) & 0x7) == 2);

  // 4. Delete (cl = 2, cid = '-1') -> op = 3
  rc = sqlite3_exec(
      db,
      "INSERT INTO crsql_changes VALUES ('foo', X'010901', '-1', NULL, 2, 4, NULL, 2, 0, '0');",
      0, 0, 0);
  assert(rc == SQLITE_OK);
  rowid = sqlite3_last_insert_rowid(db);
  assert(((rowid >> 60) & 0x7) == 3);

  // 5. Resurrection (cl = 3, col_version = 1) -> op = 4
  rc = sqlite3_exec(
      db,
      "INSERT INTO crsql_changes VALUES ('foo', X'010901', 'b', 'v3', 1, 5, NULL, 3, 0, '0');",
      0, 0, 0);
  assert(rc == SQLITE_OK);
  rowid = sqlite3_last_insert_rowid(db);
  assert(((rowid >> 60) & 0x7) == 4);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

void crsqlChangesVtabRowidTestSuite() {
  printf("\e[47m\e[1;30mSuite: crsql_changesVtabRowid\e[0m\n");
  // testRowidForInsert();
  testRowidsForReads();
  testRowidOperationEncoding();
  // testInsertRowidMatchesReadRowid();
}