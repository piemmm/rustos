//! The "Open With…" type→bundle association model (`plans/NEW-FILEMANAGER.md`
//! `FM6b`).
//!
//! When the user asks to open a regular file with a chosen application, the
//! file manager offers the installed bundles whose signed `AppInfo` claims the
//! file's type. This module is the **pure model** behind that offer, host-proven
//! without a kernel exactly as the [`Activation`](crate::activate) decision is:
//!
//! * [`applications_for`] derives a file's content type from its filename
//!   extension through the shared content-type registry
//!   ([`media_for_name`]) — the one bridge from a
//!   name (all the VFS listing gives us) to the media-type vocabulary a bundle
//!   declares its associations in. Because that registry is also what the icon
//!   classifier draws from, the applications offered and the glyph shown can
//!   never drift apart. It is a display *hint*, never authority: it decides
//!   which applications are *offered*, and the load gate still verifies and
//!   capability-checks whichever one the user picks.
//! * [`BundleSource`] is the injected enumeration seam — the installed-bundle
//!   analogue of [`DirectorySource`](crate::source). On a running system it is
//!   backed by the app store (each bundle's `AppInfo` MIME table); in tests it
//!   is an in-memory list, so the matching logic is exercised without a kernel.
//! * [`applications_for`] selects the bundles that handle a file's type or any
//!   broader type it is a subclass of
//!   ([`MediaType::parent`](crate::media::MediaType::parent)), so a text editor
//!   declaring `text/plain` is offered for a `.rs` file while an application
//!   declaring `text/x-rust` is offered ahead of it. Bundles that declare the
//!   same type keep the source's order. No match is an **honest empty answer**
//!   — the caller shows a "no application" notice, never a crash and never a
//!   fabricated default.
//!
//! * [`OpenWithChooser`] is the surface the user picks on: the ranked
//!   candidates, a selection, and a scroll offset. It is deliberately **not** a
//!   menu — the set grows with the applications a user installs, so no menu
//!   plate can promise to hold it (`plans/NEW-MENUS.md` §6, decision 2).
//!
//! The engine holds no launch authority: it *names* the candidate bundles and
//! *what should happen*; spawning the chosen bundle through the signed load gate
//! stays in the file manager's own capability-checked tail under the user's
//! identity (so the read-only picker, which composes the same engine, never
//! launches). Deciding a file's type here never opens it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{mime_type_at, AppInfoHeader, Errno};
use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::ScrollBar;

use crate::media::{ancestry, media_for_name};

/// One installed application and the file types its signed `AppInfo` claims to
/// open — a single "Open With…" candidate.
///
/// The [`mime_types`](Self::mime_types) are the bundle's *own* declared
/// associations (`AppInfo`'s MIME table), never a registry the file manager
/// invents: the manager reads what each bundle claims and offers only those.
/// [`bundle_path`](Self::bundle_path) is the absolute path of the `<Name>.app`
/// directory the caller launches through the ordinary signed load gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppAssociation {
    name: String,
    bundle_path: String,
    mime_types: Vec<String>,
}

impl AppAssociation {
    /// Construct an association from a bundle's display name, the absolute path
    /// of its `<Name>.app` directory, and the MIME types its `AppInfo` declares.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        bundle_path: impl Into<String>,
        mime_types: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            bundle_path: bundle_path.into(),
            mime_types,
        }
    }

    /// The bundle's human-readable name — the "Open With…" menu label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The absolute path of the `<Name>.app` bundle to launch through the
    /// signed load gate.
    #[must_use]
    pub fn bundle_path(&self) -> &str {
        &self.bundle_path
    }

    /// The MIME types the bundle's `AppInfo` declares it can open.
    #[must_use]
    pub fn mime_types(&self) -> &[String] {
        &self.mime_types
    }

    /// Whether this bundle declares an association with `mime`, matched
    /// ASCII-case-insensitively so a type reads the same however it was cased.
    ///
    /// This is the bundle's *own* declaration, tested exactly: a bundle that
    /// declares only `text/plain` does not "handle" `text/x-rust` here.
    /// Offering it for a Rust file is [`applications_for`]'s job, which walks
    /// the subclass chain and asks this question once per broader type.
    #[must_use]
    pub fn handles(&self, mime: &str) -> bool {
        self.mime_types
            .iter()
            .any(|declared| declared.eq_ignore_ascii_case(mime))
    }
}

