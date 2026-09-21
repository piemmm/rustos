//! integration test: the figure engine agrees with the reference digest on
//! `riscv64`.
//!
//! ## What this proves that a host test cannot
//!
//! A figure is `f64` throughout — clip sampling and easing, overlay
//! summing, the orthonormal resolve, the skinning blend, the projection and
//! its foreshortening, the two-bone planting solve, the shadow's
//! singular-value decomposition. All of it is IEEE-754 basic operations
//! over `lib/util::mathf`, whose transcendentals are first-party polynomial
//! kernels rather than a platform libm, so bit-identity *follows* from the
//! language.
//!
//! "Follows" is not evidence. This vertical is the evidence: the whole
//! pipeline runs under a different compiler backend on a different ISA, and
//! the digest it folds must equal
//! `tairix_wintersun_figure::digest::REFERENCE_DIGEST` — the same constant
//! the host suite and the sibling targets assert. Agreement between targets
//! follows from each agreeing with the constant, so no target can pass by
//! having never run.
//!
//! The one hazard the language does not foreclose is a backend fusing a
//! multiply and an add into a single rounded operation. Rust does not
//! enable that; this is what says so rather than assuming it.
//!
//! Any other outcome is a closed failure reported to QEMU.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_riscv64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;

    use tairix_arch_riscv64::{handle_panic_via_serial, qemu_exit, SERIAL_SINK};
    use tairix_itest_finisher::fail_point;
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
    use tairix_wintersun_figure::digest::{self, REFERENCE_DIGEST};

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4540);
    const TEST_PASS: EventId = EventId(4541);
    const TEST_FAIL: EventId = EventId(4542);

    /// Failure finisher code: the grid could not be placed at all.
    const FAIL_GENERATE: NonZeroU16 = fail_point!(1);
    /// Failure finisher code: the digest differed from the reference.
    const FAIL_DIGEST: NonZeroU16 = fail_point!(2);

    /// Static boot heap in the linker's dedicated `.heap` (NOLOAD)
    /// section. The figure path itself allocates nothing; the heap is here
    /// because the shared rasteriser the figure crate links names `alloc`. `static mut` because the allocator
    /// hands out disjoint slices via an atomic cursor; the storage is
    /// otherwise never aliased.
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
                message: "figure determinism: reference grid digest",
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
    fn figure_determinism_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
        note(TEST_START, "figure determinism: drawing the reference grid");

        let Ok(produced) = digest::reference() else {
            note(TEST_FAIL, "figure determinism: the grid did not place");
            qemu_exit::exit_failure(FAIL_GENERATE);
        };

        if produced == REFERENCE_DIGEST {
            report(TEST_PASS, Level::Info, produced);
            note(
                TEST_PASS,
                "figure determinism: riscv64 agrees with the reference",
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
#[cfg(not(itest_riscv64))]
fn main() {}
