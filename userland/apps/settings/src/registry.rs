//! The pane registry: the closed [`Category`] and [`Pane`] sets, and the one
//! ordered [`CATEGORIES`] table that defines the whole surface.
//!
//! The table is the single definition of the sidebar strip, the search index,
//! the location trail, the keyboard cursor, and which renderer a pane gets. A
//! pane cannot exist without a row, or a row without a pane — the registry's
//! own tests hold both directions — so adding a category is adding a row and
//! a renderer, never editing the shell.
//!
//! Each pane also declares what backs it, because a settings surface that
//! cannot say why a category is empty is a surface that lies about the
//! machine. [`PaneBacking`] is that declaration, and its three answers are
//! three different facts to a reader: the pane composes real controls,
//! nothing in this system can serve it at all, or the readings and writes
//! exist and this surface does not yet compose them — in which case the row
//! says where the setting is reached.

use alloc::vec::Vec;

use tairix_icon::IconKind;

use crate::facts::{ABOUT_FACTS, CLOCK_FACTS, RESOLVER_FACTS};
use crate::form::{Composition, Posture, Setting};
use crate::machine::MachineSetting;
use crate::volumes::VOLUME_FACTS;

/// One top-level entry of the sidebar: a group of related settings.
///
/// Closed: every variant has exactly one [`CATEGORIES`] row, and the strip,
/// the trail and the search index are all derived from that row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Category {
    /// What this system is, and how it starts.
    General,
    /// The desktop's light and dark appearance.
    Appearance,
    /// The desktop backdrop and its icon arrangement.
    Wallpaper,
    /// The attached screens.
    Displays,
    /// When the screen locks.
    LockScreen,
    /// What the screen shows once it is idle.
    Screensaver,
    /// The machine's power behaviour.
    Power,
    /// Interfaces, addressing and name resolution.
    Networking,
    /// Short-range radio devices.
    Bluetooth,
    /// Audio output and input.
    Sound,
    /// Which notifications reach the desktop.
    Notifications,
    /// Key layout and repeat.
    Keyboard,
    /// Pointer behaviour.
    Mouse,
    /// Touchpad behaviour.
    Trackpad,
    /// Touch input.
    Touchscreen,
    /// Printing and scanning.
    Printers,
    /// Contrast, density, motion, scale and cursor size.
    Accessibility,
    /// Language, region and civil time.
    Language,
    /// What this machine offers to others.
    Sharing,
    /// Accounts and groups.
    Users,
    /// The mounted volumes and how full they are.
    Storage,
}

/// One pane: the settings a single column of groups shows.
///
/// A category with one pane shares its name; a category with several names
/// each of them, and the sidebar discloses them beneath their category.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Pane {
    /// What this system is: identity, version, uptime, processors, memory.
    About,
    /// Whether this system starts at a text login or a graphical one.
    LoginStartup,
    /// How much memory this system may keep as caches.
    Caching,
    /// The wall clock and where it is set from.
    DateTime,
    /// The light and dark appearance and the desktop's visual density.
    Appearance,
    /// The desktop picture and the pinboard's arrangement.
    Wallpaper,
    /// The attached screens' modes, arrangement and scale.
    Displays,
    /// When the screen locks, and locking it now.
    LockScreen,
    /// What the screen shows once it is idle.
    Screensaver,
    /// Battery, sleep and thermal behaviour.
    Power,
    /// Each wired interface's state and addressing.
    Ethernet,
    /// Wireless networks.
    WiFi,
    /// The name servers this system resolves through.
    Dns,
    /// The stack-wide protocol options.
    TcpIp,
    /// Paired short-range radio devices.
    Bluetooth,
    /// Output and input levels and devices.
    Sound,
    /// Which sources may notify, and how loudly.
    Notifications,
    /// Key layout, repeat and shortcuts.
    Keyboard,
    /// Button order, speed and the double-click interval.
    Mouse,
    /// Touchpad gestures and sensitivity.
    Trackpad,
    /// Touch calibration and gestures.
    Touchscreen,
    /// Printers, print queues and scanners.
    Printers,
    /// Contrast, density, motion, scale and cursor size.
    Accessibility,
    /// Language, region and time zone.
    Language,
    /// File, screen and remote-access sharing.
    Sharing,
    /// Accounts, groups and their grants.
    Users,
    /// Each mounted volume and how full it is.
    Storage,
}

