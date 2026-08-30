//! Shell-surface controls: [`Notification`], [`TaskbarItem`],
//! [`WindowPreview`], and [`TraySignal`] (spec §11.25–§11.27).
//!
//! These are the desktop's *shell* surfaces — the transient message, the
//! icon-bar entry, the window-picker cell, and the notification-area status
//! capsule. Each is a
//! first-class Reactive Alloy control drawn over the shared `crate::paint`
//! core (plate, rail, Heat Seam, Signal Bead) and the shared `lib/theme`
//! tokens, so nothing here restates a visual recipe. A
//! control renders state and emits a typed userland action; the owning
//! service enforces authority, and a denied action reads distinctly from a
//! disabled one (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{IconKind, IconPicture};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::button::{icon_content_side, Button, ButtonAction};
use crate::collection::{Card, CardAction};
use crate::damage;
use crate::paint::{
    foreground, inset, key_activation, paint_bead, paint_count_badge, paint_icon_slot, paint_plate,
    paint_surface_plate, paint_text_line, plate_border, pointer_activation, rail_thickness,
    resolve_bead, resolve_frame, resolve_rail, role_font, seam_thickness, seam_width, surface_rect,
    text_plate_height, to_i32, BeadShape, ChromeLayer, PlateStyle, FULL_COLOUR,
};
use crate::state::{
    ControlDisposition, ControlRole, ControlState, PlateSeating, PointerState, RecoveryState,
    RenderInvariant, ValidationState,
};

// --- Notification ------------------------------------------------------

/// The outcome of feeding input to a [`Notification`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NotificationAction {
    /// The notification action at `index` was activated (e.g. a "Clear" or
    /// "Recover" button); the owner performs it and enforces authority.
    ActionActivated {
        /// The zero-based index of the activated action button.
        index: usize,
    },
}

/// A compact, actionable transient message (spec §11.25).
///
/// A notification *is* a [`Card`] carrying semantic beads, plus an optional
/// *source* attribution (the application or service that raised it). The card's
/// composed [`ControlState`] drives the reading: an informational notice keeps
/// the quiet rim, a background job shows a Heat Seam (its `activity`), a warning
/// shows the warning rail (its `validation`), a recoverable object shows the
/// recovery bead (its `recovery`), and a refused action shows the Authority Mark
/// (its `authority`) beside the source name — never a generic disabled look
/// (spec §13). Its actions are footer [`Button`]s; the notification routes input
/// to them and reports [`NotificationAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    card: Card,
    source: Option<String>,
}

impl Notification {
    /// A neutral, informational notification with the given title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            card: Card::new(title),
            source: None,
        }
    }

    /// This notification with a message line below the title.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.card = self.card.with_body(message);
        self
    }

    /// This notification with a source application/service attribution.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// This notification with a non-default role.
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.card = self.card.with_role(role);
        self
    }

    /// This notification with the given composed state (drives its semantics).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.card = self.card.with_state(state);
        self
    }

    /// This notification with a top-trailing count badge (grouped count).
    #[must_use]
    pub fn with_count(mut self, count: u32) -> Self {
        self.card = self.card.with_count(count);
        self
    }

    /// This notification with the given action buttons.
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<Button>) -> Self {
        self.card = self.card.with_footer(actions);
        self
    }

    /// The notification's source attribution, if any.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// The notification's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.card.state()
    }

    /// Replace the notification's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.card.set_state(state);
    }

    /// The notification's action buttons.
    #[must_use]
    pub fn actions(&self) -> &[Button] {
        self.card.footer()
    }

    /// Mutable access to the action buttons (e.g. to update their state).
    pub fn actions_mut(&mut self) -> &mut [Button] {
        self.card.footer_mut()
    }

    /// The height of the source caption strip, in surface pixels.
    fn caption_height(scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
        font.line_height()
            .saturating_add(scale.scale_length(theme.metrics().control_inset).max(1))
    }

    /// The card sub-rectangle, below the source caption when a source is shown.
    fn card_bounds(&self, bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Rect {
        if self.source.is_none() {
            return bounds;
        }
        let caption = Self::caption_height(scale, theme, font);
        let h = bounds.height.saturating_sub(caption);
        Rect::new(
            bounds.left(),
            bounds.top().saturating_add(to_i32(caption)),
            bounds.width,
            h,
        )
    }

    /// Paint the notification into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        if let Some(source) = &self.source {
            if let Some((x, y, w, _)) = surface_rect(bounds) {
                let pad = scale.scale_length(theme.metrics().control_inset).max(1);
                if w > pad.saturating_mul(2) {
                    let fitted = font.truncate_to_width(source, w - pad.saturating_mul(2));
                    font.draw_text(
                        surface,
                        to_i32(x + pad),
                        to_i32(y + pad / 2),
                        fitted,
                        foreground(theme, ControlDisposition::DisabledByState),
                    );
                }
            }
        }
        self.card.render(
            surface,
            self.card_bounds(bounds, scale, theme, font),
            scale,
            theme,
        );
    }

    /// Feed a pointer event; a footer action that completes a click reports
    /// [`NotificationAction::ActionActivated`]. A notification has no
    /// master/detail body to select, so a press on its body (as opposed to a
    /// footer action) is consumed and reported as nothing.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<NotificationAction> {
        let font = role_font(theme, scale, TextRole::Body);
        let card_bounds = self.card_bounds(bounds, scale, theme, font);
        match self
            .card
            .on_pointer(event, card_bounds, scale, theme, damage)?
        {
            CardAction::FooterActivated { index } => {
                Some(NotificationAction::ActionActivated { index })
            }
            CardAction::Pressed => None,
        }
    }

    /// Feed a key event; a focused footer action activated with Space/Enter
    /// reports [`NotificationAction::ActionActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<NotificationAction> {
        match self.card.on_key(key)? {
            CardAction::FooterActivated { index } => {
                Some(NotificationAction::ActionActivated { index })
            }
            CardAction::Pressed => None,
        }
    }
}

