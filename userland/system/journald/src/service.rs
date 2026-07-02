//! The ingress dispatcher: the one place a framed [`LogIngressRequest`] is
//! decoded, attested, admitted, and committed to the journal.

use alloc::vec::Vec;

use rustos_abi::log_ingress::LogIngressRequest;
use rustos_abi::time::{Duration64, WallClockReading};
use rustos_abi::{Errno, FieldName, FieldValue, Origin};
use rustos_log::{CallerContent, Journal, JournalError, Level, SegmentStore, Stream};

/// The journal's own ingest lane: a fixed CPU identity and the monotonically
/// increasing per-CPU record sequence used to detect ingestion gaps.
///
/// The kernel ingress path supplies a record's originating CPU and per-CPU
/// sequence where applicable (SYSLOG §5.2); for records that arrive at the
/// user-space journal over IPC there is no such source, so the journal stamps
/// them with its own ingest lane — one identity per serving task, with a
/// counter that advances for every record it places (a caller record and any
/// trusted note it authors alike), so a lost record leaves a detectable gap.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ingest {
    cpu: u32,
    next_cpu_seq: u64,
}

impl Ingest {
    /// A fresh ingest lane for CPU `cpu`, starting its sequence at zero.
    #[must_use]
    pub const fn new(cpu: u32) -> Self {
        Self {
            cpu,
            next_cpu_seq: 0,
        }
    }

    /// The lane's CPU identity.
    #[must_use]
    pub const fn cpu(&self) -> u32 {
        self.cpu
    }

    /// Consume and return the next per-CPU sequence (monotonic, saturating).
    fn take(&mut self) -> u64 {
        let seq = self.next_cpu_seq;
        self.next_cpu_seq = seq.saturating_add(1);
        seq
    }
}

/// A per-request time reading: the mandatory monotonic ordering time and the
/// optional wall-clock reading with its trust state (SYSLOG §5.1).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Clock {
    /// Monotonic ordering time within the boot. Mandatory.
    pub monotonic: Duration64,
    /// The wall-clock reading and its trust state.
    pub wall: WallClockReading,
}

/// Admit and commit one framed ingress request to `journal`.
///
/// The pipeline is fail-closed and attests every authoritative fact itself:
///
/// 1. Decode and fully validate the [`LogIngressRequest`]; any malformed byte
///    rejects the whole request (`Err`), nothing is partially applied.
/// 2. Resolve the caller's advisory stream and level *discriminants* against
///    the closed [`Stream`]/[`Level`] sets, failing closed on an unknown one.
/// 3. Build the `data.*` set, validating each key against the strict
///    [`FieldName`] grammar (which, requiring no `.`, structurally forbids the
///    reserved `record.`/`origin.`/`source.`/`integrity.`/`sys.` prefixes).
/// 4. [`admit`](Journal::admit) under the kernel-attested `origin` — never a
///    caller claim — deriving the authoritative source, stream, and sequence.
/// 5. [`commit`](Journal::commit) the record with the journal's own ingest
///    `cpu`/`cpu_seq` and the request's `clock`.
/// 6. If the admission detected a spoof (a privileged-stream or reserved-source
///    request), author a trusted `security` record with
///    [`note_spoof`](Journal::note_spoof) preserving the exact claim.
///
/// The caller (the `Run` binary) supplies the kernel-attested `origin` (read
/// from the peer origin, never the request), the journal's `ingest` lane, and
/// the per-request `clock`. `scratch` is the record-encode buffer.
///
/// # Errors
///
/// * The decode [`Errno`] if the request is malformed.
/// * [`Errno::OutOfRange`] if a stream/level discriminant or a `data.*` key is
///   invalid.
/// * [`Errno::NoSpace`] if the record cannot be persisted (a segment cannot be
///   opened, sealed, or stored) — fail-closed, never a silent drop.
pub fn serve<S: SegmentStore>(
    journal: &mut Journal<'_, S>,
    origin: &Origin,
    ingest: &mut Ingest,
    clock: Clock,
    request: &[u8],
    scratch: &mut [u8],
) -> Result<(), Errno> {
    let req = LogIngressRequest::from_bytes(request)?;

    let requested_stream = match req.stream() {
        None => None,
        // `Stream::from_u8` already fails closed with a typed `Errno` on an
        // unknown discriminant.
        Some(raw) => Some(Stream::from_u8(raw)?),
    };
    let level = match req.level() {
        None => None,
        Some(raw) => Some(Level::from_u8(raw).ok_or(Errno::OutOfRange)?),
    };

    // Build the flat `data.*` set. `FieldName::new` enforces the
    // `[a-z][a-z0-9_]{0,63}` grammar, which — permitting no `.` — cannot spell
    // a reserved `record.`/`origin.`/`source.`/`integrity.`/`sys.` prefix, so
    // a spoofed reserved field name is rejected here fail-closed.
    let mut data: Vec<(FieldName<'_>, FieldValue<'_>)> = Vec::new();
    for (key, value) in req.data() {
        data.push((FieldName::new(key)?, value));
    }

    let admission = journal.admit(
        origin,
        req.subsystem(),
        requested_stream,
        req.requested_source(),
        level,
    );

    let caller = CallerContent {
        level,
        component: req.component(),
        tag: req.tag(),
        event_id: req.event_id(),
        requested_source: req.requested_source(),
        requested_stream,
        message: req.message(),
    };

    let cpu = ingest.cpu();
    journal
        .commit(
            &admission,
            cpu,
            ingest.take(),
            clock.monotonic,
            clock.wall,
            caller,
            &data,
            scratch,
        )
        .map_err(|e| map_journal_err(&e))?;

    if admission.spoofed() {
        journal
            .note_spoof(
                &admission,
                requested_stream,
                req.requested_source(),
                cpu,
                ingest.take(),
                clock.monotonic,
                clock.wall,
                scratch,
            )
            .map_err(|e| map_journal_err(&e))?;
    }

    Ok(())
}

