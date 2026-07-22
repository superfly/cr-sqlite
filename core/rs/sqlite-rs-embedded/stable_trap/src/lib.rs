#![no_std]
#![cfg_attr(feature = "nightly", feature(core_intrinsics))]

#[inline(always)]
pub fn abort() -> ! {
    a()
}

#[cfg(feature = "nightly")]
#[inline(always)]
fn a() -> ! {
    // SAFETY: abnormal termination
    unsafe { core::intrinsics::abort() }
}

#[cfg(not(feature = "nightly"))]
#[inline(always)]
fn a() -> ! {
    // SAFETY: all branches perform a non-returning trap/abort.
    unsafe {
        #[cfg(target_arch = "wasm32")]
        core::arch::wasm32::unreachable();
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("ud2", options(noreturn));
        #[cfg(target_arch = "x86")]
        core::arch::asm!("ud2", options(noreturn));
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("brk #1", options(noreturn));
        #[cfg(target_arch = "arm")]
        core::arch::asm!("udf #0", options(noreturn));
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm",
            target_arch = "wasm32"
        )))]
        {
            unsafe extern "C" {
                fn abort() -> !;
            }
            abort();
        }
    }
}
