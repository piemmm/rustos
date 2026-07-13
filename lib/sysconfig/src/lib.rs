//! The boot-time system-configuration store engine.
//!
//! RustOS keeps its administrator-settable boot-time configuration in one
//! text document on the encrypted root volume,
//! [`CONFIG_PATH`](`/System/Settings/Configuration/system.conf`). This crate
//! is the **single definition** of that document: the line grammar, the
//! closed key registry, each key's typed value set, the fail-closed parser,
//! and the canonical render. The `configure` command app writes the store
//! through this engine and every boot-time consumer (the login service's
//! `os.loginType`, today) reads it through the same engine, so the two can
//! never diverge.
//!
//! The store is parsed only **after** the operator's `Root filesystem
//! passphrase:` unlocks the encrypted root — it lives inside
//! `/System/Settings`, which does not exist before the mount — so a
//! pre-unlock consumer simply runs on defaults ([`SystemConfig::default`]).
//!
//! # Grammar
//!
//! The text is a sequence of lines. A `#` begins a comment that runs to the
//! end of the line; blank and comment-only lines are ignored. Every other
//! line is one setting: a key from the closed registry, whitespace, and a
//! single value from that key's closed value set. Keys may appear at most
//! once. The registry today:
//!
//! * `os.loginType` — `text` (default) or `graphical`: which session type
//!   the login service offers as the boot default (`plans/DISPLAY.md` D7d;
//!   a graphical default still degrades to text when no desktop session is
//!   installed — never an error).
//!
//! # Security
//!
//! The store text is **untrusted input** to every consumer: the parser is
//! bounded ([`MAX_CONFIG_LEN`]), allocation-free, and fails closed
//! ([`ConfigError`]) on anything it does not fully understand — an unknown
//! key, a value outside the key's set, a duplicate, or an oversized
//! document. A boot-time consumer that cannot fully parse the store runs on
//! defaults rather than guessing at a partial intent; the write path
//! (`configure`) refuses the edit outright. The engine itself performs no
//! I/O and holds no authority: reading and writing the file go through the
//! secured VFS under the caller's own kernel-attested identity, so only a
//! principal the per-inode policy admits (the system administrator) can
//! change the store.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use core::fmt;

/// The directory that holds the boot-time configuration store, inside the
/// writable `/System/Settings` subtree of the encrypted root volume.
pub const CONFIG_DIR: &str = "/System/Settings/Configuration";

/// The configuration store document the `configure` command writes and
/// every boot-time consumer reads.
pub const CONFIG_PATH: &str = "/System/Settings/Configuration/system.conf";

/// Maximum length, in bytes, of a store text [`SystemConfig::parse`] will
/// consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the store is tiny, and an unboundedly large one is
/// a defect, not a workload.
pub const MAX_CONFIG_LEN: usize = 4096;

/// Which session type the login service starts for an authenticated user
/// (`os.loginType`). System policy, never a per-login prompt.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum LoginType {
    /// The text login: the authenticated account's shell — the default,
    /// and the value an absent store implies. A shell user starts the
    /// desktop on demand with the `desktop` command.
    #[default]
    Text,
    /// The graphical login: an authenticated user's session starts the
    /// desktop directly when one is installed (degrading to text when
    /// none is — never an error).
    Graphical,
}

impl LoginType {
    /// The canonical value spelling (`text` / `graphical`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Graphical => "graphical",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (values are case-sensitive — the canonical spelling only, so a store
    /// document has exactly one valid form).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "graphical" => Some(Self::Graphical),
            _ => None,
        }
    }
}

/// One key of the closed configuration registry.
///
/// Adding a key means adding a variant here, its row in [`Key::ALL`], its
/// field on [`SystemConfig`], and its arms below — the compiler then forces
/// every consumer to state what the new key means for it. There is no
/// free-form key namespace: an unknown key fails closed at parse and at
/// `configure`-time alike.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    /// `os.loginType` — the login service's boot-default session type.
    LoginType,
}

impl Key {
    /// Every registry key, in the canonical listing (and render) order.
    pub const ALL: &'static [Self] = &[Self::LoginType];

