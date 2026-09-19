//! The desktop's settings registry as form rows.
//!
//! One definition of each settable, and the panes that each select a subset
//! of it: Appearance offers light/dark and the interface axes, Accessibility
//! groups the same rows the way a reader looking for them would, and
//! Wallpaper the pinboard's own four. A reader looks for contrast in either
//! of the first two, so neither pane may carry its own copy of what contrast
//! *is* — the label, the sentence beneath it, the choices it offers, and the
//! key it writes all live here once.
//!
//! A composition also names the **group of keys** its rows write, because
//! the session merges an apply over what the desktop holds: a pane that
//! posted the whole document would reimpose whatever the *other* panes
//! happened to hold when it opened.
//!
//! Nothing here performs I/O or holds authority. A row reports the choice the
//! reader made; the pane renders the document that choice implies and the
//! session decides whether to adopt it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::desktop::{Appearance, Contrast, Density, Motion};
use tairix_controls::{
    ComboBox, FieldAction, FieldControl, FieldGroup, FieldGroupAction, FieldLayout, FieldRow,
};
use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_raster::Surface;
use tairix_theme::{CursorSetId, Theme};
use tairix_wallpaper::{
    Backdrop, CursorSize, DesktopSettings, IconFlow, IconSort, Rgb, SettingsKey, WallpaperFit,
};

/// The UI scales the surface offers, as percentages of the reference
/// density.
///
/// A ladder rather than the whole range [`Scale`] admits: every step is one a
/// reader would choose deliberately, and a continuous control would post a
/// document per pointer sample. A desktop already set to a percentage off the
/// ladder keeps it — it is appended as its own choice rather than silently
/// rounded to a neighbour, which would change a setting the reader only came
/// to look at.
const SCALE_LADDER: [u32; 7] = [100, 125, 150, 175, 200, 250, 300];

/// The backdrop colours the backdrop row offers: the active theme's own
/// desktop colour first, then a small fixed palette of named flat colours.
///
/// A named palette rather than a free-form colour entry: the settings
/// document's backdrop is one opaque `rrggbb` value, and a closed set is a
/// complete choice with no text field to validate. A backdrop already in
/// effect that this palette does not carry is still offered, under its own
/// bare `rrggbb` spelling, so opening the pane never quietly changes the
/// colour that is already on screen.
const BACKDROP_PALETTE: [(&str, Backdrop); 6] = [
    ("Theme", Backdrop::Theme),
    ("Black", Backdrop::Colour(Rgb::new(0x00, 0x00, 0x00))),
    ("Slate", Backdrop::Colour(Rgb::new(0x2e, 0x34, 0x40))),
    ("Ocean", Backdrop::Colour(Rgb::new(0x1b, 0x3a, 0x5c))),
    ("Moss", Backdrop::Colour(Rgb::new(0x2c, 0x40, 0x2c))),
    ("Linen", Backdrop::Colour(Rgb::new(0xe8, 0xe0, 0xd8))),
];

/// A choice space the settings document cannot supply on its own.
///
/// Every other row derives its choices from the closed value set the
/// registry carries, so it needs nothing beyond the document. The pointer
/// set is the exception: which sets exist is what the desktop's store
/// holds, and this application may not read it — the session lists it and
/// answers, so the answer is threaded in here rather than guessed.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Offered<'a> {
    /// The cursor sets the desktop offers besides the always-present
    /// built-in one, in the order it listed them.
    pub cursor_sets: &'a [CursorSetId],
}

/// One settable of the desktop's settings registry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Setting {
    /// Light or dark.
    Appearance,
    /// How much separation is drawn around a control.
    Contrast,
    /// How much room a control is given.
    Density,
    /// Whether a state change is animated.
    Motion,
    /// The UI scale every logical length is resolved through.
    Scale,
    /// How the desktop picture is placed on the screen.
    Fit,
    /// The flat colour shown wherever the picture does not reach.
    Backdrop,
    /// The corner the desktop's icon grid grows from.
    Icons,
    /// The order the `Desktop` folder's icons are sorted in.
    Sort,
    /// Which cursor set the pointer is drawn from.
    CursorSet,
    /// How large the pointer is drawn.
    CursorSize,
}

