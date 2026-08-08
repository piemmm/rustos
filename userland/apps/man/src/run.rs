//! The `Run` entry-point binary of the `man` tool — the program a shell
//! spawns to read a command's bundled help.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference, the `PATH` search list, and the `HOME` naming the user's own
//! `Commands` and `Applications` search roots from the inherited
//! environment (plans/APPS.md §5, §7, and §8 — the shell exports all
//! three; the tool invents no second source), and runs the parsed command
//! against the two production seams: `RtStore`, which probes and reads
//! app bundles through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side), and `RtConsole`, which
//! writes the page to the inherited standard output, emits the
//! locale-fallback advisory on fd 3, and drives the pager from the
//! inherited standard input. The tool binds only to its
//! inherited descriptors, never a console device, and holds no ambient
//! authority.
//!
//! # Pagination
//!
//! The pager needs a screen height. The kernel attests a console's geometry
//! only where it owns it (a framebuffer text console); on a serial line the
//! remote emulator owns the screen, so `terminal_size` fails closed and the
//! page streams whole — exactly what a redirected or piped consumer gets.
//! While the pager is live the console's local echo is suppressed so the
//! prompt key is not painted over the page, and restored on exit.
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

    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::fs::{DirEntry, OpenFlags};
    use tairix_abi::{Errno, InputMode, TerminalSize, STDOUT};
    use tairix_man::{parse, run, BundleStore, Console, ManError, Request, USAGE};
    use tairix_rt::io::{self, write_stderr_line, Read, StdInfo, Stdin, Stdout, Write};
    use tairix_sandbox::helpdoc::HelpService;
    use tairix_sandbox::host::ParserSandbox;
    use tairix_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use tairix_sandbox::worker::ServeEnd;

    /// Initial byte size of the directory-listing buffer. A `Help/` tree
    /// lists a handful of locale directories and a store directory a
    /// handful of bundles, so one page nearly always suffices;
    /// `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer — a validation bound, not a
    /// capacity. The engine refuses a `Help/` tree with more locales than
    /// its own bound long before this, and the recursive store search is
    /// budgeted in directories, so a single listing that cannot fit here is
    /// a hostile or corrupt tree; the read then fails closed rather than
    /// growing without limit.
    const DIR_BUF_MAX: usize = 256 * 1024;

    /// The production [`BundleStore`]: the kernel-authorised `fs_*` view of
    /// the installed bundles. It adds no authority — every path resolution,
    /// per-inode permission, and mount-flag check happens kernel-side under
    /// the caller's attested identity, and a refusal surfaces as the exact
    /// [`Errno`] the kernel chose.
    struct RtStore;

    impl BundleStore for RtStore {
        fn bundle_exists(&self, bundle_dir: &str) -> Result<bool, Errno> {
            // A resolve-only directory open: no read authority is requested,
            // the descriptor is closed at once, and only existence is
            // learned.
            let ret = tairix_rt::fs_open(bundle_dir.as_bytes(), OpenFlags::DIRECTORY);
            if ret >= 0 {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                // A non-negative fs_open result is a descriptor the kernel
                // allocated from the fd range; the conversion is exact.
                let _ = tairix_rt::fs_close(ret as u32);
                return Ok(true);
            }
            match Errno::from_syscall(ret) {
                Errno::NotFound => Ok(false),
                other => Err(other),
            }
        }

        fn locale_dirs(&self, bundle_dir: &str) -> Result<Vec<String>, Errno> {
            let path = format!("{bundle_dir}/Help");
            // A bundle without a Help/ tree simply has no locales; that is
            // the engine's clean "no help".
            list_dirs(&path)
        }

        fn subdirs(&self, dir: &str) -> Result<Vec<String>, Errno> {
            list_dirs(dir)
        }

        fn read_doc(
            &self,
            bundle_dir: &str,
            locale_dir: &str,
            file_name: &str,
            limit: usize,
        ) -> Result<Option<Vec<u8>>, Errno> {
            let path = format!("{bundle_dir}/Help/{locale_dir}/{file_name}");
            let file = match tairix_rt::open(path.as_bytes()) {
                Ok(file) => file,
                Err(ret) => {
                    return match Errno::from_syscall(ret) {
                        Errno::NotFound => Ok(None),
                        other => Err(other),
                    };
                }
            };
            // Read at most one byte past the engine's limit: the engine's
            // own document bound then rejects the oversized file, and a
            // hostile huge file cannot exhaust memory here first.
            let cap = limit.saturating_add(1);
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            while bytes.len() < cap {
                let want = chunk.len().min(cap - bytes.len());
                let read = file
                    .read_at(bytes.len() as u64, &mut chunk[..want])
                    .map_err(Errno::from_syscall)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Ok(Some(bytes))
        }
    }

    /// List the names of the directories directly inside `path`. A missing
    /// `path` has no children (`Ok(vec![])`); any other refusal is the
    /// kernel's own [`Errno`], surfaced verbatim.
    fn list_dirs(path: &str) -> Result<Vec<String>, Errno> {
        let dir = match tairix_rt::open_dir(path.as_bytes()) {
            Ok(dir) => dir,
            Err(ret) => {
                return match Errno::from_syscall(ret) {
                    Errno::NotFound => Ok(Vec::new()),
                    other => Err(other),
                };
            }
        };
        let mut buf = alloc::vec![0u8; DIR_BUF_INITIAL];
        let used = loop {
            match dir.read(&mut buf) {
                Ok(used) => break used,
                Err(ret) => match Errno::from_syscall(ret) {
                    Errno::BufferTooSmall if buf.len() < DIR_BUF_MAX => {
                        buf.resize(buf.len() * 2, 0);
                    }
                    other => return Err(other),
                },
            }
        };
        let mut dirs = Vec::new();
        let mut rest = &buf[..used];
        while !rest.is_empty() {
            let (entry, consumed) = DirEntry::decode(rest)?;
            rest = &rest[consumed..];
            if !entry.kind.is_dir() {
                continue;
            }
            // A non-UTF-8 name can never be a spelling the engine validated
            // (a locale directory) or a `<word>.app` a validated command
            // word names; skipping it loses nothing and fabricates nothing.
            if let Ok(name) = core::str::from_utf8(entry.name) {
                dirs.push(String::from(name));
            }
        }
        Ok(dirs)
    }

    /// The production [`Console`] over the inherited standard streams: the
    /// page goes to fd 1, the advisory record to fd 3 (best-effort), and the
    /// pager key is read from fd 0. The tool names only descriptors its
    /// spawner chose, so the same binary drives a serial terminal, a
    /// framebuffer console, or a future windowed terminal unchanged.
    struct RtConsole;

    impl Console for RtConsole {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(io::Error::as_errno)
        }

        fn info(&self, record: &[u8]) {
            // fd 3 is ignorable by contract: unattached is a no-op and a
            // short write is never an error a page depends on.
            let _ = StdInfo.write_all(record);
        }

        fn size(&self) -> Option<TerminalSize> {
            tairix_rt::terminal_size(STDOUT).ok()
        }

        fn read_key(&self) -> Result<Option<u8>, Errno> {
            let mut buf = [0u8; 1];
            let n = Stdin.read(&mut buf).map_err(io::Error::as_errno)?;
            Ok(if n == 0 { None } else { Some(buf[0]) })
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success, `1` when the command, its bundle, its
    /// document, or the output path fails, `2` on a usage error.
    fn main() -> i32 {
        // The worker role: this same binary, re-spawned by its own parent
        // inside the kernel sandbox spawn mode, serves the help-render
        // service over its wired standard streams and exits. Decided
        // before argument parsing — the role marker is the whole argument
        // vector's meaning.
        if worker_role() {
            let mut service = HelpService;
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }

        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let Ok(command) = parse(&arguments) else {
            write_stderr_line(USAGE);
            return 2;
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        let path = tairix_rt::env_var(b"PATH").and_then(|raw| core::str::from_utf8(raw).ok());
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        // `TERM` decides how much colour the rendered page uses (only on an
        // attested terminal); the shell exports it like `LANG`/`PATH`.
        let term = tairix_rt::env_var(b"TERM").and_then(|raw| core::str::from_utf8(raw).ok());
        let request = Request {
            locale,
            path,
            home,
            term,
        };

        // The parser sandbox the untrusted help document is rendered in:
        // this binary re-spawned in the worker role through the kernel's
        // reserved self token (the kernel substitutes the path it admitted
        // this process from — argv is data, not authority), containment
        // events routed to the system log.
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), tairix_rt::LogSink);

        let console = RtConsole;
        // The raw discipline (no echo, no indicator) only while the pager
        // can actually prompt (an attested interactive console): its
        // keystrokes are commands, and neither an echoed `q` nor the secret
        // activity marker may paint over the rendered page. The cooked
        // default is restored before exit so the next program on this
        // console sees normal interactive echo again.
        let interactive = console.size().is_some();
        if interactive {
            let _ = tairix_rt::set_input_mode(InputMode::Raw);
        }
        let result = run(&command, &request, &RtStore, &console, &mut sandbox);
        if interactive {
            let _ = tairix_rt::set_input_mode(InputMode::Cooked);
        }

        match result {
            Ok(()) => 0,
            Err(ManError::Usage) => {
                write_stderr_line(USAGE);
                2
            }
            Err(err) => {
                write_stderr_line(&format!("man: {err}"));
                1
            }
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
