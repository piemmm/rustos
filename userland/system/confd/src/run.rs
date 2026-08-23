//! The `Run` entry-point binary of the app-data service, installed at
//! `/System/Services/confd.app/Run` — the long-running user-space service PID 1
//! `init` launches so every application has a private, isolated settings store
//! (`plans/APPDATA.md` AD4).
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt`, never the C ABI. `tairix-rt` provides
//! `_start`, the per-process stack canary, the panic handler, the
//! `#[global_allocator]`, the `fs_*` file API the store reads and writes
//! through, the `random_get` draw the sealed scope's key material and nonces
//! come from, and the endpoint syscall wrappers; `tairix_rt::entry!` names this
//! program's `main`.
//!
//! # What this service does
//!
//! It binds the reserved [`APPDATA_ENDPOINT`](tairix_abi::appdata_ipc::APPDATA_ENDPOINT)
//! — a rendezvous whose bind needs the manifest's `CAP_IPC_BIND_PRIVILEGED`,
//! because a squatter that claimed it first could serve forged settings to
//! every application on the machine — and then blocks in a serve loop: receive
//! a request, read the caller's kernel-attested origin, answer, reply.
//!
//! It is a **boot-floor** service, not a graphical one: a headless machine
//! needs it exactly as much as a desktop does, because the shell and every
//! command app reach their own settings through it. It comes up before any
//! volume is unlocked and answers `DeviceOffline` until storage is reachable,
//! so an early caller gets a typed refusal rather than a guess.
//!
//! If the endpoint cannot be bound the service records `SERVICE_UNAVAILABLE`
//! and exits fail-closed; PID 1 supervises and relaunches it. It never serves
//! settings it could not authorise.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the optional
// `tairix-rt` runtime through the default `program` feature. Host tooling
// builds only this crate's *library*, so this module (and `tairix-rt`) never
// enter that build.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::string::String;
    use alloc::vec::Vec;

    use tairix_abi::appdata_ipc::{APPDATA_ENDPOINT, APPDATA_MAX_REPLY, APPDATA_MAX_REQUEST};
    use tairix_abi::fs::{DirEntries, OpenFlags};
    use tairix_abi::random::RandomFlags;
    use tairix_abi::{Errno, Origin, ORIGIN_WIRE_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_confd::events::{SERVICE_READY, SERVICE_UNAVAILABLE};
    use tairix_confd::{AppData, Entropy, Storage};
    use tairix_log::{Event, EventId, Level};
    use tairix_rt::{File, LogSink};
    use zeroize::Zeroize;

    /// Outstanding-call capacity of the endpoint (a fail-closed memory bound).
    ///
    /// A settings read is a short, synchronous exchange, so a caller is
    /// serviced and gone; this bounds how many may be queued behind the one
    /// being served before a poster is made to wait.
    const CAPACITY: usize = 16;

    /// The audit sink every record is written through.
    static LOG_SINK: LogSink = LogSink;

    /// Record a startup outcome.
    fn record(id: EventId, level: Level, message: &str) {
        let _ = tairix_log::log(
            &LOG_SINK,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// The real store filesystem, reached through `tairix-rt`'s `fs_*`
    /// wrappers under this service's own attested identity — so the VFS still
    /// authorises every path per-inode, and the gated store roots admit it
    /// only because its manifest carries `CAP_APPDATA_ADMIN`.
    struct RealStorage;

    /// Translate a raw negative kernel result into a typed [`Errno`].
    fn errno(raw: i64) -> Errno {
        Errno::from_syscall(raw)
    }

    /// The sealed scope's randomness, drawn from the one kernel random
    /// subsystem.
    ///
    /// The draw **blocks** through a required reseed rather than passing
    /// `NON_BLOCKING`: a master secret or a nonce is key material, and waiting
    /// for the generator is right where answering "not ready" to a user saving
    /// a password is not. An early-boot caller therefore waits for entropy
    /// instead of being refused, and no key material is ever derived from an
    /// unseeded generator.
    struct RealEntropy;

    impl Entropy for RealEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), Errno> {
            let mut done = 0usize;
            while done < out.len() {
                let drawn =
                    tairix_rt::random_get(&mut out[done..], RandomFlags::empty()).map_err(errno)?;
                if drawn == 0 {
                    return Err(Errno::EntropyNotReady);
                }
                done += drawn;
            }
            Ok(())
        }
    }

    impl Storage for RealStorage {
        fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno> {
            let file = File::open(path.as_bytes(), OpenFlags::READ).map_err(errno)?;
            let size = usize::try_from(file.stat().map_err(errno)?.size)
                .map_err(|_| Errno::LengthOutOfRange)?;
            let mut buf = Vec::new();
            buf.try_reserve_exact(size)
                .map_err(|_| Errno::OutOfMemory)?;
            buf.resize(size, 0);
            let mut done = 0usize;
            while done < size {
                let read = file.read_at(done as u64, &mut buf[done..]).map_err(errno)?;
                if read == 0 {
                    break;
                }
                done += read;
            }
            buf.truncate(done);
            Ok(buf)
        }

        fn write(&mut self, path: &str, bytes: &[u8]) -> Result<(), Errno> {
            let file = File::open(
                path.as_bytes(),
                OpenFlags::WRITE
                    .union(OpenFlags::CREATE)
                    .union(OpenFlags::TRUNCATE),
            )
            .map_err(errno)?;
            let mut done = 0usize;
            while done < bytes.len() {
                let written = file.write_at(done as u64, &bytes[done..]).map_err(errno)?;
                if written == 0 {
                    return Err(Errno::NoSpace);
                }
                done += written;
            }
            // The publish step renames this file over the live document, so
            // the bytes must be on the medium before the rename makes them
            // the document: without the flush a crash could leave the name
            // pointing at a shorter file than the caller committed.
            file.sync().map_err(errno)
        }

        fn rename(&mut self, src: &str, dst: &str) -> Result<(), Errno> {
            let ret = tairix_rt::fs_rename(src.as_bytes(), dst.as_bytes());
            if ret == 0 {
                Ok(())
            } else {
                Err(errno(ret))
            }
        }

        fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), Errno> {
            let ret = tairix_rt::fs_mkdir(path.as_bytes());
            if ret != 0 {
                return Err(errno(ret));
            }
            let ret = tairix_rt::fs_set_mode(path.as_bytes(), mode);
            if ret == 0 {
                Ok(())
            } else {
                Err(errno(ret))
            }
        }

        fn owner_of(&mut self, path: &str) -> Result<u32, Errno> {
            // The **resolve-only** posture (no read, no write, no directory
            // flag): the store roots and the homes above them are directories
            // this service may traverse but not read, so asking for byte
            // access would be refused — and a directory open with read access
            // is refused outright anyway. A resolve-only handle needs nothing
            // but search permission on the path's own ancestors, which is
            // exactly what the transit grant confers, and its stat reports the
            // owning uid this check turns on.
            let node = File::open(path.as_bytes(), OpenFlags::empty()).map_err(errno)?;
            Ok(node.stat().map_err(errno)?.uid)
        }

        fn list_dir(&mut self, path: &str) -> Result<Vec<String>, Errno> {
            let stream = tairix_rt::read_dir_all(path.as_bytes()).map_err(errno)?;
            let mut names = Vec::new();
            for entry in DirEntries::new(&stream) {
                let entry = entry?;
                if !entry.kind.is_dir() {
                    continue;
                }
                if let Ok(name) = core::str::from_utf8(entry.name) {
                    names.push(String::from(name));
                }
            }
            Ok(names)
        }
    }

    /// Bind `APPDATA_ENDPOINT`, or record the refusal and fail closed.
    ///
    /// `send_caps` is empty: reaching one's *own* settings is not a privileged
    /// act, so any process may ask, and the dispatcher decides what — if
    /// anything — that caller has a store for. `recv_caps` is empty because
    /// endpoint ownership already restricts receive to this task.
    fn bind() -> bool {
        let empty = CapabilitySet::empty();
        if tairix_rt::call_create(
            APPDATA_ENDPOINT,
            &empty,
            &empty,
            APPDATA_MAX_REQUEST,
            APPDATA_MAX_REPLY,
            CAPACITY,
        ) == 0
        {
            return true;
        }
        record(
            SERVICE_UNAVAILABLE,
            Level::Warn,
            "confd: cannot bind APPDATA_ENDPOINT",
        );
        false
    }

    /// Serve requests for the life of the service, returning the exit code.
    fn serve(service: &mut AppData<LogSink, RealEntropy>) -> i32 {
        let mut fs = RealStorage;
        let mut request = [0u8; APPDATA_MAX_REQUEST];
        let mut reply = alloc::vec![0u8; APPDATA_MAX_REPLY];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        loop {
            let mut ticket: u64 = 0;
            // A transient receive error must not kill the server; the
            // endpoint's own `max_request` bound means an oversize request is
            // refused at post time and never left queued.
            let Ok(len) = tairix_rt::call_recv(APPDATA_ENDPOINT, &mut request, &mut ticket) else {
                continue;
            };
            // The caller's identity comes from the kernel, never from the
            // frame. A peer whose origin cannot be read is answered nothing:
            // there is no store to serve without knowing who is asking.
            let Ok(origin_len) =
                tairix_rt::call_peer_origin(APPDATA_ENDPOINT, ticket, &mut origin_buf)
            else {
                continue;
            };
            let Ok(origin) = Origin::from_bytes(&origin_buf[..origin_len]) else {
                continue;
            };
            let reply_len = service.serve(
                &mut fs,
                &origin,
                tairix_rt::clock_get(),
                &request[..len],
                &mut reply,
            );
            if reply_len > 0 {
                let _ = tairix_rt::call_reply(APPDATA_ENDPOINT, ticket, &reply[..reply_len]);
            }
            // Both buffers are reused for the life of the service, and a
            // sealed-scope request or reply carries plaintext secret material,
            // so neither is left holding one for the next caller's request to
            // sit beside. Only the bytes actually used are wiped, so the cost
            // is proportional to the frame just served rather than to the
            // buffers' full width.
            request[..len].zeroize();
            reply[..reply_len].zeroize();
        }
    }

    /// Bind the endpoint and serve for the life of the service. Returns a
    /// non-zero exit code on a fail-closed startup error.
    fn main() -> i32 {
        if !bind() {
            return 1;
        }
        record(
            SERVICE_READY,
            Level::Info,
            "confd: serving APPDATA_ENDPOINT",
        );
        let mut service = AppData::new(LOG_SINK, RealEntropy);
        serve(&mut service)
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `tairix-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
