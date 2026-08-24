//! Panel construction: the set of demo widgets shown on each [`GalleryTab`].
//!
//! Each [`build`] arm returns the column of captioned [`DemoItem`]s for one
//! family, chosen to cover that family's variations — roles (neutral, primary,
//! recommended, destructive), states (idle, disabled, denied-by-authority,
//! invalid), and values — so the full behaviour of each control is visible in
//! one place. Every widget is a shared [`tairix_controls`] control; this module
//! only *composes* them.

use alloc::vec;
use alloc::vec::Vec;

use tairix_controls::{
    ActivityState, AuthorityState, Button, ButtonContent, Card, Checkbox, ComboBox, ControlRole,
    ControlState, Dialog, HelpTip, IconButton, ListRow, Menu, MenuItem, Panel, Progress,
    ProgressValue, Radio, ScrollBar, ScrollModel, ScrollOrientation, ScrollRange, SearchField,
    SelectionState, Slider, SplitButton, TableCell, TableRow, TextField, Toggle, Toolbar, Tooltip,
    ValidationState, WindowControl, WindowControlKind,
};
use tairix_icon::IconKind;

use crate::gallery::{DemoItem, GalleryTab};
use crate::widget::DemoWidget;

/// Build the demo-item column for one control family.
#[must_use]
pub fn build(tab: GalleryTab) -> Vec<DemoItem> {
    match tab {
        GalleryTab::Buttons => buttons(),
        GalleryTab::Selectors => selectors(),
        GalleryTab::Values => values(),
        GalleryTab::Text => text(),
        GalleryTab::Choice => choice(),
        GalleryTab::Collections => collections(),
        GalleryTab::Bars => bars(),
        GalleryTab::Feedback => feedback(),
        GalleryTab::Window => window(),
    }
}

/// A control-state that reads as authority-denied (an Authority Mark, not a
/// plain disabled look).
fn denied() -> ControlState {
    ControlState::idle().with_authority(AuthorityState::NeedsCapability)
}

/// A `Button` with the given label, role, and state, wrapped for a panel.
fn button(label: &str, role: ControlRole, state: ControlState) -> DemoWidget {
    let mut b = Button::new(ButtonContent::Label(label.into()), role);
    b.set_state(state);
    DemoWidget::Button(b)
}

fn buttons() -> Vec<DemoItem> {
    vec![
        DemoItem::new(
            "Primary",
            button("Save", ControlRole::Primary, ControlState::idle()),
            40,
        ),
        DemoItem::new(
            "Recommended",
            button("Update", ControlRole::Recommended, ControlState::idle()),
            40,
        ),
        DemoItem::new(
            "Destructive",
            button("Delete", ControlRole::Destructive, ControlState::idle()),
            40,
        ),
        DemoItem::new(
            "Disabled",
            button(
                "Unavailable",
                ControlRole::Neutral,
                ControlState::disabled(),
            ),
            40,
        ),
        DemoItem::new(
            "Denied by authority",
            button("Restricted", ControlRole::Neutral, denied()),
            40,
        ),
        DemoItem::new(
            "Icon button",
            DemoWidget::IconButton(IconButton::new(IconKind::Refresh, ControlRole::Neutral)),
            40,
        )
        .with_width(44),
        DemoItem::new(
            "Split button",
            DemoWidget::SplitButton(SplitButton::new(
                ButtonContent::Label("Export".into()),
                ControlRole::Primary,
            )),
            40,
        )
        .with_width(140),
    ]
}

fn selectors() -> Vec<DemoItem> {
    let mut denied_toggle = Toggle::new("Airplane mode", false);
    denied_toggle.set_state(denied());
    vec![
        DemoItem::new(
            "Toggle (on)",
            DemoWidget::Toggle(Toggle::new("Wi-Fi", true)),
            34,
        ),
        DemoItem::new(
            "Toggle (off)",
            DemoWidget::Toggle(Toggle::new("Bluetooth", false)),
            34,
        ),
        DemoItem::new("Toggle (denied)", DemoWidget::Toggle(denied_toggle), 34),
        DemoItem::new(
            "Checkbox (checked)",
            DemoWidget::Checkbox(Checkbox::new("Accept terms", SelectionState::Selected)),
            34,
        ),
        DemoItem::new(
            "Checkbox (mixed)",
            DemoWidget::Checkbox(Checkbox::new("Select all", SelectionState::Mixed)),
            34,
        ),
        DemoItem::new(
            "Radio (group)",
            DemoWidget::Radio(Radio::new("Light theme", false)),
            30,
        ),
        DemoItem::new(
            "Radio (group)",
            DemoWidget::Radio(Radio::new("Dark theme", true)),
            30,
        ),
    ]
}

