//! Host tests of one seat wake's drain: who receives each queued event, and
//! what is left queued for the next holder.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::input::{KeyInput, KeyValue, Modifiers as AbiModifiers, NamedKeyCode};
use tairix_abi::time::Duration64;
use tairix_abi::window_ipc::{AppBarClick, AppMenuItem, AppMenuItemId, AppMenuLabel, AppMenuRow};
use tairix_abi::Errno;
use tairix_icon::IconKind;
use tairix_taskbar::TaskbarResponse;
use tairix_wm::{Compositor, InputEvent, InputResponse, Point};

use crate::drain::{drain_away, drain_locked, Routed, Seat, SeatDrain, SeatRouter, SeatWake};
use crate::keyboard::{KeyInputChannel, KeyRepeat, KeyboardInputSource};
use crate::lock::ScreenLock;
use crate::menu::{open_desktop_menu, ChainOutcome, ChainOwner, MenuChain};
use crate::shell::{DesktopShell, ShellOutcome};
use crate::tests::{
    app_slot_point, chain_geometry_over, compositor, moved, opaque_window, open_bar_chain, shell,
    MemoryInput, ScriptedUnlocker, PRIMARY_PRESS, PRIMARY_RELEASE, SECONDARY_PRESS,
    SECONDARY_RELEASE,
};
use crate::windows::chain_geometry;

/// The session program, reduced to what a drain hands it: it opens the icon
/// bar's menus as the real one does and records everything else.
struct Program {
    routed: Vec<(ShellOutcome, Option<KeyInput>)>,
    settles: usize,
    /// Routing an outcome this accepts locks the screen.
    lock_on: fn(&ShellOutcome) -> bool,
    /// What routing an outcome decides about the session.
    decide: fn(&ShellOutcome) -> Routed,
    /// Whether the session authority lets the session step aside.
    switch_accepted: bool,
}

impl Program {
    fn new() -> Self {
        Self {
            routed: Vec::new(),
            settles: 0,
            lock_on: |_| false,
            decide: |_| Routed::Continue,
            switch_accepted: false,
        }
    }

    fn outcomes(&self) -> Vec<&ShellOutcome> {
        self.routed.iter().map(|(outcome, _)| outcome).collect()
    }
}

impl SeatRouter for Program {
    fn route(
        &mut self,
        seat: &mut Seat<'_>,
        outcome: ShellOutcome,
        key: Option<KeyInput>,
        _now_ns: u64,
    ) -> Routed {
        if let ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(request)) = &outcome {
            let request = request.clone();
            let opened = {
                let geom = chain_geometry(seat.shell.session(), seat.compositor);
                open_desktop_menu(
                    seat.menu,
                    ChainOwner::Bar(request.subject),
                    request.model,
                    request.placement,
                    seat.lock.is_locked(),
                    &geom,
                )
            };
            assert!(opened.is_ok(), "the bar's menu opens");
            assert!(
                seat.shell
                    .present_menu_chain(seat.compositor, seat.menu, None),
                "a plate drawn"
            );
        }
        if (self.lock_on)(&outcome) {
            assert!(seat
                .lock
                .engage(("ann", "ann"), seat.shell, seat.compositor));
        }
        let decided = (self.decide)(&outcome);
        self.routed.push((outcome, key));
        decided
    }

    fn settle_chain(
        &mut self,
        seat: &mut Seat<'_>,
        answered: &mut Vec<ShellOutcome>,
        _now_ns: u64,
    ) {
        self.settles += 1;
        let _ = seat
            .shell
            .present_menu_chain(seat.compositor, seat.menu, None);
        for (owner, outcome) in seat.menu.take_answers() {
            if let (ChainOwner::Bar(subject), ChainOutcome::Chosen(item)) = (owner, outcome) {
                if let Some(response) = seat
                    .shell
                    .session_mut()
                    .taskbar_mut()
                    .menu_chosen(&subject, item)
                {
                    answered.push(ShellOutcome::Taskbar(response));
                }
            }
        }
    }

    fn step_aside(&mut self, _seat: &mut Seat<'_>) -> bool {
        self.switch_accepted
    }
}

