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
    BandCorner, Button, Card, Checkbox, ComboBox, Dialog, FieldAction, FieldControl, FieldGroup,
    FieldGroupAction, FieldLayout, HelpTip, IconButton, ListRow, Menu, Panel, Progress, Radio,
    ScrollAction, ScrollBar, SearchField, SelectionState, SelectorAction, Slider, SliderAction,
    SplitButton, TableRow, Tabs, TabsAction, TextField, Toggle, Toolbar, ToolbarOutcome, Tooltip,
    WindowControl,
};
use tairix_geometry::{Rect, Region, Scale};
use tairix_icon::NoArtwork;
use tairix_input::{InputEvent, Key, Modifiers};
use tairix_raster::Surface;
use tairix_theme::Theme;

/// Where a demo widget is drawn and how.
///
/// One value rather than four loose parameters through every entry point: the
/// widget's own rectangle, the whole client a popped-up choice list has to fit
/// inside (which is the gallery's to know, not the widget's), the density, and
/// the theme.
#[derive(Copy, Clone, Debug)]
pub struct DemoContext<'a> {
    /// The widget's own rectangle.
    pub rect: Rect,
    /// The client a popped-up list must stay inside.
    pub viewport: Rect,
    /// The active UI density.
    pub scale: Scale,
    /// The active theme.
    pub theme: &'a Theme,
}

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
    FieldGroup(FieldGroup),
    Sidebar(Tabs),
    Dialog(Dialog),
    Tooltip(Tooltip),
    HelpTip(HelpTip),
    Toolbar(Toolbar),
    ScrollBar(ScrollBar),
    WindowControl(WindowControl),
}

