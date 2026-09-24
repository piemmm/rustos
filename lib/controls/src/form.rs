//! The form-field family: [`FieldRow`] and [`FieldGroup`], the shape every
//! settings surface has (`plans/GUI-CONTROLS-DESIGN.md` §11.41,
//! `docs/src/lib/controls.md`).
//!
//! A row is one setting: a label, an optional description line, and a trailing
//! slot holding one real [`Toggle`], [`ComboBox`], [`Slider`], [`TextField`],
//! [`Button`], [`FlagSet`] of checkboxes, read-only reading, or stated absence
//! of one. It composes the row chrome [`ListRow`](crate::collection::ListRow)
//! and [`TableRow`](crate::collection::TableRow) paint and restates neither
//! that nor any control. A group is the captioned plate those rows sit on,
//! resolving one slot column so every control in it begins at the same x.
//!
//! Three obligations fall on a caller, and each is what stops a settings pane
//! lying about the machine:
//!
//! - **State a refusal on the row, not the control.**
//!   [`FieldRow::set_state`] shares the row's enablement, authority and
//!   validation with the control in its slot — exactly what decides
//!   actionability — so a denied setting cannot hold an actionable control.
//!   A **disabled** row mutes, a **denied** one wears the Authority
//!   Mark in a bead band that is reserved either way — so it never moves its
//!   own control — and [`FieldControl::Unmeasured`] states why there is no
//!   reading rather than drawing a blank a reader would take for one.
//! - **Give room away in the order the reader needs it**, which the family
//!   does for you: slot, label, description. See [`FieldRow::render`].
//! - **Place the choice popup yourself.** An expanded [`ComboBox`] draws above
//!   every group, so a row cannot paint it — the group's later rows would
//!   cover it. Read [`FieldGroup::popup_anchor`], place the list, hand it back
//!   through [`FieldLayout::with_popup`], and paint it with
//!   [`FieldGroup::render_popup`] once every group is drawn.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::{TextRole, Theme};

use crate::button::Button;
use crate::combo::{ComboAction, ComboBox};
use crate::damage;
use crate::metric::StatusPill;
use crate::paint::{
    bead_band, centred_text_y, foreground, grab_after, inset, line_budget, paint_row, paint_run,
    paint_surface_plate, plate_border, role_font, route_pointer, row_content_span,
    row_width_for_content, surface_rect, text_plate_height, to_i32, ChromeLayer, TextBlock,
};
use crate::selector::{box_side, Checkbox, SelectorAction, Toggle};
use crate::state::{ControlState, PointerState, RenderInvariant, SelectionState};
use crate::text::{TextAction, TextField};
use crate::value::{Slider, SliderAction};

/// What a [`FieldRow`]'s trailing slot holds: one control, one reading, or a
/// stated absence of one.
///
/// These are the settables a pane actually has — a boolean, a few independent
/// flags, a choice, a bounded value, a string, a command — plus the two
/// read-only forms. A row holds exactly one, because a setting with two
/// controls is two settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldControl {
    /// A boolean setting.
    Toggle(Toggle),
    /// A small set of independent flags, such as the read, write and execute
    /// bits of one permission class.
    Flags(FlagSet),
    /// A one-of-several setting.
    Combo(ComboBox),
    /// A bounded value.
    Slider(Slider),
    /// A free-text setting.
    Text(TextField),
    /// A command the row offers about its setting (*Choose Picture…*,
    /// *Lock Now*).
    Button(Button),
    /// A read-only measurement, already formatted by the owner.
    Reading(String),
    /// No measurement, and the owner's statement of why — never a blank, a
    /// dash, or a fabricated zero.
    Unmeasured(String),
}

/// The outcome of feeding input to a [`FieldRow`].
///
/// Each variant is the request the slot's own control made, so the owner
/// validates and commits it exactly as it would from the control alone. A
/// [`FieldRow`] commits nothing itself and enforces no authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldAction {
    /// The [`Toggle`] slot requests its value become `on`.
    Set {
        /// The requested new on/off value.
        on: bool,
    },
    /// The [`FlagSet`] slot's flag at `index` requests its value become `on`.
    /// Only that flag is named, so the owner commits the one flag the reader
    /// changed and leaves its siblings as they are.
    SetFlag {
        /// The zero-based index of the flag, in the order the set was built.
        index: usize,
        /// The requested new on/off value.
        on: bool,
    },
    /// The [`ComboBox`] slot selected the choice at `index`.
    Selected {
        /// The zero-based index of the selected choice.
        index: usize,
    },
    /// The [`ComboBox`] slot expanded or collapsed its list; the owner places
    /// and paints the popup while it is open.
    Choices {
        /// Whether the list is now expanded.
        expanded: bool,
    },
    /// The [`Slider`] slot requests its value become `permille` (`0..=1000`)
    /// while the interaction continues: apply it live, and nothing more.
    SetValue {
        /// The requested new value, in permille.
        permille: u16,
    },
    /// The [`Slider`] slot's interaction finished at `permille`. A durable
    /// change — posting a document, writing a store — is made here and
    /// nowhere else, because acting on every [`SetValue`](Self::SetValue)
    /// means one write per pointer sample of a drag.
    Settled {
        /// The value the interaction settled on, in permille.
        permille: u16,
    },
    /// The [`TextField`] slot reported an edit, a submission, or a
    /// cancellation; the owner reads the text and validates it.
    Text(TextAction),
    /// The [`Button`] slot was activated.
    Activated,
}

/// The outcome of feeding input to a [`FieldGroup`]: which row acted, and what
/// it asked for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldGroupAction {
    /// The zero-based index of the row the action came from.
    pub row: usize,
    /// What that row's slot requested.
    pub action: FieldAction,
}

/// Where a form surface is drawn, resolved by its owner.
///
/// The three facts a row or group needs beyond the theme travel together
/// because they are resolved together and every entry point needs all of them:
/// the surface's own rectangle, the shared slot column its controls line up
/// in, and where an expanded choice list has been placed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FieldLayout {
    /// The surface's own rectangle: a [`FieldGroup`]'s plate, or one
    /// [`FieldRow`]'s row.
    pub bounds: Rect,
    /// The width, in surface pixels, of the trailing slot every row's control
    /// is drawn in. A group resolves its own with
    /// [`FieldGroup::slot_column`]; a pane that wants every group's controls
    /// on one x resolves the widest across its groups and passes that.
    pub column: u32,
    /// Where the owner has placed the choice list of an expanded
    /// [`ComboBox`] slot, or [`Rect::EMPTY`] while none is open.
    pub popup: Rect,
}

impl FieldLayout {
    /// A layout with no popup placed.
    #[must_use]
    pub const fn new(bounds: Rect, column: u32) -> Self {
        Self {
            bounds,
            column,
            popup: Rect::EMPTY,
        }
    }

    /// This layout with an expanded slot's choice list placed at `popup`.
    #[must_use]
    pub const fn with_popup(mut self, popup: Rect) -> Self {
        self.popup = popup;
        self
    }
}

/// Test-only: the span a row laid out for `layout` gives its own words,
/// which must be the span its group measured the row's height against.
#[cfg(test)]
pub(crate) fn debug_row_text_span(layout: FieldLayout, scale: Scale, theme: &Theme) -> u32 {
    FieldRow::text_span(layout, scale, theme).map_or(0, |(_, span)| span)
}

