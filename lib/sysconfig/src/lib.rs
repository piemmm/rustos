//! The boot-time system-configuration store engine.
//!
//! TAIRiX keeps its administrator-settable boot-time configuration in one
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
//! * `os.loginType` — `text` or `graphical` (default): which session type
//!   the login service offers as the boot default (`plans/DISPLAY.md` D7d).
//!   The graphical default still degrades to the text prompt on a machine
//!   that cannot run one — no live display service, no desktop bundle, or
//!   no login-screen bundle — never an error.
//! * `cache.all` — `on` (default) or `off`: the master caching switch. `off`
//!   is a ceiling that disables every SMARTRAM cache regardless of the
//!   per-class settings below.
//! * `cache.filesystem`, `cache.block`, `cache.transform`, `cache.semantic` —
//!   `auto` (default) or `off`: the per-class caching switches for the four
//!   live SMARTRAM caches (`plans/SMARTRAM.md`). `auto` lets the memory-
//!   pressure governor manage the class (today's behaviour); `off` hard-
//!   disables it (a real bypass — the cache admits and holds nothing). There
//!   is deliberately no per-class `on`: a class cannot be forced to ignore
//!   memory pressure without breaking the SMARTRAM reserve invariants. The
//!   effective mode of a class is `off` whenever `cache.all` is `off`, else
//!   the class's own value (see [`SystemConfig::effective_cache`]).
//! * `net.ipv4.enabled`, `net.ipv6.enabled` — `true` (default) or `false`:
//!   the stack-wide address-family switches (`plans/NETWORK.md` section 6.2).
//!   A disabled family binds no addresses, answers no packets, and refuses
//!   family-specific socket creation with a typed error — fail closed, not a
//!   silent drop.
//! * `net.ipv6.privacy` — `true` or `false` (default): whether the stack
//!   forms RFC 8981 temporary (privacy) IPv6 addresses in addition to the
//!   stable SLAAC address.
//! * `net.tcp.syncookies` — `auto` (default) or `always`: the SYN-flood
//!   defence policy. `auto` keeps a bounded half-open queue and falls back to
//!   stateless cookies on overflow; `always` answers every SYN statelessly.
//!   There is deliberately no `off`: an undefended SYN queue is a security
//!   regression, never a configuration.
//! * `net.tcp.keepalive` — `true` or `false` (default): whether TCP
//!   connections send RFC 9293 §3.8.4 keepalive probes on an idle link. When
//!   enabled, every connection is probed after the standard idle interval and
//!   torn down if the peer stops answering; `false` (RFC 1122 §4.2.3.6) never
//!   probes and never tears an idle connection down for inactivity.
//! * `net.tcp.ecn` — `true` or `false` (default): whether TCP connections
//!   negotiate RFC 3168 Explicit Congestion Notification. When enabled, a
//!   connection offers ECN in its SYN/SYN-ACK and, once negotiated, marks
//!   eligible segments ECT(0) and treats a CE mark as a congestion signal
//!   instead of forcing a drop; `false` leaves connections Not-ECT.
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

use tairix_abi::driver_store::SystemConfigFile;
use tairix_util::conf::strip_comment;

/// The directory that holds the boot-time configuration store.
pub const CONFIG_DIR: &str = "/System/Settings/Configuration";

/// The configuration store document, named by the closed
/// `/System/Settings/` file set so this engine and the pre-unlock reader that
/// serves it cannot name different files.
pub const CONFIG_PATH: &str = SystemConfigFile::System.path();

/// Maximum length, in bytes, of a store text [`SystemConfig::parse`] will
/// consider. A larger input is refused outright ([`ConfigError::TooLong`])
/// rather than scanned — the store is tiny, and an unboundedly large one is
/// a defect, not a workload.
pub const MAX_CONFIG_LEN: usize = 4096;

/// Which session type the login service starts for an authenticated user
/// (`os.loginType`). System policy, never a per-login prompt.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum LoginType {
    /// The text login: the authenticated account's shell. A shell user
    /// starts the desktop on demand with the `desktop` command.
    Text,
    /// The graphical login — the default, and the value an absent store
    /// implies: an authenticated user's session starts the desktop
    /// directly. A machine that cannot run one (no live display service,
    /// no desktop bundle, or no login-screen bundle) degrades to the text
    /// prompt — never an error.
    #[default]
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