impl Setting {
    /// The registry key this setting writes.
    #[must_use]
    pub const fn key(self) -> SettingsKey {
        match self {
            Self::Appearance => SettingsKey::Appearance,
            Self::Contrast => SettingsKey::Contrast,
            Self::Density => SettingsKey::Density,
            Self::Motion => SettingsKey::Motion,
            Self::Scale => SettingsKey::Scale,
            Self::Fit => SettingsKey::Fit,
            Self::Backdrop => SettingsKey::Backdrop,
            Self::Icons => SettingsKey::Icons,
            Self::Sort => SettingsKey::Sort,
            Self::CursorSet => SettingsKey::CursorSet,
            Self::CursorSize => SettingsKey::CursorSize,
        }
    }

    /// The row's leading label, which is also its search term.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Contrast => "Contrast",
            Self::Density => "Density",
            Self::Motion => "Motion",
            Self::Scale => "Interface scale",
            Self::Fit => "Fit",
            Self::Backdrop => "Backdrop",
            Self::Icons => "Icons",
            Self::Sort => "Sort",
            Self::CursorSet => "Pointer set",
            Self::CursorSize => "Pointer size",
        }
    }

    /// The sentence beneath the label: what choosing this actually does.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Appearance => "Whether windows, menus and the icon bar are drawn light or dark.",
            Self::Contrast => {
                "How strongly a control is separated from what is behind it. Monochrome tells \
                 every state apart by shape rather than by colour."
            }
            Self::Density => {
                "How much room a control is given. Nothing changes size, only spacing."
            }
            Self::Motion => {
                "Whether a state change is animated. Reduced motion still shows the change, at \
                 once rather than over time."
            }
            Self::Scale => "How large every length on the desktop is drawn.",
            Self::Fit => "How the picture is placed on the screen.",
            Self::Backdrop => {
                "The flat colour behind the picture, and instead of it wherever it does not reach."
            }
            Self::Icons => "The corner the desktop's icons are arranged from.",
            Self::Sort => "The order the Desktop folder's icons are listed in.",
            Self::CursorSet => {
                "Which artwork the pointer is drawn from. Standard is the built-in set."
            }
            Self::CursorSize => {
                "How large the pointer is drawn, on top of the interface scale above."
            }
        }
    }

    /// The choices this setting offers, in the order they are listed, and
    /// which of them the desktop currently holds.
    ///
    /// A choice list is derived from the closed set the registry carries, so
    /// a value this surface offers is always one the document accepts.
    fn choices(self, settings: &DesktopSettings, offered: Offered<'_>) -> (Vec<String>, usize) {
        match self {
            Self::Appearance => pick(&Appearance::ALL, settings.appearance, appearance_label),
            Self::Contrast => pick(&Contrast::ALL, settings.contrast, contrast_label),
            Self::Density => pick(&Density::ALL, settings.density, density_label),
            Self::Motion => pick(&Motion::ALL, settings.motion, motion_label),
            Self::Scale => scale_choices(settings.scale),
            Self::Fit => pick(&WallpaperFit::ALL, settings.fit, fit_label),
            Self::Backdrop => backdrop_choices(settings.backdrop),
            Self::Icons => pick(&IconFlow::ALL, settings.icons, icon_flow_label),
            Self::Sort => pick(&IconSort::ALL, settings.sort, icon_sort_label),
            Self::CursorSet => {
                let ladder = cursor_set_ladder(settings.cursor_set, offered.cursor_sets);
                let at = ladder
                    .iter()
                    .position(|set| *set == settings.cursor_set)
                    .unwrap_or(0);
                (
                    ladder.iter().map(|set| set.name().to_string()).collect(),
                    at,
                )
            }
            Self::CursorSize => pick(&CursorSize::ALL, settings.cursor_size, cursor_size_label),
        }
    }

    /// Write the choice at `index` onto `settings`, answering whether it
    /// named one this setting offers.
    ///
    /// Fails closed: an index outside the list this very surface built
    /// changes nothing, so a routing defect cannot post a setting the reader
    /// did not choose.
    fn adopt(self, index: usize, settings: &mut DesktopSettings, offered: Offered<'_>) -> bool {
        match self {
            Self::Appearance => set(&Appearance::ALL, index, &mut settings.appearance),
            Self::Contrast => set(&Contrast::ALL, index, &mut settings.contrast),
            Self::Density => set(&Density::ALL, index, &mut settings.density),
            Self::Motion => set(&Motion::ALL, index, &mut settings.motion),
            Self::Scale => match scale_ladder(settings.scale).get(index) {
                Some(scale) => {
                    settings.scale = *scale;
                    true
                }
                None => false,
            },
            Self::Fit => set(&WallpaperFit::ALL, index, &mut settings.fit),
            Self::Backdrop => match backdrop_ladder(settings.backdrop).get(index) {
                Some((_, backdrop)) => {
                    settings.backdrop = *backdrop;
                    true
                }
                None => false,
            },
            Self::Icons => set(&IconFlow::ALL, index, &mut settings.icons),
            Self::Sort => set(&IconSort::ALL, index, &mut settings.sort),
            Self::CursorSet => set(
                &cursor_set_ladder(settings.cursor_set, offered.cursor_sets),
                index,
                &mut settings.cursor_set,
            ),
            Self::CursorSize => set(&CursorSize::ALL, index, &mut settings.cursor_size),
        }
    }

    /// The row this setting draws, showing what the desktop currently holds.
    fn row(self, settings: &DesktopSettings, offered: Offered<'_>) -> FieldRow {
        let (choices, current) = self.choices(settings, offered);
        let mut combo = ComboBox::new(choices);
        combo.set_selected(current);
        FieldRow::new(self.label(), FieldControl::Combo(combo)).with_description(self.description())
    }
}

