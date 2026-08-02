//! The aarch64 live root-unlock bring-up (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (2)).
//!
//! The freestanding-aarch64 half of the in-kernel root-unlock service: it
//! lives in the architecture subtree ([`crate::aarch64`]) because it names
//! the aarch64 port directly (`tairix_arch_aarch64`, the GIC, the firmware
//! device tree), while the device-independent core — the boot stash and the
//! console-0 ownership gate — stays in the arch-neutral
//! [`crate::unlock_service`].
//!
//! It admits the in-kernel unlock kthread at the init seam, brings the
//! bootstrap virtio-blk root device up over the production device-IRQ path
//! (INCREMENT (1)), and runs the device-independent unlock policy
//! ([`crate::root_mount::unlock_root_disk_interactively`]) inside the
//! kthread — opening the console-0 ownership gate the instant the unlock
//! resolves so `login` can take over
//! ([`crate::unlock_service::CONSOLE0_GATE`]).
//!
//! Two bootstrap-floor block drivers are brought up
//! here, selected by which one [`crate::root_storage`] bound: the virtio-blk
//! device over the production device-IRQ path (the QEMU `virt` / x86_64
//! root, proven on `-M virt`), or the Raspberry Pi 4 EMMC2 SD host over
//! cache-synchronized ADMA2 with a programmed-I/O fallback
//! ([`crate::driver_catalog::EMMC2_PATH`], the Pi-metal root
//! — `raspi4b` cannot model EMMC2, so it is host-tested at the driver level
//! and metal-gated here, `plans/PI.md` P8/B4). The bring-up differs per
//! device; the read-only `/System` autoload, the passphrase prompt, and the
//! interactive unlock are identical and shared in [`finish_unlock`]. A bound driver that is neither fails closed
//! (logged, gate opened, no database installed;).

use core::convert::Infallible;

use tairix_abi::driver::dma::{DmaHost, DmaSlab, PoolId};
use tairix_abi::driver::sole_register_window;
use tairix_abi::{CapabilityId, DriverError, DriverHost, DriverKind, IrqHandle, MmioMapper};
use tairix_arch_aarch64::fdt::gic_device_intid;
use tairix_arch_aarch64::kernel_arch::clean_invalidate_dcache_range;
use tairix_arch_aarch64::paging::{
    configured_identity_gigapages, AddressSpace as ArchAddressSpace, PageTablePool,
};
use tairix_arch_aarch64::{gic, video, SERIAL_SINK};
use tairix_caps::CapabilitySet;
use tairix_drv_bus_mmio::virtio_mmio_bus_from_dtb;
use tairix_drv_bus_virtio::MmioTransport;
use tairix_drv_storage_emmc2::{CompletionSignal, CompletionWait};
use tairix_drv_storage_virtio_blk::{VirtioBlk, VIRTIO_BLK_DEVICE_ID};
use tairix_fdt::Fdt;
use tairix_kernel_core::{
    ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, IrqParkWaiter, YieldHandle,
};
use tairix_kernel_irq::{IrqTable, WaitOutcome};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, MmioMap, PageTable, VirtAddr,
};
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_kernel_sec::dma::{alloc_dma, DmaGateError};
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::{provision_virtio_mmio, KernelMmioMapper, KernelVirtioHost};
use tairix_log::{Level, Sink};
use tairix_reclaim::MemoryPressure;
use tairix_sync::SpinLock;

use crate::aarch64::arch_wrapper::{
    UART_CONSOLE, UART_CONSOLE_READ, VIDEO_CONSOLE, VIDEO_KEYBOARD,
};
use crate::aarch64::gic_irq::{
    published_irq_table, COMPOSITE_IRQ_CONTROLLER, CPU0_TARGET, GIC_IRQ_CONTROLLER,
};
use crate::driver_catalog::{EMMC2_PATH, VIRTIO_BLK_PATH};
use crate::driver_loader::KernelDriverLoader;
use crate::root_mount::LATE_USERS_DB;
use crate::root_storage::RootBlockBinding;
use crate::unlock_orchestrate::{finish_unlock, UnlockConsole, UnlockEnv};
use crate::unlock_service::{
    loader_caps, note, note_stage, service_caps, take_boot, CONSOLE0_GATE, UNLOCK_TASK,
};

/// Per-device DMA window capacity, in pages, the virtio-blk driver
/// allocates its request/data buffers from (transient per-request DMA).
const POOL_PAGES: usize = 64;

