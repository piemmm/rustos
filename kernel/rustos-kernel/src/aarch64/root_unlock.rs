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
//! Two bootstrap-floor block drivers (`AGENTS.md` §18.6) are brought up
//! here, selected by which one [`crate::root_storage`] bound: the virtio-blk
//! device over the production device-IRQ path (the QEMU `virt` / x86_64
//! root, proven on `-M virt`), or the Raspberry Pi 4 EMMC2 SD host over
//! programmed I/O ([`crate::driver_catalog::EMMC2_PATH`], the Pi-metal root
//! — `raspi4b` cannot model EMMC2, so it is host-tested at the driver level
//! and metal-gated here, `plans/PI.md` P8/B4). The bring-up differs per
//! device; the read-only `/System` autoload, the passphrase prompt, and the
//! interactive unlock are identical and shared in [`finish_unlock`]
//! (`AGENTS.md` §2.2). A bound driver that is neither fails closed
//! (logged, gate opened, no database installed; `AGENTS.md` §2.9 / §18.4).

use core::convert::Infallible;

use rustos_abi::driver::block::Block;
use rustos_abi::driver::dma::PoolId;
use rustos_abi::driver::sole_register_window;
use rustos_abi::{CapabilityId, DriverHost, DriverKind, HwNode, IrqHandle, MmioMapper};
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
use crate::driver_catalog::{EMMC2_PATH, KERNEL_DRIVER_SIGNER_PUBKEY, VIRTIO_BLK_PATH};
use crate::driver_loader::KernelDriverLoader;
use crate::driver_spawn_loader::InitCtxDriverProcessSpawn;
use crate::root_mount::{unlock_root_disk_interactively, UnlockOutcome, LATE_USERS_DB};
use crate::root_storage::RootBlockBinding;
use crate::shared_block::{DriverStoreService, SharedBlock};
use crate::unlock_service::{
    autoload_caps, loader_caps, note, note_stage, service_caps, store_endpoint_binder_caps,
    take_boot, KthreadConsoleRead, CONSOLE0_GATE, UNLOCK_TASK, USERS_DB_INSTALLED_MESSAGE,
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

/// Release console 0 to `login` and mark the users-database source
/// resolved.
///
/// The two always happen together — both mean "the unlock window is over,
/// `login` may take the console now" — so they are flipped through one
/// helper to keep them from diverging (`AGENTS.md` §2.2). Opening the gate
/// lets `login`'s gated console reads through ([`GatedConsoleRead`]);
/// [`LateUsersDb::resolve`](rustos_kernel_core::LateUsersDb::resolve) flips
/// a `login` parked on the pending (`WouldBlock`) `users_db_read` into its
/// prompt — against the installed database if the unlock succeeded
/// ([`install`](rustos_kernel_core::LateUsersDb) ran first and wins), else
/// fail-closed deny-all (`AGENTS.md` §5.4.5).
fn release_console0_to_login() {
    CONSOLE0_GATE.open();
    LATE_USERS_DB.resolve();
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
        release_console0_to_login();
        return false;
    };
    if binding.driver_path != VIRTIO_BLK_PATH && binding.driver_path != EMMC2_PATH {
        // The bound driver is not one of the bootstrap-floor block drivers
        // this seam knows how to bring up (virtio-blk for the QEMU `virt` /
        // x86_64 root, EMMC2 for the Raspberry Pi 4 SD card). Fail closed —
        // open the gate and leave the root unmounted rather than guess at a
        // bring-up (`AGENTS.md` §2.9 / §18.4). `root_storage` only ever binds
        // a `provides_root_block` floor driver, so reaching here is a
        // packaging defect, not an expected path.
        note(
            audit,
            Level::Error,
            "root-unlock: bound block driver is not a known floor driver; root unbound",
        );
        release_console0_to_login();
        return false;
    }
    let Some(frames) = ctx.static_frames() else {
        note(
            audit,
            Level::Error,
            "root-unlock: no kernel frame allocator; root unbound, login refuses",
        );
        release_console0_to_login();
        return false;
    };

    let dtb = boot.dtb;
    // The discovered hardware tree the kthread matches against the signed
    // driver store once the root mounts (`AGENTS.md` §18.1 / §18.3). A
    // `'static` snapshot of the authoritative inventory store (taken *after*
    // the floor bring-up has appended its enumerated children), so the
    // `'static + Send` kthread body captures it by value (Design D, D1 —
    // `crate::hwtree_store`).
    let tree = crate::unlock_service::boot_tree_snapshot();
    let caps = service_caps();
    let env = UnlockEnv { ctx, audit, tree };
    let body = move |yielder: &mut dyn YieldHandle| {
        // On success the root-unlock service never returns: it parks for life
        // as the persistent driver-store service (Design D D2a-2), having
        // already logged the unlock outcome and released console 0. Only an
        // early bring-up failure returns here — and because the success arm is
        // the uninhabited [`Infallible`], the `Err` binding is irrefutable.
        // Fail closed (`AGENTS.md` §2.9): log the stage and open the console-0
        // gate so `login` proceeds (it refuses every attempt, as a failed
        // unlock installs no database, §5.4.5).
        let Err(stage) = run_unlock(yielder, &binding, dtb, frames, caps, env);
        note(audit, Level::Error, stage);
        release_console0_to_login();
    };

    let admitted = ctx.spawn_kernel_service(alloc::boxed::Box::new(body));
    if let Some(task_id) = admitted {
        // Publish the disk-owning kthread's scheduler id so its driver-store
        // serve loop registers on `SERVE_WAITQ` and is unparked the instant
        // a request is posted (Design D D2b-2c; `AGENTS.md` §2.1 — a real
        // wake, never a busy-yield).
        crate::unlock_service::set_store_service_task(task_id);
    }
    let started = admitted.is_some();
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
        release_console0_to_login();
    }
    started
}

