//! Tests for the app-ward event hold-back ([`crate::holdback`]).

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::input::{KeyInput, KeyValue, Modifiers, PointerButtonCode};
use tairix_abi::window_ipc::{PointerAction, WindowEvent};
use tairix_abi::Errno;

use crate::holdback::{Delivery, HoldBack, HOLD_BACK_CAPACITY};

const MAILBOX: u64 = 0x0A00;
const OTHER_MAILBOX: u64 = 0x0B00;
const WINDOW: u64 = 7;
const SIBLING: u64 = 9;

fn resized(window_id: u64, width_px: u32) -> WindowEvent {
    WindowEvent::Resized {
        window_id,
        width_px,
        height_px: 100,
    }
}

fn moved(window_id: u64, x: u32) -> WindowEvent {
    WindowEvent::Pointer {
        window_id,
        x,
        y: 4,
        action: PointerAction::Moved,
    }
}

fn pressed(window_id: u64) -> WindowEvent {
    WindowEvent::Pointer {
        window_id,
        x: 1,
        y: 1,
        action: PointerAction::Pressed(PointerButtonCode::Primary),
    }
}

fn released(window_id: u64) -> WindowEvent {
    WindowEvent::Pointer {
        window_id,
        x: 1,
        y: 1,
        action: PointerAction::Released(PointerButtonCode::Primary),
    }
}

fn typed(window_id: u64) -> WindowEvent {
    WindowEvent::Key {
        window_id,
        key: KeyInput::Pressed {
            key: KeyValue::Char('a'),
            modifiers: Modifiers::default(),
        },
    }
}

fn scrolled(window_id: u64, dy: i32) -> WindowEvent {
    WindowEvent::Scrolled {
        window_id,
        dx: 0,
        dy,
    }
}

/// Offer `event` to a destination whose mailbox is full, so it is held,
/// reporting whether it asked for a room wake.
fn hold_for(held: &mut HoldBack, endpoint: u64, event: WindowEvent) -> bool {
    match held.deliver(endpoint, &event, |_| Err(Errno::WouldBlock)) {
        Ok(Delivery::Owed { watch }) => watch,
        other => panic!("back-pressure is held, not {other:?}"),
    }
}

/// The same, for the destination most tests use.
fn hold(held: &mut HoldBack, event: WindowEvent) -> bool {
    hold_for(held, MAILBOX, event)
}

/// Drain everything the hold-back owes into a transcript.
fn drain(held: &mut HoldBack) -> Vec<(u64, WindowEvent)> {
    let mut out = Vec::new();
    let report = held.flush(|endpoint, event| {
        out.push((endpoint, *event));
        Ok(())
    });
    assert!(report.gone.is_empty(), "nothing died in this drain");
    out
}

/// The same transcript with the destination dropped.
fn drain_events(held: &mut HoldBack) -> Vec<WindowEvent> {
    drain(held).into_iter().map(|(_, event)| event).collect()
}

/// A bounded destination mailbox — the kernel port an app drains — so a
/// test can put the session under real back-pressure.
struct Mailbox {
    capacity: usize,
    waiting: usize,
    received: Vec<WindowEvent>,
}

impl Mailbox {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            waiting: 0,
            received: Vec::new(),
        }
    }

    fn send(&mut self, event: &WindowEvent) -> Result<(), Errno> {
        if self.waiting == self.capacity {
            return Err(Errno::WouldBlock);
        }
        self.waiting += 1;
        self.received.push(*event);
        Ok(())
    }

    /// The app catches up and empties its mailbox.
    fn drained_by_its_owner(&mut self) {
        self.waiting = 0;
    }
}

// --- The defect ------------------------------------------------------------

/// An event a full mailbox refused is *owed*, not lost.
///
/// The mailbox is a bounded resource, so a merely slow app fills it. If the
/// refusal drops the event, the app never learns its new size — it lays out
/// and hit-tests at one the compositor no longer uses — and its picker never
/// concludes, which strands the window's one pending pick for the life of the
/// window. Both must arrive the moment the app catches up, in the order they
/// happened and behind the input that preceded them.
#[test]
fn a_resize_and_a_pick_conclusion_survive_a_full_mailbox() {
    let mut post = Mailbox::new(2);
    let mut held = HoldBack::new();

    for _ in 0..2 {
        assert_eq!(
            held.deliver(MAILBOX, &typed(WINDOW), |event| post.send(event)),
            Ok(Delivery::Sent),
            "room to spare"
        );
    }
    assert_eq!(post.received.len(), 2, "the mailbox is now full");

    // The app is behind, and the desktop has two things it must not lose.
    assert_eq!(
        held.deliver(MAILBOX, &resized(WINDOW, 800), |event| post.send(event)),
        Ok(Delivery::Owed { watch: true }),
        "the first debt asks for a room wake"
    );
    assert_eq!(
        held.deliver(
            MAILBOX,
            &WindowEvent::PickCancelled { window_id: WINDOW },
            |event| post.send(event)
        ),
        Ok(Delivery::Owed { watch: false }),
        "held behind the resize, on the wake already armed"
    );
    assert_eq!(post.received.len(), 2, "nothing more reached the app yet");

    // The app drains, the kernel reports room, and the session sends what it
    // owes.
    post.drained_by_its_owner();
    let report = held.flush(|_, event| post.send(event));
    assert_eq!(report.settled, vec![MAILBOX]);
    assert_eq!(
        post.received,
        vec![
            typed(WINDOW),
            typed(WINDOW),
            resized(WINDOW, 800),
            WindowEvent::PickCancelled { window_id: WINDOW },
        ]
    );
}

