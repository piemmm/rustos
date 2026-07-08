//! The request dispatcher: the one place a `sysinfo` request is decoded,
//! capability-checked, audited, and answered.

use rustos_abi::sysinfo::{
    spec_for, CpuTimeListRequest, CpuTimeRecord, MountListRequest, MountRecord, ProcessListRequest,
    ProcessRecord, ResourceLimitRecord, SeatListRequest, SeatRecord, SysinfoQueryId,
    SysinfoRequestHeader, UserDirectoryRecord, UserDirectoryRequest,
};
use rustos_abi::{Errno, LimitKind};
use rustos_log::{log, Event, EventId, Field, Level, Sink};

use crate::events;
use crate::source::{Caller, ProcessScope, SysinfoSource};

/// Serve one System Information request.
///
/// Decodes the [`SysinfoRequestHeader`] (and any typed payload) from
/// `request`, enforces the query's declared capability against `caller`,
/// emits an audit record through `audit` where the query demands it, and
/// writes the encoded typed response into `response`, returning the number
/// of bytes written.
///
/// The pipeline **fails closed**: the capability check
/// happens before any data is touched, and every early return leaves
/// `response` untouched. There is no path that answers a privileged query
/// without first passing its capability gate — `sysinfod` is the only
/// server of the API and the kernel exposes no bypass.
///
/// # Response encoding
///
/// * Process-list queries return zero or more [`ProcessRecord`]s packed
///   back-to-back; the caller pages with the request's `offset`/`limit` and
///   detects the end of the list when it receives fewer than `limit`
///   records.
/// * The scalar queries return the little-endian wire image of their
///   response struct.
/// * The hardware-tree query returns the source's encoded bytes verbatim.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `request` is shorter than its declared
///   payload, or `response` cannot hold the encoded answer.
/// * [`Errno::BadMagic`] / [`Errno::AbiVersionUnsupported`] /
///   [`Errno::OutOfRange`] / [`Errno::LengthOutOfRange`] — the header or
///   payload failed to decode against `sysinfo-v1`.
/// * [`Errno::PermissionDenied`] — the caller lacks the query's required
///   capability.
/// * [`Errno::NotImplemented`] — the request named a reserved-but-unassigned
///   query identifier.
/// * Any error returned by the backing [`SysinfoSource`].
pub fn serve(
    source: &dyn SysinfoSource,
    caller: &Caller,
    audit: &dyn Sink,
    request: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let header = match SysinfoRequestHeader::from_bytes(request) {
        Ok(header) => header,
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::REQUEST_MALFORMED,
                "sysinfo request rejected: header decode failed",
                &[],
            );
            return Err(err);
        }
    };

    let Some(spec) = spec_for(header.query) else {
        emit(
            audit,
            Level::Warn,
            events::QUERY_UNAVAILABLE,
            "sysinfo query rejected: unassigned query identifier",
            &[],
        );
        return Err(Errno::NotImplemented);
    };

    let payload_len = header.payload_len as usize;
    let payload_end = SysinfoRequestHeader::WIRE_LEN
        .checked_add(payload_len)
        .ok_or(Errno::LengthOutOfRange)?;
    if request.len() < payload_end {
        emit(
            audit,
            Level::Warn,
            events::REQUEST_MALFORMED,
            "sysinfo request rejected: declared payload is truncated",
            &[query_field(spec.name)],
        );
        return Err(Errno::BufferTooSmall);
    }
    let payload = &request[SysinfoRequestHeader::WIRE_LEN..payload_end];

    if let Some(required) = spec.required_capability {
        if !caller.capabilities().holds(required) {
            emit(
                audit,
                Level::Warn,
                events::QUERY_DENIED,
                "sysinfo query denied: caller lacks required capability",
                &[query_field(spec.name)],
            );
            return Err(Errno::PermissionDenied);
        }
    }

    // Record the invocation before dispatch so every privileged call is
    // accounted for even if the backing source later errors.
    if spec.audit {
        emit(
            audit,
            Level::Info,
            events::QUERY_SERVED,
            "sysinfo query served",
            &[query_field(spec.name)],
        );
    }

    dispatch(source, caller, header.query, payload, response)
}