/// Bookkeeping virtual base of the minted per-driver DMA window.
///
/// The driver reaches buffers through the identity map ([`DirectPhysMap`]),
/// so this address space is **pure bookkeeping**; the base is chosen far
/// above the boot identity window (which never exceeds a few GiB) so a
/// window mapping never collides with an identity gigapage block in the
/// throwaway bookkeeping space. Genuinely this bring-up's own constant.
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
/// (INCREMENT (1)) — no board constant. [`None`] when
/// no node matches or its `interrupts` specifier is unrepresentable
/// (fail closed).
pub(crate) fn device_spi(fdt: &Fdt<'_>, slot_base: u64) -> Option<u32> {
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

/// Find the GICv2 INTID of the console UART node — the same node
/// [`tairix_arch_aarch64::console::find_console`] selects (`arm,pl011`
/// preferred, the BCM2835 AUX mini-UART as fallback) — decoded through
/// [`gic_device_intid`], a discovered value and never a board constant.
///
/// [`None`] when no console node carries a representable `interrupts`
/// specifier (fail closed): the console then stays
/// on the polled path, and `login`'s reader keeps parking until the polled
/// re-check delivers — never an error.
pub(crate) fn console_spi(fdt: &Fdt<'_>) -> Option<u32> {
    let mut mini: Option<u32> = None;
    for node in fdt.nodes() {
        let node = node.ok()?;
        if node.is_compatible("arm,pl011") {
            return gic_device_intid(&node);
        }
        if node.is_compatible("brcm,bcm2835-aux-uart") && mini.is_none() {
            mini = gic_device_intid(&node);
        }
    }
    mini
}

/// Find the GICv2 INTID of the EMMC2 SD host node (`brcm,bcm2711-emmc2`,
/// the same node the hardware tree's Storage device is discovered from),
/// decoded through [`gic_device_intid`] — a discovered value, never a board
/// constant.
///
/// [`None`] when no EMMC2 node carries a representable `interrupts`
/// specifier (fail closed): the SD bring-up then
/// refuses rather than parking forever on a line that can never fire, since
/// the interrupt-driven driver depends on a bound completion line.
pub(crate) fn emmc2_spi(fdt: &Fdt<'_>) -> Option<u32> {
    for node in fdt.nodes() {
        let node = node.ok()?;
        if node.is_compatible("brcm,bcm2711-emmc2") {
            return gic_device_intid(&node);
        }
    }
    None
}

/// Find the GICv2 INTID of the BCM2711 PCIe root complex's internal **MSI
/// controller** — the shared SPI it raises when an endpoint behind the
/// bridge (the VL805 xHCI) sends a message-signalled interrupt — decoded
/// through [`gic_intid_from_cells`], a discovered value and never a board
/// constant.
///
/// The brcmstb PCIe binding lists two GIC interrupts on the
/// `brcm,bcm2711-pcie` node: the first is the root complex's own
/// (legacy-INTx aggregation), the **second** is the internal MSI
/// controller's shared line (Linux's `pcie-brcmstb.c` maps interrupt index
/// 1). Each specifier is the 3-cell `<type number flags>` GIC triple, so
/// the MSI entry's type/number cells sit at byte offsets 12 and 16.
///
/// [`None`] when the tree describes no `brcm,bcm2711-pcie` node, the node
/// carries fewer than two interrupt specifiers, or the MSI specifier is not
/// a GICv2 SPI/PPI this port can route (fail closed — `msi_alloc` then
/// reports no controller).
pub(crate) fn pcie_msi_spi(fdt: &Fdt<'_>) -> Option<u32> {
    use tairix_arch_aarch64::fdt::gic_intid_from_cells;
    for node in fdt.nodes() {
        let node = node.ok()?;
        if !node.is_compatible("brcm,bcm2711-pcie") {
            continue;
        }
        let interrupts = node.property("interrupts")?;
        // Second specifier: the MSI controller's shared SPI.
        let kind = interrupts.read_be_u32(12).ok()?;
        let number = interrupts.read_be_u32(16).ok()?;
        return gic_intid_from_cells(kind, number);
    }
    None
}

/// Race-free CPU park for a device wait whose context cannot be
/// scheduler-parked — the boot kthreads bringing the disk up and serving
/// the driver store ([`tairix_kernel_core::IrqParkWaiter`]'s fallback).
///
/// Mask IRQ *taking* ([`tairix_arch_aarch64::exceptions::mask_irq`]),
/// re-check the line's ready flag, `wfi`
/// ([`tairix_arch_aarch64::exceptions::wait_for_interrupt`]) only if still
/// not ready, then unmask ([`tairix_arch_aarch64::exceptions::enable_irq`])
/// so the woken completion is taken by the EL1 vector and dispatched into
/// `IrqTable::fire`. The sequence takes exactly one completion per wake and
/// loses no edge (a completion landing in the check-park window stays
/// pending and wakes the `wfi`). During the boot root-unlock everything
/// else is parked waiting on this work, so briefly halting the CPU here
/// starves nothing; every steady-state filesystem wait comes from a user
/// task's syscall context, which the shared waiter parks off the run queue
/// instead — the dispatch loop (and the buffered console drain) keeps
/// running for the whole device wait.
fn wfi_fallback_park(table: &IrqTable, handle: IrqHandle) {
    // SAFETY: the EL1 vector table is installed (boot
    // `exceptions::init_vectors`) and the production device dispatch is
    // published (the kernel-core `irq` phase), so the woken IRQ is
    // handled rather than faulting; the three calls only manipulate
    // `DAIF.I` and issue the `wfi` hint.
    unsafe {
        tairix_arch_aarch64::exceptions::mask_irq();
        if !table.ready_for(handle) {
            tairix_arch_aarch64::exceptions::wait_for_interrupt();
        }
        tairix_arch_aarch64::exceptions::enable_irq();
    }
}

/// Longest silence the EMMC2 completion wait tolerates before failing the
/// transfer closed, in nanoseconds.
///
/// The SDHCI controller signals every started operation — a completion or
/// an error status (its own data timeout included) — well inside this
/// budget, so a wait that elapses with no interrupt at all means the
/// controller or its interrupt routing is dead. The engine then surfaces
/// `DeviceFault` (a loud, typed error to the caller) instead of a task
/// parked forever holding the volume's lock.
const EMMC2_SILENCE_BUDGET_NS: u64 = 2_000_000_000;

/// The EMMC2 driver's completion seam ([`CompletionWait`]) over the same
/// shared parking waiter the virtio host uses
/// ([`tairix_kernel_core::IrqParkWaiter`] — one wait shape, no second park
/// implementation).
///
/// The SDHCI engine calls [`await_irq`](CompletionWait::await_irq) whenever
/// a command/transfer completion is outstanding; the waiter parks the
/// calling task off the run queue until the controller's ISR wakes it (or
/// takes the bounded `wfi` fallback in a boot-kthread context), so the
/// driver never busy-spins a status register and never halts the CPU under
/// a running system. The waiter owns only `'static` state, so the
/// completion is `'static` and the opened [`Emmc2`] it lives in can be
/// shared for life behind the block layer.
struct Emmc2Completion {
    waiter: IrqParkWaiter,
}

impl CompletionWait for Emmc2Completion {
    fn await_irq(&self) -> CompletionSignal {
        // The engine re-reads `INTERRUPT` on a fire, so a spurious wake is
        // harmless; every non-fire outcome (timeout, released binding,
        // aborted wait) fails the transfer closed.
        match self.waiter.park_wait(UNLOCK_TASK, EMMC2_SILENCE_BUDGET_NS) {
            WaitOutcome::Ready => CompletionSignal::Fired,
            WaitOutcome::TimedOut
            | WaitOutcome::NotFound
            | WaitOutcome::Quarantined
            | WaitOutcome::Aborted(_) => CompletionSignal::TimedOut,
        }
    }
}

/// Release console 0 to `login` and mark the users-database source
/// resolved.
///
/// The two always happen together — both mean "the unlock window is over,
/// `login` may take the console now" — so they are flipped through one
/// helper to keep them from diverging. Opening the gate
/// lets `login`'s gated console reads through ([`GatedConsoleRead`]);
/// [`LateUsersDb::resolve`](tairix_kernel_core::LateUsersDb::resolve) flips
/// a `login` parked on the pending (`WouldBlock`) `users_db_read` into its
/// prompt — against the installed database if the unlock succeeded
/// ([`install`](tairix_kernel_core::LateUsersDb) ran first and wins), else
/// fail-closed deny-all.
fn release_console0_to_login() {
    CONSOLE0_GATE.open();
    // Opening the gate is an input-availability edge for any `login` already
    // parked on the (until now) withheld console-0 read: nudge the console
    // wait-queue so it re-polls the now-open gate at once, draining any
    // type-ahead buffered in the console-0 queue during the closed window
    // rather than waiting for the next keystroke to push and wake it
    // (the wake that closes the gated-park race). A
    // no-op before the wait-queue arch hook is installed, and a spurious
    // wake for a reader on another console is harmless (it re-polls and
    // re-parks).
    tairix_kernel_core::console_wake();
    LATE_USERS_DB.resolve();
    // The passphrase poll is over: on a serial-only boot, switch the UART
    // console from polled to interrupt-driven so a `login` reader now parks
    // off the run queue and the receive interrupt wakes it. A no-op when the
    // boot path discovered no console interrupt (the console then stays on
    // the polled path). With an active video console the UART is the
    // session-free debug log line — no console is installed on it, so its
    // receive interrupt is left disabled rather than armed for a reader
    // that can never exist.
    if !video::is_active() {
        crate::aarch64::gic_irq::enable_uart_console_irq();
    }
}

/// The aarch64 console-0 seam the shared root-unlock orchestration
/// ([`crate::unlock_orchestrate`]) reaches the primary console through.
///
/// Selects the framebuffer video console when a display is active, else the
/// discovered UART — arming the UART's interrupt-driven receive so a keystroke
/// wakes the parked passphrase reader — and releases console 0 to `login`
/// through [`release_console0_to_login`] the instant the unlock resolves.
struct Aarch64UnlockConsole;

/// The single `'static` [`Aarch64UnlockConsole`] the bring-ups hand the shared
/// orchestration.
static AARCH64_UNLOCK_CONSOLE: Aarch64UnlockConsole = Aarch64UnlockConsole;

impl UnlockConsole for Aarch64UnlockConsole {
    fn acquire_console0(
        &self,
    ) -> (
        &'static dyn ConsoleWrite,
        &'static (dyn ConsoleRead + Sync + 'static),
    ) {
        // Primary console (index 0): the video console + its keyboard queue
        // when a framebuffer console is active, else the discovered UART. Both
        // input halves are interrupt-fed console queues (the keyboard
        // injection queue, or the UART's `UART_INPUT`-backed read half) whose
        // `push` wakes `CONSOLE_WAITQ`, so the kthread reader parks for input
        // rather than busy-polling a raw FIFO.
        if video::is_active() {
            let write: &'static dyn ConsoleWrite = &VIDEO_CONSOLE;
            let read: &'static (dyn ConsoleRead + Sync + 'static) = &VIDEO_KEYBOARD;
            (write, read)
        } else {
            // Switch the UART console to interrupt-driven receive for the
            // unlock window so a keystroke wakes the parked reader
            // (`console_wake`) — the RX ISR drains the FIFO into `UART_INPUT`,
            // which `UART_CONSOLE_READ` reads. Idempotent with the
            // `release_console0_to_login` handoff enable.
            crate::aarch64::gic_irq::enable_uart_console_irq();
            let write: &'static dyn ConsoleWrite = &UART_CONSOLE;
            let read: &'static (dyn ConsoleRead + Sync + 'static) = &UART_CONSOLE_READ;
            (write, read)
        }
    }

    fn release_console0_to_login(&self) {
        release_console0_to_login();
    }
}

