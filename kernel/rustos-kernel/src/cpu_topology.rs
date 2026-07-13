//! Pure, host-tested validation and ordering of the CPU list a boot
//! path discovers from its platform source (device-tree `/cpus`, ACPI
//! MADT), producing the dense-`CpuId` → hardware-affinity map the arch
//! handle is built over.
//!
//! Dense ids are positional: entry `0` is the boot CPU, entries
//! `1..n` are the secondaries in discovery order. The kernel core, the
//! scheduler, and the per-CPU storages all index by these dense ids, so
//! the list must be **valid before anything is sized from it** — this
//! module is where a malformed inventory fails closed to a single-CPU
//! boot instead of poisoning every per-CPU table downstream.

use alloc::vec::Vec;

/// Validate the discovered CPU affinity list and order it with the
/// running boot CPU first, returning the dense-id → affinity map.
///
/// * The boot CPU (identified by `boot_affinity`, the value the running
///   core reads from its own identity register) is moved to dense id
///   `0`; the remaining CPUs keep their discovery order.
/// * The result is capped at `max_cpus` entries (the kernel core's
///   per-CPU table bound); surplus CPUs are dropped from the tail. The
///   caller logs the clamp — silently ignoring cores is not an option
///   it may take quietly.
/// * A list that cannot be trusted fails closed to just the boot CPU:
///   an empty discovery, a duplicate affinity (two CPUs cannot share
///   one), or a list that does not contain the running core at all (a
///   tree describing some *other* machine must not size this one's
///   bring-up).
#[must_use]
pub fn order_cpus(discovered: &[u64], boot_affinity: u64, max_cpus: usize) -> Vec<u64> {
    let boot_only = || alloc::vec![boot_affinity];
    if discovered.is_empty() || max_cpus == 0 {
        return boot_only();
    }
    let Some(boot_index) = discovered.iter().position(|&a| a == boot_affinity) else {
        return boot_only();
    };
    for (i, &a) in discovered.iter().enumerate() {
        if discovered[i + 1..].contains(&a) {
            return boot_only();
        }
    }
    let mut ordered = Vec::with_capacity(discovered.len().min(max_cpus));
    ordered.push(boot_affinity);
    ordered.extend(
        discovered
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != boot_index)
            .map(|(_, &a)| a),
    );
    ordered.truncate(max_cpus.max(1));
    ordered
}

#[cfg(test)]
mod tests {
    use super::order_cpus;

    #[test]
    fn boot_core_is_moved_to_dense_id_zero_preserving_the_rest() {
        assert_eq!(order_cpus(&[0, 1, 2, 3], 0, 64), &[0, 1, 2, 3]);
        assert_eq!(order_cpus(&[2, 1, 0, 3], 0, 64), &[0, 2, 1, 3]);
    }

    #[test]
    fn a_list_missing_the_running_core_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[1, 2, 3], 0, 64), &[0]);
    }

    #[test]
    fn an_empty_discovery_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[], 7, 64), &[7]);
    }

    #[test]
    fn a_duplicate_affinity_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[0, 1, 1, 3], 0, 64), &[0]);
    }

    #[test]
    fn the_list_is_capped_at_the_per_cpu_table_bound() {
        assert_eq!(order_cpus(&[0, 1, 2, 3], 0, 2), &[0, 1]);
        // A pathological zero bound still yields the boot CPU.
        assert_eq!(order_cpus(&[0, 1], 0, 0), &[0]);
    }

    #[test]
    fn pi_4_shaped_affinities_survive_intact() {
        // The Pi 4's DT lists cpu@0..cpu@3 with Aff0 = 0..3 and the boot
        // core is affinity 0 — the common case is a straight pass-through.
        assert_eq!(order_cpus(&[0, 1, 2, 3], 0, 64), &[0, 1, 2, 3]);
    }
}
