//! The settings registry as form rows, and the one column of plates every
//! composed pane draws.
//!
//! One definition of each settable, and the panes that each select a subset
//! of it: Appearance offers light/dark and the interface axes, Accessibility
//! groups the same rows the way a reader looking for them would, and
//! Wallpaper the pinboard's own four. A reader looks for contrast in either
//! of the first two, so neither pane may carry its own copy of what contrast
//! *is* — the label, the sentence beneath it, the choices it offers, and the
//! key it writes all live here once.
//!
//! The settables span **three** stores, and a composition names which of
//! them its rows write: the desktop's own document, which the session owns
//! and adopts a change to at once; the machine's `system.conf`; and the
//! network store, both of which `configure` owns and a staged change is
//! applied to by re-running it as an account that may. One plate column
//! serves all three — a second would be more places to get focus,
//! scrolling and hit-testing right (`crate::machine` and `crate::network`
//! hold those two stores' settables themselves).
//!
//! A composition's groups are usually declared here, in a static table. A
//! networking one's are **discovered** instead, from a document only an
//! authenticated run can answer, so its rows name an interface by its
//! index in that document rather than by a name a static table could
//! carry.
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
use tairix_abi::net_ipc::NetServerAddr;
use tairix_controls::{
    ComboBox, ControlState, FieldAction, FieldControl, FieldGroup, FieldGroupAction, FieldLayout,
    FieldRow, StatusPill, TextAction, ValidationState,
};
use tairix_geometry::{Rect, Region, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey};
use tairix_netconfig::{ConfigError, IfaceKey, NetworkConfig};
use tairix_raster::Surface;
use tairix_sysconfig::SystemConfig;
use tairix_theme::{CursorSetId, SignalRole, Theme};
use tairix_users::Salt;
use tairix_util::conf::ValueShape;
use tairix_wallpaper::{
    Backdrop, CursorSize, DesktopSettings, IconFlow, IconSort, Rgb, SettingsKey, WallpaperFit,
};

use crate::accounts::{self, AccountFacts, AccountField, AccountRun, AccountSetting, Unappliable};
use crate::machine::MachineSetting;
use crate::network::{self, Addressing, Choice, IfaceSetting};
use crate::stack;

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

/// Which store a row writes, and which settable of it.
///
/// A row is exactly one of these and never two: the three documents have
/// different owners, different write paths, and different apply postures,
/// so a settable that could be either would be a settable with no owner.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Owner {
    /// The desktop's own settings document, which the session owns.
    Desktop(Setting),
    /// The machine's boot-time configuration store, which `configure`
    /// owns.
    Machine(MachineSetting),
    /// One interface's entry in the network store, which `configure` owns
    /// too but which this application may not read for itself.
    Interface(IfaceSetting),
    /// One field of one account, which the user-administration tools own
    /// and which this application may read no more of than its own record.
    Account(AccountSetting),
}

/// A settable a composition **declares** in a static table, as opposed to
/// one discovered from a document at runtime.
///
/// Only these two can be named ahead of time: which interfaces and which
/// accounts exist is what an authenticated run answers, so those rows are
/// built by their own discovery path and reach the owner table already
/// placed. Keeping the declared kinds in their own type is what lets a
/// declared row be built from its settable alone, with no arm standing for
/// a row that is never built this way.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Declared {
    /// The desktop's own settings document.
    Desktop(Setting),
    /// The machine's boot-time configuration store.
    Machine(MachineSetting),
}

impl Declared {
    /// The row's leading label, which is also its search term.
    const fn label(self) -> &'static str {
        match self {
            Self::Desktop(setting) => setting.label(),
            Self::Machine(setting) => setting.label(),
        }
    }

    /// The row this settable draws from what its own store currently
    /// holds.
    fn row(self, documents: Documents<'_>) -> FieldRow {
        match self {
            Self::Desktop(setting) => setting.row(
                documents.settings,
                Offered {
                    cursor_sets: documents.cursor_sets,
                },
            ),
            Self::Machine(setting) => setting.row(documents.config),
        }
    }

    /// Which store this settable writes.
    const fn owner(self) -> Owner {
        match self {
            Self::Desktop(setting) => Owner::Desktop(setting),
            Self::Machine(setting) => Owner::Machine(setting),
        }
    }
}

/// One captioned group of a composed pane: its caption and the settings it
/// holds, in order.
struct GroupSpec {
    caption: &'static str,
    settings: &'static [Declared],
}

/// The Login & startup pane's one group.
const LOGIN_GROUPS: [GroupSpec; 1] = [GroupSpec {
    caption: "STARTUP",
    settings: &[Declared::Machine(MachineSetting::LoginType)],
}];

/// The Caching pane's groups: the master switch, then the classes it is a
/// ceiling over.
const CACHING_GROUPS: [GroupSpec; 2] = [
    GroupSpec {
        caption: "CACHING",
        settings: &[Declared::Machine(MachineSetting::CacheAll)],
    },
    GroupSpec {
        caption: "WHAT IS CACHED",
        settings: &[
            Declared::Machine(MachineSetting::CacheFilesystem),
            Declared::Machine(MachineSetting::CacheBlock),
            Declared::Machine(MachineSetting::CacheTransform),
            Declared::Machine(MachineSetting::CacheSemantic),
        ],
    },
];

/// The TCP/IP pane's groups: which address families the machine speaks,
/// then the behaviour every connection over them shares.
const TCP_IP_GROUPS: [GroupSpec; 2] = [
    GroupSpec {
        caption: "ADDRESS FAMILIES",
        settings: &[
            Declared::Machine(MachineSetting::NetIpv4Enabled),
            Declared::Machine(MachineSetting::NetIpv6Enabled),
            Declared::Machine(MachineSetting::NetIpv6Privacy),
        ],
    },
    GroupSpec {
        caption: "CONNECTIONS",
        settings: &[
            Declared::Machine(MachineSetting::NetTcpSynCookies),
            Declared::Machine(MachineSetting::NetTcpKeepalive),
            Declared::Machine(MachineSetting::NetTcpEcn),
        ],
    },
];

