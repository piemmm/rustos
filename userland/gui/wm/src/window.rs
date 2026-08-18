//! Windows: a placed surface with compositing attributes.

use tairix_controls::{
    FrameInsets, FrameRim, TitleBarEvent, WindowActivationState, WindowFrame, WindowSizeState,
};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key};
use tairix_reclaim::CachedBytes;
use tairix_theme::{CursorKind, Theme};

use crate::chrome::WindowChrome;
use crate::color::{div255, DitherRow, Pixel};
use crate::corner::Corners;
use crate::geometry::{Point, Rect, Region, Scale};
use crate::surface::{self, blend_run, Surface};
use crate::viewport::RootViewport;

/// An opaque, compositor-minted window identifier.
///
/// Clients never construct a [`WindowId`]; the compositor returns one
/// from [`Compositor::add_window`] and the client names its window by
/// that token thereafter.
///
/// [`Compositor::add_window`]: crate::Compositor::add_window
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct WindowId(pub(crate) u64);

/// A window: a [`Surface`] placed at a screen [`Point`] with a
/// per-window opacity, corner style, and pointer-cursor hint.
///
/// The content pixels of an *app-presented* window
/// ([`is_app_presented`](Self::is_app_presented)) are **releasable**: the
/// compositor may hand them back to the machine under memory pressure and
/// ask the owning app to present again
/// ([`Compositor::release_content_under_pressure`]). A released window
/// keeps everything that makes it a window — its client size, origin,
/// furniture, cursor hint, viewport, focus and size state — and simply
/// composites as transparent until the next present restores its pixels.
///
/// [`Compositor::release_content_under_pressure`]: crate::Compositor::release_content_under_pressure
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    id: WindowId,
    origin: Point,
    /// The client content pixels, or `None` while released. Only the
    /// pixels are optional: `client_size` below keeps the geometry the
    /// rest of the window is laid out from, so nothing else about the
    /// window depends on whether the buffer is currently held.
    ///
    /// The buffer's own extent is the geometry the *client* last presented,
    /// which is not always `client_size`: the window manager resizes the
    /// frame it draws as a resize-grab moves, while the client learns its
    /// new size and re-renders afterwards. Whichever is larger, only the
    /// pixels inside both are drawn ([`Self::client_row`]), so the two
    /// disagreeing is an ordinary transient, never a fault.
    content: Option<Surface>,
    /// The client content extent in physical pixels the window is laid out
    /// from: what the frame reserves for the client and what the client is
    /// told to render at. Retained across a release, so a window with no
    /// pixels still has a size, bounds, and furniture band.
    client_size: (u32, u32),
    opacity: u8,
    /// Backdrop-blur radius in *logical* pixels, `0` for no blur. Logical,
    /// because it is a desktop length the app asks for at the reference
    /// density; the compositor resolves it to physical pixels through the
    /// output's scale when it blurs.
    blur_radius: u16,
    corners: Corners,
    visible: bool,
    cursor: CursorKind,
    viewport: Option<RootViewport>,
    frame: Option<WindowFrame>,
    band: Option<FrameInsets>,
    /// The frame's own rim at the active scale and theme, resolved with
    /// `band`: the shape the window is cut to, and the plate its client is
    /// clipped to. `None` for an undecorated window.
    rim: Option<FrameRim>,
    /// The owning application's identity artwork, rasterised at the title
    /// bar's slot side for the active scale. One small square per decorated
    /// window, re-derivable from the owner's bundle at any time, so it is
    /// dropped and re-resolved on a scale or theme change rather than kept in
    /// every size. `None` means the title bar falls back to the built-in
    /// glyph for its identity class, or draws no slot at all when it has no
    /// identity.
    identity_artwork: Option<Surface>,
    /// Whether the window is restored or maximized. Meaningful only for a
    /// decorated, resizable window; a plain window is always `Restored`.
    size_state: WindowSizeState,
    /// The outer rectangle to return to when a maximized window is
    /// restored — captured at the moment it was maximized, so a
    /// maximize/restore round-trip lands back exactly where it started.
    restore_outer: Option<Rect>,
    /// Whether some client presents this window's pixels and can be asked
    /// to present them again. Off until the embedder says otherwise, so a
    /// window nobody can repaint is never released.
    app_presented: bool,
    /// The window this one is a *transient* of — the surface it belongs to
    /// and is stacked immediately above (a menu's or a sheet's owner) — or
    /// `None` for a top-level window that stands on its own.
    ///
    /// Stacking reads it: a restack moves an owner and its transients
    /// together, which is what keeps a menu on its own window and stops
    /// anything landing between the two.
    parent: Option<WindowId>,
    /// The smallest client extent the owning application declared it can lay
    /// out at, in physical pixels; `(0, 0)` for an application that declared
    /// none and is content at any size.
    ///
    /// It bounds a resize, never the size the window was created at: an
    /// application asking for a small window is choosing that size, while a
    /// *user* dragging an edge is one the application cannot refuse without
    /// fighting the drag.
    min_client: (u32, u32),
}

impl Window {
    /// Build a fully-opaque, square-cornered, visible window from a
    /// surface placed at `origin`. The pointer-cursor hint defaults to the
    /// plain [`CursorKind::Arrow`]; a window whose content wants a
    /// different pointer (an editor's text I-beam, a control's hand) sets
    /// it through [`Compositor::set_window_cursor`].
    ///
    /// [`Compositor::set_window_cursor`]: crate::Compositor::set_window_cursor
    pub(crate) fn new(id: WindowId, origin: Point, surface: Surface) -> Self {
        Self {
            id,
            origin,
            client_size: (surface.width(), surface.height()),
            content: Some(surface),
            opacity: 255,
            blur_radius: 0,
            corners: Corners::Square,
            visible: true,
            cursor: CursorKind::Arrow,
            viewport: None,
            frame: None,
            band: None,
            rim: None,
            identity_artwork: None,
            size_state: WindowSizeState::Restored,
            restore_outer: None,
            app_presented: false,
            parent: None,
            min_client: (0, 0),
        }
    }

    /// This window's identifier.
    #[must_use]
    pub const fn id(&self) -> WindowId {
        self.id
    }

    /// Top-left screen position.
    #[must_use]
    pub const fn origin(&self) -> Point {
        self.origin
    }

    /// The window this one is a transient of, or `None` when it stands on
    /// its own.
    #[must_use]
    pub const fn parent(&self) -> Option<WindowId> {
        self.parent
    }

    /// Make this window a transient of `parent`, or a top-level window again
    /// when `parent` is `None`.
    ///
    /// Only the compositor calls this, and only where it restacks the family
    /// in the same breath: the link and the stacking it implies are
    /// established together, so no frame can see one without the other.
    pub(crate) fn set_parent(&mut self, parent: Option<WindowId>) {
        self.parent = parent;
    }

    /// Per-window opacity (`255` opaque).
    #[must_use]
    pub const fn opacity(&self) -> u8 {
        self.opacity
    }