    /// The canonical key spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LoginType => "os.loginType",
        }
    }

    /// Decode a key spelling; `None` for anything outside the registry
    /// (keys are case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|key| key.name() == name)
    }

    /// The key's closed value set, in canonical order, for diagnostics and
    /// the `configure` listing.
    #[must_use]
    pub const fn values(self) -> &'static [&'static str] {
        match self {
            Self::LoginType => &["text", "graphical"],
        }
    }
}

/// Why a store text (or a single setting) was refused.
///
/// Every variant is a fail-closed refusal: the parser yields no
/// [`SystemConfig`] and a writer applies nothing, rather than guess at a
/// malformed or partial intent.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// The store text is longer than [`MAX_CONFIG_LEN`].
    TooLong,
    /// A line names a key outside the closed registry.
    UnknownKey,
    /// A line's value is outside its key's closed value set.
    InvalidValue,
    /// A registry key appeared more than once.
    DuplicateKey,
    /// A line names a key but carries no value.
    MissingValue,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::TooLong => "configuration exceeds the maximum length",
            Self::UnknownKey => "configuration names an unknown key",
            Self::InvalidValue => "a configuration value is outside its key's set",
            Self::DuplicateKey => "configuration repeats a key",
            Self::MissingValue => "a configuration key is missing its value",
        };
        f.write_str(message)
    }
}

/// A parsed, validated system configuration.
///
/// [`SystemConfig::default`] is the configuration an **absent** store
/// implies — every key at its documented default — so a consumer that finds
/// no store file (a fresh installation, a boot before the root unlock) runs
/// on defaults without a special case.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SystemConfig {
    /// The login service's boot-default session type (`os.loginType`).
    pub login_type: LoginType,
}

impl SystemConfig {
    /// Parse and validate a store `text`.
    ///
    /// # Errors
    ///
    /// Returns the matching [`ConfigError`] if `text` exceeds
    /// [`MAX_CONFIG_LEN`], names a key outside the registry, carries a
    /// value outside a key's set, repeats a key, or gives a key no value.
    /// The parser fails closed: a store it cannot fully understand yields
    /// no [`SystemConfig`].
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.len() > MAX_CONFIG_LEN {
            return Err(ConfigError::TooLong);
        }

        let mut config = Self::default();
        let mut seen = [false; Key::ALL.len()];

        for raw in text.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }

            let mut fields = line.splitn(2, char::is_whitespace);
            let name = fields.next().unwrap_or_default();
            let value = fields.next().map(str::trim).filter(|v| !v.is_empty());

            let key = Key::from_name(name).ok_or(ConfigError::UnknownKey)?;
            let value = value.ok_or(ConfigError::MissingValue)?;

            let index = Key::ALL
                .iter()
                .position(|k| *k == key)
                .ok_or(ConfigError::UnknownKey)?;
            if seen[index] {
                return Err(ConfigError::DuplicateKey);
            }
            seen[index] = true;

            config.set(key, value)?;
        }

        Ok(config)
    }

    /// The current value of `key`, in its canonical spelling.
    #[must_use]
    pub const fn get(&self, key: Key) -> &'static str {
        match key {
            Key::LoginType => self.login_type.as_str(),
        }
    }

    /// Set `key` to the setting `value` names.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidValue`] when `value` is outside the
    /// key's closed set; the configuration is left unchanged (never
    /// partially applied).
    pub fn set(&mut self, key: Key, value: &str) -> Result<(), ConfigError> {
        match key {
            Key::LoginType => {
                self.login_type = LoginType::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
        }
        Ok(())
    }

    /// Render the canonical store text: the explanatory header comment and
    /// one `key value` line per registry key, in [`Key::ALL`] order.
    ///
    /// Every key is written — including keys still at their default — so
    /// the document a user opens always shows the whole registry, and a
    /// render/parse round trip is exact.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# RustOS boot-time system configuration.\n\
             # Managed by the `configure` command; parsed after the root\n\
             # filesystem is unlocked. One `key value` setting per line.\n",
        );
        for key in Key::ALL {
            out.push_str(key.name());
            out.push(' ');
            out.push_str(self.get(*key));
            out.push('\n');
        }
        out
    }
}

