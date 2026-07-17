//! The request dispatcher: the one place a seat-administration request is
//! decoded, capability-checked, audited, and forwarded to the kernel.

use tairix_abi::seat::SeatAdminRequest;
use tairix_abi::{CapabilityId, Errno, Origin};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};

use crate::events;

/// The kernel seam the authorised operations are forwarded through.
///
/// On a running kernel this is a thin shim over the `seat_switch` /
/// `seat_revoke` syscalls (which re-check `CAP_SEAT_ADMIN` and validate
/// the seat and console indices, so a compromised dispatcher still cannot
/// exceed the kernel's own gate); in tests it is an in-memory fixture.
pub trait SeatAdmin {
    /// Retarget `seat_id`'s foreground to the installed text console
    /// `console` (`seat_switch`).
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — [`Errno::NotFound`] for an unknown
    /// seat or console, [`Errno::PermissionDenied`] for a missing
    /// capability — passed through verbatim.
    fn switch(&self, seat_id: u64, console: u32) -> Result<(), Errno>;

    /// Forcibly revoke `seat_id`'s current lease (`seat_revoke`).
    ///
    /// # Errors
    ///
    /// The kernel's typed refusal — [`Errno::NotFound`] for an unknown
    /// seat, [`Errno::SeatNotOwner`] for an unowned seat — passed through
    /// verbatim.
    fn revoke(&self, seat_id: u64) -> Result<(), Errno>;
}

/// Serve one seat-administration request.
///
/// Decodes the [`SeatAdminRequest`] from `request`, requires the
/// requester's kernel-attested [`Origin`] to carry `CAP_SEAT_ADMIN`
/// **before** any state is touched — the seat-multiplexing authority is
/// never ambient, and this service's own capability must not launder a
/// requester's missing one — then forwards the operation through `admin`
/// and audits the outcome.
///
/// # Errors
///
/// * The decode refusals of [`SeatAdminRequest::from_bytes`] (fail closed
///   on any malformed request).
/// * [`Errno::PermissionDenied`] — the requester lacks `CAP_SEAT_ADMIN`.
/// * Any error returned by the backing [`SeatAdmin`].
pub fn serve(
    admin: &dyn SeatAdmin,
    requester: &Origin,
    audit: &dyn Sink,
    request: &[u8],
) -> Result<(), Errno> {
    let decoded = match SeatAdminRequest::from_bytes(request) {
        Ok(decoded) => decoded,
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::REQUEST_MALFORMED,
                "seat-admin request rejected: decode failed",
                &[],
            );
            return Err(err);
        }
    };

    // The requester must hold the authority itself: the endpoint is the
    // policy surface, the syscall the mechanism, and both gate on the one
    // capability so this broker can never widen a caller's reach.
    if !requester.capabilities().holds_cap(CapabilityId::SEAT_ADMIN) {
        emit(
            audit,
            Level::Warn,
            events::SEAT_ADMIN_DENIED,
            "seat-admin request denied: requester lacks CAP_SEAT_ADMIN",
            &[op_field(&decoded), uid_field(requester)],
        );
        return Err(Errno::PermissionDenied);
    }

    let outcome = match decoded {
        SeatAdminRequest::Switch { seat_id, console } => admin.switch(seat_id, console),
        SeatAdminRequest::Revoke { seat_id } => admin.revoke(seat_id),
    };
    match outcome {
        Ok(()) => {
            emit(
                audit,
                Level::Info,
                events::SEAT_ADMIN_APPLIED,
                "seat-admin request applied",
                &[op_field(&decoded), uid_field(requester)],
            );
            Ok(())
        }
        Err(err) => {
            emit(
                audit,
                Level::Warn,
                events::SEAT_ADMIN_DENIED,
                "seat-admin request refused by the kernel",
                &[op_field(&decoded), uid_field(requester)],
            );
            Err(err)
        }
    }
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

/// Build the `op=<name>` field carried by audit records.
fn op_field(request: &SeatAdminRequest) -> Field<'static> {
    let name = match request {
        SeatAdminRequest::Switch { .. } => "switch",
        SeatAdminRequest::Revoke { .. } => "revoke",
    };
    Field {
        key: "op",
        value: FieldValue::Str(name),
    }
}

/// Build the `requester_uid=<uid>` field carried by audit records.
fn uid_field(requester: &Origin) -> Field<'static> {
    Field {
        key: "requester_uid",
        value: FieldValue::UnsignedInt(u64::from(requester.uid())),
    }
}

#[cfg(test)]
mod tests {
    use super::{serve, SeatAdmin};
    use crate::events;
    use core::cell::RefCell;
    use tairix_abi::seat::SeatAdminRequest;
    use tairix_abi::{
        CapabilityId, CapabilitySummary, Errno, Origin, ProcId, TrustDomain, ORIGIN_CONSOLE_NONE,
    };
    use tairix_log::{Event, EventId, Level, Sink};

