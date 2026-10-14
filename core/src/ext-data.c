#include "ext-data.h"

#include <stdio.h>
#include <string.h>

#include "consts.h"

void crsql_clear_stmt_cache(crsql_ExtData *pExtData);
void crsql_init_table_info_vec(crsql_ExtData *pExtData);
void crsql_init_ordinal_map(crsql_ExtData *pExtData);
void crsql_drop_ordinal_map(crsql_ExtData *pExtData);
void crsql_drop_table_info_vec(crsql_ExtData *pExtData);
void crsql_init_last_db_versions_map(crsql_ExtData *pExtData);
void crsql_drop_last_db_versions_map(crsql_ExtData *pExtData);
// void crsql_init_table_info_vec(crsql_ExtData *pExtData);
// void crsql_drop_table_info_vec(crsql_ExtData *pExtData);

// The initialization here is incomplete! We need to call crsql_initSiteIdExt after this.
crsql_ExtData *crsql_newExtData(sqlite3 *db) {
  crsql_ExtData *pExtData = sqlite3_malloc(sizeof *pExtData);
  if (pExtData == 0) {
    return 0;
  }
  memset(pExtData, 0, sizeof *pExtData);

  pExtData->siteId = 0;
  pExtData->pPragmaSchemaVersionStmt = 0;
  int rc = sqlite3_prepare_v3(db, "PRAGMA schema_version", -1,
                              SQLITE_PREPARE_PERSISTENT,
                              &(pExtData->pPragmaSchemaVersionStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }
  pExtData->pPragmaDataVersionStmt = 0;
  rc = sqlite3_prepare_v3(db, "PRAGMA data_version", -1,
                           SQLITE_PREPARE_PERSISTENT,
                           &(pExtData->pPragmaDataVersionStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }
  pExtData->pSetSyncBitStmt = 0;
  rc = sqlite3_prepare_v3(db, SET_SYNC_BIT, -1, SQLITE_PREPARE_PERSISTENT,
                           &(pExtData->pSetSyncBitStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }
  pExtData->pClearSyncBitStmt = 0;
  rc = sqlite3_prepare_v3(db, CLEAR_SYNC_BIT, -1, SQLITE_PREPARE_PERSISTENT,
                           &(pExtData->pClearSyncBitStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }

  pExtData->pSetSiteIdOrdinalStmt = 0;

  pExtData->pSelectSiteIdOrdinalStmt = 0;

  pExtData->pSelectClockTablesStmt = 0;
  rc = sqlite3_prepare_v3(db, CLOCK_TABLES_SELECT, -1,
                          SQLITE_PREPARE_PERSISTENT,
                          &(pExtData->pSelectClockTablesStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }

  pExtData->dbVersion = -1;
  pExtData->pendingDbVersion = -1;

  pExtData->pSetDbVersionStmt = 0;
  rc = sqlite3_prepare_v3(
      db,
      "INSERT INTO crsql_db_versions (site_id, db_version) VALUES "
      "(?, ?)  ON "
      "CONFLICT (site_id) DO UPDATE SET db_version = excluded.db_version "
      "WHERE crsql_db_versions.db_version < excluded.db_version RETURNING db_version",
      -1, SQLITE_PREPARE_PERSISTENT, &(pExtData->pSetDbVersionStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }

  // printf("instantiated pSetDbVersionStmt, rc: %d\n", rc);

  pExtData->seq = 0;
  pExtData->pragmaDataVersion = -1;
  pExtData->pragmaSchemaVersion = -1;
  pExtData->pDbVersionStmt = 0;
  rc = sqlite3_prepare_v3(
      db,
      "SELECT db_version FROM crsql_db_versions WHERE site_id = ?",
      -1, SQLITE_PREPARE_PERSISTENT, &(pExtData->pDbVersionStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }

  pExtData->pConfigValueStmt = 0;
  rc = sqlite3_prepare_v3(
      db, "SELECT value FROM crsql_master WHERE key = ?", -1,
      SQLITE_PREPARE_PERSISTENT, &(pExtData->pConfigValueStmt), 0);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }

  pExtData->tableInfos = 0;
  pExtData->lastDbVersions = 0;
  pExtData->ordinalMap = 0;
  pExtData->pVersionGuardStmt = 0;
  pExtData->versionGuardEnlisted = 0;
  pExtData->rowsImpacted = 0;
  crsql_init_table_info_vec(pExtData);
  crsql_init_last_db_versions_map(pExtData);
  crsql_init_ordinal_map(pExtData);

  sqlite3_stmt *pStmt;

  // Use substr to strip the literal "config." prefix. Do NOT use
  // ltrim(key, 'config.') — SQLite's ltrim(X, Y) treats Y as a SET of
  // characters, not a prefix string, so any config key whose name portion
  // starts with one of c/o/n/f/i/g/. would be over-stripped and misparsed.
  rc = sqlite3_prepare_v2(db,
                           "SELECT substr(key, length('config.') + 1), value FROM "
                           "crsql_master WHERE key LIKE 'config.%';",
                           -1, &pStmt, 0);

  if (rc != SQLITE_OK) {
    sqlite3_finalize(pStmt);
    crsql_freeExtData(pExtData);
    return 0;
  }

  // set defaults!
  pExtData->mergeEqualValues = 0;
  pExtData->metadataWriteVersion = 1;  // V1
  pExtData->metadataUseVersion = 1;   // V1
  pExtData->syncLogVersion = 1;      // V1
  pExtData->defaultTimestamp = 0;    // off: require crsql_set_ts()

  while (sqlite3_step(pStmt) == SQLITE_ROW) {
    const unsigned char *name = sqlite3_column_text(pStmt, 0);
    int colType = sqlite3_column_type(pStmt, 1);

    if (strcmp("merge-equal-values", (char *)name) == 0) {
      if (colType == SQLITE_INTEGER) {
        const int value = sqlite3_column_int(pStmt, 1);
        pExtData->mergeEqualValues = value;
      } else {
        // broken setting...
        sqlite3_finalize(pStmt);
        crsql_freeExtData(pExtData);
        return 0;
      }
    } else if (strcmp("metadata-write-version", (char *)name) == 0) {
      if (colType == SQLITE_INTEGER) {
        pExtData->metadataWriteVersion = sqlite3_column_int(pStmt, 1);
      }
    } else if (strcmp("metadata-use-version", (char *)name) == 0) {
      if (colType == SQLITE_INTEGER) {
        pExtData->metadataUseVersion = sqlite3_column_int(pStmt, 1);
      }
    } else if (strcmp("sync-log-version", (char *)name) == 0) {
      if (colType == SQLITE_INTEGER) {
        pExtData->syncLogVersion = sqlite3_column_int(pStmt, 1);
      }
    } else {
      // unhandled config setting
    }
  }

  sqlite3_finalize(pStmt);
  int dataVersion = 0;
  rc = crsql_readPragmaDataVersion(pExtData, &dataVersion);
  if (rc != SQLITE_OK) {
    crsql_freeExtData(pExtData);
    return 0;
  }
  pExtData->pragmaDataVersion = dataVersion;

  return pExtData;
}

int crsql_initSiteIdExt(sqlite3 *db, crsql_ExtData *pExtData, unsigned char *siteIdBuffer) {
  pExtData->siteId = siteIdBuffer;

  pExtData->pSetSiteIdOrdinalStmt = 0;
  int rc = sqlite3_prepare_v3(
      db, "INSERT INTO crsql_site_id (site_id) VALUES (?) RETURNING ordinal",
      -1, SQLITE_PREPARE_PERSISTENT, &(pExtData->pSetSiteIdOrdinalStmt), 0);
  if (rc != SQLITE_OK) {
    return rc;
  }

  pExtData->pSelectSiteIdOrdinalStmt = 0;
  rc = sqlite3_prepare_v3(
      db, "SELECT ordinal FROM crsql_site_id WHERE site_id = ?", -1,
      SQLITE_PREPARE_PERSISTENT, &(pExtData->pSelectSiteIdOrdinalStmt), 0);
  if (rc != SQLITE_OK) {
    return rc;
  }

  return rc;
}

void crsql_freeExtData(crsql_ExtData *pExtData) {
  if (pExtData == 0) {
    return;
  }
  // printf("free ext\n");
  if (pExtData->siteId != 0) {
    sqlite3_free(pExtData->siteId);
  }
  sqlite3_finalize(pExtData->pDbVersionStmt);
  sqlite3_finalize(pExtData->pSetDbVersionStmt);
  sqlite3_finalize(pExtData->pVersionGuardStmt);
  sqlite3_finalize(pExtData->pPragmaSchemaVersionStmt);
  sqlite3_finalize(pExtData->pPragmaDataVersionStmt);
  sqlite3_finalize(pExtData->pConfigValueStmt);
  sqlite3_finalize(pExtData->pSetSyncBitStmt);
  sqlite3_finalize(pExtData->pClearSyncBitStmt);
  sqlite3_finalize(pExtData->pSetSiteIdOrdinalStmt);
  if (pExtData->pSelectClockTablesStmt != 0) {
    sqlite3_finalize(pExtData->pSelectClockTablesStmt);
  }
  if (pExtData->pSelectSiteIdOrdinalStmt != 0) {
    sqlite3_finalize(pExtData->pSelectSiteIdOrdinalStmt);
  }
  pExtData->pDbVersionStmt = 0;
  pExtData->pSetDbVersionStmt = 0;
  pExtData->pVersionGuardStmt = 0;
  pExtData->pPragmaSchemaVersionStmt = 0;
  pExtData->pPragmaDataVersionStmt = 0;
  pExtData->pConfigValueStmt = 0;
  pExtData->pSetSyncBitStmt = 0;
  pExtData->pClearSyncBitStmt = 0;
  pExtData->pSetSiteIdOrdinalStmt = 0;
  pExtData->pSelectSiteIdOrdinalStmt = 0;
  pExtData->pSelectClockTablesStmt = 0;
  crsql_clear_stmt_cache(pExtData);
  crsql_drop_table_info_vec(pExtData);
  crsql_drop_last_db_versions_map(pExtData);
  crsql_drop_ordinal_map(pExtData);
  sqlite3_free(pExtData);
}

// Should _only_ be called when disconnecting from the db
// for some reason finalization in extension unload methods doesn't
// work as expected
// see https://sqlite.org/forum/forumpost/c94f943821
// `freeExtData` is called after finalization when the extension unloads
void crsql_finalize(crsql_ExtData *pExtData) {
  if (pExtData == 0) {
    return;
  }
  // printf("crsql_finalize\n");
  sqlite3_finalize(pExtData->pDbVersionStmt);
  sqlite3_finalize(pExtData->pSetDbVersionStmt);
  sqlite3_finalize(pExtData->pVersionGuardStmt);
  sqlite3_finalize(pExtData->pPragmaSchemaVersionStmt);
  sqlite3_finalize(pExtData->pPragmaDataVersionStmt);
  sqlite3_finalize(pExtData->pConfigValueStmt);
  sqlite3_finalize(pExtData->pSetSyncBitStmt);
  sqlite3_finalize(pExtData->pClearSyncBitStmt);
  if (pExtData->pSetSiteIdOrdinalStmt != 0) {
    sqlite3_finalize(pExtData->pSetSiteIdOrdinalStmt);
  }
  if (pExtData->pSelectSiteIdOrdinalStmt != 0) {
    sqlite3_finalize(pExtData->pSelectSiteIdOrdinalStmt);
  }
  sqlite3_finalize(pExtData->pSelectClockTablesStmt);
  crsql_clear_stmt_cache(pExtData);
  pExtData->pDbVersionStmt = 0;
  pExtData->pSetDbVersionStmt = 0;
  pExtData->pVersionGuardStmt = 0;
  pExtData->pPragmaSchemaVersionStmt = 0;
  pExtData->pPragmaDataVersionStmt = 0;
  pExtData->pConfigValueStmt = 0;
  pExtData->pSetSyncBitStmt = 0;
  pExtData->pClearSyncBitStmt = 0;
  pExtData->pSetSiteIdOrdinalStmt = 0;
  pExtData->pSelectSiteIdOrdinalStmt = 0;
  pExtData->pSelectClockTablesStmt = 0;
}

int crsql_readPragmaSchemaVersion(crsql_ExtData *pExtData, int *schemaVersion) {
  if (!schemaVersion) return SQLITE_MISUSE;
  int rc = sqlite3_step(pExtData->pPragmaSchemaVersionStmt);
  if (rc != SQLITE_ROW) {
    int resetRc = sqlite3_reset(pExtData->pPragmaSchemaVersionStmt);
    if (resetRc != SQLITE_OK) return resetRc;
    return rc == SQLITE_DONE ? SQLITE_ERROR : rc;
  }

  *schemaVersion = sqlite3_column_int(pExtData->pPragmaSchemaVersionStmt, 0);
  return sqlite3_reset(pExtData->pPragmaSchemaVersionStmt);
}

int crsql_readPragmaDataVersion(crsql_ExtData *pExtData, int *dataVersion) {
  if (!dataVersion) return SQLITE_MISUSE;
  int rc = sqlite3_step(pExtData->pPragmaDataVersionStmt);
  if (rc != SQLITE_ROW) {
    int resetRc = sqlite3_reset(pExtData->pPragmaDataVersionStmt);
    if (resetRc != SQLITE_OK) return resetRc;
    return rc == SQLITE_DONE ? SQLITE_ERROR : rc;
  }

  *dataVersion = sqlite3_column_int(pExtData->pPragmaDataVersionStmt, 0);
  return sqlite3_reset(pExtData->pPragmaDataVersionStmt);
}
