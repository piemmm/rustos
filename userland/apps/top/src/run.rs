//! The `Run` entry-point binary of the `top` tool — the program a shell spawns
//! to watch the process list live through the System Information API.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector (the reserved `-h`/`-?`
//! short-help switches render the tool's own Help document through the
//! shared engine and exit, plans/APPS.md §4; anything else on the command
//! line is a usage error — the viewer's controls are keys pressed inside
//! the session). It then sizes the screen from the console the kernel gave
//! it, puts the terminal into raw (no-echo) input, and runs the
//! [`tairix_top`] viewer loop
//! against two seams: the shared `tairix_curses::StreamTty`, the curses byte
//! channel over the inherited
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

    use core::time::Duration;

    use tairix_abi::{InputMode, STDOUT};
    use tairix_curses::{InputMode as CursesInputMode, Screen, Size, StreamTty};
    use tairix_help::{own_short_help, BundleHelp};
    use tairix_procinfo::{user_names, IpcTransport};
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_termcap::from_term;
    use tairix_top::{parse, run, Command, Model, Scope, USAGE};

    /// The conventional fallback terminal grid — 80 columns by 24 rows —
    /// applied when the kernel cannot attest the console's size (a serial
    /// line, whose remote terminal size only the far-end emulator knows).
    const FALLBACK_ROWS: u16 = 24;
    const FALLBACK_COLS: u16 = 80;

    /// Render `top`'s own short help (`NAME` + `SYNOPSIS` + compact
    /// `OPTIONS`) from its own bundle's `Help/` tree through the one shared
    /// engine; when no document can be served (a build without the bundle's
    /// documents) the usage banner stands in — the tool's own text, not
    /// fabricated help content — so `-h` never fails.
    fn short_help() -> i32 {
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let bytes = own_short_help(&BundleHelp::new("top"), locale, "top")
            .unwrap_or_else(|| alloc::format!("{USAGE}\n").into_bytes());
        match Stdout.write_all(&bytes) {
            Ok(()) => 0,
            Err(_) => 1,
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on a clean quit (or served short help), `1` on a
    /// service or terminal failure, `2` on a usage error (a malformed
    /// argument vector or an unrecognised option).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let delay_tenths = match parse(&arguments) {
            Ok(Command::Run { delay_tenths }) => delay_tenths,
            Ok(Command::Help) => return short_help(),
            Err(_) => {
                write_stderr_line(USAGE);
                return 2;
            }
        };

        // Size the screen from the console the kernel gave us, falling back to
        // the conventional 80×24 when the kernel cannot attest the size.
        let size = match tairix_rt::terminal_size(STDOUT) {
            Ok(grid) => Size::new(grid.rows(), grid.cols()),
            Err(_) => Size::new(FALLBACK_ROWS, FALLBACK_COLS),
        };

        // The raw input discipline: keystrokes reach the viewer verbatim
        // with neither the local echo nor the secret-entry indicator drawn
        // over the display the viewer paints. Restored to the cooked
        // default on exit so the next program on this console sees normal
        // interactive echo again.
        let _ = tairix_rt::set_input_mode(InputMode::Raw);

        // The terminal's capabilities come from the inherited `TERM`
        // (fail-closed: unknown or absent degrades to the dumb baseline
        // inside `from_term`), never a hard-coded terminal model — the
        // session exports the profile its console actually implements.
        let term = tairix_rt::env_var(b"TERM")
            .and_then(|raw| core::str::from_utf8(raw).ok())
            .map_or(tairix_termcap::TermType::Dumb, from_term);
        let mut screen = Screen::new(StreamTty, term, size);
        // Bound every input wait by the refresh delay (`-d`), so the view
        // redraws itself on that cadence without a key press — the kernel
        // parks the read for the interval, never a poll loop.
        screen.set_input_mode(CursesInputMode::Timeout(Duration::from_millis(
            u64::from(delay_tenths) * 100,
        )));
        // Take over the display for the session: the alternate screen
        // where the terminal has one (restoring the covered content on
        // exit), an in-place erase otherwise — either way the viewer never
        // draws over stale text from the previous command.
        let entered = screen.enter_full_screen();
        let transport = IpcTransport;
        // Default to the caller's own processes; the `a` key toggles to the
        // global view, which `sysinfod` grants only to an entitled caller.
        let mut model = Model::new(Scope::Own);
        // The USER column's uid → name map from the ungated, secret-free
        // account directory, resolved once up front (the account database
        // changes far more rarely than the process list); an empty
        // directory degrades the column to numeric uids.
        model.set_user_names(user_names(&transport));
        let result = run(&mut model, &transport, &mut screen);
        let left = screen.leave_full_screen();

        let _ = tairix_rt::set_input_mode(InputMode::Cooked);

        // A session that ends for any reason other than the user quitting
        // states that reason on stderr — after the terminal is restored, so
        // the message is not torn down with the alternate screen. A silent
        // abnormal exit tells the user nothing.
        if let Err(err) = &result {
            write_stderr_line(&alloc::format!("top: {err}"));
        } else if entered.is_err() || left.is_err() {
            write_stderr_line("top: terminal error: the screen could not be switched");
        }

        match (result, entered, left) {
            (Ok(()), Ok(()), Ok(())) => 0,
            _ => 1,
        }
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the program's real
// entry — the freestanding `tairix-rt` `_start` path — is not compiled, so
// this inert `main` keeps the crate building under the host tooling. It
// performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
