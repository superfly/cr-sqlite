/**
 * The version guard virtual table exists to defer the db_version write to
 * just before commit. The first local write of a transaction enlists it via
 * crsql_enlist_db_version; its xSync then upserts the pending db_version
 * into crsql_db_versions atomically before the transaction commits. SQLite
 * guarantees xSync runs before either xCommit or xRollback, so the version
 * row is never persisted for a transaction that rolls back. The guard only
 * ever manages this node's own version, so xSync reads the site id and
 * pending db_version from the connection's ExtData rather than taking
 * parameters.
 *
 * Persisting the version marker in xSync means a statement or savepoint
 * rollback can no longer leave clocks committed under a db_version whose
 * crsql_db_versions row was rolled back. A committed transaction may spend
 * a version even when all of its writes were rolled back to a savepoint;
 * such gaps are harmless while version reuse is not.
 */
#include "version-guard.h"

#include <string.h>

#include "consts.h"

typedef struct {
  sqlite3_vtab base;
  crsql_ExtData *extData;
} crsql_VersionGuard;

static int versionGuardConnect(sqlite3 *db, void *aux, int argc,
                               const char *const *argv, sqlite3_vtab **vtab,
                               char **err) {
  int rc = sqlite3_declare_vtab(db, "CREATE TABLE x(dummy INTEGER)");
  if (rc != SQLITE_OK) return rc;
  crsql_VersionGuard *guard = sqlite3_malloc(sizeof(*guard));
  if (!guard) return SQLITE_NOMEM;
  memset(guard, 0, sizeof(*guard));
  guard->extData = aux;
  *vtab = &guard->base;
  return SQLITE_OK;
}

static int versionGuardDisconnect(sqlite3_vtab *vtab) {
  sqlite3_free(vtab);
  return SQLITE_OK;
}

static int versionGuardBestIndex(sqlite3_vtab *vtab, sqlite3_index_info *info) {
  return SQLITE_OK;
}

static int versionGuardOpen(sqlite3_vtab *vtab, sqlite3_vtab_cursor **cursor) {
  sqlite3_vtab_cursor *c = sqlite3_malloc(sizeof(*c));
  if (!c) return SQLITE_NOMEM;
  memset(c, 0, sizeof(*c));
  c->pVtab = vtab;
  *cursor = c;
  return SQLITE_OK;
}

static int versionGuardClose(sqlite3_vtab_cursor *cursor) {
  sqlite3_free(cursor);
  return SQLITE_OK;
}

static int versionGuardFilter(sqlite3_vtab_cursor *cursor, int idxNum,
                              const char *idxStr, int argc,
                              sqlite3_value **argv) {
  return SQLITE_OK;
}

static int versionGuardNext(sqlite3_vtab_cursor *cursor) { return SQLITE_OK; }
static int versionGuardEof(sqlite3_vtab_cursor *cursor) { return 1; }
static int versionGuardColumn(sqlite3_vtab_cursor *cursor, sqlite3_context *ctx,
                              int column) {
  sqlite3_result_null(ctx);
  return SQLITE_OK;
}
static int versionGuardRowid(sqlite3_vtab_cursor *cursor, sqlite3_int64 *rowid) {
  *rowid = 0;
  return SQLITE_OK;
}

static int versionGuardBegin(sqlite3_vtab *vtab) { return SQLITE_OK; }

static int versionGuardUpdate(sqlite3_vtab *vtab, int argc,
                              sqlite3_value **argv, sqlite3_int64 *rowid) {
  // The insert exists only to enlist this vtab in the transaction so that
  // its xSync runs before commit; the values themselves are unused.
  return SQLITE_OK;
}

static int versionGuardSync(sqlite3_vtab *vtab) {
  crsql_VersionGuard *guard = (crsql_VersionGuard *)vtab;
  crsql_ExtData *extData = guard->extData;
  if (!extData->versionGuardEnlisted) return SQLITE_OK;
  sqlite3_int64 version = extData->pendingDbVersion;
  sqlite3_stmt *stmt = extData->pSetDbVersionStmt;
  int rc = sqlite3_bind_blob(stmt, 1, extData->siteId, SITE_ID_LEN,
                             SQLITE_STATIC);
  if (rc == SQLITE_OK) rc = sqlite3_bind_int64(stmt, 2, version);
  if (rc == SQLITE_OK) rc = sqlite3_step(stmt);
  int result = SQLITE_OK;
  if (rc == SQLITE_ROW) {
    if (sqlite3_column_int64(stmt, 0) != version)
      result = SQLITE_CONSTRAINT;
  } else if (rc == SQLITE_DONE) {
    stmt = extData->pDbVersionStmt;
    rc = sqlite3_bind_blob(stmt, 1, extData->siteId, SITE_ID_LEN, SQLITE_STATIC);
    if (rc == SQLITE_OK) rc = sqlite3_step(stmt);
    if (rc != SQLITE_ROW || sqlite3_column_int64(stmt, 0) != version)
      result = rc == SQLITE_ROW || rc == SQLITE_DONE ? SQLITE_CONSTRAINT : rc;
    rc = sqlite3_reset(stmt);
    if (result == SQLITE_OK) result = rc;
    stmt = extData->pSetDbVersionStmt;
  } else {
    result = rc;
  }
  rc = sqlite3_reset(stmt);
  return result == SQLITE_OK ? rc : result;
}

static int versionGuardEnd(sqlite3_vtab *vtab) {
  crsql_VersionGuard *guard = (crsql_VersionGuard *)vtab;
  guard->extData->versionGuardEnlisted = 0;
  return SQLITE_OK;
}

sqlite3_module crsql_versionGuardModule = {
    .iVersion = 2,
    .xConnect = versionGuardConnect,
    .xBestIndex = versionGuardBestIndex,
    .xDisconnect = versionGuardDisconnect,
    .xOpen = versionGuardOpen,
    .xClose = versionGuardClose,
    .xFilter = versionGuardFilter,
    .xNext = versionGuardNext,
    .xEof = versionGuardEof,
    .xColumn = versionGuardColumn,
    .xRowid = versionGuardRowid,
    .xUpdate = versionGuardUpdate,
    .xBegin = versionGuardBegin,
    .xSync = versionGuardSync,
    .xCommit = versionGuardEnd,
    .xRollback = versionGuardEnd,
};

int crsql_enlist_db_version(sqlite3 *db, crsql_ExtData *extData) {
  if (!extData->pVersionGuardStmt) {
    int rc = sqlite3_prepare_v3(
        db, "INSERT INTO crsql_version_guard VALUES (NULL)", -1,
        SQLITE_PREPARE_PERSISTENT, &extData->pVersionGuardStmt, 0);
    if (rc != SQLITE_OK) return rc;
  }
  sqlite3_stmt *stmt = extData->pVersionGuardStmt;
  int rc = sqlite3_step(stmt);
  int result = rc == SQLITE_DONE ? SQLITE_OK : rc;
  rc = sqlite3_reset(stmt);
  if (result != SQLITE_OK) return result;
  if (rc != SQLITE_OK) return rc;
  extData->versionGuardEnlisted = 1;
  return SQLITE_OK;
}
