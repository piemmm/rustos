//! SPAWN stage `SP6b` fixture program: a minimal, separately-linked pure-Rust
//! EL0 program built in two roles from one source.
//!
//! The consuming vertical (`tests/integration/wait_qemu_aarch64`) compiles
//! this program twice — once as the **child** and once as the **parent** —
//! into two separate, hardware-isolated EL0 address spaces and drives them
//! under the live scheduler (`plans/SPAWN.md` `SP6b`):
//!
//! * the **child** simply returns the build-pinned `CHILD_CODE`, which the
//!   runtime routes through the `exit` syscall, so it terminates with that
//!   exact status;
//! * the **parent** calls `rustos_rt::wait(WAIT_ANY, &mut status)` — a real
//!   `wait` syscall the kernel blocks on until the child exits — reaps the
//!   child, and verifies the reaped exit code is `CHILD_CODE`, returning 0 on
//!   success and a distinct non-zero diagnostic code otherwise.
//!
//! The vertical asserts the parent reaped the child and exited 0, proving the
//! `wait` blocking-reap path end to end (the `SP6b` "done when").
//!
//! It is a **pure-Rust** program: it links the Rust userland
//! runtime `rustos-rt` (which provides `_start`, the stack canary, the panic
//! handler, and the `wait`/`exit` syscall wrappers), never the C ABI
//! (`crt0` + `abi-sys`), which exists solely for non-Rust programs. It is built position-independent and converted to an
//! `rxe` blob by the consuming test's build script. On
//! the host it is an inert stub so `cargo build --workspace`, clippy, and fmt
//! still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    use rustos_abi::WAIT_ANY;

    /// Exit code the child terminates with and the parent expects, used when
    /// the consuming build did not pin one via `RUSTOS_WAIT_CHILD_CODE`. A
    /// non-zero, non-trivial value so an accidental zero-exit cannot satisfy
    /// the parent's check.
    const DEFAULT_CHILD_CODE: i32 = 7;

    /// The child's exit code, read from `RUSTOS_WAIT_CHILD_CODE` (the value
    /// the consuming vertical's build script pins when it compiles both
    /// roles), falling back to [`DEFAULT_CHILD_CODE`]. The vertical is the
    /// single source of truth for the code.
    const CHILD_CODE: i32 = match option_env!("RUSTOS_WAIT_CHILD_CODE") {
        Some(s) => parse_i32(s.as_bytes()),
        None => DEFAULT_CHILD_CODE,
    };

    /// `true` when this build is the parent role, selected by
    /// `RUSTOS_WAIT_ROLE == "parent"`; any other value (including the child
    /// role and an absent variable) builds the child.
    const IS_PARENT: bool = match option_env!("RUSTOS_WAIT_ROLE") {
        Some(s) => bytes_eq(s.as_bytes(), b"parent"),
        None => false,
    };

    /// Compile-time byte-string equality (no `core::cmp` in `const`).
    const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Parse `bytes` as a non-negative decimal integer at compile time,
    /// falling back to [`DEFAULT_CHILD_CODE`] on an empty string, a non-digit
    /// byte, or overflow of the `i32` range. `const` and panic-free so the
    /// code is fixed into the image with no runtime parsing (fail closed to the default).
    const fn parse_i32(bytes: &[u8]) -> i32 {
        let mut acc: i32 = 0;
        let mut i = 0usize;
        let mut seen = false;
        while i < bytes.len() {
            let b = bytes[i];
            if b < b'0' || b > b'9' {
                return DEFAULT_CHILD_CODE;
            }
            let digit = (b - b'0') as i32;
            acc = match acc.checked_mul(10) {
                Some(v) => match v.checked_add(digit) {
                    Some(v) => v,
                    None => return DEFAULT_CHILD_CODE,
                },
                None => return DEFAULT_CHILD_CODE,
            };
            seen = true;
            i += 1;
        }
        if seen {
            acc
        } else {
            DEFAULT_CHILD_CODE
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime is
    /// set up and routes its return value through the `exit` syscall.
    ///
    /// The child returns [`CHILD_CODE`] directly. The parent waits for its
    /// (only) child, reaps it, and verifies the reaped exit code, returning 0
    /// on success and a distinct non-zero diagnostic otherwise.
    fn main() -> i32 {
        if !IS_PARENT {
            // Child: terminate with the agreed code so the parent can reap it.
            return CHILD_CODE;
        }

        // Parent: block until the child exits, reap it, and read its code.
        let mut status: i32 = -1;
        let reaped = rustos_rt::wait(WAIT_ANY, &mut status);
        if reaped < 0 {
            // `wait` failed (e.g. no child, or it was not our child): the
            // negative return is `-errno`. Report a distinct diagnostic.
            return 10;
        }
        if status != CHILD_CODE {
            // The child was reaped but its exit code did not round-trip.
            return 11;
        }
        // Reaped exactly one child with the expected exit code.
        0
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// `rustos-rt` entry path is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