// --- TaskbarItem -------------------------------------------------------

/// The outcome of feeding input to a [`TaskbarItem`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskbarItemAction {
    /// The item was activated (clicked, or Space/Enter while focused); the
    /// owner decides whether to focus, restore, or minimize the window.
    Activated,
}

/// An icon-bar slot for one running application (spec §11.26).
///
/// A slot states application identity (its icon), live activity, and
/// attention on one Alloy Plate. It is always seated in the bar
/// ([`PlateSeating::Bar`]), so it wears no perimeter and shows no plate at
/// all while it rests: a run of applications reads as one bar rather than a
/// row of boxes, and every state is stated *inside* the slot. Hover, press,
/// or keyboard focus raise the plate as a wash. Background work shows a Heat
/// Seam; an attention request or a recovery/denied state shows a shape-coded
/// Signal Bead (spec §13, §15). It renders state and reports
/// [`TaskbarItemAction`]; the owner performs the application operation.
///
/// **A slot draws no running/focused mark.** Every slot on the icon bar *is*
/// a running application — the bar lists nothing else — so a "this one is
/// running" seam would mark every slot alike, and which window holds focus
/// is stated by that window's own chrome rather than by a mark on the
/// application it belongs to.
///
/// The slot draws a centred, plate-sized icon and no text. A window title is
/// the owner's model data, not the item's: it belongs to the owner's context
/// surfaces (the window picker, the application menu), and a title change
/// must not repaint a bar whose pixels are identical.
///
/// Equal items draw the same pixels, so an icon bar may use `==` as its
/// repaint gate: the icon, attention flag, and visible state compare. The
/// pointer coordinate and press latch do not — no render path reads either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskbarItem {
    icon: IconKind,
    state: ControlState,
    attention: bool,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl TaskbarItem {
    /// A resting icon-bar slot showing the given icon.
    #[must_use]
    pub fn new(icon: IconKind) -> Self {
        Self {
            icon,
            state: ControlState::idle(),
            attention: false,
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// This item with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// This item marked as requesting attention (Signal Bead).
    #[must_use]
    pub fn with_attention(mut self, attention: bool) -> Self {
        self.attention = attention;
        self
    }

    /// The item's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the item's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the item's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// Set whether this item is requesting attention.
    pub fn set_attention(&mut self, attention: bool) {
        self.attention = attention;
    }

    /// The Signal Bead the item shows, if any: an authority/recovery/complete
    /// bead (shared priority) wins, then an attention request draws an accent
    /// bead — so a denial is never hidden behind an attention notice.
    fn bead(&self, theme: &Theme) -> Option<(Color, BeadShape)> {
        if let Some(bead) = resolve_bead(theme, self.state) {
            return Some(bead);
        }
        if self.attention {
            return Some((Color::from(theme.palette().accent), BeadShape::Check));
        }
        None
    }

    /// The pixel side the item's icon paints at inside `bounds`.
    ///
    /// This is the render geometry itself, exposed so an owner rasterising
    /// per-application artwork can produce it at exactly the size
    /// [`Self::render`] will place — the two can never disagree. The icon is
    /// sized off the plate like an icon button.
    #[must_use]
    pub fn icon_side(&self, bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        let Some((_, _, w, h)) = surface_rect(bounds) else {
            return 0;
        };
        icon_content_side(w, h, plate_border(theme, scale))
    }

    /// Paint the item into `surface` at `bounds` for the active theme.
    ///
    /// `artwork` is the application's own icon, pre-rasterised by the owner
    /// (at [`Self::icon_side`], through its cache); `None` falls back to the
    /// item's built-in class glyph. The artwork is decoded and rasterised
    /// long before it reaches this call — a control never parses image bytes.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius);
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);

        // A slot rests without a plate at all — only its icon sits on the bar
        // — so a run of applications reads as one bar; hover, press, or
        // keyboard focus raise the plate as a wash.
        if let Some((plate, rim)) = frame.face(PlateSeating::Bar) {
            paint_plate(
                surface,
                (x, y, w, h),
                &PlateStyle {
                    radius,
                    border,
                    plate,
                    rim,
                    focused: frame.focused,
                    ring: Color::from(palette.rim_active),
                },
            );
        }

        let inner_x = x + border;
        let inner_y = y + border;
        let inner_w = w.saturating_sub(border.saturating_mul(2));
        let inner_h = h.saturating_sub(border.saturating_mul(2));
        if inner_w == 0 || inner_h == 0 {
            return;
        }
        let bead_size = self.bead(theme).map_or(0, |_| {
            scale
                .scale_length(metrics.bead_size)
                .max(3)
                .min(inner_w)
                .min(inner_h)
        });

        // The application identity alone, centred in the plate; the window
        // title stays the owner's model data for its context surfaces.
        let side = self.icon_side(bounds, scale, theme);
        if side > 0 {
            let ix = inner_x + (inner_w.saturating_sub(side)) / 2;
            let iy = inner_y + (inner_h.saturating_sub(side)) / 2;
            self.paint_icon(surface, ix, iy, side, frame.label, artwork);
        }

        self.paint_status(
            surface,
            (inner_x, inner_y, inner_w, inner_h),
            bead_size,
            scale,
            theme,
        );
    }

    /// Paint the item's identity icon at `(x, y)` in a `side`-pixel slot
    /// through the shared "artwork else built-in glyph" rule, carrying this
    /// item's class glyph and the resolved frame tint.
    fn paint_icon(
        &self,
        surface: &mut Surface,
        x: u32,
        y: u32,
        side: u32,
        tint: Color,
        artwork: Option<IconPicture<'_>>,
    ) {
        paint_icon_slot(surface, (x, y, side), self.icon, tint, artwork, FULL_COLOUR);
    }

    /// Paint the item's status marks: the activity Heat Seam on the lower
    /// edge and the top-trailing Signal Bead.
    fn paint_status(
        &self,
        surface: &mut Surface,
        inner: (u32, u32, u32, u32),
        bead_size: u32,
        scale: Scale,
        theme: &Theme,
    ) {
        let (inner_x, inner_y, inner_w, inner_h) = inner;
        let palette = theme.palette();
        let seam_h = seam_thickness(theme, scale).min(inner_h);
        let activity_w = seam_width(self.state.activity, inner_w);
        if activity_w > 0 && inner_h > seam_h {
            surface.fill_rect(
                inner_x,
                inner_y + inner_h - seam_h,
                activity_w,
                seam_h,
                Color::from(palette.accent),
            );
        }

        // The top-trailing Signal Bead.
        if let Some((color, shape)) = self.bead(theme) {
            if bead_size > 0 {
                let bx = inner_x + inner_w - bead_size;
                paint_bead(surface, bx, inner_y, bead_size, color, shape);
            }
        }
    }

    /// Feed a pointer event; a completed primary click reports
    /// [`TaskbarItemAction::Activated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<TaskbarItemAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        )
        .then_some(TaskbarItemAction::Activated)
    }

    /// Feed a key event; Space/Enter while focused reports
    /// [`TaskbarItemAction::Activated`].
    pub fn on_key(&mut self, key: Key) -> Option<TaskbarItemAction> {
        key_activation(self.state, key).then_some(TaskbarItemAction::Activated)
    }
}

