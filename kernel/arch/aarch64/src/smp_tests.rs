//! Host unit tests for the SMP bring-up surface.
//!
//! The secondary-stack pool registration, the cpu-id validity check, the
//! set-once secondary-entry slot, and the `StartCpuError` decode build and
//! run on the host. The `MPIDR_EL1` read, the PSCI `CPU_ON` call, and the
//! secondary trampoline are exercised by the multi-core QEMU vertical.

use super::*;

extern "C" fn host_entry(_cpu: CpuId) -> ! {
    // Never invoked on the host: the tests only round-trip its address
    // through the set-once slot. Parking keeps the `-> !` signature
    // honest if it ever were called.
    loop {
        core::hint::spin_loop();
    }
}

#[test]
fn cpu_validity_tracks_the_registered_pool() {
    // A caller-sized pool covers exactly its `N` slots (the capacity
    // is the discovered core count, not a baked-in `MAX_CPUS`); a second
    // pool proves registration is set-once. Declared first so they precede
    // the statements that drive them.
    static POOL: SecondaryStackPool<3> = SecondaryStackPool::new();
    static POOL2: SecondaryStackPool<2> = SecondaryStackPool::new();

    reset_secondary_stacks_for_tests();
    // Fail closed before any pool is registered: every id is invalid, so
    // a `start_secondary` cannot select an unbacked stack slice.
    assert!(!is_valid_cpu(0));

    assert_eq!(POOL.register(), Ok(3));
    assert!(is_valid_cpu(0));
    assert!(is_valid_cpu(2));
    assert!(!is_valid_cpu(3));
    assert!(!is_valid_cpu(u32::MAX));

    // A second pool is refused rather than silently re-pointing the live
    // trampoline.
    assert_eq!(
        POOL2.register(),
        Err(SecondaryStackError::AlreadyRegistered)
    );

    reset_secondary_stacks_for_tests();
}

#[test]
fn secondary_entry_round_trips_and_is_set_once() {
    clear_secondary_entry_for_tests();
    assert_eq!(secondary_entry_addr(), 0);
    set_secondary_entry(host_entry).expect("first install");
    assert_eq!(secondary_entry_addr(), host_entry as *const () as usize);
    assert_eq!(
        set_secondary_entry(host_entry),
        Err(SetEntryError::AlreadyInstalled)
    );
    clear_secondary_entry_for_tests();
}

#[test]
fn current_cpu_index_is_boot_core_on_host() {
    // The host build has no `MPIDR_EL1`; it reports the single boot core.
    assert_eq!(current_cpu_index(), 0);
}

#[test]
fn affinity_mask_excludes_the_reserved_mpidr_bits() {
    // The reserved bit 31, the U bit 30, and the MT bit 24 are all above
    // the Aff2 byte and must not leak into the masked affinity.
    assert_eq!((1u64 << 31) & MPIDR_AFFINITY_MASK, 0);
    assert_eq!((1u64 << 30) & MPIDR_AFFINITY_MASK, 0);
    assert_eq!((1u64 << 24) & MPIDR_AFFINITY_MASK, 0);
    // Aff0 (core index on the `virt` board) survives the mask.
    assert_eq!(0x1u64 & MPIDR_AFFINITY_MASK, 1);
}

#[test]
fn start_cpu_error_cause_strings_are_stable() {
    assert_eq!(
        StartCpuError::CpuIdOutOfRange.as_str(),
        "cpu_id_out_of_range"
    );
    assert_eq!(
        StartCpuError::NoEntryInstalled.as_str(),
        "no_secondary_entry_installed"
    );
    assert_eq!(StartCpuError::Psci(-4).as_str(), "psci_cpu_on_failed");
}
