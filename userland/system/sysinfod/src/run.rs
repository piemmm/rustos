//! The `Run` entry-point binary of the System Information service, installed at
//! `/System/Services/sysinfod.app/Run` — the long-running user-space service
//! PID 1 `init` launches to answer the `sysinfo` API.
//!
//! This is a **pure-Rust** program: TAIRiX is Rust-only, so it links the Rust
//! userland runtime `tairix-rt` — never the C ABI, which exists solely for
//! programs *not* written in Rust. `tairix-rt` provides `_start`, the
//! per-process stack canary, the panic handler, the `#[global_allocator]`, and
//! the syscall wrappers (`call_create`/`call_recv`/`call_reply`/
//! `call_peer_origin`/`sysinfo_introspect`/`hw_tree_read`); `tairix_rt::entry!`
//! names this program's `main`.
//!
//! # What this service does
//!
//! `sysinfod` is the only server of the `sysinfo` API. At startup it binds the
//! well-known [`tairix_abi::sysinfo::SYSINFO_ENDPOINT`] (an unrestricted-sender
//! call endpoint — any process may query, but the id is a reserved rendezvous,
//! so binding it needs the manifest's `CAP_IPC_BIND_PRIVILEGED`: a squatter
//! could otherwise serve forged system state) and then blocks in a serve loop:
//! receive a request, read the caller's kernel-attested `Origin`
//! (`call_peer_origin`, never a caller claim), run the capability-checked
//! [`tairix_sysinfod::serve`] dispatcher against the production source that
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
// `tairix-rt` runtime through the default `program` feature. The kernel and
// host tooling build only this crate's *library*, so this module (and
// `tairix-rt`) never enter those builds.
#[cfg(all(freestanding, feature = "program"))]
mod program {
    extern crate alloc;

    use alloc::vec::Vec;