/// Admit the in-kernel root-unlock kthread if the boot path bound a
/// virtio-blk root block device, returning whether it was started.
///
/// With no binding (headless / no disk / ambiguous), an EMMC2 binding (the
/// staged Pi metal path), or no `'static` frame allocator, it starts
/// nothing, opens the console-0 gate so `login` proceeds normally, and
/// returns `false` — failing closed. The
/// console-0 gate is also opened by the kthread body once the unlock
/// resolves, so it is never left latched closed.
#[must_use]
pub fn spawn_if_present(ctx: &'static (dyn InitSpawnCtx + Sync)) -> bool {
    let boot = take_boot();
    // Route the unlock service's security-relevant decisions (mount /
    // install / give-up) onto the boot audit channel
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
        // No disk means no on-disk application store this boot: resolve the
        // readiness latch so a store-bundle spawn fails closed instead of
        // parking forever.
        crate::app_store::APP_STORE.note_unavailable();
        return false;
    };
    if binding.driver_path != VIRTIO_BLK_PATH && binding.driver_path != EMMC2_PATH {
        // The bound driver is not one of the bootstrap-floor block drivers
        // this seam knows how to bring up (virtio-blk for the QEMU `virt` /
        // x86_64 root, EMMC2 for the Raspberry Pi 4 SD card). Fail closed —
        // open the gate and leave the root unmounted rather than guess at a
        // bring-up. `root_storage` only ever binds
        // a `provides_root_block` floor driver, so reaching here is a
        // packaging defect, not an expected path.
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
    // The system memory-pressure gauge every mounted volume's cache
    // samples (`plans/SMARTRAM.md` SMART2), over the same `'static`
    // frame allocator the spawn path uses — physical free frames are
    // the authoritative reading. Fetched from the memory-statistics
    // registry so this boot path, every cache, and the System
    // Information export all share the one gauge and its one
    // transition history.
    let pressure: &'static MemoryPressure =
        tairix_kernel_core::memstats::MEM_STATS.system_pressure(frames);
    let env = UnlockEnv {
        ctx,
        audit,
        pressure,
    };
    let body = move |yielder: &mut dyn YieldHandle| {
        // On success the root-unlock service never returns: it parks for life
        // as the persistent driver-store service (Design D D2a-2), having
        // already logged the unlock outcome and released console 0. Only an
        // early bring-up failure returns here — and because the success arm is
        // the uninhabited [`Infallible`], the `Err` binding is irrefutable.
        // Fail closed: log the stage and open the console-0
        // gate so `login` proceeds (it refuses every attempt, as a failed
        // unlock installs no database).
        let Err(stage) = run_unlock(yielder, &binding, dtb, frames, caps, env);
        note(audit, Level::Error, stage);
        release_console0_to_login();
        // The bring-up failed before the `/System` mount install could run:
        // resolve the application-store latch so a parked store-bundle
        // spawn wakes and fails closed (a no-op when the failure happened
        // after the install already resolved it).
        crate::app_store::APP_STORE.note_unavailable();
    };

    let admitted = ctx.spawn_kernel_service(alloc::boxed::Box::new(body));
    if let Some(task_id) = admitted {
        // Publish the disk-owning kthread's scheduler id so its driver-store
        // serve loop registers on `SERVE_WAITQ` and is unparked the instant
        // a request is posted (Design D D2b-2c; — a real
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
        // console-0 `login` would park forever — and nothing will publish
        // the `/System` mount, so resolve the application-store latch too.
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
    }
    started
}

