//! QEMU integration test: the authoritative simulation agrees with the
//! reference digest on `x86_64`.
//!
//! ## What this proves that a host test cannot
//!
//! A realm and every client connected to it step the same simulation. If
//! two of them disagree by one sub-unit of position or one point of health,
//! the fight they are watching is not the same fight, and no amount of
//! netcode recovers from that.
//!
//! The arithmetic is written to be target-independent — integer throughout
//! but for one heading conversion through `lib/util::mathf` rather than a
//! platform libm, fixed widths rather than `usize` in anything folded, and
//! a total order on every traversal — but "written to be" is not evidence.
//! This vertical is the evidence: a scripted session of twelve bodies
//! walking obstructed ground, colliding, striking, healing, carrying
//! statuses that stack and diminish, and one of them dying, runs under a
//! different compiler backend on a different ISA, and the digest of its
//! whole trajectory must equal
//! `tairix_wintersun_rules::digest::REFERENCE_DIGEST` — the same constant
//! the host suite and the sibling verticals assert. Agreement between
//! targets follows from each agreeing with the constant, so no target can
//! pass by having never run.
//!
//! The world generator's own cross-target claim is a separate vertical with
//! a separate constant, and deliberately so: this session's ground is a
//! pattern rather than a generated realm, so a change to either cannot make
//! the other's evidence ambiguous.
//!
//! Any other outcome is a closed failure reported to QEMU.
#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;

    use tairix_arch_x86_64::{qemu_exit, serial};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_wintersun_rules::digest::{self, REFERENCE_DIGEST};

    /// Static boot heap. The zone's entity table, its submission queue
    /// and its notification lists all live here. `static mut` because the
    /// allocator hands out disjoint slices via an atomic cursor; the
    /// storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// Entry point for the freestanding kernel. Called by
    /// `tairix_arch_x86_64`'s boot trampoline after the multiboot magic
    /// has been validated.
    #[no_mangle]
    pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[rules_determinism] booted on x86_64");

        let Ok(produced) = digest::reference_run() else {
            let _ = writeln!(com1, "[rules_determinism] FAIL: the session did not run");
            qemu_exit::exit_failure();
        };

        // Both values on the failure path, so a mismatch names the
        // divergence rather than merely announcing one.
        if produced == REFERENCE_DIGEST {
            let _ = writeln!(
                com1,
                "[rules_determinism] PASS: digest {produced:#018x} matches the reference"
            );
            qemu_exit::exit_success();
        }
        let _ = writeln!(
            com1,
            "[rules_determinism] FAIL: digest {produced:#018x}, expected {REFERENCE_DIGEST:#018x}"
        );
        qemu_exit::exit_failure();
    }
}

/// The panic handler: nothing here panics, so reaching it is itself the
/// failure the vertical reports.
#[panic_handler]
#[cfg(itest_x86_64)]
fn rules_determinism_qemu_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write as _;
    use tairix_arch_x86_64::{qemu_exit, serial};

    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[rules_determinism] panic: {info}");
    qemu_exit::exit_failure();
}

// Host stub. The crate is only meaningful on the bare-metal target; on the
// host a no-op `main` keeps `cargo build` and `cargo test` green without
// the crate having to be special-cased.
#[cfg(not(itest_x86_64))]
fn main() {}
