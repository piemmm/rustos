//! The machine's boot-time configuration as form rows.
//!
//! The desktop's own settables live in [`crate::form`] over the session's
//! published document; these are the other store — `system.conf`, whose one
//! writer engine is `lib/sysconfig` and whose one command app is
//! `configure`. Settings grows no second writer: a change here is staged,
//! and applying it re-runs `configure` as an account that may.
//!
//! Nothing here performs I/O or holds authority. A row reports the choice
//! the reader made against a working copy of the document; the shell turns
//! the difference into the one elevated command that writes it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_controls::{ComboBox, FieldControl, FieldRow};
use tairix_sysconfig::{CacheClass, CacheMode, CacheSwitch, Key, LoginType, SystemConfig};

/// What a row states while the machine's configuration has not been read.
const UNREAD: &str = "not read yet";

/// One settable of the machine's boot-time configuration store.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MachineSetting {
    /// Whether the machine starts at a text login or a graphical one.
    LoginType,
    /// The master caching switch, which is a ceiling over every class.
    CacheAll,
    /// The filesystem cache's own switch.
    CacheFilesystem,
    /// The block cache's own switch.
    CacheBlock,
    /// The transform cache's own switch.
    CacheTransform,
    /// The launch cache's own switch.
    CacheSemantic,
}

impl MachineSetting {
    /// The store key this setting writes.
    #[must_use]
    pub const fn key(self) -> Key {
        match self {
            Self::LoginType => Key::LoginType,
            Self::CacheAll => Key::CacheAll,
            Self::CacheFilesystem => Key::CacheFilesystem,
            Self::CacheBlock => Key::CacheBlock,
            Self::CacheTransform => Key::CacheTransform,
            Self::CacheSemantic => Key::CacheSemantic,
        }
    }

    /// The cache class this setting governs, for the five that govern one.
    const fn class(self) -> Option<CacheClass> {
        match self {
            Self::CacheFilesystem => Some(CacheClass::Filesystem),
            Self::CacheBlock => Some(CacheClass::Block),
            Self::CacheTransform => Some(CacheClass::Transform),
            Self::CacheSemantic => Some(CacheClass::Semantic),
            Self::LoginType | Self::CacheAll => None,
        }
    }

