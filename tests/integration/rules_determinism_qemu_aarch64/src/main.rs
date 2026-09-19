//! QEMU integration test: the authoritative simulation agrees with the
//! reference digest on `aarch64`.
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
#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::{enable_fp_el1, handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
    use tairix_wintersun_rules::digest::{self, REFERENCE_DIGEST};

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4520);
    const TEST_PASS: EventId = EventId(4521);
    const TEST_FAIL: EventId = EventId(4522);

    /// Failure finisher code: the session could not be played at all.
    const FAIL_RUN: NonZeroU16 = fail_point!(1);
    /// Failure finisher code: the digest differed from the reference.
    const FAIL_DIGEST: NonZeroU16 = fail_point!(2);

    /// Static boot heap in the linker's dedicated `.heap` (NOLOAD)
    /// section. The zone's entity table, its submission queue and its
    /// notification lists all live here. `static mut` because the
    /// allocator hands out disjoint slices via an atomic cursor; the
    /// storage is otherwise never aliased.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    fn note(id: EventId, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// Report a digest mismatch with both values, so a failure names the
    /// divergence rather than merely announcing one.
    fn report(id: EventId, level: Level, produced: u64) {
        log(
            &SERIAL_SINK,
            &Event {
                level,
                id,
                message: "rules determinism: scripted session digest",
                fields: &[
                    Field {
                        key: "produced",
                        value: FieldValue::UnsignedInt(produced),
                    },
                    Field {
                        key: "expected",
                        value: FieldValue::UnsignedInt(REFERENCE_DIGEST),
                    },
                ],
            },
        );
    }

    #[panic_handler]
    fn rules_determinism_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        // The simulation is integer throughout but for one heading
        // conversion, and EL1 traps FP/SIMD until this is cleared.
        // SAFETY: boot CPU, called once, before any FP instruction.
        unsafe { enable_fp_el1() };

        note(
            TEST_START,
            "rules determinism: playing the scripted session",
        );

        let Ok(produced) = digest::reference_run() else {
            note(TEST_FAIL, "rules determinism: the session did not run");
            qemu_exit::exit_failure(FAIL_RUN);
        };

        if produced == REFERENCE_DIGEST {
            report(TEST_PASS, Level::Info, produced);
            note(
                TEST_PASS,
                "rules determinism: aarch64 agrees with the reference",
            );
            qemu_exit::exit_success();
        }
        report(TEST_FAIL, Level::Error, produced);
        qemu_exit::exit_failure(FAIL_DIGEST);
    }
}

// Host stub. The crate is only meaningful on the bare-metal target; on the
// host a no-op `main` keeps `cargo build` and `cargo test` green without
// the crate having to be special-cased.
#[cfg(not(itest_aarch64))]
fn main() {}