/// The most lines a row's description takes. A setting's elaboration is a
/// sentence about that setting; past three lines it belongs in the group's
/// footnote or the app's help.
const MAX_DESCRIPTION_LINES: usize = 3;

/// The most lines a group's footnote takes — the consequence of the settings
/// above it, not a document.
const MAX_FOOTNOTE_LINES: usize = 3;

/// The widest a slot may be within a row content span of `content`: half.
///
/// A label the reader cannot read names a setting they cannot find, so it
/// keeps half the span whatever column a caller asks for. Both
/// [`FieldRow::slot_rect`] and [`FieldGroup::slot_column`] clamp here, so a
/// row and its group cannot disagree about the column.
#[must_use]
const fn slot_ceiling(content: u32) -> u32 {
    content / 2
}

/// What is left of a row `content` span once its slot `column` and the `gap`
/// before it are served: the span the row's own words are laid out across.
///
/// One definition, read by the row that paints its label and description and
/// by the group that measures the heights they need, so the column a row
/// wraps into and the height reserved for it come from the same arithmetic.
#[must_use]
fn words_span(content: u32, column: u32, gap: u32) -> u32 {
    content.saturating_sub(column.min(slot_ceiling(content)).saturating_add(gap))
}

/// A [`FieldRow`]'s one child: the control in its slot. The row routes a
/// pointer to it through the same grab rule a container uses for the child
/// under the pointer, rather than a second latch of its own.
const SLOT: usize = 0;

impl FieldControl {
    /// The width this control needs, or [`None`] when it takes whatever
    /// column it is given.
    ///
    /// A boolean, a set of flags, a command, a choice and a reading are as
    /// wide as their own content; a bounded value and a free-text entry are
    /// as wide as the surface can afford, because a cramped slider cannot be
    /// aimed and a cramped entry cannot be read.
    #[must_use]
    fn wanted_width(&self, scale: Scale, theme: &Theme) -> Option<u32> {
        let font = role_font(theme, scale, TextRole::Body);
        match self {
            FieldControl::Toggle(toggle) => Some(toggle.measured_width(scale, theme)),
            FieldControl::Flags(flags) => Some(flags.measured_width(scale, theme)),
            FieldControl::Combo(combo) => Some(combo.measured_width(scale, theme)),
            FieldControl::Button(button) => Some(button.measured_width(scale, theme)),
            FieldControl::Reading(text) | FieldControl::Unmeasured(text) => {
                Some(font.text_width(text))
            }
            FieldControl::Slider(_) | FieldControl::Text(_) => None,
        }
    }

    /// The rectangle this control is actually drawn in inside the shared
    /// `slot`: the whole column for a control that fills one, its own wanted
    /// width from the column's leading edge otherwise.
    ///
    /// Every control in a group therefore begins at the same x — that is what
    /// makes the group read as a table of settings — while a narrow one leaves
    /// the trailing remainder alone rather than stretching across it. One
    /// definition serves the paint and the hit test, so a press can never land
    /// beside a control that only *looked* that wide.
    #[must_use]
    fn drawn_rect(&self, slot: Rect, scale: Scale, theme: &Theme) -> Rect {
        match self.wanted_width(scale, theme) {
            Some(want) => Rect::new(slot.left(), slot.top(), want.min(slot.width), slot.height),
            None => slot,
        }
    }

    /// Share the row's enablement, authority and validation with the control,
    /// leaving its own pointer, focus and selection alone.
    ///
    /// Those three are exactly what [`ControlState::disposition`] reads, so the
    /// control is actionable precisely when the row is — a row whose check is
    /// pending or whose value the pane found invalid cannot take a new one
    /// either. The row *is* the setting; the control is only its editor.
    fn adopt_authority(&mut self, row: ControlState) {
        let apply = |state: ControlState| {
            let mut next = state;
            next.enabled = row.enabled;
            next.authority = row.authority;
            next.validation = row.validation;
            next
        };
        match self {
            FieldControl::Toggle(c) => c.set_state(apply(c.state())),
            FieldControl::Flags(c) => {
                for flag in &mut c.flags {
                    flag.set_state(apply(flag.state()));
                }
            }
            FieldControl::Combo(c) => c.set_state(apply(c.state())),
            FieldControl::Slider(c) => c.set_state(apply(c.state())),
            FieldControl::Text(c) => c.set_state(apply(c.state())),
            FieldControl::Button(c) => c.set_state(apply(c.state())),
            FieldControl::Reading(_) | FieldControl::Unmeasured(_) => {}
        }
    }

    /// Give the control keyboard focus, if it takes any.
    ///
    /// A reading takes none, which is why [`FieldRow::set_focused`] keeps the
    /// ring on the row itself for those two arms.
    fn set_focused(&mut self, focused: bool) -> bool {
        match self {
            FieldControl::Toggle(c) => c.set_focused(focused),
            FieldControl::Flags(c) => c.set_focused(focused),
            FieldControl::Combo(c) => c.set_focused(focused),
            FieldControl::Slider(c) => c.set_focused(focused),
            FieldControl::Text(c) => c.set_focused(focused),
            FieldControl::Button(c) => c.set_focused(focused),
            FieldControl::Reading(_) | FieldControl::Unmeasured(_) => return false,
        }
        true
    }

    /// Paint the control into the rectangle [`Self::drawn_rect`] resolves for
    /// `slot`.
    ///
    /// `reading` is the colour the row resolved for its own label, which a
    /// read-only reading takes too, so a disabled setting's figure mutes
    /// alongside the words naming it. A drawn control resolves its own.
    fn render(
        &self,
        surface: &mut Surface,
        slot: Rect,
        scale: Scale,
        theme: &Theme,
        reading: Color,
    ) {
        let rect = self.drawn_rect(slot, scale, theme);
        match self {
            FieldControl::Toggle(c) => c.render(surface, rect, scale, theme),
            FieldControl::Flags(c) => c.render(surface, rect, scale, theme),
            FieldControl::Combo(c) => c.render(surface, rect, scale, theme),
            FieldControl::Slider(c) => c.render(surface, rect, scale, theme),
            FieldControl::Text(c) => c.render(surface, rect, scale, theme),
            FieldControl::Button(c) => c.render(surface, rect, scale, theme),
            FieldControl::Reading(text) => {
                Self::paint_words(surface, text, rect, scale, theme, reading);
            }
            // A stated absence is quiet whatever the row's disposition: it is
            // not a reading, and must never be mistaken for one.
            FieldControl::Unmeasured(text) => {
                let quiet = Color::from(theme.palette().on_surface_muted);
                Self::paint_words(surface, text, rect, scale, theme, quiet);
            }
        }
    }

    /// Draw a read-only reading, or the statement that there is none, at the
    /// slot's leading edge on the label's own line.
    ///
    /// A run too long for its slot ends in the shared elision mark, so a cut
    /// reading never reads as a shorter one.
    fn paint_words(
        surface: &mut Surface,
        text: &str,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        color: Color,
    ) {
        let Some((x, y, w, h)) = surface_rect(rect) else {
            return;
        };
        let font = role_font(theme, scale, TextRole::Body);
        let run = font.elide_to_width(text, w);
        paint_run(
            surface,
            font,
            run,
            (to_i32(x), centred_text_y(font, y, h)),
            color,
            None,
        );
    }
}