/// What backs a pane, and therefore what it draws.
///
/// No variant is a control that would change nothing: a pane states the
/// truth about the machine and offers what it actually has.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PaneBacking {
    /// Nothing in this system can serve the pane at all: what is missing, and
    /// what would have to exist for the pane to have anything to show.
    None {
        /// What this system does not have, stated plainly.
        missing: &'static str,
        /// What would have to exist before the pane can show anything.
        needs: &'static str,
    },
    /// The pane composes real controls over a live reading and a real write
    /// path, so there is no absence to state: what it draws is what it says.
    Composed(PaneContent),
    /// The readings and writes exist, and this surface does not yet compose
    /// them into controls.
    Elsewhere {
        /// What the pane will show once it composes them.
        shows: &'static str,
        /// Where the setting is read or set today, so the reader is not left
        /// looking for a surface that does not exist.
        elsewhere: &'static str,
    },
}

/// What a composed pane draws in the content column.
///
/// The three bodies the shell knows how to draw, declared here so a pane
/// cannot claim controls it composes nothing for: the backing *is* the
/// declaration, rather than a second field a row could contradict.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PaneContent {
    /// A form of settables over the desktop's own settings document.
    Form(Composition),
    /// The Wallpaper pane: its form fixed at the top of the column, with
    /// the shipped-picture gallery scrolling beneath it.
    Pictures(Composition),
    /// The mounted volumes, discovered at runtime and read-only: one card
    /// per volume rather than a fixed table of settables.
    Volumes,
    /// What this machine is: identity, version, uptime, processors and
    /// memory, all read-only.
    About,
    /// The wall clock and where its reading came from, read-only, with the
    /// one command that changes it beneath.
    Clock,
    /// The recursive name servers the stack resolves through, discovered at
    /// runtime and read-only: one row per server rather than a fixed table
    /// of slots.
    Dns,
}

/// One pane's registry row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PaneRow {
    /// Which pane this row is.
    pub pane: Pane,
    /// The stable name a launch target uses to reach this pane.
    ///
    /// Not the title: a title is what a reader reads and may be reworded,
    /// while this is what another program names, so the two are separate
    /// on purpose. Lower-case, no spaces, and unique across the registry,
    /// which the registry's own test holds.
    pub name: &'static str,
    /// The pane's own title: the trailing crumb of the location trail, and the
    /// sidebar label of a disclosed pane. Equal to its category's label for a
    /// category that holds one pane.
    pub title: &'static str,
    /// What backs the pane.
    pub backing: PaneBacking,
    /// The setting labels this pane shows, which is the whole search index
    /// under it. A pane that composes no controls declares none, so a
    /// searchable setting cannot exist without a row that shows it.
    pub settings: &'static [&'static str],
}

impl PaneRow {
    /// What this pane draws, or `None` for one that states an absence
    /// instead.
    #[must_use]
    pub const fn content(&self) -> Option<PaneContent> {
        match self.backing {
            PaneBacking::Composed(content) => Some(content),
            PaneBacking::None { .. } | PaneBacking::Elsewhere { .. } => None,
        }
    }

    /// The label of the command this pane's action band offers, or `None`
    /// for a pane that has no band.
    ///
    /// A band exists for exactly two reasons: a staged composition, whose
    /// change is made durable by one re-authenticated run, and a reading
    /// whose *subject* is changed by starting the application that owns it.
    /// An immediate pane has none — its effect is its feedback, and a stale
    /// Apply is a trap.
    #[must_use]
    pub const fn action(&self) -> Option<&'static str> {
        match self.content() {
            Some(PaneContent::Form(composition) | PaneContent::Pictures(composition)) => {
                match composition.posture() {
                    Posture::Staged => Some("Apply"),
                    Posture::Immediate => None,
                }
            }
            Some(PaneContent::Clock) => Some("Set Date & Time…"),
            Some(PaneContent::About | PaneContent::Volumes | PaneContent::Dns) | None => None,
        }
    }
}