// --- WindowPreview -----------------------------------------------------

/// The outcome of feeding input to a [`WindowPreview`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WindowPreviewAction {
    /// The preview was activated (clicked, or Space/Enter while focused);
    /// the owner raises and focuses the window it stands for.
    Activated,
}

/// One cell of the window picker an icon-bar slot opens: a captioned
/// thumbnail of a single window (spec §11.26).
///
/// Where a [`TaskbarItem`] is one *application*, a preview is one *window* of
/// it — so this is the surface that names a window, and the caption is the
/// window's own title. The thumbnail is a scaled copy of that window's last
/// presented frame, pre-rasterised by the owner at
/// [`thumbnail_bounds`](Self::thumbnail_bounds) exactly as every other
/// control takes owner-supplied artwork: a control never scales a live
/// surface on a paint path, and never parses image bytes.
///
/// A window with no thumbnail yet (one that has not presented, or whose
/// pixels the owner released under memory pressure) draws its application's
/// class glyph in the thumbnail's place, so a cell can never come up blank.
///
/// Equal previews draw the same pixels, so a picker may use `==` as its
/// repaint gate: the caption, the glyph, and the visible state compare. The
/// pointer coordinate and press latch do not — no render path reads either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowPreview {
    caption: String,
    icon: IconKind,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The press latch; the press *look* lives in `state.pointer`.
    armed: RenderInvariant<bool>,
}

impl WindowPreview {
    /// A resting preview captioned `caption`, falling back to `icon` when no
    /// thumbnail is supplied.
    #[must_use]
    pub fn new(caption: impl Into<String>, icon: IconKind) -> Self {
        Self {
            caption: caption.into(),
            icon,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            armed: RenderInvariant::new(false),
        }
    }

