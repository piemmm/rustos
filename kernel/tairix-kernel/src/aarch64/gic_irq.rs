//! Production aarch64 device-IRQ wiring (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (1)).
//!
//! Brings the kernel-wide [`tairix_kernel_irq::IrqTable`] to life on the
//! aarch64 boot path so a discovered device's shared-peripheral interrupt
//! (SPI) can be bound and a task parked on it is woken when the GIC
//! delivers the line. It is the aarch64 analogue of the x86_64
//! `IoApicController` + `production_external_irq_dispatch` wiring in
//! `crate::x86_64::arch_wrapper`; before it, the aarch64 port kept the
//! conservative fail-closed [`tairix_kernel_core::IrqRouting::unsupported`]
//! default and delivered no device interrupts at all.
//!
//! Three pieces compose the path the kernel core (`Phase::Irq`) drives
//! through [`tairix_kernel_core::KernelArch::irq_routing`] /
//! [`tairix_kernel_core::KernelArch::install_irq_dispatch`]:
//!
//! 1. [`GicIrqController`] — a kernel-side [`IrqController`] over the
//!    arch port's validated [`GicController`]. The arch crate cannot
//!    depend on `kernel/irq`, so the bridge from the
//!    arch HAL [`tairix_arch_api::IrqController`] to the
//!    [`tairix_kernel_irq::IrqController`] [`IrqTable::fire`] consumes
//!    lives here, in the kernel binary, exactly like the x86_64
//!    `IoApicController` does. It adds **no** masking policy of its own —
//!    it delegates to the range-checked, fence-ordered [`GicController`].
//! 2. `gic_irq_routing` — the `IrqRouting` the boot path hands
//!    [`crate::aarch64::arch_wrapper::Aarch64BinArch`], naming the
//!    `'static` `GIC_IRQ_CONTROLLER` and the GICv2 maximum INTID as the
//!    bind ceiling.
//! 3. [`install_device_irq_dispatch`] — publishes the live `IrqTable`
//!    into a set-once slot and registers `production_device_irq_dispatch`
//!    with the arch crate's EL1 IRQ-vector seam
//!    ([`tairix_arch_aarch64::exceptions::set_device_irq_dispatch`]). The
//!    EL1 IRQ handler acknowledges the GIC, forwards every non-timer INTID
//!    here, and issues the end-of-interrupt itself; this dispatcher only
//!    translates the acknowledged INTID into an [`IrqTable::fire`] (which
//!    masks the line before a waiter observes the wake —
//!    `docs/src/security/irq.md`).
//!
//! The wiring is **additive and non-regressing**: no
//! device SPI is bound or routed until INCREMENT (2)'s unlock kthread does
//! so, and `production_device_irq_dispatch` is only ever reached for a
//! non-timer INTID the GIC delivers — which cannot occur until a line is
//! routed — so the metal-confirmed boot is unaffected.

// `AtomicBool`/`Ordering` back the freestanding-only UART receive
// flow-control flag, and `IrqRouting` is returned only by the
// freestanding `gic_irq_routing`; on a host build neither is used (the
// host `KernelArch::irq_routing` returns the unsupported default from
// `arch_wrapper`), so both imports are gated to where they compile rather
// than left unused under clippy's `-D warnings`.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tairix_arch_aarch64::gic::{GicController, GicMmio};
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use tairix_kernel_core::IrqRouting;
use tairix_kernel_irq::{IrqController, IrqTable, MaskError};
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use tairix_sync::once::Once;
use tairix_sync::once::OnceCell;
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use tairix_sync::{InterruptControl, IrqSafeSpinLock, IrqState};

/// Set while the console UART's receive line is **masked at the GIC because
/// its receive queue was full** (`drain_uart_into_console_queue`): the ISR
/// applies flow control by disabling the line rather than spinning on a full
/// queue (which would storm the CPU and starve the very reader that drains
/// it). [`rearm_uart_rx_if_masked`] re-enables the line once the reader frees
/// queue space, so input resumes exactly like a hardware FIFO releasing its
/// own flow control. A plain flag, not a queue of
/// state: the line is either masked-for-full or not.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static UART_RX_MASKED: AtomicBool = AtomicBool::new(false);

/// Preemption-quantum rate, in hertz (a ~10 ms time slice).
///
/// The scheduler arms the generic-timer one-shot to one quantum at this
/// rate while a CPU is contended; a tick taken while EL0 was running
/// preempts the current user task (round-robin time-slicing over the
/// EEVDF virtual-deadline order, `kernel/sched`). TAIRiX is tickless: a CPU running a sole task disarms and takes no
/// ticks. The rate is the shared
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ)
/// the riscv64 port also uses — defined once so the two ports cannot
/// diverge.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
const PREEMPT_TICK_HZ: u64 = tairix_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// A kernel-side [`IrqController`] over the arch port's [`GicController`].
///
/// Wraps the validated GICv2 controller and re-exposes its line masking
/// through the [`tairix_kernel_irq::IrqController`] trait
/// [`IrqTable::fire`] requires. The wrapper exists only to satisfy the
/// orphan rule (both the trait and `GicController` are foreign to the arch
/// crate's dependency island) and adds no policy: every `mask` is the
/// arch controller's range-checked, `SeqCst`-fenced
/// [`tairix_arch_api::IrqController::mask`] (the
/// mask-before-wake fence lives once, in the arch port).
pub struct GicIrqController<M: GicMmio + Send + Sync> {
    inner: GicController<M>,
}

impl<M: GicMmio + Send + Sync> GicIrqController<M> {
    /// Wrap an arch-port [`GicController`] as a kernel-side controller.
    #[must_use]
    pub const fn new(inner: GicController<M>) -> Self {
        Self { inner }
    }

    /// Unmask `line` at the GIC distributor after a completion (an
    /// already-routed line; the routing is set once at bind time).
    ///
    /// [`IrqTable::fire`] masks the line before a waiter observes the wake
    /// (mask-before-wake, `docs/src/security/irq.md`), so a level- or
    /// edge-triggered device cannot re-fire while the driver drains its
    /// completion queue. Once the driver has handled the completion the
    /// line must be re-enabled for the *next* one, and that re-enable is an
    /// *arch* operation ([`tairix_arch_api::IrqController::unmask`]) the
    /// kernel-side [`tairix_kernel_irq::IrqController`] trait's `mask` half
    /// deliberately does not expose. The in-kernel block path's
    /// `crate::aarch64::root_unlock` waiter calls this directly (it routes
    /// the SPI itself, once, at setup); the user-space `irq_wait` park path
    /// goes through the trait [`rearm`](IrqController::rearm), which *also*
    /// routes. Both delegate to the range-checked [`GicController`].
    ///
    /// # Errors
    ///
    /// Surfaces [`tairix_arch_api::IrqControlError`] verbatim — an
    /// out-of-range line fails closed without touching the distributor.
    pub fn unmask_line(&self, line: u32) -> Result<(), tairix_arch_api::IrqControlError> {
        use tairix_arch_api::IrqController as ArchIrqController;
        ArchIrqController::unmask(&self.inner, line)
    }
}

/// GICv2 distributor target byte selecting the boot CPU (CPU interface 0).
///
/// Every device SPI is deliberately routed to the boot CPU: it is the one
/// core guaranteed online in every configuration (a no-PSCI tree boots
/// single-CPU), and a single interrupt target keeps the deferred-wake
/// hand-off simple while the secondaries take work through the
/// scheduler's placement IPIs instead. Spreading device IRQs across the
/// online cores is a measured optimisation, not a correctness need.
/// Defined once here so the in-kernel block path and the user-space-driver
/// re-arm path route through the same value.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub const CPU0_TARGET: u8 = 0b0000_0001;

/// The `'static` [`IrqTable`] the kernel core published in
/// [`crate::Phase::Irq`](tairix_kernel_core::Phase::Irq) through
/// [`install_device_irq_dispatch`], or [`None`] before it is published.
///
/// An in-kernel service kthread (the INCREMENT (2) root-unlock kthread)
/// that must bind and block on a device SPI binds on **this** table — the
/// one [`production_device_irq_dispatch`] fires into — never a fresh table
/// the EL1 vector would never reach. Reading the set-once slot is the only
/// way to reach the live table from the kthread, since the core owns its
/// allocation inside the leaked `KernelState` (one
/// table definition, not two that could diverge).
///
/// Freestanding-only: the in-kernel unlock kthread that consumes it is
/// itself bare-metal aarch64 ([`crate::unlock_service`]); a host build has
/// no kthread to bind a line, so the accessor is not compiled there.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[must_use]
pub fn published_irq_table() -> Option<&'static IrqTable> {
    IRQ_TABLE_SLOT.get().ok().flatten().copied()
}

