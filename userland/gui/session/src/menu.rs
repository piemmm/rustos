//! The desktop's one menu chain: the seat's singleton, and the only thing on
//! the system that renders a menu (`plans/NEW-MENUS.md` §1).
//!
//! A chain is a root plate and the descendants open beneath it. A **plate** is
//! a title band over a column of rows: the shared [`TitleBar`] seating no
//! commands, and the shared [`Menu`]. A **child** is either a submenu — more
//! rows from the same model — or an **attached window**: the session's own
//! information panel, or a surface the owning application draws. Both hang
//! where a submenu hangs and both die with the chain; they differ only in that
//! choosing an attached window's row detaches it.
//!
//! The application describes and the desktop decides. A client hands over a
//! model and an anchor; everything after that — titling, placement, drawing,
//! the grab, routing, traversal, dismissal, and the one outcome — is the
//! session's. Nothing a client sends pins a chain open, no client draws a plate
//! pixel, and no client learns where the pointer is inside one.
//!
//! Nothing here touches the compositor. The chain owns the model, the state,
//! and the geometry; the session presents [`surfaces`](MenuChain::surfaces)
//! and tears down what the chain no longer lists. That is what lets every rule
//! in section 1 be tested without a screen.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::window_ipc::{
    AppMenu, AppMenuItemId, AppMenuMark, AppMenuRole, AppMenuRowView, MenuRefusal,
};
use tairix_controls::{
    plate_rect, ControlRole, ControlState, FactList, Menu, MenuAction, MenuItem, MenuMark,
    PlateSide, TitleBar, TitleBarCommands, TitleBarEvent,
};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_taskbar::menu::{info_panel_width, INFO_ROW_LABEL};
use tairix_theme::Theme;
use tairix_wm::{ChromeEpoch, InputEvent, Key, NamedKey};

/// The floor a presented surface must have to be hung at all. Its ceiling is
/// the wire's own format bound, refused before a byte is mapped, and the
/// screen the session clamps it onto.
const MIN_ATTACHED_PX: u32 = 1;

/// What arriving on or choosing a row opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainChild {
    /// Nothing. Choosing the row is the chain's outcome.
    None,
    /// A plate holding the rows that name this one as their parent.
    Submenu,
    /// A surface the owning application draws, asked for on arrival.
    Panel,
    /// The session's own information panel. Its facts are the host's, read
    /// from the bundle's signed manifest before the chain opened, so an
    /// application cannot state an identity that is not its own.
    Info(FactList),
}

/// One row of the model a chain renders.
///
/// The service-facing model, which the wire model decodes *into*
/// ([`ChainModel::from_app_menu`]). It is a superset: a row here carries the
/// whole of [`ControlState`], because the desktop's own rows legitimately say
/// things — that the *system* lacks the authority for a command — that an
/// application must never be able to say about itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainRow {
    /// The submenu or panel row this one sits under; `None` on the root plate.
    parent: Option<usize>,
    /// The id an outcome names, for the rows that carry one.
    id: Option<AppMenuItemId>,
    /// What this row opens.
    child: ChainChild,
    /// Everything the shared row control draws.
    item: MenuItem,
}

impl ChainRow {
    /// A chooseable row: choosing it answers the chain with `id`.
    #[must_use]
    pub fn item(id: AppMenuItemId, item: MenuItem) -> Self {
        Self {
            parent: None,
            id: Some(id),
            child: ChainChild::None,
            item,
        }
    }

    /// A row whose child is the plate holding the rows filed under it.
    #[must_use]
    pub fn submenu(item: MenuItem) -> Self {
        Self {
            parent: None,
            id: None,
            child: ChainChild::Submenu,
            item: item.with_submenu(true),
        }
    }

    /// A row whose child is a surface the owning application draws. Choosing
    /// it detaches that surface, which is why it carries an id.
    #[must_use]
    pub fn panel(id: AppMenuItemId, item: MenuItem) -> Self {
        Self {
            parent: None,
            id: Some(id),
            child: ChainChild::Panel,
            item: item.with_submenu(true),
        }
    }

    /// A row whose child is the session's own information panel.
    #[must_use]
    pub fn info(item: MenuItem, facts: FactList) -> Self {
        Self {
            parent: None,
            id: None,
            child: ChainChild::Info(facts),
            item: item.with_submenu(true),
        }
    }

    /// This row filed under the plate row `parent` opens.
    #[must_use]
    pub const fn under(mut self, parent: usize) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Everything the shared row control draws for this row.
    #[must_use]
    pub const fn drawn(&self) -> &MenuItem {
        &self.item
    }

    /// What this row opens.
    #[must_use]
    pub const fn child(&self) -> &ChainChild {
        &self.child
    }

    /// This row beginning a new visual group.
    #[must_use]
    pub fn grouped(mut self) -> Self {
        self.item = self.item.with_group_break(true);
        self
    }
}

/// The model a chain renders: a root plate title and a parent-indexed list of
/// rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChainModel {
    title: String,
    rows: Vec<ChainRow>,
}