    /// Whether this window's own pixels leave what is composed beneath its
    /// rectangle showing through it as a *field*, so the compositor is worth
    /// retaining that backdrop for.
    ///
    /// Two things do it: a backdrop blur, which reads the backdrop to blur it,
    /// and a whole-window opacity below full, which admits it everywhere. Both
    /// make every frame that touches the window recompose the entire stack
    /// beneath it, which is what retaining pays for.
    ///
    /// Deliberately *not* here: an antialiased corner, whose backdrop is a few
    /// pixels of arc, and a client that paints alpha into its own content,
    /// which cannot be known without reading every pixel of it. Both still
    /// composite correctly — they simply blend the layers below rather than
    /// copying a retained picture of them, which is what a window that reads
    /// only a sliver of its backdrop should do.
    #[must_use]
    pub const fn reads_backdrop(&self) -> bool {
        self.blur_radius > 0 || self.opacity < u8::MAX
    }

    /// Backdrop-blur radius in *logical* pixels (`0` for no blur): how far
    /// the compositor spreads the already-composited content behind this
    /// window's rectangle before blending the window's own pixels over it.
    #[must_use]
    pub const fn blur_radius(&self) -> u16 {
        self.blur_radius
    }

    /// Corner style.
    #[must_use]
    pub const fn corners(&self) -> Corners {
        self.corners
    }

    /// The pointer-cursor hint for this window's content: the
    /// [`CursorKind`] the compositor shows while the pointer rests over
    /// the window and no higher-priority interaction (such as a window
    /// move-grab) is in progress.
    #[must_use]
    pub const fn cursor_hint(&self) -> CursorKind {
        self.cursor
    }

    /// `true` if the window participates in composition.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    /// Whether the window is restored or maximized.
    #[must_use]
    pub const fn size_state(&self) -> WindowSizeState {
        self.size_state
    }

    /// Adopt the owning application's declared minimum client extent.
    pub(crate) fn set_min_client_size(&mut self, min_w: u32, min_h: u32) {
        self.min_client = (min_w, min_h);
    }

    /// The screen rectangle of this window's move surface: the span of title
    /// band between its two command clusters. `None` for an undecorated
    /// window, which has no title bar to be dragged by.
    pub(crate) fn drag_surface(&self, scale: Scale, theme: &Theme) -> Option<Rect> {
        let frame = self.frame.as_ref()?;
        let band = frame.layout(self.bounds(), scale, theme).title_bar;
        Some(frame.title_bar().layout(band, scale, theme).drag)
    }

