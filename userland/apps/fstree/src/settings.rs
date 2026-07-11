//! Persisted session preferences: the configurable confirmation prompts.
//!
//! The settings live in the user's own settings tree —
//! `<home>/Settings/fstree/config` — never beside the binary (an app writes
//! only its per-user state). The file is a plain `key=value` line format
//! read and written through the injected [`Fs`] seam, so the round-trip is
//! host-testable and every permission check stays kernel-side.
//!
//! Parsing fails **safe**: a missing file, a refused read, an unknown key,
//! or a malformed value leaves the affected setting at its default — and
//! every default keeps its confirmation *on*, so damage-limiting questions
//! are never silently lost to a corrupt file.

use alloc::format;
use alloc::string::String;

use rustos_abi::Errno;

use crate::fs::Fs;

/// Upper bound on the settings file read: the file carries two short
/// lines, so one page is generous; a larger file is read only this far
/// (later garbage cannot balloon the session).
const CONFIG_MAX: usize = 4096;

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
    /// Parse the config file's text. Unknown keys and malformed lines are
    /// ignored (the setting keeps its default); only an explicit
    /// `key=off` disables a confirmation.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let on = match value.trim() {
                "on" => true,
                "off" => false,
                _ => continue,
            };
            match key.trim() {
                "confirm-delete" => settings.confirm_delete = on,
                "confirm-batch-delete" => settings.confirm_batch_delete = on,
                _ => {}
            }
        }
        settings
    }

    /// The config file's text for these settings.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "confirm-delete={}\nconfirm-batch-delete={}\n",
            if self.confirm_delete { "on" } else { "off" },
            if self.confirm_batch_delete {
                "on"
            } else {
                "off"
            },
        )
    }

    /// Load the settings from `<home>/Settings/fstree/config` through the
    /// seam. Any failure — no file, a refused read, undecodable bytes —
    /// yields the defaults; a missing preference file is the ordinary
    /// first-run state, not an error.
    #[must_use]
    pub fn load(fs: &mut dyn Fs, home: &str) -> Self {
        let path = config_path(home);
        let mut buf = alloc::vec![0u8; CONFIG_MAX];
        let Ok(used) = fs.read(&path, 0, &mut buf) else {
            return Self::default();
        };
        let Ok(text) = core::str::from_utf8(&buf[..used]) else {
            return Self::default();
        };
        Self::parse(text)
    }

    /// Persist the settings to `<home>/Settings/fstree/config`, creating
    /// the `fstree` settings directory when absent (`Settings/` itself is
    /// part of every user's fixed home shape).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the caller reports it and the
    /// in-memory settings stand for the session.
    pub fn store(&self, fs: &mut dyn Fs, home: &str) -> Result<(), Errno> {
        let dir = format!("{}/Settings/fstree", trimmed(home));
        match fs.mkdir(&dir) {
            Ok(()) | Err(Errno::AlreadyExists) => {}
            Err(errno) => return Err(errno),
        }
        let path = config_path(home);
        fs.create(&path)?;
        fs.write(&path, 0, self.encode().as_bytes())
    }
}

/// The config file's full path under `home`.
#[must_use]
pub fn config_path(home: &str) -> String {
    format!("{}/Settings/fstree/config", trimmed(home))
}

/// `home` without a trailing separator, so joins never double one.
fn trimmed(home: &str) -> &str {
    if home == "/" {
        home
    } else {
        home.trim_end_matches('/')
    }
}