impl ChainModel {
    /// An empty model titled `title`.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            rows: Vec::new(),
        }
    }

    /// Append `row`, returning the index later rows file themselves under.
    pub fn push(&mut self, row: ChainRow) -> usize {
        self.rows.push(row);
        self.rows.len() - 1
    }

    /// The root plate's title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The model's rows, in declaration order.
    #[must_use]
    pub fn rows(&self) -> &[ChainRow] {
        &self.rows
    }

    /// Decode an application's wire menu into the model the chain renders,
    /// titled `title`.
    ///
    /// The wire model is a **bounded subset** of this one, and the boundary is
    /// structural rather than checked: there is no wire field for an authority
    /// state or a progress state, so a decoded row is always
    /// [`ControlState`]'s default authority. The Authority Mark says *the
    /// system* refused a command, and only the system may say it — an
    /// application painting it on its own row would be spoofing desktop
    /// chrome.
    ///
    /// A declared separator becomes the next row's group break rather than a
    /// row of its own, so a separator inside a submenu draws the divider it
    /// draws on the root plate, and no index the chain reports is a rule
    /// nothing can be chosen on.
    #[must_use]
    pub fn from_app_menu(title: &str, menu: &AppMenu, identity: Option<&FactList>) -> Self {
        let mut model = Self::new(title);
        // A declared row's index is what its children name, and a folded
        // separator takes no index here, so the two spaces are mapped rather
        // than assumed equal.
        let mut mapped: Vec<Option<usize>> = Vec::new();
        // One pending break per plate: a separator ending the root plate must
        // not put a divider above the first row of a submenu.
        let mut pending: Vec<(Option<usize>, bool)> = Vec::new();
        for (row, declared_parent) in menu.rows() {
            let parent = declared_parent.and_then(|at| mapped.get(at).copied().flatten());
            if matches!(row, AppMenuRowView::Separator) {
                mapped.push(None);
                set_pending(&mut pending, parent, true);
                continue;
            }
            let Some(mut built) = wire_row(row, identity) else {
                mapped.push(None);
                continue;
            };
            if take_pending(&mut pending, parent) {
                built = built.grouped();
            }
            if let Some(at) = parent {
                built = built.under(at);
            }
            mapped.push(Some(model.push(built)));
        }
        model
    }
}

/// Note whether the plate under `parent` owes its next row a group break.
fn set_pending(pending: &mut Vec<(Option<usize>, bool)>, parent: Option<usize>, owed: bool) {
    if let Some(slot) = pending.iter_mut().find(|(at, _)| *at == parent) {
        slot.1 = owed;
    } else {
        pending.push((parent, owed));
    }
}

/// Take the group break the plate under `parent` was owed, if any.
fn take_pending(pending: &mut [(Option<usize>, bool)], parent: Option<usize>) -> bool {
    pending
        .iter_mut()
        .find(|(at, _)| *at == parent)
        .is_some_and(|slot| core::mem::replace(&mut slot.1, false))
}

/// One declared row as the chain's own row, or `None` for a row that renders
/// nothing here (a separator, which the caller folds, or an information row
/// on a chain whose owner attested no identity).
fn wire_row(row: AppMenuRowView<'_>, identity: Option<&FactList>) -> Option<ChainRow> {
    match row {
        AppMenuRowView::Separator => None,
        AppMenuRowView::Item(item) => {
            let mut built = MenuItem::new(item.label)
                .with_mark(wire_mark(item.mark))
                .with_role(wire_role(item.role))
                .with_state(ControlState::default().with_enabled(item.enabled));
            if !item.shortcut.is_empty() {
                built = built.with_shortcut(item.shortcut);
            }
            if !item.reason.is_empty() {
                built = built.with_reason(item.reason);
            }
            Some(ChainRow::item(item.id, built))
        }
        AppMenuRowView::Submenu { label, enabled } => Some(ChainRow::submenu(
            MenuItem::new(label).with_state(ControlState::default().with_enabled(enabled)),
        )),
        AppMenuRowView::Panel { id, label, enabled } => Some(ChainRow::panel(
            id,
            MenuItem::new(label).with_state(ControlState::default().with_enabled(enabled)),
        )),
        // Without an attested identity there is nothing truthful to put in the
        // panel, so the row is left out rather than drawn opening a blank one.
        AppMenuRowView::Info => {
            identity.map(|facts| ChainRow::info(MenuItem::new(INFO_ROW_LABEL), facts.clone()))
        }
    }
}

/// The shared mark for a declared one.
const fn wire_mark(mark: AppMenuMark) -> MenuMark {
    match mark {
        AppMenuMark::None => MenuMark::None,
        AppMenuMark::Check => MenuMark::Check,
        AppMenuMark::Radio => MenuMark::Radio,
    }
}

/// The shared role for a declared one.
const fn wire_role(role: AppMenuRole) -> ControlRole {
    match role {
        AppMenuRole::Neutral => ControlRole::Neutral,
        AppMenuRole::Destructive => ControlRole::Destructive,
    }
}

/// Who asked for the chain, and therefore where its one answer goes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChainOwner {
    /// An application's window. The answer is the `MenuClosed` naming
    /// `open_id` that the engine holds it to.
    Window {
        /// The window-channel id of the window the chain is scoped by.
        window_id: u64,
        /// The session-minted id of the open this chain answers.
        open_id: u64,
    },
    /// The session itself. The answer is returned in-process; the desktop's
    /// own menus are clients of this service like any application's, and the
    /// only difference is where the model came from.
    Session,
}

/// How a chain ended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ChainOutcome {
    /// A row was chosen.
    Chosen(AppMenuItemId),
    /// The chain closed without a choice.
    Dismissed,
    /// No chain was brought up at all, for a reason about the **seat** rather
    /// than about the request: a malformed or unauthorised ask is refused by
    /// the open call itself and mints nothing.
    Refused(MenuRefusal),
}

/// What routing one event into the chain asks the session to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainAction {
    /// The event was consumed and nothing changed.
    Consumed,
    /// The chain's geometry or pixels changed; re-present its surfaces.
    Redraw,
    /// The pointer arrived on a panel row: ask the chain's owner to present a
    /// surface for it. Nothing about the chain waits for the answer.
    RequestPanel(AppMenuItemId),
    /// The chain closed.
    ///
    /// Neither the answer nor the surfaces are here. The answer leaves through
    /// [`MenuChain::take_answers`], so the one delivery point cannot be
    /// bypassed and no path can answer a chain twice; the surfaces go by the
    /// session reconciling against [`MenuChain::surfaces`], which a closed
    /// chain reports empty. An attached window is not in that list either way:
    /// the window engine settles it against the outcome, and the session doing
    /// it too would be a second rule for one decision.
    Closed,
}

/// One surface the chain occupies on screen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ChainSurface {
    /// Where it sits, in screen pixels.
    pub rect: Rect,
    /// What it is.
    pub kind: SurfaceKind,
}

/// Which of the chain's surfaces a rectangle is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SurfaceKind {
    /// The plate at this depth, root first.
    Plate(usize),
    /// The session-drawn information panel.
    Info,
    /// The attached window the application presented, by its window-channel
    /// id. The session already holds this surface; the chain only places it.
    Attached(u64),
}

