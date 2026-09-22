//! The [`Shell`]: the whole client content of the settings window.
//!
//! It owns the navigation and nothing else. Every pixel is a shared
//! `lib/controls` control — the vertical `Tabs` strip, the `SearchField`, the
//! `Breadcrumb`, the `ScrollBar`, the `Menu` the shed strip becomes — and the
//! pane on show is drawn by the one statement renderer. The shell holds no
//! capability, performs no I/O, and reads nothing but the registry table and
//! the desktop it was handed.
//!
//! Input updates state and asks for a paint; the paint is produced from that
//! state afterwards, so a burst of pointer motion costs one frame rather than
//! one per sample.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::elevate::ElevateArgv;
use tairix_abi::net_ipc::NetServerAddr;
use tairix_controls::{
    plate_rect, Breadcrumb, BreadcrumbAction, CredentialAction, CredentialSheet, Crumb, Menu,
    MenuAction, MenuItem, PlatePlacement, PlateSide, ScrollAction, ScrollBar, ScrollModel,
    ScrollOrientation, ScrollRange, SearchField, Tab, Tabs, TabsAction, TabsOrientation,
    TextAction, CREDENTIAL_REFUSED_REASON,
};
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_icon::{IconArtwork, IconKind};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_sysconfig::SystemConfig;
use tairix_theme::{CursorSetId, Theme};
use tairix_users::Salt;
use tairix_wallpaper::{CatalogItem, DesktopSettings};

use crate::accounts::{AccountFacts, Roster};
use crate::body::{self, Body, Drawn};
use crate::facts::MachineFacts;
use crate::footer::{Footer, FooterAction, Standing};
use crate::form::{Composition, Form, FormOutcome, FormPlace, Posture};
use crate::frame::{resolve_frame, Actions, Overflow, ShellFrame};
use crate::gallery::{GalleryOutcome, PictureWanted};
use crate::network::{Addressing, NetworkFacts};
use crate::registry::{
    strip_rows, CategoryRow, Location, Pane, PaneContent, PaneRow, StripRow, CATEGORIES,
};
use crate::stack;
use crate::volumes::VolumeReading;

/// The trail's leading crumb: the surface itself, and — once the strip is
/// shed — the way back to the category list.
const ROOT_CRUMB: &str = "Settings";

/// How far one line-scroll moves the pane column, in physical pixels.
///
/// A fixed distance rather than a measured text line: the column holds prose
/// at three type roles, so no one line height is the column's, and a reader
/// turning a wheel wants a consistent step.
const LINE_STEP: u64 = 24;

/// A reading the caller takes for this window.
///
/// Each is wanted from the moment the pane that states it comes on show
/// until its answer lands, and none is ever awaited: the pane draws
/// whatever has arrived and rebuilds when the rest does.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Reading {
    /// The machine's boot-time configuration store.
    Config,
    /// The mount table.
    Volumes,
    /// The stack's resolver set.
    Resolvers,
    /// The caller's own account, the two public directories, and a salt.
    Accounts,
}

impl Reading {
    /// This reading's bit in a [`Wanted`] set.
    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

/// Which readings the caller should take for this window.
///
/// One set rather than a field per reading: every one obeys the same
/// rule — armed when its pane comes on show, cleared when its answer
/// lands — so a field apiece would be four copies of it to keep in step.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct Wanted(u8);

impl Wanted {
    /// Ask for `reading`.
    fn arm(&mut self, reading: Reading) {
        self.0 |= reading.bit();
    }

    /// Record that `reading`'s answer has landed.
    fn landed(&mut self, reading: Reading) {
        self.0 &= !reading.bit();
    }

    /// Whether `reading` is still wanted.
    const fn holds(self, reading: Reading) -> bool {
        self.0 & reading.bit() != 0
    }
}

/// Which region of the shell holds the keyboard cursor.
///
/// A region the frame did not seat is not on the ring, so `Tab` never lands
/// somewhere the reader cannot see.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Focus {
    /// The search field above the strip.
    Search,
    /// The location trail.
    Trail,
    /// The category and pane strip.
    Strip,
    /// The pane column, whose keyboard is its scrollbar's while the pane
    /// composes no controls of its own.
    Content,
    /// The pane's own action band, where it has one.
    Footer,
}

/// What the caller does with the program an offered credential runs.
///
/// Three, because three things are wanted of an elevated run and each is a
/// different exchange with the supervisor: a store write is over in
/// moments and its exit code is the whole answer, an application the user
/// then works in is started and left running, and a store *read* is waited
/// for and answered with what it printed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RunMode {
    /// Wait for it; the exit code is the answer.
    Wait,
    /// Start it and leave it running.
    Leave,
    /// Wait for it; what it printed is the answer.
    Capture,
}

/// What an elevated run came to.
///
/// The caller reports this back verbatim; the shell decides what it means
/// for the pane that asked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Elevated {
    /// The run finished with this exit code.
    Finished(i32),
    /// The run finished with this exit code, having printed these bytes.
    Printed(i32, Vec<u8>),
    /// The run happened, but printed more than the supervisor's reply
    /// carries, so nothing came back. Not a refusal: whatever it did, it
    /// did.
    Overran,
    /// Nothing ran, and why.
    Refused(ElevateRefusal),
}

/// The program an offered credential will run, and the arguments to hand
/// it.
///
/// Built by the shell and carried out by the caller: this application holds
/// no authority and starts nothing. What the caller does with the run is
/// the shell's to say, in `mode`.
#[derive(Clone, Eq, PartialEq)]
pub struct Elevation {
    /// The account the reader named.
    pub account: String,
    /// The password they offered, as bytes.
    ///
    /// A secret, and bytes rather than text so its holder can zero it in
    /// place through the one shared eraser when the exchange resolves — a
    /// `String`'s contents cannot be overwritten without `unsafe`, and a
    /// credential left in a freed block is the leak the charter makes the
    /// holder responsible for.
    pub password: Vec<u8>,
    /// The absolute path of the program to run.
    pub program: &'static str,
    /// The arguments to hand it, already in the order the program's own
    /// command line takes them.
    pub argv: Vec<String>,
    /// What the caller does with the run.
    pub mode: RunMode,
}

impl Elevation {
    /// Erase the offered secret in place.
    ///
    /// The one eraser, run by [`Drop`] and by any holder that outlives the
    /// exchange and so must erase before it is dropped — a worker desk
    /// keeps the job it was handed until the next one replaces it, which
    /// would be the whole time between two authentications.
    pub fn erase(&mut self) {
        tairix_util::secret::wipe(&mut self.password);
    }
}

impl Drop for Elevation {
    /// Erase the offered secret.
    ///
    /// Every copy ends here — the one the routing carried out of the
    /// window and the one the worker was handed — so no path out of the
    /// exchange, taken or refused or cancelled or unwound, can leave a
    /// password in a freed block.
    fn drop(&mut self) {
        self.erase();
    }
}

impl core::fmt::Debug for Elevation {
    /// Redacts the secret: a derived `Debug` would print an offered
    /// password into whatever rendered it.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Elevation")
            .field("account", &self.account)
            .field("password", &"<redacted>")
            .field("program", &self.program)
            .field("argv", &self.argv)
            .field("mode", &self.mode)
            .finish()
    }
}

/// What a captured run answers, and therefore which pane it is for.
///
/// Carried explicitly rather than inferred from the pane on show: the
/// desktop can send the window to another pane while a run is in flight,
/// and a privileged listing routed to whichever pane happens to be showing
/// when it lands would be a reading installed for a surface that never
/// asked for it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Captures {
    /// The machine's configured addressing, for the networking panes.
    Addressing,
    /// The account and group listing, for the Users pane.
    Roster,
}

impl Captures {
    /// Whether `composition` is the pane this capture was asked for.
    const fn asked_by(self, composition: Composition) -> bool {
        match self {
            Self::Addressing => composition.reads_addressing(),
            Self::Roster => composition.reads_roster(),
        }
    }
}

/// A credential question standing over the window, and what it is for.
struct Asking {
    sheet: CredentialSheet,
    program: &'static str,
    argv: Vec<String>,
    mode: RunMode,
    /// What a captured run answers, and `None` for a run whose exit code
    /// is the whole answer.
    captures: Option<Captures>,
}

/// What one routed event concluded, for a caller that must act outside the
/// window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellOutcome {
    /// Nothing on screen changed.
    Idle,
    /// The shell changed and must be re-presented.
    Changed,
    /// The reader chose a setting: the rendered document the caller posts to
    /// the desktop session, which decides whether to adopt it.
    ///
    /// The shell holds no capability and performs no I/O: it never writes
    /// the desktop's settings, it says what was asked for.
    Apply(String),
    /// The reader offered an account for a change this application may not
    /// make: the run to ask the console's elevation broker for.
    ///
    /// The shell authenticates nobody and spawns nothing. It carries the
    /// offer and renders the verdict it is told.
    Elevate(Elevation),
}

impl ShellOutcome {
    /// `Changed` when `acted`, else `Idle`.
    const fn of(acted: bool) -> Self {
        if acted {
            Self::Changed
        } else {
            Self::Idle
        }
    }

