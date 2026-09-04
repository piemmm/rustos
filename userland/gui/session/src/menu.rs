//! The desktop's one menu chain: the seat's singleton, and the only thing on
//! the system that renders a menu (`plans/NEW-MENUS.md` §1).
//!
//! A chain is a root plate and the descendants open beneath it. A **plate** is
//! a title band over a column of rows: the shared [`TitleBar`] seating no
//! commands, and the shared [`Menu`]. A **child** is either a submenu — more
//! rows from the same model — or the session's own information panel, which
//! hangs where a submenu hangs and dies with the chain.
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

use tairix_abi::window_ipc::{AppMenuItemId, MenuRefusal};
use tairix_controls::damage::{self, Repaint};
use tairix_controls::{
    plate_rect, ChainChild, ChainModel, FactList, Menu, MenuAction, PlatePlacement, TitleBar,
    TitleBarCommands, TitleBarEvent,
};
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_taskbar::MenuSubject;

use crate::windows::seat_menu_refusal;
use tairix_theme::Theme;
use tairix_wm::{ChromeEpoch, InputEvent, Key, NamedKey};

/// Who asked for the chain, and therefore where its one answer goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChainOwner {
    /// An application's window. The answer is the `MenuClosed` naming
    /// `open_id` that the engine holds it to.
    Window {
        /// The window-channel id of the window the chain is scoped by.
        window_id: u64,
        /// The session-minted id of the open this chain answers.
        open_id: u64,
    },
    /// The desktop's own pinboard backdrop. The answer is resolved in
    /// process against the desktop model.
    Backdrop,
    /// The desktop's own icon bar. The answer is resolved in process against
    /// the bar's own subject, which is what a chosen row of it acts on.
    ///
    /// The subject travels with the address rather than beside it, so a chain
    /// the next open displaces cannot have its answer read against the next
    /// chain's subject.
    Bar(MenuSubject),
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
    /// The chain closed.
    ///
    /// Neither the answer nor the surfaces are here. The answer leaves through
    /// [`MenuChain::take_answers`], so the one delivery point cannot be
    /// bypassed and no path can answer a chain twice; the surfaces go by the
    /// session reconciling against [`MenuChain::surfaces`], which a closed
    /// chain reports empty.
    Closed,
}

/// One surface the chain occupies on screen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChainSurface {
    /// Where it sits, in screen pixels.
    pub rect: Rect,
    /// What it is.
    pub kind: SurfaceKind,
    /// What of it the session has still to paint.
    pub repaint: Repaint,
}

/// Which of the chain's surfaces a rectangle is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SurfaceKind {
    /// The plate at this depth, root first.
    Plate(usize),
    /// The session-drawn information panel.
    Info,
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
    /// What of the plate the session has still to paint.
    repaint: Repaint,
}

/// Logical width of the information panel at the reference density: wide
/// enough for a one-line purpose beside its label.
const INFO_PANEL_WIDTH: u32 = 260;

/// The information panel's width in physical pixels at `scale`.
fn info_panel_width(scale: Scale, _theme: &Theme) -> u32 {
    scale.scale_length(INFO_PANEL_WIDTH).max(1)
}

/// The desktop's own information panel, hanging where a submenu's plate would.
#[derive(Clone, Debug)]
struct InfoPanel {
    /// The facts, read from the owner's signed manifest by the host.
    facts: FactList,
    /// Where it sits.
    rect: Rect,
    /// What of the panel the session has still to paint. It states facts that
    /// never change, so after its first paint it owes nothing.
    repaint: Repaint,
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
    /// The information panel hanging off the deepest plate, if one is open.
    info: Option<InfoPanel>,
    /// Where the root plate opens, in screen pixels.
    placement: PlatePlacement,
    drag: Option<Drag>,
    /// The answer this chain has settled on, once something has ended it.
    outcome: Option<ChainOutcome>,
    /// The screen and output mode the chain was placed against.
    mode: (Rect, ChromeEpoch),
    /// Whether a frame carrying this chain has reached the display, so the
    /// announcement is made once per open. Per-open rather than per-seat
    /// because a fresh chain is a fresh `OpenChain`: nothing has to remember
    /// to clear it.
    shown: bool,
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

