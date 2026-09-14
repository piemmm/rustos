//! x86_64 direct-physical-map QEMU integration test: the kernel reaches
//! **every** usable byte of a guest that has more RAM than the boot
//! trampoline's own identity window, and it reaches it in the kernel half
//! rather than in the half user programs address.
//!
//! ## Why this exists
//!
//! The kernel reaches a frame by pointer through the port's direct physical
//! map — the process-image write, the shared-region zero-on-free scrub, the
//! remap window's record store, the kernel heap's slab page supply. That map
//! used to be a fixed window sized before the firmware memory map was read,
//! so on a machine with more RAM than the window every frame above it
//! translated to nothing and its consumer failed closed while gigabytes sat
//! free (`plans/OPEN-DEFECTS.md` D55). Nothing caught it, because every
//! other x86_64 guest in the matrix is small enough to fit the window.
//!
//! It was then an *identity* map in the low half, which every process root
//! had to carry, so it had to stop short of the user image bias and a
//! machine with more RAM than that had frames the kernel could not reach
//! (D56). Both halves of that are what this vertical now watches.
//!
//! ## What this test asserts
//!
//! The guest is given more RAM than the trampoline's window, so the firmware
//! map reports usable RAM above it. Then:
//!
//! 1. The boot path sized the map past the trampoline's own window — proof
//!    it read the discovered map rather than a build-time constant.
//! 2. The early-boot RAM self-test left **no** usable byte unreachable: each
//!    one was written and read back through the direct map. A frame the map
//!    does not cover is left untested and counted, so a map that stopped
//!    short shows up here rather than as a silent skip.
//! 3. The live structure holds: a frame is reachable through the map while
//!    the user virtual address that would alias it under an identity map
//!    holds an *unrelated* mapping, the two lie in disjoint root slots, and
//!    a process root maps that frame's bare physical address to nothing.
//!    That collision is what capped the map at the user bias, and it is
//!    checked on the metal under a live root rather than only on the host.
//!
//! Only when all three hold does `BootCompleted` report success to QEMU.
//!
//! ## How it differs from the production `tairix-kernel` binary
//!
//! It reuses the whole boot pipeline from `tairix_kernel::boot`; only the
//! sink is replaced. Splitting the observer into its own bin (rather than
//! gating it behind a Cargo feature on `tairix-kernel`) keeps feature
//! unification under `cargo build --workspace` from ever leaking the
//! QEMU-exit behaviour into a real kernel image.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_api::frames::PageTableFrames as _;
    use tairix_arch_api::mmu::{AddressSpace as _, PageFlags};
    use tairix_arch_x86_64::paging::{
        self, AddressSpace, PageTablePool, BOOT_IDENTITY_GIB, PHYSMAP_PML4_FIRST_SLOT,
    };
    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::KERNEL_BOOT_DIRECT_MAP;
    use tairix_kernel::{boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink};
    use tairix_log::{Event, EventId, FieldValue, Sink};

    // --- `#[global_allocator]`, as the production bin declares it -----
    //
    // Re-declared rather than re-exported because `#[global_allocator]` is a
    // per-binary attribute (see `kernel/tairix-kernel/Cargo.toml`).

    /// Static heap for the bump allocator.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: as for the production bin's `ALLOCATOR` — the page-aligned
    /// `HEAP` static outlives the binary and the allocator is its only
    /// consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    // --- Observer ----------------------------------------------------

    /// `kernel_core::AuditEvent::RamSelfTest`, carrying the bytes the
    /// self-test verified and the usable bytes the direct map could not
    /// reach. Pinned by the `event_ids_are_unique` test in
    /// `kernel/core/src/audit.rs`.
    const RAM_SELF_TEST_EVENT_ID: EventId = EventId(4005);

    /// `kernel_core::AuditEvent::BootCompleted`: every init phase succeeded.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Set when the boot path reported a direct map wider than the
    /// trampoline's own identity window — the guest's RAM forced it.
    static MAP_SIZED_FROM_RAM: AtomicBool = AtomicBool::new(false);

    /// Set when the RAM self-test reported verifying every usable byte.
    static RAM_FULLY_REACHED: AtomicBool = AtomicBool::new(false);

    /// Page-table pages the structural probe's two roots draw from. A
    /// `.bss` pool, so the probe needs nothing of the live allocator and
    /// cannot perturb the boot it is observing.
    static PROBE_POOL: PageTablePool = PageTablePool::new();

    /// Frame the probe reaches two ways: through the kernel image's own
    /// window (it is this binary's static) and through the direct map.
    /// Its marker is distinctive, so reading it back at
    /// `PHYSMAP_VMA_BASE + phys` identifies *that* frame rather than
    /// merely proving some page is mapped there.
    static PROBE_FRAME: ProbeFrame = ProbeFrame {
        marker: PROBE_MARKER,
        rest: [0; 4096 - PROBE_MARKER.len()],
    };

    /// A page-aligned frame the probe reads through the direct map. Never
    /// written: a read proves the translation, and the RAM self-test
    /// above already writes and reads back every usable byte.
    #[repr(C, align(4096))]
    struct ProbeFrame {
        marker: [u8; 8],
        rest: [u8; 4096 - 8],
    }

    /// The marker [`PROBE_FRAME`] opens with.
    const PROBE_MARKER: [u8; 8] = [0x5D, 0x56, 0xD1, 0xA6, 0x00, 0xFF, 0x7E, 0x81];

    /// The PML4 slot a virtual address resolves through: bits 47:39, with
    /// the sign-extension above them masked off.
    fn pml4_slot(va: u64) -> usize {
        ((va >> 39) & 0x1FF) as usize
    }

    /// Prove the structure the defect was about: the direct map reaches a
    /// frame in the kernel half, the user address that would alias it under
    /// an identity map holds an *unrelated* mapping in a disjoint root slot,
    /// and a process root maps that frame's bare physical address to
    /// nothing at all.
    ///
    /// The map's own shared tables are deliberately not walked. They are
    /// carved from the firmware memory map before any frame allocator
    /// exists, so no `PageTableFrames` source ever handed them out and
    /// `table_at` refuses them — which is the fail-closed behaviour the
    /// walk owes a table it cannot vouch for. The hardware translation is
    /// witnessed by reading through it instead, which is the stronger
    /// statement anyway.
    ///
    /// Run before the boot path builds PID 1, so the root it draws is never
    /// made live.
    fn structural_probe() -> Result<(), &'static str> {
        let probe_phys = core::ptr::addr_of!(PROBE_FRAME) as u64 - paging::KERNEL_VMA_BASE;
        let map_va = paging::physmap_virt(probe_phys);

        // The hardware reaches the frame through the map, and reaches *that*
        // frame: the marker read back is the one the kernel-window alias
        // holds.
        for (offset, expected) in PROBE_MARKER.iter().enumerate() {
            // SAFETY: the map is live (the boot path widened it before this
            // record is written, and the trampoline laid its floor before
            // any Rust ran) and covers `probe_phys`, which names this
            // binary's own page-aligned static; the read is in-bounds of
            // that frame and mutates nothing.
            let seen = unsafe { core::ptr::read_volatile((map_va + offset as u64) as *const u8) };
            if seen != *expected {
                return Err("the map does not read back the frame's marker");
            }
        }

        let Some(mut process) = AddressSpace::new_process_root(&PROBE_POOL) else {
            return Err("no process root");
        };

        // The root carries the map, and carries nothing at all in the slot
        // a bare physical address would resolve through.
        let Some(root) = PROBE_POOL.table_at(process.pml4_phys()) else {
            return Err("the pool cannot reach the root it drew");
        };
        // SAFETY: the pool drew this root and nothing else holds a
        // reference into it; the reads observe two entries.
        let (physmap_slot, identity_slot) = unsafe {
            (
                (*root)[PHYSMAP_PML4_FIRST_SLOT],
                (*root)[pml4_slot(probe_phys)],
            )
        };
        if physmap_slot == 0 {
            return Err("the process root carries no direct-map slot");
        }
        if identity_slot != 0 {
            return Err("the process root carries an identity mapping");
        }

        // The user address an identity map would have collided with maps an
        // *unrelated* frame, and it lies in a different root slot from the
        // map's own address.
        let alias_va = (tairix_kernel::x86_64::USER_VA_TOP >> 1) + probe_phys;
        let other_phys = probe_phys + 4096;
        if process
            .map_page(alias_va, other_phys, PageFlags::READ | PageFlags::USER)
            .is_err()
        {
            return Err("the aliasing user address would not map");
        }
        if process.translate(alias_va).map(|(phys, _)| phys) != Some(other_phys) {
            return Err("the aliasing user address resolves to the wrong frame");
        }
        if pml4_slot(alias_va) >= PHYSMAP_PML4_FIRST_SLOT {
            return Err("the user region reaches the direct map's slots");
        }
        if pml4_slot(map_va) != PHYSMAP_PML4_FIRST_SLOT {
            return Err("the map's address is not in its own slot");
        }

        // And the frame's bare physical address reaches nothing: the
        // full-RAM identity map every process root used to carry is gone.
        if process.translate(probe_phys).is_some() {
            return Err("a bare physical address still resolves");
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

    /// Forwards every record to the serial transcript and grades the two
    /// records this vertical turns on, exiting the moment one of them fails
    /// so the transcript ends at the check that broke rather than at a
    /// timeout.
    struct PhysMapObserver;

    impl Sink for PhysMapObserver {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);

            if event.id == KERNEL_BOOT_DIRECT_MAP {
                match field_u64(event, "gigabytes") {
                    // The guest is sized so its RAM tops the trampoline's
                    // identity window: a map no wider than that means the
                    // boot path never read the discovered map, which is the
                    // defect. The map is live by the time this record is
                    // written, so the structural probe runs here.
                    Some(gib) if gib > BOOT_IDENTITY_GIB as u64 => {
                        if let Err(why) = structural_probe() {
                            SerialSink::new().write_event(&Event {
                                level: tairix_log::Level::Error,
                                id: EventId(0),
                                message: why,
                                fields: &[],
                            });
                            qemu_exit::exit_failure();
                        }
                        MAP_SIZED_FROM_RAM.store(true, Ordering::Release);
                    }
                    _ => qemu_exit::exit_failure(),
                }
            }

            if event.id == RAM_SELF_TEST_EVENT_ID {
                match (
                    field_u64(event, "verified_bytes"),
                    field_u64(event, "unreachable_bytes"),
                ) {
                    // Every usable byte was written and read back through the
                    // direct map. A window that stopped short leaves the RAM
                    // above it unreachable, which is what shows up here.
                    (Some(verified), Some(0)) if verified != 0 => {
                        RAM_FULLY_REACHED.store(true, Ordering::Release);
                    }
                    _ => qemu_exit::exit_failure(),
                }
            }

            if event.id == BOOT_COMPLETED_EVENT_ID {
                if MAP_SIZED_FROM_RAM.load(Ordering::Acquire)
                    && RAM_FULLY_REACHED.load(Ordering::Acquire)
                {
                    qemu_exit::exit_success();
                }
                // Booting without either record having been graded means the
                // step this vertical exists to watch never ran.
                qemu_exit::exit_failure();
            }
        }
    }

    static OBSERVER: PhysMapObserver = PhysMapObserver;

    // --- Panic handler ------------------------------------------------

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    ///
    /// The bridge logs through `SERIAL_SINK`, not the observer, so a panic
    /// never trips the QEMU-exit path: the run times out and the harness
    /// reports the failure loudly.
    #[panic_handler]
    fn tairix_physmap_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    // --- Entry point ---------------------------------------------------

    /// The symbol the arch crate's boot trampoline calls.
    ///
    /// The observer stands in for **both** sinks: the direct-map and
    /// RAM-self-test records are diagnostics on the log channel while
    /// `BootCompleted` is an audit record, and this vertical grades all
    /// three.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &ALLOCATOR,
            &OBSERVER,
            &OBSERVER,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