/// One plate of the open chain.
#[derive(Clone, Debug)]
struct Plate {
    /// The model rows this plate shows, in order.
    rows: Vec<usize>,
    /// The shared renderer over those rows.
    menu: Menu,
    /// The band, seating no commands.
    band: TitleBar,
    /// Where the plate sits, in screen pixels.
    rect: Rect,
    /// Whether a drag pinned it: its placement has stopped being derived from
    /// the anchor, because the user put it where it is.
    pinned: bool,
    /// The row of this plate whose child is open, if any.
    open_row: Option<usize>,
}

/// What hangs where a submenu's plate would, when it is not a plate.
#[derive(Clone, Debug)]
enum Attached {
    /// Asked for on arrival and not yet answered. The chain is fully usable
    /// meanwhile, and a surface that arrives after the pointer has moved on
    /// finds no pending row and is refused.
    Pending {
        /// The panel row that asked.
        row: AppMenuItemId,
    },
    /// The session's own information panel, with the attested facts it
    /// states.
    Info {
        /// The facts, read from the owner's signed manifest by the host.
        facts: FactList,
        /// Where it sits.
        rect: Rect,
    },
    /// A surface the owning application presented.
    App {
        /// Its window-channel id, so the session can find its compositor
        /// window to place and the engine can settle it.
        window_id: u64,
        /// Where it sits.
        rect: Rect,
    },
}

/// A drag in flight: which plate the band press landed on, and where the
/// pointer was when the plate last moved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Drag {
    /// The plate depth being dragged.
    plate: usize,
    /// The pointer position the last move was measured from.
    from: Point,
}

/// The scale and theme every geometry answer is resolved at, with the screen
/// the chain must stay inside.
#[derive(Copy, Clone, Debug)]
pub struct ChainGeometry<'a> {
    /// The seat's screen rectangle.
    pub screen: Rect,
    /// The output's density.
    pub scale: Scale,
    /// The active theme.
    pub theme: &'a Theme,
    /// The mode the output is drawn at, which a chain is placed against.
    pub epoch: ChromeEpoch,
}

impl ChainGeometry<'_> {
    /// The mode a chain records so it can tell when the ground has moved
    /// under it.
    const fn mode(&self) -> (Rect, ChromeEpoch) {
        (self.screen, self.epoch)
    }
}

/// The seat's one open chain.
///
/// One per seat, owned by the session: the open chain is a single piece of
/// seat state and not a set, so opening a menu closes whatever was up and
/// answers its requester.
#[derive(Debug, Default)]
pub struct MenuChain {
    open: Option<OpenChain>,
    /// Answers owed to chains that have closed, in the order they closed.
    ///
    /// A chain very often ends where its answer cannot be delivered — inside
    /// the window engine's own serve pass, which already holds the borrow
    /// `deliver_event` needs. Queueing them all rather than only those is what
    /// makes the delivery point single, so no close can answer twice and none
    /// can answer not at all.
    answers: Vec<(ChainOwner, ChainOutcome)>,
}

/// Everything one open chain is.
#[derive(Clone, Debug)]
struct OpenChain {
    owner: ChainOwner,
    model: ChainModel,
    plates: Vec<Plate>,
    attached: Option<Attached>,
    /// The root's anchor region, in screen pixels.
    anchor: Rect,
    /// The side the root opens on.
    side: PlateSide,
    /// The clearance the root opens with.
    gap: u32,
    drag: Option<Drag>,
    /// The answer this chain has settled on, once something has ended it.
    outcome: Option<ChainOutcome>,
    /// The screen and output mode the chain was placed against.
    mode: (Rect, ChromeEpoch),
}