/// The master caching switch (`cache.all`): a pure kill switch and ceiling
/// over every per-class caching mode.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum CacheSwitch {
    /// Caching is permitted; each class follows its own [`CacheMode`]. The
    /// default, and the value an absent store implies.
    #[default]
    On,
    /// Caching is disabled system-wide: every SMARTRAM cache is off
    /// regardless of its per-class setting.
    Off,
}

impl CacheSwitch {
    /// The canonical value spelling (`on` / `off`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

/// A per-class caching mode (`cache.<class>`).
///
/// There is deliberately no `On` variant: a class is never forced to ignore
/// memory pressure — that would break the SMARTRAM reserve invariants
/// (`plans/SMARTRAM.md` section 7). A class is either governed by the
/// pressure governor ([`Auto`](Self::Auto)) or hard-disabled
/// ([`Off`](Self::Off)).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum CacheMode {
    /// The memory-pressure governor manages the class (today's behaviour,
    /// and the value an absent store implies).
    #[default]
    Auto,
    /// The class is hard-disabled: the cache admits and holds nothing.
    Off,
}

impl CacheMode {
    /// The canonical value spelling (`auto` / `off`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// Whether a class in this mode admits entries. `true` for
    /// [`Auto`](Self::Auto), `false` for [`Off`](Self::Off).
    #[must_use]
    pub const fn admits(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// The classes of live SMARTRAM cache a per-class switch governs.
///
/// Only classes whose cache exists in the tree today are listed; adding a
/// key for a shelved or future cache would be speculative surface. When a
/// new cache lands, it gains its variant here, its `cache.<class>` key, and
/// its wiring in the same change.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CacheClass {
    /// The clean, rebuildable filesystem cache (`kernel/core::fs::CachedFs`).
    Filesystem,
    /// The whole-disk block-level cache
    /// (`kernel/tairix-kernel::block_cache::BlockCache`).
    Block,
    /// The ARXFS transform (decrypted/decompressed cluster) cache
    /// (`kernel/tairix-kernel::transform_cache::TransformClusterCache`).
    Transform,
    /// The semantic application-launch cache
    /// (`kernel/core::launch_cache::LaunchCache`).
    Semantic,
}

impl CacheClass {
    /// Every cache class, in the canonical listing order.
    pub const ALL: &'static [Self] = &[
        Self::Filesystem,
        Self::Block,
        Self::Transform,
        Self::Semantic,
    ];

    /// The registry key that carries this class's per-class switch.
    #[must_use]
    pub const fn key(self) -> Key {
        match self {
            Self::Filesystem => Key::CacheFilesystem,
            Self::Block => Key::CacheBlock,
            Self::Transform => Key::CacheTransform,
            Self::Semantic => Key::CacheSemantic,
        }
    }
}

/// A stack-wide boolean network switch (`net.ipv4.enabled`,
/// `net.ipv6.enabled`, `net.ipv6.privacy`).
///
/// The value vocabulary is `true` / `false` — the network-configuration
/// spelling (`plans/NETWORK.md` section 6.2), distinct from the caching
/// switches' `on` / `off`, so each store key reads in its own domain's
/// idiom.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NetToggle {
    /// The feature is on (`true`).
    Enabled,
    /// The feature is off (`false`).
    Disabled,
}

impl NetToggle {
    /// The canonical value spelling (`true` / `false`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "true",
            Self::Disabled => "false",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "true" => Some(Self::Enabled),
            "false" => Some(Self::Disabled),
            _ => None,
        }
    }

    /// Whether the switch is on.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// The TCP SYN-flood defence policy (`net.tcp.syncookies`).
///
/// There is deliberately no `Off` variant: an undefended or unbounded SYN
/// queue is a security regression the charter forbids, never a
/// configuration. The choice is only *how eagerly* the stack falls back to
/// stateless cookies.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum SynCookies {
    /// Keep a bounded half-open queue and issue stateless cookies only once
    /// it overflows — the default, and the value an absent store implies.
    #[default]
    Auto,
    /// Answer every SYN with a stateless cookie, holding no half-open state
    /// at all (the most aggressive posture, for a host under sustained
    /// flood).
    Always,
}