impl<M: GicMmio + Send + Sync> IrqController for GicIrqController<M> {
    /// Mask `line` by delegating to the arch controller, mapping its
    /// [`tairix_arch_api::IrqControlError`] onto the
    /// [`tairix_kernel_irq::MaskError`] [`IrqTable::fire`] expects.
    ///
    /// An out-of-range line maps to [`MaskError::OutOfRange`]; any other
    /// arch-side refusal maps to [`MaskError::Unsupported`] so the table
    /// surfaces it as the standard architecture-unsupported outcome
    /// (fail closed).
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        use tairix_arch_api::{IrqControlError, IrqController as ArchIrqController};
        match ArchIrqController::mask(&self.inner, line) {
            Ok(()) => Ok(()),
            Err(IrqControlError::OutOfRange) => Err(MaskError::OutOfRange),
        }
    }

    /// Route `line` to the boot CPU and unmask it at the distributor.
    ///
    /// This is the re-arm the user-space `irq_wait` park path drives on an
    /// interrupt-driven driver's behalf (the driver holds no GIC access): it
    /// routes the SPI to `CPU0_TARGET` (idempotent — re-routing an
    /// already-targeted line is a plain register write) and then clears its
    /// enable mask through the same range-checked, fence-ordered
    /// [`GicController`] unmask the in-kernel block path uses. An out-of-range line fails closed as [`MaskError::OutOfRange`]
    /// without touching the distributor.
    fn rearm(&self, line: u32) -> Result<(), MaskError> {
        use tairix_arch_api::{IrqControlError, IrqController as ArchIrqController};
        // SAFETY: the GICv2 distributor bases were configured from the device
        // tree and the controller brought up (`install_device_irq_dispatch`
        // → `gic::init`) before any line is bound, so the target-register
        // write addresses live, identity-mapped distributor MMIO. `route_spi`
        // ignores SGIs/PPIs and only writes the SPI target byte.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        unsafe {
            tairix_arch_aarch64::gic::route_spi(line, CPU0_TARGET);
        }
        match ArchIrqController::unmask(&self.inner, line) {
            Ok(()) => Ok(()),
            Err(IrqControlError::OutOfRange) => Err(MaskError::OutOfRange),
        }
    }
}

/// Set-once slot for the `'static` [`IrqTable`] the kernel core builds in
/// `Phase::Irq` and publishes through
/// [`install_device_irq_dispatch`].
///
/// `production_device_irq_dispatch` reads it from interrupt context to
/// translate an acknowledged GIC INTID into an [`IrqTable::fire`]. The
/// [`OnceCell`] enforces the one-shot-publish invariant (no global mutable state; this is a publish-once pointer).
static IRQ_TABLE_SLOT: OnceCell<&'static IrqTable> = OnceCell::new();

/// Set-once slot for the console UART's discovered GIC SPI INTID (the
/// `arm,pl011` / mini-UART node's `interrupts`, decoded from the firmware
/// device tree — a discovered value, never a board constant).
///
/// The boot path records it ([`set_uart_console_intid`]) when it parses the
/// device tree, and the unlock kthread's console handoff
/// (`enable_uart_console_irq`) routes + unmasks it once the passphrase
/// poll is over. `production_device_irq_dispatch` reads it from interrupt
/// context to recognise the console's receive interrupt and feed the bytes
/// to the login reader rather than the `irq_wait` table. Empty until the
/// boot path discovers a console interrupt (a UART-less or interrupt-less
/// tree simply leaves `login` on the polled path — fail closed).
static UART_RX_INTID: OnceCell<u32> = OnceCell::new();

/// Record the console UART's discovered receive-interrupt INTID so the
/// console handoff can route it and the device-IRQ dispatcher can recognise
/// it. Idempotent: a second call (there is only ever one console) is a
/// no-op (publish-once).
pub fn set_uart_console_intid(intid: u32) {
    let _ = UART_RX_INTID.set(intid);
}

/// The `'static` GICv2-backed controller every [`IrqTable::fire`] masks
/// through.
///
/// Built over the arch port's zero-sized [`VolatileGicMmio`] handle, which
/// reads the **discovered** GICv2 distributor/CPU-interface bases on every
/// access, so the controller carries no board constant. The bind ceiling is the GICv2 maximum INTID
/// ([`tairix_arch_aarch64::gic::MAX_INTID`]); a device SPI is bound below
/// it and the table refuses any line above it.
///
/// Freestanding-only: [`VolatileGicMmio`] performs real MMIO and exists
/// only on the bare-metal target. Host builds return
/// [`IrqRouting::unsupported`] from [`Aarch64BinArch::irq_routing`]
/// instead.
///
/// [`VolatileGicMmio`]: tairix_arch_aarch64::gic::VolatileGicMmio
/// [`Aarch64BinArch::irq_routing`]: crate::aarch64::arch_wrapper::Aarch64BinArch
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static GIC_IRQ_CONTROLLER: GicIrqController<tairix_arch_aarch64::gic::VolatileGicMmio> =
    GicIrqController::new(GicController::new(
        tairix_arch_aarch64::gic::Gicv2::new(tairix_arch_aarch64::gic::VolatileGicMmio),
        tairix_arch_aarch64::gic::MAX_INTID,
    ));

/// The [`IrqRouting`] the aarch64 boot path installs: the [composite
/// controller](CompositeIrqController) routing real GIC INTIDs to the GICv2
/// distributor and virtual MSI lines to the BCM2711 root-complex MSI
/// controller, with [`MSI_LINE_TOP`] as the inclusive bind ceiling so a
/// driver may `irq_bind` either a GIC SPI or an allocated MSI line.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[must_use]
pub fn gic_irq_routing() -> IrqRouting {
    IrqRouting {
        max_line: MSI_LINE_TOP,
        controller: &COMPOSITE_IRQ_CONTROLLER,
    }
}

// --- BCM2711 root-complex MSI: composite controller + vector allocator ---
//
// The VL805 xHCI raises an MSI the BCM2711 PCIe root complex demultiplexes
// onto one shared GIC SPI; the kernel owns that controller (a chained
// interrupt handler a user-space driver cannot be) and fans the SPI out to
// per-vector *virtual* IRQ lines a driver binds with `irq_wait`. The virtual
// lines live in a range immediately above the GIC INTID ceiling, so one
// composite `IrqController` routes a real GIC INTID to the GIC and a virtual
// MSI line to the root-complex controller without the kernel IRQ core needing
// a second controller field (`plans/PI.md` U-MSI).

/// Base of the virtual MSI interrupt-line range, immediately above the GICv2
/// INTID ceiling so the two line spaces never overlap.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub const MSI_LINE_BASE: u32 = tairix_arch_aarch64::gic::MAX_INTID + 1;

/// Inclusive top of the virtual MSI interrupt-line range
/// (`MSI_LINE_BASE + NUM_MSI_VECTORS - 1`).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub const MSI_LINE_TOP: u32 = MSI_LINE_BASE + tairix_arch_aarch64::brcm_msi::NUM_MSI_VECTORS - 1;

/// The kernel-side BCM2711 root-complex MSI controller over the discovered RC
/// register base. A zero-sized `VolatileMsiMmio` handle reads the base
/// `brcm_msi::configure` resolved from the device tree on every access, so the
/// controller carries no board constant.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static BRCM_MSI: tairix_arch_aarch64::brcm_msi::BrcmMsi<
    tairix_arch_aarch64::brcm_msi::VolatileMsiMmio,
> = tairix_arch_aarch64::brcm_msi::BrcmMsi::new(tairix_arch_aarch64::brcm_msi::VolatileMsiMmio);

/// The discovered shared GIC SPI INTID the root-complex MSI controller raises
/// (the `brcm,bcm2711-pcie` node's MSI `interrupts` entry — a discovered
/// value, never a board constant). Recorded once by the boot path
/// ([`set_brcm_msi_spi`]); empty on a board with no such controller, which
/// leaves [`allocate_msi_vector`] failing closed.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static BRCM_MSI_SPI: OnceCell<u32> = OnceCell::new();

/// One-shot bring-up of the root-complex MSI controller (program its
/// doorbell, route + enable its shared GIC SPI), run on the first allocation.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static BRCM_MSI_READY: Once<()> = Once::new();