/// One category's registry row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CategoryRow {
    /// Which category this row is.
    pub category: Category,
    /// The sidebar label and the middle crumb of the location trail.
    pub label: &'static str,
    /// The glyph the sidebar row draws. Every kind carries a first-party
    /// built-in glyph, so a sidebar row can never blank.
    pub icon: IconKind,
    /// The category's panes, in sidebar order. Never empty.
    pub panes: &'static [PaneRow],
}

impl CategoryRow {
    /// Whether the sidebar discloses this category's panes as rows of their
    /// own, which it does exactly when there is more than one to tell apart.
    #[must_use]
    pub const fn discloses(&self) -> bool {
        self.panes.len() > 1
    }

    /// The pane the category opens on: its first.
    #[must_use]
    pub fn first_pane(&self) -> Option<&'static PaneRow> {
        self.panes.first()
    }
}

impl Category {
    /// This category's registry row, or `None` for a category the table does
    /// not list — which the registry's totality test rules out.
    #[must_use]
    pub fn row(self) -> Option<&'static CategoryRow> {
        CATEGORIES.iter().find(|row| row.category == self)
    }
}

impl Pane {
    /// The pane a hand-over's name identifies, or `None` for a name the
    /// registry does not carry.
    ///
    /// The closed set is the whole vocabulary: a launch that names a place
    /// inside this application confers nothing and can reach nothing but a
    /// pane the registry already lists, so an unknown name is simply not a
    /// pane rather than an error.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        CATEGORIES
            .iter()
            .flat_map(|row| row.panes)
            .find(|row| row.name == name)
            .map(|row| row.pane)
    }

    /// The pane a launch's operands name, if they name one.
    ///
    /// A launch carries the pane as its single operand, and the runtime's
    /// argument reader has already dropped the program's own name — so the
    /// operand is the *first* of them. Reading the second instead silently
    /// loses every launch target, because there is never one there.
    ///
    /// Operands beyond the first name nothing: the launch vocabulary is one
    /// pane, and a command line carrying more is one this surface does not
    /// understand rather than one to take a guess at.
    #[must_use]
    pub fn launched(args: &[&str]) -> Option<Self> {
        match args {
            [name] => Self::named(name),
            _ => None,
        }
    }

    /// The category that holds this pane, and its row.
    #[must_use]
    pub fn locate(self) -> Option<(Category, &'static PaneRow)> {
        CATEGORIES.iter().find_map(|row| {
            row.panes
                .iter()
                .find(|pane| pane.pane == self)
                .map(|pane| (row.category, pane))
        })
    }
}

/// Where the surface is: a category and one of its panes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Location {
    /// The open category.
    pub category: Category,
    /// The pane on show, which is one of that category's.
    pub pane: Pane,
}

impl Location {
    /// The location the surface opens on: the first pane of the first
    /// category, or `None` for an empty table — which the registry's own test
    /// rules out.
    #[must_use]
    pub fn opening() -> Option<Self> {
        let row = CATEGORIES.first()?;
        Some(Self {
            category: row.category,
            pane: row.first_pane()?.pane,
        })
    }

    /// The location a hand-over's pane name identifies, or `None` for a
    /// name the registry does not carry.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        let (category, pane) = Pane::named(name)?.locate()?;
        Some(Self {
            category,
            pane: pane.pane,
        })
    }

    /// The category and pane rows this location names, or `None` when the
    /// pane does not belong to the category (fail closed — the caller draws
    /// nothing rather than guessing at a pane).
    #[must_use]
    pub fn rows(self) -> Option<(&'static CategoryRow, &'static PaneRow)> {
        let category = self.category.row()?;
        let pane = category.panes.iter().find(|row| row.pane == self.pane)?;
        Some((category, pane))
    }
}