    /// Whether the shell must be re-presented.
    #[must_use]
    pub const fn changed(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// The document this outcome asks the session to adopt, if any.
    #[must_use]
    pub fn document(&self) -> Option<&str> {
        match self {
            Self::Apply(document) => Some(document),
            Self::Idle | Self::Changed | Self::Elevate(_) => None,
        }
    }
}

/// The settings window's client content.
pub struct Shell {
    /// Where the surface is.
    location: Location,
    /// The strip's rows, in the order they are drawn — the one list the
    /// paint, the hit test and the cursor read.
    rows: Vec<StripRow>,
    strip: Tabs,
    search: SearchField,
    trail: Breadcrumb,
    /// The strip's own scroll: the list of categories is longer than a short
    /// window's column, and a row the reader cannot reach is a category they
    /// cannot open.
    strip_scroll: ScrollBar,
    /// The pane column's scroll.
    scroll: ScrollBar,
    /// The category list the shed strip becomes, while it is open.
    categories: Option<Menu>,
    focus: Focus,
    /// The last pointer position, for routing a press to the region under it.
    pointer: Point,
    /// The desktop settings every composed pane's rows are built from.
    settings: DesktopSettings,
    /// What the pane on show draws.
    body: Body,
    /// The shipped pictures the desktop answered, empty until it has.
    catalog: Vec<CatalogItem>,
    /// The cursor sets the desktop offers besides the built-in one, empty
    /// until it has answered.
    cursor_sets: Vec<CursorSetId>,
    /// The mounted volumes the caller last read for the Storage pane, empty
    /// until it has.
    volumes: Vec<VolumeReading>,
    /// The machine's boot-time configuration, or `None` while the read has
    /// not landed.
    config: Option<SystemConfig>,
    /// The machine readings the About and Date & Time panes state.
    machine: MachineFacts,
    /// The network readings the DNS pane states.
    network: NetworkFacts,
    /// The account readings the Users pane states.
    accounts: AccountFacts,
    /// A fresh salt the caller drew, held so a password can be hashed
    /// without the event loop waiting on a read.
    ///
    /// Consumed on use and never reused: an apply takes it and the caller
    /// draws another. With none held a password apply is refused rather
    /// than salted with something predictable.
    salt: Option<Salt>,
    /// Which readings the caller should take for this window.
    wanted: Wanted,
    /// The action band beneath the pane on show, for a pane that has one.
    footer: Option<Footer>,
    /// The credential question standing over the window, and what it will
    /// ask the broker to run once an account is offered.
    asking: Option<Asking>,
}

impl Shell {
    /// The shell as the window opens on `settings`: the first category, its
    /// first pane, and the cursor on the strip.
    ///
    /// `settings` is the desktop's own published document, read by the
    /// caller — the shell performs no I/O. An empty registry would leave
    /// nothing to show, so it answers `None` rather than inventing a
    /// location; the registry's own test rules it out.
    #[must_use]
    pub fn new(settings: DesktopSettings) -> Option<Self> {
        let location = Location::opening()?;
        let rows = strip_rows(location.category, "");
        let mut shell = Self {
            location,
            strip: strip_of(&rows, location),
            rows,
            search: SearchField::new().with_placeholder("Search settings"),
            trail: Breadcrumb::new(Vec::new()),
            strip_scroll: ScrollBar::new(ScrollOrientation::Vertical, empty_scroll()),
            scroll: ScrollBar::new(ScrollOrientation::Vertical, empty_scroll()),
            categories: None,
            focus: Focus::Strip,
            pointer: Point::ORIGIN,
            settings,
            body: Body::Statement,
            catalog: Vec::new(),
            cursor_sets: Vec::new(),
            volumes: Vec::new(),
            wanted: Wanted::default(),
            config: None,
            machine: MachineFacts::default(),
            network: NetworkFacts::default(),
            accounts: AccountFacts::default(),
            salt: None,
            footer: None,
            asking: None,
        };
        shell.restate_trail();
        shell.restate_body();
        Some(shell)
    }

    /// Adopt the cursor sets the desktop answered.
    ///
    /// Which sets exist is what the desktop's read-only store holds, and
    /// this application may not read it: the session lists it once and
    /// answers, so the pointer row's choice space arrives here. Until it
    /// does the row offers the built-in set alone, plus whatever the
    /// document already names.
    pub fn adopt_cursor_sets(&mut self, sets: Vec<CursorSetId>) {
        self.cursor_sets = sets;
        self.restate_body();
    }

    /// Adopt the shipped picture catalog the desktop answered.
    ///
    /// Requested, never awaited: the Wallpaper pane opens on whatever has
    /// arrived — nothing at all, at first — and rebuilds when it lands.
    pub fn adopt_catalog(&mut self, catalog: Vec<CatalogItem>) {
        self.catalog = catalog;
        if self.body.gallery().is_some() {
            self.restate_body();
        }
    }

    /// Whether the caller should read the machine's boot-time configuration
    /// for this window.
    ///
    /// Set when a pane that stages a machine setting comes on show and
    /// cleared when a read is answered, so the rows are as current as the
    /// last time the reader looked without the pane ever waiting on a round
    /// trip.
    #[must_use]
    pub const fn config_wanted(&self) -> bool {
        self.wanted.holds(Reading::Config)
    }

    /// Adopt the machine's boot-time configuration the caller read.
    ///
    /// `None` is a read that was refused or did not decode, which leaves
    /// the rows unmeasured rather than showing defaults the reader could
    /// not have set; a store that simply does not exist decodes as the
    /// documented defaults and is a perfectly good reading.
    pub fn adopt_config(&mut self, config: Option<SystemConfig>) {
        self.config = config;
        self.wanted.landed(Reading::Config);
        if let Some(form) = self.body.form_mut() {
            form.adopt_config(self.config.as_ref());
        }
        self.restate_footer();
    }

    /// Adopt the machine readings the caller took.
    pub fn adopt_machine(&mut self, machine: MachineFacts) {
        self.machine = machine;
        if matches!(self.body, Body::Facts(_)) {
            self.restate_body();
        }
    }

    /// Adopt a staged edit: what each plate and the band now say about it,
    /// and the column's extent where a plate learning it has changed moved
    /// one.
    ///
    /// Only where it moved: a badge appears once per plate, while an entry
    /// reports every keystroke, and repainting the whole column for each of
    /// them would hand the frame rate to the keyboard.
    fn restate_staged(
        &mut self,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let column = pane_band(frame, viewport).width;
        let before = self
            .body
            .form()
            .map_or(0, |form| form.measured_height(column, scale, theme));
        if let Some(form) = self.body.form_mut() {
            form.restate_badges();
        }
        self.restate_footer();
        if let Some(rect) = frame.footer {
            damage.add(rect);
        }
        let after = self
            .body
            .form()
            .map_or(0, |form| form.measured_height(column, scale, theme));
        if before != after {
            self.lay_out(viewport, scale, theme);
            damage.add(pane_band(frame, viewport));
        }
    }

    /// Bring the action band up to date with what the pane now holds.
    fn restate_footer(&mut self) {
        let standing = self.standing();
        if let Some(band) = self.footer.as_mut() {
            band.state(standing);
        }
    }

    /// Whether the caller should read the mount table for this window.
    ///
    /// Set when the Storage pane comes on show and cleared when a walk is
    /// answered, so the volumes are as current as the last time the reader
    /// looked at them without the pane ever waiting on a round trip.
    #[must_use]
    pub const fn volumes_wanted(&self) -> bool {
        self.wanted.holds(Reading::Volumes)
    }

    /// Adopt the mounted volumes the caller walked the mount table for.
    ///
    /// Requested, never awaited: the Storage pane opens on whatever has
    /// arrived — nothing at all, at first — and rebuilds when it lands.
    pub fn adopt_volumes(&mut self, volumes: Vec<VolumeReading>) {
        self.volumes = volumes;
        self.wanted.landed(Reading::Volumes);
        if matches!(self.body, Body::Volumes(_)) {
            self.restate_body();
        }
    }

    /// Whether the caller should read the network stack's resolver set for
    /// this window.
    ///
    /// Set when a pane that states a network reading comes on show and
    /// cleared when one is answered, exactly as the mount walk is: the set
    /// moves as leases come and go, so a pane the reader returns to shows
    /// what the stack holds now rather than what it held last time.
    #[must_use]
    pub const fn network_wanted(&self) -> bool {
        self.wanted.holds(Reading::Resolvers)
    }

    /// Adopt the network readings the caller took.
    ///
    /// Requested, never awaited: the pane opens on whatever has arrived —
    /// nothing at all, at first — and rebuilds when it lands.
    ///
    /// Only the resolver set, because that is the only network reading a
    /// desk takes: the configured addressing is answered by an
    /// authenticated run and would be thrown away by a reading that
    /// replaced the whole record.
    pub fn adopt_resolvers(&mut self, resolvers: Option<Vec<NetServerAddr>>) {
        self.network.resolvers = resolvers;
        self.wanted.landed(Reading::Resolvers);
        // Into the pane that states it, and only its rows: rebuilding the
        // pane would discard a change the reader has staged because an
        // unrelated reading happened to land.
        if !self.states_resolvers() {
            return;
        }
        let resolvers = self.network.resolvers_slice();
        if let Some(form) = self.body.form_mut() {
            form.adopt_resolvers(resolvers);
        }
    }

    /// Whether the caller should read the ungated account readings for
    /// this window.
    ///
    /// Set when the Users pane comes on show and cleared when a read is
    /// answered, exactly as the mount walk and the resolver set are: the
    /// directories move as accounts and groups come and go, so a pane the
    /// reader returns to shows what the machine holds now.
    #[must_use]
    pub const fn accounts_wanted(&self) -> bool {
        self.wanted.holds(Reading::Accounts)
    }

    /// Whether the caller should draw a fresh salt for this window.
    ///
    /// One at a time: a password apply consumes the one held, so the
    /// reserve is refilled rather than drawn at the instant it is wanted —
    /// the loop owes the reader a frame and must not wait on a read.
    #[must_use]
    pub fn salt_wanted(&self) -> bool {
        self.salt.is_none() && self.shows_accounts()
    }

    /// Adopt the ungated account readings the caller took.
    ///
    /// Requested, never awaited: the pane opens on whatever has arrived —
    /// nothing at all, at first — and rebuilds when it lands. The
    /// authenticated listing is *not* replaced: it cost a password, and a
    /// public reading landing must not throw it away.
    pub fn adopt_accounts(&mut self, accounts: AccountFacts) {
        self.accounts.adopt_public(accounts);
        self.wanted.landed(Reading::Accounts);
        if let Some(form) = self
            .body
            .form_mut()
            .filter(|form| form.composition().reads_roster())
        {
            form.adopt_accounts(&self.accounts);
        }
        self.restate_footer();
    }

    /// Adopt a fresh salt the caller drew, or record that the draw failed.
    ///
    /// `None` leaves the reserve empty, which refuses a password apply
    /// rather than reaching for a predictable salt.
    pub fn adopt_salt(&mut self, salt: Option<Salt>) {
        self.salt = salt;
    }

    /// Whether the pane the shell is *on* is discovered from the account
    /// listing.
    ///
    /// The location rather than the body on show, for the same reason the
    /// addressing capture asks it of the location: this decides whether the
    /// listing survives long enough to build the next body.
    fn shows_accounts(&self) -> bool {
        matches!(
            self.pane_row().and_then(PaneRow::content),
            Some(PaneContent::Form(composition)) if composition.reads_roster()
        )
    }