/// A small set of independent flags on one line — the read, write and execute
/// bits of one permission class, say — each a labelled [`Checkbox`].
///
/// The set owns only the layout that seats its flags side by side, which flag
/// the pointer is over, which holds a press, and which the keyboard rests on;
/// the box, the press, the focus ring, the disabled look and the Authority
/// Mark are each flag's own. Left and Right move the keyboard between flags,
/// clamping at either end; Space and Enter toggle the one it rests on. The
/// owner learns which flag changed from [`FieldAction::SetFlag`], which names
/// it by index, and commits that value alone.
///
/// Equal sets draw the same pixels, so a host may use `==` as its repaint
/// gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlagSet {
    flags: Vec<Checkbox>,
    /// The flag the keyboard rests on.
    focus: usize,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The flag the pointer was last over.
    hovered: RenderInvariant<Option<usize>>,
    /// The flag holding a press, which keeps receiving the stream wherever
    /// the pointer goes.
    armed: RenderInvariant<Option<usize>>,
}

impl FlagSet {
    /// A set of `flags`, laid out in the order given, with the keyboard on
    /// the first.
    #[must_use]
    pub fn new(flags: Vec<Checkbox>) -> Self {
        Self {
            flags,
            focus: 0,
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
        }
    }

    /// This set with the keyboard resting on flag `index`, clamped to the
    /// last flag, so an owner that rebuilds the set from its model keeps the
    /// reader's place in it.
    #[must_use]
    pub fn with_focus(mut self, index: usize) -> Self {
        self.focus = index.min(self.flags.len().saturating_sub(1));
        self
    }

    /// The flags, in layout order.
    #[must_use]
    pub fn flags(&self) -> &[Checkbox] {
        &self.flags
    }

    /// The flag the keyboard rests on.
    #[must_use]
    pub fn focus(&self) -> usize {
        self.focus
    }

    /// Set flag `index` on or off, for the owner to commit a value the set
    /// reported as a request; an out-of-range index changes nothing.
    pub fn set_on(&mut self, index: usize, on: bool) {
        if let Some(flag) = self.flags.get_mut(index) {
            flag.set_selection(if on {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            });
        }
    }

    /// The width this set needs at `scale` to seat every flag whole.
    #[must_use]
    pub fn measured_width(&self, scale: Scale, theme: &Theme) -> u32 {
        self.natural_widths(scale, theme)
            .fold(0, u32::saturating_add)
    }

    /// The room each flag keeps after its label: the theme's control gap, and
    /// never less than the Signal Bead band, because a checkbox draws its bead
    /// in its trailing corner and a flag marked denied would otherwise stamp
    /// it over the end of its own label.
    fn trail(scale: Scale, theme: &Theme) -> u32 {
        let band = text_plate_height(theme, scale, TextRole::Body);
        scale
            .scale_length(theme.metrics().control_gap)
            .max(bead_band(theme, scale, band))
            .max(1)
    }

    /// Each flag's own width: its checkbox's, and the room after it.
    fn natural_widths<'a>(
        &'a self,
        scale: Scale,
        theme: &'a Theme,
    ) -> impl Iterator<Item = u32> + 'a {
        let trail = Self::trail(scale, theme);
        self.flags
            .iter()
            .map(move |flag| flag.measured_width(scale, theme).saturating_add(trail))
    }

    /// Where each flag is drawn within `bounds`, in layout order.
    ///
    /// Each takes its own width from the leading edge while the set has room.
    /// When it does not, every box keeps its size and the labels share what
    /// is left in proportion to what each wanted, so it is words that elide —
    /// through the checkbox's own mark — and never a box that goes; only a
    /// slot too narrow for the boxes themselves narrows them, evenly. This is
    /// the one layout the paint and both hit tests read, so a press can never
    /// land on a flag drawn elsewhere.
    fn flag_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let Some((x, y, w, h)) = surface_rect(bounds) else {
            return Vec::new();
        };
        let natural: Vec<u32> = self.natural_widths(scale, theme).collect();
        let total = natural.iter().copied().fold(0u32, u32::saturating_add);
        if total == 0 || w == 0 || h == 0 {
            return Vec::new();
        }
        let side = box_side(scale, theme);
        let count = u32::try_from(natural.len()).unwrap_or(u32::MAX);
        let boxes = side.saturating_mul(count);
        let width_of = |want: u32| -> u32 {
            if total <= w {
                return want;
            }
            if boxes <= w {
                let room = u64::from(w - boxes);
                let words = u64::from(total.saturating_sub(boxes)).max(1);
                let label = u64::from(want.saturating_sub(side)) * room / words;
                return side.saturating_add(u32::try_from(label).unwrap_or(0));
            }
            w / count.max(1)
        };
        let last = natural.len().saturating_sub(1);
        let mut left = x;
        let mut rects = Vec::with_capacity(natural.len());
        for (index, want) in natural.iter().enumerate() {
            // The last flag takes the rounding remainder, so a narrowed set
            // still fills its slot to the pixel.
            let width = if total > w && index == last {
                x.saturating_add(w).saturating_sub(left)
            } else {
                width_of(*want)
            };
            rects.push(Rect::new(to_i32(left), to_i32(y), width, h));
            left = left.saturating_add(width);
        }
        rects
    }

    /// The rectangle flag `index` is drawn and pressed in within `bounds`,
    /// or [`None`] past the last flag.
    #[must_use]
    pub fn flag_rect(
        &self,
        index: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.flag_rects(bounds, scale, theme).get(index).copied()
    }

    /// The flag under `point` in `bounds`, if any.
    #[must_use]
    pub fn flag_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> Option<usize> {
        self.flag_rects(bounds, scale, theme)
            .iter()
            .position(|rect| rect.contains(point))
    }

    /// Give the set keyboard focus, which rings the flag it rests on.
    fn set_focused(&mut self, focused: bool) {
        let focus = self.focus;
        for (index, flag) in self.flags.iter_mut().enumerate() {
            flag.set_focused(focused && index == focus);
        }
    }

    /// Paint every flag into its rectangle within `bounds`.
    fn render(&self, surface: &mut Surface, bounds: Rect, scale: Scale, theme: &Theme) {
        for (flag, rect) in self.flags.iter().zip(self.flag_rects(bounds, scale, theme)) {
            flag.render(surface, rect, scale, theme);
        }
    }

    /// Route a pointer event to the flags it concerns — the one it left, the
    /// one it entered, and any holding a press — and report what one of them
    /// asked for.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.flag_rects(bounds, scale, theme);
        let over = rects.iter().position(|rect| rect.contains(*self.pointer));
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);
        let mut fired = None;
        for index in route.into_iter().flatten() {
            let (Some(flag), Some(rect)) = (self.flags.get_mut(index), rects.get(index)) else {
                continue;
            };
            if let Some(SelectorAction::Set { on }) = flag.on_pointer(event, *rect, damage) {
                fired = Some(FieldAction::SetFlag { index, on });
            }
        }
        fired
    }

    /// Feed a key event: Left and Right move the keyboard between flags,
    /// clamping at either end, and every other key goes to the flag it rests
    /// on, which toggles on Space or Enter.
    fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldAction> {
        let last = self.flags.len().checked_sub(1)?;
        let next = match key {
            Key::Named(NamedKey::Left) => self.focus.saturating_sub(1),
            Key::Named(NamedKey::Right) => self.focus.saturating_add(1).min(last),
            _ => {
                let index = self.focus;
                return self
                    .flags
                    .get_mut(index)?
                    .on_key(key)
                    .map(|SelectorAction::Set { on }| FieldAction::SetFlag { index, on });
            }
        };
        // Moving a ring the reader cannot see changes no pixel.
        let ringed = self
            .flags
            .get(self.focus)
            .is_some_and(|flag| flag.state().focus.focused);
        if ringed {
            let rects = self.flag_rects(bounds, scale, theme);
            damage::move_mark(
                Some(self.focus),
                Some(next),
                |i| rects.get(i).copied(),
                damage,
            );
        }
        self.focus = next;
        self.set_focused(ringed);
        None
    }
}

