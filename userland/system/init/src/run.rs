//! The `Run` entry-point binary of the `init` application bundle
//! (`AGENTS.md` §16.5, `plans/PI.md` P6b).
//!
//! This is the program the kernel spawns as PID 1 once it reaches user mode
//! (`plans/PI.md` P6c). It is a freestanding C-ABI program built exactly like
//! a non-Rust program: its entry point is crt0's `_start` (`rustos_crt0`),
//! which sets up the C runtime and calls the program's `main`; that `main`
//! parses the compiled-in [`startup::DEFAULT_CONFIG`] and, when it asks
//! for the console, writes the first banner line through the `abi-v1`
//! `console_write` syscall (`rustos_abi_sys::sys_console_write`, the P6a
//! `ros_sys_console_write` stub). crt0 routes `main`'s return value through the
//! `exit` syscall.
//!
//! It links **only** the curated *System runtime / C ABI* class
//! (`rustos-crt0` together with `rustos-abi-sys`, `AGENTS.md` §16.4), never an
//! architecture, kernel, or even the sibling `rustos-init` orchestrator
//! library, whose `alloc`-and-crypto dependency chain has no place in a
//! banner-printing program (`AGENTS.md` §2.3). Its tiny startup-config parser
//! therefore lives alongside it in [`startup`] and is host-tested there. The
//! binary is built position-independent and converted to an `rxe` blob by the
//! consuming boot path (`plans/PI.md` P6c). On the host it is an inert stub so
//! `cargo build --workspace`, clippy, and fmt still cover the crate.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

mod startup;

// --- Freestanding C-ABI program -----------------------------------------
#[cfg(freestanding)]
mod program {
    use core::ffi::{c_char, c_int, c_void};
    use core::panic::PanicInfo;

    // crt0 provides the `_start` trampoline that calls this program's `main`.
    // rustc only links a dependency's rlib when the crate is referenced, so
    // naming it here forces crt0's `_start` onto the link line where the link
    // script's `ENTRY(_start)` roots it. The crate is named solely for that
    // link-time side effect (see `tests/integration/cc3_program`).
    #[allow(unused_extern_crates)]
    extern crate rustos_crt0;

    use crate::startup::{StartupConfig, BANNER, DEFAULT_CONFIG};

    /// Exit code for a clean run: the config parsed and the banner was written.
    const EXIT_OK: c_int = 0;

    /// Exit code when the compiled-in startup config does not parse. A
    /// reserved, fail-closed value (`AGENTS.md` §2.9); the default config is
    /// well-formed, so reaching this is a build defect, not a runtime input.
    const EXIT_CONFIG_INVALID: c_int = 70;

    /// Program entry point, called by crt0 once the C runtime is set up.
    ///
    /// Parses the compiled-in [`DEFAULT_CONFIG`], writes the startup banner to
    /// the system console, and returns [`EXIT_OK`]. crt0 routes the return
    /// value through the `exit` syscall.
    ///
    /// The config also names the session program `init` will launch; spawning
    /// it needs the process-spawn syscall (`plans/PI.md` P6d) and a shell
    /// (P6e), neither of which exists yet. Until then `init`'s P6b job is to
    /// reach user mode and prove the console write path, so the session path is
    /// only validated as parsed, not launched.
    ///
    /// # Safety
    ///
    /// crt0 guarantees the C-ABI calling convention; the arguments are unused.
    #[no_mangle]
    pub unsafe extern "C" fn main(
        _argc: c_int,
        _argv: *const *const c_char,
        _envp: *const *const c_char,
    ) -> c_int {
        let config = match StartupConfig::parse(DEFAULT_CONFIG) {
            Ok(config) => config,
            Err(_) => return EXIT_CONFIG_INVALID,
        };
        write_console(BANNER);
        // P6d/P6e will spawn this program as the user's session; for now its
        // presence is what the parse guarantees.
        let _session = config.session();
        EXIT_OK
    }

    /// Write `text` to the system console through the `abi-v1` `console_write`
    /// syscall. The kernel validates the capability (`CAP_CONSOLE_WRITE`) and
    /// the `(buf, len)` pair before reading it (`AGENTS.md` §5.4); the stub
    /// only reads `text`, so the `*mut` it requires is a benign cast.
    fn write_console(text: &str) {
        let bytes = text.as_bytes();
        let ptr = bytes.as_ptr().cast_mut().cast::<c_void>();
        let _ = rustos_abi_sys::sys_console_write(ptr, bytes.len());
    }

    /// Panic handler: a hosted program has no unwinder, so a panic is an
    /// unrecoverable fault. Terminate through the `exit` syscall rather than
    /// returning to corrupt state (`AGENTS.md` §2.9 — fail closed). The
    /// program is written to be panic-free; this satisfies the `no_std`
    /// contract.
    #[panic_handler]
    fn panic(_info: &PanicInfo<'_>) -> ! {
        rustos_abi_sys::sys_exit(EXIT_CONFIG_INVALID)
    }
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `_start` path — is not compiled, so this inert
// `main` keeps the crate building under the host tooling. It parses the
// compiled-in default config (and touches the parser's accessors) so a
// malformed `DEFAULT_CONFIG` is caught by an ordinary `cargo build` and the
// parser is exercised, not dead code, on the host. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {
    if let Ok(config) = startup::StartupConfig::parse(startup::DEFAULT_CONFIG) {
        let _ = (config.session(), startup::BANNER);
    }
}
