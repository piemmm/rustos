//! Decision-surface controls: [`Dialog`], [`Tooltip`], and [`HelpTip`]
//! (spec §11.24, §11.32).
//!
//! These are the desktop's *decision* surfaces — the modal choice, the
//! immediate-affordance hint, and the explanation of why an action is
//! unavailable or recommended. Each is drawn over the shared `crate::paint`
//! core (the one elevated-plate recipe) and the shared `lib/theme` tokens, so
//! nothing here restates a visual recipe. A dialog and a
//! help tip render state and emit typed actions; the owning service enforces
//! authority, and a capability denial reads distinctly from a disabled control
//! — never collapsed into a generic inactive look (spec §13).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::button::{Button, ButtonAction, ButtonContent};
use crate::paint::{
    foreground, grab_after, inset, line_budget, paint_plate, paint_run, plate_border,
    prose_measure, role_font, route_pointer, surface_rect, text_plate_height, to_i32, withheld,
    PlateStyle, TextBlock,
};
use crate::state::{ControlRole, RenderInvariant};

/// The most lines a tooltip's hint takes. A tooltip explains an immediate
/// affordance: past two lines it is an explanation, which is a help tip.
const TOOLTIP_MAX_LINES: usize = 2;

/// The most lines a help tip's reason takes — one refusal and what would
/// change it, not a document.
const HELPTIP_MAX_LINES: usize = 4;

/// A placed band's top-left corner in the unsigned coordinates a block
/// paints from. A band above the surface clamps to it, so a dialog scrolled
/// partly off-screen draws what is on it rather than nothing.
fn band_origin(band: Rect) -> (u32, u32) {
    (
        u32::try_from(band.left()).unwrap_or(0),
        u32::try_from(band.top()).unwrap_or(0),
    )
}

/// The natural width one action button needs: a labelled button fits its text
/// plus horizontal padding; a glyph button is a square of the control height.
/// One definition shared by the decision surfaces so their action rows lay out
/// identically.
fn button_width(button: &Button, scale: Scale, theme: &Theme, font: BitmapFont) -> u32 {
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    match button.content() {
        ButtonContent::Label(text) => font.text_width(text).saturating_add(pad.saturating_mul(4)),
        _ => scale.scale_length(theme.metrics().control_height).max(1),
    }
}

/// Lay a row of action buttons right-aligned along the bottom of `inner`,
/// returning one rect per button in index order (so the trailing button — by
/// convention the recommended/primary action — sits on the right edge). Shared
/// by the dialog and help-tip renderers and their pointer routing so the two
/// never disagree.
fn action_row_rects(
    buttons: &[Button],
    inner: (u32, u32, u32, u32),
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) -> Vec<Rect> {
    let mut rects = Vec::new();
    if buttons.is_empty() {
        return rects;
    }
    let (ix, iy, iw, ih) = inner;
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let bh = text_plate_height(theme, scale, TextRole::Body);
    if ih <= bh.saturating_add(pad) {
        return rects;
    }
    let top = iy + ih - bh;
    let left_bound = ix.saturating_add(pad);
    let widths: Vec<u32> = buttons
        .iter()
        .map(|b| button_width(b, scale, theme, font))
        .collect();
    // Place from the right edge leftwards, then reverse into index order.
    let mut right = ix.saturating_add(iw).saturating_sub(pad);
    let mut placed: Vec<Rect> = Vec::new();
    for width in widths.iter().rev() {
        let w = (*width).min(right.saturating_sub(left_bound));
        if w == 0 || right <= left_bound {
            break;
        }
        let bx = right.saturating_sub(w);
        placed.push(Rect::new(to_i32(bx), to_i32(top), w, bh));
        right = bx.saturating_sub(gap);
    }
    placed.reverse();
    // `placed` holds the buttons that fit, from the last index toward the
    // first; align it back to the leading indices so index 0 maps to rect 0.
    let start = buttons.len().saturating_sub(placed.len());
    for _ in 0..start {
        rects.push(Rect::new(0, 0, 0, 0));
    }
    rects.extend(placed);
    rects
}

