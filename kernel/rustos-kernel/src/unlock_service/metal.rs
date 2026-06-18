//! Freestanding (aarch64) live root-unlock bring-up (`plans/PI.md` P11
//! Chunk B-2 INCREMENT (2)).
//!
//! Runs only on the bare-metal aarch64 boot path. It admits the in-kernel
//! unlock kthread at the init seam, brings the bootstrap virtio-blk root
//! device up over the production device-IRQ path (INCREMENT (1)), and runs
//! the device-independent unlock policy
//! ([`crate::root_mount::unlock_root_disk_interactively`]) inside the
//! kthread — opening the console-0 ownership gate the instant the unlock
//! resolves so `login` can take over (`super::CONSOLE0_GATE`).
//!
//! The QEMU `virt` board (virtio-blk-MMIO) is the path proven here; the
//! Raspberry Pi 4 EMMC2 SD host is the staged metal increment, so an EMMC2
//! binding fails closed (logged, gate opened, no database installed) until
//! that increment lands (`plans/PI.md` P11).

use rustos_abi::driver::dma::PoolId;
use rustos_abi::{CapabilityId, IrqHandle};
use rustos_arch_aarch64::fdt::gic_device_intid;
use rustos_arch_aarch64::paging::{
    configured_identity_gigapages, AddressSpace as ArchAddressSpace, PageTablePool,
};
use rustos_arch_aarch64::{gic, video, SERIAL_SINK};
use rustos_caps::CapabilitySet;
use rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb;
use rustos_drv_bus_virtio::MmioTransport;
use rustos_drv_storage_virtio_blk::{VirtioBlk, VIRTIO_BLK_DEVICE_ID};
use rustos_fdt::Fdt;
use rustos_kernel_core::{ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, YieldHandle};
use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
use rustos_kernel_mem::{AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, MmioMap, VirtAddr};
use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
use rustos_kernel_sec::identity::UserId;
use rustos_kernel_virtio::{provision_virtio_mmio, KernelMmioMapper, KernelVirtioHost};
use rustos_log::{log, Event, EventId, Level, Sink};

use crate::aarch64::arch_wrapper::{UART_CONSOLE, VIDEO_CONSOLE, VIDEO_KEYBOARD};
use crate::aarch64::gic_irq::{published_irq_table, GIC_IRQ_CONTROLLER};
use crate::driver_catalog::VIRTIO_BLK_PATH;
use crate::driver_loader::KernelDriverLoader;
use crate::root_mount::{unlock_root_disk_interactively, UnlockOutcome, LATE_USERS_DB};

use super::{take_boot, CONSOLE0_GATE};

/// Audit event: the in-kernel root-unlock service lifecycle (started /
/// skipped / device bring-up result), logged at the PID 1 spawn seam and
/// from the kthread (`AGENTS.md` §19.4). Sits beside the root-mount audit
/// ids (`4135`–`4138`, [`crate::root_mount`] / [`crate::root_storage`]).
const UNLOCK_SERVICE: EventId = EventId(4139);

/// Synthetic owner task id for the unlock kthread's capability context and
/// IRQ binding. Distinct from the keyboard service's so an audit observer
/// can tell the two in-kernel services apart.
const UNLOCK_TASK: TaskId = TaskId(0x5b4);

/// CPU-interface target bitmask routing the device SPI to the boot CPU.
const CPU0_TARGET: u8 = 0b0000_0001;

/// Per-device DMA window capacity, in pages, the virtio-blk driver
/// allocates its request/data buffers from (transient per-request DMA).
const POOL_PAGES: usize = 64;

/// Bookkeeping virtual base of the minted per-driver DMA window.
///
/// The driver reaches buffers through the identity map ([`DirectPhysMap`]),
/// so this address space is **pure bookkeeping**; the base is chosen far
/// above the boot identity window (which never exceeds a few GiB) so a
/// window mapping never collides with an identity gigapage block in the
/// throwaway bookkeeping space. Genuinely this bring-up's own constant
/// (`AGENTS.md` §2.2).
const POOL_VBASE: u64 = 0x60_0000_0000;

/// Bookkeeping virtual base of the MMIO register-window map (see
/// [`POOL_VBASE`] — far above the identity window, bookkeeping only).
const MMIO_VBASE: u64 = 0x40_0000_0000;