impl MenuChain {
    /// A seat with no chain up.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: None,
            answers: Vec::new(),
        }
    }

    /// Take the answers owed to chains that have closed since the last call.
    ///
    /// The session's one delivery point: an application's chain is answered
    /// with the `MenuClosed` the engine holds it to, and the desktop's own is
    /// answered in-process.
    pub fn take_answers(&mut self) -> Vec<(ChainOwner, ChainOutcome)> {
        core::mem::take(&mut self.answers)
    }

    /// Whether a chain is up. While one is, it holds the seat's pointer and
    /// keyboard.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The owner of the open chain, if one is up.
    #[must_use]
    pub fn owner(&self) -> Option<ChainOwner> {
        self.open.as_ref().map(|chain| chain.owner)
    }

    /// Open `model` for `owner`, anchored at `anchor` in screen pixels and
    /// opening on `side` with `gap` pixels of clearance.
    ///
    /// The chain that was up, if any, is closed and its owner queued a
    /// `Dismissed`: the chain is the seat's singleton, so a second open is an
    /// end to the first rather than a second menu beside it. A model with no
    /// rows on its root plate opens nothing and is refused, because a plate
    /// with nothing to choose is a surface the user cannot leave by choosing.
    ///
    /// The surfaces of the displaced chain are the caller's to take down,
    /// which it does by reconciling against [`surfaces`](Self::surfaces).
    ///
    /// # Errors
    ///
    /// [`ModelRefused`] for a model that cannot be shown; the chain that was
    /// up is left exactly as it was.
    pub fn open(
        &mut self,
        owner: ChainOwner,
        model: ChainModel,
        anchor: Rect,
        side: PlateSide,
        gap: u32,
        geom: &ChainGeometry<'_>,
    ) -> Result<(), ModelRefused> {
        let root = plate_for(&model, None, geom)?;
        self.close_open();
        let mut chain = OpenChain {
            owner,
            model,
            plates: alloc::vec![root],
            attached: None,
            anchor,
            side,
            gap,
            drag: None,
            outcome: None,
            mode: geom.mode(),
        };
        chain.replace_geometry(geom);
        self.open = Some(chain);
        Ok(())
    }

    /// End the chain that is up, queueing its owner a `Dismissed`.
    fn close_open(&mut self) -> bool {
        let Some(chain) = self.open.take() else {
            return false;
        };
        self.answers.push((chain.owner, ChainOutcome::Dismissed));
        true
    }

    /// Every surface the chain occupies, root plate first and the attached
    /// window (if any) last.
    ///
    /// The session presents exactly this list and tears down anything it holds
    /// that the list no longer names, so a surface can never outlive the state
    /// that placed it.
    #[must_use]
    pub fn surfaces(&self) -> Vec<ChainSurface> {
        let Some(chain) = self.open.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<ChainSurface> = chain
            .plates
            .iter()
            .enumerate()
            .map(|(depth, plate)| ChainSurface {
                rect: plate.rect,
                kind: SurfaceKind::Plate(depth),
            })
            .collect();
        match chain.attached.as_ref() {
            Some(Attached::Info { rect, .. }) => out.push(ChainSurface {
                rect: *rect,
                kind: SurfaceKind::Info,
            }),
            Some(Attached::App {
                window_id, rect, ..
            }) => out.push(ChainSurface {
                rect: *rect,
                kind: SurfaceKind::Attached(*window_id),
            }),
            Some(Attached::Pending { .. }) | None => {}
        }
        out
    }

    /// The screen rectangle row `row` of the plate at `depth` occupies.
    ///
    /// The forward mirror of the chain's own hit-testing, so a caller aiming
    /// *at* a row reads the rectangle the chain lays out rather than a
    /// re-derived guess. `None` for a depth or row the chain does not have.
    #[must_use]
    pub fn row_rect(&self, depth: usize, row: usize, geom: &ChainGeometry<'_>) -> Option<Rect> {
        row_rect(self.open.as_ref()?.plates.get(depth)?, row, geom)
    }

    /// Paint the plate at `depth` into `surface`, whose extent is that plate's
    /// own rectangle.
    ///
    /// The band and the rows are the two shared controls over one shared plate
    /// ground; nothing here is a second recipe for any of the three, and the
    /// rows take that ground rather than laying one of their own.
    pub fn render_plate(
        &self,
        depth: usize,
        surface: &mut tairix_raster::Surface,
        geom: &ChainGeometry<'_>,
    ) {
        let Some(chain) = self.open.as_ref() else {
            return;
        };
        let Some(plate) = chain.plates.get(depth) else {
            return;
        };
        let local = Rect::new(0, 0, plate.rect.width, plate.rect.height);
        let band_h = TitleBar::band_height(geom.scale, geom.theme).min(local.height);
        let radius = geom
            .scale
            .scale_length(geom.theme.metrics().popup_corner_radius);
        // The band lays no ground of its own, so the plate's is laid first —
        // through the one shared plate recipe, never a second wash here.
        let _ = tairix_controls::paint_surface_plate(
            surface,
            (0, 0, local.width, local.height),
            (
                radius.min(local.width / 2).min(local.height / 2),
                tairix_controls::plate_border(geom.theme, geom.scale),
            ),
            geom.theme,
            (
                geom.theme.palette().surface_raised,
                tairix_controls::ChromeLayer::Ground,
            ),
        );
        plate.band.render(
            surface,
            Rect::new(0, 0, local.width, band_h),
            geom.scale,
            geom.theme,
            None,
        );
        // Rows only: the ground and rim under them are the plate's, laid
        // once above, so a second plate here would rim and round the rows
        // inside the one they already sit on.
        plate.menu.render_rows(
            surface,
            Rect::new(
                0,
                i32::try_from(band_h).unwrap_or(i32::MAX),
                local.width,
                local.height.saturating_sub(band_h),
            ),
            geom.scale,
            geom.theme,
        );
    }

    /// The information panel the chain has open, if it has one: the attested
    /// facts it states and the rectangle it occupies.
    #[must_use]
    pub fn info_panel(&self) -> Option<(&FactList, Rect)> {
        match self.open.as_ref()?.attached.as_ref()? {
            Attached::Info { facts, rect } => Some((facts, *rect)),
            Attached::Pending { .. } | Attached::App { .. } => None,
        }
    }

    /// Place an application's attached window for `row` at `width` × `height`,
    /// or report why it cannot hang there.
    ///
    /// The host asks this the moment a `CreateMenuPanel` lands, before it
    /// commits a compositor window to it. **The chain, not the engine, decides
    /// whether the row is real and whether it is too late**: the engine does
    /// not retain the model, and only the chain knows where the pointer has
    /// settled since the arrival went out. Refusing here is what stops a slow
    /// or hostile application planting a panel under a row the user has left.
    ///
    /// # Errors
    ///
    /// [`PanelRefused`] when no chain is up, the chain up is not the one that
    /// asked, the pending row is not this one, or the surface has no extent to
    /// place.
    pub fn place_panel(
        &mut self,
        window_id: u64,
        open_id: u64,
        row: AppMenuItemId,
        width: u32,
        height: u32,
        geom: &ChainGeometry<'_>,
    ) -> Result<Rect, PanelRefused> {
        if width < MIN_ATTACHED_PX || height < MIN_ATTACHED_PX {
            return Err(PanelRefused::NoExtent);
        }
        let chain = self.open.as_mut().ok_or(PanelRefused::NoChain)?;
        // A surface answering an arrival from a chain that has since been
        // replaced belongs to no chain that is up, whatever its row says.
        if !matches!(chain.owner, ChainOwner::Window { open_id: id, .. } if id == open_id) {
            return Err(PanelRefused::TooLate);
        }
        match chain.attached.as_ref() {
            Some(Attached::Pending { row: pending }) if *pending == row => {}
            _ => return Err(PanelRefused::TooLate),
        }
        let rect = chain.hang(width, height, geom);
        chain.attached = Some(Attached::App { window_id, rect });
        Ok(rect)
    }

    /// Close the chain, answering its owner `Dismissed`.
    ///
    /// The seat calls this for every reason a chain ends that is not the user
    /// choosing a row: the owner died, the seat was lost, the output was
    /// resized, the scale or theme changed. Re-placing a plate the user has
    /// dragged is not defined, so a mode change under the gesture ends the
    /// chain rather than moving it.
    /// Returns whether a chain was up.
    pub fn dismiss(&mut self) -> bool {
        self.close_open()
    }

    /// Answer `owner` that no chain could be brought up, for a reason about
    /// the seat.
    ///
    /// The open was accepted, so the application is owed exactly one answer;
    /// this is it. Whatever chain is up stays up: a refusal is a fact about
    /// the seat at this instant, not a reason to take down a menu the user is
    /// already using.
    pub fn refuse(&mut self, owner: ChainOwner, reason: MenuRefusal) {
        self.answers.push((owner, ChainOutcome::Refused(reason)));
    }

    /// End the chain that is up because the session cannot give it a surface,
    /// answering `NoResources`. Returns whether one was up.
    ///
    /// The one place that reason is honest: a chain is refused for want of
    /// memory when the memory is actually refused, which is when its plate is
    /// drawn, not when it is asked for.
    pub fn exhausted(&mut self) -> bool {
        let Some(chain) = self.open.take() else {
            return false;
        };
        self.answers
            .push((chain.owner, ChainOutcome::Refused(MenuRefusal::NoResources)));
        true
    }

    /// End the chain if the ground has moved under it: the seat's output
    /// resized, or its scale or theme switched. Returns whether it did.
    ///
    /// One rule the chain enforces on itself rather than a dismissal every
    /// mode-changing call site has to remember, because forgetting one leaves
    /// a plate placed against a screen that no longer exists. Re-placing is
    /// not the answer: a plate the user has dragged has a position that is
    /// theirs, and no rule can carry it onto a different screen.
    pub fn settle_mode(&mut self, geom: &ChainGeometry<'_>) -> bool {
        let moved = self
            .open
            .as_ref()
            .is_some_and(|chain| chain.mode != geom.mode());
        moved && self.close_open()
    }

    /// Close the chain if `window_id` owns it — the window closed, or the
    /// application behind it died. Returns whether it did.
    pub fn dismiss_owner(&mut self, window_id: u64) -> bool {
        let owned = matches!(
            self.owner(),
            Some(ChainOwner::Window { window_id: id, .. }) if id == window_id
        );
        owned && self.close_open()
    }

    /// Whether `window_id` is the attached window of the open chain.
    ///
    /// The session asks before treating a served window as an ordinary one:
    /// an attached window's pixels are the application's, but its placement
    /// and its lifetime are the chain's.
    #[must_use]
    pub fn attaches(&self, window_id: u64) -> bool {
        matches!(
            self.open.as_ref().and_then(|c| c.attached.as_ref()),
            Some(Attached::App { window_id: id, .. }) if *id == window_id
        )
    }

    /// Route one seat event into the chain.
    ///
    /// While a chain is up the seat's pointer and keyboard are its own: every
    /// event routes here first and none of them reaches what is behind. A
    /// press outside the chain dismisses it and is **consumed** — a dismissal
    /// must not double as a click on whatever the menu was covering.
    pub fn handle(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        geom: &ChainGeometry<'_>,
    ) -> ChainAction {
        let Some(chain) = self.open.as_mut() else {
            return ChainAction::Consumed;
        };
        let acted = match event {
            InputEvent::KeyPressed { key, .. } => chain.on_key(*key, geom),
            InputEvent::KeyReleased { .. } | InputEvent::ModifiersChanged { .. } => {
                ChainAction::Consumed
            }
            _ => chain.on_pointer(event, pointer, geom),
        };
        if matches!(acted, ChainAction::Closed) {
            let outcome = chain_outcome(self.open.as_mut());
            if let Some(chain) = self.open.take() {
                self.answers.push((chain.owner, outcome));
            }
        }
        acted
    }
}

