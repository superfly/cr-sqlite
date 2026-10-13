#ifndef CRSQLITE_VERSION_GUARD_H
#define CRSQLITE_VERSION_GUARD_H

#if !defined(SQLITEINT_H)
#include "sqlite3ext.h"
#endif
SQLITE_EXTENSION_INIT3

#include "ext-data.h"

extern sqlite3_module crsql_versionGuardModule;
int crsql_enlist_db_version(sqlite3 *db, crsql_ExtData *pExtData);

#endif
