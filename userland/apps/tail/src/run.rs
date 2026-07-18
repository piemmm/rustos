//! The `Run` entry-point binary of the `tail` tool — the program a shell
//! spawns to output the last part of files.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5 — the shell
//! exports it; the tool invents no second source), and runs the parsed
//! command against the production seams: `RtFileSource`, which reads named
//! files through the kernel-authorised `fs_*` syscalls (every per-inode and
//! mount check stays kernel-side), `RtStdin`, which reads the inherited
//! standard input, the shared `tairix_help::BundleHelp`, which reads the
//! tool's own bundle's `Help/` tree for the short-help switches, and
//! `RtOutput`/`RtErrors`/`RtInfo`, which write to the inherited standard
//! output, standard error, and standard information stream (fd 3). The tool
//! binds only to its inherited descriptors, never a console device, and holds
//! no ambient authority.
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

    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::String;
    use core::cell::RefCell;

    use tairix_abi::fs::OpenFlags;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::Errno;
    use tairix_help::BundleHelp;
    use tairix_procinfo::{for_each_process, IpcTransport};
    use tairix_rt::io::{write_stderr_line, StdInfo, Stderr, Stdout, Write};
    use tairix_rt::{waitset_create, waitset_ctl, waitset_wait, File};
    use tairix_tail::{parse, run, FileSource, Info, Input, Meta, Output, Watcher, USAGE};

    /// The production [`FileSource`]: the kernel-authorised `fs_*` view of the
    /// filesystem. It adds no authority — every path resolution, per-inode
    /// permission, and mount-flag check happens kernel-side under the caller's
    /// attested identity, and a refusal surfaces as the exact [`Errno`] the
    /// kernel chose.
    ///
    /// The client streams one source at a time with an advancing offset, so
    /// the handle of the file currently being streamed is kept open across
    /// calls — a file is opened once, not once per chunk — and replaced when
    /// the client moves to the next path.
    struct RtFileSource {
        open: RefCell<Option<(String, File)>>,
    }

    impl RtFileSource {
        fn new() -> Self {
            Self {
                open: RefCell::new(None),
            }
        }
    }

    impl FileSource for RtFileSource {
        fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let mut open = self.open.borrow_mut();
            let cached = matches!(&*open, Some((name, _)) if name == path);
            if !cached {
                let file =
                    File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
                *open = Some((String::from(path), file));
            }
            match &*open {
                Some((_, file)) => file.read_at(offset, buf).map_err(Errno::from_syscall),
                // Unreachable by construction (the handle was just installed),
                // but fail closed rather than panic.
                None => Err(Errno::NotFound),
            }
        }
    }

    /// The production [`Input`] over the inherited standard input (fd 0): the
    /// tool names only descriptors its spawner chose, never a console device,
    /// so the same binary reads a serial terminal, a pipe, or a future
    /// windowed terminal unchanged.
    struct RtStdin;

    impl Input for RtStdin {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            // The stream backing reports end-of-input as a zero-length read;
            // there is no error channel on the wrapper.
            Ok(tairix_rt::stdin(buf))
        }
    }

    /// The production [`Output`] over the inherited standard output (fd 1).
    struct RtOutput;

    impl Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting bytes
            // fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production diagnostic [`Output`] over the inherited standard error
    /// (fd 2).
    struct RtErrors;

    impl Output for RtErrors {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            Stderr.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// The production [`Info`] over the inherited standard information stream
    /// (fd 3). Advisory by contract: an unattached consumer or a short write
    /// is a silent no-op and never affects the output or the exit status.
    struct RtInfo;

    impl Info for RtInfo {
        fn emit(&self, record: &[u8]) {
            let _ = StdInfo.write_all(record);
        }
    }

    /// The production [`Watcher`]: the kernel-backed follow mechanism. Each
    /// opened source is an owned [`File`] (kept alive so its descriptor is
    /// not closed under us), keyed by its descriptor number; a wait-set of
    /// [`WaitSourceKind::File`] members parks the follow off-CPU until a
    /// watched node changes. `--pid` liveness is answered by the shared
    /// System Information process-list client, never a private path.
    struct RtWatcher {
        /// The wait-set the File members join; `None` if the kernel refused
        /// to mint one (then a follow degrades to its bounded timeout
        /// re-checks).
        set: Option<u64>,
        /// Descriptor number -> owning handle, so the file stays open for the
        /// life of the follow and closing is explicit.
        open: RefCell<BTreeMap<u64, File>>,
    }

    impl RtWatcher {
        fn new() -> Self {
            // `waitset_create` returns a handle or a negative `-errno`; a
            // negative result becomes `None` (no wait-set).
            let handle = waitset_create();
            Self {
                set: u64::try_from(handle).ok(),
                open: RefCell::new(BTreeMap::new()),
            }
        }

        /// Insert an opened handle into the table, returning its descriptor
        /// number as the opaque watch id.
        fn insert(&self, file: File) -> u64 {
            let fd = u64::from(file.fd());
            self.open.borrow_mut().insert(fd, file);
            fd
        }
    }

    impl Watcher for RtWatcher {
        fn open(&self, path: &str) -> Result<u64, Errno> {
            let file = File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
            Ok(self.insert(file))
        }

        fn open_dir(&self, path: &str) -> Result<u64, Errno> {
            let file =
                File::open(path.as_bytes(), OpenFlags::DIRECTORY).map_err(Errno::from_syscall)?;
            Ok(self.insert(file))
        }

        fn close(&self, handle: u64) {
            // Dropping the `File` issues `fs_close`; the wait-set member is
            // best-effort removed first so a stale id never lingers.
            self.unwatch(handle);
            let _ = self.open.borrow_mut().remove(&handle);
        }

        fn read_at(&self, handle: u64, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            let open = self.open.borrow();
            let file = open.get(&handle).ok_or(Errno::NotFound)?;
            file.read_at(offset, buf).map_err(Errno::from_syscall)
        }

        fn meta(&self, handle: u64) -> Result<Meta, Errno> {
            let open = self.open.borrow();
            let file = open.get(&handle).ok_or(Errno::NotFound)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Meta {
                id: stat.id,
                size: stat.size,
            })
        }

        fn meta_path(&self, path: &str) -> Result<Meta, Errno> {
            // A transient open purely to read the current identity/size at the
            // name; the handle closes on drop.
            let file = File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(Meta {
                id: stat.id,
                size: stat.size,
            })
        }

        fn watch(&self, handle: u64) -> Result<(), Errno> {
            let Some(set) = self.set else {
                return Err(Errno::NotImplemented);
            };
            let ret = waitset_ctl(set, WaitSetOp::Add, WaitSourceKind::File, handle, handle);
            // Already-watching (`AlreadyExists`) is success for an idempotent
            // watch; any other negative result is the real refusal.
            if ret >= 0 || ret == -i64::from(Errno::AlreadyExists.as_i32()) {
                Ok(())
            } else {
                Err(Errno::from_syscall(ret))
            }
        }

        fn unwatch(&self, handle: u64) {
            if let Some(set) = self.set {
                let _ = waitset_ctl(set, WaitSetOp::Del, WaitSourceKind::File, handle, 0);
            }
        }

        fn block(&self, timeout_ns: u64) {
            let Some(set) = self.set else {
                return;
            };
            let mut token = 0u64;
            // A spurious or timed return is fine — the follow loop re-reads
            // every source and re-parks.
            let _ = waitset_wait(set, timeout_ns, &mut token);
        }

        fn pid_alive(&self, pid: u64) -> bool {
            let transport = IpcTransport;
            // Scan one process list, returning `(pid-present, list-obtained)`.
            // Each scope owns its own `found` so the closure's mutable borrow
            // ends before the result is read.
            let scan = |all: bool| -> (bool, bool) {
                let mut found = false;
                let ok = for_each_process(&transport, all, |record| {
                    if record.pid == pid {
                        found = true;
                    }
                    Ok(())
                })
                .is_ok();
                (found, ok)
            };
            // The system-wide list is authoritative when the caller may read
            // it; if it is refused, fall back to the caller's own processes.
            // A pid the caller cannot observe reads as gone (fail closed),
            // ending the follow rather than waiting on an invisible process.
            let (found, ok) = scan(true);
            if found {
                return true;
            }
            if ok {
                return false;
            }
            scan(false).0
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` when every source was served (or the short help was
    /// written), `1` when a source failed or output could not be delivered,
    /// `2` on a usage error (a malformed argument vector, an unrecognised
    /// option, or an invalid count).
    fn main() -> i32 {
        // A malformed (non-UTF-8) argument vector is a usage error, reported
        // rather than guessed at.
        let Some(arguments) = tairix_rt::args() else {
            write_stderr_line(USAGE);
            return 2;
        };
        let command = match parse(&arguments) {
            Ok(command) => command,
            Err(err) => {
                write_stderr_line(&format!("tail: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        match run(
            command,
            locale,
            &RtFileSource::new(),
            &RtStdin,
            &RtWatcher::new(),
            &BundleHelp::new("tail"),
            &RtOutput,
            &RtErrors,
            &RtInfo,
        ) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(err) => {
                write_stderr_line(&format!("tail: {err}"));
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
