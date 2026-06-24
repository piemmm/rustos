//! Production aarch64 device-IRQ wiring (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (1)).
//!
//! Brings the kernel-wide [`rustos_kernel_irq::IrqTable`] to life on the
//! aarch64 boot path so a discovered device's shared-peripheral interrupt
//! (SPI) can be bound and a task parked on it is woken when the GIC
//! delivers the line. It is the aarch64 analogue of the x86_64
//! `IoApicController` + `production_external_irq_dispatch` wiring in
//! [`crate::x86_64::arch_wrapper`]; before it, the aarch64 port kept the
//! conservative fail-closed [`rustos_kernel_core::IrqRouting::unsupported`]
//! default and delivered no device interrupts at all.
//!
//! Three pieces compose the path the kernel core (`Phase::Irq`) drives
//! through [`rustos_kernel_core::KernelArch::irq_routing`] /
//! [`rustos_kernel_core::KernelArch::install_irq_dispatch`]:
//!
//! 1. [`GicIrqController`] — a kernel-side [`IrqController`] over the
//!    arch port's validated [`GicController`]. The arch crate cannot
//!    depend on `kernel/irq`, so the bridge from the
//!    arch HAL [`rustos_arch_api::IrqController`] to the
//!    [`rustos_kernel_irq::IrqController`] [`IrqTable::fire`] consumes
//!    lives here, in the kernel binary, exactly like the x86_64
//!    `IoApicController` does. It adds **no** masking policy of its own —
//!    it delegates to the range-checked, fence-ordered [`GicController`].
//! 2. [`gic_irq_routing`] — the [`IrqRouting`] the boot path hands
//!    [`crate::aarch64::arch_wrapper::Aarch64BinArch`], naming the
//!    `'static` [`GIC_IRQ_CONTROLLER`] and the GICv2 maximum INTID as the
//!    bind ceiling.
//! 3. [`install_device_irq_dispatch`] — publishes the live `IrqTable`
//!    into a set-once slot and registers [`production_device_irq_dispatch`]
//!    with the arch crate's EL1 IRQ-vector seam
//!    ([`rustos_arch_aarch64::exceptions::set_device_irq_dispatch`]). The
//!    EL1 IRQ handler acknowledges the GIC, forwards every non-timer INTID
//!    here, and issues the end-of-interrupt itself; this dispatcher only
//!    translates the acknowledged INTID into an [`IrqTable::fire`] (which
//!    masks the line before a waiter observes the wake —
//!    `docs/src/security/irq.md`).
//!
//! The wiring is **additive and non-regressing**: no
//! device SPI is bound or routed until INCREMENT (2)'s unlock kthread does
//! so, and [`production_device_irq_dispatch`] is only ever reached for a
//! non-timer INTID the GIC delivers — which cannot occur until a line is
//! routed — so the metal-confirmed boot is unaffected.

// `AtomicBool`/`Ordering` back the freestanding-only UART receive
// flow-control flag, and `IrqRouting` is returned only by the
// freestanding `gic_irq_routing`; on a host build neither is used (the
// host `KernelArch::irq_routing` returns the unsupported default from
// `arch_wrapper`), so both imports are gated to where they compile rather
// than left unused under clippy's `-D warnings`.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_arch_aarch64::gic::{GicController, GicMmio};
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
use rustos_kernel_core::IrqRouting;
use rustos_kernel_irq::{IrqController, IrqTable, MaskError};
use rustos_sync::once::OnceCell;

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
/// EEVDF virtual-deadline order, `kernel/sched`). RustOS is tickless: a CPU running a sole task disarms and takes no
/// ticks. The rate is the shared
/// [`DEFAULT_PREEMPT_QUANTUM_HZ`](rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ)
/// the riscv64 port also uses — defined once so the two ports cannot
/// diverge.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
const PREEMPT_TICK_HZ: u64 = rustos_arch_api::timer::DEFAULT_PREEMPT_QUANTUM_HZ;