/// Bring up the bound bootstrap-floor block device and run the interactive
/// unlock policy.
///
/// Dispatches on which floor block driver [`crate::root_storage`] bound: [`VIRTIO_BLK_PATH`] over the production device-IRQ
/// path, or [`EMMC2_PATH`] (the Raspberry Pi 4 SD host) over programmed
/// I/O. The bring-up differs per device; the read-only `/System` autoload,
/// the passphrase prompt, and the interactive unlock are shared in
/// [`finish_unlock`]. A bound driver that is neither
/// fails closed.
///
/// **On success this never returns:** [`finish_unlock`] logs the outcome,
/// releases the console-0 gate, and parks the kthread for life as the
/// persistent driver-store service (Design D D2a-2), so the [`Infallible`]
/// `Ok` is never produced. Only an early bring-up failure returns `Err` with
/// a stable stage string; on that path the caller logs it and opens the
/// console-0 gate.
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
    // through (one cooperative-yield definition).
    let coop = CooperativeYield::new(yielder);

    // The bus-driver task capability context: the unlock kthread's caps,
    // owner `UNLOCK_TASK`, audited onto the service's audit sink. Both the
    // virtio and the EMMC2 register-window maps gate on its `CAP_MMIO_MAP`. Leaked to `'static` because the brought-up device
    // host borrows it for the life of the (now `'static`, shared) disk
    // (kernel state is never freed); a single boot-time leak,
    // like `boot_tree_snapshot`.
    let caller: &'static TaskCapabilities = alloc::boxed::Box::leak(alloc::boxed::Box::new(
        TaskCapabilities::derive(UNLOCK_TASK, UserId(0), caps, caps, env.audit),
    ));

    match binding.driver_path {
        VIRTIO_BLK_PATH => virtio_blk_unlock(&coop, caller, dtb, frames, env),
        EMMC2_PATH => emmc2_unlock(&coop, caller, binding, dtb, frames, env),
        _ => Err("root-unlock: bound block driver is not a known floor driver"),
    }
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
    // (`kernel/core/src/spawn.rs`) and uses only safe `Box::leak`, never an
    // `unsafe` lifetime cast.
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
    let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);

    // Mint the per-driver DMA host the driver allocates through, driven by
    // the shared parking waiter.
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
    let waiter: &'static IrqParkWaiter =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(IrqParkWaiter::new(
            table,
            handle,
            intid,
            &COMPOSITE_IRQ_CONTROLLER,
            wfi_fallback_park,
        )));
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
    // hardware — a refusal fails closed.
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(VIRTIO_BLK_PATH, &loader_caps())
        .map_err(|_| "root-unlock: virtio-blk refused at the signed load gate")?;

    // Open the whole-disk block device over the provisioned transport. Every
    // borrowed backing (`transport`/`vhost`/`mmio`/`pool`/`waiter`/`phys`) is
    // `'static`, so the opened device is `VirtioBlk<'static>` and can be
    // shared for life behind the block-sharing layer (`finish_unlock`).
    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "root-unlock: virtio-blk open")?;
    finish_unlock(blk, coop, env, &AARCH64_UNLOCK_CONSOLE)
}