    /// This preview with the given composed state.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.state = state;
        self
    }

    /// The preview's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the preview's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
    }

    /// Set the preview's keyboard focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.state.focus.focused = focused;
    }

    /// The window's title, as the caption states it.
    #[must_use]
    pub fn caption(&self) -> &str {
        &self.caption
    }

    /// The rectangle inside `bounds` the thumbnail occupies: the plate's
    /// interior above the one caption line.
    ///
    /// This is the render geometry itself, exposed so an owner scaling a
    /// window's frame produces it at exactly the size
    /// [`render`](Self::render) will place — the two can never disagree, and
    /// a picker sizes a whole row of thumbnails from this one query.
    /// [`Rect::EMPTY`] when `bounds` cannot hold a caption and a thumbnail.
    #[must_use]
    pub fn thumbnail_bounds(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Rect {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Rect::EMPTY;
        };
        let border = plate_border(theme, scale);
        let pad = scale.scale_length(theme.metrics().control_inset);
        let font = role_font(theme, scale, TextRole::Body);
        let caption_h = font.line_height().saturating_add(pad);
        let inner_w = w.saturating_sub(border.saturating_add(pad).saturating_mul(2));
        let inner_h = h
            .saturating_sub(border.saturating_add(pad).saturating_mul(2))
            .saturating_sub(caption_h);
        if inner_w == 0 || inner_h == 0 {
            return Rect::EMPTY;
        }
        Rect::new(
            to_i32(x.saturating_add(border).saturating_add(pad)),
            to_i32(y.saturating_add(border).saturating_add(pad)),
            inner_w,
            inner_h,
        )
    }

    /// Paint the preview into `surface` at `bounds` for the active theme.
    ///
    /// `thumbnail` is the window's own scaled frame, pre-rasterised by the
    /// owner at [`thumbnail_bounds`](Self::thumbnail_bounds); `None` falls
    /// back to the class icon so the cell always states something, drawn from
    /// the `artwork` the owner resolved for it.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        thumbnail: Option<&Surface>,
        artwork: Option<IconPicture<'_>>,
    ) {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius);
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);
        if let Some((plate, rim)) = frame.face(PlateSeating::Panel) {
            paint_plate(
                surface,
                (x, y, w, h),
                &PlateStyle {
                    radius,
                    border,
                    plate,
                    rim,
                    focused: frame.focused,
                    ring: Color::from(palette.rim_active),
                },
            );
        }

        let thumb = self.thumbnail_bounds(bounds, scale, theme);
        if thumb.is_empty() {
            return;
        }
        let (tx, ty, tw, th) = (thumb.left(), thumb.top(), thumb.width, thumb.height);
        // The owner scaled the frame to exactly this rectangle, so the copy
        // is a straight blit — the pixels are already right. A window with no
        // frame yet draws its application's glyph in its place.
        if let Some(image) = thumbnail {
            surface.blit(tx, ty, image);
        } else {
            let side = tw.min(th);
            paint_icon_slot(
                surface,
                (
                    u32::try_from(tx).unwrap_or(0) + (tw.saturating_sub(side)) / 2,
                    u32::try_from(ty).unwrap_or(0) + (th.saturating_sub(side)) / 2,
                    side,
                ),
                self.icon,
                frame.label,
                artwork,
                FULL_COLOUR,
            );
        }

        let pad = scale.scale_length(metrics.control_inset);
        let font = role_font(theme, scale, TextRole::Body);
        let caption_y = u32::try_from(ty).unwrap_or(0).saturating_add(th);
        paint_text_line(
            surface,
            &self.caption,
            (
                u32::try_from(tx).unwrap_or(0),
                caption_y.saturating_add(pad),
            ),
            (y.saturating_add(h).saturating_sub(border), tw, 0),
            font,
            frame.label,
        );
    }

    /// Feed a pointer event; a completed primary click reports
    /// [`WindowPreviewAction::Activated`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        damage: &mut Region,
    ) -> Option<WindowPreviewAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let inside = bounds.contains(*self.pointer);
        pointer_activation(
            &mut self.state,
            &mut self.armed,
            event,
            inside,
            bounds,
            damage,
        )
        .then_some(WindowPreviewAction::Activated)
    }

    /// Feed a key event; Space or Enter while focused reports
    /// [`WindowPreviewAction::Activated`].
    pub fn on_key(&mut self, key: Key) -> Option<WindowPreviewAction> {
        key_activation(self.state, key).then_some(WindowPreviewAction::Activated)
    }
}

// --- TraySignal --------------------------------------------------------

/// The outcome of feeding input to a [`TraySignal`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TraySignalAction {
    /// The readout's primary safe action was activated; the owner performs it.
    Activated,
}

/// The content a [`TraySignal`]'s live-state badge shows.
///
/// A count reads as its literal digit; once it would need more than one
/// digit the badge caps at `"9+"` rather than growing arbitrarily wide. An
/// urgent state with no natural count (e.g. a hung application) shows an
/// exclamation mark instead.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrayBadgeContent {
    /// A small count, rendered as its literal digit while it fits in one
    /// (`"1"` through `"9"`), else capped at `"9+"`.
    Count(u16),
    /// An urgent state with no natural count.
    Alert,
}