    use tairix_abi::net_ipc::{
        NetBondMemberRecord, NetInterfaceCountersRecord, NetInterfaceFactsRecord,
        NetInterfaceRatesRecord, NetInterfaceStateRecord, NetResolverServer, NetSocketRecord,
        NetStackDefenceCounters, NetstackRequest, NETSTACK_ENDPOINT, NETSTACK_LIST_LIMIT_MAX,
        NETSTACK_MAX_REPLY,
    };
    use tairix_abi::raid_admin::{
        RaidArrayRecord, RaidControlOp, RaidMemberRecord, RAID_CONTROL_ENDPOINT,
        RAID_CONTROL_MAX_REPLY, RAID_CONTROL_MAX_REQUEST, RAID_LIST_LIMIT_MAX,
    };
    use tairix_abi::reply::decode_page_reply;
    use tairix_abi::sysinfo::{
        encode_reply_err, encode_reply_ok, CacheLedgerRecord, CpuInfoRecord, CpuLoadRecord,
        CpuTimeRecord, CrashRecord, IntrospectDomain, IrqRecord, KernelMemoryStats, LoadAverage,
        MemoryPressureBand, MemoryPressureStats, MemoryTotal, MountRecord, ProcessRecord,
        RamzipStats, ResourceLimitRecord, SeatRecord, SystemIdentity, Uptime, UserDirectoryRecord,
        VolumeIoHealthRecord, RESOURCE_LIMITS_REPORT_LEN, SYSINFO_ENDPOINT, SYSINFO_MAX_REPLY,
        SYSINFO_MAX_REQUEST, SYSINFO_REPLY_STATUS_LEN,
    };
    use tairix_abi::time::Duration64;
    use tairix_abi::{Errno, LimitKind, Origin, ProcId, ORIGIN_WIRE_LEN, PROC_ID_LEN};
    use tairix_caps::CapabilitySet;
    use tairix_rt::LogSink;
    use tairix_sysinfod::{serve, CacheLedgerRegistry, Caller, ProcessScope, SysinfoSource};

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
        let n = tairix_rt::sysinfo_introspect(domain.as_u32(), 0, &mut buf).map_err(errno_from)?;
        Ok(buf[..n].to_vec())
    }

    /// Page a list domain (`Processes`/`Mounts`) to completion, returning the
    /// concatenated record bytes. Each call reads at most `chunk` bytes; a
    /// short (zero-record) read terminates the walk.
    fn read_list(domain: IntrospectDomain, record_len: usize) -> Result<Vec<u8>, Errno> {
        let mut out = Vec::new();
        // Read a healthy number of records per call to bound the syscall count.
        let per_call: usize = 64;
        let mut scratch = alloc::vec![0u8; per_call * record_len];
        let mut offset: u64 = 0;
        loop {
            let n = tairix_rt::sysinfo_introspect(domain.as_u32(), offset, &mut scratch)
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
            for chunk in bytes.as_chunks::<{ ProcessRecord::WIRE_LEN }>().0 {
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
                match tairix_rt::hw_tree_read(&mut buf) {
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
            for chunk in bytes.as_chunks::<{ MountRecord::WIRE_LEN }>().0 {
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
            for chunk in bytes.as_chunks::<{ UserDirectoryRecord::WIRE_LEN }>().0 {
                records.push(UserDirectoryRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn cpu_times(&self, _caller: &Caller) -> Result<Vec<CpuTimeRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CpuTimes, CpuTimeRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ CpuTimeRecord::WIRE_LEN }>().0 {
                records.push(CpuTimeRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn seats(&self, _caller: &Caller) -> Result<Vec<SeatRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Seats, SeatRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ SeatRecord::WIRE_LEN }>().0 {
                records.push(SeatRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn memory_pressure(&self, _caller: &Caller) -> Result<MemoryPressureStats, Errno> {
            MemoryPressureStats::from_bytes(&read_scalar(IntrospectDomain::MemoryPressure)?)
        }

        fn memory_pressure_band(&self, _caller: &Caller) -> Result<MemoryPressureBand, Errno> {
            MemoryPressureBand::from_bytes(&read_scalar(IntrospectDomain::MemoryPressureBand)?)
        }

        fn memory_total(&self, _caller: &Caller) -> Result<MemoryTotal, Errno> {
            MemoryTotal::from_bytes(&read_scalar(IntrospectDomain::MemoryTotalBytes)?)
        }

        fn cache_ledger_records(&self, _caller: &Caller) -> Result<Vec<CacheLedgerRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CacheLedgers, CacheLedgerRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ CacheLedgerRecord::WIRE_LEN }>().0 {
                records.push(CacheLedgerRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn live_process_instances(&self) -> Result<Vec<ProcId>, Errno> {
            let bytes = read_list(IntrospectDomain::Processes, ProcessRecord::WIRE_LEN)?;
            let mut instances = Vec::new();
            for chunk in bytes.as_chunks::<{ ProcessRecord::WIRE_LEN }>().0 {
                instances.push(ProcessRecord::from_bytes(chunk)?.proc_id);
            }
            Ok(instances)
        }

        fn ramzip_stats(&self, _caller: &Caller) -> Result<RamzipStats, Errno> {
            RamzipStats::from_bytes(&read_scalar(IntrospectDomain::Ramzip)?)
        }

        fn cpu_load(&self, _caller: &Caller) -> Result<Vec<CpuLoadRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CpuLoad, CpuLoadRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ CpuLoadRecord::WIRE_LEN }>().0 {
                records.push(CpuLoadRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn cpu_info(&self, _caller: &Caller) -> Result<Vec<CpuInfoRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::CpuInfo, CpuInfoRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ CpuInfoRecord::WIRE_LEN }>().0 {
                records.push(CpuInfoRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn net_interface_facts(
            &self,
            _caller: &Caller,
        ) -> Result<Vec<NetInterfaceFactsRecord>, Errno> {
            page_netstack(&NetstackFactsPage)
        }

        fn net_interface_state(
            &self,
            _caller: &Caller,
        ) -> Result<Vec<NetInterfaceStateRecord>, Errno> {
            page_netstack(&NetstackStatePage)
        }

        fn net_interface_counters(
            &self,
            _caller: &Caller,
        ) -> Result<Vec<NetInterfaceCountersRecord>, Errno> {
            page_netstack(&NetstackCountersPage)
        }

        fn net_interface_rates(
            &self,
            _caller: &Caller,
            window: Duration64,
        ) -> Result<Vec<NetInterfaceRatesRecord>, Errno> {
            page_netstack(&NetstackRatesPage { window })
        }

        fn net_stack_defence(&self, _caller: &Caller) -> Result<NetStackDefenceCounters, Errno> {
            read_netstack_defence()
        }

        fn net_sockets(&self, _caller: &Caller) -> Result<Vec<NetSocketRecord>, Errno> {
            page_netstack(&NetstackSocketsPage)
        }

        fn net_bond_members(&self, _caller: &Caller) -> Result<Vec<NetBondMemberRecord>, Errno> {
            page_netstack(&NetstackBondMembersPage)
        }

        fn net_resolver_servers(&self, _caller: &Caller) -> Result<Vec<NetResolverServer>, Errno> {
            page_netstack(&NetstackResolverServersPage)
        }

        fn irqs(&self, _caller: &Caller) -> Result<Vec<IrqRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Irqs, IrqRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ IrqRecord::WIRE_LEN }>().0 {
                records.push(IrqRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn crashes(&self, _caller: &Caller) -> Result<Vec<CrashRecord>, Errno> {
            let bytes = read_list(IntrospectDomain::Crashes, CrashRecord::WIRE_LEN)?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ CrashRecord::WIRE_LEN }>().0 {
                records.push(CrashRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn volume_io_health(&self, _caller: &Caller) -> Result<Vec<VolumeIoHealthRecord>, Errno> {
            let bytes = read_list(
                IntrospectDomain::VolumeIoHealth,
                VolumeIoHealthRecord::WIRE_LEN,
            )?;
            let mut records = Vec::new();
            for chunk in bytes.as_chunks::<{ VolumeIoHealthRecord::WIRE_LEN }>().0 {
                records.push(VolumeIoHealthRecord::from_bytes(chunk)?);
            }
            Ok(records)
        }

        fn raid_arrays(&self, _caller: &Caller) -> Result<Vec<RaidArrayRecord>, Errno> {
            page_raid(&RaidArraysPage)
        }

        fn raid_members(&self, _caller: &Caller) -> Result<Vec<RaidMemberRecord>, Errno> {
            page_raid(&RaidMembersPage)
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
                tairix_rt::sysinfo_introspect(IntrospectDomain::TaskLimits.as_u32(), 0, &mut buf)
                    .map_err(errno_from)?;
            if n < RESOURCE_LIMITS_REPORT_LEN {
                return Err(Errno::BufferTooSmall);
            }
            // Decode the positional per-kind report, one record per LimitKind.
            let mut out = [ResourceLimitRecord::new(
                LimitKind::AddressSpaceBytes,
                tairix_abi::ResourceLimit::UNLIMITED,
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

    /// One paged `netstack` broker read: which request a page issues and
    /// how its record bytes decode.
    trait NetstackPage {
        /// The decoded record type.
        type Record;
        /// The record's fixed wire length.
        const RECORD_LEN: usize;
        /// Build the request frame for one page window. Takes `&self` so a
        /// page can carry per-query parameters (the rates page's window).
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest;
        /// Decode one record, failing closed on malformed bytes.
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno>;
    }

    /// The interface-facts page.
    struct NetstackFactsPage;
    impl NetstackPage for NetstackFactsPage {
        type Record = NetInterfaceFactsRecord;
        const RECORD_LEN: usize = NetInterfaceFactsRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::InterfaceFacts { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetInterfaceFactsRecord::from_bytes(chunk)
        }
    }

    /// The interface-state page.
    struct NetstackStatePage;
    impl NetstackPage for NetstackStatePage {
        type Record = NetInterfaceStateRecord;
        const RECORD_LEN: usize = NetInterfaceStateRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::InterfaceState { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetInterfaceStateRecord::from_bytes(chunk)
        }
    }

    /// The interface-counters page.
    struct NetstackCountersPage;
    impl NetstackPage for NetstackCountersPage {
        type Record = NetInterfaceCountersRecord;
        const RECORD_LEN: usize = NetInterfaceCountersRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::InterfaceCounters { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetInterfaceCountersRecord::from_bytes(chunk)
        }
    }

    /// The interface-rates page, carrying the caller's averaging window.
    struct NetstackRatesPage {
        window: Duration64,
    }
    impl NetstackPage for NetstackRatesPage {
        type Record = NetInterfaceRatesRecord;
        const RECORD_LEN: usize = NetInterfaceRatesRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::InterfaceRates {
                offset,
                limit,
                window: self.window,
            }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetInterfaceRatesRecord::from_bytes(chunk)
        }
    }

    /// The socket-listing page.
    struct NetstackSocketsPage;
    impl NetstackPage for NetstackSocketsPage {
        type Record = NetSocketRecord;
        const RECORD_LEN: usize = NetSocketRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::Sockets { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetSocketRecord::from_bytes(chunk)
        }
    }

    /// The bond-members page.
    struct NetstackBondMembersPage;
    impl NetstackPage for NetstackBondMembersPage {
        type Record = NetBondMemberRecord;
        const RECORD_LEN: usize = NetBondMemberRecord::WIRE_LEN;
        fn request(&self, offset: u32, limit: u16) -> NetstackRequest {
            NetstackRequest::BondMembers { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetBondMemberRecord::from_bytes(chunk)
        }
    }

    /// The resolver-servers page. The set is small and closed, so the
    /// request carries no page window (the stack answers it whole); the
    /// generic pager still terminates after the single short page.
    struct NetstackResolverServersPage;
    impl NetstackPage for NetstackResolverServersPage {
        type Record = NetResolverServer;
        const RECORD_LEN: usize = NetResolverServer::WIRE_LEN;
        fn request(&self, _offset: u32, _limit: u16) -> NetstackRequest {
            NetstackRequest::ResolverServers
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            NetResolverServer::from_bytes(chunk)
        }
    }

    /// Page one `netstack` broker read to completion over the reserved
    /// endpoint. The service gates these reads on the caller's
    /// `CAP_SYSINFO_INTROSPECT` — this broker's own privileged grant —
    /// and the per-client `CAP_SYSINFO_HW`/`CAP_SYSINFO_GLOBAL`
    /// narrowing already happened in the dispatcher before this runs. A
    /// system without a running `netstack` fails closed with the
    /// transport's typed error, never a fabricated empty table.
    fn page_netstack<P: NetstackPage>(page: &P) -> Result<Vec<P::Record>, Errno> {
        let mut records = Vec::new();
        let mut reply = [0u8; NETSTACK_MAX_REPLY];
        let mut offset: u32 = 0;
        loop {
            let request = page.request(offset, NETSTACK_LIST_LIMIT_MAX);
            let n = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request.to_le_bytes(), &mut reply)
                .map_err(errno_from)?;
            let record_len = P::RECORD_LEN;
            let (count, body) =
                decode_page_reply(&reply[..n], record_len, NETSTACK_LIST_LIMIT_MAX)?;
            for chunk in body.chunks_exact(record_len) {
                records.push(P::decode(chunk)?);
            }
            if count < NETSTACK_LIST_LIMIT_MAX {
                return Ok(records);
            }
            offset = offset.saturating_add(u32::from(count));
        }
    }

    /// Read the stack-wide connection-defence counters from `netstack`.
    ///
    /// A sibling of [`page_netstack`] rather than a use of it: the reply is
    /// one fixed record, not the count-plus-records page every listing read
    /// returns, so there is no paging loop and nothing to abstract over. A
    /// short or oversized reply is refused rather than zero-filled, and a
    /// system without a running `netstack` fails closed with the
    /// transport's typed error.
    fn read_netstack_defence() -> Result<NetStackDefenceCounters, Errno> {
        let mut reply = [0u8; NETSTACK_MAX_REPLY];
        let request = NetstackRequest::StackDefence.to_le_bytes();
        let n = tairix_rt::ipc_call(NETSTACK_ENDPOINT, &request, &mut reply).map_err(errno_from)?;
        if n != NetStackDefenceCounters::WIRE_LEN {
            return Err(Errno::BadMagic);
        }
        NetStackDefenceCounters::from_bytes(&reply[..n])
    }

    /// One paged RAID composer control read: which operation a page issues
    /// and how its record bytes decode.
    ///
    /// A sibling of [`NetstackPage`]/[`page_netstack`] for a different
    /// control protocol rather than a reuse of it: the composer's
    /// [`RaidControlOp::encode`] writes a variable-length frame into a
    /// caller-supplied buffer and returns the length written, while
    /// `netstack`'s [`NetstackRequest::to_le_bytes`] returns a fixed-size
    /// array — the two request shapes do not share a signature to abstract
    /// over — and the endpoint and reply-size bound each protocol enforces
    /// also differ. The paging shape (offset/limit, page until short) is
    /// identical, which is exactly why this trait and [`page_raid`] mirror
    /// [`NetstackPage`]/[`page_netstack`] structurally rather than each
    /// query inventing its own loop.
    trait RaidPage {
        /// The decoded record type.
        type Record;
        /// The record's fixed wire length.
        const RECORD_LEN: usize;
        /// Build the control operation for one page window.
        fn op(&self, offset: u32, limit: u16) -> RaidControlOp;
        /// Decode one record, failing closed on malformed bytes.
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno>;
    }

    /// The live-arrays page.
    struct RaidArraysPage;
    impl RaidPage for RaidArraysPage {
        type Record = RaidArrayRecord;
        const RECORD_LEN: usize = RaidArrayRecord::WIRE_LEN;
        fn op(&self, offset: u32, limit: u16) -> RaidControlOp {
            RaidControlOp::ListArrays { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            RaidArrayRecord::from_bytes(chunk)
        }
    }

    /// The device-list page (array members and unaffiliated candidates
    /// alike).
    struct RaidMembersPage;
    impl RaidPage for RaidMembersPage {
        type Record = RaidMemberRecord;
        const RECORD_LEN: usize = RaidMemberRecord::WIRE_LEN;
        fn op(&self, offset: u32, limit: u16) -> RaidControlOp {
            RaidControlOp::ListMembers { offset, limit }
        }
        fn decode(chunk: &[u8]) -> Result<Self::Record, Errno> {
            RaidMemberRecord::from_bytes(chunk)
        }
    }

    /// Page one RAID composer control read to completion over the reserved
    /// [`RAID_CONTROL_ENDPOINT`]. The per-client `CAP_SYSINFO_HW` gate
    /// already passed in the dispatcher before this runs. A machine with no
    /// running array composer fails closed with the transport's typed
    /// error, never a fabricated empty table.
    fn page_raid<P: RaidPage>(page: &P) -> Result<Vec<P::Record>, Errno> {
        let mut records = Vec::new();
        let mut request = [0u8; RAID_CONTROL_MAX_REQUEST];
        let mut reply = [0u8; RAID_CONTROL_MAX_REPLY];
        let mut offset: u32 = 0;
        loop {
            let op = page.op(offset, RAID_LIST_LIMIT_MAX);
            let request_len = op.encode(&mut request)?;
            let n = tairix_rt::ipc_call(RAID_CONTROL_ENDPOINT, &request[..request_len], &mut reply)
                .map_err(errno_from)?;
            let record_len = P::RECORD_LEN;
            let (count, body) = decode_page_reply(&reply[..n], record_len, RAID_LIST_LIMIT_MAX)?;
            for chunk in body.chunks_exact(record_len) {
                records.push(P::decode(chunk)?);
            }
            if count < RAID_LIST_LIMIT_MAX {
                return Ok(records);
            }
            offset = offset.saturating_add(u32::from(count));
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
        let bound = tairix_rt::call_create(
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

        // Size the reported-cache-ledger registry from the machine's own
        // installed RAM (`CacheLedgerRegistry::new`'s policy) rather than a
        // hand-picked constant. A machine that cannot even report its own
        // RAM size is not one this service can serve correctly, so this
        // fails closed exactly like the endpoint bind above.
        let total_ram_bytes = match read_scalar(IntrospectDomain::MemoryTotalBytes)
            .and_then(|bytes| MemoryTotal::from_bytes(&bytes))
        {
            Ok(total) => total.total_bytes,
            Err(_) => return 1,
        };
        let mut registry = CacheLedgerRegistry::new(total_ram_bytes);

        let source = KernelSysinfoSource;
        let mut request = [0u8; SYSINFO_MAX_REQUEST];
        let mut origin_buf = [0u8; ORIGIN_WIRE_LEN];
        let mut reply = [0u8; SYSINFO_MAX_REPLY];
        loop {
            let mut ticket: u64 = 0;
            // A transient recv error (e.g. an oversize request left queued)
            // must not kill the server; drop it and continue.
            let Ok(request_len) = tairix_rt::call_recv(SYSINFO_ENDPOINT, &mut request, &mut ticket)
            else {
                continue;
            };

            // Attest the caller. A failure to read the peer origin is
            // fail-closed: reply an error rather than serving an unattested
            // request.
            let caller =
                match tairix_rt::call_peer_origin(SYSINFO_ENDPOINT, ticket, &mut origin_buf) {
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
                &mut registry,
                &LogSink,
                &request[..request_len],
                &mut payload,
            ) {
                Ok(len) => match encode_reply_ok(&payload[..len], &mut reply) {
                    Ok(total) => {
                        let _ = tairix_rt::call_reply(SYSINFO_ENDPOINT, ticket, &reply[..total]);
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
            let _ = tairix_rt::call_reply(SYSINFO_ENDPOINT, ticket, &reply[..total]);
        }
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