    /// The smallest outer rectangle this window may be resized to, in
    /// physical pixels: the greater of its own furniture's floor and the
    /// owning application's declared minimum client extent grown by the
    /// furniture band.
    ///
    /// Both floors are real. The furniture's is what keeps the title bar's
    /// commands seated with a drag surface between them, so it holds even
    /// for an application that declared nothing; the application's is what
    /// keeps its content laying out, so it holds even where the furniture
    /// would fit in less. Never zero on either axis.
    pub(crate) fn min_outer_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let (band_w, band_h) = match self.band {
            Some(insets) => (
                insets.left.saturating_add(insets.right),
                insets.top.saturating_add(insets.bottom),
            ),
            None => (0, 0),
        };
        let (floor_w, floor_h) = self
            .frame
            .as_ref()
            .map_or((1, 1), |frame| frame.min_outer_size(scale, theme));
        (
            floor_w.max(self.min_client.0.saturating_add(band_w)).max(1),
            floor_h.max(self.min_client.1.saturating_add(band_h)).max(1),
        )
    }

    /// This window's window-manager-owned root-viewport scrollbars, if any.
    ///
    /// A window with a root viewport has the window manager compose its
    /// scrollbars as furniture around the client; a window without one is a
    /// plain client surface.
    #[must_use]
    pub const fn viewport(&self) -> Option<&RootViewport> {
        self.viewport.as_ref()
    }

    /// Borrow the window's content pixels, or `None` while they are
    /// released. A released window still has a size
    /// ([`client_size`](Self::client_size)) and everything else that makes
    /// it a window; only the buffer is gone.
    #[must_use]
    pub const fn content(&self) -> Option<&Surface> {
        self.content.as_ref()
    }

    /// The client content extent in physical pixels.
    ///
    /// This is retained independently of the pixels, so it answers the
    /// same value whether the content is held or released — every layout
    /// question (bounds, client rectangle, furniture bands) reads it here
    /// rather than measuring a buffer that may not exist.
    #[must_use]
    pub const fn client_size(&self) -> (u32, u32) {
        self.client_size
    }

    /// Whether the window is currently holding its content pixels.
    #[must_use]
    pub const fn has_content(&self) -> bool {
        self.content.is_some()
    }

    /// Whether a client presents this window's pixels and can therefore be
    /// asked to present them again.
    ///
    /// Only such a window's content may be released under memory pressure:
    /// releasing pixels nobody can redraw would blank the window with no
    /// way back, which is the same reason the focused window is spared.
    /// The embedder declares it
    /// ([`Compositor::set_app_presented`]); a window it paints itself —
    /// the taskbar, a session dialog, the lock screen — leaves it off.
    ///
    /// [`Compositor::set_app_presented`]: crate::Compositor::set_app_presented
    #[must_use]
    pub const fn is_app_presented(&self) -> bool {
        self.app_presented
    }

    /// Declare whether a client presents this window's pixels, reporting
    /// whether the answer changed.
    pub(crate) fn set_app_presented(&mut self, app_presented: bool) -> bool {
        let changed = self.app_presented != app_presented;
        self.app_presented = app_presented;
        changed
    }

    /// Heap bytes this window's retained content pixels occupy; zero while
    /// released.
    pub(crate) fn content_bytes(&self) -> usize {
        self.content.as_ref().map_or(0, CachedBytes::payload_bytes)
    }

    /// Take the content pixels out of the window, overwritten, and hand
    /// the spent buffer to the caller to drop.
    ///
    /// The overwrite happens here, before ownership leaves the window: a
    /// window's content is whatever the user was looking at, so its heap
    /// must not become reusable still carrying it. Handing the buffer back
    /// rather than dropping it in place is what lets the wipe be observed
    /// — otherwise the only witness to it is freed memory.
    pub(crate) fn take_content_wiped(&mut self) -> Option<Surface> {
        let mut surface = self.content.take()?;
        surface.wipe();
        Some(surface)
    }

    /// Give the content pixels back to the machine, returning the bytes
    /// released (zero when there were none). The buffer is overwritten
    /// before it is dropped.
    pub(crate) fn release_content(&mut self) -> usize {
        self.take_content_wiped()
            .map_or(0, |surface| surface.payload_bytes())
    }

    /// Borrow the window's content buffer to convert a client present of a
    /// `width` × `height` frame into it, establishing the buffer whenever
    /// the one held does not describe that frame, and reporting whether it
    /// had to be.
    ///
    /// **The presented frame is what sizes the buffer.** The pixels are the
    /// client's, so their extent is the client's to state, and the window
    /// manager's own resize of the frame it draws never reshapes them. A
    /// buffer therefore has to be established here in two cases: it was
    /// released under memory pressure, or the client has re-rendered at a
    /// new size. An established buffer starts fully transparent and carries
    /// nothing over, so the window is correct once the client's present has
    /// been converted into it — which is why a whole-window present is what
    /// both a redraw request and a resize ask the client for. The caller
    /// ([`Compositor::present_window_content`]) repaints the whole client
    /// area when the answer is `true`, and only the rectangle the
    /// conversion reported otherwise.
    ///
    /// Returns `None` only when a buffer of that size cannot be allocated,
    /// leaving the retained pixels exactly as they were: a present under
    /// memory exhaustion is refused whole rather than half-applied, and
    /// never at the cost of blanking a window that still has content.
    ///
    /// [`Compositor::present_window_content`]: crate::Compositor::present_window_content
    pub(crate) fn content_for_present(
        &mut self,
        width: u32,
        height: u32,
    ) -> Option<(&mut Surface, bool)> {
        let held = self
            .content
            .as_ref()
            .is_some_and(|content| content.width() == width && content.height() == height);
        if held {
            return self.content.as_mut().map(|content| (content, false));
        }
        // Allocated before the held buffer is given up, so a refusal leaves
        // the window showing what it was showing.
        let fresh = Surface::new(width, height)?;
        self.take_content_wiped();
        self.content = Some(fresh);
        self.content.as_mut().map(|content| (content, true))
    }

    /// The window's screen rectangle.
    ///
    /// For a plain window this is the origin plus the content surface size. For
    /// a *decorated* window it is the **outer** rectangle: the content surface
    /// grown by the frame band ([`FrameInsets`]) the window manager reserves
    /// for the title bar, borders, and resize edges, so the decoration lives
    /// around the client rather than over it.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        let (w, h) = self.outer_size();
        Rect::new(self.origin.x, self.origin.y, w, h)
    }

    /// The window's outer size in physical pixels: the content surface grown by
    /// the reserved frame band when decorated, else the bare surface size.
    fn outer_size(&self) -> (u32, u32) {
        let (client_w, client_h) = self.client_size;
        match self.band {
            Some(insets) => (
                client_w
                    .saturating_add(insets.left)
                    .saturating_add(insets.right),
                client_h
                    .saturating_add(insets.top)
                    .saturating_add(insets.bottom),
            ),
            None => (client_w, client_h),
        }
    }

    /// The screen rectangle the application content occupies: the inset client
    /// viewport for a decorated window, or the whole bounds for a plain one.
    ///
    /// The content surface is presented here and never overlaps the furniture
    /// band, so a decorated window's client never receives frame input.
    #[must_use]
    pub fn client_rect(&self) -> Rect {
        match self.band {
            Some(insets) => Rect::new(
                self.origin.x.saturating_add_unsigned(insets.left),
                self.origin.y.saturating_add_unsigned(insets.top),
                self.client_size.0,
                self.client_size.1,
            ),
            None => self.bounds(),
        }
    }

    /// This window's window-manager-owned decoration frame, if it is decorated.
    #[must_use]
    pub fn frame(&self) -> Option<&WindowFrame> {
        self.frame.as_ref()
    }

    /// This window's contribution to screen row `y`, resolved once for the
    /// whole row, or `None` when the row draws nothing at all (the window
    /// is hidden, the row falls outside its outer bounds, or the furniture
    /// gutter clips the row away).
    ///
    /// `chrome` is this window's rendered furniture, which the compositor
    /// holds in its shared reclaimable cache rather than the window holding
    /// its own copy; `None` draws the content alone, exactly as an
    /// undecorated window does.
    ///
    /// Which layer a column comes from, where its source row sits in the
    /// buffer, and what alpha applies are all fixed for a row. Answering
    /// them here leaves the per-column path a single slice index, instead
    /// of re-deriving the coordinate conversion, the layer choice, and two
    /// bounds checks for every pixel of a repainted window.
    #[must_use]
    pub(crate) fn row<'a>(
        &'a self,
        y: i32,
        chrome: Option<&'a WindowChrome>,
    ) -> Option<WindowRow<'a>> {
        if !self.visible {
            return None;
        }
        let ly = u32::try_from(i64::from(y) - i64::from(self.origin.y)).ok()?;
        if ly >= self.outer_size().1 {
            return None;
        }
        let (inset_x, inset_y) = match self.band {
            Some(insets) => (insets.left, insets.top),
            None => (0, 0),
        };
        let content_row = ly
            .checked_sub(inset_y)
            .filter(|sy| *sy < self.client_size.1);
        let (content, client_cols) = match content_row {
            Some(sy) => (self.client_row(sy), self.client_size.0),
            None => (&[][..], 0),
        };
        let decoration = self.decoration_spans(ly, chrome);
        if content.is_empty() && decoration[0].pixels.is_empty() && decoration[1].pixels.is_empty()
        {
            return None;
        }
        Some(WindowRow {
            content,
            decoration,
            client_x: self.origin.x.saturating_add_unsigned(inset_x),
            client_cols,
            opacity: self.opacity,
            cut: content_row.and_then(|sy| self.row_cut(sy)),
        })
    }

    /// This window's decoration spans for outer-local row `ly` (`0` at the
    /// outer top edge): the top strip's row while `ly` is in the top band,
    /// the bottom strip's row while it is in the bottom band, or — for a row
    /// between them — the left strip's row at the outer left edge together
    /// with the right strip's row at the client's right edge. Both spans are
    /// empty for an undecorated window, and a row is never in more than one of
    /// these cases, so at most one side of the pair is ever non-empty outside
    /// the middle case.
    ///
    /// The bands are the ones [`local_furniture_bands`](Self::local_furniture_bands)
    /// rendered, so a corner arc's rows take the full-width top or bottom strip
    /// even where the client also has pixels on them: what is drawn there is
    /// the rim's curve, and the client is clipped out of it
    /// ([`client_cut`](Self::client_cut)).
    fn decoration_spans<'a>(
        &'a self,
        ly: u32,
        chrome: Option<&'a WindowChrome>,
    ) -> [DecorationSpan<'a>; 2] {
        let (Some(chrome), Some(_)) = (chrome, self.band) else {
            return [DecorationSpan::EMPTY; 2];
        };
        let (top_depth, bottom_depth) = self.corner_depths();
        if ly < top_depth {
            return [
                DecorationSpan::new(chrome.top_row(ly), self.origin.x),
                DecorationSpan::EMPTY,
            ];
        }
        let (_, oh) = self.outer_size();
        let bottom_start = oh.saturating_sub(bottom_depth);
        if ly >= bottom_start {
            return [
                DecorationSpan::new(chrome.bottom_row(ly - bottom_start), self.origin.x),
                DecorationSpan::EMPTY,
            ];
        }
        let side_row = ly - top_depth;
        let right_x = self.client_rect().right();
        [
            DecorationSpan::new(chrome.left_row(side_row), self.origin.x),
            DecorationSpan::new(chrome.right_row(side_row), right_x),
        ]
    }

    /// The drawable client pixels of content row `sy`: the content surface
    /// row truncated where the furniture gutter clips it, and empty when
    /// the gutter clips the whole row away — or when the content has been
    /// released, which draws nothing and lets the desktop show through.
    fn client_row(&self, sy: u32) -> &[Pixel] {
        let (cols, rows) = self.client_extent();
        if sy >= rows {
            return &[];
        }
        let Some(surface) = self.content.as_ref() else {
            return &[];
        };
        let row = surface::row(surface, sy);
        let cols = usize::try_from(cols).unwrap_or(row.len());
        row.get(..cols.min(row.len())).unwrap_or(&[])
    }

    /// The content columns and rows the furniture gutter leaves drawable:
    /// the whole surface unless a root viewport reserves a gutter for its
    /// scrollbars, whose track is furniture and never shows client pixels.
    fn client_extent(&self) -> (u32, u32) {
        let (client_w, client_h) = self.client_size;
        let Some(viewport) = self.viewport else {
            return (client_w, client_h);
        };
        let local = Rect::new(0, 0, client_w, client_h);
        let client = viewport.layout(local).client;
        (client.width, client.height)
    }

    /// The rounded shape this window's outer rectangle is cut to, or `None`
    /// where every pixel of it is fully covered.
    ///
    /// A decorated window takes its frame's rim radius and an undecorated one
    /// its own corner selection; either way the extent is its outer
    /// [`bounds`](Self::bounds), so a caller weights by it in window-local
    /// coordinates without knowing which kind it holds.
    ///
    /// This is the single definition of the window's *silhouette*: the
    /// decoration bakes it into its own pixels as partial alpha, and any
    /// effect confined to the window's rectangle (the compositor's frosted
    /// backdrop) weights itself by it, so a frosted corner cannot square off
    /// what the rim curves around.
    pub(crate) fn shape(&self) -> Option<WindowShape> {
        let (ow, oh) = self.outer_size();
        let corners = match self.rim {
            Some(rim) => Corners::from_radius(rim.radius),
            None => self.corners,
        };
        match corners {
            Corners::Rounded { .. } => Some(WindowShape {
                corners,
                width: ow,
                height: oh,
            }),
            Corners::Square => None,
        }
    }

    /// The shape this window's *client* pixels are clipped to, or `None` where
    /// none of them is clipped.
    ///
    /// An undecorated window's client is the window, so the shape is its own
    /// [`silhouette`](Self::shape) and its coverage anti-aliases the edge the
    /// client itself presents. A decorated window's client is clipped to the
    /// *plate* the frame fills inside its rim: content that reached the rim
    /// would draw over the very curve the rim traces, and content past it
    /// would square off the window's corner altogether.
    fn client_cut(&self) -> Option<ClientCut> {
        let shape = self.shape()?;
        let Some(rim) = self.rim else {
            return Some(ClientCut {
                shape,
                offset: (0, 0),
            });
        };
        let (inset, radius) = rim.plate();
        let (ow, oh) = self.outer_size();
        let insets = self.band?;
        Some(ClientCut {
            shape: WindowShape {
                corners: Corners::from_radius(radius),
                width: ow.saturating_sub(inset.saturating_mul(2)),
                height: oh.saturating_sub(inset.saturating_mul(2)),
            },
            offset: (
                insets.left.saturating_sub(inset),
                insets.top.saturating_sub(inset),
            ),
        })
    }

    /// The [`client_cut`](Self::client_cut) across content row `sy`, or `None`
    /// where every client pixel of that row is fully inside it.
    ///
    /// Resolved once for a whole row, so a column costs a coverage lookup
    /// rather than a fresh shape decision — and a row no arc reaches keeps the
    /// unclipped path, which is what lets an opaque run be copied
    /// ([`WindowRow::opaque_run`]) everywhere but the corners.
    fn row_cut(&self, sy: u32) -> Option<RowCut> {
        let cut = self.client_cut()?;
        let (lx0, ly) = (cut.offset.0, sy.saturating_add(cut.offset.1));
        cut.shape.clips_row(ly).then_some(RowCut {
            shape: cut.shape,
            ly,
            lx0,
        })
    }

    /// The composited contribution of this window at *window-local*
    /// `(lx, ly)` (origin the outer top-left): the source pixel scaled by the
    /// combined opacity and rounded-corner coverage, or `None` outside the
    /// content, in the reserved frame band, or when the window is hidden.
    ///
    /// This is [`Self::row`] addressed in the window's own coordinate
    /// space — the same single definition of what the window draws where —
    /// which the hardware-layer present path
    /// (`Compositor::present_accelerated`) uses to bake a window into a
    /// premultiplied layer. `chrome` is this window's rendered furniture,
    /// resolved by the caller exactly as for [`Self::row`].
    ///
    /// The ordered dither is read at the pixel's **screen** position, not the
    /// layer's, so a window baked into a layer holds exactly the pixels the
    /// software composite would have written at the same place.
    pub(crate) fn sample_local(
        &self,
        lx: u32,
        ly: u32,
        chrome: Option<&WindowChrome>,
    ) -> Option<Pixel> {
        let x = self.origin.x.checked_add(i32::try_from(lx).ok()?)?;
        let y = self.origin.y.checked_add(i32::try_from(ly).ok()?)?;
        self.row(y, chrome)?
            .sample(x, DitherRow::at(y.cast_unsigned()).bias(x.cast_unsigned()))
    }

    /// Move the window to `origin`, returning whether it actually changed
    /// (`false` when it was already there, so the caller marks no damage
    /// for a no-op move).
    pub(crate) fn set_origin(&mut self, origin: Point) -> bool {
        if origin == self.origin {
            return false;
        }
        self.origin = origin;
        true
    }

    /// Set the window's opacity, returning whether it actually changed.
    pub(crate) fn set_opacity(&mut self, opacity: u8) -> bool {
        if opacity == self.opacity {
            return false;
        }
        self.opacity = opacity;
        true
    }

    /// Set the window's backdrop-blur radius in logical pixels (`0`
    /// disables the effect), returning whether it actually changed.
    ///
    /// The radius is not clipped to any window extent here: it is a
    /// spread distance, not a coordinate, and a window that is resized
    /// keeps the frosting the app asked for.
    pub(crate) fn set_backdrop_blur(&mut self, radius_px: u16) -> bool {
        if radius_px == self.blur_radius {
            return false;
        }
        self.blur_radius = radius_px;
        true
    }

    /// Set the window's corner style, returning whether it actually changed.
    pub(crate) fn set_corners(&mut self, corners: Corners) -> bool {
        if corners == self.corners {
            return false;
        }
        self.corners = corners;
        true
    }

    /// Show or hide the window, returning whether it actually changed.
    pub(crate) fn set_visible(&mut self, visible: bool) -> bool {
        if visible == self.visible {
            return false;
        }
        self.visible = visible;
        true
    }

    pub(crate) fn set_cursor_hint(&mut self, cursor: CursorKind) {
        self.cursor = cursor;
    }

    /// Replace the content pixels with `surface`, adopting its extent as
    /// the window's client size (a replacement may be a different shape).
    pub(crate) fn replace_surface(&mut self, surface: Surface) {
        self.client_size = (surface.width(), surface.height());
        self.content = Some(surface);
    }

    /// Attach, replace, or clear the window's scrollable-content viewport,
    /// returning whether it actually changed.
    pub(crate) fn set_viewport(&mut self, viewport: Option<RootViewport>) -> bool {
        if viewport == self.viewport {
            return false;
        }
        self.viewport = viewport;
        true
    }

    pub(crate) fn viewport_mut(&mut self) -> Option<&mut RootViewport> {
        self.viewport.as_mut()
    }

    /// Attach or replace this window's decoration frame, resolving its band for
    /// the given output `scale`/`theme` so [`Self::bounds`] reflects the outer
    /// rectangle immediately. Passing `None` removes the decoration.
    pub(crate) fn set_frame(&mut self, frame: Option<WindowFrame>, scale: Scale, theme: &Theme) {
        self.frame = frame;
        self.refresh_band(scale, theme);
    }

    /// Re-resolve the decoration band for a new output `scale`/`theme`, so a
    /// runtime DPI or theme change re-sizes the reserved band.
    pub(crate) fn refresh_band(&mut self, scale: Scale, theme: &Theme) {
        self.band = self.frame.as_ref().map(|f| f.insets(scale, theme));
        self.rim = self.frame.as_ref().map(|f| f.rim(scale, theme));
    }

    /// Set the decorated window's activation, so the frame rim, title, and
    /// controls redraw under the new state. Returns `false` for an
    /// undecorated window (nothing to activate).
    pub(crate) fn set_frame_active(&mut self, active: bool) -> bool {
        let Some(frame) = self.frame.as_mut() else {
            return false;
        };
        let mut furniture = frame.furniture();
        furniture.activation = activation_for(furniture.activation, active);
        frame.set_furniture(furniture);
        true
    }

    /// Whether marking this window's frame `active` would change what it
    /// draws, or `None` for an undecorated window (nothing to activate).
    pub(crate) fn frame_activation_changes(&self, active: bool) -> Option<bool> {
        let current = self.frame.as_ref()?.furniture().activation;
        Some(activation_for(current, active) != current)
    }

    /// Set the decorated window's title. Returns `false` for an undecorated
    /// window (there is no title bar to label).
    pub(crate) fn set_frame_title(&mut self, title: &str) -> bool {
        let Some(frame) = self.frame.as_mut() else {
            return false;
        };
        frame.title_bar_mut().set_title(title);
        true
    }

    /// The owning application's identity artwork, rasterised at the title
    /// bar's identity-slot side, or `None` when this window has none — an
    /// unidentified window, or an identified one whose picture would not
    /// resolve and so draws its built-in glyph.
    #[must_use]
    pub fn identity_artwork(&self) -> Option<&Surface> {
        self.identity_artwork.as_ref()
    }

    /// Set the decorated window's owning-application identity: the icon class
    /// its title bar reserves a slot for, and the `artwork` to draw there.
    ///
    /// `artwork` must already be rasterised at
    /// [`TitleBar::icon_side`](tairix_controls::TitleBar::icon_side) of the
    /// laid-out title band; `None` leaves the built-in glyph for `identity`.
    /// The identity is the embedder's attestation of who owns the window,
    /// never anything the application claimed. Returns `false` for an
    /// undecorated window (there is no title bar to identify).
    pub(crate) fn set_frame_identity(
        &mut self,
        identity: IconKind,
        artwork: Option<Surface>,
    ) -> bool {
        let Some(frame) = self.frame.as_mut() else {
            return false;
        };
        frame.title_bar_mut().set_identity(Some(identity));
        self.identity_artwork = artwork;
        true
    }

    /// Feed a pointer `event` to this window's decoration furniture (the title
    /// bar and its command controls) so hover and press states advance, and
    /// return the typed [`TitleBarEvent`] it produced. Returns `None` for an
    /// undecorated window — it has no furniture to receive the event.
    pub(crate) fn on_frame_pointer(
        &mut self,
        event: &InputEvent,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        let bounds = self.bounds();
        let frame = self.frame.as_mut()?;
        let title_rect = frame.layout(bounds, scale, theme).title_bar;
        frame
            .title_bar_mut()
            .on_pointer(event, title_rect, scale, theme, damage)
    }

    /// Feed a key `key` to this window's decoration furniture (the title bar's
    /// command controls: Space/Enter activate the focused control, the arrows
    /// move focus between them) and return the typed [`TitleBarEvent`] it
    /// produced. Returns `None` for an undecorated window.
    pub(crate) fn on_frame_key(
        &mut self,
        key: Key,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        let bounds = self.bounds();
        let frame = self.frame.as_mut()?;
        let title_rect = frame.layout(bounds, scale, theme).title_bar;
        frame
            .title_bar_mut()
            .on_key(key, title_rect, scale, theme, damage)
    }

    /// Toggle a decorated, resizable window between restored and maximized,
    /// resizing it to `work_area` (the session work rectangle) on maximize
    /// and back to the geometry it had when maximized on restore. Returns
    /// the new size state and the resulting client rectangle, or `None`
    /// (changing nothing) for an undecorated window, a non-resizable one,
    /// or when the resize itself fails closed.
    ///
    /// The frame's furniture size is updated in step, so the size-toggle
    /// control shows the *next* action (Restore while maximized, Maximize
    /// while restored) and the decoration is repainted.
    pub(crate) fn toggle_size(
        &mut self,
        work_area: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<(WindowSizeState, Rect)> {
        let furniture = self.frame.as_ref()?.furniture();
        if !furniture.resizable {
            return None;
        }
        let (target, next_state) = match self.size_state {
            WindowSizeState::Restored => (work_area, WindowSizeState::Maximized),
            WindowSizeState::Maximized => (
                self.restore_outer.unwrap_or_else(|| self.bounds()),
                WindowSizeState::Restored,
            ),
        };
        if self.size_state == WindowSizeState::Restored {
            self.restore_outer = Some(self.bounds());
        }
        if !self.resize_to_outer(target, scale, theme) {
            // Fail closed: nothing moved, so do not record a state change or
            // strand the saved restore geometry.
            if next_state == WindowSizeState::Maximized {
                self.restore_outer = None;
            }
            return None;
        }
        self.size_state = next_state;
        if next_state == WindowSizeState::Restored {
            self.restore_outer = None;
        }
        if let Some(frame) = self.frame.as_mut() {
            let mut furniture = frame.furniture();
            furniture.size = next_state;
            frame.set_furniture(furniture);
        }
        Some((next_state, self.client_rect()))
    }

    /// Resize this window so its outer rectangle becomes `new_outer`: the
    /// client size becomes the implied one (the outer extent minus the
    /// reserved frame band), the origin follows the new top-left, and the
    /// decoration is repainted at the new size. Returns `false` (changing
    /// nothing) when the implied client size is empty (fail closed).
    ///
    /// The client's own pixels are left alone: they are the client's to
    /// resize, and it does so by presenting at its new size once the resize
    /// reaches it. Until then the window draws the pixels it has over as
    /// much of the new client area as they cover, which is what makes a
    /// live resize-grab track the pointer without the client in the loop.
    pub(crate) fn resize_to_outer(&mut self, new_outer: Rect, scale: Scale, theme: &Theme) -> bool {
        let (band_w, band_h) = match self.band {
            Some(insets) => (
                insets.left.saturating_add(insets.right),
                insets.top.saturating_add(insets.bottom),
            ),
            None => (0, 0),
        };
        if !self.set_client_size(
            new_outer.width.saturating_sub(band_w),
            new_outer.height.saturating_sub(band_h),
        ) {
            return false;
        }
        self.origin = new_outer.origin;
        self.refresh_band(scale, theme);
        true
    }

    /// Set this window's client size in place — the origin unchanged —
    /// repainting the decoration at the new size. This is the client-driven
    /// counterpart to [`resize_to_outer`](Self::resize_to_outer) (which
    /// sizes from an outer rectangle and moves the origin): the window
    /// channel's `Resize` hands the session a new *client* content size, so
    /// the compositor takes it directly. Returns `false` (changing nothing)
    /// when the size is empty.
    pub(crate) fn resize_client(
        &mut self,
        client_w: u32,
        client_h: u32,
        scale: Scale,
        theme: &Theme,
    ) -> bool {
        if !self.set_client_size(client_w, client_h) {
            return false;
        }
        self.refresh_band(scale, theme);
        true
    }

    /// Adopt `client_w` × `client_h` as the client extent the window is laid
    /// out from, refusing an empty one — the one definition both
    /// [`resize_to_outer`](Self::resize_to_outer) and
    /// [`resize_client`](Self::resize_client) share.
    fn set_client_size(&mut self, client_w: u32, client_h: u32) -> bool {
        if client_w == 0 || client_h == 0 {
            return false;
        }
        self.client_size = (client_w, client_h);
        true
    }

    /// Render this window's furniture chrome: the [`WindowFrame`] rim, body,
    /// title bar, and command controls, plus a corner resize grabber on a
    /// resizable window, as only the four furniture strips
    /// [`local_furniture_bands`](Self::local_furniture_bands) describes
    /// rather than a surface the size of the whole outer window (see
    /// [`WindowChrome`] for why). The client region between the strips is
    /// never sampled — the compositor draws the content there — and the
    /// rounded rim corners stay transparent so the desktop shows through.
    ///
    /// Returns `None` for an undecorated window, or when the render cannot
    /// allocate: the window then draws its content over the background band
    /// rather than the caller retaining a half-painted frame.
    ///
    /// The result is not stored here. Furniture is derived pixels the
    /// compositor keeps in the shared reclaimable cache every window's
    /// chrome competes for, so the desktop's total is bounded and released
    /// under memory pressure instead of each window pinning its own copy
    /// for as long as it exists.
    pub(crate) fn render_chrome(&self, scale: Scale, theme: &Theme) -> Option<WindowChrome> {
        let frame = self.frame.as_ref()?;
        let bands = self.local_furniture_bands();
        WindowChrome::render(
            frame,
            bands,
            self.outer_size(),
            scale,
            theme,
            self.identity_artwork.as_ref(),
        )
    }

    /// Whether this window has furniture to render at all — the test the
    /// compositor's residency pass uses to skip every undecorated window
    /// before it reaches the cache, so a plain window neither counts as a
    /// miss nor costs a lookup.
    pub(crate) const fn is_decorated(&self) -> bool {
        self.frame.is_some()
    }

    /// The reserved furniture bands relative to the window's own outer
    /// top-left (local coordinates: the outer rectangle's corner is
    /// `(0, 0)`) — the top (title), bottom, left, and right strips around
    /// the client — for a decorated window, or four empty rectangles for an
    /// undecorated one.
    ///
    /// This is the single definition of the band geometry: the screen-space
    /// [`furniture_bands`](Self::furniture_bands) translates it by the
    /// window's origin, and [`render_chrome`](Self::render_chrome) paints
    /// directly in this local space, so the four rectangles are derived
    /// once rather than separately in each caller.
    ///
    /// The top and bottom strips reach at least the rim's corner radius, even
    /// where the reserved inset is thinner: a corner arc's rows must be drawn
    /// as *furniture* over their whole width, or the client's square row would
    /// be the only pixels there and the curve the rim traces could not be seen
    /// at all. The side strips take only the rows between them, so no strip
    /// retains a pixel another already holds.
    fn local_furniture_bands(&self) -> [Rect; 4] {
        let Some(insets) = self.band else {
            return [Rect::EMPTY; 4];
        };
        let (ow, oh) = self.outer_size();
        let outer = Rect::new(0, 0, ow, oh);
        let (top_depth, bottom_depth) = self.corner_depths();
        let middle = oh.saturating_sub(top_depth).saturating_sub(bottom_depth);
        let side_top = outer.top().saturating_add_unsigned(top_depth);
        let top = Rect::new(outer.left(), outer.top(), outer.width, top_depth);
        let bottom = Rect::new(
            outer.left(),
            outer.bottom().saturating_sub_unsigned(bottom_depth),
            outer.width,
            bottom_depth,
        );
        let left = Rect::new(outer.left(), side_top, insets.left, middle);
        let right = Rect::new(
            outer.right().saturating_sub_unsigned(insets.right),
            side_top,
            insets.right,
            middle,
        );
        [top, bottom, left, right]
    }

    /// How deep the top and bottom furniture strips are: the reserved inset,
    /// grown to the rim's corner radius where the arc reaches further in than
    /// the inset does, and never past the rows the window has.
    ///
    /// [`decoration_spans`](Self::decoration_spans) maps rows to strips by
    /// exactly these depths, so the pixels rendered and the pixels sampled
    /// cannot disagree.
    fn corner_depths(&self) -> (u32, u32) {
        let Some(insets) = self.band else {
            return (0, 0);
        };
        let (_, oh) = self.outer_size();
        let arc = self.rim.map_or(0, |rim| rim.radius);
        let top = insets.top.max(arc).min(oh);
        let bottom = insets.bottom.max(arc).min(oh.saturating_sub(top));
        (top, bottom)
    }

    /// The reserved furniture bands in screen coordinates — the top (title),
    /// bottom, left, and right strips around the client — for a decorated
    /// window, or four empty rectangles for an undecorated one.
    ///
    /// A furniture-only change (focus flip, title edit, control state) marks
    /// just these bands dirty, so the client area is never needlessly
    /// recomposited (damage stays confined to the furniture).
    pub(crate) fn furniture_bands(&self) -> [Rect; 4] {
        if self.band.is_none() {
            return [Rect::EMPTY; 4];
        }
        self.local_furniture_bands().map(|band| {
            Rect::new(
                self.origin.x.saturating_add(band.left()),
                self.origin.y.saturating_add(band.top()),
                band.width,
                band.height,
            )
        })
    }

    /// The top (title-bar) furniture band in screen coordinates for a decorated
    /// window, or [`Rect::EMPTY`] for an undecorated one — the region a title
    /// change repaints.
    pub(crate) fn title_band(&self) -> Rect {
        match self.band {
            Some(insets) => {
                let outer = self.bounds();
                Rect::new(outer.left(), outer.top(), outer.width, insets.top)
            }
            None => Rect::EMPTY,
        }
    }
}