/// Collapse a [`JournalError`] into the fail-closed [`Errno`] the ingress reply
/// carries: an encode error surfaces the caller's own fault verbatim, while a
/// segment- or store-level failure is a persistence refusal
/// ([`Errno::NoSpace`]) — never a silent drop (SYSLOG §11).
fn map_journal_err<E>(err: &JournalError<E>) -> Errno {
    match err {
        JournalError::Encode(errno) => *errno,
        JournalError::Segment(_) | JournalError::Store(_) => Errno::NoSpace,
    }
}

#[cfg(test)]
mod tests {
    use super::{serve, Clock, Ingest};
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use rustos_abi::log_ingress::{encode_request, LogIngressFields};
    use rustos_abi::time::{Duration64, Time64, WallClockReading, WallTimeState};
    use rustos_abi::{
        CapabilitySummary, Errno, FieldValue, Origin, ProcId, TrustDomain, BOOT_ID_LEN, PROC_ID_LEN,
    };
    use rustos_log::{
        decode_record, machine_id_hash, DictionaryView, Journal, LogAttestationKey, SegmentReader,
        SegmentStore, Stream, STREAM_COUNT,
    };

    const MID: [u8; 16] = [0x33; 16];

    /// A `SegmentStore` capturing every persisted segment for inspection.
    #[derive(Clone)]
    struct CaptureStore {
        segments: Rc<RefCell<Vec<Vec<u8>>>>,
    }
    impl CaptureStore {
        fn new() -> Self {
            Self {
                segments: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }
    impl SegmentStore for CaptureStore {
        type Error = ();
        fn store_segment(&mut self, bytes: &[u8]) -> Result<(), ()> {
            self.segments.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    fn user_origin(uid: u32) -> Origin {
        Origin::new(
            TrustDomain::User,
            uid,
            uid,
            42,
            ProcId::from_raw([0x5A; PROC_ID_LEN]),
            CapabilitySummary::EMPTY,
        )
    }

    fn journal_origin() -> Origin {
        Origin::new(
            TrustDomain::Kernel,
            0,
            0,
            1,
            ProcId::KERNEL,
            CapabilitySummary::EMPTY,
        )
    }

    fn clock() -> Clock {
        Clock {
            monotonic: Duration64::from_secs(5),
            wall: WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted),
        }
    }

    /// Run `body` against a fresh journal over six equal buffers, returning the
    /// persisted segments.
    fn with_journal<F>(seal: Option<LogAttestationKey>, body: F) -> Vec<Vec<u8>>
    where
        F: FnOnce(&mut Journal<'_, CaptureStore>),
    {
        let store = CaptureStore::new();
        let sink = store.segments.clone();
        let mut b: [Vec<u8>; STREAM_COUNT] = core::array::from_fn(|_| alloc::vec![0u8; 8192]);
        let [b0, b1, b2, b3, b4, b5] = &mut b;
        let bufs: [&mut [u8]; STREAM_COUNT] = [
            b0.as_mut_slice(),
            b1.as_mut_slice(),
            b2.as_mut_slice(),
            b3.as_mut_slice(),
            b4.as_mut_slice(),
            b5.as_mut_slice(),
        ];
        let mut journal = Journal::new(
            store,
            machine_id_hash(&MID),
            rustos_abi::BootId::from_raw([0x5A; BOOT_ID_LEN]),
            seal,
            journal_origin(),
            bufs,
        );
        body(&mut journal);
        let out = sink.borrow().clone();
        out
    }

    /// Decode a segment's records to `(source, message, requested_stream)`.
    fn decoded(segment: &[u8]) -> Vec<(String, String, Option<Stream>)> {
        let reader = SegmentReader::open(segment).expect("open");
        let mut view = DictionaryView::new();
        let mut out = Vec::new();
        for block in reader {
            let rec = decode_record(block.payload, &mut view).expect("decode");
            out.push((
                String::from(rec.source_name()),
                String::from(rec.caller().message),
                rec.caller().requested_stream,
            ));
        }
        out
    }

    fn request(fields: &LogIngressFields<'_>, data: &[(&str, FieldValue<'_>)]) -> Vec<u8> {
        let mut buf = alloc::vec![0u8; rustos_abi::LOG_INGRESS_MAX_REQUEST];
        let n = encode_request(&mut buf, fields, data).expect("encode request");
        buf.truncate(n);
        buf
    }

    #[test]
    fn a_plain_user_record_is_committed_to_runtime() {
        let scratch = &mut [0u8; 1024];
        let segments = with_journal(None, |j| {
            let mut ingest = Ingest::new(0);
            let req = request(
                &LogIngressFields {
                    message: "started",
                    component: Some("shell"),
                    ..LogIngressFields::default()
                },
                &[("pid", FieldValue::UnsignedInt(42))],
            );
            serve(j, &user_origin(1000), &mut ingest, clock(), &req, scratch).expect("served");
            j.flush().expect("flush");
        });
        assert_eq!(segments.len(), 1, "one runtime segment");
        let recs = decoded(&segments[0]);
        assert_eq!(recs.len(), 1);
        assert!(recs[0].0.starts_with("user.1000.proc."));
        assert_eq!(recs[0].1, "started");
    }

    #[test]
    fn a_spoofed_privileged_stream_is_downgraded_and_a_security_note_authored() {
        let key = LogAttestationKey::from_key([0x42; 32]);
        let scratch = &mut [0u8; 1024];
        let segments = with_journal(Some(LogAttestationKey::from_key([0x42; 32])), |j| {
            let mut ingest = Ingest::new(0);
            // A user requests the audit stream — a spoof it is not trusted for.
            let req = request(
                &LogIngressFields {
                    stream: Some(Stream::Audit.as_u8()),
                    message: "audit please",
                    ..LogIngressFields::default()
                },
                &[],
            );
            serve(j, &user_origin(7), &mut ingest, clock(), &req, scratch).expect("served");
            j.flush().expect("flush");
        });
        // The record landed on runtime; a sealed security note records the
        // attempt, preserving the audit-stream claim.
        let mut saw_runtime = false;
        let mut saw_security = false;
        for seg in &segments {
            let s = rustos_log::verify_segment(seg, Some(&key)).expect("verify");
            match s.header.stream {
                Stream::Runtime => {
                    saw_runtime = true;
                    let recs = decoded(seg);
                    assert_eq!(recs[0].2, Some(Stream::Audit), "claim preserved");
                }
                Stream::Security => {
                    saw_security = true;
                    assert!(s.sealed);
                    let recs = decoded(seg);
                    assert_eq!(recs[0].1, "caller spoof attempt rejected");
                }
                other => panic!("unexpected stream {other:?}"),
            }
        }
        assert!(saw_runtime && saw_security);
    }

    #[test]
    fn a_malformed_request_is_rejected() {
        let scratch = &mut [0u8; 1024];
        with_journal(None, |j| {
            let mut ingest = Ingest::new(0);
            assert_eq!(
                serve(j, &user_origin(1), &mut ingest, clock(), &[0u8; 4], scratch),
                Err(Errno::BufferTooSmall)
            );
        });
    }

    #[test]
    fn an_unknown_stream_discriminant_fails_closed() {
        let scratch = &mut [0u8; 1024];
        with_journal(None, |j| {
            let mut ingest = Ingest::new(0);
            // 250 is not a valid Stream discriminant.
            let req = request(
                &LogIngressFields {
                    stream: Some(250),
                    message: "m",
                    ..LogIngressFields::default()
                },
                &[],
            );
            assert_eq!(
                serve(j, &user_origin(1), &mut ingest, clock(), &req, scratch),
                Err(Errno::OutOfRange)
            );
        });
    }

    #[test]
    fn ingest_sequence_advances_monotonically() {
        let mut ingest = Ingest::new(3);
        assert_eq!(ingest.cpu(), 3);
        assert_eq!(ingest.take(), 0);
        assert_eq!(ingest.take(), 1);
        assert_eq!(ingest.take(), 2);
    }
}
