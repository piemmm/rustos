//! Capacity policy for the compressed anonymous-memory tier
//! (`plans/SWAPSWAPSWAP.md` section 4).
//!
//! The tier's footprint is bounded by three derived figures — a
//! minimum capacity *guarantee*, a soft cap for ordinary pressure
//! relief, and a hard cap for emergency growth — all fractions of the
//! physical RAM actually discovered at boot, never free-standing magic
//! numbers. The guarantee is exactly that: a promise that admission
//! policy will always leave the tier at least this much headroom under
//! sudden pressure. It is **not** an eager allocation; the tier starts
//! empty and every byte is charged lazily as pages are compressed.
//!
//! The policy also owns the *decompression floor*: the free-memory
//! level compression must never push the system below, so a compressed
//! page can always be restored (a fault-in needs a free frame plus
//! working headroom). Preserving the floor by refusing to compress is
//! a normal typed refusal, never a panic.

use tairix_reclaim::PressureBand;

use crate::frame::PAGE_SIZE;

/// Denominator of the minimum-capacity fraction: 1% of physical RAM.
const MIN_DIVISOR: usize = 100;

/// Absolute floor of the minimum capacity guarantee: 64 MiB, the
/// plan's "somewhere safe for cold pages under sudden pressure" on
/// small boards (clamped to the hard cap on machines where 64 MiB
/// would exceed it).
const MIN_FLOOR_BYTES: usize = 64 * 1024 * 1024;

/// Denominator of the soft cap: 10% of physical RAM. Normal pressure
/// relief (the moderate band) stays under this.
const SOFT_DIVISOR: usize = 10;

/// Denominator of the hard cap: 25% of physical RAM. Emergency growth
/// (the severe band) may reach but never exceed this.
const HARD_DIVISOR: usize = 4;

/// Pages of working headroom kept above the pressure reserve so a
/// compressed-page fault always has a free frame plus room for the
/// bounded cluster restore ([`super::tier`]'s cluster budget) without
/// touching the emergency reserve.
const DECOMPRESSION_HEADROOM_PAGES: usize = 8;

/// The derived capacity policy: minimum guarantee, soft cap, hard cap.
///
/// Invariants (established at construction, hold for every input):
/// `min <= hard`, `soft <= hard`, and a zero backing yields all-zero
/// caps — an unknown machine admits nothing (fail closed).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RamzipCaps {
    min: usize,
    soft: usize,
    hard: usize,
}

impl RamzipCaps {
    /// Derive the caps from the byte size of discovered physical RAM.
    ///
    /// `min = max(1% of RAM, 64 MiB)` clamped to the hard cap;
    /// `soft = 10%`; `hard = 25%`. On a machine small enough that
    /// 64 MiB exceeds the 10% soft cap, the guarantee wins:
    /// [`Self::band_cap`] never reports less than `min` while
    /// compression is permitted at all. A zero `total` produces
    /// all-zero caps.
    #[must_use]
    pub const fn from_physical(total: usize) -> Self {
        let hard = total / HARD_DIVISOR;
        let soft = total / SOFT_DIVISOR;
        let fraction = total / MIN_DIVISOR;
        let wanted_min = if fraction > MIN_FLOOR_BYTES {
            fraction
        } else {
            MIN_FLOOR_BYTES
        };
        let min = if wanted_min > hard { hard } else { wanted_min };
        Self { min, soft, hard }
    }

    /// The minimum capacity guarantee, in bytes.
    #[must_use]
    pub const fn min(&self) -> usize {
        self.min
    }

    /// The soft cap, in bytes: the ceiling for ordinary (moderate-band)
    /// pressure relief.
    #[must_use]
    pub const fn soft(&self) -> usize {
        self.soft
    }

    /// The hard cap, in bytes: the ceiling for emergency (severe-band)
    /// growth. Never exceeded, ever.
    #[must_use]
    pub const fn hard(&self) -> usize {
        self.hard
    }

    /// The tier's byte ceiling at `band`.
    ///
    /// Compression is only ever attempted where
    /// [`crate::pressure::ramzip_handoff`] permits it (moderate with
    /// caches drained, or severe); this cap bounds the tier's footprint
    /// there. At moderate pressure the ceiling is the soft cap, raised
    /// to the minimum guarantee where the guarantee is larger (small
    /// machines); at severe pressure the tier may grow toward the hard
    /// cap. Every other band reports zero — no growth is admissible
    /// there at all.
    #[must_use]
    pub const fn band_cap(&self, band: PressureBand) -> usize {
        match band {
            PressureBand::Moderate => {
                if self.min > self.soft {
                    self.min
                } else {
                    self.soft
                }
            }
            PressureBand::Severe => self.hard,
            PressureBand::Normal | PressureBand::Mild | PressureBand::Critical => 0,
        }
    }