/// The activation a frame currently in `current` takes when the window
/// manager marks it `active`.
///
/// Attention requests are a separate client-driven state, so a window that has
/// raised one keeps it while inactive rather than being forced quiet.
fn activation_for(current: WindowActivationState, active: bool) -> WindowActivationState {
    if active {
        WindowActivationState::Active
    } else if current == WindowActivationState::AttentionRequested {
        current
    } else {
        WindowActivationState::Inactive
    }
}

/// One screen row of a window's composited contribution, resolved by
/// [`Window::row`].
///
/// Holds the source row slices this window draws from and the row-constant
/// factors that apply to them, so [`sample`](Self::sample) is a slice
/// index rather than a fresh layer decision per column.
pub(crate) struct WindowRow<'a> {
    /// Drawable client pixels, the first at screen column `client_x`.
    /// Shorter than `client_cols` where the furniture gutter clips the
    /// row's tail, and empty where it clips the whole row.
    content: &'a [Pixel],
    /// Decoration pixels, from at most two furniture strips (a row is
    /// either in the top or bottom band — one strip, the other span empty —
    /// or crosses the client's own vertical range, where the left and right
    /// side strips each contribute one span). Empty for an undecorated
    /// window.
    decoration: [DecorationSpan<'a>; 2],
    /// Screen column of `content`'s first pixel.
    client_x: i32,
    /// Columns from `client_x` the client owns, drawable or gutter-clipped:
    /// a clipped client pixel draws nothing rather than letting the
    /// decoration behind it show through.
    client_cols: u32,
    /// Alpha applied to every pixel this row draws.
    opacity: u8,
    /// The clip the client's own pixels meet on this row, or `None` where
    /// every one of them is fully inside it.
    cut: Option<RowCut>,
}

