//! Cinder's persisted state: how he was feeling and whether he was out.
//!
//! Small on purpose. A companion should be where you left him and in roughly
//! the mood you left him in; everything else is this run's business and is not
//! written anywhere.

use tairix_appconf::{ConfError, Document, PERMILLE_FULL};

use crate::mind::Needs;

/// The document key the energy level is written under.
const KEY_ENERGY: &str = "energy";
/// The document key the play level is written under.
const KEY_PLAY: &str = "play";
/// The document key the affection level is written under.
const KEY_AFFECTION: &str = "affection";
/// The document key the whereabouts is written under.
const KEY_LOOSE: &str = "loose";

/// What survives a restart.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct Saved {
    /// How Cinder was feeling.
    pub needs: Needs,
    /// Whether he was out on the desktop.
    pub was_loose: bool,
}

impl Saved {
    /// Render the state as a settings document.
    ///
    /// Levels are written in parts per thousand, the shared settings grammar's
    /// own fixed-point form: exact in the text, legible to anyone reading the
    /// file, and needing no float parser to read back.
    ///
    /// # Errors
    ///
    /// Whatever the document refuses a key or value with; a level is bounded
    /// into range before it is offered, so only a malformed key can fail.
    pub fn to_document(self) -> Result<Document, ConfError> {
        let mut document = Document::new();
        document.set_permille(KEY_ENERGY, permille_of(self.needs.energy))?;
        document.set_permille(KEY_PLAY, permille_of(self.needs.play))?;
        document.set_permille(KEY_AFFECTION, permille_of(self.needs.affection))?;
        document.set_bool(KEY_LOOSE, self.was_loose)?;
        Ok(document)
    }

    /// Read the state from a settings document.
    ///
    /// Every field falls back to its default independently, so a document that
    /// has lost a line — or that carries a value this never wrote — still
    /// yields a usable companion rather than nothing. A malformed value is
    /// treated as absent rather than repaired: a level this did not write is
    /// not one to believe.
    #[must_use]
    pub fn from_document(document: &Document) -> Self {
        let fallback = Self::default();
        Self {
            needs: Needs {
                energy: level(document, KEY_ENERGY, fallback.needs.energy),
                play: level(document, KEY_PLAY, fallback.needs.play),
                affection: level(document, KEY_AFFECTION, fallback.needs.affection),
            },
            was_loose: document
                .bool(KEY_LOOSE)
                .ok()
                .flatten()
                .unwrap_or(fallback.was_loose),
        }
    }
}

/// `level` as parts per thousand, bounded into range.
fn permille_of(level: f64) -> u32 {
    let scaled = tairix_util::mathf::clamp(level, 0.0, 1.0) * f64::from(PERMILLE_FULL);
    // The clamp bounds this to `0..=PERMILLE_FULL`, well inside a `u32`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u32
    }
}

/// A need level read from `key`, or `fallback`.
fn level(document: &Document, key: &str, fallback: f64) -> f64 {
    match document.permille(key) {
        Ok(Some(parts)) => f64::from(parts) / f64::from(PERMILLE_FULL),
        Ok(None) | Err(_) => fallback,
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
