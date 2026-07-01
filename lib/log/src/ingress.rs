//! Record ingress — the journal's admission decision.
//!
//! Ingress is the point where an *untrusted* caller's request becomes an
//! *authoritative* record. The kernel ingress path hands the journal a record
//! that already carries the system-attested facts it alone can vouch for — the
//! emitting principal's [`Origin`], its per-CPU sequence (`cpu_seq`), the
//! monotonic and wall-clock readings — and the caller's own content (its
//! message, level, component/tag/event-id, and its *requests* for a stream and
//! a source). Ingress turns that pair into an [`Admission`]: the authoritative
//! stream, append sequence, source name, and effective level a committed
//! record carries, plus a faithful, non-authoritative copy of the caller's
//! content.
//!
//! The three authority decisions this makes are the ones the caller must never
//! make for itself:
//!
//! * **Which stream** the record is committed to — [`resolve_stream`] honours a
//!   trusted emitter's request and downgrades an untrusted request for a
//!   privileged stream (`boot`/`security`/`audit`/`journal`) to `runtime`,
//!   flagging the attempt as a spoof.
//! * **What the source name is** — [`derive_source`] computes it from the
//!   attested origin (`kernel.<subsystem>`, `user.<uid>.proc.<hex>`, …); a
//!   caller's advisory `requested_source` is only ever a claim, and one that
//!   impersonates a reserved namespace is flagged as a spoof.
//! * **What append sequence** the record gets — a per-stream monotonically
//!   increasing `seq`, assigned here so a caller can neither pick nor skip one.
//!
//! Ingress deliberately does **not** own `cpu_id`/`cpu_seq`/`monotonic`/`wall`
//! /`origin` (those are supplied already-attested by the kernel ingress path),
//! nor does it write segments, detect per-CPU gaps, rate-limit, or emit the
//! trusted security record a spoof warrants — those are the journal service's
//! concern and are built on top of the decision this returns. Everything here
//! is `no_std` and allocation-free: the [`Admission`] holds the derived
//! [`SourceName`] inline, so ingress runs with no allocator.

use rustos_abi::{FieldName, FieldValue, Origin, WallClockReading};

use crate::authority::{derive_source, reserved_source_prefix, resolve_stream, SourceName};
use crate::record::{CallerContent, LogRecord};
use crate::stream::Stream;
use crate::Level;

/// The number of closed [`Stream`] variants — the width of the per-stream
/// append-sequence table. A unit test asserts it stays in step with the
/// [`Stream`] enum, so widening the enum without widening the table is caught.
pub const STREAM_COUNT: usize = 6;

/// The effective level assigned to a record whose caller supplied no level.
///
/// A caller that does not label its severity is treated as routine
/// operational information rather than being guessed at a higher or lower
/// severity.
pub const DEFAULT_EFFECTIVE_LEVEL: Level = Level::Info;

/// Per-stream append sequencer and admission authority.
///
/// One [`Ingress`] owns the next append sequence for each stream. It is the
/// single writer of those counters, so a record's `seq` is monotonic within
/// its stream regardless of what any caller requests. Construct a fresh one
/// with [`Ingress::new`] at genesis, or resume an existing on-disk stream set
/// with [`Ingress::resume`] so the first admitted record continues the
/// stream's append sequence rather than colliding with a committed one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ingress {
    /// Next append sequence to assign, indexed by [`Stream::as_u8`].
    next_seq: [u64; STREAM_COUNT],
}

impl Default for Ingress {
    fn default() -> Self {
        Self::new()
    }
}