    /// Report who owns the open chain if the frame just handed to the display
    /// is the first to carry it.
    ///
    /// Called immediately after a frame reached the display, which is what
    /// makes the claim true — the chain's surfaces are compositor windows, so
    /// a frame composited while it is up carries its plates. Once per open,
    /// and never for a chain that has closed: a chain the session could not
    /// draw is refused rather than announced.
    ///
    /// Takes a reporter rather than returning anything so an idle wake — which
    /// is nearly every wake — costs a bool test.
    pub fn report_newly_shown(&mut self, report: impl FnOnce(&ChainOwner)) {
        let Some(chain) = self.open.as_mut() else {
            return;
        };
        if chain.shown {
            return;
        }
        chain.shown = true;
        report(&chain.owner);
    }

    /// Whether a chain is up. While one is, it holds the seat's pointer and
    /// keyboard.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The owner of the open chain, if one is up.
    ///
    /// Borrowed rather than cloned: an owner carries the bar's subject, which
    /// holds a catalog id, and this is read on the pointer-sample path.
    #[must_use]
    pub fn owner(&self) -> Option<&ChainOwner> {
        self.open.as_ref().map(|chain| &chain.owner)
    }

    /// The window-channel id of the open chain's owning window, for a chain an
    /// application asked for; `None` for one the desktop opened for itself.
    #[must_use]
    pub fn owner_window(&self) -> Option<u64> {
        match self.owner()? {
            ChainOwner::Window { window_id, .. } => Some(*window_id),
            ChainOwner::Backdrop | ChainOwner::Bar(_) => None,
        }
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
        placement: PlatePlacement,
        geom: &ChainGeometry<'_>,
    ) -> Result<(), ModelRefused> {
        let root = plate_for(&model, None, geom)?;
        self.close_open();
        let mut chain = OpenChain {
            owner,
            model,
            plates: alloc::vec![root],
            info: None,
            placement,
            drag: None,
            outcome: None,
            mode: geom.mode(),
            shown: false,
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
                repaint: plate.repaint.clone(),
            })
            .collect();
        if let Some(panel) = chain.info.as_ref() {
            out.push(ChainSurface {
                rect: panel.rect,
                kind: SurfaceKind::Info,
                repaint: panel.repaint.clone(),
            });
        }
        out
    }

    /// Record that the surface `kind` now carries what the chain last asked
    /// for, so what it owes from here is only what changes after this.
    ///
    /// Called per surface the session actually painted. A surface the heap
    /// refused keeps what it owes and is painted on the next pass instead, so
    /// a refusal can never leave stale pixels reported as current.
    pub fn presented(&mut self, kind: SurfaceKind) {
        let Some(chain) = self.open.as_mut() else {
            return;
        };
        let owed = match kind {
            SurfaceKind::Plate(depth) => {
                chain.plates.get_mut(depth).map(|plate| &mut plate.repaint)
            }
            SurfaceKind::Info => chain.info.as_mut().map(|panel| &mut panel.repaint),
        };
        if let Some(owed) = owed {
            *owed = Repaint::clean();
        }
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

    /// Paint the surface `kind` into `surface`, whose extent is that
    /// surface's own rectangle.
    ///
    /// The one entry point for a chain's pixels, so the session supplies a
    /// surface and never a recipe. A caller repainting part of a surface
    /// clips `surface` to the rectangles it means to write and calls this:
    /// every pixel inside the clip is re-derived exactly as a whole paint
    /// would have laid it, so a partial repaint and a whole one cannot
    /// disagree.
    pub fn render_surface(
        &self,
        kind: SurfaceKind,
        surface: &mut tairix_raster::Surface,
        geom: &ChainGeometry<'_>,
    ) {
        match kind {
            SurfaceKind::Plate(depth) => self.render_plate(depth, surface, geom),
            SurfaceKind::Info => self.render_info(surface, geom),
        }
    }

    /// Paint the plate at `depth`.
    ///
    /// The band and the rows are the two shared controls over one shared plate
    /// ground; nothing here is a second recipe for any of the three, and the
    /// rows take that ground rather than laying one of their own.
    fn render_plate(
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
        // The band lays no ground of its own, so the plate's is laid first.
        lay_plate(surface, (local.width, local.height), geom);
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

    /// Paint the information panel: the attested facts on the same floating
    /// ground every plate stands on.
    fn render_info(&self, surface: &mut tairix_raster::Surface, geom: &ChainGeometry<'_>) {
        let Some(panel) = self.open.as_ref().and_then(|chain| chain.info.as_ref()) else {
            return;
        };
        let local = Rect::new(0, 0, panel.rect.width, panel.rect.height);
        lay_plate(surface, (local.width, local.height), geom);
        panel.facts.render(surface, local, geom.scale, geom.theme);
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
        owned_by(self.owner(), window_id) && self.close_open()
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

/// Whether `owner` is the window-channel window `window_id`.
fn owned_by(owner: Option<&ChainOwner>, window_id: u64) -> bool {
    matches!(owner, Some(ChainOwner::Window { window_id: id, .. }) if *id == window_id)
}

/// Why a model cannot be shown as a chain.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModelRefused {
    /// Its root plate has no rows: a plate the user cannot leave by choosing
    /// anything is not a menu.
    NoRows,
}

/// Why one of the desktop's own menus did not come up.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DesktopMenuRefused {
    /// The seat cannot show a menu at all: there is no output to place on, or
    /// a surface a menu may not displace holds it.
    Seat(MenuRefusal),
    /// The model itself cannot be shown.
    Model(ModelRefused),
}

/// Open one of the desktop's own menus as the seat's one chain.
///
/// The backdrop's and the icon bar's menus are clients of the menu service
/// exactly as an application's is; the only difference is that their models are
/// built in process, so their rows may state things an application
/// structurally cannot. Both arrive through here, so a desktop press cannot
/// take the grab from the lock screen or the trusted picker by arriving from a
/// direction that skipped the seat rule — the same rule an application's
/// `OpenMenu` resolves through.
///
/// # Errors
///
/// [`DesktopMenuRefused`] when the seat will not show a menu or the model
/// cannot be drawn as one; the chain that was up is left exactly as it was.
pub fn open_desktop_menu(
    chain: &mut MenuChain,
    owner: ChainOwner,
    model: ChainModel,
    placement: PlatePlacement,
    seat_held: bool,
    geom: &ChainGeometry<'_>,
) -> Result<(), DesktopMenuRefused> {
    if let Some(reason) = seat_menu_refusal(geom.screen, seat_held) {
        return Err(DesktopMenuRefused::Seat(reason));
    }
    chain
        .open(owner, model, placement, geom)
        .map_err(DesktopMenuRefused::Model)
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
                None => plate_rect(width, height, self.placement, geom.screen),
                Some(parent) => {
                    let against = self.child_anchor(parent, geom);
                    plate_rect(
                        width,
                        height,
                        PlatePlacement::adjacent(against),
                        geom.screen,
                    )
                }
            };
            self.plates[depth].rect = rect;
        }
        if let Some((width, height)) = self
            .info
            .as_ref()
            .map(|panel| (panel.rect.width, panel.rect.height))
        {
            let rect = self.hang(width, height, geom);
            if let Some(panel) = self.info.as_mut() {
                panel.rect = rect;
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

    /// Where a `width` × `height` information panel hangs: exactly where the
    /// child plate of the same row would, so a panel and a submenu are placed
    /// by one rule.
    fn hang(&self, width: u32, height: u32, geom: &ChainGeometry<'_>) -> Rect {
        let deepest = self.plates.len().saturating_sub(1);
        let against = self.child_anchor(deepest, geom);
        plate_rect(
            width,
            height,
            PlatePlacement::adjacent(against),
            geom.screen,
        )
    }

    /// Close every plate deeper than `depth`, and anything hanging off them.
    fn truncate_to(&mut self, depth: usize) {
        self.plates.truncate(depth + 1);
        self.info = None;
        if let Some(plate) = self.plates.get_mut(depth) {
            plate.open_row = None;
        }
    }

    /// The chain's surface the pointer is over, deepest first — a child is
    /// drawn over its parent, so the deepest match owns the point.
    fn surface_at(&self, pointer: Point) -> Option<Hit> {
        if self.info.as_ref().is_some_and(|p| p.rect.contains(pointer)) {
            return Some(Hit::Info);
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
            // The information panel states facts and offers no action, so a
            // pointer over it is claimed and does nothing.
            (_, Some(Hit::Info) | None) => ChainAction::Consumed,
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
        let mut damage = damage::sink();
        if band.contains(pointer) {
            let acted = plate
                .band
                .on_pointer(event, band, geom.scale, geom.theme, &mut damage);
            self.mark_plate(depth, &damage);
            return self.band_event(depth, acted, pointer, geom);
        }
        let rows = rows_rect(plate, geom);
        let acted = plate
            .menu
            .on_pointer(event, rows, geom.scale, geom.theme, &mut damage);
        let over = plate.menu.row_at(rows, geom.scale, geom.theme, pointer);
        self.mark_plate(depth, &damage);
        match acted {
            Some(MenuAction::Activated { index }) => self.choose(depth, index),
            // A click on a row that opens a child keeps its child open
            // rather than acting: the child is what the row is for.
            Some(MenuAction::OpenSubmenu { index }) => self.arrive(depth, Some(index), geom),
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

    /// Fold a control's reported damage — screen rectangles, because that is
    /// the space a plate's rows are laid out and hit-tested in — into what the
    /// plate at `depth` owes, in that plate's own local pixels.
    fn mark_plate(&mut self, depth: usize, damage: &Region) {
        if damage.is_empty() {
            return;
        }
        let Some(plate) = self.plates.get_mut(depth) else {
            return;
        };
        let origin = plate.rect.origin;
        for rect in damage.rects() {
            plate.repaint.add(Rect::new(
                rect.left().saturating_sub(origin.x),
                rect.top().saturating_sub(origin.y),
                rect.width,
                rect.height,
            ));
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
        let Some(entry) = self.model.rows().get(model_row) else {
            return ChainAction::Consumed;
        };
        // A disabled row opens nothing, and leaves whatever was open alone: a
        // pointer resting on a row it cannot use has not chosen to close
        // anything.
        if !entry.drawn().state().is_actionable() {
            return ChainAction::Consumed;
        }
        let child = entry.child().clone();
        // Whether anything was actually taken down, which is what tells a
        // no-child arrival from one that closed a plate.
        let closed = self.plates.len() > depth + 1 || self.info.is_some();
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
                self.info = Some(InfoPanel {
                    facts,
                    rect: self.hang(width, height, geom),
                    repaint: Repaint::Whole,
                });
                ChainAction::Redraw
            }
        }
    }

    /// A row of the plate at `depth` was chosen.
    fn choose(&mut self, depth: usize, index: usize) -> ChainAction {
        let Some(&model_row) = self.plates.get(depth).and_then(|p| p.rows.get(index)) else {
            return ChainAction::Consumed;
        };
        let Some(entry) = self.model.rows().get(model_row) else {
            return ChainAction::Consumed;
        };
        match entry.id() {
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
        // always gets the user out.
        if key == Key::Named(NamedKey::Escape) {
            if self.info.is_some() {
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
            if deepest == 0 && self.info.is_none() {
                return ChainAction::Consumed;
            }
            let back = if self.info.is_some() {
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
        let rows = rows_rect(plate, geom);
        let mut damage = damage::sink();
        let acted = plate
            .menu
            .on_key(key, rows, geom.scale, geom.theme, &mut damage);
        self.mark_plate(deepest, &damage);
        match acted {
            Some(MenuAction::Activated { index }) => self.choose(deepest, index),
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
    /// The information panel hanging off the deepest plate.
    Info,
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
        .rows()
        .iter()
        .enumerate()
        .filter(|(_, row)| row.parent() == opener)
        .map(|(index, _)| index)
        .collect();
    if rows.is_empty() {
        return Err(ModelRefused::NoRows);
    }
    let items: Vec<tairix_controls::MenuItem> = rows
        .iter()
        .filter_map(|&index| model.rows().get(index))
        .map(|row| row.drawn().clone())
        .collect();
    let mut band = TitleBar::plate();
    // A submenu's band states its parent row's label; the root's states the
    // title the chain was opened with. Neither is a new wire field, and both
    // go through the same untrusted-label bounding a window title does.
    band.set_title(&match opener {
        None => model.title().to_string(),
        Some(at) => model
            .rows()
            .get(at)
            .map_or_else(String::new, |row| row.drawn().label().to_string()),
    });
    let _ = geom;
    Ok(Plate {
        rows,
        menu: Menu::new(items),
        band,
        rect: Rect::EMPTY,
        pinned: false,
        open_row: None,
        repaint: Repaint::Whole,
    })
}

/// Lay the shared floating-plate ground over a `size` surface: the recipe
/// every chain surface stands on, plate and information panel alike.
///
/// The rectangle it lands on has been cleared by
/// [`tairix_controls::damage::paint_parts`], which is what
/// lets a repaint of part of a retained plate land the same pixels: a
/// translucent plate's arc pixels are blended by coverage, so laying the ground
/// over what one already held would tint the corner.
fn lay_plate(surface: &mut tairix_raster::Surface, size: (u32, u32), geom: &ChainGeometry<'_>) {
    let (width, height) = size;
    let _ = tairix_controls::paint_surface_plate(
        surface,
        (0, 0, width, height),
        (
            geom.scale
                .scale_length(geom.theme.metrics().popup_corner_radius),
            tairix_controls::plate_border(geom.theme, geom.scale),
        ),
        geom.theme,
        (
            geom.theme.palette().surface_raised,
            tairix_controls::ChromeLayer::Ground,
        ),
    );
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