/// The page-table frame pool the two throwaway *bookkeeping* address
/// spaces (the MMIO map and the DMA pool) allocate their root + window
/// tables from. Private to the unlock service, so it never contends with
/// the boot/init page-table pools. The bookkeeping spaces are never made
/// live (device access is via the boot identity map through
/// [`DirectPhysMap`]); the pool only backs the guard-bracketed window
/// accounting `kernel/mem` performs.
static UNLOCK_PT_POOL: PageTablePool = PageTablePool::new();

/// Capacity, in pages, of the MMIO register-window map.
const MMIO_CAP_PAGES: usize = 64;

/// The capabilities the unlock kthread holds: [`CapabilityId::MMIO_MAP`]
/// (the virtio register window), [`CapabilityId::MEM_DMA`] (the request
/// DMA), and [`CapabilityId::DRV_LOAD`] (the signed driver-load gate). No
/// more — every map/alloc/load is re-checked against this set (§5.4).
fn service_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::MMIO_MAP);
    caps.insert(CapabilityId::MEM_DMA);
    caps.insert(CapabilityId::DRV_LOAD);
    caps
}

/// The capability set the signed driver-load gate is presented with:
/// `CAP_DRV_LOAD` + `CAP_DRV_KERNEL` (the bootstrap virtio-blk manifest is
/// `kind = InKernel`). Each driver receives only the intersection with its
/// manifest request (`AGENTS.md` §5.2).
fn loader_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::DRV_LOAD);
    caps.insert(CapabilityId::DRV_KERNEL);
    caps
}

/// Log an unlock-service lifecycle decision onto the service's audit sink.
fn note(audit: &dyn Sink, level: Level, message: &'static str) {
    log(
        audit,
        &Event {
            level,
            id: UNLOCK_SERVICE,
            message,
            fields: &[],
        },
    );
}

/// Find the GICv2 INTID of the `virtio,mmio` node whose `reg` base equals
/// `slot_base`, decoded through the production [`gic_device_intid`]
/// (INCREMENT (1)) — no board constant (`AGENTS.md` §2.20). [`None`] when
/// no node matches or its `interrupts` specifier is unrepresentable
/// (fail closed, §18.4).
fn device_spi(fdt: &Fdt<'_>, slot_base: u64) -> Option<u32> {
    for node in fdt.nodes() {
        let node = node.ok()?;
        if !node.is_compatible("virtio,mmio") {
            continue;
        }
        let reg = node.property("reg")?;
        if reg.read_be_u64(0).ok()? != slot_base {
            continue;
        }
        return gic_device_intid(&node);
    }
    None
}

/// A blocking [`IrqWaiter`] for the unlock kthread: it **re-arms** the
/// device's GIC line, then parks on a race-free `wfi` until the next
/// completion.
///
/// [`IrqTable::fire`] masks the line on every completion (mask-before-wake),
/// so the line is first re-enabled through [`GIC_IRQ_CONTROLLER`] (the arch
/// unmask the kernel-side [`rustos_kernel_irq::IrqController`] trait
/// deliberately does not expose). It then performs the canonical race-free
/// park — mask IRQ *taking* ([`rustos_arch_aarch64::exceptions::mask_irq`]),
/// re-check the line's ready flag, `wfi`
/// ([`rustos_arch_aarch64::exceptions::wait_for_interrupt`]) only if still
/// not ready, then unmask ([`rustos_arch_aarch64::exceptions::enable_irq`])
/// so the woken completion is taken by the EL1 vector and dispatched into
/// `IrqTable::fire`. This is exactly the discipline the proven `-M virt`
/// `WfiWaiter` uses (`AGENTS.md` §2.2 — one wait shape).
///
/// Parking on `wfi` (rather than busy-yielding through the scheduler) is
/// both correct and §2.1-clean. The production cooperative dispatch runs
/// with `DAIF.I` masked once a user task's `svc` trap has masked it, and the
/// register-only context switch (`context.s`) never restores it, so a
/// kthread that merely yielded would spin a tight poll on a line whose
/// interrupt is never taken — and re-arming the GIC line on every such spin
/// can mis-deliver back-to-back completions, corrupting a multi-block read.
/// The mask → check → `wfi` → unmask sequence takes exactly one completion
/// per wake and loses no edge (a completion landing in the check-park window
/// stays pending and wakes the `wfi`). During the boot root-unlock PID 1
/// (`wait`) and `login` (gated console read) are parked, so halting the CPU
/// until the completion starves nothing.
struct RearmingIrqWaiter {
    table: &'static IrqTable,
    handle: IrqHandle,
    line: u32,
}

