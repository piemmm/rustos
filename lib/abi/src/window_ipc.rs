//! The window-channel IPC protocol (`plans/APPWIN.md` AW2): the reserved
//! rendezvous the desktop session binds and the fixed-width, fail-closed
//! requests an application presents its windows through.
//!
//! The transport reuses the display-service zero-copy shape: an app
//! `shm_create`s a region holding its window frames, hands the session the
//! endpoint-directed `shm_grant` handle once (`Create`), and thereafter
//! presents by **frame index** plus a damage rectangle (`Present`) — no
//! pixel bytes ever cross the IPC. The session keys every window to the
//! kernel-attested identity of the task that created it (`call_peer_origin`),
//! never to anything claimed on the wire, so one app can never present or
//! close another's window.
//!
//! Input travels the other way: the session encodes each routed event as a
//! fixed-width [`WindowEvent`] and sends it to the owning app's own event
//! endpoint (named in `Create`), where the app parks until one arrives.
//! Events are advisory data about the app's own windows; they carry no
//! ambient authority and no secret. The one authority-adjacent field — the
//! [`WindowEvent::FilePicked`] delegation handle — is owner-bound
//! kernel-side (it redeems only when presented by the task it was minted
//! to, `fd_redeem`), so the number is useless to any observer or forger;
//! an app still accepts events only from the session identity the create
//! reply named.
//!
//! Requests are the fixed-width [`WindowRequest`]. `Create` answers with
//! the [`WINDOW_CREATE_REPLY_LEN`]-byte window-id reply
//! ([`encode_create_reply`] / [`decode_create_reply`]); `Present` and
//! `Close` answer with the shared status frame
//! ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]). Every decode fails closed: an
//! unknown magic, version, operation, format, an out-of-bounds frame
//! count, an empty damage rectangle, a malformed title, a minimum client
//! size declared by a window that cannot be resized, a request frame that
//! is not exactly as long as its operation, or a dirty reserved field
//! refuses rather than guessing.

use core::cmp::Ordering;

use crate::bounded_text::BoundedText;
use crate::desktop::DesktopInfo;
use crate::driver::display::{DamageRect, DisplayFormat};
use crate::input::KeyInput;
use crate::input::Modifiers;
use crate::input::PointerButtonCode;
use crate::le::{put_i32, put_u16, put_u32, put_u64, read_i32, read_u16, read_u32, read_u64};
use crate::{Errno, ProcId};

/// Reserved well-known call-endpoint id of the desktop session's window
/// service (`"WI"` ASCII hex-spelled prefix, mirroring
/// [`crate::seat::SEATMGR_ENDPOINT`]'s convention). Like the
/// notification and Switchboard tray-summary rendezvous it is
/// **seat-scoped** ([`crate::ipc::is_reserved_endpoint`],
/// [`crate::ipc::is_seat_scoped_endpoint`]): the kernel authorises its
/// bind either by `CAP_IPC_BIND_PRIVILEGED`
/// or by the caller's kernel-attested **live seat lease** — the desktop
/// session that owns the seat serves the windows shown on it, and
/// nothing else may. A squatter claiming the rendezvous first would
/// receive every app's shared-surface grants and could feed apps
/// fabricated input events, so an unentitled bind fails closed.
pub const WINDOW_ENDPOINT: u64 = 0x5749_1001;

/// Magic number identifying a window-channel request (`"WIN1"`
/// little-endian).
pub const WINDOW_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"WIN1");

/// Magic number identifying a window-channel event (`"WEV1"`
/// little-endian).
pub const WINDOW_EVENT_MAGIC: u32 = u32::from_le_bytes(*b"WEV1");

/// The `window-v1` protocol version.
pub const WINDOW_VERSION_V1: u16 = 1;

/// Most frames one `Create` may lay out in its shared region. A validation
/// bound, not a capacity: two frames are the double-buffer steady state,
/// and anything beyond four buys no latency while letting a hostile app
/// reserve unbounded pinned memory. Deliberately its own constant — the
/// display protocol's bound merely coincides today and the two may
/// diverge.
pub const WINDOW_MAX_FRAMES: u32 = 4;

/// Maximum request, in bytes, the [`WINDOW_ENDPOINT`] accepts: the longest
/// [`WindowRequest`] any operation encodes to.
///
/// A ceiling for the receive buffer, not the shape of a request — each
/// operation sends only its own [`WindowRequest::wire_len`] bytes.
pub const WINDOW_MAX_REQUEST: usize = WindowRequest::MAX_WIRE_LEN;

/// Maximum encoded length, in bytes, of a window title.
pub const WINDOW_TITLE_MAX: usize = 64;

/// Widest backdrop-blur radius a window may request, in **logical** pixels
/// ([`WindowRequest::SetBackdropBlur`]).
///
/// This is a validation bound on the compositor's per-frame work, not a
/// growable capacity: the blur cost of a window is already proportional to
/// its own area regardless of radius (a separable box blur with a running
/// sum), but a larger radius still widens the initial window-sum build at
/// each backdrop's edges and the physical radius after the desktop's UI
/// scale is applied, so a client cannot ask for an unbounded one.
pub const WINDOW_BACKDROP_BLUR_MAX_PX: u16 = 64;

/// Most rows one **plate** of a menu may hold ([`AppMenu`]).
///
/// A **format** bound, not a capacity: a plate is one column the desktop
/// draws in full, and the longest column a real menu offers runs to around
/// twenty rows, so thirty-two is generous for what a plate *is* while staying
/// a bound a hostile client cannot widen. The whole menu has its own bound
/// ([`APP_MENU_MAX_TOTAL_ROWS`]).
pub const APP_MENU_MAX_ROWS: usize = 32;

/// Most rows one whole menu may hold, across every plate of its chain
/// ([`AppMenu`]).
///
/// A **format** bound. It is its own bound rather than the product of
/// [`APP_MENU_MAX_ROWS`] and [`APP_MENU_MAX_DEPTH`], which is not a bound at
/// all: it is what holds the one frame a whole menu crosses in, and so the
/// receive ceiling every window client's buffer is sized to
/// ([`WINDOW_MAX_REQUEST`]). Twice the per-plate bound, so a plate can be
/// filled without exhausting the menu and the per-plate bound still bites.
pub const APP_MENU_MAX_TOTAL_ROWS: usize = 64;

/// Deepest chain of plates a menu may describe: the root plate is depth 1,
/// so four permits a root and three levels of submenu beneath it.
///
/// A **format** bound on the shape the parent index already expresses, so
/// nesting costs no encoding. A submenu on the deepest plate is refused
/// rather than drawn opening nothing.
pub const APP_MENU_MAX_DEPTH: usize = 4;

/// Bytes of row text one whole menu may carry, across every row and every
/// text field ([`AppMenu`]).
///
/// A **format** bound on the total size of the model, which is what keeps a
/// menu's frame — and the model held in memory — bounded without paying the
/// widest label, shortcut and reason for every row a menu does not have.
/// Enough for every row of a full menu to carry a twenty-four byte label, or
/// for fewer rows to carry the widest of all three fields.
pub const APP_MENU_TEXT_BYTES: usize = 1536;

/// Maximum encoded length, in bytes, of one menu row's label.
pub const APP_MENU_LABEL_MAX: usize = 36;

/// Maximum encoded length, in bytes, of one menu row's accelerator caption
/// (`"Ctrl Shift K"`).
///
/// A caption naming a key combination, not a sentence: the longest the
/// desktop states runs to a dozen bytes.
pub const APP_MENU_SHORTCUT_MAX: usize = 24;

/// Maximum encoded length, in bytes, of the reason a disabled menu row
/// states.
///
/// Shown in place of the row's accelerator while it is disabled and current,
/// so the bound is a clause rather than a sentence.
pub const APP_MENU_REASON_MAX: usize = 64;

/// A validated menu row label: bounded UTF-8 with no control characters,
/// over the shared [`BoundedText`] validator.
///
/// A label crosses a trust boundary into session-drawn chrome and carries
/// no authority — it is a name, not a credential. An empty label is
/// admissible in the type and refused per row kind: only a row that draws
/// text requires one ([`AppMenuRow`]). A menu's own title
/// ([`AppMenu::titled`]) is bounded and validated identically, for the same
/// reason.
pub type AppMenuLabel = BoundedText<0, APP_MENU_LABEL_MAX>;

/// A validated menu row accelerator caption, over the shared validator.
///
/// Display text like a label: the desktop draws it and never acts on it, so a
/// caption naming a key the application does not handle is that application's
/// own cosmetic defect, never an input path.
pub type AppMenuShortcut = BoundedText<0, APP_MENU_SHORTCUT_MAX>;

/// A validated disabled-row reason, over the shared validator.
pub type AppMenuReason = BoundedText<0, APP_MENU_REASON_MAX>;

/// The mark an [`AppMenuItem`] draws beside its label.
///
/// Closed, and an unknown discriminant on decode fails closed rather than
/// being guessed at.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum AppMenuMark {
    /// No mark.
    #[default]
    None = 0,
    /// A tick: an independent setting this row turns on.
    Check = 1,
    /// A filled bullet: the chosen member of a group of alternatives.
    Radio = 2,
}

impl AppMenuMark {
    /// Recover a mark from its wire discriminant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an unknown value (fail closed — never
    /// guess an unrecognised mark).
    const fn from_u8(raw: u8) -> Result<Self, Errno> {
        match raw {
            0 => Ok(Self::None),
            1 => Ok(Self::Check),
            2 => Ok(Self::Radio),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// How prominently an [`AppMenuItem`] draws: the emphasis the desktop gives
/// the row, not an authority it grants.
///
/// A destructive row wears a danger rail so an action that is hard to undo
/// reads as one before it is chosen. The emphasis is cosmetic — marking a
/// row destructive neither adds nor withholds any authority — and the set is
/// deliberately the two a menu row actually distinguishes.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum AppMenuRole {
    /// An ordinary action with no special emphasis.
    #[default]
    Neutral,
    /// An action that destroys data or is otherwise hard to undo.
    Destructive,
}

/// An [`AppMenuItem`]'s application-chosen id: any non-zero `u16`.
///
/// Zero is reserved so a decoded outcome can never be confused with an
/// absent id.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AppMenuItemId(u16);

impl AppMenuItemId {
    /// Build an item id.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `raw` is zero, which names no row.
    pub const fn new(raw: u16) -> Result<Self, Errno> {
        if raw == 0 {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// The raw id.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The id numbering position `index` of a menu builder's own command
    /// list, or `None` for an index no id can number.
    ///
    /// One-based, because an id is never zero. Every menu whose rows come
    /// from an ordered command list numbers them this way — the file
    /// manager's context menu, the desktop backdrop's, the icon bar's — so
    /// the numbering and its inverse ([`index`](Self::index)) are one rule
    /// rather than a copy per builder. Numbering the *command's* position
    /// rather than the row's on the plate is what lets a menu leave a row out
    /// without shifting any other row's meaning.
    #[must_use]
    pub fn for_index(index: usize) -> Option<Self> {
        Some(Self(u16::try_from(index).ok()?.checked_add(1)?))
    }

    /// The command-list position this id numbers.
    #[must_use]
    pub fn index(self) -> usize {
        // The id is one-based and never zero, so the subtraction is total.
        usize::from(self.0) - 1
    }
}

/// A chooseable menu row, as an application builds it.
///
/// Built rather than spelled field by field, because a row states one
/// mandatory thing — the id an outcome names and the label the user reads —
/// and everything else is emphasis the desktop draws. Read a built row back
/// through [`AppMenuItemView`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AppMenuItem {
    id: AppMenuItemId,
    label: AppMenuLabel,
    enabled: bool,
    mark: AppMenuMark,
    shortcut: AppMenuShortcut,
    reason: AppMenuReason,
    role: AppMenuRole,
}

impl AppMenuItem {
    /// An enabled, unmarked, neutral row with no accelerator.
    #[must_use]
    pub const fn new(id: AppMenuItemId, label: AppMenuLabel) -> Self {
        Self {
            id,
            label,
            enabled: true,
            mark: AppMenuMark::None,
            shortcut: AppMenuShortcut::EMPTY,
            reason: AppMenuReason::EMPTY,
            role: AppMenuRole::Neutral,
        }
    }

    /// This row greyed and unchooseable. A disabled row never acts.
    #[must_use]
    pub const fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// This row drawing `mark` beside its label.
    #[must_use]
    pub const fn with_mark(mut self, mark: AppMenuMark) -> Self {
        self.mark = mark;
        self
    }

    /// This row stating `shortcut` as its accelerator caption.
    #[must_use]
    pub const fn with_shortcut(mut self, shortcut: AppMenuShortcut) -> Self {
        self.shortcut = shortcut;
        self
    }

    /// This row stating why it cannot be chosen, shown while it is disabled
    /// and current.
    #[must_use]
    pub const fn with_reason(mut self, reason: AppMenuReason) -> Self {
        self.reason = reason;
        self
    }

    /// This row drawn with `role`'s emphasis.
    #[must_use]
    pub const fn with_role(mut self, role: AppMenuRole) -> Self {
        self.role = role;
        self
    }
}

/// One row of a menu, as an application builds it.
///
/// The row kinds are exactly what a menu is made of, so a row that cannot
/// mean anything is unrepresentable: only an [`Item`](Self::Item) carries an
/// id, and only the session-rendered [`Info`](Self::Info) row has content
/// the application does not describe.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppMenuRow {
    /// A chooseable row. Choosing it delivers
    /// [`WindowEvent::AppBarMenu`] carrying its id to the declaring
    /// application, which decides what it means — the session never
    /// interprets an id.
    Item(AppMenuItem),
    /// A horizontal rule grouping the rows around it. Never chooseable.
    Separator,
    /// A row that opens a plate holding the rows that name it as their
    /// parent. Never chooseable itself.
    Submenu {
        /// The row's label, which is also the plate's title.
        label: AppMenuLabel,
        /// Whether the submenu opens. A disabled submenu draws greyed and
        /// never opens.
        enabled: bool,
    },
    /// The application-information row, whose child is the session's own
    /// info panel, drawn from the **signed manifest** of the bundle the
    /// kernel attested owns the declaring process.
    ///
    /// The application declares only that the row exists; it supplies none
    /// of the panel's text, so it cannot state an identity that is not its
    /// own. At most one such row per menu, always at the top level.
    Info,
}

/// A chooseable row as a built menu reports it: the row's own text borrowed
/// from the menu that holds it, so reading a menu copies nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AppMenuItemView<'a> {
    /// The application's own id for this row.
    pub id: AppMenuItemId,
    /// The row's label.
    pub label: &'a str,
    /// Whether the row may be chosen.
    pub enabled: bool,
    /// The mark drawn beside the label.
    pub mark: AppMenuMark,
    /// The accelerator caption, empty when the row states none.
    pub shortcut: &'a str,
    /// Why the row cannot be chosen, empty when it states no reason.
    pub reason: &'a str,
    /// The emphasis the row draws with.
    pub role: AppMenuRole,
}

/// One row of a built menu, with its text borrowed from the menu
/// ([`AppMenu::rows`]).
///
/// The reading half of [`AppMenuRow`]: the same four kinds, holding `&str`
/// where the built row holds a validated bounded field, because a menu keeps
/// every row's text in one block rather than a widest-case buffer per row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppMenuRowView<'a> {
    /// A chooseable row.
    Item(AppMenuItemView<'a>),
    /// A horizontal rule grouping the rows around it.
    Separator,
    /// A row that opens the plate holding its children.
    Submenu {
        /// The row's label, which is also the plate's title.
        label: &'a str,
        /// Whether the submenu opens.
        enabled: bool,
    },
    /// The application-information row.
    Info,
}

/// Which of the four kinds a stored row is, carrying the id the chooseable
/// one has.
///
/// Held as this closed kind rather than the wire byte, so reporting a stored
/// row back is total: there is no kind whose id has to be reconstructed, and
/// no impossible discriminant to guess at.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RowKind {
    Item(AppMenuItemId),
    Separator,
    Submenu,
    Info,
}

impl RowKind {
    /// The wire discriminant of this kind.
    const fn wire(self) -> u8 {
        match self {
            Self::Item(_) => APP_MENU_KIND_ITEM,
            Self::Separator => APP_MENU_KIND_SEPARATOR,
            Self::Submenu => APP_MENU_KIND_SUBMENU,
            Self::Info => APP_MENU_KIND_INFO,
        }
    }

    /// The id this kind states, zero for the kinds that state none.
    const fn wire_id(self) -> u16 {
        match self {
            Self::Item(id) => id.get(),
            Self::Separator | Self::Submenu | Self::Info => 0,
        }
    }
}

/// One row as a menu stores it: everything about the row but its text,
/// which lies in the menu's own text block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct RowRecord {
    kind: RowKind,
    enabled: bool,
    mark: AppMenuMark,
    role: AppMenuRole,
    parent: u8,
    /// Where this row's label, shortcut and reason lie, in that order,
    /// within the menu's text block. Follows from the rows before it, so it
    /// is stored but never encoded.
    text_at: u16,
    label_len: u8,
    shortcut_len: u8,
    reason_len: u8,
}

impl RowRecord {
    /// The record an unfilled slot holds.
    const VACANT: Self = Self {
        kind: RowKind::Separator,
        enabled: false,
        mark: AppMenuMark::None,
        role: AppMenuRole::Neutral,
        parent: PARENT_NONE,
        text_at: 0,
        label_len: 0,
        shortcut_len: 0,
        reason_len: 0,
    };
}

/// An application's menu: an ordered, bounded list of rows, each either on
/// the root plate or inside the plate an earlier [`AppMenuRow::Submenu`]
/// opens.
///
/// Every row's text — label, accelerator caption and disabled-row reason —
/// lives in one bounded text block rather than a widest-case buffer per row,
/// so the model's size is what its rows actually say
/// ([`APP_MENU_TEXT_BYTES`]) and not the product of its bounds. Build with
/// [`push`](Self::push) / [`push_under`](Self::push_under) and read back
/// with [`rows`](Self::rows).
///
/// A menu's shape is checked as it is built, so building one cannot produce
/// a shape the session would have to re-reject: a row's parent names an
/// earlier submenu row, a plate holds at most [`APP_MENU_MAX_ROWS`] rows, a
/// chain runs at most [`APP_MENU_MAX_DEPTH`] levels deep, and no two rows
/// state the same id.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AppMenu {
    rows: [RowRecord; APP_MENU_MAX_TOTAL_ROWS],
    text: [u8; APP_MENU_TEXT_BYTES],
    title: AppMenuLabel,
    text_len: u16,
    len: u8,
}

impl AppMenu {
    /// The empty, untitled menu: an application that offers no menu at all,
    /// which is a legitimate declaration (a secondary press on its icon-bar
    /// slot then opens nothing).
    pub const EMPTY: Self = Self {
        rows: [RowRecord::VACANT; APP_MENU_MAX_TOTAL_ROWS],
        text: [0; APP_MENU_TEXT_BYTES],
        title: AppMenuLabel::EMPTY,
        text_len: 0,
        len: 0,
    };

    /// The empty menu with `title` on its root plate's band.
    ///
    /// A title is the application's own name for *this* menu, bounded and
    /// validated exactly as a row label is. It is not how the icon-bar menu
    /// is titled: that one is titled from the bundle's signed manifest, so a
    /// menu can never be titled as an application it is not, and a titled
    /// menu is refused where a declaration carries it
    /// ([`WindowRequest::SetAppBar`]). A plate opened by a submenu row takes
    /// its title from that row's label and states none of its own.
    #[must_use]
    pub const fn titled(title: AppMenuLabel) -> Self {
        Self {
            rows: [RowRecord::VACANT; APP_MENU_MAX_TOTAL_ROWS],
            text: [0; APP_MENU_TEXT_BYTES],
            title,
            text_len: 0,
            len: 0,
        }
    }

    /// The menu's own title, empty when the surface that draws it supplies
    /// one.
    #[must_use]
    pub fn title(&self) -> &str {
        self.title.as_str()
    }

    /// Append `row` to the root plate.
    ///
    /// # Errors
    ///
    /// * [`Errno::NoSpace`] — the menu already holds
    ///   [`APP_MENU_MAX_TOTAL_ROWS`] rows, the root plate already holds
    ///   [`APP_MENU_MAX_ROWS`], or the row's text does not fit
    ///   [`APP_MENU_TEXT_BYTES`].
    /// * [`Errno::OutOfRange`] — the row cannot stand at the top level, or
    ///   duplicates something the menu already holds (a second
    ///   [`AppMenuRow::Info`], an id an earlier row already states, a
    ///   labelled row with no label).
    pub fn push(&mut self, row: AppMenuRow) -> Result<(), Errno> {
        self.push_row(row, None)
    }

    /// Append `row` inside the plate opened by the row at `parent` (0-based,
    /// as [`Self::rows`] reports it).
    ///
    /// # Errors
    ///
    /// As [`Self::push`], plus [`Errno::OutOfRange`] when `parent` does not
    /// name an earlier [`AppMenuRow::Submenu`], when `row` is an
    /// [`AppMenuRow::Info`] row (which is always top-level), or when `row`
    /// would open a plate past [`APP_MENU_MAX_DEPTH`].
    pub fn push_under(&mut self, row: AppMenuRow, parent: usize) -> Result<(), Errno> {
        let parent = u8::try_from(parent).map_err(|_| Errno::OutOfRange)?;
        if parent == PARENT_NONE {
            return Err(Errno::OutOfRange);
        }
        self.push_row(row, Some(parent))
    }

    /// The rows in declaration order, each with the 0-based index of the
    /// submenu row whose plate it is on, or `None` on the root plate.
    pub fn rows(&self) -> impl Iterator<Item = (AppMenuRowView<'_>, Option<usize>)> + '_ {
        self.rows[..usize::from(self.len)].iter().map(|record| {
            (
                self.view(record),
                (record.parent != PARENT_NONE).then_some(usize::from(record.parent)),
            )
        })
    }

    /// The number of declared rows, across every plate.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the menu declares no rows at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The stored row at `record` as a borrowed view.
    fn view(&self, record: &RowRecord) -> AppMenuRowView<'_> {
        let at = usize::from(record.text_at);
        let label = self.text_field(at, record.label_len);
        let shortcut_at = at.saturating_add(usize::from(record.label_len));
        let shortcut = self.text_field(shortcut_at, record.shortcut_len);
        let reason_at = shortcut_at.saturating_add(usize::from(record.shortcut_len));
        match record.kind {
            RowKind::Item(id) => AppMenuRowView::Item(AppMenuItemView {
                id,
                label,
                enabled: record.enabled,
                mark: record.mark,
                shortcut,
                reason: self.text_field(reason_at, record.reason_len),
                role: record.role,
            }),
            RowKind::Separator => AppMenuRowView::Separator,
            RowKind::Submenu => AppMenuRowView::Submenu {
                label,
                enabled: record.enabled,
            },
            RowKind::Info => AppMenuRowView::Info,
        }
    }

