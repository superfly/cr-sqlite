#![no_std]

extern crate alloc;

use core::ffi::c_char;
use core::panic::PanicInfo;
use crsql_core;
use crsql_core::sqlite3_crsqlcore_init;
#[cfg(feature = "test")]
pub use crsql_core::test_exports;
use crsql_fractindex_core::sqlite3_crsqlfractionalindex_init;
#[cfg(feature = "test")]
use libc_print::std_name::println;
use sqlite_nostd as sqlite;
use sqlite_nostd::SQLite3Allocator;

// This must be our allocator so we can transfer ownership of memory to SQLite and have SQLite free that memory for us.
// This drastically reduces copies when passing strings and blobs back and forth between Rust and C.
#[global_allocator]
static ALLOCATOR: SQLite3Allocator = SQLite3Allocator {};

// This must be our panic handler for WASM builds. For simplicity, we make it our panic handler for
// all builds. Abort is also more portable than unwind, enabling us to go to more embedded use cases.
#[cfg(not(feature = "test"))]
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    stable_trap::abort()
}

// Print panic info for tests
#[cfg(feature = "test")]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("PANIC!: {}", info);
    stable_trap::abort()
}

// Otherwise we would need to use nightly features
#[cfg(not(target_family = "wasm"))]
#[no_mangle]
extern "C" fn rust_eh_personality() {}
#[cfg(target_arch = "arm")]
#[no_mangle]
extern "C" fn _rust_eh_personality() {}

#[cfg(target_family = "wasm")]
#[no_mangle]
pub fn __rust_alloc_error_handler(_: Layout) -> ! {
    stable_trap::abort()
}

#[no_mangle]
pub extern "C" fn sqlite3_crsqlrustbundle_init(
    db: *mut sqlite::sqlite3,
    err_msg: *mut *mut c_char,
    api: *mut sqlite::api_routines,
) -> *mut ::core::ffi::c_void {
    sqlite::EXTENSION_INIT2(api);

    let rc = sqlite3_crsqlfractionalindex_init(db, err_msg, api);
    if rc != 0 {
        return core::ptr::null_mut();
    }

    sqlite3_crsqlcore_init(db, err_msg, api)
}