impl TrayBadgeContent {
    /// The literal text the badge paints for this content.
    pub(crate) fn text(self) -> String {
        match self {
            Self::Count(n) if n <= 9 => {
                let digit = b'0' + u8::try_from(n).unwrap_or(9);
                String::from(char::from(digit))
            }
            Self::Count(_) => String::from("9+"),
            Self::Alert => String::from("!"),
        }
    }
}

/// The palette role a [`TraySignal`] badge's fill encodes — the dominant live
/// state driving it (spec §11.27).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrayBadgeTone {
    /// A background job — the theme accent.
    Accent,
    /// A resource pressure — the theme warning role.
    Warning,
    /// A hung, unresponsive application — the same danger role a destructive
    /// action's rim takes.
    Danger,
    /// A recovery-available state — the theme recovery role.
    Recovery,
}

impl TrayBadgeTone {
    /// The badge fill colour this tone paints, from the active theme.
    fn fill(self, theme: &Theme) -> Color {
        let palette = theme.palette();
        Color::from(match self {
            Self::Accent => palette.accent,
            Self::Warning => palette.warning,
            Self::Danger => palette.danger,
            Self::Recovery => palette.recovery,
        })
    }
}

/// A [`TraySignal`]'s live-state badge: a small filled badge on the capsule's
/// top-trailing corner showing a count or an exclamation mark, its colour
/// encoding the dominant live state driving it (spec §11.27).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TrayBadge {
    content: TrayBadgeContent,
    tone: TrayBadgeTone,
}

impl TrayBadge {
    /// A badge with the given content and tone.
    #[must_use]
    pub fn new(content: TrayBadgeContent, tone: TrayBadgeTone) -> Self {
        Self { content, tone }
    }

    /// The badge's content.
    #[must_use]
    pub fn content(&self) -> TrayBadgeContent {
        self.content
    }

    /// The badge's tone.
    #[must_use]
    pub fn tone(&self) -> TrayBadgeTone {
        self.tone
    }
}

/// A compact live status capsule in the notification area (spec §11.27).
///
/// A tray signal is a small icon capsule seated in the bar
/// ([`PlateSeating::Bar`]): it wears no perimeter of its own and rests with no
/// plate at all, so the always-rightmost system control point reads as part of
/// the bar and states itself entirely through its own marks. Background work
/// adds a lower Heat Seam, a resource pressure adds a leading semantic rail, an
/// optional [`TrayBadge`] shows a live count or alert on the top-trailing
/// corner, and one or more alert states stack as severity-ordered mini Signal
/// Beads starting after it (so several states read at once without colour,
/// spec §15). On hover or keyboard focus it expands to a short instrument
/// readout — the state name, a count or value, and one primary safe action —
/// which the owner positions as a popup. It renders state and reports
/// [`TraySignalAction`]; the owner enforces authority (spec §13).
///
/// Equal signals draw the same pixels, so a tray may use `==` as its repaint
/// gate: the glyph, label, readout value, badge, action button, and visible
/// state — including the hover that expands the readout — all compare. The
/// pointer coordinate does not: it decides *which* region a press lands on,
/// and the hover it implies is already in `state`.
///
/// A signal is drawn on *two* surfaces, though, and they draw different parts
/// of it: the bar always shows the capsule, while the readout exists only
/// while expanded. A whole-signal `==` therefore over-reports for the bar — a
/// live value line the readout alone shows moves it on every reading. An owner
/// repainting the two surfaces separately gates the capsule on
/// [`draws_same_capsule`](Self::draws_same_capsule) instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TraySignal {
    capsule: TrayCapsule,
    label: String,
    value: Option<String>,
    action: Option<Button>,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
}

/// Everything the bar capsule draws, and nothing else.
///
/// [`TraySignal::render`] paints the capsule from this alone, so equality here
/// *is* "the capsule draws the same pixels" — and stays so as the control
/// grows, because the paint physically cannot reach the readout-only fields
/// and a new drawn field has to be added here to be drawn at all. That is the
/// same reasoning as [`RenderInvariant`], put the other way round: rather than
/// exempting one field from a whole-struct comparison, this names the subset
/// one of the two surfaces is a pure function of.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TrayCapsule {
    icon: IconKind,
    state: ControlState,
    badge: Option<TrayBadge>,
}

impl TraySignal {
    /// A calm tray signal with the given glyph and state-name label.
    #[must_use]
    pub fn new(icon: IconKind, label: impl Into<String>) -> Self {
        Self {
            capsule: TrayCapsule {
                icon,
                state: ControlState::idle(),
                badge: None,
            },
            label: label.into(),
            value: None,
            action: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
        }
    }