/// The Appearance pane's groups.
const APPEARANCE_GROUPS: [GroupSpec; 2] = [
    GroupSpec {
        caption: "APPEARANCE",
        settings: &[Declared::Desktop(Setting::Appearance)],
    },
    GroupSpec {
        caption: "INTERFACE",
        settings: &[
            Declared::Desktop(Setting::Contrast),
            Declared::Desktop(Setting::Density),
            Declared::Desktop(Setting::Motion),
            Declared::Desktop(Setting::Scale),
        ],
    },
];

/// The Accessibility pane's groups: the same settings, grouped the way a
/// reader looking for them would.
const ACCESSIBILITY_GROUPS: [GroupSpec; 3] = [
    GroupSpec {
        caption: "DISPLAY",
        settings: &[
            Declared::Desktop(Setting::Contrast),
            Declared::Desktop(Setting::Density),
            Declared::Desktop(Setting::Scale),
        ],
    },
    GroupSpec {
        caption: "MOTION",
        settings: &[Declared::Desktop(Setting::Motion)],
    },
    GroupSpec {
        caption: "POINTER",
        settings: &[
            Declared::Desktop(Setting::CursorSet),
            Declared::Desktop(Setting::CursorSize),
        ],
    },
];

/// The Wallpaper pane's one group: how the picture is placed, and how the
/// icons standing on it are arranged.
const WALLPAPER_GROUPS: [GroupSpec; 1] = [GroupSpec {
    caption: "DESKTOP",
    settings: &[
        Declared::Desktop(Setting::Fit),
        Declared::Desktop(Setting::Backdrop),
        Declared::Desktop(Setting::Icons),
        Declared::Desktop(Setting::Sort),
    ],
}];

/// How a composition's changes become durable.
///
/// Declared per composition and never improvised, so a reader learns the
/// rule once rather than per pane.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Posture {
    /// The change is cheap, reversible, and its effect is the feedback: the
    /// row commits on interaction and the desktop adopts it. There is no
    /// Apply button, because there is nothing to batch and a stale Apply is
    /// a trap.
    Immediate,
    /// The change needs re-authentication, so it is edited as a working
    /// copy and applied as one command: the pane shows which rows differ
    /// from what is in effect and offers Apply and Revert.
    Staged,
}

/// The per-interface keys the DNS pane stages: the resolver list alone.
///
/// The rest of an interface's entry is its addressing, which is the
/// Ethernet pane's; a reader who came looking for name servers is offered
/// the one key that decides them.
const DNS_KEYS: [IfaceKey; 1] = [IfaceKey::DnsServers];

/// Which settings a pane composes, in the order its groups list them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Composition {
    /// The Appearance pane.
    Appearance,
    /// The Accessibility pane.
    Accessibility,
    /// The Wallpaper pane's settings rows, beneath its picture gallery.
    Wallpaper,
    /// The Login & startup pane.
    LoginStartup,
    /// The Caching pane.
    Caching,
    /// The Networking → TCP/IP pane.
    TcpIp,
    /// The Networking → Ethernet pane: one plate per configured interface,
    /// discovered from the addressing capture.
    Ethernet,
    /// The Networking → DNS pane: the live resolver set the stack answered,
    /// then each interface's own resolver list.
    Dns,
    /// The Users & Groups pane: the caller's own record, one plate per
    /// account once an administrator has answered the listing, and the
    /// group directory.
    Users,
}

