#include "crsqlite.h"
SQLITE_EXTENSION_INIT1
#ifdef LIBSQL
LIBSQL_EXTENSION_INIT1
#endif

#include <assert.h>
#include <ctype.h>
#include <limits.h>
#include <stdint.h>
#include <string.h>

#include "changes-vtab.h"
#include "consts.h"
#include "ext-data.h"
#include "rust.h"

// see
// https://github.com/chromium/chromium/commit/579b3dd0ea41a40da8a61ab87a8b0bc39e158998
// & https://github.com/rust-lang/rust/issues/73632 &
// https://sourcegraph.com/github.com/chromium/chromium/-/commit/579b3dd0ea41a40da8a61ab87a8b0bc39e158998?visible=1
#ifdef CRSQLITE_WASM
unsigned char __rust_no_alloc_shim_is_unstable;
#endif

int crsql_commit_hook(void *pUserData);
void crsql_rollback_hook(void *pUserData);

#ifdef LIBSQL
static void closeHook(void *pUserData, sqlite3 *db) {
  crsql_ExtData *pExtData = (crsql_ExtData *)pUserData;
  crsql_finalize(pExtData);
}
#endif

void *sqlite3_crsqlrustbundle_init(sqlite3 *db, char **pzErrMsg,
                                   const sqlite3_api_routines *pApi);

#ifdef _WIN32
__declspec(dllexport)
#endif
    int sqlite3_crsqlite_init(sqlite3 *db, char **pzErrMsg,
                              const sqlite3_api_routines *pApi
#ifdef LIBSQL
                              ,
                              const libsql_api_routines *pLibsqlApi
#endif
    ) {
  int rc = SQLITE_OK;

  SQLITE_EXTENSION_INIT2(pApi);
#ifdef LIBSQL
  LIBSQL_EXTENSION_INIT2(pLibsqlApi);
#endif

  // TODO: should be moved lower once we finish migrating to rust.
  // RN it is safe here since the rust bundle init is largely just reigstering
  // function pointers. we need to init the rust bundle otherwise sqlite api
  // methods are not isntalled when we start calling rust
  crsql_ExtData *pExtData = sqlite3_crsqlrustbundle_init(db, pzErrMsg, pApi);
  if (pExtData == 0) {
    if (pzErrMsg != 0 && *pzErrMsg == 0) {
      *pzErrMsg = sqlite3_mprintf(
          "crsqlrustbundle_init failed to initialize extension data");
    }
    return SQLITE_ERROR;
  }

  rc = sqlite3_create_module_v2(db, "crsql_changes", &crsql_changesModule,
                                pExtData, 0);
  if (rc != SQLITE_OK) {
    // M32: On module creation failure, free the extension data so we do not
    // leave a half-loaded extension state. SQL functions registered by the
    // rust bundle init remain, but without a working module or commit/rollback
    // hooks the CRDT version tracking would silently break, so we bail out
    // rather than continuing with a partially initialized extension.
    crsql_finalize(pExtData);
    return rc;
  }

  {
#ifdef LIBSQL
    libsql_close_hook(db, closeHook, pExtData);
#endif
    // TODO: get the prior callback so we can call it rather than replace
    // it?
    // M33: crsqlite must be loaded before any other extension that installs
    // commit/rollback hooks. sqlite3_commit_hook / sqlite3_rollback_hook
    // unconditionally replace any pre-existing hooks, so loading crsqlite
    // after another extension that relies on those hooks will silently drop
    // the prior hooks and break that extension's behavior.
    sqlite3_commit_hook(db, crsql_commit_hook, pExtData);
    sqlite3_rollback_hook(db, crsql_rollback_hook, pExtData);
  }

  return rc;
}