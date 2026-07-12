//! The `Run` entry-point binary of the System Information service, installed
//! at `/System/Services/sysinfod.app/Run` — the long-running user-space service PID 1
//! `init` launches to answer the `sysinfo` API (`AGENTS.md` §16.6).
//!
//! This is a **pure-Rust** program: RustOS is Rust-only, so it links the Rust
//! userland runtime `rustos-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `rustos-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `#[global_allocator]`, and
//! the syscall wrappers (`call_create`/`call_recv`/`call_reply`/
//! `call_peer_origin`/`sysinfo_introspect`/`hw_tree_read`); `rustos_rt::entry!`
//! names this program's `main`.
//!
//! # What this service does
//!
//! `sysinfod` is the only server of the `sysinfo` API. At startup it binds the
//! well-known [`rustos_abi::sysinfo::SYSINFO_ENDPOINT`] (an unrestricted-sender
//! call endpoint — any process may query, but the id is a reserved rendezvous,
//! so binding it needs the manifest's `CAP_IPC_BIND_PRIVILEGED`: a squatter
//! could otherwise serve forged system state) and then blocks in a serve loop:
//! receive a request, read the caller's kernel-attested `Origin`
//! (`call_peer_origin`, never a caller claim), run the capability-checked
//! [`rustos_sysinfod::serve`] dispatcher against the production source that
//! reads the kernel's live introspection view, and reply.
//!
//! The dispatcher is the **broker**: it holds the privileged
//! `CAP_SYSINFO_INTROSPECT` (and `CAP_SYSINFO_HW`) and enforces every
//! per-client scope (self vs global, the `CAP_SYSINFO_GLOBAL`/`_KERNEL`/`_HW`
//! gates) against each requester's attested origin before it ever reads the
//! kernel's global introspection view. The kernel primitive always answers
//! with the whole system's state; all narrowing lives here in the audited
//! userland broker.
//!
//! On the host it is an inert stub so `cargo build --workspace`, clippy, and
//! fmt still cover the file.