impl IrqWaiter for RearmingIrqWaiter {
    fn now_ns(&self) -> u64 {
        // The wait is the unbounded `u64::MAX` sentinel, so the clock value
        // never reaches a deadline and is immaterial — the `WfiWaiter`
        // returns `0` for the same reason.
        0
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // Re-arm the line for the next completion; the previous
        // `IrqTable::fire` masked it. A re-arm refusal (an out-of-range
        // line — impossible for a bound SPI) is harmless: the park below
        // then waits on a line that cannot fire and the run budget bounds
        // it (`AGENTS.md` §2.9).
        let _ = GIC_IRQ_CONTROLLER.rearm(self.line);
        // Canonical race-free park: mask IRQ taking, re-check the ready
        // flag, `wfi` only if still not ready, then unmask so the woken
        // completion is dispatched.
        // SAFETY: the EL1 vector table is installed (boot
        // `exceptions::init_vectors`) and the production device dispatch is
        // published (the kernel-core `irq` phase), so the woken IRQ is
        // handled rather than faulting; the three calls only manipulate
        // `DAIF.I` and issue the `wfi` hint.
        unsafe {
            rustos_arch_aarch64::exceptions::mask_irq();
            if !self.table.ready_for(self.handle) {
                rustos_arch_aarch64::exceptions::wait_for_interrupt();
            }
            rustos_arch_aarch64::exceptions::enable_irq();
        }
        Ok(())
    }
}

/// A cooperative blocking console reader for the unlock kthread.
///
/// The kthread analogue of kernel-core's `BlockingConsoleRead` (which parks
/// only a *user* kthread, via `reschedule_current`): an empty device poll
/// suspends the kthread through its shared [`CooperativeYield`] cell and
/// re-polls on the next dispatch, so the passphrase prompt blocks for input
/// without busy-spinning (`AGENTS.md` §2.1) and never fabricates an end of
/// input. It drives the kthread's single [`YieldHandle`] through the
/// [`CooperativeYield`] cell `run_unlock` lends it (`!Sync`, never shared
/// across CPUs); the device-IRQ wait parks on `wfi` separately
/// ([`RearmingIrqWaiter`]), so the console poll and the block-I/O wait do
/// not share a suspend mechanism.
struct KthreadConsoleRead<'a> {
    inner: &'static (dyn ConsoleRead + Sync + 'static),
    yielder: &'a CooperativeYield<'a>,
}

impl ConsoleRead for KthreadConsoleRead<'_> {
    fn read(&self, buf: &mut [u8]) -> Result<usize, rustos_abi::Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.inner.read(buf)?;
            if read > 0 {
                return Ok(read);
            }
            self.yielder.yield_now();
        }
    }
}

