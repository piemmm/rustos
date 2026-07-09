//! The `Run` entry-point binary of the `fstree` tool — the full-screen
//! tree file manager a shell spawns.
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
//! shared engine and exit; at most one operand names the starting
//! directory), sizes the screen from the console the kernel gave it, puts
//! the terminal into raw (no-echo) input, and runs the [`rustos_fstree`]
//! session against two seams: `RtTty`, the curses byte channel over the
//! inherited standard input/output (fd 0/1), and `RtFs`, which lists
//! directories through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side) and asks the System
//! Information API's `MOUNT_LIST` query for the status line's volume free
//! space (best-effort; an unreachable service simply omits the figure).
//! The tool binds only to its inherited descriptors, never a console
//! device, and holds no ambient authority.
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

    use rustos_abi::fs::{DirEntry, OpenFlags, FS_IO_MAX, FS_MODE_MASK};
    use rustos_abi::{Errno, InputMode, STDOUT};
    use rustos_curses::{
        CursesError, InputMode as CursesInputMode, Result as CursesResult, Screen, Size, Tty,
    };
    use rustos_fstree::{run, Fs, FsEntry, Model, VolumeSpace};
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_procinfo::{for_each_mount, IpcTransport};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_termcap::from_term;
    use rustos_vt::{Op, Parser};

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

    /// Initial byte size of the directory-listing buffer: one page covers a
    /// typical directory; `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// The usage banner printed when the arguments cannot be understood.
    const USAGE: &str = "usage: fstree [-h | -?] [directory]";

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
            // available right now". The session runs the blocking input
            // mode, so this path is never its wait.
            Ok(Vec::new())
        }

        fn read_blocking(&mut self) -> CursesResult<Vec<u8>> {
            let mut buf = [0u8; INPUT_CHUNK];
            // `rustos_rt::stdin` parks the task in the kernel until at
            // least one byte arrives, then returns the count read. A
            // zero-length return means the stream ended: the session's
            // input is gone, reported as an error so the tool ends
            // loudly instead of spinning on a dead channel.
            let read = rustos_rt::stdin(&mut buf);
            if read == 0 {
                return Err(CursesError::Io);
            }
            Ok(buf[..read.min(buf.len())].to_vec())
        }
    }

    /// The production [`Fs`]: directory listings through the
    /// kernel-authorised `fs_*` syscalls (each entry's kind, sizes, and
    /// modification stamp ride the one `fs_readdir` stream), and volume
    /// free space through the System Information API's shared mount walk.
    struct RtFs;

    impl Fs for RtFs {
        fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, Errno> {
            let dir = rustos_rt::open_dir(path.as_bytes()).map_err(Errno::from_syscall)?;
            let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
            let used = loop {
                match dir.read(&mut buf) {
                    Ok(used) => break used,
                    Err(ret) => match Errno::from_syscall(ret) {
                        Errno::BufferTooSmall if buf.len() < DIR_BUF_MAX => {
                            buf.resize((buf.len() * 2).min(DIR_BUF_MAX), 0);
                        }
                        other => return Err(other),
                    },
                }
            };
            let mut entries = Vec::new();
            let mut rest = &buf[..used];
            while !rest.is_empty() {
                let (entry, consumed) = DirEntry::decode(rest)?;
                rest = &rest[consumed..];
                // The ABI contract makes every entry name UTF-8; a name
                // that is not is a corrupt or hostile stream, refused whole
                // rather than silently dropped from the listing.
                let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
                entries.push(FsEntry {
                    name: String::from(name),
                    kind: entry.kind,
                    size: entry.size,
                    modified: entry.modified,
                });
            }
            Ok(entries)
        }

        fn stat_mode(&mut self, path: &str) -> Result<u32, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file = rustos_rt::File::open(path.as_bytes(), OpenFlags::empty())
                .map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            // Only the permission bits are the editor's subject; the
            // file-type bits the backing reports above the mask are not.
            Ok(stat.mode & FS_MODE_MASK)
        }

        fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Errno> {
            // The kernel authorises the change (owner-only, mount-flag,
            // per-inode checks); the prompt's four-octal-digit bound keeps
            // `mode` within the permission mask already.
            let ret = rustos_rt::fs_set_mode(path.as_bytes(), mode);
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn volume_space(&mut self, path: &str) -> Option<VolumeSpace> {
            // The mount whose target is the longest prefix of `path` backs
            // it. Best-effort by contract: an unreachable service or a
            // failed walk yields `None` and the status line omits the
            // figure — never an error, never a fabricated count.
            let mut best: Option<(usize, VolumeSpace)> = None;
            let walked = for_each_mount(&IpcTransport, |record| {
                let Ok(target) = core::str::from_utf8(record.target_bytes()) else {
                    return Ok(());
                };
                if !path_has_prefix(path, target) {
                    return Ok(());
                }
                let usage = record.usage();
                let block = u64::from(usage.block_size);
                let space = VolumeSpace {
                    free_bytes: usage.free_blocks.saturating_mul(block),
                    total_bytes: usage.total_blocks.saturating_mul(block),
                };
                if best.as_ref().is_none_or(|(len, _)| target.len() > *len) {
                    best = Some((target.len(), space));
                }
                Ok(())
            });
            match walked {
                Ok(()) => best.map(|(_, space)| space),
                Err(_) => None,
            }
        }
    }

    /// Whether `path` lives under the mount target `target` (`/` covers
    /// everything; otherwise the prefix must end on a component boundary).
    fn path_has_prefix(path: &str, target: &str) -> bool {
        if target == "/" {
            return true;
        }
        match path.strip_prefix(target) {
            Some(rest) => rest.is_empty() || rest.starts_with('/'),
            None => false,
        }
    }

    /// Decode vt-encoded help bytes to the plain text the `?` overlay
    /// shows, through the one shared `lib/vt` parser — styling is dropped,
    /// text and line breaks are kept, and nothing else can reach the grid.
    fn plain_help_text(bytes: &[u8]) -> String {
        let mut text = String::new();
        let mut parser = Parser::new();
        parser.feed(bytes, |op| match op {
            Op::Print(ch) => text.push(ch),
            Op::LineFeed => text.push('\n'),
            _ => {}
        });
        text
    }

    /// Print the tool's own short help (`-h` / `-?`) through the shared
    /// engine; the usage banner is the fallback when no document serves.
    fn short_help() -> i32 {
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match own_short_help(&BundleHelp::new("fstree"), locale, "fstree") {
            Some(bytes) => {
                let _ = Stdout.write_all(&bytes);
                0
            }
            None => {
                write_stderr_line(USAGE);
                2
            }
        }
    }

    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error,
        // reported rather than guessed at.
        let Some(arguments) = rustos_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let mut root: Option<String> = None;
        for &argument in arguments.iter().skip(1) {
            match argument {
                "-h" | "-?" => return short_help(),
                other if root.is_none() && !other.starts_with('-') => {
                    root = Some(String::from(other));
                }
                other => {
                    write_stderr_line(&alloc::format!("fstree: unknown argument: {other}"));
                    write_stderr_line(USAGE);
                    return 2;
                }
            }
        }
        let root = root.unwrap_or_else(|| String::from("/"));

        // The `?` overlay's text: the bundle's own Help document rendered
        // by the shared engine, decoded to plain text. A bundle whose help
        // cannot be served shows the key line alone — never embedded text.
        let locale = rustos_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let help_text = own_short_help(&BundleHelp::new("fstree"), locale, "fstree")
            .map(|bytes| plain_help_text(&bytes))
            .unwrap_or_else(|| String::from(USAGE));

        // Size the screen from the console the kernel gave us, falling
        // back to the conventional 80×24 when the kernel cannot attest
        // the size.
        let size = match rustos_rt::terminal_size(STDOUT) {
            Ok(grid) => Size::new(grid.rows(), grid.cols()),
            Err(_) => Size::new(FALLBACK_ROWS, FALLBACK_COLS),
        };

        let mut fs = RtFs;
        // The starting listing is read before the terminal is switched, so
        // a refused root fails loudly on a normal screen.
        let mut model = match Model::new(&mut fs, &root, help_text) {
            Ok(model) => model,
            Err(errno) => {
                write_stderr_line(&alloc::format!("fstree: {root}: {errno:?}"));
                return 1;
            }
        };

        // The raw input discipline: keystrokes reach the session verbatim
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
        // The session blocks on each keystroke; the kernel parks the read.
        screen.set_input_mode(CursesInputMode::Blocking);
        // Take over the display for the session: the alternate screen
        // where the terminal has one (restoring the covered content on
        // exit), an in-place erase otherwise.
        let entered = screen.enter_full_screen();
        let result = run(&mut model, &mut fs, &mut screen);
        let left = screen.leave_full_screen();

        let _ = rustos_rt::set_input_mode(InputMode::Cooked);

        // A session that ends for any reason other than the user quitting
        // states that reason on stderr — after the terminal is restored,
        // so the message is not torn down with the alternate screen.
        if let Err(err) = &result {
            write_stderr_line(&alloc::format!("fstree: terminal error: {err:?}"));
        } else if entered.is_err() || left.is_err() {
            write_stderr_line("fstree: terminal error: the screen could not be switched");
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
