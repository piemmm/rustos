//! The aarch64 live root-unlock bring-up (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (2)).
//!
//! The freestanding-aarch64 half of the in-kernel root-unlock service: it
//! lives in the architecture subtree ([`crate::aarch64`]) because it names
//! the aarch64 port directly (`rustos_arch_aarch64`, the GIC, the firmware
//! device tree), while the device-independent core — the boot stash and the
//! console-0 ownership gate — stays in the arch-neutral
//! [`crate::unlock_service`] (`AGENTS.md` §2.2 / §17.2).
//!
//! It admits the in-kernel unlock kthread at the init seam, brings the
//! bootstrap virtio-blk root device up over the production device-IRQ path
//! (INCREMENT (1)), and runs the device-independent unlock policy
//! ([`crate::root_mount::unlock_root_disk_interactively`]) inside the
//! kthread — opening the console-0 ownership gate the instant the unlock
//! resolves so `login` can take over
//! ([`crate::unlock_service::CONSOLE0_GATE`]).
//!
//! The QEMU `virt` board (virtio-blk-MMIO) is the path proven here; the
//! Raspberry Pi 4 EMMC2 SD host is the staged metal increment, so an EMMC2
//! binding fails closed (logged, gate opened, no database installed) until
//! that increment lands (`plans/PI.md` P11).

use rustos_abi::driver::dma::PoolId;
use rustos_abi::{HwNode, IrqHandle};
use rustos_arch_aarch64::fdt::gic_device_intid;
use rustos_arch_aarch64::paging::{
    configured_identity_gigapages, AddressSpace as ArchAddressSpace, PageTablePool,
};
use rustos_arch_aarch64::{gic, video, SERIAL_SINK};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_bus_mmio::virtio_mmio_bus_from_dtb;
use rustos_drv_bus_virtio::MmioTransport;
use rustos_drv_storage_virtio_blk::{VirtioBlk, VIRTIO_BLK_DEVICE_ID};
use rustos_fdt::Fdt;
use rustos_kernel_core::{ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, YieldHandle};
use rustos_kernel_irq::{IrqTable, IrqWaitAbort, IrqWaiter};
use rustos_kernel_mem::{AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, MmioMap, VirtAddr};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_sec::identity::UserId;
use rustos_kernel_virtio::{provision_virtio_mmio, KernelMmioMapper, KernelVirtioHost};
use rustos_log::{Level, Sink};

use crate::aarch64::arch_wrapper::{UART_CONSOLE, VIDEO_CONSOLE, VIDEO_KEYBOARD};
use crate::aarch64::gic_irq::{published_irq_table, GIC_IRQ_CONTROLLER};
use crate::aarch64::spawn_producer::AARCH64_PROCESS_SPAWN;
use crate::driver_catalog::{KERNEL_DRIVER_SIGNER_PUBKEY, VIRTIO_BLK_PATH};
use crate::driver_loader::KernelDriverLoader;
use crate::driver_spawn_loader::InitCtxDriverProcessSpawn;
use crate::root_mount::{
    autoload_system_drivers, unlock_root_disk_interactively, UnlockOutcome, LATE_USERS_DB,
};
use crate::unlock_service::{
    autoload_caps, loader_caps, note, service_caps, take_boot, AutoloadHook, KthreadConsoleRead,
    CONSOLE0_GATE, UNLOCK_TASK, USERS_DB_INSTALLED_MESSAGE,
};

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
pub fn spawn_if_present(ctx: &'static (dyn InitSpawnCtx + Sync)) -> bool {
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
    // The discovered hardware tree the kthread matches against the signed
    // driver store once the root mounts (`AGENTS.md` §18.1 / §18.3). A
    // `&'static [HwNode]` (the boot path leaked it), so the `'static + Send`
    // kthread body captures it by value.
    let tree = boot.tree;
    let caps = service_caps();
    let body = move |yielder: &mut dyn YieldHandle| {
        let outcome = run_unlock(yielder, dtb, frames, caps, audit, ctx, tree);
        match outcome {
            Ok(UnlockOutcome::Installed) => {
                note(audit, Level::Info, USERS_DB_INSTALLED_MESSAGE);
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
/// Before prompting for the passphrase, the [`AutoloadHook`] this builds
/// autoloads user-space drivers off the read-only `/System` volume's signed
/// `/System/Drivers/` store ([`autoload_system_drivers`]) — matching every
/// node of the discovered `tree` against the store and spawning each winner
/// into its own process through `ctx`'s [`InitSpawnCtx::spawn_driver_process`]
/// seam (`AGENTS.md` §18.3). Running it *first* brings the keyboard up in
/// user space in time to type the unlock secret (design B); it is fail-soft
/// and cannot fail the boot.
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
    ctx: &'static (dyn InitSpawnCtx + Sync),
    tree: &'static [HwNode],
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
    let mut blk = VirtioBlk::open(transport, &vhost).map_err(|_| "root-unlock: virtio-blk open")?;

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
    let reader = KthreadConsoleRead::new(raw_read, &coop);

    // The driver-signing trust anchor the autoload load gate verifies each
    // winning bundle against — the kernel's own embedded key, the single
    // source `KernelDriverLoader` also trusts (`AGENTS.md` §8 / §9 / §2.2). A
    // corrupt key is a broken build, not an admissible state: fail closed
    // (`AGENTS.md` §2.9) rather than autoload against no anchor.
    let trust_anchor = Ed25519PublicKey::from_bytes(&KERNEL_DRIVER_SIGNER_PUBKEY)
        .map_err(|_| "root-unlock: driver trust anchor")?;
    let trusted = [trust_anchor];
    // The scheduler-agnostic driver-spawn seam over the captured boot
    // context + the aarch64 process producer (`AGENTS.md` §17.1 / §2.2) —
    // the one per-arch input the otherwise arch-neutral autoload hook needs.
    let driver_spawn = InitCtxDriverProcessSpawn::new(ctx, &AARCH64_PROCESS_SPAWN);
    // The pre-unlock autoload hook: run against the read-only `/System`
    // volume below, it matches every discovered node against that volume's
    // signed driver store and spawns each winner into its own user-space
    // process (`AGENTS.md` §18.3). It presents the gate the delegatable `autoload_caps` superset
    // (`CAP_DRV_LOAD` to pass the gate plus the resource capabilities an
    // autoloaded driver's class may request — including `CAP_INPUT_INJECT` for
    // an input driver), intersected per driver with its signed manifest
    // request (`AGENTS.md` §5.2 / §18.3); the kthread's *own* context stays the
    // minimal `service_caps` (§5.4).
    let mut autoload = AutoloadHook::new(&driver_spawn, tree, &trusted, autoload_caps(), audit);

    // Design B2: autoload off the read-only `/System` volume's signed store
    // **before** the passphrase prompt, so the keyboard (and any other
    // matched driver) is brought up in user space in time for the operator
    // to type the encrypted-root unlock secret — the chicken-and-egg design B
    // resolves. Fail-soft and fail-closed (`AGENTS.md` §18.4 / §2.9): a disk
    // with no `/System` volume autoloads nothing and the boot still reaches
    // the prompt (on the UART the passphrase can still be typed). The borrow
    // of `blk` ends here, returning the device for the interactive unlock.
    autoload_system_drivers(&mut blk, &mut autoload, audit);

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