    /// The row's leading label, which is also its search term.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::LoginType => "Start up to",
            Self::CacheAll => "Caching",
            Self::CacheFilesystem => "Filesystem cache",
            Self::CacheBlock => "Disk cache",
            Self::CacheTransform => "Decoded-data cache",
            Self::CacheSemantic => "Application-launch cache",
        }
    }

    /// The sentence beneath the label: what choosing this actually does.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::LoginType => {
                "Whether this machine asks for a login at a text prompt or on the desktop. A \
                 machine that cannot draw a desktop asks at the text prompt either way."
            }
            Self::CacheAll => {
                "Whether this machine may keep caches at all. Turning it off disables every \
                 cache below, whatever each of them is set to."
            }
            Self::CacheFilesystem => {
                "Directory listings and file metadata the machine has already read."
            }
            Self::CacheBlock => "Disk blocks the machine has already read.",
            Self::CacheTransform => {
                "Data the machine has already decrypted or decompressed, kept in that form."
            }
            Self::CacheSemantic => "What an application needs to start, kept ready for next time.",
        }
    }

    /// What a per-class row states while the master switch is off.
    ///
    /// The row keeps its own value, because that is what the store says and
    /// what would take effect if the master switch came back on — but a
    /// reader who saw `Automatic` and nothing else would reasonably believe
    /// the cache was running.
    const OVERRIDDEN: &'static str = "Caching is off for the whole machine, so this has no effect.";

    /// The sentence this row shows against `config`: its own, plus the
    /// ceiling where the master switch has taken it away.
    fn stated(self, config: &SystemConfig) -> String {
        let mut text = String::from(self.description());
        if self.class().is_some() && config.cache_all == CacheSwitch::Off {
            text.push(' ');
            text.push_str(Self::OVERRIDDEN);
        }
        text
    }

    /// The choices this setting offers, in order, and which of them the
    /// store currently holds.
    fn choices(self, config: &SystemConfig) -> (Vec<String>, usize) {
        match self {
            Self::LoginType => pick(
                &[LoginType::Graphical, LoginType::Text],
                config.login_type,
                login_label,
            ),
            Self::CacheAll => pick(
                &[CacheSwitch::On, CacheSwitch::Off],
                config.cache_all,
                switch_label,
            ),
            Self::CacheFilesystem
            | Self::CacheBlock
            | Self::CacheTransform
            | Self::CacheSemantic => pick(
                &[CacheMode::Auto, CacheMode::Off],
                self.mode(config),
                mode_label,
            ),
        }
    }

    /// The per-class mode this setting holds in `config`.
    ///
    /// The class's **own** value, not its effective one: the effective mode
    /// folds in the master switch, and a row that showed it would silently
    /// rewrite what the store says the moment the master switch went off.
    fn mode(self, config: &SystemConfig) -> CacheMode {
        match self {
            Self::CacheFilesystem => config.cache_filesystem,
            Self::CacheBlock => config.cache_block,
            Self::CacheTransform => config.cache_transform,
            Self::CacheSemantic => config.cache_semantic,
            Self::LoginType | Self::CacheAll => CacheMode::Auto,
        }
    }

    /// Write the choice at `index` onto `config`, answering whether it
    /// named one this setting offers.
    ///
    /// Fails closed: an index outside the list this very surface built
    /// changes nothing, so a routing defect cannot stage a value the reader
    /// did not choose.
    pub(crate) fn adopt(self, index: usize, config: &mut SystemConfig) -> bool {
        match self {
            Self::LoginType => set(
                &[LoginType::Graphical, LoginType::Text],
                index,
                &mut config.login_type,
            ),
            Self::CacheAll => set(
                &[CacheSwitch::On, CacheSwitch::Off],
                index,
                &mut config.cache_all,
            ),
            Self::CacheFilesystem => modes(index, &mut config.cache_filesystem),
            Self::CacheBlock => modes(index, &mut config.cache_block),
            Self::CacheTransform => modes(index, &mut config.cache_transform),
            Self::CacheSemantic => modes(index, &mut config.cache_semantic),
        }
    }

    /// The value `config` holds for this setting, spelled as the store
    /// spells it — which is what an elevated `configure` is handed.
    #[must_use]
    pub fn value(self, config: &SystemConfig) -> &'static str {
        match self {
            Self::LoginType => config.login_type.as_str(),
            Self::CacheAll => config.cache_all.as_str(),
            Self::CacheFilesystem
            | Self::CacheBlock
            | Self::CacheTransform
            | Self::CacheSemantic => self.mode(config).as_str(),
        }
    }

    /// The row this setting draws for `config`, or the row it draws while
    /// the store has not been read.
    ///
    /// An unread store is not a store of defaults: showing one would be a
    /// value the reader could not account for and could not have set. The
    /// row says so instead, and offers nothing to change until the reading
    /// lands.
    pub(crate) fn row(self, config: Option<&SystemConfig>) -> FieldRow {
        let Some(config) = config else {
            return FieldRow::new(self.label(), FieldControl::Unmeasured(String::from(UNREAD)))
                .with_description(self.description());
        };
        let (choices, current) = self.choices(config);
        let mut combo = ComboBox::new(choices);
        combo.set_selected(current);
        FieldRow::new(self.label(), FieldControl::Combo(combo))
            .with_description(self.stated(config))
    }
}

/// `values`' labels, and the index of `current` among them.
fn pick<T: Copy + PartialEq>(
    values: &[T],
    current: T,
    label: fn(T) -> &'static str,
) -> (Vec<String>, usize) {
    (
        values
            .iter()
            .map(|value| label(*value).to_string())
            .collect(),
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

/// The per-class mode ladder, shared by all four class rows.
fn modes(index: usize, field: &mut CacheMode) -> bool {
    set(&[CacheMode::Auto, CacheMode::Off], index, field)
}

/// The display label of a login type. Distinct from the document spelling
/// on purpose: one is what a reader reads, the other what the store holds.
const fn login_label(kind: LoginType) -> &'static str {
    match kind {
        LoginType::Graphical => "The desktop",
        LoginType::Text => "A text prompt",
    }
}

/// The display label of the master caching switch.
const fn switch_label(switch: CacheSwitch) -> &'static str {
    match switch {
        CacheSwitch::On => "On",
        CacheSwitch::Off => "Off",
    }
}

/// The display label of a per-class caching mode.
///
/// There is deliberately no forced-on choice: a class is either governed by
/// the memory-pressure governor or hard-disabled, and offering a third
/// would promise something the store cannot hold.
const fn mode_label(mode: CacheMode) -> &'static str {
    match mode {
        CacheMode::Auto => "Automatic",
        CacheMode::Off => "Off",
    }
}