/// One setting: a label, an optional secondary description line, and a
/// trailing slot holding one [`FieldControl`].
///
/// The row draws the shared row chrome and its own two lines of text, and
/// forwards everything else to the control in its slot. Its own state carries
/// the setting's hover, selection, focus, pressure, activity and **authority**;
/// the last of those is shared with the control, so a denied setting cannot
/// hold an actionable control.
///
/// Equal rows draw the same pixels, so a host may use `==` as its repaint
/// gate: the label, description, control (with the control's own state) and
/// every visible part of the row's state compare, while the pointer coordinate
/// and press latch beneath them — which no render path reads — do not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldRow {
    label: String,
    description: Option<String>,
    control: FieldControl,
    state: ControlState,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The slot's control while the pointer is over it, so the motion that
    /// leaves it still reaches it and takes its hover look away.
    hovered: RenderInvariant<Option<usize>>,
    /// The slot's control while it holds a press, so a drag that leaves the
    /// slot still resolves on it.
    armed: RenderInvariant<Option<usize>>,
}

impl FieldRow {
    /// A row stating `label`, with `control` in its slot.
    #[must_use]
    pub fn new(label: impl Into<String>, control: FieldControl) -> Self {
        Self {
            label: label.into(),
            description: None,
            control,
            state: ControlState::idle(),
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
        }
    }

    /// This row with a secondary description line beneath its label.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// This row with the given composed state, shared with its control.
    #[must_use]
    pub fn with_state(mut self, state: ControlState) -> Self {
        self.set_state(state);
        self
    }

    /// The row's label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The row's description line, if it has one.
    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The control in the row's slot.
    #[must_use]
    pub fn control(&self) -> &FieldControl {
        &self.control
    }

    /// Mutable access to the control in the row's slot, for the owner to
    /// commit a value the row reported as a request.
    pub fn control_mut(&mut self) -> &mut FieldControl {
        &mut self.control
    }

    /// The row's composed state.
    #[must_use]
    pub fn state(&self) -> ControlState {
        self.state
    }

    /// Replace the row's composed state, sharing its enablement and authority
    /// with the control in the slot.
    ///
    /// A row's disposition is the *setting's*: there is no sense in which the
    /// label may be read but the control beside it obeys a different rule. So
    /// unlike a [`Card`](crate::collection::Card), whose footer actions are
    /// separate actions with authorities of their own, a field row imposes its
    /// own — which is what makes a denied row's control keep its value and
    /// refuse activation without a caller having to remember to set both.
    pub fn set_state(&mut self, state: ControlState) {
        self.state = state;
        self.control.adopt_authority(state);
    }

    /// Give the row keyboard focus.
    ///
    /// The ring goes where the keyboard goes: onto the slot's control when it
    /// takes focus, so Space and Enter reach the thing that acts, with the row
    /// marked a Focus Field member; onto the row itself for a reading, which
    /// has no control to ring but must still show where the cursor rests.
    pub fn set_focused(&mut self, focused: bool) {
        let taken = self.control.set_focused(focused);
        self.state.focus.focused = focused && !taken;
        self.state.focus.in_focus_field = focused && taken;
    }

    /// Set the row's selection, which the shared row chrome draws as its
    /// leading accent rail (a pane scrolling a searched-for setting into view
    /// marks it this way).
    pub fn set_selected(&mut self, selected: bool) {
        self.state.selection = if selected {
            SelectionState::Selected
        } else {
            SelectionState::Unselected
        };
    }

    /// Whether the row is selected.
    #[must_use]
    pub fn is_selected(&self) -> bool {
        matches!(
            self.state.selection,
            SelectionState::Selected | SelectionState::Mixed
        )
    }

    /// The height this row needs at `scale` when its own text is laid out
    /// across `span` pixels: one standard control band, plus its wrapped
    /// description where it draws one.
    ///
    /// One definition shared by [`FieldGroup`]'s layout and by
    /// [`render`](Self::render), so a group stacking rows cannot disagree with
    /// what a row actually draws. The span is part of the question because a
    /// description is prose: it wraps, so a narrow column needs a taller row
    /// — and a row that reserved one line for it would cut the sentence off
    /// at the column's edge.
    ///
    /// A row whose *label* does not fit the span whole draws no description
    /// at all, and so measures the band alone: once the setting's own name
    /// has had to be cut, an elaboration beneath it is noise.
    #[must_use]
    pub fn measured_height(&self, span: u32, scale: Scale, theme: &Theme) -> u32 {
        let band = text_plate_height(theme, scale, TextRole::Body);
        match self.described(span, scale, theme) {
            Some((description, block)) => band.saturating_add(block.height(description)),
            None => band,
        }
    }

    /// The description this row draws across `span`, and the block it is laid
    /// out in — or [`None`] when it has none, or when the label itself had to
    /// be cut to fit.
    fn described(&self, span: u32, scale: Scale, theme: &Theme) -> Option<(&str, TextBlock)> {
        let description = self.description.as_deref()?;
        let label = role_font(theme, scale, TextRole::Body);
        if label.text_width(&self.label) > span {
            return None;
        }
        let font = role_font(theme, scale, TextRole::Caption);
        Some((
            description,
            TextBlock::prose(
                font,
                span,
                MAX_DESCRIPTION_LINES,
                Color::from(theme.palette().on_surface_muted),
            ),
        ))
    }

    /// The width this row's slot wants, or [`None`] when its control takes
    /// whatever column it is given.
    #[must_use]
    pub fn slot_width(&self, scale: Scale, theme: &Theme) -> Option<u32> {
        self.control.wanted_width(scale, theme)
    }

    /// The trailing slot rectangle for `layout`: the shared column, against
    /// the trailing edge of the chrome's own content span, on the label's
    /// line — never wider than half that span, so however narrow the row gets
    /// its label keeps room to be read.
    ///
    /// The span comes from the same reservation the row chrome paints with, so
    /// the slot sits inside the trailing Signal Bead band rather than under
    /// it: a row that becomes denied gains its Authority Mark without moving
    /// its own control. [`None`] when the row is too small to seat a slot.
    #[must_use]
    pub fn slot_rect(&self, layout: FieldLayout, scale: Scale, theme: &Theme) -> Option<Rect> {
        let (x, y, w, h) = surface_rect(layout.bounds)?;
        if w == 0 || h == 0 {
            return None;
        }
        let (cx, cw) = row_content_span(scale, theme, x, w, h)?;
        let column = layout.column.min(slot_ceiling(cw));
        if column == 0 {
            return None;
        }
        let band = text_plate_height(theme, scale, TextRole::Body).min(h);
        Some(Rect::new(
            to_i32(cx.saturating_add(cw).saturating_sub(column)),
            to_i32(y),
            column,
            band,
        ))
    }

    /// The rectangle the slot's control is drawn and hit-tested in, which is
    /// the slot itself for a control that fills its column and the control's
    /// own wanted width for one that does not.
    #[must_use]
    pub fn control_rect(&self, layout: FieldLayout, scale: Scale, theme: &Theme) -> Option<Rect> {
        let slot = self.slot_rect(layout, scale, theme)?;
        Some(self.control.drawn_rect(slot, scale, theme))
    }

