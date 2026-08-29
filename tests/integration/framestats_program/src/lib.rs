//! The `framestats` fixture's sample vocabulary (`plans/FIX-DESKTOP-SPEEDUP.md`
//! A.4).
//!
//! The fixture program (`src/run.rs`) reads the desktop's published
//! [`DesktopFrameTotals`] and re-emits the counters as one structured record;
//! the desktop-hover vertical's freestanding guest sink decodes that record
//! and differences two of them. Both sides read this module, so the spelling
//! the emitter authors and the spelling the decoder expects cannot drift.
//!
//! # Why a log record rather than the reply itself
//!
//! The query is userland IPC, which a freestanding test kernel cannot issue.
//! The kernel *does* observe every record a task emits through the system log,
//! so a userland client that samples the query and re-emits the counters is
//! the whole bridge — no test hook in the desktop, no second reading of the
//! ABI, and the numbers land in the serial transcript where a failing run is
//! diagnosable.
//!
//! # Why eight counters
//!
//! `abi-v1` bounds one log record to eight fields, so the record carries
//! exactly the counters the hover gate reads and no others. Everything else in
//! [`DesktopFrameTotals`] answers a different question and is served to any
//! reader by the query itself.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_abi::sysinfo::DesktopFrameTotals;
use tairix_log::{Event, EventId, Field, FieldValue, Level};

/// Record the fixture emits per successful sample.
pub const SAMPLE_EVENT: EventId = EventId(4510);

/// Record the fixture emits when it cannot sample at all.
pub const SAMPLE_FAILED_EVENT: EventId = EventId(4511);

/// Message of a successful sample — the marker the host runner gates the
/// hover on, and the one the guest sink counts.
pub const SAMPLE_MESSAGE: &str = "framestats desktop frame sample";

/// Message of a failed sample. The guest sink fails the run on sight of it
/// rather than waiting for a second sample that will never come.
pub const SAMPLE_FAILED_MESSAGE: &str = "framestats could not sample the desktop frame accounting";

/// Exit status of a run that could not sample.
pub const SAMPLE_FAILED_STATUS: i32 = 1;

/// Field keys of one sample record, in field order.
pub mod field {
    /// [`super::Sample::screen_px`].
    pub const SCREEN_PX: &str = "screen_px";
    /// [`super::Sample::frames`].
    pub const FRAMES: &str = "frames";
    /// [`super::Sample::damaged_px`].
    pub const DAMAGED_PX: &str = "damaged_px";
    /// [`super::Sample::blended_px`].
    pub const BLENDED_PX: &str = "blended_px";
    /// [`super::Sample::blur_px`].
    pub const BLUR_PX: &str = "blur_px";
    /// [`super::Sample::dirty_rects`].
    pub const DIRTY_RECTS: &str = "dirty_rects";
    /// [`super::Sample::present_calls`].
    pub const PRESENT_CALLS: &str = "present_calls";
    /// [`super::Sample::chrome_misses`].
    pub const CHROME_MISSES: &str = "chrome_misses";
}

/// One reading of a compositing session's since-epoch frame accounting.
///
/// A projection of [`DesktopFrameTotals`] onto the counters the hover gate
/// reads. Every field is cumulative work, never a duration, so a difference of
/// two samples is exactly reproducible for a given sequence of frames and may
/// be asserted under any machine load.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Sample {
    /// Screen pixels the epoch's frames were composed against.
    pub screen_px: u64,
    /// Frames composed.
    pub frames: u64,
    /// Screen pixels those frames recomposed.
    pub damaged_px: u64,
    /// Layer contributions blended to resolve them.
    pub blended_px: u64,
    /// Pixels rewritten by a *recomputed* backdrop frost.
    pub blur_px: u64,
    /// Dirty rectangles those frames recomposed.
    pub dirty_rects: u64,
    /// Calls into the display driver that published them.
    pub present_calls: u64,
    /// Window-furniture lookups that had to be rendered.
    pub chrome_misses: u64,
}

impl Sample {
    /// Project the counters the gate reads out of a served record.
    #[must_use]
    pub const fn from_totals(totals: &DesktopFrameTotals) -> Self {
        Self {
            screen_px: totals.screen_px,
            frames: totals.frames,
            damaged_px: totals.damaged_px,
            blended_px: totals.blended_px,
            blur_px: totals.blur_px,
            dirty_rects: totals.dirty_rects,
            present_calls: totals.present_calls,
            chrome_misses: totals.chrome_misses,
        }
    }

