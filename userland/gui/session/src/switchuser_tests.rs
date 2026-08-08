//! Tests for the session's half of fast user switching
//! ([`crate::switchuser`]).

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::driver::display::{DisplayFormat, DisplayMode};
use tairix_abi::origin::{CapabilitySummary, TrustDomain};
use tairix_abi::session_ipc::{SessionVerdict, SessionWake, SESSION_WAKE_LEN};
use tairix_abi::time::Duration64;
use tairix_abi::{CapabilityId, Errno, Origin, ProcId};

use crate::switchuser::{
    ResumeFailure, SeatPresentation, SessionAuthority, SwitchRefusal, SwitchUser, WakeRefusal,
    NO_DEADLINE_NS,
};

const WAKE_ENDPOINT: u64 = 0x5345_0000_0000_002a;
const CONSOLE: u64 = 3;
const OTHER_CONSOLE: u64 = 4;

/// One presentation step, recorded in the order it was performed, so a test
/// can assert that the seat is given up *after* the authority answered and
/// taken back *before* anything is drawn.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Step {
    Suspend,
    ReleaseSeat,
    AcquireSeat,
    QueryMode,
    Reconfigure(u32, u32),
    RepaintAll(u32, u32),
}

/// A scripted authority: answers `answer` and counts the asks.
struct FakeAuthority {
    answer: Result<SessionVerdict, Errno>,
    asked: usize,
    /// The shared step log, so an ask is ordered against the presentation
    /// steps around it.
    log: Vec<Step>,
}

impl FakeAuthority {
    fn accepting() -> Self {
        Self::answering(Ok(SessionVerdict::Accepted))
    }

    fn refusing() -> Self {
        Self::answering(Ok(SessionVerdict::Refused {
            retry_after: Duration64::ZERO,
        }))
    }

    fn answering(answer: Result<SessionVerdict, Errno>) -> Self {
        Self {
            answer,
            asked: 0,
            log: Vec::new(),
        }
    }
}

impl SessionAuthority for FakeAuthority {
    fn request_background(&mut self) -> Result<SessionVerdict, Errno> {
        self.asked += 1;
        // Nothing may have touched the screen before the answer.
        assert!(self.log.is_empty(), "presentation moved before the reply");
        self.answer
    }
}

/// A recording presentation with scripted answers for each fallible step.
struct FakePresentation {
    log: Vec<Step>,
    acquire: Result<(), Errno>,
    mode: Result<DisplayMode, Errno>,
    reconfigure: Result<(), Errno>,
    repaint: Result<(), Errno>,
}

impl FakePresentation {
    fn showing(mode: DisplayMode) -> Self {
        Self {
            log: Vec::new(),
            acquire: Ok(()),
            mode: Ok(mode),
            reconfigure: Ok(()),
            repaint: Ok(()),
        }
    }
}

impl SeatPresentation for FakePresentation {
    fn suspend(&mut self) {
        self.log.push(Step::Suspend);
    }

    fn release_seat(&mut self) {
        self.log.push(Step::ReleaseSeat);
    }

    fn acquire_seat(&mut self) -> Result<(), Errno> {
        self.log.push(Step::AcquireSeat);
        self.acquire
    }

    fn query_mode(&mut self) -> Result<DisplayMode, Errno> {
        self.log.push(Step::QueryMode);
        self.mode
    }

    fn reconfigure(&mut self, mode: DisplayMode) -> Result<(), Errno> {
        self.log
            .push(Step::Reconfigure(mode.width_px, mode.height_px));
        self.reconfigure
    }

    fn repaint_all(&mut self, mode: DisplayMode) -> Result<(), Errno> {
        self.log
            .push(Step::RepaintAll(mode.width_px, mode.height_px));
        self.repaint
    }
}

fn mode(width_px: u32, height_px: u32) -> DisplayMode {
    DisplayMode {
        width_px,
        height_px,
        stride_bytes: width_px * 4,
        format: DisplayFormat::Bgra8888,
    }
}

/// A wake-mailbox sender the kernel attested: on `console`, holding
/// `IPC_BIND_PRIVILEGED` when `privileged` — which is what separates the
/// session authority from every other process on the machine.
fn sender(console: u64, privileged: bool) -> Origin {
    let mut caps = CapabilitySummary::EMPTY;
    if privileged {
        caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
    }
    Origin::new(
        TrustDomain::User,
        13,
        13,
        42,
        ProcId::from_raw([9u8; tairix_abi::PROC_ID_LEN]),
        caps,
        console,
    )
}

fn authority() -> Origin {
    sender(CONSOLE, true)
}

fn encoded(wake: SessionWake) -> [u8; SESSION_WAKE_LEN] {
    let mut out = [0u8; SESSION_WAKE_LEN];
    let len = wake.encode(&mut out).expect("the buffer is exactly sized");
    assert_eq!(len, SESSION_WAKE_LEN);
    out
}

