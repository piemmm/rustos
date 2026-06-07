//! The `Run` entry-point binary of the `Shell` application bundle
//! (`AGENTS.md` §16.5) — the program PID 1 `init` launches as the user's
//! session through the `spawn` syscall (`plans/SPAWN.md` `SP3b`,
//! `plans/PI.md` P6e).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only (`AGENTS.md` §1), so
//! it links the Rust userland runtime `rustos-rt` — never the C ABI, which
//! exists solely for programs **not** written in Rust (`AGENTS.md` §16.4).
//! `rustos-rt` provides `_start`, the per-process stack canary (`AGENTS.md`
//! §19.2), the panic handler, and the syscall wrappers; `rustos_rt::entry!`
//! names this program's `main`.
//!
//! For `SP3b` its job is to prove a *second*, hardware-isolated process — built
//! by the runtime-spawn producer in its own address space and admitted Ready
//! alongside the still-running PID 1 — actually runs: it writes a banner
//! through the `abi-v1` `console_write` syscall and exits. Reaching the exit
//! is itself the proof, because the write is *gated* (see `main`). Growing
//! this into a real REPL over the console — wiring in the `rustos-shell`
//! interpreter library this crate already provides — is `plans/PI.md` P6e.
//!
//! It links **only** the runtime, never the sibling `rustos-shell`
//! interpreter library (whose `alloc`-using parser has no place in a
//! banner-printing stub yet, `AGENTS.md` §2.3). On the host it is an inert
//! stub so `cargo build --workspace`, clippy, and fmt still cover the file.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    /// Exit code for a clean run: the banner was written in full.
    const EXIT_OK: i32 = 0;

    /// The line the session writes to the console once it is running. A
    /// fixed, terse banner (`AGENTS.md` §13 — no aimless waffle) that proves
    /// a second isolated process spawned by PID 1 reached EL0 and its own
    /// `console_write` path works end to end.
    const BANNER: &[u8] = b"RustOS shell: session started\n";

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Writes the session banner to the system console and returns
    /// [`EXIT_OK`]. The write is *gated*: `console_write` returns the number
    /// of bytes the kernel accepted, so a short count means the privileged
    /// console write did not fully land (a missing `CAP_CONSOLE_WRITE`, an
    /// unresolved address space, or a closed-fail kernel path). The session
    /// cannot usefully proceed without the console, so it parks fail-closed
    /// rather than exiting "successfully" on a console it never reached
    /// (`AGENTS.md` §2.9). This is a terminal park, not a retry loop
    /// (`AGENTS.md` §2.1).
    fn main() -> i32 {
        if rustos_rt::console_write(BANNER) != BANNER.len() {
            loop {
                core::hint::spin_loop();
            }
        }
        EXIT_OK
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