    /// Records every event it receives so tests can assert on audit output.
    struct RecordingSink {
        events: RefCell<[Option<(Level, EventId)>; 8]>,
        len: RefCell<usize>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                events: RefCell::new([None; 8]),
                len: RefCell::new(0),
            }
        }
        fn ids(&self) -> [Option<(Level, EventId)>; 8] {
            *self.events.borrow()
        }
    }
    impl Sink for RecordingSink {
        fn write_event(&self, event: &Event<'_>) {
            let mut events = self.events.borrow_mut();
            let mut len = self.len.borrow_mut();
            if *len < events.len() {
                events[*len] = Some((event.level, event.id));
                *len += 1;
            }
        }
    }

    /// One recorded forward: `(seat_id, Some(console))` for a switch,
    /// `(seat_id, None)` for a revoke.
    type ForwardedCall = (u64, Option<u32>);

    /// In-memory kernel seam recording the forwarded operations.
    struct FixtureAdmin {
        refuse: Option<Errno>,
        calls: RefCell<[Option<ForwardedCall>; 8]>,
        len: RefCell<usize>,
    }
    impl FixtureAdmin {
        fn new(refuse: Option<Errno>) -> Self {
            Self {
                refuse,
                calls: RefCell::new([None; 8]),
                len: RefCell::new(0),
            }
        }
        fn record(&self, seat_id: u64, console: Option<u32>) {
            let mut calls = self.calls.borrow_mut();
            let mut len = self.len.borrow_mut();
            if *len < calls.len() {
                calls[*len] = Some((seat_id, console));
                *len += 1;
            }
        }
        fn calls(&self) -> [Option<ForwardedCall>; 8] {
            *self.calls.borrow()
        }
    }
    impl SeatAdmin for FixtureAdmin {
        fn switch(&self, seat_id: u64, console: u32) -> Result<(), Errno> {
            self.record(seat_id, Some(console));
            self.refuse.map_or(Ok(()), Err)
        }
        fn revoke(&self, seat_id: u64) -> Result<(), Errno> {
            self.record(seat_id, None);
            self.refuse.map_or(Ok(()), Err)
        }
    }

    fn requester(caps: &[CapabilityId]) -> Origin {
        let mut summary = CapabilitySummary::EMPTY;
        for cap in caps {
            summary.insert(*cap);
        }
        Origin::new(
            TrustDomain::User,
            0,
            0,
            42,
            ProcId::from_raw([0x21; 16]),
            summary,
            ORIGIN_CONSOLE_NONE,
        )
    }

    #[test]
    fn an_authorised_switch_is_forwarded_and_audited() {
        let admin = FixtureAdmin::new(None);
        let sink = RecordingSink::new();
        let who = requester(&[CapabilityId::SEAT_ADMIN]);
        let request = SeatAdminRequest::Switch {
            seat_id: 0,
            console: 1,
        }
        .to_le_bytes();
        assert_eq!(serve(&admin, &who, &sink, &request), Ok(()));
        assert_eq!(admin.calls()[0], Some((0, Some(1))));
        assert_eq!(
            sink.ids()[0],
            Some((Level::Info, events::SEAT_ADMIN_APPLIED))
        );
    }

    #[test]
    fn an_authorised_revoke_is_forwarded_and_audited() {
        let admin = FixtureAdmin::new(None);
        let sink = RecordingSink::new();
        let who = requester(&[CapabilityId::SEAT_ADMIN]);
        let request = SeatAdminRequest::Revoke { seat_id: 0 }.to_le_bytes();
        assert_eq!(serve(&admin, &who, &sink, &request), Ok(()));
        assert_eq!(admin.calls()[0], Some((0, None)));
        assert_eq!(
            sink.ids()[0],
            Some((Level::Info, events::SEAT_ADMIN_APPLIED))
        );
    }

    #[test]
    fn an_unprivileged_requester_is_denied_before_any_state() {
        let admin = FixtureAdmin::new(None);
        let sink = RecordingSink::new();
        let who = requester(&[]);
        let request = SeatAdminRequest::Revoke { seat_id: 0 }.to_le_bytes();
        assert_eq!(
            serve(&admin, &who, &sink, &request),
            Err(Errno::PermissionDenied)
        );
        // The kernel seam was never reached (capability check before state).
        assert_eq!(admin.calls()[0], None);
        assert_eq!(
            sink.ids()[0],
            Some((Level::Warn, events::SEAT_ADMIN_DENIED))
        );
    }

    #[test]
    fn a_kernel_refusal_passes_through_and_is_audited() {
        let admin = FixtureAdmin::new(Some(Errno::SeatNotOwner));
        let sink = RecordingSink::new();
        let who = requester(&[CapabilityId::SEAT_ADMIN]);
        let request = SeatAdminRequest::Revoke { seat_id: 0 }.to_le_bytes();
        assert_eq!(
            serve(&admin, &who, &sink, &request),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(
            sink.ids()[0],
            Some((Level::Warn, events::SEAT_ADMIN_DENIED))
        );
    }

    #[test]
    fn a_malformed_request_is_rejected_and_logged() {
        let admin = FixtureAdmin::new(None);
        let sink = RecordingSink::new();
        let who = requester(&[CapabilityId::SEAT_ADMIN]);
        assert_eq!(
            serve(&admin, &who, &sink, &[0u8; 4]),
            Err(Errno::BufferTooSmall)
        );
        let mut corrupt = SeatAdminRequest::Revoke { seat_id: 0 }.to_le_bytes();
        corrupt[0] ^= 0xFF;
        assert_eq!(serve(&admin, &who, &sink, &corrupt), Err(Errno::BadMagic));
        assert_eq!(admin.calls()[0], None);
        assert_eq!(
            sink.ids()[0],
            Some((Level::Warn, events::REQUEST_MALFORMED))
        );
        assert_eq!(
            sink.ids()[1],
            Some((Level::Warn, events::REQUEST_MALFORMED))
        );
    }
}
