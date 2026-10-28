extern crate alloc;

use sqlite_nostd::SQLite3Allocator;
#[global_allocator]
static ALLOCATOR: SQLite3Allocator = SQLite3Allocator {};

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    stable_trap::abort()
}

use core::alloc::Layout;

#[no_mangle]
pub fn __rust_alloc_error_handler(_: Layout) -> ! {
    stable_trap::abort()
}
