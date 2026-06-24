//! Host unit tests for the SMP bring-up surface.
//!
//! The caller-sized stack pool, the hart-id validity check, the set-once
//! secondary-entry slot, and the `StartHartError` decode build and run on
//! the host. The `tp` read, the SBI HSM call, and the secondary
//! trampoline are exercised by the multi-hart QEMU vertical.

use super::*;

extern "C" fn host_entry(_hartid: CpuId) -> ! {
    // Never invoked on the host: the tests only round-trip its address
    // through the set-once slot. Parking keeps the `-> !` signature
    // honest if it ever were called.
    loop {
        core::hint::spin_loop();
    }
}

#[test]
fn hartid_validity_tracks_the_registered_pool() {
    // A caller-sized pool covers exactly its `N` slots (the capacity
    // is the discovered hart count, not a baked-in `MAX_HARTS`); a second
    // pool proves registration is set-once. Declared first so they precede
    // the statements that drive them.
    static POOL: SecondaryStackPool<3> = SecondaryStackPool::new();
    static POOL2: SecondaryStackPool<2> = SecondaryStackPool::new();

    reset_secondary_stacks_for_tests();

    // Before any pool is registered every id is invalid (fail closed).
    assert!(!is_valid_hartid(0));
    assert!(!is_valid_hartid(u32::MAX));

    assert_eq!(POOL.register(), Ok(3));
    assert!(is_valid_hartid(0));
    assert!(is_valid_hartid(2));
    assert!(!is_valid_hartid(3));
    assert!(!is_valid_hartid(u32::MAX));

    // Registration is set-once: a second pool is refused rather than
    // silently re-pointing the live trampoline.
    assert_eq!(
        POOL2.register(),
        Err(SecondaryStackError::AlreadyRegistered)
    );

    reset_secondary_stacks_for_tests();
    // Cleared again: every id fails closed.
    assert!(!is_valid_hartid(0));
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
fn current_hartid_is_boot_hart_on_host() {
    // The host build has no `tp`; it reports the single boot hart.
    assert_eq!(current_hartid(), 0);
}

#[test]
fn start_hart_error_cause_strings_are_stable() {
    assert_eq!(
        StartHartError::HartIdOutOfRange.as_str(),
        "hartid_out_of_range"
    );
    assert_eq!(
        StartHartError::NoEntryInstalled.as_str(),
        "no_secondary_entry_installed"
    );
    assert_eq!(StartHartError::Sbi(-3).as_str(), "sbi_hart_start_failed");
}
