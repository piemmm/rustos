//! The shared mount-record view model: what a volume holds, how full it is,
//! and how it is faring.
//!
//! Every surface that reports a mounted volume — `df`, `mount`, `sysmon`,
//! the desktop's Switchboard and its Settings — turns the same
//! [`MountRecord`](tairix_abi::sysinfo::MountRecord) into the same handful
//! of facts. The block counts become bytes, the bytes become a share, and
//! the availability becomes a word. Each of those is one derivation, so a
//! volume cannot read half-full on one surface and nearly-full on another.
//!
//! Two spellings of the availability exist on purpose: a bracketed marker
//! for the `mount(8)`-shaped listings, and prose for a desktop fact list.
//! They are two renderings of one classification, not two classifications.

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::filesystem::VolumeStats;
use tairix_abi::sysinfo::{MountAvailability, VolumeHealth};

/// What a mounted volume holds, in bytes.
///
/// A named record rather than a tuple: "total and available" and "used and
/// total" are both plausible readings of two byte figures, and a caller that
/// picks the wrong one reports a full disk as empty. Naming them makes that
/// mistake unrepresentable.
///
/// [`free`](Self::free) and [`available`](Self::available) are distinct
/// because a format may hold blocks back: `available` is what an ordinary
/// allocation may still consume, `free` is what is unallocated, and
/// `available <= free` always holds.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumeBytes {
    /// The volume's whole capacity.
    pub total: u64,
    /// What is unallocated on it, including any reserve the format
    /// withholds from ordinary allocation.
    pub free: u64,
    /// What an ordinary data allocation may still consume.
    pub available: u64,
}

impl VolumeBytes {
    /// What `stats` holds in bytes, or [`None`] when the format tracks no
    /// fixed capacity — the in-RAM layout mounts' all-zero accounting, and
    /// equally a positive block count denominated in a zero-byte block,
    /// which is no capacity either and must not read as a full disk of
    /// nothing.
    ///
    /// Saturating throughout, so a driver reporting an implausible block
    /// count yields a saturated figure rather than a wrapped one.
    #[must_use]
    pub const fn of(stats: &VolumeStats) -> Option<Self> {
        if stats.total_blocks == 0 || stats.block_size == 0 {
            return None;
        }
        let block = stats.block_size as u64;
        Some(Self {
            total: stats.total_blocks.saturating_mul(block),
            free: stats.free_blocks.saturating_mul(block),
            available: stats.avail_blocks.saturating_mul(block),
        })
    }

    /// What is allocated: the capacity less what is unallocated, saturating
    /// so a driver reporting more free than total reads as nothing used.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// The capacity an ordinary allocation sees: what is allocated plus what
    /// it may still consume, which excludes any reserve the format withholds.
    ///
    /// This is the denominator a `df`-style `Use%` is a fraction of; it is
    /// deliberately not [`total`](Self::total), which is the whole medium.
    #[must_use]
    pub const fn usable(self) -> u64 {
        self.used().saturating_add(self.available)
    }

    /// How full the *medium* is, in permille of [`total`](Self::total).
    ///
    /// The reading a capacity track shows. A `df`-style `Use%` asks a
    /// different question — how much of what a caller may *allocate* is
    /// gone — and divides by [`usable`](Self::usable) instead, so on a
    /// format that withholds a reserve it reads higher: the reserve is
    /// unallocated either way, but only this share counts it as part of
    /// the medium it physically is.
    ///
    /// Scaled in [`u128`], because a volume of the size TAIRiX must serve
    /// overflows a [`u64`] once multiplied by a thousand and a saturated
    /// numerator would under-report a full disk as very nearly empty.
    #[must_use]
    pub fn used_permille(self) -> u16 {
        if self.total == 0 {
            return 0;
        }
        let permille = u128::from(self.used()).saturating_mul(1000) / u128::from(self.total);
        u16::try_from(permille).unwrap_or(1000).min(1000)
    }

    /// The two volumes' figures summed, saturating: what a device holding
    /// both has between them.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            total: self.total.saturating_add(other.total),
            free: self.free.saturating_add(other.free),
            available: self.available.saturating_add(other.available),
        }
    }
}

