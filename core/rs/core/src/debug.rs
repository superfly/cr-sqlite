use core::ffi::{c_char, c_void};
use sqlite::{context, value};
use sqlite_nostd as sqlite;

pub const DEBUG_CALLBACK_TYPE: &[u8] = b"crsql_debug_callback\0";

#[repr(C)]
#[derive(Copy, Clone)]
pub struct CrsqlDebugCallback {
    pub callback:
        Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>,
    pub context: *mut c_void,
}

// Debug state is process-global because debug_log is called from code paths that
// do not have a connection-specific ExtData pointer available.
static mut DEBUG_ENABLED: bool = false;
static mut DEBUG_CALLBACK: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)> = None;
static mut DEBUG_CALLBACK_CONTEXT: *mut c_void = core::ptr::null_mut();

pub fn debug_log(msg: &str) {
    unsafe {
        if DEBUG_ENABLED {
            if let Some(callback) = DEBUG_CALLBACK {
                callback(DEBUG_CALLBACK_CONTEXT, msg.as_ptr(), msg.len());
            } else {
                libc_print::libc_println!("[DEBUG] {}", msg);
            }
        }
    }
}

pub unsafe extern "C" fn x_crsql_set_debug_callback(
    ctx: *mut context,
    argc: i32,
    argv: *mut *mut value,
) {
    if argc != 1 {
        let msg = b"crsql_set_debug_callback expects one pointer argument";
        sqlite::result_error(ctx, msg.as_ptr() as *mut c_char, msg.len() as i32);
        return;
    }

    let registration = sqlite::value_pointer(
        *argv,
        DEBUG_CALLBACK_TYPE.as_ptr() as *mut c_char,
    ) as *mut CrsqlDebugCallback;

    if registration.is_null() {
        DEBUG_CALLBACK = None;
        DEBUG_CALLBACK_CONTEXT = core::ptr::null_mut();
    } else {
        DEBUG_CALLBACK = (*registration).callback;
        DEBUG_CALLBACK_CONTEXT = (*registration).context;
    }

    sqlite::result_int(ctx, 1);
}

pub unsafe extern "C" fn x_crsql_set_debug(ctx: *mut context, argc: i32, argv: *mut *mut value) {
    if argc == 0 {
        // If no arguments, return current state
        sqlite::result_int(ctx, if DEBUG_ENABLED { 1 } else { 0 });
        return;
    }

    if argc > 1 {
        // Too many arguments
        return;
    }

    let enabled = {
        let arg = *argv;
        sqlite::value_int(arg) != 0
    };

    DEBUG_ENABLED = enabled;

    // Return success (the new state)
    sqlite::result_int(ctx, if enabled { 1 } else { 0 });
}