    /// The record's fields, ready to log.
    #[must_use]
    pub const fn fields(&self) -> [Field<'static>; 8] {
        [
            Field {
                key: field::SCREEN_PX,
                value: FieldValue::UnsignedInt(self.screen_px),
            },
            Field {
                key: field::FRAMES,
                value: FieldValue::UnsignedInt(self.frames),
            },
            Field {
                key: field::DAMAGED_PX,
                value: FieldValue::UnsignedInt(self.damaged_px),
            },
            Field {
                key: field::BLENDED_PX,
                value: FieldValue::UnsignedInt(self.blended_px),
            },
            Field {
                key: field::BLUR_PX,
                value: FieldValue::UnsignedInt(self.blur_px),
            },
            Field {
                key: field::DIRTY_RECTS,
                value: FieldValue::UnsignedInt(self.dirty_rects),
            },
            Field {
                key: field::PRESENT_CALLS,
                value: FieldValue::UnsignedInt(self.present_calls),
            },
            Field {
                key: field::CHROME_MISSES,
                value: FieldValue::UnsignedInt(self.chrome_misses),
            },
        ]
    }

    /// Decode a sample from an emitted record, or `None` when `event` is not
    /// one — a foreign id, or a record missing any counter.
    ///
    /// Fails closed on a partial record: a decoder that defaulted an absent
    /// counter to zero would turn a truncated record into a difference that
    /// reads as an implausibly quiet desktop.
    #[must_use]
    pub fn from_event(event: &Event<'_>) -> Option<Self> {
        if event.id != SAMPLE_EVENT {
            return None;
        }
        let read = |key: &str| {
            event.fields.iter().find_map(|f| match f.value {
                FieldValue::UnsignedInt(v) if f.key == key => Some(v),
                _ => None,
            })
        };
        Some(Self {
            screen_px: read(field::SCREEN_PX)?,
            frames: read(field::FRAMES)?,
            damaged_px: read(field::DAMAGED_PX)?,
            blended_px: read(field::BLENDED_PX)?,
            blur_px: read(field::BLUR_PX)?,
            dirty_rects: read(field::DIRTY_RECTS)?,
            present_calls: read(field::PRESENT_CALLS)?,
            chrome_misses: read(field::CHROME_MISSES)?,
        })
    }

    /// The record to log for this sample.
    #[must_use]
    pub const fn event<'a>(fields: &'a [Field<'a>]) -> Event<'a> {
        Event {
            level: Level::Info,
            id: SAMPLE_EVENT,
            message: SAMPLE_MESSAGE,
            fields,
        }
    }

    /// The work done between `self` and the later `then`, or `None` when the
    /// two do not describe one continuous epoch.
    ///
    /// A counter that went backwards, or a screen extent that changed, means
    /// the desktop started a fresh epoch (or a different session published)
    /// between the samples: the pair then measures nothing and is refused
    /// rather than differenced into a plausible-looking figure.
    #[must_use]
    pub fn work_until(&self, then: &Self) -> Option<Delta> {
        if then.screen_px != self.screen_px {
            return None;
        }
        Some(Delta {
            screen_px: self.screen_px,
            frames: then.frames.checked_sub(self.frames)?,
            damaged_px: then.damaged_px.checked_sub(self.damaged_px)?,
            blended_px: then.blended_px.checked_sub(self.blended_px)?,
            blur_px: then.blur_px.checked_sub(self.blur_px)?,
            dirty_rects: then.dirty_rects.checked_sub(self.dirty_rects)?,
            present_calls: then.present_calls.checked_sub(self.present_calls)?,
            chrome_misses: then.chrome_misses.checked_sub(self.chrome_misses)?,
        })
    }
}

/// The work one continuous epoch did between two [`Sample`]s.
///
/// This — never a whole-epoch total — is what a gesture may be asserted
/// against: the desktop's bring-up composes a handful of full-screen frames,
/// so a cumulative mean and a cumulative peak are both bring-up's and say
/// nothing about the gesture that followed.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Delta {
    /// Screen pixels both samples were composed against.
    pub screen_px: u64,
    /// Frames composed between the samples.
    pub frames: u64,
    /// Screen pixels those frames recomposed.
    pub damaged_px: u64,
    /// Layer contributions blended to resolve them.
    pub blended_px: u64,
    /// Pixels rewritten by a recomputed backdrop frost.
    pub blur_px: u64,
    /// Dirty rectangles those frames recomposed.
    pub dirty_rects: u64,
    /// Calls into the display driver that published them.
    pub present_calls: u64,
    /// Window-furniture lookups that had to be rendered.
    pub chrome_misses: u64,
}