impl WindowRow<'_> {
    /// The composited contribution at screen column `x`: the source pixel
    /// scaled by the combined opacity and clip coverage, or `None` where this
    /// window draws nothing there.
    ///
    /// A client pixel the clip does not fully cover gives way to the
    /// decoration, because on such a column the frame's own arc — its rim and
    /// the plate inside it — is what the window's curve is made of. Only where
    /// there is no decoration to give way to does the coverage anti-alias the
    /// client's own edge, which is how a plain rounded window (a popup, the
    /// taskbar) presents its corners.
    ///
    /// `bias` is this pixel's share of the composite's ordered dither, taken
    /// here as well as in the blend because scaling by an opacity is the same
    /// loss of tonal resolution: a translucent window's own gradients would
    /// otherwise step before they ever reached the backdrop.
    #[must_use]
    pub(crate) fn sample(&self, x: i32, bias: u32) -> Option<Pixel> {
        if let Some(lx) = x
            .checked_sub(self.client_x)
            .and_then(|d| u32::try_from(d).ok())
        {
            if lx < self.client_cols {
                let coverage = self.cut.map_or(u8::MAX, |cut| cut.coverage(lx));
                if coverage == u8::MAX {
                    return Some(
                        self.content
                            .get(lx as usize)?
                            .scale_alpha_biased(self.opacity, bias),
                    );
                }
                if let Some(pixel) = self.decoration_sample(x) {
                    return Some(pixel.scale_alpha_biased(self.opacity, bias));
                }
                let pixel = *self.content.get(lx as usize)?;
                return Some(pixel.scale_alpha_biased(combine(self.opacity, coverage), bias));
            }
        }
        Some(
            self.decoration_sample(x)?
                .scale_alpha_biased(self.opacity, bias),
        )
    }

    /// The decoration pixel at screen column `x`, from whichever of the row's
    /// two furniture spans holds it.
    fn decoration_sample(&self, x: i32) -> Option<Pixel> {
        self.decoration[0]
            .sample(x)
            .or_else(|| self.decoration[1].sample(x))
    }

    /// Blend this row's contribution over `dst`, whose first pixel is screen
    /// column `first_x`, and report how many columns it contributed to — the
    /// blend count the frame counters read, which counts a contribution
    /// whatever its alpha, exactly as sampling column by column did.
    ///
    /// Outside its rounded corners a window's contribution to a row is three
    /// straight runs — the two furniture strips and the client's own drawable
    /// pixels — laid at a single opacity, so the row is composited a *run* at
    /// a time through the shared span blend rather than a column at a time
    /// through [`sample`](Self::sample). The arithmetic is the same operator
    /// at the same rounding; what goes is the per-column layer decision, which
    /// is what the composite was actually spending its time on.
    ///
    /// Two rows keep the column-by-column path, because on them the three runs
    /// are not the whole truth: a row the shape cuts, where coverage varies
    /// across the arc and the frame's own rim takes precedence over the client
    /// beneath it, and the row of a window whose furniture somehow reaches
    /// into its client columns, where that same precedence decides. The second
    /// cannot arise from the furniture bands this window manager lays out — a
    /// strip that overlaps the client is a corner row, and those are cut — so
    /// it is a guard against a layout that has changed rather than a case in
    /// use, and it fails onto the slower path rather than into the wrong
    /// pixels.
    pub(crate) fn blend_into(&self, dst: &mut [Pixel], first_x: i32, dither: DitherRow) -> u64 {
        if self.cut.is_some() || self.decoration.iter().any(|s| self.overlaps_client(s)) {
            let mut blended = 0;
            for (dst, x) in dst.iter_mut().zip(first_x..) {
                let bias = dither.bias(x.cast_unsigned());
                if let Some(src) = self.sample(x, bias) {
                    *dst = src.over_biased(*dst, bias);
                    blended += 1;
                }
            }
            return blended;
        }
        let mut blended = blend_run(
            dst,
            first_x,
            self.drawable(),
            self.client_x,
            self.opacity,
            dither,
        );
        for span in &self.decoration {
            blended += blend_run(dst, first_x, span.pixels, span.x, self.opacity, dither);
        }
        blended
    }

    /// The client pixels this row actually draws: the content row, never
    /// longer than the columns the client owns, so a gutter-clipped tail draws
    /// nothing rather than spilling past the gutter.
    fn drawable(&self) -> &[Pixel] {
        let cols = usize::try_from(self.client_cols).unwrap_or(usize::MAX);
        self.content
            .get(..cols.min(self.content.len()))
            .unwrap_or(&[])
    }

    /// Whether `span` reaches any column the client owns, where
    /// [`sample`](Self::sample) decides between the two by coverage rather
    /// than by position.
    fn overlaps_client(&self, span: &DecorationSpan<'_>) -> bool {
        let Ok(len) = i64::try_from(span.pixels.len()) else {
            return true;
        };
        if len == 0 || self.client_cols == 0 {
            return false;
        }
        let client = i64::from(self.client_x);
        i64::from(span.x) < client + i64::from(self.client_cols) && i64::from(span.x) + len > client
    }

    /// How many columns from screen column `x` towards `limit` cannot begin an
    /// [`opaque_run`](Self::opaque_run), so a caller that has just been
    /// refused one knows how far to compose before asking again.
    ///
    /// Always at least one column, so a caller stepping by it makes progress
    /// whatever it was refused for.
    #[must_use]
    pub(crate) fn blend_len(&self, x: i32, limit: i32) -> usize {
        let all = usize::try_from(i64::from(limit) - i64::from(x)).unwrap_or(0);
        if self.opacity != u8::MAX || self.cut.is_some() {
            return all;
        }
        let Ok(lx) = usize::try_from(i64::from(x) - i64::from(self.client_x)) else {
            // Before the client's own pixels, where no run can start.
            return usize::try_from(i64::from(self.client_x) - i64::from(x))
                .unwrap_or(all)
                .clamp(1, all.max(1));
        };
        // Past the client's own pixels — the furniture beyond the gutter, or a
        // short content buffer — nothing further along the row can begin one.
        let Some(span) = self.drawable().get(lx..).filter(|s| !s.is_empty()) else {
            return all;
        };
        span.iter()
            .take_while(|pixel| pixel.a != u8::MAX)
            .count()
            .clamp(1, all.max(1))
    }

    /// The longest run of source pixels this row contributes from screen
    /// column `x` towards `limit` that each replace whatever is beneath them
    /// exactly, or `None` when the pixel at `x` is not one.
    ///
    /// A caller may copy such a run and skip every layer below it without
    /// changing one byte of the result, because *over* with a fully opaque
    /// source is the source. Three things must hold, and they are the only
    /// three: full window opacity and no clip on the row (either would scale
    /// or drop the pixel), and a source alpha of 255. Only the client's own
    /// pixels qualify — furniture is left to the general blend, which keeps
    /// this a loop specialisation rather than a second blend.
    #[must_use]
    pub(crate) fn opaque_run(&self, x: i32, limit: i32) -> Option<&[Pixel]> {
        if self.opacity != u8::MAX || self.cut.is_some() {
            return None;
        }
        let start = usize::try_from(x.checked_sub(self.client_x)?).ok()?;
        // A client column past the drawable extent contributes nothing at
        // all, so the run stops where the gutter or a short buffer does.
        let drawable = usize::try_from(self.client_cols)
            .unwrap_or(usize::MAX)
            .min(self.content.len());
        let end = usize::try_from(limit.checked_sub(self.client_x)?)
            .ok()?
            .min(drawable);
        let span = self.content.get(start..end)?;
        let len = span.iter().take_while(|p| p.a == u8::MAX).count();
        span.get(..len).filter(|run| !run.is_empty())
    }
}

