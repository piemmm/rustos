//! The places / devices sidebar model: the shortcut rail the file manager
//! draws down the left edge of its window.
//!
//! Two kinds of row share one list. The **user's own places** — Home, Desktop,
//! Documents, and the machine's application and system roots — are fixed: they
//! are always offered, in one order, because they are where a session's work
//! lives. The **volumes** are whatever is mounted right now, learned from the
//! live mount table and each carrying the storage medium it actually sits on
//! (`tairix_icon::disk_icon` turns that medium into the shipped drive artwork),
//! so a USB stick never masquerades as an internal disk.
//!
//! # Data in, no I/O
//!
//! [`Places::new`] is pure: the caller supplies the home directory's path
//! components and the volumes it has already learned about, and gets back an
//! ordered, validated, deduplicated list. Nothing here opens, stats, or lists
//! a directory — the model cannot be made to touch the filesystem, so it is
//! host-testable and can never smuggle an unchecked read past the app's own
//! capability-checked seam.
//!
//! # Fail closed on malformed input
//!
//! A volume arrives from the mount table, which reports what the machine has
//! mounted — including labels and targets this process did not author. A
//! volume whose target does not parse as an absolute path, whose label is
//! empty, over-long, or carries control characters, or which lands on a target
//! an earlier row already covers, is **dropped**. It is never guessed at,
//! repaired, or shown as a row that would navigate somewhere else.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_icon::{disk_icon, IconKind};

use crate::vfs::components_from_absolute_path;

/// The longest volume label a sidebar row will accept, in bytes.
///
/// A fixed fail-closed validation bound on text this process did not author,
/// not a capacity that scales with the machine: a label longer than this is a
/// malformed record, so the row is dropped rather than truncated into
/// something that reads like a different volume. Comfortably longer than any
/// legitimate volume name while staying far below the row width a rail can
/// draw.
pub const MAX_PLACE_LABEL: usize = 64;

/// The leaf name of the user's desktop directory within their home.
const DESKTOP_DIR: &str = "Desktop";

/// The leaf name of the user's documents directory within their home.
const DOCUMENTS_DIR: &str = "Documents";

/// The machine-wide application store's root component.
const APPS_ROOT: &str = "Apps";

/// The OS-provided system tree's root component.
const SYSTEM_ROOT: &str = "System";

/// The longest label among the fixed user places.
///
/// The rail derives its width by measuring this in the theme's body face at
/// the desktop scale, so every fixed row's label fits without truncation at
/// any UI density. A volume whose label is longer simply truncates in its row,
/// as any over-long label does.
pub const WIDEST_FIXED_LABEL: &str = DOCUMENTS_DIR;

/// What one sidebar row is, and therefore how it was learned.
///
/// The kind is what separates the always-offered user places from the rows
/// that exist only because something is mounted: the rail draws its
/// separation where the first [`PlaceKind::Volume`] row begins, and a caller
/// refreshing the mount table replaces exactly the volume rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PlaceKind {
    /// The user's own home directory.
    Home,
    /// A fixed folder inside the user's home (Desktop, Documents).
    UserFolder,
    /// A machine-wide root (the application store, the system tree).
    SystemRoot,
    /// A mounted volume, learned from the live mount table.
    Volume,
}

/// One row of the places rail: what it is called, what it looks like, and
/// where activating it navigates to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Place {
    label: String,
    icon: IconKind,
    components: Vec<String>,
    kind: PlaceKind,
    available: bool,
}

impl Place {
    /// The row's display label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The icon the row draws: the shipped artwork for this kind when the
    /// system has it, and the built-in glyph otherwise.
    #[must_use]
    pub const fn icon(&self) -> IconKind {
        self.icon
    }

    /// The root-first path components activating the row navigates to.
    #[must_use]
    pub fn components(&self) -> &[String] {
        &self.components
    }

    /// What kind of place this row is.
    #[must_use]
    pub const fn kind(&self) -> PlaceKind {
        self.kind
    }

    /// Whether the row can still be navigated to.
    ///
    /// Every row starts available: this model performs no I/O, so it cannot
    /// know in advance whether a target is listable. A row is marked
    /// unavailable only once a navigation to it has actually been refused
    /// ([`Places::set_unavailable`]), so the rail shows a place whose
    /// directory has gone as disabled rather than silently doing nothing each
    /// time it is clicked.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        self.available
    }
}

/// One mounted volume offered to [`Places::new`].
///
/// The caller reads these from the live mount table; the model validates them
/// rather than trusting them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Volume {
    /// The volume's display label.
    pub label: String,
    /// The absolute path the volume is mounted at, in the ordinary path
    /// spelling. A target that does not parse drops the volume.
    pub target: String,
    /// The storage medium the volume's backing device sits on, or `None` when
    /// the mount reports no medium (a synthetic mount, or a class this build
    /// does not know). `None` draws the generic drive icon — never a guessed
    /// medium.
    pub medium: Option<BlkDeviceClass>,
}

/// The ordered rail: the user's fixed places, then every accepted volume.
///
/// The order is fixed and deterministic — Home, Desktop, Documents, the
/// application root, the system root, then the volumes sorted by label — so
/// the rail never reshuffles under the user between two paints of the same
/// state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Places {
    rows: Vec<Place>,
    cursor: usize,
    focused: bool,
    hovered: Option<usize>,
}