/// `values`' labels, and the index of `current` among them.
fn pick<T: Copy + PartialEq>(
    values: &[T],
    current: T,
    label: fn(T) -> &'static str,
) -> (Vec<String>, usize) {
    let labels = values
        .iter()
        .map(|value| label(*value).to_string())
        .collect();
    (
        labels,
        values.iter().position(|v| *v == current).unwrap_or(0),
    )
}

/// Set `field` to `values[index]`, answering whether the index named one.
fn set<T: Copy>(values: &[T], index: usize, field: &mut T) -> bool {
    match values.get(index) {
        Some(value) => {
            *field = *value;
            true
        }
        None => false,
    }
}

/// The scale ladder as it stands for a desktop currently at `current`: the
/// offered steps, with `current` appended when it is not one of them.
fn scale_ladder(current: Scale) -> Vec<Scale> {
    let mut ladder: Vec<Scale> = SCALE_LADDER
        .iter()
        .filter_map(|percent| Scale::from_percent(*percent))
        .collect();
    if !ladder.contains(&current) {
        ladder.push(current);
        ladder.sort_by_key(|scale| scale.percent());
    }
    ladder
}

/// The scale choices and which one is in effect.
fn scale_choices(current: Scale) -> (Vec<String>, usize) {
    let ladder = scale_ladder(current);
    let at = ladder
        .iter()
        .position(|scale| *scale == current)
        .unwrap_or(0);
    let labels = ladder
        .iter()
        .map(|scale| alloc::format!("{}%", scale.percent()))
        .collect();
    (labels, at)
}

/// The display label of an appearance. Distinct from the document spelling
/// on purpose: one is what a reader reads, the other what the store holds.
const fn appearance_label(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "Dark",
        Appearance::Light => "Light",
    }
}

/// The display label of a contrast policy.
const fn contrast_label(contrast: Contrast) -> &'static str {
    match contrast {
        Contrast::Normal => "Normal",
        Contrast::High => "High",
        Contrast::Monochrome => "Monochrome",
    }
}

/// The display label of a density.
const fn density_label(density: Density) -> &'static str {
    match density {
        Density::Compact => "Compact",
        Density::Normal => "Normal",
        Density::Comfortable => "Comfortable",
    }
}

/// The display label of a motion policy.
const fn motion_label(motion: Motion) -> &'static str {
    match motion {
        Motion::Full => "Full",
        Motion::Reduced => "Reduced",
    }
}

/// The display label of a wallpaper fit.
///
/// The document spells a fit as its own bare keyword; a person reading a
/// row is owed a phrase that says what will happen to their picture, so the
/// two vocabularies are deliberately separate.
const fn fit_label(fit: WallpaperFit) -> &'static str {
    match fit {
        WallpaperFit::Fill => "Fill screen",
        WallpaperFit::Fit => "Fit to screen",
        WallpaperFit::Stretch => "Stretch",
        WallpaperFit::Centre => "Centre",
        WallpaperFit::Tile => "Tile",
    }
}