fn values() -> Vec<DemoItem> {
    let mut disabled_slider = Slider::new(300);
    disabled_slider.set_state(ControlState::disabled());

    let mut progress = Progress::new().with_label("Copying");
    progress.set_state(
        ControlState::idle().with_activity(ActivityState::Progress(ProgressValue::new(660))),
    );
    let mut working = Progress::new().with_label("Scanning");
    working.set_state(ControlState::idle().with_activity(ActivityState::Indeterminate));
    let mut failed = Progress::new().with_label("Failed");
    failed.set_state(
        ControlState::idle()
            .with_activity(ActivityState::Working)
            .with_validation(ValidationState::Invalid),
    );

    vec![
        DemoItem::new(
            "Slider",
            DemoWidget::Slider(Slider::new(400).with_steps(50, 200)),
            36,
        ),
        DemoItem::new(
            "Slider (capped)",
            DemoWidget::Slider(Slider::new(700).with_cap(850)),
            36,
        ),
        DemoItem::new("Slider (disabled)", DemoWidget::Slider(disabled_slider), 36),
        DemoItem::new("Progress (66%)", DemoWidget::Progress(progress), 30),
        DemoItem::new("Progress (busy)", DemoWidget::Progress(working), 30),
        DemoItem::new("Progress (failed)", DemoWidget::Progress(failed), 30),
    ]
}

fn text() -> Vec<DemoItem> {
    let mut invalid = TextField::new()
        .with_text("bad@")
        .with_message("Enter a valid address.");
    invalid.set_state(ControlState::idle().with_validation(ValidationState::Invalid));
    vec![
        DemoItem::new(
            "Text field",
            DemoWidget::TextField(TextField::new().with_text("Editable text")),
            36,
        ),
        DemoItem::new(
            "Placeholder",
            DemoWidget::TextField(TextField::new().with_placeholder("Type here")),
            36,
        ),
        DemoItem::new(
            "Read-only",
            DemoWidget::TextField(TextField::new().with_text("Read only").read_only(true)),
            36,
        ),
        DemoItem::new("Invalid", DemoWidget::TextField(invalid), 52),
        DemoItem::new(
            "Search field",
            DemoWidget::SearchField(SearchField::new().with_placeholder("Search")),
            36,
        ),
    ]
}

fn choice() -> Vec<DemoItem> {
    let mut locked = MenuItem::new("Locked action").with_role(ControlRole::Neutral);
    locked.set_state(denied());
    let menu = Menu::new(vec![
        MenuItem::new("Open").with_shortcut("Ctrl+O"),
        MenuItem::new("Save").with_shortcut("Ctrl+S"),
        MenuItem::new("Delete").with_role(ControlRole::Destructive),
        locked,
    ]);
    vec![
        DemoItem::new(
            "Combo box",
            DemoWidget::ComboBox(
                ComboBox::new(vec!["One".into(), "Two".into(), "Three".into()]).with_selected(0),
            ),
            36,
        ),
        DemoItem::new(
            "Combo (placeholder)",
            DemoWidget::ComboBox(
                ComboBox::new(vec!["Red".into(), "Green".into(), "Blue".into()])
                    .with_placeholder("Pick a colour"),
            ),
            36,
        ),
        DemoItem::new("Menu", DemoWidget::Menu(menu), 150),
    ]
}

