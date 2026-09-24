//! Keeping the settings sheet responsive: what a store write is asked to do,
//! and what its answer means for the profile in force.
//!
//! - **A drag changes the profile, not the store.** Wiring a slider to a
//!   write costs a configuration-service round trip and a disk commit per
//!   pointer-motion sample, so every sample is applied live and the one
//!   published moment is where the interaction settles.
//! - **The store is written off the loop, and its answer is what applies** —
//!   to every setting the user has not changed since that write was asked
//!   for. A machine policy or a restore therefore wins wherever the user is
//!   not editing, and an answer never moves a control the user is dragging.
//! - **One write is outstanding at a time.** What is asked for meanwhile is
//!   owed until the answer lands, so every answer describes the only write in
//!   flight, a restore cannot be displaced by a later save, and no save
//!   writes back values a restore is about to remove.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;

use crate::profile::{Invalidation, Profile, ProfileKey, ProfileKeys};

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
/// `adopted` is what the store last said and `live` is what the windows
/// render. They differ only in settings the user has edited since and in
/// those the outstanding write carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publication {
    adopted: Profile,
    live: Profile,
    rendered: Profile,
    /// The settings the user changed that no asked-for write carries — what
    /// an answer must leave as they are.
    edited: ProfileKeys,
    /// The write asked for and not yet answered.
    outstanding: Option<Asked>,
    owed: Owed,
}

/// Which kind of write is outstanding, so a refusal names what failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Asked {
    Save,
    Restore,
}

/// What was asked for while a write was outstanding; a restore goes first,
/// because a save asked for after it describes the profile the restore leaves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Owed {
    restore: bool,
    save: bool,
}

impl Publication {
    /// Start from the profile the store implied at bring-up.
    #[must_use]
    pub const fn new(profile: Profile) -> Self {
        Self {
            adopted: profile,
            live: profile,
            rendered: profile,
            edited: ProfileKeys::EMPTY,
            outstanding: None,
            owed: Owed {
                restore: false,
                save: false,
            },
        }
    }

    /// The profile the windows render.
    #[must_use]
    pub const fn live(&self) -> &Profile {
        &self.live
    }

    /// The profile the store last said.
    #[must_use]
    pub const fn adopted(&self) -> &Profile {
        &self.adopted
    }

    /// Show one edit without writing anything: the settings `was` and `now`
    /// disagree on — an editor's profile either side of one event — are taken
    /// from `now`.
    ///
    /// Only what the edit changed is taken, so an editor holding a copy that
    /// has fallen behind the live profile cannot put its stale values back.
    /// What the surface must draw to catch up is asked for separately, by
    /// [`take_pending`](Self::take_pending), so any number of edits between
    /// two paints cost one paint.
    pub fn edit(&mut self, was: &Profile, now: &Profile) {
        let touched = was.differing(now);
        self.live.set_from(now, touched);
        self.edited = self.edited.union(touched);
    }

    /// What the surface has yet to draw: the difference between the profile in
    /// force and the one it last rendered. Taking it records the live profile
    /// as rendered.
    ///
    /// Diffing against what is *on screen* rather than against each individual
    /// edit is what makes a burst of them cost one repaint, and is why no
    /// change can be lost on the way: an edit that is never drawn stays in the
    /// difference until one is.
    pub fn take_pending(&mut self) -> Invalidation {
        let changed = Invalidation::between(&self.rendered, &self.live);
        self.rendered = self.live;
        changed
    }

    /// Ask for the user's edits to be written, because the interaction that
    /// made them has finished.
    ///
    /// Answers the write to submit now: `None` when nothing is left unwritten,
    /// or when a write is outstanding — this one is then owed and handed out
    /// by [`adopt`](Self::adopt) once that one answers.
    #[must_use]
    pub fn settle(&mut self) -> Option<PublishJob> {
        if !self.edited.is_empty() {
            self.owed.save = true;
        }
        self.next()
    }

    /// Ask for the user's opinions to be removed, with every edit not yet
    /// written.
    ///
    /// Nothing on screen changes until the store answers: what applies then is
    /// its to say, and guessing here would flash a look it may not agree with.
    /// Answers as [`settle`](Self::settle) does.
    #[must_use]
    pub fn restore(&mut self) -> Option<PublishJob> {
        self.edited = ProfileKeys::EMPTY;
        self.owed = Owed {
            restore: true,
            save: false,
        };
        self.next()
    }

    /// Adopt what the store said about the outstanding write, or state why it
    /// said nothing, and answer the owed write to submit next.
    ///
    /// The answer applies to every setting the user has not changed since the
    /// write was asked for, so a machine policy or a shipped default the
    /// user's document does not override wins there; a refusal puts those back
    /// to what the store last held. A setting edited since keeps its value
    /// either way, because the answer describes a moment the user has already
    /// moved on from, and that setting's own settle writes it.
    ///
    /// `warnings` receives whatever the caller should print. What must be
    /// *drawn* is [`take_pending`](Self::take_pending)'s answer, measured
    /// against the screen rather than against this call.
    #[must_use]
    pub fn adopt(
        &mut self,
        answer: Result<Published, Errno>,
        warnings: &mut Vec<String>,
    ) -> Option<PublishJob> {
        let asked = self.outstanding.take();
        let mut profile = match answer {
            Ok(published) => {
                warnings.extend(published.warnings);
                self.adopted = published.profile;
                published.profile
            }
            Err(err) => {
                let what = match asked {
                    Some(Asked::Restore) => "the defaults were not restored",
                    Some(Asked::Save) | None => "the profile was not saved",
                };
                warnings.push(alloc::format!(
                    "terminal: {what} ({err:?}); keeping the settings in force\n"
                ));
                self.adopted
            }
        };
        profile.set_from(&self.live, self.edited);
        self.live = profile;
        self.next()
    }

    /// Hand out the owed write, if one is owed and none is outstanding.
    fn next(&mut self) -> Option<PublishJob> {
        if self.outstanding.is_some() {
            return None;
        }
        if self.owed.restore {
            self.owed.restore = false;
            self.outstanding = Some(Asked::Restore);
            return Some(PublishJob::Restore);
        }
        if !core::mem::take(&mut self.owed.save) || self.edited.is_empty() {
            return None;
        }
        self.edited = ProfileKeys::EMPTY;
        // Edits that came back to rest on what the store holds leave nothing
        // to write.
        if self.live == self.adopted {
            return None;
        }
        self.outstanding = Some(Asked::Save);
        Some(PublishJob::Save(self.live))
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
