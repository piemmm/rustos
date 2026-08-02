//! The riscv64 (QEMU `virt` / SiFive) live root-unlock bring-up
//! (`plans/NETWORK.md` N4e-riscv64).
//!
//! The freestanding-riscv64 half of the in-kernel root-unlock service, the
//! cross-port sibling of [`crate::aarch64::root_unlock`]. It lives in the
//! architecture subtree because it names the riscv64 port directly (the PLIC
//! external-IRQ dispatch, the Sv39 paging primitives, the firmware device
//! tree); the device- and architecture-independent two-task tail — the
//! spawned interactive unlock and the persistent driver-store serve loop —
//! stays in the shared [`crate::unlock_orchestrate::finish_unlock`].
//!
//! It admits the in-kernel unlock kthread at the init seam, brings the
//! bootstrap virtio-blk root device up over the production PLIC device-IRQ
//! path ([`crate::riscv64::irq`]), and hands the opened disk to the shared
//! tail. The `virt` board's only floor block driver is virtio-blk (the
//! aarch64 EMMC2 SD path has no riscv64 analogue), so there is one bring-up.

use core::convert::Infallible;

use tairix_abi::driver::dma::PoolId;
use tairix_abi::IrqHandle;
use tairix_arch_riscv64::fdt::{plic_device_source, Fdt};
use tairix_arch_riscv64::paging::{AddressSpace as ArchAddressSpace, PageTablePool};
use tairix_arch_riscv64::{trap, SERIAL_SINK};
use tairix_caps::CapabilitySet;
use tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb;
use tairix_drv_bus_virtio::MmioTransport;
use tairix_drv_storage_virtio_blk::{VirtioBlk, VIRTIO_BLK_DEVICE_ID};
use tairix_kernel_core::{
    ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, IrqParkWaiter, YieldHandle,
    NULL_CONSOLE_READ,
};
use tairix_kernel_irq::{IrqController, IrqTable};
use tairix_kernel_mem::{AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, MmioMap, VirtAddr};
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::{provision_virtio_mmio, KernelMmioMapper, KernelVirtioHost};
use tairix_log::{Level, Sink};
use tairix_reclaim::MemoryPressure;

use crate::driver_catalog::VIRTIO_BLK_PATH;
use crate::driver_loader::KernelDriverLoader;
use crate::riscv64::boot::IDENTITY_GIGABYTES;
use crate::riscv64::irq::{plic_controller, published_irq_table};
use crate::root_storage::RootBlockBinding;
use crate::unlock_orchestrate::{finish_unlock, UnlockConsole, UnlockEnv};
use crate::unlock_service::{
    loader_caps, note, service_caps, take_boot, CONSOLE0_GATE, UNLOCK_TASK,
};

/// Per-device DMA window capacity, in pages, the virtio-blk driver allocates
/// its request/data buffers from (transient per-request DMA).
const POOL_PAGES: usize = 64;

/// Capacity, in pages, of the MMIO register-window map.
const MMIO_CAP_PAGES: usize = 64;

/// Identity extent, in GiB, the throwaway bookkeeping MMIO/DMA Sv39 spaces
/// map. Their identity coverage is irrelevant to reachability — their tables
/// are written through the live boot MMU, and device access is via the
/// identity [`DirectPhysMap`] — so this need only leave the window bases
/// (below) above it; 4 GiB comfortably covers the kernel image + low RAM.
const BOOKKEEPING_GIB: usize = 4;

/// Bookkeeping virtual base of the MMIO register-window map. Above
/// [`BOOKKEEPING_GIB`] and inside the Sv39 canonical lower half (`< 256 GiB`)
/// so it never collides with an identity gigapage. Pure bookkeeping — the
/// driver reaches the window through the identity map.
const MMIO_VBASE: u64 = 64 << 30;

/// Bookkeeping virtual base of the minted per-driver DMA window (see
/// [`MMIO_VBASE`]).
const POOL_VBASE: u64 = 128 << 30;

/// The page-table frame pool the two throwaway bookkeeping Sv39 spaces (the
/// MMIO map and the DMA pool) draw their root + intermediate tables from.
/// Private to the unlock service so it never contends with the boot/init
/// pools. The spaces are never made live; the pool only backs the
/// guard-bracketed window accounting `kernel/mem` performs.
static UNLOCK_PT_POOL: PageTablePool = PageTablePool::new();

/// The production riscv64 identity-map extent the device physical map reaches
/// frames and register windows through — the same `[0, IDENTITY_GIGABYTES GiB)`
/// window the live boot MMU maps, so the map matches the boot identity extent
/// rather than a fresh guess.
fn identity_limit() -> u64 {
    (IDENTITY_GIGABYTES as u64) << 30
}

