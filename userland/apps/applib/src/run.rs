//! The `Run` entry-point binary of the `applib` tool — the program a shell
//! spawns to administer the desktop's program-library catalog.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference and the `HOME` directory from the inherited environment
//! (plans/APPS.md §5), parses the arguments with the pure [`tairix_applib`]
//! grammar, and runs the resulting command against the production seams: the
//! syscall-backed machine store at `tairix_proglib::LIBRARY_PATH`, the
//! caller's own overlay in this application's *published* app-data scope,
//! the secured-VFS store tree for the bundle-manifest reads and the `rescan`
//! walk, and the inherited standard streams (fd 1 for listings, fd 3 for the
//! advisory records).
//!
//! The two layers are gated differently, and each by the principal that owns
//! it. The machine store is an ordinary `/System/Settings` document: every
//! path resolution, per-inode permission, and mount-flag decision happens
//! kernel-side under the caller's attested identity — the tool adds no
//! authority, so only a principal that tree's policy admits can change the
//! machine-wide catalog. The overlay is reached over `APPDATA_ENDPOINT` and
//! gated on the bundle identity the kernel attests for *this* program, so
//! only `applib` can write it and no other application the user launches can
//! rewrite the account's library behind their back (plans/APPDATA.md §1.1).
//! The tool binds only to its inherited descriptors, never a console device.
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

    use tairix_abi::fs::{DirEntries, FileKind, OpenFlags};
    use tairix_abi::{BundleEntry, Errno, APPINFO_WIRE_MAX};
    use tairix_appconf::{Document, MAX_DOCUMENT_LEN};
    use tairix_appdata::RtHost;
    use tairix_applib::{
        parse, run, AppDataStore, AppLibError, Bundles, DirEntryInfo, Output, Store, Stores,
        OWN_WORD, USAGE,
    };
    use tairix_help::BundleHelp;
    use tairix_proglib::LIBRARY_PATH;
    use tairix_rt::io::{write_stderr_line, StdInfo, Stdout, Write};

    /// The production [`Store`] over the machine-wide catalog document,
    /// read and replaced whole through the secured VFS. Every path
    /// resolution, per-inode permission, and mount-flag decision happens
    /// kernel-side under the caller's attested identity; the seam adds no
    /// authority.
    ///
    /// It backs the machine layer only. The account's overlay is
    /// [`AppDataStore`], which reaches the app-data service instead — two
    /// backings of the one seam, so the tool's editing logic never learns
    /// where a catalog lives.
    struct FileStore {
        /// The document's absolute path.
        path: String,
    }

    impl FileStore {
        /// Read the whole store into memory, bounded by the format engine's
        /// own document ceiling — a larger file is refused here exactly as
        /// the engine would refuse it, never half-read.
        fn read_all(fd: u32) -> Result<String, Errno> {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 512];
            while bytes.len() <= MAX_DOCUMENT_LEN {
                let read = tairix_rt::fs_read(fd, bytes.len() as u64, &mut chunk)
                    .map_err(Errno::from_syscall)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            if bytes.len() > MAX_DOCUMENT_LEN {
                return Err(Errno::LengthOutOfRange);
            }
            String::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
        }
    }

    impl Store for FileStore {
        fn read(&self) -> Result<Option<Document>, Errno> {
            let ret = tairix_rt::fs_open(self.path.as_bytes(), OpenFlags::READ);
            if ret < 0 {
                // An absent store is the empty library, not a failure.
                // Every other refusal surfaces.
                let err = Errno::from_syscall(ret);
                return if err == Errno::NotFound {
                    Ok(None)
                } else {
                    Err(err)
                };
            }
            // `ret >= 0` is a descriptor by the syscall contract.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let fd = ret as u32;
            let outcome = Self::read_all(fd);
            let _ = tairix_rt::fs_close(fd);
            // The grammar is the format engine's, so a document it refuses is
            // refused here rather than half-read into a catalog.
            let text = outcome?;
            Document::parse(&text)
                .map(Some)
                .map_err(|_| Errno::OutOfRange)
        }

        fn write(&self, document: &Document) -> Result<(), Errno> {
            // The ProgramLibrary directory may not exist yet (a machine store
            // on an image predating it); create it first. `AlreadyExists` is
            // the normal steady state.
            if let Some((dir, _)) = self.path.rsplit_once('/') {
                let ret = tairix_rt::fs_mkdir(dir.as_bytes());
                if ret != 0 && Errno::from_syscall(ret) != Errno::AlreadyExists {
                    return Err(Errno::from_syscall(ret));
                }
            }
            let flags = OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::TRUNCATE);
            let ret = tairix_rt::fs_open(self.path.as_bytes(), flags);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let fd = ret as u32;
            let outcome = write_all(fd, document.render().as_bytes());
            let _ = tairix_rt::fs_close(fd);
            outcome
        }
    }

    /// Write every byte of `bytes` to `fd` from offset 0, looping over
    /// benign short writes; a backing that stops accepting bytes fails
    /// closed rather than spinning.
    fn write_all(fd: u32, bytes: &[u8]) -> Result<(), Errno> {
        let mut written = 0usize;
        while written < bytes.len() {
            let n = tairix_rt::fs_write(fd, written as u64, &bytes[written..])
                .map_err(Errno::from_syscall)?;
            if n == 0 {
                return Err(Errno::NoSpace);
            }
            written += n;
        }
        Ok(())
    }

    /// The production [`Bundles`] over the secured VFS: directory listings
    /// through the shared packed-stream walker, and bounded bundle-manifest
    /// reads. The kernel authorises every access; the seam adds nothing.
    struct VfsBundles;

    impl Bundles for VfsBundles {
        fn list_dir(&self, path: &str) -> Result<Option<Vec<DirEntryInfo>>, Errno> {
            let stream = match tairix_rt::read_dir_all(path.as_bytes()) {
                Ok(stream) => stream,
                Err(ret) => {
                    // An absent store root is ordinary; everything else
                    // surfaces.
                    let err = Errno::from_syscall(ret);
                    return if err == Errno::NotFound {
                        Ok(None)
                    } else {
                        Err(err)
                    };
                }
            };
            let mut entries = Vec::new();
            for entry in DirEntries::new(&stream) {
                let entry = entry?;
                let name = core::str::from_utf8(entry.name).map_err(|_| Errno::OutOfRange)?;
                entries.push(DirEntryInfo {
                    name: String::from(name),
                    directory: entry.kind == FileKind::Directory,
                });
            }
            Ok(Some(entries))
        }

        fn read_appinfo(&self, bundle: &str) -> Result<Option<Vec<u8>>, Errno> {
            let path = format!("{bundle}/{}", BundleEntry::AppInfo.as_str());
            let ret = tairix_rt::fs_open(path.as_bytes(), OpenFlags::READ);
            if ret < 0 {
                // A directory without a manifest is simply not a bundle.
                let err = Errno::from_syscall(ret);
                return if err == Errno::NotFound {
                    Ok(None)
                } else {
                    Err(err)
                };
            }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let fd = ret as u32;
            let outcome = read_bounded(fd);
            let _ = tairix_rt::fs_close(fd);
            outcome.map(Some)
        }
    }

    /// Read a manifest of at most [`APPINFO_WIRE_MAX`] bytes from `fd`; a
    /// longer file cannot be a valid manifest and is refused rather than
    /// half-read.
    fn read_bounded(fd: u32) -> Result<Vec<u8>, Errno> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 512];
        while bytes.len() <= APPINFO_WIRE_MAX {
            let read = tairix_rt::fs_read(fd, bytes.len() as u64, &mut chunk)
                .map_err(Errno::from_syscall)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        if bytes.len() > APPINFO_WIRE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(bytes)
    }

    /// The production [`Output`]: listings on the inherited standard output
    /// (fd 1), advisory records on the inherited `stdinfo` (fd 3,
    /// best-effort by the stream contract).
    struct RtOutput;

    impl Output for RtOutput {
        fn out(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }

        fn info(&self, bytes: &[u8]) {
            // fd 3 is advisory: a missing or refusing consumer never
            // changes the command's result.
            let _ = StdInfo.write_all(bytes);
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success (a listing, a completed change, a rescan,
    /// or the short help), `1` on a store, bundle, or output failure —
    /// notably a permission denial, whose reason is stated on the
    /// diagnostic stream — `2` on a usage or validation error (a malformed
    /// argument vector, an unknown option, folder, or entry, or a bundle
    /// that is not registrable as asked).
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
                write_stderr_line(&format!("{OWN_WORD}: {err}"));
                write_stderr_line(USAGE);
                return 2;
            }
        };
        let locale = tairix_rt::env_var(b"LANG").and_then(|raw| core::str::from_utf8(raw).ok());
        // The inherited HOME is the `rescan --user` walk's, and nothing
        // else's: the overlay itself is resolved by the app-data service from
        // the identity the kernel attests for this task, so no path here
        // names it and a home is not needed to reach it.
        let home = tairix_rt::env_var(b"HOME").and_then(|raw| core::str::from_utf8(raw).ok());
        let machine = FileStore {
            path: String::from(LIBRARY_PATH),
        };
        let user = AppDataStore::new(RtHost);
        let stores = Stores {
            machine: &machine,
            user: &user,
            home,
        };
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &stores,
            &VfsBundles,
            &BundleHelp::new(OWN_WORD),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err @ AppLibError::Usage) => {
                write_stderr_line(&format!("{OWN_WORD}: {err}"));
                write_stderr_line(USAGE);
                2
            }
            Err(
                err @ (AppLibError::UnknownFolder
                | AppLibError::UnknownEntry
                | AppLibError::NotListed
                | AppLibError::NoManifest
                | AppLibError::BadManifest
                | AppLibError::Entry(_)),
            ) => {
                write_stderr_line(&format!("{OWN_WORD}: {err}"));
                2
            }
            Err(err) => {
                write_stderr_line(&format!("{OWN_WORD}: {err}"));
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