/// A mount's availability as the bracketed marker a `mount(8)`-shaped
/// listing appends, or [`None`] for an available volume — which carries no
/// marker at all, so a healthy line stays byte-identical to the classic
/// shape.
///
/// The caller supplies its own separator and brackets, so a listing that
/// pads its columns differently shares the vocabulary rather than copying
/// it.
#[must_use]
pub const fn availability_marker(availability: MountAvailability) -> Option<&'static str> {
    match availability {
        MountAvailability::Available => None,
        MountAvailability::UnavailableDirty => Some("unavailable-dirty"),
        MountAvailability::UnavailableLost => Some("unavailable-lost"),
        MountAvailability::RecoveryConflict => Some("recovery-conflict"),
        MountAvailability::Degraded => Some("degraded"),
        MountAvailability::Recovering => Some("recovering"),
    }
}

/// A mount's availability as the prose a fact list reads.
#[must_use]
pub const fn availability_name(availability: MountAvailability) -> &'static str {
    match availability {
        MountAvailability::Available => "available",
        MountAvailability::UnavailableDirty => "unavailable (dirty)",
        MountAvailability::UnavailableLost => "unavailable (lost)",
        MountAvailability::RecoveryConflict => "recovery conflict",
        MountAvailability::Degraded => "degraded",
        MountAvailability::Recovering => "recovering",
    }
}

/// A volume's banded health as the one word a status badge carries.
#[must_use]
pub const fn volume_health_name(health: VolumeHealth) -> &'static str {
    match health {
        VolumeHealth::Healthy => "Healthy",
        VolumeHealth::Degraded => "Degraded",
        VolumeHealth::Failing => "Failing",
    }
}

