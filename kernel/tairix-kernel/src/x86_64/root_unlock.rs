//! The x86_64 (QEMU `q35`/`pc`, UEFI-class PC) live root-unlock bring-up
//! (`plans/ARCHSUPPORT.md` A2).
//!
//! The freestanding-x86_64 half of the in-kernel root-unlock service, the
//! cross-port sibling of [`crate::riscv64::root_unlock`] and
//! [`crate::aarch64::root_unlock`]. It lives in the architecture subtree
//! because it names the x86_64 port directly (the PCI configuration-access
//! mechanism, MSI-X interrupt routing, the `hlt` CPU park); the device- and
//! architecture-independent two-task tail — the spawned interactive unlock
//! and the persistent driver-store serve loop — stays in the shared
//! [`crate::unlock_orchestrate::finish_unlock`].
//!
//! It admits the in-kernel root-unlock kthread at the init seam, brings the
//! bootstrap virtio-blk-PCI root device up over the production MSI-X
//! interrupt path, and hands the opened disk to the shared tail. The only
//! floor block driver on the QEMU PC target is virtio-blk (there is no
//! directly-described SD host as on the Raspberry Pi), so there is one
//! bring-up.
//!
//! # Interrupt delivery: MSI-X, not legacy INTx
//!
//! A modern virtio-PCI function delivers through MSI-X: the device writes
//! the bound line's LAPIC vector directly, so no IO-APIC redirection entry
//! is on the delivery path. The bring-up still binds the function's
//! firmware-assigned PCI Interrupt-Line GSI (a discovered value read from
//! configuration space, never a board constant) as the kernel-side line
//! handle, reuses the vector the boot pipeline assigned that GSI
//! (`discover_and_program_io_apics`) to build the MSI message, and routes
//! it into the device's MSI-X table. The IO-APIC pin stays masked
//! throughout; the shared [`IrqParkWaiter`] re-arm path's mask/unmask of it
//! is harmless because MSI-X does not use the pin.

use core::convert::Infallible;

use alloc::boxed::Box;

use tairix_abi::driver::dma::PoolId;
use tairix_abi::driver::msix::MsixBus;
use tairix_abi::driver::pci::PciBus;
use tairix_abi::IrqHandle;
use tairix_arch_x86_64::irq::{global_routing, msi_message};
use tairix_arch_x86_64::paging::{AddressSpace as ArchAddressSpace, PageTablePool};
use tairix_arch_x86_64::pio::x86_port_io;
use tairix_arch_x86_64::smp::bsp_lapic_id;
use tairix_caps::CapabilitySet;
use tairix_drv_bus_virtio::PciTransport;
use tairix_drv_storage_virtio_blk::{VirtioBlk, VIRTIO_BLK_DEVICE_ID};
use tairix_kernel_core::{
    ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, IrqParkWaiter, YieldHandle,
};
use tairix_kernel_irq::{IrqController, IrqTable};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, DmaPool, FrameAllocator, MemoryPressure, MmioMap, VirtAddr,
};
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::{provision_virtio_pci, KernelMmioMapper, KernelVirtioHost};
use tairix_log::{Level, Sink};

use crate::driver_catalog::VIRTIO_BLK_PATH;
use crate::hwdiscovery::virtio_pci_modern_device_id;
use crate::root_storage::RootBlockBinding;
use crate::unlock_orchestrate::{finish_unlock, UnlockConsole, UnlockEnv};
use crate::unlock_service::{
    loader_caps, note, service_caps, take_boot, CONSOLE0_GATE, UNLOCK_TASK,
};
use crate::x86_64::arch_wrapper::published_irq_table;
use crate::x86_64::ioapic_controller::published_typed;
use crate::x86_64::serial_sink::{COM1_CONSOLE, SERIAL_SINK};
use crate::x86_64::spawn_producer::X86_64_PROCESS_SPAWN;

/// PCI configuration-space offset of the Interrupt Line register (PCI 3.0
/// §6.2.4, low byte of the dword at 0x3C). Firmware programs it with the
/// GSI the function's INTx pin is routed to; `0xFF` is the "no connection"
/// sentinel.
const INTERRUPT_LINE_OFFSET: u16 = 0x3C;