/// A kernel-side [`IrqController`] over the arch port's [`GicController`].
///
/// Wraps the validated GICv2 controller and re-exposes its line masking
/// through the [`rustos_kernel_irq::IrqController`] trait
/// [`IrqTable::fire`] requires. The wrapper exists only to satisfy the
/// orphan rule (both the trait and `GicController` are foreign to the arch
/// crate's dependency island) and adds no policy: every `mask` is the
/// arch controller's range-checked, `SeqCst`-fenced
/// [`rustos_arch_api::IrqController::mask`] (the
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
    /// *arch* operation ([`rustos_arch_api::IrqController::unmask`]) the
    /// kernel-side [`rustos_kernel_irq::IrqController`] trait's [`mask`] half
    /// deliberately does not expose. The in-kernel block path's
    /// [`crate::aarch64::root_unlock`] waiter calls this directly (it routes
    /// the SPI itself, once, at setup); the user-space `irq_wait` park path
    /// goes through the trait [`rearm`](IrqController::rearm), which *also*
    /// routes. Both delegate to the range-checked [`GicController`].
    ///
    /// # Errors
    ///
    /// Surfaces [`rustos_arch_api::IrqControlError`] verbatim — an
    /// out-of-range line fails closed without touching the distributor.
    pub fn unmask_line(&self, line: u32) -> Result<(), rustos_arch_api::IrqControlError> {
        use rustos_arch_api::IrqController as ArchIrqController;
        ArchIrqController::unmask(&self.inner, line)
    }
}

/// GICv2 distributor target byte selecting the boot CPU (CPU interface 0).
///
/// Production is single-CPU today (`BootInfo::new(BOOT_CPU, 1, …)`), so a
/// device SPI is routed to CPU 0; secondary-core routing is selected from
/// the discovered core count when SMP bring-up lands.
/// Defined once here so the in-kernel block path and the user-space-driver
/// re-arm path route through the same value.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub const CPU0_TARGET: u8 = 0b0000_0001;