/// Race-free hart park for a device wait whose context cannot be
/// scheduler-parked — the boot kthreads bringing the disk up and serving the
/// driver store ([`IrqParkWaiter`]'s fallback).
///
/// Mask S-mode interrupt *taking* (`sstatus.SIE`), re-check the line's ready
/// flag, `wfi` only if still not ready, then unmask so the woken completion is
/// taken by the trap vector and dispatched into `IrqTable::fire`. Masking
/// makes a completion landing in the check-park window *pending* (not taken)
/// until `wfi` is entered, so no edge is lost. During the boot root-unlock
/// everything else is parked waiting on this work, so briefly halting the hart
/// starves nothing; every steady-state wait comes from a task's syscall
/// context, which the shared waiter parks off the run queue instead.
fn wfi_fallback_park(table: &IrqTable, handle: IrqHandle) {
    // SAFETY: the S-mode trap vector is installed (boot `enable_mmu_and_vectors`)
    // and the production PLIC dispatch is published (`irq::install_dispatch`),
    // so a woken external interrupt is handled rather than faulting; the calls
    // only toggle `sstatus.SIE` and issue the `wfi` hint.
    unsafe {
        trap::set_supervisor_interrupts(false);
        if !table.ready_for(handle) {
            trap::wait_for_interrupt();
        }
        trap::set_supervisor_interrupts(true);
    }
}

/// Release console 0 to `login` and mark the users-database source resolved.
///
/// Both mean "the unlock window is over, `login` may take the console now", so
/// they flip together. Opening the gate lets `login`'s gated console reads
/// through; resolving the late users-database flips a `login` parked on the
/// pending `users_db_read` into its prompt — against the installed database if
/// the unlock succeeded, else fail-closed deny-all. No receive-interrupt arm:
/// the SBI console exposes no interrupt-driven input this slice, so `login`
/// fails closed on fd 0 (a real interactive console-input path is a separate,
/// later tranche).
fn release_console0_to_login() {
    CONSOLE0_GATE.open();
    // Nudge the console wait-queue so any `login` already parked on the
    // (until now) withheld console-0 read re-polls the now-open gate; a no-op
    // before the wait-queue arch hook is installed.
    tairix_kernel_core::console_wake();
    crate::root_mount::LATE_USERS_DB.resolve();
}

/// The riscv64 console-0 seam the shared root-unlock orchestration reaches the
/// primary console through.
///
/// The SBI console is the primary console: its write half streams the
/// passphrase prompt, and its read half is the fail-closed
/// [`NULL_CONSOLE_READ`] (the SBI legacy console exposes no non-blocking input
/// drain), so `unlock_root_disk_interactively`'s first passphrase read returns
/// an error and the unlock gives up fail-closed at once — never a reader parked
/// forever on input that cannot arrive (which would deadlock `login`). Login is
/// then refused (the correct secure default): the users database resolves
/// deny-all and the console-0 gate opens.
struct RiscvUnlockConsole;

/// The single `'static` [`RiscvUnlockConsole`] the bring-up hands the shared
/// orchestration.
static RISCV_UNLOCK_CONSOLE: RiscvUnlockConsole = RiscvUnlockConsole;

impl UnlockConsole for RiscvUnlockConsole {
    fn acquire_console0(
        &self,
    ) -> (
        &'static dyn ConsoleWrite,
        &'static (dyn ConsoleRead + Sync + 'static),
    ) {
        let write: &'static dyn ConsoleWrite = &crate::riscv64::boot::RISCV_UART_CONSOLE;
        let read: &'static (dyn ConsoleRead + Sync + 'static) = &NULL_CONSOLE_READ;
        (write, read)
    }

    fn release_console0_to_login(&self) {
        release_console0_to_login();
    }
}