/// Why a model cannot be shown as a chain.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModelRefused {
    /// Its root plate has no rows: a plate the user cannot leave by choosing
    /// anything is not a menu.
    NoRows,
}

/// Why an application's attached window cannot hang where it asked.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PanelRefused {
    /// No chain is up. The gesture the surface answers is over.
    NoChain,
    /// The pointer has settled somewhere else since the arrival went out, or
    /// the row never asked for a surface at all.
    TooLate,
    /// The surface has no extent to place.
    NoExtent,
}

impl OpenChain {
    /// The chain ending with `outcome`, recorded so `MenuChain::handle` can
    /// queue the answer and snapshot the surfaces the chain still holds.
    fn closing(&mut self, outcome: ChainOutcome) -> ChainAction {
        self.outcome = Some(outcome);
        ChainAction::Closed
    }

    /// Re-derive every unpinned plate's placement, and whatever hangs off the
    /// deepest one.
    ///
    /// A dragged plate keeps the position the user gave it; its children are
    /// placed against it as usual, so a chain drags as one piece and its
    /// descendants stay edge-adjacent.
    fn replace_geometry(&mut self, geom: &ChainGeometry<'_>) {
        for depth in 0..self.plates.len() {
            if self.plates[depth].pinned {
                continue;
            }
            let (width, height) = plate_extent(&self.plates[depth], geom);
            let rect = match depth.checked_sub(1) {
                None => plate_rect(width, height, self.anchor, self.side, self.gap, geom.screen),
                Some(parent) => {
                    let against = self.child_anchor(parent, geom);
                    plate_rect(width, height, against, PlateSide::Trailing, 0, geom.screen)
                }
            };
            self.plates[depth].rect = rect;
        }
        let hung = match self.attached.as_ref() {
            Some(Attached::Info { rect, .. } | Attached::App { rect, .. }) => {
                Some((rect.width, rect.height))
            }
            Some(Attached::Pending { .. }) | None => None,
        };
        if let Some((width, height)) = hung {
            let rect = self.hang(width, height, geom);
            match self.attached.as_mut() {
                Some(Attached::Info { rect: at, .. } | Attached::App { rect: at, .. }) => {
                    *at = rect;
                }
                Some(Attached::Pending { .. }) | None => {}
            }
        }
    }