impl Ingress {
    /// A fresh ingress with every stream's append sequence starting at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_seq: [0; STREAM_COUNT],
        }
    }

    /// Resume from a known set of next-append-sequence values, one per stream
    /// indexed by [`Stream::as_u8`].
    ///
    /// The journal passes each stream's last committed `seq + 1` so a restart
    /// (or a new segment) continues the append sequence rather than reusing a
    /// sequence a committed record already carries.
    #[must_use]
    pub const fn resume(next_seq: [u64; STREAM_COUNT]) -> Self {
        Self { next_seq }
    }

    /// The next append sequence `stream` will be assigned, without consuming
    /// it. Useful for anchoring and for seeding a new segment header.
    #[must_use]
    pub fn next_seq(&self, stream: Stream) -> u64 {
        self.next_seq[stream.as_u8() as usize]
    }

    /// Consume and return the next append sequence for `stream`.
    ///
    /// This is the *trusted* author's path: the journal itself reserves a
    /// sequence for a record it originates (a loss, seal, rotation, or
    /// verification self-event on the `journal` stream), for which there is no
    /// untrusted caller to run [`Self::admit`] against. Ordinary records take
    /// their sequence through `admit`, which reserves one for the stream it
    /// resolves. The counter is monotonic per stream (a saturating bump; at any
    /// realizable rate it cannot wrap within a machine's life).
    pub fn reserve(&mut self, stream: Stream) -> u64 {
        let index = stream.as_u8() as usize;
        let seq = self.next_seq[index];
        self.next_seq[index] = seq.saturating_add(1);
        seq
    }

    /// Admit one record: resolve its authoritative stream, source, effective
    /// level, and append sequence from the attested `origin` and the caller's
    /// requests.
    ///
    /// * `origin` — the kernel-attested identity of the emitter. Never a
    ///   caller claim.
    /// * `subsystem` — the kernel-context subsystem label used to derive a
    ///   `kernel.<subsystem>` source; ignored for a user origin. See
    ///   [`derive_source`].
    /// * `requested_stream` / `requested_source` / `caller_level` — the
    ///   caller's advisory requests, honoured only within the caller's
    ///   authority.
    ///
    /// The returned [`Admission`] owns the derived source name; call
    /// [`Admission::build_record`] to assemble the [`LogRecord`] body once the
    /// container-owned `cpu_seq` and `wall` reading are known.
    ///
    /// This consumes one append sequence for the resolved stream.
    pub fn admit(
        &mut self,
        origin: &Origin,
        subsystem: Option<&str>,
        requested_stream: Option<Stream>,
        requested_source: Option<&str>,
        caller_level: Option<Level>,
    ) -> Admission {
        let decision = resolve_stream(origin, requested_stream);
        let source = derive_source(origin, subsystem);
        // A requested source that impersonates a reserved system namespace
        // (`kernel.`, `audit.`, `service.`, …) is a spoof: the authoritative
        // source stays the derived value, but the attempt is surfaced so the
        // journal can preserve it as evidence and may raise a security record.
        let source_spoofed =
            requested_source.is_some_and(|name| reserved_source_prefix(name).is_some());

        // The append sequence is monotonic per stream (see [`Self::reserve`]),
        // so admit and the trusted internal author share one counter.
        let seq = self.reserve(decision.effective);

        Admission {
            stream: decision.effective,
            seq,
            effective_level: caller_level.unwrap_or(DEFAULT_EFFECTIVE_LEVEL),
            source,
            origin: *origin,
            stream_spoofed: decision.spoofed,
            source_spoofed,
        }
    }
}

/// The authoritative decision ingress made for one record.
///
/// Holds the system-attested facts a committed record carries — its stream,
/// append sequence, effective level, derived source, and attested origin —
/// plus the two spoof flags the journal acts on. [`Self::build_record`]
/// assembles the [`LogRecord`] body once the caller content and the
/// container-owned `cpu_seq`/`wall` are available.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Admission {
    stream: Stream,
    seq: u64,
    effective_level: Level,
    source: SourceName,
    origin: Origin,
    stream_spoofed: bool,
    source_spoofed: bool,
}

impl Admission {
    /// The stream the record is committed to.
    #[must_use]
    pub fn stream(&self) -> Stream {
        self.stream
    }

    /// The per-stream append sequence assigned to this record.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The authoritative effective level.
    #[must_use]
    pub fn effective_level(&self) -> Level {
        self.effective_level
    }

    /// The system-derived source name.
    #[must_use]
    pub fn source(&self) -> &SourceName {
        &self.source
    }

    /// The attested origin the source was derived from.
    #[must_use]
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// `true` when the caller requested a privileged stream it was not trusted
    /// for; the request was downgraded to [`Stream::Runtime`]. The journal
    /// preserves the request as a `caller.requested_stream` claim and may emit
    /// a trusted security record.
    #[must_use]
    pub fn stream_spoofed(&self) -> bool {
        self.stream_spoofed
    }