/// Admit the in-kernel root-unlock kthread if the boot path bound a virtio-blk
/// root block device, returning whether it was started.
///
/// With no binding (headless / no disk / ambiguous), a non-virtio-blk binding
/// (there is no other floor block driver on the `virt` board), or no `'static`
/// frame allocator, it starts nothing, opens the console-0 gate so `login`
/// proceeds (and fails closed, as no database is installed), and returns
/// `false`. The console-0 gate is also opened by the kthread body once the
/// unlock resolves, so it is never left latched closed.
#[must_use]
pub fn spawn_if_present(ctx: &'static (dyn InitSpawnCtx + Sync)) -> bool {
    let boot = take_boot();
    // Route the unlock service's security-relevant decisions onto the boot
    // audit channel when the init seam wired a `'static` audit sink; fall back
    // to the SBI serial log otherwise. The kthread body and the unlock policy
    // share it.
    let audit: &'static (dyn Sink + Sync) = ctx.static_audit().unwrap_or(&SERIAL_SINK);
    let Some(binding) = boot.binding else {
        note(
            audit,
            Level::Info,
            "root-unlock: no root block device bound; root unbound, login refuses",
        );
        release_console0_to_login();
        // No disk means no on-disk application store this boot: resolve the
        // readiness latch so a store-bundle spawn fails closed instead of
        // parking forever.
        crate::app_store::APP_STORE.note_unavailable();
        return false;
    };
    if binding.driver_path != VIRTIO_BLK_PATH {
        // The bound driver is not the one bootstrap-floor block driver this
        // seam knows how to bring up (virtio-blk). Fail closed rather than
        // guess at a bring-up. `root_storage` only ever binds a
        // `provides_root_block` floor driver, so reaching here is a packaging
        // defect, not an expected path.
        note(
            audit,
            Level::Error,
            "root-unlock: bound block driver is not a known floor driver; root unbound",
        );
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
        return false;
    }
    let Some(frames) = ctx.static_frames() else {
        note(
            audit,
            Level::Error,
            "root-unlock: no kernel frame allocator; root unbound, login refuses",
        );
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
        return false;
    };

    let dtb = boot.dtb;
    let caps = service_caps();
    // The system memory-pressure gauge every mounted volume's cache samples,
    // over the same `'static` frame allocator the spawn path uses — physical
    // free frames are the authoritative reading. Fetched from the
    // memory-statistics registry so this boot path, every cache, and the
    // System Information export all share the one gauge.
    let pressure: &'static MemoryPressure =
        tairix_kernel_core::memstats::MEM_STATS.system_pressure(frames);
    let env = UnlockEnv {
        ctx,
        audit,
        pressure,
    };
    let body = move |yielder: &mut dyn YieldHandle| {
        // On success the root-unlock service never returns: it parks for life
        // as the persistent driver-store service, having already logged the
        // unlock outcome and released console 0. Only an early bring-up failure
        // returns here — and because the success arm is the uninhabited
        // [`Infallible`], the `Err` binding is irrefutable. Fail closed: log
        // the stage and open the console-0 gate so `login` proceeds (it refuses
        // every attempt, as a failed unlock installs no database).
        let Err(stage) = run_unlock(yielder, &binding, dtb, frames, caps, env);
        note(audit, Level::Error, stage);
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
    };

    let admitted = ctx.spawn_kernel_service(alloc::boxed::Box::new(body));
    if let Some(task_id) = admitted {
        // Publish the disk-owning kthread's scheduler id so its driver-store
        // serve loop registers on `SERVE_WAITQ` and is unparked the instant a
        // request is posted (a real wake, never a busy-yield).
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
        // Admission failed: nothing will open the gate or publish the `/System`
        // mount, so do both here or console-0 `login` would park forever.
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
    }
    started
}

/// Bring up the bound virtio-blk root device and run the interactive unlock
/// policy.
///
/// **On success this never returns:** [`finish_unlock`] logs the outcome,
/// releases the console-0 gate, and parks the kthread for life as the
/// persistent driver-store service, so the [`Infallible`] `Ok` is never
/// produced. Only an early bring-up failure returns `Err` with a stable stage
/// string; on that path the caller logs it and opens the console-0 gate.
fn run_unlock(
    yielder: &mut dyn YieldHandle,
    binding: &RootBlockBinding,
    dtb: u64,
    frames: &'static FrameAllocator,
    caps: CapabilitySet,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    // Move the kthread's single yield handle into the shared cell both the
    // re-arming IRQ waiter and the cooperative console reader suspend through
    // (one cooperative-yield definition).
    let coop = CooperativeYield::new(yielder);

    // The bus-driver task capability context: the unlock kthread's caps, owner
    // `UNLOCK_TASK`, audited onto the service's audit sink; the virtio
    // register-window map gates on its `CAP_MMIO_MAP`. Leaked to `'static`
    // because the brought-up device host borrows it for the life of the (now
    // `'static`, shared) disk (kernel state is never freed).
    let caller: &'static TaskCapabilities = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        TaskCapabilities::derive(UNLOCK_TASK, UserId(0), caps, caps, env.audit),
    ));

    match binding.driver_path {
        VIRTIO_BLK_PATH => virtio_blk_unlock(&coop, caller, dtb, frames, env),
        _ => Err("root-unlock: bound block driver is not a known floor driver"),
    }
}