    /// The region a child of the plate at `depth` is placed against: that
    /// plate's own horizontal span, at the height of the row whose child it
    /// is. Edge-adjacency to the plate is what leaves the pointer no dead gap
    /// to cross on its way in.
    fn child_anchor(&self, depth: usize, geom: &ChainGeometry<'_>) -> Rect {
        let Some(plate) = self.plates.get(depth) else {
            return Rect::EMPTY;
        };
        let row = plate
            .open_row
            .and_then(|index| row_rect(plate, index, geom))
            .unwrap_or(plate.rect);
        Rect::new(plate.rect.left(), row.top(), plate.rect.width, row.height)
    }

    /// Where a `width` × `height` attached window hangs: exactly where the
    /// child plate of the same row would, so a surface and a submenu are
    /// placed by one rule.
    fn hang(&self, width: u32, height: u32, geom: &ChainGeometry<'_>) -> Rect {
        let deepest = self.plates.len().saturating_sub(1);
        let against = self.child_anchor(deepest, geom);
        plate_rect(width, height, against, PlateSide::Trailing, 0, geom.screen)
    }

    /// Close every plate deeper than `depth`, and anything hanging off them.
    fn truncate_to(&mut self, depth: usize) {
        self.plates.truncate(depth + 1);
        self.attached = None;
        if let Some(plate) = self.plates.get_mut(depth) {
            plate.open_row = None;
        }
    }

    /// The chain's surface the pointer is over, deepest first — a child is
    /// drawn over its parent, so the deepest match owns the point.
    fn surface_at(&self, pointer: Point) -> Option<Hit> {
        match self.attached.as_ref() {
            Some(Attached::Info { rect, .. } | Attached::App { rect, .. })
                if rect.contains(pointer) =>
            {
                return Some(Hit::Attached);
            }
            _ => {}
        }
        self.plates
            .iter()
            .enumerate()
            .rev()
            .find(|(_, plate)| plate.rect.contains(pointer))
            .map(|(depth, _)| Hit::Plate(depth))
    }