/// Where a [`Dialog`]'s bands sit within its plate, resolved once per entry
/// point rather than re-derived by each.
struct Bands {
    /// The title line's baseline `y`.
    title_y: i32,
    /// The band the message is wrapped into, where it has one and there is
    /// room for a line of it.
    message: Option<Rect>,
    /// The band the owner draws its own content in.
    content: Rect,
    /// The band the inline reason is wrapped into, where it has one and
    /// there is room for a line of it.
    reason: Option<Rect>,
    /// The `x` every band's text begins at.
    content_left: u32,
    /// The width every band's text is fitted to.
    content_w: u32,
    /// The action buttons' rectangles, in action order.
    actions: Vec<Rect>,
}

// --- Dialog ------------------------------------------------------------

/// The outcome of feeding input to a [`Dialog`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DialogAction {
    /// The dialog action at `index` was activated; the owner performs it and
    /// enforces authority.
    ActionActivated {
        /// The zero-based index of the activated action button.
        index: usize,
    },
}

/// A modal decision surface (spec §11.24).
///
/// A dialog is an elevated plate carrying a title, a message, and a right-
/// aligned row of action [`Button`]s (the trailing one being, by convention,
/// the recommended action). Action Warmth is honest: an action is warm only
/// when its [`ControlRole`] is [`Recommended`](ControlRole::Recommended) or
/// [`Primary`](ControlRole::Primary); a destructive action carries
/// [`Destructive`](ControlRole::Destructive) and the caller sets its
/// confirmation posture, and a blocked action shows the Authority Mark rather
/// than a generic disabled look (spec §13). An optional inline reason explains
/// why an action is unavailable. The dialog routes input to its actions and
/// reports [`DialogAction`]; it performs no privileged work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dialog {
    title: String,
    message: Option<String>,
    reason: Option<String>,
    actions: Vec<Button>,
    /// The last pointer position, resolved against the action rects —
    /// hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The action the pointer was last over, so a motion sample reaches the
    /// action it left and the one it entered rather than the whole row.
    hovered: RenderInvariant<Option<usize>>,
    /// The action holding a press, which keeps receiving the stream wherever
    /// the pointer goes.
    armed: RenderInvariant<Option<usize>>,
}