/// The `'static` [`IrqTable`] the kernel core published in
/// [`crate::Phase::Irq`](rustos_kernel_core::Phase::Irq) through
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
    /// [`rustos_arch_api::IrqControlError`] onto the
    /// [`rustos_kernel_irq::MaskError`] [`IrqTable::fire`] expects.
    ///
    /// An out-of-range line maps to [`MaskError::OutOfRange`]; any other
    /// arch-side refusal maps to [`MaskError::Unsupported`] so the table
    /// surfaces it as the standard architecture-unsupported outcome
    /// (fail closed).
    fn mask(&self, line: u32) -> Result<(), MaskError> {
        use rustos_arch_api::{IrqControlError, IrqController as ArchIrqController};
        match ArchIrqController::mask(&self.inner, line) {
            Ok(()) => Ok(()),
            Err(IrqControlError::OutOfRange) => Err(MaskError::OutOfRange),
        }
    }

    /// Route `line` to the boot CPU and unmask it at the distributor.
    ///
    /// This is the re-arm the user-space `irq_wait` park path drives on an
    /// interrupt-driven driver's behalf (the driver holds no GIC access): it
    /// routes the SPI to [`CPU0_TARGET`] (idempotent — re-routing an
    /// already-targeted line is a plain register write) and then clears its
    /// enable mask through the same range-checked, fence-ordered
    /// [`GicController`] unmask the in-kernel block path uses. An out-of-range line fails closed as [`MaskError::OutOfRange`]
    /// without touching the distributor.
    fn rearm(&self, line: u32) -> Result<(), MaskError> {
        use rustos_arch_api::{IrqControlError, IrqController as ArchIrqController};
        // SAFETY: the GICv2 distributor bases were configured from the device
        // tree and the controller brought up (`install_device_irq_dispatch`
        // → `gic::init`) before any line is bound, so the target-register
        // write addresses live, identity-mapped distributor MMIO. `route_spi`
        // ignores SGIs/PPIs and only writes the SPI target byte.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        unsafe {
            rustos_arch_aarch64::gic::route_spi(line, CPU0_TARGET);
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
/// [`production_device_irq_dispatch`] reads it from interrupt context to
/// translate an acknowledged GIC INTID into an [`IrqTable::fire`]. The
/// [`OnceCell`] enforces the one-shot-publish invariant (no global mutable state; this is a publish-once pointer).
static IRQ_TABLE_SLOT: OnceCell<&'static IrqTable> = OnceCell::new();

/// Set-once slot for the console UART's discovered GIC SPI INTID (the
/// `arm,pl011` / mini-UART node's `interrupts`, decoded from the firmware
/// device tree — a discovered value, never a board constant).
///
/// The boot path records it ([`set_uart_console_intid`]) when it parses the
/// device tree, and the unlock kthread's console handoff
/// ([`enable_uart_console_irq`]) routes + unmasks it once the passphrase
/// poll is over. [`production_device_irq_dispatch`] reads it from interrupt
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
/// ([`rustos_arch_aarch64::gic::MAX_INTID`]); a device SPI is bound below
/// it and the table refuses any line above it.
///
/// Freestanding-only: [`VolatileGicMmio`] performs real MMIO and exists
/// only on the bare-metal target. Host builds return
/// [`IrqRouting::unsupported`] from [`Aarch64BinArch::irq_routing`]
/// instead.
///
/// [`VolatileGicMmio`]: rustos_arch_aarch64::gic::VolatileGicMmio
/// [`Aarch64BinArch::irq_routing`]: crate::aarch64::arch_wrapper::Aarch64BinArch
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub static GIC_IRQ_CONTROLLER: GicIrqController<rustos_arch_aarch64::gic::VolatileGicMmio> =
    GicIrqController::new(GicController::new(
        rustos_arch_aarch64::gic::Gicv2::new(rustos_arch_aarch64::gic::VolatileGicMmio),
        rustos_arch_aarch64::gic::MAX_INTID,
    ));

/// The [`IrqRouting`] the aarch64 boot path installs: the GICv2 controller
/// plus the GICv2 maximum INTID as the inclusive bind ceiling.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
#[must_use]
pub fn gic_irq_routing() -> IrqRouting {
    IrqRouting {
        max_line: rustos_arch_aarch64::gic::MAX_INTID,
        controller: &GIC_IRQ_CONTROLLER,
    }
}

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
/// the table's own [`rustos_kernel_irq::WaitStep`] taxonomy, and the line is
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
        let rx_pending = rustos_arch_aarch64::serial::service_uart_tx_irq();
        if rx_pending {
            drain_uart_into_console_queue();
        }
        return;
    }
    let Ok(Some(table)) = IRQ_TABLE_SLOT.get() else {
        return;
    };
    let _ = table.fire(intid, &GIC_IRQ_CONTROLLER);
    // Wake any `irq_wait` caller parked on a bound line: `fire` set the
    // per-line ready flag (after masking — mask-before-wake holds), so a
    // woken waiter that consumes it observes the mask. A spurious wake for
    // a waiter on a different line is harmless — it re-checks its own line
    // and parks again. Wait-free and
    // allocation-free, safe from this interrupt context.
    rustos_kernel_core::irq_wake();
}

