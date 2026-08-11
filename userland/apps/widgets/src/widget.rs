//! [`DemoWidget`]: one enum over every shared [`tairix_controls`] control the
//! gallery shows, with a uniform render / pointer / key / focus surface.
//!
//! Wrapping the controls in one enum lets a gallery panel store a
//! heterogeneous column of demo widgets in a single `Vec` and route a routed
//! event to the one under focus without a bespoke branch per panel. Each arm
//! forwards to the wrapped control's own drawing and input methods — the
//! gallery adds no second control implementation — and, where a control emits
//! a value-changing action, reflects that typed action straight back into the
//! control so the demo reacts (a toggle flips, a slider moves, a scrollbar
//! scrolls). The gallery is the control's owner; no privileged work happens
//! here.

use alloc::vec::Vec;

use tairix_controls::{
    Button, Card, Checkbox, ComboBox, Dialog, HelpTip, IconButton, ListRow, Menu, Panel, Progress,
    Radio, ScrollAction, ScrollBar, SearchField, SelectionState, SelectorAction, Slider,
    SliderAction, SplitButton, TableRow, TextField, Toggle, Toolbar, Tooltip, WindowControl,
};
use tairix_geometry::{Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// One shared control shown in a gallery panel.
///
/// The variants cover every drawn [`tairix_controls`] family: the button
/// family, the boolean selectors, the value controls, the text entries, the
/// choice controls, the collection surfaces, the feedback surfaces, the bars,
/// and a window-manager command button. Read-only instruments ([`Progress`],
/// [`Tooltip`]) have no input; every other variant is interactive.
#[derive(Clone, Debug)]
#[allow(missing_docs)] // Each variant simply names the wrapped control; the control's own type documents it.
pub enum DemoWidget {
    Button(Button),
    IconButton(IconButton),
    SplitButton(SplitButton),
    Toggle(Toggle),
    Checkbox(Checkbox),
    Radio(Radio),
    Slider(Slider),
    Progress(Progress),
    TextField(TextField),
    SearchField(SearchField),
    ComboBox(ComboBox),
    Menu(Menu),
    ListRow(ListRow),
    TableRow(TableRow),
    Card(Card),
    Panel(Panel),
    Dialog(Dialog),
    Tooltip(Tooltip),
    HelpTip(HelpTip),
    Toolbar(Toolbar),
    ScrollBar(ScrollBar),
    WindowControl(WindowControl),
}

/// The popup rectangle a [`ComboBox`] expands into: directly below its field,
/// sized by the control's own preferred popup size, so the field draw and the
/// popup hit-test agree.
fn combo_popup_rect(combo: &ComboBox, field: Rect, scale: Scale, theme: &Theme) -> Rect {
    let (w, h) = combo.popup_size(field.width, scale, theme);
    Rect::new(field.left(), field.bottom(), w, h)
}

/// Equal-width column boundaries for a [`TableRow`] with `cells` cells across
/// `width` physical pixels, the shape the row renderer expects.
fn equal_columns(cells: usize, width: u32) -> Vec<u32> {
    let count = u32::try_from(cells).unwrap_or(0).max(1);
    let each = width / count;
    (0..count)
        .map(|i| {
            if i + 1 == count {
                width - i * each
            } else {
                each
            }
        })
        .collect()
}

impl DemoWidget {
    /// Whether this widget can take keyboard focus and pointer interaction.
    /// The read-only instruments ([`Progress`], [`Tooltip`]) cannot.
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        !matches!(self, DemoWidget::Progress(_) | DemoWidget::Tooltip(_))
    }

    /// Whether this widget is a selected radio button (the gallery clears the
    /// other radios in a panel when one becomes selected, so a group shows
    /// exactly one choice).
    #[must_use]
    pub fn is_selected_radio(&self) -> bool {
        matches!(self, DemoWidget::Radio(r) if r.is_selected())
    }

    /// Clear this widget's selection if it is a radio button (used to enforce
    /// single-selection across a radio group), answering whether that cleared a
    /// selection the radio was drawing.
    ///
    /// The owner clearing it holds the rectangle it is drawn at, so the answer
    /// is what tells the owner which radios to report.
    pub fn clear_radio(&mut self) -> bool {
        match self {
            DemoWidget::Radio(r) if r.is_selected() => {
                r.set_selected(false);
                true
            }
            _ => false,
        }
    }

    /// Set (or clear) this widget's keyboard focus where it has one, at the
    /// `rect` the widget is rendered at.
    pub fn set_focused(&mut self, focused: bool, rect: Rect, damage: &mut Region) {
        match self {
            DemoWidget::Button(w) => w.set_focused(focused),
            DemoWidget::IconButton(w) => w.set_focused(focused),
            DemoWidget::Toggle(w) => w.set_focused(focused),
            DemoWidget::Checkbox(w) => w.set_focused(focused),
            DemoWidget::Radio(w) => w.set_focused(focused),
            DemoWidget::Slider(w) => w.set_focused(focused),
            DemoWidget::TextField(w) => w.set_focused(focused),
            DemoWidget::SearchField(w) => w.set_focused(focused),
            DemoWidget::ComboBox(w) => w.set_focused(focused),
            DemoWidget::ListRow(w) => w.set_focused(focused),
            DemoWidget::TableRow(w) => w.set_focused(focused),
            DemoWidget::ScrollBar(w) => w.set_focused(focused),
            DemoWidget::WindowControl(w) => w.set_focused(focused),
            // The gallery reports this item's whole rectangle when the ring
            // moves, and the highlighted row is drawn inside it.
            DemoWidget::Menu(w) => w.adopt_current(focused.then_some(0)),
            DemoWidget::Toolbar(w) => w.set_focus(focused.then_some(0), rect, damage),
            // No focus ring: split button, progress, card, panel, dialog,
            // tooltip, help tip. Focus is a no-op rather than an error.
            DemoWidget::SplitButton(_)
            | DemoWidget::Progress(_)
            | DemoWidget::Card(_)
            | DemoWidget::Panel(_)
            | DemoWidget::Dialog(_)
            | DemoWidget::Tooltip(_)
            | DemoWidget::HelpTip(_) => {}
        }
    }

    /// Draw the widget into `surface` at `rect` for the active theme.
    pub fn render(&self, surface: &mut Surface, rect: Rect, scale: Scale, theme: &Theme) {
        match self {
            DemoWidget::Button(w) => w.render(surface, rect, scale, theme),
            // The gallery shows the built-in glyph: it is a control catalogue,
            // not an application with icon artwork of its own to supply.
            DemoWidget::IconButton(w) => w.render(surface, rect, scale, theme, None),
            DemoWidget::SplitButton(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Toggle(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Checkbox(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Radio(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Slider(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Progress(w) => w.render(surface, rect, scale, theme),
            DemoWidget::TextField(w) => w.render(surface, rect, scale, theme),
            DemoWidget::SearchField(w) => w.render(surface, rect, scale, theme),
            DemoWidget::ComboBox(w) => {
                w.render(surface, rect, scale, theme);
                if w.is_expanded() {
                    let popup = combo_popup_rect(w, rect, scale, theme);
                    w.render_popup(surface, popup, scale, theme);
                }
            }
            DemoWidget::Menu(w) => w.render(surface, rect, scale, theme),
            DemoWidget::ListRow(w) => w.render(surface, rect, scale, theme, None),
            DemoWidget::TableRow(w) => {
                let columns = equal_columns(w.cells().len(), rect.width);
                w.render(surface, rect, scale, theme, &columns);
            }
            DemoWidget::Card(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Panel(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Dialog(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Tooltip(w) => w.render(surface, rect, scale, theme),
            DemoWidget::HelpTip(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Toolbar(w) => w.render(surface, rect, scale, theme),
            DemoWidget::ScrollBar(w) => w.render(surface, rect, scale, theme),
            DemoWidget::WindowControl(w) => w.render(surface, rect, scale, theme),
        }
    }

    /// Route one pointer event at `rect`, reflecting any value-changing action
    /// back into the control. Returns whether the view should repaint (an
    /// action fired or a drag moved the value).
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        match self {
            DemoWidget::Button(w) => w.on_pointer(event, rect, damage).is_some(),
            DemoWidget::IconButton(w) => w.on_pointer(event, rect, damage).is_some(),
            DemoWidget::SplitButton(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Toggle(w) => match w.on_pointer(event, rect, damage) {
                Some(SelectorAction::Set { on }) => {
                    w.set_on(on);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Checkbox(w) => match w.on_pointer(event, rect, damage) {
                Some(SelectorAction::Set { on }) => {
                    w.set_selection(selection_for(on));
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Radio(w) => match w.on_pointer(event, rect, damage) {
                Some(SelectorAction::Set { on }) => {
                    w.set_selected(on);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Slider(w) => match w.on_pointer(event, rect, damage) {
                Some(SliderAction::SetValue { permille }) => {
                    w.set_value(permille);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Progress(_) | DemoWidget::Tooltip(_) => false,
            DemoWidget::TextField(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::SearchField(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::ComboBox(w) => {
                let popup = combo_popup_rect(w, rect, scale, theme);
                w.on_pointer(event, rect, popup, scale, theme, damage)
                    .is_some()
            }
            DemoWidget::Menu(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::ListRow(w) => match w.on_pointer(event, rect, damage) {
                Some(_) => {
                    w.set_selected(!w.is_selected());
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::TableRow(w) => match w.on_pointer(event, rect, damage) {
                Some(_) => {
                    w.set_selected(!w.is_selected());
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Card(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Panel(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Dialog(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::HelpTip(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Toolbar(w) => match w.on_pointer(event, rect, scale, theme, damage) {
                Some(action) => {
                    w.set_active(action.index);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::ScrollBar(w) => match w.on_pointer(event, rect, scale, theme, damage) {
                Some(ScrollAction::ScrollTo { offset }) => {
                    w.set_model(w.model().scroll_to(offset));
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::WindowControl(w) => w.on_pointer(event, rect, damage).is_some(),
        }
    }

    /// Route one key press to the focused widget at the `rect` it is rendered
    /// at, reflecting any value-changing action back into the control. Returns
    /// whether the view should repaint.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) -> bool {
        match self {
            DemoWidget::Button(w) => w.on_key(key).is_some(),
            DemoWidget::IconButton(w) => w.on_key(key).is_some(),
            DemoWidget::SplitButton(w) => w.on_key(key).is_some(),
            DemoWidget::Toggle(w) => match w.on_key(key) {
                Some(SelectorAction::Set { on }) => {
                    w.set_on(on);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Checkbox(w) => match w.on_key(key) {
                Some(SelectorAction::Set { on }) => {
                    w.set_selection(selection_for(on));
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Radio(w) => match w.on_key(key) {
                Some(SelectorAction::Set { on }) => {
                    w.set_selected(on);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Slider(w) => match w.on_key(key, rect, damage) {
                Some(SliderAction::SetValue { permille }) => {
                    w.set_value(permille);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Progress(_) | DemoWidget::Tooltip(_) => false,
            DemoWidget::TextField(w) => w.on_key(key, modifiers, rect, damage).is_some(),
            DemoWidget::SearchField(w) => w.on_key(key, modifiers, rect, damage).is_some(),
            DemoWidget::ComboBox(w) => {
                let popup = combo_popup_rect(w, rect, scale, theme);
                w.on_key(key, rect, popup, scale, theme, damage).is_some()
            }
            DemoWidget::Menu(w) => w.on_key(key, rect, scale, theme, damage).is_some(),
            DemoWidget::ListRow(w) => match w.on_key(key) {
                Some(_) => {
                    w.set_selected(!w.is_selected());
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::TableRow(w) => match w.on_key(key) {
                Some(_) => {
                    w.set_selected(!w.is_selected());
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Card(w) => w.on_key(key).is_some(),
            DemoWidget::Panel(w) => w.on_key(key).is_some(),
            DemoWidget::Dialog(w) => w.on_key(key).is_some(),
            DemoWidget::HelpTip(w) => w.on_key(key).is_some(),
            DemoWidget::Toolbar(w) => match w.on_key(key, rect, damage) {
                Some(action) => {
                    w.set_active(action.index);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::ScrollBar(w) => match w.on_key(key, rect, damage) {
                Some(ScrollAction::ScrollTo { offset }) => {
                    w.set_model(w.model().scroll_to(offset));
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::WindowControl(w) => w.on_key(key, rect, damage).is_some(),
        }
    }
}

/// Report the control drawn at `rect` after the owner has committed a value into
/// it, and answer the `true` the caller returns.
///
/// A control reports the pixels it changes itself, but the value it holds is its
/// owner's to commit, and the owner is the only party that knows where it drew
/// the control. The committed value is drawn inside that rectangle.
fn committed(rect: Rect, damage: &mut Region) -> bool {
    damage.add(rect);
    true
}

/// The [`SelectionState`] a checkbox takes for a boolean set request.
fn selection_for(on: bool) -> SelectionState {
    if on {
        SelectionState::Selected
    } else {
        SelectionState::Unselected
    }
}
