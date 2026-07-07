//! The `Run` entry-point binary of the `vim` tool — the program a shell
//! spawns to edit text files.
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the
//! Rust userland runtime `rustos-rt` — never the C ABI, which exists solely
//! for programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `rustos_rt::entry!` names this
//! program's `main`.
//!
//! `main` parses the inherited argument vector (the reserved `-h`/`-?`
//! short-help switches render the tool's own Help document through the
//! shared engine and exit; a malformed argument is a usage error), sizes
//! the screen from the console the kernel gave it, puts the terminal into
//! raw (no-echo) input, and runs the [`rustos_vim`] session against two
//! seams: `RtTty`, the curses byte channel over the inherited standard
//! input/output (fd 0/1), and `RtFileIo`, which reads and writes named
//! files through the kernel-authorised `fs_*` syscalls (every per-inode
//! and mount check stays kernel-side). The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
//! authority.
//!
//! # Terminal size
//!
//! The editor draws into a fixed grid, so it asks the kernel how big its
//! console is with `terminal_size`. The kernel answers only for a console
//! whose geometry it knows; a serial terminal's true size is a property of
//! the remote emulator, so the query fails closed and this program applies
//! the conventional 80×24 fallback — the size policy lives here, in the
//! client, not in the kernel.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy,
//! and fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;

    use rustos_abi::fs::OpenFlags;
    use rustos_abi::{Errno, InputMode, STDOUT};
    use rustos_curses::{
        CursesError, InputMode as CursesInputMode, Result as CursesResult, Screen, Size, Tty,
    };
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_rt::File;
    use rustos_termcap::from_term;
    use rustos_vim::{parse, run, Command, Editor, FileIo, USAGE};

    /// The conventional fallback terminal grid — 80 columns by 24 rows —
    /// applied when the kernel cannot attest the console's size (a serial
    /// line, whose remote terminal size only the far-end emulator knows).
    const FALLBACK_ROWS: u16 = 24;
    const FALLBACK_COLS: u16 = 80;

    /// The maximum input bytes drained from standard input in one blocking
    /// read. A key press (even a multi-byte escape sequence) is a handful
    /// of bytes; a small stack buffer absorbs a burst without allocating,
    /// and the curses input decoder reassembles sequences that span reads.
    const INPUT_CHUNK: usize = 64;

    /// The read granularity for whole-file loads.
    const FILE_CHUNK: usize = 4096;

    /// The curses [`Tty`] over the program's inherited standard streams:
    /// writes go to standard output (fd 1) and blocking reads draw from
    /// standard input (fd 0), both through `rustos-rt`. The program names
    /// only its inherited descriptors, never a console device, so the same
    /// binary drives a serial terminal, a framebuffer console, or a future
    /// windowed terminal unchanged — the stream layer owns which backing
    /// that is.
    struct RtTty;

    impl Tty for RtTty {
        fn write(&mut self, bytes: &[u8]) -> CursesResult<()> {
            // The shared `rustos_rt::io` short-write loop — no tty-private
            // copy. `write_all` loops over short writes and fails closed
            // (never spins) if the backing stops accepting bytes, which
            // the seam reports as an I/O error.
            Stdout.write_all(bytes).map_err(|_| CursesError::Io)
        }

        fn read(&mut self) -> CursesResult<Vec<u8>> {
            // The standard-input backing owns blocking and offers no
            // peek/poll; a non-blocking read honestly reports "nothing
            // available right now". The editor runs the blocking input
            // mode, so this path is never its wait.
            Ok(Vec::new())
        }

        fn read_blocking(&mut self) -> CursesResult<Vec<u8>> {
            let mut buf = [0u8; INPUT_CHUNK];
            // `rustos_rt::stdin` parks the task in the kernel until at
            // least one byte arrives, then returns the count read. A
            // zero-length return means the stream ended: the session's
            // input is gone, reported as an error so the editor ends
            // loudly instead of spinning on a dead channel.
            let read = rustos_rt::stdin(&mut buf);
            if read == 0 {
                return Err(CursesError::Io);
            }
            Ok(buf[..read.min(buf.len())].to_vec())
        }
    }

    /// The production [`FileIo`]: the kernel-authorised `fs_*` view of
    /// named files. Every open is checked per-inode against the caller's
    /// kernel-attested identity; a refusal comes back as the frozen
    /// `Errno` the editor spells out on its status line.
    struct RtFileIo;

    impl FileIo for RtFileIo {
        fn read(&self, path: &str) -> Result<Option<Vec<u8>>, Errno> {
            let file = match File::open(path.as_bytes(), OpenFlags::READ) {
                Ok(file) => file,
                Err(raw) => {
                    let errno = Errno::from_syscall(raw);
                    // A missing file is vim's "new file", not an error.
                    if errno == Errno::NotFound {
                        return Ok(None);
                    }
                    return Err(errno);
                }
            };
            let mut bytes: Vec<u8> = Vec::new();
            let mut offset = 0u64;
            let mut chunk = [0u8; FILE_CHUNK];
            loop {
                let read = file
                    .read_at(offset, &mut chunk)
                    .map_err(Errno::from_syscall)?;
                if read == 0 {
                    return Ok(Some(bytes));
                }
                bytes.extend_from_slice(&chunk[..read.min(chunk.len())]);
                offset = offset.saturating_add(read as u64);
            }
        }

        fn write(&self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
            let flags = OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::TRUNCATE);
            let file = File::open(path.as_bytes(), flags).map_err(Errno::from_syscall)?;
            let mut offset = 0u64;
            let mut remaining = bytes;
            // The kernel may accept a short write; every byte is this
            // seam's to deliver, and a backing that accepts nothing is a
            // refusal, never a spin.
            while !remaining.is_empty() {
                let written = file
                    .write_at(offset, remaining)
                    .map_err(Errno::from_syscall)?;
                if written == 0 {
                    return Err(Errno::NotImplemented);
                }
                let written = written.min(remaining.len());
                offset = offset.saturating_add(written as u64);
                remaining = &remaining[written..];
            }
            Ok(())
        }
    }

    /// Render `vim`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one
    /// shared engine; when no document can be served (a build without the
    /// bundle's documents) the usage banner stands in — the tool's own
    /// text, not fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("vim"), locale, "vim")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `rustos-rt`'s `_start` calls it once the
    /// runtime is set up and routes its return value through the `exit`
    /// syscall.
    ///
    /// Exit codes: `0` on a clean quit (or served short help), `1` on a
    /// terminal failure, `2` on a usage error (a malformed argument vector
    /// or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error,
        // reported rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let (readonly, start, files) = match parse(&arguments) {
            Ok(Command::Run {
                readonly,
                start,
                files,
            }) => (readonly, start, files),
            Ok(Command::Help) => return short_help(),
            Err(err) => {
                write_stderr_line(&alloc::format!("vim: unknown argument: {}", err.argument));
                write_stderr_line(USAGE);
                return 2;
            }
        };

        // Size the screen from the console the kernel gave us, falling
        // back to the conventional 80×24 when the kernel cannot attest
        // the size.
        let size = match rustos_rt::terminal_size(STDOUT) {
            Ok(grid) => Size::new(grid.rows(), grid.cols()),
            Err(_) => Size::new(FALLBACK_ROWS, FALLBACK_COLS),
        };

        // The raw input discipline: keystrokes reach the editor verbatim
        // with no local echo. Restored to the cooked default on exit so
        // the next program on this console sees normal interactive echo.
        let _ = rustos_rt::set_input_mode(InputMode::Raw);

        // The terminal's capabilities come from the inherited `TERM`
        // (fail-closed: unknown or absent degrades to the dumb baseline
        // inside `from_term`), never a hard-coded terminal model.
        let term = rustos_rt::env_var(b"TERM")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .map_or(rustos_termcap::TermType::Dumb, from_term);
        let mut screen = Screen::new(RtTty, term, size);
        // The editor blocks on each keystroke; the kernel parks the read.
        screen.set_input_mode(CursesInputMode::Blocking);
        // Take over the display for the session: the alternate screen
        // where the terminal has one (restoring the covered content on
        // exit), an in-place erase otherwise.
        let entered = screen.enter_full_screen();
        let files: Vec<String> = files;
        let mut editor = Editor::new(files, readonly);
        let io = RtFileIo;
        let result = run(&mut editor, &io, &mut screen, start);
        let left = screen.leave_full_screen();

        let _ = rustos_rt::set_input_mode(InputMode::Cooked);

        // A session that ends for any reason other than the user quitting
        // states that reason on stderr — after the terminal is restored,
        // so the message is not torn down with the alternate screen.
        if let Err(err) = &result {
            write_stderr_line(&alloc::format!("vim: {err}"));
        } else if entered.is_err() || left.is_err() {
            write_stderr_line("vim: terminal error: the screen could not be switched");
        }

        match (result, entered, left) {
            (Ok(code), Ok(()), Ok(())) => code,
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