/// The MSI-X table entry the device's vector is programmed into. Every
/// virtqueue shares it, so one bound [`IrqHandle`] covers the whole device.
const MSIX_ENTRY: u16 = 0;

/// Per-device DMA window capacity, in pages, the virtio-blk driver allocates
/// its request/data buffers from (transient per-request DMA).
const POOL_PAGES: usize = 64;

/// Capacity, in pages, of the MMIO register-window map (the four virtio
/// configuration windows plus the MSI-X BAR).
const MMIO_CAP_PAGES: usize = 64;

/// Upper bound (exclusive) of the boot trampoline's identity map: the CPU
/// reaches device register windows and DMA frames through
/// [`DirectPhysMap::identity`] over `[0, 4 GiB)`, the same window `boot.s`
/// identity-maps, so every physical address the bring-up touches keeps its
/// address. The kernel frame allocator draws the DMA frames from usable RAM
/// below this bound on the QEMU PC target.
const IDENTITY_LIMIT: u64 = 4 << 30;

/// Bookkeeping virtual base of the MMIO register-window map. The map's
/// page-table writes land in a throwaway address space (never made live);
/// the CPU reaches the device registers through the identity
/// [`DirectPhysMap`], so this base is pure bookkeeping.
const MMIO_VBASE: u64 = 0x6000_0000;

/// Bookkeeping virtual base of the minted per-driver DMA window (see
/// [`MMIO_VBASE`]).
const POOL_VBASE: u64 = 0x2000_0000;

/// The page-table frame pool the two throwaway bookkeeping address spaces
/// (the MMIO register-window map and the DMA pool) draw their PML4 +
/// intermediate tables from. Private to the unlock service so it never
/// contends with the boot/init pools. The spaces are never made live
/// (device access is via the identity [`DirectPhysMap`]); the pool only
/// backs the guard-bracketed window accounting `kernel/mem` performs. Both
/// window bases ([`MMIO_VBASE`], [`POOL_VBASE`]) sit above the 32 MiB low
/// identity each space maps, so a window mapping never collides with an
/// identity huge page.
static UNLOCK_PT_POOL: PageTablePool = PageTablePool::new();