/// Drain the console UART's hardware receive FIFO into the UART console's
/// receive queue, waking the parked `login` reader.
///
/// Invoked from interrupt context by [`production_device_irq_dispatch`] when
/// the console's receive interrupt fires. Each `push` enqueues the bytes and
/// wakes any reader parked in kernel-core's `BlockingConsoleRead`
/// ([`rustos_kernel_core::ConsoleInputQueue::push`] →
/// `crate::waitq::console_wake`). It is **bounded** by the console queue's
/// free space (at most one queue capacity per interrupt) and **lossless**:
/// it dequeues from the FIFO only what the queue can accept and leaves any
/// surplus in the FIFO, so the level-sensitive receive interrupt re-fires as
/// the reader drains the queue. Wait-free and
/// allocation-free.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
fn drain_uart_into_console_queue() {
    use rustos_kernel_core::ConsoleInput as _;
    let queue = &crate::aarch64::arch_wrapper::UART_INPUT;
    loop {
        // Lossless backpressure: dequeue from the hardware FIFO only what the
        // console queue can accept this instant. Reading more would force the
        // surplus to be dropped (the bytes leave the FIFO but the queue's
        // `push` is short), which truncates a `login` line — including its
        // terminating newline — and wedges the line-oriented reader. Leaving
        // the surplus in the FIFO keeps the receive interrupt asserted; it
        // re-fires once `login` drains the queue and frees space, so input
        // streams through a sliding window with no byte lost (the software
        // analogue of the FIFO's own flow control). `login`
        // is already runnable (the first push woke it), so it drains promptly
        // and the re-fire is progress, not a storm.
        let free = queue.free_capacity();
        if free == 0 {
            // The console queue is full and the reader has not yet drained
            // it. Spinning here (re-reading a full queue) would storm the CPU
            // and starve the reader, so apply flow control: **mask** the
            // receive line at the GIC and leave the surplus in the hardware
            // FIFO. [`rearm_uart_rx_if_masked`], called from the reader's
            // drain path, re-enables the line once space frees, and the
            // level-sensitive line re-asserts on the bytes still in the FIFO
            // — the software analogue of a hardware FIFO releasing flow
            // control, with no byte lost and no storm.
            if let Some(intid) = UART_RX_INTID.get().ok().flatten().copied() {
                let _ = GIC_IRQ_CONTROLLER.mask(intid);
                UART_RX_MASKED.store(true, Ordering::Release);
            }
            break;
        }
        let mut buf = [0u8; 32];
        let want = free.min(buf.len());
        let n = rustos_arch_aarch64::serial::read_console_bytes(&mut buf[..want]);
        if n == 0 {
            // The FIFO read empty: clear the latched receive / receive-timeout
            // interrupt so the line deasserts. The PL011 receive-timeout latch
            // is *not* cleared by emptying the FIFO, so without this the line
            // stays asserted and this ISR re-fires forever, starving every
            // other task.
            rustos_arch_aarch64::serial::clear_rx_interrupt();
            // Clear-then-recheck (no lost byte): a byte the device latched in
            // the window *between* the empty read above and the clear just
            // issued would have had its interrupt cleared with it — stranding
            // it in the FIFO with no re-fire, which wedges the parked
            // line-oriented `login` reader forever (the lost-wakeup race
            // the charter forbids). So read once more: a byte that raced
            // in before the clear is drained now, and any byte that arrives
            // *after* the clear latches a fresh, uncleared interrupt that
            // re-fires this ISR. Only a genuinely still-empty FIFO ends the
            // drain.
            let n2 = rustos_arch_aarch64::serial::read_console_bytes(&mut buf[..want]);
            if n2 == 0 {
                break;
            }
            // `n2 <= want <= free`: the raced-in chunk fits and its push wakes
            // the parked reader. Loop to keep draining (and to re-clear on the
            // next genuine empty).
            let _ = queue.push(&buf[..n2]);
            continue;
        }
        // `n <= want <= free`, so the whole chunk fits and the push wakes the
        // parked reader (`ConsoleInputQueue::push` → `console_wake`).
        let _ = queue.push(&buf[..n]);
    }
}