/// A destination already owed something takes the next event unsent, even
/// when its mailbox has room again: sending it now would put it ahead of what
/// is queued, and the app must see its events in the order they happened.
#[test]
fn a_later_event_never_overtakes_one_already_owed() {
    let mut post = Mailbox::new(1);
    let mut held = HoldBack::new();

    let _ = held
        .deliver(MAILBOX, &typed(WINDOW), |event| post.send(event))
        .expect("room to spare");
    let _ = held
        .deliver(MAILBOX, &resized(WINDOW, 800), |event| post.send(event))
        .expect("refused and held");
    post.drained_by_its_owner();
    assert_eq!(
        held.deliver(MAILBOX, &pressed(WINDOW), |event| post.send(event)),
        Ok(Delivery::Owed { watch: false }),
        "the destination has room again, but it owes something first"
    );
    assert_eq!(post.received.len(), 1, "the press did not jump the queue");

    // Each time the app catches up, the session sends more of what it owes.
    while held.owes(MAILBOX) {
        post.drained_by_its_owner();
        let _ = held.flush(|_, event| post.send(event));
    }
    assert_eq!(
        post.received,
        vec![typed(WINDOW), resized(WINDOW, 800), pressed(WINDOW)]
    );
}

/// An event that is owed says so, so its caller can keep the evidence its
/// responsiveness verdict rests on.
///
/// The regression this guards: an event the hold-back takes issues no send,
/// so the desktop's "not responding" detector sees no refusal of its own.
/// If the hold-back reported only "arm a wake", the very case the detector
/// exists for — an app that stops draining, so no room wake ever comes —
/// would stop producing evidence after the *first* refusal and the app
/// would never be flagged. `Owed` is that evidence; only `Sent` clears it.
#[test]
fn an_owed_event_is_distinguishable_from_a_sent_one() {
    let mut post = Mailbox::new(1);
    let mut held = HoldBack::new();

    assert_eq!(
        held.deliver(MAILBOX, &typed(WINDOW), |event| post.send(event)),
        Ok(Delivery::Sent)
    );
    for expected in [
        Delivery::Owed { watch: true },
        Delivery::Owed { watch: false },
        Delivery::Owed { watch: false },
    ] {
        assert_eq!(
            held.deliver(MAILBOX, &typed(WINDOW), |event| post.send(event)),
            Ok(expected),
            "every event the app cannot take is still owed"
        );
    }
}

/// A refusal that is not back-pressure is the destination's, not the
/// session's: it is surfaced rather than held, so the caller can tear the
/// owner down instead of accumulating events for a corpse.
#[test]
fn a_refusal_that_is_not_back_pressure_is_surfaced() {
    let mut held = HoldBack::new();
    assert_eq!(
        held.deliver(MAILBOX, &resized(WINDOW, 800), |_| Err(Errno::NotFound)),
        Err(Errno::NotFound)
    );
    assert!(!held.owes(MAILBOX), "nothing is owed to a dead port");
}

// --- Folding ---------------------------------------------------------------

/// A state edge names what *is*: a second one replaces the first where it
/// stands, so the app converges on the truth and the queue does not grow.
/// The occurrences queued around it keep their order.
#[test]
fn a_later_state_edge_replaces_the_one_it_supersedes() {
    let mut held = HoldBack::new();
    assert!(hold(&mut held, resized(WINDOW, 100)));
    assert!(!hold(&mut held, typed(WINDOW)));
    assert!(!hold(&mut held, resized(WINDOW, 200)));

    assert_eq!(
        held.depth(MAILBOX, WINDOW),
        2,
        "one resize is owed, not two"
    );
    assert_eq!(
        drain_events(&mut held),
        vec![resized(WINDOW, 200), typed(WINDOW)]
    );
}