/// Race-free CPU park for a device wait whose context cannot be
/// scheduler-parked — the boot kthread bringing the disk up before the
/// dispatch loop runs its first task ([`IrqParkWaiter`]'s fallback).
///
/// Mask maskable interrupts (`cli`), re-check the line's ready flag, and
/// only if still not ready enter the atomic `sti; hlt` (which enables `IF`
/// exactly as `hlt` begins, so a completion landing in the check-park
/// window is taken *during* the halt and no edge is lost), then leave
/// interrupts enabled on return. During the boot root-unlock everything
/// else is parked waiting on this work, so briefly halting the CPU starves
/// nothing; every steady-state wait comes from a task's syscall context,
/// which the shared waiter parks off the run queue instead.
fn hlt_fallback_park(table: &IrqTable, handle: IrqHandle) {
    // SAFETY: the IDT and LAPIC are installed by this point (the boot
    // pipeline's per-CPU init), and the device's MSI-X vector is the routed
    // wake source, so a taken interrupt dispatches through a valid handler.
    // `cli`/`sti`/`hlt` are privileged but well-defined in ring 0 and touch
    // only `IF`; `preserves_flags` is intentionally omitted because
    // `sti`/`cli` modify `IF`.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
        if !table.ready_for(handle) {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Release console 0 to `login` and mark the users-database source resolved.
///
/// Both mean "the unlock window is over, `login` may take the console now",
/// so they flip together. Opening the gate lets `login`'s gated console
/// reads through; resolving the late users-database flips a `login` parked
/// on the pending `users_db_read` into its prompt — against the installed
/// database if the unlock succeeded, else fail-closed deny-all. The COM1
/// receive interrupt is (idempotently) armed here too, so a `login` reader
/// now parks off the run queue and a keystroke wakes it.
fn release_console0_to_login() {
    CONSOLE0_GATE.open();
    // Nudge the console wait-queue so any `login` already parked on the
    // (until now) withheld console-0 read re-polls the now-open gate; a
    // no-op before the wait-queue arch hook is installed.
    tairix_kernel_core::console_wake();
    crate::root_mount::LATE_USERS_DB.resolve();
    // Switch COM1 from the poll-backed to the interrupt-driven receive path
    // for the `login` session: a keystroke now wakes the parked reader
    // rather than requiring a poll. Idempotent with the `acquire_console0`
    // arm; a no-op if the boot path could not resolve the console GSI.
    crate::x86_64::com1_rx::enable_uart_console_irq();
}

/// The x86_64 console-0 seam the shared root-unlock orchestration reaches
/// the primary console through.
///
/// The COM1 UART is the primary console: its write half streams the
/// passphrase prompt, and its read half is the interrupt-fed
/// [`Com1ConsoleRead`](crate::x86_64::com1_rx::Com1ConsoleRead) — arming
/// the device's receive interrupt so a typed passphrase wakes the parked
/// unlock kthread rather than busy-polling the FIFO. A boot that could not
/// resolve the console interrupt leaves the receive line disabled and the
/// reader on the poll-backed path (fail closed), never a reader parked
/// forever.
struct X86UnlockConsole;

/// The single `'static` [`X86UnlockConsole`] the bring-up hands the shared
/// orchestration.
static X86_UNLOCK_CONSOLE: X86UnlockConsole = X86UnlockConsole;

impl UnlockConsole for X86UnlockConsole {
    fn acquire_console0(
        &self,
    ) -> (
        &'static dyn ConsoleWrite,
        &'static (dyn ConsoleRead + Sync + 'static),
    ) {
        // Arm COM1's interrupt-driven receive for the unlock window so a
        // keystroke wakes the parked passphrase reader (`console_wake`) —
        // the receive ISR drains the FIFO into `COM1_INPUT`, which
        // `COM1_CONSOLE_READ` reads. Idempotent with the
        // `release_console0_to_login` handoff arm.
        crate::x86_64::com1_rx::enable_uart_console_irq();
        let write: &'static dyn ConsoleWrite = &COM1_CONSOLE;
        // The unlock kthread reads the *ungated* interrupt-fed read half
        // directly; the console list installs the gate-wrapped sibling for
        // `login`.
        let read: &'static (dyn ConsoleRead + Sync + 'static) =
            &crate::x86_64::com1_rx::COM1_CONSOLE_READ;
        (write, read)
    }

    fn release_console0_to_login(&self) {
        release_console0_to_login();
    }
}

