//! Aggregating member device properties into one array-level answer.
//!
//! A composed array is itself a device, so it must answer a consumer's
//! questions about *itself* from what its members report rather than inherit
//! a trait default that hides them. This module is the *one* definition of
//! how each such property folds, shared by every composition
//! (`AGENTS.md` §2.2) so they cannot answer differently: health telemetry
//! ([`aggregate_device_health`]) and the device class the consumer derives
//! its I/O budget from ([`aggregate_device_class`]).
//!
//! # Health telemetry
//!
//! Every RAID composition ([`MirrorArray`](crate::MirrorArray),
//! [`StripeArray`](crate::StripeArray), [`ParityArray`](crate::ParityArray),
//! [`DualParityArray`](crate::DualParityArray)) is itself a
//! [`Block`](tairix_abi::driver::block::Block), so a consumer that schedules a
//! scrub from a device's `SMART` / `NVMe` telemetry
//! (`docs/src/filesystem/arxfs-spec.md` §11) queries the *array* through
//! [`Block::device_health`](tairix_abi::driver::block::Block::device_health)
//! and must still see the health of the disks underneath it. A composed array
//! that inherited the trait's default (`Unavailable`) would silently hide
//! every member's telemetry — a failing disk in an array would look like a
//! device with no health data at all (`AGENTS.md` §26.5 "a disk may be
//! failing"). This module is the *one* definition of how member telemetry is
//! folded into the array-level answer, shared by all four compositions
//! (`AGENTS.md` §2.2) so they cannot aggregate health differently.
//!
//! # How the counters fold
//!
//! The [`HealthSnapshot`] counters are monotonic over a device's lifetime, and
//! a consumer compares successive array snapshots against a stored baseline to
//! decide whether a *new* fault has appeared since. The fold is chosen so that
//! comparison stays meaningful for an array:
//!
//! * **Independent per-device faults are summed** — `media_errors`,
//!   `reallocated_sectors`, `pending_sectors`, `uncorrectable_sectors`, and
//!   `crc_errors`. Each member's integrity errors are its own, so the array's
//!   total is their sum; a rise in the sum is what schedules a deep scrub.
//!   Sums saturate rather than wrap, so a very wide array of very old disks can
//!   never overflow a counter into a smaller value (`AGENTS.md` §2.9, §26.6).
//! * **Shared / whole-array conditions take the worst member** — an unclean
//!   shutdown or a power-on interval hits every member together, so
//!   `unsafe_shutdowns` and `power_on_hours` are the maximum (summing would
//!   multiply one array-wide power loss by the member count and fabricate a
//!   fault that never happened). `percentage_used` and `temperature_kelvin`
//!   are the maximum because the most-worn / hottest member bounds the array,
//!   and `available_spare` is the minimum because the member with the least
//!   spare is the array's weakest link. `critical_warning` is the logical OR:
//!   the array is critical the moment *any* member is.
//!
//! # What counts as a member
//!
//! Only live, participating devices contribute: an in-sync copy and a
//! resyncing copy (a real device being rebuilt) report valid telemetry, while
//! a faulted-and-dropped slot and an empty (absent) slot have none. A member
//! that itself reports [`DeviceHealth::Unavailable`], or whose health read
//! *errors*, is skipped rather than failing the whole array-level query: a
//! single member with no telemetry, or a transient telemetry read fault, never
//! denies the consumer the health of the members that *can* be read
//! (`AGENTS.md` §26.5 degrade gracefully). Only when *no* participating member
//! exposes telemetry does the array report [`DeviceHealth::Unavailable`], so an
//! absence of data is never mistaken for a perfectly-healthy array (the ABI's
//! "recorded, not failed" contract).

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{DeviceHealth, HealthSnapshot};
use tairix_abi::driver::DriverError;

use tairix_abi::raid::MemberState;

/// Whether a redundant array member contributes to the array-level answers
/// this module folds.
///
/// An in-sync copy and a resyncing one are real devices the array is driving,
/// so both speak for it; a faulted-and-dropped slot and an empty (absent) one
/// are not devices at all and contribute nothing. Every composition and every
/// folded property share this one predicate, so an array can never report its
/// health from one set of members and its class from another.
pub(crate) fn member_participates(state: MemberState) -> bool {
    matches!(state, MemberState::InSync | MemberState::Resyncing)
}