/// Each state edge holds its own slot: they supersede their own kind and
/// nothing else.
#[test]
fn state_edges_of_different_kinds_are_each_owed() {
    let mut held = HoldBack::new();
    let edges = [
        WindowEvent::Focus {
            window_id: WINDOW,
            focused: true,
        },
        resized(WINDOW, 100),
        WindowEvent::RedrawRequested { window_id: WINDOW },
        WindowEvent::CloseRequested { window_id: WINDOW },
        WindowEvent::Minimized { window_id: WINDOW },
    ];
    for edge in edges {
        let _ = hold(&mut held, edge);
    }
    for edge in edges {
        let _ = hold(&mut held, edge);
    }
    assert_eq!(held.depth(MAILBOX, WINDOW), edges.len());
}

/// A position is level-triggered and a wheel tick is additive, so a run of
/// either collapses — but only while it is unbroken, and a wheel reversal is
/// a distinct gesture (an intermediate clamp at a range end is a real
/// difference the app must see).
#[test]
fn a_run_of_samples_collapses_and_a_run_of_ticks_sums() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, moved(WINDOW, 1));
    let _ = hold(&mut held, moved(WINDOW, 2));
    let _ = hold(&mut held, moved(WINDOW, 3));
    let _ = hold(&mut held, scrolled(WINDOW, 1));
    let _ = hold(&mut held, scrolled(WINDOW, 2));
    let _ = hold(&mut held, scrolled(WINDOW, -1));
    let _ = hold(&mut held, moved(WINDOW, 4));

    assert_eq!(
        drain_events(&mut held),
        vec![
            moved(WINDOW, 3),
            scrolled(WINDOW, 3),
            scrolled(WINDOW, -1),
            moved(WINDOW, 4),
        ]
    );
}

/// A press between two samples breaks the run: an occurrence the app must
/// witness is never reordered around by a fold.
#[test]
fn an_occurrence_between_two_samples_breaks_the_run() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, moved(WINDOW, 1));
    let _ = hold(&mut held, pressed(WINDOW));
    let _ = hold(&mut held, moved(WINDOW, 2));

    assert_eq!(
        drain_events(&mut held),
        vec![moved(WINDOW, 1), pressed(WINDOW), moved(WINDOW, 2)]
    );
}

// --- The bound -------------------------------------------------------------

/// The bound holds however long an owner stays away, and what it sheds is
/// always an input occurrence: the state edge and the pick conclusion the
/// app cannot re-derive survive the whole storm.
#[test]
fn overflow_sheds_input_and_never_a_state_edge_or_a_conclusion() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, resized(WINDOW, 100));
    let _ = hold(&mut held, WindowEvent::PickCancelled { window_id: WINDOW });
    for _ in 0..HOLD_BACK_CAPACITY * 4 {
        let _ = hold(&mut held, typed(WINDOW));
    }

    assert_eq!(held.depth(MAILBOX, WINDOW), HOLD_BACK_CAPACITY);
    let drained = drain_events(&mut held);
    assert_eq!(
        drained.first(),
        Some(&resized(WINDOW, 100)),
        "the resize outlives every keystroke"
    );
    assert!(
        drained.contains(&WindowEvent::PickCancelled { window_id: WINDOW }),
        "the picker's one conclusion is never shed"
    );
}

/// Oldest-first is the safe direction for a button: the app can be left
/// with a release it did not press, never a press it never released — the
/// latch that would leave a widget dragging for ever.
#[test]
fn a_press_is_shed_before_its_release() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, pressed(WINDOW));
    let _ = hold(&mut held, released(WINDOW));
    // One past the bound, so exactly one event is shed.
    for _ in 0..HOLD_BACK_CAPACITY - 1 {
        let _ = hold(&mut held, typed(WINDOW));
    }

    let drained = drain_events(&mut held);
    assert_eq!(drained.len(), HOLD_BACK_CAPACITY);
    assert!(!drained.contains(&pressed(WINDOW)), "the press went first");
    assert_eq!(
        drained.first(),
        Some(&released(WINDOW)),
        "its release is still owed"
    );
}

/// One window's overflow is its own: a sibling window's queue is a separate
/// debt and keeps everything it was owed.
#[test]
fn the_bound_is_per_window_not_per_owner() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, resized(SIBLING, 640));
    for _ in 0..HOLD_BACK_CAPACITY * 2 {
        let _ = hold(&mut held, typed(WINDOW));
    }

    assert_eq!(held.depth(MAILBOX, WINDOW), HOLD_BACK_CAPACITY);
    assert_eq!(held.depth(MAILBOX, SIBLING), 1);
}

// --- Flushing --------------------------------------------------------------