impl Delta {
    /// Screen pixels the average frame in this window recomposed, rounded up,
    /// or `None` for a window that composed no frame.
    ///
    /// Rounded up so a bound is never met by a truncation.
    #[must_use]
    pub const fn damage_per_frame(&self) -> Option<u64> {
        if self.frames == 0 {
            return None;
        }
        Some(self.damaged_px.div_ceil(self.frames))
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec::Vec;

    use super::{Delta, Sample, SAMPLE_EVENT, SAMPLE_FAILED_EVENT};
    use tairix_abi::sysinfo::DesktopFrameTotals;
    use tairix_log::{Event, Field, FieldValue, Level};

    fn totals() -> DesktopFrameTotals {
        DesktopFrameTotals {
            screen_px: 1024 * 768,
            frames: 40,
            damaged_px: 900_000,
            blended_px: 1_100_000,
            opaque_px: 400_000,
            blur_px: 7,
            encoded_px: 880_000,
            dirty_rects: 61,
            present_calls: 44,
            chrome_hits: 12,
            chrome_misses: 3,
            peak_damaged_px: 786_432,
            peak_blended_px: 900_000,
        }
    }

    #[test]
    fn a_sample_round_trips_through_its_own_record() {
        let sample = Sample::from_totals(&totals());
        let fields = sample.fields();
        let event = Sample::event(&fields);
        assert_eq!(event.id, SAMPLE_EVENT);
        assert_eq!(Sample::from_event(&event), Some(sample));
    }

    #[test]
    fn the_projection_takes_the_counters_the_gate_reads() {
        let full = totals();
        let sample = Sample::from_totals(&full);
        assert_eq!(sample.screen_px, full.screen_px);
        assert_eq!(sample.frames, full.frames);
        assert_eq!(sample.damaged_px, full.damaged_px);
        assert_eq!(sample.blended_px, full.blended_px);
        assert_eq!(sample.blur_px, full.blur_px);
        assert_eq!(sample.dirty_rects, full.dirty_rects);
        assert_eq!(sample.present_calls, full.present_calls);
        assert_eq!(sample.chrome_misses, full.chrome_misses);
    }

    #[test]
    fn a_foreign_record_is_not_a_sample() {
        let sample = Sample::from_totals(&totals());
        let fields = sample.fields();
        let foreign = Event {
            level: Level::Info,
            id: SAMPLE_FAILED_EVENT,
            message: "something else",
            fields: &fields,
        };
        assert_eq!(Sample::from_event(&foreign), None);
    }

    #[test]
    fn a_record_missing_a_counter_decodes_to_nothing() {
        let sample = Sample::from_totals(&totals());
        let all = sample.fields();
        for drop in 0..all.len() {
            let mut short: Vec<Field<'_>> = all.to_vec();
            short.remove(drop);
            let event = Sample::event(&short);
            assert_eq!(
                Sample::from_event(&event),
                None,
                "field {} absent must refuse the whole record",
                all[drop].key
            );
        }
    }

    #[test]
    fn a_counter_of_the_wrong_shape_decodes_to_nothing() {
        let sample = Sample::from_totals(&totals());
        let mut fields: Vec<Field<'_>> = sample.fields().to_vec();
        fields[1].value = FieldValue::Str("40");
        let event = Sample::event(&fields);
        assert_eq!(Sample::from_event(&event), None);
    }

    #[test]
    fn work_between_two_samples_is_their_difference() {
        let first = Sample::from_totals(&totals());
        let mut later = first;
        later.frames += 9;
        later.damaged_px += 45_000;
        later.blended_px += 60_000;
        later.dirty_rects += 11;
        later.present_calls += 9;
        let delta = first.work_until(&later).expect("one continuous epoch");
        assert_eq!(
            delta,
            Delta {
                screen_px: first.screen_px,
                frames: 9,
                damaged_px: 45_000,
                blended_px: 60_000,
                blur_px: 0,
                dirty_rects: 11,
                present_calls: 9,
                chrome_misses: 0,
            }
        );
        assert_eq!(delta.damage_per_frame(), Some(5000));
    }

    #[test]
    fn a_restarted_epoch_is_refused_rather_than_differenced() {
        let first = Sample::from_totals(&totals());
        let mut fresh = first;
        fresh.frames = 2;
        assert_eq!(first.work_until(&fresh), None, "a counter went backwards");

        let mut resized = first;
        resized.screen_px = 640 * 480;
        resized.frames += 3;
        assert_eq!(
            first.work_until(&resized),
            None,
            "counts against a different screen answer a different question"
        );
    }

    #[test]
    fn a_window_with_no_frame_has_no_mean() {
        let first = Sample::from_totals(&totals());
        let delta = first.work_until(&first).expect("the same sample twice");
        assert_eq!(delta.frames, 0);
        assert_eq!(delta.damage_per_frame(), None);
    }

    #[test]
    fn the_mean_rounds_up_so_a_bound_is_never_met_by_truncation() {
        let delta = Delta {
            screen_px: 1024 * 768,
            frames: 4,
            damaged_px: 9,
            ..Delta::default()
        };
        assert_eq!(delta.damage_per_frame(), Some(3));
    }
}