/// Admit the in-kernel root-unlock kthread if the boot path bound a
/// virtio-blk root block device, returning whether it was started.
///
/// With no binding (headless / no disk / ambiguous), an EMMC2 binding (the
/// staged Pi metal path), or no `'static` frame allocator, it starts
/// nothing, opens the console-0 gate so `login` proceeds normally, and
/// returns `false` — failing closed (`AGENTS.md` §18.4 / §2.9). The
/// console-0 gate is also opened by the kthread body once the unlock
/// resolves, so it is never left latched closed.
#[must_use]
pub fn spawn_if_present(ctx: &dyn InitSpawnCtx) -> bool {
    let boot = take_boot();
    // Route the unlock service's security-relevant decisions (mount /
    // install / give-up) onto the boot audit channel (`AGENTS.md` §19.4)
    // when the init seam wired a `'static` audit sink; fall back to the
    // serial log otherwise. The kthread body and the unlock policy share it.
    let audit: &'static (dyn Sink + Sync) = ctx.static_audit().unwrap_or(&SERIAL_SINK);
    let Some(binding) = boot.binding else {
        note(
            audit,
            Level::Info,
            "root-unlock: no root block device bound; root unbound, login refuses (§18.4)",
        );
        CONSOLE0_GATE.open();
        return false;
    };
    if binding.driver_path != VIRTIO_BLK_PATH {
        // EMMC2 (the Raspberry Pi 4 SD host) is the staged metal increment
        // (`plans/PI.md` P11): the live EMMC2 bring-up is not yet wired, so
        // fail closed — open the gate and leave the root unmounted rather
        // than half-bring it up (`AGENTS.md` §2.9 / §2.19 — a real
        // fail-closed boundary, not a disguised partial path).
        note(
            audit,
            Level::Info,
            "root-unlock: EMMC2 root bring-up is the staged Pi metal increment; root unbound",
        );
        CONSOLE0_GATE.open();
        return false;
    }
    let Some(frames) = ctx.static_frames() else {
        note(
            audit,
            Level::Error,
            "root-unlock: no kernel frame allocator; root unbound, login refuses",
        );
        CONSOLE0_GATE.open();
        return false;
    };

    let dtb = boot.dtb;
    let caps = service_caps();
    let body = move |yielder: &mut dyn YieldHandle| {
        let outcome = run_unlock(yielder, dtb, frames, caps, audit);
        match outcome {
            Ok(UnlockOutcome::Installed) => {
                note(audit, Level::Info, super::USERS_DB_INSTALLED_MESSAGE);
            }
            Ok(UnlockOutcome::GaveUp) => {
                note(
                    audit,
                    Level::Error,
                    "root-unlock: gave up fail-closed; login refused until reboot",
                );
            }
            Err(stage) => {
                note(audit, Level::Error, stage);
            }
        }
        // Release console 0 to `login` regardless of outcome: the unlock is
        // done (installed or fail-closed), so the byte-contention window is
        // over (`plans/PI.md` P11 item 5). A failed unlock leaves
        // `LATE_USERS_DB` empty, so `login` still refuses every attempt
        // (`AGENTS.md` §5.4.5).
        CONSOLE0_GATE.open();
    };

    let started = ctx.spawn_kernel_service(alloc::boxed::Box::new(body));
    note(
        audit,
        if started { Level::Info } else { Level::Error },
        if started {
            "root-unlock service kthread admitted (bring-up runs on first dispatch)"
        } else {
            "root-unlock service kthread could not be admitted; opening console gate"
        },
    );
    if !started {
        // Admission failed: nothing will open the gate, so do it here or
        // console-0 `login` would park forever (`AGENTS.md` §2.9).
        CONSOLE0_GATE.open();
    }
    started
}