/// Fold the [`device_health`](tairix_abi::driver::block::Block::device_health)
/// results of an array's live members into one array-level [`DeviceHealth`].
///
/// See the module documentation for the per-counter fold rules and for which
/// members contribute. The iterator yields one result per *participating*
/// member (the array selects those); an [`Err`] or an
/// [`DeviceHealth::Unavailable`] member is skipped, and the array reports
/// [`DeviceHealth::Unavailable`] only when no member yields a snapshot.
pub(crate) fn aggregate_device_health<I>(members: I) -> DeviceHealth
where
    I: IntoIterator<Item = Result<DeviceHealth, DriverError>>,
{
    let mut acc: Option<HealthSnapshot> = None;
    for result in members {
        // A member whose telemetry could not be read (an errored query) or
        // that exposes none (`Unavailable`) contributes nothing, but never
        // denies the array the health of the members that can be read.
        if let Ok(DeviceHealth::Available(snapshot)) = result {
            acc = Some(match acc {
                None => snapshot,
                Some(current) => merge(current, snapshot),
            });
        }
    }
    match acc {
        Some(snapshot) => DeviceHealth::Available(snapshot),
        None => DeviceHealth::Unavailable,
    }
}

/// Fold the [`device_class`](tairix_abi::driver::block::Block::device_class)
/// of an array's live members into the one class the array declares.
///
/// An array answers only as fast as the slowest member it is waiting on, so
/// it declares the *most patient* member's class
/// ([`BlkDeviceClass::most_patient`]): a mirror of an SSD and a spinning disk
/// must be given the spinning disk's spin-up budget, or a consumer would time
/// out a perfectly healthy array whenever the slow member answered a read.
/// The fold is commutative, so member order cannot change the answer.
///
/// The iterator yields one class per *participating* member (the array
/// selects those, exactly as for the health fold). An array with no live
/// member declares the bounded unclassified envelope
/// ([`BlkDeviceClass::Virtual`]) rather than the widest one: such an array
/// can serve nothing anyway, and the bounded budget fails its callers closed
/// sooner instead of making them wait out a disk that is not there.
pub(crate) fn aggregate_device_class<I>(members: I) -> BlkDeviceClass
where
    I: IntoIterator<Item = BlkDeviceClass>,
{
    members
        .into_iter()
        .reduce(BlkDeviceClass::most_patient)
        .unwrap_or(BlkDeviceClass::Virtual)
}

