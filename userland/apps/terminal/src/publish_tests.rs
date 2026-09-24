use alloc::vec::Vec;

use tairix_abi::Errno;

use super::{Publication, PublishJob, Published};
use crate::profile::{Profile, ProfileKey};
use crate::scheme::Scheme;

/// A profile distinguishable from the default in the field a slider drives.
fn larger() -> Profile {
    sized(4)
}

/// A profile distinguishable from both the default and [`larger`].
fn largest() -> Profile {
    sized(8)
}

/// The default profile with its text `steps` pixels larger.
fn sized(steps: u16) -> Profile {
    Profile {
        font_size_px: Profile::default().font_size_px + steps,
        ..Profile::default()
    }
}

/// One edit made by an editor in step with the live profile — a sample of a
/// drag, a menu command.
fn edit(publication: &mut Publication, change: impl FnOnce(&mut Profile)) {
    let was = *publication.live();
    let mut now = was;
    change(&mut now);
    publication.edit(&was, &now);
}

fn resize_to(profile: Profile) -> impl FnOnce(&mut Profile) {
    move |now| now.font_size_px = profile.font_size_px
}

fn set_opacity(opacity: u16) -> impl FnOnce(&mut Profile) {
    move |now| now.effects.opacity = opacity
}

/// The store answering the outstanding write: it now implies `profile`.
fn confirm(publication: &mut Publication, profile: Profile) -> Option<PublishJob> {
    let answer = Published {
        profile,
        warnings: Vec::new(),
    };
    publication.adopt(Ok(answer), &mut Vec::new())
}

/// The store refusing the outstanding write with `err`.
fn refuse(publication: &mut Publication, err: Errno) -> Option<PublishJob> {
    publication.adopt(Err(err), &mut Vec::new())
}

/// The regression the whole arrangement exists for: dragging a slider shows
/// every sample and writes nothing. Persisting per sample is one IPC round
/// trip and one disk commit per pointer motion.
#[test]
fn a_drag_shows_every_sample_and_writes_nothing() {
    let mut publication = Publication::new(Profile::default());
    for steps in 1..=8 {
        edit(&mut publication, resize_to(sized(steps)));
    }
    assert_eq!(*publication.live(), largest(), "the last sample is shown");
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
    edit(&mut publication, resize_to(larger()));
    edit(&mut publication, resize_to(largest()));
    assert_eq!(publication.settle(), Some(PublishJob::Save(largest())));
    assert_eq!(publication.settle(), None, "nothing is left unwritten");
    assert_eq!(
        *publication.adopted(),
        Profile::default(),
        "the write has not answered yet"
    );
    assert_eq!(
        confirm(&mut publication, largest()),
        None,
        "and nothing is owed behind it"
    );
}

#[test]
fn settling_with_nothing_edited_writes_nothing() {
    let mut publication = Publication::new(Profile::default());
    assert_eq!(publication.settle(), None);
    let unchanged = *publication.live();
    publication.edit(&unchanged, &unchanged);
    assert_eq!(publication.settle(), None);
}

/// A drag that comes to rest where it began leaves the store holding exactly
/// what is shown, so it costs no round trip.
#[test]
fn a_drag_that_ends_where_it_began_writes_nothing() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    edit(&mut publication, resize_to(Profile::default()));
    assert_eq!(publication.settle(), None);
}

/// Persist-then-adopt: the profile in force becomes what the store said, not
/// what the widget asked for, so a lower layer the user's document does not
/// override still wins.
#[test]
fn the_stores_answer_is_what_gets_adopted() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(largest()));
    let _ = publication.settle();
    // A machine policy pinned the size somewhere else.
    assert_eq!(confirm(&mut publication, larger()), None);
    assert_eq!(*publication.adopted(), larger());
    assert_eq!(*publication.live(), larger());
}

/// A refused write reverts the edit and says why, so the window never keeps
/// showing a look the next start would not restore.
#[test]
fn a_refused_write_reverts_the_edit_and_states_the_reason() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(largest()));
    let _ = publication.settle();
    let mut warnings = Vec::new();
    assert_eq!(publication.adopt(Err(Errno::NoSpace), &mut warnings), None);
    assert_eq!(
        *publication.live(),
        Profile::default(),
        "the live profile goes back to what the store holds"
    );
    assert_eq!(*publication.adopted(), Profile::default());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("the profile was not saved"));
    assert!(warnings[0].contains("NoSpace"));
    assert!(warnings[0].ends_with("keeping the settings in force\n"));
}

/// An answer that matches what is already on screen owes no repaint: a settle
/// whose write the store simply confirmed costs nothing further.
#[test]
fn a_confirmed_write_owes_the_screen_nothing() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(largest()));
    let _ = publication.take_pending();
    let _ = publication.settle();
    let _ = confirm(&mut publication, largest());
    assert!(!publication.take_pending().any());
    assert_eq!(*publication.adopted(), largest());
}