fn collections() -> Vec<DemoItem> {
    let selected = ListRow::new("Document.txt")
        .with_icon(IconKind::Text)
        .with_trailing("4 KB")
        .with_state(ControlState::idle().with_selection(SelectionState::Selected));
    let row = ListRow::new("Photo.png")
        .with_icon(IconKind::Image)
        .with_trailing("1.2 MB");
    let table = TableRow::new(vec![
        TableCell::new("Report"),
        TableCell::new("Document"),
        TableCell::numeric("12 KB"),
    ]);
    let card = Card::new("Backup")
        .with_body("Completed at 12:44")
        .with_count(3);
    let panel = Panel::new("Details").with_actions(vec![
        Button::new(ButtonContent::Label("Apply".into()), ControlRole::Primary),
        Button::new(ButtonContent::Label("Reset".into()), ControlRole::Neutral),
    ]);
    vec![
        DemoItem::new("List row (selected)", DemoWidget::ListRow(selected), 34),
        DemoItem::new("List row", DemoWidget::ListRow(row), 34),
        DemoItem::new("Table row", DemoWidget::TableRow(table), 34),
        DemoItem::new("Card", DemoWidget::Card(card), 110),
        DemoItem::new("Panel", DemoWidget::Panel(panel), 140),
    ]
}

fn bars() -> Vec<DemoItem> {
    let mut toolbar = Toolbar::new()
        .with_icon(
            IconButton::new(IconKind::NavBack, ControlRole::Navigation),
            0,
        )
        .with_icon(
            IconButton::new(IconKind::NavForward, ControlRole::Navigation),
            0,
        )
        .with_icon(IconButton::new(IconKind::NavUp, ControlRole::Navigation), 0)
        .with_icon(IconButton::new(IconKind::Refresh, ControlRole::Neutral), 1)
        .with_split(
            SplitButton::new(ButtonContent::Label("New".into()), ControlRole::Primary),
            2,
        );
    toolbar.set_active(3);

    let model = ScrollModel::new(ScrollRange::new(1000, 300, 250), 30, 200);
    vec![
        DemoItem::new("Toolbar", DemoWidget::Toolbar(toolbar), 40),
        DemoItem::new(
            "Scroll bar (vertical)",
            DemoWidget::ScrollBar(ScrollBar::new(ScrollOrientation::Vertical, model)),
            150,
        )
        .with_width(22),
        DemoItem::new(
            "Scroll bar (horizontal)",
            DemoWidget::ScrollBar(ScrollBar::new(ScrollOrientation::Horizontal, model)),
            24,
        ),
    ]
}

fn feedback() -> Vec<DemoItem> {
    let dialog = Dialog::new("Delete file?")
        .with_message("This cannot be undone.")
        .with_actions(vec![
            Button::new(
                ButtonContent::Label("Delete".into()),
                ControlRole::Destructive,
            ),
            Button::new(ButtonContent::Label("Cancel".into()), ControlRole::Neutral),
        ]);
    let help = HelpTip::new("Writing here needs the storage capability.").with_step(Button::new(
        ButtonContent::Label("Learn more".into()),
        ControlRole::Navigation,
    ));
    vec![
        DemoItem::new("Dialog", DemoWidget::Dialog(dialog), 130),
        DemoItem::new(
            "Tooltip",
            DemoWidget::Tooltip(Tooltip::new("Rename the selected item")),
            34,
        ),
        DemoItem::new("Help tip", DemoWidget::HelpTip(help), 90),
    ]
}

fn window() -> Vec<DemoItem> {
    vec![
        DemoItem::new(
            "Close",
            DemoWidget::WindowControl(WindowControl::new(WindowControlKind::Close)),
            32,
        )
        .with_width(44),
        DemoItem::new(
            "Minimize",
            DemoWidget::WindowControl(WindowControl::new(WindowControlKind::Minimize)),
            32,
        )
        .with_width(44),
        DemoItem::new(
            "Put to back",
            DemoWidget::WindowControl(WindowControl::new(WindowControlKind::PutToBack)),
            32,
        )
        .with_width(44),
        DemoItem::new(
            "Size toggle",
            DemoWidget::WindowControl(WindowControl::new(WindowControlKind::SizeToggle)),
            32,
        )
        .with_width(44),
    ]
}