    /// `len` bytes of the text block from `at`.
    fn text_field(&self, at: usize, len: u8) -> &str {
        let end = at.saturating_add(usize::from(len));
        // The block holds only text a validated field wrote, sliced back at
        // the length that field reported; an impossible failure yields the
        // empty string, never a panic.
        self.text
            .get(at..end)
            .and_then(|bytes| core::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    /// The shared append: validate `row` against the rows already held,
    /// intern its text, and record it with its parent.
    fn push_row(&mut self, row: AppMenuRow, parent: Option<u8>) -> Result<(), Errno> {
        let at = usize::from(self.len);
        if at == APP_MENU_MAX_TOTAL_ROWS {
            return Err(Errno::NoSpace);
        }
        self.check_shape(&row, parent, at)?;
        let mut record = RowRecord {
            parent: parent.unwrap_or(PARENT_NONE),
            ..RowRecord::VACANT
        };
        let (label, shortcut, reason) = match row {
            AppMenuRow::Item(item) => {
                record.enabled = item.enabled;
                record.mark = item.mark;
                record.kind = RowKind::Item(item.id);
                record.role = item.role;
                (item.label, item.shortcut, item.reason)
            }
            AppMenuRow::Separator => (
                AppMenuLabel::EMPTY,
                AppMenuShortcut::EMPTY,
                AppMenuReason::EMPTY,
            ),
            AppMenuRow::Submenu { label, enabled } => {
                record.kind = RowKind::Submenu;
                record.enabled = enabled;
                (label, AppMenuShortcut::EMPTY, AppMenuReason::EMPTY)
            }
            AppMenuRow::Info => {
                record.kind = RowKind::Info;
                (
                    AppMenuLabel::EMPTY,
                    AppMenuShortcut::EMPTY,
                    AppMenuReason::EMPTY,
                )
            }
        };
        // The whole row's text is admitted or none of it is, so a refusal
        // leaves the text block exactly as it was.
        let text_at = usize::from(self.text_len);
        let needed = usize::from(label.len_byte())
            + usize::from(shortcut.len_byte())
            + usize::from(reason.len_byte());
        let end = text_at.saturating_add(needed);
        if end > APP_MENU_TEXT_BYTES {
            return Err(Errno::NoSpace);
        }
        let mut cursor = text_at;
        for field in [
            &label.raw_bytes()[..usize::from(label.len_byte())],
            &shortcut.raw_bytes()[..usize::from(shortcut.len_byte())],
            &reason.raw_bytes()[..usize::from(reason.len_byte())],
        ] {
            self.text[cursor..cursor + field.len()].copy_from_slice(field);
            cursor += field.len();
        }
        record.text_at = u16::try_from(text_at).map_err(|_| Errno::NoSpace)?;
        record.label_len = label.len_byte();
        record.shortcut_len = shortcut.len_byte();
        record.reason_len = reason.len_byte();
        self.rows[at] = record;
        self.text_len = u16::try_from(end).map_err(|_| Errno::NoSpace)?;
        self.len = self.len.saturating_add(1);
        Ok(())
    }

    /// Whether `row` may be appended at index `at` under `parent`: the one
    /// rule the builder and the wire decoder both apply, so a menu that
    /// crossed the wire is exactly a menu that could have been built.
    fn check_shape(&self, row: &AppMenuRow, parent: Option<u8>, at: usize) -> Result<(), Errno> {
        let depth = match parent {
            None => 1,
            Some(parent) => {
                let parent = usize::from(parent);
                if parent >= at || self.rows[parent].kind != RowKind::Submenu {
                    return Err(Errno::OutOfRange);
                }
                self.depth_of(parent).saturating_add(1)
            }
        };
        let plate = parent.unwrap_or(PARENT_NONE);
        if self.rows[..at]
            .iter()
            .filter(|held| held.parent == plate)
            .count()
            >= APP_MENU_MAX_ROWS
        {
            return Err(Errno::NoSpace);
        }
        let states_id = match row {
            AppMenuRow::Item(item) => Some(item.id),
            AppMenuRow::Separator | AppMenuRow::Submenu { .. } | AppMenuRow::Info => None,
        };
        if let Some(id) = states_id {
            if self.rows[..at]
                .iter()
                .any(|held| held.kind.wire_id() == id.get())
            {
                return Err(Errno::OutOfRange);
            }
        }
        match row {
            AppMenuRow::Item(item) => {
                if item.label.is_empty() {
                    return Err(Errno::OutOfRange);
                }
            }
            AppMenuRow::Separator => {}
            // A submenu row on the deepest plate would open its plate past
            // the bound and so draw a chevron that opens nothing.
            AppMenuRow::Submenu { label, .. } => {
                if label.is_empty() || depth >= APP_MENU_MAX_DEPTH {
                    return Err(Errno::OutOfRange);
                }
            }
            AppMenuRow::Info => {
                if parent.is_some()
                    || self.rows[..at]
                        .iter()
                        .any(|held| held.kind == RowKind::Info)
                {
                    return Err(Errno::OutOfRange);
                }
            }
        }
        Ok(())
    }

    /// How many plates deep the already-validated row at `index` sits.
    ///
    /// The walk terminates because a parent always names an earlier row.
    fn depth_of(&self, index: usize) -> usize {
        let mut depth = 1;
        let mut at = index;
        while self.rows[at].parent != PARENT_NONE {
            at = usize::from(self.rows[at].parent);
            depth += 1;
        }
        depth
    }
}

/// What a primary click on an application's icon-bar slot does.
///
/// The closed set of answers, declared by the application because only it
/// knows whether a second window means anything to it. Raising is the
/// session's to do — an application cannot restack its own window — so the
/// two mixed answers differ in *when* the click is handed over, not in who
/// raises.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppBarClick {
    /// The session raises the application's most recently used window. With
    /// no window there is nothing to raise, and the click does nothing.
    Raise,
    /// The session raises the most recently used window; with none it
    /// delivers [`WindowEvent::AppBarDefault`], so the slot of an
    /// application still resident with its last window closed is the way
    /// back to one.
    RaiseOrOpen,
    /// Every primary click is delivered to the application, window or not —
    /// what an application whose slot means "another one of these" wants.
    Open,
}

impl AppBarClick {
    /// Decode the wire byte, or `None` for a value no declaration could
    /// have carried (fail closed).
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Raise),
            1 => Some(Self::RaiseOrOpen),
            2 => Some(Self::Open),
            _ => None,
        }
    }

    /// The wire byte for this behaviour.
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Raise => 0,
            Self::RaiseOrOpen => 1,
            Self::Open => 2,
        }
    }

    /// Whether a click reaches the application when it owns **no** window.
    #[must_use]
    pub const fn opens_when_windowless(self) -> bool {
        matches!(self, Self::RaiseOrOpen | Self::Open)
    }
}

/// An application's whole icon-bar declaration: where the bar's events
/// reach it, what its slot's primary click does, and its menu.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AppBar {
    /// The declaring application's own endpoint the session delivers
    /// [`WindowEvent::AppBarDefault`] and [`WindowEvent::AppBarMenu`] to.
    /// Never a reserved endpoint.
    pub event_endpoint: u64,
    /// What a primary click on the application's slot does.
    pub click: AppBarClick,
    /// The menu a secondary press on the slot opens. Empty means the
    /// application offers no menu, which the session honours by opening
    /// nothing.
    pub menu: AppMenu,
}

/// The `parent` byte value meaning "this row is at the top level".
///
/// `u8::MAX` rather than zero, so a parent index needs no bias: row 0 is a
/// legitimate submenu parent and spells itself.
const PARENT_NONE: u8 = u8::MAX;

/// The parent index is one byte, and [`PARENT_NONE`] is spent on "no parent",
/// so the row bound has to leave it a value of its own.
const _: () = assert!(APP_MENU_MAX_TOTAL_ROWS < PARENT_NONE as usize);

/// Where a per-gesture menu chain's root plate is anchored, in the
/// requesting window's own client pixels ([`WindowRequest::OpenMenu`]).
///
/// **Window-local, never seat-global.** An application is never told where
/// its window sits on screen, and never learns a pointer position inside a
/// menu, so window-local is the only anchor it can state truthfully — and it
/// is exactly the coordinate space [`WindowEvent::Pointer`] already hands it,
/// so an application anchoring a context menu at the press it just received
/// passes back the very numbers it was given. The session resolves the point
/// against the window's live position and places the chain itself.
///
/// It is a **region**, not a point, because that is what the placement rule
/// reads: a plate hangs clear of the control that opened it and flips to the
/// region's other side when the screen edge leaves no room. A zero-extent
/// anchor is the point case — the region is one pixel position — so a
/// context gesture and a menu-bar button share one placement rule rather
/// than needing two.
///
/// The origin is deliberately unconstrained: any signed offset is a
/// legitimate ask about a control that is partly scrolled out of view, and
/// the session clamps the chain onto the screen. Only the far edge is
/// checked, so the placement arithmetic has no unrepresentable input.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MenuAnchor {
    x: i32,
    y: i32,
    width_px: u32,
    height_px: u32,
}

impl MenuAnchor {
    /// An anchor region whose top-left is `x`/`y` client pixels from the
    /// window's own origin. A zero extent anchors at that single point.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if either far edge is not a representable
    /// window-local coordinate.
    pub const fn new(x: i32, y: i32, width_px: u32, height_px: u32) -> Result<Self, Errno> {
        if x.checked_add_unsigned(width_px).is_none() || y.checked_add_unsigned(height_px).is_none()
        {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            x,
            y,
            width_px,
            height_px,
        })
    }

    /// Client pixels from the window's left edge.
    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Client pixels from the window's top edge.
    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Width of the anchored region; `0` anchors at a point.
    #[must_use]
    pub const fn width_px(self) -> u32 {
        self.width_px
    }

    /// Height of the anchored region; `0` anchors at a point.
    #[must_use]
    pub const fn height_px(self) -> u32 {
        self.height_px
    }

    /// Write the anchor at `at`, in the offsets [`Self::read_at`] reads it
    /// from, so the two cannot disagree about where it sits.
    fn write_to(self, out: &mut [u8], at: usize) {
        put_i32(out, at, self.x);
        put_i32(out, at + 4, self.y);
        put_u32(out, at + 8, self.width_px);
        put_u32(out, at + 12, self.height_px);
    }

    /// Decode and validate the anchor at `at`, refusing an unrepresentable
    /// far edge through the very constructor a builder goes through.
    fn read_at(bytes: &[u8], at: usize) -> Result<Self, Errno> {
        Self::new(
            read_i32(bytes, at),
            read_i32(bytes, at + 4),
            read_u32(bytes, at + 8),
            read_u32(bytes, at + 12),
        )
    }
}

/// Why an accepted menu open never brought a chain up
/// ([`MenuOutcome::Refused`]).
///
/// Closed, and an unknown discriminant on decode fails closed rather than
/// being guessed at. Every reason is a fact about the **seat**, not about the
/// request — a malformed or unauthorised request is refused by the open
/// call itself and mints no open at all — so each one tells the application
/// whether asking again could ever help.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuRefusal {
    /// The desktop has no seat output to place a chain on, or is tearing its
    /// session down. Asking again will not help.
    NoDisplay = 1,
    /// A surface a menu may not displace holds the seat's input — a lock
    /// screen, a system-modal prompt. The application may ask again later;
    /// it must never be able to draw over one itself.
    SeatBusy = 2,
    /// The desktop could not compose the chain's own surfaces. Transient
    /// under memory pressure, so the application may ask again.
    NoResources = 3,
}

impl MenuRefusal {
    /// Recover a refusal reason from its wire discriminant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for an unknown value (fail closed — never
    /// guess why the desktop refused).
    const fn from_u16(raw: u16) -> Result<Self, Errno> {
        match raw {
            1 => Ok(Self::NoDisplay),
            2 => Ok(Self::SeatBusy),
            3 => Ok(Self::NoResources),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The wire discriminant of this reason.
    const fn as_u16(self) -> u16 {
        self as u16
    }
}

/// What became of one accepted menu open: the whole answer, delivered
/// exactly once ([`WindowEvent::MenuClosed`]).
///
/// One type rather than three events, so an application's handling is a
/// total `match` the compiler checks and the engine's "this event answers an
/// open" rule is one variant that cannot drift as the cases grow.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MenuOutcome {
    /// The user chose the row carrying this id. The session never
    /// interprets an id, and never sends one the opened menu did not carry.
    Chosen(AppMenuItemId),
    /// The chain closed without a choice: pressed outside, dismissed with
    /// Escape, displaced by another open, or ended with the seat or the
    /// owning window.
    Dismissed,
    /// The chain never came up, for the stated reason.
    Refused(MenuRefusal),
}

/// Wire discriminant of [`MenuOutcome::Chosen`].
const MENU_OUTCOME_CHOSEN: u16 = 1;
/// Wire discriminant of [`MenuOutcome::Dismissed`].
const MENU_OUTCOME_DISMISSED: u16 = 2;
/// Wire discriminant of [`MenuOutcome::Refused`].
const MENU_OUTCOME_REFUSED: u16 = 3;

/// A validated window title: bounded UTF-8 with no control characters.
///
/// The title crosses a trust boundary into the session's taskbar and
/// window chrome, so it is validated at construction and again at decode:
/// at most [`WINDOW_TITLE_MAX`] bytes, well-formed UTF-8, and no control
/// characters (no escape sequences, no line breaks) — a malformed title
/// is refused, never sanitised.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct WindowTitle {
    bytes: [u8; WINDOW_TITLE_MAX],
    len: u8,
}

impl WindowTitle {
    /// Build a title from `text`, validating length and content.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] — longer than [`WINDOW_TITLE_MAX`]
    ///   bytes when UTF-8 encoded.
    /// * [`Errno::OutOfRange`] — contains a control character.
    pub fn new(text: &str) -> Result<Self, Errno> {
        let len = u8::try_from(text.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if text.len() > WINDOW_TITLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if text.chars().any(char::is_control) {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; WINDOW_TITLE_MAX];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Ok(Self { bytes, len })
    }

    /// The title text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The buffer was validated as UTF-8 at construction/decode; an
        // impossible failure yields the empty title, never a panic.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    /// Decode a title from its fixed-width wire image: one length byte's
    /// worth of validated text, with the tail required zero.
    fn from_wire(len: u8, bytes: &[u8; WINDOW_TITLE_MAX]) -> Result<Self, Errno> {
        let len_usize = usize::from(len);
        if len_usize > WINDOW_TITLE_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[len_usize..].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        let text = core::str::from_utf8(&bytes[..len_usize]).map_err(|_| Errno::OutOfRange)?;
        if text.chars().any(char::is_control) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { bytes: *bytes, len })
    }
}

impl core::fmt::Debug for WindowTitle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("WindowTitle").field(&self.as_str()).finish()
    }
}

/// How the window manager may size one window: fixed at its create
/// geometry, or resizable down to the smallest client the app can lay out.
///
/// The two facts travel as one value rather than as a flag beside a pair of
/// numbers that could contradict it: a window that is never resized has no
/// floor to be measured against, so "fixed, minimum 640×480" is a statement
/// that cannot be built, encoded, or decoded.
///
/// One definition serves both halves — the app fills it in for
/// [`WindowRequest::Create`] and the session's window engine hands its host
/// the same value — so the two cannot drift.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum WindowSizing {
    /// Neither a resize grabber nor a live maximize/restore size toggle is
    /// offered, and the window never receives a [`WindowEvent::Resized`]:
    /// its create geometry is its only size.
    #[default]
    Fixed,
    /// A resize grabber and a live maximize/restore size toggle are offered,
    /// and each new client extent arrives as [`WindowEvent::Resized`]; the
    /// app re-lays-out and re-maps its frame region with
    /// [`WindowRequest::Resize`].
    Resizable {
        /// Smallest client width, in physical pixels, an interactive resize
        /// may reach; `0` declares no minimum of the app's own, leaving the
        /// frame furniture's floor to stand alone.
        ///
        /// The window manager is the enforcer, never the app: a drag stops
        /// at the larger of this and the furniture floor, so the app lays
        /// out at exactly the size it is told. An app that resized itself
        /// back up instead would fight the drag, frame by frame.
        min_width_px: u32,
        /// Smallest client height, in physical pixels, on the same terms as
        /// [`min_width_px`](Self::Resizable::min_width_px).
        min_height_px: u32,
    },
}

impl WindowSizing {
    /// Whether the window manager presents this window as resizable.
    #[must_use]
    pub const fn resizable(self) -> bool {
        matches!(self, Self::Resizable { .. })
    }

    /// The smallest client width an interactive resize may reach, or `0`
    /// where none is declared — including a fixed window, which is never
    /// resized at all.
    #[must_use]
    pub const fn min_width_px(self) -> u32 {
        match self {
            Self::Fixed => 0,
            Self::Resizable { min_width_px, .. } => min_width_px,
        }
    }

    /// The smallest client height an interactive resize may reach, on the
    /// same terms as [`min_width_px`](Self::min_width_px).
    #[must_use]
    pub const fn min_height_px(self) -> u32 {
        match self {
            Self::Fixed => 0,
            Self::Resizable { min_height_px, .. } => min_height_px,
        }
    }
}

/// One window-channel operation (`plans/APPWIN.md` AW2).
///
/// Every request acts on the caller's **own** windows: the session derives
/// ownership from the kernel-attested identity of the in-flight caller,
/// never from a claimed id, so the window id here is a name, not a
/// credential.
// `SetAppBar` carries its whole bounded menu inline: a fixed-frame wire
// request is `Copy` and allocation-free, so the enum's size is its largest
// variant's by design. Boxing to equalise the variants would force an
// allocation into an ABI decode type and drop `Copy` — the wrong trade for a
// transient per-call request that is encoded and dropped, never stored in
// bulk — so the size difference is deliberate.
#[allow(clippy::large_enum_variant)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowRequest {
    /// Open a window over the caller's granted frame region: map the
    /// region once, validate the frame layout, and list the window under
    /// `title`. The session assigns and replies the window id.
    Create {
        /// The `shm_grant` handle minted to the session's serving task,
        /// naming the region that holds the window's frames back-to-back.
        shm_handle: u64,
        /// The caller's own endpoint the session delivers this window's
        /// [`WindowEvent`]s to. Never a reserved endpoint.
        event_endpoint: u64,
        /// Frames laid out back-to-back in the region
        /// (`1..=WINDOW_MAX_FRAMES`).
        frame_count: u32,
        /// Window width in pixels; never zero.
        width_px: u32,
        /// Window height in pixels; never zero.
        height_px: u32,
        /// Bytes between consecutive scanlines; at least one scanline.
        stride_bytes: u32,
        /// Pixel encoding of the frames.
        format: DisplayFormat,
        /// The window's title, listed on the taskbar.
        title: WindowTitle,
        /// Whether the window manager may resize this window, and the
        /// smallest client it may be resized to.
        sizing: WindowSizing,
    },
    /// Open an **undecorated, app-positioned popup surface** stacked
    /// directly above the caller's own window `parent_window_id`: a
    /// context menu or a settings sheet that must not be clipped by the
    /// bounds of the window that owns it (`plans/APPWIN.md`).
    ///
    /// A popup differs from a top-level [`Self::Create`] in kind, not
    /// degree, so it is its own operation rather than extra `Create`
    /// fields: it carries no title (a popup is never a taskbar entry) and
    /// no resizability (a popup is a fixed-size transient), and it is
    /// positioned by an offset **relative to the parent window's client
    /// origin** — an app is never told its own window's screen position,
    /// so the session resolves the absolute point and clamps the whole
    /// popup onto the screen. The popup is undecorated: the window manager
    /// draws it no frame furniture, exactly like the session's own trusted
    /// modal surfaces.
    ///
    /// It counts against the same per-client window budget as
    /// [`Self::Create`], so "popup" cannot be used to exceed the cap. It
    /// answers with the same [`WINDOW_CREATE_REPLY_LEN`]-byte reply as
    /// `Create` (the assigned window id and the serving session identity),
    /// and thereafter [`Self::Present`] and [`Self::Close`] act on the
    /// popup's own id unchanged. Closing the parent window closes its
    /// popups with it.
    CreatePopup {
        /// The caller's own window the popup is anchored above and owned
        /// by (from that window's `Create` reply); never zero.
        parent_window_id: u64,
        /// The `shm_grant` handle minted to the session's serving task,
        /// naming the region that holds the popup's frames back-to-back.
        shm_handle: u64,
        /// The caller's own endpoint the session delivers this popup's
        /// [`WindowEvent`]s to. Never a reserved endpoint.
        event_endpoint: u64,
        /// Frames laid out back-to-back in the region
        /// (`1..=WINDOW_MAX_FRAMES`).
        frame_count: u32,
        /// Popup width in pixels; never zero.
        width_px: u32,
        /// Popup height in pixels; never zero.
        height_px: u32,
        /// Bytes between consecutive scanlines; at least one scanline.
        stride_bytes: u32,
        /// Pixel encoding of the frames.
        format: DisplayFormat,
        /// Horizontal offset of the popup's top-left from the parent
        /// window's client origin, in physical pixels; may be negative.
        offset_x: i32,
        /// Vertical offset of the popup's top-left from the parent
        /// window's client origin, in physical pixels; may be negative.
        offset_y: i32,
    },
    /// Show frame `frame_index` of window `window_id`, of which only
    /// `damage` changed since the previously presented frame.
    Present {
        /// The window being presented (from the `Create` reply).
        window_id: u64,
        /// Index of the frame inside the window's region.
        frame_index: u32,
        /// The changed rectangle; never empty.
        damage: DamageRect,
    },
    /// Close window `window_id`, tearing down its region mapping and its
    /// taskbar entry.
    Close {
        /// The window being closed.
        window_id: u64,
    },
    /// Re-map window `window_id`'s frame region at a new geometry, keeping
    /// the same window id, owner, event endpoint, and taskbar entry.
    ///
    /// A resizable app issues this after the window manager tells it a new
    /// client size (`WindowEvent::Resized`): it allocates a fresh frame
    /// region of the new geometry, grants it to the session, and re-maps
    /// the *existing* window onto it, so a resize/maximize keeps the window
    /// identity rather than opening a new window. The session drops the old
    /// mapping and adopts the new one; the frame layout is validated
    /// exactly as [`Self::Create`].
    Resize {
        /// The window being resized (from the `Create` reply).
        window_id: u64,
        /// The `shm_grant` handle for the new frame region.
        shm_handle: u64,
        /// Frames laid out back-to-back in the new region
        /// (`1..=WINDOW_MAX_FRAMES`).
        frame_count: u32,
        /// New window width in pixels; never zero.
        width_px: u32,
        /// New window height in pixels; never zero.
        height_px: u32,
        /// Bytes between consecutive scanlines; at least one scanline.
        stride_bytes: u32,
        /// Pixel encoding of the new frames.
        format: DisplayFormat,
    },
    /// Retitle window `window_id`, replacing the title given at
    /// [`Self::Create`] with `title`.
    ///
    /// A window's title is not fixed at birth: an app whose window shows a
    /// changing subject — the folder a file manager is browsing, the
    /// document an editor holds — names that subject in its title bar. The
    /// session applies the new title to the window's chrome and to its
    /// taskbar entry together, so the two can never disagree.
    ///
    /// The title is the same bounded, control-character-refusing
    /// [`WindowTitle`] a `Create` carries, and the request acts only on a
    /// window the caller owns.
    SetTitle {
        /// The caller's own window being retitled (from the `Create`
        /// reply).
        window_id: u64,
        /// The window's new title.
        title: WindowTitle,
    },
    /// Ask the session to run its **trusted file picker** for window
    /// `window_id` (`plans/CAPABILITY_USE.md` CU6). The reply is only the
    /// acceptance: the pick is asynchronous — the user browses in the
    /// session's own UI under the session's own authority — and concludes
    /// with a [`WindowEvent::FilePicked`] (carrying a one-shot `fd_redeem`
    /// handle for the chosen file) or a [`WindowEvent::PickCancelled`]
    /// delivered to the window's event endpoint. One pick may be pending
    /// per window; a second request while one is pending is refused
    /// (`AlreadyExists`).
    PickFile {
        /// The requesting app's own window the pick concludes to.
        window_id: u64,
    },
    /// Set window `window_id`'s backdrop-blur radius, in **logical** pixels
    /// (at most [`WINDOW_BACKDROP_BLUR_MAX_PX`]): the compositor blurs
    /// whatever is already composited behind the window's rectangle before
    /// blending the window's own (typically translucent) pixels over it, so
    /// a frosted-glass panel reads correctly instead of compositing flatly
    /// over sharp content. A radius of `0` disables the effect — the
    /// window's own opacity still applies, but nothing behind it is
    /// blurred first.
    ///
    /// The radius is a request the compositor honours as its own retention
    /// budget allows: retaining one window's frosted backdrop costs that
    /// window's pixels, and windows stacked on the same pixels each want
    /// their own, so a window buried under a pile of frosted ones may be
    /// composited with its opacity alone and no blur behind it. Nothing is
    /// drawn wrong by that, and no error is reported for it — the effect is
    /// the frosted *look*, never correctness.
    SetBackdropBlur {
        /// The window whose backdrop blur is being set.
        window_id: u64,
        /// The blur radius in logical pixels; `0` disables the effect.
        radius_px: u16,
    },
    /// Describe the desktop the caller's windows are displayed on: the
    /// screen extent, the UI scale, and the active appearance
    /// ([`DesktopInfo`]). The reply is the
    /// [`WINDOW_DESKTOP_REPLY_LEN`]-byte desktop frame
    /// ([`encode_desktop_reply`] / [`decode_desktop_reply`]).
    ///
    /// Read-only, and the one request that names no window: an app asks
    /// *before* it opens anything, so its first frame is already the right
    /// size, at the right density, in the right colours rather than a
    /// guess it must correct. Thereafter the session pushes a
    /// [`WindowEvent::DesktopChanged`] to each of the app's windows when
    /// any of it changes.
    ///
    /// The reply also carries the serving session's own [`ProcId`], so an
    /// app that declares an icon-bar presence before it owns a window —
    /// or that never opens one — can still authenticate the bar events it
    /// receives (see [`encode_desktop_reply`]).
    ///
    /// It carries no capability: the reply describes the seat's own screen
    /// and theme — no other principal's data, and no authority to act — so
    /// gating it would only force every application to guess at facts the
    /// user can see by looking at their monitor.
    QueryDesktop,
    /// Declare (or re-declare) the calling **application's** presence on
    /// the desktop's icon bar: where its bar events reach it, whether it
    /// handles the primary click itself, and the menu a secondary press
    /// opens.
    ///
    /// Scoped to the caller, not to a window — it is the *application*
    /// that occupies a slot, so the slot outlives any one window and an
    /// application with no window open at all still has somewhere to be
    /// clicked. The session keeps the declaration until the calling
    /// process exits; re-issuing it replaces the previous one whole, which
    /// is how an application changes a row's enablement or its mark.
    ///
    /// It carries no capability: an application asks only to be listed
    /// under its own attested identity, and the session draws the slot
    /// from the bundle the kernel attested owns the caller — never from
    /// anything the caller claims.
    SetAppBar(AppBar),
    /// Open a menu chain for the caller's own window `window_id`, anchored
    /// at `anchor` in that window's client pixels.
    ///
    /// **Per gesture, not a standing declaration.** Unlike
    /// [`Self::SetAppBar`] — which the caller re-issues to replace an
    /// application's whole icon-bar presence — this one asks for a chain to
    /// come up *now*, once, for a window the caller owns. The reply is only
    /// the acceptance: it carries the session-minted, never-reused **open
    /// id**, and the chain's whole answer arrives later as exactly one
    /// [`WindowEvent::MenuClosed`] naming that id.
    ///
    /// The application describes and the desktop decides: it sends the row
    /// model, the plate's title (carried by the menu itself,
    /// [`AppMenu::titled`]) and the anchor, and the session titles, places,
    /// draws, grabs, routes, dismisses, and answers. Nothing here can pin a
    /// chain open, and an empty menu is refused — there would be nothing to
    /// open.
    ///
    /// It carries no capability: asking is scoped by the window the caller
    /// already owns, and ownership is the kernel-attested identity of the
    /// in-flight caller, never the named id. One open may be accepted per
    /// window; while one is unanswered a second is refused
    /// (`AlreadyExists`), which an application cannot legitimately reach —
    /// while its chain is up the seat's grab consumes the press that would
    /// have opened another.
    OpenMenu {
        /// The caller's own window the chain belongs to and its outcome is
        /// delivered to (from the `Create` reply).
        window_id: u64,
        /// Where the root plate is anchored, in that window's client
        /// pixels.
        anchor: MenuAnchor,
        /// The rows to open, and the title of the root plate's band.
        menu: AppMenu,
    },
}