    /// The span the row's own text is laid out across: the chrome's content
    /// span less the slot column and one gap, so narrowing a row costs its
    /// words and never its control.
    fn text_span(layout: FieldLayout, scale: Scale, theme: &Theme) -> Option<(u32, u32)> {
        let (x, _, w, h) = surface_rect(layout.bounds)?;
        let (cx, cw) = row_content_span(scale, theme, x, w, h)?;
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let span = words_span(cw, layout.column, gap);
        (span > 0).then_some((cx, span))
    }

    /// Paint the row into `surface` for `layout`: the shared row chrome, the
    /// label on its own line, the description beneath it, and the control in
    /// the trailing slot.
    ///
    /// Room is given out in one fixed order — control, label, description —
    /// because that is the order the reader needs them in. The slot is served
    /// first, and never past half the row's content span, so words are what a
    /// narrowing row loses. The label takes the span that remains and elides into it. The
    /// description draws only while the label fits *whole*: once the setting's
    /// own name has had to be cut, a second cut line beneath it is noise, so
    /// the elaboration goes rather than the name. It goes for want of vertical
    /// room the same way.
    pub fn render(&self, surface: &mut Surface, layout: FieldLayout, scale: Scale, theme: &Theme) {
        let Some(rect) = surface_rect(layout.bounds) else {
            return;
        };
        let Some((_, cy, _, ch)) = paint_row(surface, rect, scale, theme, self.state) else {
            return;
        };
        let fg = foreground(theme, self.state.disposition());
        if let Some((tx, tw)) = Self::text_span(layout, scale, theme) {
            let label_font = role_font(theme, scale, TextRole::Body);
            let band = text_plate_height(theme, scale, TextRole::Body).min(ch);

            let (label, elided) = label_font.elide_to_width(&self.label, tw);
            paint_run(
                surface,
                label_font,
                (label, elided),
                (to_i32(tx), centred_text_y(label_font, cy, band)),
                fg,
                None,
            );

            if let Some((description, mut block)) = self.described(tw, scale, theme) {
                let top = cy.saturating_add(band);
                let room = cy.saturating_add(ch).saturating_sub(top);
                block.lines = block.lines.min(line_budget(block.font, room));
                block.paint(surface, description, (tx, top));
            }
        }

        if let Some(slot) = self.slot_rect(layout, scale, theme) {
            self.control.render(surface, slot, scale, theme, fg);
        }
    }

    /// Paint an expanded [`FieldControl::Combo`] slot's choice list at the
    /// rectangle the owner placed it, and nothing at all for any other slot.
    pub fn render_popup(&self, surface: &mut Surface, popup: Rect, scale: Scale, theme: &Theme) {
        if let FieldControl::Combo(combo) = &self.control {
            if combo.is_expanded() {
                combo.render_popup(surface, popup, scale, theme);
            }
        }
    }

    /// Whether the row's slot is showing an expanded choice list the owner
    /// must place and paint.
    #[must_use]
    pub fn popup_open(&self) -> bool {
        matches!(&self.control, FieldControl::Combo(combo) if combo.is_expanded())
    }

    /// Feed a pointer event, reporting what the slot's control asked for.
    ///
    /// The row's own chrome takes the hover, exactly as a list row's does, and
    /// no press look: a setting row is not itself activatable, so a press
    /// belongs to the control in its slot. The event reaches that control while
    /// the pointer is over its drawn rectangle, on the motion that leaves it,
    /// while it is holding a press — so a drag that leaves the slot still
    /// reaches the slider it began on — and throughout while its choice list is
    /// open.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
            let next = if layout.bounds.contains(*to) {
                PointerState::Hover
            } else {
                PointerState::None
            };
            damage::set(&mut self.state.pointer, next, layout.bounds, damage);
        }

        let rect = self.control_rect(layout, scale, theme)?;
        let over = rect.contains(*self.pointer).then_some(SLOT);
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);
        // An open choice list is modal and is drawn outside the row, so the row
        // keeps the stream until the list itself resolves it.
        if route.iter().all(Option::is_none) && !self.popup_open() {
            return None;
        }
        match &mut self.control {
            FieldControl::Toggle(c) => c
                .on_pointer(event, rect, damage)
                .map(|SelectorAction::Set { on }| FieldAction::Set { on }),
            FieldControl::Flags(c) => c.on_pointer(event, rect, scale, theme, damage),
            FieldControl::Combo(c) => c
                .on_pointer(event, rect, layout.popup, scale, theme, damage)
                .map(combo_action),
            FieldControl::Slider(c) => c.on_pointer(event, rect, damage).map(slider_action),
            FieldControl::Text(c) => c
                .on_pointer(event, rect, scale, theme, damage)
                .map(FieldAction::Text),
            FieldControl::Button(c) => c
                .on_pointer(event, rect, damage)
                .map(|_| FieldAction::Activated),
            FieldControl::Reading(_) | FieldControl::Unmeasured(_) => None,
        }
    }

    /// Feed a key event to the slot's control.
    ///
    /// A row whose slot holds a reading, and a row whose control is disabled
    /// or denied, consume the key without acting: the pane's shape never
    /// shifts under the reader, and a refusal is stated rather than performed.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldAction> {
        let rect = self.control_rect(layout, scale, theme)?;
        match &mut self.control {
            FieldControl::Toggle(c) => c
                .on_key(key)
                .map(|SelectorAction::Set { on }| FieldAction::Set { on }),
            FieldControl::Flags(c) => c.on_key(key, rect, scale, theme, damage),
            FieldControl::Combo(c) => c
                .on_key(key, rect, layout.popup, scale, theme, damage)
                .map(combo_action),
            FieldControl::Slider(c) => c.on_key(key, rect, damage).map(slider_action),
            FieldControl::Text(c) => c
                .on_key(key, modifiers, rect, damage)
                .map(FieldAction::Text),
            FieldControl::Button(c) => c.on_key(key).map(|_| FieldAction::Activated),
            FieldControl::Reading(_) | FieldControl::Unmeasured(_) => None,
        }
    }
}

/// One [`SliderAction`] as the row's own request, keeping the live value and
/// the settle point apart.
fn slider_action(action: SliderAction) -> FieldAction {
    match action {
        SliderAction::SetValue { permille } => FieldAction::SetValue { permille },
        SliderAction::Settled { permille } => FieldAction::Settled { permille },
    }
}

/// One [`ComboAction`] as the row's own request.
fn combo_action(action: ComboAction) -> FieldAction {
    match action {
        ComboAction::Selected { index } => FieldAction::Selected { index },
        ComboAction::Opened => FieldAction::Choices { expanded: true },
        ComboAction::Closed => FieldAction::Choices { expanded: false },
    }
}