/// Bitmap of allocated MSI vectors (bit `v` set means vector `v` is in use).
/// Vectors are minted by [`allocate_msi_vector`]; a driver holds its line for
/// its lifetime, so a set-only bitmap suffices (no free path today).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static BRCM_MSI_ALLOCATED: AtomicU32 = AtomicU32::new(0);

/// Record the discovered shared GIC SPI INTID the root-complex MSI controller
/// raises. Idempotent (publish-once); a board with no such controller never
/// calls it and [`allocate_msi_vector`] fails closed.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn set_brcm_msi_spi(intid: u32) {
    let _ = BRCM_MSI_SPI.set(intid);
}

/// Names the kernel-internal GIC SPIs this port enables **without** a task
/// `irq_wait` binding, so the lockup watchdog attributes a stuck one to a
/// stable category name (`stuck_owner=<name>`) instead of a bare `unbound`.
///
/// Two enabled lines have no task owner by construction because the kernel
/// services them itself: the platform PCIe root-complex MSI multiplexer's
/// shared SPI (the chained handler in [`production_device_irq_dispatch`] that
/// fans messages out to virtual MSI lines) and the console UART's receive
/// SPI. Their interrupt numbers are discovered from the device tree at boot
/// and recorded in [`BRCM_MSI_SPI`] / [`UART_RX_INTID`] — never board
/// constants — so this lookup compares against those discovered values and
/// returns board-neutral category names. Any other line is unknown here and
/// falls back to the task-owner attribution.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub struct KernelInternalLineNames;

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
impl tairix_kernel_core::KernelInternalLines for KernelInternalLineNames {
    fn name_of_line(&self, line: u32) -> Option<&'static str> {
        if BRCM_MSI_SPI.get().ok().flatten().copied() == Some(line) {
            // The root-complex MSI multiplexer's shared SPI: a chained line
            // the kernel owns, never a driver's `irq_wait` binding.
            return Some("pcie-msi");
        }
        if UART_RX_INTID.get().ok().flatten().copied() == Some(line) {
            // The console UART receive line the kernel drains into the
            // console queue, never bound through the `irq_wait` table.
            return Some("console-uart");
        }
        None
    }
}

/// The `'static` kernel-internal line-name resolver the boot path hands the
/// watchdog through [`crate::aarch64::arch_wrapper::Aarch64BinArch`]'s
/// `watchdog_line_names`. Zero-sized, so it has no `.bss`/`.data` footprint.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static KERNEL_INTERNAL_LINE_NAMES: KernelInternalLineNames = KernelInternalLineNames;

/// The virtual-MSI vector a routing line names, or [`None`] if `line` is a
/// real GIC INTID.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn msi_vector_of_line(line: u32) -> Option<u32> {
    if (MSI_LINE_BASE..=MSI_LINE_TOP).contains(&line) {
        Some(line - MSI_LINE_BASE)
    } else {
        None
    }
}

/// A kernel-side [`IrqController`] routing a real GIC INTID to
/// [`GIC_IRQ_CONTROLLER`] and a virtual MSI line to [`BRCM_MSI`].
///
/// This is the one line->controller fan-out the kernel IRQ core drives
/// through `IrqRouting.controller`: a line in `[MSI_LINE_BASE, MSI_LINE_TOP]`
/// is a BCM2711 MSI vector (masked/unmasked at the root complex's `INTR2`
/// block), every other line is a GIC INTID. It adds no policy of its own —
/// each half delegates to the range-checked controller it wraps.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub struct CompositeIrqController;

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
impl IrqController for CompositeIrqController {
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        match msi_vector_of_line(line) {
            Some(vector) => {
                BRCM_MSI.mask(vector);
                Ok(())
            }
            None => GIC_IRQ_CONTROLLER.mask(line),
        }
    }

    fn rearm(&self, line: u32) -> Result<(), MaskError> {
        match msi_vector_of_line(line) {
            // The MSI controller's shared GIC SPI is routed + enabled once at
            // controller bring-up; re-arming a vector only unmasks its
            // `INTR2` bit for the next message.
            Some(vector) => {
                BRCM_MSI.unmask(vector);
                Ok(())
            }
            None => GIC_IRQ_CONTROLLER.rearm(line),
        }
    }
}

/// The `'static` composite controller [`IrqTable::fire`] masks through and
/// the `irq_wait` park path re-arms through.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static COMPOSITE_IRQ_CONTROLLER: CompositeIrqController = CompositeIrqController;

/// Allocate a free BCM2711 MSI vector, bring the controller up on first use,
/// and return the [`MsiAllocation`](tairix_abi::MsiAllocation) the `msi_alloc`
/// syscall reports.
///
/// The returned line is `MSI_LINE_BASE + vector`; the doorbell is
/// `brcm_msi::msi_message(vector)`. The vector's `INTR2` bit stays **masked**
/// until the binding driver's first `irq_wait` re-arm unmasks it, so no
/// message is delivered before a waiter exists. Fails closed with
/// [`Errno::NotImplemented`](tairix_abi::Errno) when no MSI SPI was discovered
/// (no controller on this board) and [`Errno::OutOfRange`](tairix_abi::Errno)
/// when every vector is in use.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn allocate_msi_vector() -> Result<tairix_abi::MsiAllocation, tairix_abi::Errno> {
    // No discovered MSI SPI means no root-complex MSI controller on this
    // board: fail closed rather than fabricating a vector.
    let Some(spi) = BRCM_MSI_SPI.get().ok().flatten().copied() else {
        return Err(tairix_abi::Errno::NotImplemented);
    };
    // Bring the controller up exactly once: program its doorbell + data
    // pattern and mask every vector, then route + enable its shared GIC SPI
    // so a later message reaches the chained dispatcher. Every vector stays
    // masked, so enabling the SPI is additive — no message fires until a
    // vector is unmasked by its driver's first `irq_wait`.
    let _ = BRCM_MSI_READY.call_once_infallible(|| {
        BRCM_MSI.init();
        // SAFETY: the GIC distributor + CPU interface are up
        // (`install_device_irq_dispatch` ran `gic::init`) and the EL1 device
        // dispatch is installed, so routing this discovered SPI to the boot
        // CPU addresses live distributor MMIO and a delivered SPI reaches
        // `production_device_irq_dispatch`.
        unsafe {
            tairix_arch_aarch64::gic::route_spi(spi, CPU0_TARGET);
        }
        let _ = GIC_IRQ_CONTROLLER.unmask_line(spi);
    });
    // Claim the lowest free vector with a CAS loop over the set-only bitmap.
    let vector = loop {
        let current = BRCM_MSI_ALLOCATED.load(Ordering::Acquire);
        let Some(vector) = (0..tairix_arch_aarch64::brcm_msi::NUM_MSI_VECTORS)
            .find(|v| current & (1u32 << v) == 0)
        else {
            return Err(tairix_abi::Errno::OutOfRange);
        };
        let updated = current | (1u32 << vector);
        if BRCM_MSI_ALLOCATED
            .compare_exchange(current, updated, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            break vector;
        }
    };
    let (address, data) = tairix_arch_aarch64::brcm_msi::msi_message(vector);
    Ok(tairix_abi::MsiAllocation::new(
        address,
        data,
        MSI_LINE_BASE + vector,
    ))
}

/// The `'static` [`MsiAllocFacility`](tairix_kernel_core::MsiAllocFacility)
/// the `msi_alloc` syscall handler drives (installed by the boot path through
/// [`tairix_kernel_core::KernelArch::msi_alloc_facility`]).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub struct BrcmMsiAllocFacility;

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
impl tairix_kernel_core::MsiAllocFacility for BrcmMsiAllocFacility {
    fn allocate(&self) -> Result<tairix_abi::MsiAllocation, tairix_abi::Errno> {
        allocate_msi_vector()
    }
}

/// The shared [`BrcmMsiAllocFacility`] the boot path installs.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static BRCM_MSI_ALLOC_FACILITY: BrcmMsiAllocFacility = BrcmMsiAllocFacility;

