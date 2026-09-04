use alloc::vec::Vec;

use tairix_abi::Errno;

use super::{Publication, PublishJob, Published};
use crate::profile::{Profile, ProfileKey};
use crate::scheme::Scheme;

/// A profile distinguishable from the default in the field a slider drives.
fn larger() -> Profile {
    Profile {
        font_size_px: Profile::default().font_size_px + 4,
        ..Profile::default()
    }
}

/// A profile distinguishable from both the default and [`larger`].
fn largest() -> Profile {
    Profile {
        font_size_px: Profile::default().font_size_px + 8,
        ..Profile::default()
    }
}

/// The regression the whole arrangement exists for: dragging a slider shows
/// every sample and asks for **no** write. Persisting per sample is one IPC
/// round trip and one disk commit per pointer motion.
#[test]
fn a_drag_previews_every_sample_and_asks_for_no_write() {
    let mut publication = Publication::new(Profile::default());
    for size in 1..=8 {
        publication.preview(Profile {
            font_size_px: Profile::default().font_size_px + size,
            ..Profile::default()
        });
    }
    assert_eq!(
        publication.live().font_size_px,
        Profile::default().font_size_px + 8,
        "the last sample is what the windows render"
    );
    assert_eq!(
        *publication.adopted(),
        Profile::default(),
        "nothing was written, so nothing is adopted"
    );
}

/// The settle is the one moment a write is asked for, and it carries the
/// value the interaction ended on.
#[test]
fn a_settle_asks_for_exactly_one_write() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(larger());
    publication.preview(largest());
    assert_eq!(
        publication.request_save(),
        PublishJob::Save(largest()),
        "the write carries the value the interaction ended on"
    );
    assert_eq!(*publication.live(), largest());
    assert_eq!(
        *publication.adopted(),
        Profile::default(),
        "the write has not answered yet"
    );
}

/// Persist-then-adopt: the profile in force becomes what the store said, not
/// what the widget asked for, so a lower layer the user's document does not
/// override still wins.
#[test]
fn the_stores_answer_is_what_gets_adopted() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(largest());
    let _ = publication.request_save();
    let mut warnings = Vec::new();
    // The store answers with a *different* profile: a machine policy pinned
    // the size somewhere else.
    let adopted = publication.adopt(
        Ok(Published {
            profile: larger(),
            warnings: Vec::new(),
        }),
        &mut warnings,
    );
    assert!(
        adopted,
        "the sheet's own copy is now stale, and so are the pixels"
    );
    assert_eq!(*publication.adopted(), larger());
    assert_eq!(*publication.live(), larger());
    assert!(warnings.is_empty());
}

/// A refused write reverts the preview and says why, so the window never keeps
/// showing a look the next start would not restore.
#[test]
fn a_refused_write_reverts_the_preview_and_states_the_reason() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(largest());
    let _ = publication.request_save();
    assert_eq!(*publication.live(), largest());
    let mut warnings = Vec::new();
    let adopted = publication.adopt(Err(Errno::NoSpace), &mut warnings);
    assert!(adopted, "the reverted look must be redrawn");
    assert_eq!(
        *publication.live(),
        Profile::default(),
        "the live profile snaps back to what the store holds"
    );
    assert_eq!(*publication.adopted(), Profile::default());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("NoSpace"));
    assert!(warnings[0].ends_with("keeping the settings in force\n"));
}

/// An answer that matches what is already on screen asks for no repaint: a
/// settle whose write the store simply confirmed costs nothing further.
#[test]
fn a_confirmed_write_asks_for_no_repaint() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(largest());
    let _ = publication.request_save();
    let mut warnings = Vec::new();
    let adopted = publication.adopt(
        Ok(Published {
            profile: largest(),
            warnings: Vec::new(),
        }),
        &mut warnings,
    );
    assert!(!adopted);
    assert_eq!(*publication.adopted(), largest());
}

/// *Restore defaults* guesses at nothing: it asks the store to drop the user's
/// opinions and adopts whatever the store then implies.
#[test]
fn a_restore_asks_the_store_and_adopts_its_answer() {
    let mut publication = Publication::new(largest());
    assert_eq!(publication.restore(), PublishJob::Restore);
    assert_eq!(
        *publication.live(),
        largest(),
        "nothing changes until the store has spoken"
    );
    let mut warnings = Vec::new();
    // The layers beneath the user's document name a scheme of their own.
    let policy = Profile {
        scheme: Scheme::Contrast,
        ..Profile::default()
    };
    let adopted = publication.adopt(
        Ok(Published {
            profile: policy,
            warnings: Vec::new(),
        }),
        &mut warnings,
    );
    assert!(adopted);
    assert_eq!(*publication.adopted(), policy);
}

/// Warnings the store's answer carried are passed through for the caller to
/// print, so a value the registry refused is never silent.
#[test]
fn the_answers_warnings_reach_the_caller() {
    let mut publication = Publication::new(Profile::default());
    let mut warnings = Vec::new();
    let _ = publication.adopt(
        Ok(Published {
            profile: Profile::default(),
            warnings: super::refusal_warnings(&[ProfileKey::Scheme]),
        }),
        &mut warnings,
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains(ProfileKey::Scheme.name()));
    assert!(warnings[0].ends_with("using its default\n"));
}

// --- What the screen still owes -----------------------------------------------

/// The compute half of the same regression: a burst of previews between two
/// paints costs **one** paint, because what is owed is measured against the
/// screen rather than against each edit.
#[test]
fn a_burst_of_previews_owes_the_screen_one_paint() {
    let mut publication = Publication::new(Profile::default());
    for size in 1..=8 {
        publication.preview(Profile {
            font_size_px: Profile::default().font_size_px + size,
            ..Profile::default()
        });
    }
    assert!(
        publication.take_pending().metrics(),
        "the folded burst still owes the size change"
    );
    assert!(
        !publication.take_pending().any(),
        "and owes nothing once drawn"
    );
}

/// A preview the surface never drew stays owed. Measuring against each edit
/// instead would let a change slip through whenever the outcome that carried
/// it lost to another.
#[test]
fn an_undrawn_preview_stays_owed_until_it_is_drawn() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(larger());
    publication.preview(Profile::default());
    assert!(
        !publication.take_pending().any(),
        "a preview returned to what the screen already shows owes nothing"
    );
    publication.preview(larger());
    assert!(publication.take_pending().metrics(), "but a real one does");
}

/// Adopting the store's answer leaves the screen owing the difference from
/// what it is *showing*, not from what the widget last asked for.
#[test]
fn adopting_owes_the_difference_from_the_screen() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(larger());
    let _ = publication.take_pending();
    let mut warnings = Vec::new();
    let changed = publication.adopt(
        Ok(Published {
            profile: largest(),
            warnings: Vec::new(),
        }),
        &mut warnings,
    );
    assert!(changed, "the store's answer differs from what was asked");
    assert!(
        publication.take_pending().metrics(),
        "and the screen owes the difference from the size it is showing"
    );
}

/// A refused write reverts the preview, and the screen owes that revert —
/// otherwise a window keeps showing a look the next start would not restore.
#[test]
fn a_refused_write_owes_the_revert() {
    let mut publication = Publication::new(Profile::default());
    publication.preview(larger());
    let _ = publication.take_pending();
    let mut warnings = Vec::new();
    let changed = publication.adopt(Err(Errno::PermissionDenied), &mut warnings);
    assert!(changed, "the live profile snapped back");
    assert!(
        publication.take_pending().metrics(),
        "and the screen owes the size it snapped back to"
    );
}
