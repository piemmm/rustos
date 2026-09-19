//! QEMU integration test: the client's composited frames agree with the
//! reference digest on `aarch64`.
//!
//! ## What this proves that a host test cannot
//!
//! Three crates meet in a frame. The world generator works in `f64`; the
//! ground art is integer throughout and carries no vertical of its own
//! because bit-identity follows from the language there; this client's
//! projection, lattice sampling and shading are integer too. Each part
//! agreeing separately does not say the composition does.
//!
//! So two whole frames are drawn here — one wide at full quality, one
//! close with every degradation rung shed — and every pixel is folded,
//! with `tairix_wintersun_art::digest::REFERENCE_DIGEST` folded in after
//! them, into a single number that must equal
//! `tairix_wintersun_app::digest::REFERENCE_DIGEST`. That is the
//! constant the host suite and the sibling verticals assert, so
//! agreement between targets follows from each agreeing with it and no
//! target can pass by having never run.
//!
//! It is also what first builds the ground art for this target at all.
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
    use tairix_wintersun_app::digest::{self, REFERENCE_DIGEST};

    /// Stable audit-event ids for the QEMU transcript.
    const TEST_START: EventId = EventId(4530);
    const TEST_PASS: EventId = EventId(4531);
    const TEST_FAIL: EventId = EventId(4532);

    /// Failure finisher code: the frames could not be drawn at all.
    const FAIL_RUN: NonZeroU16 = fail_point!(1);
    /// Failure finisher code: the digest differed from the reference.
    const FAIL_DIGEST: NonZeroU16 = fail_point!(2);

    /// Static boot heap in the linker's dedicated `.heap` (NOLOAD)
    /// section. The realm field, the generated chunks, the synthesised
    /// material tiles and the two frame buffers all live here.
    /// `static mut` because the allocator hands out disjoint slices via
    /// an atomic cursor; the storage is otherwise never aliased.
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

    /// Report the digest with both values, so a failure names the
    /// divergence rather than merely announcing one.
    fn report(id: EventId, level: Level, produced: u64) {
        log(
            &SERIAL_SINK,
            &Event {
                level,
                id,
                message: "client frame: reference frame digest",
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
    fn client_frame_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Entry point for the freestanding kernel.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        // The world generator's relief, hydrology and climate are `f64`,
        // and EL1 traps FP/SIMD until this is cleared.
        // SAFETY: boot CPU, called once, before any FP instruction.
        unsafe { enable_fp_el1() };

        note(TEST_START, "client frame: drawing the reference frames");

        let Ok(produced) = digest::reference() else {
            note(TEST_FAIL, "client frame: the frames could not be drawn");
            qemu_exit::exit_failure(FAIL_RUN);
        };

        if produced == REFERENCE_DIGEST {
            report(TEST_PASS, Level::Info, produced);
            note(TEST_PASS, "client frame: aarch64 agrees with the reference");
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
