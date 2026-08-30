//! Host tests for the service's orchestration.
//!
//! Every case drives the whole parent path — the clock policy, the engine, the
//! sandboxed evaluation over the in-process loopback worker, the persisted
//! record, and the audit records — with no processes and no sockets.

use super::{
    events, Clock, ConfigRetry, RecordStore, Step, Timed, TimedConfig, Transport,
    CONFIG_RETRY_ATTEMPTS, CONFIG_RETRY_BASE_NANOS,
};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use tairix_abi::time::{Duration64, Time64};
use tairix_abi::{Errno, WallClockReading, WallTimeState, RELEASE_EPOCH_SECS};
use tairix_log::{Event, EventId, Sink};
use tairix_net::ntp::{NtpTimestamp, PACKET_LEN};
use tairix_sandbox::host::ParserSandbox;
use tairix_sandbox::loopback::LoopbackLauncher;
use tairix_sandbox::timesync::TimeSyncService;
use tairix_sysconfig::{RefreshCadence, SystemConfig};
use tairix_timesync::{SyncRecord, STALE_BOOT_GAP};

/// Captures every logged event id, so a case can assert what was audited.
#[derive(Clone, Default)]
struct RecordingSink {
    events: Rc<RefCell<Vec<EventId>>>,
}

impl Sink for RecordingSink {
    fn write_event(&self, event: &Event<'_>) {
        self.events.borrow_mut().push(event.id);
    }
}

impl RecordingSink {
    fn saw(&self, id: EventId) -> bool {
        self.events.borrow().contains(&id)
    }
}

/// A settable in-memory clock: the monotonic reading the test advances, and
/// the wall reading the service reads and writes.
#[derive(Clone)]
struct FakeClock {
    monotonic: Rc<RefCell<Duration64>>,
    wall: Rc<RefCell<Result<WallClockReading, Errno>>>,
    set_result: Result<(), Errno>,
    sets: Rc<RefCell<Vec<(Time64, WallTimeState)>>>,
}