    /// Whether the body on show is discovered from the account listing.
    fn states_accounts(&self) -> bool {
        self.body
            .composition()
            .is_some_and(Composition::reads_roster)
    }

    /// Whether the body on show states the live resolver set.
    fn states_resolvers(&self) -> bool {
        self.body
            .composition()
            .is_some_and(Composition::reads_resolvers)
    }

    /// Whether the pane the shell is *on* is discovered from the
    /// addressing capture.
    ///
    /// The location rather than the body on show, because this decides
    /// whether the capture survives long enough to build the next body.
    fn shows_addressing(&self) -> bool {
        matches!(
            self.pane_row().and_then(PaneRow::content),
            Some(PaneContent::Form(composition)) if composition.reads_addressing()
        )
    }

    /// The next picture the caller should ask the desktop to render for the
    /// gallery, or `None` when there is nothing to ask for.
    #[must_use]
    pub fn next_picture_wanted(
        &self,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<PictureWanted> {
        let frame = self.frame(viewport, scale, theme);
        let band = self.gallery_band(&frame, scale, theme)?;
        self.body.gallery()?.next_wanted(band, scale, theme)
    }

    /// Adopt the pixels the desktop rendered for catalog position `index`,
    /// answering whether anything on screen changed.
    pub fn set_picture(&mut self, index: u16, side: u16, pixels: &[u8]) -> bool {
        self.body
            .gallery_mut()
            .is_some_and(|gallery| gallery.set_picture(index, side, pixels))
    }

    /// Record that the desktop refused catalog position `index`, answering
    /// whether anything on screen changed.
    pub fn mark_picture_refused(&mut self, index: u16) -> bool {
        self.body
            .gallery_mut()
            .is_some_and(|gallery| gallery.mark_refused(index))
    }

    /// Ask for every rendered picture again, because a picture is square
    /// at one side only and the desktop's scale has moved.
    pub fn invalidate_pictures(&mut self) {
        if let Some(gallery) = self.body.gallery_mut() {
            gallery.invalidate_pictures();
        }
    }

    /// Show the pane `name` identifies, answering whether it named one.
    ///
    /// The desktop hands this over when it sends the reader here to change
    /// a particular setting. It confers nothing and names nothing outside
    /// the closed registry, so an unknown name leaves the window on the
    /// pane it already showed.
    pub fn go_to_pane(
        &mut self,
        name: &str,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(pane) = Pane::named(name) else {
            return false;
        };
        self.go_to(pane, viewport, scale, theme, damage)
    }

    /// Show `pane`, answering whether the registry could place it.
    pub fn go_to(
        &mut self,
        pane: Pane,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some((category, row)) = pane.locate() else {
            return false;
        };
        let location = Location {
            category,
            pane: row.pane,
        };
        if location == self.location {
            return true;
        }
        self.show(location, viewport, scale, theme, damage);
        true
    }

    /// Adopt the desktop settings the session now holds.
    ///
    /// What the window does when an apply is answered, and on regaining
    /// focus: every composed row shows what the store actually holds, so a
    /// refused apply reverts rather than leaving a value on screen the next
    /// login would not restore.
    pub fn adopt_settings(&mut self, settings: DesktopSettings) {
        // Against what the *form* shows, not against the last answer: a
        // choice the reader made moved the rows on screen without moving
        // this, so an answer equal to the last one is exactly the refusal
        // that has to put them back.
        let shown = match self.body.form() {
            Some(form) => form.settings() == &settings,
            None => self.settings == settings,
        };
        self.settings = settings;
        if shown {
            return;
        }
        if self.body.form().is_some() {
            self.body.adopt(&self.settings);
        } else {
            self.restate_body();
        }
    }

    /// Build what the pane on show draws.
    fn restate_body(&mut self) {
        let listed = matches!(self.body, Body::Volumes(_));
        let resolved = self.states_resolvers();
        // A pane that is not discovered from the capture drops it before
        // it is built: the machine's address book is a privileged reading,
        // and one held while the reader browses elsewhere is one this
        // application had no business keeping. Both networking panes are
        // discovered from it, so moving between them costs no second
        // authentication.
        if !self.shows_addressing() {
            self.network.addressing = Addressing::Unasked;
        }
        // And the account listing, for exactly the same reason: it names
        // every account's home, shell, lock state and capability ceiling,
        // which is a privileged reading this application had no business
        // keeping while the reader browses elsewhere.
        if !self.shows_accounts() {
            self.accounts.roster = Roster::Unasked;
        }
        let rostered = self.states_accounts();
        let staged = self.body.staged().to_vec();
        let staged_accounts = self.body.staged_accounts().to_vec();
        let answered = body::Answered {
            settings: &self.settings,
            cursor_sets: &self.cursor_sets,
            catalog: &self.catalog,
            volumes: &self.volumes,
            config: self.config.as_ref(),
            machine: &self.machine,
            network: &self.network,
            staged: &staged,
            accounts: &self.accounts,
            staged_accounts: &staged_accounts,
        };
        self.body = match self.location.rows() {
            Some((_, pane)) => Body::of(pane, &answered),
            None => Body::Statement,
        };
        // Asked for when the pane that lists them *comes* on show, so the
        // table is current each time the reader looks. Not on every rebuild:
        // adopting an answered walk rebuilds too, and re-arming there would
        // ask again for the table just handed over.
        if !listed && matches!(self.body, Body::Volumes(_)) {
            self.wanted.arm(Reading::Volumes);
        }
        // The resolver set is live state the stack changes on its own, so
        // it is re-read when its pane *comes* on show for the same reason
        // the mount table is, and not on the rebuild that adopting one
        // causes.
        if !resolved && self.states_resolvers() {
            self.wanted.arm(Reading::Resolvers);
        }
        // The directories move on their own too, so they are re-read when
        // the pane that lists them *comes* on show — not on the rebuild
        // that adopting one causes, which would ask again for the readings
        // just handed over.
        if !rostered && self.states_accounts() {
            self.wanted.arm(Reading::Accounts);
        }
        // Rebuilt rather than kept: a band belongs to the pane that offers
        // it, and one carried across a navigation would state the last
        // pane's standing under this pane's rows.
        // Asked for when a pane that stages a machine setting comes on
        // show, for the same reason the mount table is: the store moves,
        // and a stale reading is a value the reader cannot account for.
        if self.body.stages_machine_settings() && self.config.is_none() {
            self.wanted.arm(Reading::Config);
        }
        self.footer = self.band();
    }

    /// The action band the pane on show offers, or `None` for a pane that
    /// offers none.
    ///
    /// A pane with a working copy offers Revert and Apply over it. One
    /// without offers the single command the registry names for it — the
    /// application that owns its subject, or the authenticated reading its
    /// rows cannot exist without.
    fn band(&self) -> Option<Footer> {
        if self.stageable() {
            let mut band = Footer::staged();
            band.state(self.standing());
            return Some(band);
        }
        self.pane_row()
            .and_then(PaneRow::action)
            .map(Footer::command)
    }

    /// Whether the pane on show has a working copy to stage a change
    /// against.
    ///
    /// A machine composition always has one: an unread store leaves its
    /// rows unmeasured, but the pane is still that store's editor and the
    /// reading arrives without the reader doing anything. A networking
    /// composition has one only once a capture has landed, because until
    /// then there is no document — and getting one is what its band's
    /// command is for.
    fn stageable(&self) -> bool {
        let Some(form) = self.body.form() else {
            return false;
        };
        if form.posture() != Posture::Staged {
            return false;
        }
        let composition = form.composition();
        if composition.reads_roster() {
            return !form.roster().accounts().is_empty();
        }
        !composition.reads_addressing() || form.addressing().document().is_some()
    }

    /// What the pane on show has to say about the change it is holding.
    ///
    /// A pane with no working copy holds no change: its band offers one
    /// named command and has nothing to count.
    fn standing(&self) -> Standing {
        let Some(form) = self.body.form().filter(|_| self.stageable()) else {
            return Standing::Offered;
        };
        match (form.refused(), form.changes()) {
            (0, 0) => Standing::Unchanged,
            (0, count) => Standing::Changed(count),
            (refused, _) => Standing::Refusing(refused),
        }
    }

    /// Whether the pane on show has an action band beneath its column.
    const fn actions(&self) -> Actions {
        match self.footer {
            Some(_) => Actions::Band,
            None => Actions::None,
        }
    }

    /// The pane row on show, whose statement a stated absence draws from.
    fn pane_row(&self) -> Option<&'static PaneRow> {
        self.location.rows().map(|(_, pane)| pane)
    }

    /// The band the gallery is drawn in: what `frame.content` has left
    /// below the form that sits fixed at its top.
    ///
    /// The rows are the part the reader came for, so they keep their place
    /// and the pictures are what scrolls — and a window too short for both
    /// still shows the rows.
    fn gallery_band(&self, frame: &ShellFrame, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.body.gallery()?;
        let header = self.body.form().map_or(0, |form| {
            form.measured_height(frame.content.width, scale, theme)
        });
        let height = frame.content.height.checked_sub(header)?;
        Some(Rect::new(
            frame.content.left(),
            frame.content.top().saturating_add(to_i32(header)),
            frame.content.width,
            height,
        ))
    }

    /// Where the surface is.
    #[must_use]
    pub fn location(&self) -> Location {
        self.location
    }

    /// The strip's rows, in drawn order.
    #[must_use]
    pub fn rows(&self) -> &[StripRow] {
        &self.rows
    }

    /// The category list drawn over the content while the shed strip is open.
    #[must_use]
    pub fn category_list_open(&self) -> bool {
        self.categories.is_some()
    }

    /// The frame this shell is drawn with in `viewport`.
    ///
    /// Cheap enough for the input path: whether the pane scrolls is the
    /// scroll range's own answer, laid in by [`lay_out`](Self::lay_out),
    /// rather than a statement re-measured per event.
    #[must_use]
    pub fn frame(&self, viewport: Rect, scale: Scale, theme: &Theme) -> ShellFrame {
        resolve_frame(
            viewport,
            scale,
            theme,
            Overflow {
                strip: self.strip_scroll.model().range().is_scrollable(),
                pane: self.scroll.model().range().is_scrollable(),
            },
            self.actions(),
        )
    }