/// Bring up the bound bootstrap-floor block device and run the interactive
/// unlock policy.
///
/// Dispatches on which floor block driver [`crate::root_storage`] bound
/// (`AGENTS.md` §18.6): [`VIRTIO_BLK_PATH`] over the production device-IRQ
/// path, or [`EMMC2_PATH`] (the Raspberry Pi 4 SD host) over programmed
/// I/O. The bring-up differs per device; the read-only `/System` autoload,
/// the passphrase prompt, and the interactive unlock are shared in
/// [`finish_unlock`] (`AGENTS.md` §2.2). A bound driver that is neither
/// fails closed (`AGENTS.md` §2.9 / §18.4).
///
/// **On success this never returns:** [`finish_unlock`] logs the outcome,
/// releases the console-0 gate, and parks the kthread for life as the
/// persistent driver-store service (Design D D2a-2), so the [`Infallible`]
/// `Ok` is never produced. Only an early bring-up failure returns `Err` with
/// a stable stage string; on that path the caller logs it and opens the
/// console-0 gate (`AGENTS.md` §2.9).
fn run_unlock(
    yielder: &mut dyn YieldHandle,
    binding: &RootBlockBinding,
    dtb: u64,
    frames: &'static FrameAllocator,
    caps: CapabilitySet,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    // Move the kthread's single yield handle into the shared cell both the
    // re-arming IRQ waiter and the cooperative console reader suspend
    // through (`AGENTS.md` §2.2 — one cooperative-yield definition).
    let coop = CooperativeYield::new(yielder);

    // The bus-driver task capability context: the unlock kthread's caps,
    // owner `UNLOCK_TASK`, audited onto the service's audit sink. Both the
    // virtio and the EMMC2 register-window maps gate on its `CAP_MMIO_MAP`
    // (`AGENTS.md` §5.4). Leaked to `'static` because the brought-up device
    // host borrows it for the life of the (now `'static`, shared) disk
    // (`AGENTS.md` §4 — kernel state is never freed); a single boot-time leak,
    // like `boot_tree_snapshot`.
    let caller: &'static TaskCapabilities = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        TaskCapabilities::derive(UNLOCK_TASK, UserId(0), caps, caps, env.audit),
    ));

    match binding.driver_path {
        VIRTIO_BLK_PATH => virtio_blk_unlock(&coop, caller, dtb, frames, env),
        EMMC2_PATH => emmc2_unlock(&coop, caller, binding, env),
        _ => Err("root-unlock: bound block driver is not a known floor driver"),
    }
}