/// Route a capability-cleared request to its [`SysinfoSource`] method and
/// encode the answer.
fn dispatch(
    source: &dyn SysinfoSource,
    caller: &Caller,
    query: SysinfoQueryId,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    if query == SysinfoQueryId::SELF_PROCESS_LIST {
        process_list(source, caller, ProcessScope::Caller, payload, response)
    } else if query == SysinfoQueryId::GLOBAL_PROCESS_LIST {
        process_list(source, caller, ProcessScope::Global, payload, response)
    } else if query == SysinfoQueryId::KERNEL_MEMORY_STATS {
        write_bytes(&source.kernel_memory_stats(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::HARDWARE_TREE {
        let tree = source.hardware_tree(caller)?;
        write_bytes(&tree, response)
    } else if query == SysinfoQueryId::SYSTEM_IDENTITY {
        write_bytes(&source.system_identity(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::UPTIME {
        write_bytes(&source.uptime(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::LOAD_AVERAGE {
        write_bytes(&source.load_average(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::MOUNT_LIST {
        mount_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::CPU_TIME_STATS {
        cpu_time_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::SEAT_LIST {
        seat_list(source, caller, payload, response)
    } else if query == SysinfoQueryId::RESOURCE_LIMITS {
        resource_limits(source, caller, response)
    } else if query == SysinfoQueryId::USER_DIRECTORY {
        user_directory(source, caller, payload, response)
    } else if query == SysinfoQueryId::PROCESS_IDENTITY {
        // The answer is the caller's own kernel-attested origin, which the
        // dispatcher already holds: it is the attested principal, not state a
        // `SysinfoSource` would supply, so it is encoded here directly rather
        // than echoed through the source seam.
        write_bytes(&caller.origin().to_le_bytes(), response)
    } else {
        Err(Errno::NotImplemented)
    }
}

/// Encode the caller's per-[`LimitKind`] effective-limit + live-usage report
/// into `response`.
///
/// The response is the fixed `LimitKind::COUNT` records packed back-to-back
/// in discriminant order (no paging — the set is small and closed). Fails
/// closed with [`Errno::BufferTooSmall`] if `response` cannot hold them.
fn resource_limits(
    source: &dyn SysinfoSource,
    caller: &Caller,
    response: &mut [u8],
) -> Result<usize, Errno> {
    let records = source.resource_limits(caller)?;
    let needed = ResourceLimitRecord::WIRE_LEN * LimitKind::COUNT;
    if response.len() < needed {
        return Err(Errno::BufferTooSmall);
    }
    let mut written = 0;
    for record in &records {
        response[written..written + ResourceLimitRecord::WIRE_LEN]
            .copy_from_slice(&record.to_le_bytes());
        written += ResourceLimitRecord::WIRE_LEN;
    }
    Ok(written)
}

/// Decode the [`ProcessListRequest`], apply paging, and pack the selected
/// [`ProcessRecord`]s into `response`.
fn process_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    scope: ProcessScope,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = ProcessListRequest::from_bytes(payload)?;
    let records = source.process_records(caller, scope)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        ProcessRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`MountListRequest`], apply paging, and pack the selected
/// [`MountRecord`]s into `response`.
fn mount_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = MountListRequest::from_bytes(payload)?;
    let records = source.mount_records(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        MountRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`CpuTimeListRequest`], apply paging, and pack the selected
/// [`CpuTimeRecord`]s into `response`.
fn cpu_time_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = CpuTimeListRequest::from_bytes(payload)?;
    let records = source.cpu_times(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        CpuTimeRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`SeatListRequest`], apply paging, and pack the selected
/// [`SeatRecord`]s into `response`.
fn seat_list(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = SeatListRequest::from_bytes(payload)?;
    let records = source.seats(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        SeatRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Decode the [`UserDirectoryRequest`], apply paging, and pack the selected
/// [`UserDirectoryRecord`]s into `response`.
fn user_directory(
    source: &dyn SysinfoSource,
    caller: &Caller,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = UserDirectoryRequest::from_bytes(payload)?;
    let records = source.user_directory(caller)?;
    page_records(
        response,
        request.offset as usize,
        request.limit as usize,
        records.len(),
        UserDirectoryRecord::WIRE_LEN,
        |index, slot| slot.copy_from_slice(&records[index].to_le_bytes()),
    )
}

/// Pack a paged window of fixed-`wire_len` records into `response`.
///
/// Shared by every list query so the paging arithmetic — offset bounds, the
/// `limit` window, the buffer-capacity check, and the fail-closed
/// `BufferTooSmall` — lives in exactly one place. `encode`
/// writes record `index` into the supplied `wire_len`-byte slot.
fn page_records(
    response: &mut [u8],
    offset: usize,
    limit: usize,
    count: usize,
    wire_len: usize,
    mut encode: impl FnMut(usize, &mut [u8]),
) -> Result<usize, Errno> {
    if offset >= count {
        return Ok(0);
    }
    let take = core::cmp::min(count - offset, limit);
    let needed = take.checked_mul(wire_len).ok_or(Errno::LengthOutOfRange)?;
    if response.len() < needed {
        return Err(Errno::BufferTooSmall);
    }
    let mut written = 0;
    for index in offset..offset + take {
        encode(index, &mut response[written..written + wire_len]);
        written += wire_len;
    }
    Ok(written)
}

/// Copy `src` into `dst`, failing closed if `dst` is too small.
fn write_bytes(src: &[u8], dst: &mut [u8]) -> Result<usize, Errno> {
    if dst.len() < src.len() {
        return Err(Errno::BufferTooSmall);
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(src.len())
}

/// Submit one audit record to `audit`.
fn emit(audit: &dyn Sink, level: Level, id: EventId, message: &str, fields: &[Field<'_>]) {
    log(
        audit,
        &Event {
            level,
            id,
            message,
            fields,
        },
    );
}

/// Build the `query=<name>` field carried by audit records.
fn query_field(name: &'static str) -> Field<'static> {
    Field {
        key: "query",
        value: rustos_log::FieldValue::Str(name),
    }
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::events;
    use crate::source::{Caller, ProcessScope, SysinfoSource};
    use core::cell::RefCell;
    use rustos_abi::driver::filesystem::{MountFlags, VolumeStats};
    use rustos_abi::sysinfo::{
        CpuTimeListRequest, CpuTimeRecord, KernelMemoryStats, LoadAverage, MountListRequest,
        MountRecord, ProcessListRequest, ProcessRecord, ProcessState, ResourceLimitRecord,
        SeatListRequest, SeatRecord, SysinfoQueryId, SysinfoRequestHeader, SystemIdentity, Uptime,
        UserDirectoryRecord, UserDirectoryRequest, LOAD_FIXED_SHIFT, MACHINE_ID_LEN,
        RESOURCE_LIMITS_REPORT_LEN, SEAT_FLAG_OWNED, SYSINFO_REQUEST_MAGIC,
        SYSINFO_VERSION_CURRENT,
    };
    use rustos_abi::time::{Duration64, Time64};
    use rustos_abi::{
        CapabilityId, CapabilitySummary, Errno, LimitKind, Origin, ProcId, ResourceLimit,
        TrustDomain, ORIGIN_WIRE_LEN,
    };
    use rustos_log::{Event, Level, Sink};

    /// The capabilities a test caller's attested origin should summarise.
    struct Caps(&'static [CapabilityId]);

    /// Records every event it receives so tests can assert on audit output.
    struct RecordingSink {
        events: RefCell<heapless_vec::Vec>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new(heapless_vec::Vec::new()),
            }
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.events.borrow_mut().push((event.level, event.id));
        }
    }

    /// Tiny fixed-capacity vector so the test sink needs no allocator.
    mod heapless_vec {
        use rustos_log::{EventId, Level};

        pub struct Vec {
            buf: [(Level, EventId); 8],
            len: usize,
        }
        impl Default for Vec {
            fn default() -> Self {
                Self {
                    buf: [(Level::Trace, EventId(0)); 8],
                    len: 0,
                }
            }
        }
        impl Vec {
            pub fn new() -> Self {
                Self::default()
            }
            pub fn push(&mut self, item: (Level, EventId)) {
                if self.len < self.buf.len() {
                    self.buf[self.len] = item;
                    self.len += 1;
                }
            }
            pub fn as_slice(&self) -> &[(Level, EventId)] {
                &self.buf[..self.len]
            }
        }
    }

    /// In-memory fixture standing in for the kernel's live state.
    struct FixtureSource {
        own: [ProcessRecord; 2],
        global: [ProcessRecord; 3],
        hwtree: [u8; 5],
        mounts: [MountRecord; 2],
    }
    impl FixtureSource {
        fn new() -> Self {
            let mk = |pid, uid, name: &[u8]| {
                ProcessRecord::new(
                    pid,
                    1,
                    ProcId::KERNEL,
                    ProcId::KERNEL,
                    uid,
                    uid,
                    ProcessState::Running,
                    0,
                    0,
                    0,
                    name,
                )
                .unwrap()
            };
            Self {
                own: [mk(10, 1000, b"shell"), mk(11, 1000, b"editor")],
                global: [
                    mk(1, 0, b"init"),
                    mk(10, 1000, b"shell"),
                    mk(11, 1000, b"editor"),
                ],
                hwtree: [1, 2, 3, 4, 5],
                mounts: [
                    MountRecord::new(
                        b"rootfs",
                        b"/",
                        b"rustfs",
                        MountFlags::READ_ONLY,
                        VolumeStats::default(),
                    )
                    .unwrap(),
                    MountRecord::new(
                        b"data",
                        b"/Storage/data",
                        b"rustfs",
                        MountFlags::NOSUID.union(MountFlags::NODEV),
                        VolumeStats::default(),
                    )
                    .unwrap(),
                ],
            }
        }
    }
    impl SysinfoSource for FixtureSource {
        fn process_records(
            &self,
            _caller: &Caller,
            scope: ProcessScope,
        ) -> Result<alloc::vec::Vec<ProcessRecord>, Errno> {
            Ok(match scope {
                ProcessScope::Caller => self.own.to_vec(),
                ProcessScope::Global => self.global.to_vec(),
            })
        }
        fn kernel_memory_stats(&self, _caller: &Caller) -> Result<KernelMemoryStats, Errno> {
            Ok(KernelMemoryStats {
                total_bytes: 1 << 30,
                free_bytes: 1 << 29,
                kernel_heap_bytes: 4096,
                user_resident_bytes: 1 << 20,
                page_size: 4096,
                reserved: 0,
            })
        }
        fn hardware_tree(&self, _caller: &Caller) -> Result<alloc::vec::Vec<u8>, Errno> {
            Ok(self.hwtree.to_vec())
        }
        fn user_directory(
            &self,
            _caller: &Caller,
        ) -> Result<alloc::vec::Vec<UserDirectoryRecord>, Errno> {
            Ok(alloc::vec![
                UserDirectoryRecord::new(0, b"root").unwrap(),
                UserDirectoryRecord::new(1000, b"alice").unwrap(),
                UserDirectoryRecord::new(1001, b"bob").unwrap(),
            ])
        }
        fn system_identity(&self, _caller: &Caller) -> Result<SystemIdentity, Errno> {
            SystemIdentity::new([9u8; MACHINE_ID_LEN], 1, 0, 0, b"rustos-box")
        }
        fn uptime(&self, _caller: &Caller) -> Result<Uptime, Errno> {
            Ok(Uptime {
                since_boot: Duration64::from_nanos(1_000),
                boot_time: Time64::from_secs(1_700_000_000),
            })
        }
        fn load_average(&self, _caller: &Caller) -> Result<LoadAverage, Errno> {
            Ok(LoadAverage {
                load1: 3 << LOAD_FIXED_SHIFT,
                load5: 2 << LOAD_FIXED_SHIFT,
                load15: 1 << LOAD_FIXED_SHIFT,
                runnable: 3,
                total_tasks: 11,
                users: 2,
            })
        }
        fn mount_records(&self, _caller: &Caller) -> Result<alloc::vec::Vec<MountRecord>, Errno> {
            Ok(self.mounts.to_vec())
        }
        fn cpu_times(&self, _caller: &Caller) -> Result<alloc::vec::Vec<CpuTimeRecord>, Errno> {
            Ok(alloc::vec![
                CpuTimeRecord {
                    cpu: 0,
                    reserved: 0,
                    busy_ns: 750,
                    idle_ns: 250,
                },
                CpuTimeRecord {
                    cpu: 1,
                    reserved: 0,
                    busy_ns: 100,
                    idle_ns: 900,
                },
            ])
        }
        fn seats(&self, _caller: &Caller) -> Result<alloc::vec::Vec<SeatRecord>, Errno> {
            Ok(alloc::vec![SeatRecord {
                seat_id: 0,
                owner_task: 7,
                generation: 3,
                foreground_console: 1,
                flags: SEAT_FLAG_OWNED,
            }])
        }
        fn resource_limits(
            &self,
            _caller: &Caller,
        ) -> Result<[ResourceLimitRecord; LimitKind::COUNT], Errno> {
            // A distinct usage per kind so the positional decode is checkable.
            Ok([
                ResourceLimitRecord::new(
                    LimitKind::AddressSpaceBytes,
                    ResourceLimit::new(1 << 20, 1 << 21).unwrap(),
                    4096,
                ),
                ResourceLimitRecord::new(
                    LimitKind::OpenStreams,
                    ResourceLimit::new(4, 8).unwrap(),
                    3,
                ),
                ResourceLimitRecord::new(LimitKind::Processes, ResourceLimit::UNLIMITED, 2),
                ResourceLimitRecord::new(
                    LimitKind::StackBytes,
                    ResourceLimit::new(64 * 1024, 64 * 1024).unwrap(),
                    0,
                ),
            ])
        }
    }

    fn request_bytes(query: SysinfoQueryId, payload: &[u8]) -> [u8; 64] {
        let header = SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query,
            reserved: 0,
            payload_len: u32::try_from(payload.len()).unwrap(),
            request_id: 7,
        };
        let mut buf = [0u8; 64];
        let head = header.to_le_bytes();
        buf[..head.len()].copy_from_slice(&head);
        buf[head.len()..head.len() + payload.len()].copy_from_slice(payload);
        buf
    }

    fn caller(caps: &Caps) -> Caller {
        let mut summary = CapabilitySummary::EMPTY;
        for cap in caps.0 {
            summary.insert(*cap);
        }
        // A representative attested user-process origin; the dispatcher gates
        // on the capability summary and scopes by uid, both read from here.
        Caller::new(Origin::new(
            TrustDomain::User,
            1000,
            100,
            10,
            ProcId::from_raw([0x10; 16]),
            summary,
            rustos_abi::ORIGIN_CONSOLE_NONE,
        ))
    }

    #[test]
    fn process_identity_returns_the_callers_attested_origin() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let who = caller(&caps);
        let expected = *who.origin();
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::PROCESS_IDENTITY, &[]);
        let mut resp = [0u8; 128];
        let n = serve(&source, &who, &sink, &req, &mut resp).expect("served");
        assert_eq!(n, ORIGIN_WIRE_LEN);
        let decoded = Origin::from_bytes(&resp[..n]).expect("valid origin");
        assert_eq!(decoded, expected);
        assert_eq!(decoded.uid(), 1000);
        assert_eq!(decoded.trust_domain(), TrustDomain::User);
        assert!(decoded
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));
    }

    #[test]
    fn self_process_list_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::SELF_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * ProcessRecord::WIRE_LEN);
        let first = ProcessRecord::from_bytes(&resp[..ProcessRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.name_bytes(), b"shell");
        // Self-scoped queries are not audited.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    #[test]
    fn process_list_paging_offset_and_limit() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_GLOBAL]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest {
            offset: 1,
            limit: 1,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, ProcessRecord::WIRE_LEN);
        let rec = ProcessRecord::from_bytes(&resp[..ProcessRecord::WIRE_LEN]).unwrap();
        assert_eq!(rec.name_bytes(), b"shell");
        // Offset past the end returns an empty page.
        let plr_end = ProcessListRequest {
            offset: 99,
            limit: 10,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr_end.to_le_bytes());
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn global_process_list_denied_without_capability() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let plr = ProcessListRequest::default();
        let req = request_bytes(SysinfoQueryId::GLOBAL_PROCESS_LIST, &plr.to_le_bytes());
        let mut resp = [0u8; 256];
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        let events = sink.events.borrow();
        assert_eq!(events.as_slice(), &[(Level::Warn, events::QUERY_DENIED)]);
    }

    #[test]
    fn audited_query_emits_served_record() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::KERNEL_MEMORY_STATS, &[]);
        let mut resp = [0u8; 64];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, KernelMemoryStats::WIRE_LEN);
        let stats = KernelMemoryStats::from_bytes(&resp).unwrap();
        assert_eq!(stats.page_size, 4096);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Info, events::QUERY_SERVED)]
        );
    }

    #[test]
    fn hardware_tree_passes_through_and_is_gated() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::HARDWARE_TREE, &[]);
        let mut resp = [0u8; 16];

        let denied = Caps(&[]);
        assert_eq!(
            serve(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );

        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let n = serve(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(&resp[..n], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn ungated_scalar_queries_round_trip() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();

        let req = request_bytes(SysinfoQueryId::UPTIME, &[]);
        let mut resp = [0u8; 64];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let up = Uptime::from_bytes(&resp[..n]).unwrap();
        assert_eq!(up.since_boot, Duration64::from_nanos(1_000));
        assert_eq!(up.boot_time, Time64::from_secs(1_700_000_000));

        let req = request_bytes(SysinfoQueryId::SYSTEM_IDENTITY, &[]);
        let mut resp = [0u8; 128];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let id = SystemIdentity::from_bytes(&resp[..n]).unwrap();
        assert_eq!(id.hostname_bytes(), b"rustos-box");
        // Neither query is audited.
        assert!(sink.events.borrow().as_slice().is_empty());
    }

    #[test]
    fn load_average_needs_no_capability_and_round_trips() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::LOAD_AVERAGE, &[]);
        let mut resp = [0u8; 64];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let load = LoadAverage::from_bytes(&resp[..n]).unwrap();
        assert_eq!(LoadAverage::whole(load.load1), 3);
        assert_eq!(load.runnable, 3);
        assert_eq!(load.total_tasks, 11);
        assert_eq!(load.users, 2);
        // System-wide and secret-free, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Fails closed when the response buffer cannot hold the record.
        let mut tiny = [0u8; LoadAverage::WIRE_LEN - 1];
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut tiny),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn resource_limits_needs_no_capability_and_round_trips() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::RESOURCE_LIMITS, &[]);
        let mut resp = [0u8; 256];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, RESOURCE_LIMITS_REPORT_LEN);
        // The records decode positionally, one per LimitKind in order.
        for (index, kind) in LimitKind::ALL.iter().enumerate() {
            let base = index * ResourceLimitRecord::WIRE_LEN;
            let rec =
                ResourceLimitRecord::from_bytes(&resp[base..base + ResourceLimitRecord::WIRE_LEN])
                    .unwrap();
            assert_eq!(rec.kind, *kind);
        }
        let first =
            ResourceLimitRecord::from_bytes(&resp[..ResourceLimitRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.kind, LimitKind::AddressSpaceBytes);
        assert_eq!(first.limit, ResourceLimit::new(1 << 20, 1 << 21).unwrap());
        assert_eq!(first.usage, 4096);
        // Self-scoped, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Fails closed when the response buffer cannot hold the report.
        let mut tiny = [0u8; RESOURCE_LIMITS_REPORT_LEN - 1];
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut tiny),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn user_directory_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let udr = UserDirectoryRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr.to_le_bytes());
        let mut resp = [0u8; 1024];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 3 * UserDirectoryRecord::WIRE_LEN);
        let first =
            UserDirectoryRecord::from_bytes(&resp[..UserDirectoryRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.uid, 0);
        assert_eq!(first.name_bytes(), b"root");
        // Secret-free and self-evidently public, so unaudited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging: a window starting past the first record returns the tail.
        let udr_tail = UserDirectoryRequest {
            offset: 2,
            limit: 10,
            flags: 0,
        };
        let req_tail = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr_tail.to_le_bytes());
        let n = serve(&source, &caller(&caps), &sink, &req_tail, &mut resp).unwrap();
        assert_eq!(n, UserDirectoryRecord::WIRE_LEN);
        let tail = UserDirectoryRecord::from_bytes(&resp[..UserDirectoryRecord::WIRE_LEN]).unwrap();
        assert_eq!(tail.name_bytes(), b"bob");

        // Paging past the end returns an empty page (the terminator).
        let udr_end = UserDirectoryRequest {
            offset: 9,
            limit: 10,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::USER_DIRECTORY, &udr_end.to_le_bytes());
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn mount_list_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let mlr = MountListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::MOUNT_LIST, &mlr.to_le_bytes());
        let mut resp = [0u8; 1024];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * MountRecord::WIRE_LEN);
        let first = MountRecord::from_bytes(&resp[..MountRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.target_bytes(), b"/");
        assert!(first.flags().contains(MountFlags::READ_ONLY));
        // The mount table is not audited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging past the end returns an empty page.
        let mlr_end = MountListRequest {
            offset: 9,
            limit: 4,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::MOUNT_LIST, &mlr_end.to_le_bytes());
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn cpu_time_stats_needs_no_capability_and_pages() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let ctr = CpuTimeListRequest {
            offset: 0,
            limit: 10,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::CPU_TIME_STATS, &ctr.to_le_bytes());
        let mut resp = [0u8; 256];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, 2 * CpuTimeRecord::WIRE_LEN);
        let first = CpuTimeRecord::from_bytes(&resp[..CpuTimeRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.cpu, 0);
        assert_eq!(first.busy_ns, 750);
        assert_eq!(first.idle_ns, 250);
        // The utilisation figures are not audited.
        assert!(sink.events.borrow().as_slice().is_empty());

        // Paging past the end returns an empty page.
        let ctr_end = CpuTimeListRequest {
            offset: 5,
            limit: 4,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::CPU_TIME_STATS, &ctr_end.to_le_bytes());
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn seat_list_is_gated_audited_and_pages() {
        let source = FixtureSource::new();
        let sink = RecordingSink::new();
        let slr = SeatListRequest {
            offset: 0,
            limit: 8,
            flags: 0,
        };
        let req = request_bytes(SysinfoQueryId::SEAT_LIST, &slr.to_le_bytes());
        let mut resp = [0u8; 256];

        // Denied without `CAP_SYSINFO_HW`; the refusal is logged.
        let denied = Caps(&[]);
        assert_eq!(
            serve(&source, &caller(&denied), &sink, &req, &mut resp),
            Err(Errno::PermissionDenied)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_DENIED)]
        );

        // Served (and audited) for a `CAP_SYSINFO_HW` holder.
        let granted = Caps(&[CapabilityId::SYSINFO_HW]);
        let sink = RecordingSink::new();
        let n = serve(&source, &caller(&granted), &sink, &req, &mut resp).unwrap();
        assert_eq!(n, SeatRecord::WIRE_LEN);
        let record = SeatRecord::from_bytes(&resp[..n]).unwrap();
        assert_eq!(record.seat_id, 0);
        assert_eq!(record.owner(), Some(7));
        assert_eq!(record.generation, 3);
        assert_eq!(record.foreground_console, 1);
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Info, events::QUERY_SERVED)]
        );

        // Paging past the single seat returns the empty terminator.
        let slr_end = SeatListRequest {
            offset: 1,
            limit: 8,
            flags: 0,
        };
        let req_end = request_bytes(SysinfoQueryId::SEAT_LIST, &slr_end.to_le_bytes());
        assert_eq!(
            serve(&source, &caller(&granted), &sink, &req_end, &mut resp),
            Ok(0)
        );
    }

    #[test]
    fn malformed_header_is_rejected_and_logged() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let mut resp = [0u8; 64];
        // Too short to hold a header.
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &[0u8; 4], &mut resp),
            Err(Errno::BufferTooSmall)
        );
        // Corrupt magic.
        let mut req = request_bytes(SysinfoQueryId::UPTIME, &[]);
        req[0] ^= 0xFF;
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::BadMagic)
        );
        let events = sink.events.borrow();
        assert_eq!(events.as_slice().len(), 2);
        assert!(events
            .as_slice()
            .iter()
            .all(|&(level, id)| level == Level::Warn && id == events::REQUEST_MALFORMED));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        // Claim an 8-byte payload but supply only the header.
        let header = SysinfoRequestHeader {
            magic: SYSINFO_REQUEST_MAGIC,
            version: SYSINFO_VERSION_CURRENT,
            flags: 0,
            query: SysinfoQueryId::SELF_PROCESS_LIST,
            reserved: 0,
            payload_len: u32::try_from(ProcessListRequest::WIRE_LEN).unwrap(),
            request_id: 1,
        };
        let mut resp = [0u8; 64];
        assert_eq!(
            serve(
                &source,
                &caller(&caps),
                &sink,
                &header.to_le_bytes(),
                &mut resp
            ),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn unassigned_query_is_rejected() {
        let source = FixtureSource::new();
        let caps = Caps(&[]);
        let sink = RecordingSink::new();
        let unassigned = SysinfoQueryId::from_raw(900).unwrap();
        let req = request_bytes(unassigned, &[]);
        let mut resp = [0u8; 64];
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            sink.events.borrow().as_slice(),
            &[(Level::Warn, events::QUERY_UNAVAILABLE)]
        );
    }

    #[test]
    fn response_buffer_too_small_fails_closed() {
        let source = FixtureSource::new();
        let caps = Caps(&[CapabilityId::SYSINFO_KERNEL]);
        let sink = RecordingSink::new();
        let req = request_bytes(SysinfoQueryId::KERNEL_MEMORY_STATS, &[]);
        let mut resp = [0u8; 8]; // smaller than KernelMemoryStats::WIRE_LEN
        assert_eq!(
            serve(&source, &caller(&caps), &sink, &req, &mut resp),
            Err(Errno::BufferTooSmall)
        );
    }
}
