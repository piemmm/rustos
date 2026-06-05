//! Heterogeneous (`big.LITTLE`) core classification for aarch64.
//!
//! Arm asymmetric parts mix high-throughput `big` cores with low-power
//! `LITTLE` cores. The scheduler needs each logical CPU's
//! [`rustos_arch_api::CoreClass`] so it can keep background work on the
//! efficiency cores and migrate
//! throughput-bound work onto the performance cores
//! (`docs/src/architecture/scheduler.md`).
//!
//! Detection is the architecture port's job (`AGENTS.md` §17.2 / §18.2).
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
//! freestanding target (`AGENTS.md` §1 — no fake hardware in production
//! paths).
//!
//! It fails conservative (`AGENTS.md` §2.9): a homogeneous machine (every
//! advertised rating equal, or no ratings at all) classifies every core
//! as [`rustos_arch_api::CoreClass::Performance`], the safe homogeneous
//! default the Arch HAL mandates for
//! [`rustos_arch_api::SchedulerArch::core_class`]. A core
//! with no advertised rating on an otherwise heterogeneous part is also
//! treated as a performance core rather than guessed.

use rustos_arch_api::CoreClass;

use crate::kernel_arch::MAX_CPUS;

/// Classify each dense `CpuId` from its `capacity-dmips-mhz` rating.
///
/// `capacities[cpu]` is the DMIPS-per-MHz rating advertised for dense
/// `CpuId` `cpu`, or `None` when the device tree advertised none for that
/// core. The highest rating present defines the performance tier: a core
/// rated *strictly below* the peak is an [`CoreClass::Efficiency`] core;
/// a core at the peak — or one with no advertised rating — is a
/// [`CoreClass::Performance`] core. When no core advertises a rating the
/// result is the all-[`CoreClass::Performance`] homogeneous default.
#[must_use]
pub fn classify_by_capacity(capacities: &[Option<u64>; MAX_CPUS]) -> [CoreClass; MAX_CPUS] {
    let mut classes = [CoreClass::Performance; MAX_CPUS];
    let Some(peak) = capacities.iter().flatten().copied().max() else {
        // No core advertised a capacity: a homogeneous machine.
        return classes;
    };
    for (slot, capacity) in classes.iter_mut().zip(capacities.iter()) {
        if let Some(rating) = capacity {
            if *rating < peak {
                *slot = CoreClass::Efficiency;
            }
        }
    }
    classes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_capacities_is_homogeneous_performance() {
        let caps = [None; MAX_CPUS];
        let classes = classify_by_capacity(&caps);
        assert!(classes.iter().all(|c| c.is_performance()));
    }

    #[test]
    fn equal_capacities_are_all_performance() {
        let mut caps = [None; MAX_CPUS];
        caps[0] = Some(1024);
        caps[1] = Some(1024);
        let classes = classify_by_capacity(&caps);
        assert!(classes[0].is_performance());
        assert!(classes[1].is_performance());
    }

    #[test]
    fn lower_capacity_cores_are_efficiency() {
        // A 2+2 big.LITTLE layout: peak 1024 = big, 512 = LITTLE.
        let mut caps = [None; MAX_CPUS];
        caps[0] = Some(1024);
        caps[1] = Some(1024);
        caps[2] = Some(512);
        caps[3] = Some(512);
        let classes = classify_by_capacity(&caps);
        assert!(classes[0].is_performance());
        assert!(classes[1].is_performance());
        assert!(classes[2].is_efficiency());
        assert!(classes[3].is_efficiency());
    }

    #[test]
    fn missing_rating_on_a_hetero_part_defaults_to_performance() {
        // One big core advertises a rating; the other core advertises
        // none and is not guessed down to efficiency.
        let mut caps = [None; MAX_CPUS];
        caps[0] = Some(1024);
        caps[1] = None;
        caps[2] = Some(512);
        let classes = classify_by_capacity(&caps);
        assert!(classes[0].is_performance());
        assert!(classes[1].is_performance());
        assert!(classes[2].is_efficiency());
    }

    #[test]
    fn three_tier_capacities_only_the_peak_is_performance() {
        // DynamIQ parts can expose three tiers; only the top tier is a
        // performance core, every lower tier is efficiency.
        let mut caps = [None; MAX_CPUS];
        caps[0] = Some(1024);
        caps[1] = Some(768);
        caps[2] = Some(512);
        let classes = classify_by_capacity(&caps);
        assert!(classes[0].is_performance());
        assert!(classes[1].is_efficiency());
        assert!(classes[2].is_efficiency());
    }
}