    /// Measure the pane for `viewport` and adopt the scroll range it implies.
    ///
    /// Called whenever what the column holds or how wide it is can have
    /// changed — the window opening, a navigation, a resize, a desktop change
    /// — and never from the input path: measuring a wrapped statement is a
    /// pass over its every word, which a pointer sample did not ask for.
    ///
    /// The column's width decides how the statement wraps and the wrap decides
    /// its height, while a column that needs a scrollbar is the narrower for
    /// it. So it measures without one, and again at the narrowed width when
    /// one turns out to be needed.
    pub fn lay_out(&mut self, viewport: Rect, scale: Scale, theme: &Theme) {
        let bare = resolve_frame(viewport, scale, theme, Overflow::default(), self.actions());
        let overflow = Overflow {
            strip: bare
                .sidebar
                .is_some_and(|rect| self.strip.seated(rect, scale, theme) < self.rows.len()),
            pane: self.body.gallery().map_or_else(
                || self.content_height(bare.content.width, scale, theme) > bare.content.height,
                |gallery| {
                    self.gallery_band(&bare, scale, theme).is_some_and(|band| {
                        gallery.scroll_range(band, scale, theme, 0).is_scrollable()
                    })
                },
            ),
        };
        // A column that needs a bar is the narrower for it, and a narrower
        // pane column wraps its statement into more lines — so the columns
        // the ranges are set from are the ones a bar has already been taken
        // out of.
        let frame = resolve_frame(viewport, scale, theme, overflow, self.actions());
        // Tile lines for a gallery, plates for a form or a card column —
        // each is the unit the thing that scrolls actually moves in — and
        // pixels for a statement, which is drawn at any offset.
        let band = self
            .gallery_band(&frame, scale, theme)
            .unwrap_or(frame.content);
        let (extent, seen) = self.pane_row().map_or((0, 0), |pane| {
            self.body.scroll_range(
                pane,
                place(frame.content, viewport, scale, theme),
                band,
                self.scroll.model().offset(),
            )
        });
        self.scroll.set_model(stepped(
            self.scroll.model().resize(extent, seen),
            self.body.scrolls_in_pixels(),
        ));
        // The strip's scroll is counted in *rows*, because that is the unit a
        // strip drawing from an entry of its own moves in.
        let seats = frame
            .sidebar
            .map_or(0, |rect| self.strip.seated(rect, scale, theme));
        // In rows, because that is the unit a strip drawing from an entry
        // of its own moves in.
        self.strip_scroll.set_model(stepped(
            self.strip_scroll
                .model()
                .resize(stack::as_extent(self.rows.len()), stack::as_extent(seats)),
            false,
        ));
    }

    /// Scroll the strip so row `index` is one the column shows.
    ///
    /// A strip draws from an entry of its own, so the scroll is in *rows*: a
    /// row above the window becomes the first drawn, and a row below it
    /// becomes the last. That is also why the window is the strip's own
    /// answer ([`Tabs::seated`]) rather than arithmetic here — entries stack
    /// at their own content height.
    fn reveal_row(
        &mut self,
        index: usize,
        frame: &ShellFrame,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let Some(sidebar) = frame.sidebar else {
            return;
        };
        let first = self.strip.first();
        if index < first {
            self.adopt_first(index, sidebar, damage);
            return;
        }
        let seats = self.strip.seated(sidebar, scale, theme).max(1);
        if index >= first.saturating_add(seats) {
            // Walk the first-drawn row forward until the wanted one is the
            // last seated: each step may seat a different number of rows, so
            // the count is re-asked rather than assumed uniform.
            let mut want = first;
            while want < index
                && index >= want.saturating_add(self.seats_from(want, sidebar, scale, theme).max(1))
            {
                want = want.saturating_add(1);
            }
            self.adopt_first(want, sidebar, damage);
        }
    }

    /// How many rows the column seats from row `index`, without disturbing
    /// where the strip is actually scrolled to.
    fn seats_from(&self, index: usize, sidebar: Rect, scale: Scale, theme: &Theme) -> usize {
        let mut probe = self.strip.clone();
        probe.set_first(index);
        probe.seated(sidebar, scale, theme)
    }

    /// Draw the strip from row `first`, and report the column it redraws.
    fn adopt_first(&mut self, first: usize, sidebar: Rect, damage: &mut Region) {
        if self.strip.first() == first {
            return;
        }
        self.strip.set_first(first);
        self.strip_scroll
            .set_model(self.strip_scroll.model().scroll_to(stack::as_extent(first)));
        damage.add(sidebar);
    }

    /// The pane's own height in a column `width` pixels wide.
    fn content_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        self.pane_row().map_or(0, |pane| {
            self.body.measured_height(pane, width, scale, theme)
        })
    }

    /// Draw the shell into `surface` filling `viewport`.
    pub fn render(
        &self,
        surface: &mut Surface,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: &mut dyn IconArtwork,
    ) {
        surface.fill_rect(
            0,
            0,
            viewport.width,
            viewport.height,
            Color::from(theme.palette().surface),
        );
        let frame = self.frame(viewport, scale, theme);
        self.trail.render(surface, frame.breadcrumb, scale, theme);
        if let Some(rect) = frame.search {
            self.search.render(surface, rect, scale, theme);
        }
        if let Some(rect) = frame.sidebar {
            self.strip.render(surface, rect, scale, theme, artwork);
        }
        if let Some(rect) = frame.strip_scrollbar {
            self.strip_scroll.render(surface, rect, scale, theme);
        }
        if let Some(pane) = self.pane_row() {
            let column = self.pane_column(&frame);
            let drawn = Drawn {
                place: place(column, viewport, scale, theme),
                column,
                band: self
                    .gallery_band(&frame, scale, theme)
                    .unwrap_or(Rect::EMPTY),
                offset: self.scroll.model().offset(),
            };
            surface.with_clip(
                u32::try_from(frame.content.left()).unwrap_or(0),
                u32::try_from(frame.content.top()).unwrap_or(0),
                frame.content.width,
                frame.content.height,
                |clipped| self.body.render(clipped, pane, drawn, artwork),
            );
        }
        if let Some(rect) = frame.scrollbar {
            self.scroll.render(surface, rect, scale, theme);
        }
        if let (Some(band), Some(rect)) = (self.footer.as_ref(), frame.footer) {
            band.render(surface, rect, scale, theme);
        }
        // The category list stands over everything it was opened from.
        if let Some(menu) = &self.categories {
            menu.render(
                surface,
                Self::list_rect(menu, &frame, viewport, scale, theme),
                scale,
                theme,
            );
        }
        // And the credential question stands over even that: while it is
        // up it holds the keyboard, and nothing behind it can be reached.
        if let Some(asking) = &self.asking {
            asking.sheet.render(
                surface,
                CredentialSheet::centred_in(viewport, scale),
                scale,
                theme,
            );
        }
    }

    /// Scroll the form's focused row into view.
    ///
    /// A row the cursor reached but the column does not show is a control
    /// the reader cannot use, which is the same correctness property the
    /// strip's own gutter exists for.
    fn reveal_focused_group(
        &mut self,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let column = self.pane_column(frame);
        let spot = place(column, viewport, scale, theme);
        let Some(form) = self.body.form() else {
            return;
        };
        let want = form.reveal_from(form.focused_group(), spot);
        if want == form.first() {
            return;
        }
        self.body.set_first(want);
        self.scroll
            .set_model(self.scroll.model().scroll_to(stack::as_extent(want)));
        damage.add(frame.content);
    }

    /// The pane's own rectangle, scrolled: the column the form or the
    /// statement is drawn in and hit-tested against.
    ///
    /// The top rides above the frame while the column is scrolled, so a
    /// press lands on the row the reader can actually see. One definition,
    /// read by the paint and the hit test alike.
    fn pane_column(&self, frame: &ShellFrame) -> Rect {
        if !self.body.scrolls_in_pixels() {
            // A plate is *placed* on the surface rather than clipped to it,
            // so it never rides above the column's own top; such a body
            // scrolls by whole plates instead, exactly as the strip scrolls
            // by rows.
            return frame.content;
        }
        let offset = u32::try_from(self.scroll.model().offset()).unwrap_or(u32::MAX);
        Rect::new(
            frame.content.left(),
            frame.content.top().saturating_sub(to_i32(offset)),
            frame.content.width,
            frame.content.height.saturating_add(offset),
        )
    }

    /// Route one pointer event.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }
        // The credential question is modal: while it is up nothing behind
        // it can be pressed, so a stray click cannot change a pane the
        // reader is about to authenticate for.
        if self.asking.is_some() {
            return self.asked(event, viewport, scale, theme, damage);
        }
        let frame = self.frame(viewport, scale, theme);

        if let Some(outcome) = self.pressed_band(event, &frame, viewport, scale, theme, damage) {
            return outcome;
        }

        if let Some(menu) = &mut self.categories {
            let rect = Self::list_rect(menu, &frame, viewport, scale, theme);
            let acted = menu.on_pointer(event, rect, scale, theme, damage);
            return self.list_acted(acted, viewport, scale, theme, damage);
        }