    /// The largest share of `band_cap` one task may hold: half.
    ///
    /// Per-task fairness (`plans/SWAPSWAPSWAP.md` section 10): one
    /// process cannot push unlimited cold memory into the tier and
    /// externalise the cost. The v1 policy is deliberately simple —
    /// half the active ceiling — and is enforced per admission.
    #[must_use]
    pub const fn task_share(&self, band: PressureBand) -> usize {
        self.band_cap(band) / 2
    }
}

/// The free-memory floor compression must never push the system below:
/// the pressure reserve plus fixed working headroom for fault-in
/// restores. `reserve` is [`tairix_reclaim::PressureThresholds::reserve`].
#[must_use]
pub const fn decompression_floor(reserve: usize) -> usize {
    reserve.saturating_add(DECOMPRESSION_HEADROOM_PAGES * PAGE_SIZE)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const GIB: usize = 1 << 30;

    #[test]
    fn caps_scale_with_discovered_ram() {
        let caps = RamzipCaps::from_physical(8 * GIB);
        assert_eq!(caps.hard(), 2 * GIB);
        assert_eq!(caps.soft(), 8 * GIB / 10);
        // 1% of 8 GiB (~82 MiB) exceeds the 64 MiB floor.
        assert_eq!(caps.min(), 8 * GIB / 100);
        assert!(caps.min() <= caps.soft());
    }

    #[test]
    fn small_board_minimum_floor_applies_and_clamps_to_hard() {
        // 512 MiB: 1% is 5.12 MiB, so the 64 MiB floor wins; the hard
        // cap (128 MiB) still bounds it.
        let caps = RamzipCaps::from_physical(512 * 1024 * 1024);
        assert_eq!(caps.min(), MIN_FLOOR_BYTES);
        assert!(caps.min() <= caps.hard());
        // 128 MiB machine: the floor would exceed the 32 MiB hard cap,
        // so the guarantee clamps to it.
        let tiny = RamzipCaps::from_physical(128 * 1024 * 1024);
        assert_eq!(tiny.min(), tiny.hard());
    }

    #[test]
    fn zero_backing_fails_closed_to_zero_caps() {
        let caps = RamzipCaps::from_physical(0);
        assert_eq!(caps.min(), 0);
        assert_eq!(caps.soft(), 0);
        assert_eq!(caps.hard(), 0);
        for band in PressureBand::ALL {
            assert_eq!(caps.band_cap(band), 0);
        }
    }

    #[test]
    fn band_caps_follow_the_pressure_policy() {
        let caps = RamzipCaps::from_physical(8 * GIB);
        assert_eq!(caps.band_cap(PressureBand::Normal), 0);
        assert_eq!(caps.band_cap(PressureBand::Mild), 0);
        assert_eq!(caps.band_cap(PressureBand::Moderate), caps.soft());
        assert_eq!(caps.band_cap(PressureBand::Severe), caps.hard());
        assert_eq!(caps.band_cap(PressureBand::Critical), 0);
    }

    #[test]
    fn minimum_guarantee_raises_the_moderate_cap_on_small_machines() {
        // 512 MiB: soft (51 MiB) < min (64 MiB); the guarantee wins.
        let caps = RamzipCaps::from_physical(512 * 1024 * 1024);
        assert!(caps.min() > caps.soft());
        assert_eq!(caps.band_cap(PressureBand::Moderate), caps.min());
    }

    #[test]
    fn task_share_is_half_the_active_ceiling() {
        let caps = RamzipCaps::from_physical(8 * GIB);
        assert_eq!(
            caps.task_share(PressureBand::Moderate),
            caps.band_cap(PressureBand::Moderate) / 2
        );
        assert_eq!(caps.task_share(PressureBand::Severe), caps.hard() / 2);
        assert_eq!(caps.task_share(PressureBand::Normal), 0);
    }

    #[test]
    fn decompression_floor_sits_above_the_reserve() {
        let reserve = GIB / 64;
        let floor = decompression_floor(reserve);
        assert!(floor > reserve);
        assert_eq!(floor - reserve, DECOMPRESSION_HEADROOM_PAGES * PAGE_SIZE);
        // Saturates rather than wrapping on an absurd reserve.
        assert_eq!(decompression_floor(usize::MAX), usize::MAX);
    }
}
