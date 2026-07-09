//! The `Run` entry-point binary of the `edit` tool — the program a shell
//! spawns to edit a text file full-screen.
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
//! shared engine and exit, plans/APPS.md §4; at most one file operand is
//! accepted). It then sizes the screen from the console the kernel gave it,
//! puts the terminal into raw (no-echo) input, and runs the [`rustos_edit`]
//! editor loop against two seams: the shared `rustos_curses::StreamTty`, the
//! curses byte channel over the
//! inherited standard input/output (fd 0/1), and `RtFs`, whole-file
//! load/save over the kernel-authorised `fs_*` syscalls. The tool binds
//! only to its inherited descriptors, never a console device, and holds no
//! ambient authority: every path resolution and per-inode permission check
//! happens kernel-side under the caller's attested identity.
//!
//! # Terminal size
//!
//! The editor draws into a fixed grid, so it asks the kernel how big its
//! console is with `terminal_size`. The kernel answers only for a console
//! whose geometry it knows (a framebuffer text console); a serial
//! terminal's true size is a property of the remote emulator, unknowable to
//! the kernel, so the query fails closed and this program applies the
//! conventional 80×24 fallback — the size policy lives here, in the client,
//! not in the kernel.
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

    use alloc::vec;
    use alloc::vec::Vec;

    use rustos_abi::{Errno, InputMode, OpenFlags, STDOUT};
    use rustos_curses::{Screen, Size, StreamTty};
    use rustos_edit::{parse, run, Command, Fs, Model, MAX_FILE_BYTES, USAGE};
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_rt::File;
    use rustos_termcap::from_term;

    /// The conventional fallback terminal grid — 80 columns by 24 rows —
    /// applied when the kernel cannot attest the console's size (a serial
    /// line, whose remote terminal size only the far-end emulator knows).
    const FALLBACK_ROWS: u16 = 24;
    const FALLBACK_COLS: u16 = 80;

    /// The production [`Fs`]: whole-file load and save over the
    /// kernel-authorised `fs_*` view of the filesystem. It adds no
    /// authority — every path resolution, per-inode permission, and
    /// mount-flag check happens kernel-side under the caller's attested
    /// identity, and a refusal surfaces as the exact [`Errno`] the kernel
    /// chose.
    struct RtFs;

    impl Fs for RtFs {
        fn read(&self, path: &str) -> Result<Vec<u8>, Errno> {
            let file = File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
            let size = file.stat().map_err(Errno::from_syscall)?.size;
            // Allocate at most one byte past the editor's load bound: a
            // larger file still reads enough for the decoder to refuse it
            // as over-large, without ballooning memory on a huge operand.
            let capacity = usize::try_from(size)
                .unwrap_or(usize::MAX)
                .min(MAX_FILE_BYTES + 1);
            let mut buf = vec![0u8; capacity];
            let read = file.read_at(0, &mut buf).map_err(Errno::from_syscall)?;
            buf.truncate(read);
            Ok(buf)
        }

        fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
            let flags = OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::TRUNCATE);
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            // `write_at` already loops over short writes; a partial count
            // means the backing stopped accepting bytes, which must fail
            // closed rather than report a truncated file as saved.
            let written = file.write_at(0, bytes).map_err(Errno::from_syscall)?;
            if written != bytes.len() {
                return Err(Errno::LengthOutOfRange);
            }
            Ok(())
        }
    }

    /// Render `edit`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("edit"), locale, "edit")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on a clean exit (or served short help), `1` on a
    /// failed initial load or a terminal failure, `2` on a usage error (a
    /// malformed argument vector, an unrecognised option, or a second
    /// operand).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let path = match parse(&arguments) {
            Ok(Command::Run { path }) => path,
            Ok(Command::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        };

        let fs = RtFs;
        let mut model = Model::new();
        if let Some(path) = &path {
            // An unreadable or non-text operand fails loudly before the
            // screen is taken over — never an empty buffer silently opened
            // over real data.
            if let Err(reason) = model.open_initial(&fs, path) {
                write_stderr_line(&alloc::format!("edit: {reason}"));
                return 1;
            }
        }

        // Size the screen from the console the kernel gave us, falling back
        // to the conventional 80×24 when the kernel cannot attest the size.
        let size = match rustos_rt::terminal_size(STDOUT) {
            Ok(grid) => Size::new(grid.rows(), grid.cols()),
            Err(_) => Size::new(FALLBACK_ROWS, FALLBACK_COLS),
        };

        // The raw input discipline: keystrokes reach the editor verbatim
        // with neither the local echo nor the secret-entry indicator drawn
        // over the display the editor paints. Restored to the cooked
        // default on exit so the next program on this console sees normal
        // interactive echo again.
        let _ = rustos_rt::set_input_mode(InputMode::Raw);

        // The terminal's capabilities come from the inherited `TERM`
        // (fail-closed: unknown or absent degrades to the dumb baseline
        // inside `from_term`), never a hard-coded terminal model — the
        // session exports the profile its console actually implements.
        let term = rustos_rt::env_var(b"TERM")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .map_or(rustos_termcap::TermType::Dumb, from_term);
        let mut screen = Screen::new(StreamTty, term, size);
        // Take over the display for the session: the alternate screen
        // where the terminal has one (restoring the covered content on
        // exit), an in-place erase otherwise — either way the editor never
        // draws over stale text from the previous command.
        let entered = screen.enter_full_screen();
        let result = run(&mut model, &fs, &mut screen);
        let left = screen.leave_full_screen();

        let _ = rustos_rt::set_input_mode(InputMode::Cooked);

        // A session that ends for any reason other than the user exiting
        // states that reason on stderr — after the terminal is restored, so
        // the message is not torn down with the alternate screen. A silent
        // abnormal exit tells the user nothing.
        if let Err(err) = &result {
            write_stderr_line(&alloc::format!("edit: {err}"));
        } else if entered.is_err() || left.is_err() {
            write_stderr_line("edit: terminal error: the screen could not be switched");
        }

        match (result, entered, left) {
            (Ok(()), Ok(()), Ok(())) => 0,
            _ => 1,
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
