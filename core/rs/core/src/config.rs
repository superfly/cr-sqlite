use alloc::format;

use sqlite::{Connection, Context};
use sqlite_nostd as sqlite;
use sqlite_nostd::{ColumnType, ManagedStmt, ResultCode, Value};

use crate::c::crsql_ExtData;

pub const MERGE_EQUAL_VALUES: &str = "merge-equal-values";

pub fn reload_connection_config(
    db: *mut sqlite::sqlite3,
    ext_data: *mut crsql_ExtData,
) -> Result<(), alloc::string::String> {
    let stmt = db
        .prepare_v2("SELECT value FROM crsql_master WHERE key = 'config.merge-equal-values'")
        .map_err(|rc| format!("failed to prepare config refresh: {rc}"))?;
    let value = match stmt.step() {
        Ok(ResultCode::ROW) => {
            if stmt.column_type(0) != Ok(ColumnType::Integer) {
                return Err("invalid merge-equal-values setting".into());
            }
            stmt.column_int(0)
        }
        Ok(ResultCode::DONE) => 0,
        Ok(rc) | Err(rc) => return Err(format!("failed to read config: {rc}")),
    };
    unsafe { (*ext_data).mergeEqualValues = value };
    Ok(())
}

pub extern "C" fn crsql_config_set(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    let name = args[0].text();

    let value = match name {
        MERGE_EQUAL_VALUES => {
            let value = args[1];
            let ext_data = ctx.user_data() as *mut crsql_ExtData;
            unsafe { (*ext_data).mergeEqualValues = value.int() };
            value
        }
        _ => {
            ctx.result_error("Unknown setting name");
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    };

    let db = ctx.db_handle();
    match insert_config_setting(db, name, value) {
        Ok((_stmt, value)) => {
            ctx.result_value(value);
        }
        Err(rc) => {
            ctx.result_error("Could not persist config in database");
            ctx.result_error_code(rc);
            return;
        }
    }
}

fn insert_config_setting(
    db: *mut sqlite_nostd::sqlite3,
    name: &str,
    value: *mut sqlite::value,
) -> Result<(ManagedStmt, *mut sqlite::value), ResultCode> {
    let stmt =
        db.prepare_v2("INSERT OR REPLACE INTO crsql_master VALUES (?, ?) RETURNING value")?;

    stmt.bind_text(1, &format!("config.{name}"), sqlite::Destructor::TRANSIENT)?;
    stmt.bind_value(2, value)?;

    if let ResultCode::ROW = stmt.step()? {
        let res = stmt.column_value(0)?;
        // Res will get invalidated when stmt gets dropped
        // The lifetime of res is not currently checked by the compiler
        Ok((stmt, res))
    } else {
        Err(ResultCode::ERROR)
    }
}

pub extern "C" fn crsql_config_get(
    ctx: *mut sqlite::context,
    argc: i32,
    argv: *mut *mut sqlite::value,
) {
    let args = sqlite::args!(argc, argv);

    let name = args[0].text();
    let ext_data = ctx.user_data() as *mut crsql_ExtData;
    if let Err(message) = crate::ext_data::refresh_connection_state_if_needed(ctx.db_handle(), ext_data) {
        ctx.result_error(&message);
        return;
    }

    match name {
        MERGE_EQUAL_VALUES => {
            ctx.result_int(unsafe { (*ext_data).mergeEqualValues });
        }
        _ => {
            ctx.result_error("Unknown setting name");
            ctx.result_error_code(ResultCode::ERROR);
            return;
        }
    }
}