/// A keyboard channel yielding queued records, then `fault` forever once
/// they are gone, if one is set.
struct Keys {
    records: VecDeque<[u8; KeyInput::WIRE_LEN]>,
    fault: Option<Errno>,
}

impl KeyInputChannel for Keys {
    fn next_record(&mut self) -> Result<Option<[u8; KeyInput::WIRE_LEN]>, Errno> {
        match self.records.pop_front() {
            Some(record) => Ok(Some(record)),
            None => self.fault.map_or(Ok(None), Err),
        }
    }
}

const NO_REPEAT: KeyRepeat = KeyRepeat {
    delay: Duration64::from_millis(500),
    interval: None,
};

fn keyboard(records: &[KeyInput]) -> KeyboardInputSource<Keys> {
    KeyboardInputSource::new(
        Keys {
            records: records.iter().map(KeyInput::to_le_bytes).collect(),
            fault: None,
        },
        NO_REPEAT,
    )
}

fn queued_keys(keyboard: &KeyboardInputSource<Keys>) -> usize {
    keyboard.channel().records.len()
}

fn char_key(c: char) -> KeyInput {
    KeyInput::Pressed {
        key: KeyValue::Char(c),
        modifiers: AbiModifiers::default(),
    }
}

fn named_key(named: NamedKeyCode) -> KeyInput {
    KeyInput::Pressed {
        key: KeyValue::Named(named),
        modifiers: AbiModifiers::default(),
    }
}

fn shift_held(held: bool) -> KeyInput {
    KeyInput::ModifiersChanged {
        modifiers: AbiModifiers {
            shift: held,
            ..AbiModifiers::default()
        },
    }
}

/// The desktop and the seat's other holders, as one wake finds them.
struct Desk {
    shell: DesktopShell,
    comp: Compositor,
    menu: MenuChain,
    lock: ScreenLock,
}

impl Desk {
    fn new((shell, comp): (DesktopShell, Compositor)) -> Self {
        Self {
            shell,
            comp,
            menu: MenuChain::new(),
            lock: ScreenLock::new(),
        }
    }

    fn seat(&mut self) -> Seat<'_> {
        Seat {
            shell: &mut self.shell,
            compositor: &mut self.comp,
            menu: &mut self.menu,
            lock: &mut self.lock,
        }
    }

    fn wake(
        &mut self,
        pointer: &mut MemoryInput,
        keys: &mut KeyboardInputSource<Keys>,
        program: &mut Program,
    ) -> Result<SeatWake, Errno> {
        SeatDrain::new().wake(&mut self.seat(), pointer, keys, program, 0)
    }
}

/// The *New window* row every slot of [`bar_desktop`] declares.
fn new_window() -> AppMenuItemId {
    AppMenuItemId::new(tairix_window::QUIT_ROW + 1).expect("non-zero")
}

/// A desktop with one application slot whose declared menu carries
/// *New window*.
fn bar_desktop() -> (DesktopShell, Compositor) {
    let bar = tairix_window::declaration(
        0,
        AppBarClick::Open,
        &[AppMenuRow::Item(AppMenuItem::new(
            new_window(),
            AppMenuLabel::new("New window").expect("short"),
        ))],
    )
    .expect("the convention fits");
    let mut shell = shell();
    let mut comp = compositor();
    shell.set_apps(
        &mut comp,
        vec![
            tairix_taskbar::AppSlot::new("Terminal", IconKind::AppBundle)
                .with_declaration(bar.menu, bar.click),
        ],
    );
    (shell, comp)
}

/// The slot, and where its menu draws the *New window* row, read off a twin
/// desktop the way the vertical's script reconstructs it.
fn slot_and_row() -> (Point, Point) {
    let (mut twin, mut comp) = bar_desktop();
    let slot = app_slot_point(&twin, 0);
    twin.handle(moved(slot.x, slot.y), &mut comp, 0);
    let ShellOutcome::Taskbar(TaskbarResponse::OpenMenu(asked)) =
        twin.handle(SECONDARY_PRESS, &mut comp, 0)
    else {
        panic!("a secondary press on a declared slot asked for no menu");
    };
    let theme = twin.session().floating_theme().clone();
    let geom = chain_geometry_over(&comp, &theme);
    let (_, row) = open_bar_chain(&mut twin, &mut comp, asked, "New window", &geom);
    (slot, row)
}

