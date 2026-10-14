#include "ext-data.h"

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "consts.h"
#include "crsqlite.h"

int crsql_close(sqlite3 *db);

crsql_ExtData *crsqlExtDataInit(sqlite3 *db, unsigned char *siteIdBuffer) {
  crsql_ExtData *pExtData = crsql_newExtData(db);
  if (pExtData == 0) {
    fprintf(stderr, "crsql_newExtData returned NULL: %s\n", sqlite3_errmsg(db));
    assert(0);
  }
  int rc = crsql_initSiteIdExt(db, pExtData, siteIdBuffer);
  assert(rc == 0);
  return pExtData;
}

static void textNewExtData() {
  printf("NewExtData\n");
  sqlite3 *db;
  int rc = SQLITE_OK;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);
  unsigned char *siteIdBuffer =
      sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData = crsqlExtDataInit(db, siteIdBuffer);

  assert(pExtData->dbVersion == -1);
  // statement used to determine schema version
  assert(pExtData->pPragmaSchemaVersionStmt != 0);
  // statement used to determine data version
  assert(pExtData->pPragmaDataVersionStmt != 0);
  // last schema version fetched -- none so -1
  assert(pExtData->pragmaSchemaVersion == -1);
  // set in initSiteId
  assert(pExtData->siteId != 0);
  // statement used to get pDbVersionStmt
  assert(pExtData->pDbVersionStmt != 0);
  // table info allocated to an empty vec
  assert(pExtData->tableInfos != 0);
  // ordinal map allocated to an empty map
  assert(pExtData->ordinalMap != 0);

  // data version should have been fetched
  assert(pExtData->pragmaDataVersion != -1);
  crsql_finalize(pExtData);
  crsql_freeExtData(pExtData);
  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