/// Build an [`AppAssociation`] from a bundle's raw `AppInfo` manifest bytes and
/// its `<Name>.app` directory path.
///
/// This is the pure decode the running-system [`BundleSource`] uses per bundle:
/// it reads the manifest header and the declared MIME table (the same body
/// layout the loader reads) and returns the bundle's name and declared types.
/// It is **fail-closed** — a manifest that does not parse, or whose MIME table
/// is malformed or non-UTF-8, yields `None`, so a corrupt bundle is silently
/// skipped rather than offered on a guess. The MIME set is a display *hint*
/// only: this does **not** verify the manifest signature (the signed load gate
/// does that when the chosen bundle is launched), it only reads what the
/// bundle claims. Keeping the decode here means it is host-tested without a
/// kernel, exactly like the rest of this model.
#[must_use]
pub fn association_from_appinfo(bundle_path: &str, appinfo: &[u8]) -> Option<AppAssociation> {
    let header = AppInfoHeader::from_bytes(appinfo).ok()?;
    let body = appinfo.get(AppInfoHeader::WIRE_LEN..)?;
    let caps = usize::from(header.capability_count);
    let mut mimes = Vec::with_capacity(usize::from(header.mime_count));
    for index in 0..usize::from(header.mime_count) {
        mimes.push(mime_type_at(body, caps, index).ok()?.to_string());
    }
    Some(AppAssociation::new(
        header.bundle_name(),
        bundle_path,
        mimes,
    ))
}

/// The installed-application enumeration seam — the "Open With…" analogue of
/// [`DirectorySource`](crate::source).
///
/// It is the one thing the association model needs from the outside world: the
/// installed bundles and the file types each declares. Keeping it a trait means
/// the matching logic is exhaustively testable against an in-memory list without
/// a kernel, exactly as the browser's directory reads are.
///
/// On a running system the source is backed by the app store, reading each
/// bundle's signed `AppInfo` MIME table under the caller's own identity — the
/// permission decision stays in the store behind the seam, never here.
pub trait BundleSource {
    /// Enumerate the installed applications and their declared file-type
    /// associations.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the app store cannot be
    /// enumerated (for example [`Errno::PermissionDenied`]).
    fn installed_bundles(&mut self) -> Result<Vec<AppAssociation>, Errno>;
}

/// The installed applications that can open a file named `name`, most specific
/// declaration first — the "Open With…" candidate list.
///
/// The file's type is derived by the shared content-type registry
/// ([`media_for_name`]) and named by its media-type spelling
/// ([`MediaType::as_str`](crate::media::MediaType::as_str)), so the association
/// vocabulary is exactly the one the icon classifier draws from — the two can
/// never drift apart.
///
/// A bundle is offered when it [`handles`](AppAssociation::handles) that type
/// **or any broader type it is a subclass of**
/// ([`MediaType::parent`](crate::media::MediaType::parent)): an editor
/// declaring `text/plain` opens a `.rs` file, because Rust source is readable
/// text. Candidates are ordered by how specifically they claim the file — an
/// application declaring the file's own type comes before one declaring an
/// ancestor — and bundles claiming at the same level keep `bundles`'
/// enumeration order, so no existing ordering is disturbed.
///
/// The result is empty — an honest "no application" answer — when the file's
/// type is unrecognised or no installed bundle claims it or any of its broader
/// types; it never falls back to a guessed default.
#[must_use]
pub fn applications_for<'a>(name: &str, bundles: &'a [AppAssociation]) -> Vec<&'a AppAssociation> {
    let Some(media) = media_for_name(name) else {
        return Vec::new();
    };
    let mut ranked: Vec<(usize, &AppAssociation)> = bundles
        .iter()
        .filter_map(|bundle| {
            ancestry(media)
                .position(|claim| bundle.handles(claim.as_str()))
                .map(|distance| (distance, bundle))
        })
        .collect();
    ranked.sort_by_key(|(distance, _)| *distance);
    ranked.into_iter().map(|(_, bundle)| bundle).collect()
}
/// One candidate application the "Open With…" chooser offers: what the row
/// says, and the bundle a chosen row launches.
///
/// The chooser holds its own copies rather than borrowing the enumerated
/// associations, because the enumeration is a one-shot read of the app store
/// and the chooser outlives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenWithCandidate {
    name: String,
    bundle_path: String,
}

impl OpenWithCandidate {
    /// The bundle's human-readable name — the chooser row's label.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The absolute path of the `<Name>.app` bundle to launch through the
    /// signed load gate.
    #[must_use]
    pub fn bundle_path(&self) -> &str {
        &self.bundle_path
    }
}

/// The open "Open With…" chooser: the candidate applications, which one is
/// current, and where the list is scrolled to.
///
/// **This is not a menu** (`plans/NEW-MENUS.md` §6, decision 2). The candidate
/// set is as long as the applications a user has installed, so no format bound
/// can promise a plate holds it, and the desktop's menu model crosses the wire
/// complete, so no row of one can be filled in lazily. A chooser over an
/// unbounded set is a *list*, and a list scrolls: this one is the file
/// manager's own modal surface, reached by the one context-menu row that
/// concludes the chain.
///
/// It is a pure model — the candidates, a selection, and a scroll offset — so
/// what the chooser *decides* is host-proven, exactly as the association
/// matching above it is. It performs nothing: launching the chosen bundle is
/// the file manager's own capability-checked hand-off under the user's
/// identity, so composing it grants no authority (the read-only picker never
/// launches, so it never builds one).
#[derive(Clone, Debug)]
pub struct OpenWithChooser {
    candidates: Vec<OpenWithCandidate>,
    file_path: String,
    display_name: String,
    selected: usize,
    offset: u64,
    bar: ScrollBar,
}