    /// Route a pointer event.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        geom: &ChainGeometry<'_>,
    ) -> ChainAction {
        if let Some(action) = self.drag_pointer(event, pointer, geom) {
            return action;
        }
        let hit = self.surface_at(pointer);
        match (event, hit) {
            // A press with nothing of the chain under it ends the chain, and
            // the press goes no further: a dismissal is not also a click on
            // what the menu was covering.
            (InputEvent::PointerPressed { .. }, None) => self.closing(ChainOutcome::Dismissed),
            // An attached window's own input is the application's, as any
            // window's is. The chain neither reads it nor reports it.
            (_, Some(Hit::Attached) | None) => ChainAction::Consumed,
            (_, Some(Hit::Plate(depth))) => self.plate_pointer(depth, event, pointer, geom),
        }
    }

    /// Route a pointer event that landed on the plate at `depth`.
    fn plate_pointer(
        &mut self,
        depth: usize,
        event: &InputEvent,
        pointer: Point,
        geom: &ChainGeometry<'_>,
    ) -> ChainAction {
        let Some(plate) = self.plates.get_mut(depth) else {
            return ChainAction::Consumed;
        };
        let band = Rect::new(
            plate.rect.left(),
            plate.rect.top(),
            plate.rect.width,
            TitleBar::band_height(geom.scale, geom.theme).min(plate.rect.height),
        );
        let mut damage = Region::new();
        if band.contains(pointer) {
            let acted = plate
                .band
                .on_pointer(event, band, geom.scale, geom.theme, &mut damage);
            return self.band_event(depth, acted, pointer, geom);
        }
        let rows = rows_rect(plate, geom);
        let acted = plate
            .menu
            .on_pointer(event, rows, geom.scale, geom.theme, &mut damage);
        let over = plate.menu.row_at(rows, geom.scale, geom.theme, pointer);
        match acted {
            Some(MenuAction::Activated { index }) => self.choose(depth, index),
            // A panel row wears a chevron like a submenu's, so the shared
            // control reports a click on it the same way. Clicking one
            // detaches its window; clicking a submenu row keeps its plate.
            Some(MenuAction::OpenSubmenu { index }) => {
                if self.detachable(depth, index) {
                    self.choose(depth, index)
                } else {
                    self.arrive(depth, Some(index), geom)
                }
            }
            Some(MenuAction::Dismissed) => self.closing(ChainOutcome::Dismissed),
            None => {
                // Arrival, with no click and no timer: a child opens when the
                // pointer reaches its row and closes when the pointer settles
                // on a *different* row of the same plate — never merely
                // because it left the row's rectangle, which is what
                // travelling into the child does.
                //
                // Whether the *highlight* moved is the row control's answer,
                // reported as damage, so a pointer travelling within one row
                // costs no repaint.
                let acted = if matches!(event, InputEvent::PointerMoved { .. }) {
                    self.arrive(depth, over, geom)
                } else {
                    ChainAction::Consumed
                };
                if acted == ChainAction::Consumed && !damage.is_empty() {
                    return ChainAction::Redraw;
                }
                acted
            }
        }
    }

    /// Apply whatever the band reported for the plate at `depth`.
    fn band_event(
        &mut self,
        depth: usize,
        acted: Option<TitleBarEvent>,
        pointer: Point,
        geom: &ChainGeometry<'_>,
    ) -> ChainAction {
        match acted {
            // The press itself takes the pointer, before the threshold has
            // decided whether it is a drag: a gesture that crosses off the
            // band must still be the one the press began.
            Some(TitleBarEvent::Activate) => {
                self.drag = Some(Drag {
                    plate: depth,
                    from: pointer,
                });
                ChainAction::Consumed
            }
            Some(TitleBarEvent::DragBegin) => {
                // Dragging pins the plate: its placement stops being derived
                // from the anchor, because the user put it where it is. The
                // motion that crossed the threshold is part of the drag, so it
                // moves the plate too — otherwise a short drag ending on that
                // very motion leaves the plate where it started.
                if let Some(plate) = self.plates.get_mut(depth) {
                    plate.pinned = true;
                }
                self.drag_to(pointer, geom)
            }
            Some(TitleBarEvent::DragMoved { to }) => self.drag_to(to, geom),
            // A plate band seats no commands, so nothing else it can report
            // means anything here.
            Some(
                TitleBarEvent::DragEnd
                | TitleBarEvent::Control(_)
                | TitleBarEvent::AlternateControl(_),
            )
            | None => ChainAction::Consumed,
        }
    }

    /// Feed a band press that has taken the pointer, wherever it has gone.
    fn drag_pointer(
        &mut self,
        event: &InputEvent,
        pointer: Point,
        geom: &ChainGeometry<'_>,
    ) -> Option<ChainAction> {
        let depth = self.drag?.plate;
        let plate = self.plates.get_mut(depth)?;
        let band = Rect::new(
            plate.rect.left(),
            plate.rect.top(),
            plate.rect.width,
            TitleBar::band_height(geom.scale, geom.theme).min(plate.rect.height),
        );
        let mut damage = Region::new();
        let acted = plate
            .band
            .on_pointer(event, band, geom.scale, geom.theme, &mut damage);
        let action = self.band_event(depth, acted, pointer, geom);
        // The button coming up ends the gesture whether or not it ever passed
        // the drag threshold, so a press that merely tapped the band does not
        // leave the pointer held.
        if matches!(event, InputEvent::PointerReleased { .. }) {
            self.drag = None;
        }
        Some(action)
    }

    /// Move the dragged plate and its descendants to follow the pointer.
    ///
    /// Ancestors stay put: a press on a band moves that plate and what hangs
    /// beneath it, which is what lets a user pull a submenu aside to read what
    /// is under it without disturbing the chain it came from.
    fn drag_to(&mut self, to: Point, geom: &ChainGeometry<'_>) -> ChainAction {
        let Some(drag) = self.drag.as_mut() else {
            return ChainAction::Consumed;
        };
        let depth = drag.plate;
        let dx = to.x - drag.from.x;
        let dy = to.y - drag.from.y;
        drag.from = to;
        if dx == 0 && dy == 0 {
            return ChainAction::Consumed;
        }
        let Some(plate) = self.plates.get_mut(depth) else {
            return ChainAction::Consumed;
        };
        plate.rect = Rect::new(
            plate.rect.left() + dx,
            plate.rect.top() + dy,
            plate.rect.width,
            plate.rect.height,
        )
        .clamped_onto(geom.screen);
        // A descendant is placed against its parent, so re-deriving the
        // unpinned ones below is what makes the chain travel as one piece.
        for below in self.plates.iter_mut().skip(depth + 1) {
            below.pinned = false;
        }
        self.replace_geometry(geom);
        ChainAction::Redraw
    }

    /// The pointer settled on `row` of the plate at `depth`.
    fn arrive(
        &mut self,
        depth: usize,
        row: Option<usize>,
        geom: &ChainGeometry<'_>,
    ) -> ChainAction {
        let Some(plate) = self.plates.get(depth) else {
            return ChainAction::Consumed;
        };
        if plate.open_row == row {
            return ChainAction::Consumed;
        }
        // A row with no child still closes the child the plate had: the
        // pointer settling elsewhere on this plate is exactly the rule that
        // closes an open child.
        let Some(index) = row else {
            if plate.open_row.is_none() {
                return ChainAction::Consumed;
            }
            self.truncate_to(depth);
            return ChainAction::Redraw;
        };
        let Some(&model_row) = plate.rows.get(index) else {
            return ChainAction::Consumed;
        };
        let Some(entry) = self.model.rows.get(model_row) else {
            return ChainAction::Consumed;
        };
        // A disabled row opens nothing, and leaves whatever was open alone: a
        // pointer resting on a row it cannot use has not chosen to close
        // anything.
        if !entry.item.state().is_actionable() {
            return ChainAction::Consumed;
        }
        let child = entry.child.clone();
        let id = entry.id;
        // Whether anything was actually taken down, which is what tells a
        // no-child arrival from one that closed a plate.
        let closed = self.plates.len() > depth + 1 || self.attached.is_some();
        self.truncate_to(depth);
        if let Some(plate) = self.plates.get_mut(depth) {
            plate.open_row = Some(index);
        }
        match child {
            // A row with no child opened nothing, so whether anything has to
            // be redrawn is the highlight's question, not this one's.
            ChainChild::None => {
                if let Some(plate) = self.plates.get_mut(depth) {
                    plate.open_row = None;
                }
                if closed {
                    ChainAction::Redraw
                } else {
                    ChainAction::Consumed
                }
            }
            ChainChild::Submenu => {
                match plate_for(&self.model, Some(model_row), geom) {
                    Ok(child) => self.plates.push(child),
                    // A submenu row whose plate holds nothing opens nothing,
                    // rather than a plate with no way out of it.
                    Err(ModelRefused::NoRows) => {
                        if let Some(plate) = self.plates.get_mut(depth) {
                            plate.open_row = None;
                        }
                        return ChainAction::Redraw;
                    }
                }
                self.replace_geometry(geom);
                ChainAction::Redraw
            }
            ChainChild::Info(facts) => {
                let width = info_panel_width(geom.scale, geom.theme);
                let height = facts.measured_height(geom.scale, geom.theme).max(1);
                self.attached = Some(Attached::Info {
                    facts,
                    rect: self.hang(width, height, geom),
                });
                ChainAction::Redraw
            }
            // Nothing about the chain waits for the surface: the arrival goes
            // out, the chain stays live, and one that lands after the pointer
            // has moved on finds no pending row. A chain the session itself
            // opened has no client to ask, so the row opens nothing.
            ChainChild::Panel => match (id, self.owner) {
                (Some(row), ChainOwner::Window { .. }) => {
                    self.attached = Some(Attached::Pending { row });
                    ChainAction::RequestPanel(row)
                }
                _ => ChainAction::Redraw,
            },
        }
    }

    /// Whether row `index` of the plate at `depth` is one whose click
    /// detaches an attached window rather than opening a plate.
    fn detachable(&self, depth: usize, index: usize) -> bool {
        self.plates
            .get(depth)
            .and_then(|plate| plate.rows.get(index))
            .and_then(|&row| self.model.rows.get(row))
            .is_some_and(|row| matches!(row.child, ChainChild::Panel))
    }

    /// A row of the plate at `depth` was chosen.
    fn choose(&mut self, depth: usize, index: usize) -> ChainAction {
        let Some(&model_row) = self.plates.get(depth).and_then(|p| p.rows.get(index)) else {
            return ChainAction::Consumed;
        };
        let Some(entry) = self.model.rows.get(model_row) else {
            return ChainAction::Consumed;
        };
        match entry.id {
            Some(id) => self.closing(ChainOutcome::Chosen(id)),
            // A submenu and the information row name no id, so there is
            // nothing for choosing one to answer: it opens or keeps its child.
            None => ChainAction::Consumed,
        }
    }

    /// Route a key. Traversal is the service's, not any application's.
    fn on_key(&mut self, key: Key, geom: &ChainGeometry<'_>) -> ChainAction {
        let deepest = self.plates.len().saturating_sub(1);
        // Escape closes the deepest open child first, so repeated Escape
        // always gets the user out and a panel with a field in it closes
        // before the menu that opened it.
        if key == Key::Named(NamedKey::Escape) {
            if self.attached.is_some() {
                self.truncate_to(deepest);
                return ChainAction::Redraw;
            }
            if deepest == 0 {
                return self.closing(ChainOutcome::Dismissed);
            }
            self.truncate_to(deepest - 1);
            return ChainAction::Redraw;
        }
        if key == Key::Named(NamedKey::Left) {
            if deepest == 0 && self.attached.is_none() {
                return ChainAction::Consumed;
            }
            let back = if self.attached.is_some() {
                deepest
            } else {
                deepest - 1
            };
            self.truncate_to(back);
            return ChainAction::Redraw;
        }
        let Some(plate) = self.plates.get_mut(deepest) else {
            return ChainAction::Consumed;
        };
        // Right enters the highlighted row's child; Enter and Space activate
        // it. The shared control reports both as opening a submenu for any
        // chevroned row, so a panel row's *activation* — which detaches its
        // window — is told apart here, where the row kinds live.
        let activating = matches!(key, Key::Named(NamedKey::Enter) | Key::Char(' '));
        let current = plate.menu.current();
        let rows = rows_rect(plate, geom);
        let mut damage = Region::new();
        let acted = plate
            .menu
            .on_key(key, rows, geom.scale, geom.theme, &mut damage);
        match acted {
            Some(MenuAction::Activated { index }) => self.choose(deepest, index),
            Some(MenuAction::OpenSubmenu { index })
                if activating && current == Some(index) && self.detachable(deepest, index) =>
            {
                self.choose(deepest, index)
            }
            Some(MenuAction::OpenSubmenu { index }) => self.arrive(deepest, Some(index), geom),
            Some(MenuAction::Dismissed) => self.closing(ChainOutcome::Dismissed),
            None => {
                if damage.is_empty() {
                    ChainAction::Consumed
                } else {
                    ChainAction::Redraw
                }
            }
        }
    }
}