impl SynCookies {
    /// The canonical value spelling (`auto` / `always`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set
    /// (case-sensitive — one canonical spelling).
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
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
    /// `cache.all` — the master caching switch / ceiling.
    CacheAll,
    /// `cache.filesystem` — the filesystem cache's per-class switch.
    CacheFilesystem,
    /// `cache.block` — the block cache's per-class switch.
    CacheBlock,
    /// `cache.transform` — the transform cache's per-class switch.
    CacheTransform,
    /// `cache.semantic` — the launch cache's per-class switch.
    CacheSemantic,
    /// `net.ipv4.enabled` — the stack-wide IPv4 address-family switch.
    NetIpv4Enabled,
    /// `net.ipv6.enabled` — the stack-wide IPv6 address-family switch.
    NetIpv6Enabled,
    /// `net.ipv6.privacy` — RFC 8981 temporary (privacy) IPv6 addresses.
    NetIpv6Privacy,
    /// `net.tcp.syncookies` — the TCP SYN-flood defence policy.
    NetTcpSynCookies,
    /// `net.tcp.keepalive` — the stack-wide TCP keepalive switch.
    NetTcpKeepalive,
    /// `net.tcp.ecn` — the stack-wide RFC 3168 TCP ECN switch.
    NetTcpEcn,
}

impl Key {
    /// Every registry key, in the canonical listing (and render) order.
    pub const ALL: &'static [Self] = &[
        Self::LoginType,
        Self::CacheAll,
        Self::CacheFilesystem,
        Self::CacheBlock,
        Self::CacheTransform,
        Self::CacheSemantic,
        Self::NetIpv4Enabled,
        Self::NetIpv6Enabled,
        Self::NetIpv6Privacy,
        Self::NetTcpSynCookies,
        Self::NetTcpKeepalive,
        Self::NetTcpEcn,
    ];

    /// The canonical key spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::LoginType => "os.loginType",
            Self::CacheAll => "cache.all",
            Self::CacheFilesystem => "cache.filesystem",
            Self::CacheBlock => "cache.block",
            Self::CacheTransform => "cache.transform",
            Self::CacheSemantic => "cache.semantic",
            Self::NetIpv4Enabled => "net.ipv4.enabled",
            Self::NetIpv6Enabled => "net.ipv6.enabled",
            Self::NetIpv6Privacy => "net.ipv6.privacy",
            Self::NetTcpSynCookies => "net.tcp.syncookies",
            Self::NetTcpKeepalive => "net.tcp.keepalive",
            Self::NetTcpEcn => "net.tcp.ecn",
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
            Self::CacheAll => &["on", "off"],
            Self::CacheFilesystem
            | Self::CacheBlock
            | Self::CacheTransform
            | Self::CacheSemantic => &["auto", "off"],
            Self::NetIpv4Enabled | Self::NetIpv6Enabled | Self::NetIpv6Privacy => {
                &["true", "false"]
            }
            Self::NetTcpSynCookies => &["auto", "always"],
            Self::NetTcpKeepalive | Self::NetTcpEcn => &["true", "false"],
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
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SystemConfig {
    /// The login service's boot-default session type (`os.loginType`).
    pub login_type: LoginType,
    /// The master caching switch (`cache.all`): a ceiling over every
    /// per-class mode below.
    pub cache_all: CacheSwitch,
    /// The filesystem cache's per-class switch (`cache.filesystem`).
    pub cache_filesystem: CacheMode,
    /// The block cache's per-class switch (`cache.block`).
    pub cache_block: CacheMode,
    /// The transform cache's per-class switch (`cache.transform`).
    pub cache_transform: CacheMode,
    /// The launch cache's per-class switch (`cache.semantic`).
    pub cache_semantic: CacheMode,
    /// The stack-wide IPv4 address-family switch (`net.ipv4.enabled`).
    /// Enabled by default.
    pub net_ipv4_enabled: NetToggle,
    /// The stack-wide IPv6 address-family switch (`net.ipv6.enabled`).
    /// Enabled by default.
    pub net_ipv6_enabled: NetToggle,
    /// Whether the stack forms RFC 8981 temporary (privacy) IPv6 addresses
    /// (`net.ipv6.privacy`). Disabled by default — the stable SLAAC address
    /// only, unless the operator opts in.
    pub net_ipv6_privacy: NetToggle,
    /// The TCP SYN-flood defence policy (`net.tcp.syncookies`).
    pub net_tcp_syncookies: SynCookies,
    /// Whether TCP connections send RFC 9293 §3.8.4 keepalive probes on an
    /// idle link (`net.tcp.keepalive`). Disabled by default (RFC 1122
    /// §4.2.3.6): an idle connection is never probed unless the operator opts
    /// in.
    pub net_tcp_keepalive: NetToggle,
    /// Whether TCP connections negotiate RFC 3168 Explicit Congestion
    /// Notification (`net.tcp.ecn`). Disabled by default: connections are
    /// Not-ECT unless the operator opts in.
    pub net_tcp_ecn: NetToggle,
}