/// Commit a row's request into the control that made it, so the demo reacts
/// exactly as a pane's own commit would.
///
/// The gallery holds its values in memory and nothing else, so a live sample
/// and the settle that follows it are the same acknowledgement; a real pane
/// acts durably on the settle alone.
fn commit_field(group: &mut FieldGroup, action: FieldGroupAction, rect: Rect, damage: &mut Region) {
    let Some(row) = group.rows_mut().get_mut(action.row) else {
        return;
    };
    match (row.control_mut(), action.action) {
        (FieldControl::Toggle(c), FieldAction::Set { on }) => c.set_on(on),
        (FieldControl::Combo(c), FieldAction::Selected { index }) => c.set_selected(index),
        (
            FieldControl::Slider(c),
            FieldAction::SetValue { permille } | FieldAction::Settled { permille },
        ) => c.set_value(permille),
        // A text edit is already in the field's own buffer; a command press
        // and a list opening or closing change no value.
        _ => {}
    }
    damage.add(rect);
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
    pub fn set_focused(&mut self, focused: bool, ctx: DemoContext<'_>, damage: &mut Region) {
        let (rect, scale, theme) = (ctx.rect, ctx.scale, ctx.theme);
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
            DemoWidget::Sidebar(w) => w.adopt_current(focused.then_some(0)),
            DemoWidget::FieldGroup(w) => w.adopt_focus(focused.then_some(0)),
            DemoWidget::Toolbar(w) => {
                w.set_focus(focused.then_some(0), rect, scale, theme, damage);
            }
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
    ///
    /// `viewport` is the whole client a popped-up choice list has to fit
    /// inside, which is the gallery's to know rather than the widget's.
    pub fn render(&self, surface: &mut Surface, ctx: DemoContext<'_>) {
        let (rect, viewport, scale, theme) = (ctx.rect, ctx.viewport, ctx.scale, ctx.theme);
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
                    let popup = w.popup_rect(rect, viewport, scale, theme);
                    w.render_popup(surface, popup, scale, theme);
                }
            }
            DemoWidget::Menu(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Sidebar(w) => w.render(surface, rect, scale, theme, &mut NoArtwork),
            DemoWidget::ListRow(w) => w.render(surface, rect, scale, theme, None),
            DemoWidget::TableRow(w) => {
                let columns = equal_columns(w.cells().len(), rect.width);
                w.render(surface, rect, scale, theme, &columns, None);
            }
            DemoWidget::Card(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Panel(w) => w.render(surface, rect, scale, theme),
            DemoWidget::FieldGroup(w) => {
                let layout = w.layout(rect, viewport, scale, theme);
                w.render(surface, layout, scale, theme);
                w.render_popup(surface, layout.popup, scale, theme);
            }
            DemoWidget::Dialog(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Tooltip(w) => w.render(surface, rect, scale, theme),
            DemoWidget::HelpTip(w) => w.render(surface, rect, scale, theme),
            DemoWidget::Toolbar(w) => w.render(surface, rect, scale, theme, &mut NoArtwork),
            DemoWidget::ScrollBar(w) => w.render(surface, rect, scale, theme),
            // Shown in the gallery rather than seated in a real title bar, so
            // it has no band end to curve against.
            DemoWidget::WindowControl(w) => {
                w.render(surface, rect, scale, theme, BandCorner::Square);
            }
        }
    }

    /// Route one pointer event at `rect`, reflecting any value-changing action
    /// back into the control. Returns whether the view should repaint (an
    /// action fired or a drag moved the value).
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: DemoContext<'_>,
        damage: &mut Region,
    ) -> bool {
        let (rect, viewport, scale, theme) = (ctx.rect, ctx.viewport, ctx.scale, ctx.theme);
        match self {
            DemoWidget::Button(w) => w.on_pointer(event, rect, damage).is_some(),
            DemoWidget::IconButton(w) => w.on_pointer(event, rect, damage).is_some(),
            DemoWidget::SplitButton(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Toggle(w) => {
                let acted = w.on_pointer(event, rect, damage);
                set_on(acted, rect, damage, |on| w.set_on(on))
            }
            DemoWidget::Checkbox(w) => {
                let acted = w.on_pointer(event, rect, damage);
                set_on(acted, rect, damage, |on| w.set_selection(selection_for(on)))
            }
            DemoWidget::Radio(w) => {
                let acted = w.on_pointer(event, rect, damage);
                set_on(acted, rect, damage, |on| w.set_selected(on))
            }
            DemoWidget::Slider(w) => match w.on_pointer(event, rect, damage) {
                // The gallery holds the value in memory and nothing else, so
                // the live sample and the settle are the same acknowledgement.
                Some(SliderAction::SetValue { permille } | SliderAction::Settled { permille }) => {
                    w.set_value(permille);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Progress(_) | DemoWidget::Tooltip(_) => false,
            DemoWidget::TextField(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::SearchField(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::ComboBox(w) => {
                let popup = w.popup_rect(rect, viewport, scale, theme);
                w.on_pointer(event, rect, popup, scale, theme, damage)
                    .is_some()
            }
            DemoWidget::Menu(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Sidebar(w) => match w.on_pointer(event, rect, scale, theme, damage) {
                Some(TabsAction::Selected { index }) => {
                    w.set_selected(index, rect, scale, theme, damage);
                    true
                }
                None => false,
            },
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
            DemoWidget::FieldGroup(w) => {
                let before = w.layout(rect, viewport, scale, theme);
                let acted = w.on_pointer(event, before, scale, theme, damage);
                field_popup_moved(
                    w,
                    before,
                    rect,
                    viewport,
                    scale,
                    theme,
                    damage,
                    acted.is_some(),
                );
                if let Some(action) = acted {
                    commit_field(w, action, rect, damage);
                    return true;
                }
                false
            }
            DemoWidget::Dialog(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::HelpTip(w) => w.on_pointer(event, rect, scale, theme, damage).is_some(),
            DemoWidget::Toolbar(w) => match w.on_pointer(event, rect, scale, theme, damage) {
                ToolbarOutcome::Activated(action) => {
                    w.set_active(action.index);
                    committed(rect, damage)
                }
                // A hover, a press, or a scrolled strip: the damage the
                // control reported is what the gallery repaints.
                ToolbarOutcome::Redraw => true,
                ToolbarOutcome::Idle => false,
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
        ctx: DemoContext<'_>,
        damage: &mut Region,
    ) -> bool {
        let (rect, viewport, scale, theme) = (ctx.rect, ctx.viewport, ctx.scale, ctx.theme);
        match self {
            DemoWidget::Button(w) => w.on_key(key).is_some(),
            DemoWidget::IconButton(w) => w.on_key(key).is_some(),
            DemoWidget::SplitButton(w) => w.on_key(key).is_some(),
            DemoWidget::Toggle(w) => {
                let acted = w.on_key(key);
                set_on(acted, rect, damage, |on| w.set_on(on))
            }
            DemoWidget::Checkbox(w) => {
                let acted = w.on_key(key);
                set_on(acted, rect, damage, |on| w.set_selection(selection_for(on)))
            }
            DemoWidget::Radio(w) => {
                let acted = w.on_key(key);
                set_on(acted, rect, damage, |on| w.set_selected(on))
            }
            DemoWidget::Slider(w) => match w.on_key(key, rect, damage) {
                Some(SliderAction::SetValue { permille } | SliderAction::Settled { permille }) => {
                    w.set_value(permille);
                    committed(rect, damage)
                }
                None => false,
            },
            DemoWidget::Progress(_) | DemoWidget::Tooltip(_) => false,
            DemoWidget::TextField(w) => w.on_key(key, modifiers, rect, damage).is_some(),
            DemoWidget::SearchField(w) => w.on_key(key, modifiers, rect, damage).is_some(),
            DemoWidget::ComboBox(w) => {
                let popup = w.popup_rect(rect, viewport, scale, theme);
                w.on_key(key, rect, popup, scale, theme, damage).is_some()
            }
            DemoWidget::Menu(w) => w.on_key(key, rect, scale, theme, damage).is_some(),
            DemoWidget::Sidebar(w) => match w.on_key(key, rect, scale, theme, damage) {
                Some(TabsAction::Selected { index }) => {
                    w.set_selected(index, rect, scale, theme, damage);
                    true
                }
                None => false,
            },
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
            DemoWidget::FieldGroup(w) => {
                let before = w.layout(rect, viewport, scale, theme);
                let acted = w.on_key(key, modifiers, before, scale, theme, damage);
                field_popup_moved(
                    w,
                    before,
                    rect,
                    viewport,
                    scale,
                    theme,
                    damage,
                    acted.is_some(),
                );
                if let Some(action) = acted {
                    commit_field(w, action, rect, damage);
                    return true;
                }
                false
            }
            DemoWidget::Dialog(w) => w.on_key(key).is_some(),
            DemoWidget::HelpTip(w) => w.on_key(key).is_some(),
            DemoWidget::Toolbar(w) => match w.on_key(key, rect, scale, theme, damage) {
                ToolbarOutcome::Activated(action) => {
                    w.set_active(action.index);
                    committed(rect, damage)
                }
                ToolbarOutcome::Redraw => true,
                ToolbarOutcome::Idle => false,
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

/// Report a choice list that has just appeared or vacated.
///
/// A list is drawn outside the group's own plate, so only the owner that
/// placed it holds the rectangle it covered — and the *open* is reported from
/// a layout that had no list in it yet, the *close* from one that no longer
/// does. Reporting both the list that was there and the one that is now covers
/// either transition; an absent list is an empty rectangle and covers nothing.
#[allow(clippy::too_many_arguments)] // The two layouts' inputs, threaded explicitly.
fn field_popup_moved(
    group: &FieldGroup,
    before: FieldLayout,
    rect: Rect,
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    damage: &mut Region,
    acted: bool,
) {
    if !acted {
        return;
    }
    let after = group.layout(rect, viewport, scale, theme);
    if before.popup != after.popup {
        damage.add(before.popup);
        damage.add(after.popup);
    }
}

/// Commit the boolean a selector asked for through `apply`, and report the
/// control's own pixels — the shape all three boolean selectors share.
fn set_on(
    acted: Option<SelectorAction>,
    rect: Rect,
    damage: &mut Region,
    mut apply: impl FnMut(bool),
) -> bool {
    match acted {
        Some(SelectorAction::Set { on }) => {
            apply(on);
            committed(rect, damage)
        }
        None => false,
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