/// The `'static` boot environment a root-unlock bring-up threads through:
/// the init-spawn context (the per-arch driver-spawn seam), the audit sink,
/// and the discovered hardware tree the pre-unlock autoload matches against.
///
/// Grouped because all three travel together from the kthread body through
/// the per-device bring-up into the shared [`finish_unlock`] tail; passing
/// one cohesive `Copy` value rather than re-listing three `'static`
/// references in every signature keeps the seams readable and below the
/// argument-count bar (`AGENTS.md` §2.2).
#[derive(Clone, Copy)]
struct UnlockEnv {
    ctx: &'static (dyn InitSpawnCtx + Sync),
    audit: &'static (dyn Sink + Sync),
    tree: &'static [HwNode],
}

/// Bring the virtio-blk root device up over the production device-IRQ path
/// and hand it to the shared [`finish_unlock`] tail (the QEMU `virt` /
/// x86_64 root, proven on `-M virt`).
fn virtio_blk_unlock<'a>(
    coop: &'a CooperativeYield<'a>,
    caller: &'static TaskCapabilities,
    dtb: u64,
    frames: &'static FrameAllocator,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    let audit = env.audit;
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

    // Build the `virt`-board virtio-MMIO bus and provision the block
    // transport through the `CAP_MMIO_MAP`-gated kernel mapper.
    // SAFETY: the virtio-MMIO aperture the device tree describes is
    // identity-mapped Device memory the bus alone reads.
    let bus =
        unsafe { virtio_mmio_bus_from_dtb(dtb_bytes) }.map_err(|_| "root-unlock: virtio bus")?;
    // The device backing is boot-leaked to `'static`: the brought-up disk is
    // shared for the life of the system by two independent preemptive tasks
    // (the driver-store serve task and the encrypted-root unlock task, see
    // `finish_unlock`), so its backing must outlive both frames. Leaking is
    // the sanctioned "kernel state is never freed" pattern
    // (`kernel/core/src/spawn.rs`; cf. `crate::unlock_service::boot_tree_snapshot`)
    // and uses only safe `Box::leak`, never an `unsafe` lifetime cast
    // (`AGENTS.md` §4).
    let phys: &'static DirectPhysMap = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        DirectPhysMap::identity(identity_limit()),
    ));
    let gib = configured_identity_gigapages();
    // Two throwaway *bookkeeping* page tables (device access is via the
    // boot identity map through `phys`): one for the MMIO window map, one
    // for the DMA pool. Each identity-maps the boot window so the
    // bookkeeping tables themselves are reachable; the window/pool VAs sit
    // far above it so they never collide with an identity block.
    let mmio_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, gib)
        .ok_or("root-unlock: mmio bookkeeping space")?;
    let mmio: &'static mut MmioMap<'static, _> = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        MmioMap::new(
            AddressSpace::new(mmio_space),
            VirtAddr::new(MMIO_VBASE),
            MMIO_CAP_PAGES,
            phys,
        )
        .map_err(|_| "root-unlock: mmio map")?,
    ));
    let (transport, slot_base) = {
        let mapper = KernelMmioMapper::new(mmio, caller, audit);
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
        phys,
    )
    .map_err(|_| "root-unlock: dma pool")?;
    let waiter: &'static RearmingIrqWaiter =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(RearmingIrqWaiter {
            table,
            handle,
            line: intid,
        }));
    let vhost: &'static KernelVirtioHost<'static, _, dyn Sink + Sync> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(KernelVirtioHost::new(
            pool,
            caller,
            audit,
            PoolId::fresh(),
            table,
            handle,
            waiter,
        )));

    // Admit the virtio-blk driver through the signed §8 load gate (Ed25519
    // signature + `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) before it drives
    // hardware — a refusal fails closed (`AGENTS.md` §5.4 / §23.1).
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(VIRTIO_BLK_PATH, &loader_caps())
        .map_err(|_| "root-unlock: virtio-blk refused at the signed load gate")?;

    // Open the whole-disk block device over the provisioned transport. Every
    // borrowed backing (`transport`/`vhost`/`mmio`/`pool`/`waiter`/`phys`) is
    // `'static`, so the opened device is `VirtioBlk<'static>` and can be
    // shared for life behind the block-sharing layer (`finish_unlock`).
    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "root-unlock: virtio-blk open")?;
    finish_unlock(blk, coop, env)
}