/// Bring the virtio-blk root device up over the production PLIC device-IRQ
/// path and hand it to the shared [`finish_unlock`] tail (the QEMU `virt`
/// root).
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
    // SAFETY: on the boot hand-off `dtb` is the firmware/OpenSBI device-tree
    // pointer (`a1`, preserved by the arch trampoline), identity-mapped and
    // immutable for the life of the kernel. `Fdt::from_ptr` validates the magic
    // and bounds the blob by its own `totalsize` before any read.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }
        .map_err(|_| "root-unlock: device tree unreadable; root unbound")?;
    let total = fdt.total_size();
    // SAFETY: `dtb`/`total` bound the same firmware blob `Fdt::from_ptr`
    // validated; it is identity-mapped, read-only, and outlives the kernel.
    let dtb_bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(dtb as *const u8, total) };

    // Build the `virt`-board virtio-MMIO bus and provision the block transport
    // through the `CAP_MMIO_MAP`-gated kernel mapper.
    // SAFETY: the virtio-MMIO aperture the device tree describes is
    // identity-mapped Device memory the bus alone reads (MMU on — `boot`
    // enabled it before `try_boot`).
    let bus =
        unsafe { virtio_mmio_bus_from_dtb(dtb_bytes) }.map_err(|_| "root-unlock: virtio bus")?;
    // The device backing is boot-leaked to `'static`: the brought-up disk is
    // shared for the life of the system by two independent preemptive tasks
    // (the driver-store serve task and the encrypted-root unlock task), so its
    // backing must outlive both frames. Safe `Box::leak`, never an `unsafe`
    // lifetime cast.
    let phys: &'static DirectPhysMap = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        DirectPhysMap::identity(identity_limit()),
    ));
    // Two throwaway bookkeeping Sv39 spaces (device access is via the boot
    // identity map through `phys`): one for the MMIO window map, one for the
    // DMA pool. Each identity-maps a small low extent; the window VAs sit above
    // it so they never collide with an identity gigapage.
    let mmio_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, BOOKKEEPING_GIB)
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

    // Resolve, bind, and arm the device's PLIC source on the table the kernel
    // core published (`irq::install_dispatch`, run in the core's `Irq` phase).
    // The S-mode external-interrupt dispatch is already installed, firing into
    // this table. `plic_device_source` reads the slot's single `interrupts`
    // cell — a discovered value, never a board constant.
    let source =
        plic_device_source(&fdt, slot_base).ok_or("root-unlock: no device interrupt in DTB")?;
    let table: &'static IrqTable =
        published_irq_table().ok_or("root-unlock: no published IRQ table")?;
    let controller = plic_controller().ok_or("root-unlock: no PLIC controller")?;
    let bind = table
        .bind(source, UNLOCK_TASK)
        .map_err(|_| "root-unlock: bind device source")?;
    let handle: IrqHandle = bind.handle;
    // Arm the source for the first completion (enable it in the S-mode context
    // and set its delivering priority); the waiter re-arms it after each
    // subsequent one through the controller's `rearm`.
    controller
        .arm(source)
        .map_err(|_| "root-unlock: arm PLIC source")?;

    // Mint the per-driver DMA host the driver allocates through, driven by the
    // shared parking waiter.
    let dma_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, BOOKKEEPING_GIB)
        .ok_or("root-unlock: dma bookkeeping space")?;
    let pool = DmaPool::new(
        AddressSpace::new(dma_space),
        VirtAddr::new(POOL_VBASE),
        POOL_PAGES,
        frames,
        phys,
    )
    .map_err(|_| "root-unlock: dma pool")?;
    let controller_dyn: &'static (dyn IrqController + Sync) = controller;
    let waiter: &'static IrqParkWaiter = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        IrqParkWaiter::new(table, handle, source, controller_dyn, wfi_fallback_park),
    ));
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

    // Admit the virtio-blk driver through the signed load gate (Ed25519
    // signature + `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) before it drives hardware
    // — a refusal fails closed.
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(VIRTIO_BLK_PATH, &loader_caps())
        .map_err(|_| "root-unlock: virtio-blk refused at the signed load gate")?;

    // Open the whole-disk block device over the provisioned transport. Every
    // borrowed backing is `'static`, so the opened device is `VirtioBlk<'static>`
    // and can be shared for life behind the block-sharing layer.
    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "root-unlock: virtio-blk open")?;
    finish_unlock(blk, coop, env, &RISCV_UNLOCK_CONSOLE)
}
