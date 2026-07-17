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

/// Failure to construct a dense CPU topology from firmware discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuTopologyError {
    /// Temporary storage for validation or the result could not be allocated.
    AllocationFailed,
    /// The discovered count cannot be represented by the kernel's CPU id.
    TooManyCpus,
}

fn reserve<T>(slots: &mut Vec<T>, count: usize) -> Result<(), CpuTopologyError> {
    slots
        .try_reserve_exact(count)
        .map_err(|_| CpuTopologyError::AllocationFailed)
}

fn boot_only(boot_affinity: u64) -> Result<Vec<u64>, CpuTopologyError> {
    let mut ordered = Vec::new();
    reserve(&mut ordered, 1)?;
    ordered.push(boot_affinity);
    Ok(ordered)
}

/// Validate the discovered CPU affinity list and order it with the
/// running boot CPU first, returning the dense-id → affinity map.
///
/// * The boot CPU (identified by `boot_affinity`, the value the running
///   core reads from its own identity register) is moved to dense id
///   `0`; the remaining CPUs keep their discovery order.
/// * A list that cannot be trusted fails closed to just the boot CPU:
///   an empty discovery, a duplicate affinity (two CPUs cannot share
///   one), or a list that does not contain the running core at all (a
///   tree describing some *other* machine must not size this one's
///   bring-up).
pub fn order_cpus(discovered: &[u64], boot_affinity: u64) -> Result<Vec<u64>, CpuTopologyError> {
    if u32::try_from(discovered.len()).is_err() {
        return Err(CpuTopologyError::TooManyCpus);
    }
    if discovered.is_empty() {
        return boot_only(boot_affinity);
    }
    let Some(boot_index) = discovered.iter().position(|&a| a == boot_affinity) else {
        return boot_only(boot_affinity);
    };
    let mut sorted = Vec::new();
    reserve(&mut sorted, discovered.len())?;
    sorted.extend_from_slice(discovered);
    sorted.sort_unstable();
    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return boot_only(boot_affinity);
    }
    let mut ordered = Vec::new();
    reserve(&mut ordered, discovered.len())?;
    ordered.push(boot_affinity);
    ordered.extend(
        discovered
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != boot_index)
            .map(|(_, &a)| a),
    );
    Ok(ordered)
}

/// Align the discovered per-CPU spin-table release addresses with the
/// dense-id order [`order_cpus`] produced, returning one release-word
/// address per dense id (`0` = the tree declared none for that CPU —
/// including the boot CPU, which is never released).
///
/// `declared` carries `(affinity, release_addr)` pairs in discovery
/// order; each ordered affinity takes its declared address, and an
/// affinity with no declared pair (or a duplicate — first declaration
/// wins, matching the duplicate-free dense map) takes `0`, so a start
/// request for it fails closed rather than writing an invented word.
pub fn align_release_addrs(
    ordered: &[u64],
    declared: &[(u64, u64)],
) -> Result<Vec<u64>, CpuTopologyError> {
    let mut aligned = Vec::new();
    reserve(&mut aligned, ordered.len())?;
    aligned.extend(ordered.iter().map(|&affinity| {
        declared
            .iter()
            .find(|&&(a, _)| a == affinity)
            .map_or(0, |&(_, addr)| addr)
    }));
    Ok(aligned)
}

#[cfg(test)]
mod tests {
    use super::{align_release_addrs, order_cpus};

    #[test]
    fn boot_core_is_moved_to_dense_id_zero_preserving_the_rest() {
        assert_eq!(order_cpus(&[0, 1, 2, 3], 0).unwrap(), &[0, 1, 2, 3]);
        assert_eq!(order_cpus(&[2, 1, 0, 3], 0).unwrap(), &[0, 2, 1, 3]);
    }

    #[test]
    fn a_list_missing_the_running_core_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[1, 2, 3], 0).unwrap(), &[0]);
    }

    #[test]
    fn an_empty_discovery_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[], 7).unwrap(), &[7]);
    }

    #[test]
    fn a_duplicate_affinity_fails_closed_to_single_cpu() {
        assert_eq!(order_cpus(&[0, 1, 1, 3], 0).unwrap(), &[0]);
    }

    #[test]
    fn pi_4_shaped_affinities_survive_intact() {
        // The Pi 4's DT lists cpu@0..cpu@3 with Aff0 = 0..3 and the boot
        // core is affinity 0 — the common case is a straight pass-through.
        assert_eq!(order_cpus(&[0, 1, 2, 3], 0).unwrap(), &[0, 1, 2, 3]);
    }

    #[test]
    fn release_addrs_follow_the_dense_order() {
        // The Pi 4 shape: no release word for the boot CPU, one per
        // secondary, aligned to the dense ids whatever the tree order.
        let ordered = order_cpus(&[2, 1, 0, 3], 0).unwrap();
        assert_eq!(ordered, &[0, 2, 1, 3]);
        let declared = [(1, 0xe0), (2, 0xe8), (3, 0xf0)];
        assert_eq!(
            align_release_addrs(&ordered, &declared).unwrap(),
            &[0, 0xe8, 0xe0, 0xf0]
        );
    }

    #[test]
    fn an_undeclared_affinity_gets_no_release_word() {
        // CPU 2 declared no `cpu-release-addr`: its slot is 0 so a start
        // request for it fails closed.
        assert_eq!(
            align_release_addrs(&[0, 1, 2], &[(1, 0xe0)]).unwrap(),
            &[0, 0xe0, 0]
        );
        // No declarations at all: every slot is 0.
        assert_eq!(align_release_addrs(&[0, 1], &[]).unwrap(), &[0, 0]);
    }

    #[test]
    fn a_duplicate_declaration_takes_the_first() {
        assert_eq!(
            align_release_addrs(&[0, 1], &[(1, 0xe0), (1, 0xf0)]).unwrap(),
            &[0, 0xe0]
        );
    }
}
