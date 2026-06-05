//! CCOMPAT stage CC3 fixture program: a minimal, separately-linked C-ABI
//! program that proves the spawn round-trip end to end.
//!
//! The kernel-side test (`tests/integration/spawn_program_qemu_*`) builds this
//! program into a fresh user address space with
//! `rustos_kernel_mem::build_process_image` — passing an argument vector
//! `["prog", "<N>"]` — and drops into user mode through the Arch HAL
//! `rustos_arch_api::EnterUser` primitive. Control arrives at crt0's `_start`
//! (`rustos_crt0`), which marshals the kernel's startup vector into C
//! `argc`/`argv`/`envp`, installs the stack canary, and calls the [`main`]
//! below. `main` parses `argv[1]` as a decimal integer and returns it; crt0
//! routes that return value through the `exit` syscall
//! (`rustos_abi_sys::sys_exit`). The test asserts the kernel-observed `exit`
//! code equals `N`, proving argv marshalling and program teardown across the
//! whole curated *System runtime / C ABI* class (`AGENTS.md` §16.4).
//!
//! The program links **only** `rustos-crt0` and `rustos-abi-sys` — never an
//! architecture crate — so its `_start` is crt0's program entry trampoline,
//! not a kernel boot vector (the two would collide). It is built position-
//! independent and converted to an `rxe` blob by the consuming test's build
//! script (`AGENTS.md` §9, §19.2).

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Freestanding C-ABI program -----------------------------------------
#[cfg(freestanding)]
mod program {
    use core::ffi::{c_char, c_int};
    use core::panic::PanicInfo;

    // The program's entry point is crt0's `_start` (`rustos_crt0`), but the
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
    extern crate rustos_crt0;

    /// Exit code returned when the program is spawned without the single
    /// decimal argument it expects (`argc < 2`). A reserved, fail-closed
    /// value distinct from the small codes the round-trip exercises
    /// (`AGENTS.md` §2.9).
    const EXIT_MISSING_ARG: c_int = 71;

    /// Exit code returned when `argv[1]` is not a well-formed non-negative
    /// decimal integer in range. Also reserved and fail-closed.
    const EXIT_BAD_ARG: c_int = 72;

    /// Program entry point, called by crt0 once the C runtime is set up.
    ///
    /// Parses `argv[1]` as a non-negative decimal integer and returns it as
    /// the program's exit code. crt0 routes the return value through the
    /// `exit` syscall.
    ///
    /// # Safety
    ///
    /// `argv` must be a valid NULL-terminated array of at least `argc`
    /// NUL-terminated C strings, as crt0 guarantees by construction.
    #[no_mangle]
    pub unsafe extern "C" fn main(
        argc: c_int,
        argv: *const *const c_char,
        _envp: *const *const c_char,
    ) -> c_int {
        if argc < 2 || argv.is_null() {
            return EXIT_MISSING_ARG;
        }
        // SAFETY: the caller guarantees `argv[1]` is a valid C string pointer
        // (`argc >= 2`); `argv` is a readable array of `argc` pointers.
        let arg1 = unsafe { *argv.add(1) };
        if arg1.is_null() {
            return EXIT_MISSING_ARG;
        }
        // SAFETY: `arg1` is a NUL-terminated C string by crt0's contract.
        match unsafe { parse_decimal(arg1) } {
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
        let mut acc: i32 = 0;
        let mut seen = false;
        let mut i = 0usize;
        loop {
            // SAFETY: the caller guarantees `s` is NUL-terminated, so the scan
            // stops at or before the terminator; each read is in bounds.
            let byte = unsafe { *s.add(i) } as u8;
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
    /// (`AGENTS.md` §2.9 — fail closed). The program is written to be
    /// panic-free; this exists only to satisfy the `no_std` contract.
    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        rustos_abi_sys::sys_exit(EXIT_BAD_ARG)
    }
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program is an
// inert binary so the workspace tooling still covers this crate; the
// freestanding C-ABI body above compiles only for the native targets.
#[cfg(not(freestanding))]
fn main() {}