impl FakeClock {
    fn unset() -> Self {
        Self {
            monotonic: Rc::new(RefCell::new(Duration64::ZERO)),
            wall: Rc::new(RefCell::new(Ok(WallClockReading::new(
                Time64::from_secs(0),
                WallTimeState::Unset,
            )))),
            set_result: Ok(()),
            sets: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn reading(time: Time64, state: WallTimeState) -> Self {
        let clock = Self::unset();
        *clock.wall.borrow_mut() = Ok(WallClockReading::new(time, state));
        clock
    }
}

impl Clock for FakeClock {
    fn monotonic(&self) -> Duration64 {
        *self.monotonic.borrow()
    }

    fn wall(&self) -> Result<WallClockReading, Errno> {
        *self.wall.borrow()
    }

    fn set_wall(&self, time: Time64, state: WallTimeState) -> Result<(), Errno> {
        self.set_result?;
        self.sets.borrow_mut().push((time, state));
        *self.wall.borrow_mut() = Ok(WallClockReading::new(time, state));
        Ok(())
    }
}

/// An in-memory record document.
#[derive(Clone, Default)]
struct FakeStore {
    document: Rc<RefCell<Option<Vec<u8>>>>,
    write_result: Option<Errno>,
}

impl RecordStore for FakeStore {
    fn read(&self) -> Result<Option<Vec<u8>>, Errno> {
        Ok(self.document.borrow().clone())
    }

    fn write(&self, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(err) = self.write_result {
            return Err(err);
        }
        *self.document.borrow_mut() = Some(bytes.to_vec());
        Ok(())
    }
}

/// One `(server index, packet)` pair a fake transport was handed.
type SentPacket = (u8, Vec<u8>);

/// A transport that records every packet it was handed, and can refuse.
#[derive(Clone, Default)]
struct FakeTransport {
    sent: Rc<RefCell<Vec<SentPacket>>>,
    refuse: Option<Errno>,
}

impl Transport for FakeTransport {
    fn send(&mut self, index: u8, packet: &[u8]) -> Result<(), Errno> {
        if let Some(err) = self.refuse {
            return Err(err);
        }
        self.sent.borrow_mut().push((index, packet.to_vec()));
        Ok(())
    }
}

type TestService = Timed<
    FakeClock,
    FakeStore,
    FakeTransport,
    LoopbackLauncher<fn() -> TimeSyncService>,
    RecordingSink,
>;

/// A wall instant comfortably inside the plausibility window.
fn plausible() -> Time64 {
    Time64::from_secs(RELEASE_EPOCH_SECS + 86_400)
}

/// The configuration a test machine boots with: two servers, daily refresh.
fn configured() -> SystemConfig {
    SystemConfig {
        time_servers: vec![String::from("a.test"), String::from("b.test")],
        time_refresh: RefreshCadence::Daily,
        ..SystemConfig::default()
    }
}

fn service(
    clock: FakeClock,
    store: FakeStore,
    transport: FakeTransport,
    config: SystemConfig,
    sink: RecordingSink,
) -> TestService {
    Timed::new(TimedConfig {
        clock,
        store,
        transport,
        sandbox: ParserSandbox::new(
            LoopbackLauncher::new(TimeSyncService::default as fn() -> TimeSyncService),
            sink.clone(),
        ),
        sink,
        config,
        entropy: 0x0F0F_0F0F_0F0F_0F0F,
    })
}

/// Seconds from the NTP epoch (1900) to the Unix epoch (1970).
const NTP_UNIX_DELTA: i64 = 2_208_988_800;

/// A well-formed stratum-2 server reply echoing `nonce` and reporting `at`.
fn reply(nonce: u64, at: Time64) -> [u8; PACKET_LEN] {
    let field = u32::try_from((at.secs() + NTP_UNIX_DELTA).rem_euclid(1 << 32))
        .expect("reduced modulo 2^32");
    let ts = NtpTimestamp::from_raw(u64::from(field) << 32);
    let mut p = [0u8; PACKET_LEN];
    p[0] = (4 << 3) | 4; // version 4, mode 4 (server)
    p[1] = 2; // stratum
    p[24..32].copy_from_slice(&nonce.to_be_bytes());
    p[32..40].copy_from_slice(&ts.raw().to_be_bytes());
    p[40..48].copy_from_slice(&ts.raw().to_be_bytes());
    p
}

/// Drive the service to an in-flight request, returning the nonce it carries
/// and a receive instant 20 ms later.
fn in_flight(svc: &mut TestService) -> (u64, Duration64) {
    const NONCE: u64 = 0xDEAD_BEEF_FEED_FACE;
    let due = svc.next_deadline().expect("a query is scheduled");
    assert_eq!(svc.poll(due, NONCE), Step::Queried(0));
    (
        NONCE,
        Duration64::from_nanos(due.saturating_total_nanos() + 20_000_000),
    )
}

#[test]
fn an_unset_clock_is_set_from_a_validated_sample_and_recorded() {
    let clock = FakeClock::unset();
    let store = FakeStore::default();
    let sink = RecordingSink::default();
    let mut svc = service(
        clock.clone(),
        store.clone(),
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    assert!(sink.saw(events::SERVICE_READY));

    let (nonce, at) = in_flight(&mut svc);
    let Step::ClockSet(update) = svc.on_datagram(at, &reply(nonce, plausible())) else {
        panic!("a well-formed reply must set the clock");
    };

    // A network sample is always `Trusted`: the reading comes wholly from the
    // network time source.
    assert_eq!(update.state, WallTimeState::Trusted);
    // The instant applied is exactly the one the update named — the server's
    // transmit instant advanced by half the measured round trip, never the
    // raw packet field.
    assert_eq!(
        clock.sets.borrow().as_slice(),
        &[(update.wall, WallTimeState::Trusted)]
    );
    assert_eq!(update.wall.secs(), plausible().secs());
    assert_eq!(update.wall.subsec_nanos(), 10_000_000);
    assert!(sink.saw(events::CLOCK_SET));

    // The record now says when time was last seen, so a future boot can tell
    // a short power-off from a long one.
    let document = store
        .document
        .borrow()
        .clone()
        .expect("a record was written");
    assert_eq!(
        SyncRecord::from_bytes(&document),
        SyncRecord::EMPTY.synced_at(update.wall)
    );
    assert_eq!(svc.record(), SyncRecord::EMPTY.synced_at(update.wall));
}

#[test]
fn a_wrong_nonce_reply_is_refused_and_leaves_the_transaction_outstanding() {
    // The anti-spoof gate: an injected flood must neither set the clock nor
    // cancel the real answer, or it becomes a denial of service.
    let clock = FakeClock::unset();
    let sink = RecordingSink::default();
    let mut svc = service(
        clock.clone(),
        FakeStore::default(),
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    let (nonce, at) = in_flight(&mut svc);
    let outstanding = svc.next_deadline();

    for spoof in 1..8u64 {
        assert_eq!(
            svc.on_datagram(at, &reply(nonce ^ spoof, plausible())),
            Step::Idle
        );
    }
    assert!(clock.sets.borrow().is_empty(), "the clock must not be set");
    assert_eq!(
        svc.next_deadline(),
        outstanding,
        "the real answer is still awaited"
    );
    assert!(!sink.saw(events::CLOCK_SET));

    // ...and the genuine reply still lands.
    assert!(matches!(
        svc.on_datagram(at, &reply(nonce, plausible())),
        Step::ClockSet(_)
    ));
    assert_eq!(clock.sets.borrow().len(), 1);
}

#[test]
fn an_implausible_server_instant_never_reaches_the_clock() {
    // The window is a validation bound: a server claiming a time before this
    // release existed, or a century hence, is refused whole and audited.
    for claimed in [
        Time64::from_secs(RELEASE_EPOCH_SECS - 1),
        Time64::from_secs(0),
        Time64::from_secs(RELEASE_EPOCH_SECS + tairix_abi::PLAUSIBLE_FUTURE_SECS + 1),
    ] {
        let clock = FakeClock::unset();
        let sink = RecordingSink::default();
        let mut svc = service(
            clock.clone(),
            FakeStore::default(),
            FakeTransport::default(),
            configured(),
            sink.clone(),
        );
        let (nonce, at) = in_flight(&mut svc);
        assert_eq!(svc.on_datagram(at, &reply(nonce, claimed)), Step::NoSample);
        assert!(
            clock.sets.borrow().is_empty(),
            "{claimed:?} must not reach the clock"
        );
        assert!(sink.saw(events::SAMPLE_REFUSED));
    }
}

#[test]
fn a_believed_clock_waits_the_refresh_cadence_instead_of_querying_at_boot() {
    // A machine with a working RTC must not query a public server merely
    // because it rebooted.
    let clock = FakeClock::reading(plausible(), WallTimeState::Firmware);
    let store = FakeStore::default();
    *store.document.borrow_mut() =
        Some(SyncRecord::EMPTY.synced_at(plausible()).to_bytes().to_vec());
    let sink = RecordingSink::default();
    let transport = FakeTransport::default();
    let mut svc = service(clock, store, transport.clone(), configured(), sink);

    // Nothing is due for the best part of a day, and polling early sends
    // nothing.
    let due = svc.next_deadline().expect("a refresh is scheduled");
    assert!(
        due.secs() > 3_600,
        "the refresh must not be imminent: {due:?}"
    );
    assert_eq!(svc.poll(Duration64::from_secs(60), 1), Step::Idle);
    assert!(transport.sent.borrow().is_empty());
}

#[test]
fn a_record_from_a_long_power_off_makes_the_boot_query_urgent() {
    // The one input that distinguishes "off for an hour" from "off for a
    // month", which no clock reading alone can tell.
    let seen = plausible();
    let now = seen
        .saturating_add(STALE_BOOT_GAP)
        .saturating_add(Duration64::from_secs(3_600));
    let store = FakeStore::default();
    *store.document.borrow_mut() = Some(SyncRecord::EMPTY.synced_at(seen).to_bytes().to_vec());
    let sink = RecordingSink::default();
    let svc = service(
        FakeClock::reading(now, WallTimeState::Firmware),
        store,
        FakeTransport::default(),
        configured(),
        sink,
    );
    let due = svc.next_deadline().expect("a query is scheduled");
    assert!(
        due <= tairix_timesync::INITIAL_DELAY_SPAN,
        "a stale boot must query at once, not on the refresh cadence: {due:?}"
    );
}

#[test]
fn a_corrupt_record_makes_no_rule_fire_rather_than_firing_on_a_fiction() {
    let store = FakeStore::default();
    *store.document.borrow_mut() = Some(vec![0xFF; 33]);
    let sink = RecordingSink::default();
    let svc = service(
        FakeClock::reading(plausible(), WallTimeState::Firmware),
        store,
        FakeTransport::default(),
        configured(),
        sink,
    );
    assert_eq!(svc.record(), SyncRecord::EMPTY);
    // The clock is believed, so only the refresh cadence applies.
    let due = svc.next_deadline().expect("a refresh is scheduled");
    assert!(due.secs() > 3_600, "{due:?}");
}

#[test]
fn a_machine_with_no_configured_servers_never_queries_and_says_so() {
    let sink = RecordingSink::default();
    let transport = FakeTransport::default();
    let mut svc = service(
        FakeClock::unset(),
        FakeStore::default(),
        transport.clone(),
        SystemConfig::default(),
        sink.clone(),
    );
    assert!(sink.saw(events::NO_SERVERS_CONFIGURED));
    assert!(svc.is_exhausted());
    assert_eq!(svc.next_deadline(), None);
    assert_eq!(svc.poll(Duration64::from_secs(3_600), 1), Step::Idle);
    assert!(transport.sent.borrow().is_empty());
}

#[test]
fn a_refused_send_is_audited_and_left_to_the_engines_own_backoff() {
    // No packet left the machine, so the request must not be retried on the
    // spot: the response timeout ends the transaction and the backoff paces
    // the next attempt.
    let sink = RecordingSink::default();
    let transport = FakeTransport {
        refuse: Some(Errno::NetworkUnreachable),
        ..FakeTransport::default()
    };
    let mut svc = service(
        FakeClock::unset(),
        FakeStore::default(),
        transport,
        configured(),
        sink.clone(),
    );
    let due = svc.next_deadline().expect("a query is scheduled");
    assert_eq!(
        svc.poll(due, 1),
        Step::NotSent(Errno::NetworkUnreachable),
        "the refusal is surfaced, not swallowed"
    );
    assert!(sink.saw(events::QUERY_NOT_SENT));
    // A second poll at the same instant sends nothing more: the transaction
    // is awaiting its own timeout.
    assert_eq!(svc.poll(due, 2), Step::Idle);
    let next = svc.next_deadline().expect("still scheduled");
    assert!(next > due, "the next attempt is later, never immediate");
}

#[test]
fn a_kernel_refusal_to_set_the_clock_is_audited_and_leaves_no_record() {
    // The service holds `CAP_TIME_SET`, so a refusal is a defect or a revoked
    // grant. It must be loud, and it must not claim a sync that never landed.
    let clock = FakeClock {
        set_result: Err(Errno::PermissionDenied),
        ..FakeClock::unset()
    };
    let store = FakeStore::default();
    let sink = RecordingSink::default();
    let mut svc = service(
        clock,
        store.clone(),
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    let (nonce, at) = in_flight(&mut svc);
    assert_eq!(
        svc.on_datagram(at, &reply(nonce, plausible())),
        Step::ClockRefused(Errno::PermissionDenied)
    );
    assert!(sink.saw(events::CLOCK_SET_REFUSED));
    assert!(!sink.saw(events::CLOCK_SET));
    assert!(
        store.document.borrow().is_none(),
        "an unapplied sample must not be recorded as one"
    );
    assert_eq!(svc.record(), SyncRecord::EMPTY);
}

#[test]
fn a_clock_that_was_set_survives_an_unwritable_record() {
    // Losing the record costs a future boot the short-vs-long power-off
    // distinction; it must never cost the clock.
    let clock = FakeClock::unset();
    let store = FakeStore {
        write_result: Some(Errno::PermissionDenied),
        ..FakeStore::default()
    };
    let sink = RecordingSink::default();
    let mut svc = service(
        clock.clone(),
        store,
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    let (nonce, at) = in_flight(&mut svc);
    assert!(matches!(
        svc.on_datagram(at, &reply(nonce, plausible())),
        Step::ClockSet(_)
    ));
    assert_eq!(clock.sets.borrow().len(), 1);
    assert!(sink.saw(events::CLOCK_SET));
    assert!(sink.saw(events::RECORD_NOT_WRITTEN));
}

#[test]
fn a_kiss_of_death_retires_the_server_and_is_audited() {
    let sink = RecordingSink::default();
    let clock = FakeClock::unset();
    let mut svc = service(
        clock.clone(),
        FakeStore::default(),
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    let (nonce, at) = in_flight(&mut svc);
    let mut kiss = reply(nonce, plausible());
    kiss[1] = 0; // stratum 0 — a Kiss-o'-Death
    kiss[12..16].copy_from_slice(b"DENY");
    assert_eq!(svc.on_datagram(at, &kiss), Step::NoSample);
    assert!(sink.saw(events::SERVER_RETIRED));
    assert!(clock.sets.borrow().is_empty());
}

#[test]
fn a_rate_limit_kiss_widens_the_interval_and_is_audited() {
    let sink = RecordingSink::default();
    let mut svc = service(
        FakeClock::unset(),
        FakeStore::default(),
        FakeTransport::default(),
        configured(),
        sink.clone(),
    );
    let (nonce, at) = in_flight(&mut svc);
    let mut kiss = reply(nonce, plausible());
    kiss[1] = 0;
    kiss[12..16].copy_from_slice(b"RATE");
    assert_eq!(svc.on_datagram(at, &kiss), Step::NoSample);
    assert!(sink.saw(events::SERVER_RATE_LIMITED));
}

#[test]
fn a_datagram_arriving_with_nothing_outstanding_changes_nothing() {
    let clock = FakeClock::unset();
    let sink = RecordingSink::default();
    let mut svc = service(
        clock.clone(),
        FakeStore::default(),
        FakeTransport::default(),
        configured(),
        sink,
    );
    // No query has gone out yet.
    assert_eq!(
        svc.on_datagram(Duration64::ZERO, &reply(1, plausible())),
        Step::Idle
    );
    assert!(clock.sets.borrow().is_empty());
}

#[test]
fn the_event_ids_are_frozen_inside_the_reserved_range() {
    // The identifiers are a contract with audit-log consumers; renumbering
    // them is a break this test refuses.
    for id in [
        events::SERVICE_READY,
        events::SERVICE_UNAVAILABLE,
        events::NO_SERVERS_CONFIGURED,
        events::CLOCK_SET,
        events::CLOCK_SET_REFUSED,
        events::SAMPLE_REFUSED,
        events::SERVER_RETIRED,
        events::SERVER_RATE_LIMITED,
        events::EVALUATION_FAILED,
        events::QUERY_NOT_SENT,
        events::RECORD_NOT_WRITTEN,
        events::SERVERS_EXHAUSTED,
    ] {
        assert!(
            (events::TIMED_RANGE_START..events::TIMED_RANGE_END).contains(&id.0),
            "{id:?} is outside the reserved range"
        );
    }
    assert_eq!(events::SERVICE_READY, EventId(23_001));
    assert_eq!(events::TIMED_RANGE_START, 23_000);
    assert_eq!(events::TIMED_RANGE_END, 24_000);
}

/// A boot-floor service starts before the encrypted root holding its store is
/// mounted, so its first read finds nothing. The ladder must arm on that, or
/// the service never re-reads and the clock is never set — which is exactly
/// what stranded it when a failed open was mistaken for a volume-less boot.
#[test]
fn the_reread_ladder_arms_whenever_no_server_is_configured() {
    let armed = ConfigRetry::arm(0, false).expect("an unconfigured service re-reads");
    assert_eq!(armed.at, CONFIG_RETRY_BASE_NANOS);
}

/// Nothing to wait for once a server is known.
#[test]
fn the_reread_ladder_stays_disarmed_when_a_server_is_configured() {
    assert_eq!(ConfigRetry::arm(0, true), None);
}

/// The rungs double and the ladder is finite, so a machine that genuinely has
/// no server configured stops reading instead of re-reading for the whole
/// boot. The window must still outlast an unlock by a wide margin.
#[test]
fn the_reread_ladder_doubles_and_is_spent_after_its_attempts() {
    let mut rung = ConfigRetry::arm(0, false).expect("armed");
    let mut at = rung.at;
    let mut climbed = 1;
    while rung.advance(at) {
        assert!(rung.at > at, "each rung waits longer than the last");
        at = rung.at;
        climbed += 1;
    }
    assert_eq!(climbed, CONFIG_RETRY_ATTEMPTS);
    // Rungs on time sum to (2^attempts - 1) base waits: ~17 minutes, against
    // an encrypted-root unlock measured in seconds.
    let window = u64::from(2u32.pow(CONFIG_RETRY_ATTEMPTS) - 1) * CONFIG_RETRY_BASE_NANOS;
    assert_eq!(at, window);
    assert!(
        at >= 600_000_000_000,
        "the window must outlast any unlock, got {at} ns"
    );
}

/// The deadline is absolute, so a rung reached late schedules from when it
/// actually ran rather than compounding the lateness.
#[test]
fn a_late_rung_schedules_from_the_moment_it_ran() {
    let mut rung = ConfigRetry::arm(0, false).expect("armed");
    let late = rung.at + 60_000_000_000;
    assert!(rung.advance(late));
    assert_eq!(rung.at, late + 2 * CONFIG_RETRY_BASE_NANOS);
}

/// A clock near the end of its range must not wrap the deadline backwards and
/// fire the ladder continuously.
#[test]
fn the_ladder_saturates_instead_of_wrapping_a_late_clock() {
    let mut rung = ConfigRetry::arm(u64::MAX, false).expect("armed");
    assert_eq!(rung.at, u64::MAX);
    assert!(rung.advance(u64::MAX));
    assert_eq!(rung.at, u64::MAX);
}
