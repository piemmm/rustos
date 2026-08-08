//! The live-session table: which accounts have a running desktop session,
//! and which one of them holds the seat.
//!
//! Fast user switching means several accounts may have a desktop running at
//! once while only one presents. The authority is the only component that
//! can know that — it started every one of them — so it keeps the record
//! here: one entry per account, created when its session starts and removed
//! when that session ends.
//!
//! The single invariant is that **at most one entry presents**. It is
//! enforced by construction rather than checked: promoting an entry demotes
//! whatever held the foreground, so no sequence of operations can produce
//! two.
//!
//! The table also feeds the login screen's `live` badge (through
//! [`LiveSessions::is_live`]) and the wake mailbox the authority resumes a
//! background session through
//! ([`LiveSession::wake_endpoint`]).
//!
//! [`end_live_sessions`] is the other end of that mailbox: the authority is
//! the only thing that can wake a background session, so when it exits it
//! ends them rather than leaving them running with nothing to reach them.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::session_ipc::{session_wake_endpoint, SessionWake};
use tairix_abi::Errno;
use tairix_log::{log, Event, Field, FieldValue, Level, Sink};

use crate::events;

/// Whether a live session currently presents.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SessionState {
    /// Holds the seat and presents. At most one session is in this state.
    Foreground,
    /// Runs, but presents nothing: its processes and windows are alive and
    /// the seat belongs to someone else.
    Background,
}

/// One account's live desktop session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveSession {
    login_name: String,
    uid: u32,
    pid: u64,
    state: SessionState,
}

impl LiveSession {
    /// The account this session belongs to.
    #[must_use]
    pub fn login_name(&self) -> &str {
        &self.login_name
    }

    /// The session process's kernel task id.
    #[must_use]
    pub const fn pid(&self) -> u64 {
        self.pid
    }

    /// The mailbox the authority posts this session's wake messages to.
    ///
    /// Derived from the pid rather than stored, so the id the authority
    /// sends to and the id the session bound cannot drift apart.
    #[must_use]
    pub const fn wake_endpoint(&self) -> u64 {
        session_wake_endpoint(self.pid)
    }
}

/// Every account with a live desktop session, in the order they started.
///
/// A growable list: how many accounts a seat's users leave running is not
/// something a compiled-in ceiling may decide.
#[derive(Debug, Default)]
pub struct LiveSessions {
    entries: Vec<LiveSession>,
}

impl LiveSessions {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Record a session that has just started for `login_name`, in the
    /// foreground; whatever held the foreground is demoted.
    ///
    /// # Errors
    ///
    /// [`Errno::AlreadyExists`] when the account already has a live
    /// session. The authority resumes an existing session rather than
    /// starting a second desktop for one account, so a duplicate is a
    /// caller defect and is refused rather than silently accepted.
    pub fn insert(&mut self, login_name: &str, uid: u32, pid: u64) -> Result<(), Errno> {
        if self.get(login_name).is_some() {
            return Err(Errno::AlreadyExists);
        }
        self.demote_all();
        self.entries.push(LiveSession {
            login_name: login_name.to_string(),
            uid,
            pid,
            state: SessionState::Foreground,
        });
        Ok(())
    }

    /// Promote `login_name`'s session to the foreground, demoting whatever
    /// held it. Reports whether the account had a live session.
    #[must_use]
    pub fn set_foreground(&mut self, login_name: &str) -> bool {
        let Some(index) = self.position(login_name) else {
            return false;
        };
        self.demote_all();
        self.entries[index].state = SessionState::Foreground;
        true
    }

    /// Step the session `uid` owns aside, leaving the seat free. Reports
    /// whether it did.
    ///
    /// `uid` is a caller's kernel-attested identity, so only the account
    /// actually holding the screen can give it up: a background session
    /// cannot demote the foreground one, and a stranger cannot demote
    /// anyone. The entry stays live and resumable — stepping aside never
    /// ends a session.
    #[must_use]
    pub fn background(&mut self, uid: u32) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.state == SessionState::Foreground && entry.uid == uid)
        else {
            return false;
        };
        entry.state = SessionState::Background;
        true
    }

    /// Remove `login_name`'s session — it ended — and return it.
    pub fn remove(&mut self, login_name: &str) -> Option<LiveSession> {
        let index = self.position(login_name)?;
        Some(self.entries.remove(index))
    }

    /// `login_name`'s live session, if it has one.
    #[must_use]
    pub fn get(&self, login_name: &str) -> Option<&LiveSession> {
        self.entries
            .iter()
            .find(|entry| entry.login_name == login_name)
    }

    /// Whether `login_name` has a live session — the login screen's `live`
    /// badge.
    #[must_use]
    pub fn is_live(&self, login_name: &str) -> bool {
        self.get(login_name).is_some()
    }

    /// Empty the table, returning every session newest first — the order
    /// shutdown ends them in, so a session never outlives one started
    /// after it.
    fn drain_newest_first(&mut self) -> Vec<LiveSession> {
        let mut drained = core::mem::take(&mut self.entries);
        drained.reverse();
        drained
    }

    fn position(&self, login_name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.login_name == login_name)
    }

    fn demote_all(&mut self) {
        for entry in &mut self.entries {
            entry.state = SessionState::Background;
        }
    }
}