impl Composition {
    /// The groups this composition declares, which is none for one whose
    /// plates are discovered from a document instead.
    const fn groups(self) -> &'static [GroupSpec] {
        match self {
            Self::Appearance => &APPEARANCE_GROUPS,
            Self::Accessibility => &ACCESSIBILITY_GROUPS,
            Self::Wallpaper => &WALLPAPER_GROUPS,
            Self::LoginStartup => &LOGIN_GROUPS,
            Self::Caching => &CACHING_GROUPS,
            Self::TcpIp => &TCP_IP_GROUPS,
            Self::Ethernet | Self::Dns | Self::Users => &[],
        }
    }

    /// How this composition's changes become durable.
    #[must_use]
    pub const fn posture(self) -> Posture {
        match self {
            Self::Appearance | Self::Accessibility | Self::Wallpaper => Posture::Immediate,
            // Writing either of the machine's stores is a re-authenticated
            // run of the tool that owns them, which is not something to ask
            // for per pointer sample.
            Self::LoginStartup
            | Self::Caching
            | Self::TcpIp
            | Self::Ethernet
            | Self::Dns
            | Self::Users => Posture::Staged,
        }
    }

    /// Whether this composition's rows read the desktop's own settings
    /// document.
    ///
    /// What decides whether a desktop answer rebuilds them. A pane that
    /// reads none of it must not be re-derived when the session answers an
    /// apply: the rebuild would cost the whole surface for a change none
    /// of its rows names, and would discard what a reader has typed into
    /// one.
    #[must_use]
    pub(crate) const fn reads_desktop(self) -> bool {
        matches!(
            self,
            Self::Appearance | Self::Accessibility | Self::Wallpaper
        )
    }

    /// Whether this composition's rows read the machine's boot-time store.
    #[must_use]
    pub(crate) const fn reads_machine(self) -> bool {
        matches!(self, Self::LoginStartup | Self::Caching | Self::TcpIp)
    }

    /// Whether this composition's plates are discovered from the addressing
    /// capture, and so exist only once an account has answered one.
    #[must_use]
    pub(crate) const fn reads_addressing(self) -> bool {
        matches!(self, Self::Ethernet | Self::Dns)
    }

    /// Whether this composition states the live resolver set the stack
    /// answered.
    #[must_use]
    pub(crate) const fn reads_resolvers(self) -> bool {
        matches!(self, Self::Dns)
    }

    /// Whether this composition's account plates are discovered from the
    /// administrator-authenticated listing, and so exist only once an
    /// account has answered one.
    #[must_use]
    pub(crate) const fn reads_roster(self) -> bool {
        matches!(self, Self::Users)
    }

    /// The registry keys an apply from this composition renders.
    ///
    /// Only its own, because the session merges an apply over what the
    /// desktop holds: a pane that rendered the whole document would
    /// reimpose whatever the other panes happened to hold when it opened.
    /// A staged composition renders no desktop document at all.
    const fn keys(self) -> &'static [SettingsKey] {
        match self {
            Self::Appearance | Self::Accessibility => &SettingsKey::APPEARANCE,
            Self::Wallpaper => &SettingsKey::PINBOARD,
            Self::LoginStartup
            | Self::Caching
            | Self::TcpIp
            | Self::Ethernet
            | Self::Dns
            | Self::Users => &[],
        }
    }

    /// Every setting label this composition shows, which is its whole
    /// contribution to the search index.
    ///
    /// A composition whose plates are discovered contributes the subject a
    /// reader searches for rather than one term per interface: which
    /// interfaces exist is what an authenticated run answers, and the index
    /// is built before anyone has asked.
    #[must_use]
    pub fn labels(self) -> Vec<&'static str> {
        match self {
            Self::Ethernet => network::ADDRESSING_FACTS.to_vec(),
            Self::Dns => network::RESOLVER_FACTS.to_vec(),
            Self::Users => accounts::ACCOUNT_FACTS.to_vec(),
            _ => self
                .groups()
                .iter()
                .flat_map(|group| group.settings.iter().map(|declared| declared.label()))
                .collect(),
        }
    }

    /// The groups and the settable each of their rows carries, built from
    /// what each store currently holds and the choice spaces the desktop
    /// answered.
    fn build(self, documents: Documents<'_>) -> (Vec<FieldGroup>, Vec<Vec<Owner>>) {
        match self {
            Self::Ethernet => interfaces(documents, IfaceKey::ALL),
            Self::Dns => {
                let (mut groups, mut owners) = interfaces(documents, &DNS_KEYS);
                groups.insert(0, network::resolver_group(documents.resolvers));
                owners.insert(0, Vec::new());
                (groups, owners)
            }
            Self::Users => users(documents),
            _ => self.declared(documents),
        }
    }

    /// The groups a composition that declares its own draws.
    fn declared(self, documents: Documents<'_>) -> (Vec<FieldGroup>, Vec<Vec<Owner>>) {
        let mut groups = Vec::with_capacity(self.groups().len());
        let mut owners = Vec::with_capacity(self.groups().len());
        for spec in self.groups() {
            groups.push(FieldGroup::new(
                spec.caption,
                spec.settings
                    .iter()
                    .map(|declared| declared.row(documents))
                    .collect(),
            ));
            owners.push(
                spec.settings
                    .iter()
                    .map(|declared| declared.owner())
                    .collect(),
            );
        }
        (groups, owners)
    }
}

/// The Users pane's plates: the caller's own record, the accounts the
/// listing answered (or why there are none), and the group directory.
///
/// The own-account and group plates carry no owner, because neither is
/// settable here: a principal reads its own record without holding
/// anything, and a group is created and deleted by its own tools.
fn users(documents: Documents<'_>) -> (Vec<FieldGroup>, Vec<Vec<Owner>>) {
    let facts = documents.accounts;
    let (plates, settings) = accounts::roster_groups(facts, documents.staged_accounts);
    let mut groups = Vec::with_capacity(plates.len().saturating_add(2));
    let mut owners = Vec::with_capacity(plates.len().saturating_add(2));
    groups.push(accounts::own_group(&facts.own, facts.groups_slice()));
    owners.push(Vec::new());
    groups.extend(plates);
    owners.extend(
        settings
            .into_iter()
            .map(|rows| rows.into_iter().map(Owner::Account).collect()),
    );
    groups.push(accounts::groups_group(facts.groups_slice()));
    owners.push(Vec::new());
    (groups, owners)
}

/// The per-interface plates `documents` implies, with each row's owner.
fn interfaces(documents: Documents<'_>, keys: &[IfaceKey]) -> (Vec<FieldGroup>, Vec<Vec<Owner>>) {
    let (groups, settings) =
        network::interface_groups(documents.addressing, documents.staged, keys);
    (
        groups,
        settings
            .into_iter()
            .map(|rows| rows.into_iter().map(Owner::Interface).collect())
            .collect(),
    )
}

/// The stores a form's rows are built from.
///
/// The two the machine owns are [`Option`]s because they are *read*, and a
/// reading that has not landed is not the same fact as a store of defaults:
/// a row with no reading says so rather than showing a value the reader
/// could not have set.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Documents<'a> {
    /// The desktop's own settings document, which the caller always holds
    /// (an unpublished one means the documented defaults).
    pub(crate) settings: &'a DesktopSettings,
    /// The cursor sets the desktop answered with.
    pub(crate) cursor_sets: &'a [CursorSetId],
    /// The machine's boot-time configuration, or `None` while it has not
    /// been read.
    pub(crate) config: Option<&'a SystemConfig>,
    /// What the addressing capture answered: the plates a networking
    /// composition is discovered from, or what it says instead.
    pub(crate) addressing: &'a Addressing,
    /// What the reader has changed on the network store's rows since the
    /// capture, which is what each of them now says.
    pub(crate) staged: &'a [(IfaceSetting, String)],
    /// The live resolver set the stack answered with, or `None` while the
    /// reading has not landed.
    pub(crate) resolvers: Option<&'a [NetServerAddr]>,
    /// The account readings: the caller's own record, the two ungated
    /// directories, and whatever an authenticated listing answered.
    pub(crate) accounts: &'a AccountFacts,
    /// What the reader has changed on the account rows since the listing,
    /// which is what each of them now says.
    pub(crate) staged_accounts: &'a [(AccountSetting, String)],
}