/// Bring the Raspberry Pi 4 EMMC2 SD host up over its interrupt-driven SDHCI
/// path and hand it to the shared [`finish_unlock`] tail (`plans/PI.md`
/// P8/B4).
///
/// Like the virtio path it wires a per-driver DMA pool so the SDHCI engine
/// moves transfers by ADMA2 (the fast path), and the controller's
/// completions are taken on its **bound GIC interrupt line**, never by
/// busy-spinning a status register. Three resources are therefore wired:
/// the SDHCI register window (the matched node's sole register-window
/// grant) is mapped under `CAP_MMIO_MAP` through the kernel mapper; a DMA
/// staging region is carved from a `CAP_MEM_DMA`-gated [`DmaPool`] through
/// the [`Emmc2DmaHost`]; and the controller's GIC SPI — discovered from the
/// firmware device tree ([`emmc2_spi`]), never a board constant — is bound,
/// routed, and armed on the published IRQ table so the driver blocks on
/// completion through the shared parking waiter ([`Emmc2Completion`]).
/// `raspi4b` cannot model EMMC2 (`plans/PI.md` §0.4), so this path is
/// host-tested at the driver level and metal-gated here.
fn emmc2_unlock<'a>(
    coop: &'a CooperativeYield<'a>,
    caller: &'static TaskCapabilities,
    binding: &RootBlockBinding,
    dtb: u64,
    frames: &'static FrameAllocator,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    let audit = env.audit;
    // Admit the EMMC2 driver through the signed load gate before it
    // drives hardware — a refusal fails closed.
    let loader = KernelDriverLoader::new(audit).ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(EMMC2_PATH, &loader_caps())
        .map_err(|_| "root-unlock: emmc2 refused at the signed load gate")?;

    // The SDHCI register window the matched node requested. `sole_register_window`
    // fails closed on a missing or ambiguous window rather
    // than guessing an address.
    let (regs_phys, _len) = sole_register_window(binding.node.resources())
        .map_err(|_| "root-unlock: emmc2 register window")?;

    // Resolve, bind, route, and arm the EMMC2 controller's GIC SPI on the
    // table the kernel core published (the same production device-IRQ path the
    // virtio bring-up uses). The driver parks on this line for every command
    // and block-transfer completion instead of busy-spinning a status
    // register; with no interrupt the driver would
    // park forever, so fail closed.
    if dtb == 0 {
        return Err("root-unlock: no device tree; emmc2 root unbound");
    }
    // SAFETY: on the boot hand-off `dtb` is the firmware/loader device-tree
    // pointer (`boot.s` preserves x0), identity-mapped and immutable for the
    // life of the kernel. `Fdt::from_ptr` validates the magic and bounds the
    // blob by its own `totalsize` before any read.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8) }
        .map_err(|_| "root-unlock: device tree unreadable; emmc2 root unbound")?;
    let intid = emmc2_spi(&fdt).ok_or("root-unlock: no emmc2 interrupt in DTB")?;
    let table: &'static IrqTable =
        published_irq_table().ok_or("root-unlock: no published IRQ table")?;
    let bind = table
        .bind(intid, UNLOCK_TASK)
        .map_err(|_| "root-unlock: bind emmc2 SPI")?;
    let handle: IrqHandle = bind.handle;
    // SAFETY: the GIC distributor + CPU interface are up (the kernel-core
    // `irq` phase brought them up via `gic_irq::install_device_irq_dispatch`
    // -> `gic::init`), and the EL1 vectors + device dispatch are installed;
    // this routes + enables the bound SPI on CPU 0.
    unsafe {
        gic::route_spi(intid, CPU0_TARGET);
    }
    // Arm the line for the first completion; the waiter re-arms it after each
    // subsequent one.
    let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
    // The completion seam the SDHCI engine blocks on, over the shared
    // parking waiter. Owns only `'static` state, so the opened `Emmc2` it
    // lives in is `'static` and shareable for life.
    let waiter = Emmc2Completion {
        waiter: IrqParkWaiter::new(
            table,
            handle,
            intid,
            &COMPOSITE_IRQ_CONTROLLER,
            wfi_fallback_park,
        ),
    };

    // A throwaway *bookkeeping* page table for the register-window map
    // (device access is via the boot identity map through `phys`; the window
    // VA sits far above the identity window so it never collides with a
    // gigapage block). Boot-leaked to `'static` (safe `Box::leak`) like the
    // virtio path: the brought-up disk is shared for life by the two
    // independent tasks `finish_unlock` runs, so the `Emmc2`'s window backing
    // must outlive both frames (kernel state is never freed).
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

    // Mint the per-driver DMA host the SDHCI engine carves its one ADMA2
    // staging region from (the fast path). A second throwaway *bookkeeping*
    // page table backs the DMA pool (device access is via the identity map
    // through `phys`). Boot-leaked to `'static` like the rest of the device
    // backing: the pool's frames must outlive the shared-for-life `Emmc2`,
    // and the slab it mints is itself leaked (kernel state is never freed).
    let dma_space = ArchAddressSpace::new_identity_gigapages(&UNLOCK_PT_POOL, gib)
        .ok_or("root-unlock: emmc2 dma bookkeeping space")?;
    let dma_pool = DmaPool::new(
        AddressSpace::new(dma_space),
        VirtAddr::new(POOL_VBASE),
        POOL_PAGES,
        frames,
        phys,
    )
    .map_err(|_| "root-unlock: emmc2 dma pool")?;
    let dma_host: &'static Emmc2DmaHost<'static, _, dyn Sink + Sync> = alloc::boxed::Box::leak(
        alloc::boxed::Box::new(Emmc2DmaHost::new(dma_pool, caller, audit, PoolId::fresh())),
    );

    // Map the window under `CAP_MMIO_MAP` and bring the card online over the
    // ADMA2 fast path (`CAP_MEM_DMA`-gated DMA host). The opened `Emmc2`
    // retains a `RegisterWindow` pointing into the leaked `'static` `mmio`
    // window backing and owns its DMA staging slab, so both stay valid for
    // life. The mapper/host borrow ends with this block.
    let blk = {
        let mapper = KernelMmioMapper::new(mmio, caller, audit);
        let host = Emmc2Host::new(*caller.effective(), &mapper, Some(dma_host));
        tairix_drv_storage_emmc2::wiring::open_discovered(&host, regs_phys, waiter).map_err(
            |fault| {
                // `raspi4b` cannot model EMMC2 (`plans/PI.md` §0.4), so the metal
                // UART log is the only signal that localises an SD bring-up
                // failure: record which identification step the card stalled at
                // *and* how it failed, so a controller/command fault is told
                // apart from a decode rejection at the same step (e.g. CMD9
                // `SEND_CSD` timing out vs. returning an unsupported CSD)
                // (measure, do not guess).
                note_stage(
                    audit,
                    Level::Error,
                    "root-unlock: emmc2 open failed during SD bring-up",
                    fault.stage.as_str(),
                    driver_error_name(fault.error),
                );
                "root-unlock: emmc2 open"
            },
        )?
    };
    finish_unlock(blk, coop, env, &AARCH64_UNLOCK_CONSOLE)
}