/// Posts the authority's wake messages to a live session's mailbox.
///
/// An injected seam so the shutdown drain is host-testable; the `Run`
/// binary's implementation is one `ipc_send`. It is also the path a resume
/// takes, so both messages a session can receive are delivered the same way.
pub trait SessionWaker {
    /// Post `message` to `mailbox`, reporting whether it was delivered.
    fn wake(&self, mailbox: u64, message: SessionWake) -> bool;
}

/// Tell every session on the table to end, newest first, emptying it.
///
/// Called when the authority itself is going: its wake mailboxes die with
/// it, and the `login` PID 1 relaunches starts with an empty table, so a
/// session left recorded as background would hold no seat, have nothing that
/// could ever wake it, and never be resumable again. Newest first, so a
/// session never outlives one started after it.
///
/// Every message is audited with [`events::SESSION_ENDED_ON_EXIT`]. An
/// undeliverable wake is recorded and skipped — never retried, never waited
/// on — so one wedged session cannot hold the exit open.
pub fn end_live_sessions(live: &mut LiveSessions, waker: &dyn SessionWaker, sink: &dyn Sink) {
    for session in live.drain_newest_first() {
        let (level, message) = if waker.wake(session.wake_endpoint(), SessionWake::End) {
            (
                Level::Info,
                "live desktop session told to end; the authority is exiting",
            )
        } else {
            (Level::Warn, "live desktop session could not be told to end")
        };
        log(
            sink,
            &Event {
                level,
                id: events::SESSION_ENDED_ON_EXIT,
                message,
                fields: &[Field {
                    key: "user",
                    value: FieldValue::Str(session.login_name()),
                }],
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{end_live_sessions, LiveSession, LiveSessions, SessionState, SessionWaker};
    use crate::events;
    use alloc::vec::Vec;
    use core::cell::RefCell;
    use tairix_abi::session_ipc::{session_wake_endpoint, SessionWake};
    use tairix_abi::Errno;
    use tairix_log::{Event, EventId, Sink};

    /// The invariant every test asserts after every mutation.
    fn at_most_one_foreground(table: &LiveSessions) {
        let claimants = table
            .entries
            .iter()
            .filter(|entry| entry.state == SessionState::Foreground)
            .count();
        assert!(claimants <= 1, "{claimants} sessions claim the seat");
    }

    /// The account presenting, read straight off the entries the table keeps
    /// private — nothing in production needs to ask.
    fn presenting(table: &LiveSessions) -> Option<&str> {
        table
            .entries
            .iter()
            .find(|entry| entry.state == SessionState::Foreground)
            .map(LiveSession::login_name)
    }

    fn state_of(table: &LiveSessions, login_name: &str) -> Option<SessionState> {
        table.get(login_name).map(|entry| entry.state)
    }

    #[test]
    fn a_started_session_takes_the_foreground() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        at_most_one_foreground(&table);
        let session = table.get("ada").expect("inserted");
        assert_eq!(session.pid(), 42);
        assert_eq!(session.wake_endpoint(), session_wake_endpoint(42));
        assert_eq!(state_of(&table, "ada"), Some(SessionState::Foreground));
        assert!(table.is_live("ada"));
        assert_eq!(table.entries.len(), 1);
    }

    #[test]
    fn a_second_session_for_the_same_account_is_refused() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        assert_eq!(table.insert("ada", 1000, 43), Err(Errno::AlreadyExists));
        assert_eq!(table.entries.len(), 1);
        assert_eq!(table.get("ada").map(LiveSession::pid), Some(42));
    }

    #[test]
    fn starting_a_second_account_demotes_the_first() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        table.insert("grace", 1001, 43).expect("a fresh account");
        at_most_one_foreground(&table);
        assert_eq!(state_of(&table, "ada"), Some(SessionState::Background));
        assert_eq!(presenting(&table), Some("grace"));
    }

    #[test]
    fn two_accounts_alternate_the_foreground() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        table.insert("grace", 1001, 43).expect("a fresh account");
        for expected in ["ada", "grace", "ada", "grace"] {
            assert!(table.set_foreground(expected));
            at_most_one_foreground(&table);
            assert_eq!(presenting(&table), Some(expected));
        }
        // Both are still live throughout: switching never ends a session.
        assert_eq!(table.entries.len(), 2);
    }

    #[test]
    fn switching_away_leaves_the_seat_free_and_the_session_live() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        assert!(table.background(1000));
        at_most_one_foreground(&table);
        assert!(presenting(&table).is_none());
        assert!(table.is_live("ada"));
        assert_eq!(table.get("ada").map(LiveSession::pid), Some(42));
        // And switching back restores it, same session.
        assert!(table.set_foreground("ada"));
        assert_eq!(presenting(&table), Some("ada"));
        assert_eq!(table.entries.len(), 1);
    }

    #[test]
    fn only_the_uid_holding_the_screen_may_step_aside() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        table.insert("grace", 1001, 43).expect("a fresh account");
        // `grace` presents; `ada` is behind it and cannot take the screen
        // away, nor can a stranger.
        assert!(!table.background(1000));
        assert!(!table.background(9999));
        assert_eq!(presenting(&table), Some("grace"));
        assert!(table.background(1001));
        assert!(presenting(&table).is_none());
        // With nothing presenting, nobody may step aside — including the
        // account that just did.
        assert!(!table.background(1001));
        at_most_one_foreground(&table);
    }

    #[test]
    fn stepping_aside_from_an_empty_table_changes_nothing() {
        let mut table = LiveSessions::new();
        assert!(!table.background(1000));
        assert!(presenting(&table).is_none());
    }

    #[test]
    fn logging_out_removes_the_entry() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        let ended = table.remove("ada").expect("was live");
        assert_eq!(ended.login_name(), "ada");
        assert!(!table.is_live("ada"));
        assert!(presenting(&table).is_none());
        assert!(table.entries.is_empty());
        // A second removal is simply nothing to do.
        assert!(table.remove("ada").is_none());
    }

    #[test]
    fn a_background_session_that_dies_is_removed_without_disturbing_the_foreground() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        table.insert("grace", 1001, 43).expect("a fresh account");
        assert_eq!(state_of(&table, "ada"), Some(SessionState::Background));
        let ended = table.remove("ada").expect("was live");
        assert_eq!(ended.login_name(), "ada");
        at_most_one_foreground(&table);
        assert_eq!(presenting(&table), Some("grace"));
    }

    #[test]
    fn an_unknown_account_cannot_be_promoted() {
        let mut table = LiveSessions::new();
        table.insert("ada", 1000, 42).expect("a fresh account");
        assert!(!table.set_foreground("mallory"));
        at_most_one_foreground(&table);
        assert_eq!(presenting(&table), Some("ada"));
    }

    /// A waker recording every posted message, optionally refusing the
    /// mailbox of one session that cannot be reached.
    struct MockWaker {
        posted: RefCell<Vec<(u64, SessionWake)>>,
        undeliverable: Option<u64>,
    }

    impl MockWaker {
        fn new(undeliverable: Option<u64>) -> Self {
            Self {
                posted: RefCell::new(Vec::new()),
                undeliverable,
            }
        }
    }

    impl SessionWaker for MockWaker {
        fn wake(&self, mailbox: u64, message: SessionWake) -> bool {
            self.posted.borrow_mut().push((mailbox, message));
            self.undeliverable != Some(mailbox)
        }
    }

    #[derive(Default)]
    struct CountingSink {
        seen: RefCell<Vec<EventId>>,
    }

    impl Sink for CountingSink {
        fn write_event(&self, event: &Event<'_>) {
            self.seen.borrow_mut().push(event.id);
        }
    }

    fn three_sessions() -> LiveSessions {
        let mut table = LiveSessions::new();
        for (index, name) in ["ada", "grace", "linus"].iter().enumerate() {
            let index = u32::try_from(index).expect("three accounts");
            table
                .insert(name, 1000 + index, u64::from(42 + index))
                .expect("a fresh account");
        }
        table
    }

    #[test]
    fn the_exit_drain_ends_every_session_newest_first() {
        let mut table = three_sessions();
        let waker = MockWaker::new(None);
        let sink = CountingSink::default();
        end_live_sessions(&mut table, &waker, &sink);
        assert_eq!(
            *waker.posted.borrow(),
            [
                (session_wake_endpoint(44), SessionWake::End),
                (session_wake_endpoint(43), SessionWake::End),
                (session_wake_endpoint(42), SessionWake::End),
            ]
        );
        assert!(table.entries.is_empty());
        assert_eq!(
            *sink.seen.borrow(),
            [events::SESSION_ENDED_ON_EXIT; 3],
            "every session ended is audited"
        );
    }

    #[test]
    fn an_undeliverable_wake_is_audited_and_the_drain_carries_on() {
        let mut table = three_sessions();
        // The middle session's mailbox refuses the message.
        let waker = MockWaker::new(Some(session_wake_endpoint(43)));
        let sink = CountingSink::default();
        end_live_sessions(&mut table, &waker, &sink);
        assert_eq!(waker.posted.borrow().len(), 3, "one attempt each, no retry");
        assert_eq!(sink.seen.borrow().len(), 3);
        assert!(table.entries.is_empty());
    }

    #[test]
    fn the_exit_drain_of_an_empty_table_does_nothing() {
        let mut table = LiveSessions::new();
        let waker = MockWaker::new(None);
        let sink = CountingSink::default();
        end_live_sessions(&mut table, &waker, &sink);
        assert!(waker.posted.borrow().is_empty());
        assert!(sink.seen.borrow().is_empty());
    }
}