impl Dialog {
    /// A dialog with the given title and no message or actions.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: None,
            reason: None,
            actions: Vec::new(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
        }
    }

    /// This dialog with a message body.
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// This dialog with an inline reason explaining why an action is blocked or
    /// recommended (concise, never a secret or capability token, spec §13).
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// This dialog with the given action buttons (trailing = recommended).
    #[must_use]
    pub fn with_actions(mut self, actions: Vec<Button>) -> Self {
        self.actions = actions;
        self
    }

    /// The dialog's title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The dialog's message body, if any.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The dialog's inline reason, if any.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// The dialog's action buttons.
    #[must_use]
    pub fn actions(&self) -> &[Button] {
        &self.actions
    }

    /// Mutable access to the action buttons (e.g. to update their state).
    pub fn actions_mut(&mut self) -> &mut [Button] {
        &mut self.actions
    }

    /// The inner content rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The most lines of prose a dialog gives its message or its reason when
    /// it measures itself.
    ///
    /// A dialog's sentences come from whichever program opened it, so this is
    /// a containment bound rather than a capacity: no message may grow a
    /// dialog past the screen it has to fit on. Past it the prose is elided
    /// and the reader is told so.
    const MAX_PROSE_LINES: usize = 8;

    /// How far below the plate's interior top the content band begins: the
    /// title, the message wrapped over `message_lines` where it has one, and
    /// the pads around them.
    fn head_height(&self, pad: u32, line: u32, message_lines: usize) -> u32 {
        let message = if self.message.is_some() {
            line.saturating_mul(u32::try_from(message_lines).unwrap_or(u32::MAX))
                .saturating_add(pad / 2)
        } else {
            0
        };
        pad.saturating_add(line)
            .saturating_add(message)
            .saturating_add(pad)
    }

    /// How far above the plate's interior bottom the content band ends: the
    /// action row, the inline reason wrapped over `reason_lines` where it has
    /// one, and the gap above.
    fn tail_height(&self, pad: u32, line: u32, action_h: u32, reason_lines: usize) -> u32 {
        let reason = if self.reason.is_some() {
            line.saturating_mul(u32::try_from(reason_lines).unwrap_or(u32::MAX))
                .saturating_add(pad / 2)
        } else {
            0
        };
        action_h.saturating_add(reason).saturating_add(pad / 2)
    }

    /// The plate height that leaves a content band exactly `content` pixels
    /// tall, for a dialog `width` pixels wide carrying this title, message,
    /// reason and actions.
    ///
    /// A dialog that carries a *form* rather than a sentence is sized by what
    /// the form measures, and this is what turns that figure into a window
    /// extent. It reads the same two spans the band is placed between, so a
    /// window sized by it seats its content exactly.
    ///
    /// The width is part of the question because the message and the reason
    /// are prose: they wrap, so how tall a dialog has to be depends on how
    /// wide it is. The prose is measured through the same block the paint
    /// draws, so the two cannot disagree on where a sentence ends.
    #[must_use]
    pub fn height_for_content(&self, content: u32, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let line = font.line_height();
        let content_w = Self::content_width(width, scale, theme);
        let action_h = if self.actions.is_empty() {
            0
        } else {
            text_plate_height(theme, scale, TextRole::Body)
        };
        let wanted = |text: &Option<String>| {
            text.as_ref().map_or(0, |text| {
                Self::prose(font, content_w, Self::MAX_PROSE_LINES, theme).line_count(text)
            })
        };
        plate_border(theme, scale)
            .saturating_mul(2)
            .saturating_add(self.head_height(pad, line, wanted(&self.message)))
            .saturating_add(content)
            .saturating_add(self.tail_height(pad, line, action_h, wanted(&self.reason)))
    }

    /// The span a dialog `width` pixels wide lays its own text and its
    /// owner's content across.
    ///
    /// An owner sizing a dialog has to measure its content *before* it knows
    /// the height — and therefore before [`content_rect`](Self::content_rect)
    /// can answer — so this is the band's width from the outer width alone.
    /// It is the same figure the band itself reports once the dialog is laid
    /// out, so measuring against it and then drawing into the band cannot
    /// disagree.
    #[must_use]
    pub fn content_width(width: u32, scale: Scale, theme: &Theme) -> u32 {
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        width
            .saturating_sub(plate_border(theme, scale).saturating_mul(2))
            .saturating_sub(pad.saturating_mul(2))
    }

    /// The block a dialog's prose is laid out in.
    fn prose(font: BitmapFont, width: u32, lines: usize, theme: &Theme) -> TextBlock {
        TextBlock::prose(
            font,
            width,
            lines.min(Self::MAX_PROSE_LINES),
            Color::from(theme.palette().on_surface_muted),
        )
    }

    /// Where every band of the dialog sits for `bounds`: the title, the
    /// optional message, the content band an owner draws its own form in, the
    /// optional inline reason, and the action row.
    ///
    /// One definition read by [`render`](Self::render),
    /// [`action_rects`](Self::action_rects) and
    /// [`content_rect`](Self::content_rect), so an owner that draws inside the
    /// dialog cannot place its content over the title or under the actions
    /// when the theme's type ladder or insets change.
    fn bands(&self, bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Option<Bands> {
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let content_left = ix.saturating_add(pad);
        let content_w = iw.saturating_sub(pad.saturating_mul(2));
        if content_w == 0 {
            return None;
        }
        let line = font.line_height();
        let title_y = to_i32(iy) + to_i32(pad);
        let actions = action_row_rects(&self.actions, (ix, iy, iw, ih), scale, theme, font);
        let action_h = actions
            .iter()
            .filter(|r| r.height > 0)
            .map(|r| r.height)
            .max()
            .unwrap_or(0);
        let action_top = actions.iter().filter(|r| r.height > 0).map(Rect::top).min();

        // The reason sits directly above the actions, and the message
        // directly below the title, so each is given the room between what
        // it is anchored to and the other's side of the plate: a paragraph
        // grows into the space the dialog has, never over the actions a
        // reader has to reach or the title that names them.
        let reason_bottom = action_top.unwrap_or(to_i32(iy + ih)) - to_i32(pad) / 2;
        let message_top = title_y + to_i32(line) + to_i32(pad) / 2;
        let reason = self.reason.as_ref().and_then(|reason| {
            let room = reason_bottom - message_top;
            Self::wrapped_band(
                font,
                (to_i32(content_left), reason_bottom, content_w),
                room,
                reason,
                theme,
            )
        });
        let message = self.message.as_ref().and_then(|message| {
            let floor = reason.map_or(reason_bottom, |band| band.top()) - to_i32(pad) / 2;
            let room = floor - message_top;
            let lines = u32::try_from(room.max(0)).unwrap_or(0);
            let block = Self::prose(font, content_w, line_budget(font, lines), theme);
            let height = block.height(message);
            (height > 0).then(|| Rect::new(to_i32(content_left), message_top, content_w, height))
        });

        // The content band runs from below the head the dialog draws to above
        // its tail, both measured by the same spans `height_for_content`
        // inverts — so a window sized to a form seats that form exactly.
        let lines_of = |band: Option<Rect>| band.map_or(0, |rect| line_budget(font, rect.height));
        let top = to_i32(iy) + to_i32(self.head_height(pad, line, lines_of(message)));
        let bottom =
            to_i32(iy)
                .saturating_add(to_i32(ih))
                .saturating_sub(to_i32(self.tail_height(
                    pad,
                    line,
                    action_h,
                    lines_of(reason),
                )));
        let content = Rect::new(
            to_i32(content_left),
            top,
            content_w,
            u32::try_from(bottom - top).unwrap_or(0),
        );
        Some(Bands {
            title_y,
            message,
            content,
            reason,
            content_left,
            content_w,
            actions,
        })
    }

    /// The band `text` wraps into when it hangs *above* a fixed edge: the
    /// lines it needs, laid out upward from `anchor`'s bottom and never
    /// taller than `room`.
    ///
    /// An inline reason is anchored to the action row it explains, so it has
    /// to grow away from it rather than push it down.
    fn wrapped_band(
        font: BitmapFont,
        anchor: (i32, i32, u32),
        room: i32,
        text: &str,
        theme: &Theme,
    ) -> Option<Rect> {
        let (x, bottom, width) = anchor;
        let lines = u32::try_from(room.max(0)).unwrap_or(0);
        let height = Self::prose(font, width, line_budget(font, lines), theme).height(text);
        (height > 0).then(|| Rect::new(x, bottom - to_i32(height), width, height))
    }

    /// The band an owner draws its own content in — beneath the message,
    /// above the inline reason and the action row — or [`None`] when the
    /// plate has no room for one.
    ///
    /// A dialog that carries a form rather than a sentence (the Date & Time
    /// window's field groups) lays that form out here, so it cannot drift out
    /// of the dialog's own bands.
    #[must_use]
    pub fn content_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        let font = role_font(theme, scale, TextRole::Body);
        self.bands(bounds, scale, theme, font)
            .map(|bands| bands.content)
            .filter(|rect| !rect.is_empty())
    }

    /// Paint the dialog into `surface` at `bounds` for the active theme.
    ///
    /// The content band is *not* painted: it belongs to the owner, which
    /// resolves it with [`content_rect`](Self::content_rect) and draws its own
    /// content there after this.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let Some(bands) = self.bands(bounds, scale, theme, font) else {
            return;
        };

        let run = font.elide_to_width(&self.title, bands.content_w);
        paint_run(
            surface,
            font,
            run,
            (to_i32(bands.content_left), bands.title_y),
            foreground(theme, crate::state::ControlDisposition::Interactive),
            None,
        );
        if let (Some(message), Some(band)) = (&self.message, bands.message) {
            Self::prose(font, band.width, line_budget(font, band.height), theme).paint(
                surface,
                message,
                band_origin(band),
            );
        }
        if let (Some(reason), Some(band)) = (&self.reason, bands.reason) {
            let mut block = Self::prose(font, band.width, line_budget(font, band.height), theme);
            block.color = Color::from(palette.warning);
            block.paint(surface, reason, band_origin(band));
        }
        for (button, rect) in self.actions.iter().zip(bands.actions) {
            if rect.width > 0 {
                button.render(surface, rect, scale, theme);
            }
        }
    }

    /// The surface-pixel rectangles of the action buttons for `bounds`, in
    /// action order (index `0` first).
    ///
    /// An empty vector when the plate has no drawable interior, and a
    /// zero-width rect for any trailing button that did not fit the action
    /// band. One definition so [`on_pointer`](Self::on_pointer) and an owner
    /// that routes clicks through its own press-point hit-test resolve the
    /// exact same button geometry rather than each re-deriving it.
    #[must_use]
    pub fn action_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let font = role_font(theme, scale, TextRole::Body);
        self.bands(bounds, scale, theme, font)
            .map(|bands| bands.actions)
            .unwrap_or_default()
    }

    /// Route a pointer event to the actions it concerns; one that completes a
    /// click reports [`DialogAction::ActionActivated`].
    ///
    /// One hit test decides where the pointer is; the event then reaches only
    /// the action it left, the action it entered, and any action holding a
    /// press.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<DialogAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.action_rects(bounds, scale, theme);
        let over = rects.iter().position(|r| r.contains(*self.pointer));
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);

        let mut action = None;
        for i in route.into_iter().flatten() {
            let (Some(button), Some(rect)) = (self.actions.get_mut(i), rects.get(i)) else {
                continue;
            };
            if button.on_pointer(event, *rect, damage) == Some(ButtonAction::Activated)
                && action.is_none()
            {
                action = Some(DialogAction::ActionActivated { index: i });
            }
        }
        action
    }

    /// Feed a key event; a focused action activated with Space/Enter reports
    /// [`DialogAction::ActionActivated`].
    pub fn on_key(&mut self, key: Key) -> Option<DialogAction> {
        let mut action = None;
        for (i, button) in self.actions.iter_mut().enumerate() {
            if button.on_key(key) == Some(ButtonAction::Activated) && action.is_none() {
                action = Some(DialogAction::ActionActivated { index: i });
            }
        }
        action
    }
}