/// A minimal in-kernel [`DriverHost`] exposing a capability-gated
/// [`MmioMapper`] and, for the fast transfer path, a [`DmaHost`] — the host
/// the bootstrap-floor EMMC2 SD driver is brought up over.
///
/// The driver uses [`MmioMapper::map_window`] for its SDHCI register block
/// and, when present, [`DmaHost::alloc_dma_zeroed`] for the one device-
/// shared ADMA2 staging region it drives transfers through. Every map and
/// DMA carve is re-checked kernel-side against `caps` (by the wrapped
/// [`KernelMmioMapper`] and [`alloc_dma`]), so the host cannot widen its
/// own authority. Kept local to this bring-up rather than generalised,
/// since it is the only in-kernel MMIO/DMA host of this shape today.
struct Emmc2Host<'a> {
    caps: CapabilitySet,
    mmio: &'a dyn MmioMapper,
    dma: Option<&'a dyn DmaHost>,
}

impl<'a> Emmc2Host<'a> {
    /// Build the host over the floor driver's `caps`, the kernel's `mmio`
    /// mapper, and an optional `dma` host (the ADMA2 fast path; `None`
    /// leaves the driver on programmed I/O).
    fn new(caps: CapabilitySet, mmio: &'a dyn MmioMapper, dma: Option<&'a dyn DmaHost>) -> Self {
        Self { caps, mmio, dma }
    }
}