/// Admit the in-kernel root-unlock kthread if the boot path bound a
/// virtio-blk root block device, returning whether it was started.
///
/// With no binding (headless / no disk / ambiguous), a non-virtio-blk
/// binding (there is no other floor block driver on the QEMU PC target), or
/// no `'static` frame allocator, it starts nothing, opens the console-0 gate
/// so `login` proceeds (and fails closed, as no database is installed), and
/// returns `false`. The console-0 gate is also opened by the kthread body
/// once the unlock resolves, so it is never left latched closed.
#[must_use]
pub fn spawn_if_present(ctx: &'static (dyn InitSpawnCtx + Sync)) -> bool {
    let boot = take_boot();
    // Route the unlock service's security-relevant decisions onto the boot
    // audit channel when the init seam wired a `'static` audit sink; fall
    // back to the COM1 serial log otherwise. The kthread body and the unlock
    // policy share it.
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
        // `provides_root_block` floor driver, so reaching here is a
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
        // unlock outcome and released console 0. Only an early bring-up
        // failure returns here — and because the success arm is the
        // uninhabited [`Infallible`], the `Err` binding is irrefutable. Fail
        // closed: log the stage and open the console-0 gate so `login`
        // proceeds (it refuses every attempt, as a failed unlock installs no
        // database).
        let Err(stage) = run_unlock(yielder, &binding, frames, caps, env);
        note(audit, Level::Error, stage);
        release_console0_to_login();
        crate::app_store::APP_STORE.note_unavailable();
    };

    let admitted = ctx.spawn_kernel_service(Box::new(body));
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
        // Admission failed: nothing will open the gate or publish the
        // `/System` mount, so do both here or console-0 `login` would park
        // forever.
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
/// produced. Only an early bring-up failure returns `Err` with a stable
/// stage string; on that path the caller logs it and opens the console-0
/// gate.
fn run_unlock(
    yielder: &mut dyn YieldHandle,
    binding: &RootBlockBinding,
    frames: &'static FrameAllocator,
    caps: CapabilitySet,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    // Move the kthread's single yield handle into the shared cell both the
    // re-arming IRQ waiter and the cooperative console reader suspend through
    // (one cooperative-yield definition).
    let coop = CooperativeYield::new(yielder);

    // The bus-driver task capability context: the unlock kthread's caps,
    // owner `UNLOCK_TASK`, audited onto the service's audit sink; the virtio
    // register-window map gates on its `CAP_MMIO_MAP`. Leaked to `'static`
    // because the brought-up device host borrows it for the life of the (now
    // `'static`, shared) disk (kernel state is never freed).
    let caller: &'static TaskCapabilities = Box::leak(Box::new(TaskCapabilities::derive(
        UNLOCK_TASK,
        UserId(0),
        caps,
        caps,
        env.audit,
    )));

    match binding.driver_path {
        VIRTIO_BLK_PATH => virtio_blk_unlock(&coop, caller, frames, env),
        _ => Err("root-unlock: bound block driver is not a known floor driver"),
    }
}