    /// Whether `self` and `other` would paint the identical bar capsule
    /// ([`render`](Self::render)).
    ///
    /// The capsule draws the glyph, the composed state, and the badge; the
    /// label, value, and action belong to the readout. An owner that presents
    /// the bar and the readout as separate surfaces gates the bar on this, so
    /// a reading that only moves the readout's value line does not repaint a
    /// capsule whose pixels are unchanged.
    #[must_use]
    pub fn draws_same_capsule(&self, other: &Self) -> bool {
        self.capsule == other.capsule
    }

    /// This signal with a readout count/value (e.g. a throughput or a count).
    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    /// This signal with the given composed state (drives seam/rail/beads).
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.capsule.state = state;
        self
    }

    /// This signal with a live-state badge on its top-trailing corner.
    #[must_use]
    pub fn with_badge(mut self, badge: TrayBadge) -> Self {
        self.capsule.badge = Some(badge);
        self
    }

    /// This signal with a primary safe action shown in its readout.
    #[must_use]
    pub fn with_action(mut self, action: Button) -> Self {
        self.action = Some(action);
        self
    }

    /// The signal's icon.
    ///
    /// Exposed so an owner resolves the shipped artwork for the kind the
    /// capsule actually draws, rather than naming that kind a second time.
    #[must_use]
    pub fn icon(&self) -> IconKind {
        self.capsule.icon
    }

    /// The signal's state-name label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The signal's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.capsule.state
    }

    /// Replace the signal's composed state.
    pub fn set_state(&mut self, state: ControlState) {
        self.capsule.state = state;
    }

    /// The signal's live-state badge, if any.
    #[must_use]
    pub fn badge(&self) -> Option<TrayBadge> {
        self.capsule.badge
    }

    /// Replace the signal's live-state badge.
    pub fn set_badge(&mut self, badge: Option<TrayBadge>) {
        self.capsule.badge = badge;
    }

    /// Set the signal's keyboard focus (focus also expands the readout).
    pub fn set_focused(&mut self, focused: bool) {
        self.capsule.state.focus.focused = focused;
    }

    /// Whether the readout is expanded — on hover or keyboard focus.
    #[must_use]
    pub fn is_expanded(&self) -> bool {
        self.capsule.state.pointer == PointerState::Hover || self.capsule.state.focus.focused
    }
}

impl TrayCapsule {
    /// The severity-ordered alert beads the capsule stacks, highest severity
    /// first: an authority denial, then a recovery/failed-closed state, then a
    /// validation warning, then a completion. Several states stack; none hides
    /// another.
    fn beads(&self, theme: &Theme) -> Vec<(Color, BeadShape)> {
        let palette = theme.palette();
        let mut beads = Vec::new();
        match self.state.disposition() {
            ControlDisposition::DeniedByAuthority => {
                beads.push((Color::from(palette.denied), BeadShape::Lock));
            }
            ControlDisposition::FailedClosed => {
                beads.push((Color::from(palette.recovery), BeadShape::Diamond));
            }
            _ => {}
        }
        if self.state.recovery != RecoveryState::None
            && self.state.disposition() != ControlDisposition::FailedClosed
        {
            beads.push((Color::from(palette.recovery), BeadShape::Diamond));
        }
        if self.state.validation == ValidationState::Warning {
            beads.push((Color::from(palette.warning), BeadShape::Diamond));
        }
        if matches!(self.state.activity, crate::state::ActivityState::Complete) {
            beads.push((Color::from(palette.success), BeadShape::Check));
        }
        beads
    }