impl DriverHost for Emmc2Host<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.caps.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        // The driver runs inside the kernel image as a bootstrap-floor block
        // driver (below the signed store it reads).
        DriverKind::InKernel
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mmio)
    }

    fn dma_host(&self) -> Option<&dyn DmaHost> {
        self.dma
    }
}

/// A minimal boot-floor [`DmaHost`]: carves one coherent staging region for
/// the EMMC2 driver's ADMA2 transfers from a [`DmaPool`].
///
/// This is deliberately *not* the tracked/freeable
/// `KernelVirtioHost` DMA host: the root device is boot-leaked to `'static`
/// and lives for the whole life of the kernel (kernel state is never
/// freed), so the carve is minted with [`DmaSlab::from_leaked`] — no live
/// slab map and no free shim — and the host carries none of the virtio
/// interrupt machinery. `alloc_dma` re-checks `CAP_MEM_DMA` against
/// `caller` and audits every grant, so the host adds no authority.
struct Emmc2DmaHost<'a, P: PageTable, S: Sink + Sync + ?Sized> {
    /// The per-driver DMA pool, behind a [`SpinLock`] so the host is
    /// [`Sync`] (it is leaked `'static` like the rest of the device
    /// backing). Effectively uncontended — the boot bring-up carves once.
    pool: SpinLock<DmaPool<'a, P>>,
    caller: &'a TaskCapabilities,
    audit: &'a S,
    id: PoolId,
}