/// Return the portion of `line` before its first `#`, dropping an inline or
/// whole-line comment. No registry key or value contains `#`, so cutting at
/// the first one is unambiguous.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(index) => &line[..index],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::format;
    use std::string::String;

    use super::{ConfigError, Key, LoginType, SystemConfig, CONFIG_PATH, MAX_CONFIG_LEN};

    #[test]
    fn an_empty_store_is_the_default_configuration() {
        assert_eq!(SystemConfig::parse(""), Ok(SystemConfig::default()));
        assert_eq!(SystemConfig::default().login_type, LoginType::Text);
    }

    #[test]
    fn login_type_parses_both_values() {
        let config = SystemConfig::parse("os.loginType graphical\n").expect("parses");
        assert_eq!(config.login_type, LoginType::Graphical);
        let config = SystemConfig::parse("os.loginType text\n").expect("parses");
        assert_eq!(config.login_type, LoginType::Text);
    }

    #[test]
    fn comments_blank_lines_and_whitespace_are_tolerated() {
        let text = "\
# a leading comment
\t
   os.loginType    graphical   # boot to the desktop
";
        let config = SystemConfig::parse(text).expect("parses");
        assert_eq!(config.login_type, LoginType::Graphical);
    }

    #[test]
    fn unknown_key_fails_closed() {
        assert_eq!(
            SystemConfig::parse("os.unknown text\n"),
            Err(ConfigError::UnknownKey),
        );
    }

    #[test]
    fn invalid_value_fails_closed() {
        assert_eq!(
            SystemConfig::parse("os.loginType desktop\n"),
            Err(ConfigError::InvalidValue),
        );
        // Values are case-sensitive: one canonical spelling.
        assert_eq!(
            SystemConfig::parse("os.loginType Graphical\n"),
            Err(ConfigError::InvalidValue),
        );
    }

    #[test]
    fn missing_value_fails_closed() {
        assert_eq!(
            SystemConfig::parse("os.loginType\n"),
            Err(ConfigError::MissingValue),
        );
        assert_eq!(
            SystemConfig::parse("os.loginType   # no value\n"),
            Err(ConfigError::MissingValue),
        );
    }

    #[test]
    fn duplicate_key_fails_closed() {
        assert_eq!(
            SystemConfig::parse("os.loginType text\nos.loginType graphical\n"),
            Err(ConfigError::DuplicateKey),
        );
    }

    #[test]
    fn an_oversized_store_is_refused_before_scanning() {
        let mut text = String::from("os.loginType text\n");
        while text.len() <= MAX_CONFIG_LEN {
            text.push_str("# padding comment line\n");
        }
        assert_eq!(SystemConfig::parse(&text), Err(ConfigError::TooLong));
    }

    #[test]
    fn render_parse_round_trips_exactly() {
        for login_type in [LoginType::Text, LoginType::Graphical] {
            let config = SystemConfig { login_type };
            assert_eq!(SystemConfig::parse(&config.render()), Ok(config));
        }
    }

    #[test]
    fn render_lists_every_registry_key() {
        let text = SystemConfig::default().render();
        for key in Key::ALL {
            assert!(text.contains(key.name()), "render omits {}", key.name());
        }
    }

    #[test]
    fn key_registry_round_trips_names_and_values() {
        for key in Key::ALL {
            assert_eq!(Key::from_name(key.name()), Some(*key));
            assert!(!key.values().is_empty());
        }
        assert_eq!(
            Key::from_name("os.LoginType"),
            None,
            "keys are case-sensitive"
        );
        assert_eq!(Key::from_name(""), None);
    }

    #[test]
    fn set_and_get_agree_with_the_typed_field() {
        let mut config = SystemConfig::default();
        config
            .set(Key::LoginType, "graphical")
            .expect("value in set");
        assert_eq!(config.login_type, LoginType::Graphical);
        assert_eq!(config.get(Key::LoginType), "graphical");
        assert_eq!(
            config.set(Key::LoginType, "bogus"),
            Err(ConfigError::InvalidValue),
        );
        // A refused set leaves the configuration unchanged.
        assert_eq!(config.login_type, LoginType::Graphical);
    }

    #[test]
    fn path_constants_are_inside_the_settings_subtree() {
        assert!(CONFIG_PATH.starts_with(super::CONFIG_DIR));
        assert!(CONFIG_PATH.starts_with("/System/Settings/"));
    }

    #[test]
    fn error_display_is_stable() {
        assert_eq!(
            format!("{}", ConfigError::UnknownKey),
            "configuration names an unknown key",
        );
    }
}