/// A captioned plate of [`FieldRow`]s, with an optional footnote beneath.
///
/// The group owns the plate, the one slot column every row lines its control
/// up in, the stacking geometry, which row the pointer is over, which row
/// holds the keyboard, and the typed [`FieldGroupAction`] it reports. It draws
/// one plate for the whole group rather than a plate per row, and it restates
/// nothing a row or a control already draws.
///
/// Equal groups draw the same pixels, so a host may use `==` as its repaint
/// gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldGroup {
    caption: String,
    /// A state capsule on the caption's own line, at its trailing edge.
    badge: Option<StatusPill>,
    rows: Vec<FieldRow>,
    footnote: Option<String>,
    focus: Option<usize>,
    /// The last pointer position — hit-testing input, never drawn.
    pointer: RenderInvariant<Point>,
    /// The row the pointer was last over, so a motion sample reaches the row
    /// it left and the one it entered rather than the whole plate.
    hovered: RenderInvariant<Option<usize>>,
    /// The row holding a press, which keeps receiving the stream wherever the
    /// pointer goes.
    armed: RenderInvariant<Option<usize>>,
}

impl FieldGroup {
    /// A group captioned `caption` over `rows`, with no footnote and no row
    /// focused.
    #[must_use]
    pub fn new(caption: impl Into<String>, rows: Vec<FieldRow>) -> Self {
        Self {
            caption: caption.into(),
            badge: None,
            rows,
            footnote: None,
            focus: None,
            pointer: RenderInvariant::new(Point::ORIGIN),
            hovered: RenderInvariant::new(None),
            armed: RenderInvariant::new(None),
        }
    }

    /// This group with a state capsule on its caption's own line, at the
    /// trailing edge.
    ///
    /// The group places it rather than the owner, because it is the only
    /// thing that can also take the room out of the caption: a badge an
    /// owner drew over the band would sit on top of a long caption rather
    /// than beside it. The caption elides into whatever is left, and a band
    /// too narrow for both keeps the badge — the state of the thing is what
    /// a reader is scanning for, and the name is still legible cut.
    #[must_use]
    pub fn with_badge(mut self, badge: StatusPill) -> Self {
        self.badge = Some(badge);
        self
    }

    /// Put a capsule on this group's caption line, or take the one it has
    /// off, leaving the rows alone.
    ///
    /// [`with_badge`](Self::with_badge) consumes the group, so restating a
    /// state that moves while the reader works — which rows now differ from
    /// what is in effect — would mean rebuilding rows that hold a caret and
    /// a selection. The capsule rides a band of its own, so an owner that
    /// puts one on or takes one off re-measures
    /// ([`measured_height`](Self::measured_height) moves with it).
    pub fn set_badge(&mut self, badge: Option<StatusPill>) {
        self.badge = badge;
    }

    /// The capsule on this group's caption line, if it carries one.
    #[must_use]
    pub fn badge(&self) -> Option<&StatusPill> {
        self.badge.as_ref()
    }

    /// This group with a footnote beneath its rows — the sentence of
    /// consequence a setting sometimes needs, which belongs on the surface
    /// rather than behind a tooltip a pointer has to find.
    #[must_use]
    pub fn with_footnote(mut self, footnote: impl Into<String>) -> Self {
        self.footnote = Some(footnote.into());
        self
    }

    /// The group's caption.
    #[must_use]
    pub fn caption(&self) -> &str {
        &self.caption
    }

    /// The group's footnote, if it has one.
    #[must_use]
    pub fn footnote(&self) -> Option<&str> {
        self.footnote.as_deref()
    }

    /// The group's rows.
    #[must_use]
    pub fn rows(&self) -> &[FieldRow] {
        &self.rows
    }

    /// Mutable access to the group's rows (e.g. to commit a reported value).
    pub fn rows_mut(&mut self) -> &mut [FieldRow] {
        &mut self.rows
    }

    /// The number of rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the group holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The row holding keyboard focus, if any.
    #[must_use]
    pub fn focus(&self) -> Option<usize> {
        self.focus
    }

    /// Focus the row at `index` (or clear focus with `None`), reporting the
    /// two rows the ring moves between; an out-of-range index clears focus
    /// (fail closed).
    pub fn set_focus(
        &mut self,
        index: Option<usize>,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        let index = index.filter(|&i| i < self.rows.len());
        let rects = self.row_rects(layout, scale, theme);
        damage::move_mark(self.focus, index, |i| rects.get(i).copied(), damage);
        self.adopt_focus(index);
    }

    /// Adopt `index` as the focused row without reporting, for an owner that
    /// is rebuilding this group and presents it whole.
    pub fn adopt_focus(&mut self, index: Option<usize>) {
        let index = index.filter(|&i| i < self.rows.len());
        self.focus = index;
        for (i, row) in self.rows.iter_mut().enumerate() {
            row.set_focused(Some(i) == index);
        }
    }

    /// The one slot column this group's controls line up in, in a plate
    /// `width` pixels wide: the widest width any of its rows wants, and half
    /// the row content span when any row's control fills whatever it is
    /// given.
    ///
    /// An owner stacking several groups may lay them all out in the widest of
    /// their columns, so controls line up down the whole surface; every
    /// geometric question a group answers is asked with the column it is laid
    /// out in, because the column decides how much room a row's words wrap
    /// into. The span is measured against the *band* a row's control sits in
    /// rather than the row's own height, so resolving the column cannot depend
    /// on the heights the column itself decides.
    #[must_use]
    pub fn slot_column(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let Some(cw) = Self::content_span(width, scale, theme) else {
            return 0;
        };
        let ceiling = slot_ceiling(cw);
        let mut widest = 0;
        for row in &self.rows {
            match row.slot_width(scale, theme) {
                Some(want) => widest = widest.max(want),
                None => return ceiling,
            }
        }
        widest.min(ceiling)
    }

    /// The one column every group in `groups` lines up in, in plates `width`
    /// pixels wide: the widest any of them resolves, so a control does not
    /// step left and right down a surface stacking them.
    #[must_use]
    pub fn shared_column(groups: &[Self], width: u32, scale: Scale, theme: &Theme) -> u32 {
        groups
            .iter()
            .map(|group| group.slot_column(width, scale, theme))
            .max()
            .unwrap_or(0)
    }

    /// The narrowest plate this group can be given without cutting any row's
    /// control: every control that measures its own width is seated whole,
    /// and the labels keep the other half of the span to elide into.
    ///
    /// A control that takes whatever column it is given constrains nothing,
    /// so a group of only those answers the plate's own chrome. An owner that
    /// sizes its surface from this opens it with every control readable.
    #[must_use]
    pub fn natural_width(&self, scale: Scale, theme: &Theme) -> u32 {
        let widest = self
            .rows
            .iter()
            .filter_map(|row| row.slot_width(scale, theme))
            .max()
            .unwrap_or(0);
        let band = text_plate_height(theme, scale, TextRole::Body);
        // The slot is never granted more than half the span.
        row_width_for_content(scale, theme, widest.saturating_mul(2), band)
            .saturating_add(plate_border(theme, scale).saturating_mul(2))
    }

    /// The content span a row of a plate `width` pixels wide is laid out
    /// across, or [`None`] when the plate is too narrow to hold one.
    fn content_span(width: u32, scale: Scale, theme: &Theme) -> Option<u32> {
        let border = plate_border(theme, scale);
        let inner_w = width.saturating_sub(border.saturating_mul(2));
        if inner_w == 0 {
            return None;
        }
        let band = text_plate_height(theme, scale, TextRole::Body);
        row_content_span(scale, theme, border, inner_w, band).map(|(_, cw)| cw)
    }

    /// The span a row's own text is laid out across in a plate `width` pixels
    /// wide whose slots line up in `column`: the content span less the column
    /// and one gap.
    ///
    /// This is what a row's description wraps into, so it is what
    /// [`FieldRow::measured_height`] is asked about.
    #[must_use]
    pub fn row_text_span(&self, width: u32, column: u32, scale: Scale, theme: &Theme) -> u32 {
        let Some(cw) = Self::content_span(width, scale, theme) else {
            return 0;
        };
        let (_, gap) = Self::insets(scale, theme);
        words_span(cw, column, gap)
    }