/// The events that right-click the slot, opening its menu.
fn open_menu(slot: Point) -> [InputEvent; 3] {
    [moved(slot.x, slot.y), SECONDARY_PRESS, SECONDARY_RELEASE]
}

/// Under load a right-click on a slot and the click on the row of the menu
/// it asks for arrive in one wake: the row click is the chain's, because the
/// grab begins at the press that opens it, not at the frame that draws it.
#[test]
fn a_row_click_queued_behind_the_press_that_opens_a_bar_menu_is_the_chains() {
    let (slot, row) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let mut pointer = MemoryInput::new(
        &[
            &open_menu(slot)[..],
            &[moved(row.x, row.y), PRIMARY_PRESS, PRIMARY_RELEASE],
        ]
        .concat(),
    );
    let mut program = Program::new();

    let woke = desk.wake(&mut pointer, &mut keyboard(&[]), &mut program);

    assert_eq!(woke, Ok(SeatWake::Served));
    assert_eq!(pointer.remaining(), 0);
    assert!(!desk.menu.is_open(), "the chosen row closed the chain");
    assert!(
        matches!(
            program.outcomes().last(),
            Some(ShellOutcome::Taskbar(TaskbarResponse::AppMenuChosen { app: 0, item }))
                if *item == new_window()
        ),
        "the queued click chose the row it was aimed at: {:?}",
        program.outcomes()
    );
}

/// A chain dismissed part-way through a wake hands the rest of the pointer
/// queue to the shell in that same wake; the dismissing press is consumed.
#[test]
fn input_behind_the_press_that_dismisses_a_chain_reaches_the_shell() {
    let (slot, _) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let window = opaque_window(&mut desk.comp, Point::new(200, 200), 300, 300);
    let mut pointer = MemoryInput::new(
        &[
            &open_menu(slot)[..],
            &[
                moved(250, 250),
                PRIMARY_PRESS,
                PRIMARY_RELEASE,
                moved(260, 260),
                PRIMARY_PRESS,
            ],
        ]
        .concat(),
    );
    let mut program = Program::new();

    let woke = desk.wake(&mut pointer, &mut keyboard(&[]), &mut program);

    assert_eq!(woke, Ok(SeatWake::Served));
    assert!(
        !desk.menu.is_open(),
        "the press outside dismissed the chain"
    );
    assert_eq!(pointer.remaining(), 0);
    let activations: Vec<_> = program
        .outcomes()
        .into_iter()
        .filter(|outcome| {
            matches!(
                outcome,
                ShellOutcome::WindowManager(InputResponse::Activated { window: w, .. })
                    if *w == window
            )
        })
        .collect();
    assert_eq!(
        activations.len(),
        1,
        "only the press after the dismissal reached the window: {:?}",
        program.outcomes()
    );
}

/// The chain is brought into line with the screen once per phase that
/// changed it, not once per event.
#[test]
fn a_chain_settles_once_per_drain_phase() {
    let (slot, row) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let mut pointer = MemoryInput::new(
        &[
            &open_menu(slot)[..],
            &[
                moved(row.x - 4, row.y),
                moved(row.x - 2, row.y),
                moved(row.x, row.y),
                PRIMARY_PRESS,
                PRIMARY_RELEASE,
            ],
        ]
        .concat(),
    );
    let mut program = Program::new();

    desk.wake(&mut pointer, &mut keyboard(&[]), &mut program)
        .expect("in-memory sources do not fault");

    assert_eq!(program.settles, 1, "five chain events, one settle");
}

/// Input behind the event that locks the screen stays queued for the lock's
/// own drain; none of it reaches the shell.
#[test]
fn input_behind_the_event_that_locks_waits_for_the_lock() {
    let mut desk = Desk::new((shell(), compositor()));
    let window = opaque_window(&mut desk.comp, Point::new(200, 200), 300, 300);
    let mut pointer = MemoryInput::new(&[moved(250, 250), PRIMARY_PRESS, PRIMARY_RELEASE]);
    let mut keys = keyboard(&[char_key('p')]);
    let mut program = Program::new();
    program.lock_on = |outcome| {
        matches!(
            outcome,
            ShellOutcome::WindowManager(InputResponse::Activated { .. })
        )
    };

    let woke = desk.wake(&mut pointer, &mut keys, &mut program);

    assert_eq!(woke, Ok(SeatWake::Served));
    assert!(desk.lock.is_locked());
    assert_eq!(pointer.remaining(), 1, "the release waits for the lock");
    assert_eq!(queued_keys(&keys), 1, "so does the key");
    assert!(matches!(
        program.outcomes().last(),
        Some(ShellOutcome::WindowManager(InputResponse::Activated { window: w, .. }))
            if *w == window
    ));
}