/// One row of the sidebar strip: a category, or one of its disclosed panes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StripRow {
    /// A category's own row. Choosing it opens the category on its first pane.
    Category(Category),
    /// A disclosed pane of the category above it. Choosing it shows the pane.
    Pane(Category, Pane),
}

impl StripRow {
    /// The location choosing this row goes to, or `None` for a row the table
    /// no longer holds.
    #[must_use]
    pub fn location(self) -> Option<Location> {
        match self {
            Self::Category(category) => Some(Location {
                category,
                pane: category.row()?.first_pane()?.pane,
            }),
            Self::Pane(category, pane) => Some(Location { category, pane }),
        }
    }
}

/// The strip's rows for an `open` category and a search `query`.
///
/// With no query the strip is every category, and the open category's panes
/// disclosed beneath it. With a query it is every category the query reaches —
/// by its own label, by a pane's title, or by a setting label a pane declares
/// — with the panes that matched disclosed beneath their category, so a
/// matched setting is always reachable in one press. A query that reaches
/// nothing yields no rows, which is the honest answer.
#[must_use]
pub fn strip_rows(open: Category, query: &str) -> Vec<StripRow> {
    let mut rows = Vec::with_capacity(CATEGORIES.len());
    for row in CATEGORIES {
        if query.is_empty() {
            rows.push(StripRow::Category(row.category));
            if row.discloses() && row.category == open {
                rows.extend(
                    row.panes
                        .iter()
                        .map(|pane| StripRow::Pane(row.category, pane.pane)),
                );
            }
            continue;
        }
        let label_hit = contains_fold(row.label, query);
        let matched: Vec<&PaneRow> = row
            .panes
            .iter()
            .filter(|pane| pane_matches(pane, query))
            .collect();
        if !label_hit && matched.is_empty() {
            continue;
        }
        rows.push(StripRow::Category(row.category));
        if !row.discloses() {
            continue;
        }
        // A category reached only by its own label offers every pane, because
        // the reader has not said which; one reached through its panes offers
        // exactly those.
        if matched.is_empty() {
            rows.extend(
                row.panes
                    .iter()
                    .map(|pane| StripRow::Pane(row.category, pane.pane)),
            );
        } else {
            rows.extend(
                matched
                    .iter()
                    .map(|pane| StripRow::Pane(row.category, pane.pane)),
            );
        }
    }
    rows
}

/// Whether `query` reaches `pane`: its title, or any setting label it shows.
fn pane_matches(pane: &PaneRow, query: &str) -> bool {
    contains_fold(pane.title, query)
        || pane
            .settings
            .iter()
            .any(|setting| contains_fold(setting, query))
}