/// The first debt to a destination is what arms its wake; everything after
/// it rides the same one, and the wake is dropped only once the destination
/// owes nothing.
#[test]
fn only_the_first_debt_asks_for_a_wake() {
    let mut held = HoldBack::new();
    assert!(hold(&mut held, resized(WINDOW, 100)));
    assert!(!hold(&mut held, typed(WINDOW)));
    assert!(!hold(&mut held, resized(SIBLING, 200)));
    assert!(
        hold_for(&mut held, OTHER_MAILBOX, typed(WINDOW)),
        "a new destination"
    );
    assert!(held.owes(MAILBOX));

    let report = held.flush(|_, _| Ok(()));
    assert_eq!(report.settled, vec![MAILBOX, OTHER_MAILBOX]);
    assert!(report.gone.is_empty());
    assert!(!held.owes(MAILBOX));
}

/// A mailbox that fills again keeps the rest owed — nothing is dropped and
/// the wake stands, so the next drain resumes exactly where this one
/// stopped.
#[test]
fn a_mailbox_that_fills_again_keeps_the_rest_owed() {
    let mut held = HoldBack::new();
    for _ in 0..5 {
        let _ = hold(&mut held, typed(WINDOW));
    }

    let mut room = 2;
    let report = held.flush(|_, _| {
        if room == 0 {
            return Err(Errno::WouldBlock);
        }
        room -= 1;
        Ok(())
    });
    assert!(report.settled.is_empty(), "the destination is not settled");
    assert!(report.gone.is_empty());
    assert_eq!(held.depth(MAILBOX, WINDOW), 3);

    assert_eq!(drain_events(&mut held).len(), 3);
    assert!(!held.owes(MAILBOX));
}

/// Windows share their owner's one mailbox, so a window with a long backlog
/// must not spend all the room: each pass serves one event per window.
#[test]
fn windows_are_served_round_robin_so_a_backlog_starves_no_sibling() {
    let mut held = HoldBack::new();
    for _ in 0..10 {
        let _ = hold(&mut held, typed(WINDOW));
    }
    let _ = hold(&mut held, resized(SIBLING, 640));

    let mut room = 2;
    let mut served: Vec<u64> = Vec::new();
    let _ = held.flush(|_, event| {
        if room == 0 {
            return Err(Errno::WouldBlock);
        }
        room -= 1;
        served.push(event.window_id());
        Ok(())
    });
    assert_eq!(
        served,
        vec![WINDOW, SIBLING],
        "the sibling's resize got the second slot, not the backlog"
    );
}

/// An owner the send proves gone takes its debts with it, and is reported so
/// the caller can drop its wake and tear its windows down.
#[test]
fn a_destination_that_is_gone_is_discarded_and_reported() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, typed(WINDOW));
    let _ = hold(&mut held, resized(SIBLING, 640));
    let _ = hold_for(&mut held, OTHER_MAILBOX, typed(WINDOW));

    let report = held.flush(|endpoint, _| {
        if endpoint == MAILBOX {
            Err(Errno::NotFound)
        } else {
            Ok(())
        }
    });
    assert_eq!(report.gone, vec![MAILBOX]);
    assert_eq!(report.settled, vec![OTHER_MAILBOX]);
    assert!(!held.owes(MAILBOX));
}

/// A refusal that waiting cannot fix is not back-pressure: that one event is
/// dropped and the rest go on, so a flush can never re-offer the same event
/// for ever.
#[test]
fn an_unrecoverable_refusal_drops_only_that_event() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, resized(WINDOW, 100));
    let _ = hold(&mut held, typed(WINDOW));

    let mut offered = 0_u32;
    let report = held.flush(|_, _| {
        offered += 1;
        if offered == 1 {
            Err(Errno::MessageTooLarge)
        } else {
            Ok(())
        }
    });
    assert_eq!(offered, 2, "the second event was still offered");
    assert_eq!(report.settled, vec![MAILBOX]);
    assert!(!held.owes(MAILBOX));
}

/// A reaped owner's debts go with it, and the caller is told a wake it armed
/// is now stale.
#[test]
fn forgetting_an_owner_discards_everything_it_was_owed() {
    let mut held = HoldBack::new();
    let _ = hold(&mut held, typed(WINDOW));
    let _ = hold(&mut held, resized(SIBLING, 640));
    let _ = hold_for(&mut held, OTHER_MAILBOX, typed(WINDOW));

    assert!(held.forget(MAILBOX), "a wake was armed for it");
    assert!(!held.owes(MAILBOX));
    assert!(held.owes(OTHER_MAILBOX), "its neighbour is untouched");
    assert!(!held.forget(MAILBOX), "forgetting twice arms nothing");
}