/// Wire operation discriminant of [`WindowRequest::Create`].
const OP_CREATE: u16 = 1;
/// Wire operation discriminant of [`WindowRequest::Present`].
const OP_PRESENT: u16 = 2;
/// Wire operation discriminant of [`WindowRequest::Close`].
const OP_CLOSE: u16 = 3;
/// Wire operation discriminant of [`WindowRequest::PickFile`].
const OP_PICK_FILE: u16 = 4;
/// Wire operation discriminant of [`WindowRequest::Resize`].
const OP_RESIZE: u16 = 5;
/// Wire operation discriminant of [`WindowRequest::QueryDesktop`].
const OP_QUERY_DESKTOP: u16 = 9;
/// Wire operation discriminant of [`WindowRequest::SetBackdropBlur`].
const OP_SET_BACKDROP_BLUR: u16 = 10;
/// Wire operation discriminant of [`WindowRequest::CreatePopup`].
const OP_CREATE_POPUP: u16 = 11;
/// Wire operation discriminant of [`WindowRequest::SetTitle`].
const OP_SET_TITLE: u16 = 12;
/// Wire operation discriminant of [`WindowRequest::SetAppBar`].
const OP_SET_APP_BAR: u16 = 13;
/// Wire operation discriminant of [`WindowRequest::OpenMenu`].
const OP_OPEN_MENU: u16 = 14;

/// Encoded size of every request's header: magic (4), version (2), op (2).
///
/// The operand block follows it, and each operation's block has its own
/// length — the frame is as long as the operation needs and no longer.
const REQUEST_HEADER_LEN: usize = 8;

/// Encoded size of a [`WindowRequest::Present`]: the header, the window id,
/// the frame index, and the four-word damage rectangle.
///
/// This is the hottest operation on the channel — one per composited frame
/// per window — so it is deliberately the shortest frame that carries it.
const PRESENT_WIRE_LEN: usize = 36;
/// Encoded size of a request whose whole operand block is one window id
/// ([`WindowRequest::Close`], [`WindowRequest::PickFile`]).
const WINDOW_ID_WIRE_LEN: usize = REQUEST_HEADER_LEN + 8;
/// Byte offset of the frame-layout block [`WindowRequest::Create`],
/// [`WindowRequest::CreatePopup`] and [`WindowRequest::Resize`] share
/// verbatim ([`FrameLayout::write_to`] / [`read_frame_layout`]).
const FRAME_LAYOUT_AT: usize = 24;
/// One past the shared frame-layout block's last byte: four `u32` fields
/// then the one-byte pixel format.
///
/// Named once, because every operation that carries the block puts its own
/// operands after it — a literal per operation would be three spellings of
/// one fact, and moving a field in the block would silently desynchronise
/// two of them.
const FRAME_LAYOUT_END: usize = FRAME_LAYOUT_AT + 17;

/// Encoded size of a [`WindowRequest::Resize`]: the header, the window id,
/// the shared-memory handle, and the shared frame-layout block.
const RESIZE_WIRE_LEN: usize = FRAME_LAYOUT_END;
/// Encoded size of a [`WindowRequest::SetBackdropBlur`]: the header, the
/// window id, and the radius.
const SET_BACKDROP_BLUR_WIRE_LEN: usize = 18;
/// Encoded size of a [`WindowRequest::QueryDesktop`]: the header alone —
/// the one request that names no window and carries no operand.
const QUERY_DESKTOP_WIRE_LEN: usize = REQUEST_HEADER_LEN;

/// Byte offset of a [`WindowRequest::CreatePopup`] operand tail that
/// follows the shared frame-layout block: the parent window id (8), then
/// the two signed placement offsets (4 each). Only this tail is
/// popup-specific.
const POPUP_PARENT_OFFSET: usize = FRAME_LAYOUT_END;
/// Byte offset of [`WindowRequest::CreatePopup::offset_x`].
const POPUP_OFFSET_X: usize = POPUP_PARENT_OFFSET + 8;
/// Byte offset of [`WindowRequest::CreatePopup::offset_y`].
const POPUP_OFFSET_Y: usize = POPUP_OFFSET_X + 4;
/// Encoded size of a [`WindowRequest::CreatePopup`].
const CREATE_POPUP_WIRE_LEN: usize = POPUP_OFFSET_Y + 4;

/// Byte offset of a [`WindowRequest::Create`] title length, immediately
/// after the shared frame-layout block. The create tail runs on from here:
/// the title text, the resizable flag, and the declared minimum client
/// size.
const CREATE_TITLE_LEN_OFFSET: usize = FRAME_LAYOUT_END;
/// Byte offset of a [`WindowRequest::Create`] title's text.
const CREATE_TITLE_TEXT_OFFSET: usize = CREATE_TITLE_LEN_OFFSET + 1;
/// Byte offset of a [`WindowSizing`]'s resizable flag.
const CREATE_RESIZABLE_OFFSET: usize = CREATE_TITLE_TEXT_OFFSET + WINDOW_TITLE_MAX;
/// Byte offset of [`WindowSizing::Resizable::min_width_px`].
const CREATE_MIN_WIDTH_OFFSET: usize = CREATE_RESIZABLE_OFFSET + 1;
/// Byte offset of [`WindowSizing::Resizable::min_height_px`].
const CREATE_MIN_HEIGHT_OFFSET: usize = CREATE_MIN_WIDTH_OFFSET + 4;
/// Encoded size of a [`WindowRequest::Create`].
const CREATE_WIRE_LEN: usize = CREATE_MIN_HEIGHT_OFFSET + 4;

/// Byte offset of a [`WindowRequest::SetTitle`] title length, immediately
/// after the window id it retitles.
const SET_TITLE_LEN_OFFSET: usize = 16;
/// Byte offset of a [`WindowRequest::SetTitle`] title's text.
const SET_TITLE_TEXT_OFFSET: usize = SET_TITLE_LEN_OFFSET + 1;
/// Encoded size of a [`WindowRequest::SetTitle`].
const SET_TITLE_WIRE_LEN: usize = SET_TITLE_TEXT_OFFSET + WINDOW_TITLE_MAX;

/// Byte offset of a [`WindowRequest::SetAppBar`]'s [`AppBarClick`] byte,
/// immediately after the event endpoint it routes to.
const APP_BAR_CLICK_OFFSET: usize = 16;
/// Byte offset of a [`WindowRequest::SetAppBar`]'s declared row count.
const APP_BAR_ROW_COUNT_OFFSET: usize = APP_BAR_CLICK_OFFSET + 1;
/// Byte offset of the length of a declaration's trailing text block.
const APP_BAR_TEXT_LEN_OFFSET: usize = APP_BAR_ROW_COUNT_OFFSET + 1;
/// Byte offset of the first of a [`WindowRequest::SetAppBar`]'s fixed-width
/// row records. The text block follows the last of them.
const APP_BAR_ROWS_OFFSET: usize = APP_BAR_TEXT_LEN_OFFSET + 2;

/// Encoded size, in bytes, of one menu row record: its kind, flag byte,
/// parent, the lengths of its three text fields, and its item id.
///
/// The text itself is not here — it lies in the declaration's one trailing
/// text block, in row order — so a row costs what it says rather than the
/// widest label, caption and reason it could have said.
const APP_MENU_ROW_WIRE_LEN: usize = APP_MENU_ROW_ID_OFFSET + 2;
/// Byte offset, within one row record, of its flag byte.
const APP_MENU_ROW_FLAGS_OFFSET: usize = 1;
/// Byte offset, within one row record, of the 0-based index of the submenu
/// row whose plate it is on ([`PARENT_NONE`] on the root plate).
const APP_MENU_ROW_PARENT_OFFSET: usize = 2;
/// Byte offset, within one row record, of its label's length.
const APP_MENU_ROW_LABEL_LEN_OFFSET: usize = 3;
/// Byte offset, within one row record, of its accelerator caption's length.
const APP_MENU_ROW_SHORTCUT_LEN_OFFSET: usize = 4;
/// Byte offset, within one row record, of its disabled-row reason's length.
const APP_MENU_ROW_REASON_LEN_OFFSET: usize = 5;
/// Byte offset, within one row record, of its item id.
const APP_MENU_ROW_ID_OFFSET: usize = 6;

/// Encoded size of the menu block a request carries: one fixed-width record
/// per declared row, then those rows' text in row order.
///
/// The one operand block whose length depends on the value it carries, and
/// the reason two operations are variable-length: a menu costs exactly the
/// rows it declares and exactly the text they say, so a frame never pays for
/// what a menu does not have and the counts and the frame length cannot
/// disagree.
const fn menu_block_len(rows: usize, text: usize) -> usize {
    rows * APP_MENU_ROW_WIRE_LEN + text
}

/// Encoded size of a [`WindowRequest::SetAppBar`] declaring `rows` rows
/// whose text runs to `text` bytes.
const fn app_bar_wire_len(rows: usize, text: usize) -> usize {
    APP_BAR_ROWS_OFFSET + menu_block_len(rows, text)
}

/// Encoded size of the longest possible [`WindowRequest::SetAppBar`]: a
/// declaration holding [`APP_MENU_MAX_TOTAL_ROWS`] rows and the whole of
/// [`APP_MENU_TEXT_BYTES`].
const APP_BAR_MAX_WIRE_LEN: usize = app_bar_wire_len(APP_MENU_MAX_TOTAL_ROWS, APP_MENU_TEXT_BYTES);

/// Encoded size of a [`MenuAnchor`]: the signed origin then the extent, as
/// [`MenuAnchor::write_to`] lays them out.
const MENU_ANCHOR_WIRE_LEN: usize = 16;

/// Byte offset of a [`WindowRequest::OpenMenu`]'s anchor, immediately after
/// the window the chain belongs to.
const OPEN_MENU_ANCHOR_OFFSET: usize = 16;
/// Byte offset of the length of an open's root-plate title.
const OPEN_MENU_TITLE_LEN_OFFSET: usize = OPEN_MENU_ANCHOR_OFFSET + MENU_ANCHOR_WIRE_LEN;
/// Byte offset of an open's declared row count.
const OPEN_MENU_ROW_COUNT_OFFSET: usize = OPEN_MENU_TITLE_LEN_OFFSET + 1;
/// Byte offset of the length of an open's rows' text block.
const OPEN_MENU_TEXT_LEN_OFFSET: usize = OPEN_MENU_ROW_COUNT_OFFSET + 1;
/// Byte offset of the first of an open's fixed-width row records. The rows'
/// text follows the last of them, and the title's text follows that.
const OPEN_MENU_ROWS_OFFSET: usize = OPEN_MENU_TEXT_LEN_OFFSET + 2;

/// Encoded size of a [`WindowRequest::OpenMenu`] carrying `rows` rows whose
/// text runs to `text` bytes, under a `title` of that many bytes.
///
/// The title is length-prefixed and trails the rows' text rather than
/// occupying a widest-case field, for the same reason a row's own text does:
/// a menu costs what it says.
const fn open_menu_wire_len(rows: usize, text: usize, title: usize) -> usize {
    OPEN_MENU_ROWS_OFFSET + menu_block_len(rows, text) + title
}

/// Encoded size of the longest possible [`WindowRequest::OpenMenu`]: the
/// widest menu under the widest title.
const OPEN_MENU_MAX_WIRE_LEN: usize = open_menu_wire_len(
    APP_MENU_MAX_TOTAL_ROWS,
    APP_MENU_TEXT_BYTES,
    APP_MENU_LABEL_MAX,
);

/// The larger of two encoded lengths.
const fn longer(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}

/// Bit of a menu row's flag byte meaning "this row is enabled".
const APP_MENU_ROW_FLAG_ENABLED: u8 = 1 << 0;
/// Bits of a menu row's flag byte holding its [`AppMenuMark`].
const APP_MENU_ROW_MARK_MASK: u8 = 0b110;
/// Bit position of [`APP_MENU_ROW_MARK_MASK`] within the flag byte.
const APP_MENU_ROW_MARK_SHIFT: u32 = 1;
/// Bit of a menu row's flag byte meaning [`AppMenuRole::Destructive`].
const APP_MENU_ROW_FLAG_DESTRUCTIVE: u8 = 1 << 3;
/// Every flag bit a menu row defines; anything else is refused.
const APP_MENU_ROW_FLAG_MASK: u8 =
    APP_MENU_ROW_FLAG_ENABLED | APP_MENU_ROW_MARK_MASK | APP_MENU_ROW_FLAG_DESTRUCTIVE;

/// Wire kind of [`AppMenuRow::Item`].
const APP_MENU_KIND_ITEM: u8 = 1;
/// Wire kind of [`AppMenuRow::Separator`].
const APP_MENU_KIND_SEPARATOR: u8 = 2;
/// Wire kind of [`AppMenuRow::Submenu`].
const APP_MENU_KIND_SUBMENU: u8 = 3;
/// Wire kind of [`AppMenuRow::Info`].
const APP_MENU_KIND_INFO: u8 = 4;

impl WindowRequest {
    /// Encoded size of the longest request any operation can produce, and so
    /// the receive bound the window endpoint is bound with
    /// ([`WINDOW_MAX_REQUEST`]).
    ///
    /// It is a *ceiling*, not the frame shape: each operation encodes to its
    /// own [`wire_len`](Self::wire_len), so a short operation sends a short
    /// frame rather than padding out to this.
    pub const MAX_WIRE_LEN: usize = longer(
        CREATE_WIRE_LEN,
        longer(APP_BAR_MAX_WIRE_LEN, OPEN_MENU_MAX_WIRE_LEN),
    );

    /// Encoded size of `self`: the header plus this operation's own operand
    /// block.
    ///
    /// One length per operation, read by both the encoder and the decoder's
    /// exact-length check, so the two cannot disagree about where a frame
    /// ends.
    #[must_use]
    pub const fn wire_len(&self) -> usize {
        match *self {
            Self::Create { .. } => CREATE_WIRE_LEN,
            Self::CreatePopup { .. } => CREATE_POPUP_WIRE_LEN,
            Self::Present { .. } => PRESENT_WIRE_LEN,
            Self::Close { .. } | Self::PickFile { .. } => WINDOW_ID_WIRE_LEN,
            Self::Resize { .. } => RESIZE_WIRE_LEN,
            Self::SetTitle { .. } => SET_TITLE_WIRE_LEN,
            Self::SetBackdropBlur { .. } => SET_BACKDROP_BLUR_WIRE_LEN,
            Self::QueryDesktop => QUERY_DESKTOP_WIRE_LEN,
            Self::SetAppBar(ref bar) => {
                app_bar_wire_len(bar.menu.len(), bar.menu.text_len as usize)
            }
            Self::OpenMenu { ref menu, .. } => open_menu_wire_len(
                menu.len(),
                menu.text_len as usize,
                menu.title.len_byte() as usize,
            ),
        }
    }