/// The display label of an icon flow: the corner the first icon takes.
const fn icon_flow_label(flow: IconFlow) -> &'static str {
    match flow {
        IconFlow::Leading => "Top left",
        IconFlow::Trailing => "Top right",
    }
}

/// The display label of an icon sort order.
const fn icon_sort_label(sort: IconSort) -> &'static str {
    match sort {
        IconSort::Name => "Name",
        IconSort::Kind => "Kind",
        IconSort::Size => "Size",
        IconSort::Date => "Date",
    }
}

/// The backdrops offered to a desktop currently showing `current`:
/// [`BACKDROP_PALETTE`], plus `current` under its bare `rrggbb` spelling
/// when the palette does not carry it.
fn backdrop_ladder(current: Backdrop) -> Vec<(String, Backdrop)> {
    let mut ladder: Vec<(String, Backdrop)> = BACKDROP_PALETTE
        .iter()
        .map(|(label, backdrop)| (String::from(*label), *backdrop))
        .collect();
    if let Backdrop::Colour(rgb) = current {
        if !ladder.iter().any(|(_, offered)| *offered == current) {
            ladder.push((rgb.to_hex(), current));
        }
    }
    ladder
}

/// The cursor sets offered to a desktop currently drawing `current`: the
/// built-in set first, then whatever the desktop listed, plus `current`
/// itself when it is none of them.
///
/// The stale entry is what keeps opening the pane from quietly changing the
/// pointer: a stored set an update has since removed is still what the
/// document says, so it is offered under its own name rather than silently
/// re-read as the built-in one the desktop is drawing in its place.
fn cursor_set_ladder(current: CursorSetId, offered: &[CursorSetId]) -> Vec<CursorSetId> {
    let mut ladder: Vec<CursorSetId> = core::iter::once(CursorSetId::builtin())
        .chain(offered.iter().copied())
        .collect();
    if !ladder.contains(&current) {
        ladder.push(current);
    }
    ladder
}

/// The display label of a pointer size.
const fn cursor_size_label(size: CursorSize) -> &'static str {
    match size {
        CursorSize::Normal => "Normal",
        CursorSize::Large => "Large",
        CursorSize::Larger => "Larger",
        CursorSize::Largest => "Largest",
    }
}

/// The backdrop choices and which one is in effect.
fn backdrop_choices(current: Backdrop) -> (Vec<String>, usize) {
    let ladder = backdrop_ladder(current);
    let at = ladder
        .iter()
        .position(|(_, backdrop)| *backdrop == current)
        .unwrap_or(0);
    (ladder.into_iter().map(|(label, _)| label).collect(), at)
}

/// One captioned group of a composed pane: its caption and the settings it
/// holds, in order.
struct GroupSpec {
    caption: &'static str,
    settings: &'static [Setting],
}

/// The Appearance pane's groups.
const APPEARANCE_GROUPS: [GroupSpec; 2] = [
    GroupSpec {
        caption: "APPEARANCE",
        settings: &[Setting::Appearance],
    },
    GroupSpec {
        caption: "INTERFACE",
        settings: &[
            Setting::Contrast,
            Setting::Density,
            Setting::Motion,
            Setting::Scale,
        ],
    },
];

/// The Accessibility pane's groups: the same settings, grouped the way a
/// reader looking for them would.
const ACCESSIBILITY_GROUPS: [GroupSpec; 3] = [
    GroupSpec {
        caption: "DISPLAY",
        settings: &[Setting::Contrast, Setting::Density, Setting::Scale],
    },
    GroupSpec {
        caption: "MOTION",
        settings: &[Setting::Motion],
    },
    GroupSpec {
        caption: "POINTER",
        settings: &[Setting::CursorSet, Setting::CursorSize],
    },
];

/// The Wallpaper pane's one group: how the picture is placed, and how the
/// icons standing on it are arranged.
const WALLPAPER_GROUPS: [GroupSpec; 1] = [GroupSpec {
    caption: "DESKTOP",
    settings: &[
        Setting::Fit,
        Setting::Backdrop,
        Setting::Icons,
        Setting::Sort,
    ],
}];

/// Which settings a pane composes, in the order its groups list them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Composition {
    /// The Appearance pane.
    Appearance,
    /// The Accessibility pane.
    Accessibility,
    /// The Wallpaper pane's settings rows, beneath its picture gallery.
    Wallpaper,
}

