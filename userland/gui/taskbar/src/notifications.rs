//! The notification area: persistent status signals and transient
//! notifications, immediately before the clock and the reserved Switchboard
//! slot.
//!
//! The area holds two distinct things. The **status signals** are the
//! persistent tray glyphs (network, volume, battery); each names a
//! [`StatusKind`] the renderer draws as a calm glyph resolved from the loaded
//! `/System/Graphics` icon set. The **transient notifications** are the
//! short, severity-ranked messages a producer service raises and clears over
//! the notification IPC (`plans/NEW-TASKBAR.md` T8); the session relays each
//! into this model keyed to the producer's kernel-attested identity, and the
//! taskbar presents them as shared `lib/controls` notification cards.
//!
//! The model holds no authority and performs no I/O: the session owns the
//! live feed (status signals from the tray-signal feed; notifications from
//! the notification IPC) and hands it to the taskbar, exactly as it feeds the
//! application strip and the program library.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{BundleId, Errno, ProcId};
use tairix_icon::IconKind;

pub use tairix_abi::notify_ipc::NotifySeverity;

/// Most notifications the area holds at once.
///
/// A containment bound: a producer is any program of the user's, so an
/// unbounded area would let one grow the session without limit. A raise past
/// it is refused, never made room for by dropping another source's notice.
pub const NOTIFICATIONS_MAX: usize = 32;

/// Most notifications one source holds at once, so no one program can fill
/// the area and crowd every other out.
pub const SOURCE_NOTIFICATIONS_MAX: usize = 8;

/// Who raised a notification, as the kernel attested it at the raise.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Producer {
    /// The process instance, which alone may update or clear the notice: a
    /// recycled pid names a different instance.
    pub instance: ProcId,
    /// The pid the instance called as, by which a reaped child's notices are
    /// dropped.
    pub pid: u64,
    /// The signed bundle it runs, which is what the notification policy is
    /// keyed on.
    pub source: BundleId,
}

/// A stable identifier for a status signal, so the session can replace the
/// signal set without a glyph losing its identity.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct IconId(pub u64);

/// The kind of a persistent status signal, selecting the glyph it draws.
///
/// A closed set: a status signal names *what it is*, so the renderer resolves
/// the one right glyph and a later live feed can attach a reading — never a
/// free-form asset string. Adding a kind is a reviewed one-line change.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum StatusKind {
    /// Network connectivity.
    Network,
    /// Audio output volume.
    Volume,
    /// Battery charge.
    Battery,
}

impl StatusKind {
    /// The shared `lib/icon` glyph this kind draws.
    #[must_use]
    pub const fn icon(self) -> IconKind {
        match self {
            Self::Network => IconKind::Network,
            Self::Volume => IconKind::Volume,
            Self::Battery => IconKind::Battery,
        }
    }
}

/// One persistent status signal in the notification area.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSignal {
    /// The signal's stable id.
    pub id: IconId,
    /// What the signal reports (selects its glyph).
    pub kind: StatusKind,
}

impl StatusSignal {
    /// A status signal of `kind` identified by `id`.
    #[must_use]
    pub const fn new(id: IconId, kind: StatusKind) -> Self {
        Self { id, kind }
    }
}

/// A transient notification raised by a producer service.
///
/// Within its producer's instance the `key` names one notification, so a
/// later raise with the same `(producer, key)` updates it in place and a clear
/// removes exactly it. The `title`/`body` are producer-supplied display text
/// (already validated by the notification IPC decoder); they carry no
/// authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransientNotification {
    /// Who raised it.
    pub producer: Producer,
    /// The producer-chosen slot naming this notification within `producer`.
    pub key: u32,
    /// How prominently the notification is presented.
    pub severity: NotifySeverity,
    /// The one-line heading.
    pub title: String,
    /// The short body (may be empty for a title-only notification).
    pub body: String,
}

impl TransientNotification {
    /// A notification from its attested producer, key, severity, and text.
    #[must_use]
    pub fn new(
        producer: Producer,
        key: u32,
        severity: NotifySeverity,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            producer,
            key,
            severity,
            title: title.into(),
            body: body.into(),
        }
    }
}

/// The presentation rank of a severity: higher sorts ahead. `NotifySeverity`
/// is a wire enum with no ordering of its own, so the *display* precedence
/// lives here, beside the model that presents it.
const fn severity_rank(severity: NotifySeverity) -> u8 {
    match severity {
        NotifySeverity::Info => 0,
        NotifySeverity::Success => 1,
        NotifySeverity::Warning => 2,
        NotifySeverity::Critical => 3,
    }
}

/// One stored notification plus the recency sequence that orders it within
/// its severity.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Stored {
    seq: u64,
    note: TransientNotification,
}

/// The notification area's model: the persistent status signals and the
/// transient notifications, each fed by the session.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NotificationArea {
    signals: Vec<StatusSignal>,
    notifications: Vec<Stored>,
    next_seq: u64,
}