/// Bring the Raspberry Pi 4 EMMC2 SD host up over programmed I/O and hand
/// it to the shared [`finish_unlock`] tail (`plans/PI.md` P8/B4).
///
/// EMMC2 transfers are programmed-I/O (the driver polls the SDHCI
/// buffer-data port), so — unlike the virtio path — there is no DMA pool
/// and no device interrupt to bind: the only resource is the SDHCI register
/// window, which is the matched node's sole register-window grant
/// (`AGENTS.md` §18.3) and never a board constant (`AGENTS.md` §18.1 /
/// §2.20). The window is mapped under `CAP_MMIO_MAP` through the kernel
/// mapper and the driver brought up over it; `raspi4b` cannot model EMMC2
/// (`plans/PI.md` §0.4), so this path is host-tested at the driver level
/// and metal-gated here.
fn emmc2_unlock<'a>(
    coop: &'a CooperativeYield<'a>,
    caller: &'static TaskCapabilities,
    binding: &RootBlockBinding,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    let audit = env.audit;
    // Admit the EMMC2 driver through the signed §8 load gate before it
    // drives hardware — a refusal fails closed (`AGENTS.md` §5.4 / §23.1 /
    // §18.6).
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(EMMC2_PATH, &loader_caps())
        .map_err(|_| "root-unlock: emmc2 refused at the signed load gate")?;

    // The SDHCI register window the matched node requested. `sole_register_window`
    // fails closed on a missing or ambiguous window (`AGENTS.md` §2.9) rather
    // than guessing an address.
    let (regs_phys, _len) = sole_register_window(binding.node.resources())
        .map_err(|_| "root-unlock: emmc2 register window")?;

    // A throwaway *bookkeeping* page table for the register-window map
    // (device access is via the boot identity map through `phys`; the window
    // VA sits far above the identity window so it never collides with a
    // gigapage block). Boot-leaked to `'static` (safe `Box::leak`) like the
    // virtio path: the brought-up disk is shared for life by the two
    // independent tasks `finish_unlock` runs, so the `Emmc2`'s window backing
    // must outlive both frames (`AGENTS.md` §4 — kernel state is never freed).
    let phys: &'static DirectPhysMap = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        DirectPhysMap::identity(identity_limit()),
    ));
    let gib = configured_identity_gigapages();
    let mmio_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, gib)
        .ok_or("root-unlock: mmio bookkeeping space")?;
    let mmio: &'static mut MmioMap<'static, _> = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        MmioMap::new(
            AddressSpace::new(mmio_space),
            VirtAddr::new(MMIO_VBASE),
            MMIO_CAP_PAGES,
            phys,
        )
        .map_err(|_| "root-unlock: mmio map")?,
    ));

    // Map the window under `CAP_MMIO_MAP` and bring the card online. The
    // opened `Emmc2` retains a `RegisterWindow` pointing into the leaked
    // `'static` `mmio` window backing, so it stays valid for life. The
    // mapper/host borrow of `mmio` ends with this block.
    let blk = {
        let mapper = KernelMmioMapper::new(mmio, caller, audit);
        let host = Emmc2Host::new(*caller.effective(), &mapper);
        rustos_drv_storage_emmc2::wiring::open_discovered(&host, regs_phys).map_err(|fault| {
            // `raspi4b` cannot model EMMC2 (`plans/PI.md` §0.4), so the metal
            // UART log is the only signal that localises an SD bring-up
            // failure: record which identification step the card stalled at
            // *and* how it failed, so a controller/command fault is told
            // apart from a decode rejection at the same step (e.g. CMD9
            // `SEND_CSD` timing out vs. returning an unsupported CSD)
            // (`AGENTS.md` §19.4 / §2.16 — measure, do not guess).
            note_stage(
                audit,
                Level::Error,
                "root-unlock: emmc2 open failed during SD bring-up",
                fault.stage.as_str(),
                driver_error_name(fault.error),
            );
            "root-unlock: emmc2 open"
        })?
    };
    finish_unlock(blk, coop, env)
}