fn switchable() -> SwitchUser {
    SwitchUser::new(Some(WAKE_ENDPOINT), CONSOLE)
}

#[test]
fn a_bound_mailbox_is_what_makes_switching_available() {
    assert!(switchable().is_available());
    assert_eq!(switchable().wake_endpoint(), Some(WAKE_ENDPOINT));

    // A bind that failed leaves a session nothing could resume, so it
    // offers no switch at all rather than a one-way trip.
    let unbound = SwitchUser::new(None, CONSOLE);
    assert!(!unbound.is_available());
    assert_eq!(unbound.wake_endpoint(), None);
    assert!(!unbound.is_background());
}

#[test]
fn an_accepted_background_releases_the_seat_after_the_reply() {
    let mut switch = switchable();
    let mut authority = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));

    assert_eq!(switch.step_aside(&mut authority, &mut screen), Ok(()));

    assert_eq!(authority.asked, 1);
    // Presentation is torn down while the seat is still held, and the seat
    // goes last: the screen is never left owned by nobody.
    assert_eq!(screen.log, vec![Step::Suspend, Step::ReleaseSeat]);
    assert!(switch.is_background());
}

#[test]
fn a_refused_background_keeps_the_seat_and_the_session_drawing() {
    let mut switch = switchable();
    let mut authority = FakeAuthority::refusing();
    let mut screen = FakePresentation::showing(mode(1920, 1080));

    assert_eq!(
        switch.step_aside(&mut authority, &mut screen),
        Err(SwitchRefusal::Refused)
    );

    assert_eq!(authority.asked, 1);
    assert!(screen.log.is_empty(), "a refusal touched the screen");
    assert!(!switch.is_background());
}

#[test]
fn a_transport_failure_is_a_refusal() {
    let mut switch = switchable();
    let mut authority = FakeAuthority::answering(Err(Errno::NotFound));
    let mut screen = FakePresentation::showing(mode(1920, 1080));

    assert_eq!(
        switch.step_aside(&mut authority, &mut screen),
        Err(SwitchRefusal::Unreachable(Errno::NotFound))
    );

    assert!(
        screen.log.is_empty(),
        "an unreachable authority took the screen"
    );
    assert!(!switch.is_background());
}

#[test]
fn a_session_that_cannot_be_resumed_never_steps_aside() {
    let mut switch = SwitchUser::new(None, CONSOLE);
    let mut authority = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));

    assert_eq!(
        switch.step_aside(&mut authority, &mut screen),
        Err(SwitchRefusal::Unavailable)
    );

    // Not even asked: without a mailbox the answer could only strand us.
    assert_eq!(authority.asked, 0);
    assert!(screen.log.is_empty());
}

#[test]
fn a_background_session_does_not_step_aside_twice() {
    let mut switch = switchable();
    let mut authority = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));
    assert_eq!(switch.step_aside(&mut authority, &mut screen), Ok(()));

    let mut again = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));
    assert_eq!(
        switch.step_aside(&mut again, &mut screen),
        Err(SwitchRefusal::Unavailable)
    );
    assert_eq!(again.asked, 0);
    assert!(screen.log.is_empty());
}

#[test]
fn a_background_session_parks_with_no_deadline() {
    // What a foreground loop would arm: a held-back cache report tightening
    // the wait to the moment it may be sent.
    const HELD_BACK_NS: u64 = 250_000_000;

    let mut switch = switchable();
    assert_eq!(switch.park_deadline_ns(HELD_BACK_NS), HELD_BACK_NS);

    let mut authority = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));
    assert_eq!(switch.step_aside(&mut authority, &mut screen), Ok(()));

    // Background: no timer whatsoever, whatever the loop would have armed.
    assert_eq!(switch.park_deadline_ns(HELD_BACK_NS), NO_DEADLINE_NS);
    assert_eq!(switch.park_deadline_ns(0), NO_DEADLINE_NS);
}

#[test]
fn a_foreground_wake_reacquires_requeries_and_repaints_in_full() {
    let mut switch = switchable();
    let mut login = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));
    assert_eq!(switch.step_aside(&mut login, &mut screen), Ok(()));

    // The mode changed while the other account held the screen.
    let mut screen = FakePresentation::showing(mode(1280, 720));
    assert_eq!(
        switch.classify(&encoded(SessionWake::Foreground), &authority()),
        Ok(SessionWake::Foreground)
    );
    // The mode the resume adopted is answered back, so the caller rebuilds
    // what it owns (the pointer's clamp rectangle) against the new screen.
    assert_eq!(switch.resume(&mut screen), Ok(mode(1280, 720)));

    // The seat first, then the mode as it is *now*, then a frame sized to
    // it, then every pixel of it.
    assert_eq!(
        screen.log,
        vec![
            Step::AcquireSeat,
            Step::QueryMode,
            Step::Reconfigure(1280, 720),
            Step::RepaintAll(1280, 720),
        ]
    );
    assert!(!switch.is_background());
}