// --- Tooltip -----------------------------------------------------------

/// A short, anchored affordance hint (spec §11.32).
///
/// A tooltip is a small elevated plate carrying one short line that explains
/// the immediate affordance of the control it is anchored to. It is
/// non-interactive: the owner shows and hides it, positioning it beside its
/// anchor. Its text must stay concise and must never carry secrets or
/// capability tokens (spec §13); that is the caller's responsibility, the
/// tooltip simply draws the string it is given.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tooltip {
    text: String,
}

impl Tooltip {
    /// A tooltip carrying the given short text.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// The tooltip's text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The tooltip's preferred `(width, height)` in surface pixels, so the
    /// owner can size the popup surface it anchors the tooltip in.
    ///
    /// A hint longer than the prose measure wraps instead of growing a plate
    /// across the screen, so the height is however many lines it takes and
    /// the width is the widest of them — a short hint stays a short hint.
    #[must_use]
    pub fn preferred_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let font = role_font(theme, scale, TextRole::Body);
        let margin = Self::margin(scale, theme);
        let block = Self::block(font, prose_measure(font), theme);
        let w = block
            .measured_width(&self.text)
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        let h = block
            .height(&self.text)
            .max(font.line_height())
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        (w, h)
    }

    /// The inset between the plate's edge and its text.
    fn margin(scale: Scale, theme: &Theme) -> u32 {
        plate_border(theme, scale)
            .saturating_add(scale.scale_length(theme.metrics().control_inset).max(1))
    }

    /// The block the hint is laid out in: prose in the ordinary foreground,
    /// over as many lines as the measure needs.
    fn block(font: BitmapFont, width: u32, theme: &Theme) -> TextBlock {
        TextBlock::prose(
            font,
            width,
            TOOLTIP_MAX_LINES,
            foreground(theme, crate::state::ControlDisposition::Interactive),
        )
    }

    /// Paint the tooltip into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let margin = Self::margin(scale, theme);
        let text_w = w.saturating_sub(margin.saturating_mul(2));
        let room = h.saturating_sub(margin.saturating_mul(2));
        if text_w > 0 {
            let mut block = Self::block(font, text_w, theme);
            block.lines = block.lines.min(line_budget(font, room));
            block.paint(surface, &self.text, (x + margin, y + margin));
        }
    }
}