/// Whether `owner`'s row holds a secret, and so is carried across a
/// rebuild rather than re-derived.
const fn secret_owner(owner: Owner) -> bool {
    match owner {
        Owner::Account(setting) => setting.field.is_secret(),
        Owner::Desktop(_) | Owner::Machine(_) | Owner::Interface(_) => false,
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
    /// The reader changed a staged row. Nothing durable happened and
    /// nothing was asked for; the pane's own action band has to re-render,
    /// because what it offers depends on whether anything now differs.
    Staged,
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
    /// The settable each row writes, indexed as `groups`.
    owners: Vec<Vec<Owner>>,
    settings: DesktopSettings,
    /// The working copy of the machine's store the staged rows edit, and
    /// `None` while it has not been read.
    config: Option<SystemConfig>,
    /// What the machine's store actually holds, so a changed row is the
    /// difference between the two rather than a flag a revert could leave
    /// set.
    config_in_effect: Option<SystemConfig>,
    /// What the addressing capture answered, which is both the document
    /// every plate was discovered from and what a staged change to the
    /// network store is measured against.
    addressing: Addressing,
    /// What the reader has changed on the network store's rows and what
    /// each now says, with the empty value meaning the document no longer
    /// declares that key.
    ///
    /// The edits rather than an edited document, because a document is only
    /// ever checked whole: an interface cannot be moved from a static
    /// address to DHCP by either half of that change alone, so a working
    /// copy the engine would accept could not hold the reader's change half
    /// made. The whole is checked once, when they apply it.
    staged: Vec<(IfaceSetting, String)>,
    /// The live resolver set the stack answered with, kept so a rebuild
    /// restates it rather than dropping back to unmeasured.
    resolvers: Option<Vec<NetServerAddr>>,
    /// The account readings the plates are discovered from and a staged
    /// change is measured against.
    accounts: AccountFacts,
    /// What the reader has changed on the account rows and what each now
    /// says.
    ///
    /// Never a password: a secret's only home is the masked entry's own
    /// bounded, self-erasing buffer, and a copy here would be a plaintext
    /// in a string that grows as it is typed.
    staged_accounts: Vec<(AccountSetting, String)>,
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
    /// The form `composition` draws for the stores in `documents`.
    #[must_use]
    pub(crate) fn new(composition: Composition, documents: Documents<'_>) -> Self {
        let mut form = Self {
            composition,
            groups: Vec::new(),
            owners: Vec::new(),
            settings: documents.settings.clone(),
            config: documents.config.cloned(),
            config_in_effect: documents.config.cloned(),
            addressing: documents.addressing.clone(),
            staged: documents.staged.to_vec(),
            resolvers: documents.resolvers.map(<[_]>::to_vec),
            accounts: documents.accounts.clone(),
            staged_accounts: documents.staged_accounts.to_vec(),
            cursor_sets: documents.cursor_sets.to_vec(),
            focus: 0,
            first: 0,
        };
        form.rebuild();
        form
    }

    /// Adopt the desktop settings the session now holds.
    ///
    /// A pane that reads the document is rebuilt from it, so an apply the
    /// session refused reverts rather than standing. A pane that reads
    /// none of it is *not*: re-deriving every row for a change no row of
    /// it names would cost the whole surface and discard what a reader has
    /// typed into one — a change must never reach state it does not name.
    pub fn adopt(&mut self, settings: &DesktopSettings) {
        self.settings = settings.clone();
        if self.composition.reads_desktop() {
            self.rebuild();
        }
    }

    /// Adopt what the machine's store now holds.
    ///
    /// The working copy goes with it: a reading that lands is the truth,
    /// and an edit staged against an older one would apply a change the
    /// reader made to a value that has since moved.
    pub fn adopt_config(&mut self, config: Option<&SystemConfig>) {
        self.config = config.cloned();
        self.config_in_effect = config.cloned();
        self.rebuild();
    }

    /// Adopt the addressing an authenticated run answered.
    ///
    /// The staged edits go with it for the same reason the machine store's
    /// working copy does: a change staged against a document that has
    /// since been re-read is a change to a value that has moved.
    pub(crate) fn adopt_addressing(&mut self, addressing: &Addressing) {
        self.addressing = addressing.clone();
        self.staged.clear();
        self.rebuild();
    }

    /// Adopt the live resolver set the stack answered with.
    pub(crate) fn adopt_resolvers(&mut self, resolvers: Option<&[NetServerAddr]>) {
        self.resolvers = resolvers.map(<[_]>::to_vec);
        self.rebuild();
    }

    /// Adopt the ungated account readings: the caller's own record and the
    /// two directories.
    ///
    /// The listing the plates were discovered from is left exactly as it
    /// is, and so are the staged edits over it: these are the *public*
    /// readings, and one landing must not throw away a change the reader
    /// has made to a privileged one they had to authenticate for.
    pub(crate) fn adopt_accounts(&mut self, facts: &AccountFacts) {
        self.accounts.adopt_public(facts.clone());
        self.rebuild();
    }

    /// Adopt the listing an administrator-authenticated run answered.
    ///
    /// The staged edits go with it for the same reason the network store's
    /// working copy does: a change staged against a listing that has since
    /// been re-read is a change to a value that has moved.
    pub(crate) fn adopt_roster(&mut self, roster: accounts::Roster) {
        self.accounts.roster = roster;
        self.staged_accounts.clear();
        self.drop_secrets();
        self.rebuild();
    }

    /// The listing a returning reader would be shown, which an apply moves
    /// on.
    pub(crate) const fn roster(&self) -> &accounts::Roster {
        &self.accounts.roster
    }

    /// Put the working copies back to what the stores hold.
    pub fn revert(&mut self) {
        self.config.clone_from(&self.config_in_effect);
        self.staged.clear();
        self.staged_accounts.clear();
        self.drop_secrets();
        self.rebuild();
    }

    /// Erase every masked entry, so reverting a pane leaves no password
    /// behind in the control that held it.
    ///
    /// The entry zeroes its own buffer when it is emptied, so clearing the
    /// text *is* the erasure; dropping the control on the following
    /// rebuild erases it again, which costs one pass and is never wrong.
    fn drop_secrets(&mut self) {
        for group in &mut self.groups {
            for row in group.rows_mut() {
                if let FieldControl::Text(entry) = row.control_mut() {
                    if entry.is_secret() {
                        entry.set_text("");
                    }
                }
            }
        }
    }

    /// The document the staged edits make of the capture.
    ///
    /// `None` where there is no capture to stage against; otherwise the
    /// engine's own verdict on the whole document, which is the only level
    /// at which it can be given — a change that moves an interface off a
    /// static address is inconsistent until both of its halves are in.
    pub(crate) fn proposal(&self) -> Option<Result<NetworkConfig, ConfigError>> {
        let captured = self.addressing.document()?;
        let mut draft = captured.edit();
        for (setting, value) in &self.staged {
            let Some(alias) = setting.alias(captured) else {
                continue;
            };
            if value.is_empty() {
                draft.unset(alias, setting.key);
            } else if let Err(err) = draft.set(alias, setting.key, value) {
                return Some(Err(err));
            }
        }
        Some(draft.commit())
    }

    /// Take `written` as the document now in effect, after a run that
    /// wrote it.
    ///
    /// Not a reading, and no substitute for one: `configure` applies every
    /// named pair or none and both sides render through the same engine,
    /// so a run that exited cleanly wrote exactly what was staged. What
    /// the pane records is that acknowledgement, for the keys it named —
    /// the only ones it claims to know. Leaving the pane drops the
    /// capture, so a reader who wants the document as it now stands asks
    /// for it again.
    pub(crate) fn adopt_written(&mut self, written: NetworkConfig) {
        self.addressing = Addressing::Listed(written);
        self.staged.clear();
        self.rebuild();
    }

    /// The addressing a returning reader would be shown, which an apply
    /// moves on.
    #[must_use]
    pub(crate) const fn addressing(&self) -> &Addressing {
        &self.addressing
    }

    /// Which settings this form composes.
    #[must_use]
    pub(crate) const fn composition(&self) -> Composition {
        self.composition
    }

    /// The per-interface edits the reader has staged, so a pane rebuilt
    /// around this form keeps them.
    #[must_use]
    pub(crate) fn staged(&self) -> &[(IfaceSetting, String)] {
        &self.staged
    }

    /// The per-account edits the reader has staged, so a pane rebuilt
    /// around this form keeps them.
    ///
    /// A password is not among them, and cannot be: it lives only in the
    /// masked entry that holds it, which a rebuild carries across rather
    /// than re-deriving.
    #[must_use]
    pub(crate) fn staged_accounts(&self) -> &[(AccountSetting, String)] {
        &self.staged_accounts
    }

    /// The store settings this form's working copy differs from what is in
    /// effect on, each with the value it would be set to.
    ///
    /// The whole of what an apply asks for, as the `<key> <value>` pairs
    /// the one elevated run takes, so every change is written together and
    /// the document is rendered once. A key the reader cleared is spelled
    /// as the empty value, which is how that registry says *remove this*.
    #[must_use]
    pub fn pending(&self) -> Vec<(String, String)> {
        self.owners
            .iter()
            .flatten()
            .filter_map(|owner| self.pending_for(*owner))
            .collect()
    }

    /// What row `owner` would have written, or `None` where it matches
    /// what is in effect.
    ///
    /// Only the two stores `configure` writes as `<key> <value>` pairs
    /// answer one: the desktop's document is posted whole and an account
    /// is changed by a command line of its own, so neither is a pair.
    fn pending_for(&self, owner: Owner) -> Option<(String, String)> {
        match owner {
            Owner::Desktop(_) | Owner::Account(_) => None,
            Owner::Machine(setting) => {
                let (working, effect) = (self.config.as_ref()?, self.config_in_effect.as_ref()?);
                let value = setting.value(working);
                (value != setting.value(effect))
                    .then(|| (String::from(setting.key().name()), String::from(value)))
            }
            Owner::Interface(setting) => {
                let captured = self.addressing.document()?;
                let value = self.edited(setting)?;
                if value == setting.held(captured).unwrap_or_default() {
                    return None;
                }
                Some((setting.name(captured)?, value))
            }
        }
    }

    /// What the reader has made `setting` say, or `None` where they have
    /// not touched it.
    fn edited(&self, setting: IfaceSetting) -> Option<String> {
        self.staged
            .iter()
            .find(|(held, _)| *held == setting)
            .map(|(_, value)| value.clone())
    }

    /// How many of group `group`'s rows differ from what is in effect.
    fn changed_in(&self, group: usize) -> usize {
        self.owners.get(group).map_or(0, |rows| {
            rows.iter()
                .enumerate()
                .filter(|(row, owner)| self.differs(group, *row, **owner))
                .count()
        })
    }

    /// How many rows across the whole pane differ from what is in effect.
    ///
    /// The one count the band states, so a pane whose changes are not
    /// `<key> <value>` pairs is counted by the same rule as one whose are.
    #[must_use]
    pub fn changes(&self) -> usize {
        (0..self.groups.len())
            .map(|group| self.changed_in(group))
            .sum()
    }

    /// Whether the row at `group`/`row` differs from what its store holds.
    ///
    /// A secret is read off the row rather than from a staged copy,
    /// because the row is the only place it is: something typed into it is
    /// a change, and an empty one is the password left alone.
    fn differs(&self, group: usize, row: usize, owner: Owner) -> bool {
        let Owner::Account(setting) = owner else {
            return self.pending_for(owner).is_some();
        };
        let Some(account) = self.accounts.roster.account(setting.account) else {
            return false;
        };
        if setting.field.is_secret() {
            return self
                .secret_at(group, row)
                .is_some_and(|typed| !typed.is_empty());
        }
        let Some(value) = self.staged_value(setting) else {
            return false;
        };
        accounts::differs(setting, value, account, self.accounts.groups_slice())
    }

    /// The text of the masked entry at `group`/`row`.
    ///
    /// Borrowed, never copied: a plaintext password in a second buffer is
    /// one no erasure can reach, and this is read on every keystroke.
    fn secret_at(&self, group: usize, row: usize) -> Option<&str> {
        let FieldControl::Text(entry) = self
            .groups
            .get(group)?
            .rows()
            .get(row)
            .map(FieldRow::control)?
        else {
            return None;
        };
        entry.is_secret().then(|| entry.text())
    }

    /// What the reader has made `setting` say, or `None` where they have
    /// not touched it.
    fn staged_value(&self, setting: AccountSetting) -> Option<&str> {
        self.staged_accounts
            .iter()
            .find(|(held, _)| *held == setting)
            .map(|(_, value)| value.as_str())
    }

    /// The one elevated run the staged account change becomes, or `None`
    /// where nothing is staged.
    ///
    /// A change spanning more than one account, or one account's password
    /// together with its other fields, is **refused**: the seam carries
    /// one program and one argv, and a change split over two runs can
    /// leave half of it durable. `salt` is the randomness a password is
    /// hashed under, drawn by the caller and consumed here.
    pub(crate) fn account_run(
        &self,
        salt: Option<Salt>,
    ) -> Option<Result<AccountRun, Unappliable>> {
        let mut named: Option<usize> = None;
        let mut changes: Vec<(AccountField, String)> = Vec::new();
        // Borrowed from the entry that holds it, so the plaintext is never
        // copied into a buffer no erasure can reach.
        let mut secret: Option<&str> = None;
        for (group, rows) in self.owners.iter().enumerate() {
            for (row, owner) in rows.iter().enumerate() {
                let Owner::Account(setting) = *owner else {
                    continue;
                };
                if !self.differs(group, row, *owner) {
                    continue;
                }
                if named
                    .replace(setting.account)
                    .is_some_and(|held| held != setting.account)
                {
                    return Some(Err(Unappliable::ManyAccounts));
                }
                if setting.field.is_secret() {
                    secret = self.secret_at(group, row);
                } else {
                    changes.push((setting.field, self.spelled(setting)?));
                }
            }
        }
        let account = self.accounts.roster.account(named?)?;
        Some(accounts::run_of(account, &changes, secret, salt))
    }

    /// The staged value for `setting` as the tool that applies it spells
    /// it.
    ///
    /// The group fields are shown by name and applied by id, so the one
    /// resolution happens here — on the way to the command line, against
    /// the same directory the row was rendered from.
    fn spelled(&self, setting: AccountSetting) -> Option<String> {
        let value = self.staged_value(setting)?;
        Some(
            accounts::spelled_for(setting.field, value, self.accounts.groups_slice())
                .unwrap_or_else(|| String::from(value)),
        )
    }

    /// Take the staged account change as the listing now holding it, after
    /// a run that exited cleanly.
    ///
    /// The listing stays rather than being dropped: re-reading it costs
    /// another password, and the tool applied every field it was given or
    /// refused the run. The masked entries are erased with the rest,
    /// because a password that is now in effect is one this window has no
    /// reason to hold.
    pub(crate) fn adopt_applied(&mut self) {
        self.accounts.adopt_applied(&self.staged_accounts);
        self.staged_accounts.clear();
        self.drop_secrets();
        self.rebuild();
    }

    /// How many rows hold a value their store would refuse.
    ///
    /// What stops an apply: the reader is shown which, and corrects or
    /// reverts it, rather than having part of their change silently
    /// dropped by the tool that writes it.
    #[must_use]
    pub(crate) fn refused(&self) -> usize {
        self.groups
            .iter()
            .flat_map(FieldGroup::rows)
            .filter(|row| row.state().validation == ValidationState::Invalid)
            .count()
    }

    /// Rebuild every row from the stores the form currently holds.
    ///
    /// A masked entry is **moved** across rather than rebuilt: it owns the
    /// only copy of what the reader has typed, so re-deriving the row
    /// would discard a password half entered, and copying the text out to
    /// restore it afterwards would put a plaintext in a second buffer.
    /// Moving the control keeps one buffer and its own erasure intact.
    fn rebuild(&mut self) {
        let carried = self.take_secrets();
        let (groups, owners) = self.composition.build(Documents {
            settings: &self.settings,
            cursor_sets: &self.cursor_sets,
            config: self.config.as_ref(),
            addressing: &self.addressing,
            staged: &self.staged,
            resolvers: self.resolvers.as_deref(),
            accounts: &self.accounts,
            staged_accounts: &self.staged_accounts,
        });
        self.groups = groups;
        self.owners = owners;
        self.restore_secrets(carried);
        self.restate_badges();
        let last = self.groups.len().saturating_sub(1);
        self.focus = self.focus.min(last);
        self.first = self.first.min(last);
    }

    /// Take every masked entry out of the rows it is in, leaving the row
    /// to be discarded by the rebuild that follows.
    ///
    /// Keyed by the settable rather than by position, because a rebuild
    /// may place the same field on a different row.
    fn take_secrets(&mut self) -> Vec<(Owner, FieldControl)> {
        let mut taken = Vec::new();
        for (group, rows) in self.owners.iter().enumerate() {
            for (row, owner) in rows.iter().enumerate() {
                // Only one the reader has typed into: an empty entry is
                // rebuilt identically, so carrying it would be a scan of
                // the whole pane for nothing.
                if !secret_owner(*owner) || self.secret_at(group, row).is_none_or(str::is_empty) {
                    continue;
                }
                let Some(held) = self
                    .groups
                    .get_mut(group)
                    .and_then(|plate| plate.rows_mut().get_mut(row))
                else {
                    continue;
                };
                taken.push((
                    *owner,
                    core::mem::replace(held.control_mut(), FieldControl::Reading(String::new())),
                ));
            }
        }
        taken
    }

    /// Put each taken masked entry back on the row its settable now
    /// occupies.
    ///
    /// An entry whose settable the rebuild no longer draws is dropped
    /// here, which erases it: a secret for an account that has left the
    /// listing is one this window has no reason to hold.
    fn restore_secrets(&mut self, carried: Vec<(Owner, FieldControl)>) {
        for (owner, control) in carried {
            let Some((group, row)) = self.locate(owner) else {
                continue;
            };
            if let Some(held) = self
                .groups
                .get_mut(group)
                .and_then(|plate| plate.rows_mut().get_mut(row))
            {
                *held.control_mut() = control;
            }
        }
    }

    /// Which group and row `owner` occupies.
    fn locate(&self, owner: Owner) -> Option<(usize, usize)> {
        self.owners.iter().enumerate().find_map(|(group, rows)| {
            rows.iter()
                .position(|held| *held == owner)
                .map(|row| (group, row))
        })
    }

    /// Say on each plate's own caption how many of its rows are staged, so
    /// the band's count names a part of the pane rather than the whole.
    ///
    /// Set rather than rebuilt: a row holding a caret must survive its own
    /// plate learning that it has changed.
    pub(crate) fn restate_badges(&mut self) {
        for index in 0..self.groups.len() {
            let badge = match self.changed_in(index) {
                0 => None,
                1 => Some(StatusPill::new("1 change").with_tone(SignalRole::Warning)),
                count => Some(
                    StatusPill::new(alloc::format!("{count} changes"))
                        .with_tone(SignalRole::Warning),
                ),
            };
            if let Some(group) = self.groups.get_mut(index) {
                group.set_badge(badge);
            }
        }
    }

    /// The physical height this form needs in a column `width` pixels wide.
    ///
    /// The width is part of the question: a row's description and a group's
    /// footnote are prose and wrap, so a narrower column costs a taller pane
    /// rather than a cut sentence.
    #[must_use]
    pub fn measured_height(&self, width: u32, scale: Scale, theme: &Theme) -> u32 {
        let gap = stack::gap(scale, theme);
        let column = stack::plate_width(width, scale, theme);
        let plates: u32 = self
            .groups
            .iter()
            .map(|group| group.measured_height(column, scale, theme))
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
        let Some(owner) = self
            .owners
            .get(group)
            .and_then(|rows| rows.get(action.row))
            .copied()
        else {
            return FormOutcome::Changed;
        };
        match action.action {
            FieldAction::Selected { index } => self.chose(owner, index),
            // An entry reports every keystroke, and what it now holds is
            // read straight back off the row: the working copy takes it
            // where the store would, and says so on the row where it would
            // not.
            FieldAction::Text(TextAction::Edited | TextAction::Submitted) => {
                self.typed(owner, group, action.row)
            }
            // Every other action a slot can report is a list opening or
            // closing, or an entry dismissed, which changes the pixels and
            // nothing else.
            _ => FormOutcome::Changed,
        }
    }

    /// Adopt the choice at `index` for `owner`.
    fn chose(&mut self, owner: Owner, index: usize) -> FormOutcome {
        match owner {
            Owner::Desktop(setting) => {
                let offered = Offered {
                    cursor_sets: &self.cursor_sets,
                };
                if !setting.adopt(index, &mut self.settings, offered) {
                    return FormOutcome::Changed;
                }
                FormOutcome::Apply(self.applied())
            }
            Owner::Machine(setting) => {
                // A working copy, never a write: the store is reached by
                // re-running the tool that owns it, and doing that per
                // pointer sample is exactly what a staged pane exists to
                // avoid.
                let Some(config) = self.config.as_mut() else {
                    return FormOutcome::Changed;
                };
                if !setting.adopt(index, config) {
                    return FormOutcome::Changed;
                }
                // The master switch is a ceiling over the rows beneath it,
                // so turning it off restates them rather than leaving four
                // rows claiming to be running.
                if setting == MachineSetting::CacheAll {
                    self.rebuild();
                }
                FormOutcome::Staged
            }
            Owner::Interface(setting) => {
                let ValueShape::Closed(values) = setting.key.shape() else {
                    return FormOutcome::Changed;
                };
                // Fails closed on an index outside the list this very
                // surface built, so a routing defect stages nothing.
                let Some(chosen) = network::choice(values, index) else {
                    return FormOutcome::Changed;
                };
                self.record(
                    setting,
                    match chosen {
                        Choice::Undeclared => String::new(),
                        Choice::Spelled(value) => String::from(value),
                    },
                );
                FormOutcome::Staged
            }
            Owner::Account(setting) => {
                // The lock state is the pane's one closed account field;
                // an index outside the list this surface built stages
                // nothing.
                let Some(word) = accounts::lock_choice(index) else {
                    return FormOutcome::Changed;
                };
                self.record_account(setting, word);
                FormOutcome::Staged
            }
        }
    }

    /// Adopt what the entry in row `row` of group `group` now holds.
    ///
    /// The row is left exactly as the reader typed it — the caret included
    /// — and only its verdict moves: a value the key does not admit is
    /// staged and marked refused rather than dropped, so the band can say
    /// there is something to correct instead of quietly applying the rest.
    fn typed(&mut self, owner: Owner, group: usize, row: usize) -> FormOutcome {
        let Some(FieldControl::Text(entry)) = self
            .groups
            .get(group)
            .and_then(|held| held.rows().get(row))
            .map(FieldRow::control)
        else {
            return FormOutcome::Changed;
        };
        let admits = match owner {
            Owner::Interface(setting) => {
                let typed = String::from(entry.text());
                let admits = network::admits(setting.key, &typed);
                self.record(setting, typed);
                admits
            }
            Owner::Account(setting) => {
                let admits = setting
                    .field
                    .admits(entry.text(), self.accounts.groups_slice());
                // A secret is left where it was typed and staged nowhere:
                // the entry is its only home, and a copy in the staged set
                // would be a plaintext password in a string that grows as
                // it is typed.
                if !setting.field.is_secret() {
                    let typed = String::from(entry.text());
                    self.record_account(setting, typed);
                }
                admits
            }
            Owner::Desktop(_) | Owner::Machine(_) => return FormOutcome::Changed,
        };
        if let Some(held) = self
            .groups
            .get_mut(group)
            .and_then(|plate| plate.rows_mut().get_mut(row))
        {
            held.set_state(ControlState {
                validation: ValidationState::of(admits),
                ..held.state()
            });
        }
        FormOutcome::Staged
    }

    /// Record what the reader has made `setting` say, replacing whatever
    /// they last made it say.
    fn record(&mut self, setting: IfaceSetting, value: String) {
        match self.staged.iter_mut().find(|(held, _)| *held == setting) {
            Some((_, held)) => *held = value,
            None => self.staged.push((setting, value)),
        }
    }

    /// The account form of [`record`](Self::record).
    ///
    /// A secret never reaches here: [`typed`](Self::typed) leaves it in
    /// the entry that holds it.
    fn record_account(&mut self, setting: AccountSetting, value: String) {
        match self
            .staged_accounts
            .iter_mut()
            .find(|(held, _)| *held == setting)
        {
            Some((_, held)) => *held = value,
            None => self.staged_accounts.push((setting, value)),
        }
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
    /// One column across every group, resolved here rather than per plate,
    /// and the expanded choice list placed against the row it belongs to.
    /// The stacking itself is the shared one every plate column uses.
    fn layouts_from(&self, first: usize, place: FormPlace<'_>) -> Vec<(usize, FieldLayout)> {
        let FormPlace {
            bounds,
            viewport,
            scale,
            theme,
        } = place;
        let column = self
            .groups
            .iter()
            .map(|group| group.slot_column(bounds, scale, theme))
            .max()
            .unwrap_or(0);
        let across = stack::plate_width(bounds.width, scale, theme);
        stack::place(bounds, first, self.groups.len(), scale, theme, |index| {
            self.groups
                .get(index)
                .map_or(0, |group| group.measured_height(across, scale, theme))
        })
        .into_iter()
        .filter_map(|(index, rect)| {
            let group = self.groups.get(index)?;
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
            Some((
                index,
                match popup {
                    Some(rect) => layout.with_popup(rect),
                    None => layout,
                },
            ))
        })
        .collect()
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
    #[must_use]
    pub fn reveal_from(&self, index: usize, place: FormPlace<'_>) -> usize {
        stack::reveal_from(
            self.first,
            index,
            place.bounds,
            self.groups.len(),
            place.scale,
            place.theme,
            |at| {
                let across = stack::plate_width(place.bounds.width, place.scale, place.theme);
                self.groups.get(at).map_or(0, |group| {
                    group.measured_height(across, place.scale, place.theme)
                })
            },
        )
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

    /// How this form's changes become durable.
    #[must_use]
    pub const fn posture(&self) -> Posture {
        self.composition.posture()
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

    /// Put `text` in group `group`'s row `row` and route the edit it
    /// reports, through the same path a keystroke takes.
    #[cfg(test)]
    pub(crate) fn type_for_test(&mut self, group: usize, row: usize, text: &str) -> FormOutcome {
        if let Some(FieldControl::Text(entry)) = self
            .groups
            .get_mut(group)
            .and_then(|plate| plate.rows_mut().get_mut(row))
            .map(FieldRow::control_mut)
        {
            entry.set_text(text);
        }
        self.acted(Some((
            group,
            tairix_controls::FieldGroupAction {
                row,
                action: FieldAction::Text(TextAction::Edited),
            },
        )))
    }

    /// Choose the value at `index` for group `group`'s row `row`, through
    /// the same adoption path a committed choice list takes.
    ///
    /// A test seam over the *routing* only: what it exercises is the
    /// working copy, the dirty set and the ceiling restatement, none of
    /// which a choice list's own keyboard mechanics (which `lib/controls`
    /// tests) has any part in.
    #[cfg(test)]
    pub(crate) fn choose_for_test(
        &mut self,
        group: usize,
        row: usize,
        index: usize,
    ) -> FormOutcome {
        self.acted(Some((
            group,
            tairix_controls::FieldGroupAction {
                row,
                action: tairix_controls::FieldAction::Selected { index },
            },
        )))
    }
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