/// The answer a chain that has ended settled on, defaulting closed: an ended
/// chain always owes exactly one answer, and a dismissal is the answer that
/// grants nothing.
fn chain_outcome(chain: Option<&mut OpenChain>) -> ChainOutcome {
    chain
        .and_then(|chain| chain.outcome.take())
        .unwrap_or(ChainOutcome::Dismissed)
}

/// Which surface of the chain a point landed on.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Hit {
    /// The plate at this depth.
    Plate(usize),
    /// Whatever hangs off the deepest plate.
    Attached,
}

/// Build the plate holding the rows filed under `opener` (`None` for the
/// root).
///
/// # Errors
///
/// [`ModelRefused::NoRows`] when that plate would hold nothing.
fn plate_for(
    model: &ChainModel,
    opener: Option<usize>,
    geom: &ChainGeometry<'_>,
) -> Result<Plate, ModelRefused> {
    let rows: Vec<usize> = model
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.parent == opener)
        .map(|(index, _)| index)
        .collect();
    if rows.is_empty() {
        return Err(ModelRefused::NoRows);
    }
    let items: Vec<MenuItem> = rows
        .iter()
        .filter_map(|&index| model.rows.get(index))
        .map(|row| row.item.clone())
        .collect();
    let mut band = TitleBar::plate();
    // A submenu's band states its parent row's label; the root's states the
    // title the chain was opened with. Neither is a new wire field, and both
    // go through the same untrusted-label bounding a window title does.
    band.set_title(&match opener {
        None => model.title.clone(),
        Some(at) => model
            .rows
            .get(at)
            .map_or_else(String::new, |row| row.item.label().to_string()),
    });
    let _ = geom;
    Ok(Plate {
        rows,
        menu: Menu::new(items),
        band,
        rect: Rect::EMPTY,
        pinned: false,
        open_row: None,
    })
}

/// A plate's preferred extent: the band over the rows, never narrower than a
/// band can be drawn.
fn plate_extent(plate: &Plate, geom: &ChainGeometry<'_>) -> (u32, u32) {
    let band_h = TitleBar::band_height(geom.scale, geom.theme);
    let width = plate
        .menu
        .preferred_width(geom.scale, geom.theme)
        .max(TitleBar::min_band_width(
            TitleBarCommands::Empty,
            geom.scale,
            geom.theme,
        ))
        .max(1);
    let height = band_h
        .saturating_add(plate.menu.preferred_height(geom.scale, geom.theme))
        .max(1);
    (width, height)
}

/// The screen rectangle a plate's rows occupy, below its band.
fn rows_rect(plate: &Plate, geom: &ChainGeometry<'_>) -> Rect {
    let band_h = TitleBar::band_height(geom.scale, geom.theme).min(plate.rect.height);
    Rect::new(
        plate.rect.left(),
        plate.rect.top().saturating_add_unsigned(band_h),
        plate.rect.width,
        plate.rect.height.saturating_sub(band_h),
    )
}

/// The screen rectangle of row `index` of `plate`.
fn row_rect(plate: &Plate, index: usize, geom: &ChainGeometry<'_>) -> Option<Rect> {
    plate
        .menu
        .row_rect(index, rows_rect(plate, geom), geom.scale, geom.theme)
}
