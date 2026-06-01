//! The request dispatcher: the one place a `sysinfo` request is decoded,
//! capability-checked, audited, and answered (`AGENTS.md` §16.6).

use rustos_abi::sysinfo::{
    spec_for, ProcessListRequest, ProcessRecord, SysinfoQueryId, SysinfoRequestHeader,
};
use rustos_abi::Errno;
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
/// The pipeline **fails closed** (`AGENTS.md` §5.4.5): the capability check
/// happens before any data is touched, and every early return leaves
/// `response` untouched. There is no path that answers a privileged query
/// without first passing its capability gate — `sysinfod` is the only
/// server of the API and the kernel exposes no bypass (`AGENTS.md` §16.6).
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
    caller: &Caller<'_>,
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
        if !caller.capabilities.holds(required) {
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
    caller: &Caller<'_>,
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
        write_bytes(source.hardware_tree(caller)?, response)
    } else if query == SysinfoQueryId::SYSTEM_IDENTITY {
        write_bytes(&source.system_identity(caller)?.to_le_bytes(), response)
    } else if query == SysinfoQueryId::UPTIME {
        write_bytes(&source.uptime(caller)?.to_le_bytes(), response)
    } else {
        Err(Errno::NotImplemented)
    }
}

/// Decode the [`ProcessListRequest`], apply paging, and pack the selected
/// [`ProcessRecord`]s into `response`.
fn process_list(
    source: &dyn SysinfoSource,
    caller: &Caller<'_>,
    scope: ProcessScope,
    payload: &[u8],
    response: &mut [u8],
) -> Result<usize, Errno> {
    let request = ProcessListRequest::from_bytes(payload)?;
    let records = source.process_records(caller, scope)?;
    let offset = request.offset as usize;
    if offset >= records.len() {
        return Ok(0);
    }
    let take = core::cmp::min(records.len() - offset, request.limit as usize);
    let needed = take
        .checked_mul(ProcessRecord::WIRE_LEN)
        .ok_or(Errno::LengthOutOfRange)?;
    if response.len() < needed {
        return Err(Errno::BufferTooSmall);
    }
    let mut written = 0;
    for record in &records[offset..offset + take] {
        response[written..written + ProcessRecord::WIRE_LEN].copy_from_slice(&record.to_le_bytes());
        written += ProcessRecord::WIRE_LEN;
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
        value: name,
    }
}

#[cfg(test)]
mod tests {
    use super::serve;
    use crate::events;
    use crate::source::{Caller, ProcessScope, SysinfoSource};
    use core::cell::RefCell;
    use rustos_abi::sysinfo::{
        KernelMemoryStats, ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId,
        SysinfoRequestHeader, SystemIdentity, Uptime, MACHINE_ID_LEN, SYSINFO_REQUEST_MAGIC,
        SYSINFO_VERSION_CURRENT,
    };
    use rustos_abi::{CapabilityId, CapabilityQuery, Errno};
    use rustos_log::{Event, Level, Sink};

    /// A capability view granting exactly the listed capabilities.
    struct Caps(&'static [CapabilityId]);
    impl CapabilityQuery for Caps {
        fn holds(&self, cap: CapabilityId) -> bool {
            self.0.contains(&cap)
        }
    }

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
    }
    impl FixtureSource {
        fn new() -> Self {
            let mk = |pid, uid, name: &[u8]| {
                ProcessRecord::new(pid, 1, uid, uid, ProcessState::Running, 0, name).unwrap()
            };
            Self {
                own: [mk(10, 1000, b"shell"), mk(11, 1000, b"editor")],
                global: [
                    mk(1, 0, b"init"),
                    mk(10, 1000, b"shell"),
                    mk(11, 1000, b"editor"),
                ],
                hwtree: [1, 2, 3, 4, 5],
            }
        }
    }
    impl SysinfoSource for FixtureSource {
        fn process_records(
            &self,
            _caller: &Caller<'_>,
            scope: ProcessScope,
        ) -> Result<&[ProcessRecord], Errno> {
            Ok(match scope {
                ProcessScope::Caller => &self.own,
                ProcessScope::Global => &self.global,
            })
        }
        fn kernel_memory_stats(&self, _caller: &Caller<'_>) -> Result<KernelMemoryStats, Errno> {
            Ok(KernelMemoryStats {
                total_bytes: 1 << 30,
                free_bytes: 1 << 29,
                kernel_heap_bytes: 4096,
                user_resident_bytes: 1 << 20,
                page_size: 4096,
                reserved: 0,
            })
        }
        fn hardware_tree(&self, _caller: &Caller<'_>) -> Result<&[u8], Errno> {
            Ok(&self.hwtree)
        }
        fn system_identity(&self, _caller: &Caller<'_>) -> Result<SystemIdentity, Errno> {
            SystemIdentity::new([9u8; MACHINE_ID_LEN], 1, 0, 0, b"rustos-box")
        }
        fn uptime(&self, _caller: &Caller<'_>) -> Result<Uptime, Errno> {
            Ok(Uptime {
                uptime_ns: 1_000,
                boot_unix_secs: 1_700_000_000,
            })
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

    fn caller(caps: &Caps) -> Caller<'_> {
        Caller {
            uid: 1000,
            capabilities: caps,
        }
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
        assert_eq!(Uptime::from_bytes(&resp[..n]).unwrap().uptime_ns, 1_000);

        let req = request_bytes(SysinfoQueryId::SYSTEM_IDENTITY, &[]);
        let mut resp = [0u8; 128];
        let n = serve(&source, &caller(&caps), &sink, &req, &mut resp).unwrap();
        let id = SystemIdentity::from_bytes(&resp[..n]).unwrap();
        assert_eq!(id.hostname_bytes(), b"rustos-box");
        // Neither query is audited.
        assert!(sink.events.borrow().as_slice().is_empty());
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