#![cfg_attr(all(freestanding, feature = "program"), no_std)]
#![cfg_attr(all(freestanding, feature = "program"), no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
// Compiled only for the freestanding service binary, which links the optional
// `rustos-rt` runtime through the default `program` feature. The kernel and
// host tooling build only this crate's *library*, so this module (and
// `rustos-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;

    use rustos_abi::sysinfo::{
        encode_reply_err, encode_reply_ok, CpuLoadRecord, CpuTimeRecord, IntrospectDomain,
        KernelMemoryStats, LoadAverage, MemoryPressureStats, MountRecord, ProcessRecord,
        RamzipStats, ReclaimClassRecord, ResourceLimitRecord, SeatRecord, SystemIdentity, Uptime,
        UserDirectoryRecord, RESOURCE_LIMITS_REPORT_LEN, SYSINFO_ENDPOINT, SYSINFO_MAX_REPLY,
        SYSINFO_MAX_REQUEST, SYSINFO_REPLY_STATUS_LEN,
    };
    use rustos_abi::{Errno, LimitKind, Origin, ORIGIN_WIRE_LEN, PROC_ID_LEN};
    use rustos_caps::CapabilitySet;
    use rustos_rt::LogSink;
    use rustos_sysinfod::{serve, Caller, ProcessScope, SysinfoSource};

    /// Outstanding-call capacity of the endpoint (a fail-closed memory bound).
    const CAPACITY: usize = 8;
    /// Payload capacity a framed reply leaves after its status word.
    const REPLY_PAYLOAD_CAP: usize = SYSINFO_MAX_REPLY - SYSINFO_REPLY_STATUS_LEN;

    /// Recover the [`Errno`] a syscall encoded as a negative register
    /// (`-errno`); an unrecognised code fails closed as
    /// [`Errno::NotImplemented`] rather than being guessed.
    fn errno_from(ret: i64) -> Errno {
        i32::try_from(-ret)
            .ok()
            .and_then(Errno::from_i32)
            .unwrap_or(Errno::NotImplemented)
    }

    /// Read one whole scalar domain (`KernelMemory`/`Identity`/`Uptime`) into
    /// an owned buffer via a single `sysinfo_introspect` call.
    fn read_scalar(domain: IntrospectDomain) -> Result<Vec<u8>, Errno> {
        // A page comfortably larger than any scalar record; the kernel writes
        // only the record's bytes and returns the count.
        let mut buf = [0u8; 256];
        let n = rustos_rt::sysinfo_introspect(domain.as_u32(), 0, &mut buf).map_err(errno_from)?;
        Ok(buf[..n].to_vec())
    }

    /// Page a list domain (`Processes`/`Mounts`) to completion, returning the
    /// concatenated record bytes. Each call reads at most `chunk` bytes; a
    /// short (zero-record) read terminates the walk.
    fn read_list(domain: IntrospectDomain, record_len: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::new();
        // Read a healthy number of records per call to bound the syscall count.
        let per_call = 64usize.max(1);
        let mut scratch = alloc::vec![0u8; per_call * record_len];
        let mut offset: u64 = 0;
        loop {
            let n = rustos_rt::sysinfo_introspect(domain.as_u32(), offset, &mut scratch)
                .map_err(errno_from)?;
            if n == 0 {
                break;
            }
            let records = n / record_len;
            if records == 0 {
                // The kernel guarantees whole records; a partial read is
                // impossible, so treat it as end-of-list rather than looping.
                break;
            }
            out.extend_from_slice(&scratch[..records * record_len]);
            offset += records as u64;
        }
        Ok(out)
    }

    /// The production [`SysinfoSource`]: it answers every query from the
    /// kernel's live introspection primitive (`sysinfo_introspect`) and the
    /// discovered hardware tree (`hw_tree_read`), decoding each wire answer
    /// into the owned records the dispatcher pages. It holds no state and adds
    /// no authority: the kernel re-checks `CAP_SYSINFO_INTROSPECT` /
    /// `CAP_SYSINFO_HW` on every call.
    struct KernelSysinfoSource;

    impl SysinfoSource for KernelSysinfoSource {
        fn process_records(
            &self,
            caller: &Caller,
            scope: ProcessScope,
        ) -> Result<Vec<ProcessRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Processes, ProcessRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(ProcessRecord::WIRE_LEN) {
                let record = ProcessRecord::from_bytes(chunk)?;
                // Self-scope narrowing happens here in the broker: the kernel
                // returns every process, and a self-scoped query keeps only
                // those the attested caller owns.
                if scope == ProcessScope::Caller && record.uid != caller.uid() {
                    continue;
                }
                records.push(record);
            }
            Ok(records)
        }

        fn kernel_memory_stats(&self, _caller: &Caller) -> Result<KernelMemoryStats, Errno> {
            KernelMemoryStats::from_bytes(&read_scalar(IntrospectDomain::KernelMemory)?)
        }

        fn hardware_tree(&self, _caller: &Caller) -> Result<Vec<u8>, Errno> {
            // Read the discovered tree, growing the buffer until it fits.
            let mut buf = alloc::vec![0u8; 4096];
            loop {
                match rustos_rt::hw_tree_read(&mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        return Ok(buf);
                    }
                    Err(ret) => {
                        let err = errno_from(ret);
                        // Grow once on a too-small buffer; any other error is
                        // surfaced fail-closed.
                        if err == Errno::BufferTooSmall && buf.len() < (1 << 20) {
                            let len = buf.len() * 2;
                            buf.resize(len, 0);
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }

        fn system_identity(&self, _caller: &Caller) -> Result<SystemIdentity, Errno> {
            SystemIdentity::from_bytes(&read_scalar(IntrospectDomain::Identity)?)
        }

        fn uptime(&self, _caller: &Caller) -> Result<Uptime, Errno> {
            Uptime::from_bytes(&read_scalar(IntrospectDomain::Uptime)?)
        }

        fn load_average(&self, _caller: &Caller) -> Result<LoadAverage, Errno> {
            LoadAverage::from_bytes(&read_scalar(IntrospectDomain::LoadAverage)?)
        }

        fn mount_records(&self, _caller: &Caller) -> Result<Vec<MountRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Mounts, MountRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(MountRecord::WIRE_LEN) {
                records.push(MountRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn user_directory(&self, _caller: &Caller) -> Result<Vec<UserDirectoryRecord>, Errno> {
            let bytes = read_list(
                IntrospectDomain::UserDirectory,
                UserDirectoryRecord::WIRE_LEN,
            )?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(UserDirectoryRecord::WIRE_LEN) {
                records.push(UserDirectoryRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn cpu_times(&self, _caller: &Caller) -> Result<Vec<CpuTimeRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CpuTimes, CpuTimeRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(CpuTimeRecord::WIRE_LEN) {
                records.push(CpuTimeRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn seats(&self, _caller: &Caller) -> Result<Vec<SeatRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Seats, SeatRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(SeatRecord::WIRE_LEN) {
                records.push(SeatRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn memory_pressure(&self, _caller: &Caller) -> Result<MemoryPressureStats, Errno> {
            MemoryPressureStats::from_bytes(&read_scalar(IntrospectDomain::MemoryPressure)?)
        }

        fn reclaim_records(&self, _caller: &Caller) -> Result<Vec<ReclaimClassRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Reclaim, ReclaimClassRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(ReclaimClassRecord::WIRE_LEN) {
                records.push(ReclaimClassRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn ramzip_stats(&self, _caller: &Caller) -> Result<RamzipStats, Errno> {
            RamzipStats::from_bytes(&read_scalar(IntrospectDomain::Ramzip)?)
        }

        fn cpu_load(&self, _caller: &Caller) -> Result<Vec<CpuLoadRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CpuLoad, CpuLoadRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.chunks_exact(CpuLoadRecord::WIRE_LEN) {
                records.push(CpuLoadRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn resource_limits(
            &self,
            caller: &Caller,
        ) -> Result<[ResourceLimitRecord; LimitKind::COUNT], Errno> {
            // The per-task-limits domain names its target by ProcId, written
            // into the output buffer on entry (a u64 arg cannot carry it). A
            // client reads only its *own* limits: the broker passes the
            // caller's attested ProcId, never a caller-supplied one.
            let mut buf = [0u8; RESOURCE_LIMITS_REPORT_LEN];
            let proc_id = caller.origin().proc_id().to_le_bytes();
            buf[..PROC_ID_LEN].copy_from_slice(&proc_id);
            let n =
                rustos_rt::sysinfo_introspect(IntrospectDomain::TaskLimits.as_u32(), 0, &mut buf)
                    .map_err(errno_from)?;
            if n < RESOURCE_LIMITS_REPORT_LEN {
                return Err(Errno::BufferTooSmall);
            }
            // Decode the positional per-kind report, one record per LimitKind.
            let mut out = [ResourceLimitRecord::new(
                LimitKind::AddressSpaceBytes,
                rustos_abi::ResourceLimit::UNLIMITED,
                0,
            ); LimitKind::COUNT];
            for (index, slot) in out.iter_mut().enumerate() {
                let base = index * ResourceLimitRecord::WIRE_LEN;
                *slot = ResourceLimitRecord::from_bytes(
                    &buf[base..base + ResourceLimitRecord::WIRE_LEN],
                )?;
            }
            Ok(out)
        }
    }

    /// Bind the endpoint and serve requests for the life of the service.
    ///
    /// The endpoint is unrestricted-sender (empty `send_caps`), so any process
    /// may post — per-query scoping is enforced by the dispatcher against each
    /// caller's attested origin, not by the transport. `recv_caps` is empty:
    /// endpoint ownership already restricts receive to this task.
    fn main() -> i32 {
        let empty = CapabilitySet::empty();
        let bound = rustos_rt::call_create(
            SYSINFO_ENDPOINT,
            &empty,
            &empty,
            SYSINFO_MAX_REQUEST,
            SYSINFO_MAX_REPLY,
            CAPACITY,
        );
        if bound != 0 {
            // Could not publish the endpoint (already bound, or no registry):
            // fail closed; PID 1 supervises and relaunches.
            return 1;
        }

        let source = KernelSysinfoSource;
        let mut request = [0u8; SYSINFO_MAX_REQUEST];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        let mut reply = [0u8; SYSINFO_MAX_REPLY];
        loop {
            let mut ticket: u64 = 0;
            let request_len =
                match rustos_rt::call_recv(SYSINFO_ENDPOINT, &mut request, &mut ticket) {
                    Ok(len) => len,
                    // A transient recv error (e.g. an oversize request left
                    // queued) must not kill the server; drop it and continue.
                    Err(_) => continue,
                };

            // Attest the caller. A failure to read the peer origin is
            // fail-closed: reply an error rather than serving an unattested
            // request.
            let caller =
                match rustos_rt::call_peer_origin(SYSINFO_ENDPOINT, ticket, &mut origin_buf) {
                    Ok(n) => match Origin::from_bytes(&origin_buf[..n]) {
                        Ok(origin) => Caller::new(origin),
                        Err(err) => {
                            reply_error(&mut reply, ticket, err);
                            continue;
                        }
                    },
                    Err(ret) => {
                        reply_error(&mut reply, ticket, errno_from(ret));
                        continue;
                    }
                };

            // Serve into the framed reply's payload region, then prepend the
            // status word. A dispatcher error becomes an error frame so the
            // client sees the exact refusal (e.g. `PermissionDenied`).
            let mut payload = [0u8; REPLY_PAYLOAD_CAP];
            match serve(
                &source,
                &caller,
                &LogSink,
                &request[..request_len],
                &mut payload,
            ) {
                Ok(len) => match encode_reply_ok(&payload[..len], &mut reply) {
                    Ok(total) => {
                        let _ = rustos_rt::call_reply(SYSINFO_ENDPOINT, ticket, &reply[..total]);
                    }
                    Err(err) => reply_error(&mut reply, ticket, err),
                },
                Err(err) => reply_error(&mut reply, ticket, err),
            }
        }
    }

    /// Answer `ticket` with a framed error reply. A failure to even frame the
    /// error is dropped: the client's `ipc_call` then observes a truncated
    /// reply and fails closed on decode.
    fn reply_error(reply: &mut [u8], ticket: u64, err: Errno) {
        if let Ok(total) = encode_reply_err(err, reply) {
            let _ = rustos_rt::call_reply(SYSINFO_ENDPOINT, ticket, &reply[..total]);
        }
    }

    rustos_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// Whenever the real freestanding `rustos-rt` `_start` path is not compiled —
// on the host (`cargo build --workspace`, clippy, fmt), or for a
// `program`-less build of this crate — this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(all(freestanding, feature = "program")))]
fn main() {}