    /// Encode `self` little-endian into the front of `out`, returning the
    /// number of bytes written ([`wire_len`](Self::wire_len)).
    ///
    /// The written frame is zeroed first, so the fixed-width fields a
    /// bounded label or title does not fill are zero however dirty the
    /// caller's buffer was, and one value has exactly one encoding.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the whole frame.
    /// * [`Errno::OutOfRange`] — a [`SetAppBar`](Self::SetAppBar) whose menu
    ///   states a title of its own, which an icon-bar menu never does
    ///   ([`AppMenu::titled`]), or an [`OpenMenu`](Self::OpenMenu) carrying
    ///   no rows, which would open nothing.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        self.encodable()?;
        let len = self.wire_len();
        let Some(frame) = out.get_mut(..len) else {
            return Err(Errno::BufferTooSmall);
        };
        frame.fill(0);
        put_u32(frame, 0, WINDOW_REQUEST_MAGIC);
        put_u16(frame, 4, WINDOW_VERSION_V1);
        put_u16(frame, 6, self.op());
        self.write_operands(frame);
        Ok(len)
    }

    /// Whether `self` holds a value the wire carries at all, checked before
    /// a byte is written so a refusal leaves `out` untouched.
    ///
    /// Both cases are shapes the decoder refuses too, so a client cannot
    /// send a frame the session would only reject.
    fn encodable(&self) -> Result<(), Errno> {
        match *self {
            // An icon-bar menu is titled from the bundle's signed manifest,
            // so the declaration has no title field to carry one and a menu
            // that states its own is refused rather than silently retitled.
            Self::SetAppBar(ref bar) if !bar.menu.title.is_empty() => Err(Errno::OutOfRange),
            Self::OpenMenu { ref menu, .. } if menu.is_empty() => Err(Errno::OutOfRange),
            _ => Ok(()),
        }
    }

    /// The wire operation discriminant of `self`, which
    /// [`from_bytes`](Self::from_bytes) dispatches on.
    const fn op(&self) -> u16 {
        match *self {
            Self::Create { .. } => OP_CREATE,
            Self::CreatePopup { .. } => OP_CREATE_POPUP,
            Self::Present { .. } => OP_PRESENT,
            Self::Close { .. } => OP_CLOSE,
            Self::PickFile { .. } => OP_PICK_FILE,
            Self::Resize { .. } => OP_RESIZE,
            Self::SetTitle { .. } => OP_SET_TITLE,
            Self::SetBackdropBlur { .. } => OP_SET_BACKDROP_BLUR,
            Self::QueryDesktop => OP_QUERY_DESKTOP,
            Self::SetAppBar(_) => OP_SET_APP_BAR,
            Self::OpenMenu { .. } => OP_OPEN_MENU,
        }
    }

    /// Write `self`'s operand block into the already-headed frame `out`,
    /// which is exactly [`wire_len`](Self::wire_len) bytes long.
    fn write_operands(&self, out: &mut [u8]) {
        match *self {
            Self::Create { .. } => self.write_create_operands(out),
            Self::CreatePopup { .. } => self.write_popup_operands(out),
            Self::Present {
                window_id,
                frame_index,
                damage,
            } => {
                put_u64(out, 8, window_id);
                put_u32(out, 16, frame_index);
                put_u32(out, 20, damage.x);
                put_u32(out, 24, damage.y);
                put_u32(out, 28, damage.width_px);
                put_u32(out, 32, damage.height_px);
            }
            Self::Close { window_id } | Self::PickFile { window_id } => {
                put_u64(out, 8, window_id);
            }
            Self::Resize {
                window_id,
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            } => {
                put_u64(out, 8, window_id);
                put_u64(out, 16, shm_handle);
                FrameLayout {
                    frame_count,
                    width_px,
                    height_px,
                    stride_bytes,
                    format,
                }
                .write_to(out);
            }
            Self::SetTitle { window_id, title } => encode_set_title(out, window_id, &title),
            Self::SetBackdropBlur {
                window_id,
                radius_px,
            } => {
                put_u64(out, 8, window_id);
                put_u16(out, 16, radius_px);
            }
            Self::QueryDesktop => {}
            Self::SetAppBar(ref bar) => write_app_bar(out, bar),
            Self::OpenMenu {
                window_id,
                anchor,
                ref menu,
            } => write_open_menu(out, window_id, anchor, menu),
        }
    }

    /// Write a [`CreatePopup`](Self::CreatePopup)'s operand block: the
    /// shared surface prologue, then the parent it hangs above and the
    /// offsets from that parent's client origin. A no-op for any other
    /// request.
    fn write_popup_operands(&self, out: &mut [u8]) {
        let Self::CreatePopup {
            parent_window_id,
            shm_handle,
            event_endpoint,
            frame_count,
            width_px,
            height_px,
            stride_bytes,
            format,
            offset_x,
            offset_y,
        } = *self
        else {
            return;
        };
        write_surface_operands(
            out,
            shm_handle,
            event_endpoint,
            &FrameLayout {
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            },
        );
        put_u64(out, POPUP_PARENT_OFFSET, parent_window_id);
        put_i32(out, POPUP_OFFSET_X, offset_x);
        put_i32(out, POPUP_OFFSET_Y, offset_y);
    }

    /// Write a [`Create`](Self::Create)'s operand block: the shared frame
    /// layout, then the title and the sizing contract that follow it.
    /// A no-op for any other request.
    fn write_create_operands(&self, out: &mut [u8]) {
        let Self::Create {
            shm_handle,
            event_endpoint,
            frame_count,
            width_px,
            height_px,
            stride_bytes,
            format,
            title,
            sizing,
        } = *self
        else {
            return;
        };
        write_surface_operands(
            out,
            shm_handle,
            event_endpoint,
            &FrameLayout {
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            },
        );
        encode_title(out, CREATE_TITLE_LEN_OFFSET, &title);
        out[CREATE_RESIZABLE_OFFSET] = u8::from(sizing.resizable());
        put_u32(out, CREATE_MIN_WIDTH_OFFSET, sizing.min_width_px());
        put_u32(out, CREATE_MIN_HEIGHT_OFFSET, sizing.min_height_px());
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// Semantic bounds a decoder can already see are enforced here — the
    /// frame count within `1..=WINDOW_MAX_FRAMES`, a plausible geometry
    /// (no zero extent, a stride that holds one scanline), a valid title,
    /// a non-reserved event endpoint, a non-zero window id, a non-empty
    /// damage rectangle — so no accepted request ever carries a value the
    /// session would have to re-reject structurally. Bounds only the
    /// session knows (which windows exist, who owns them, the configured
    /// frame count) stay server-side.
    ///
    /// The frame must be **exactly** as long as its operation
    /// ([`wire_len`](Self::wire_len)): a shorter frame is truncation and a
    /// longer one is a smuggled field, and neither is tolerated.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than the operation
    ///   it names needs.
    /// * [`Errno::BadMagic`] — wrong magic, a frame longer than the
    ///   operation needs, or a dirty title tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `window-v1`.
    /// * [`Errno::OutOfRange`] — an operation or pixel format outside the
    ///   closed set, a malformed title, a zero window id, open id or menu
    ///   row, a reserved event endpoint, or a menu open carrying no rows.
    /// * [`Errno::LengthOutOfRange`] — a frame count outside
    ///   `1..=WINDOW_MAX_FRAMES`, a zero-extent geometry, a stride too
    ///   small for one scanline, an over-long title length, a minimum
    ///   client size declared by a window that is not resizable, an empty
    ///   damage rectangle, a backdrop-blur radius above
    ///   [`WINDOW_BACKDROP_BLUR_MAX_PX`], or a menu anchor whose far edge is
    ///   not a representable window-local coordinate.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < REQUEST_HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != WINDOW_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != WINDOW_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        match op {
            OP_CREATE => read_create(bytes),
            OP_CREATE_POPUP => read_create_popup(bytes),
            OP_PRESENT => read_present(bytes),
            OP_CLOSE => {
                exact_len(bytes, WINDOW_ID_WIRE_LEN)?;
                let window_id = nonzero_id(read_u64(bytes, 8))?;
                Ok(Self::Close { window_id })
            }
            OP_PICK_FILE => {
                exact_len(bytes, WINDOW_ID_WIRE_LEN)?;
                let window_id = nonzero_id(read_u64(bytes, 8))?;
                Ok(Self::PickFile { window_id })
            }
            OP_RESIZE => {
                exact_len(bytes, RESIZE_WIRE_LEN)?;
                let window_id = nonzero_id(read_u64(bytes, 8))?;
                let shm_handle = read_u64(bytes, 16);
                let layout = read_frame_layout(bytes)?;
                Ok(Self::Resize {
                    window_id,
                    shm_handle,
                    frame_count: layout.frame_count,
                    width_px: layout.width_px,
                    height_px: layout.height_px,
                    stride_bytes: layout.stride_bytes,
                    format: layout.format,
                })
            }
            OP_SET_TITLE => read_set_title(bytes),
            OP_SET_APP_BAR => read_app_bar(bytes),
            OP_OPEN_MENU => read_open_menu(bytes),
            OP_SET_BACKDROP_BLUR => {
                exact_len(bytes, SET_BACKDROP_BLUR_WIRE_LEN)?;
                let window_id = nonzero_id(read_u64(bytes, 8))?;
                let radius_px = read_u16(bytes, 16);
                if radius_px > WINDOW_BACKDROP_BLUR_MAX_PX {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::SetBackdropBlur {
                    window_id,
                    radius_px,
                })
            }
            OP_QUERY_DESKTOP => {
                exact_len(bytes, QUERY_DESKTOP_WIRE_LEN)?;
                Ok(Self::QueryDesktop)
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Decode the operands of a [`WindowRequest::Present`]: the window, the
/// frame index, and the damage rectangle, which is never empty.
fn read_present(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    exact_len(bytes, PRESENT_WIRE_LEN)?;
    let window_id = nonzero_id(read_u64(bytes, 8))?;
    let frame_index = read_u32(bytes, 16);
    let damage = DamageRect {
        x: read_u32(bytes, 20),
        y: read_u32(bytes, 24),
        width_px: read_u32(bytes, 28),
        height_px: read_u32(bytes, 32),
    };
    if damage.width_px == 0 || damage.height_px == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(WindowRequest::Present {
        window_id,
        frame_index,
        damage,
    })
}

/// Decode the operands of a [`WindowRequest::SetTitle`]: the window being
/// retitled and its new title, validated by the same [`WindowTitle`] wire
/// decode a `Create` title goes through, with the reserved tail required
/// zero.
fn read_set_title(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    exact_len(bytes, SET_TITLE_WIRE_LEN)?;
    let window_id = nonzero_id(read_u64(bytes, 8))?;
    let mut title_bytes = [0u8; WINDOW_TITLE_MAX];
    title_bytes.copy_from_slice(&bytes[SET_TITLE_TEXT_OFFSET..SET_TITLE_WIRE_LEN]);
    let title = WindowTitle::from_wire(bytes[SET_TITLE_LEN_OFFSET], &title_bytes)?;
    Ok(WindowRequest::SetTitle { window_id, title })
}

/// Write `menu`'s rows into `out` at `at`: one fixed-width record per
/// declared row, then those rows' text in row order. Returns the offset just
/// past the block.
///
/// The one definition both variable-length operations share
/// ([`WindowRequest::SetAppBar`], [`WindowRequest::OpenMenu`]), so a row
/// record cannot be laid out one way by a declaration and another by an
/// open (mirrors [`read_menu_block`]).
fn write_menu_block(out: &mut [u8], at: usize, menu: &AppMenu) -> usize {
    let text = at + menu.len() * APP_MENU_ROW_WIRE_LEN;
    for (index, record) in menu.rows[..menu.len()].iter().enumerate() {
        let record_at = at + index * APP_MENU_ROW_WIRE_LEN;
        out[record_at] = record.kind.wire();
        out[record_at + APP_MENU_ROW_FLAGS_OFFSET] = (u8::from(record.enabled)
            * APP_MENU_ROW_FLAG_ENABLED)
            | ((record.mark as u8) << APP_MENU_ROW_MARK_SHIFT)
            | (u8::from(record.role == AppMenuRole::Destructive) * APP_MENU_ROW_FLAG_DESTRUCTIVE);
        out[record_at + APP_MENU_ROW_PARENT_OFFSET] = record.parent;
        out[record_at + APP_MENU_ROW_LABEL_LEN_OFFSET] = record.label_len;
        out[record_at + APP_MENU_ROW_SHORTCUT_LEN_OFFSET] = record.shortcut_len;
        out[record_at + APP_MENU_ROW_REASON_LEN_OFFSET] = record.reason_len;
        put_u16(
            out,
            record_at + APP_MENU_ROW_ID_OFFSET,
            record.kind.wire_id(),
        );
    }
    let len = usize::from(menu.text_len);
    out[text..text + len].copy_from_slice(&menu.text[..len]);
    text + len
}

/// Decode `count` row records at `at` in `bytes`, and the `text_len` bytes of
/// row text that follow them, into `menu` — refusing anything a builder could
/// not have produced (mirrors [`write_menu_block`], offset for offset).
///
/// Every row goes through the same [`AppMenu::push_row`] shape rule the
/// builder applies, so a menu that crossed the wire is exactly a menu an
/// application could have constructed — there is no second, weaker set of
/// rules on the receiving side. Each row's text is taken strictly in row
/// order and the text block must be consumed exactly, so there is no offset
/// to point anywhere, no two rows can share bytes, and no text can ride along
/// unread.
///
/// The caller has already fixed the frame's length against these very counts
/// ([`exact_len`]), which is what makes every read here in bounds.
fn read_menu_block(
    menu: &mut AppMenu,
    bytes: &[u8],
    at: usize,
    count: usize,
    text_len: usize,
) -> Result<(), Errno> {
    let text_at = at + count * APP_MENU_ROW_WIRE_LEN;
    let text = &bytes[text_at..text_at + text_len];
    let mut cursor = 0usize;
    for index in 0..count {
        let record_at = at + index * APP_MENU_ROW_WIRE_LEN;
        let record = &bytes[record_at..record_at + APP_MENU_ROW_WIRE_LEN];
        let (row, parent) = read_app_menu_row(record, text, &mut cursor)?;
        menu.push_row(row, parent)?;
    }
    if cursor != text_len {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(())
}

/// Write an icon-bar declaration's operand block: the event route, the
/// click behaviour, the declared row count and text length, then the shared
/// menu block (mirrors [`read_app_bar`]).
///
/// The block ends with the last text byte, so the counts and the frame
/// length state the same thing and one menu has exactly one encoding.
fn write_app_bar(out: &mut [u8], bar: &AppBar) {
    put_u64(out, 8, bar.event_endpoint);
    out[APP_BAR_CLICK_OFFSET] = bar.click.to_wire();
    out[APP_BAR_ROW_COUNT_OFFSET] = bar.menu.len;
    put_u16(out, APP_BAR_TEXT_LEN_OFFSET, bar.menu.text_len);
    write_menu_block(out, APP_BAR_ROWS_OFFSET, &bar.menu);
}

/// Decode an icon-bar declaration (mirrors [`write_app_bar`]).
fn read_app_bar(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    if bytes.len() < APP_BAR_ROWS_OFFSET {
        return Err(Errno::BufferTooSmall);
    }
    let count = usize::from(bytes[APP_BAR_ROW_COUNT_OFFSET]);
    let text_len = usize::from(read_u16(bytes, APP_BAR_TEXT_LEN_OFFSET));
    if count > APP_MENU_MAX_TOTAL_ROWS || text_len > APP_MENU_TEXT_BYTES {
        return Err(Errno::LengthOutOfRange);
    }
    // The counts are read before the length is fixed, so a declaration whose
    // frame does not hold exactly the rows and text it claims is refused
    // rather than read short or read into a neighbouring field.
    exact_len(bytes, app_bar_wire_len(count, text_len))?;
    let event_endpoint = read_u64(bytes, 8);
    if crate::ipc::is_reserved_endpoint(event_endpoint) {
        return Err(Errno::OutOfRange);
    }
    let click = AppBarClick::from_wire(bytes[APP_BAR_CLICK_OFFSET]).ok_or(Errno::OutOfRange)?;
    let mut menu = AppMenu::EMPTY;
    read_menu_block(&mut menu, bytes, APP_BAR_ROWS_OFFSET, count, text_len)?;
    Ok(WindowRequest::SetAppBar(AppBar {
        event_endpoint,
        click,
        menu,
    }))
}

/// Write a menu open's operand block: the window the chain belongs to, the
/// anchor, the three lengths, the shared menu block, and the root plate's
/// title (mirrors [`read_open_menu`]).
///
/// The title trails the rows' text so the shared block keeps one layout;
/// where a declaration's block simply ends, an open's carries the title
/// after it.
fn write_open_menu(out: &mut [u8], window_id: u64, anchor: MenuAnchor, menu: &AppMenu) {
    put_u64(out, 8, window_id);
    anchor.write_to(out, OPEN_MENU_ANCHOR_OFFSET);
    out[OPEN_MENU_TITLE_LEN_OFFSET] = menu.title.len_byte();
    out[OPEN_MENU_ROW_COUNT_OFFSET] = menu.len;
    put_u16(out, OPEN_MENU_TEXT_LEN_OFFSET, menu.text_len);
    let title_at = write_menu_block(out, OPEN_MENU_ROWS_OFFSET, menu);
    let title = &menu.title.raw_bytes()[..usize::from(menu.title.len_byte())];
    out[title_at..title_at + title.len()].copy_from_slice(title);
}

/// Decode a menu open (mirrors [`write_open_menu`]).
///
/// The title is bounded and content-checked by the very validator a row
/// label goes through, because that is exactly what it is: a name the
/// desktop draws in its own chrome, never a credential. A menu with no rows
/// is refused — there would be nothing to open — and the anchor's far edge
/// must be a representable window-local coordinate, so the session's
/// placement arithmetic has no unrepresentable input.
fn read_open_menu(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    if bytes.len() < OPEN_MENU_ROWS_OFFSET {
        return Err(Errno::BufferTooSmall);
    }
    let title_len = usize::from(bytes[OPEN_MENU_TITLE_LEN_OFFSET]);
    let count = usize::from(bytes[OPEN_MENU_ROW_COUNT_OFFSET]);
    let text_len = usize::from(read_u16(bytes, OPEN_MENU_TEXT_LEN_OFFSET));
    if count > APP_MENU_MAX_TOTAL_ROWS
        || text_len > APP_MENU_TEXT_BYTES
        || title_len > APP_MENU_LABEL_MAX
    {
        return Err(Errno::LengthOutOfRange);
    }
    if count == 0 {
        return Err(Errno::OutOfRange);
    }
    exact_len(bytes, open_menu_wire_len(count, text_len, title_len))?;
    let window_id = nonzero_id(read_u64(bytes, 8))?;
    let anchor = MenuAnchor::read_at(bytes, OPEN_MENU_ANCHOR_OFFSET)?;
    // The title trails the whole menu block, whose length the counts above
    // already state, so it is read before the rows the menu is then filled
    // with rather than needing a second pass to find.
    let title_at = OPEN_MENU_ROWS_OFFSET + menu_block_len(count, text_len);
    let mut cursor = 0usize;
    let title = read_app_menu_text::<0, APP_MENU_LABEL_MAX>(
        &bytes[title_at..],
        &mut cursor,
        bytes[OPEN_MENU_TITLE_LEN_OFFSET],
    )?;
    let mut menu = AppMenu::titled(title);
    read_menu_block(&mut menu, bytes, OPEN_MENU_ROWS_OFFSET, count, text_len)?;
    Ok(WindowRequest::OpenMenu {
        window_id,
        anchor,
        menu,
    })
}

/// Decode one fixed-width menu row record, taking its text from `text` at
/// `cursor` and advancing it past what the row claimed.
///
/// A field a row's kind does not use must be zero, so a row has exactly one
/// encoding and a client cannot smuggle bytes through an ignored field.
fn read_app_menu_row(
    record: &[u8],
    text: &[u8],
    cursor: &mut usize,
) -> Result<(AppMenuRow, Option<u8>), Errno> {
    let flags = record[APP_MENU_ROW_FLAGS_OFFSET];
    if flags & !APP_MENU_ROW_FLAG_MASK != 0 {
        return Err(Errno::OutOfRange);
    }
    let enabled = flags & APP_MENU_ROW_FLAG_ENABLED != 0;
    let mark = AppMenuMark::from_u8((flags & APP_MENU_ROW_MARK_MASK) >> APP_MENU_ROW_MARK_SHIFT)?;
    let role = if flags & APP_MENU_ROW_FLAG_DESTRUCTIVE == 0 {
        AppMenuRole::Neutral
    } else {
        AppMenuRole::Destructive
    };
    let parent = match record[APP_MENU_ROW_PARENT_OFFSET] {
        PARENT_NONE => None,
        parent => Some(parent),
    };
    let id = read_u16(record, APP_MENU_ROW_ID_OFFSET);
    let label = read_app_menu_text::<0, APP_MENU_LABEL_MAX>(
        text,
        cursor,
        record[APP_MENU_ROW_LABEL_LEN_OFFSET],
    )?;
    let shortcut = read_app_menu_text::<0, APP_MENU_SHORTCUT_MAX>(
        text,
        cursor,
        record[APP_MENU_ROW_SHORTCUT_LEN_OFFSET],
    )?;
    let reason = read_app_menu_text::<0, APP_MENU_REASON_MAX>(
        text,
        cursor,
        record[APP_MENU_ROW_REASON_LEN_OFFSET],
    )?;
    // A row that opens a child draws a chevron where an item draws its
    // caption, and opens rather than acting, so it states none of an item's
    // emphasis.
    let opens_a_child = shortcut.is_empty()
        && reason.is_empty()
        && mark == AppMenuMark::None
        && role == AppMenuRole::Neutral;
    let bare = opens_a_child && label.is_empty() && !enabled && id == 0;
    let row = match record[0] {
        APP_MENU_KIND_ITEM => {
            let item = AppMenuItem::new(AppMenuItemId::new(id)?, label)
                .with_mark(mark)
                .with_shortcut(shortcut)
                .with_reason(reason)
                .with_role(role);
            AppMenuRow::Item(if enabled { item } else { item.disabled() })
        }
        // A submenu draws a chevron where an item draws its caption, and
        // opens rather than acting, so it states neither and has no id.
        APP_MENU_KIND_SUBMENU if opens_a_child && id == 0 => AppMenuRow::Submenu { label, enabled },
        APP_MENU_KIND_SEPARATOR if bare => AppMenuRow::Separator,
        APP_MENU_KIND_INFO if bare => AppMenuRow::Info,
        // A known kind whose guard failed and an unknown one are the same
        // refusal: the row states something its kind cannot.
        _ => return Err(Errno::OutOfRange),
    };
    Ok((row, parent))
}

/// Take `len` bytes of validated text from `text` at `cursor`, advancing it.
///
/// One reader for all three of a row's fields, so each is bounded and
/// content-checked by its own type's validator rather than by a per-field
/// copy of the same rule.
fn read_app_menu_text<const MIN: usize, const MAX: usize>(
    text: &[u8],
    cursor: &mut usize,
    len: u8,
) -> Result<BoundedText<MIN, MAX>, Errno> {
    let end = usize::from(len)
        .checked_add(*cursor)
        .ok_or(Errno::LengthOutOfRange)?;
    let bytes = text.get(*cursor..end).ok_or(Errno::LengthOutOfRange)?;
    let field = BoundedText::new(core::str::from_utf8(bytes).map_err(|_| Errno::OutOfRange)?)?;
    *cursor = end;
    Ok(field)
}

/// Write `title` into the fixed-width title field whose length byte sits at
/// `len_at` and whose text follows it.
///
/// The one title encoding the create and retitle frames share, so the two
/// cannot lay the same field out differently.
fn encode_title(out: &mut [u8], len_at: usize, title: &WindowTitle) {
    out[len_at] = title.len;
    let text = len_at.saturating_add(1);
    out[text..text.saturating_add(WINDOW_TITLE_MAX)].copy_from_slice(&title.bytes);
}

/// Encode the window id + title payload of [`WindowRequest::SetTitle`]
/// (mirrors [`read_set_title`]).
fn encode_set_title(out: &mut [u8], window_id: u64, title: &WindowTitle) {
    put_u64(out, 8, window_id);
    encode_title(out, SET_TITLE_LEN_OFFSET, title);
}

/// Decode the operands of a [`WindowRequest::Create`]: the granted region
/// and event route, the frame layout, the title, and the resizability and
/// minimum client size the app asks the window manager for.
///
/// The widest operand block the protocol carries, so it reads as its own
/// step rather than crowding out every other operation in the decoder.
///
/// The minimum is untrusted, so it is taken only where it can mean
/// something: any size is a legitimate floor for a resizable window (the
/// window manager enforces it against its own furniture floor and the
/// screen), while a fixed-size window that declares one is contradicting
/// itself and is refused.
fn read_create(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    exact_len(bytes, CREATE_WIRE_LEN)?;
    let shm_handle = read_u64(bytes, 8);
    let event_endpoint = read_u64(bytes, 16);
    if crate::ipc::is_reserved_endpoint(event_endpoint) {
        return Err(Errno::OutOfRange);
    }
    let layout = read_frame_layout(bytes)?;
    let mut title_bytes = [0u8; WINDOW_TITLE_MAX];
    title_bytes.copy_from_slice(
        &bytes[CREATE_TITLE_TEXT_OFFSET..CREATE_TITLE_TEXT_OFFSET + WINDOW_TITLE_MAX],
    );
    let title = WindowTitle::from_wire(bytes[CREATE_TITLE_LEN_OFFSET], &title_bytes)?;
    Ok(WindowRequest::Create {
        shm_handle,
        event_endpoint,
        frame_count: layout.frame_count,
        width_px: layout.width_px,
        height_px: layout.height_px,
        stride_bytes: layout.stride_bytes,
        format: layout.format,
        title,
        sizing: read_sizing(bytes)?,
    })
}

/// Decode a `Create`'s sizing contract from the resizable flag and the two
/// minimum fields that follow it.
///
/// A minimum stated by a window that is never resized is a contradiction
/// [`WindowSizing`] cannot hold, so a frame carrying one is refused rather
/// than silently stripped of the half that does not fit.
fn read_sizing(bytes: &[u8]) -> Result<WindowSizing, Errno> {
    let min_width_px = read_u32(bytes, CREATE_MIN_WIDTH_OFFSET);
    let min_height_px = read_u32(bytes, CREATE_MIN_HEIGHT_OFFSET);
    match bytes[CREATE_RESIZABLE_OFFSET] {
        0 if min_width_px == 0 && min_height_px == 0 => Ok(WindowSizing::Fixed),
        0 => Err(Errno::LengthOutOfRange),
        1 => Ok(WindowSizing::Resizable {
            min_width_px,
            min_height_px,
        }),
        _ => Err(Errno::OutOfRange),
    }
}

/// Decode the operands of a [`WindowRequest::CreatePopup`]: the granted
/// region and event route and the frame layout (the same offsets a
/// `Create` carries, reusing [`read_frame_layout`]), then the popup's own
/// tail — the parent window id it is anchored above and the signed
/// placement offsets from the parent's client origin.
///
/// Fails closed exactly as `Create`: a reserved endpoint, a bad geometry,
/// a zero parent window id, or a dirty reserved tail is refused. The
/// offsets are unconstrained signed values — the session clamps the popup
/// onto the screen, so any offset is a legitimate request.
fn read_create_popup(bytes: &[u8]) -> Result<WindowRequest, Errno> {
    exact_len(bytes, CREATE_POPUP_WIRE_LEN)?;
    let shm_handle = read_u64(bytes, 8);
    let event_endpoint = read_u64(bytes, 16);
    if crate::ipc::is_reserved_endpoint(event_endpoint) {
        return Err(Errno::OutOfRange);
    }
    let layout = read_frame_layout(bytes)?;
    let parent_window_id = nonzero_id(read_u64(bytes, POPUP_PARENT_OFFSET))?;
    let offset_x = read_i32(bytes, POPUP_OFFSET_X);
    let offset_y = read_i32(bytes, POPUP_OFFSET_Y);
    Ok(WindowRequest::CreatePopup {
        parent_window_id,
        shm_handle,
        event_endpoint,
        frame_count: layout.frame_count,
        width_px: layout.width_px,
        height_px: layout.height_px,
        stride_bytes: layout.stride_bytes,
        format: layout.format,
        offset_x,
        offset_y,
    })
}

/// The frame-layout fields `Create`, `CreatePopup` and `Resize` share
/// verbatim at the same wire offsets: the frame count, geometry, stride, and
/// pixel format.
struct FrameLayout {
    frame_count: u32,
    width_px: u32,
    height_px: u32,
    stride_bytes: u32,
    format: DisplayFormat,
}

impl FrameLayout {
    /// Write the block at the offsets [`read_frame_layout`] reads it from, so
    /// encoding and decoding can never disagree about where it sits.
    fn write_to(&self, out: &mut [u8]) {
        put_u32(out, FRAME_LAYOUT_AT, self.frame_count);
        put_u32(out, FRAME_LAYOUT_AT + 4, self.width_px);
        put_u32(out, FRAME_LAYOUT_AT + 8, self.height_px);
        put_u32(out, FRAME_LAYOUT_AT + 12, self.stride_bytes);
        out[FRAME_LAYOUT_AT + 16] = self.format.as_u8();
    }
}

/// Write the operand block every surface-opening request begins with: the
/// granted region, the event route, and the shared frame layout, at the
/// offsets [`read_frame_layout`] and its callers read them back from.
fn write_surface_operands(
    out: &mut [u8],
    shm_handle: u64,
    event_endpoint: u64,
    layout: &FrameLayout,
) {
    put_u64(out, 8, shm_handle);
    put_u64(out, 16, event_endpoint);
    layout.write_to(out);
}

/// Decode and validate the frame layout those requests carry at
/// [`FRAME_LAYOUT_AT`] — the frame count within `1..=WINDOW_MAX_FRAMES`, a
/// non-zero geometry, a known pixel format, and a stride that holds at least
/// one scanline. The one definition every such arm shares, so the geometry
/// bounds can never diverge between opening, popping up, and resizing a
/// window.
fn read_frame_layout(bytes: &[u8]) -> Result<FrameLayout, Errno> {
    let frame_count = read_u32(bytes, FRAME_LAYOUT_AT);
    if frame_count == 0 || frame_count > WINDOW_MAX_FRAMES {
        return Err(Errno::LengthOutOfRange);
    }
    let width_px = read_u32(bytes, FRAME_LAYOUT_AT + 4);
    let height_px = read_u32(bytes, FRAME_LAYOUT_AT + 8);
    let stride_bytes = read_u32(bytes, FRAME_LAYOUT_AT + 12);
    let format = DisplayFormat::from_u8(bytes[FRAME_LAYOUT_AT + 16])?;
    if width_px == 0 || height_px == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let min_stride = u64::from(width_px) * u64::from(format.bytes_per_pixel());
    if u64::from(stride_bytes) < min_stride {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(FrameLayout {
        frame_count,
        width_px,
        height_px,
        stride_bytes,
        format,
    })
}

/// Refuse a request frame that is not exactly `len` bytes long.
///
/// Short is truncation and long is a field smuggled past the operation's
/// own end; both fail closed. Checking the length is what makes every
/// operand read below it in-bounds by construction.
fn exact_len(bytes: &[u8], len: usize) -> Result<(), Errno> {
    match bytes.len().cmp(&len) {
        Ordering::Less => Err(Errno::BufferTooSmall),
        Ordering::Greater => Err(Errno::BadMagic),
        Ordering::Equal => Ok(()),
    }
}

/// A session-minted id — a window's, or one menu open's — starts at 1 and is
/// never reused; zero names nothing and is refused rather than looked up.
fn nonzero_id(id: u64) -> Result<u64, Errno> {
    if id == 0 {
        return Err(Errno::OutOfRange);
    }
    Ok(id)
}

/// Reply length, in bytes, of a request that mints an id: the shared status
/// word then, on success, the id the session assigned.
///
/// [`WindowRequest::OpenMenu`] answers with exactly this; a `Create` answers
/// with it plus the serving session's identity
/// ([`WINDOW_CREATE_REPLY_LEN`]).
pub const WINDOW_MINTED_ID_REPLY_LEN: usize = 12;

/// Encode a minted-id outcome: on success the non-zero id after a zero
/// status word; on refusal the shared status frame (a negative [`Errno`]
/// discriminant) zero-padded to the same length, so a client always issues
/// one fixed-size receive.
#[must_use]
pub fn encode_minted_id_reply(result: Result<u64, Errno>) -> [u8; WINDOW_MINTED_ID_REPLY_LEN] {
    let mut out = [0u8; WINDOW_MINTED_ID_REPLY_LEN];
    match result {
        Ok(id) => put_u64(&mut out, 4, id),
        Err(err) => out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err))),
    }
    out
}

/// Decode a minted-id reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word, or a successful reply
///   carrying the never-minted zero id.
/// * The decoded [`Errno`] itself, when the session refused the request.
pub fn decode_minted_id_reply(bytes: &[u8]) -> Result<u64, Errno> {
    let frame = bytes
        .get(..WINDOW_MINTED_ID_REPLY_LEN)
        .ok_or(Errno::BufferTooSmall)?;
    crate::reply::decode_status_reply(&frame[..4])?;
    nonzero_id(read_u64(frame, 4))
}

/// Reply length, in bytes, of a `Create`: the minted-id reply, then the
/// serving session's [`ProcId`].
pub const WINDOW_CREATE_REPLY_LEN: usize = WINDOW_MINTED_ID_REPLY_LEN + crate::PROC_ID_LEN;

/// Encode a `Create` outcome: on success the assigned (non-zero) window
/// id followed by the serving session's own [`ProcId`] — the identity an
/// app then requires of every event's kernel-attested sender, closing
/// the event channel against forged input from any other process (the
/// reply itself is trustworthy because the window rendezvous is
/// squat-protected). On refusal, the shared status frame (a negative
/// [`Errno`] discriminant), zero-padded to the same length, so a client
/// always issues one fixed-size receive.
#[must_use]
pub fn encode_create_reply(
    result: Result<u64, Errno>,
    server: ProcId,
) -> [u8; WINDOW_CREATE_REPLY_LEN] {
    let mut out = [0u8; WINDOW_CREATE_REPLY_LEN];
    out[..WINDOW_MINTED_ID_REPLY_LEN].copy_from_slice(&encode_minted_id_reply(result));
    if result.is_ok() {
        out[WINDOW_MINTED_ID_REPLY_LEN..].copy_from_slice(server.as_bytes());
    }
    out
}

/// Reply length, in bytes, of a [`WindowRequest::QueryDesktop`]: the
/// shared status word, the [`DesktopInfo`] record, and the serving
/// session's [`ProcId`].
pub const WINDOW_DESKTOP_REPLY_LEN: usize = 4 + DesktopInfo::WIRE_LEN + crate::PROC_ID_LEN;

/// Byte offset of the serving session's [`ProcId`] in a desktop reply.
const DESKTOP_REPLY_SERVER_OFFSET: usize = 4 + DesktopInfo::WIRE_LEN;

/// Encode a `QueryDesktop` outcome: on success the desktop record after a
/// zero status word, followed by the serving session's own [`ProcId`]; on
/// refusal the shared status frame (a negative [`Errno`] discriminant)
/// zero-padded to the same length, so a client always issues one fixed-size
/// receive.
///
/// The identity is the same one [`encode_create_reply`] stamps, and it is
/// here for the same reason — an app requires it of every event's
/// kernel-attested sender, closing its event channel against forged input.
/// It is carried on *this* reply as well because an application may declare
/// an icon-bar presence, and so start receiving bar events, before it owns
/// any window ([`WindowRequest::SetAppBar`]); the desktop query is the one
/// call every windowed app makes first, so it is where an app that has no
/// window yet learns whom to trust.
#[must_use]
pub fn encode_desktop_reply(
    result: Result<DesktopInfo, Errno>,
    server: ProcId,
) -> [u8; WINDOW_DESKTOP_REPLY_LEN] {
    let mut out = [0u8; WINDOW_DESKTOP_REPLY_LEN];
    match result {
        Ok(desktop) => {
            desktop.write_to_at(&mut out, 4);
            out[DESKTOP_REPLY_SERVER_OFFSET..].copy_from_slice(server.as_bytes());
        }
        Err(err) => out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err))),
    }
    out
}

/// Decode a `QueryDesktop` reply frame into the desktop record and the
/// serving session's [`ProcId`].
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word, a successful reply
///   carrying a desktop no screen or scale could describe, or a malformed
///   server identity.
/// * [`Errno::BadMagic`] — a dirty reserved byte in the record.
/// * The decoded [`Errno`] itself, when the session refused the query.
pub fn decode_desktop_reply(bytes: &[u8]) -> Result<(DesktopInfo, ProcId), Errno> {
    if bytes.len() < WINDOW_DESKTOP_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let desktop = DesktopInfo::from_bytes_at(bytes, 4)?;
    let server = ProcId::from_bytes(
        &bytes[DESKTOP_REPLY_SERVER_OFFSET..DESKTOP_REPLY_SERVER_OFFSET + crate::PROC_ID_LEN],
    )?;
    Ok((desktop, server))
}

/// Decode a `Create` reply frame into the assigned window id and the
/// serving session's [`ProcId`].
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word, a successful reply
///   carrying the never-minted zero window id, or the kernel-reserved
///   all-zero server identity (fail closed: an app must never accept an
///   event stream it cannot authenticate).
/// * The decoded [`Errno`] itself, when the session refused the request.
pub fn decode_create_reply(bytes: &[u8]) -> Result<(u64, ProcId), Errno> {
    if bytes.len() < WINDOW_CREATE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let window_id = decode_minted_id_reply(bytes)?;
    let server = ProcId::from_bytes(&bytes[WINDOW_MINTED_ID_REPLY_LEN..WINDOW_CREATE_REPLY_LEN])?;
    if server.is_kernel() {
        return Err(Errno::OutOfRange);
    }
    Ok((window_id, server))
}