// --- HelpTip -----------------------------------------------------------

/// The outcome of feeding input to a [`HelpTip`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HelpTipAction {
    /// The one safe next-step action was activated; the owner performs it.
    NextStep,
}

/// An explanation of why an action is unavailable or recommended (spec §11.32).
///
/// A help tip is an elevated plate carrying one reason line and, optionally, one
/// safe next-step [`Button`]. It is the surface that explains a capability
/// denial or a recommendation in concise, user-facing terms — never a secret or
/// a capability token (spec §13). Its reason takes a warning tone; its role
/// tints the reason toward recommendation or denial. It routes input to the
/// next-step action and reports [`HelpTipAction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpTip {
    reason: String,
    role: ControlRole,
    step: Option<Button>,
}

impl HelpTip {
    /// A help tip carrying the given reason and no next step.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            role: ControlRole::Neutral,
            step: None,
        }
    }

    /// This help tip with a non-default role (drives the reason's tone).
    #[must_use]
    pub fn with_role(mut self, role: ControlRole) -> Self {
        self.role = role;
        self
    }

    /// This help tip with one safe next-step action.
    #[must_use]
    pub fn with_step(mut self, step: Button) -> Self {
        self.step = Some(step);
        self
    }

    /// The help tip's reason text.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// The help tip's next-step action, if any.
    #[must_use]
    pub fn step(&self) -> Option<&Button> {
        self.step.as_ref()
    }

    /// The reason's tint: a recommendation reads accent, a denial reads denied,
    /// otherwise a caution warning tone.
    fn reason_color(&self, theme: &Theme) -> Color {
        let palette = theme.palette();
        Color::from(match self.role {
            ControlRole::Recommended | ControlRole::Primary => palette.accent,
            ControlRole::Destructive => palette.danger,
            _ => palette.warning,
        })
    }

    /// The help tip's preferred `(width, height)` in surface pixels.
    ///
    /// The reason is prose and wraps at the prose measure, so an explanation
    /// of a refusal is a paragraph the reader can read rather than a line cut
    /// off before it says what to do.
    #[must_use]
    pub fn preferred_size(&self, scale: Scale, theme: &Theme) -> (u32, u32) {
        let font = role_font(theme, scale, TextRole::Body);
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let margin = plate_border(theme, scale).saturating_add(pad);
        let block = self.block(font, prose_measure(font), theme);
        let mut text_w = block.measured_width(&self.reason);
        let step_h = self
            .step
            .as_ref()
            .map_or(0, |_| text_plate_height(theme, scale, TextRole::Body) + pad);
        if let Some(step) = &self.step {
            text_w = text_w.max(button_width(step, scale, theme, font));
        }
        let w = text_w.saturating_add(margin.saturating_mul(2)).max(1);
        let h = block
            .height(&self.reason)
            .max(font.line_height())
            .saturating_add(step_h)
            .saturating_add(margin.saturating_mul(2))
            .max(1);
        (w, h)
    }

    /// The block the reason is laid out in: prose in the tint its role calls
    /// for, over as many lines as the measure needs.
    fn block(&self, font: BitmapFont, width: u32, theme: &Theme) -> TextBlock {
        TextBlock::prose(font, width, HELPTIP_MAX_LINES, self.reason_color(theme))
    }

    /// The inner content rectangle (inside the rim) as surface pixels.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The next-step button rectangle within `bounds`, shared by rendering and
    /// pointer routing so the two never disagree.
    fn step_rect(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<Rect> {
        let step = self.step.as_ref()?;
        let (ix, iy, iw, ih) = Self::inner(bounds, scale, theme)?;
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let bh = text_plate_height(theme, scale, TextRole::Body);
        if ih <= bh.saturating_add(pad) {
            return None;
        }
        let w = button_width(step, scale, theme, font)
            .min(iw.saturating_sub(pad.saturating_mul(2)))
            .max(1);
        Some(Rect::new(to_i32(ix + pad), to_i32(iy + ih - bh), w, bh))
    }

    /// Paint the help tip into `surface` at `bounds` for the active theme.
    pub fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        if withheld(surface, bounds) {
            return;
        }
        let font = role_font(theme, scale, TextRole::Body);
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let palette = theme.palette();
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().popup_corner_radius)
            .min(w / 2)
            .min(h / 2);
        paint_plate(
            surface,
            (x, y, w, h),
            &PlateStyle {
                radius,
                border,
                plate: Color::from(palette.surface_raised),
                rim: Color::from(palette.rim),
                focused: false,
                ring: Color::from(palette.rim_active),
            },
        );
        let Some((ix, iy, iw, ih)) = Self::inner(bounds, scale, theme) else {
            return;
        };
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let text_w = iw.saturating_sub(pad.saturating_mul(2));
        let step_rect = self.step_rect(bounds, scale, theme, font);
        if text_w > 0 {
            // The reason stops a pad short of the next step it explains, so
            // a long refusal never runs under the button that answers it.
            let floor = step_rect
                .map_or(ih, |rect| {
                    u32::try_from(rect.top()).unwrap_or(0).saturating_sub(iy)
                })
                .saturating_sub(pad.saturating_mul(2));
            let mut block = self.block(font, text_w, theme);
            block.lines = block.lines.min(line_budget(font, floor));
            block.paint(surface, &self.reason, (ix + pad, iy + pad));
        }
        if let (Some(step), Some(rect)) = (&self.step, step_rect) {
            step.render(surface, rect, scale, theme);
        }
    }

    /// Feed a pointer event; the next-step action completing a click reports
    /// [`HelpTipAction::NextStep`].
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<HelpTipAction> {
        let font = role_font(theme, scale, TextRole::Body);
        let rect = self.step_rect(bounds, scale, theme, font)?;
        let step = self.step.as_mut()?;
        (step.on_pointer(event, rect, damage) == Some(ButtonAction::Activated))
            .then_some(HelpTipAction::NextStep)
    }

    /// Feed a key event; a focused next-step action activated with Space/Enter
    /// reports [`HelpTipAction::NextStep`].
    pub fn on_key(&mut self, key: Key) -> Option<HelpTipAction> {
        let step = self.step.as_mut()?;
        (step.on_key(key) == Some(ButtonAction::Activated)).then_some(HelpTipAction::NextStep)
    }
}