/// The shared two-task tail both floor block bring-ups feed (`AGENTS.md`
/// §2.2), turning the one brought-up disk into a disk shared for life by two
/// independent preemptive tasks (Design D D2b-2c).
///
/// `blk` is the brought-up whole-disk [`Block`] device (virtio-blk or EMMC2),
/// already boot-leaked to `'static` by its bring-up. It is wrapped in a
/// leaked `&'static` [`DriverStoreService`] (over the [`SharedBlock`] layer),
/// so two tasks reach it through independent serialised windows
/// (`AGENTS.md` §4):
///
/// * A **separate, spawned** preemptive task runs the interactive
///   encrypted-root unlock against the *user-data* volume and, when it
///   resolves (installed or fail-closed), releases the console-0 gate to
///   `login` (`AGENTS.md` §5.4.5).
/// * **This** task becomes the persistent driver-store serve loop: it binds
///   and serves the capability-gated store IPC endpoint the user-space
///   `devmgr` loads signed `/System` drivers through (`AGENTS.md` §18.3 /
///   §18.4), real-parking on `SERVE_WAITQ` between requests, and never
///   returns on success.
///
/// Crucially the store endpoint binds **independently of** the user-data
/// passphrase (the signed driver store lives on the always-readable `/System`
/// volume, `plans/PI.md` design B), so the keyboard driver loads in user
/// space *before* the unlock prompt — no chicken-and-egg, and no cooperative
/// interleaving of the two on one kthread.
///
/// On success this never returns. Every fallible *setup* step fails closed
/// with a stable stage string the caller logs (`AGENTS.md` §2.9).
fn finish_unlock<B: Block + 'static>(
    blk: B,
    coop: &CooperativeYield<'_>,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    let UnlockEnv { ctx, audit, tree } = env;

    // The one brought-up disk, boot-leaked to `'static` behind the
    // block-sharing layer so two independent preemptive tasks drive it through
    // their own serialised windows (`AGENTS.md` §4): *this* task is the
    // driver-store serve loop (below), and a *separate* spawned task runs the
    // encrypted-root unlock. A geometry fault refuses the device fail-closed
    // (`AGENTS.md` §2.9).
    let store: &'static DriverStoreService<B> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(DriverStoreService::new(
            SharedBlock::new(blk).map_err(|_| "root-unlock: block device geometry")?,
        )));

    // Spawn the encrypted-root unlock as its own preemptive task. The
    // user-data volume's passphrase is independent of the always-readable
    // `/System` driver store (`plans/PI.md` design B), so the store endpoint
    // (bound + served by *this* task below) answers `devmgr` immediately —
    // the keyboard driver loads in user space *before* the prompt, with no
    // cooperative interleaving on one kthread (`AGENTS.md` §4 / §17.1 — two
    // independent tasks sharing the disk). The unlock task drives its own
    // console reader over its own scheduler yield handle.
    let unlock_body = move |yielder: &mut dyn YieldHandle| {
        let coop = CooperativeYield::new(yielder);
        // Primary console (index 0): the video console + its keyboard queue
        // when a framebuffer console is active, else the discovered UART. The
        // unlock reads the *raw* device directly (bypassing the console-0 gate
        // `login` reads through), wrapped in the kthread blocking reader.
        let (console_write, raw_read): (
            &'static dyn ConsoleWrite,
            &'static (dyn ConsoleRead + Sync + 'static),
        ) = if video::is_active() {
            (&VIDEO_CONSOLE, &VIDEO_KEYBOARD)
        } else {
            (&UART_CONSOLE, &UART_CONSOLE)
        };
        let reader = KthreadConsoleRead::new(raw_read, &coop);
        match unlock_root_disk_interactively(
            store.window(),
            console_write,
            &reader,
            &LATE_USERS_DB,
            audit,
        ) {
            UnlockOutcome::Installed => note(audit, Level::Info, USERS_DB_INSTALLED_MESSAGE),
            UnlockOutcome::GaveUp => note(
                audit,
                Level::Error,
                "root-unlock: gave up fail-closed; login refused until reboot",
            ),
        }
        // The unlock is resolved (installed or fail-closed), so release
        // console 0 to `login`: the byte-contention window is over
        // (`plans/PI.md` P11 item 5). A failed unlock installs no users
        // database, so `login` still refuses every attempt (`AGENTS.md`
        // §5.4.5). The unlock task then ends (the disk stays alive — it is
        // `'static`-leaked — and this task's window borrow ends with it).
    };
    if ctx
        .spawn_kernel_service(alloc::boxed::Box::new(unlock_body))
        .is_none()
    {
        // The unlock task could not be admitted: nothing will prompt for the
        // passphrase or open the console-0 gate, so open it here (login still
        // refuses, as no database is installed) and serve the store anyway so
        // `devmgr` can load drivers (`AGENTS.md` §2.9).
        note(
            audit,
            Level::Error,
            "root-unlock: unlock task not admitted; console gate opened, store still served",
        );
        release_console0_to_login();
    }

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
    // the one per-arch input the otherwise arch-neutral driver-store load op
    // needs to spawn a verified driver into its own process.
    let driver_spawn = InitCtxDriverProcessSpawn::new(ctx, &AARCH64_PROCESS_SPAWN);
    // The kernel-side load mechanism the persistent driver-store service
    // keeps in its trusted base (Design D D2b-2c): the driver-signing trust
    // anchor, the delegatable `autoload_caps` gate superset (`CAP_DRV_LOAD`
    // to pass the §8 gate plus the resource caps an autoloaded driver's class
    // may request — including `CAP_INPUT_INJECT` for an input driver,
    // intersected per driver with its signed manifest request, `AGENTS.md`
    // §5.2 / §18.3), the aarch64 process-spawn seam, and the discovered tree a
    // matched `node_id` is resolved against to mint exactly that node's
    // grants (§4 — no ambient authority). The user-space `devmgr` owns the
    // matching *policy*; this kthread serves the *mechanism* over the
    // capability-gated store endpoint below.
    let serve_ctx = crate::driver_store_server::StoreServeContext {
        trusted: &trusted,
        caps: autoload_caps(),
        spawn: &driver_spawn,
        tree,
    };

    // This task is now the persistent driver-store service: it binds and
    // serves the capability-gated store IPC endpoint the user-space `devmgr`
    // reads the signed `/System` driver store through (`AGENTS.md` §18.3 /
    // §18.4), real-parking on `SERVE_WAITQ` between requests. It serves over
    // its own `/System` window onto the `'static`-leaked shared disk,
    // independently of the encrypted-root unlock task spawned above
    // (`plans/PI.md` design B). `login`, PID 1, `devmgr`, the unlock task, and
    // every other task run on their own tasks.
    //
    // The binder context holds only `IPC_BIND_PRIVILEGED` (the privileged
    // authority to bind the restricted-sender store endpoint, §5.2), distinct
    // from the kthread's own minimal `service_caps` (§5.4 — no ambient
    // authority).
    let binder = TaskCapabilities::derive(
        UNLOCK_TASK,
        UserId(0),
        store_endpoint_binder_caps(),
        store_endpoint_binder_caps(),
        audit,
    );
    // The persistent `/System` window is taken in an inner scope so that, on
    // a fail-closed fallback, the window borrow of `store` ends before the
    // `store.hold` park. On the success path `serve_system_store` never
    // returns, so the window stays borrowed for the life of the system.
    let outcome = {
        let mut window = store.window();
        crate::root_mount::with_system_volume(&mut window, audit, |volume| {
            crate::driver_store_server::serve_system_store(volume, &serve_ctx, &binder, coop, audit)
        })
    };
    match outcome {
        // The serve loop never returns on success (`Infallible`).
        Some(Ok(never)) => match never {},
        // The endpoint could not be bound (e.g. its well-known id is already
        // registered, or the mount became unreadable). Fail closed
        // (`AGENTS.md` §2.9): log the stage and park the kthread for life
        // still owning the disk, so an `ipc_call` to the unbound store
        // endpoint fails closed with `NotFound` rather than blocking.
        Some(Err(stage)) => {
            note(audit, Level::Error, stage);
            store.hold(coop)
        }
        // No read-only `/System` volume on this disk (already audited
        // `SYSTEM_VOLUME_UNAVAILABLE`): nothing to serve. Park for life
        // owning the disk; `devmgr`'s store reads fail closed with
        // `NotFound` (`AGENTS.md` §18.4 / §2.9).
        None => {
            note(
                audit,
                Level::Error,
                "driver-store: no /System volume to serve; driver-store endpoint not bound",
            );
            store.hold(coop)
        }
    }
}