impl Default for SystemConfig {
    /// The configuration an **absent** store implies: graphical login,
    /// every cache enabled, both address families enabled, IPv6 privacy
    /// addresses off, and the `auto` SYN-cookie policy. Written by hand
    /// because the per-field defaults are not uniform (IPv6 privacy and TCP
    /// keepalive default *off* while the family switches default *on*), so
    /// a blanket derive would be wrong.
    fn default() -> Self {
        Self {
            login_type: LoginType::default(),
            cache_all: CacheSwitch::default(),
            cache_filesystem: CacheMode::default(),
            cache_block: CacheMode::default(),
            cache_transform: CacheMode::default(),
            cache_semantic: CacheMode::default(),
            net_ipv4_enabled: NetToggle::Enabled,
            net_ipv6_enabled: NetToggle::Enabled,
            net_ipv6_privacy: NetToggle::Disabled,
            net_tcp_syncookies: SynCookies::default(),
            net_tcp_keepalive: NetToggle::Disabled,
            net_tcp_ecn: NetToggle::Disabled,
        }
    }
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
            Key::CacheAll => self.cache_all.as_str(),
            Key::CacheFilesystem => self.cache_filesystem.as_str(),
            Key::CacheBlock => self.cache_block.as_str(),
            Key::CacheTransform => self.cache_transform.as_str(),
            Key::CacheSemantic => self.cache_semantic.as_str(),
            Key::NetIpv4Enabled => self.net_ipv4_enabled.as_str(),
            Key::NetIpv6Enabled => self.net_ipv6_enabled.as_str(),
            Key::NetIpv6Privacy => self.net_ipv6_privacy.as_str(),
            Key::NetTcpSynCookies => self.net_tcp_syncookies.as_str(),
            Key::NetTcpKeepalive => self.net_tcp_keepalive.as_str(),
            Key::NetTcpEcn => self.net_tcp_ecn.as_str(),
        }
    }

    /// The **effective** caching mode for `class`, applying the master
    /// ceiling: [`CacheMode::Off`] whenever `cache.all` is
    /// [`CacheSwitch::Off`], otherwise the class's own configured mode.
    ///
    /// This is the one canonical interpretation of the two persisted keys —
    /// deterministic and fail-closed: the master `off` disables everything,
    /// a per-class `off` disables just that class, and they can never
    /// contradict ambiguously.
    #[must_use]
    pub const fn effective_cache(&self, class: CacheClass) -> CacheMode {
        if matches!(self.cache_all, CacheSwitch::Off) {
            return CacheMode::Off;
        }
        match class {
            CacheClass::Filesystem => self.cache_filesystem,
            CacheClass::Block => self.cache_block,
            CacheClass::Transform => self.cache_transform,
            CacheClass::Semantic => self.cache_semantic,
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
            Key::CacheAll => {
                self.cache_all = CacheSwitch::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::CacheFilesystem => {
                self.cache_filesystem =
                    CacheMode::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::CacheBlock => {
                self.cache_block = CacheMode::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::CacheTransform => {
                self.cache_transform =
                    CacheMode::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::CacheSemantic => {
                self.cache_semantic =
                    CacheMode::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetIpv4Enabled => {
                self.net_ipv4_enabled =
                    NetToggle::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetIpv6Enabled => {
                self.net_ipv6_enabled =
                    NetToggle::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetIpv6Privacy => {
                self.net_ipv6_privacy =
                    NetToggle::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetTcpSynCookies => {
                self.net_tcp_syncookies =
                    SynCookies::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetTcpKeepalive => {
                self.net_tcp_keepalive =
                    NetToggle::from_value(value).ok_or(ConfigError::InvalidValue)?;
            }
            Key::NetTcpEcn => {
                self.net_tcp_ecn = NetToggle::from_value(value).ok_or(ConfigError::InvalidValue)?;
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
            "# TAIRiX boot-time system configuration.\n\
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

#[cfg(test)]
mod tests {
    extern crate std;

    use std::format;
    use std::string::String;

    use super::{
        CacheClass, CacheMode, CacheSwitch, ConfigError, Key, LoginType, NetToggle, SynCookies,
        SystemConfig, CONFIG_PATH, MAX_CONFIG_LEN,
    };

    #[test]
    fn an_empty_store_is_the_default_configuration() {
        assert_eq!(SystemConfig::parse(""), Ok(SystemConfig::default()));
        // A machine that can run a desktop boots to one; login degrades to
        // the text prompt on one that cannot.
        assert_eq!(SystemConfig::default().login_type, LoginType::Graphical);
        assert_eq!(SystemConfig::default().get(Key::LoginType), "graphical");
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
            for cache_all in [CacheSwitch::On, CacheSwitch::Off] {
                for cache_filesystem in [CacheMode::Auto, CacheMode::Off] {
                    for net_ipv4_enabled in [NetToggle::Enabled, NetToggle::Disabled] {
                        for syncookies in [SynCookies::Auto, SynCookies::Always] {
                            for keepalive in [NetToggle::Enabled, NetToggle::Disabled] {
                                let config = SystemConfig {
                                    login_type,
                                    cache_all,
                                    cache_filesystem,
                                    cache_block: CacheMode::Off,
                                    cache_transform: CacheMode::Auto,
                                    cache_semantic: CacheMode::Off,
                                    net_ipv4_enabled,
                                    net_ipv6_enabled: NetToggle::Disabled,
                                    net_ipv6_privacy: NetToggle::Enabled,
                                    net_tcp_syncookies: syncookies,
                                    net_tcp_keepalive: keepalive,
                                    net_tcp_ecn: NetToggle::Enabled,
                                };
                                assert_eq!(SystemConfig::parse(&config.render()), Ok(config));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn net_defaults_match_the_documented_posture() {
        // An absent store: both families on, privacy off, cookies auto.
        let config = SystemConfig::default();
        assert_eq!(config.net_ipv4_enabled, NetToggle::Enabled);
        assert_eq!(config.net_ipv6_enabled, NetToggle::Enabled);
        assert!(config.net_ipv4_enabled.is_enabled());
        assert!(config.net_ipv6_enabled.is_enabled());
        assert_eq!(config.net_ipv6_privacy, NetToggle::Disabled);
        assert!(!config.net_ipv6_privacy.is_enabled());
        assert_eq!(config.net_tcp_syncookies, SynCookies::Auto);
        // Keepalive is off by default (RFC 1122 §4.2.3.6).
        assert_eq!(config.net_tcp_keepalive, NetToggle::Disabled);
        assert!(!config.net_tcp_keepalive.is_enabled());
        // ECN is off by default (RFC 3168): connections are Not-ECT.
        assert_eq!(config.net_tcp_ecn, NetToggle::Disabled);
        assert!(!config.net_tcp_ecn.is_enabled());
    }

    #[test]
    fn net_keys_parse_their_closed_value_sets() {
        let config = SystemConfig::parse(
            "net.ipv4.enabled false\n\
             net.ipv6.enabled true\n\
             net.ipv6.privacy true\n\
             net.tcp.syncookies always\n\
             net.tcp.keepalive true\n\
             net.tcp.ecn true\n",
        )
        .expect("parses");
        assert_eq!(config.net_ipv4_enabled, NetToggle::Disabled);
        assert_eq!(config.net_ipv6_enabled, NetToggle::Enabled);
        assert_eq!(config.net_ipv6_privacy, NetToggle::Enabled);
        assert_eq!(config.net_tcp_syncookies, SynCookies::Always);
        assert_eq!(config.net_tcp_keepalive, NetToggle::Enabled);
        assert_eq!(config.net_tcp_ecn, NetToggle::Enabled);
    }

    #[test]
    fn net_rejects_the_wrong_value_vocabulary() {
        // The family switches take true/false, never the caches' on/off.
        assert_eq!(
            SystemConfig::parse("net.ipv4.enabled on\n"),
            Err(ConfigError::InvalidValue),
        );
        // Values are case-sensitive: one canonical spelling.
        assert_eq!(
            SystemConfig::parse("net.ipv6.enabled True\n"),
            Err(ConfigError::InvalidValue),
        );
        // SYN-cookies has no `off`: an undefended queue is not a setting.
        assert_eq!(
            SystemConfig::parse("net.tcp.syncookies off\n"),
            Err(ConfigError::InvalidValue),
        );
    }

    #[test]
    fn cache_defaults_are_all_enabled() {
        // An absent store reproduces today's behaviour: every cache on.
        let config = SystemConfig::default();
        assert_eq!(config.cache_all, CacheSwitch::On);
        for class in CacheClass::ALL {
            assert_eq!(config.effective_cache(*class), CacheMode::Auto);
            assert!(config.effective_cache(*class).admits());
        }
    }

    #[test]
    fn cache_keys_parse_their_closed_value_sets() {
        let config = SystemConfig::parse(
            "cache.all off\n\
             cache.filesystem off\n\
             cache.block auto\n\
             cache.transform off\n\
             cache.semantic auto\n",
        )
        .expect("parses");
        assert_eq!(config.cache_all, CacheSwitch::Off);
        assert_eq!(config.cache_filesystem, CacheMode::Off);
        assert_eq!(config.cache_block, CacheMode::Auto);
        assert_eq!(config.cache_transform, CacheMode::Off);
        assert_eq!(config.cache_semantic, CacheMode::Auto);
    }

    #[test]
    fn cache_all_off_is_a_ceiling_over_every_class() {
        // Master off disables every class regardless of the per-class value.
        let config = SystemConfig::parse("cache.all off\ncache.filesystem auto\n").expect("parses");
        for class in CacheClass::ALL {
            assert_eq!(config.effective_cache(*class), CacheMode::Off);
            assert!(!config.effective_cache(*class).admits());
        }
    }

    #[test]
    fn per_class_off_disables_only_that_class() {
        let config = SystemConfig::parse("cache.filesystem off\n").expect("parses");
        assert_eq!(
            config.effective_cache(CacheClass::Filesystem),
            CacheMode::Off
        );
        assert_eq!(config.effective_cache(CacheClass::Block), CacheMode::Auto);
        assert_eq!(
            config.effective_cache(CacheClass::Transform),
            CacheMode::Auto
        );
        assert_eq!(
            config.effective_cache(CacheClass::Semantic),
            CacheMode::Auto
        );
    }

    #[test]
    fn cache_class_maps_to_its_key() {
        for class in CacheClass::ALL {
            // The key a class points at must decode its own per-class value
            // set (`auto`/`off`), never the master's (`on`/`off`).
            assert_eq!(class.key().values(), &["auto", "off"]);
        }
    }

    #[test]
    fn cache_rejects_the_wrong_value_vocabulary() {
        // The master takes on/off, a per-class takes auto/off; they never mix.
        assert_eq!(
            SystemConfig::parse("cache.all auto\n"),
            Err(ConfigError::InvalidValue),
        );
        assert_eq!(
            SystemConfig::parse("cache.filesystem on\n"),
            Err(ConfigError::InvalidValue),
        );
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