/// *Restore defaults* guesses at nothing: it asks the store to drop the user's
/// opinions and adopts whatever the store then implies.
#[test]
fn a_restore_asks_the_store_and_adopts_its_answer() {
    let mut publication = Publication::new(largest());
    assert_eq!(publication.restore(), Some(PublishJob::Restore));
    assert_eq!(
        *publication.live(),
        largest(),
        "nothing changes until the store has spoken"
    );
    // The layers beneath the user's document name a scheme of their own.
    let policy = Profile {
        scheme: Scheme::Contrast,
        ..Profile::default()
    };
    assert_eq!(confirm(&mut publication, policy), None);
    assert_eq!(*publication.adopted(), policy);
    assert_eq!(*publication.live(), policy);
}

#[test]
fn a_refused_restore_says_the_defaults_were_not_restored() {
    let mut publication = Publication::new(largest());
    let _ = publication.restore();
    let mut warnings = Vec::new();
    let _ = publication.adopt(Err(Errno::PermissionDenied), &mut warnings);
    assert_eq!(*publication.live(), largest());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("the defaults were not restored"));
}

/// Warnings the store's answer carried are passed through for the caller to
/// print, so a value the registry refused is never silent.
#[test]
fn the_answers_warnings_reach_the_caller() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    let mut warnings = Vec::new();
    let _ = publication.adopt(
        Ok(Published {
            profile: larger(),
            warnings: super::refusal_warnings(&[ProfileKey::Scheme]),
        }),
        &mut warnings,
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains(ProfileKey::Scheme.name()));
    assert!(warnings[0].ends_with("using its default\n"));
}

// --- An answer that lands while the user is still editing -------------------

/// The D179 regression: the answer to one settle lands between two samples of
/// the next drag. The slider stays where the user has it, and the drag's own
/// settle writes the drag rather than the value the answer carried.
#[test]
fn an_answer_landing_mid_drag_leaves_the_dragged_setting_alone() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    assert_eq!(publication.settle(), Some(PublishJob::Save(larger())));

    edit(&mut publication, resize_to(sized(5)));
    assert_eq!(confirm(&mut publication, larger()), None);
    assert_eq!(
        *publication.live(),
        sized(5),
        "the answer does not snap the slider back"
    );

    edit(&mut publication, resize_to(largest()));
    assert_eq!(
        publication.settle(),
        Some(PublishJob::Save(largest())),
        "the drag's settle writes the drag"
    );
}

/// A setting dragged away and back onto the value its write asked for is still
/// under the pointer, so a policy answer does not move it either.
#[test]
fn a_setting_dragged_back_to_its_written_value_is_still_the_users() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    edit(&mut publication, resize_to(largest()));
    edit(&mut publication, resize_to(larger()));
    let pinned = Profile::default();
    let _ = confirm(&mut publication, pinned);
    assert_eq!(*publication.live(), larger());
    assert_eq!(*publication.adopted(), pinned);
}

/// Where the user is not editing, the answer still wins: a machine policy is
/// adopted around the control under the pointer.
#[test]
fn a_policy_answer_mid_drag_wins_everywhere_the_user_is_not_editing() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    edit(&mut publication, set_opacity(500));

    let policy = Profile {
        scheme: Scheme::Contrast,
        ..larger()
    };
    let _ = confirm(&mut publication, policy);
    let live = *publication.live();
    assert_eq!(live.scheme, Scheme::Contrast, "the policy wins here");
    assert_eq!(live.font_size_px, larger().font_size_px);
    assert_eq!(
        live.effects.opacity, 500,
        "the dragged setting is the user's"
    );
}

/// A restore whose answer lands mid-drag resets everything but the dragged
/// setting — and the drag's settle then writes on top of the restored
/// profile, so the restore is not undone by the next write.
#[test]
fn a_restore_answered_mid_drag_is_not_undone_by_the_drags_settle() {
    let opinions = Profile {
        scheme: Scheme::Amber,
        ..largest()
    };
    let mut publication = Publication::new(opinions);
    assert_eq!(publication.restore(), Some(PublishJob::Restore));
    edit(&mut publication, set_opacity(500));

    let restored = Profile {
        scheme: Scheme::Contrast,
        ..Profile::default()
    };
    assert_eq!(confirm(&mut publication, restored), None);
    let mut expected = restored;
    expected.effects.opacity = 500;
    assert_eq!(*publication.live(), expected);
    assert_eq!(publication.settle(), Some(PublishJob::Save(expected)));
}

/// A refusal landing mid-drag says why and reverts the refused write's own
/// setting, but leaves the control under the pointer alone: that setting's
/// settle is what decides it.
#[test]
fn a_refusal_landing_mid_drag_leaves_the_dragged_setting_alone() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    edit(&mut publication, set_opacity(500));

    let mut warnings = Vec::new();
    assert_eq!(publication.adopt(Err(Errno::NoSpace), &mut warnings), None);
    assert_eq!(warnings.len(), 1, "the refusal is stated");
    let mut expected = Profile::default();
    expected.effects.opacity = 500;
    assert_eq!(*publication.live(), expected);
    assert_eq!(publication.settle(), Some(PublishJob::Save(expected)));
}