    /// The live-state badge's fill colour, text, and `(w, h)` size within a
    /// `inner_w`×`inner_h` capsule, or `None` when there is no badge or it
    /// cannot fit. The badge is sized from the theme's bead metric scaled up
    /// enough to hold its text legibly, never wider or taller than the
    /// capsule it paints into.
    fn badge_paint(
        &self,
        inner_w: u32,
        inner_h: u32,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<(Color, String, u32, u32)> {
        let badge = self.badge?;
        let text = badge.content.text();
        let pad = (font.glyph_height() / 4).max(1);
        let h = scale
            .scale_length(theme.metrics().bead_size)
            .max(3)
            .max(font.glyph_height().saturating_add(pad))
            .min(inner_h);
        let w = font
            .text_width(&text)
            .saturating_add(pad.saturating_mul(2))
            .max(h)
            .min(inner_w);
        if w == 0 || h == 0 {
            return None;
        }
        Some((badge.tone.fill(theme), text, w, h))
    }

    /// The pixel side the capsule's icon paints at inside `bounds`, `0` when
    /// the bounds are off-surface or leave no room inside the plate border.
    fn icon_side(bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        let Some((_, _, w, h)) = surface_rect(bounds) else {
            return 0;
        };
        let border = plate_border(theme, scale);
        role_font(theme, scale, TextRole::Body)
            .glyph_height()
            .min(w.saturating_sub(border.saturating_mul(2)))
            .min(h.saturating_sub(border.saturating_mul(2)))
    }

    /// Paint the compact capsule into `surface` at `bounds`.
    fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let metrics = theme.metrics();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(metrics.control_corner_radius);
        let frame = resolve_frame(theme, ControlRole::Neutral, self.state);
        if let Some((plate, rim)) = frame.face(PlateSeating::Bar) {
            paint_plate(
                surface,
                (x, y, w, h),
                &PlateStyle {
                    radius,
                    border,
                    plate,
                    rim,
                    focused: frame.focused,
                    ring: Color::from(palette.rim_active),
                },
            );
        }

        let inner_x = x + border;
        let inner_y = y + border;
        let inner_w = w.saturating_sub(border.saturating_mul(2));
        let inner_h = h.saturating_sub(border.saturating_mul(2));
        if inner_w == 0 || inner_h == 0 {
            return;
        }

        // Leading pressure rail.
        if let Some(color) = resolve_rail(theme, self.state) {
            let rail_w = rail_thickness(theme, scale).min(inner_w);
            surface.fill_rect(inner_x, inner_y, rail_w, inner_h, color);
        }

        // The calm icon, centred.
        let side = Self::icon_side(bounds, scale, theme);
        if side > 0 {
            let ix = inner_x + (inner_w.saturating_sub(side)) / 2;
            let iy = inner_y + (inner_h.saturating_sub(side)) / 2;
            paint_icon_slot(
                surface,
                (ix, iy, side),
                self.icon,
                frame.label,
                artwork,
                FULL_COLOUR,
            );
        }

        // Lower Heat Seam for background work.
        let seam_h = seam_thickness(theme, scale).min(inner_h);
        let seam_w = seam_width(self.state.activity, inner_w);
        if seam_w > 0 {
            surface.fill_rect(
                inner_x,
                inner_y + inner_h - seam_h,
                seam_w,
                seam_h,
                Color::from(palette.accent),
            );
        }

        // The optional live-state badge on the top-trailing corner, then the
        // severity-ordered mini beads stacked leftward starting after it.
        let mut bx = inner_x + inner_w;
        if let Some((fill, text, badge_w, badge_h)) =
            self.badge_paint(inner_w, inner_h, scale, theme, font)
        {
            if bx >= inner_x + badge_w {
                bx = bx.saturating_sub(badge_w);
                paint_count_badge(
                    surface,
                    (bx, inner_y, badge_w, badge_h),
                    fill,
                    Color::from(palette.on_accent),
                    font,
                    &text,
                );
                bx = bx.saturating_sub((badge_h / 3).max(1));
            }
        }

        let beads = self.beads(theme);
        if !beads.is_empty() {
            let mini = (scale
                .scale_length(metrics.bead_size)
                .max(3)
                .saturating_mul(2)
                / 3)
            .max(2)
            .min(inner_w)
            .min(inner_h);
            let gap = (mini / 3).max(1);
            for (color, shape) in beads {
                if bx < inner_x + mini {
                    break;
                }
                bx = bx.saturating_sub(mini);
                paint_bead(surface, bx, inner_y, mini, color, shape);
                bx = bx.saturating_sub(gap);
            }
        }
    }
}

impl TraySignal {
    /// The pixel side the capsule's icon paints at inside `bounds`.
    ///
    /// This is the render geometry itself, exposed so an owner rasterising the
    /// shipped artwork produces it at exactly the size [`Self::render`] will
    /// place — the two can never disagree. `0` when the bounds are off-surface
    /// or leave no room inside the plate border.
    #[must_use]
    pub fn icon_side(&self, bounds: Rect, scale: Scale, theme: &Theme) -> u32 {
        TrayCapsule::icon_side(bounds, scale, theme)
    }

    /// Paint the compact capsule into `surface` at `bounds`.
    ///
    /// `artwork` is the shipped icon for [`Self::icon`], pre-rasterised by the
    /// owner at [`Self::icon_side`] (through its cache); `None` falls back to
    /// the built-in class glyph. The artwork is decoded and rasterised long
    /// before it reaches this call — a control never parses image bytes. Both
    /// go through the one shared icon slot every other bar control draws with.
    pub fn render(
        &self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        artwork: Option<IconPicture<'_>>,
    ) {
        self.capsule.render(surface, bounds, scale, theme, artwork);
    }