/// The production device-IRQ dispatcher the arch crate's EL1 IRQ-vector
/// path invokes with each acknowledged non-timer GIC INTID.
///
/// Looks up the published [`IrqTable`] and forwards to
/// [`IrqTable::fire`], which masks the line through [`GIC_IRQ_CONTROLLER`]
/// before setting the per-handle ready flag a parked waiter observes
/// (mask-before-wake, `docs/src/security/irq.md`). The GIC
/// end-of-interrupt handshake is the arch handler's job and happens after
/// this returns. The `fire` outcome is intentionally ignored: a stray INTID
/// (no binding) or an out-of-range line surfaces to the next waiter through
/// the table's own [`tairix_kernel_irq::WaitStep`] taxonomy, and the line is
/// already masked.
///
/// Safe to invoke from interrupt context: every operation is wait-free and
/// allocation-free. A delivery before the table is
/// published (impossible in production — the core installs the table in
/// `Phase::Irq`, strictly before any SPI is routed) returns silently.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub extern "C" fn production_device_irq_dispatch(intid: u32) {
    // The console UART is a kernel-internal source on a single shared GIC
    // line carrying both directions, not an `irq_wait` binding. Service it by
    // reading the masked interrupt status once: push buffered transmit bytes
    // into the FIFO (`service_uart_tx_irq`, the interrupt-driven drain that
    // keeps logging flowing without stalling any task), and drain the receive FIFO into the console queue **only** when a
    // receive interrupt actually fired. Receive is interrupt-driven for the
    // whole interactive session (enabled by the root-unlock kthread for its
    // passphrase prompt and again at the `login` handoff), so draining wakes
    // whichever reader is parked on `CONSOLE_WAITQ` — the unlock kthread's
    // `KthreadConsoleRead` or, after the handoff, `login`'s
    // `BlockingConsoleRead`. While the receive source is still masked (no
    // interactive reader yet) it never fires, so this returns without
    // draining. Checked first so the console line never reaches the
    // `irq_wait` table it was never bound on.
    if UART_RX_INTID.get().ok().flatten().copied() == Some(intid) {
        let rx_pending = tairix_arch_aarch64::serial::service_uart_tx_irq();
        if rx_pending {
            drain_uart_into_console_queue();
            // A receive drain may have made a parked console reader
            // runnable; flag the reschedule so the running EL0 task yields
            // on return and the dispatcher drains the wake. A transmit-only
            // service woke nothing, so it deliberately does not latch (the
            // UART-TX drain IRQ is frequent during logging).
            note_resched_here();
        }
        return;
    }
    let Ok(Some(table)) = IRQ_TABLE_SLOT.get() else {
        return;
    };
    // The BCM2711 root-complex MSI controller multiplexes up to 32 message
    // vectors onto one shared GIC SPI: this is the *chained* handler. When
    // that SPI fires, read which vectors are pending, fire each onto its
    // virtual MSI line (masking that vector before the waiter wakes —
    // mask-before-wake holds through the composite controller), and clear its
    // `INTR2` status so the level-sensitive SPI deasserts rather than
    // re-storming. A vector with no binding still has its status cleared, so a
    // stray message cannot wedge the line.
    if BRCM_MSI_SPI.get().ok().flatten().copied() == Some(intid) {
        let pending = BRCM_MSI.pending();
        for vector in tairix_arch_aarch64::brcm_msi::pending_vectors(pending) {
            let _ = table.fire(MSI_LINE_BASE + vector, &COMPOSITE_IRQ_CONTROLLER);
            BRCM_MSI.clear(vector);
        }
        tairix_kernel_core::irq_wake();
        note_resched_here();
        return;
    }
    let _ = table.fire(intid, &COMPOSITE_IRQ_CONTROLLER);
    // Wake any `irq_wait` caller parked on a bound line: `fire` set the
    // per-line ready flag (after masking — mask-before-wake holds), so a
    // woken waiter that consumes it observes the mask. A spurious wake for
    // a waiter on a different line is harmless — it re-checks its own line
    // and parks again. Wait-free and
    // allocation-free, safe from this interrupt context.
    tairix_kernel_core::irq_wake();
    note_resched_here();
}

/// Latch a pending reschedule on the CPU currently servicing this device
/// interrupt, so that when it returns to EL0 the running user task yields
/// into the scheduler — which runs `drain_pending_wakes` and dispatches
/// the task this interrupt just woke. Called only where a wake was
/// actually requested (a bound-line/MSI `irq_wake`, or a console receive
/// drain); a device interrupt that woke nothing does not latch, so a
/// CPU-bound EL0 task is disturbed only when there is genuinely new work.
/// Pure accounting (one atomic store), safe from interrupt context.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn note_resched_here() {
    tairix_kernel_core::note_preempt_tick(tairix_arch_aarch64::smp::current_cpu_index());
}

/// Saved DAIF state for [`DaifIrqControl`].
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[derive(Copy, Clone)]
pub struct DaifState(u64);

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
impl IrqState for DaifState {}

/// The aarch64 `InterruptControl` behind [`UART_RX_GATE`]: masks
/// asynchronous interrupts via DAIF for the critical section and restores
/// the exact prior state on release (reentrant — an already-masked state
/// round-trips unchanged). Plugged into `lib/sync`'s IRQ-safe spinlock so
/// the gate is also correct across CPUs (mask locally, spin globally).
///
/// The section masks IRQ+FIQ (the classic discipline, matching the arch
/// port's video render lock). The debug watchdog build additionally
/// re-clears FIQ (`DAIF.F`) — so its non-maskable Group-0/FIQ self-sample
/// can observe a core wedged inside this section (`plans/WATCHDOG.md`) —
/// but **only** when the boot probe proved a non-maskable FIQ is
/// deliverable to this kernel (`tairix_arch_aarch64::watchdog::fiq_cadence_enabled`).
/// Where the probe found FIQ undeliverable (a two-Security-state GIC-400,
/// a Raspberry Pi 4, where Group 0 is secure) FIQ stays masked exactly as
/// in a shippable build (fail closed).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub struct DaifIrqControl;

// SAFETY: `disable` reads DAIF and sets its IRQ+FIQ mask bits — always
// permitted at EL1, touches no memory — atomically masking asynchronous
// interrupts on this CPU and returning the exact prior state; `restore`
// writes that state back verbatim. A `disable` while already masked returns
// the masked state, whose restore leaves interrupts masked (reentrant),
// exactly as the trait requires. The debug watchdog build re-clears FIQ
// (`DAIF.F`) only when the boot probe proved FIQ is genuinely deliverable
// to this kernel; that is sound because on such a GIC the only Group-0/FIQ
// source is the watchdog self-sample, which reads the interrupted context
// and never takes this lock, so it cannot deadlock against a held critical
// section. Where the probe found FIQ undeliverable (a secure Group 0 on a
// two-Security-state GIC-400) FIQ stays masked, so a held section is never
// exposed to a secure-world Group-0 FIQ the kernel cannot service.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
unsafe impl InterruptControl for DaifIrqControl {
    type State = DaifState;