// valgrind will let us know if we failed to free something
// mainly testing that we can finalize + free without error
static void testFreeExtData() {
  printf("FreeExtData\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);
  unsigned char *siteIdBuffer =
      sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData = crsqlExtDataInit(db, siteIdBuffer);

  crsql_finalize(pExtData);
  crsql_freeExtData(pExtData);
  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testFinalize() {
  printf("FinalizeExtData\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);
  unsigned char *siteIdBuffer =
      sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData = crsqlExtDataInit(db, siteIdBuffer);

  crsql_finalize(pExtData);
  assert(pExtData->pDbVersionStmt == 0);
  assert(pExtData->pPragmaSchemaVersionStmt == 0);

  // finalizing twice should be a no-op
  crsql_finalize(pExtData);
  crsql_freeExtData(pExtData);
  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testReadPragmaSchemaVersion() {
  printf("ReadPragmaSchemaVersion\n");
  sqlite3 *db;
  int rc;
  rc = sqlite3_open(":memory:", &db);
  assert(rc == SQLITE_OK);
  unsigned char *siteIdBuffer =
      sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData = crsqlExtDataInit(db, siteIdBuffer);

  int schemaVersion = -1;
  rc = crsql_readPragmaSchemaVersion(pExtData, &schemaVersion);
  assert(rc == SQLITE_OK);
  assert(schemaVersion != -1);
  assert(pExtData->pragmaSchemaVersion == -1);

  int oldVersion = schemaVersion;
  rc = crsql_readPragmaSchemaVersion(pExtData, &schemaVersion);
  assert(rc == SQLITE_OK);
  assert(schemaVersion == oldVersion);
  assert(pExtData->pragmaSchemaVersion == -1);

  sqlite3_exec(db, "CREATE TABLE foo (a)", 0, 0, 0);
  rc = crsql_readPragmaSchemaVersion(pExtData, &schemaVersion);
  assert(rc == SQLITE_OK);
  assert(schemaVersion != oldVersion);
  assert(pExtData->pragmaSchemaVersion == -1);

  oldVersion = schemaVersion;
  assert(sqlite3_exec(
             db,
             "SAVEPOINT schema_version_rollback; CREATE TABLE rollback_foo (a)",
             0, 0, 0) == SQLITE_OK);
  rc = crsql_readPragmaSchemaVersion(pExtData, &schemaVersion);
  assert(rc == SQLITE_OK);
  assert(schemaVersion != oldVersion);
  assert(pExtData->pragmaSchemaVersion == -1);
  assert(sqlite3_exec(
             db,
             "ROLLBACK TO schema_version_rollback; RELEASE schema_version_rollback",
             0, 0, 0) == SQLITE_OK);
  rc = crsql_readPragmaSchemaVersion(pExtData, &schemaVersion);
  assert(rc == SQLITE_OK);
  assert(schemaVersion == oldVersion);
  assert(pExtData->pragmaSchemaVersion == -1);

  crsql_finalize(pExtData);
  crsql_freeExtData(pExtData);
  crsql_close(db);
  printf("\t\e[0;32mSuccess\e[0m\n");
}

static void testFetchPragmaDataVersion() {
  printf("FetchPragmaDataVersion\n");
  remove("testFetchPragmaDataVersion.db");
  sqlite3 *db1;
  sqlite3 *db2;
  int rc;
  int dataVersion1;
  int dataVersion2;
  char *errmsg = 0;
  rc = sqlite3_open("testFetchPragmaDataVersion.db", &db1);
  assert(rc == SQLITE_OK);
  rc = sqlite3_open("testFetchPragmaDataVersion.db", &db2);
  assert(rc == SQLITE_OK);

  rc = sqlite3_exec(db1, "CREATE TABLE fpdv (a)", 0, 0, &errmsg);
  assert(rc == SQLITE_OK);

  unsigned char *siteIdBuffer =
      sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData1 = crsqlExtDataInit(db1, siteIdBuffer);
  siteIdBuffer = sqlite3_malloc(SITE_ID_LEN * sizeof *(siteIdBuffer));
  crsql_ExtData *pExtData2 = crsqlExtDataInit(db2, siteIdBuffer);

  rc = crsql_readPragmaDataVersion(pExtData1, &dataVersion1);
  assert(rc == SQLITE_OK);
  assert(dataVersion1 == pExtData1->pragmaDataVersion);
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 == pExtData2->pragmaDataVersion);

  // A connection's own writes do not change its data_version.
  rc = sqlite3_exec(db1, "INSERT INTO fpdv VALUES (1)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = crsql_readPragmaDataVersion(pExtData1, &dataVersion1);
  assert(rc == SQLITE_OK);
  assert(dataVersion1 == pExtData1->pragmaDataVersion);

  // Another connection's commit changes the value without changing the cache.
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 != pExtData2->pragmaDataVersion);
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 != pExtData2->pragmaDataVersion);
  pExtData2->pragmaDataVersion = dataVersion2;
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 == pExtData2->pragmaDataVersion);

  rc = sqlite3_exec(db2, "INSERT INTO fpdv VALUES (1)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 == pExtData2->pragmaDataVersion);

  rc = crsql_readPragmaDataVersion(pExtData1, &dataVersion1);
  assert(rc == SQLITE_OK);
  assert(dataVersion1 != pExtData1->pragmaDataVersion);
  pExtData1->pragmaDataVersion = dataVersion1;

  // A schema commit from another connection can invalidate the cached
  // PRAGMA statement. The helper must reprepare it before retrying.
  rc = sqlite3_exec(db1, "CREATE TABLE fpdv_schema_change (a)", 0, 0, 0);
  assert(rc == SQLITE_OK);
  rc = crsql_readPragmaDataVersion(pExtData2, &dataVersion2);
  assert(rc == SQLITE_OK);
  assert(dataVersion2 != pExtData2->pragmaDataVersion);
  rc = crsql_readPragmaDataVersion(pExtData1, &dataVersion1);
  assert(rc == SQLITE_OK);
  assert(dataVersion1 == pExtData1->pragmaDataVersion);

  crsql_finalize(pExtData1);
  crsql_freeExtData(pExtData1);
  crsql_close(db1);
  crsql_finalize(pExtData2);
  crsql_freeExtData(pExtData2);
  sqlite3_free(errmsg);
  errmsg = 0;
  crsql_close(db2);
  remove("testFetchPragmaDataVersion.db");
  printf("\t\e[0;32mSuccess\e[0m\n");
}

void crsqlExtDataTestSuite() {
  printf("\e[47m\e[1;30mSuite: crsql_ExtData\e[0m\n");
  textNewExtData();
  testFreeExtData();
  testFinalize();
  testReadPragmaSchemaVersion();
  testFetchPragmaDataVersion();
}
