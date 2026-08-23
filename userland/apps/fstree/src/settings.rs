//! Persisted session preferences: the configurable confirmation prompts.
//!
//! They live in this application's own app-data store, reached through
//! [`tairix_appdata`] — so they are private to `fstree`, gated on the
//! kernel-attested bundle identity, and readable or writable by no other
//! application the user launches. Nothing here spells a path, a user, or a
//! bundle identifier: the store derives all three from the identity the
//! kernel attests for this task.
//!
//! This module is the **closed registry** over the store's open key
//! namespace: the fixed set of keys the file manager reads
//! ([`SettingKey`]), their typed bridges, and nothing else. A key outside
//! the registry is one this session leaves alone rather than destroying on
//! the next save.
//!
//! Reading fails **safe**: a store the service cannot serve, an absent key,
//! or a value that is not a boolean leaves the affected setting at its
//! default — and every default keeps its confirmation *on*, so a
//! damage-limiting question is never silently lost. A refused value is
//! *named* to the caller rather than swallowed, so one broken setting costs
//! only itself and the user can be told which.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_appdata::Settings as SettingsStore;

/// One key of the closed preference registry.
///
/// Adding a key means adding a variant here, its row in [`SettingKey::ALL`],
/// its field on [`Settings`], and its arms in this module's private
/// `set_field` and `field_value` bridges — the compiler then forces every
/// consumer to state what the new key means.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SettingKey {
    /// `confirm.delete` — whether a single delete (`d`) asks first.
    ConfirmDelete,
    /// `confirm.batch-delete` — whether a batch delete over the tagged set
    /// asks first.
    ConfirmBatchDelete,
}

impl SettingKey {
    /// Every registry key, in the order the settings menu lists them.
    pub const ALL: [Self; 2] = [Self::ConfirmDelete, Self::ConfirmBatchDelete];

    /// The canonical key spelling in the store.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ConfirmDelete => "confirm.delete",
            Self::ConfirmBatchDelete => "confirm.batch-delete",
        }
    }

    /// How the settings menu labels the key.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ConfirmDelete => "confirm delete",
            Self::ConfirmBatchDelete => "confirm batch delete",
        }
    }
}

/// The persisted preferences. Every field defaults to the safe choice.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    /// Whether a single delete (`d`) asks before removing (default on).
    pub confirm_delete: bool,
    /// Whether a batch delete over the tagged set asks first (default on).
    pub confirm_batch_delete: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            confirm_delete: true,
            confirm_batch_delete: true,
        }
    }
}

impl Settings {
    /// The preferences the store's layers imply, and every key whose stored
    /// value the registry refused.
    ///
    /// A key no layer sets reads as its documented default, so a fresh
    /// account and an unreachable store both yield [`Settings::default`]. A
    /// value that is not a boolean leaves that one setting at its default and
    /// is named in the returned list, so the caller reports the broken
    /// setting instead of running on a value the user cannot account for.
    #[must_use]
    pub fn load(store: &SettingsStore<'_>) -> (Self, Vec<SettingKey>) {
        let mut settings = Self::default();
        let mut refused = Vec::new();
        for key in SettingKey::ALL {
            match store.bool(key.name()) {
                Ok(Some(value)) => set_field(&mut settings, key, value),
                Ok(None) => {}
                Err(_) => refused.push(key),
            }
        }
        (settings, refused)
    }

    /// Publish these preferences, writing only what the store's layers do not
    /// already imply.
    ///
    /// A key whose effective value already matches is left alone, so saving
    /// preferences the user did not change rewrites nothing and a value that
    /// comes from the machine's policy is never copied up into the user's own
    /// document. Both settings land as one atomic commit.
    ///
    /// # Errors
    ///
    /// The app-data service's own typed refusal — no service bound, no store
    /// for a caller running no signed bundle, or an unreachable volume. The
    /// edits stay staged, so a caller may retry.
    pub fn save(&self, store: &mut SettingsStore<'_>) -> Result<(), Errno> {
        // The comparison is on the decoded *meaning*, not on the rendered
        // text, which is what makes it more than the client's own no-op
        // check: a layer beneath may spell the same boolean `off` where a
        // write renders `false`, and shadowing a policy value with an
        // equal one is exactly the copying-up the store exists to avoid.
        let (stored, _) = Self::load(store);
        for key in SettingKey::ALL {
            let value = field_value(*self, key);
            if field_value(stored, key) == value {
                continue;
            }
            // The registry's own spellings are inside the format's grammar and
            // a boolean renders as one of two fixed words, so a refusal here
            // would be a defect in this module rather than a user's mistake;
            // it is reported as a refused write either way.
            store
                .set_bool(key.name(), value)
                .map_err(|_| Errno::OutOfRange)?;
        }
        store.commit()
    }

    /// Whether `key` is currently on.
    #[must_use]
    pub const fn is_on(self, key: SettingKey) -> bool {
        field_value(self, key)
    }

    /// Flip `key`.
    pub fn toggle(&mut self, key: SettingKey) {
        set_field(self, key, !field_value(*self, key));
    }
}

/// Set `key` on `settings`.
fn set_field(settings: &mut Settings, key: SettingKey, value: bool) {
    match key {
        SettingKey::ConfirmDelete => settings.confirm_delete = value,
        SettingKey::ConfirmBatchDelete => settings.confirm_batch_delete = value,
    }
}

/// The current value of `key` on `settings`.
const fn field_value(settings: Settings, key: SettingKey) -> bool {
    match key {
        SettingKey::ConfirmDelete => settings.confirm_delete,
        SettingKey::ConfirmBatchDelete => settings.confirm_batch_delete,
    }
}