/// Each key is the shell's only while the shell holds the seat: a key that
/// locks the screen keeps the next from the focused window.
#[test]
fn a_key_that_locks_the_screen_keeps_the_next_from_the_shell() {
    let mut desk = Desk::new((shell(), compositor()));
    let mut keys = keyboard(&[char_key('l'), char_key('p')]);
    let mut program = Program::new();
    program.lock_on = |_| true;

    desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program)
        .expect("in-memory sources do not fault");

    assert_eq!(program.routed.len(), 1);
    assert_eq!(program.routed[0].1, Some(char_key('l')));
    assert_eq!(queued_keys(&keys), 1, "the second key waits for the lock");
}

/// A key that closes the chain leaves the keys after it to the shell, and
/// none reaches the closed chain.
#[test]
fn keys_after_the_key_that_closes_a_chain_reach_the_shell() {
    let (slot, _) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let mut pointer = MemoryInput::new(&open_menu(slot));
    let mut program = Program::new();
    desk.wake(&mut pointer, &mut keyboard(&[]), &mut program)
        .expect("in-memory sources do not fault");
    assert!(desk.menu.is_open());
    program.routed.clear();

    let mut keys = keyboard(&[named_key(NamedKeyCode::Escape), char_key('a')]);
    let woke = desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program);

    assert_eq!(woke, Ok(SeatWake::Served));
    assert!(!desk.menu.is_open(), "Escape closed the chain");
    assert_eq!(queued_keys(&keys), 0);
    assert_eq!(program.routed.len(), 1, "{:?}", program.routed);
    assert_eq!(program.routed[0].1, Some(char_key('a')));
}

/// A modifier edge reaches the seat's modifier state whoever holds the seat,
/// so a click after the grab is not stamped with a modifier already released.
#[test]
fn a_modifier_edge_under_a_chain_reaches_the_seat() {
    let (slot, _) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let mut program = Program::new();
    desk.wake(
        &mut MemoryInput::new(&open_menu(slot)),
        &mut keyboard(&[]),
        &mut program,
    )
    .expect("in-memory sources do not fault");
    assert!(desk.menu.is_open());

    let mut keys = keyboard(&[shift_held(true)]);
    desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program)
        .expect("in-memory sources do not fault");

    assert!(
        desk.menu.is_open(),
        "a modifier edge is not a key to the chain"
    );
    assert!(desk.shell.modifiers().shift);
}

#[test]
fn a_modifier_edge_under_the_lock_reaches_the_seat() {
    let mut desk = Desk::new((shell(), compositor()));
    assert!(desk
        .lock
        .engage(("ann", "ann"), &desk.shell, &mut desk.comp));

    drain_locked(
        &mut desk.seat(),
        &mut MemoryInput::new(&[]),
        &mut keyboard(&[shift_held(true)]),
        &mut ScriptedUnlocker::refusing(),
        0,
    )
    .expect("in-memory sources do not fault");

    assert!(desk.lock.is_locked());
    assert!(desk.shell.modifiers().shift);
}

#[test]
fn a_modifier_edge_behind_the_screensaver_reaches_the_seat() {
    let mut desk = Desk::new((shell(), compositor()));
    let mut keys = keyboard(&[shift_held(true), char_key('a')]);

    drain_away(&mut desk.seat(), &mut MemoryInput::new(&[]), &mut keys, 0)
        .expect("in-memory sources do not fault");

    assert!(desk.shell.modifiers().shift);
    assert_eq!(queued_keys(&keys), 0, "the waking key reaches nothing");
}