    fn disable() -> Self::State {
        let daif: u64;
        // Capture the prior state and mask IRQ+FIQ — the shippable-safe
        // default (the classic discipline, matching the arch port's video
        // render lock). `restore` writes the captured state back verbatim,
        // so this is reentrant.
        // SAFETY: reading DAIF and setting its mask bits is always
        // permitted at EL1 and touches no memory.
        unsafe {
            core::arch::asm!(
                "mrs {0}, daif",
                "msr daifset, #{mask}",
                out(reg) daif,
                mask = const (tairix_arch_aarch64::exceptions::daif::I
                    | tairix_arch_aarch64::exceptions::daif::F),
                options(nomem, nostack, preserves_flags)
            );
        }
        // Debug watchdog self-sample: re-clear FIQ (`DAIF.F`) for the
        // critical section ONLY when the boot probe *proved* a non-maskable
        // FIQ is deliverable to this kernel, so a Group-0/FIQ cadence can
        // observe a core wedged inside this section (`plans/WATCHDOG.md`).
        // Where the probe found FIQ undeliverable (a two-Security-state
        // GIC-400 — a Raspberry Pi 4, where Group 0 belongs to the secure
        // world) FIQ stays masked exactly as in a shippable build, so a held
        // critical section is never exposed to a secure-world Group-0 FIQ the
        // non-secure kernel cannot service (fail closed). The decision is a
        // run-time property of the hardware, not of the build, so it is read
        // from the probe rather than a compile-time constant.
        #[cfg(feature = "watchdog-diagnostics")]
        if tairix_arch_aarch64::watchdog::fiq_cadence_enabled() {
            // SAFETY: clearing `DAIF.F` only unmasks FIQ and touches no
            // memory; the probe has confirmed a taken FIQ reaches this
            // kernel, and the self-sample reads the interrupted context and
            // never takes this lock, so it cannot deadlock against the hold.
            unsafe {
                core::arch::asm!(
                    "msr daifclr, #{f}",
                    f = const tairix_arch_aarch64::exceptions::daif::F,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
        DaifState(daif)
    }

    unsafe fn restore(state: Self::State) {
        // SAFETY: writing back the DAIF value captured by `disable` on
        // this CPU restores exactly the prior mask state.
        unsafe {
            core::arch::asm!(
                "msr daif, {0}",
                in(reg) state.0,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

/// Mask this CPU's interrupts for a kernel-heap-allocator critical section,
/// returning the prior `DAIF` state as an opaque token.
///
/// The `fn`-pointer adapter the boot path installs into the global heap
/// (`tairix_kalloc::FreeListAllocator::install_irq_control`) so the
/// allocator's lock is interrupt-safe: an interrupt taken on a CPU already
/// holding the lock can no longer reenter `alloc`/`dealloc` and spin forever
/// on the lock its own interrupted mainline holds. Delegates to the same
/// [`DaifIrqControl`] every IRQ-safe spinlock uses, so the masking discipline
/// is defined once.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub(crate) fn kalloc_irq_disable() -> usize {
    <DaifIrqControl as InterruptControl>::disable().0 as usize
}

/// Restore this CPU's interrupt state from a token
/// [`kalloc_irq_disable`] returned, closing the allocator critical section.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub(crate) fn kalloc_irq_restore(token: usize) {
    // SAFETY: `token` is a `DAIF` value a paired `kalloc_irq_disable`
    // captured on this CPU; writing it back restores exactly the prior mask.
    unsafe {
        <DaifIrqControl as InterruptControl>::restore(DaifState(token as u64));
    }
}

/// Serialises **every** access to the console UART's receive path — the
/// destructive hardware-FIFO reads *and* the [`UART_INPUT`] ring they
/// feed — across the RX ISR and the reader's own poll-and-read
/// ([`poll_and_read_uart`]).
///
/// Without it two destructive FIFO readers race: a reader-context drain
/// interrupted between its FIFO read and its queue push lets the ISR
/// drain and push the remaining bytes first, reordering input across the
/// line terminator — and a data-register read racing the ISR on the last
/// byte returns a stale duplicate that the next reader receives as a
/// phantom keystroke (the observed corrupted-login-line defect). Masking
/// interrupts for the hold (DAIF, via [`DaifIrqControl`]) also removes
/// the single-CPU deadlock of the ISR spinning on the ring lock its
/// interrupted holder cannot release. The hold is short and bounded (one
/// FIFO drain), and the wake it publishes (`console_wake`) is lock-free,
/// so masked delivery is deferred by at most that bound.
///
/// [`UART_INPUT`]: crate::aarch64::arch_wrapper::UART_INPUT
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static UART_RX_GATE: IrqSafeSpinLock<(), DaifIrqControl> = IrqSafeSpinLock::new(());

/// Drain the console UART's hardware receive FIFO into the UART console's
/// receive queue, waking the parked `login` reader.
///
/// Invoked from interrupt context by [`production_device_irq_dispatch`] when
/// the console's receive interrupt fires. Each `push` enqueues the bytes and
/// wakes any reader parked in kernel-core's `BlockingConsoleRead`
/// ([`tairix_kernel_core::ConsoleInputQueue::push`] →
/// `crate::waitq::console_wake`). It is **bounded** by the console queue's
/// free space (at most one queue capacity per interrupt) and **lossless**:
/// it dequeues from the FIFO only what the queue can accept and leaves any
/// surplus in the FIFO, so the level-sensitive receive interrupt re-fires as
/// the reader drains the queue. Allocation-free, and
/// serialised against the reader's poll-and-read by [`UART_RX_GATE`].
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn drain_uart_into_console_queue() {
    let _gate = UART_RX_GATE.lock();
    drain_uart_locked();
}

/// The shared FIFO-to-queue drain body. Callers **must** hold
/// [`UART_RX_GATE`]: the FIFO reads are destructive, so two concurrent
/// drains reorder or duplicate input (see the gate's docs).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn drain_uart_locked() {
    let queue = &crate::aarch64::arch_wrapper::UART_INPUT;
    // Push through the installed UART console *device*, not the raw queue:
    // its line discipline maps a cooked-mode `^C`/`^Z` to a foreground
    // signal at arrival time (`plans/SPAWN.md` SP9) and forwards every
    // other byte to this same queue. The lossless-backpressure /
    // clear-then-recheck loop is the one shared definition both this port
    // and the x86_64 16550 console reuse — the PL011 specifics are the
    // injected closures: `read_console_bytes` reads the hardware FIFO,
    // `clear_rx_interrupt` clears the receive-timeout latch the PL011
    // asserts even with a drained FIFO, and the flow-control brake masks the
    // receive line at the GIC (`rearm_uart_rx_if_masked` re-opens it once the
    // reader frees queue space, and the level-sensitive line re-asserts on
    // the bytes left in the FIFO).
    let console = crate::aarch64::arch_wrapper::uart_console_device();
    crate::console_uart::drain_fifo_into_console(
        console,
        queue,
        |buf| tairix_arch_aarch64::serial::read_console_bytes(buf),
        || tairix_arch_aarch64::serial::clear_rx_interrupt(),
        || {
            if let Some(intid) = UART_RX_INTID.get().ok().flatten().copied() {
                let _ = GIC_IRQ_CONTROLLER.mask(intid);
                UART_RX_MASKED.store(true, Ordering::Release);
            }
        },
    );
}

/// Synchronously drain the console UART's hardware receive FIFO into the
/// receive queue and read from that queue, all from the **reader's** own
/// context (a `stream_read` syscall or the unlock kthread) under one
/// [`UART_RX_GATE`] hold.
///
/// Called by [`crate::aarch64::arch_wrapper::UartConsoleRead::read`]. It
/// makes console input **poll-backed**, not solely interrupt-driven: the
/// reader pulls any byte already sitting in the hardware FIFO directly, so
/// it only ever parks when the FIFO *and* the software queue are genuinely
/// empty. That closes every residual device-IRQ-delivery race — a receive
/// interrupt the CPU has not yet taken, or a sub-trigger FIFO tail still
/// awaiting the PL011 receive-timeout — because the reader no longer
/// *depends* on the interrupt to see a byte that is already in the FIFO;
/// the interrupt remains only the wake that unparks it once it has parked
/// (a genuine park, never a busy-poll: a byte arriving after this drain
/// raises the interrupt that wakes the parked task).
///
/// Reader-context code runs with IRQs **deliverable** (the kernel takes
/// interrupts while in-kernel code runs), so this drain genuinely races
/// the RX ISR without the gate; holding [`UART_RX_GATE`] across the FIFO
/// drain *and* the queue read makes the whole step atomic against it — no
/// reordering, no stale duplicate byte, no ISR spin on an interrupted
/// ring-lock holder. The shared drain body is the one definition both
/// entry points reuse.
///
/// # Errors
///
/// Propagates the queue read's error (the queue itself is infallible;
/// the `Result` mirrors the `ConsoleRead` contract).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn poll_and_read_uart(buf: &mut [u8]) -> Result<usize, tairix_abi::Errno> {
    use tairix_kernel_core::ConsoleRead as _;
    let _gate = UART_RX_GATE.lock();
    drain_uart_locked();
    crate::aarch64::arch_wrapper::UART_INPUT.read(buf)
}

/// Re-enable the console UART's receive line if the ISR masked it on a full
/// queue ([`drain_uart_into_console_queue`]).
///
/// Called from the reader's drain path
/// ([`crate::aarch64::arch_wrapper::UartConsoleRead`]) after it frees queue
/// space: re-routing + unmasking the line lets the level-sensitive PL011
/// re-assert on the bytes it left in the FIFO, resuming delivery — the
/// software analogue of a hardware FIFO releasing flow control. A cheap
/// `Acquire` load on the common (not-masked) path, so it adds no cost to a
/// normal read.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn rearm_uart_rx_if_masked() {
    if !UART_RX_MASKED.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Some(intid) = UART_RX_INTID.get().ok().flatten().copied() {
        let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
    }
}

/// Enable the console UART's receive interrupt and route + unmask its GIC
/// line, so console input is interrupt-driven and parked readers are woken
/// by a keystroke rather than busy-polling the FIFO.
///
/// Idempotent, and called at the start of the interactive session: the
/// in-kernel root-unlock kthread calls it before its passphrase prompt (so
/// the parked `KthreadConsoleRead` is woken by RX), and the `login` handoff
/// ([`crate::aarch64::root_unlock::release_console0_to_login`]) calls it
/// again — a second call is a harmless re-enable, and the fail-closed paths
/// that open the gate without ever running the unlock kthread still enable it
/// here for `login`. A console whose interrupt the boot path could not
/// discover leaves the slot empty and this a no-op — the reader stays on the
/// poll-backed path rather than failing (fail closed).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn enable_uart_console_irq() {
    let Some(intid) = UART_RX_INTID.get().ok().flatten().copied() else {
        return;
    };
    // Enable the receive interrupt at the device first, then route + unmask it
    // at the GIC, so the first delivered edge already has a drain target.
    tairix_arch_aarch64::serial::enable_rx_interrupt();
    // SAFETY: the GIC distributor + CPU interface are up (the core's `irq`
    // phase ran `gic::init`) and the EL1 vectors + device dispatch are
    // installed, so a routed line is delivered to a valid handler; this routes
    // the discovered console SPI to the boot CPU.
    unsafe {
        tairix_arch_aarch64::gic::route_spi(intid, CPU0_TARGET);
    }
    let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
}

/// Publish `table` and register `production_device_irq_dispatch` with
/// the arch crate's EL1 IRQ-vector seam.
///
/// Called once per boot by
/// [`Aarch64BinArch::install_irq_dispatch`](crate::aarch64::arch_wrapper::Aarch64BinArch).
/// A second publish (a stray re-call) fails closed by halting the CPU; the boot pipeline calls it exactly once,
/// so the halt branch is unreachable in production.
pub fn install_device_irq_dispatch(table: &'static IrqTable) {
    if IRQ_TABLE_SLOT.set(table).is_err() {
        tairix_arch_aarch64::halt_current_cpu();
    }
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        if tairix_arch_aarch64::exceptions::set_device_irq_dispatch(production_device_irq_dispatch)
            .is_err()
        {
            tairix_arch_aarch64::halt_current_cpu();
        }
        // Bring the GICv2 up for delivery: enable the distributor and this
        // (boot) CPU's interface so a routed device SPI can reach the EL1
        // vector once IRQs are unmasked (`crate::aarch64::init_spawn`). Reset
        // state leaves every line disabled, so no interrupt fires until a
        // driver routes + enables its own line (the root-unlock kthread does
        // so for the virtio-blk completion SPI,
        // [`crate::unlock_service`]); enabling the controller is therefore
        // additive — it changes no behaviour until the first line is armed. It is the production counterpart of the
        // `gic::init()` the `-M virt` IRQ verticals call.
        //
        // SAFETY: the GICv2 bases were configured from the device tree
        // (`gic::configure_from_fdt`, boot discovery), the MMU is on (this
        // runs in the kernel-core `irq` phase), and this is the one-time
        // boot-CPU bring-up `gic::init` documents.
        unsafe {
            tairix_arch_aarch64::gic::init();
        }

        // Bring the console UART's shared interrupt line up now — route it to
        // the boot CPU and unmask it at the GIC — so buffered serial output is
        // **transmit-interrupt-driven** from the first boot phase
        // (`crate::aarch64::arch_wrapper`'s ring + `serial::service_uart_tx_irq`),
        // draining at the UART's real throughput regardless of scheduler state. This stays additive: the
        // device-level sources are masked at reset, so no interrupt fires
        // until a producer arms the transmit source (`serial::enable_tx_interrupt`)
        // or the login handoff enables receive (`enable_uart_console_irq`).
        // `prime_tx_irq` arms the transmit source if early-boot log output is
        // already buffered, so it starts draining without waiting for the next
        // producer. A UART-less / interrupt-less tree left the slot empty — then
        // this is skipped and output drains on the dispatch loop's non-blocking
        // top-up (`serial::pump_tx`), fail closed.
        if let Some(intid) = UART_RX_INTID.get().ok().flatten().copied() {
            // SAFETY: the GICv2 distributor bases were configured from the
            // device tree and `gic::init` ran just above, so routing this
            // discovered console SPI to the boot CPU addresses live,
            // identity-mapped distributor MMIO.
            unsafe {
                tairix_arch_aarch64::gic::route_spi(intid, CPU0_TARGET);
            }
            let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
            tairix_arch_aarch64::serial::prime_tx_irq();
        }
    }
}