impl NotificationArea {
    /// An empty notification area.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            signals: Vec::new(),
            notifications: Vec::new(),
            next_seq: 0,
        }
    }

    // --- Status signals ------------------------------------------------

    /// The status signals in display order (leading to trailing).
    #[must_use]
    pub fn signals(&self) -> &[StatusSignal] {
        &self.signals
    }

    /// The number of status signals — the count the bar lays icon slots for.
    #[must_use]
    pub fn signal_count(&self) -> usize {
        self.signals.len()
    }

    /// Replace the status signals, dropping any later duplicate id (fail
    /// closed: a repeated id keeps its first, deterministic slot rather than
    /// drawing the same signal twice).
    pub fn set_signals(&mut self, signals: Vec<StatusSignal>) {
        let mut deduped: Vec<StatusSignal> = Vec::with_capacity(signals.len());
        for signal in signals {
            if deduped.iter().any(|kept| kept.id == signal.id) {
                continue;
            }
            deduped.push(signal);
        }
        self.signals = deduped;
    }

    // --- Transient notifications --------------------------------------

    /// The transient notifications in display order: highest severity first,
    /// then most recently raised.
    pub fn notifications(&self) -> impl Iterator<Item = &TransientNotification> + '_ {
        self.notifications.iter().map(|stored| &stored.note)
    }

    /// The transient notification at display `index`, if any.
    #[must_use]
    pub fn notification(&self, index: usize) -> Option<&TransientNotification> {
        self.notifications.get(index).map(|stored| &stored.note)
    }

    /// The number of transient notifications currently raised.
    #[must_use]
    pub fn notification_count(&self) -> usize {
        self.notifications.len()
    }

    /// Whether any transient notification is raised.
    #[must_use]
    pub fn has_notifications(&self) -> bool {
        !self.notifications.is_empty()
    }

    /// Raise a notification, or update it in place when one with the same
    /// `(producer, key)` is already showing (refreshing its recency).
    /// Answers whether anything changed — re-raising byte-identical content
    /// keeps its place and reports `false`.
    ///
    /// # Errors
    ///
    /// [`Errno::LimitExceeded`] for a new notification past
    /// [`NOTIFICATIONS_MAX`] or its source's [`SOURCE_NOTIFICATIONS_MAX`];
    /// nothing changes. An update in place is never refused.
    pub fn raise(&mut self, note: TransientNotification) -> Result<bool, Errno> {
        if let Some(pos) = self.position(note.producer.instance, note.key) {
            if self.notifications[pos].note == note {
                return Ok(false);
            }
            let seq = self.alloc_seq();
            self.notifications[pos].note = note;
            self.notifications[pos].seq = seq;
            self.sort();
            return Ok(true);
        }
        let held = self
            .notifications
            .iter()
            .filter(|stored| stored.note.producer.source == note.producer.source)
            .count();
        if self.notifications.len() >= NOTIFICATIONS_MAX || held >= SOURCE_NOTIFICATIONS_MAX {
            return Err(Errno::LimitExceeded);
        }
        let seq = self.alloc_seq();
        self.notifications.push(Stored { seq, note });
        self.sort();
        Ok(true)
    }

    /// Clear the notification identified by `(producer, key)`. Returns whether
    /// one was removed (idempotent: clearing an absent notification is a
    /// no-op, not an error).
    pub fn clear(&mut self, producer: ProcId, key: u32) -> bool {
        self.retain(|note| !(note.producer.instance == producer && note.key == key))
    }

    /// Clear every notification raised as `pid` — how the session drops a
    /// reaped child's notifications. Returns whether any were removed.
    ///
    /// By pid rather than instance because a reap names a pid; any notice
    /// under it is from that child or from an earlier holder of the pid, and
    /// both are gone.
    pub fn clear_pid(&mut self, pid: u64) -> bool {
        self.retain(|note| note.producer.pid != pid)
    }

    /// Keep only the notifications `keep` answers `true` for — how the session
    /// withdraws what a changed policy no longer admits. Returns whether any
    /// were removed.
    pub fn retain(&mut self, mut keep: impl FnMut(&TransientNotification) -> bool) -> bool {
        let before = self.notifications.len();
        self.notifications.retain(|stored| keep(&stored.note));
        self.notifications.len() != before
    }

    fn position(&self, producer: ProcId, key: u32) -> Option<usize> {
        self.notifications
            .iter()
            .position(|stored| stored.note.producer.instance == producer && stored.note.key == key)
    }

    /// Allocate the next recency sequence (saturating; never wraps).
    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Re-sort the notifications into display order: highest severity first,
    /// then most recently raised within a severity.
    fn sort(&mut self) {
        self.notifications.sort_by(|a, b| {
            severity_rank(b.note.severity)
                .cmp(&severity_rank(a.note.severity))
                .then_with(|| b.seq.cmp(&a.seq))
        });
    }
}
