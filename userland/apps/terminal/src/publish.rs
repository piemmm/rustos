//! Keeping the settings sheet responsive: what a store write is asked to do,
//! and what the answer means for the profile in force.
//!
//! The sheet's sliders are continuous, so wiring their value to a store write
//! costs one commit — an IPC round trip to the configuration service and a disk
//! write — per pointer-motion sample, and the window is frozen for every one of
//! them. Two rules remove that, and both live here so they are host tests
//! rather than arguments:
//!
//! - **A drag changes the profile, not the store.** Every sample is applied
//!   *live* (colours, font, grid) and published nothing. The one published
//!   moment is where the interaction settles.
//! - **The store is written off the loop, and the answer is what the profile
//!   becomes.** The publish happens on a worker; the profile in force is
//!   replaced by what the store then implies, so a value a machine policy
//!   supplies wins over the widget's guess and a refused write reverts the
//!   preview rather than leaving a look the next start would not restore.
//!
//! [`Publication`] is the pair that makes that honest: `adopted` is always what
//! the store said, and `live` is what the windows currently render. They differ
//! only while an edit is in flight.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::profile::{Profile, ProfileKey};

/// What the settings worker is asked to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishJob {
    /// Write this profile to the user's own document.
    Save(Profile),
    /// Remove the user's opinions, so the layers beneath them apply again.
    ///
    /// *Restore defaults* is deliberately not "write this build's defaults":
    /// the profile that then applies is whatever the machine's policy and the
    /// bundle's shipped defaults imply, which only the store knows.
    Restore,
}

/// What the store said after a write: the profile it now implies, and the
/// stored values this build's registry refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Published {
    /// The profile the store's layers now imply — what the windows adopt.
    pub profile: Profile,
    /// One ready-to-print line per stored value that could not be used.
    pub warnings: Vec<String>,
}

/// The profile in force and the one on screen.
///
/// `adopted` is what the store last said; `live` is what the windows render.
/// A settling interaction moves `live` and asks for a write, and only the
/// write's answer moves `adopted` — so the two are equal except while an edit
/// is in flight, and a refused write puts `live` back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication {
    adopted: Profile,
    live: Profile,
}

impl Publication {
    /// Start from the profile the store implied at bring-up.
    #[must_use]
    pub const fn new(profile: Profile) -> Self {
        Self {
            adopted: profile,
            live: profile,
        }
    }

    /// The profile the windows render.
    #[must_use]
    pub const fn live(&self) -> &Profile {
        &self.live
    }

    /// The profile the store last said, which the live one reverts to when a
    /// write is refused.
    #[must_use]
    pub const fn adopted(&self) -> &Profile {
        &self.adopted
    }

    /// Show `profile` without writing anything — one sample of a drag still
    /// under the pointer, or a settled edit whose write is asked for
    /// separately.
    pub fn preview(&mut self, profile: Profile) {
        self.live = profile;
    }

    /// Ask for what is on screen to be written, because the interaction that
    /// produced it has finished.
    #[must_use]
    pub const fn request_save(&self) -> PublishJob {
        PublishJob::Save(self.live)
    }

    /// Ask for the user's opinions to be removed.
    ///
    /// The live profile is left alone: what applies afterwards is whatever the
    /// store answers with, and guessing at it here would flash a look the
    /// store may not agree with.
    #[must_use]
    pub const fn restore(&self) -> PublishJob {
        PublishJob::Restore
    }

    /// Adopt what the store said, or state why it said nothing.
    ///
    /// On success the store's answer becomes both the adopted and the live
    /// profile — so a machine policy or a shipped default the user's document
    /// does not override wins over what the widget asked for. On a refusal the
    /// live profile snaps back to the adopted one, which is the last thing the
    /// store actually holds.
    ///
    /// `warnings` receives whatever the caller should print. Answers whether
    /// the profile in force changed, so every window re-derives its look and
    /// repaints and the open sheet is re-seeded — the sheet holds its own copy
    /// of what it is editing, and a store answer that differs from what it
    /// asked for leaves that copy describing settings nothing is using.
    pub fn adopt(&mut self, answer: Result<Published, Errno>, warnings: &mut Vec<String>) -> bool {
        let before = self.live;
        match answer {
            Ok(published) => {
                warnings.extend(published.warnings);
                self.adopted = published.profile;
                self.live = published.profile;
            }
            Err(err) => {
                warnings.push(alloc::format!(
                    "terminal: the profile was not saved ({err:?}); keeping the settings in force\n"
                ));
                self.live = self.adopted;
            }
        }
        self.live != before
    }
}

/// One ready-to-print line per stored value this build's registry refused.
///
/// Shared by the bring-up read and by every publish answer, so a broken
/// setting is worded the same whichever noticed it.
#[must_use]
pub fn refusal_warnings(refused: &[ProfileKey]) -> Vec<String> {
    refused
        .iter()
        .map(|key| {
            alloc::format!(
                "terminal: {}: not a value this setting accepts; using its default\n",
                key.name()
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