impl Composition {
    /// The groups this composition draws.
    const fn groups(self) -> &'static [GroupSpec] {
        match self {
            Self::Appearance => &APPEARANCE_GROUPS,
            Self::Accessibility => &ACCESSIBILITY_GROUPS,
            Self::Wallpaper => &WALLPAPER_GROUPS,
        }
    }

    /// The registry keys an apply from this composition renders.
    ///
    /// Only its own, because the session merges an apply over what the
    /// desktop holds: a pane that rendered the whole document would
    /// reimpose whatever the other panes happened to hold when it opened.
    const fn keys(self) -> &'static [SettingsKey] {
        match self {
            Self::Appearance | Self::Accessibility => &SettingsKey::APPEARANCE,
            Self::Wallpaper => &SettingsKey::PINBOARD,
        }
    }

    /// Every setting label this composition shows, which is its whole
    /// contribution to the search index.
    #[must_use]
    pub fn labels(self) -> Vec<&'static str> {
        self.groups()
            .iter()
            .flat_map(|group| group.settings.iter().map(|setting| setting.label()))
            .collect()
    }

    /// The groups and the setting each of their rows carries, built from
    /// what the desktop currently holds and the choice spaces it answered.
    fn build(
        self,
        settings: &DesktopSettings,
        offered: Offered<'_>,
    ) -> (Vec<FieldGroup>, Vec<Vec<Setting>>) {
        let mut groups = Vec::with_capacity(self.groups().len());
        let mut owners = Vec::with_capacity(self.groups().len());
        for spec in self.groups() {
            groups.push(FieldGroup::new(
                spec.caption,
                spec.settings
                    .iter()
                    .map(|setting| setting.row(settings, offered))
                    .collect(),
            ));
            owners.push(spec.settings.to_vec());
        }
        (groups, owners)
    }
}

/// Which end of the group the cursor lands on when it steps into it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Landing {
    /// Stepping downward: the first row.
    First,
    /// Stepping upward: the last.
    Last,
}

/// Where a form is drawn, and what it is drawn with.
///
/// The four facts every entry point needs together: the column the groups
/// stack down, the client an expanded choice list has to fit inside, and the
/// density and theme every length and colour is resolved through.
#[derive(Copy, Clone, Debug)]
pub struct FormPlace<'a> {
    /// The pane column the groups stack down, which rides above the frame
    /// while the column is scrolled.
    pub bounds: Rect,
    /// The whole client, which an expanded choice list must fit inside.
    pub viewport: Rect,
    /// The desktop density every logical length is resolved through.
    pub scale: Scale,
    /// The theme every colour and metric comes from.
    pub theme: &'a Theme,
}

/// What routing one event to a [`Form`] concluded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FormOutcome {
    /// Nothing on screen changed.
    Idle,
    /// The form changed and must be re-presented, but asks for nothing
    /// durable — a hover, a cursor move, a list opening.
    Changed,
    /// The reader chose a value: the document that choice implies, ready to
    /// post to the desktop session.
    ///
    /// Only the keys this surface edits are rendered, because the session
    /// merges an apply over what the desktop holds: a pane that posted the
    /// whole document would reimpose whatever the other panes happened to
    /// hold when it opened.
    Apply(String),
}

/// A composed pane: the groups it draws, and the setting behind each row.
///
/// The form shows the reader's choice as soon as they make it and posts the
/// document that choice implies; it is [`adopt`](Self::adopt) that makes a
/// value durable, driven by what the desktop answers. A refused apply
/// therefore visibly reverts to what the desktop actually holds rather than
/// leaving a value on screen the next login would not restore.
pub struct Form {
    composition: Composition,
    groups: Vec<FieldGroup>,
    /// The setting each row writes, indexed as `groups`.
    owners: Vec<Vec<Setting>>,
    settings: DesktopSettings,
    /// The cursor sets the desktop answered with, kept so a rebuild offers
    /// the same choice space rather than collapsing to the built-in one.
    cursor_sets: Vec<CursorSetId>,
    /// Which group holds the keyboard cursor.
    focus: usize,
    /// The first group drawn.
    ///
    /// A form scrolls by whole groups, as the category strip scrolls by
    /// whole rows, because a plate is *placed* on the surface rather than
    /// clipped to it: a group given a negative top draws nothing and
    /// hit-tests as nothing, so sliding the column up by pixels would make
    /// the group above the fold vanish instead of scroll.
    first: usize,
}