    /// The readout popup's preferred `(width, height)` in surface pixels — a
    /// state name, an optional value, and the primary action, so the owner can
    /// size the popup surface it hosts the readout in.
    #[must_use]
    pub fn readout_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let line = font.line_height();
        let mut text_w = font.text_width(&self.label);
        if let Some(value) = &self.value {
            text_w = text_w.max(font.text_width(value));
        }
        let action_h = self
            .action
            .as_ref()
            .map_or(0, |_| text_plate_height(theme, scale, TextRole::Body) + pad);
        let action_w = self.action.as_ref().map_or(0, |a| match a.content() {
            crate::button::ButtonContent::Label(t) => font.text_width(t) + pad.saturating_mul(4),
            _ => scale.scale_length(theme.metrics().control_height).max(1),
        });
        let value_h = self.value.as_ref().map_or(0, |_| line);
        let w = text_w.max(action_w).saturating_add(pad.saturating_mul(2));
        let h = line
            .saturating_add(value_h)
            .saturating_add(action_h)
            .saturating_add(pad.saturating_mul(2));
        (w.max(1), h.max(1))
    }

    /// The readout's primary-action button rectangle within `bounds`, shared by
    /// [`render_readout`](Self::render_readout) and pointer routing so the two
    /// never disagree.
    fn action_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.action.as_ref()?;
        let (x, y, w, h) = surface_rect(bounds)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let bh = text_plate_height(theme, scale, TextRole::Body);
        let (ix, iy, iw, ih) = inset(x, y, w, h, pad)?;
        if ih <= bh {
            return None;
        }
        Some(Rect::new(to_i32(ix), to_i32(iy + ih - bh), iw, bh))
    }

    /// Paint the expanded readout into `surface` at `bounds` — an elevated plate
    /// with the state name, the value, and the primary action. The owner calls
    /// this when [`is_expanded`](Self::is_expanded) is set, at the popup
    /// rectangle it sized from [`readout_size`](Self::readout_size).
    pub fn render_readout(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale.scale_length(theme.metrics().popup_corner_radius);
        let plate = (palette.surface_raised, ChromeLayer::Ground);
        let Some((ix, iy, iw, _)) =
            paint_surface_plate(surface, (x, y, w, h), (radius, border), theme, plate)
        else {
            return;
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_x = to_i32(ix + pad);
        let mut text_y = to_i32(iy + pad);
        let text_w = iw.saturating_sub(pad.saturating_mul(2));
        if text_w > 0 {
            let fitted = font.truncate_to_width(&self.label, text_w);
            font.draw_text(
                surface,
                text_x,
                text_y,
                fitted,
                foreground(theme, self.capsule.state.disposition()),
            );
            if let Some(value) = &self.value {
                text_y += to_i32(font.line_height());
                let fitted = font.truncate_to_width(value, text_w);
                font.draw_text(
                    surface,
                    text_x,
                    text_y,
                    fitted,
                    Color::from(palette.on_surface_muted),
                );
            }
        }
        if let (Some(action), Some(rect)) = (&self.action, self.action_rect(bounds, scale, theme)) {
            action.render(surface, rect, scale, theme);
        }
    }

    /// Feed a pointer event. `capsule_bounds` is the compact capsule; when the
    /// readout is expanded, `readout_bounds` is the popup rectangle. Hovering
    /// either keeps the readout open, and the readout's action reports
    /// [`TraySignalAction::Activated`].
    ///
    /// A hover change reports the capsule, and the readout as well whenever it
    /// has just opened or closed — a popup that vanished must be repainted or
    /// its pixels would stay on screen.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        capsule_bounds: Rect,
        readout_bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<TraySignalAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let expanded_before = self.is_expanded();
        let over_capsule = capsule_bounds.contains(*self.pointer);
        let over_readout = expanded_before && readout_bounds.contains(*self.pointer);
        let hover = if over_capsule || over_readout {
            PointerState::Hover
        } else {
            PointerState::None
        };
        damage::set(
            &mut self.capsule.state.pointer,
            hover,
            capsule_bounds,
            damage,
        );
        if self.is_expanded() != expanded_before {
            damage.add(readout_bounds);
        }
        if self.is_expanded() {
            if let Some(rect) = self.action_rect(readout_bounds, scale, theme) {
                if let Some(button) = self.action.as_mut() {
                    if button.on_pointer(event, rect, damage) == Some(ButtonAction::Activated) {
                        return Some(TraySignalAction::Activated);
                    }
                }
            }
        }
        None
    }

    /// The pointer has left the capsule and its readout: drop the hover, which
    /// collapses the readout unless the keyboard is holding it open. Reports
    /// whether anything changed, and damages what it changed.
    ///
    /// The expansion rule ([`is_expanded`](Self::is_expanded)) lives here, so
    /// its ending lives here too rather than being re-derived by a caller. A
    /// leave is not expressible as a motion elsewhere: the pointer is often
    /// still at the capsule's own coordinates when it stops resting on it —
    /// a window was raised over the bar — and hit-testing those coordinates
    /// would answer "still hovered" and leave an instrument readout floating
    /// over that window with nothing hovering it.
    pub fn pointer_left(
        &mut self,
        capsule_bounds: Rect,
        readout_bounds: Rect,
        damage: &mut Region,
    ) -> bool {
        let expanded_before = self.is_expanded();
        let mut state = self.capsule.state;
        if !damage::set(
            &mut state.pointer,
            PointerState::None,
            capsule_bounds,
            damage,
        ) {
            return false;
        }
        self.capsule.state = state;
        if self.is_expanded() != expanded_before {
            damage.add(readout_bounds);
        }
        true
    }

    /// Feed a key event; when focused (readout expanded) Space/Enter activates
    /// the primary action, reporting [`TraySignalAction::Activated`].
    pub fn on_key(&mut self, key: Key) -> Option<TraySignalAction> {
        if self.action.is_some() && key_activation(self.capsule.state, key) {
            return Some(TraySignalAction::Activated);
        }
        None
    }
}