/// A minimal in-kernel [`DriverHost`] exposing only a capability-gated
/// [`MmioMapper`] — the host the bootstrap-floor EMMC2 SD driver is brought
/// up over.
///
/// EMMC2 is programmed-I/O, so it needs no virtio/DMA host: the only host
/// service it uses is [`MmioMapper::map_window`] for its SDHCI register
/// block. Every map is re-checked kernel-side against `caps` by the wrapped
/// [`KernelMmioMapper`] (`AGENTS.md` §5.4), so the host cannot widen its own
/// authority. Kept local to this bring-up rather than generalised, since it
/// is the only in-kernel MMIO-only host today (`AGENTS.md` §2.3 / §15.5).
struct Emmc2Host<'a> {
    caps: CapabilitySet,
    mmio: &'a dyn MmioMapper,
}

impl<'a> Emmc2Host<'a> {
    /// Build the host over the floor driver's `caps` and the kernel's `mmio`
    /// mapper.
    fn new(caps: CapabilitySet, mmio: &'a dyn MmioMapper) -> Self {
        Self { caps, mmio }
    }
}

impl DriverHost for Emmc2Host<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        // The driver runs inside the kernel image as a bootstrap-floor block
        // driver (`AGENTS.md` §18.6 — below the signed store it reads).
        DriverKind::InKernel
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mmio)
    }
}

