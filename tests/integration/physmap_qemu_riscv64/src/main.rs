//! riscv64 direct-physical-map QEMU integration test: the kernel reaches
//! **every** usable byte of a guest that has more RAM than the window the
//! spawn path used to reach frames through, and it reaches it above the
//! port's user region rather than inside it.
//!
//! ## Why this exists
//!
//! The kernel reaches a frame by pointer through the port's direct physical
//! map — the process-image write, the shared-region zero-on-free scrub, the
//! kernel heap's slab page supply, the page-table walk's own table
//! recovery. On this port that map was an *identity* map of the low four
//! gigabytes (`plans/OPEN-DEFECTS.md` D56), so a board with more RAM than
//! that failed spawn closed for every frame above it, and the boot space's
//! own identity extent claimed gigabytes that Sv39 sign-extension made an
//! upper-half address — an identity mapping in neither direction. Nothing
//! caught either, because every other riscv64 guest in the matrix is small
//! enough to fit the window.
//!
//! ## What this test asserts
//!
//! The guest is given enough RAM that its top sits above four gigabytes, so
//! the firmware tree reports usable RAM the old window could not reach.
//! Then:
//!
//! 1. The boot path sized the map past that window — proof it read the
//!    discovered tree rather than a build-time constant.
//! 2. The early-boot RAM self-test left **no** usable byte unreachable:
//!    each one was written and read back through the direct map. A frame
//!    the map does not cover is left untested and counted, so a map that
//!    stopped short shows up here rather than as a silent skip.
//! 3. The live structure holds: the hardware reads a known frame back
//!    through the map, the map's root slot is disjoint from every user
//!    address, a user leaf is refused in it, and a process root resolves
//!    a high physical address to nothing while the map still reaches it.
//!
//! Only when all three hold does `BootCompleted` report success to QEMU.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! It reuses the whole boot pipeline from `tairix_kernel::riscv64::boot`;
//! only the sink is replaced. Splitting the observer into its own bin
//! (rather than gating it behind a Cargo feature on `tairix-kernel`) keeps
//! feature unification under `cargo build --workspace` from ever leaking
//! the QEMU-exit behaviour into a real kernel image.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`riscv64gc-unknown-none-elf`) ----------