/// Leak a zeroed `&'static [AtomicU64]` of `count` slots — the per-CPU
/// preemption backing sized to the *discovered* core count (never a
/// baked-in ceiling), alive for the kernel's lifetime like the other
/// boot-leaked state.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn leak_per_cpu_slots(count: usize) -> &'static [core::sync::atomic::AtomicU64] {
    use core::sync::atomic::AtomicU64;
    let mut slots = alloc::vec::Vec::with_capacity(count);
    slots.resize_with(count, || AtomicU64::new(0));
    alloc::boxed::Box::leak(slots.into_boxed_slice())
}

/// The EL0-preemption callback the arch IRQ path invokes on
/// return-to-EL0 for **any** interrupt (installed via
/// [`tairix_arch_aarch64::preempt::set_preempt_callback`]).
///
/// It reschedules **only** when this CPU owes one — i.e. the per-CPU
/// need-resched latch ([`tairix_kernel_core::take_preempt_pending`]) is
/// set. A timer quantum expiry, a cross-CPU reschedule IPI, and a device
/// IRQ that woke a higher-priority task all latch it (respectively
/// [`production_tick_dispatch`], [`production_ipi_dispatch`], and
/// [`production_device_irq_dispatch`]); an interrupt that woke nothing
/// leaves it clear, so the common case (e.g. a UART-TX drain IRQ taken
/// while EL0 runs) returns straight to user mode with **no** gratuitous
/// context switch. Consuming the latch here also means an EL1 tick — which
/// never reaches this EL0-only path — keeps its latch until the
/// interrupted syscall's completion honours it.
///
/// When a reschedule is owed it suspends the user task currently running
/// on `cpu` back to the scheduler with
/// [`tairix_kernel_core::RescheduleAction::Yield`] — the *involuntary*
/// analogue of a `yield` syscall: the task is re-enqueued at its priority
/// and the scheduler picks the next runnable task, giving EEVDF-ordered
/// time-slicing and, crucially, running `drain_pending_wakes` so work a
/// device IRQ just woke actually gets dispatched.
/// [`tairix_kernel_core::reschedule_current`] returns `false` when no
/// resumable user kthread is published on `cpu` (it cannot be reached from
/// EL0 with none switched in, but the fail-closed return means a stray
/// invocation is a harmless no-op rather than an unsound switch). The call
/// only ever runs after the GIC end-of-interrupt handshake (see
/// [`tairix_arch_aarch64::exceptions::handle_irq`]), so the interrupt line
/// is already deactivated across the context switch.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_preempt_dispatch(cpu: tairix_arch_api::CpuId) {
    let _ = tairix_kernel_core::preempt_current(cpu);
}

/// The IPI callback the SGI IRQ path invokes on every delivered
/// inter-processor interrupt (installed via
/// [`tairix_arch_aarch64::preempt::set_ipi_callback`]).
///
/// The scheduler sends an IPI when it places new or newly-woken work on
/// another CPU. Delivery alone already does the load-bearing part — it
/// pulls an idle core out of the dispatch loop's `wfi` park, whose next
/// `step` finds the queued task. The callback's own body handles the
/// busy-target case: latching the pending reschedule
/// ([`tairix_kernel_core::note_preempt_tick`]) makes an EL0 task on the
/// targeted CPU yield at its next syscall boundary, so cross-CPU
/// placement is honoured promptly on a busy core too (pure accounting,
/// safe from interrupt context; never a context switch).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_ipi_dispatch(cpu: tairix_arch_api::CpuId) {
    tairix_kernel_core::note_preempt_tick(cpu);
}

/// The per-tick callback the timer IRQ path invokes on **every** tick
/// (EL0 *or* idle EL1), installed via
/// [`tairix_arch_aarch64::preempt::set_timer_callback`].
///
/// It latches the fired tick as this CPU's pending preemption
/// ([`tairix_kernel_core::note_preempt_tick`]) and runs the blocking-wait
/// timed-wake sweep (Design D P-2): any waiter whose finite deadline has
/// elapsed is unparked and the one-shot is re-armed to the next pending
/// deadline ([`tairix_kernel_core::timed_wake_sweep`]). This is what makes
/// a finite `hw_tree_wait` timeout fire even when the CPU is otherwise
/// idle (every task parked) and takes no preemption tick. Both halves are
/// pure accounting — they never context-switch — so they are safe on a
/// tick taken in EL1; the *immediate* preemption of an EL0 task is the
/// separate [`production_preempt_dispatch`] EL0-only callback, while a
/// tick taken in EL1 is honoured through the latch at the interrupted
/// syscall's completion — the running task's quantum is never silently
/// lost to a tick the non-preemptible kernel could not act on.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_tick_dispatch(cpu: tairix_arch_api::CpuId) {
    tairix_kernel_core::note_preempt_tick(cpu);
    tairix_kernel_core::timed_wake_sweep();
    // Sample the stall watchdog: a tick still fires on a CPU whose task is
    // looping without returning to the scheduler (interrupts stay
    // deliverable in-kernel), so this is where a soft lockup on `cpu`
    // becomes observable and is reported.
    tairix_kernel_core::check_stall(cpu);
}