/// Losing the seat part-way through any drain is reported as losing the
/// seat, not as a generic input fault.
#[test]
fn every_drain_reports_the_fault_its_channel_raised() {
    let mut desk = Desk::new((shell(), compositor()));
    let revoked = || MemoryInput::faulting(&[moved(10, 10)], Errno::SeatRevoked);

    assert_eq!(
        desk.wake(&mut revoked(), &mut keyboard(&[]), &mut Program::new()),
        Err(Errno::SeatRevoked)
    );
    assert_eq!(
        drain_away(&mut desk.seat(), &mut revoked(), &mut keyboard(&[]), 0),
        Err(Errno::SeatRevoked)
    );
    assert!(desk
        .lock
        .engage(("ann", "ann"), &desk.shell, &mut desk.comp));
    assert_eq!(
        drain_locked(
            &mut desk.seat(),
            &mut revoked(),
            &mut keyboard(&[]),
            &mut ScriptedUnlocker::refusing(),
            0,
        ),
        Err(Errno::SeatRevoked)
    );
}

/// A keyboard fault is reported the same way, from the lock's drain and from
/// the shell's own.
#[test]
fn a_keyboard_fault_is_reported_as_raised() {
    let faulting = || {
        KeyboardInputSource::new(
            Keys {
                records: VecDeque::new(),
                fault: Some(Errno::SeatNotOwner),
            },
            NO_REPEAT,
        )
    };
    let mut desk = Desk::new((shell(), compositor()));
    assert_eq!(
        desk.wake(
            &mut MemoryInput::new(&[]),
            &mut faulting(),
            &mut Program::new()
        ),
        Err(Errno::SeatNotOwner)
    );
    assert!(desk
        .lock
        .engage(("ann", "ann"), &desk.shell, &mut desk.comp));
    assert_eq!(
        drain_locked(
            &mut desk.seat(),
            &mut MemoryInput::new(&[]),
            &mut faulting(),
            &mut ScriptedUnlocker::refusing(),
            0,
        ),
        Err(Errno::SeatNotOwner)
    );
}

/// A chain's drain reports the fault too.
#[test]
fn a_chain_drain_reports_the_fault_its_channel_raised() {
    let (slot, _) = slot_and_row();
    let mut desk = Desk::new(bar_desktop());
    let mut program = Program::new();
    desk.wake(
        &mut MemoryInput::new(&open_menu(slot)),
        &mut keyboard(&[]),
        &mut program,
    )
    .expect("in-memory sources do not fault");
    assert!(desk.menu.is_open());

    let mut faulting = MemoryInput::faulting(&[], Errno::SeatNotOwner);
    assert_eq!(
        desk.wake(&mut faulting, &mut keyboard(&[]), &mut program),
        Err(Errno::SeatNotOwner)
    );
}

#[test]
fn a_logout_ends_the_wake_at_once() {
    let mut desk = Desk::new((shell(), compositor()));
    let mut keys = keyboard(&[char_key('q'), char_key('x')]);
    let mut program = Program::new();
    program.decide = |_| Routed::EndSession;

    let woke = desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program);

    assert_eq!(woke, Ok(SeatWake::EndSession));
    assert_eq!(queued_keys(&keys), 1, "nothing after the logout is applied");
}

#[test]
fn stepping_aside_applies_nothing_after_it() {
    let mut desk = Desk::new((shell(), compositor()));
    let mut keys = keyboard(&[char_key('s'), char_key('x')]);
    let mut program = Program::new();
    program.decide = |_| Routed::SwitchUser;
    program.switch_accepted = true;

    let woke = desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program);

    assert_eq!(woke, Ok(SeatWake::SteppedAside));
    assert_eq!(program.routed.len(), 1);
    assert_eq!(queued_keys(&keys), 1);
}

#[test]
fn a_refused_switch_keeps_serving() {
    let mut desk = Desk::new((shell(), compositor()));
    let mut keys = keyboard(&[char_key('s'), char_key('x')]);
    let mut program = Program::new();
    program.decide = |_| Routed::SwitchUser;

    let woke = desk.wake(&mut MemoryInput::new(&[]), &mut keys, &mut program);

    assert_eq!(woke, Ok(SeatWake::Served));
    assert_eq!(program.routed.len(), 2);
    assert_eq!(queued_keys(&keys), 0);
}