/// The medium a volume lives on, or that the mount table did not classify
/// it.
#[must_use]
pub const fn medium_name(medium: Option<BlkDeviceClass>) -> &'static str {
    match medium {
        Some(BlkDeviceClass::Rotational) => "rotational",
        Some(BlkDeviceClass::SolidState) => "solid state",
        Some(BlkDeviceClass::Removable) => "removable",
        Some(BlkDeviceClass::Virtual) => "virtual",
        None => "unclassified",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        availability_marker, availability_name, medium_name, volume_health_name, VolumeBytes,
    };
    use tairix_abi::blkio::BlkDeviceClass;
    use tairix_abi::driver::filesystem::VolumeStats;
    use tairix_abi::sysinfo::{MountAvailability, VolumeHealth};

    fn stats(block_size: u32, total: u64, free: u64, avail: u64) -> VolumeStats {
        VolumeStats {
            block_size,
            total_blocks: total,
            free_blocks: free,
            avail_blocks: avail,
            ..VolumeStats::default()
        }
    }

    #[test]
    fn a_capacityless_volume_has_no_byte_figures() {
        assert_eq!(VolumeBytes::of(&VolumeStats::default()), None);
        // A block size with no blocks is still no capacity, never a
        // zero-byte disk — and neither is a block count denominated in a
        // zero-byte block.
        assert_eq!(VolumeBytes::of(&stats(4096, 0, 0, 0)), None);
        assert_eq!(VolumeBytes::of(&stats(0, 100, 50, 50)), None);
    }

    #[test]
    fn block_counts_scale_by_the_volumes_own_block_size() {
        let bytes = VolumeBytes::of(&stats(4096, 100, 40, 30)).expect("a sized volume");
        assert_eq!(bytes.total, 409_600);
        assert_eq!(bytes.free, 163_840);
        assert_eq!(bytes.available, 122_880);
        // Used counts the withheld reserve as occupied; usable does not.
        assert_eq!(bytes.used(), 245_760);
        assert_eq!(bytes.usable(), 368_640);
    }

    #[test]
    fn the_share_is_of_the_whole_medium_and_never_wraps() {
        let half = VolumeBytes::of(&stats(1, 1000, 500, 500)).expect("a sized volume");
        assert_eq!(half.used_permille(), 500);
        // A volume of the size TAIRiX must serve: scaling by a thousand
        // overflows a u64, so a narrower intermediate would report a full
        // disk as nearly empty.
        let huge = VolumeBytes::of(&stats(4096, 1 << 50, 1 << 46, 1 << 46)).expect("a huge volume");
        assert_eq!(huge.used_permille(), 937);
        // A driver reporting more free than total reads as nothing used
        // rather than underflowing.
        let odd = VolumeBytes::of(&stats(1, 10, 20, 20)).expect("a sized volume");
        assert_eq!(odd.used(), 0);
        assert_eq!(odd.used_permille(), 0);
        assert_eq!(VolumeBytes::default().used_permille(), 0);
        assert_eq!(
            VolumeBytes {
                total: u64::MAX,
                free: 0,
                available: 0,
            }
            .used_permille(),
            1000
        );
    }

    #[test]
    fn a_withheld_reserve_separates_the_two_shares() {
        // 1000 blocks, 200 unallocated, of which only 100 may be handed
        // out: 100 blocks are the format's own reserve.
        let held = VolumeBytes::of(&stats(1, 1000, 200, 100)).expect("a sized volume");
        assert_eq!(held.used(), 800, "a reserve is unallocated, not used");
        assert_eq!(held.usable(), 900, "a reserve is not allocatable either");
        // The medium is 80% full. `df` divides by what a caller may
        // allocate instead, so it reads higher on the same volume — which
        // is why neither share is the other's default.
        assert_eq!(held.used_permille(), 800);
        let df_permille = held.used() * 1000 / held.usable();
        assert!(df_permille > u64::from(held.used_permille()));
        assert_eq!(df_permille, 888);
        // With nothing withheld the two questions have the same answer.
        let plain = VolumeBytes::of(&stats(1, 1000, 200, 200)).expect("a sized volume");
        assert_eq!(plain.usable(), plain.total);
        assert_eq!(plain.used() * 1000 / plain.usable(), 800);
    }

    #[test]
    fn summing_two_volumes_saturates_rather_than_wrapping() {
        let a = VolumeBytes {
            total: 10,
            free: 4,
            available: 3,
        };
        let b = VolumeBytes {
            total: u64::MAX,
            free: u64::MAX,
            available: u64::MAX,
        };
        assert_eq!(
            a.plus(VolumeBytes {
                total: 30,
                free: 6,
                available: 5,
            }),
            VolumeBytes {
                total: 40,
                free: 10,
                available: 8,
            }
        );
        assert_eq!(a.plus(b).total, u64::MAX);
    }

    #[test]
    fn every_availability_has_both_spellings_and_only_available_has_no_marker() {
        const ALL: [MountAvailability; 6] = [
            MountAvailability::Available,
            MountAvailability::UnavailableDirty,
            MountAvailability::UnavailableLost,
            MountAvailability::RecoveryConflict,
            MountAvailability::Degraded,
            MountAvailability::Recovering,
        ];
        for state in ALL {
            let marker = availability_marker(state);
            assert_eq!(
                marker.is_none(),
                state == MountAvailability::Available,
                "{state:?} marker"
            );
            assert!(!availability_name(state).is_empty(), "{state:?} name");
            assert!(
                marker.is_none_or(|marker| !marker.contains(' ')),
                "{state:?}: a marker is one bracketed word, never prose"
            );
        }
        // The two spellings never collide: a listing's marker and a fact
        // list's prose are told apart by shape, not only by context.
        assert_eq!(
            availability_marker(MountAvailability::UnavailableDirty),
            Some("unavailable-dirty")
        );
        assert_eq!(
            availability_name(MountAvailability::UnavailableDirty),
            "unavailable (dirty)"
        );
    }

    #[test]
    fn health_bands_and_media_each_name_every_case() {
        for health in [
            VolumeHealth::Healthy,
            VolumeHealth::Degraded,
            VolumeHealth::Failing,
        ] {
            assert!(!volume_health_name(health).is_empty());
        }
        assert_eq!(volume_health_name(VolumeHealth::Failing), "Failing");
        for class in [
            BlkDeviceClass::Rotational,
            BlkDeviceClass::SolidState,
            BlkDeviceClass::Removable,
            BlkDeviceClass::Virtual,
        ] {
            assert_ne!(medium_name(Some(class)), medium_name(None));
        }
        assert_eq!(medium_name(None), "unclassified");
    }
}