/// Bring the virtio-blk root device up over the production device-IRQ path
/// and run the interactive unlock policy, returning its outcome.
///
/// Every fallible step fails closed with a stable stage string the caller
/// logs (`AGENTS.md` §2.9); the caller opens the console-0 gate on every
/// path.
fn run_unlock(
    yielder: &mut dyn YieldHandle,
    dtb: u64,
    frames: &'static FrameAllocator,
    caps: CapabilitySet,
    audit: &'static (dyn Sink + Sync),
) -> Result<UnlockOutcome, &'static str> {
    // Move the kthread's single yield handle into the shared cell both the
    // re-arming IRQ waiter and the cooperative console reader suspend
    // through (`AGENTS.md` §2.2 — one cooperative-yield definition).
    let coop = CooperativeYield::new(yielder);
    if dtb == 0 {
        return Err("root-unlock: no device tree; root unbound");
    }
    // SAFETY: on the boot hand-off `dtb` is the firmware/loader device-tree
    // pointer (`boot.s` preserves x0), identity-mapped and immutable for the
    // life of the kernel. `Fdt::from_ptr` validates the magic and bounds the
    // blob by its own `totalsize` before any read.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }
        .map_err(|_| "root-unlock: device tree unreadable; root unbound")?;
    // The DTB bytes the bus builder needs: the blob the validated `Fdt`
    // bounds, reborrowed as a `'static` slice (the firmware tree outlives
    // the kernel).
    let total = fdt.total_size();
    // SAFETY: `dtb`/`total` bound the same firmware blob `Fdt::from_ptr`
    // validated; it is identity-mapped, read-only, and outlives the kernel.
    let dtb_bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(dtb as *const u8, total) };

    // The bus-driver task capability context: the unlock kthread's caps,
    // owner `UNLOCK_TASK`, audited onto the service's audit sink.
    let caller = TaskCapabilities::derive(UNLOCK_TASK, UserId(0), caps, caps, audit);

    // Build the `virt`-board virtio-MMIO bus and provision the block
    // transport through the `CAP_MMIO_MAP`-gated kernel mapper.
    // SAFETY: the virtio-MMIO aperture the device tree describes is
    // identity-mapped Device memory the bus alone reads.
    let bus =
        unsafe { virtio_mmio_bus_from_dtb(dtb_bytes) }.map_err(|_| "root-unlock: virtio bus")?;
    let phys = DirectPhysMap::identity(identity_limit());
    let gib = configured_identity_gigapages();
    // Two throwaway *bookkeeping* page tables (device access is via the
    // boot identity map through `phys`): one for the MMIO window map, one
    // for the DMA pool. Each identity-maps the boot window so the
    // bookkeeping tables themselves are reachable; the window/pool VAs sit
    // far above it so they never collide with an identity block.
    let mmio_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, gib)
        .ok_or("root-unlock: mmio bookkeeping space")?;
    let mut mmio = MmioMap::new(
        AddressSpace::new(mmio_space),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    )
    .map_err(|_| "root-unlock: mmio map")?;
    let (transport, slot_base) = {
        let mapper = KernelMmioMapper::new(&mut mmio, &caller, audit);
        let prov = provision_virtio_mmio(&bus, VIRTIO_BLK_DEVICE_ID, &mapper, MmioTransport::new)
            .map_err(|_| "root-unlock: virtio provisioning")?;
        (prov.transport, prov.base)
    };

    // Resolve, bind, route, and arm the device's GIC SPI on the table the
    // kernel core published (INCREMENT (1)). The EL1 device-IRQ dispatch is
    // already installed by the core's `irq` phase, firing into this table.
    let intid = device_spi(&fdt, slot_base).ok_or("root-unlock: no device interrupt in DTB")?;
    let table: &'static IrqTable =
        published_irq_table().ok_or("root-unlock: no published IRQ table")?;
    let bind = table
        .bind(intid, UNLOCK_TASK)
        .map_err(|_| "root-unlock: bind device SPI")?;
    let handle: IrqHandle = bind.handle;
    // SAFETY: the GIC distributor + CPU interface are up (the kernel-core
    // `irq` phase brought them up via `gic_irq::install_device_irq_dispatch`
    // -> `gic::init`), and the EL1 vectors + device dispatch are installed;
    // this routes + enables the bound SPI on CPU 0.
    unsafe {
        gic::route_spi(intid, CPU0_TARGET);
    }
    // Arm the line for the first completion; the waiter re-arms it after
    // each subsequent one.
    let _ = GIC_IRQ_CONTROLLER.rearm(intid);

    // Mint the per-driver DMA host the driver allocates through, driven by
    // the re-arming `wfi` waiter.
    let dma_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, gib)
        .ok_or("root-unlock: dma bookkeeping space")?;
    let pool = DmaPool::new(
        AddressSpace::new(dma_space),
        VirtAddr::new(POOL_VBASE),
        POOL_PAGES,
        frames,
        &phys,
    )
    .map_err(|_| "root-unlock: dma pool")?;
    let waiter = RearmingIrqWaiter {
        table,
        handle,
        line: intid,
    };
    let vhost = KernelVirtioHost::new(
        pool,
        &caller,
        audit,
        PoolId::fresh(),
        table,
        handle,
        &waiter,
    );

    // Admit the virtio-blk driver through the signed §8 load gate (Ed25519
    // signature + `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) before it drives
    // hardware — a refusal fails closed (`AGENTS.md` §5.4 / §23.1).
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(VIRTIO_BLK_PATH, &loader_caps())
        .map_err(|_| "root-unlock: virtio-blk refused at the signed load gate")?;

    // Open the whole-disk block device over the provisioned transport.
    let blk = VirtioBlk::open(transport, &vhost).map_err(|_| "root-unlock: virtio-blk open")?;

    // Primary console (index 0): the video console + its keyboard queue when
    // a framebuffer console is active, else the discovered UART. The unlock
    // reads the *raw* device directly (bypassing the console-0 gate `login`
    // reads through), wrapped in the kthread cooperative blocking reader.
    let (console_write, raw_read): (
        &'static dyn ConsoleWrite,
        &'static (dyn ConsoleRead + Sync + 'static),
    ) = if video::is_active() {
        (&VIDEO_CONSOLE, &VIDEO_KEYBOARD)
    } else {
        (&UART_CONSOLE, &UART_CONSOLE)
    };
    let reader = KthreadConsoleRead {
        inner: raw_read,
        yielder: &coop,
    };

    Ok(unlock_root_disk_interactively(
        blk,
        console_write,
        &reader,
        &LATE_USERS_DB,
        audit,
    ))
}

/// The production aarch64 identity-map extent the DMA/MMIO physical map
/// reaches frames and device windows through: the configured number of
/// identity-mapped gigapages (`plans/PI.md` P6), so the map matches the
/// boot path's own identity extent rather than a fixed guess
/// (`AGENTS.md` §24.1).
fn identity_limit() -> u64 {
    (configured_identity_gigapages() as u64) << 30
}