    /// `true` when the caller's advisory `requested_source` impersonated a
    /// reserved system namespace. The authoritative source is unaffected; the
    /// attempt is preserved as a claim and may warrant a security record.
    #[must_use]
    pub fn source_spoofed(&self) -> bool {
        self.source_spoofed
    }

    /// `true` when this admission detected any spoof attempt (stream or
    /// source).
    #[must_use]
    pub fn spoofed(&self) -> bool {
        self.stream_spoofed || self.source_spoofed
    }

    /// Assemble the [`LogRecord`] body for this admission.
    ///
    /// `cpu_seq` and `wall` are the container-owned, kernel-attested values
    /// supplied by the ingress path. `caller` and `data` are the caller's own
    /// content, stored faithfully but never as authority — in particular the
    /// caller's `requested_stream`/`requested_source` are carried through as
    /// claims even when [`Self::spoofed`] is set, so a spoof is preserved as
    /// evidence under the authoritative source this admission derived.
    ///
    /// The returned record borrows this admission's derived source name, so
    /// the admission must outlive the record.
    #[must_use]
    pub fn build_record<'a>(
        &'a self,
        cpu_seq: u64,
        wall: WallClockReading,
        caller: CallerContent<'a>,
        data: &'a [(FieldName<'a>, FieldValue<'a>)],
    ) -> LogRecord<'a> {
        LogRecord {
            effective_level: self.effective_level,
            cpu_seq,
            wall,
            origin: self.origin,
            source_name: self.source.as_str(),
            caller,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Admission, Ingress, DEFAULT_EFFECTIVE_LEVEL, STREAM_COUNT};
    use crate::record::{decode as decode_record, CallerContent};
    use crate::stream::Stream;
    use crate::{DictionaryBuilder, DictionaryView, Level};
    use rustos_abi::{
        CapabilitySummary, Origin, ProcId, Time64, TrustDomain, WallClockReading, WallTimeState,
        PROC_ID_LEN,
    };

    fn kernel_origin() -> Origin {
        Origin::new(
            TrustDomain::Kernel,
            0,
            0,
            1,
            ProcId::KERNEL,
            CapabilitySummary::EMPTY,
        )
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

    fn wall() -> WallClockReading {
        WallClockReading::new(Time64::from_secs(1_700_000_000), WallTimeState::Trusted)
    }

    #[test]
    fn stream_count_matches() {
        // The append-sequence table must have one slot per closed stream;
        // adding a stream without widening the table would panic-index below.
        let all = [
            Stream::Boot,
            Stream::Runtime,
            Stream::Debug,
            Stream::Security,
            Stream::Audit,
            Stream::Journal,
        ];
        assert_eq!(all.len(), STREAM_COUNT);
        let mut ingress = Ingress::new();
        for s in all {
            // Every stream is a valid index into the table.
            let _ = ingress.admit(&kernel_origin(), None, Some(s), None, None);
        }
    }

    #[test]
    fn kernel_record_keeps_requested_stream_and_derives_source() {
        let mut ingress = Ingress::new();
        let adm = ingress.admit(
            &kernel_origin(),
            Some("mem"),
            Some(Stream::Security),
            None,
            Some(Level::Error),
        );
        assert_eq!(adm.stream(), Stream::Security);
        assert_eq!(adm.source().as_str(), "kernel.mem");
        assert_eq!(adm.effective_level(), Level::Error);
        assert!(!adm.spoofed());
    }

    #[test]
    fn user_privileged_stream_request_is_downgraded_and_flagged() {
        let mut ingress = Ingress::new();
        let adm = ingress.admit(
            &user_origin(1000),
            None,
            Some(Stream::Audit),
            None,
            Some(Level::Critical),
        );
        assert_eq!(adm.stream(), Stream::Runtime);
        assert!(adm.stream_spoofed());
        // Even a user-labelled `critical` is honoured as the effective level;
        // the source and stream — not the level — carry the authority.
        assert_eq!(adm.effective_level(), Level::Critical);
        assert!(adm.source().as_str().starts_with("user.1000.proc."));
    }

    #[test]
    fn requested_source_impersonating_reserved_namespace_is_flagged() {
        let mut ingress = Ingress::new();
        let adm = ingress.admit(&user_origin(7), None, None, Some("kernel.audit"), None);
        assert!(adm.source_spoofed());
        // The authoritative source stays the derived user source.
        assert!(adm.source().as_str().starts_with("user.7.proc."));
        assert_eq!(adm.effective_level(), DEFAULT_EFFECTIVE_LEVEL);
    }

    #[test]
    fn non_reserved_requested_source_is_not_flagged() {
        let mut ingress = Ingress::new();
        let adm = ingress.admit(&user_origin(7), None, None, Some("dhcp"), None);
        assert!(!adm.source_spoofed());
    }

    #[test]
    fn append_sequence_is_monotonic_per_stream_and_independent() {
        let mut ingress = Ingress::new();
        // Two runtime records and one debug record: runtime seqs are 0,1 and
        // the debug seq is its own 0 — streams do not share a counter.
        let r0 = ingress.admit(&user_origin(1), None, Some(Stream::Runtime), None, None);
        let d0 = ingress.admit(&user_origin(1), None, Some(Stream::Debug), None, None);
        let r1 = ingress.admit(&user_origin(1), None, Some(Stream::Runtime), None, None);
        assert_eq!((r0.seq(), r1.seq()), (0, 1));
        assert_eq!(d0.seq(), 0);
        assert_eq!(ingress.next_seq(Stream::Runtime), 2);
        assert_eq!(ingress.next_seq(Stream::Debug), 1);
    }

    #[test]
    fn resume_continues_the_append_sequence() {
        let mut seeds = [0u64; STREAM_COUNT];
        seeds[Stream::Runtime.as_u8() as usize] = 100;
        let mut ingress = Ingress::resume(seeds);
        let adm = ingress.admit(&user_origin(1), None, Some(Stream::Runtime), None, None);
        assert_eq!(adm.seq(), 100);
        assert_eq!(ingress.next_seq(Stream::Runtime), 101);
    }

    #[test]
    fn built_record_carries_attested_fields_and_preserves_claims() {
        let mut ingress = Ingress::new();
        let adm = ingress.admit(
            &user_origin(1000),
            None,
            Some(Stream::Security), // spoof: downgraded to runtime
            Some("kernel.mem"),     // spoof: reserved namespace
            Some(Level::Warn),
        );
        let caller = CallerContent {
            level: Some(Level::Warn),
            component: Some("dhcp"),
            tag: None,
            event_id: None,
            // The spoof requests are preserved verbatim as caller claims.
            requested_source: Some("kernel.mem"),
            requested_stream: Some(Stream::Security),
            message: "lease lost",
        };
        let record = adm.build_record(9, wall(), caller, &[]);
        assert_eq!(record.effective_level, Level::Warn);
        assert_eq!(record.cpu_seq, 9);
        assert!(record.source_name.starts_with("user.1000.proc."));

        // The produced record encodes and decodes to the same attested facts,
        // proving the admission builds an on-disk-valid body.
        let mut buf = [0u8; 512];
        let n = record
            .encode(&mut buf, &mut DictionaryBuilder::new())
            .expect("record encodes");
        let mut view = DictionaryView::new();
        let decoded = decode_record(&buf[..n], &mut view).expect("record decodes");
        assert_eq!(decoded.effective_level(), Level::Warn);
        assert_eq!(decoded.cpu_seq(), 9);
        assert_eq!(decoded.source_name(), record.source_name);
        assert_eq!(decoded.caller().requested_source, Some("kernel.mem"));
        assert_eq!(decoded.caller().requested_stream, Some(Stream::Security));
    }

    #[test]
    fn admission_is_copy_and_outlives_record_build() {
        // `Admission` is `Copy`, so the journal can keep the decision (its
        // spoof flags, seq, stream) after building the borrowed record.
        let mut ingress = Ingress::new();
        let adm: Admission = ingress.admit(
            &kernel_origin(),
            Some("net"),
            Some(Stream::Boot),
            None,
            None,
        );
        let caller = CallerContent {
            level: None,
            component: None,
            tag: None,
            event_id: None,
            requested_source: None,
            requested_stream: Some(Stream::Boot),
            message: "link up",
        };
        let _record = adm.build_record(0, wall(), caller, &[]);
        assert_eq!(adm.stream(), Stream::Boot);
        assert_eq!(adm.seq(), 0);
    }
}