/// Wire event discriminant of [`WindowEvent::Focus`].
const EV_FOCUS: u16 = 1;
/// Wire event discriminant of [`WindowEvent::Key`].
const EV_KEY: u16 = 2;
/// Wire event discriminant of [`WindowEvent::Pointer`].
const EV_POINTER: u16 = 3;
/// Wire event discriminant of [`WindowEvent::CloseRequested`].
const EV_CLOSE_REQUESTED: u16 = 4;
/// Wire event discriminant of [`WindowEvent::FilePicked`].
const EV_FILE_PICKED: u16 = 5;
/// Wire event discriminant of [`WindowEvent::PickCancelled`].
const EV_PICK_CANCELLED: u16 = 6;
/// Wire event discriminant of [`WindowEvent::Scrolled`].
const EV_SCROLLED: u16 = 7;
/// Wire event discriminant of [`WindowEvent::Minimized`].
const EV_MINIMIZED: u16 = 8;
/// Wire event discriminant of [`WindowEvent::Resized`].
const EV_RESIZED: u16 = 9;
/// Wire event discriminant of [`WindowEvent::RedrawRequested`].
const EV_REDRAW_REQUESTED: u16 = 10;
/// Wire event discriminant of [`WindowEvent::DesktopChanged`].
const EV_DESKTOP_CHANGED: u16 = 11;
/// Wire event discriminant of [`WindowEvent::AlternateCloseRequested`].
const EV_ALTERNATE_CLOSE_REQUESTED: u16 = 12;
/// Wire kind of [`WindowEvent::AppBarDefault`].
const EV_APP_BAR_DEFAULT: u16 = 13;
/// Wire kind of [`WindowEvent::AppBarMenu`].
const EV_APP_BAR_MENU: u16 = 14;

/// Wire event discriminant of [`WindowEvent::ContentReleased`].
const EV_CONTENT_RELEASED: u16 = 15;
/// Wire event discriminant of [`WindowEvent::MenuClosed`].
const EV_MENU_CLOSED: u16 = 16;

/// Wire pointer-action discriminant of [`PointerAction::Moved`].
const PTR_MOVED: u16 = 0;
/// Wire pointer-action discriminant of [`PointerAction::Pressed`].
const PTR_PRESSED: u16 = 1;
/// Wire pointer-action discriminant of [`PointerAction::Released`].
const PTR_RELEASED: u16 = 2;

/// What a routed pointer event did at its window-local position.
///
/// The type makes illegal states unrepresentable: a move carries no
/// button, a press/release exactly one resolved button.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerAction {
    /// The pointer moved to the carried position.
    Moved,
    /// A button went down at the carried position.
    Pressed(PointerButtonCode),
    /// A button came up at the carried position.
    Released(PointerButtonCode),
}

/// One window event the session delivers to a window's owning app.
///
/// Events are routed by the session's focus policy: only the app owning
/// the addressed window receives them, and only for windows it created.
/// Pointer positions are **window-local** pixels (origin the window's
/// top-left), already inside the window's extent when the session encodes
/// them; keyboard events reuse the one desktop [`KeyInput`] codec so the
/// key vocabulary has a single definition.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowEvent {
    /// The window gained (`true`) or lost (`false`) keyboard focus.
    Focus {
        /// The window whose focus changed.
        window_id: u64,
        /// Whether the window now holds focus.
        focused: bool,
    },
    /// A key event routed to the focused window.
    Key {
        /// The focused window.
        window_id: u64,
        /// The key event, exactly as the desktop vocabulary defines it.
        key: KeyInput,
    },
    /// A pointer event at a window-local position.
    Pointer {
        /// The window under the pointer.
        window_id: u64,
        /// Window-local x, in pixels from the window's left edge.
        x: u32,
        /// Window-local y, in pixels from the window's top edge.
        y: u32,
        /// What happened at that position.
        action: PointerAction,
        /// The keyboard modifiers held when it happened, so a gesture can be
        /// qualified by one (a shift-click) without the app having to shadow
        /// the seat's modifier state from key events it may never see.
        modifiers: Modifiers,
    },
    /// The user asked the session to close the window (title-bar close).
    /// The app owns the decision: it saves, then issues
    /// [`WindowRequest::Close`] — the session never destroys an app's
    /// window behind its back while the app lives.
    CloseRequested {
        /// The window the user asked to close.
        window_id: u64,
    },
    /// The user made a **secondary** press on the window's title-bar close
    /// control — a distinct request the owning app interprets for itself,
    /// never a close.
    ///
    /// A primary press on the same control still means close
    /// ([`Self::CloseRequested`]); this is the alternate gesture beside
    /// it, for an app whose window has somewhere to *step back* to (a file
    /// manager going up a folder, closing only at the top). An app with no
    /// such notion ignores it, and the window stays exactly as it was: the
    /// session neither closes nor changes the window on this event, and the
    /// control's own drawn state is untouched by a secondary press.
    ///
    /// It reaches only the owning app. A window the session itself owns
    /// (the trusted picker, the greeter) has no app to tell, so a secondary
    /// press on its close control does nothing at all.
    AlternateCloseRequested {
        /// The window whose close control took the secondary press.
        window_id: u64,
    },
    /// The user chose a file in the session's trusted picker
    /// ([`WindowRequest::PickFile`]'s conclusion). `handle` is the
    /// kernel-minted one-shot delegation the app redeems with `fd_redeem`
    /// into a read-only descriptor operated under the *session's* captured
    /// authority — the CU6 user-mediated file capability. The handle is
    /// owner-bound kernel-side, so the value is useless to any other
    /// process.
    FilePicked {
        /// The window whose pick concluded.
        window_id: u64,
        /// The `fd_redeem` handle minted to the app's task; never zero
        /// (the reserved invalid handle).
        handle: u64,
    },
    /// The user dismissed the session's trusted picker without choosing
    /// ([`WindowRequest::PickFile`]'s other conclusion). No authority was
    /// delegated; the app may ask again.
    PickCancelled {
        /// The window whose pick was dismissed.
        window_id: u64,
    },
    /// The window manager minimized the window (the user pressed the
    /// title-bar minimize control, or clicked the taskbar entry): it is
    /// hidden from the workspace but still alive and still listed on the
    /// taskbar. The app may pause non-essential rendering until it is
    /// restored (a later focus/resize event); it need not, and the window
    /// manager destroys nothing behind its back.
    Minimized {
        /// The window that was minimized.
        window_id: u64,
    },
    /// The window manager changed the window's client content size — one
    /// sample of a live resize-grab, or a maximize/restore size toggle. The
    /// app re-lays-out to the new size: it allocates a fresh frame region
    /// of `width_px` × `height_px`, re-maps the window onto it
    /// ([`WindowRequest::Resize`]), and presents. The size is the client
    /// content area in pixels (the window-manager furniture is not the
    /// app's to size); it is never zero.
    ///
    /// A drag reports every sample, so content is resized with the frame
    /// rather than stretched until the button comes up. The extent is a
    /// value the window converges on, not an occurrence it must witness, so
    /// an unbroken run of them folds to the newest — in the session's
    /// hold-back when the app is behind, and in the client's own reader
    /// otherwise — and the window manager owns the geometry until the drag
    /// ends: the app lays out at the size it is told and never asks for one
    /// of its own mid-drag.
    Resized {
        /// The window whose client size changed.
        window_id: u64,
        /// New client width in pixels; never zero.
        width_px: u32,
        /// New client height in pixels; never zero.
        height_px: u32,
    },
    /// The session released this window's retained content pixels to
    /// reclaim memory, and needs the window presented again.
    ///
    /// The app has lost nothing: its own frame regions, size, title,
    /// furniture, focus and place in the stack are all untouched — only
    /// the session's copy of the pixels went away. Presenting any frame
    /// with full-window damage restores the window exactly as it was.
    ///
    /// A client that ignores the event is not broken: its window simply
    /// shows through to the desktop until it next presents for a reason
    /// of its own. The `tairix-window` client library answers the event
    /// on the app's behalf, so an app only handles it when it wants to
    /// genuinely re-render rather than re-send its last frame.
    RedrawRequested {
        /// The window whose content must be presented again.
        window_id: u64,
    },
    /// The session released this window's pixels *and* unmapped the frame
    /// region they were presented from, because nobody can currently see the
    /// window and the machine is short of memory.
    ///
    /// Unlike [`RedrawRequested`](Self::RedrawRequested) this is not a request
    /// to draw — the window is not visible, and drawing it now would spend the
    /// very memory the release recovered. It is permission to let go: the app
    /// may release its own frame region and whatever it renders from, and will
    /// be sent a `RedrawRequested` before the window is seen again. That is
    /// what makes a hidden window cost nothing rather than two copies of its
    /// pixels — on a 4K display, sixty megabytes each.
    ///
    /// A client that ignores it keeps its own copies and its window keeps
    /// working: the next present re-attaches a region as any resize does. What
    /// it must not do is assume the region it holds is still mapped by the
    /// session — a present against a released window is refused, typed, and the
    /// `tairix-window` client library re-attaches on the next paint.
    ContentReleased {
        /// The window whose frame region the session let go.
        window_id: u64,
    },
    /// The scroll wheel turned over the window while the window owns its
    /// own content scrolling (it exposes no window-manager root viewport,
    /// so the session forwards the ticks to the app instead of consuming
    /// them into furniture). The app applies them to its nested scroll
    /// model exactly as it would a keyboard line step. Ticks are in the
    /// device's detent units: positive `dx` toward the logical end,
    /// positive `dy` downward (the `evdev` orientation), one line step per
    /// tick by convention.
    Scrolled {
        /// The window the pointer was over when the wheel turned.
        window_id: u64,
        /// Signed horizontal scroll ticks.
        dx: i32,
        /// Signed vertical scroll ticks.
        dy: i32,
    },
    /// The desktop this window is displayed on changed: a different screen
    /// extent, a different UI scale, or a switch between the light and
    /// dark appearance ([`WindowRequest::QueryDesktop`] is how an app
    /// learns the state it started from).
    ///
    /// The app re-resolves whatever it derived from the old state — its
    /// scale-dependent metrics, its font sizes, its theme colours — and
    /// presents again. Ignoring the event is not broken: the window simply
    /// keeps the appearance it opened with until the app next re-renders
    /// for a reason of its own.
    ///
    /// The desktop belongs to the seat, not to one window, so the session
    /// sends the event to every live window of every client. A client with
    /// two windows is told twice, and both tell it the same thing.
    DesktopChanged {
        /// The window whose desktop is described.
        window_id: u64,
        /// The desktop as it now is.
        desktop: DesktopInfo,
    },
    /// A primary click landed on the application's icon-bar slot and the
    /// click was the application's to handle ([`AppBar::click`]).
    ///
    /// Addressed to the **application**, not to a window: an application
    /// that declared [`AppBarClick::RaiseOrOpen`] is told only when it has
    /// no window, and one that declared [`AppBarClick::Open`] may have
    /// none, so there is no window to address it to. The application
    /// decides what the click means.
    AppBarDefault,
    /// The user chose a row of the application's own icon-bar menu.
    ///
    /// Addressed to the application for the same reason as
    /// [`Self::AppBarDefault`]. The id is the one the application gave the
    /// row; the session never interprets it, and never sends an id the
    /// declaration did not carry.
    AppBarMenu {
        /// The chosen row's application-chosen id.
        item: AppMenuItemId,
    },
    /// The whole answer to one accepted [`WindowRequest::OpenMenu`],
    /// delivered **exactly once**.
    ///
    /// `open_id` is the id that open's reply carried. It is what makes an
    /// answer unmistakable: ids are minted per open and never reused, so an
    /// application that asked again while a previous answer was still in its
    /// mailbox can tell the two apart instead of reading one gesture's
    /// dismissal as the next one's.
    ///
    /// A `Refused` outcome is not a failure to handle — it says the desktop
    /// could not bring a chain up, and the application reports it and
    /// carries on. It never draws a menu of its own instead.
    MenuClosed {
        /// The window the chain belonged to.
        window_id: u64,
        /// The open this answers, from its reply; never zero.
        open_id: u64,
        /// What became of the chain.
        outcome: MenuOutcome,
    },
}

impl WindowEvent {
    /// Encoded size on the wire: magic (4), version (2), kind (2), window
    /// id (8), and a 24-byte event block whose unused tail must be zero
    /// (the embedded [`KeyInput`] record is the widest).
    pub const WIRE_LEN: usize = 40;