/// The same refusal while the user is dragging the very setting the refused
/// write carried: the slider stays where the user has it.
#[test]
fn a_refusal_landing_mid_drag_of_the_same_setting_leaves_it_alone() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    edit(&mut publication, resize_to(largest()));

    let _ = refuse(&mut publication, Errno::NoSpace);
    assert_eq!(*publication.live(), largest());
    assert_eq!(publication.settle(), Some(PublishJob::Save(largest())));
}

// --- One write at a time ------------------------------------------------------

/// A settle while a write is outstanding is owed, and goes out as soon as the
/// outstanding one answers — so every answer describes the only write in
/// flight.
#[test]
fn a_save_asked_for_while_a_write_is_outstanding_waits_for_its_answer() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    assert_eq!(publication.settle(), Some(PublishJob::Save(larger())));
    edit(&mut publication, resize_to(largest()));
    assert_eq!(publication.settle(), None, "one write at a time");

    assert_eq!(
        confirm(&mut publication, larger()),
        Some(PublishJob::Save(largest())),
        "the owed write goes out with the answer"
    );
    assert_eq!(*publication.live(), largest());
    assert_eq!(confirm(&mut publication, largest()), None);
    assert_eq!(*publication.adopted(), largest());
}

/// A restore asked for behind an outstanding write is carried out, never
/// displaced by a save asked for after it — and that save writes on top of
/// what the restore left.
#[test]
fn a_restore_behind_a_write_is_not_displaced_by_a_later_save() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    assert_eq!(publication.restore(), None, "owed behind the write");
    edit(&mut publication, set_opacity(500));
    assert_eq!(publication.settle(), None, "owed behind the restore");

    assert_eq!(
        confirm(&mut publication, larger()),
        Some(PublishJob::Restore)
    );
    let restored = Profile {
        scheme: Scheme::Contrast,
        ..Profile::default()
    };
    let mut expected = restored;
    expected.effects.opacity = 500;
    assert_eq!(
        confirm(&mut publication, restored),
        Some(PublishJob::Save(expected))
    );
}

/// Asking for the defaults drops the edits not yet written along with the
/// user's other opinions.
#[test]
fn an_edit_made_before_a_restore_is_dropped_with_the_users_opinions() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.settle();
    edit(&mut publication, resize_to(largest()));
    assert_eq!(publication.settle(), None);
    assert_eq!(publication.restore(), None);

    assert_eq!(
        confirm(&mut publication, larger()),
        Some(PublishJob::Restore)
    );
    assert_eq!(
        confirm(&mut publication, Profile::default()),
        None,
        "nothing asked for after the restore is left to write"
    );
    assert_eq!(*publication.live(), Profile::default());
}

/// An editor whose copy has fallen behind — a second window's sheet, opened
/// before a menu command changed the size — changes only what it edited.
#[test]
fn an_editor_holding_a_stale_copy_changes_only_what_it_edited() {
    let mut publication = Publication::new(Profile::default());
    let stale = *publication.live();
    edit(&mut publication, resize_to(larger()));

    let mut edited = stale;
    edited.effects.opacity = 500;
    publication.edit(&stale, &edited);
    let live = *publication.live();
    assert_eq!(
        live.font_size_px,
        larger().font_size_px,
        "the stale size stayed out"
    );
    assert_eq!(live.effects.opacity, 500);
}

// --- What the screen still owes -----------------------------------------------

/// The compute half of the same regression: a burst of edits between two
/// paints costs **one** paint, because what is owed is measured against the
/// screen rather than against each edit.
#[test]
fn a_burst_of_edits_owes_the_screen_one_paint() {
    let mut publication = Publication::new(Profile::default());
    for steps in 1..=8 {
        edit(&mut publication, resize_to(sized(steps)));
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

/// An edit the surface never drew stays owed. Measuring against each edit
/// instead would let a change slip through whenever the outcome that carried
/// it lost to another.
#[test]
fn an_undrawn_edit_stays_owed_until_it_is_drawn() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    edit(&mut publication, resize_to(Profile::default()));
    assert!(
        !publication.take_pending().any(),
        "an edit returned to what the screen already shows owes nothing"
    );
    edit(&mut publication, resize_to(larger()));
    assert!(publication.take_pending().metrics(), "but a real one does");
}

/// Adopting the store's answer leaves the screen owing the difference from
/// what it is *showing*, not from what the widget last asked for.
#[test]
fn adopting_owes_the_difference_from_the_screen() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.take_pending();
    let _ = publication.settle();
    let _ = confirm(&mut publication, largest());
    assert!(
        publication.take_pending().metrics(),
        "the screen owes the difference from the size it is showing"
    );
}

/// A refused write reverts the edit, and the screen owes that revert —
/// otherwise a window keeps showing a look the next start would not restore.
#[test]
fn a_refused_write_owes_the_revert() {
    let mut publication = Publication::new(Profile::default());
    edit(&mut publication, resize_to(larger()));
    let _ = publication.take_pending();
    let _ = publication.settle();
    let _ = refuse(&mut publication, Errno::PermissionDenied);
    assert!(
        publication.take_pending().metrics(),
        "the screen owes the size it went back to"
    );
}