        if let Some(rect) = frame.scrollbar {
            if rect.contains(self.pointer) || self.scroll.is_pressing() {
                let acted = self.scroll.on_pointer(event, rect, scale, theme, damage);
                return ShellOutcome::of(self.scrolled(frame.content, acted, damage));
            }
        }
        if frame.content.contains(self.pointer) || self.body.is_listing() {
            if let InputEvent::PointerScrolled { dx, dy } = event {
                let acted = self.scroll.wheel(*dx, *dy, frame.content, damage);
                return ShellOutcome::of(self.scrolled(frame.content, acted, damage));
            }
            return self.pressed_pane(event, &frame, viewport, scale, theme, damage);
        }
        self.pressed_chrome(event, &frame, viewport, scale, theme, damage)
    }

    /// Route a press into the pane column: its form first, then the
    /// gallery beneath a form that stays put.
    fn pressed_pane(
        &mut self,
        event: &InputEvent,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        {
            let column = self.pane_column(frame);
            let spot = place(column, viewport, scale, theme);
            if let Some(form) = self.body.form_mut() {
                let acted = form.on_pointer(event, spot, damage);
                if !matches!(acted, FormOutcome::Idle) {
                    self.focus_on(Focus::Content, viewport, scale, theme, damage);
                    if matches!(acted, FormOutcome::Staged) {
                        self.restate_staged(frame, viewport, scale, theme, damage);
                    }
                    return outcome_of(acted);
                }
            }
            // Under the form, never over it: an open choice list hangs over
            // the gallery and keeps the pointer until it resolves, which
            // the form has already answered for above.
            if let Some(band) = self.gallery_band(frame, scale, theme) {
                let offset = self.scroll.model().offset();
                if let Some(gallery) = self.body.gallery_mut() {
                    let acted = gallery.on_pointer(event, band, offset, scale, theme, damage);
                    if acted.changed() {
                        self.focus_on(Focus::Content, viewport, scale, theme, damage);
                    }
                    return match acted {
                        GalleryOutcome::Idle => ShellOutcome::Idle,
                        GalleryOutcome::Changed => ShellOutcome::Changed,
                        GalleryOutcome::Chose(settings) => {
                            self.settings = settings;
                            match self.body.form_mut() {
                                Some(form) => {
                                    form.adopt(&self.settings);
                                    ShellOutcome::Apply(form.applied())
                                }
                                None => ShellOutcome::Changed,
                            }
                        }
                    };
                }
            }
        }
        ShellOutcome::Idle
    }

    /// Route a press into the chrome around the pane: the search field,
    /// the strip and its gutter, and the location trail.
    fn pressed_chrome(
        &mut self,
        event: &InputEvent,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        if let Some(rect) = frame.search {
            if rect.contains(self.pointer) {
                let acted = self.search.on_pointer(event, rect, scale, theme, damage);
                self.focus_on(Focus::Search, viewport, scale, theme, damage);
                return self.searched(acted, viewport, scale, theme, damage);
            }
        }
        if let Some(rect) = frame.sidebar {
            if rect.contains(self.pointer) {
                if let InputEvent::PointerScrolled { dx, dy } = event {
                    let acted = self.strip_scroll.wheel(*dx, *dy, rect, damage);
                    return ShellOutcome::of(self.strip_scrolled(frame, acted, damage));
                }
                let acted = self.strip.on_pointer(event, rect, scale, theme, damage);
                self.focus_on(Focus::Strip, viewport, scale, theme, damage);
                if let Some(TabsAction::Selected { index }) = acted {
                    return ShellOutcome::of(self.choose(index, viewport, scale, theme, damage));
                }
                return ShellOutcome::Idle;
            }
        }
        if let Some(rect) = frame.strip_scrollbar {
            if rect.contains(self.pointer) || self.strip_scroll.is_pressing() {
                let acted = self
                    .strip_scroll
                    .on_pointer(event, rect, scale, theme, damage);
                return ShellOutcome::of(self.strip_scrolled(frame, acted, damage));
            }
        }
        if frame.breadcrumb.contains(self.pointer) {
            let acted = self
                .trail
                .on_pointer(event, frame.breadcrumb, scale, theme, damage);
            self.focus_on(Focus::Trail, viewport, scale, theme, damage);
            return ShellOutcome::of(self.navigated(acted, viewport, scale, theme, damage));
        }
        ShellOutcome::Idle
    }

    /// Route one key press.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        if self.asking.is_some() {
            return self.asked(
                &InputEvent::KeyPressed { key, modifiers },
                viewport,
                scale,
                theme,
                damage,
            );
        }
        let frame = self.frame(viewport, scale, theme);

        if let Some(menu) = &mut self.categories {
            let rect = Self::list_rect(menu, &frame, viewport, scale, theme);
            let acted = menu.on_key(key, rect, scale, theme, damage);
            return self.list_acted(acted, viewport, scale, theme, damage);
        }
        if key == Key::Named(NamedKey::Tab) {
            self.step_focus(!modifiers.shift, viewport, scale, theme, damage);
            return ShellOutcome::Changed;
        }
        match self.focus {
            Focus::Search => {
                let Some(rect) = frame.search else {
                    return ShellOutcome::Idle;
                };
                let acted = self.search.on_key(key, modifiers, rect, damage);
                self.searched(acted, viewport, scale, theme, damage)
            }
            Focus::Trail => {
                let acted = self
                    .trail
                    .on_key(key, frame.breadcrumb, scale, theme, damage);
                ShellOutcome::of(self.navigated(acted, viewport, scale, theme, damage))
            }
            Focus::Strip => {
                let Some(rect) = frame.sidebar else {
                    return ShellOutcome::Idle;
                };
                let acted = self.strip.on_key(key, rect, scale, theme, damage);
                if let Some(cursor) = self.strip.current() {
                    self.reveal_row(cursor, &frame, scale, theme, damage);
                }
                match acted {
                    Some(TabsAction::Selected { index }) => {
                        ShellOutcome::of(self.choose(index, viewport, scale, theme, damage))
                    }
                    // The cursor moved, which the strip reported itself.
                    None => ShellOutcome::of(self.strip.current().is_some()),
                }
            }
            Focus::Content => {
                let column = self.pane_column(&frame);
                let spot = place(column, viewport, scale, theme);
                if let Some(form) = self.body.form_mut() {
                    let acted = form.on_key(key, modifiers, spot, damage);
                    if !matches!(acted, FormOutcome::Idle) {
                        self.reveal_focused_group(&frame, viewport, scale, theme, damage);
                        if matches!(acted, FormOutcome::Staged) {
                            self.restate_staged(&frame, viewport, scale, theme, damage);
                        }
                        return outcome_of(acted);
                    }
                }
                let Some(rect) = frame.scrollbar else {
                    return ShellOutcome::Idle;
                };
                let acted = self.scroll.on_key(key, rect, damage);
                ShellOutcome::of(self.scrolled(frame.content, acted, damage))
            }
            Focus::Footer => {
                let acted = self.footer.as_mut().and_then(|band| band.on_key(key));
                self.commanded(acted, viewport, scale, theme, damage)
            }
        }
    }

    /// Route a press in the pane's action band, or answer `None` when the
    /// pointer is not over one.
    fn pressed_band(
        &mut self,
        event: &InputEvent,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<ShellOutcome> {
        let rect = frame.footer.filter(|rect| rect.contains(self.pointer))?;
        let acted = self
            .footer
            .as_mut()
            .and_then(|band| band.on_pointer(event, rect, scale, theme, damage));
        Some(self.commanded(acted, viewport, scale, theme, damage))
    }

    /// Route one event into the credential question standing over the
    /// window.
    ///
    /// A cancellation takes it down and changes nothing anywhere. An offer
    /// is handed to the caller as the one run to ask the broker for, and
    /// the question stays up: only the caller's verdict takes it down, so a
    /// refusal can be corrected without retyping the account.
    fn asked(
        &mut self,
        event: &InputEvent,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let bounds = CredentialSheet::centred_in(viewport, scale);
        let Some(asking) = self.asking.as_mut() else {
            return ShellOutcome::Idle;
        };
        match asking.sheet.handle(event, bounds, scale, theme, damage) {
            Some(CredentialAction::Cancelled) => {
                self.asking = None;
                damage.add(viewport);
                ShellOutcome::Changed
            }
            Some(CredentialAction::Offered) => ShellOutcome::Elevate(Elevation {
                account: String::from(asking.sheet.account()),
                password: asking.sheet.secret().as_bytes().to_vec(),
                program: asking.program,
                argv: asking.argv.clone(),
                mode: asking.mode,
            }),
            None => ShellOutcome::Changed,
        }
    }

    /// Adopt what the elevated run came to.
    ///
    /// Persist-then-adopt: the rows showed a working copy, and the durable
    /// value is whatever the store answers on the next read, which the
    /// caller takes as soon as a run succeeds. A refusal leaves the working
    /// copy exactly as it was so the reader can correct it rather than
    /// retype it, and states why.
    pub fn adopt_elevation(&mut self, outcome: Elevated) {
        match outcome {
            Elevated::Finished(0) => {
                self.asking = None;
                self.settled();
            }
            Elevated::Finished(_) => self.refuse(String::from(
                "The command ran but did not accept the change.",
            )),
            Elevated::Printed(0, output) => {
                let captures = self.asking.as_ref().and_then(|asking| asking.captures);
                self.asking = None;
                match captures {
                    Some(Captures::Addressing) => {
                        self.read_addressing(Addressing::from_listing(&output));
                    }
                    Some(Captures::Roster) => {
                        self.read_roster(Roster::from_capture(&output));
                    }
                    None => {}
                }
            }
            Elevated::Printed(..) => self.refuse(String::from(
                "The command ran but could not read the configuration.",
            )),
            // The run happened; the reply could not carry what it printed.
            // Stated in the pane rather than as a refusal of the question,
            // because asking again would answer the same.
            Elevated::Overran => {
                let captures = self.asking.as_ref().and_then(|asking| asking.captures);
                self.asking = None;
                match captures {
                    Some(Captures::Addressing) => self.read_addressing(Addressing::Overran),
                    Some(Captures::Roster) => self.read_roster(Roster::Overran),
                    None => {}
                }
            }
            Elevated::Refused(ElevateRefusal::Credentials) => {
                self.refuse(String::from(CREDENTIAL_REFUSED_REASON));
            }
            Elevated::Refused(ElevateRefusal::NotRun(reason)) => self.refuse(reason),
        }
    }

    /// Adopt what a run that wrote a store came to.
    ///
    /// The machine's store is re-read, because that reading is free and a
    /// working copy declared durable without one would be a guess. The
    /// network store cannot be re-read without another password, so the
    /// pane records the document it asked for and says so: `configure`
    /// writes every named pair or none, and both sides render through the
    /// same engine, so a clean exit wrote exactly what was staged.
    fn settled(&mut self) {
        if self.body.stages_machine_settings() {
            self.wanted.arm(Reading::Config);
        }
        // An account change went in whole or was refused whole, so the
        // listing moves on to hold it rather than being dropped and
        // re-read — re-reading costs another password, and the reader is
        // left where they were for the next change. The public directories
        // are free, so they are re-read at once.
        if self
            .body
            .composition()
            .is_some_and(Composition::reads_roster)
        {
            self.wanted.arm(Reading::Accounts);
            if let Some(form) = self.body.form_mut() {
                form.adopt_applied();
            }
            if let Some(moved) = self.body.form().map(|form| form.roster().clone()) {
                self.accounts.roster = moved;
            }
        }
        let written = self
            .body
            .form()
            .filter(|form| form.composition().reads_addressing())
            .and_then(Form::proposal)
            .and_then(Result::ok);
        if let Some(written) = written {
            self.network.addressing = Addressing::Listed(written.clone());
            if let Some(form) = self.body.form_mut() {
                form.adopt_written(written);
            }
        }
        if let Some(band) = self.footer.as_mut() {
            band.settled();
        }
    }

    /// Adopt what a capture of the machine's addressing came to.
    ///
    /// A capture that lands for a pane the window has since left is
    /// **dropped**: the desktop can send the window elsewhere while a run
    /// is in flight, and a privileged reading installed for a surface that
    /// is no longer showing is one this application has no business
    /// holding.
    fn read_addressing(&mut self, addressing: Addressing) {
        if !self.captured_for(Captures::Addressing) {
            return;
        }
        self.network.addressing = addressing;
        if let Some(form) = self.body.form_mut() {
            form.adopt_addressing(&self.network.addressing);
        }
        // The pane has a working copy where it had none, so its band is a
        // different band: Revert and Apply rather than the reading.
        self.footer = self.band();
    }

    /// Adopt what a capture of the account listing came to, on exactly the
    /// terms the addressing capture is adopted on.
    fn read_roster(&mut self, roster: Roster) {
        if !self.captured_for(Captures::Roster) {
            return;
        }
        self.accounts.roster = roster;
        if let Some(form) = self.body.form_mut() {
            form.adopt_roster(self.accounts.roster.clone());
        }
        self.footer = self.band();
    }

    /// Whether the pane on show is the one `captures` was asked for.
    fn captured_for(&self, captures: Captures) -> bool {
        self.body
            .composition()
            .is_some_and(|composition| captures.asked_by(composition))
    }

    /// State a refusal on the question that is up, or in the band when the
    /// question has already gone.
    fn refuse(&mut self, reason: String) {
        if let Some(asking) = self.asking.as_mut() {
            asking.sheet.refuse(&reason);
        }
        if let Some(band) = self.footer.as_mut() {
            band.state(Standing::Refused(reason));
        }
    }

    /// Whether a credential question is standing over the window.
    #[must_use]
    pub const fn asking(&self) -> bool {
        self.asking.is_some()
    }

    /// Adopt what the pane's action band reported.
    fn commanded(
        &mut self,
        acted: Option<FooterAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        match acted {
            Some(FooterAction::Revert) => {
                if let Some(form) = self.body.form_mut() {
                    form.revert();
                }
                self.restate_footer();
                self.lay_out(viewport, scale, theme);
                damage.add(viewport);
                ShellOutcome::Changed
            }
            Some(FooterAction::Apply) => {
                self.ask_to_apply();
                damage.add(viewport);
                ShellOutcome::Changed
            }
            None => ShellOutcome::Changed,
        }
    }

    /// Put the credential question up for whatever the pane's command
    /// needs an account for.
    ///
    /// A staged pane's is the one `configure` run that writes every row
    /// that differs, together, so the document is rendered once and a
    /// partly-applied store is not a state this can reach. A reading's is
    /// the application that owns its subject, started and left running.
    fn ask_to_apply(&mut self) {
        let asking = if self.stageable() {
            self.staging()
        } else {
            self.reading()
        };
        if let Some(asking) = asking {
            self.asking = Some(asking);
        }
    }

    /// The run that applies what the pane on show has staged.
    ///
    /// A networking pane's document is checked whole first: an interface
    /// half-moved off a static address is a change the store would refuse,
    /// and saying so here names what is wrong rather than leaving the
    /// reader with a run that failed.
    fn staging(&mut self) -> Option<Asking> {
        let composition = self.body.composition()?;
        if composition.reads_roster() {
            return self.staging_account();
        }
        let reads_addressing = composition.reads_addressing();
        if reads_addressing {
            if let Some(Err(err)) = self.body.form().and_then(Form::proposal) {
                self.refuse(alloc::format!("{err}"));
                return None;
            }
        }
        let argv = self.body.form().map(configure_argv).unwrap_or_default();
        if argv.is_empty() {
            return None;
        }
        let purpose = if reads_addressing {
            SET_ADDRESSING_PURPOSE
        } else {
            SET_MACHINE_PURPOSE
        };
        self.carried(CONFIGURE_RUN_PATH, argv, purpose)
    }

    /// The run that applies what the Users pane has staged.
    ///
    /// One account and one tool, or nothing: a change spanning two
    /// accounts, or a password together with the fields beside it, needs
    /// two runs — and half of a change made durable is exactly the state
    /// the one-invocation rule exists to prevent. The salt is *consumed*
    /// here, so the next password is hashed under a fresh one.
    fn staging_account(&mut self) -> Option<Asking> {
        let run = match self.body.form()?.account_run(self.salt)? {
            Ok(run) => run,
            Err(err) => {
                self.refuse(String::from(err.reason()));
                return None;
            }
        };
        self.salt = None;
        self.carried(run.program, run.argv, SET_ACCOUNT_PURPOSE)
    }

    /// The credential question for a run of `program` with `argv`, or a
    /// stated refusal where the seam could not carry it.
    ///
    /// Checked against the seam's own bound rather than a second copy of
    /// it, and before the reader has typed a password: a change too large
    /// for one request is never split across two runs.
    fn carried(
        &mut self,
        program: &'static str,
        argv: Vec<String>,
        purpose: &'static str,
    ) -> Option<Asking> {
        let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
        if ElevateArgv::new(&borrowed).is_err() {
            self.refuse(String::from(TOO_MANY_CHANGES));
            return None;
        }
        Some(Asking {
            sheet: CredentialSheet::new(ASK_TITLE, purpose),
            program,
            argv,
            mode: RunMode::Wait,
            captures: None,
        })
    }

    /// The run behind the one command a pane with no working copy offers.
    fn reading(&self) -> Option<Asking> {
        match self.pane_row()?.content()? {
            PaneContent::Clock => Some(Asking {
                sheet: CredentialSheet::new(ASK_TITLE, SET_CLOCK_PURPOSE),
                program: DATETIME_RUN_PATH,
                argv: Vec::new(),
                mode: RunMode::Leave,
                captures: None,
            }),
            // A read, not a write: the same tool, run with no operand, and
            // what it prints is the answer. This application may not read
            // that store itself and never will, so the authenticated run
            // is the only way the pane can state it.
            PaneContent::Form(composition) if composition.reads_addressing() => Some(Asking {
                sheet: CredentialSheet::new(ASK_TITLE, SHOW_ADDRESSING_PURPOSE),
                program: CONFIGURE_RUN_PATH,
                argv: Vec::new(),
                mode: RunMode::Capture,
                captures: Some(Captures::Addressing),
            }),
            // A read on the same terms: the whole account registry is
            // gated, so what an authenticated listing printed is the only
            // way this pane can state another account's fields at all. The
            // interactive session cannot serve it — a captured run has its
            // standard input closed — so the non-interactive listing is
            // what is asked for.
            PaneContent::Form(composition) if composition.reads_roster() => Some(Asking {
                sheet: CredentialSheet::new(ASK_TITLE, SHOW_ACCOUNTS_PURPOSE),
                program: USERS_RUN_PATH,
                argv: alloc::vec![String::from("-l")],
                mode: RunMode::Capture,
                captures: Some(Captures::Roster),
            }),
            PaneContent::Form(_)
            | PaneContent::Pictures(_)
            | PaneContent::About
            | PaneContent::Volumes => None,
        }
    }

    /// Adopt a scroll request, answering whether the `column` moved.
    fn scrolled(&mut self, column: Rect, acted: Option<ScrollAction>, damage: &mut Region) -> bool {
        match acted {
            Some(ScrollAction::ScrollTo { offset }) => {
                self.scroll.set_model(self.scroll.model().scroll_to(offset));
                // A plate column's scroll is counted in plates, because that
                // is the unit a placed plate can move in — while on a pane
                // whose gallery is what scrolls, the form is the fixed
                // header and the offset is in tile lines.
                self.body
                    .set_first(usize::try_from(offset).unwrap_or(usize::MAX));
                damage.add(column);
                true
            }
            None => false,
        }
    }

    /// Adopt a strip-scroll request, answering whether the strip moved.
    fn strip_scrolled(
        &mut self,
        frame: &ShellFrame,
        acted: Option<ScrollAction>,
        damage: &mut Region,
    ) -> bool {
        match acted {
            Some(ScrollAction::ScrollTo { offset }) => {
                self.strip_scroll
                    .set_model(self.strip_scroll.model().scroll_to(offset));
                self.strip
                    .set_first(usize::try_from(offset).unwrap_or(usize::MAX));
                if let Some(rect) = frame.sidebar {
                    damage.add(rect);
                }
                true
            }
            None => false,
        }
    }

    /// Adopt a search edit: the strip is rebuilt from the query, and the
    /// first row a query reaches is shown, so a search always lands
    /// somewhere.
    fn searched(
        &mut self,
        acted: Option<TextAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        let Some(action) = acted else {
            return ShellOutcome::Idle;
        };
        match action {
            TextAction::Cancelled => self.search.set_text(""),
            TextAction::Edited | TextAction::Submitted => {}
        }
        self.restate_strip(viewport, scale, theme, damage);
        if matches!(action, TextAction::Submitted) {
            if let Some(location) = self.rows.first().and_then(|row| row.location()) {
                self.show(location, viewport, scale, theme, damage);
            }
        }
        ShellOutcome::Changed
    }

    /// Adopt a trail activation: a crumb other than the trailing one goes
    /// back, and the leading crumb opens the category list once the strip has
    /// been shed and there is no strip to walk.
    fn navigated(
        &mut self,
        acted: Option<BreadcrumbAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let frame = self.frame(viewport, scale, theme);
        let Some(BreadcrumbAction::Activate { index }) = acted else {
            return false;
        };
        if index == 0 {
            if frame.sidebar.is_some() {
                // The strip is on screen, so the category list would be a
                // second way to the same rows.
                self.focus_on(Focus::Strip, viewport, scale, theme, damage);
                return true;
            }
            self.open_category_list(frame, damage);
            return true;
        }
        // The middle crumb is the open category: going to it shows that
        // category's first pane.
        let Some(location) = self
            .location
            .category
            .row()
            .and_then(CategoryRow::first_pane)
            .map(|pane| Location {
                category: self.location.category,
                pane: pane.pane,
            })
        else {
            return false;
        };
        self.show(location, viewport, scale, theme, damage);
        true
    }

    /// Adopt the strip row at `index`.
    fn choose(
        &mut self,
        index: usize,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        let Some(location) = self.rows.get(index).copied().and_then(StripRow::location) else {
            return false;
        };
        self.show(location, viewport, scale, theme, damage);
        true
    }

    /// Show `location`: the strip is restated (its disclosure may have moved),
    /// the trail rewritten, the pane re-measured, and its scroll reset to the
    /// top.
    ///
    /// The whole pane band is reported rather than the column alone, because a
    /// pane of a different height may have gained or lost the scrollbar
    /// beside it.
    fn show(
        &mut self,
        location: Location,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let frame = self.frame(viewport, scale, theme);
        self.location = location;
        self.scroll.set_model(self.scroll.model().scroll_to(0));
        self.restate_body();
        self.restate_trail();
        self.restate_strip(viewport, scale, theme, damage);
        self.lay_out(viewport, scale, theme);
        damage.add(frame.breadcrumb);
        damage.add(pane_band(&frame, viewport));
    }

    /// Rebuild the strip from the registry for the open category and the
    /// current query, keeping the cursor on the row that is on show.
    fn restate_strip(&mut self, viewport: Rect, scale: Scale, theme: &Theme, damage: &mut Region) {
        self.rows = strip_rows(self.location.category, self.search.text());
        self.strip.restate(strip_of(&self.rows, self.location));
        // The row count changed, so what the strip wants and what it can show
        // did too.
        self.lay_out(viewport, scale, theme);
        let frame = self.frame(viewport, scale, theme);
        if let (Focus::Strip, Some(rect)) = (self.focus, frame.sidebar) {
            let cursor = self.selected_row();
            self.strip.set_current(cursor, rect, scale, theme, damage);
        }
        if let Some(index) = self.selected_row() {
            self.reveal_row(index, &frame, scale, theme, damage);
        }
        if let Some(rect) = frame.sidebar {
            damage.add(rect);
        }
        if let Some(rect) = frame.strip_scrollbar {
            damage.add(rect);
        }
    }

    /// Rewrite the location trail for where the surface is.
    fn restate_trail(&mut self) {
        let mut crumbs = Vec::with_capacity(3);
        crumbs.push(Crumb::new(ROOT_CRUMB));
        if let Some((category, pane)) = self.location.rows() {
            crumbs.push(Crumb::new(category.label));
            // A category holding one pane shares its name, and a trail that
            // said it twice would read as two places.
            if category.discloses() {
                crumbs.push(Crumb::new(pane.title));
            }
        }
        self.trail = Breadcrumb::new(crumbs);
    }

    /// Which strip row is the one on show, if the strip is drawing it.
    fn selected_row(&self) -> Option<usize> {
        row_on_show(&self.rows, self.location)
    }

    /// Open the category list the shed strip becomes.
    fn open_category_list(&mut self, frame: ShellFrame, damage: &mut Region) {
        let mut menu = Menu::new(
            CATEGORIES
                .iter()
                .map(|row| MenuItem::new(row.label))
                .collect(),
        );
        menu.adopt_current(
            CATEGORIES
                .iter()
                .position(|row| row.category == self.location.category),
        );
        self.categories = Some(menu);
        damage.add(frame.content);
    }

    /// Adopt what the open category list reported.
    fn list_acted(
        &mut self,
        acted: Option<MenuAction>,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> ShellOutcome {
        // The list's own rectangle, resolved while it is still open, so
        // closing it reports the pixels it actually covered.
        let frame = self.frame(viewport, scale, theme);
        let rect = self.categories.as_ref().map_or(Rect::EMPTY, |menu| {
            Self::list_rect(menu, &frame, viewport, scale, theme)
        });
        match acted {
            Some(MenuAction::Activated { index }) => {
                self.categories = None;
                damage.add(rect);
                let chosen = CATEGORIES
                    .get(index)
                    .and_then(|row| StripRow::Category(row.category).location());
                match chosen {
                    Some(location) => {
                        self.show(location, viewport, scale, theme, damage);
                        ShellOutcome::Changed
                    }
                    None => ShellOutcome::Changed,
                }
            }
            Some(MenuAction::Dismissed) => {
                self.categories = None;
                damage.add(rect);
                ShellOutcome::Changed
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => ShellOutcome::Idle,
        }
    }

    /// Where the category list is drawn: hanging off the trail's leading
    /// crumb, placed by the one shared plate rule so it never leaves the
    /// client.
    fn list_rect(
        menu: &Menu,
        frame: &ShellFrame,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Rect {
        let anchor = Rect::new(
            frame.breadcrumb.left(),
            frame.breadcrumb.top(),
            frame.breadcrumb.height,
            frame.breadcrumb.height,
        );
        plate_rect(
            menu.preferred_width(scale, theme),
            menu.preferred_height(scale, theme),
            PlatePlacement {
                anchor,
                side: PlateSide::Below,
                gap: 0,
            },
            viewport,
        )
    }

    /// The focus ring for `frame`: every region the frame actually seated, in
    /// Tab order.
    fn ring(&self, frame: &ShellFrame) -> Vec<Focus> {
        let mut ring = Vec::with_capacity(5);
        if frame.search.is_some() {
            ring.push(Focus::Search);
        }
        ring.push(Focus::Trail);
        if frame.sidebar.is_some() {
            ring.push(Focus::Strip);
        }
        // A pane composing controls is reachable whether or not it is long
        // enough to scroll; one that only scrolls is reachable only when
        // there is something to scroll.
        if self.body.composes_controls() || frame.scrollbar.is_some() {
            ring.push(Focus::Content);
        }
        if frame.footer.is_some() {
            ring.push(Focus::Footer);
        }
        ring
    }

    /// Move the cursor one step round the ring.
    fn step_focus(
        &mut self,
        forward: bool,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let ring = self.ring(&self.frame(viewport, scale, theme));
        if ring.is_empty() {
            return;
        }
        let at = ring.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % ring.len()
        } else {
            (at + ring.len() - 1) % ring.len()
        };
        let Some(&focus) = ring.get(next) else {
            return;
        };
        self.focus_on(focus, viewport, scale, theme, damage);
    }

    /// Put the cursor on `focus`, so exactly one region reads as focused.
    fn focus_on(
        &mut self,
        focus: Focus,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let frame = self.frame(viewport, scale, theme);
        if !self.ring(&frame).contains(&focus) || self.focus == focus {
            return;
        }
        self.focus = focus;
        self.search.set_focused(focus == Focus::Search);
        self.trail.adopt_focus((focus == Focus::Trail).then_some(0));
        self.scroll
            .set_focused(focus == Focus::Content && !self.body.composes_controls());
        if let Some(form) = self.body.form_mut() {
            form.set_focused(focus == Focus::Content);
            damage.add(frame.content);
        }
        if let Some(rect) = frame.sidebar {
            let cursor = (focus == Focus::Strip)
                .then(|| self.selected_row())
                .flatten();
            self.strip.set_current(cursor, rect, scale, theme, damage);
        }
        if let Some(rect) = frame.search {
            damage.add(rect);
        }
        damage.add(frame.breadcrumb);
        if let Some(rect) = frame.scrollbar {
            damage.add(rect);
        }
        if let Some(band) = self.footer.as_mut() {
            band.set_focused(focus == Focus::Footer);
        }
        if let Some(rect) = frame.footer {
            damage.add(rect);
        }
    }

    /// The readings the pane on show states, for a test that asks what a
    /// reader would see rather than comparing pixels.
    #[cfg(test)]
    pub(crate) fn facts_for_test(&self) -> Option<&crate::facts::Facts> {
        match &self.body {
            Body::Facts(facts) => Some(facts),
            Body::Statement | Body::Form(_) | Body::Pictures { .. } | Body::Volumes(_) => None,
        }
    }

    /// Where the pane's action band draws each of its commands, so a test
    /// presses the command the band actually placed rather than arithmetic
    /// of its own.
    #[cfg(test)]
    pub(crate) fn action_rects(&self, viewport: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let frame = self.frame(viewport, scale, theme);
        match (self.footer.as_ref(), frame.footer) {
            (Some(band), Some(rect)) => band.command_rects(rect, scale, theme),
            _ => Vec::new(),
        }
    }

    /// The strip rectangle of row `index` within a sidebar at `bounds`, so a
    /// test aims at the row the strip actually drew rather than at arithmetic
    /// of its own.
    #[cfg(test)]
    pub(crate) fn strip_row_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.strip.tab_area(index, bounds, scale, theme)
    }

    /// The location trail's labels, in order.
    #[cfg(test)]
    pub(crate) fn trail_labels(&self) -> Vec<&str> {
        self.trail.crumbs().iter().map(Crumb::label).collect()
    }

    /// The pane's own height in a column `width` pixels wide.
    #[cfg(test)]
    pub(crate) fn pane_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        self.content_height(width, scale, theme)
    }

    /// How far the pane column is scrolled, in physical pixels.
    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> u64 {
        self.scroll.model().offset()
    }

    /// Which row the strip is drawing from.
    #[cfg(test)]
    pub(crate) fn strip_first_for_test(&self) -> usize {
        self.strip.first()
    }

    /// Where the strip's keyboard cursor is.
    #[cfg(test)]
    pub(crate) fn strip_cursor_for_test(&self) -> Option<usize> {
        self.strip.current()
    }

    /// Scroll row `index` into view.
    #[cfg(test)]
    pub(crate) fn reveal_for_test(
        &mut self,
        index: usize,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
    ) {
        let frame = self.frame(viewport, scale, theme);
        self.reveal_row(
            index,
            &frame,
            scale,
            theme,
            &mut tairix_controls::damage::sink(),
        );
    }

    /// The strip, for a test that asks it where it seated a row.
    #[cfg(test)]
    pub(crate) fn strip_for_test(&self) -> &Tabs {
        &self.strip
    }

    /// Show `location`, as choosing its strip row would.
    #[cfg(test)]
    pub(crate) fn go_to_for_test(
        &mut self,
        location: Location,
        viewport: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        self.show(location, viewport, scale, theme, damage);
    }

    /// The form the pane on show composes, for a test that asks what it
    /// composed.
    #[cfg(test)]
    pub(crate) fn form_for_test(&self) -> Option<&crate::form::Form> {
        self.body.form()
    }

    /// The volume cards the pane on show draws, for a test that asks what
    /// the machine reported.
    #[cfg(test)]
    pub(crate) fn readings_for_test(&self) -> Option<&crate::volumes::Readings> {
        match &self.body {
            Body::Volumes(readings) => Some(readings),
            Body::Statement | Body::Form(_) | Body::Pictures { .. } | Body::Facts(_) => None,
        }
    }

    /// Which group the form is drawing from.
    #[cfg(test)]
    pub(crate) fn form_first_for_test(&self) -> Option<usize> {
        self.body.form().map(crate::form::Form::first)
    }

    /// Which group and row the form's keyboard cursor is on.
    #[cfg(test)]
    pub(crate) fn form_group_cursor_for_test(&self) -> Option<(usize, usize)> {
        self.body.form().and_then(crate::form::Form::cursor)
    }

    /// The settings the shell is showing.
    #[cfg(test)]
    pub(crate) fn settings_for_test(&self) -> &DesktopSettings {
        &self.settings
    }

    /// Choose the value at `index` for the pane's group `group` row `row`,
    /// and bring the action band up to date with it exactly as a committed
    /// choice does.
    #[cfg(test)]
    pub(crate) fn choose_for_test(&mut self, group: usize, row: usize, index: usize) -> bool {
        let staged = self
            .body
            .form_mut()
            .map(|form| form.choose_for_test(group, row, index));
        if let Some(form) = self.body.form_mut() {
            form.restate_badges();
        }
        self.restate_footer();
        matches!(staged, Some(FormOutcome::Staged))
    }

    /// What the window is holding of the machine's addressing, for a test
    /// that asks whether a privileged reading outlived its pane.
    #[cfg(test)]
    pub(crate) const fn addressing_for_test(&self) -> &Addressing {
        &self.network.addressing
    }

    /// What the window is holding of the account listing, for a test that
    /// asks whether a privileged reading outlived its pane.
    #[cfg(test)]
    pub(crate) const fn roster_for_test(&self) -> &Roster {
        &self.accounts.roster
    }

    /// Whether the window holds a salt to hash a password under, for a
    /// test that asks what an apply does without one.
    #[cfg(test)]
    pub(crate) const fn has_salt_for_test(&self) -> bool {
        self.salt.is_some()
    }

    /// What the pane's action band is saying, for a test that asks what a
    /// reader would read there.
    #[cfg(test)]
    pub(crate) fn band_line_for_test(&self) -> Option<String> {
        self.footer.as_ref().map(crate::footer::Footer::line)
    }

    /// Type `text` into the pane's group `group` row `row`, and bring the
    /// plates and the band up to date with it exactly as a keystroke does.
    ///
    /// A test seam over the *routing* only: what it exercises is the
    /// staged set, the row's verdict and the band, none of which an
    /// entry's own editing mechanics (which `lib/controls` tests) has any
    /// part in.
    #[cfg(test)]
    pub(crate) fn type_for_test(&mut self, group: usize, row: usize, text: &str) -> bool {
        let typed = self
            .body
            .form_mut()
            .map(|form| form.type_for_test(group, row, text));
        if let Some(form) = self.body.form_mut() {
            form.restate_badges();
        }
        self.restate_footer();
        matches!(typed, Some(FormOutcome::Staged))
    }

    /// Put the keyboard cursor on the pane column.
    #[cfg(test)]
    pub(crate) fn focus_content_for_test(&mut self, viewport: Rect, scale: Scale, theme: &Theme) {
        self.focus_on(
            Focus::Content,
            viewport,
            scale,
            theme,
            &mut tairix_controls::damage::sink(),
        );
    }
}

