#ifndef CRSQLITE_H
#define CRSQLITE_H

/**
 * Extension initialization routine that is run once per connection.
 *
 * When LIBSQL is defined the init routine takes an additional
 * libsql_api_routines argument (provided by the libsql fork's sqlite3ext.h).
 */
int sqlite3_crsqlite_init(sqlite3 *db, char **pzErrMsg,
                          const sqlite3_api_routines *pApi
#ifdef LIBSQL
                          ,
                          const libsql_api_routines *pLibsqlApi
#endif
);

#endif