/// The production aarch64 identity-map extent the DMA/MMIO physical map
/// reaches frames and device windows through: the configured number of
/// identity-mapped gigapages (`plans/PI.md` P6), so the map matches the
/// boot path's own identity extent rather than a fixed guess
/// (`AGENTS.md` §24.1).
fn identity_limit() -> u64 {
    (configured_identity_gigapages() as u64) << 30
}

/// A stable, terse name for the `DriverError` a floor block bring-up
/// failed with, for the `error=` field of the `EventId(4139)` audit line.
///
/// Pairs with the `BringUpStage` name so the metal UART log distinguishes
/// *how* a step failed — a controller/command fault (`DeviceFault`) from a
/// decode rejection (`Unsupported`) at the same step (e.g. CMD9 `SEND_CSD`
/// timing out vs. returning a CSD the driver does not support) — which
/// `raspi4b` cannot reveal (`plans/PI.md` §0.4 / P8 / B4). Fails closed on an
/// unforeseen variant (`DriverError` is `#[non_exhaustive]`) with a generic
/// name rather than asserting (`AGENTS.md` §2.9).
fn driver_error_name(error: rustos_abi::DriverError) -> &'static str {
    use rustos_abi::DriverError;
    match error {
        DriverError::Unsupported => "unsupported",
        DriverError::DeviceFault => "device fault",
        DriverError::PermissionDenied => "permission denied",
        DriverError::NotFound => "not found",
        DriverError::BufferTooSmall => "buffer too small",
        DriverError::LengthOutOfRange => "length out of range",
        _ => "driver error",
    }
}
