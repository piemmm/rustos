//! Heterogeneous (`big.LITTLE`) core classification for aarch64.
//!
//! Arm asymmetric parts mix high-throughput `big` cores with low-power
//! `LITTLE` cores. The scheduler needs each logical CPU's
//! [`rustos_arch_api::CoreClass`] so it can keep background work on the
//! efficiency cores and migrate
//! throughput-bound work onto the performance cores
//! (`docs/src/architecture/scheduler.md`).
//!
//! Detection is the architecture port's job.
//! Unlike x86_64 — where each core reports its own class through a
//! per-core CPUID leaf — Arm advertises the per-core capacity in the
//! flattened device tree: each `/cpus/cpu@*` node carries an optional
//! `capacity-dmips-mhz` rating (Devicetree Specification, the CPU
//! capacity binding). [`crate::kernel_arch::Aarch64Arch`] reads those
//! ratings through [`rustos_fdt::Fdt::each_cpu`] and classifies them
//! here.
//!
//! The classifier is pure and host-testable; the device-tree read that
//! feeds it is itself host-testable against the `rustos_fdt` fixture
//! builder, so no part of heterogeneous-core discovery needs a
//! freestanding target (no fake hardware in production
//! paths).
//!
//! It fails conservative: a homogeneous machine (every
//! advertised rating equal, or no ratings at all) classifies every core
//! as [`rustos_arch_api::CoreClass::Performance`], the safe homogeneous
//! default the Arch HAL mandates for
//! [`rustos_arch_api::SchedulerArch::core_class`]. A core
//! with no advertised rating on an otherwise heterogeneous part is also
//! treated as a performance core rather than guessed.

use rustos_arch_api::CoreClass;

/// Classify one core from its `capacity-dmips-mhz` rating against the
/// machine's peak rating.
///
/// `capacity` is the DMIPS-per-MHz rating advertised for the core, or
/// `None` when the device tree advertised none for it; `peak` is the
/// highest rating advertised by any classified core (`None` when no core
/// advertised one). A core rated *strictly below* the peak is an
/// [`CoreClass::Efficiency`] core; a core at the peak — or one with no
/// advertised rating, or on a machine where no core advertised one — is a
/// [`CoreClass::Performance`] core (the homogeneous default).
///
/// This is the pure, host-testable heart of the classifier;
/// [`crate::kernel_arch::Aarch64Arch::classify_from_fdt`] computes `peak`
/// in one device-tree pass and calls this per core in a second, so the
/// classification needs no fixed-size buffer and scales to any CPU count.
#[must_use]
pub fn class_for_capacity(capacity: Option<u64>, peak: Option<u64>) -> CoreClass {
    match (capacity, peak) {
        (Some(rating), Some(peak)) if rating < peak => CoreClass::Efficiency,
        _ => CoreClass::Performance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The peak rating over a set of advertised capacities, mirroring the
    /// pass-1 reduction in `classify_from_fdt` so the test exercises the
    /// same `peak`/`class_for_capacity` pairing the production path uses.
    fn peak_of(capacities: &[Option<u64>]) -> Option<u64> {
        capacities.iter().flatten().copied().max()
    }

    #[test]
    fn no_capacities_is_homogeneous_performance() {
        let caps = [None, None, None];
        let peak = peak_of(&caps);
        assert!(caps
            .iter()
            .all(|&c| class_for_capacity(c, peak).is_performance()));
    }

    #[test]
    fn equal_capacities_are_all_performance() {
        let caps = [Some(1024), Some(1024)];
        let peak = peak_of(&caps);
        assert!(class_for_capacity(caps[0], peak).is_performance());
        assert!(class_for_capacity(caps[1], peak).is_performance());
    }

    #[test]
    fn lower_capacity_cores_are_efficiency() {
        // A 2+2 big.LITTLE layout: peak 1024 = big, 512 = LITTLE.
        let caps = [Some(1024), Some(1024), Some(512), Some(512)];
        let peak = peak_of(&caps);
        assert!(class_for_capacity(caps[0], peak).is_performance());
        assert!(class_for_capacity(caps[1], peak).is_performance());
        assert!(class_for_capacity(caps[2], peak).is_efficiency());
        assert!(class_for_capacity(caps[3], peak).is_efficiency());
    }

    #[test]
    fn missing_rating_on_a_hetero_part_defaults_to_performance() {
        // One big core advertises a rating; the other core advertises
        // none and is not guessed down to efficiency.
        let caps = [Some(1024), None, Some(512)];
        let peak = peak_of(&caps);
        assert!(class_for_capacity(caps[0], peak).is_performance());
        assert!(class_for_capacity(caps[1], peak).is_performance());
        assert!(class_for_capacity(caps[2], peak).is_efficiency());
    }

    #[test]
    fn three_tier_capacities_only_the_peak_is_performance() {
        // DynamIQ parts can expose three tiers; only the top tier is a
        // performance core, every lower tier is efficiency.
        let caps = [Some(1024), Some(768), Some(512)];
        let peak = peak_of(&caps);
        assert!(class_for_capacity(caps[0], peak).is_performance());
        assert!(class_for_capacity(caps[1], peak).is_efficiency());
        assert!(class_for_capacity(caps[2], peak).is_efficiency());
    }
}
