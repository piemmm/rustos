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
//! session against two seams: the shared `rustos_curses::StreamTty`, the
//! curses byte channel over the
//! inherited standard input/output (fd 0/1), and `RtFs`, which lists
//! directories through the kernel-authorised `fs_*` syscalls (every
//! per-inode and mount check stays kernel-side) and asks the System
//! Information API's `MOUNT_LIST` query for the status line's volume free
//! space (best-effort; an unreachable service simply omits the figure).
//! The tool binds only to its inherited descriptors, never a console
//! device, and holds no ambient authority. Invoked in the worker role
//! (`--parser-sandbox-worker`), it instead serves the sandboxed decode
//! service over its wired standard streams — the disassembly viewer's
//! container and instruction decoding runs there, never in the manager's
//! own address space.
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
    use rustos_abi::{Errno, FileKind, InputMode, UnlinkFlags, STDOUT};
    use rustos_curses::{InputMode as CursesInputMode, Screen, Size, StreamTty};
    use rustos_fstree::{run, Fs, FsEntry, Model, RenameOutcome, VolumeSpace};
    use rustos_help::{own_short_help, BundleHelp};
    use rustos_procinfo::{for_each_mount, IpcTransport};
    use rustos_rt::io::{write_stderr_line, Stdout, Write};
    use rustos_rt::File;
    use rustos_sandbox::decode::DecodeService;
    use rustos_sandbox::host::ParserSandbox;
    use rustos_sandbox::rt::{serve_stdio, worker_role, RtLauncher};
    use rustos_sandbox::worker::ServeEnd;
    use rustos_termcap::from_term;
    use rustos_vt::{Op, Parser};

    /// The conventional fallback terminal grid — 80 columns by 24 rows —
    /// applied when the kernel cannot attest the console's size (a serial
    /// line, whose remote terminal size only the far-end emulator knows).
    const FALLBACK_ROWS: u16 = 24;
    const FALLBACK_COLS: u16 = 80;

    /// Initial byte size of the directory-listing buffer: one page covers a
    /// typical directory; `BufferTooSmall` grows it (below).
    const DIR_BUF_INITIAL: usize = 4096;

    /// Ceiling for the directory-listing buffer: the kernel's own per-call
    /// staging cap ([`FS_IO_MAX`]), so the buffer grows exactly as far as
    /// one `fs_readdir` transfer can ever fill and no further.
    const DIR_BUF_MAX: usize = FS_IO_MAX;

    /// The usage banner printed when the arguments cannot be understood.
    const USAGE: &str = "usage: fstree [-h | -?] [directory]";

    /// The production [`Fs`]: directory listings and every file mutation
    /// through the kernel-authorised `fs_*` syscalls (each entry's kind,
    /// sizes, and modification stamp ride the one `fs_readdir` stream),
    /// and volume free space through the System Information API's shared
    /// mount walk.
    ///
    /// The copy engine streams a file chunk-by-chunk through path-based
    /// seam calls, so two one-slot handle caches (the open source and the
    /// open destination — the `cp` host's pattern) hoist the per-chunk
    /// open off the copy path; a mutation of a cached path drops its
    /// handle so a stale one is never written through.
    struct RtFs {
        reader: Option<(String, File)>,
        writer: Option<(String, File)>,
        mapped: Option<MappedFile>,
    }

    /// One live demand-paged mapping of the file the viewers are reading
    /// (`file_map`): the kernel backs each page on first access, so one
    /// mapping serves a file of any size — a 20 TB file costs only the
    /// pages the viewer actually shows. Dropped (released) when another
    /// file is read or the file is mutated.
    struct MappedFile {
        path: String,
        base: u64,
        len: u64,
        /// Apparent file size at map time: the hard bound on every copy,
        /// because a page wholly past end-of-file is never touched (the
        /// kernel would terminate the process — the `SIGBUS` analogue).
        size: u64,
    }

    impl MappedFile {
        /// Map the whole of `path` read-only, or `None` when the file is
        /// empty or the kernel refuses (no file-mapping window on this
        /// port, a non-mappable backing) — the caller then streams.
        fn open(path: &str) -> Option<MappedFile> {
            let file = File::open(path.as_bytes(), OpenFlags::READ).ok()?;
            let size = file.stat().ok()?.size;
            if size == 0 {
                return None;
            }
            let ret = rustos_rt::file_map(file.fd(), 0, size);
            // The mapping carries its own authority snapshot, so the
            // descriptor closes here (on drop) without affecting it.
            let base = u64::try_from(ret).ok()?;
            Some(MappedFile {
                path: String::from(path),
                base,
                len: size,
                size,
            })
        }

        /// Copy up to `buf.len()` bytes from `offset`, bounded by the
        /// mapped size (`0` at or past end of file — the seam's end
        /// signal).
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> usize {
            if offset >= self.size {
                return 0;
            }
            let available = self.size - offset;
            let count = usize::try_from(available.min(buf.len() as u64)).unwrap_or(buf.len());
            // SAFETY: the kernel mapped `[base, base + len)` read-only into
            // this process at `file_map` time and `offset + count <= size
            // <= len`, so every byte read lies inside the mapping and below
            // end-of-file; `buf` is a live, disjoint local slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (self.base + offset) as *const u8,
                    buf.as_mut_ptr(),
                    count,
                );
            }
            count
        }
    }

    impl Drop for MappedFile {
        fn drop(&mut self) {
            // Best-effort release; the kernel reclaims the region at exit
            // regardless, and a refusal here has nothing to act on.
            let _ = rustos_rt::file_unmap(self.base, self.len);
        }
    }

    impl RtFs {
        fn new() -> Self {
            Self {
                reader: None,
                writer: None,
                mapped: None,
            }
        }

        /// Drop any cached handle or mapping on `path` after a mutation of
        /// it, so stale bytes are never served or written through.
        fn forget(&mut self, path: &str) {
            if matches!(&self.reader, Some((name, _)) if name == path) {
                self.reader = None;
            }
            if matches!(&self.writer, Some((name, _)) if name == path) {
                self.writer = None;
            }
            self.forget_mapping(path);
        }

        /// Drop the cached mapping on `path` alone (resident pages are a
        /// map-time snapshot; a write to the file must invalidate them).
        fn forget_mapping(&mut self, path: &str) {
            if matches!(&self.mapped, Some(mapped) if mapped.path == path) {
                self.mapped = None;
            }
        }
    }

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
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
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

        fn stat_kind(&mut self, path: &str) -> Result<FileKind, Errno> {
            // A resolve-only open: no read authority is requested, the
            // handle is closed on drop, and only the metadata is learned.
            let file =
                File::open(path.as_bytes(), OpenFlags::empty()).map_err(Errno::from_syscall)?;
            let stat = file.stat().map_err(Errno::from_syscall)?;
            Ok(stat.kind)
        }

        fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
            // The demand-paged mapping is the preferred window source: it
            // serves a file of any size at the cost of the pages actually
            // touched. One mapping is cached — the file the viewer is
            // paging — and replaced when another file is read.
            if !matches!(&self.mapped, Some(mapped) if mapped.path == path) {
                self.mapped = MappedFile::open(path);
            }
            if let Some(mapped) = &self.mapped {
                if mapped.path == path {
                    return Ok(mapped.read_at(offset, buf));
                }
            }
            // Streamed fallback — the same bytes through `fs_read` — for a
            // file the kernel declined to map (an empty file, or a port
            // with no file-mapping window yet).
            if !matches!(&self.reader, Some((name, _)) if name == path) {
                let file =
                    File::open(path.as_bytes(), OpenFlags::READ).map_err(Errno::from_syscall)?;
                self.reader = Some((String::from(path), file));
            }
            match &self.reader {
                Some((_, file)) => file.read_at(offset, buf).map_err(Errno::from_syscall),
                // Unreachable by construction (the handle was just
                // installed), but fail closed rather than panic.
                None => Err(Errno::NotFound),
            }
        }

        fn create(&mut self, path: &str) -> Result<(), Errno> {
            // Create-or-truncate, then close: the engine writes through
            // `write`, which re-opens the destination for the stream.
            let file = rustos_rt::create(path.as_bytes()).map_err(Errno::from_syscall)?;
            drop(file);
            self.forget(path);
            Ok(())
        }

        fn write(&mut self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno> {
            // A write invalidates any mapping of the same file: resident
            // pages are a map-time snapshot and must not be served stale.
            self.forget_mapping(path);
            if !matches!(&self.writer, Some((name, _)) if name == path) {
                let file =
                    File::open(path.as_bytes(), OpenFlags::WRITE).map_err(Errno::from_syscall)?;
                self.writer = Some((String::from(path), file));
            }
            let Some((_, file)) = &self.writer else {
                // Unreachable by construction (the handle was just
                // installed), but fail closed rather than panic.
                return Err(Errno::NotFound);
            };
            // The kernel may accept a short write; every byte is the
            // seam's contract, so loop until the chunk is on disk or
            // refused.
            let mut written = 0usize;
            while written < bytes.len() {
                let n = file
                    .write_at(offset + written as u64, &bytes[written..])
                    .map_err(Errno::from_syscall)?;
                if n == 0 {
                    // A zero-byte accept would spin forever; fail closed.
                    return Err(Errno::LengthOutOfRange);
                }
                written += n;
            }
            Ok(())
        }

        fn mkdir(&mut self, path: &str) -> Result<(), Errno> {
            let ret = rustos_rt::fs_mkdir(path.as_bytes());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn remove_file(&mut self, path: &str) -> Result<(), Errno> {
            let ret = rustos_rt::fs_unlink(path.as_bytes(), UnlinkFlags::empty());
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            self.forget(path);
            Ok(())
        }

        fn remove_dir(&mut self, path: &str) -> Result<(), Errno> {
            let ret = rustos_rt::fs_unlink(path.as_bytes(), UnlinkFlags::DIRECTORY);
            if ret != 0 {
                return Err(Errno::from_syscall(ret));
            }
            Ok(())
        }

        fn rename(&mut self, src: &str, dst: &str) -> Result<RenameOutcome, Errno> {
            let ret = rustos_rt::fs_rename(src.as_bytes(), dst.as_bytes());
            if ret == 0 {
                self.forget(src);
                self.forget(dst);
                return Ok(RenameOutcome::Renamed);
            }
            match Errno::from_syscall(ret) {
                // The honest boundary report that drives the engine's
                // copy-then-remove fallback; nothing was changed.
                Errno::CrossVolume => Ok(RenameOutcome::CrossDevice),
                errno => Err(errno),
            }
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
        // The worker role: this same binary, re-spawned by its own parent
        // inside the kernel sandbox spawn mode, serves the container/
        // disassembly decode service over its wired standard streams and
        // exits. Decided before argument parsing — the role marker is the
        // whole argument vector's meaning.
        if worker_role() {
            let mut service = DecodeService;
            return match serve_stdio(&mut service) {
                ServeEnd::Finished => 0,
                ServeEnd::Failed(_) => 1,
            };
        }

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

        let mut fs = RtFs::new();
        // The parser sandbox the disassembly viewer decodes in: this
        // binary re-spawned in the worker role through the kernel's
        // reserved self token (the kernel substitutes the path it admitted
        // this process from — argv is data, not authority), containment
        // events routed to the system log.
        let mut sandbox = ParserSandbox::new(RtLauncher::own_binary(), rustos_rt::LogSink);
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
        let mut screen = Screen::new(StreamTty, term, size);
        // The session blocks on each keystroke; the kernel parks the read.
        screen.set_input_mode(CursesInputMode::Blocking);
        // Take over the display for the session: the alternate screen
        // where the terminal has one (restoring the covered content on
        // exit), an in-place erase otherwise.
        let entered = screen.enter_full_screen();
        let result = run(&mut model, &mut fs, &mut sandbox, &mut screen);
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