/// Synchronously drain the console UART's hardware receive FIFO into the
/// receive queue from the **reader's** context (a `stream_read` syscall),
/// not interrupt context.
///
/// Called by [`crate::aarch64::arch_wrapper::UartConsoleRead::read`] on the
/// path that is about to park an empty-handed reader. It makes console input
/// **poll-backed**, not solely interrupt-driven: the reader pulls any byte
/// already sitting in the hardware FIFO directly, so it only ever parks when
/// the FIFO *and* the software queue are genuinely empty. That closes every
/// residual device-IRQ-delivery race — a receive interrupt the CPU has not
/// yet taken (it is busy in the masked EL1 dispatch loop), or a sub-trigger
/// FIFO tail still awaiting the PL011 receive-timeout — because the reader no
/// longer *depends* on the interrupt to see a byte that is already in the
/// FIFO; the interrupt remains only the wake that unparks it once it has
/// parked (the park is genuine, never a busy-poll: a byte
/// arriving after this drain raises the interrupt that wakes the parked task).
///
/// Runs in an EL1 syscall with IRQ taking masked, so it cannot race the ISR
/// ([`drain_uart_into_console_queue`]) on this single console; the shared
/// drain body is the one definition both entry points reuse.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub fn poll_uart_into_console_queue() {
    drain_uart_into_console_queue();
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
    rustos_arch_aarch64::serial::enable_rx_interrupt();
    // SAFETY: the GIC distributor + CPU interface are up (the core's `irq`
    // phase ran `gic::init`) and the EL1 vectors + device dispatch are
    // installed, so a routed line is delivered to a valid handler; this routes
    // the discovered console SPI to the boot CPU.
    unsafe {
        rustos_arch_aarch64::gic::route_spi(intid, CPU0_TARGET);
    }
    let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
}

/// Publish `table` and register [`production_device_irq_dispatch`] with
/// the arch crate's EL1 IRQ-vector seam.
///
/// Called once per boot by
/// [`Aarch64BinArch::install_irq_dispatch`](crate::aarch64::arch_wrapper::Aarch64BinArch).
/// A second publish (a stray re-call) fails closed by halting the CPU; the boot pipeline calls it exactly once,
/// so the halt branch is unreachable in production.
pub fn install_device_irq_dispatch(table: &'static IrqTable) {
    if IRQ_TABLE_SLOT.set(table).is_err() {
        rustos_arch_aarch64::halt_current_cpu();
    }
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        if rustos_arch_aarch64::exceptions::set_device_irq_dispatch(production_device_irq_dispatch)
            .is_err()
        {
            rustos_arch_aarch64::halt_current_cpu();
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
            rustos_arch_aarch64::gic::init();
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
                rustos_arch_aarch64::gic::route_spi(intid, CPU0_TARGET);
            }
            let _ = GIC_IRQ_CONTROLLER.unmask_line(intid);
            rustos_arch_aarch64::serial::prime_tx_irq();
        }
    }
}

/// Caller-owned per-CPU preemption backing for the production boot CPU.
///
/// The production aarch64 image is single-CPU (`BootInfo::new(BOOT_CPU, 1,
/// …)`), so a `PreemptStorage<1>` covers it; secondary-core preemption is
/// sized from the discovered CPU count when SMP bring-up lands (the per-CPU timer bookkeeping is the discovered core count,
/// never a baked-in ceiling). Published once by [`arm_preemption`].
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static PREEMPT_STORAGE: rustos_arch_aarch64::preempt::PreemptStorage<1> =
    rustos_arch_aarch64::preempt::PreemptStorage::new();

/// The EL0-preemption callback the timer IRQ path invokes for a tick taken
/// from EL0 (installed via
/// [`rustos_arch_aarch64::preempt::set_preempt_callback`]).
///
/// It suspends the user task currently running on `cpu` back to the
/// scheduler with [`rustos_kernel_core::RescheduleAction::Yield`] — the
/// *involuntary* analogue of a `yield` syscall: the task is re-enqueued at
/// its priority and the scheduler picks the next runnable task, giving
/// EEVDF-ordered time-slicing. [`rustos_kernel_core::reschedule_current`]
/// returns `false` when no resumable user kthread is published on `cpu`
/// (it cannot be reached from EL0 with none switched in, but the
/// fail-closed return means a stray invocation is a harmless no-op rather
/// than an unsound switch). The call only ever runs
/// after the GIC end-of-interrupt handshake (see
/// [`rustos_arch_aarch64::exceptions::handle_irq`]), so the timer line is
/// already deactivated across the context switch.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_preempt_dispatch(cpu: rustos_arch_api::CpuId) {
    let _ =
        rustos_kernel_core::reschedule_current(cpu, rustos_kernel_core::RescheduleAction::Yield);
}