#[cfg(itest_riscv64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_api::frames::PageTableFrames as _;
    use tairix_arch_api::mmu::{AddressSpace as _, PageFlags};
    use tairix_arch_riscv64::paging::{self, AddressSpace, PageTablePool};
    use tairix_arch_riscv64::{handle_panic_via_serial, qemu_exit, SerialSink};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::riscv64::boot as boot_riscv64;
    use tairix_kernel::KERNEL_BOOT_DIRECT_MAP;
    use tairix_log::{Event, EventId, FieldValue, Sink};

    /// Static boot heap, in the linker's dedicated `.heap` (NOLOAD)
    /// section so the boot pipeline excludes it from the usable map,
    /// exactly as the production riscv64 binary's heap is.
    #[link_section = ".heap"]
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// The window the spawn path used to reach frames through, in bytes.
    /// The guest's RAM tops it, so a map no wider is the defect.
    const OLD_SPAWN_WINDOW_GIB: u64 = 4;

    /// `kernel_core::AuditEvent::RamSelfTest`, carrying the bytes the
    /// self-test verified and the usable bytes the direct map could not
    /// reach. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const RAM_SELF_TEST_EVENT_ID: EventId = EventId(4005);

    /// `kernel_core::AuditEvent::BootCompleted`: every init phase succeeded.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Set when the boot path reported a direct map wider than the window
    /// the spawn path used to carry — the guest's RAM forced it.
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
    const PROBE_MARKER: [u8; 8] = [0x5D, 0x56, 0xD1, 0xA6, 0x00, 0xFF, 0x7E, 0x81];

    /// Marker the probe writes to the highest frame the map covers, through
    /// the map, and reads back at that frame's physical address.
    const HIGH_MARKER: u64 = 0xD56_0000_5D56_D1A6;

    /// One page, the granule the probe's high frame is aligned to.
    const PAGE_BYTES: u64 = 4096;

    /// The root-table slot a virtual address resolves through: bits 38:30,
    /// with the sign extension above them masked off.
    fn root_slot(va: u64) -> usize {
        ((va >> 30) & 0x1FF) as usize
    }

    /// Prove the structure the defect was about: the direct map reaches a
    /// frame above the port's user region, the map's slot is disjoint from
    /// every user address and refuses a user leaf, and a process root
    /// resolves a high physical address to nothing while the map reaches
    /// it.
    ///
    /// The map's own gigapage leaves are deliberately not walked through a
    /// frame source: they live in a root slot rather than in a drawn table,
    /// so there is no table to recover. The hardware translation is
    /// witnessed by reading through it instead, which is the stronger
    /// statement anyway.
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

        // The map's address is in its own slot. That the *user region*
        // cannot name that slot is a build-time pin in `riscv64.rs`; what
        // is checked here is the live arithmetic under a real root.
        if root_slot(map_va) < paging::PHYSMAP_FIRST_SLOT {
            return Err("the map's address is not in its own slot");
        }

        let Some(mut process) = AddressSpace::new_identity_gigapages(
            &PROBE_POOL,
            tairix_kernel::riscv64::spawn_producer::identity_gigapages(),
        ) else {
            return Err("no process root");
        };

        // The root carries the map.
        let Some(root) = PROBE_POOL.table_at(process.root_phys()) else {
            return Err("the pool cannot reach the root it drew");
        };
        // SAFETY: the pool drew this root and nothing else holds a
        // reference into it; the read observes one entry.
        let physmap_slot = unsafe { (*root)[root_slot(map_va)] };
        if physmap_slot == 0 {
            return Err("the process root carries no direct-map slot");
        }

        // The map reaches a frame *above* the window the spawn path used to
        // carry, and reaches the right one: the marker is written through
        // the map and read back through the boot root's own identity
        // window, so the two addresses are witnessed to alias the same
        // physical frame. Nothing owns the frame — the frame allocator does
        // not exist yet at this point in the boot — and the RAM self-test
        // overwrites every usable byte afterwards regardless.
        let high_phys = (paging::physmap_bytes() - PAGE_BYTES) & !(PAGE_BYTES - 1);
        if high_phys <= (OLD_SPAWN_WINDOW_GIB << 30) {
            return Err("the guest's RAM does not top the old window");
        }
        let high_map_va = paging::physmap_virt(high_phys);
        if root_slot(high_map_va) < paging::PHYSMAP_FIRST_SLOT {
            return Err("a high frame's direct-map address is outside the map");
        }
        // SAFETY: `high_phys` is the last page the live map covers, so the
        // map's own translation of it is valid; the boot root additionally
        // identity-maps the canonical lower half, so the same frame is
        // reachable at its physical address. The frame is unowned at this
        // point in the boot (no allocator exists) and the write is
        // in-bounds of that one page.
        let witnessed = unsafe {
            core::ptr::write_volatile(high_map_va as *mut u64, HIGH_MARKER);
            core::ptr::read_volatile(high_phys as *const u64)
        };
        if witnessed != HIGH_MARKER {
            return Err("the map and the identity window disagree on a high frame");
        }

        // And that frame's bare physical address reaches nothing under a
        // *process* root: the full-RAM identity mapping a root would
        // otherwise have to carry is what capped this port's reach.
        if process.translate(high_phys).is_some() {
            return Err("a high frame's bare physical address still resolves");
        }

        // A user leaf in the map's slot is refused outright: the slot holds
        // a mapping every root shares, so a user page there would be
        // visible from every address space.
        if process
            .map_page(map_va, probe_phys, PageFlags::READ | PageFlags::USER)
            .is_ok()
        {
            return Err("a user leaf was accepted in the direct map's slot");
        }

        // And a user address below the map still maps normally, in a
        // disjoint slot.
        let alias_va = tairix_kernel::riscv64::USER_VA_TOP - (2 << 30);
        let other_phys = probe_phys + 4096;
        if process
            .map_page(alias_va, other_phys, PageFlags::READ | PageFlags::USER)
            .is_err()
        {
            return Err("a user address below the map would not map");
        }
        if process.translate(alias_va).map(|(phys, _)| phys) != Some(other_phys) {
            return Err("the user address resolves to the wrong frame");
        }
        if root_slot(alias_va) >= paging::PHYSMAP_FIRST_SLOT {
            return Err("a user address landed in the map's slots");
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
    const FAIL_CODE: NonZeroU16 = NonZeroU16::new(1).expect("1 is non-zero");

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
                match field_u64(event, "gigabytes") {
                    // The guest is sized so its RAM tops the window the
                    // spawn path used to carry: a map no wider than that
                    // means the boot path never read the discovered tree,
                    // which is the defect. The map is live by the time this
                    // record is written, so the structural probe runs here.
                    Some(gib) if gib > OLD_SPAWN_WINDOW_GIB => {
                        if let Err(why) = structural_probe() {
                            fail(why);
                        }
                        MAP_SIZED_FROM_RAM.store(true, Ordering::Release);
                    }
                    _ => fail("the direct map is no wider than the old spawn window"),
                }
            }

            if event.id == RAM_SELF_TEST_EVENT_ID {
                match (
                    field_u64(event, "verified_bytes"),
                    field_u64(event, "unreachable_bytes"),
                ) {
                    // Every usable byte was written and read back through
                    // the direct map. A window that stopped short leaves
                    // the RAM above it unreachable, which is what shows up
                    // here.
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

    /// Forward to the shared riscv64 panic bridge. A panic before the
    /// finisher parks the hart, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_physmap_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_riscv64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
        // The observer is *both* sinks: the direct-map record and the RAM
        // self-test are log records, not audit ones, so grading them needs
        // the log sink too.
        boot_riscv64::boot(
            hartid,
            dtb,
            &ALLOCATOR,
            &OBSERVER,
            &OBSERVER,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