/// One furniture strip's contribution to a screen row: its pixels, the
/// first at screen column `x`. Empty when the row draws nothing from that
/// strip.
#[derive(Copy, Clone)]
struct DecorationSpan<'a> {
    pixels: &'a [Pixel],
    x: i32,
}

impl<'a> DecorationSpan<'a> {
    /// The span that contributes nothing.
    const EMPTY: Self = Self { pixels: &[], x: 0 };

    const fn new(pixels: &'a [Pixel], x: i32) -> Self {
        Self { pixels, x }
    }

    /// The pixel at screen column `x`, or `None` outside this span.
    fn sample(&self, x: i32) -> Option<Pixel> {
        let lx = x
            .checked_sub(self.x)
            .and_then(|d| usize::try_from(d).ok())?;
        self.pixels.get(lx).copied()
    }
}

/// The rounded shape a plain window's rectangle is cut to: its corner style
/// and the extent that style is measured against.
///
/// Read once and reused across a whole row or region — the compositor
/// weights a window's frosted backdrop by it — so the style and the extent
/// can never be paired wrongly and the corner arithmetic is not re-derived
/// per pixel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct WindowShape {
    corners: Corners,
    width: u32,
    height: u32,
}

impl WindowShape {
    /// Coverage in `0..=255` at window-local `(lx, ly)`.
    pub(crate) fn coverage(self, lx: u32, ly: u32) -> u8 {
        self.corners.coverage(lx, ly, self.width, self.height)
    }

