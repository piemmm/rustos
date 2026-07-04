//! The `Run` entry-point binary of the `top` tool — the program a shell spawns
//! to watch the process list live through the System Information API.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector (the reserved `-h`/`-?`
//! short-help switches render the tool's own Help document through the
//! shared engine and exit, plans/APPS.md §4; anything else on the command
//! line is a usage error — the viewer's controls are keys pressed inside
//! the session). It then sizes the screen from the console the kernel gave
//! it, puts the terminal into raw (no-echo) input, and runs the
//! [`rustos_top`] viewer loop
//! against two seams: `RtTty`, the curses byte channel over the inherited
//! standard input/output (fd 0/1), and `IpcTransport` (shared through
//! `lib/procinfo`), which carries the framed `sysinfo-v1` request to
//! `/System/Services/sysinfod.app/Run` over the well-known IPC call endpoint. The tool
//! binds only to its inherited descriptors, never a console device, and holds
//! no ambient authority: `sysinfod` gates every query against the caller's
//! kernel-attested origin.
//!
//! # Terminal size
//!
//! The viewer draws into a fixed grid, so it asks the kernel how big its
//! console is with `terminal_size`. The kernel answers only for a console
//! whose geometry it knows (a framebuffer text console); a serial terminal's
//! true size is a property of the remote emulator, unknowable to the kernel,
//! so the query fails closed and this program applies the conventional 80×24
//! fallback — the size policy lives here, in the client, not in the kernel.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;

    use rustos_abi::STDOUT;
    use rustos_curses::{CursesError, Result as CursesResult, Screen, Size, Tty};
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_procinfo::IpcTransport;
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_termcap::TermType;
    use rustos_top::{parse, run, Command, Model, Scope, USAGE};

    /// The conventional fallback terminal grid — 80 columns by 24 rows —
    /// applied when the kernel cannot attest the console's size (a serial
    /// line, whose remote terminal size only the far-end emulator knows).
    const FALLBACK_ROWS: u16 = 24;
    const FALLBACK_COLS: u16 = 80;

    /// The maximum input bytes drained from standard input in one blocking
    /// read. A key press (even a multi-byte escape sequence) is a handful of
    /// bytes; a small stack buffer absorbs a burst without allocating, and the
    /// curses input decoder reassembles sequences that span reads.
    const INPUT_CHUNK: usize = 64;

    /// The curses [`Tty`] over the program's inherited standard streams: writes
    /// go to standard output (fd 1) and blocking reads draw from standard input
    /// (fd 0), both through `rustos-rt`. The program names only its inherited
    /// descriptors, never a console device, so the same binary drives a serial
    /// terminal, a framebuffer console, or a future windowed terminal
    /// unchanged — the stream layer owns which backing that is.
    struct RtTty;

    impl Tty for RtTty {
        fn write(&mut self, bytes: &[u8]) -> CursesResult<()> {
            // The shared `rustos_rt::io` short-write loop — no tty-private copy
            // (the charter forbids that duplication). `write_all` loops over
            // short writes and fails closed (never spins) if the backing stops
            // accepting bytes, which the seam reports as an I/O error.
            Stdout.write_all(bytes).map_err(|_| CursesError::Io)
        }

        fn read(&mut self) -> CursesResult<Vec<u8>> {
            // The standard-input backing owns blocking and offers no
            // peek/poll, so a non-blocking read cannot know what is pending: it
            // honestly reports "nothing available right now" rather than
            // blocking or fabricating input. The viewer runs the blocking
            // input mode, so this path is never its wait; it exists to satisfy
            // the seam and never lies about available bytes.
            Ok(Vec::new())
        }

        fn read_blocking(&mut self) -> CursesResult<Vec<u8>> {
            let mut buf = [0u8; INPUT_CHUNK];
            // `rustos_rt::stdin` parks the task in the kernel until at least
            // one byte arrives (the backing owns blocking), then returns the
            // count read; a zero-length return means the stream reported end of
            // input, which the seam encodes as an empty vector.
            let read = rustos_rt::stdin(&mut buf);
            Ok(buf[..read.min(buf.len())].to_vec())
        }
    }

    /// Render `top`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("top"), locale, "top")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on a clean quit (or served short help), `1` on a
    /// service or terminal failure, `2` on a usage error (a malformed
    /// argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        match parse(&arguments) {
            Ok(Command::Run) => {}
            Ok(Command::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        }

        // Size the screen from the console the kernel gave us, falling back to
        // the conventional 80×24 when the kernel cannot attest the size.
        let size = match rustos_rt::terminal_size(STDOUT) {
            Ok(grid) => Size::new(grid.rows(), grid.cols()),
            Err(_) => Size::new(FALLBACK_ROWS, FALLBACK_COLS),
        };

        // Raw input: suppress the console's local echo so keystrokes reach the
        // viewer verbatim (a full-screen UI paints its own display and must not
        // have input echoed onto it). Restored on exit so the next program on
        // this console sees normal cooked echo again.
        let _ = rustos_rt::set_echo(false);

        let mut screen = Screen::new(RtTty, TermType::Vt100, size);
        let transport = IpcTransport;
        // Default to the caller's own processes; the `a` key toggles to the
        // global view, which `sysinfod` grants only to an entitled caller.
        let mut model = Model::new(Scope::Own);
        let result = run(&mut model, &transport, &mut screen);

        let _ = rustos_rt::set_echo(true);

        match result {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `rustos-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