#[test]
fn a_resume_that_cannot_take_the_seat_back_reports_and_stays_background() {
    let mut switch = switchable();
    let mut authority = FakeAuthority::accepting();
    let mut screen = FakePresentation::showing(mode(1920, 1080));
    assert_eq!(switch.step_aside(&mut authority, &mut screen), Ok(()));

    let mut screen = FakePresentation::showing(mode(1920, 1080));
    screen.acquire = Err(Errno::PermissionDenied);
    let failure = switch.resume(&mut screen).expect_err("the seat was taken");

    assert_eq!(failure, ResumeFailure::SeatRefused(Errno::PermissionDenied));
    assert_eq!(failure.errno(), Errno::PermissionDenied);
    assert!(failure.reason().contains("ending the session"));
    // Nothing was drawn against a seat we do not hold.
    assert_eq!(screen.log, vec![Step::AcquireSeat]);
    assert!(switch.is_background());
}

#[test]
fn each_failing_resume_step_stops_the_ones_after_it() {
    for (script, expected, after) in [
        (
            FakePresentation {
                mode: Err(Errno::NotFound),
                ..FakePresentation::showing(mode(800, 600))
            },
            ResumeFailure::ModeUnavailable(Errno::NotFound),
            vec![Step::AcquireSeat, Step::QueryMode],
        ),
        (
            FakePresentation {
                reconfigure: Err(Errno::OutOfMemory),
                ..FakePresentation::showing(mode(800, 600))
            },
            ResumeFailure::FrameRefused(Errno::OutOfMemory),
            vec![
                Step::AcquireSeat,
                Step::QueryMode,
                Step::Reconfigure(800, 600),
            ],
        ),
        (
            FakePresentation {
                repaint: Err(Errno::DeviceFault),
                ..FakePresentation::showing(mode(800, 600))
            },
            ResumeFailure::PaintRefused(Errno::DeviceFault),
            vec![
                Step::AcquireSeat,
                Step::QueryMode,
                Step::Reconfigure(800, 600),
                Step::RepaintAll(800, 600),
            ],
        ),
    ] {
        let mut switch = switchable();
        let mut authority = FakeAuthority::accepting();
        let mut screen = FakePresentation::showing(mode(800, 600));
        assert_eq!(switch.step_aside(&mut authority, &mut screen), Ok(()));

        let mut screen = script;
        assert_eq!(switch.resume(&mut screen), Err(expected));
        assert_eq!(screen.log, after);
        assert!(switch.is_background());
    }
}

#[test]
fn an_end_wake_is_decoded_from_the_authority() {
    assert_eq!(
        switchable().classify(&encoded(SessionWake::End), &authority()),
        Ok(SessionWake::End)
    );
}

#[test]
fn an_undecodable_wake_is_dropped() {
    let switch = switchable();
    let mut corrupt = encoded(SessionWake::Foreground);
    corrupt[0] ^= 0xff;
    assert_eq!(
        switch.classify(&corrupt, &authority()),
        Err(WakeRefusal::Malformed(Errno::BadMagic))
    );

    // A short frame is not a truncated message to guess at either.
    assert_eq!(
        switch.classify(&[], &authority()),
        Err(WakeRefusal::Malformed(Errno::LengthOutOfRange))
    );
}

#[test]
fn a_wake_from_anybody_but_the_authority_is_dropped() {
    let switch = switchable();
    let wake = encoded(SessionWake::Foreground);

    // An unprivileged process of this user on this very console: it can
    // send to the mailbox, but it could never have served the rendezvous.
    assert_eq!(
        switch.classify(&wake, &sender(CONSOLE, false)),
        Err(WakeRefusal::Unattested)
    );
    // A privileged service, but on another console: it is somebody else's
    // authority, not this session's.
    assert_eq!(
        switch.classify(&wake, &sender(OTHER_CONSOLE, true)),
        Err(WakeRefusal::Unattested)
    );
    assert_eq!(
        switch.classify(&wake, &sender(OTHER_CONSOLE, false)),
        Err(WakeRefusal::Unattested)
    );
}

#[test]
fn an_unattested_sender_is_refused_before_the_message_is_read() {
    // Attestation first: a frame that would also fail to decode still
    // reports the sender as the reason, so an impostor learns nothing
    // about what the mailbox accepts.
    assert_eq!(
        switchable().classify(&[0xff; SESSION_WAKE_LEN], &sender(CONSOLE, false)),
        Err(WakeRefusal::Unattested)
    );
}

#[test]
fn every_refusal_states_a_reason() {
    for refusal in [
        SwitchRefusal::Unavailable,
        SwitchRefusal::Refused,
        SwitchRefusal::Unreachable(Errno::NotFound),
    ] {
        assert!(!refusal.reason().is_empty());
    }
    for refusal in [
        WakeRefusal::Unattested,
        WakeRefusal::Malformed(Errno::BadMagic),
    ] {
        assert!(!refusal.reason().is_empty());
    }
}