impl<'a, P: PageTable, S: Sink + Sync + ?Sized> Emmc2DmaHost<'a, P, S> {
    /// Take ownership of a [`DmaPool`] behind the capability-checking host.
    fn new(pool: DmaPool<'a, P>, caller: &'a TaskCapabilities, audit: &'a S, id: PoolId) -> Self {
        Self {
            pool: SpinLock::new(pool),
            caller,
            audit,
            id,
        }
    }
}

/// Synchronize cacheable EMMC2 DMA staging bytes with the non-coherent SD
/// host before or after a DMA ownership hand-off.
fn sync_emmc2_dma_range(base: *const u8, len: usize) {
    clean_invalidate_dcache_range(base as usize, len);
}

impl<P: PageTable, S: Sink + Sync + ?Sized> DmaHost for Emmc2DmaHost<'_, P, S> {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        if size == 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let mut pool = self.pool.lock();
        let buf = alloc_dma(&mut *pool, self.caller, size, self.audit).map_err(|e| match e {
            DmaGateError::CapabilityMissing => DriverError::PermissionDenied,
            // Pool exhaustion / oversize carve, and any future gate error:
            // fail closed. The driver then degrades to programmed I/O.
            _ => DriverError::LengthOutOfRange,
        })?;
        let base = pool
            .slot_base(&buf)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        let phys = buf.phys().as_u64();
        let len = buf.len();
        // SAFETY: `alloc_dma` carved exactly `len` bytes of zeroed,
        // physically-contiguous, guard-bracketed DMA memory; `base` is its
        // non-null cacheable CPU base and `phys` its device-visible base.
        // The buffer is exclusively this slab's (a fresh carve). The buffer
        // is intentionally leaked (no free shim): this host and its pool are
        // boot-leaked to `'static`, so the frames stay valid for the life of
        // the kernel and are never reclaimed. The attached coherency shim
        // cleans and invalidates each range at the driver's ownership
        // hand-offs, because BCM2711 EMMC2 does not snoop the CPU caches.
        Ok(unsafe { DmaSlab::from_leaked(phys, base, len, self.id, 0) }
            .with_coherency(sync_emmc2_dma_range))
    }
}

/// The production aarch64 identity-map extent the DMA/MMIO physical map
/// reaches frames and device windows through: the configured number of
/// identity-mapped gigapages (`plans/PI.md` P6), so the map matches the
/// boot path's own identity extent rather than a fixed guess.
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
/// name rather than asserting.
fn driver_error_name(error: tairix_abi::DriverError) -> &'static str {
    use tairix_abi::DriverError;
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
