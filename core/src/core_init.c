/*
  This file is appended to the end of a sqlite3.c amalgammation
  file to include crsqlite functions statically in
  a build. This is used for the demo CLI and WASM implementations.
*/
#include "ext.h"

#ifdef LIBSQL
// M34: sqlite3_auto_extension only provides 3 arguments to the callback, but
// the LIBSQL build of sqlite3_crsqlite_init expects a 4th libsql_api_routines
// argument. This shim adapts the 3-parameter auto_extension callback to the
// 4-parameter init function, passing NULL for the libsql api routines.
static int crsqlite_init_shim(sqlite3 *db, char **pzErrMsg,
                              const sqlite3_api_routines *pApi) {
  return sqlite3_crsqlite_init(db, pzErrMsg, pApi, 0);
}
#endif

int core_init(const char *dummy) {
  // L61: dummy is unused but kept for ABI compatibility with the static
  // amalgamation init entry point.
  (void)dummy;
#ifdef LIBSQL
  return sqlite3_auto_extension((void *)crsqlite_init_shim);
#else
  return sqlite3_auto_extension((void *)sqlite3_crsqlite_init);
#endif
}