/// Bring the virtio-blk-PCI root device up over the production MSI-X
/// interrupt path and hand it to the shared [`finish_unlock`] tail (the
/// QEMU PC root).
fn virtio_blk_unlock<'a>(
    coop: &'a CooperativeYield<'a>,
    caller: &'static TaskCapabilities,
    frames: &'static FrameAllocator,
    env: UnlockEnv,
) -> Result<Infallible, &'static str> {
    let audit = env.audit;

    // Build the PCI bus over configuration mechanism #1 (port I/O), the same
    // access path the x86_64 virtio-PCI verticals drive; the arch port
    // supplies the `PortIo` backend.
    let bus = tairix_pci::mechanism_one(x86_port_io());

    // The device backing is boot-leaked to `'static`: the brought-up disk is
    // shared for the life of the system by two independent preemptive tasks
    // (the driver-store serve task and the encrypted-root unlock task), so
    // its backing must outlive both frames. The CPU reaches the device
    // register windows and DMA frames through the boot trampoline's identity
    // map; every physical address the bring-up touches lies below
    // [`IDENTITY_LIMIT`].
    let phys: &'static DirectPhysMap = Box::leak(Box::new(DirectPhysMap::identity(IDENTITY_LIMIT)));

    // Throwaway MMIO register-window map: its page-table writes land in a
    // bookkeeping arch space (never made live) and device access is through
    // the identity `phys` map, so the base is pure bookkeeping. A 32 MiB
    // identity base leaves the window base (1.5 GiB) free of collision.
    let mmio_space = ArchAddressSpace::new_identity_first_32mib(&UNLOCK_PT_POOL)
        .ok_or("root-unlock: mmio bookkeeping space")?;
    let mmio: &'static mut MmioMap<'static, ArchAddressSpace> = Box::leak(Box::new(
        MmioMap::new(
            AddressSpace::new(mmio_space),
            VirtAddr::new(MMIO_VBASE),
            MMIO_CAP_PAGES,
            phys,
        )
        .map_err(|_| "root-unlock: mmio map")?,
    ));

    // The modern virtio-blk PCI device id (`0x1040 + type`) the provisioning
    // walk matches; a value that does not fit the 16-bit PCI device-id field
    // is refused fail-closed rather than truncated.
    let device_id = u16::try_from(virtio_pci_modern_device_id(VIRTIO_BLK_DEVICE_ID))
        .map_err(|_| "root-unlock: virtio-blk device id out of range")?;

    // Provision the four virtio configuration windows into a `PciTransport`
    // through the `CAP_MMIO_MAP`-gated kernel mapper, capturing the function
    // address for the interrupt-line read and MSI-X routing below.
    let (mut transport, bdf) = {
        let mapper = KernelMmioMapper::new(&mut *mmio, caller, audit);
        let prov = provision_virtio_pci(&bus, device_id, &mapper, PciTransport::new)
            .map_err(|_| "root-unlock: virtio-PCI provisioning")?;
        (prov.transport, prov.bdf)
    };

    // Resolve the function's firmware-assigned PCI Interrupt-Line GSI from
    // its own configuration space (a discovered value, never a board
    // constant). `0xFF` is the "no connection" sentinel — a function with no
    // routed line is refused fail-closed.
    let line = PciBus::read_config(&bus, bdf, INTERRUPT_LINE_OFFSET)
        .map_err(|_| "root-unlock: read interrupt line")?
        & 0xFF;
    if line == 0xFF {
        return Err("root-unlock: device has no routed interrupt line");
    }
    let gsi = line;

    // Bind the line in the table the kernel core published
    // (`BinArch::install_irq_dispatch`, run in the core's `Irq` phase) and
    // reuse the vector the boot pipeline assigned that GSI to build the MSI
    // message MSI-X delivers through.
    let table: &'static IrqTable =
        published_irq_table().ok_or("root-unlock: no published IRQ table")?;
    let controller = published_typed().ok_or("root-unlock: no IO-APIC controller")?;
    let bind = table
        .bind(gsi, UNLOCK_TASK)
        .map_err(|_| "root-unlock: bind device source")?;
    let handle: IrqHandle = bind.handle;
    let vector = global_routing()
        .vector_for_gsi(gsi)
        .ok_or("root-unlock: no vector for interrupt line")?;
    let msi = msi_message(vector, bsp_lapic_id());

    // Route the MSI message into the device's MSI-X table entry, then enable
    // MSI-X on the transport so every queue signals through it.
    {
        let mapper = KernelMmioMapper::new(&mut *mmio, caller, audit);
        bus.route_msix(bdf, MSIX_ENTRY, msi, &mapper)
            .map_err(|_| "root-unlock: route MSI-X")?;
    }
    transport.enable_msix(MSIX_ENTRY);

    // Mint the per-driver DMA host the driver allocates through, over the
    // kernel's live frame allocator and the identity physical map, driven by
    // the shared parking waiter. The IO-APIC pin is never on the MSI-X
    // delivery path; the waiter's re-arm of it through the controller is
    // therefore harmless.
    let dma_space = ArchAddressSpace::new_identity_first_32mib(&UNLOCK_PT_POOL)
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
    let waiter: &'static IrqParkWaiter = Box::leak(Box::new(IrqParkWaiter::new(
        table,
        handle,
        gsi,
        controller_dyn,
        hlt_fallback_park,
    )));
    let vhost: &'static KernelVirtioHost<'static, _, dyn Sink + Sync> = Box::leak(Box::new(
        KernelVirtioHost::new(pool, caller, audit, PoolId::fresh(), table, handle, waiter),
    ));

    // Admit the virtio-blk driver through the signed load gate (Ed25519
    // signature + `CAP_DRV_LOAD` / `CAP_DRV_KERNEL`) before it drives
    // hardware — a refusal fails closed.
    let loader = crate::driver_loader::KernelDriverLoader::new(audit)
        .ok_or("root-unlock: driver trust anchor")?;
    loader
        .admit(VIRTIO_BLK_PATH, &loader_caps())
        .map_err(|_| "root-unlock: virtio-blk refused at the signed load gate")?;

    // Open the whole-disk block device over the provisioned transport. Every
    // borrowed backing is `'static`, so the opened device is
    // `VirtioBlk<'static>` and can be shared for life behind the
    // block-sharing layer.
    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "root-unlock: virtio-blk open")?;
    finish_unlock(blk, coop, env, &X86_UNLOCK_CONSOLE, &X86_64_PROCESS_SPAWN)
}