impl Form {
    /// The form `composition` draws for the desktop `settings`, offering
    /// the cursor sets in `cursor_sets`.
    #[must_use]
    pub fn new(
        composition: Composition,
        settings: &DesktopSettings,
        cursor_sets: &[CursorSetId],
    ) -> Self {
        let (groups, owners) = composition.build(settings, Offered { cursor_sets });
        Self {
            composition,
            groups,
            owners,
            settings: settings.clone(),
            cursor_sets: cursor_sets.to_vec(),
            focus: 0,
            first: 0,
        }
    }

    /// Rebuild every row from `settings`.
    ///
    /// What the window does when the desktop answers: the values on screen
    /// become the ones the store actually holds, so an apply the session
    /// refused reverts rather than standing.
    pub fn adopt(&mut self, settings: &DesktopSettings) {
        let (groups, owners) = self.composition.build(settings, self.offered());
        self.groups = groups;
        self.owners = owners;
        self.settings = settings.clone();
        let last = self.groups.len().saturating_sub(1);
        self.focus = self.focus.min(last);
        self.first = self.first.min(last);
    }

    /// The choice spaces the document cannot supply on its own.
    fn offered(&self) -> Offered<'_> {
        Offered {
            cursor_sets: &self.cursor_sets,
        }
    }

    /// The physical height this form needs.
    ///
    /// No width: a row elides rather than wrapping, so a narrower column
    /// costs a shorter label and never a taller pane.
    #[must_use]
    pub fn measured_height(&self, scale: Scale, theme: &Theme) -> u32 {
        let gap = group_gap(scale, theme);
        let plates: u32 = self
            .groups
            .iter()
            .map(|group| group.measured_height(scale, theme))
            .fold(0, u32::saturating_add);
        let gaps =
            gap.saturating_mul(u32::try_from(self.groups.len().saturating_add(1)).unwrap_or(1));
        plates.saturating_add(gaps)
    }

    /// Draw the form into `surface` stacked down `bounds`, with any expanded
    /// choice list over the top.
    pub fn render(&self, surface: &mut Surface, place: FormPlace<'_>) {
        let placed = self.placed(place);
        for (group, layout) in &placed {
            group.render(surface, *layout, place.scale, place.theme);
        }
        // Above every plate, so a list opened on the first group is not
        // painted over by the second.
        for (group, layout) in &placed {
            group.render_popup(surface, layout.popup, place.scale, place.theme);
        }
    }

    /// Route one pointer event.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        place: FormPlace<'_>,
        damage: &mut Region,
    ) -> FormOutcome {
        let layouts = self.layouts(place);
        let mut own = tairix_controls::damage::sink();
        let mut acted = None;
        for (index, layout) in layouts {
            let Some(group) = self.groups.get_mut(index) else {
                continue;
            };
            if let Some(action) =
                group.on_pointer(event, layout, place.scale, place.theme, &mut own)
            {
                acted = Some((index, action));
            }
        }
        self.concluded(acted, &own, damage)
    }

    /// Route one key press.
    pub fn on_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        place: FormPlace<'_>,
        damage: &mut Region,
    ) -> FormOutcome {
        let seated = self
            .layouts(place)
            .into_iter()
            .find_map(|(index, layout)| (index == self.focus).then_some(layout));
        let mut own = tairix_controls::damage::sink();
        let mut acted = None;
        let mut kept = false;
        if let (Some(layout), Some(group)) = (seated, self.groups.get_mut(self.focus)) {
            let was = group.focus();
            acted = group
                .on_key(key, modifiers, layout, place.scale, place.theme, &mut own)
                .map(|action| (self.focus, action));
            // An open choice list is modal: every key is the list's until
            // it resolves, so the cursor must not step out from under it.
            let listing = group.rows().iter().any(FieldRow::popup_open);
            kept = acted.is_some() || listing || group.focus() != was;
        }
        if !kept {
            // The group clamps at its own ends and says so by not moving:
            // carrying the cursor *between* groups is the owner's job, and
            // without it every row below the first plate would be
            // unreachable from the keyboard.
            self.step_group(key, place, &mut own);
        }
        self.concluded(acted, &own, damage)
    }

    /// Move the cursor to the neighbouring group when the focused one has
    /// nothing further in the direction asked for.
    fn step_group(&mut self, key: Key, place: FormPlace<'_>, damage: &mut Region) {
        let (next, landing) = match key {
            Key::Named(NamedKey::Down) if self.focus + 1 < self.groups.len() => {
                (self.focus + 1, Landing::First)
            }
            Key::Named(NamedKey::Up) if self.focus > 0 => (self.focus - 1, Landing::Last),
            _ => return,
        };
        // Placed without a layout, because the group being stepped into may
        // not be one the column currently seats — the owner scrolls it in
        // afterwards, and a cursor that refused to leave a seated group
        // could never reach the ones past the fold.
        if let Some(group) = self.groups.get_mut(self.focus) {
            group.adopt_focus(None);
        }
        self.focus = next;
        let Some(group) = self.groups.get_mut(next) else {
            return;
        };
        let row = match landing {
            Landing::First => 0,
            Landing::Last => group.len().saturating_sub(1),
        };
        group.adopt_focus(Some(row));
        damage.add(place.bounds);
    }

    /// Which group and row the keyboard cursor is on.
    #[must_use]
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let row = self.groups.get(self.focus)?.focus()?;
        Some((self.focus, row))
    }

    /// Which group the keyboard cursor is in, so the owner can scroll it
    /// into view.
    ///
    /// A group the cursor reached but the column does not show holds
    /// controls the reader cannot use, which is a correctness property
    /// rather than a convenience.
    #[must_use]
    pub const fn focused_group(&self) -> usize {
        self.focus
    }

    /// Adopt what a group reported, folding the pixels it repainted into
    /// the caller's damage.
    ///
    /// A control that moved a highlight inside an open list asks for
    /// nothing durable and reports no action, but it has still redrawn
    /// itself — so the damage it reported, not the action, is what decides
    /// whether a frame is owed. Without that the highlight would move in
    /// memory and never reach the screen.
    fn concluded(
        &mut self,
        acted: Option<(usize, FieldGroupAction)>,
        own: &Region,
        damage: &mut Region,
    ) -> FormOutcome {
        let redrew = !own.is_empty();
        for rect in own.rects() {
            damage.add(*rect);
        }
        match self.acted(acted) {
            FormOutcome::Idle if redrew => FormOutcome::Changed,
            outcome => outcome,
        }
    }

    /// Adopt what a group reported.
    fn acted(&mut self, acted: Option<(usize, FieldGroupAction)>) -> FormOutcome {
        let Some((group, action)) = acted else {
            return FormOutcome::Idle;
        };
        let FieldAction::Selected { index } = action.action else {
            // Every other action a slot can report is the list opening or
            // closing, which changes the pixels and nothing else.
            return FormOutcome::Changed;
        };
        let Some(setting) = self
            .owners
            .get(group)
            .and_then(|rows| rows.get(action.row))
            .copied()
        else {
            return FormOutcome::Changed;
        };
        let offered = Offered {
            cursor_sets: &self.cursor_sets,
        };
        if !setting.adopt(index, &mut self.settings, offered) {
            return FormOutcome::Changed;
        }
        FormOutcome::Apply(self.applied())
    }

    /// The document this form's current values mean, over its own keys
    /// alone.
    pub(crate) fn applied(&self) -> String {
        self.settings.document_of(self.composition.keys()).render()
    }

    /// Where each group is drawn, with the shared slot column and any
    /// expanded list placed.
    ///
    /// One column across every group, so a control does not step left and
    /// right down the pane as each plate resolves its own widest choice.
    fn layouts(&self, place: FormPlace<'_>) -> Vec<(usize, FieldLayout)> {
        self.layouts_from(self.first, place)
    }

    /// The groups drawn from `first`, each with where it is placed.
    ///
    /// Only the groups that fit whole are placed: a plate half off the
    /// bottom would draw its rows over the window's edge, and a reader
    /// cannot press a row they cannot see.
    fn layouts_from(&self, first: usize, place: FormPlace<'_>) -> Vec<(usize, FieldLayout)> {
        let FormPlace {
            bounds,
            viewport,
            scale,
            theme,
        } = place;
        let gap = group_gap(scale, theme);
        let column = self
            .groups
            .iter()
            .map(|group| group.slot_column(bounds, scale, theme))
            .max()
            .unwrap_or(0);
        let mut top = bounds.top().saturating_add(to_i32(gap));
        let limit = bounds.bottom();
        let mut placed = Vec::with_capacity(self.groups.len());
        for (index, group) in self.groups.iter().enumerate().skip(first) {
            let height = group.measured_height(scale, theme);
            let rect = Rect::new(
                bounds.left().saturating_add(to_i32(gap)),
                top,
                bounds.width.saturating_sub(gap.saturating_mul(2)),
                height,
            );
            // The first group always draws, however short the column: a
            // pane that seated nothing at all would be a blank window.
            if rect.bottom() > limit && index > first {
                break;
            }
            let layout = FieldLayout::new(rect, column);
            let popup =
                group
                    .popup_anchor(layout, scale, theme)
                    .and_then(|(row, slot)| match group.rows().get(row)?.control() {
                        FieldControl::Combo(combo) => {
                            Some(combo.popup_rect(slot, viewport, scale, theme))
                        }
                        FieldControl::Toggle(_)
                        | FieldControl::Slider(_)
                        | FieldControl::Text(_)
                        | FieldControl::Button(_)
                        | FieldControl::Reading(_)
                        | FieldControl::Unmeasured(_) => None,
                    });
            placed.push((
                index,
                match popup {
                    Some(rect) => layout.with_popup(rect),
                    None => layout,
                },
            ));
            top = top.saturating_add(to_i32(height.saturating_add(gap)));
        }
        placed
    }

    /// How many groups the column seats from the one it draws from.
    #[must_use]
    pub fn seated(&self, place: FormPlace<'_>) -> usize {
        self.layouts_from(self.first, place).len()
    }

    /// How many groups this form has.
    #[must_use]
    pub fn groups_len(&self) -> usize {
        self.groups.len()
    }

    /// The first group drawn.
    #[must_use]
    pub const fn first(&self) -> usize {
        self.first
    }

    /// Draw from group `index`, clamped to the last group.
    pub fn set_first(&mut self, index: usize) {
        self.first = index.min(self.groups.len().saturating_sub(1));
    }

    /// The first group to draw from so that group `index` is seated.
    ///
    /// A group above the window becomes the first drawn; one below it
    /// becomes the last. Each step may seat a different number of groups,
    /// so the count is re-asked rather than assumed uniform.
    #[must_use]
    pub fn reveal_from(&self, index: usize, place: FormPlace<'_>) -> usize {
        if index < self.first {
            return index;
        }
        let mut want = self.first;
        while want < index
            && !self
                .layouts_from(want, place)
                .iter()
                .any(|(seated, _)| *seated == index)
        {
            want = want.saturating_add(1);
        }
        want
    }

    /// The groups paired with where they are drawn.
    fn placed(&self, place: FormPlace<'_>) -> Vec<(&FieldGroup, FieldLayout)> {
        self.layouts(place)
            .into_iter()
            .filter_map(|(index, layout)| Some((self.groups.get(index)?, layout)))
            .collect()
    }

    /// Put the keyboard cursor on the form's first row, or take it off.
    pub fn set_focused(&mut self, focused: bool) {
        if focused {
            self.focus = 0;
        }
        for (index, group) in self.groups.iter_mut().enumerate() {
            group.adopt_focus((focused && index == self.focus).then_some(0));
        }
    }

    /// The settings the form currently shows.
    #[must_use]
    pub const fn settings(&self) -> &DesktopSettings {
        &self.settings
    }

    /// The groups, for a test that asks what a pane composed.
    #[cfg(test)]
    pub(crate) fn groups(&self) -> &[FieldGroup] {
        &self.groups
    }
}

/// The gap between stacked group plates, and between them and the column's
/// own edges: the theme's control gap, so a pane breathes at whatever
/// density the desktop is drawn at.
fn group_gap(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_gap).max(1)
}

impl Form {
    /// Whether a choice list is open, which is modal: the list keeps the
    /// pointer even where it hangs outside the pane's own column.
    #[must_use]
    pub fn is_listing(&self) -> bool {
        self.groups.iter().any(|group| {
            group
                .rows()
                .iter()
                .any(tairix_controls::FieldRow::popup_open)
        })
    }
}