/// Where a composed pane's form is drawn, gathered once for the call that
/// needs it.
fn place(bounds: Rect, viewport: Rect, scale: Scale, theme: &Theme) -> FormPlace<'_> {
    FormPlace {
        bounds,
        viewport,
        scale,
        theme,
    }
}

/// Why an elevated run did not change anything.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElevateRefusal {
    /// The account and password were not accepted. One answer for a wrong
    /// password, an unknown account, and a locked one — the broker gives
    /// no more, and this repeats exactly what it was told.
    Credentials,
    /// The account was accepted and the program did not run, with the
    /// reason the caller was given.
    NotRun(String),
}

/// The title every credential question this surface asks carries.
const ASK_TITLE: &str = "Authenticate";

/// What the question says a machine setting needs an account for.
const SET_MACHINE_PURPOSE: &str =
    "Changing this machine's configuration needs an account that may write it.";

/// What the question says the clock needs an account for.
const SET_CLOCK_PURPOSE: &str = "Setting the date and time needs an account that may.";

/// What the question says reading the machine's addressing needs an
/// account for.
const SHOW_ADDRESSING_PURPOSE: &str =
    "This machine's network addressing names its hardware and its addresses, so reading it needs \
     an account that may.";

/// What the band says when a change is larger than one request carries.
///
/// The seam bounds what an unprivileged caller may hand a privileged run,
/// and the change goes in one invocation or not at all, so the answer is to
/// make it in smaller pieces — one interface's addressing is always small
/// enough.
const TOO_MANY_CHANGES: &str =
    "More changes than one command can carry. Apply one interface at a time.";