/// Whether `haystack` contains `needle`, comparing ASCII letters without
/// regard to case.
///
/// Case folding beyond ASCII would need a table this surface has no business
/// carrying, so the comparison is byte-wise over ASCII and exact elsewhere;
/// every label in the table is ASCII.
fn contains_fold(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    let hay = haystack.as_bytes();
    hay.len() >= needle.len()
        && (0..=hay.len() - needle.len()).any(|start| {
            hay[start..start + needle.len()]
                .iter()
                .zip(needle)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
}

/// The Login & startup pane's setting labels.
const LOGIN_SETTINGS: &[&str] = &[MachineSetting::LoginType.label()];

/// The TCP/IP pane's setting labels.
const TCP_IP_SETTINGS: &[&str] = &[
    MachineSetting::NetIpv4Enabled.label(),
    MachineSetting::NetIpv6Enabled.label(),
    MachineSetting::NetIpv6Privacy.label(),
    MachineSetting::NetTcpSynCookies.label(),
    MachineSetting::NetTcpKeepalive.label(),
    MachineSetting::NetTcpEcn.label(),
];

/// The Caching pane's setting labels.
const CACHING_SETTINGS: &[&str] = &[
    MachineSetting::CacheAll.label(),
    MachineSetting::CacheFilesystem.label(),
    MachineSetting::CacheBlock.label(),
    MachineSetting::CacheTransform.label(),
    MachineSetting::CacheSemantic.label(),
];

/// The Appearance pane's setting labels.
const APPEARANCE_SETTINGS: &[&str] = &[
    Setting::Appearance.label(),
    Setting::Contrast.label(),
    Setting::Density.label(),
    Setting::Motion.label(),
    Setting::Scale.label(),
];

/// The Accessibility pane's setting labels: the shared rows plus the
/// pointer's own two.
const ACCESSIBILITY_SETTINGS: &[&str] = &[
    Setting::Contrast.label(),
    Setting::Density.label(),
    Setting::Scale.label(),
    Setting::Motion.label(),
    Setting::CursorSet.label(),
    Setting::CursorSize.label(),
];

/// The Wallpaper pane's setting labels: its four rows, plus the picture
/// the gallery beneath them chooses.
const WALLPAPER_SETTINGS: &[&str] = &[
    "Desktop picture",
    Setting::Fit.label(),
    Setting::Backdrop.label(),
    Setting::Icons.label(),
    Setting::Sort.label(),
];

/// Every category, in sidebar order, with its panes.
///
/// The single definition of the whole surface. The order is the reading order
/// the desktop presents: what the system is, then how it looks, then the
/// screen, then the machine's own behaviour, then its connections, then how it
/// is driven, then who uses it and what it holds.
pub const CATEGORIES: &[CategoryRow] = &[
    CategoryRow {
        category: Category::General,
        label: "General",
        icon: IconKind::Settings,
        panes: &[
            PaneRow {
                pane: Pane::About,
                name: "about",
                title: "About",
                backing: PaneBacking::Composed(PaneContent::About),
                settings: ABOUT_FACTS,
            },
            PaneRow {
                pane: Pane::LoginStartup,
                name: "login-startup",
                title: "Login & startup",
                backing: PaneBacking::Composed(PaneContent::Form(Composition::LoginStartup)),
                settings: LOGIN_SETTINGS,
            },
            PaneRow {
                pane: Pane::Caching,
                name: "caching",
                title: "Caching",
                backing: PaneBacking::Composed(PaneContent::Form(Composition::Caching)),
                settings: CACHING_SETTINGS,
            },
            PaneRow {
                pane: Pane::DateTime,
                name: "date-time",
                title: "Date & Time",
                backing: PaneBacking::Composed(PaneContent::Clock),
                settings: CLOCK_FACTS,
            },
        ],
    },
    CategoryRow {
        category: Category::Appearance,
        label: "Appearance",
        icon: IconKind::Appearance,
        panes: &[PaneRow {
            pane: Pane::Appearance,
            name: "appearance",
            title: "Appearance",
            backing: PaneBacking::Composed(PaneContent::Form(Composition::Appearance)),
            settings: APPEARANCE_SETTINGS,
        }],
    },
    CategoryRow {
        category: Category::Wallpaper,
        label: "Wallpaper",
        icon: IconKind::Wallpaper,
        panes: &[PaneRow {
            pane: Pane::Wallpaper,
            name: "wallpaper",
            title: "Wallpaper",
            backing: PaneBacking::Composed(PaneContent::Pictures(Composition::Wallpaper)),
            settings: WALLPAPER_SETTINGS,
        }],
    },
    CategoryRow {
        category: Category::Displays,
        label: "Displays",
        icon: IconKind::Display,
        panes: &[PaneRow {
            pane: Pane::Displays,
            name: "displays",
            title: "Displays",
            backing: PaneBacking::None {
                missing: "This system cannot change how a screen is driven: the display \
                          interface can be asked what a screen is doing and told to present to \
                          it, but it lists no modes and sets none. Nothing sets the interface \
                          scale either.",
                needs: "Mode enumeration and mode setting in the display interface, with driver \
                        support behind them.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::LockScreen,
        label: "Lock Screen",
        icon: IconKind::LockScreen,
        panes: &[PaneRow {
            pane: Pane::LockScreen,
            name: "lock-screen",
            title: "Lock Screen",
            backing: PaneBacking::None {
                missing: "This desktop keeps no idle time, so there is no moment for it to lock \
                          the screen after. Locking it now is on the icon bar's system menu, and \
                          unlocking always asks for this account's password.",
                needs: "An idle deadline in the desktop session.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Screensaver,
        label: "Screensaver",
        icon: IconKind::Screensaver,
        panes: &[PaneRow {
            pane: Pane::Screensaver,
            name: "screensaver",
            title: "Screensaver",
            backing: PaneBacking::None {
                missing: "This desktop keeps no idle time, so there is no moment for it to blank \
                          or cover the screen after.",
                needs: "An idle deadline in the desktop session.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Power,
        label: "Power",
        icon: IconKind::Power,
        panes: &[PaneRow {
            pane: Pane::Power,
            name: "power",
            title: "Power",
            backing: PaneBacking::None {
                missing: "This system reads no power supply, battery or temperature, and has no \
                          driver that could report one. Restarting and shutting down are on the \
                          icon bar's system menu.",
                needs: "A sensor interface, a driver to serve it, and a firmware sleep path.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Networking,
        label: "Networking",
        icon: IconKind::Network,
        panes: &[
            PaneRow {
                pane: Pane::Ethernet,
                name: "ethernet",
                title: "Ethernet",
                backing: PaneBacking::Elsewhere {
                    shows: "How each wired interface is addressed: its method, static address, \
                            gateway and MTU.",
                    elsewhere: "Link state, addresses and throughput are reported by the \
                                Switchboard, which is the surface that may read them: an \
                                interface's hardware identity and this machine's address book \
                                are privileged readings, and Settings deliberately holds no \
                                authority of any kind. Addressing is written with the \
                                `configure` command, by an account that may write the system \
                                configuration.",
                },
                settings: &[],
            },
            PaneRow {
                pane: Pane::WiFi,
                name: "wifi",
                title: "Wi-Fi",
                backing: PaneBacking::None {
                    missing: "This system has no wireless driver, nothing that could join a \
                              network, and no way to describe one.",
                    needs: "An 802.11 driver and a supplicant service.",
                },
                settings: &[],
            },
            PaneRow {
                pane: Pane::Dns,
                name: "dns",
                title: "DNS",
                backing: PaneBacking::Composed(PaneContent::Dns),
                settings: RESOLVER_FACTS,
            },
            PaneRow {
                pane: Pane::TcpIp,
                name: "tcp-ip",
                title: "TCP/IP",
                backing: PaneBacking::Composed(PaneContent::Form(Composition::TcpIp)),
                settings: TCP_IP_SETTINGS,
            },
        ],
    },
    CategoryRow {
        category: Category::Bluetooth,
        label: "Bluetooth",
        icon: IconKind::Bluetooth,
        panes: &[PaneRow {
            pane: Pane::Bluetooth,
            name: "bluetooth",
            title: "Bluetooth",
            backing: PaneBacking::None {
                missing: "This system has no Bluetooth support at all: nothing to reach a radio \
                          through, nothing to speak the protocol, and nowhere to remember a \
                          paired device.",
                needs: "A host-controller transport, a host stack, and a pairing store.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Sound,
        label: "Sound",
        icon: IconKind::Sound,
        panes: &[PaneRow {
            pane: Pane::Sound,
            name: "sound",
            title: "Sound",
            backing: PaneBacking::None {
                missing: "This system has no audio support at all: no sound-device driver, \
                          nothing to mix what programs play, and no way for a program to ask to \
                          play anything.",
                needs: "An audio driver class, a mixer service, and an audio interface for \
                        programs to reach it through.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Notifications,
        label: "Notifications",
        icon: IconKind::Notifications,
        panes: &[PaneRow {
            pane: Pane::Notifications,
            name: "notifications",
            title: "Notifications",
            backing: PaneBacking::None {
                missing: "The desktop shows notifications but keeps no policy for them, so there \
                          is nothing here to allow or refuse.",
                needs: "A per-source notification policy in the desktop session.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Keyboard,
        label: "Keyboard",
        icon: IconKind::Keyboard,
        panes: &[PaneRow {
            pane: Pane::Keyboard,
            name: "keyboard",
            title: "Keyboard",
            backing: PaneBacking::None {
                missing: "This system has one built-in key layout, keeps no key-repeat setting, \
                          and holds no list of the desktop's shortcuts.",
                needs: "A key-layout registry, a key-repeat policy in the desktop session, and a \
                        desktop-wide shortcut registry.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Mouse,
        label: "Mouse",
        icon: IconKind::Mouse,
        panes: &[PaneRow {
            pane: Pane::Mouse,
            name: "mouse",
            title: "Mouse",
            backing: PaneBacking::None {
                missing: "The desktop keeps no pointer policy, so there is no button order, no \
                          speed and no double-click interval to set.",
                needs: "A pointer policy in the desktop session.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Trackpad,
        label: "Trackpad",
        icon: IconKind::Trackpad,
        panes: &[PaneRow {
            pane: Pane::Trackpad,
            name: "trackpad",
            title: "Trackpad",
            backing: PaneBacking::None {
                missing: "This system has no touchpad driver: the shared input decode \
                          understands a plain mouse and nothing else.",
                needs: "A multi-touch input driver.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Touchscreen,
        label: "Touchscreen",
        icon: IconKind::Touchscreen,
        panes: &[PaneRow {
            pane: Pane::Touchscreen,
            name: "touchscreen",
            title: "Touchscreen",
            backing: PaneBacking::None {
                missing: "No touch reaches the desktop: there is no touch driver, and the shared \
                          input vocabulary has no touch event to carry one.",
                needs: "A multi-touch input driver, and a touch event in the shared input \
                        vocabulary.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Printers,
        label: "Printers & Scanners",
        icon: IconKind::Printer,
        panes: &[PaneRow {
            pane: Pane::Printers,
            name: "printers",
            title: "Printers & Scanners",
            backing: PaneBacking::None {
                missing: "This system cannot print or scan: there is nothing to hold a print \
                          queue, no way for a program to ask to scan, and no driver class for \
                          either.",
                needs: "A print spooler, a scanning interface, and a driver class for both.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Accessibility,
        label: "Accessibility",
        icon: IconKind::Accessibility,
        panes: &[PaneRow {
            pane: Pane::Accessibility,
            name: "accessibility",
            title: "Accessibility",
            backing: PaneBacking::Composed(PaneContent::Form(Composition::Accessibility)),
            settings: ACCESSIBILITY_SETTINGS,
        }],
    },
    CategoryRow {
        category: Category::Language,
        label: "Language & Region",
        icon: IconKind::Language,
        panes: &[PaneRow {
            pane: Pane::Language,
            name: "language",
            title: "Language & Region",
            backing: PaneBacking::None {
                missing: "This system ships its help in several languages but keeps no language, \
                          region or time-zone setting, and holds no civil time-zone data.",
                needs: "A language and region setting, and a compiled time-zone store.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Sharing,
        label: "Sharing",
        icon: IconKind::Sharing,
        panes: &[PaneRow {
            pane: Pane::Sharing,
            name: "sharing",
            title: "Sharing",
            backing: PaneBacking::None {
                missing: "This system offers nothing to other machines: it runs no file server, \
                          no remote-screen server, and no web server.",
                needs: "A sharing service for each thing a machine may offer.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Users,
        label: "Users & Groups",
        icon: IconKind::Users,
        panes: &[PaneRow {
            pane: Pane::Users,
            name: "users",
            title: "Users & Groups",
            backing: PaneBacking::Elsewhere {
                shows: "Every account on this system, its groups, and what it is allowed to do.",
                elsewhere: "Listed by the `users` command; an account is created with `useradd` \
                            by an account that may administer users.",
            },
            settings: &[],
        }],
    },
    CategoryRow {
        category: Category::Storage,
        label: "Storage",
        icon: IconKind::Storage,
        panes: &[PaneRow {
            pane: Pane::Storage,
            name: "storage",
            title: "Storage",
            backing: PaneBacking::Composed(PaneContent::Volumes),
            settings: VOLUME_FACTS,
        }],
    },
];