/// The cadence callback the lockup-watchdog's virtual-timer IRQ path
/// invokes on every ~1 Hz sample (installed via
/// [`tairix_arch_aarch64::watchdog::set_watchdog_callback`]).
///
/// It reads what this CPU interrupted — the return PC (`ELR_EL1`) and
/// processor state (`SPSR_EL1`, from which the exception level says whether
/// the CPU was in the kernel) — packages it as the architecture-neutral
/// [`tairix_arch_api::WatchdogSample`], and hands it to the detector
/// (`kernel/core`). The detector stamps this CPU's liveness heartbeat,
/// records the sample as its last-known context (the "why"), and scans the
/// other CPUs for a lockup only this cross-CPU path can see — a CPU that
/// has stopped taking even this sample. Reading `ELR`/`SPSR` here is sound:
/// they hold the interrupted state throughout the handler, until the
/// `eret`. Before the monotonic clock hook is installed the sample is
/// skipped (fail-safe).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_watchdog_dispatch(cpu: tairix_arch_api::CpuId, frame: *const u64) {
    let Some(now_ns) = tairix_kernel_core::wait_now_ns() else {
        return;
    };
    let spsr = tairix_arch_aarch64::watchdog::read_spsr_el1();
    let in_kernel = tairix_arch_aarch64::watchdog::spsr_in_kernel(spsr);
    let sample = tairix_arch_api::WatchdogSample {
        pc: tairix_arch_aarch64::watchdog::read_elr_el1(),
        // The kernel-side running-task id is not exposed to the arch layer;
        // the PC + processor state + kernel/user verdict below are the
        // load-bearing "why", and the detector names the CPU.
        task: tairix_arch_api::WatchdogSample::NO_TASK,
        aux: spsr,
        in_kernel,
    };
    tairix_kernel_core::on_watchdog_tick(cpu, now_ns, &sample);
    // Capture the pre-silence backtrace: unwind the interrupted context from
    // the saved frame so a later hard-lockup report names the whole call
    // nest this CPU was in, not just the stale `pre_silence` PC. Only for a
    // sample that interrupted **kernel** code — a hard lockup is always an
    // in-kernel wedge (a user task is preemptible), and confining the walk
    // to the kernel stack the CPU is running on keeps it off an untrusted
    // user stack. The walk is bounded and fail-closed.
    //
    // This is a debug-diagnostics facility: a shippable image compiles the
    // stack walk out entirely (the `frame` is then unused), so the ~1 Hz
    // sample costs only the always-on liveness heartbeat above.
    #[cfg(feature = "watchdog-diagnostics")]
    if in_kernel {
        let mut bt = [0u64; tairix_kernel_core::WATCHDOG_BACKTRACE_MAX];
        // SAFETY: `frame` is the live saved register frame the trap handler
        // forwarded, and this sample ran on the interrupted kernel context's
        // own CPU, so the walk reads this CPU's live, mapped kernel stack.
        let n = unsafe { tairix_arch_aarch64::watchdog::capture_sample_backtrace(frame, &mut bt) };
        tairix_kernel_core::note_watchdog_backtrace(cpu, &bt[..n]);
    }
    #[cfg(not(feature = "watchdog-diagnostics"))]
    let _ = frame;
}

/// Set up tickless timer-driven preemption on the boot CPU: register the
/// per-CPU preempt storage, install the EL0-preemption callback, record
/// the per-quantum interval derived from `PREEMPT_TICK_HZ`, and enable
/// the timer PPI — but leave the generic timer **disarmed**. TAIRiX is
/// tickless (`NO_HZ`): the scheduler arms the one-shot to
/// one quantum only when it dispatches a task onto a contended CPU (via
/// `Aarch64Arch::set_preemption`), and disarms when a CPU runs a sole
/// task, so an otherwise-quiet core takes no timer interrupts.
///
/// Called once per boot by
/// [`Aarch64BinArch::install_irq_dispatch`](crate::aarch64::arch_wrapper::Aarch64BinArch),
/// immediately after [`install_device_irq_dispatch`] has brought the GICv2
/// up — the earliest point the timer PPI can be enabled. The PE keeps IRQs
/// masked here (the kernel-core `Irq` phase runs with `DAIF.I` set), so no
/// tick is *taken* until EL0 runs with IRQs unmasked
/// (`crate::aarch64::userentry`'s preemptible `SPSR`) or the root-unlock
/// kthread unmasks at EL1 — the armed timer simply leaves PPI 30 pending
/// until then, so this is **additive and non-regressing**: a tick taken
/// in EL0 drives `production_preempt_dispatch` immediately, and a
/// one-shot tick taken in EL1 disarms without context-switching (the
/// kernel is non-preemptible) but is latched by
/// `production_tick_dispatch` and honoured when the interrupted
/// syscall completes — an expired quantum is never silently lost. The
/// scheduler re-arms the next one-shot on its following dispatch.
///
/// No *scheduler-fairness* tick callback is installed: EEVDF is tickless
/// (fairness is advanced inside `Scheduler::step`, not by a periodic
/// count). The per-tick callback that *is* installed
/// (`production_tick_dispatch`) latches the pending preemption and runs
/// the blocking-wait timed-wake sweep (Design D P-2): it releases any
/// elapsed `hw_tree_wait`-style waiter and re-arms the one-shot to the
/// next deadline, so the timer is armed only for a real pending event —
/// a preemption quantum and/or the nearest wakeup — never a fixed
/// periodic tick.
///
/// A zero `CNTFRQ_EL0` reading (a board that does not report the counter
/// frequency) leaves the kernel cooperative rather than arming a nonsense
/// interval — fail-safe.
pub fn arm_preemption(cpu_count: u32) {
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        use tairix_arch_aarch64::preempt;

        // Per-CPU backing sized to the discovered core count and leaked
        // for the kernel's lifetime (the heap is live: this runs post-MMU
        // inside the kernel-core init phases). Set-once per boot; a stray
        // re-call or mismatched sizing fails closed by halting rather
        // than re-pointing the live per-CPU slices.
        let count = cpu_count.max(1) as usize;
        if preempt::register_preempt_slices(
            leak_per_cpu_slots(count),
            leak_per_cpu_slots(count),
            leak_per_cpu_slots(count),
            leak_per_cpu_slots(count),
        )
        .is_err()
        {
            tairix_arch_aarch64::halt_current_cpu();
        }

        // Install the EL0-preemption callback *before* arming the timer, so
        // the first tick taken from EL0 already has a handler.
        preempt::set_preempt_callback(production_preempt_dispatch);

        // Install the per-tick timed-wake sweep callback (Design D P-2), so
        // every tick — including one taken on an idle EL1 CPU armed solely
        // for a blocking-wait deadline — releases any elapsed waiter and
        // re-arms the one-shot to the next deadline.
        preempt::set_timer_callback(production_tick_dispatch);

        // Install the IPI callback before any core — this one included —
        // can receive the scheduler's placement SGI.
        preempt::set_ipi_callback(production_ipi_dispatch);

        // Install the lockup-watchdog cadence callback before arming its
        // virtual-timer, so the first sample already feeds the detector.
        tairix_arch_aarch64::watchdog::set_watchdog_callback(production_watchdog_dispatch);

        // Register the kernel image's runtime base so a debug-diagnostics
        // build renders lockup-report program counters image-relative
        // (`+0x…`) rather than absolute — the `%pK`-style discipline that
        // keeps the (KASLR-relocatable) load base secret. A shippable image
        // compiles this out along with the whole address-bearing detail.
        #[cfg(feature = "watchdog-diagnostics")]
        tairix_kernel_core::set_kernel_image_base(crate::aarch64::boot::kernel_start_addr());

        // Wire the lock-site diagnostics once, on the boot CPU: the
        // `tairix_sync` lock observer is process-global (the resolver reads
        // each core's own banked dense id), so a single install covers every
        // core. After this a hard-lockup report names the exact spinlock a
        // wedged core is stuck on (`k_lock`) — the culprit the maskable
        // watchdog sample cannot observe when the core wedges with
        // interrupts off inside the critical section. A shippable image
        // compiles this out along with the whole facility.
        #[cfg(feature = "watchdog-diagnostics")]
        tairix_kernel_core::install_lock_diagnostics(lock_diagnostics_current_cpu);

        // Derive the tick interval from the discovered counter frequency
        // (never a board constant). A zero reading is a
        // fail-safe skip.
        let counter_hz = tairix_arch_aarch64::kernel_arch::read_cntfrq();
        if counter_hz == 0 {
            return;
        }
        let interval = preempt::interval_for_hz(counter_hz, PREEMPT_TICK_HZ);

        // Probe non-secure FIQ (Group 0) deliverability once on the boot
        // CPU, before arming the cadence. If the self-sample can reach a
        // `DAIF.I`-masked wedge (a single-Security-state GIC — measured:
        // QEMU `virt` with `secure=off`), the watchdog routes its cadence to
        // Group 0 as a non-maskable FIQ; if not (a two-Security-state GIC —
        // QEMU `virt,secure=on` or a real Pi 4 GIC-400, where Group 0 is
        // secure) it falls back to the complete cross-CPU buddy detector with
        // no broken channel — the empirical, fail-closed capability the D13
        // masked-section sampler consumes (`plans/WATCHDOG.md`,
        // `plans/FIX-HARDWARE-FEATURES.md`). Debug image only; a shippable
        // image compiles this and the whole FIQ path out.
        #[cfg(feature = "watchdog-diagnostics")]
        // SAFETY: boot CPU during bring-up; the GIC is up
        // (`install_device_irq_dispatch` ran) and the EL1 vector table is
        // installed (`boot::init_vectors`), with IRQs still masked.
        let _ = unsafe { tairix_arch_aarch64::watchdog::probe_fiq_deliverability(counter_hz) };

        // SAFETY: this is the boot CPU (id 0); the preempt and IPI
        // callbacks are installed (above), the per-CPU storage is
        // registered (above), the EL1 vector table is installed
        // (`boot::init_vectors`), and the GIC is up
        // (`install_device_irq_dispatch` ran immediately before).
        // `enable_ipi` unmasks this core's SGI line so a secondary's
        // placement IPI reaches the boot CPU; `init_local_preempt`
        // records the quantum, enables the timer PPI, and leaves the
        // timer disarmed — the scheduler arms the first one-shot on its
        // next dispatch onto a contended CPU (tickless).
        //
        // The lockup watchdog's ~1 Hz virtual-timer cadence is armed here
        // too, and — unlike the tickless preemption one-shot — stays armed
        // for the CPU's lifetime, so every core keeps a fresh liveness
        // heartbeat and runs the cross-CPU lockup scan even when idle. The
        // interval is one second of counter ticks (`counter_hz`), the same
        // discovered frequency the preempt interval derives from.
        unsafe {
            preempt::enable_ipi();
            preempt::init_local_preempt(0, interval);
            tairix_arch_aarch64::watchdog::init_local_watchdog(counter_hz);
        }
    }
    #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
    {
        let _ = cpu_count;
    }
}

