#include "changes-vtab.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "crsqlite.h"

int crsql_close(sqlite3 *db);

static void testInsertOrReplaceBasic() {
  printf("InsertOrReplaceBasic\n");

  sqlite3 *db;
  sqlite3_stmt *pStmt;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc = sqlite3_exec(db, "CREATE TABLE foo (a INTEGER PRIMARY KEY NOT NULL, b TEXT);", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
  if (rc != SQLITE_OK) {
    printf("crsql_as_crr failed: %s\n", sqlite3_errmsg(db));
  }
  assert(rc == SQLITE_OK);

  // Insert a row
  rc = sqlite3_exec(db, "INSERT INTO foo VALUES (1, 'a');", 0, 0, 0);
  assert(rc == SQLITE_OK);

  // INSERT OR REPLACE on existing pk — should fire DELETE then INSERT trigger
  rc = sqlite3_exec(db, "INSERT OR REPLACE INTO foo VALUES (1, 'b');", 0, 0, 0);
  assert(rc == SQLITE_OK);

  // Verify the row has correct data
  rc = sqlite3_prepare_v2(db, "SELECT b FROM foo WHERE a = 1;", -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(strcmp("b", (const char *)sqlite3_column_text(pStmt, 0)) == 0);
  sqlite3_finalize(pStmt);

  // Verify crsql_changes shows the latest state.
  // With recursive_triggers ON, INSERT OR REPLACE fires DELETE then INSERT.
  // The delete creates a tombstone (cl=2), then the insert resurrects (cl=3).
  // crsql_changes should show the final alive state with cl=3.
  rc = sqlite3_prepare_v2(db,
      "SELECT [table], quote(pk), cid, cl, quote(val) FROM crsql_changes WHERE [table] = 'foo'",
      -1, &pStmt, 0);
  assert(rc == SQLITE_OK);

  int found_insert = 0;
  while (sqlite3_step(pStmt) == SQLITE_ROW) {
    sqlite3_int64 cl = sqlite3_column_int64(pStmt, 3);
    if (cl == 3) {
      found_insert = 1;
    }
  }
  sqlite3_finalize(pStmt);

  // The final state should show cl=3 (delete cl=2 + resurrect cl=3)
  assert(found_insert);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testInsertOrReplaceNoLingeringMetadata() {
  printf("InsertOrReplaceNoLingeringMetadata\n");

  sqlite3 *db;
  sqlite3_stmt *pStmt;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc = sqlite3_exec(db, "CREATE TABLE foo (a INTEGER PRIMARY KEY NOT NULL, b TEXT);", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
  if (rc != SQLITE_OK) {
    printf("crsql_as_crr failed: %s\n", sqlite3_errmsg(db));
  }
  assert(rc == SQLITE_OK);

  // Insert and then replace multiple times
  rc = sqlite3_exec(db, "INSERT INTO foo VALUES (1, 'a');", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT OR REPLACE INTO foo VALUES (1, 'b');", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT OR REPLACE INTO foo VALUES (1, 'c');", 0, 0, 0);
  rc += sqlite3_exec(db, "INSERT OR REPLACE INTO foo VALUES (1, 'd');", 0, 0, 0);
  assert(rc == SQLITE_OK);

  // Verify only one row in the base table
  rc = sqlite3_prepare_v2(db, "SELECT COUNT(*) FROM foo;", -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1);
  sqlite3_finalize(pStmt);

  // Verify only one PK entry in v2_pks (no lingering duplicates)
  rc = sqlite3_prepare_v2(db, "SELECT COUNT(*) FROM foo__crsql_v2_pks;", -1, &pStmt, 0);
  if (rc == SQLITE_OK && sqlite3_step(pStmt) == SQLITE_ROW) {
    assert(sqlite3_column_int64(pStmt, 0) == 1);
    sqlite3_finalize(pStmt);
  }

  // Verify no lingering tombstones (row should be alive, not dead)
  rc = sqlite3_prepare_v2(db, "SELECT COUNT(*) FROM foo__crsql_v2_tombstones;", -1, &pStmt, 0);
  if (rc == SQLITE_OK && sqlite3_step(pStmt) == SQLITE_ROW) {
    assert(sqlite3_column_int64(pStmt, 0) == 0);
    sqlite3_finalize(pStmt);
  }

  // Verify the latest value is correct
  rc = sqlite3_prepare_v2(db, "SELECT b FROM foo WHERE a = 1;", -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(strcmp("d", (const char *)sqlite3_column_text(pStmt, 0)) == 0);
  sqlite3_finalize(pStmt);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testInsertOrReplaceNewRow() {
  printf("InsertOrReplaceNewRow\n");

  sqlite3 *db;
  sqlite3_stmt *pStmt;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc = sqlite3_exec(db, "CREATE TABLE foo (a INTEGER PRIMARY KEY NOT NULL, b TEXT);", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0, 0, 0);
  if (rc != SQLITE_OK) {
    printf("crsql_as_crr failed: %s\n", sqlite3_errmsg(db));
  }
  assert(rc == SQLITE_OK);

  // INSERT OR REPLACE on a non-existing row — should just insert
  rc = sqlite3_exec(db, "INSERT OR REPLACE INTO foo VALUES (1, 'a');", 0, 0, 0);
  if (rc != SQLITE_OK) {
    printf("INSERT OR REPLACE failed: %s\n", sqlite3_errmsg(db));
  }
  assert(rc == SQLITE_OK);

  // Verify only one change entry (no delete, just insert with cl=1)
  rc = sqlite3_prepare_v2(db,
      "SELECT COUNT(*) FROM crsql_changes WHERE [table] = 'foo'",
      -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  // For a table with a non-PK column, we expect 1 change entry (the insert)
  assert(sqlite3_column_int64(pStmt, 0) == 1);
  sqlite3_finalize(pStmt);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testRecursiveTriggersEnabled() {
  printf("RecursiveTriggersEnabled\n");

  sqlite3 *db;
  sqlite3_stmt *pStmt;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  // Load cr-sqlite — should enable recursive_triggers
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);

  rc = sqlite3_prepare_v2(db, "PRAGMA recursive_triggers;", -1, &pStmt, 0);
  assert(rc == SQLITE_OK);
  assert(sqlite3_step(pStmt) == SQLITE_ROW);
  assert(sqlite3_column_int64(pStmt, 0) == 1);
  sqlite3_finalize(pStmt);

  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

int crsql_insertOrReplaceTestSuite() {
  printf("\n\e[47m\e[1;30mSuite: insert-or-replace\e[0m\n");
  testRecursiveTriggersEnabled();
  testInsertOrReplaceNewRow();
  testInsertOrReplaceBasic();
  testInsertOrReplaceNoLingeringMetadata();
  return 0;
}