    /// The height this group needs at `scale` to draw its caption, every row,
    /// and its footnote, in a plate `width` pixels wide whose slots line up in
    /// `column`.
    ///
    /// Both are part of the question because a row's description and the
    /// group's footnote are prose: they wrap, so how tall a group has to be
    /// depends on how much room its words are given.
    #[must_use]
    pub fn measured_height(&self, width: u32, column: u32, scale: Scale, theme: &Theme) -> u32 {
        let (pad, gap) = Self::insets(scale, theme);
        let span = self.row_text_span(width, column, scale, theme);
        let rows = self
            .rows
            .iter()
            .map(|row| row.measured_height(span, scale, theme))
            .fold(0u32, u32::saturating_add);
        plate_border(theme, scale)
            .saturating_mul(2)
            .saturating_add(pad.saturating_mul(2))
            .saturating_add(self.caption_height(scale, theme))
            .saturating_add(gap)
            .saturating_add(rows)
            .saturating_add(self.footnote_height(width, scale, theme))
    }

    /// The plate's content inset and the gap between its bands, in surface
    /// pixels.
    fn insets(scale: Scale, theme: &Theme) -> (u32, u32) {
        (
            scale.scale_length(theme.metrics().control_inset).max(1),
            scale.scale_length(theme.metrics().control_gap).max(1),
        )
    }