    /// The window this event addresses, or `None` for an event addressed
    /// to the whole application rather than to one of its windows (the
    /// icon-bar events, which an application with no window open still
    /// receives).
    #[must_use]
    pub const fn window_id(&self) -> Option<u64> {
        match *self {
            Self::Focus { window_id, .. }
            | Self::Key { window_id, .. }
            | Self::Pointer { window_id, .. }
            | Self::CloseRequested { window_id }
            | Self::AlternateCloseRequested { window_id }
            | Self::FilePicked { window_id, .. }
            | Self::PickCancelled { window_id }
            | Self::Minimized { window_id }
            | Self::Resized { window_id, .. }
            | Self::RedrawRequested { window_id }
            | Self::ContentReleased { window_id }
            | Self::Scrolled { window_id, .. }
            | Self::DesktopChanged { window_id, .. }
            | Self::MenuClosed { window_id, .. } => Some(window_id),
            Self::AppBarDefault | Self::AppBarMenu { .. } => None,
        }
    }

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, WINDOW_EVENT_MAGIC);
        put_u16(&mut out, 4, WINDOW_VERSION_V1);
        put_u64(&mut out, 8, self.window_id().unwrap_or(0));
        match *self {
            Self::Focus { focused, .. } => {
                put_u16(&mut out, 6, EV_FOCUS);
                out[16] = u8::from(focused);
            }
            Self::Key { key, .. } => {
                put_u16(&mut out, 6, EV_KEY);
                out[16..16 + KeyInput::WIRE_LEN].copy_from_slice(&key.to_le_bytes());
            }
            Self::Pointer {
                x,
                y,
                action,
                modifiers,
                ..
            } => {
                put_u16(&mut out, 6, EV_POINTER);
                put_u32(&mut out, 16, x);
                put_u32(&mut out, 20, y);
                let (kind, button) = match action {
                    PointerAction::Moved => (PTR_MOVED, crate::input::BUTTON_NONE),
                    PointerAction::Pressed(button) => (PTR_PRESSED, button.code()),
                    PointerAction::Released(button) => (PTR_RELEASED, button.code()),
                };
                put_u16(&mut out, 24, kind);
                put_u16(&mut out, 26, button);
                put_u16(&mut out, 28, modifiers.to_bits());
            }
            Self::CloseRequested { .. } => {
                put_u16(&mut out, 6, EV_CLOSE_REQUESTED);
            }
            Self::AlternateCloseRequested { .. } => {
                put_u16(&mut out, 6, EV_ALTERNATE_CLOSE_REQUESTED);
            }
            Self::FilePicked { handle, .. } => {
                put_u16(&mut out, 6, EV_FILE_PICKED);
                put_u64(&mut out, 16, handle);
            }
            Self::PickCancelled { .. } => {
                put_u16(&mut out, 6, EV_PICK_CANCELLED);
            }
            Self::Scrolled { dx, dy, .. } => {
                put_u16(&mut out, 6, EV_SCROLLED);
                put_i32(&mut out, 16, dx);
                put_i32(&mut out, 20, dy);
            }
            Self::DesktopChanged { desktop, .. } => {
                put_u16(&mut out, 6, EV_DESKTOP_CHANGED);
                desktop.write_to_at(&mut out, 16);
            }
            Self::Minimized { .. } => {
                put_u16(&mut out, 6, EV_MINIMIZED);
            }
            Self::Resized {
                width_px,
                height_px,
                ..
            } => {
                put_u16(&mut out, 6, EV_RESIZED);
                put_u32(&mut out, 16, width_px);
                put_u32(&mut out, 20, height_px);
            }
            Self::RedrawRequested { .. } => {
                put_u16(&mut out, 6, EV_REDRAW_REQUESTED);
            }
            Self::ContentReleased { .. } => {
                put_u16(&mut out, 6, EV_CONTENT_RELEASED);
            }
            Self::AppBarDefault => {
                put_u16(&mut out, 6, EV_APP_BAR_DEFAULT);
            }
            Self::AppBarMenu { item } => {
                put_u16(&mut out, 6, EV_APP_BAR_MENU);
                put_u16(&mut out, 16, item.get());
            }
            Self::MenuClosed {
                open_id, outcome, ..
            } => {
                put_u16(&mut out, 6, EV_MENU_CLOSED);
                put_u64(&mut out, 16, open_id);
                let (kind, item, refusal) = match outcome {
                    MenuOutcome::Chosen(item) => (MENU_OUTCOME_CHOSEN, item.get(), 0),
                    MenuOutcome::Dismissed => (MENU_OUTCOME_DISMISSED, 0, 0),
                    MenuOutcome::Refused(refusal) => (MENU_OUTCOME_REFUSED, 0, refusal.as_u16()),
                };
                put_u16(&mut out, MENU_CLOSED_OUTCOME_OFFSET, kind);
                put_u16(&mut out, MENU_CLOSED_ITEM_OFFSET, item);
                put_u16(&mut out, MENU_CLOSED_REFUSAL_OFFSET, refusal);
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole event.
    /// * [`Errno::BadMagic`] — wrong magic, a dirty reserved tail, or a
    ///   malformed embedded key record.
    /// * [`Errno::AbiVersionUnsupported`] — not `window-v1`.
    /// * [`Errno::OutOfRange`] — an event kind, focus flag, pointer
    ///   action, button, menu outcome, or refusal reason outside the closed
    ///   set, a zero window id on a window-scoped event, a non-zero one on
    ///   an application-scoped event, a zero menu row or open id, or a
    ///   menu outcome stating a field its own case does not carry.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != WINDOW_EVENT_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != WINDOW_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let kind = read_u16(bytes, 6);
        // The icon-bar events address the application, not a window, so
        // their window-id field must be zero: a non-zero one would be a
        // second, contradictory addressing of the same event.
        if let Some(event) = read_app_scoped_event(kind, bytes) {
            return event;
        }
        let window_id = nonzero_id(read_u64(bytes, 8))?;
        if let Some(event) = read_id_only_event(kind, window_id, bytes) {
            return event;
        }
        match kind {
            EV_FOCUS => {
                event_reserved_zero(bytes, 17)?;
                let focused = match bytes[16] {
                    0 => false,
                    1 => true,
                    _ => return Err(Errno::OutOfRange),
                };
                Ok(Self::Focus { window_id, focused })
            }
            EV_KEY => {
                event_reserved_zero(bytes, 16 + KeyInput::WIRE_LEN)?;
                let key = KeyInput::from_bytes(&bytes[16..16 + KeyInput::WIRE_LEN])?;
                Ok(Self::Key { window_id, key })
            }
            EV_POINTER => read_pointer_event(window_id, bytes),
            EV_FILE_PICKED => {
                event_reserved_zero(bytes, 24)?;
                let handle = read_u64(bytes, 16);
                // Handle 0 is the reserved invalid value the kernel never
                // mints; a "picked" event without a redeemable delegation
                // is refused rather than guessed at.
                if handle == 0 {
                    return Err(Errno::OutOfRange);
                }
                Ok(Self::FilePicked { window_id, handle })
            }
            EV_SCROLLED => {
                event_reserved_zero(bytes, 24)?;
                let dx = read_i32(bytes, 16);
                let dy = read_i32(bytes, 20);
                Ok(Self::Scrolled { window_id, dx, dy })
            }
            EV_RESIZED => {
                event_reserved_zero(bytes, 24)?;
                let width_px = read_u32(bytes, 16);
                let height_px = read_u32(bytes, 20);
                if width_px == 0 || height_px == 0 {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::Resized {
                    window_id,
                    width_px,
                    height_px,
                })
            }
            EV_DESKTOP_CHANGED => {
                event_reserved_zero(bytes, 16 + DesktopInfo::WIRE_LEN)?;
                let desktop = DesktopInfo::from_bytes_at(bytes, 16)?;
                Ok(Self::DesktopChanged { window_id, desktop })
            }
            EV_MENU_CLOSED => {
                event_reserved_zero(bytes, MENU_CLOSED_WIRE_END)?;
                Ok(Self::MenuClosed {
                    window_id,
                    open_id: nonzero_id(read_u64(bytes, 16))?,
                    outcome: read_menu_outcome(bytes)?,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Decode a window-local pointer event: the position, the action, and the
/// modifiers held when it happened.
///
/// A button code on a move, or one outside the closed set, is refused rather
/// than dropped, so an action has exactly one encoding.
fn read_pointer_event(window_id: u64, bytes: &[u8]) -> Result<WindowEvent, Errno> {
    event_reserved_zero(bytes, 30)?;
    let x = read_u32(bytes, 16);
    let y = read_u32(bytes, 20);
    let button = read_u16(bytes, 26);
    let modifiers = Modifiers::from_bits(read_u16(bytes, 28))?;
    let action = match read_u16(bytes, 24) {
        PTR_MOVED => {
            if button != crate::input::BUTTON_NONE {
                return Err(Errno::OutOfRange);
            }
            PointerAction::Moved
        }
        PTR_PRESSED => PointerAction::Pressed(PointerButtonCode::from_code(button)?),
        PTR_RELEASED => PointerAction::Released(PointerButtonCode::from_code(button)?),
        _ => return Err(Errno::OutOfRange),
    };
    Ok(WindowEvent::Pointer {
        window_id,
        x,
        y,
        action,
        modifiers,
    })
}

/// Decode an event addressed to the whole application rather than to one
/// of its windows, or `None` when `kind` names a window-scoped event.
///
/// The window-id field must be zero: an icon-bar event names no window, and
/// a non-zero field would be a second, contradictory addressing of it.
fn read_app_scoped_event(kind: u16, bytes: &[u8]) -> Option<Result<WindowEvent, Errno>> {
    let event = match kind {
        EV_APP_BAR_DEFAULT => {
            if let Err(err) = event_reserved_zero(bytes, 16) {
                return Some(Err(err));
            }
            WindowEvent::AppBarDefault
        }
        EV_APP_BAR_MENU => {
            if let Err(err) = event_reserved_zero(bytes, 18) {
                return Some(Err(err));
            }
            match AppMenuItemId::new(read_u16(bytes, 16)) {
                Ok(item) => WindowEvent::AppBarMenu { item },
                Err(err) => return Some(Err(err)),
            }
        }
        _ => return None,
    };
    if read_u64(bytes, 8) != 0 {
        return Some(Err(Errno::OutOfRange));
    }
    Some(Ok(event))
}

/// Decode an event whose whole payload is its window id, or `None` when
/// `kind` names an event that carries more than that.
///
/// These share one frame shape byte for byte — the id, then a tail required
/// zero — so they share one decoder and cannot drift apart in what they
/// accept.
fn read_id_only_event(
    kind: u16,
    window_id: u64,
    bytes: &[u8],
) -> Option<Result<WindowEvent, Errno>> {
    let event = match kind {
        EV_CLOSE_REQUESTED => WindowEvent::CloseRequested { window_id },
        EV_ALTERNATE_CLOSE_REQUESTED => WindowEvent::AlternateCloseRequested { window_id },
        EV_PICK_CANCELLED => WindowEvent::PickCancelled { window_id },
        EV_MINIMIZED => WindowEvent::Minimized { window_id },
        EV_REDRAW_REQUESTED => WindowEvent::RedrawRequested { window_id },
        EV_CONTENT_RELEASED => WindowEvent::ContentReleased { window_id },
        _ => return None,
    };
    Some(event_reserved_zero(bytes, 16).map(|()| event))
}

/// Byte offset, within a [`WindowEvent::MenuClosed`] frame, of the outcome
/// discriminant that follows the open id.
const MENU_CLOSED_OUTCOME_OFFSET: usize = 24;
/// Byte offset of the chosen row's id, zero unless the outcome is
/// [`MenuOutcome::Chosen`].
const MENU_CLOSED_ITEM_OFFSET: usize = MENU_CLOSED_OUTCOME_OFFSET + 2;
/// Byte offset of the refusal reason, zero unless the outcome is
/// [`MenuOutcome::Refused`].
const MENU_CLOSED_REFUSAL_OFFSET: usize = MENU_CLOSED_ITEM_OFFSET + 2;
/// End of a [`WindowEvent::MenuClosed`]'s own block; the rest of the fixed
/// event frame is reserved and required zero.
const MENU_CLOSED_WIRE_END: usize = MENU_CLOSED_REFUSAL_OFFSET + 2;

/// The three-way outcome a [`WindowEvent::MenuClosed`] frame carries.
///
/// Each case reads exactly one of the two payload fields, and the other must
/// be zero, so an outcome has one encoding and a session cannot state a
/// chosen row *and* a refusal in the same answer.
fn read_menu_outcome(bytes: &[u8]) -> Result<MenuOutcome, Errno> {
    let item = read_u16(bytes, MENU_CLOSED_ITEM_OFFSET);
    let refusal = read_u16(bytes, MENU_CLOSED_REFUSAL_OFFSET);
    match read_u16(bytes, MENU_CLOSED_OUTCOME_OFFSET) {
        MENU_OUTCOME_CHOSEN if refusal == 0 => Ok(MenuOutcome::Chosen(AppMenuItemId::new(item)?)),
        MENU_OUTCOME_DISMISSED if item == 0 && refusal == 0 => Ok(MenuOutcome::Dismissed),
        MENU_OUTCOME_REFUSED if item == 0 => {
            Ok(MenuOutcome::Refused(MenuRefusal::from_u16(refusal)?))
        }
        _ => Err(Errno::OutOfRange),
    }
}

/// Refuse an event whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte.
fn event_reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..WindowEvent::WIRE_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        app_bar_wire_len, decode_create_reply, decode_desktop_reply, decode_minted_id_reply,
        encode_create_reply, encode_desktop_reply, encode_minted_id_reply, open_menu_wire_len,
        put_i32, put_u16, put_u64, read_u16, AppBar, AppBarClick, AppMenu, AppMenuItem,
        AppMenuItemId, AppMenuLabel, AppMenuMark, AppMenuReason, AppMenuRole, AppMenuRow,
        AppMenuRowView, AppMenuShortcut, MenuAnchor, MenuOutcome, MenuRefusal, PointerAction,
        WindowEvent, WindowRequest, WindowSizing, WindowTitle, APP_BAR_CLICK_OFFSET,
        APP_BAR_MAX_WIRE_LEN, APP_BAR_ROWS_OFFSET, APP_BAR_ROW_COUNT_OFFSET,
        APP_BAR_TEXT_LEN_OFFSET, APP_MENU_KIND_SEPARATOR, APP_MENU_KIND_SUBMENU,
        APP_MENU_LABEL_MAX, APP_MENU_MAX_DEPTH, APP_MENU_MAX_ROWS, APP_MENU_MAX_TOTAL_ROWS,
        APP_MENU_REASON_MAX, APP_MENU_ROW_FLAGS_OFFSET, APP_MENU_ROW_FLAG_ENABLED,
        APP_MENU_ROW_ID_OFFSET, APP_MENU_ROW_LABEL_LEN_OFFSET, APP_MENU_ROW_PARENT_OFFSET,
        APP_MENU_ROW_SHORTCUT_LEN_OFFSET, APP_MENU_ROW_WIRE_LEN, APP_MENU_SHORTCUT_MAX,
        APP_MENU_TEXT_BYTES, CREATE_MIN_HEIGHT_OFFSET, CREATE_MIN_WIDTH_OFFSET,
        CREATE_POPUP_WIRE_LEN, CREATE_RESIZABLE_OFFSET, CREATE_WIRE_LEN,
        DESKTOP_REPLY_SERVER_OFFSET, MENU_CLOSED_ITEM_OFFSET, MENU_CLOSED_OUTCOME_OFFSET,
        MENU_CLOSED_REFUSAL_OFFSET, MENU_CLOSED_WIRE_END, OPEN_MENU_ANCHOR_OFFSET,
        OPEN_MENU_MAX_WIRE_LEN, OPEN_MENU_ROW_COUNT_OFFSET, OPEN_MENU_TEXT_LEN_OFFSET,
        OPEN_MENU_TITLE_LEN_OFFSET, PRESENT_WIRE_LEN, REQUEST_HEADER_LEN, SET_TITLE_LEN_OFFSET,
        SET_TITLE_TEXT_OFFSET, SET_TITLE_WIRE_LEN, WINDOW_BACKDROP_BLUR_MAX_PX,
        WINDOW_CREATE_REPLY_LEN, WINDOW_DESKTOP_REPLY_LEN, WINDOW_ENDPOINT, WINDOW_EVENT_MAGIC,
        WINDOW_MAX_FRAMES, WINDOW_MINTED_ID_REPLY_LEN, WINDOW_REQUEST_MAGIC, WINDOW_TITLE_MAX,
    };
    use crate::desktop::{Appearance, DesktopInfo};
    use crate::driver::display::{DamageRect, DisplayFormat};
    use crate::input::{KeyInput, KeyValue, Modifiers, PointerButtonCode};
    use crate::seat::SEATMGR_ENDPOINT;
    use crate::Errno;
    use crate::ProcId;

    /// One encoded request exactly as a caller sends it, so a test mutates
    /// and decodes the bytes that actually went out rather than a padded
    /// frame the protocol no longer has.
    ///
    /// It derefs to its own `len` bytes, and holds room for one more so
    /// [`over_long`](Frame::over_long) can append the byte an exact-length
    /// decode must refuse.
    #[derive(Copy, Clone)]
    struct Frame {
        bytes: [u8; WindowRequest::MAX_WIRE_LEN + 1],
        len: usize,
    }

    impl Frame {
        fn new(request: &WindowRequest) -> Self {
            let mut bytes = [0u8; WindowRequest::MAX_WIRE_LEN + 1];
            let len = request.encode(&mut bytes).expect("the max frame fits");
            assert_eq!(len, request.wire_len());
            Self { bytes, len }
        }

        /// The frame with `filler` appended: a byte past the operation's own
        /// end, which is a smuggled field however innocuous its value.
        fn over_long(mut self, filler: u8) -> Self {
            self.bytes[self.len] = filler;
            self.len += 1;
            self
        }

        /// The frame one byte short: truncation.
        fn truncated(mut self) -> Self {
            self.len -= 1;
            self
        }
    }

    impl core::ops::Deref for Frame {
        type Target = [u8];

        fn deref(&self) -> &[u8] {
            &self.bytes[..self.len]
        }
    }

    impl core::ops::DerefMut for Frame {
        fn deref_mut(&mut self) -> &mut [u8] {
            &mut self.bytes[..self.len]
        }
    }

    impl WindowRequest {
        /// This request encoded exactly as a caller sends it.
        fn frame(&self) -> Frame {
            Frame::new(self)
        }
    }

    fn sample_create() -> WindowRequest {
        sample_create_sized(WindowSizing::Fixed)
    }

    /// The same window, opened with `sizing`.
    fn sample_create_sized(sizing: WindowSizing) -> WindowRequest {
        WindowRequest::Create {
            shm_handle: 7,
            event_endpoint: 0x900d,
            frame_count: 2,
            width_px: 320,
            height_px: 200,
            stride_bytes: 1280,
            format: DisplayFormat::Bgra8888,
            title: WindowTitle::new("Files").expect("a valid title"),
            sizing,
        }
    }

    /// The same window opened **resizable**, declaring the smallest client
    /// size the window manager may resize it to.
    fn sample_create_min(min_width_px: u32, min_height_px: u32) -> WindowRequest {
        sample_create_sized(WindowSizing::Resizable {
            min_width_px,
            min_height_px,
        })
    }

    fn sample_present() -> WindowRequest {
        WindowRequest::Present {
            window_id: 3,
            frame_index: 1,
            damage: DamageRect {
                x: 10,
                y: 20,
                width_px: 30,
                height_px: 40,
            },
        }
    }

    fn sample_create_popup() -> WindowRequest {
        WindowRequest::CreatePopup {
            parent_window_id: 3,
            shm_handle: 7,
            event_endpoint: 0x900d,
            frame_count: 2,
            width_px: 160,
            height_px: 96,
            stride_bytes: 640,
            format: DisplayFormat::Bgra8888,
            offset_x: -12,
            offset_y: 24,
        }
    }

    #[test]
    fn magic_and_endpoint_are_frozen() {
        assert_eq!(WINDOW_REQUEST_MAGIC, u32::from_le_bytes(*b"WIN1"));
        assert_eq!(WINDOW_EVENT_MAGIC, u32::from_le_bytes(*b"WEV1"));
        // "WI" ASCII hex-spelled, the reserved-endpoint convention.
        assert_eq!(WINDOW_ENDPOINT, 0x5749_1001);
        assert!(crate::ipc::is_reserved_endpoint(WINDOW_ENDPOINT));
    }

    #[test]
    fn titles_validate_length_and_content() {
        assert_eq!(WindowTitle::new("").expect("empty is fine").as_str(), "");
        let widest = "w".repeat(WINDOW_TITLE_MAX);
        assert_eq!(
            WindowTitle::new(&widest).expect("max length fits").as_str(),
            widest
        );
        let over = "w".repeat(WINDOW_TITLE_MAX + 1);
        assert_eq!(
            WindowTitle::new(&over).unwrap_err(),
            Errno::LengthOutOfRange
        );
        assert_eq!(
            WindowTitle::new("bad\x1bescape").unwrap_err(),
            Errno::OutOfRange
        );
        assert_eq!(
            WindowTitle::new("two\nlines").unwrap_err(),
            Errno::OutOfRange
        );
    }

    /// A bounded label, for the rows under test.
    fn label(text: &str) -> AppMenuLabel {
        AppMenuLabel::new(text).expect("a valid label")
    }

    /// An enabled, unmarked, neutral row.
    fn item(id: u16, text: &str) -> AppMenuRow {
        AppMenuRow::Item(AppMenuItem::new(
            AppMenuItemId::new(id).expect("a valid id"),
            label(text),
        ))
    }

    /// A menu with one row of every kind, a chain of submenus at the depth
    /// bound, and every marking — the shape the wire tests exercise.
    fn sample_menu() -> AppMenu {
        sample_menu_into(AppMenu::EMPTY)
    }

    /// Those same rows appended to `menu`, so one builder serves the untitled
    /// menu a declaration carries and the titled one an open does.
    fn sample_menu_into(mut menu: AppMenu) -> AppMenu {
        menu.push(AppMenuRow::Item(
            AppMenuItem::new(
                AppMenuItemId::new(1).expect("a valid id"),
                label("New window"),
            )
            .with_shortcut(AppMenuShortcut::new("Ctrl Shift N").expect("a valid caption")),
        ))
        .expect("room for the first row");
        menu.push(AppMenuRow::Submenu {
            label: label("Display"),
            enabled: true,
        })
        .expect("room for a submenu");
        menu.push_under(
            AppMenuRow::Item(
                AppMenuItem::new(
                    AppMenuItemId::new(2).expect("a valid id"),
                    label("Full screen"),
                )
                .with_mark(AppMenuMark::Check),
            ),
            1,
        )
        .expect("room inside the submenu");
        // A submenu inside a submenu, to the depth bound: the whole point of
        // a chain, and what the one-level model could not express.
        menu.push_under(
            AppMenuRow::Submenu {
                label: label("Colour"),
                enabled: true,
            },
            1,
        )
        .expect("a nested submenu");
        menu.push_under(
            AppMenuRow::Item(
                AppMenuItem::new(AppMenuItemId::new(3).expect("a valid id"), label("Green"))
                    .with_mark(AppMenuMark::Radio)
                    .disabled()
                    .with_reason(AppMenuReason::new("No colour profile").expect("a valid reason")),
            ),
            3,
        )
        .expect("room inside the nested submenu");
        menu.push(AppMenuRow::Separator).expect("a separator");
        menu.push(AppMenuRow::Info).expect("an Info row");
        menu.push(AppMenuRow::Item(
            AppMenuItem::new(AppMenuItemId::new(4).expect("a valid id"), label("Quit"))
                .with_role(AppMenuRole::Destructive),
        ))
        .expect("room for Quit");
        menu
    }

    /// The sample declaration: the sample menu, delivered to a plain
    /// (non-reserved) endpoint, with every click the application's.
    fn sample_app_bar() -> AppBar {
        AppBar {
            event_endpoint: 0xE117_0000_0000_0009,
            click: AppBarClick::Open,
            menu: sample_menu(),
        }
    }

    /// The declaration around `menu`, to the same plain endpoint.
    fn declaring(menu: &AppMenu) -> WindowRequest {
        WindowRequest::SetAppBar(AppBar {
            event_endpoint: 0xE117_0000_0000_0009,
            click: AppBarClick::Open,
            menu: *menu,
        })
    }

    /// A menu filled to the frame's own upper bound: every row the model
    /// holds, and the text block full to the byte.
    ///
    /// The rows spread over two plates because a plate holds fewer rows than
    /// the menu does, and each row takes as much text as it can while
    /// leaving every later row a label, so the widest declaration is
    /// genuinely the widest.
    fn widest_menu() -> AppMenu {
        widest_menu_into(AppMenu::EMPTY)
    }

    /// Those same rows appended to `menu`, on the same terms as
    /// [`sample_menu_into`].
    fn widest_menu_into(mut menu: AppMenu) -> AppMenu {
        let wide = "l".repeat(APP_MENU_LABEL_MAX);
        menu.push(AppMenuRow::Submenu {
            label: label(&wide),
            enabled: true,
        })
        .expect("room for a second plate");
        let mut used = APP_MENU_LABEL_MAX;
        for id in 1..APP_MENU_MAX_TOTAL_ROWS {
            let left = APP_MENU_MAX_TOTAL_ROWS - menu.len();
            let text = APP_MENU_TEXT_BYTES
                .saturating_sub(used)
                .saturating_sub(left - 1)
                .clamp(1, APP_MENU_LABEL_MAX);
            let row = item(u16::try_from(id).expect("fits"), &wide[..text]);
            if menu.len() < APP_MENU_MAX_ROWS {
                menu.push(row).expect("room on the root plate");
            } else {
                menu.push_under(row, 0).expect("room on the second plate");
            }
            used += text;
        }
        assert_eq!(menu.len(), APP_MENU_MAX_TOTAL_ROWS);
        assert_eq!(used, APP_MENU_TEXT_BYTES, "the text block fills exactly");
        menu
    }

    /// The sample menu under a title of its own — the shape only an open
    /// carries, since a declaration is titled from the signed manifest.
    fn sample_titled_menu() -> AppMenu {
        sample_menu_into(AppMenu::titled(label("Edit")))
    }

    /// The widest open: the widest menu the model holds, under the widest
    /// title a plate band can state.
    fn widest_open_menu() -> AppMenu {
        widest_menu_into(AppMenu::titled(label(&"t".repeat(APP_MENU_LABEL_MAX))))
    }

    /// The anchor the open tests use: a region, so the placement rule has an
    /// extent to hang a plate clear of.
    fn sample_anchor() -> MenuAnchor {
        MenuAnchor::new(-8, 24, 96, 20).expect("a representable anchor")
    }

    /// An open of `menu` for a window the caller owns, at [`sample_anchor`].
    fn opening(menu: &AppMenu) -> WindowRequest {
        WindowRequest::OpenMenu {
            window_id: 3,
            anchor: sample_anchor(),
            menu: *menu,
        }
    }

    /// Visit one of every operation the request codec encodes, including the
    /// narrowest and widest form of each variable-width one.
    ///
    /// Both the round-trip and the framing tests walk this one list, so an
    /// operation added without a framing test cannot slip through. Visited
    /// one at a time rather than collected: a request carries a whole menu
    /// inline, so a list of them is more than belongs on a stack frame.
    fn each_request(mut visit: impl FnMut(WindowRequest)) {
        visit(sample_create());
        visit(sample_create_popup());
        visit(sample_present());
        visit(WindowRequest::Close { window_id: 9 });
        visit(WindowRequest::PickFile { window_id: 9 });
        visit(WindowRequest::Resize {
            window_id: 3,
            shm_handle: 11,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        });
        visit(WindowRequest::SetTitle {
            window_id: 3,
            title: WindowTitle::new("").expect("an empty title"),
        });
        visit(WindowRequest::SetTitle {
            window_id: 3,
            title: WindowTitle::new(&"t".repeat(WINDOW_TITLE_MAX)).expect("the widest title"),
        });
        visit(WindowRequest::SetBackdropBlur {
            window_id: 5,
            radius_px: 0,
        });
        visit(WindowRequest::SetBackdropBlur {
            window_id: 5,
            radius_px: WINDOW_BACKDROP_BLUR_MAX_PX,
        });
        visit(declaring(&AppMenu::EMPTY));
        visit(WindowRequest::SetAppBar(sample_app_bar()));
        visit(declaring(&one_of_each_bare_row()));
        visit(declaring(&widest_menu()));
        visit(opening(&sample_titled_menu()));
        visit(opening(&one_of_each_bare_row()));
        visit(opening(&widest_open_menu()));
        visit(WindowRequest::QueryDesktop);
    }

    /// A menu whose rows carry no text at all: the narrowest each kind gets,
    /// so the framing tests see a declaration with an empty text block.
    fn one_of_each_bare_row() -> AppMenu {
        let mut menu = AppMenu::EMPTY;
        menu.push(AppMenuRow::Separator).expect("a separator");
        menu.push(AppMenuRow::Info).expect("an Info row");
        menu
    }

    #[test]
    fn requests_round_trip() {
        each_request(|request| {
            let bytes = request.frame();
            assert_eq!(WindowRequest::from_bytes(&bytes), Ok(request));
        });
    }

    /// Every operation encodes to exactly its own declared length, and a
    /// frame that is not that length is refused either way: short is
    /// truncation, long is a field smuggled past the operation's end.
    ///
    /// The over-long case is checked with a zero filler as well as a set
    /// one, because it is the *length* that refuses — a decoder that
    /// tolerated trailing padding so long as it was zero is what this
    /// replaces.
    #[test]
    fn requests_are_framed_to_their_own_length() {
        each_request(|request| {
            let frame = request.frame();
            assert_eq!(frame.len(), request.wire_len());
            assert!(frame.len() <= WindowRequest::MAX_WIRE_LEN);
            for filler in [0, 1] {
                assert_eq!(
                    WindowRequest::from_bytes(&frame.over_long(filler)),
                    Err(Errno::BadMagic),
                    "an over-long frame must be refused: {request:?}"
                );
            }
            assert_eq!(
                WindowRequest::from_bytes(&frame.truncated()),
                Err(Errno::BufferTooSmall),
                "a truncated frame must be refused: {request:?}"
            );

            // No operation declares a byte it does not read: flipping the
            // final byte must change what decodes. Were a length larger
            // than its operand block, the tail would be unread padding —
            // which an exact-length decode would accept and no round-trip
            // would notice.
            let mut last = frame;
            let at = last.len() - 1;
            last[at] ^= 0xFF;
            assert_ne!(
                WindowRequest::from_bytes(&last),
                Ok(request),
                "the last byte of a frame must be read: {request:?}"
            );
        });
    }

    /// The hot path pays for what it carries and nothing more: a present is
    /// one short frame, not the widest operation's.
    ///
    /// Pinned because the cost is invisible at the call site — an operation
    /// added with a wider block would silently re-inflate every present if
    /// the frame were shared again.
    #[test]
    fn present_frames_are_short() {
        assert_eq!(sample_present().wire_len(), PRESENT_WIRE_LEN);
        assert_eq!(PRESENT_WIRE_LEN, 36);
        const { assert!(PRESENT_WIRE_LEN * 4 < WindowRequest::MAX_WIRE_LEN) };
    }

    /// A menu holds its rows' text in one block, so neither the model nor
    /// the frame pays the widest label, caption and reason for every row a
    /// menu does not have.
    ///
    /// Pinned in bytes because the cost is invisible at the call site: the
    /// model rides inside every decoded `WindowRequest`, so a row that grew
    /// a fixed-width text field would multiply itself by the row bound and
    /// inflate the hot path's frame with it.
    #[test]
    fn a_menus_size_is_what_its_rows_say() {
        const ROWS: usize = APP_MENU_MAX_TOTAL_ROWS;
        const WIDEST_ROW_TEXT: usize =
            APP_MENU_LABEL_MAX + APP_MENU_SHORTCUT_MAX + APP_MENU_REASON_MAX;
        // Fixed-width row text would cost this much more than the shared
        // block does, and `WindowRequest` carries a menu inline.
        const { assert!(core::mem::size_of::<AppMenu>() < ROWS * WIDEST_ROW_TEXT / 2) };
        const { assert!(core::mem::size_of::<WindowRequest>() < 4096) };
        // The widest variant carries a menu plus a handful of words — the
        // window it belongs to and its anchor — so the enum's size is one
        // menu's. Fixed-width row text would have dwarfed the lot.
        assert!(
            core::mem::size_of::<WindowRequest>() < core::mem::size_of::<AppMenu>() + 64,
            "a request is one inline menu wide, not the product of the row bounds"
        );

        // A row's text costs its bytes, not its bounds.
        let mut menu = AppMenu::EMPTY;
        menu.push(item(1, "Cut")).expect("room");
        let one = declaring(&menu).wire_len();
        assert_eq!(one, app_bar_wire_len(1, "Cut".len()));
        menu.push(item(2, "Copy")).expect("room");
        assert_eq!(
            declaring(&menu).wire_len(),
            one + APP_MENU_ROW_WIRE_LEN + "Copy".len()
        );
    }

    /// An anchor is window-local geometry the session clamps, so any origin
    /// is legitimate — but its far edge must be a coordinate that exists, so
    /// the placement arithmetic has no unrepresentable input.
    #[test]
    fn menu_anchors_admit_any_origin_and_refuse_an_unrepresentable_edge() {
        let point = MenuAnchor::new(-4096, -1, 0, 0).expect("a point off the client origin");
        assert_eq!((point.x(), point.y()), (-4096, -1));
        assert_eq!((point.width_px(), point.height_px()), (0, 0));
        let region = MenuAnchor::new(i32::MIN, 0, u32::MAX, 0).expect("a region that fits");
        assert_eq!(region.width_px(), u32::MAX);

        for (x, y, width_px, height_px) in [
            (i32::MAX, 0, 1, 0),
            (0, i32::MAX, 0, 1),
            (1, 1, u32::MAX, 0),
            (1, 1, 0, u32::MAX),
        ] {
            assert_eq!(
                MenuAnchor::new(x, y, width_px, height_px).unwrap_err(),
                Errno::LengthOutOfRange,
                "an unrepresentable far edge must be refused: {x},{y} {width_px}x{height_px}"
            );
        }
    }

    /// A menu's own title crosses the wire on an **open** and cannot cross on
    /// a declaration: the icon-bar menu is titled from the bundle's signed
    /// manifest, so an application titling one is refused rather than encoded
    /// and quietly retitled.
    #[test]
    fn a_menu_title_crosses_the_wire_only_on_an_open() {
        let titled = sample_titled_menu();
        assert_eq!(titled.title(), "Edit");
        let request = opening(&titled);
        let Ok(WindowRequest::OpenMenu { menu, anchor, .. }) =
            WindowRequest::from_bytes(&request.frame())
        else {
            panic!("an open round-trips");
        };
        assert_eq!(menu.title(), "Edit");
        assert_eq!(anchor, sample_anchor());

        // Untitled is admissible: the plate's band then states nothing.
        let plain = opening(&sample_menu());
        let Ok(WindowRequest::OpenMenu { menu, .. }) = WindowRequest::from_bytes(&plain.frame())
        else {
            panic!("an untitled open round-trips");
        };
        assert_eq!(menu.title(), "");

        // The same titled menu has no field to travel in on a declaration.
        let mut out = [0u8; WindowRequest::MAX_WIRE_LEN];
        assert_eq!(
            declaring(&titled).encode(&mut out),
            Err(Errno::OutOfRange),
            "a declaration cannot carry a title"
        );
    }

    /// An open with no rows would open nothing, so it is refused at both
    /// ends: a client cannot send one and a session cannot be made to
    /// receive one.
    #[test]
    fn an_open_with_no_rows_is_refused_at_both_ends() {
        let mut out = [0u8; WindowRequest::MAX_WIRE_LEN];
        assert_eq!(
            opening(&AppMenu::EMPTY).encode(&mut out),
            Err(Errno::OutOfRange)
        );
        // A declaration, by contrast, legitimately offers no menu at all.
        assert!(declaring(&AppMenu::EMPTY).encode(&mut out).is_ok());

        let mut rowless = opening(&sample_menu()).frame();
        rowless[OPEN_MENU_ROW_COUNT_OFFSET] = 0;
        assert_eq!(
            WindowRequest::from_bytes(&rowless),
            Err(Errno::OutOfRange),
            "and the decoder refuses one however it was framed"
        );
    }

    /// An open's length follows its rows, their text, and its title, and the
    /// widest of them is what the endpoint's receive bound is sized to.
    #[test]
    fn open_menu_frames_grow_with_their_menu_and_title() {
        let one_row = {
            let mut menu = AppMenu::EMPTY;
            menu.push(item(1, "Cut")).expect("room");
            opening(&menu).wire_len()
        };
        assert_eq!(one_row, open_menu_wire_len(1, "Cut".len(), 0));

        let titled = {
            let mut menu = AppMenu::titled(label("Edit"));
            menu.push(item(1, "Cut")).expect("room");
            opening(&menu).wire_len()
        };
        assert_eq!(titled, one_row + "Edit".len(), "a title costs its bytes");

        let widest = opening(&widest_open_menu());
        assert_eq!(widest.wire_len(), OPEN_MENU_MAX_WIRE_LEN);
        assert_eq!(
            OPEN_MENU_MAX_WIRE_LEN,
            WindowRequest::MAX_WIRE_LEN,
            "the widest open defines the endpoint's receive bound"
        );
        // And the hot path is untouched by carrying it.
        assert_eq!(sample_present().wire_len(), PRESENT_WIRE_LEN);
    }

    /// Every field an open states is bounded and checked, and a frame whose
    /// counts do not match its length is refused rather than read short.
    #[test]
    fn open_menu_refuses_every_malformed_frame() {
        let base = opening(&sample_titled_menu()).frame();
        assert!(WindowRequest::from_bytes(&base).is_ok());

        // Zero names no window.
        let mut nameless = base;
        put_u64(&mut nameless, 8, 0);
        assert_eq!(WindowRequest::from_bytes(&nameless), Err(Errno::OutOfRange));

        // An anchor whose far edge does not exist.
        let mut unplaceable = base;
        put_i32(&mut unplaceable, OPEN_MENU_ANCHOR_OFFSET, i32::MAX);
        assert_eq!(
            WindowRequest::from_bytes(&unplaceable),
            Err(Errno::LengthOutOfRange)
        );

        // Each of the three lengths is bounded before the frame's own length
        // is fixed against them, so an over-long count cannot be read into a
        // neighbouring field.
        for (at, over) in [
            (OPEN_MENU_TITLE_LEN_OFFSET, APP_MENU_LABEL_MAX + 1),
            (OPEN_MENU_ROW_COUNT_OFFSET, APP_MENU_MAX_TOTAL_ROWS + 1),
        ] {
            let mut greedy = base;
            greedy[at] = u8::try_from(over).expect("the bound fits a byte");
            assert_eq!(
                WindowRequest::from_bytes(&greedy),
                Err(Errno::LengthOutOfRange)
            );
        }
        let mut greedy = base;
        put_u16(
            &mut greedy,
            OPEN_MENU_TEXT_LEN_OFFSET,
            u16::try_from(APP_MENU_TEXT_BYTES + 1).expect("the bound fits"),
        );
        assert_eq!(
            WindowRequest::from_bytes(&greedy),
            Err(Errno::LengthOutOfRange)
        );

        // A count the frame's length contradicts.
        let mut miscounted = base;
        let text_len = read_u16(&miscounted, OPEN_MENU_TEXT_LEN_OFFSET);
        put_u16(&mut miscounted, OPEN_MENU_TEXT_LEN_OFFSET, text_len - 1);
        assert_eq!(WindowRequest::from_bytes(&miscounted), Err(Errno::BadMagic));

        // The title is held to the very validator a row label is: it lands in
        // session-drawn chrome, so an escape sequence in it is refused, never
        // sanitised.
        let mut escaped = base;
        let last = escaped.len() - 1;
        escaped[last] = 0x1b;
        assert_eq!(WindowRequest::from_bytes(&escaped), Err(Errno::OutOfRange));
    }

    /// An outcome is one of exactly three answers, each stating only the
    /// field its own case carries, so a session cannot name a chosen row and
    /// a refusal in the same breath.
    #[test]
    fn menu_outcomes_round_trip_and_fail_closed() {
        let outcomes = [
            MenuOutcome::Chosen(AppMenuItemId::new(7).expect("a valid id")),
            MenuOutcome::Dismissed,
            MenuOutcome::Refused(MenuRefusal::NoDisplay),
            MenuOutcome::Refused(MenuRefusal::SeatBusy),
            MenuOutcome::Refused(MenuRefusal::NoResources),
        ];
        for outcome in outcomes {
            let event = WindowEvent::MenuClosed {
                window_id: 4,
                open_id: 11,
                outcome,
            };
            let bytes = event.to_le_bytes();
            assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
            assert_eq!(event.window_id(), Some(4));
            // The one fixed event frame still holds it: an outcome costs no
            // other event a byte.
            assert_eq!(bytes.len(), WindowEvent::WIRE_LEN);
        }

        let base = WindowEvent::MenuClosed {
            window_id: 4,
            open_id: 11,
            outcome: MenuOutcome::Chosen(AppMenuItemId::new(7).expect("a valid id")),
        }
        .to_le_bytes();

        // An open id of zero answers no open.
        let mut nameless = base;
        put_u64(&mut nameless, 16, 0);
        assert_eq!(WindowEvent::from_bytes(&nameless), Err(Errno::OutOfRange));

        // An outcome discriminant outside the closed set, and a refusal
        // reason outside its own, are refused rather than guessed at.
        for kind in [0, 4, u16::MAX] {
            let mut unknown = base;
            put_u16(&mut unknown, MENU_CLOSED_OUTCOME_OFFSET, kind);
            assert_eq!(WindowEvent::from_bytes(&unknown), Err(Errno::OutOfRange));
        }
        for reason in [0, 4, u16::MAX] {
            let mut refused = base;
            put_u16(&mut refused, MENU_CLOSED_OUTCOME_OFFSET, 3);
            put_u16(&mut refused, MENU_CLOSED_ITEM_OFFSET, 0);
            put_u16(&mut refused, MENU_CLOSED_REFUSAL_OFFSET, reason);
            assert_eq!(WindowEvent::from_bytes(&refused), Err(Errno::OutOfRange));
        }

        // A chosen row with no id, and a case stating a field it does not
        // carry, are each refused.
        let mut idless = base;
        put_u16(&mut idless, MENU_CLOSED_ITEM_OFFSET, 0);
        assert_eq!(WindowEvent::from_bytes(&idless), Err(Errno::OutOfRange));
        for (kind, item, refusal) in [(1, 7, 1), (2, 7, 0), (2, 0, 1), (3, 7, 1)] {
            let mut crossed = base;
            put_u16(&mut crossed, MENU_CLOSED_OUTCOME_OFFSET, kind);
            put_u16(&mut crossed, MENU_CLOSED_ITEM_OFFSET, item);
            put_u16(&mut crossed, MENU_CLOSED_REFUSAL_OFFSET, refusal);
            assert_eq!(
                WindowEvent::from_bytes(&crossed),
                Err(Errno::OutOfRange),
                "kind {kind} may not state item {item} and refusal {refusal}"
            );
        }

        // The reserved tail past the outcome's own block stays zero.
        let mut dirty = base;
        dirty[MENU_CLOSED_WIRE_END] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    /// The reply that carries a session-minted id — a window's or one menu
    /// open's — refuses a corrupt frame and the never-minted zero.
    #[test]
    fn a_minted_id_reply_round_trips_and_fails_closed() {
        assert_eq!(
            decode_minted_id_reply(&encode_minted_id_reply(Ok(9))),
            Ok(9)
        );
        assert_eq!(
            decode_minted_id_reply(&encode_minted_id_reply(Ok(0))),
            Err(Errno::OutOfRange),
            "zero names no open"
        );
        assert_eq!(
            decode_minted_id_reply(&encode_minted_id_reply(Err(Errno::NotSupported))),
            Err(Errno::NotSupported)
        );
        assert_eq!(
            decode_minted_id_reply(&[0u8; WINDOW_MINTED_ID_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // The create reply is this frame plus the serving session's identity,
        // so the two cannot disagree about where the id sits.
        let create = encode_create_reply(Ok(9), server());
        assert_eq!(
            decode_minted_id_reply(&create[..WINDOW_MINTED_ID_REPLY_LEN]),
            Ok(9)
        );
    }

    /// A declaration's length follows its rows and their text, so the rows a
    /// menu does not have and the text they do not say cost nothing.
    #[test]
    fn app_bar_frames_grow_with_their_rows() {
        let empty = declaring(&AppMenu::EMPTY);
        let widest = declaring(&widest_menu());
        assert_eq!(empty.wire_len(), app_bar_wire_len(0, 0));
        assert_eq!(
            widest.wire_len(),
            app_bar_wire_len(APP_MENU_MAX_TOTAL_ROWS, APP_MENU_TEXT_BYTES),
            "the widest menu fills both bounds"
        );
        assert!(empty.wire_len() < widest.wire_len());
        assert!(widest.wire_len() <= WindowRequest::MAX_WIRE_LEN);
    }

    /// Encoding into a buffer that cannot hold the frame fails closed
    /// rather than writing a partial request.
    #[test]
    fn encode_refuses_a_short_buffer() {
        let request = sample_present();
        let mut out = [0u8; PRESENT_WIRE_LEN - 1];
        assert_eq!(request.encode(&mut out), Err(Errno::BufferTooSmall));
        assert!(out.iter().all(|&b| b == 0));
    }

    /// A menu's own title is the application's name for that menu, and a
    /// submenu's plate takes its parent row's label instead of stating one.
    ///
    /// The icon-bar declaration cannot carry a title at all: that menu is
    /// titled from the bundle's signed manifest, so a titled menu handed to
    /// it is refused rather than encoded and quietly retitled — an
    /// application must not be able to title system chrome as something it
    /// is not.
    #[test]
    fn a_menus_title_is_its_own_and_never_the_bars() {
        assert!(AppMenu::EMPTY.title().is_empty());
        let mut titled = AppMenu::titled(label("Edit"));
        assert_eq!(titled.title(), "Edit");
        titled.push(item(1, "Cut")).expect("room");
        assert_eq!(titled.title(), "Edit", "rows do not disturb the title");

        let mut out = [0u8; WindowRequest::MAX_WIRE_LEN];
        assert_eq!(
            declaring(&titled).encode(&mut out),
            Err(Errno::OutOfRange),
            "a declaration may not carry a title"
        );
        assert!(out.iter().all(|&b| b == 0), "and writes nothing");

        // The submenu row's label is the plate's title, so no decoded menu
        // has a title of its own.
        let decoded = WindowRequest::from_bytes(&declaring(&sample_menu()).frame());
        let Ok(WindowRequest::SetAppBar(bar)) = decoded else {
            panic!("the sample declaration decodes");
        };
        assert!(bar.menu.title().is_empty());
    }

    /// The rows a menu reports back are exactly the rows it was given, text
    /// and marking and depth included.
    #[test]
    fn a_menu_reports_the_rows_it_was_given() {
        let menu = sample_menu();
        assert_eq!(menu.rows().count(), menu.len());
        let row = |at: usize| menu.rows().nth(at).expect("a declared row");
        let AppMenuRowView::Item(first) = row(0).0 else {
            panic!("the first row is an item");
        };
        assert_eq!(first.label, "New window");
        assert_eq!(first.shortcut, "Ctrl Shift N");
        assert_eq!(first.reason, "");
        assert!(first.enabled);
        assert_eq!(first.role, AppMenuRole::Neutral);
        assert_eq!(row(0).1, None);

        let AppMenuRowView::Item(deep) = row(4).0 else {
            panic!("the nested row is an item");
        };
        assert_eq!(deep.label, "Green");
        assert_eq!(deep.reason, "No colour profile");
        assert_eq!(deep.mark, AppMenuMark::Radio);
        assert!(!deep.enabled);
        // The chain the parents spell: row 4 is on row 3's plate, row 3 on
        // row 1's, and row 1 on the root — three plates deep.
        assert_eq!(row(4).1, Some(3));
        assert_eq!(row(3).1, Some(1));
        assert_eq!(row(1).1, None);

        let AppMenuRowView::Item(last) = row(menu.len() - 1).0 else {
            panic!("the last row is an item");
        };
        assert_eq!(last.role, AppMenuRole::Destructive);
        assert!(matches!(row(5).0, AppMenuRowView::Separator));
        assert!(matches!(row(6).0, AppMenuRowView::Info));

        // Every field a menu reports is still a value its own validator
        // would accept, because the block holds only what one wrote.
        for (row, _) in menu.rows() {
            if let AppMenuRowView::Item(item) = row {
                assert!(AppMenuLabel::new(item.label).is_ok());
                assert!(AppMenuShortcut::new(item.shortcut).is_ok());
                assert!(AppMenuReason::new(item.reason).is_ok());
            }
        }
    }

    /// A submenu may hold a submenu, down to the depth bound and no further.
    ///
    /// The one-level model refused a nested submenu outright, so a chain
    /// could not be expressed at all — the bound was load-bearing in the
    /// builder rather than only in what the desktop chose to draw.
    #[test]
    fn submenus_nest_to_the_depth_bound() {
        let mut menu = AppMenu::EMPTY;
        menu.push(AppMenuRow::Submenu {
            label: label("Root"),
            enabled: true,
        })
        .expect("the root plate's submenu");
        let mut parent = 0;
        // A submenu opens the plate one deeper, so the deepest plate a
        // submenu may open is the last one within the bound.
        for depth in 2..APP_MENU_MAX_DEPTH {
            menu.push_under(
                AppMenuRow::Submenu {
                    label: label("Deeper"),
                    enabled: true,
                },
                parent,
            )
            .unwrap_or_else(|error| panic!("a submenu at depth {depth}: {error:?}"));
            parent = menu.len() - 1;
        }
        // One more plate would run past the bound, so the row that would
        // open it is refused rather than drawn opening nothing.
        assert_eq!(
            menu.push_under(
                AppMenuRow::Submenu {
                    label: label("Too deep"),
                    enabled: true,
                },
                parent
            ),
            Err(Errno::OutOfRange)
        );
        // Ordinary rows still fill the deepest plate, and the chain of
        // parents above the row runs the full depth.
        menu.push_under(item(1, "Deepest"), parent)
            .expect("a row on the deepest plate");
        let mut plates = 1;
        let mut at = menu.len() - 1;
        while let Some(above) = menu.rows().nth(at).and_then(|(_, parent)| parent) {
            at = above;
            plates += 1;
        }
        assert_eq!(plates, APP_MENU_MAX_DEPTH, "the chain runs the full depth");
    }

    #[test]
    fn app_menus_refuse_a_shape_no_menu_can_have() {
        // A zero item id names no row.
        assert_eq!(AppMenuItemId::new(0), Err(Errno::OutOfRange));

        // A labelled row must carry a label; a separator and an Info row
        // must not (their text is not the application's to write).
        let mut menu = AppMenu::EMPTY;
        assert_eq!(menu.push(item(1, "")), Err(Errno::OutOfRange));
        assert_eq!(
            menu.push(AppMenuRow::Submenu {
                label: label(""),
                enabled: true,
            }),
            Err(Errno::OutOfRange)
        );

        // A duplicate item id would make an outcome ambiguous.
        menu.push(item(1, "Row")).expect("the first row");
        assert_eq!(menu.push(item(1, "Again")), Err(Errno::OutOfRange));

        // At most one Info row: two info panels mean nothing.
        menu.push(AppMenuRow::Info).expect("the Info row");
        assert_eq!(menu.push(AppMenuRow::Info), Err(Errno::OutOfRange));

        // A parent must name an earlier submenu row, and the Info row is
        // always top-level.
        assert_eq!(menu.push_under(item(2, "Row"), 0), Err(Errno::OutOfRange));
        assert_eq!(menu.push_under(item(2, "Row"), 9), Err(Errno::OutOfRange));
        menu.push(AppMenuRow::Submenu {
            label: label("Plate"),
            enabled: true,
        })
        .expect("a submenu");
        let submenu = menu.len() - 1;
        assert_eq!(
            menu.push_under(AppMenuRow::Info, submenu),
            Err(Errno::OutOfRange)
        );
        menu.push_under(item(2, "Row"), submenu)
            .expect("a row inside it");

        // A plate fills and refuses independently of the menu's own total:
        // the root plate cannot outgrow one column even though the menu
        // holds more rows than a plate does.
        const { assert!(APP_MENU_MAX_ROWS < APP_MENU_MAX_TOTAL_ROWS) };
        let mut plates = AppMenu::EMPTY;
        plates
            .push(AppMenuRow::Submenu {
                label: label("Plate"),
                enabled: true,
            })
            .expect("a submenu to spill into");
        let mut id = 1u16;
        while plates.len() < APP_MENU_MAX_ROWS {
            plates
                .push(item(id, "Row"))
                .expect("room on the root plate");
            id += 1;
        }
        assert_eq!(plates.push(item(id, "Row")), Err(Errno::NoSpace));
        // The refused row fits the *other* plate, so it was the plate that
        // filled and not the menu.
        plates
            .push_under(item(id, "Row"), 0)
            .expect("room on the second plate");

        // The menu's own total is its own bound: spread over three plates
        // none of which is full, the only thing left to refuse is the menu.
        let mut full = AppMenu::EMPTY;
        for plate in 0..2 {
            full.push(AppMenuRow::Submenu {
                label: label("Plate"),
                enabled: true,
            })
            .unwrap_or_else(|error| panic!("plate {plate}: {error:?}"));
        }
        let mut id = 1u16;
        while full.len() < APP_MENU_MAX_TOTAL_ROWS {
            let row = item(id, "Row");
            match id % 3 {
                0 => full.push_under(row, 0),
                1 => full.push_under(row, 1),
                _ => full.push(row),
            }
            .unwrap_or_else(|error| panic!("room for row {id}: {error:?}"));
            id += 1;
        }
        assert_eq!(full.len(), APP_MENU_MAX_TOTAL_ROWS);
        for plate in [None, Some(0), Some(1)] {
            let row = item(u16::MAX, "Row");
            let refused = match plate {
                None => full.push(row),
                Some(parent) => full.push_under(row, parent),
            };
            assert_eq!(
                refused,
                Err(Errno::NoSpace),
                "the menu is full whichever plate the row would join"
            );
        }
    }

    /// A menu's text is bounded in total, so a hostile client cannot make a
    /// declaration arbitrarily wide by loading every row's fields.
    ///
    /// The refusal leaves the menu exactly as it was: a row is admitted with
    /// all of its text or none of it.
    #[test]
    fn a_menus_text_is_bounded_in_total() {
        let mut menu = AppMenu::EMPTY;
        let wide = label(&"l".repeat(APP_MENU_LABEL_MAX));
        let reason = AppMenuReason::new(&"r".repeat(APP_MENU_REASON_MAX)).expect("a wide reason");
        let mut id = 1u16;
        let mut refused = false;
        while menu.len() < APP_MENU_MAX_ROWS {
            let row = AppMenuRow::Item(
                AppMenuItem::new(AppMenuItemId::new(id).expect("a valid id"), wide)
                    .disabled()
                    .with_reason(reason),
            );
            if menu.push(row) == Err(Errno::NoSpace) {
                refused = true;
                break;
            }
            id += 1;
        }
        assert!(refused, "the text block fills before the row bound");
        let held = menu.rows().count();
        // The refused row left nothing behind: a shorter one still fits the
        // bytes the wide one could not.
        let before = declaring(&menu).wire_len();
        menu.push(item(id, "x")).expect("a row that fits");
        assert_eq!(
            declaring(&menu).wire_len(),
            before + APP_MENU_ROW_WIRE_LEN + 1
        );
        assert_eq!(menu.rows().count(), held + 1);
    }

    #[test]
    fn set_app_bar_refuses_every_malformed_frame() {
        let base = WindowRequest::SetAppBar(sample_app_bar()).frame();

        // The event route may not be a reserved (system-served) endpoint:
        // an application cannot ask for its bar events to be delivered to
        // one of the desktop's own rendezvous.
        let mut reserved = base;
        reserved[8..16].copy_from_slice(&WINDOW_ENDPOINT.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&reserved), Err(Errno::OutOfRange));

        // A click behaviour outside the closed set is refused rather than
        // read as one of them.
        let mut click = base;
        click[APP_BAR_CLICK_OFFSET] = 3;
        assert_eq!(WindowRequest::from_bytes(&click), Err(Errno::OutOfRange));

        // A row count or a text length past its bound is refused.
        let mut over = base;
        over[APP_BAR_ROW_COUNT_OFFSET] =
            u8::try_from(APP_MENU_MAX_TOTAL_ROWS + 1).expect("fits a byte");
        assert_eq!(
            WindowRequest::from_bytes(&over),
            Err(Errno::LengthOutOfRange)
        );
        let mut wide = base;
        put_u16(
            &mut wide,
            APP_BAR_TEXT_LEN_OFFSET,
            u16::try_from(APP_MENU_TEXT_BYTES + 1).expect("fits"),
        );
        assert_eq!(
            WindowRequest::from_bytes(&wide),
            Err(Errno::LengthOutOfRange)
        );

        // The frame ends with the last text byte, so a byte past it is a
        // smuggled field however innocuous — and the length alone refuses,
        // which is why a zero filler is refused too.
        let declared = usize::from(base[APP_BAR_ROW_COUNT_OFFSET]);
        let text = usize::from(read_u16(&base, APP_BAR_TEXT_LEN_OFFSET));
        assert_eq!(base.len(), app_bar_wire_len(declared, text));
        for filler in [0, 1] {
            assert_eq!(
                WindowRequest::from_bytes(&base.over_long(filler)),
                Err(Errno::BadMagic)
            );
        }

        // A count that does not match the frame's own length is refused
        // rather than read short: dropping the count with the frame intact
        // leaves rows the declaration no longer claims.
        let mut miscounted = base;
        miscounted[APP_BAR_ROW_COUNT_OFFSET] = u8::try_from(declared - 1).expect("fits");
        assert_eq!(WindowRequest::from_bytes(&miscounted), Err(Errno::BadMagic));

        // Text the rows do not claim cannot ride along: the block must be
        // consumed exactly, so shortening one row's label leaves a byte
        // over and is refused rather than ignored.
        let mut slack = base;
        slack[APP_BAR_ROWS_OFFSET + APP_MENU_ROW_LABEL_LEN_OFFSET] -= 1;
        assert_eq!(
            WindowRequest::from_bytes(&slack),
            Err(Errno::LengthOutOfRange)
        );

        // A row claiming more text than the block holds is refused rather
        // than read into the next row's.
        let mut greedy = base;
        greedy[APP_BAR_ROWS_OFFSET + APP_MENU_ROW_LABEL_LEN_OFFSET] = u8::MAX;
        assert_eq!(
            WindowRequest::from_bytes(&greedy),
            Err(Errno::LengthOutOfRange)
        );

        // The widest declaration is not quite the endpoint's receive bound:
        // a menu open carries the same menu under a title.
        const { assert!(APP_BAR_MAX_WIRE_LEN < WindowRequest::MAX_WIRE_LEN) };
    }

    #[test]
    fn every_app_bar_click_round_trips_and_names_who_takes_a_windowless_click() {
        for (click, opens) in [
            (AppBarClick::Raise, false),
            (AppBarClick::RaiseOrOpen, true),
            (AppBarClick::Open, true),
        ] {
            assert_eq!(AppBarClick::from_wire(click.to_wire()), Some(click));
            assert_eq!(click.opens_when_windowless(), opens);
            let declared = WindowRequest::SetAppBar(AppBar {
                click,
                ..sample_app_bar()
            });
            assert_eq!(
                WindowRequest::from_bytes(&declared.frame()),
                Ok(declared),
                "the declared behaviour survives the wire"
            );
        }
        assert_eq!(AppBarClick::from_wire(3), None, "the set is closed");
    }

    /// A decoded row is exactly a row the builder could have made: an
    /// unknown kind, an undefined flag bit, a field the row's kind does not
    /// use, and a shape the builder refuses are each refused here too.
    #[test]
    fn set_app_bar_refuses_a_row_no_builder_could_have_made() {
        let base = WindowRequest::SetAppBar(sample_app_bar()).frame();
        let record_at = |kind: u8| {
            (0..usize::from(base[APP_BAR_ROW_COUNT_OFFSET]))
                .map(|row| APP_BAR_ROWS_OFFSET + row * APP_MENU_ROW_WIRE_LEN)
                .find(|&at| base[at] == kind)
                .expect("the sample menu holds a row of this kind")
        };
        let separator = record_at(APP_MENU_KIND_SEPARATOR);
        let submenu = record_at(APP_MENU_KIND_SUBMENU);

        // An unknown row kind, an undefined row flag bit, and the unused
        // mark encoding are each refused.
        let mut kind = base;
        kind[APP_BAR_ROWS_OFFSET] = 0;
        assert_eq!(WindowRequest::from_bytes(&kind), Err(Errno::OutOfRange));
        let mut row_flags = base;
        row_flags[APP_BAR_ROWS_OFFSET + APP_MENU_ROW_FLAGS_OFFSET] |= 0b1_0000;
        assert_eq!(
            WindowRequest::from_bytes(&row_flags),
            Err(Errno::OutOfRange)
        );
        let mut mark = base;
        mark[APP_BAR_ROWS_OFFSET + APP_MENU_ROW_FLAGS_OFFSET] |= 0b110;
        assert_eq!(WindowRequest::from_bytes(&mark), Err(Errno::OutOfRange));

        // A field the row's kind does not use must be zero: the separator of
        // the sample menu carries no id, mark, role, or text of any kind.
        let mut marked_separator = base;
        marked_separator[separator + APP_MENU_ROW_FLAGS_OFFSET] |= APP_MENU_ROW_FLAG_ENABLED;
        assert_eq!(
            WindowRequest::from_bytes(&marked_separator),
            Err(Errno::OutOfRange)
        );
        let mut identified_separator = base;
        put_u16(
            &mut identified_separator,
            separator + APP_MENU_ROW_ID_OFFSET,
            7,
        );
        assert_eq!(
            WindowRequest::from_bytes(&identified_separator),
            Err(Errno::OutOfRange)
        );

        // A submenu row states no accelerator: the trailing column is its
        // chevron, so a caption there could only be a field smuggled past a
        // kind that does not use it.
        let mut captioned_submenu = base;
        captioned_submenu[submenu + APP_MENU_ROW_LABEL_LEN_OFFSET] -= 1;
        captioned_submenu[submenu + APP_MENU_ROW_SHORTCUT_LEN_OFFSET] = 1;
        assert_eq!(
            WindowRequest::from_bytes(&captioned_submenu),
            Err(Errno::OutOfRange)
        );

        // And a decoded menu obeys exactly the builder's shape rule: a row
        // claiming a parent that is not an earlier submenu is refused, and
        // so is a submenu past the depth bound.
        let mut bad_parent = base;
        bad_parent[APP_BAR_ROWS_OFFSET + APP_MENU_ROW_PARENT_OFFSET] = 0;
        assert_eq!(
            WindowRequest::from_bytes(&bad_parent),
            Err(Errno::OutOfRange)
        );
        let mut too_deep = base;
        too_deep[submenu + APP_MENU_ROW_WIRE_LEN * 2 + APP_MENU_ROW_PARENT_OFFSET] =
            u8::try_from(3).expect("fits");
        assert_ne!(
            WindowRequest::from_bytes(&too_deep),
            Ok(WindowRequest::SetAppBar(sample_app_bar())),
            "a re-parented submenu is not the sample declaration"
        );
    }

    /// Each of a row's three text fields is bounded by its own type on the
    /// way in, not merely by the block holding enough bytes.
    ///
    /// Checked with the block's total left intact — the length moved from one
    /// field to another — so it is the field's own bound that refuses and not
    /// the block running short.
    #[test]
    fn a_rows_text_field_is_bounded_by_its_own_type() {
        let mut menu = AppMenu::EMPTY;
        menu.push(AppMenuRow::Item(
            AppMenuItem::new(
                AppMenuItemId::new(1).expect("a valid id"),
                label(&"l".repeat(APP_MENU_LABEL_MAX)),
            )
            .with_shortcut(
                AppMenuShortcut::new(&"s".repeat(APP_MENU_SHORTCUT_MAX)).expect("the widest"),
            ),
        ))
        .expect("room");
        let base = declaring(&menu).frame();
        let row = APP_BAR_ROWS_OFFSET;

        // One byte of the caption re-labelled as label text: the block still
        // holds exactly what the rows claim, and the label is one past its
        // own bound.
        let mut over_label = base;
        over_label[row + APP_MENU_ROW_LABEL_LEN_OFFSET] += 1;
        over_label[row + APP_MENU_ROW_SHORTCUT_LEN_OFFSET] -= 1;
        assert_eq!(
            WindowRequest::from_bytes(&over_label),
            Err(Errno::LengthOutOfRange)
        );

        // And the other way about, so the caption's own bound refuses too.
        let mut over_shortcut = base;
        over_shortcut[row + APP_MENU_ROW_LABEL_LEN_OFFSET] -= 1;
        over_shortcut[row + APP_MENU_ROW_SHORTCUT_LEN_OFFSET] += 1;
        assert_eq!(
            WindowRequest::from_bytes(&over_shortcut),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn resize_request_enforces_bounds_and_a_clean_tail() {
        let base = WindowRequest::Resize {
            window_id: 3,
            shm_handle: 11,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        };
        // A zero window id is refused.
        let mut zero_id = base.frame();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        // A zero / over-large frame count.
        let mut zero_frames = base.frame();
        zero_frames[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_frames),
            Err(Errno::LengthOutOfRange)
        );
        // A zero extent, and a stride too small for one scanline.
        let mut zero_w = base.frame();
        zero_w[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_w),
            Err(Errno::LengthOutOfRange)
        );
        let mut short_stride = base.frame();
        short_stride[36..40].copy_from_slice(&2559u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&short_stride),
            Err(Errno::LengthOutOfRange)
        );
        // A byte past the format at offset 40 is past the operation's own
        // end, so the frame is over-long and refused.
        assert_eq!(
            WindowRequest::from_bytes(&base.frame().over_long(1)),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn resized_event_refuses_a_zero_extent_and_a_dirty_tail() {
        let base = WindowEvent::Resized {
            window_id: 4,
            width_px: 800,
            height_px: 600,
        };
        let mut zero_w = base.to_le_bytes();
        zero_w[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_w),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_h = base.to_le_bytes();
        zero_h[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_h),
            Err(Errno::LengthOutOfRange)
        );
        let mut dirty = base.to_le_bytes();
        dirty[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
        // Minimized carries no payload past the window id; its tail is dirty-checked.
        let mut minimized = WindowEvent::Minimized { window_id: 4 }.to_le_bytes();
        minimized[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&minimized), Err(Errno::BadMagic));
    }

    #[test]
    fn pick_file_refuses_a_zero_id_and_an_over_long_frame() {
        let mut zero_id = WindowRequest::PickFile { window_id: 9 }.frame();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        assert_eq!(
            WindowRequest::from_bytes(
                &WindowRequest::PickFile { window_id: 9 }
                    .frame()
                    .over_long(1)
            ),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn set_title_refuses_a_zero_id_a_malformed_title_and_an_over_long_frame() {
        let base = WindowRequest::SetTitle {
            window_id: 9,
            title: WindowTitle::new("Documents").expect("a valid title"),
        }
        .frame();
        let mut zero_id = base;
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        // A declared length past the fixed title block.
        let mut over_len = base;
        over_len[SET_TITLE_LEN_OFFSET] = u8::try_from(WINDOW_TITLE_MAX + 1).expect("fits a u8");
        assert_eq!(
            WindowRequest::from_bytes(&over_len),
            Err(Errno::LengthOutOfRange)
        );
        // A control character and invalid UTF-8 inside the declared text.
        let mut control_char = base;
        control_char[SET_TITLE_TEXT_OFFSET] = 0x1b;
        assert_eq!(
            WindowRequest::from_bytes(&control_char),
            Err(Errno::OutOfRange)
        );
        let mut invalid_utf8 = base;
        invalid_utf8[SET_TITLE_TEXT_OFFSET] = 0xFF;
        assert_eq!(
            WindowRequest::from_bytes(&invalid_utf8),
            Err(Errno::OutOfRange)
        );
        // A dirty byte past the declared text, and past the whole block.
        let title_len = usize::from(base[SET_TITLE_LEN_OFFSET]);
        let mut dirty_title_tail = base;
        dirty_title_tail[SET_TITLE_TEXT_OFFSET + title_len] = 1;
        assert_eq!(
            WindowRequest::from_bytes(&dirty_title_tail),
            Err(Errno::BadMagic)
        );
        assert_eq!(base.len(), SET_TITLE_WIRE_LEN);
        assert_eq!(
            WindowRequest::from_bytes(&base.over_long(1)),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn set_backdrop_blur_refuses_a_zero_id_an_over_large_radius_and_an_over_long_frame() {
        let base = WindowRequest::SetBackdropBlur {
            window_id: 9,
            radius_px: 4,
        };
        let mut zero_id = base.frame();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        let mut over_radius = base.frame();
        let over = WINDOW_BACKDROP_BLUR_MAX_PX + 1;
        over_radius[16..18].copy_from_slice(&over.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&over_radius),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            WindowRequest::from_bytes(&base.frame().over_long(1)),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn decode_fails_closed_on_malformed_framing() {
        let good = sample_create().frame();

        assert_eq!(
            WindowRequest::from_bytes(&good.truncated()),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(WindowRequest::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            WindowRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        // Neither the never-allocated zero nor a far-future operation
        // decodes: an unknown op is refused before its payload is read.
        for op in [0u8, 250] {
            let mut bad_op = good;
            bad_op[6] = op;
            assert_eq!(WindowRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn create_bounds_are_enforced() {
        let encode = |frame_count: u32, width: u32, height: u32, stride: u32, format: u8| {
            let mut bytes = sample_create().frame();
            bytes[24..28].copy_from_slice(&frame_count.to_le_bytes());
            bytes[28..32].copy_from_slice(&width.to_le_bytes());
            bytes[32..36].copy_from_slice(&height.to_le_bytes());
            bytes[36..40].copy_from_slice(&stride.to_le_bytes());
            bytes[40] = format;
            WindowRequest::from_bytes(&bytes)
        };
        assert_eq!(encode(0, 320, 200, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(
            encode(WINDOW_MAX_FRAMES + 1, 320, 200, 1280, 2),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(encode(2, 0, 200, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 0, 1280, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 200, 1279, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 320, 200, 1280, 9), Err(Errno::OutOfRange));
        assert!(encode(WINDOW_MAX_FRAMES, 320, 200, 1280, 2).is_ok());
    }

    #[test]
    fn create_carries_the_resizable_flag_and_rejects_a_dirty_flag_byte() {
        // The flag round-trips both ways.
        let resizable = sample_create_min(0, 0);
        let bytes = resizable.frame();
        assert_eq!(WindowRequest::from_bytes(&bytes), Ok(resizable));
        // The flag lives at the byte just past the title.
        assert_eq!(bytes[CREATE_RESIZABLE_OFFSET], 1);
        assert_eq!(sample_create().frame()[CREATE_RESIZABLE_OFFSET], 0);
        // A flag byte outside {0, 1} is refused, never coerced.
        let mut bad = sample_create().frame();
        bad[CREATE_RESIZABLE_OFFSET] = 2;
        assert_eq!(WindowRequest::from_bytes(&bad), Err(Errno::OutOfRange));
    }

    #[test]
    fn create_carries_the_declared_minimum_client_size() {
        let request = sample_create_min(240, 160);
        let bytes = request.frame();
        assert_eq!(WindowRequest::from_bytes(&bytes), Ok(request));
        // The pair follows the resizable flag, in that order.
        assert_eq!(
            bytes[CREATE_MIN_WIDTH_OFFSET..CREATE_MIN_WIDTH_OFFSET + 4],
            240u32.to_le_bytes()
        );
        assert_eq!(
            bytes[CREATE_MIN_HEIGHT_OFFSET..CREATE_MIN_HEIGHT_OFFSET + 4],
            160u32.to_le_bytes()
        );
        // Zero declares no minimum of the app's own, per axis, and the
        // widest floor an app can state survives the round trip intact.
        for (min_w, min_h) in [(0, 0), (240, 0), (0, 160), (u32::MAX, u32::MAX)] {
            let request = sample_create_min(min_w, min_h);
            assert_eq!(WindowRequest::from_bytes(&request.frame()), Ok(request));
        }
    }

    #[test]
    fn create_refuses_a_minimum_on_a_fixed_size_window() {
        // A window the window manager never resizes has nothing to measure
        // a floor against, so the contradiction is refused, not ignored.
        for (min_w, min_h) in [(240u32, 160u32), (240, 0), (0, 160)] {
            let mut bytes = sample_create().frame();
            bytes[CREATE_MIN_WIDTH_OFFSET..CREATE_MIN_WIDTH_OFFSET + 4]
                .copy_from_slice(&min_w.to_le_bytes());
            bytes[CREATE_MIN_HEIGHT_OFFSET..CREATE_MIN_HEIGHT_OFFSET + 4]
                .copy_from_slice(&min_h.to_le_bytes());
            assert_eq!(
                WindowRequest::from_bytes(&bytes),
                Err(Errno::LengthOutOfRange)
            );
        }
        // The fixed-size window that declares nothing still opens.
        assert!(WindowRequest::from_bytes(&sample_create().frame()).is_ok());
    }

    #[test]
    fn every_sizing_an_app_can_ask_for_survives_the_round_trip() {
        // A create an app can build but the session must refuse is a launch
        // that dies with no window, so the refusable combination has to be
        // unspellable rather than merely rejected: these are every sizing
        // that exists, and each decodes back to itself.
        for sizing in [
            WindowSizing::Fixed,
            WindowSizing::Resizable {
                min_width_px: 0,
                min_height_px: 0,
            },
            WindowSizing::Resizable {
                min_width_px: 240,
                min_height_px: 160,
            },
            WindowSizing::Resizable {
                min_width_px: u32::MAX,
                min_height_px: u32::MAX,
            },
        ] {
            let request = sample_create_sized(sizing);
            assert_eq!(
                WindowRequest::from_bytes(&request.frame()),
                Ok(request),
                "{sizing:?} encodes a frame the session would refuse"
            );
        }
        // An app that states no sizing gets the window that is never
        // resized, not a resizable one with no floor.
        assert_eq!(WindowSizing::default(), WindowSizing::Fixed);
    }

    #[test]
    fn create_refuses_a_truncated_or_dirty_minimum() {
        let bytes = sample_create_min(240, 160).frame();
        // A frame cut short is refused whole, never decoded from the part
        // that arrived — including one cut inside the minimum itself.
        for short in [CREATE_WIRE_LEN - 1, CREATE_MIN_WIDTH_OFFSET + 2] {
            assert_eq!(
                WindowRequest::from_bytes(&bytes[..short]),
                Err(Errno::BufferTooSmall)
            );
        }
        assert_eq!(bytes.len(), CREATE_WIRE_LEN);
        assert_eq!(
            WindowRequest::from_bytes(&bytes.over_long(1)),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn create_refuses_a_reserved_event_endpoint() {
        let mut bytes = sample_create().frame();
        bytes[16..24].copy_from_slice(&SEATMGR_ENDPOINT.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&bytes), Err(Errno::OutOfRange));
        let mut own = sample_create().frame();
        own[16..24].copy_from_slice(&WINDOW_ENDPOINT.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&own), Err(Errno::OutOfRange));
    }

    #[test]
    fn create_popup_enforces_bounds_a_parent_id_an_endpoint_and_a_clean_tail() {
        let base = sample_create_popup();
        // A reserved event endpoint is refused, exactly as `Create`.
        let mut reserved_endpoint = base.frame();
        reserved_endpoint[16..24].copy_from_slice(&SEATMGR_ENDPOINT.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&reserved_endpoint),
            Err(Errno::OutOfRange)
        );
        // The shared frame-layout bounds still hold: a zero/over-large
        // frame count, a zero extent, a stride too small for one scanline.
        let mut zero_frames = base.frame();
        zero_frames[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_frames),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_w = base.frame();
        zero_w[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_w),
            Err(Errno::LengthOutOfRange)
        );
        let mut short_stride = base.frame();
        short_stride[36..40].copy_from_slice(&639u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&short_stride),
            Err(Errno::LengthOutOfRange)
        );
        // A zero parent window id is refused: a popup must name a real
        // window it is anchored above.
        let mut zero_parent = base.frame();
        zero_parent[super::POPUP_PARENT_OFFSET..super::POPUP_PARENT_OFFSET + 8]
            .copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_parent),
            Err(Errno::OutOfRange)
        );
        assert_eq!(base.frame().len(), CREATE_POPUP_WIRE_LEN);
        assert_eq!(
            WindowRequest::from_bytes(&base.frame().over_long(1)),
            Err(Errno::BadMagic)
        );
    }

    #[test]
    fn create_popup_carries_signed_offsets_including_negatives() {
        let request = sample_create_popup();
        let bytes = request.frame();
        // The offsets sit just past the parent id and round-trip signed.
        assert_eq!(
            i32::from_le_bytes([
                bytes[super::POPUP_OFFSET_X],
                bytes[super::POPUP_OFFSET_X + 1],
                bytes[super::POPUP_OFFSET_X + 2],
                bytes[super::POPUP_OFFSET_X + 3],
            ]),
            -12
        );
        assert_eq!(WindowRequest::from_bytes(&bytes), Ok(request));
    }

    /// The desktop the query tests round-trip.
    fn sample_desktop() -> DesktopInfo {
        match DesktopInfo::new(1024, 768, 100, Appearance::Dark) {
            Ok(info) => info,
            Err(_) => unreachable!("a 1024x768 screen at 100% is in range"),
        }
    }

    #[test]
    fn the_desktop_query_round_trips_and_names_no_window() {
        let request = WindowRequest::QueryDesktop;
        assert_eq!(WindowRequest::from_bytes(&request.frame()), Ok(request));
        // The one request with no operands at all: its frame is the header,
        // so a smuggled window id or payload cannot ride along unread.
        assert_eq!(request.wire_len(), REQUEST_HEADER_LEN);
    }

    #[test]
    fn the_desktop_reply_round_trips_and_fails_closed() {
        let desktop = sample_desktop();
        let session = ProcId::KERNEL;
        assert_eq!(
            decode_desktop_reply(&encode_desktop_reply(Ok(desktop), session)),
            Ok((desktop, session))
        );
        assert_eq!(
            decode_desktop_reply(&encode_desktop_reply(Err(Errno::PermissionDenied), session)),
            Err(Errno::PermissionDenied)
        );

        let good = encode_desktop_reply(Ok(desktop), session);
        assert_eq!(
            decode_desktop_reply(&good[..WINDOW_DESKTOP_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A refusal reply carries no record, and a success reply whose
        // record is blank is not a desktop: neither can be read as one.
        assert!(decode_desktop_reply(&[0u8; WINDOW_DESKTOP_REPLY_LEN]).is_err());
        // The record's own reserved bytes still fail closed; the identity
        // now sits after it, so the dirty byte is taken inside the record.
        let mut dirty = good;
        dirty[DESKTOP_REPLY_SERVER_OFFSET - 1] = 1;
        assert_eq!(decode_desktop_reply(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn a_desktop_change_event_round_trips_and_fails_closed() {
        let event = WindowEvent::DesktopChanged {
            window_id: 7,
            desktop: sample_desktop(),
        };
        let wire = event.to_le_bytes();
        assert_eq!(WindowEvent::from_bytes(&wire), Ok(event));
        assert_eq!(event.window_id(), Some(7));

        // The record ends well before the frame does; the tail past it
        // must be zero.
        let mut dirty = wire;
        dirty[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
        // A malformed record inside a well-formed frame is refused, not
        // clamped to something plausible.
        let mut blank = wire;
        blank[16..16 + DesktopInfo::WIRE_LEN].fill(0);
        assert_eq!(WindowEvent::from_bytes(&blank), Err(Errno::OutOfRange));
    }

    #[test]
    fn create_refuses_a_malformed_title() {
        // An over-long claimed length.
        let mut long = sample_create().frame();
        long[41] = u8::try_from(WINDOW_TITLE_MAX + 1).expect("a small test constant");
        assert_eq!(
            WindowRequest::from_bytes(&long),
            Err(Errno::LengthOutOfRange)
        );
        // Bytes past the claimed length must be zero.
        let mut dirty = sample_create().frame();
        dirty[42 + 10] = b'x';
        assert_eq!(WindowRequest::from_bytes(&dirty), Err(Errno::BadMagic));
        // Invalid UTF-8 inside the claimed length.
        let mut bad_utf8 = sample_create().frame();
        bad_utf8[42] = 0xFF;
        assert_eq!(WindowRequest::from_bytes(&bad_utf8), Err(Errno::OutOfRange));
        // A control character inside the claimed length.
        let mut control = sample_create().frame();
        control[42] = 0x1B;
        assert_eq!(WindowRequest::from_bytes(&control), Err(Errno::OutOfRange));
    }

    #[test]
    fn present_refuses_an_empty_damage_rectangle_and_a_zero_id() {
        let mut zero_width = sample_present().frame();
        zero_width[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_width),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_height = sample_present().frame();
        zero_height[32..36].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_height),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_id = sample_present().frame();
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowRequest::from_bytes(&zero_id), Err(Errno::OutOfRange));
        let mut zero_close = WindowRequest::Close { window_id: 9 }.frame();
        zero_close[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            WindowRequest::from_bytes(&zero_close),
            Err(Errno::OutOfRange)
        );
    }

    /// The serving session identity the reply tests stamp.
    fn server() -> ProcId {
        ProcId::from_raw([0x5A; 16])
    }

    #[test]
    fn create_replies_round_trip_ok_and_error() {
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(42), server())),
            Ok((42, server()))
        );
        assert_eq!(
            decode_create_reply(&encode_create_reply(Err(Errno::NoSpace), server())),
            Err(Errno::NoSpace)
        );
        assert_eq!(
            decode_create_reply(&encode_create_reply(Err(Errno::PermissionDenied), server())),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn create_reply_decode_fails_closed() {
        let good = encode_create_reply(Ok(42), server());
        assert_eq!(
            decode_create_reply(&good[..WINDOW_CREATE_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A corrupt (positive) status word.
        let mut bad_status = good;
        bad_status[0] = 1;
        assert_eq!(decode_create_reply(&bad_status), Err(Errno::OutOfRange));
        // A "successful" reply carrying the never-minted zero id.
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(0), server())),
            Err(Errno::OutOfRange)
        );
        // A "successful" reply carrying the kernel-reserved all-zero
        // server identity: an app must never accept an event stream it
        // cannot authenticate.
        assert_eq!(
            decode_create_reply(&encode_create_reply(Ok(42), ProcId::KERNEL)),
            Err(Errno::OutOfRange)
        );
    }

    fn sample_key() -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char('q'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::default()
            },
        }
    }

    #[test]
    fn events_round_trip() {
        for event in [
            WindowEvent::Focus {
                window_id: 4,
                focused: true,
            },
            WindowEvent::Focus {
                window_id: 4,
                focused: false,
            },
            WindowEvent::Key {
                window_id: 4,
                key: sample_key(),
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 17,
                y: 23,
                action: PointerAction::Moved,
                modifiers: Modifiers::default(),
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 0,
                y: 0,
                action: PointerAction::Pressed(PointerButtonCode::Primary),
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
            },
            WindowEvent::Pointer {
                window_id: 4,
                x: 1,
                y: 2,
                action: PointerAction::Released(PointerButtonCode::Middle),
                modifiers: Modifiers {
                    ctrl: true,
                    alt: true,
                    meta: true,
                    ..Modifiers::default()
                },
            },
            WindowEvent::CloseRequested { window_id: 4 },
            WindowEvent::AlternateCloseRequested { window_id: 4 },
            WindowEvent::FilePicked {
                window_id: 4,
                handle: 7,
            },
            WindowEvent::PickCancelled { window_id: 4 },
            WindowEvent::Minimized { window_id: 4 },
            WindowEvent::Resized {
                window_id: 4,
                width_px: 800,
                height_px: 600,
            },
            WindowEvent::Scrolled {
                window_id: 4,
                dx: 0,
                dy: 3,
            },
            WindowEvent::Scrolled {
                window_id: 4,
                dx: -2,
                dy: -5,
            },
            WindowEvent::RedrawRequested { window_id: 4 },
            WindowEvent::ContentReleased { window_id: 4 },
        ] {
            let bytes = event.to_le_bytes();
            assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
            assert_eq!(event.window_id(), Some(4));
        }
    }

    /// The two release-related events are distinct on the wire, because they
    /// ask for opposite things: one to draw, one to let go.
    #[test]
    fn a_release_is_not_a_redraw_request() {
        let released = WindowEvent::ContentReleased { window_id: 9 };
        let redraw = WindowEvent::RedrawRequested { window_id: 9 };
        assert_ne!(released.to_le_bytes(), redraw.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&released.to_le_bytes()),
            Ok(released)
        );
    }

    #[test]
    fn icon_bar_events_round_trip_and_address_no_window() {
        for event in [
            WindowEvent::AppBarDefault,
            WindowEvent::AppBarMenu {
                item: AppMenuItemId::new(1).expect("a valid id"),
            },
            WindowEvent::AppBarMenu {
                item: AppMenuItemId::new(u16::MAX).expect("a valid id"),
            },
        ] {
            let bytes = event.to_le_bytes();
            assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
            // The application is the subject, so there is no window to name
            // — and the wire field stays zero to say exactly that.
            assert_eq!(event.window_id(), None);
            assert_eq!(&bytes[8..16], &[0u8; 8]);
        }
    }

    #[test]
    fn an_icon_bar_event_naming_a_window_is_refused() {
        // A window id on an application-scoped event is a second,
        // contradictory addressing of it, so it is refused rather than
        // silently ignored.
        let mut named = WindowEvent::AppBarDefault.to_le_bytes();
        named[8..16].copy_from_slice(&4u64.to_le_bytes());
        assert_eq!(WindowEvent::from_bytes(&named), Err(Errno::OutOfRange));

        // A zero item id names no row and is refused, never guessed at.
        let mut zero_item = WindowEvent::AppBarMenu {
            item: AppMenuItemId::new(3).expect("a valid id"),
        }
        .to_le_bytes();
        zero_item[16..18].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(WindowEvent::from_bytes(&zero_item), Err(Errno::OutOfRange));
    }

    #[test]
    fn scroll_events_carry_signed_ticks_and_fail_closed_on_a_dirty_tail() {
        let event = WindowEvent::Scrolled {
            window_id: 9,
            dx: -7,
            dy: 11,
        };
        let bytes = event.to_le_bytes();
        assert_eq!(WindowEvent::from_bytes(&bytes), Ok(event));
        // The 8 bytes past the two i32 ticks are reserved and must be zero.
        let mut dirty = bytes;
        dirty[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&dirty), Err(Errno::BadMagic));
    }

    #[test]
    fn pick_events_fail_closed_on_a_zero_handle_and_dirty_tails() {
        // A "picked" event must carry a redeemable (non-zero) handle.
        let mut zero_handle = WindowEvent::FilePicked {
            window_id: 4,
            handle: 7,
        }
        .to_le_bytes();
        zero_handle[16..24].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            WindowEvent::from_bytes(&zero_handle),
            Err(Errno::OutOfRange)
        );
        // Reserved tails must be zero for both conclusions.
        let mut picked = WindowEvent::FilePicked {
            window_id: 4,
            handle: 7,
        }
        .to_le_bytes();
        picked[24] = 1;
        assert_eq!(WindowEvent::from_bytes(&picked), Err(Errno::BadMagic));
        let mut cancelled = WindowEvent::PickCancelled { window_id: 4 }.to_le_bytes();
        cancelled[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&cancelled), Err(Errno::BadMagic));
    }

    #[test]
    fn event_decode_fails_closed_on_malformed_framing() {
        let good = WindowEvent::CloseRequested { window_id: 4 }.to_le_bytes();

        assert_eq!(
            WindowEvent::from_bytes(&good[..WindowEvent::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(WindowEvent::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            WindowEvent::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_kind = good;
        bad_kind[6] = 99;
        assert_eq!(WindowEvent::from_bytes(&bad_kind), Err(Errno::OutOfRange));
        let mut zero_id = good;
        zero_id[8..16].copy_from_slice(&0u64.to_le_bytes());
        assert_eq!(WindowEvent::from_bytes(&zero_id), Err(Errno::OutOfRange));
    }

    #[test]
    fn event_decode_refuses_dirty_reserved_tails() {
        let mut focus = WindowEvent::Focus {
            window_id: 4,
            focused: true,
        }
        .to_le_bytes();
        focus[17] = 1;
        assert_eq!(WindowEvent::from_bytes(&focus), Err(Errno::BadMagic));
        let mut key = WindowEvent::Key {
            window_id: 4,
            key: sample_key(),
        }
        .to_le_bytes();
        key[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&key), Err(Errno::BadMagic));
        let mut pointer = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        pointer[30] = 1;
        assert_eq!(WindowEvent::from_bytes(&pointer), Err(Errno::BadMagic));
        let mut close = WindowEvent::CloseRequested { window_id: 4 }.to_le_bytes();
        close[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&close), Err(Errno::BadMagic));
        let mut alternate = WindowEvent::AlternateCloseRequested { window_id: 4 }.to_le_bytes();
        alternate[16] = 1;
        assert_eq!(WindowEvent::from_bytes(&alternate), Err(Errno::BadMagic));
        let mut redraw = WindowEvent::RedrawRequested { window_id: 4 }.to_le_bytes();
        redraw[WindowEvent::WIRE_LEN - 1] = 1;
        assert_eq!(WindowEvent::from_bytes(&redraw), Err(Errno::BadMagic));
    }

    #[test]
    fn event_decode_refuses_inconsistent_payloads() {
        // A focus flag outside {0, 1}.
        let mut focus = WindowEvent::Focus {
            window_id: 4,
            focused: true,
        }
        .to_le_bytes();
        focus[16] = 2;
        assert_eq!(WindowEvent::from_bytes(&focus), Err(Errno::OutOfRange));
        // A malformed embedded key record (bad key magic).
        let mut key = WindowEvent::Key {
            window_id: 4,
            key: sample_key(),
        }
        .to_le_bytes();
        key[16] ^= 0xFF;
        assert_eq!(WindowEvent::from_bytes(&key), Err(Errno::BadMagic));
        // A button on a move.
        let mut moved = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        moved[26] = 1;
        assert_eq!(WindowEvent::from_bytes(&moved), Err(Errno::OutOfRange));
        // No button on a press, and an unknown pointer action.
        let mut pressed = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Pressed(PointerButtonCode::Primary),
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        pressed[26] = 0;
        assert_eq!(WindowEvent::from_bytes(&pressed), Err(Errno::OutOfRange));
        let mut bad_action = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bad_action[24] = 9;
        assert_eq!(WindowEvent::from_bytes(&bad_action), Err(Errno::OutOfRange));
        // A modifier bit outside the defined mask.
        let mut bad_modifiers = WindowEvent::Pointer {
            window_id: 4,
            x: 1,
            y: 2,
            action: PointerAction::Moved,
            modifiers: Modifiers::default(),
        }
        .to_le_bytes();
        bad_modifiers[28] = 0x80;
        assert_eq!(
            WindowEvent::from_bytes(&bad_modifiers),
            Err(Errno::OutOfRange)
        );
    }
}
