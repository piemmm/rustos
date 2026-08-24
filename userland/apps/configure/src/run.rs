//! The `Run` entry-point binary of the `configure` tool — the program a
//! shell spawns to read and set the boot-time system-configuration store.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `mem_map`-backed global
//! allocator, and the syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! `main` collects the inherited argument vector, reads the `LANG` locale
//! preference from the inherited environment (plans/APPS.md §5), parses the
//! arguments with the pure [`tairix_configure`] grammar, and runs the
//! resulting command against the production seams: the syscall-backed store
//! file at `tairix_sysconfig::CONFIG_PATH` (read and replaced whole through
//! the secured VFS, which authorises every access per-inode under the
//! caller's attested identity — the tool adds no authority), the shared
//! `tairix_help::BundleHelp` for the short-help switches, and the inherited
//! standard output (fd 1). The tool binds only to its inherited
//! descriptors, never a console device.
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

    use tairix_abi::fs::OpenFlags;
    use tairix_abi::Errno;
    use tairix_configure::{parse, run, ConfigureError, Store, USAGE};
    use tairix_help::BundleHelp;
    use tairix_rt::io::{write_stderr_line, Stdout, Write};
    use tairix_sysconfig::{CONFIG_DIR, CONFIG_PATH, MAX_CONFIG_LEN};

    /// The production [`Store`] over the syscall-backed store file at
    /// [`CONFIG_PATH`], read and replaced whole. Every path resolution,
    /// per-inode permission, and mount-flag decision happens kernel-side
    /// under the caller's attested identity; the seam adds no authority.
    struct FileStore;

    impl FileStore {
        /// Read the whole store into memory, bounded by the shared
        /// engine's own document ceiling — a larger file is refused here
        /// exactly as the parser would refuse it, never half-read.
        fn read_all(fd: u32) -> Result<String, Errno> {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 512];
            while bytes.len() <= MAX_CONFIG_LEN {
                let read = tairix_rt::fs_read(fd, bytes.len() as u64, &mut chunk)
                    .map_err(Errno::from_syscall)?;
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            if bytes.len() > MAX_CONFIG_LEN {
                return Err(Errno::LengthOutOfRange);
            }
            String::from_utf8(bytes).map_err(|_| Errno::OutOfRange)
        }
    }

    impl Store for FileStore {
        fn read(&self) -> Result<Option<String>, Errno> {
            let ret = tairix_rt::fs_open(CONFIG_PATH.as_bytes(), OpenFlags::READ);
            if ret < 0 {
                // An absent store is the fresh installation, not a failure:
                // the defaults apply. Every other refusal surfaces.
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
            outcome.map(Some)
        }

        fn write(&self, text: &str) -> Result<(), Errno> {
            // The Configuration directory may not exist yet on a fresh
            // installation; create it first. `AlreadyExists` is the normal
            // steady state, not a failure.
            let ret = tairix_rt::fs_mkdir(CONFIG_DIR.as_bytes());
            if ret != 0 && Errno::from_syscall(ret) != Errno::AlreadyExists {
                return Err(Errno::from_syscall(ret));
            }
            let flags = OpenFlags::WRITE
                .union(OpenFlags::CREATE)
                .union(OpenFlags::TRUNCATE);
            let ret = tairix_rt::fs_open(CONFIG_PATH.as_bytes(), flags);
            if ret < 0 {
                return Err(Errno::from_syscall(ret));
            }
            // `ret >= 0` is a descriptor by the syscall contract.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let fd = ret as u32;
            let outcome = write_all(fd, text.as_bytes());
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

    /// The production [`tairix_configure::Output`] over the inherited
    /// standard output (fd 1).
    struct RtOutput;

    impl tairix_configure::Output for RtOutput {
        fn write_all(&self, bytes: &[u8]) -> Result<(), Errno> {
            // The shared short-write loop; a stream that stops accepting
            // bytes fails closed rather than spinning.
            Stdout.write_all(bytes).map_err(|_| Errno::NotImplemented)
        }
    }

    /// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
    /// is set up and routes its return value through the `exit` syscall.
    ///
    /// Exit codes: `0` on success (a listing, a shown value, a completed
    /// set, or the short help), `1` on a store or output failure — notably
    /// a permission denial, whose reason is stated on the diagnostic
    /// stream — `2` on a usage error (a malformed argument vector, an
    /// unknown option, an unknown key, or a value outside its key's set).
    fn main() -> i32 {
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
        // The tool's own bundle's `Help/` tree, read through the shared
        // syscall-backed source for the short-help switches.
        match run(
            command,
            locale,
            &FileStore,
            &BundleHelp::new("configure"),
            &RtOutput,
        ) {
            Ok(()) => 0,
            Err(err @ (ConfigureError::Usage | ConfigureError::UnknownKey)) => {
                write_stderr_line(&format!("configure: {err}"));
                write_stderr_line(USAGE);
                2
            }
            Err(err @ ConfigureError::InvalidValue(_)) => {
                write_stderr_line(&format!("configure: {err}"));
                2
            }
            Err(err) => {
                write_stderr_line(&format!("configure: {err}"));
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