    /// The plate's interior, inside its rim: the width every row spans and the
    /// box the caption, the rows and the footnote are laid out in.
    fn inner(bounds: Rect, scale: Scale, theme: &Theme) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = surface_rect(bounds)?;
        inset(x, y, w, h, plate_border(theme, scale))
    }

    /// The height of the caption band above the rows.
    ///
    /// A badge rides on that line, so the band is the taller of the two —
    /// otherwise a capsule would overhang the first row.
    fn caption_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let text = role_font(theme, scale, TextRole::SectionHeader).line_height();
        match self.badge {
            Some(_) => text.max(StatusPill::measured_height(scale, theme)),
            None => text,
        }
    }

    /// The height this group's footnote band occupies below its rows — the gap
    /// that separates it plus its own wrapped lines — or nothing when it has
    /// none.
    fn footnote_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let (_, gap) = Self::insets(scale, theme);
        let Some(footnote) = &self.footnote else {
            return 0;
        };
        gap.saturating_add(Self::footnote_block(width, scale, theme).height(footnote))
    }

    /// The block the footnote is laid out in: caption-weight prose across the
    /// same column a row's label begins at.
    ///
    /// A footnote is where a setting needs a sentence of consequence, so it
    /// wraps — a consequence cut off at the plate's edge is one the reader
    /// has to guess at.
    fn footnote_block(width: u32, scale: Scale, theme: &Theme) -> TextBlock {
        let font = role_font(theme, scale, TextRole::Caption);
        TextBlock::prose(
            font,
            Self::content_span(width, scale, theme).unwrap_or(0),
            MAX_FOOTNOTE_LINES,
            Color::from(theme.palette().on_surface_muted),
        )
    }

    /// The `(top, bottom)` the group's rows may occupy within `inner`: below
    /// the caption band, above the footnote band.
    ///
    /// One definition for the layout and the paint, so the band a row is seated
    /// in and the band the footnote is drawn under cannot drift apart.
    fn rows_span(&self, inner: (u32, u32, u32, u32), scale: Scale, theme: &Theme) -> (u32, u32) {
        let (_, iy, iw, ih) = inner;
        let (pad, gap) = Self::insets(scale, theme);
        let top = iy
            .saturating_add(pad)
            .saturating_add(self.caption_height(scale, theme))
            .saturating_add(gap);
        let bottom = iy
            .saturating_add(ih)
            .saturating_sub(pad)
            .saturating_sub(self.footnote_height(iw, scale, theme));
        (top, bottom)
    }

    /// The rectangle of each *drawn* row, in order: as many whole rows as fit
    /// beneath the caption, each spanning the plate's inner width so its hover
    /// wash reaches the plate's edges.
    ///
    /// This is the one layout the paint, the hit test, and the focus reporting
    /// all read, so a press can never land on a row [`render`](Self::render)
    /// did not draw.
    fn row_rects(&self, layout: FieldLayout, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let Some(inner) = Self::inner(layout.bounds, scale, theme) else {
            return Vec::new();
        };
        let (inner_x, _, inner_w, _) = inner;
        let (mut top, bottom) = self.rows_span(inner, scale, theme);
        let span = self.row_text_span(layout.bounds.width, layout.column, scale, theme);
        let mut rects = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let row_h = row.measured_height(span, scale, theme);
            if top.saturating_add(row_h) > bottom {
                break;
            }
            rects.push(Rect::new(to_i32(inner_x), to_i32(top), inner_w, row_h));
            top = top.saturating_add(row_h);
        }
        rects
    }

    /// The rectangle row `index` occupies, or [`None`] when it is out of range
    /// or was omitted for lack of room (fail closed).
    #[must_use]
    pub fn row_rect(
        &self,
        index: usize,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.row_rects(layout, scale, theme).get(index).copied()
    }

    /// The row under `point`, if any. A point over a row omitted for lack of
    /// room answers [`None`].
    #[must_use]
    pub fn row_at(
        &self,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> Option<usize> {
        self.row_rects(layout, scale, theme)
            .iter()
            .position(|r| r.contains(point))
    }

    /// The row whose slot is showing a choice list, and the slot rectangle the
    /// owner should anchor it to.
    ///
    /// The owner places the list against this rectangle — below it where the
    /// window has room, above it where it does not — and hands the result back
    /// through [`FieldLayout::with_popup`], because the list is drawn above
    /// every group and only the owner knows the surface it has to fit in.
    #[must_use]
    pub fn popup_anchor(
        &self,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
    ) -> Option<(usize, Rect)> {
        let index = self.rows.iter().position(FieldRow::popup_open)?;
        let rect = self.row_rect(index, layout, scale, theme)?;
        let slot =
            self.rows
                .get(index)?
                .slot_rect(FieldLayout::new(rect, layout.column), scale, theme)?;
        Some((index, slot))
    }

    /// The layout this group is drawn with in `bounds`, inside `viewport`:
    /// the slot column its rows line up in, and an expanded slot's choice
    /// list placed where it fits.
    ///
    /// The ready-made application of [`slot_column`](Self::slot_column),
    /// [`popup_anchor`](Self::popup_anchor) and
    /// [`FieldLayout::with_popup`]: an owner that lays its groups out
    /// independently hands in the surface the list has to fit in and gets the
    /// whole layout back, rather than each owner carrying the same three
    /// calls. An owner that shares one column across several groups resolves
    /// the widest itself and places the list through those pieces instead.
    #[must_use]
    pub fn layout(&self, bounds: Rect, viewport: Rect, scale: Scale, theme: &Theme) -> FieldLayout {
        let layout = FieldLayout::new(bounds, self.slot_column(bounds.width, scale, theme));
        let placed = self
            .popup_anchor(layout, scale, theme)
            .and_then(|(row, slot)| match self.rows.get(row)?.control() {
                FieldControl::Combo(combo) => Some(combo.popup_rect(slot, viewport, scale, theme)),
                // Every other slot control draws wholly inside its own row.
                FieldControl::Toggle(_)
                | FieldControl::Flags(_)
                | FieldControl::Slider(_)
                | FieldControl::Text(_)
                | FieldControl::Button(_)
                | FieldControl::Reading(_)
                | FieldControl::Unmeasured(_) => None,
            });
        match placed {
            Some(popup) => layout.with_popup(popup),
            None => layout,
        }
    }

    /// Paint the group into `surface` for `layout`: its plate, its caption,
    /// every whole row that fits, and its footnote.
    ///
    /// An expanded slot's choice list is *not* drawn here — it belongs above
    /// every group, so [`render_popup`](Self::render_popup) draws it after the
    /// owner has painted them all.
    pub fn render(&self, surface: &mut Surface, layout: FieldLayout, scale: Scale, theme: &Theme) {
        let Some((x, y, w, h)) = surface_rect(layout.bounds) else {
            return;
        };
        if w == 0 || h == 0 {
            return;
        }
        let (pad, gap) = Self::insets(scale, theme);
        let border = plate_border(theme, scale);
        let radius = scale
            .scale_length(theme.metrics().window_corner_radius)
            .min(w / 2)
            .min(h / 2);
        let plate = (theme.palette().surface, ChromeLayer::Ground);
        let Some(inner) =
            paint_surface_plate(surface, (x, y, w, h), (radius, border), theme, plate)
        else {
            return;
        };
        let (inner_x, inner_y, inner_w, _) = inner;
        let caption_font = role_font(theme, scale, TextRole::SectionHeader);
        let caption_top = inner_y.saturating_add(pad);
        // The caption and footnote begin exactly where a row's label does, so
        // the three read as one column rather than three indents.
        if let Some((text_x, text_w)) =
            row_content_span(scale, theme, inner_x, inner_w, caption_font.line_height())
        {
            let badge = self.badge.as_ref().map(|badge| {
                let width = badge.measured_width(scale, theme).min(text_w);
                let height = StatusPill::measured_height(scale, theme);
                (
                    badge,
                    Rect::new(
                        to_i32(text_x.saturating_add(text_w).saturating_sub(width)),
                        to_i32(caption_top),
                        width,
                        height,
                    ),
                )
            });
            // Whatever the badge did not take, less a gap, so the two never
            // touch and the caption is cut rather than drawn under it.
            let caption_w = badge.as_ref().map_or(text_w, |(_, rect)| {
                text_w.saturating_sub(rect.width.saturating_add(gap))
            });
            let run = caption_font.elide_to_width(&self.caption, caption_w);
            paint_run(
                surface,
                caption_font,
                run,
                (to_i32(text_x), to_i32(caption_top)),
                Color::from(theme.palette().on_surface_muted),
                None,
            );
            if let Some((badge, rect)) = badge {
                badge.render(surface, rect, scale, theme);
            }
        }

        let rects = self.row_rects(layout, scale, theme);
        for (row, rect) in self.rows.iter().zip(rects.iter()) {
            row.render(
                surface,
                FieldLayout::new(*rect, layout.column).with_popup(layout.popup),
                scale,
                theme,
            );
        }

        if let Some(footnote) = &self.footnote {
            let font = role_font(theme, scale, TextRole::Caption);
            // Beneath the rows actually drawn, so a plate too short for every
            // row keeps its footnote attached to the last one that fitted.
            let drawn = rects
                .iter()
                .map(|rect| rect.height)
                .fold(0, u32::saturating_add);
            let top = self
                .rows_span(inner, scale, theme)
                .0
                .saturating_add(drawn)
                .saturating_add(gap);
            let room = y.saturating_add(h).saturating_sub(top);
            if let Some((text_x, _)) =
                row_content_span(scale, theme, inner_x, inner_w, font.line_height())
            {
                let mut block = Self::footnote_block(layout.bounds.width, scale, theme);
                block.lines = block.lines.min(line_budget(font, room));
                block.paint(surface, footnote, (text_x, top));
            }
        }
    }

    /// Paint the expanded slot's choice list at `popup`, after every group has
    /// been drawn.
    pub fn render_popup(&self, surface: &mut Surface, popup: Rect, scale: Scale, theme: &Theme) {
        for row in &self.rows {
            row.render_popup(surface, popup, scale, theme);
        }
    }

    /// Route a pointer event to the rows it concerns and report what one of
    /// them asked for.
    ///
    /// One hit test decides where the pointer is; the event then reaches only
    /// the row it left, the row it entered, and any row holding a press. A row
    /// with an open choice list keeps the stream while the list is up, because
    /// the list is drawn outside the group's own plate.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldGroupAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }
        let rects = self.row_rects(layout, scale, theme);
        let over = rects.iter().position(|r| r.contains(*self.pointer));
        let expanded = self.rows.iter().position(FieldRow::popup_open);
        let route = route_pointer(&mut self.hovered, *self.armed, over);
        *self.armed = grab_after(*self.armed, event, over);

        let mut fired = None;
        let open = expanded.filter(|i| !route.contains(&Some(*i)));
        for index in route.into_iter().flatten().chain(open) {
            let (Some(row), Some(rect)) = (self.rows.get_mut(index), rects.get(index)) else {
                continue;
            };
            let row_layout = FieldLayout::new(*rect, layout.column).with_popup(layout.popup);
            if let Some(action) = row.on_pointer(event, row_layout, scale, theme, damage) {
                fired = Some(FieldGroupAction { row: index, action });
            }
        }
        fired
    }

    /// Feed a key event: Up/Down move the keyboard cursor between rows,
    /// clamping at the first and last rather than wrapping (a group is a fixed
    /// set of settings, not a cycling ring), Home/End jump to the ends unless
    /// the focused row is editing text, and every other key goes to the
    /// focused row's control.
    ///
    /// The pane above the group is what carries the cursor *between* groups,
    /// which is why this clamps rather than wrapping.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        layout: FieldLayout,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> Option<FieldGroupAction> {
        if self.rows.is_empty() {
            return None;
        }
        let last = self.rows.len() - 1;
        let focused = self.focus.and_then(|i| self.rows.get(i));
        // An open choice list is modal: every key is the list's until it
        // resolves. A text slot keeps only the keys its editor means — Home and
        // End move a caret along a line — so Up and Down always move the
        // cursor and can never trap it in a field.
        let listing = focused.is_some_and(FieldRow::popup_open);
        let editing = focused.is_some_and(|row| matches!(row.control(), FieldControl::Text(_)));
        let moved = match key {
            _ if listing => None,
            Key::Named(NamedKey::Down) => Some(self.focus.map_or(0, |i| (i + 1).min(last))),
            Key::Named(NamedKey::Up) => Some(self.focus.map_or(0, |i| i.saturating_sub(1))),
            Key::Named(NamedKey::Home) if !editing => Some(0),
            Key::Named(NamedKey::End) if !editing => Some(last),
            _ => None,
        };
        if let Some(next) = moved {
            self.set_focus(Some(next), layout, scale, theme, damage);
            return None;
        }
        let index = self.focus?;
        let rect = self.row_rect(index, layout, scale, theme)?;
        let row_layout = FieldLayout::new(rect, layout.column).with_popup(layout.popup);
        let row = self.rows.get_mut(index)?;
        row.on_key(key, modifiers, row_layout, scale, theme, damage)
            .map(|action| FieldGroupAction { row: index, action })
    }
}