/// Merge one more member `snapshot` into the running array-level `acc`,
/// applying the per-counter fold rules documented on the module.
fn merge(acc: HealthSnapshot, snapshot: HealthSnapshot) -> HealthSnapshot {
    HealthSnapshot {
        // Shared / whole-array conditions: the worst member speaks for the
        // array (summing an array-wide event would fabricate a larger fault).
        power_on_hours: acc.power_on_hours.max(snapshot.power_on_hours),
        unsafe_shutdowns: acc.unsafe_shutdowns.max(snapshot.unsafe_shutdowns),
        percentage_used: acc.percentage_used.max(snapshot.percentage_used),
        temperature_kelvin: acc.temperature_kelvin.max(snapshot.temperature_kelvin),
        available_spare: acc.available_spare.min(snapshot.available_spare),
        critical_warning: acc.critical_warning || snapshot.critical_warning,
        // Independent per-device integrity faults: the array's total is their
        // sum, saturating so a wide array of old disks can never wrap.
        media_errors: acc.media_errors.saturating_add(snapshot.media_errors),
        reallocated_sectors: acc
            .reallocated_sectors
            .saturating_add(snapshot.reallocated_sectors),
        pending_sectors: acc.pending_sectors.saturating_add(snapshot.pending_sectors),
        uncorrectable_sectors: acc
            .uncorrectable_sectors
            .saturating_add(snapshot.uncorrectable_sectors),
        crc_errors: acc.crc_errors.saturating_add(snapshot.crc_errors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> HealthSnapshot {
        HealthSnapshot {
            power_on_hours: 0,
            unsafe_shutdowns: 0,
            media_errors: 0,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_sectors: 0,
            crc_errors: 0,
            percentage_used: 0,
            available_spare: 100,
            temperature_kelvin: 300,
            critical_warning: false,
        }
    }

    #[test]
    fn no_members_is_unavailable() {
        assert_eq!(
            aggregate_device_health(core::iter::empty()),
            DeviceHealth::Unavailable
        );
    }

    #[test]
    fn all_members_without_telemetry_is_unavailable() {
        let members = [Ok(DeviceHealth::Unavailable), Ok(DeviceHealth::Unavailable)];
        assert_eq!(aggregate_device_health(members), DeviceHealth::Unavailable);
    }

    #[test]
    fn an_errored_or_absent_member_never_denies_the_readable_ones() {
        let mut only = snapshot();
        only.media_errors = 7;
        let members = [
            Err(DriverError::DeviceFault),
            Ok(DeviceHealth::Unavailable),
            Ok(DeviceHealth::Available(only)),
        ];
        assert_eq!(
            aggregate_device_health(members),
            DeviceHealth::Available(only)
        );
    }

    #[test]
    fn integrity_faults_sum_and_shared_conditions_take_the_worst() {
        let mut a = snapshot();
        a.media_errors = 3;
        a.reallocated_sectors = 10;
        a.pending_sectors = 1;
        a.uncorrectable_sectors = 2;
        a.crc_errors = 4;
        a.power_on_hours = 100;
        a.unsafe_shutdowns = 2;
        a.percentage_used = 40;
        a.temperature_kelvin = 305;
        a.available_spare = 90;
        a.critical_warning = false;

        let mut b = snapshot();
        b.media_errors = 5;
        b.reallocated_sectors = 1;
        b.pending_sectors = 0;
        b.uncorrectable_sectors = 6;
        b.crc_errors = 1;
        b.power_on_hours = 80;
        b.unsafe_shutdowns = 5;
        b.percentage_used = 70;
        b.temperature_kelvin = 300;
        b.available_spare = 60;
        b.critical_warning = true;

        let DeviceHealth::Available(got) = aggregate_device_health([
            Ok(DeviceHealth::Available(a)),
            Ok(DeviceHealth::Available(b)),
        ]) else {
            panic!("expected an aggregated snapshot");
        };

        // Independent integrity faults sum.
        assert_eq!(got.media_errors, 8);
        assert_eq!(got.reallocated_sectors, 11);
        assert_eq!(got.pending_sectors, 1);
        assert_eq!(got.uncorrectable_sectors, 8);
        assert_eq!(got.crc_errors, 5);
        // Shared / whole-array conditions take the worst member.
        assert_eq!(got.power_on_hours, 100);
        assert_eq!(got.unsafe_shutdowns, 5);
        assert_eq!(got.percentage_used, 70);
        assert_eq!(got.temperature_kelvin, 305);
        assert_eq!(got.available_spare, 60);
        assert!(got.critical_warning);
    }

    #[test]
    fn summed_counters_saturate_rather_than_wrap() {
        let mut a = snapshot();
        a.media_errors = u64::MAX;
        let mut b = snapshot();
        b.media_errors = 5;
        let DeviceHealth::Available(got) = aggregate_device_health([
            Ok(DeviceHealth::Available(a)),
            Ok(DeviceHealth::Available(b)),
        ]) else {
            panic!("expected an aggregated snapshot");
        };
        assert_eq!(got.media_errors, u64::MAX);
    }

    #[test]
    fn only_in_sync_and_resyncing_members_participate() {
        // Both are real devices the array is driving; a dropped or empty
        // slot is not a device at all.
        assert!(member_participates(MemberState::InSync));
        assert!(member_participates(MemberState::Resyncing));
        assert!(!member_participates(MemberState::Faulted));
        assert!(!member_participates(MemberState::Absent));
    }

    #[test]
    fn no_members_declares_the_bounded_unclassified_envelope() {
        // An array that can serve nothing fails its callers closed sooner
        // rather than making them wait out the widest budget.
        assert_eq!(
            aggregate_device_class(core::iter::empty()),
            BlkDeviceClass::Virtual
        );
    }

    #[test]
    fn the_array_declares_its_most_patient_member() {
        // The array answers only as fast as the slowest member it waits on.
        assert_eq!(
            aggregate_device_class([BlkDeviceClass::SolidState, BlkDeviceClass::Rotational]),
            BlkDeviceClass::Rotational
        );
        // Commutative: member order cannot change the array's class.
        assert_eq!(
            aggregate_device_class([BlkDeviceClass::Rotational, BlkDeviceClass::SolidState]),
            BlkDeviceClass::Rotational
        );
        // A lone member speaks for the whole array.
        assert_eq!(
            aggregate_device_class([BlkDeviceClass::Removable]),
            BlkDeviceClass::Removable
        );
        // The fold is over the whole set, not just the first pair.
        assert_eq!(
            aggregate_device_class([
                BlkDeviceClass::SolidState,
                BlkDeviceClass::Virtual,
                BlkDeviceClass::Removable,
                BlkDeviceClass::SolidState,
            ]),
            BlkDeviceClass::Removable
        );
    }
}