/// The per-tick callback the timer IRQ path invokes on **every** tick
/// (EL0 *or* idle EL1), installed via
/// [`rustos_arch_aarch64::preempt::set_timer_callback`].
///
/// It runs the blocking-wait timed-wake sweep (Design D P-2): any waiter
/// whose finite deadline has elapsed is unparked and the one-shot is
/// re-armed to the next pending deadline
/// ([`rustos_kernel_core::timed_wake_sweep`]). This is what makes a finite
/// `hw_tree_wait` timeout fire even when the CPU is otherwise idle (every
/// task parked) and takes no preemption tick. It is
/// pure accounting — it never context-switches — so it is safe on a tick
/// taken in EL1; the *preemption* of an EL0 task is the separate
/// [`production_preempt_dispatch`] EL0-only callback.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
extern "C" fn production_tick_dispatch(_cpu: rustos_arch_api::CpuId) {
    rustos_kernel_core::timed_wake_sweep();
}

/// Set up tickless timer-driven preemption on the boot CPU: register the
/// per-CPU preempt storage, install the EL0-preemption callback, record
/// the per-quantum interval derived from [`PREEMPT_TICK_HZ`], and enable
/// the timer PPI — but leave the generic timer **disarmed**. RustOS is
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
/// until then, so this is **additive and non-regressing**: a one-shot tick taken in EL1 only disarms (it never preempts —
/// the kernel is non-preemptible), and a tick taken in EL0 drives
/// [`production_preempt_dispatch`]; the scheduler re-arms the next
/// one-shot on its following dispatch.
///
/// No *scheduler-fairness* tick callback is installed: EEVDF is tickless
/// (fairness is advanced inside `Scheduler::step`, not by a periodic
/// count). The per-tick callback that *is* installed
/// ([`production_tick_dispatch`]) runs only the blocking-wait timed-wake
/// sweep (Design D P-2): it releases any elapsed `hw_tree_wait`-style
/// waiter and re-arms the one-shot to the next deadline, so the timer is
/// armed only for a real pending event — a preemption quantum and/or the
/// nearest wakeup — never a fixed periodic tick.
///
/// A zero `CNTFRQ_EL0` reading (a board that does not report the counter
/// frequency) leaves the kernel cooperative rather than arming a nonsense
/// interval — fail-safe.
pub fn arm_preemption() {
    #[cfg(all(freestanding, kernel_isa = "aarch64"))]
    {
        use rustos_arch_aarch64::preempt;

        // Set-once per boot; a stray re-call fails closed by halting rather
        // than re-pointing the live per-CPU slices.
        if PREEMPT_STORAGE.register().is_err() {
            rustos_arch_aarch64::halt_current_cpu();
        }

        // Install the EL0-preemption callback *before* arming the timer, so
        // the first tick taken from EL0 already has a handler.
        preempt::set_preempt_callback(production_preempt_dispatch);

        // Install the per-tick timed-wake sweep callback (Design D P-2), so
        // every tick — including one taken on an idle EL1 CPU armed solely
        // for a blocking-wait deadline — releases any elapsed waiter and
        // re-arms the one-shot to the next deadline.
        preempt::set_timer_callback(production_tick_dispatch);

        // Derive the tick interval from the discovered counter frequency
        // (never a board constant). A zero reading is a
        // fail-safe skip.
        let counter_hz = rustos_arch_aarch64::kernel_arch::read_cntfrq();
        if counter_hz == 0 {
            return;
        }
        let interval = preempt::interval_for_hz(counter_hz, PREEMPT_TICK_HZ);

        // SAFETY: this is the boot CPU (id 0); the preempt callback is
        // installed (above), the per-CPU storage is registered (above), the
        // EL1 vector table is installed (`boot::init_vectors`), and the GIC
        // is up (`install_device_irq_dispatch` ran immediately before). It
        // records the quantum, enables the timer PPI, and leaves the timer
        // disarmed; the scheduler arms the first one-shot on its next
        // dispatch onto a contended CPU (tickless).
        unsafe {
            preempt::init_local_preempt(0, interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_aarch64::gic::{Gicv2, MAX_INTID};

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