/// What the question says reading the accounts needs an account for.
const SHOW_ACCOUNTS_PURPOSE: &str =
    "An account's own fields, whether it may log in, and what it is allowed to do are not public, \
     so reading them needs an account that may administer users.";

/// What the question says changing an account needs an account for.
///
/// The whole registry is gated, so this is asked even of a reader editing
/// their own record: an unprivileged self-service change would be a new
/// authority path rather than a wider gate, and this surface offers none.
const SET_ACCOUNT_PURPOSE: &str = "Changing an account needs an account that may administer users.";

/// What the question says the machine's addressing needs an account for.
const SET_ADDRESSING_PURPOSE: &str =
    "Changing how this machine's interfaces are addressed needs an account that may write it.";

/// The tool that owns the machine's boot-time configuration store, which is
/// the only thing that writes it.
const CONFIGURE_RUN_PATH: &str = "/System/Commands/configure.app/Run";

/// The application that owns the wall clock.
const DATETIME_RUN_PATH: &str = "/System/Applications/datetime.app/Run";

/// The tool that answers the account and group listing.
const USERS_RUN_PATH: &str = "/System/Commands/users.app/Run";

/// The command line that applies everything `form` has staged: one
/// `<key> <value>` pair per row that differs, in registry order.
///
/// One invocation rather than one per row, because the tool renders the
/// document once: a run per key could leave the store holding half a
/// change if the second were refused.
fn configure_argv(form: &Form) -> Vec<String> {
    form.pending()
        .into_iter()
        .flat_map(|(key, value)| [key, value])
        .collect()
}