impl OpenWithChooser {
    /// Open a chooser over `apps` — the candidates [`applications_for`]
    /// returned, in that ranked order — for the file at absolute `file_path`,
    /// whose leaf name is `display_name`.
    ///
    /// `None` when `apps` is empty: no installed application claiming the type
    /// is an honest answer the caller states, never an empty chooser.
    #[must_use]
    pub fn new(
        apps: &[&AppAssociation],
        file_path: impl Into<String>,
        display_name: impl Into<String>,
    ) -> Option<Self> {
        if apps.is_empty() {
            return None;
        }
        Some(Self {
            candidates: apps
                .iter()
                .map(|app| OpenWithCandidate {
                    name: app.name().to_string(),
                    bundle_path: app.bundle_path().to_string(),
                })
                .collect(),
            file_path: file_path.into(),
            display_name: display_name.into(),
            selected: 0,
            offset: 0,
            bar: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
        })
    }

    /// The candidates, most specific claim first.
    #[must_use]
    pub fn candidates(&self) -> &[OpenWithCandidate] {
        &self.candidates
    }

    /// Which candidate is current.
    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    /// The current candidate — what activating the chooser launches.
    ///
    /// A chooser is never built over an empty list and [`select`](Self::select)
    /// clamps, so this always answers; it reads the row rather than indexing
    /// it, so an index that somehow left the list refuses instead of faulting.
    #[must_use]
    pub fn chosen(&self) -> Option<&OpenWithCandidate> {
        self.candidates.get(self.selected)
    }

    /// The absolute path of the file the chosen application opens.
    #[must_use]
    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    /// The file's leaf name — the title handed to the launched application.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The first candidate row the list shows.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Make `index` current, clamped to the candidates, reporting whether the
    /// selection moved.
    pub fn select(&mut self, index: usize) -> bool {
        let clamped = index.min(self.candidates.len().saturating_sub(1));
        let moved = clamped != self.selected;
        self.selected = clamped;
        moved
    }

    /// Move the selection by `delta` rows (positive moves toward the end),
    /// stopping at either end, reporting whether it moved.
    pub fn step(&mut self, delta: i64) -> bool {
        let from = i64::try_from(self.selected).unwrap_or(i64::MAX);
        let to = from.saturating_add(delta).max(0);
        self.select(usize::try_from(to).unwrap_or(usize::MAX))
    }

    /// The scroll geometry for a list showing `visible` rows at a time, in row
    /// units, over the shared [`ScrollRange`] normalisation — so an offset can
    /// never exceed what the list holds.
    #[must_use]
    pub fn scroll_range(&self, visible: usize) -> ScrollRange {
        ScrollRange::new(
            u64::try_from(self.candidates.len()).unwrap_or(u64::MAX),
            u64::try_from(visible).unwrap_or(u64::MAX),
            self.offset,
        )
    }

    /// The scroll model the drawn bar and the wheel both move through: one row
    /// per line, one list per page.
    #[must_use]
    pub fn scroll_model(&self, visible: usize) -> ScrollModel {
        let page = u64::try_from(visible.max(1)).unwrap_or(u64::MAX);
        ScrollModel::new(self.scroll_range(visible), 1, page)
    }

    /// Scroll so `offset` is the first visible row, clamped through
    /// [`scroll_range`](Self::scroll_range), reporting whether it moved.
    pub fn set_offset(&mut self, offset: u64, visible: usize) -> bool {
        let clamped = self.scroll_range(visible).with_offset(offset).offset();
        let moved = clamped != self.offset;
        self.offset = clamped;
        moved
    }

    /// Scroll by `delta` rows (positive scrolls toward the end), clamped,
    /// reporting whether it moved.
    pub fn scroll_by(&mut self, delta: i64, visible: usize) -> bool {
        let offset = self.scroll_model(visible).scroll_by(delta).offset();
        self.set_offset(offset, visible)
    }

    /// Scroll the least that brings the current selection into a list showing
    /// `visible` rows, reporting whether it moved.
    ///
    /// The one rule keyboard traversal reveals through, so a selection can
    /// never sit outside the drawn list.
    pub fn reveal(&mut self, visible: usize) -> bool {
        let rows = u64::try_from(visible.max(1)).unwrap_or(u64::MAX);
        let selected = u64::try_from(self.selected).unwrap_or(u64::MAX);
        let target = if selected < self.offset {
            selected
        } else if selected >= self.offset.saturating_add(rows) {
            selected.saturating_sub(rows.saturating_sub(1))
        } else {
            self.offset
        };
        self.set_offset(target, visible)
    }

    /// The chooser's own drawn scrollbar, carrying its live hover/drag state.
    #[must_use]
    pub const fn scrollbar(&self) -> &ScrollBar {
        &self.bar
    }

    /// Mutable access to the drawn scrollbar, for the pointer routing that
    /// drives it.
    pub const fn scrollbar_mut(&mut self) -> &mut ScrollBar {
        &mut self.bar
    }
}
