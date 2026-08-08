//! CCOMPAT stage CC3 fixture program: a minimal, separately-linked C-ABI
//! program that proves the spawn round-trip end to end.
//!
//! The kernel-side test (`tests/integration/spawn_program_qemu_*`) builds this
//! program into a fresh user address space with
//! `tairix_kernel_mem::build_process_image` — passing an argument vector
//! `["prog", "<N>"]` — and drops into user mode through the Arch HAL
//! `tairix_arch_api::EnterUser` primitive. Control arrives at crt0's `_start`
//! (`tairix_crt0`), which marshals the kernel's startup vector into C
//! `argc`/`argv`/`envp`, installs the stack canary, and calls the [`main`]
//! below. `main` parses `argv[1]` as a decimal integer and returns it; crt0
//! routes that return value through the `exit` syscall
//! (`tairix_abi_sys::sys_exit`). The test asserts the kernel-observed `exit`
//! code equals `N`, proving argv marshalling and program teardown across the
//! whole curated *System runtime / C ABI* class.
//!
//! The program links **only** `tairix-crt0` and `tairix-abi-sys` — never an
//! architecture crate — so its `_start` is crt0's program entry trampoline,
//! not a kernel boot vector (the two would collide). It is built position-
//! independent and converted to an `rxe` blob by the consuming test's build
//! script.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Freestanding C-ABI program -----------------------------------------
#[cfg(freestanding)]
mod program {
    use core::ffi::{c_char, c_int};
    use core::panic::PanicInfo;

    // The program's entry point is crt0's `_start` (`tairix_crt0`), but the
    // program never names the crate in a Rust path — crt0 only provides the
    // startup trampoline that *calls into* this program, not the other way
    // round. rustc links a dependency's rlib only when the crate is
    // referenced, so without this `extern crate` the crt0 rlib (and its
    // `_start`) would be dropped from the link and the entry point would be
    // undefined. Naming it here forces the rlib onto the link line; the link
    // script's `ENTRY(_start)` then roots the trampoline. The crate is named
    // solely for its link-time side effect, so the rust-2018 "unused extern
    // crate" idiom lint does not apply.
    #[allow(unused_extern_crates)]
    extern crate tairix_crt0;

    /// Exit code returned when the program is spawned without the single
    /// decimal argument it expects (`argc < 2`). A reserved, fail-closed
    /// value distinct from the small codes the round-trip exercises.
    const EXIT_MISSING_ARG: c_int = 71;

    /// Exit code returned when `argv[1]` is not a well-formed non-negative
    /// decimal integer in range. Also reserved and fail-closed.
    const EXIT_BAD_ARG: c_int = 72;

    /// Program entry point, called by crt0 once the C runtime is set up.
    ///
    /// Parses the C argument vector's `[1]` entry as a non-negative decimal
    /// integer and returns it as the program's exit code. crt0 routes the
    /// return value through the `exit` syscall.
    ///
    /// # Safety
    ///
    /// `arg_vector` must be a valid NULL-terminated array of at least
    /// `arg_count` NUL-terminated C strings, as crt0 guarantees by
    /// construction.
    #[no_mangle]
    pub unsafe extern "C" fn main(
        arg_count: c_int,
        arg_vector: *const *const c_char,
        _envp: *const *const c_char,
    ) -> c_int {
        if arg_count < 2 || arg_vector.is_null() {
            return EXIT_MISSING_ARG;
        }
        // SAFETY: the caller guarantees entry `[1]` is a valid C string
        // pointer (`arg_count >= 2`); the vector is a readable array of
        // `arg_count` pointers.
        let decimal_arg = unsafe { *arg_vector.add(1) };
        if decimal_arg.is_null() {
            return EXIT_MISSING_ARG;
        }
        // SAFETY: `decimal_arg` is a NUL-terminated C string by crt0's
        // contract.
        match unsafe { parse_decimal(decimal_arg) } {
            Some(value) => value,
            None => EXIT_BAD_ARG,
        }
    }

    /// Parse a NUL-terminated C string as a non-negative decimal integer,
    /// returning `None` on an empty string, a non-digit byte, or overflow of
    /// the `c_int` range. Panic-free.
    ///
    /// # Safety
    ///
    /// `s` must point at a NUL-terminated, readable C string.
    unsafe fn parse_decimal(s: *const c_char) -> Option<c_int> {
        // C's `char` signedness is target-dependent, so scan the string as
        // the raw bytes it is rather than through `c_char`.
        let bytes = s.cast::<u8>();
        let mut acc: i32 = 0;
        let mut seen = false;
        let mut i = 0usize;
        loop {
            // SAFETY: the caller guarantees `s` is NUL-terminated, so the scan
            // stops at or before the terminator; each read is in bounds, and a
            // `u8` read of a `c_char` byte has the same layout and alignment.
            let byte = unsafe { *bytes.add(i) };
            if byte == 0 {
                break;
            }
            if !byte.is_ascii_digit() {
                return None;
            }
            let digit = i32::from(byte - b'0');
            acc = acc.checked_mul(10)?.checked_add(digit)?;
            seen = true;
            i += 1;
        }
        if seen {
            Some(acc)
        } else {
            None
        }
    }

    /// Panic handler: a hosted program has no unwinder, so a panic is an
    /// unrecoverable fault. Terminate through the `exit` syscall with the
    /// reserved bad-argument code rather than returning to corrupt state
    /// (fail closed). The program is written to be
    /// panic-free; this exists only to satisfy the `no_std` contract.
    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        tairix_abi_sys::sys_exit(EXIT_BAD_ARG)
    }
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program is an
// inert binary so the workspace tooling still covers this crate; the
// freestanding C-ABI body above compiles only for the native targets.
#[cfg(not(freestanding))]
fn main() {}
