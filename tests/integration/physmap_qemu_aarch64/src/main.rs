//! aarch64 direct-physical-map QEMU integration test: the kernel reaches
//! **every** usable byte of a guest whose RAM spans gigapages above the one
//! holding its own image, and it reaches them through a translation regime
//! no user address can name.
//!
//! ## Why this exists
//!
//! The kernel reaches a frame by pointer through the port's direct physical
//! map — the process-image write, the shared-region zero-on-free scrub, the
//! kernel heap's slab page supply, the page-table walk's own table
//! recovery. On this port that map used to be the *identity* window every
//! process root carried, widened over the whole of discovered RAM
//! (`plans/OPEN-DEFECTS.md` D56). Three costs rode on that: the window had
//! to stop where the child image begins, so a machine with more RAM than
//! the image bias had frames the kernel could not reach; every process root
//! carried a full-RAM kernel-only mapping in the half its own code
//! addresses; and KPTI was blocked outright. Nothing caught it, because
//! every other aarch64 guest in the matrix has less RAM than one gigapage
//! past its kernel.
//!
//! ## What this test asserts
//!
//! The guest is given more RAM than the gigapage its kernel image sits in,
//! so a root that identity-mapped RAM would be visible as one. Then:
//!
//! 1. The boot path sized the map from the discovered tree — proof it read
//!    `/memory` rather than a build-time constant.
//! 2. The early-boot RAM self-test left **no** usable byte unreachable:
//!    each one was written and read back through the direct map. A frame
//!    the map does not cover is left untested and counted, so a map that
//!    stopped short shows up here rather than as a silent skip.
//! 3. The live structure holds: the hardware reads a known frame back
//!    through the map, the map lies above every addressable user address,
//!    a process root refuses a mapping there, and a RAM frame outside the
//!    kernel's own extents resolves to nothing under a process root while
//!    the map still reaches it.
//!
//! Only when all three hold does `BootCompleted` report success to QEMU.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! It reuses the whole boot pipeline from `tairix_kernel::aarch64::boot`;
//! only the sinks are replaced. Splitting the observer into its own bin
//! (rather than gating it behind a Cargo feature on `tairix-kernel`) keeps
//! feature unification under `cargo build --workspace` from ever leaking
//! the QEMU-exit behaviour into a real kernel image.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_aarch64::paging::{self, AddressSpace, PageTablePool};
    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink};
    use tairix_arch_api::mmu::{AddressSpace as _, PageFlags};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_kernel::KERNEL_BOOT_DIRECT_MAP;
    use tairix_log::{Event, EventId, FieldValue, Sink};

    // `DTB_BLOB` + `GUEST_RAM_MIB`: the `virt` tree dumped for this
    // vertical's guest RAM, and the figure it was dumped for.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, inside the kernel image so the boot pipeline
    /// excludes it from the usable map exactly as the production binary's
    /// heap is.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The QEMU `virt` board's RAM base. The kernel image is linked 2 MiB
    /// above it, so gigapage 1 is the kernel's own and every gigapage above
    /// it is RAM the identity window has no reason to carry.
    const RAM_BASE: u64 = 0x4000_0000;

    /// One page, the granule the probe's frames are aligned to.
    const PAGE_BYTES: u64 = 4096;

    /// `kernel_core::AuditEvent::RamSelfTest`, carrying the bytes the
    /// self-test verified and the usable bytes the direct map could not
    /// reach. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const RAM_SELF_TEST_EVENT_ID: EventId = EventId(4005);

    /// `kernel_core::AuditEvent::BootCompleted`: every init phase succeeded.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Set when the boot path reported a direct map sized from the tree's
    /// `/memory` window rather than from one gigapage.
    static MAP_SIZED_FROM_RAM: AtomicBool = AtomicBool::new(false);

    /// Set when the RAM self-test reported verifying every usable byte.
    static RAM_FULLY_REACHED: AtomicBool = AtomicBool::new(false);

    /// Page-table pages the structural probe's root draws from. A `.bss`
    /// pool, so the probe needs nothing of the live allocator and cannot
    /// perturb the boot it is observing.
    static PROBE_POOL: PageTablePool = PageTablePool::new();

    /// Frame the probe reaches two ways: at its own identity address (it is
    /// this binary's static, and the kernel is identity-linked) and through
    /// the direct map. Its marker is distinctive, so reading it back at
    /// `PHYSMAP_VMA_BASE + phys` identifies *that* frame rather than merely
    /// proving some page is mapped there.
    static PROBE_FRAME: ProbeFrame = ProbeFrame {
        marker: PROBE_MARKER,
        rest: [0; 4096 - PROBE_MARKER.len()],
    };

    /// A page-aligned frame the probe reads through the direct map. Never
    /// written: a read proves the translation, and the RAM self-test above
    /// already writes and reads back every usable byte.
    #[repr(C, align(4096))]
    struct ProbeFrame {
        marker: [u8; 8],
        rest: [u8; 4096 - 8],
    }

    /// The marker [`PROBE_FRAME`] opens with.
    const PROBE_MARKER: [u8; 8] = [0x5D, 0x56, 0xA6, 0x41, 0x00, 0xFF, 0x7E, 0x81];

    /// Marker the probe writes to the highest frame the map covers, through
    /// the map, and reads back through the map at a second alias.
    const HIGH_MARKER: u64 = 0xD56_0000_5D56_A641;

    /// Prove the structure the defect was about: the direct map reaches a
    /// frame through a regime no user address can name, and a process root
    /// carries no mapping of RAM outside the kernel's own extents.
    ///
    /// The map's own gigapage leaves are deliberately not walked through a
    /// frame source: they live in the kernel root's own slots rather than
    /// in a drawn table, so there is no table to recover. The hardware
    /// translation is witnessed by reading through it instead, which is the
    /// stronger statement anyway.
    ///
    /// Run before the boot path builds PID 1, so the root it draws is never
    /// made live.
    fn structural_probe() -> Result<(), &'static str> {
        // The kernel is identity-linked, so its own static's virtual
        // address *is* its physical one.
        let probe_phys = core::ptr::addr_of!(PROBE_FRAME) as u64;
        let map_va = paging::physmap_virt(probe_phys);

        // The hardware reaches the frame through the map, and reaches
        // *that* frame: the marker read back is the one the identity alias
        // holds.
        if !paging::physmap_covers(probe_phys, PAGE_BYTES) {
            return Err("the map does not cover the kernel's own frame");
        }
        for (offset, expected) in PROBE_MARKER.iter().enumerate() {
            // SAFETY: the map is live (the boot path sized it before this
            // record is written) and covers `probe_phys`, which names this
            // binary's own page-aligned static; the read is in-bounds of
            // that frame and mutates nothing.
            let seen = unsafe { core::ptr::read_volatile((map_va + offset as u64) as *const u8) };
            if seen != *expected {
                return Err("the map does not read back the frame's marker");
            }
        }

        // The map is in the kernel translation regime, which begins above
        // every address `TTBR0_EL1` can translate. That the *user* region
        // cannot name it is a build-time pin in `aarch64.rs`; what is
        // checked here is the live arithmetic.
        if map_va < paging::KERNEL_VA_BASE {
            return Err("the map's address is not in the kernel regime");
        }
        if tairix_kernel::aarch64::USER_VA_TOP > paging::KERNEL_VA_BASE {
            return Err("a user address could name the kernel regime");
        }

        // A process root built exactly as the production producer builds
        // one: the identity window the discovered masks describe, and
        // nothing else.
        let identity_gib = paging::configured_identity_gigapages();
        let Some(mut process) = AddressSpace::new_identity_gigapages(&PROBE_POOL, identity_gib)
        else {
            return Err("no process root");
        };

        // The map's address resolves to nothing through that root: it is not
        // in the root's regime at all, so the walk refuses it outright
        // rather than indexing an identically-numbered user slot.
        if process.translate(map_va).is_some() {
            return Err("a process root resolves the map's address");
        }
        if process
            .map_page(map_va, probe_phys, PageFlags::READ | PageFlags::USER)
            .is_ok()
        {
            return Err("a user leaf was accepted at the map's address");
        }

        // The heart of it: a RAM frame in a gigapage the kernel does not
        // address physically is reachable through the map and reachable
        // *only* through it. The frame is the last page of the guest's RAM;
        // nothing owns it at this point in the boot (no allocator exists
        // yet) and the RAM self-test overwrites every usable byte
        // afterwards regardless.
        let ram_top = RAM_BASE + (GUEST_RAM_MIB << 20);
        let high_phys = (ram_top - PAGE_BYTES) & !(PAGE_BYTES - 1);
        // A precondition on the *guest*, not on the mapping: without RAM
        // above the gigapage holding the kernel image (which is where this
        // binary's own `PROBE_FRAME` lives) the property below is vacuous.
        if high_phys >> 30 <= probe_phys >> 30 {
            return Err("the guest has no RAM above the kernel's own gigapage");
        }
        if !paging::physmap_covers(high_phys, PAGE_BYTES) {
            return Err("the map does not cover the top of RAM");
        }
        let high_map_va = paging::physmap_virt(high_phys);
        // SAFETY: the map covers `high_phys`, so its translation is valid,
        // and the frame is unowned at this point in the boot. The write and
        // the read-back are in-bounds of that one page. The read-back is a
        // second volatile access through the same alias, so the value is
        // witnessed to have reached memory rather than been forwarded from
        // the store — the identity window deliberately does not cover this
        // frame, which is the property under test.
        let witnessed = unsafe {
            core::ptr::write_volatile(high_map_va as *mut u64, HIGH_MARKER);
            core::ptr::read_volatile(high_map_va as *const u64)
        };
        if witnessed != HIGH_MARKER {
            return Err("the map does not read back a high frame");
        }
        if process.translate(high_phys).is_some() {
            return Err("a process root still identity-maps RAM");
        }

        // And a user address below the child image bias still maps
        // normally, through the root's own regime.
        let alias_va = 8u64 << 30;
        let other_phys = probe_phys + PAGE_BYTES;
        if process
            .map_page(alias_va, other_phys, PageFlags::READ | PageFlags::USER)
            .is_err()
        {
            return Err("a user address would not map");
        }
        if process.translate(alias_va).map(|(phys, _)| phys) != Some(other_phys) {
            return Err("the user address resolves to the wrong frame");
        }
        Ok(())
    }

    /// Read an unsigned field of `event` by key.
    fn field_u64(event: &Event<'_>, key: &str) -> Option<u64> {
        event.fields.iter().find_map(|field| match field.value {
            FieldValue::UnsignedInt(value) if field.key == key => Some(value),
            _ => None,
        })
    }

    /// QEMU exit code for a graded check that did not hold. Any non-zero
    /// value fails the run; naming it once keeps every exit the same.
    const FAIL_CODE: NonZeroU16 = match NonZeroU16::new(1) {
        Some(code) => code,
        None => panic!("1 is non-zero"),
    };

    /// Report which check failed on the serial, so the transcript ends at
    /// the broken check rather than at a timeout.
    fn fail(why: &'static str) -> ! {
        SerialSink::new().write_event(&Event {
            level: tairix_log::Level::Error,
            id: EventId(0),
            message: why,
            fields: &[],
        });
        qemu_exit::exit_failure(FAIL_CODE)
    }

    /// Forwards every record to the serial transcript and grades the two
    /// records this vertical turns on, exiting the moment one of them fails.
    struct PhysMapObserver;

    impl Sink for PhysMapObserver {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == KERNEL_BOOT_DIRECT_MAP {
                // The guest's RAM spans three gigapages, so a map covering
                // one means the boot path never read the discovered tree —
                // which is the defect. The map is live by the time this
                // record is written, so the structural probe runs here.
                match field_u64(event, "gigabytes") {
                    Some(gib) if gib > 1 => {
                        if let Err(why) = structural_probe() {
                            fail(why);
                        }
                        MAP_SIZED_FROM_RAM.store(true, Ordering::Release);
                    }
                    _ => fail("the direct map covers no more than one gigapage"),
                }
            }

            if event.id == RAM_SELF_TEST_EVENT_ID {
                match (
                    field_u64(event, "verified_bytes"),
                    field_u64(event, "unreachable_bytes"),
                ) {
                    // Every usable byte was written and read back through
                    // the direct map. A map that stopped short leaves the
                    // RAM above it unreachable, which is what shows up here.
                    (Some(verified), Some(0)) if verified != 0 => {
                        RAM_FULLY_REACHED.store(true, Ordering::Release);
                    }
                    _ => fail("the self-test left usable RAM outside the direct map"),
                }
            }

            if event.id == BOOT_COMPLETED_EVENT_ID {
                if MAP_SIZED_FROM_RAM.load(Ordering::Acquire)
                    && RAM_FULLY_REACHED.load(Ordering::Acquire)
                {
                    qemu_exit::exit_success();
                }
                // Booting without either record having been graded means
                // the step this vertical exists to watch never ran.
                fail("boot completed without the direct-map records");
            }
        }
    }

    static OBSERVER: PhysMapObserver = PhysMapObserver;

    /// Forward to the shared aarch64 panic bridge. A panic before the
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_physmap_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline. The
    /// observer is *both* sinks: the direct-map record and the RAM
    /// self-test are log records, not audit ones, so grading them needs the
    /// log sink too.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        boot_aarch64::boot(
            DTB_BLOB.as_ptr() as u64,
            &ALLOCATOR,
            &OBSERVER,
            &OBSERVER,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