impl Places {
    /// Build the rail from the user's `home` path components and the
    /// `volumes` the caller has learned about.
    ///
    /// The fixed user places come first, in their one order. They are listed
    /// whether or not their directories exist: this model performs no I/O, so
    /// it cannot know, and a user's Desktop shortcut is expected to be there
    /// even on the day the directory is missing (navigating to it then fails
    /// closed and says so, which is honest — a silently absent shortcut is
    /// not). The three home-derived rows need a home to hang off, so an empty
    /// `home` drops them rather than spelling a row that navigates nowhere.
    ///
    /// The volumes follow, sorted by label (stably, so equal labels keep the
    /// caller's order) after every malformed and duplicate entry has been
    /// dropped. A volume is rejected when its label is empty, longer than
    /// [`MAX_PLACE_LABEL`], or carries a control character; when its target
    /// does not parse as an absolute path; or when an already-accepted row —
    /// fixed or volume — already navigates to that same target.
    #[must_use]
    pub fn new(home: &[String], volumes: &[Volume]) -> Self {
        let mut rows: Vec<Place> = Vec::new();
        if !home.is_empty() {
            rows.push(Place {
                label: String::from("Home"),
                icon: IconKind::Folder,
                components: home.to_vec(),
                kind: PlaceKind::Home,
                available: true,
            });
            for leaf in [DESKTOP_DIR, DOCUMENTS_DIR] {
                let mut components = home.to_vec();
                components.push(String::from(leaf));
                rows.push(Place {
                    label: String::from(leaf),
                    icon: IconKind::Folder,
                    components,
                    kind: PlaceKind::UserFolder,
                    available: true,
                });
            }
        }
        rows.push(Place {
            label: String::from(APPS_ROOT),
            icon: IconKind::Library,
            components: alloc::vec![String::from(APPS_ROOT)],
            kind: PlaceKind::SystemRoot,
            available: true,
        });
        rows.push(Place {
            label: String::from(SYSTEM_ROOT),
            icon: IconKind::Folder,
            components: alloc::vec![String::from(SYSTEM_ROOT)],
            kind: PlaceKind::SystemRoot,
            available: true,
        });

        let mut accepted: Vec<Place> = volumes.iter().filter_map(volume_row).collect();
        // Sorting before deduplicating makes the surviving row of a duplicated
        // target depend only on the set of volumes, never on the order the
        // mount table happened to page them out in.
        accepted.sort_by(|a, b| a.label.cmp(&b.label));
        for row in accepted {
            if rows.iter().any(|seen| seen.components == row.components) {
                continue;
            }
            rows.push(row);
        }
        Self {
            rows,
            cursor: 0,
            focused: false,
            hovered: None,
        }
    }

    /// Every row, in rail order.
    #[must_use]
    pub fn rows(&self) -> &[Place] {
        &self.rows
    }

    /// How many rows the rail has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the rail has no rows at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The index of the first volume row — where the rail draws its
    /// separation between the user's own places and the mounted volumes — or
    /// `None` when nothing is mounted (there is then nothing to separate).
    #[must_use]
    pub fn volume_start(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.kind == PlaceKind::Volume)
    }

    /// The row that navigates to exactly `components`, or `None` when no row
    /// does.
    ///
    /// This is how the rail shows the browser's current location as selected:
    /// an exact match only, so standing inside a subdirectory of a place
    /// highlights nothing rather than claiming the user is at the place
    /// itself.
    #[must_use]
    pub fn index_of(&self, components: &[String]) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.components == components)
    }

    /// The row the rail's keyboard cursor is on.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Put the keyboard cursor on `index`, ignoring an index the rail does not
    /// have.
    pub fn set_cursor(&mut self, index: usize) {
        if index < self.rows.len() {
            self.cursor = index;
        }
    }

    /// Step the keyboard cursor by `delta` rows, clamping at both ends, and
    /// report whether it moved.
    ///
    /// Clamping rather than wrapping keeps a held arrow key from cycling the
    /// rail endlessly, and it never leaves the cursor on a row that is not
    /// there.
    pub fn move_cursor(&mut self, delta: i32) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let last = self.rows.len().saturating_sub(1);
        let moved = i64::from(delta).saturating_add(i64::try_from(self.cursor).unwrap_or(i64::MAX));
        let next = usize::try_from(moved.max(0)).unwrap_or(last).min(last);
        let changed = next != self.cursor;
        self.cursor = next;
        changed
    }

    /// Whether the rail currently owns the keyboard focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Give the rail the keyboard focus, or take it away.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// The row the pointer is over, or `None` when it is elsewhere.
    #[must_use]
    pub const fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    /// Record the row the pointer is over, reporting whether it changed (so a
    /// caller repaints only when the highlight actually moves).
    pub fn set_hovered(&mut self, index: Option<usize>) -> bool {
        let index = index.filter(|&i| i < self.rows.len());
        let changed = index != self.hovered;
        self.hovered = index;
        changed
    }

    /// Mark the row at `index` as one that could not be navigated to, so it
    /// reads as disabled until the rail is rebuilt.
    ///
    /// A place is never *assumed* unavailable — this is only ever called after
    /// a real navigation was refused, so the rail reports what the filesystem
    /// actually said rather than a guess.
    pub fn set_unavailable(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(index) {
            row.available = false;
        }
    }
}

/// Validate one offered volume into a rail row, or `None` when it is
/// malformed.
fn volume_row(volume: &Volume) -> Option<Place> {
    if volume.label.is_empty() || volume.label.len() > MAX_PLACE_LABEL {
        return None;
    }
    if volume.label.chars().any(char::is_control) {
        return None;
    }
    // A target that does not start at the root is not a mount point: the
    // shared component parser tolerates a leading-slash-free spelling, so the
    // absoluteness is checked here rather than inferred.
    if !volume.target.starts_with('/') {
        return None;
    }
    let components = components_from_absolute_path(&volume.target).ok()?;
    Some(Place {
        label: volume.label.clone(),
        icon: disk_icon(volume.medium),
        components,
        kind: PlaceKind::Volume,
        available: true,
    })
}