/// The shell outcome a form's answer implies.
fn outcome_of(acted: FormOutcome) -> ShellOutcome {
    match acted {
        FormOutcome::Idle => ShellOutcome::Idle,
        // A staged edit changed only a working copy; like any other change
        // that is not durable, all it asks for is a repaint — the pane's
        // action band included, which the caller redraws with it.
        FormOutcome::Changed | FormOutcome::Staged => ShellOutcome::Changed,
        FormOutcome::Apply(document) => ShellOutcome::Apply(document),
    }
}

/// The band a pane occupies: its column and whatever the scrollbar takes
/// beside it, so a change that moves the bar reports the strip it vacated.
fn pane_band(frame: &ShellFrame, viewport: Rect) -> Rect {
    let width = u32::try_from(viewport.right().saturating_sub(frame.content.left())).unwrap_or(0);
    Rect::new(
        frame.content.left(),
        frame.content.top(),
        width,
        frame.content.height,
    )
}

/// The scroll model an unmeasured column starts at: nothing to scroll, and
/// nothing to move it by.
///
/// The steps belong to the unit the column turns out to be counted in, which
/// only a layout knows, so a model with no column yet declares none — and a
/// zero step moves nothing rather than a guessed distance.
fn empty_scroll() -> ScrollModel {
    ScrollModel::new(ScrollRange::new(0, 0, 0), 0, 0)
}

/// `model` with the step distances of the unit its extent is counted in.
///
/// A model's steps are in its own scroll unit, so a column counted in whole
/// plates or rows steps by *one of them* — a pixel distance there would send
/// a single wheel tick to the far end of the list. A page is what the column
/// actually shows either way, which is what the reader expects a page to be.
fn stepped(model: ScrollModel, in_pixels: bool) -> ScrollModel {
    let seen = model.range().viewport_extent();
    if in_pixels {
        return ScrollModel::new(model.range(), LINE_STEP, seen.max(LINE_STEP));
    }
    ScrollModel::new(model.range(), 1, seen.max(1))
}

/// The strip a row list implies: a glyph and a disclosure chevron on each
/// category row, an indent on each disclosed pane row, and the row on show
/// selected.
fn strip_of(rows: &[StripRow], location: Location) -> Tabs {
    let mut strip = Tabs::new(
        rows.iter()
            .map(|row| match row {
                StripRow::Category(category) => {
                    let (label, icon, discloses) = category
                        .row()
                        .map_or(("", IconKind::Generic, false), |row| {
                            (row.label, row.icon, row.discloses())
                        });
                    let tab = Tab::new(label).with_icon(icon);
                    if discloses {
                        tab.with_disclosure(*category == location.category)
                    } else {
                        tab
                    }
                }
                StripRow::Pane(_, pane) => {
                    Tab::new(pane.locate().map_or("", |(_, row)| row.title)).nested()
                }
            })
            .collect(),
    )
    .with_orientation(TabsOrientation::Vertical);
    if let Some(index) = row_on_show(rows, location) {
        strip.adopt_selected(index);
    }
    strip
}

/// Which of `rows` is the pane on show: its own row where the category
/// discloses its panes, else the category row that stands for it.
///
/// One definition, read by the strip's selection and by the keyboard cursor,
/// so the row the reader sees selected is the row the cursor sits on.
fn row_on_show(rows: &[StripRow], location: Location) -> Option<usize> {
    rows.iter().position(|row| match row {
        StripRow::Pane(_, pane) => *pane == location.pane,
        StripRow::Category(category) => {
            *category == location.category && category.row().is_some_and(|row| !row.discloses())
        }
    })
}