/// Arm tickless timer-driven preemption on a freshly-started secondary
/// core: record its per-quantum interval and enable its timer PPI and
/// IPI SGI, leaving the one-shot disarmed exactly as the boot CPU's
/// [`arm_preemption`] does.
///
/// Called on the secondary core itself, from the production secondary
/// entry, after its EL1 vectors and GICv2 CPU interface are up. The
/// callbacks and the per-CPU backing were installed once by the boot
/// CPU's `arm_preemption` before any `CPU_ON` was issued, so this is
/// purely the per-core half. A zero counter frequency leaves the core
/// cooperative rather than arming a nonsense interval (fail-safe, same
/// as the boot CPU).
pub fn init_secondary_preemption(cpu: tairix_arch_api::CpuId) {
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        use tairix_arch_aarch64::preempt;

        let counter_hz = tairix_arch_aarch64::kernel_arch::read_cntfrq();
        if counter_hz == 0 {
            return;
        }
        let interval = preempt::interval_for_hz(counter_hz, PREEMPT_TICK_HZ);
        // SAFETY: runs on `cpu` itself during its bring-up, before it
        // enters the dispatch loop: its EL1 vectors and GICv2 CPU
        // interface are installed (the secondary entry ran them just
        // before), and the callbacks + per-CPU storage were registered by
        // the boot CPU before it issued `CPU_ON` for this core.
        unsafe {
            preempt::enable_ipi();
            preempt::init_local_preempt(cpu, interval);
            // Arm this secondary's lockup-watchdog cadence too (the boot
            // CPU installed the shared callback before `CPU_ON`).
            tairix_arch_aarch64::watchdog::init_local_watchdog(counter_hz);
        }
    }
    #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
    {
        let _ = cpu;
    }
}

/// The lock-diagnostics current-CPU resolver: the running core's dense id,
/// read from its banked `TPIDR_EL1` per-CPU word (`current_cpu_index`) — a
/// lock-free register read, so it is safe to call from *inside* the lock
/// primitives (the observer must never take a lock). Always `Some`; a core
/// that has not yet published its dense id reads the boot default `0`, and
/// the observer's per-CPU-slot lookup fails closed on any out-of-range id.
/// Present only in a debug-diagnostics freestanding build.
#[cfg(all(feature = "watchdog-diagnostics", freestanding, kernel_isa = "aarch64"))]
fn lock_diagnostics_current_cpu() -> Option<tairix_arch_api::CpuId> {
    Some(tairix_arch_aarch64::smp::current_cpu_index())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_aarch64::gic::{Gicv2, MAX_INTID};

    /// A host-side [`GicMmio`] that records the last distributor word
    /// written so a test can assert the controller cleared the right
    /// enable bit when masking a line.
    #[derive(Default)]
    struct MockGicMmio {
        last_icenabler_off: core::cell::Cell<usize>,
        last_icenabler_val: core::cell::Cell<u32>,
    }

    impl GicMmio for MockGicMmio {
        fn gicd_read(&self, _off: usize) -> u32 {
            0
        }
        fn gicd_write(&self, off: usize, val: u32) {
            // ICENABLER lives at 0x180..; record the disable write.
            if (0x180..0x200).contains(&off) {
                self.last_icenabler_off.set(off);
                self.last_icenabler_val.set(val);
            }
        }
        fn gicd_write_byte(&self, _off: usize, _val: u8) {}
        fn gicc_read(&self, _off: usize) -> u32 {
            0
        }
        fn gicc_write(&self, _off: usize, _val: u32) {}
        fn publish_barrier(&self) {}
    }

    // SAFETY: the mock holds only `Cell`s and is never shared across
    // threads in these single-threaded host tests; the `Send + Sync`
    // bound `GicIrqController` requires is satisfied trivially because the
    // test constructs and drops it on one thread.
    unsafe impl Send for MockGicMmio {}
    unsafe impl Sync for MockGicMmio {}

    fn controller(max_intid: u32) -> GicIrqController<MockGicMmio> {
        GicIrqController::new(GicController::new(
            Gicv2::new(MockGicMmio::default()),
            max_intid,
        ))
    }

    #[test]
    fn mask_delegates_to_the_gic_controller_for_an_in_range_line() {
        // A device SPI (INTID 32 = SPI 0) is in range and masks cleanly.
        let c = controller(MAX_INTID);
        assert_eq!(c.mask(32), Ok(()));
    }

    #[test]
    fn mask_maps_an_out_of_range_line_to_out_of_range() {
        // A controller whose ceiling is INTID 47 refuses INTID 48,
        // surfacing the arch `OutOfRange` as the kernel `MaskError`.
        let c = controller(47);
        assert_eq!(c.mask(48), Err(MaskError::OutOfRange));
    }

    #[test]
    fn rearm_unmasks_an_in_range_line() {
        // Re-arming a device SPI delegates to the arch controller's
        // unmask and succeeds for an in-range line (the re-arm lives in the bin layer that owns the GIC).
        let c = controller(MAX_INTID);
        assert_eq!(c.rearm(32), Ok(()));
    }

    #[test]
    fn rearm_maps_an_out_of_range_line_to_out_of_range() {
        // A line above the controller's ceiling fails closed without
        // touching the distributor.
        let c = controller(47);
        assert_eq!(c.rearm(48), Err(MaskError::OutOfRange));
    }
}
