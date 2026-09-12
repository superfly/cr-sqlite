#include <assert.h>
#include <stdio.h>

#include "crsqlite.h"
#include "rust.h"

int crsql_close(sqlite3 *db);

static void testTableIsNotCrr() {
  printf("TableIsNotCrr\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc =
      sqlite3_exec(db, "CREATE TABLE foo (a PRIMARY KEY NOT NULL, b)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  int isCrr = crsql_is_crr(db, "foo");
  assert(crsql_close(db) == SQLITE_OK);
  assert(isCrr >= 0);
  assert(isCrr == 0);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testCrrIsCrr() {
  printf("CrrIsCrr\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc =
      sqlite3_exec(db, "CREATE TABLE foo (a PRIMARY KEY NOT NULL, b)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_crr('foo')", 0, 0, 0);
  assert(rc == SQLITE_OK);

  int isCrr = crsql_is_crr(db, "foo");
  assert(crsql_close(db) == SQLITE_OK);
  assert(isCrr >= 0);
  assert(isCrr == 1);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testDestroyedCrrIsNotCrr() {
  printf("DestroyedCrrIsNotCrr\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);

  rc =
      sqlite3_exec(db, "CREATE TABLE foo (a PRIMARY KEY NOT NULL, b)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_set_ts('1700000000')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_config_set('default-ts', 1700000000)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_crr('foo')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = sqlite3_exec(db, "SELECT crsql_as_table('foo')", 0, 0, 0);
  assert(rc == SQLITE_OK);
  int isCrr = crsql_is_crr(db, "foo");
  assert(crsql_close(db) == SQLITE_OK);
  assert(isCrr >= 0);
  assert(isCrr == 0);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

void crsqlIsCrrTestSuite() {
  printf("\e[47m\e[1;30mSuite: is_crr\e[0m\n");

  testTableIsNotCrr();
  testCrrIsCrr();
  testDestroyedCrrIsNotCrr();
}
