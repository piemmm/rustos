//! The notification policy the desktop document carries: whether the
//! desktop shows notifications at all, and how loudly each source may
//! reach it.
//!
//! A source is the kernel-attested bundle identity of the program that
//! posted a notice, never a name the program gave itself, so a policy can
//! only ever be keyed on something the sender cannot choose. A source with no
//! entry shows everything; the entries record only the sources the user has
//! quietened.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::notify_ipc::NotifySeverity;
use tairix_abi::{validate_bundle_id, BundleId};
use tairix_appconf::MAX_VALUE_LEN;

/// The least severe notice a source may show.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum NotifyLevel {
    /// Every notice.
    #[default]
    All,
    /// Warnings and critical notices.
    Warnings,
    /// Critical notices alone.
    Critical,
    /// Nothing.
    None,
}

impl NotifyLevel {
    /// Every level, from the most permissive to the least.
    pub const ALL: [Self; 4] = [Self::All, Self::Warnings, Self::Critical, Self::None];

    /// The canonical value spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Warnings => "warning",
            Self::Critical => "critical",
            Self::None => "none",
        }
    }

    /// Decode a value spelling; `None` for anything outside the closed set.
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == value)
    }

    /// Whether a notice of `severity` reaches the desktop at this level.
    #[must_use]
    pub const fn admits(self, severity: NotifySeverity) -> bool {
        match self {
            Self::All => true,
            Self::Warnings => {
                matches!(severity, NotifySeverity::Warning | NotifySeverity::Critical)
            }
            Self::Critical => matches!(severity, NotifySeverity::Critical),
            Self::None => false,
        }
    }
}

/// A source policy the document cannot hold: its canonical spelling would
/// outgrow one settings value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PolicyFull;

impl fmt::Display for PolicyFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the desktop cannot remember a setting for any more sources")
    }
}

/// Which notifications reach the desktop.
///
/// The per-source entries are held sorted by identity, each identity once and
/// none at [`NotifyLevel::All`], so the canonical spelling is a function of the
/// policy alone and a lookup is a binary search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotifyPolicy {
    enabled: bool,
    sources: Vec<(BundleId, NotifyLevel)>,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            sources: Vec::new(),
        }
    }
}

impl NotifyPolicy {
    /// Whether the desktop shows notifications at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Show notifications, or show none whatever each source's level says.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// The level `source` shows at.
    #[must_use]
    pub fn level_of(&self, source: &str) -> NotifyLevel {
        self.position(source)
            .ok()
            .map_or(NotifyLevel::All, |at| self.sources[at].1)
    }

    /// Whether a notice of `severity` from `source` reaches the desktop.
    #[must_use]
    pub fn admits(&self, source: &str, severity: NotifySeverity) -> bool {
        self.enabled && self.level_of(source).admits(severity)
    }

    /// Every source with a level of its own, in identity order.
    pub fn sources(&self) -> impl Iterator<Item = (&str, NotifyLevel)> + '_ {
        self.sources
            .iter()
            .map(|(source, level)| (source.as_str(), *level))
    }

    /// Show `source` at `level`.
    ///
    /// # Errors
    ///
    /// [`PolicyFull`] when the change would make the policy's spelling
    /// outgrow one settings value; the policy is left as it was.
    pub fn set_level(&mut self, source: BundleId, level: NotifyLevel) -> Result<(), PolicyFull> {
        let found = self.position(source.as_str());
        if level == NotifyLevel::All {
            if let Ok(at) = found {
                self.sources.remove(at);
            }
            return Ok(());
        }
        let grown = match found {
            Ok(at) => self
                .rendered_len()
                .saturating_sub(self.sources[at].1.as_str().len())
                .saturating_add(level.as_str().len()),
            Err(_) => self
                .rendered_len()
                .saturating_add(usize::from(!self.sources.is_empty()))
                .saturating_add(entry_len(&source, level)),
        };
        if grown > MAX_VALUE_LEN {
            return Err(PolicyFull);
        }
        match found {
            Ok(at) => self.sources[at].1 = level,
            Err(at) => self.sources.insert(at, (source, level)),
        }
        Ok(())
    }

    /// The per-source entries as the one settings value that carries them:
    /// `<source>:<level>` pairs in identity order, one space between.
    #[must_use]
    pub fn render_sources(&self) -> String {
        let mut text = String::with_capacity(self.rendered_len());
        for (source, level) in &self.sources {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(source.as_str());
            text.push(':');
            text.push_str(level.as_str());
        }
        text
    }

    /// Replace the per-source entries with those `value` spells.
    ///
    /// Answers whether `value` was a policy: every entry a legal bundle
    /// identity and a level other than `all` (whose spelling is the absence
    /// of an entry), and no identity twice. The entries are unchanged on a
    /// refusal.
    #[must_use]
    pub fn set_sources(&mut self, value: &str) -> bool {
        let mut parsed: Vec<(BundleId, NotifyLevel)> = Vec::new();
        for entry in value.split_ascii_whitespace() {
            let Some((source, level)) = entry.split_once(':') else {
                return false;
            };
            let Some(level) = NotifyLevel::from_value(level) else {
                return false;
            };
            if level == NotifyLevel::All || validate_bundle_id(source).is_err() {
                return false;
            }
            let Ok(source) = BundleId::new(source) else {
                return false;
            };
            parsed.push((source, level));
        }
        parsed.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        if parsed
            .windows(2)
            .any(|pair| pair[0].0.as_str() == pair[1].0.as_str())
        {
            return false;
        }
        self.sources = parsed;
        true
    }

    fn position(&self, source: &str) -> Result<usize, usize> {
        self.sources
            .binary_search_by(|(held, _)| held.as_str().cmp(source))
    }

    fn rendered_len(&self) -> usize {
        let entries: usize = self
            .sources
            .iter()
            .map(|(source, level)| entry_len(source, *level))
            .sum();
        entries.saturating_add(self.sources.len().saturating_sub(1))
    }
}

/// The spelled length of one `<source>:<level>` entry.
fn entry_len(source: &BundleId, level: NotifyLevel) -> usize {
    source.as_str().len() + 1 + level.as_str().len()
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