    /// How far in from its own edges this shape can weight a pixel by less than
    /// full coverage: the radius it actually rounds by, clamped as the shared
    /// rounded-rectangle definition clamps it. A pixel at least this far inside
    /// every edge is fully covered, whatever the corner style.
    pub(crate) fn corner_reach(self) -> u32 {
        self.corners.radius(self.width, self.height)
    }

    /// Whether row `ly` carries an arc at all: `false` where the shape covers
    /// every column of it.
    fn clips_row(self, ly: u32) -> bool {
        self.corners.clips_row(ly, self.width, self.height)
    }
}

/// The shape a window's client pixels are clipped to, and where the client
/// sits inside it: `offset` is the client's top-left in the shape's own
/// coordinates, which is the rim thickness in from the outer rectangle for a
/// decorated window and nothing at all for a plain one.
#[derive(Copy, Clone)]
struct ClientCut {
    shape: WindowShape,
    offset: (u32, u32),
}

/// One content row of a window's [`ClientCut`], addressed in client columns:
/// both `ly` and `lx0` are already in the shape's own coordinates, so a column
/// costs one addition and one coverage lookup.
#[derive(Copy, Clone)]
pub(crate) struct RowCut {
    shape: WindowShape,
    /// This row in the shape's own coordinates.
    ly: u32,
    /// The shape column the client's first pixel sits at.
    lx0: u32,
}

impl RowCut {
    /// Coverage in `0..=255` for client column `lx` of this row.
    pub(crate) fn coverage(self, lx: u32) -> u8 {
        self.shape.coverage(lx.saturating_add(self.lx0), self.ly)
    }
}

/// Combine two `0..=255` factors as `a * b / 255` (shared `div255`).
fn combine(a: u8, b: u8) -> u8 {
    div255(u32::from(a) * u32::from(b))
}
