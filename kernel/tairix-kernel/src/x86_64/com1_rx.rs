//! Interrupt-driven receive for the x86_64 COM1 (16550) boot console
//! (`plans/ARCHSUPPORT.md` A3).
//!
//! Until this module the x86_64 console was write-only: its read half was
//! the fail-closed `NULL_CONSOLE_READ`, so the boot root-unlock passphrase
//! prompt and `login` could not accept typed input. This is the 16550
//! sibling of the aarch64 PL011 console path
//! (`crate::aarch64::gic_irq`): a receive interrupt drains the hardware
//! FIFO into a `kernel/core` [`ConsoleInputQueue`], whose `push` wakes the
//! reader parked in `BlockingConsoleRead`, so a keystroke wakes the parked
//! reader rather than a busy-poll of the FIFO.
//!
//! The subtle, arch-independent part — the lossless backpressured
//! FIFO→queue drain with its clear-then-recheck lost-wakeup guard — is the
//! one shared [`crate::console_uart::drain_fifo_into_console`] definition
//! both ports call; only the 16550-specific closures and the x86_64
//! interrupt-masking gate live here.
//!
//! # Receive gate
//!
//! Reader-context code (a `stream_read` syscall, the unlock kthread) runs
//! with interrupts deliverable, so the reader's own poll-and-read genuinely
//! races the receive ISR — both destructively read the same FIFO.
//! [`COM1_RX_GATE`] serialises the whole drain-and-read against the ISR by
//! masking this CPU's interrupts for the hold (`cli`/`sti` via
//! the port's one masking primitive), the x86_64 analogue of the aarch64
//! `UART_RX_GATE`. The hold is short (one FIFO drain) and the wake it publishes
//! (`console_wake`) is lock-free, so masked delivery is deferred by at most
//! that bound.
//!
//! # Flow control on the ISA edge-triggered line
//!
//! The 16550's ISA interrupt reaches the IO-APIC as an edge, so partial
//! FIFO drains cannot rely on a level re-assert the way the PL011 does.
//! When the software queue is full the drain therefore brakes at the
//! **device**: it clears the Received-Data-Available enable (IER bit 0) and
//! leaves the surplus in the hardware FIFO. Once the reader frees queue
//! space it re-enables IER, and the 16550 raises a fresh interrupt edge for
//! the bytes still in the FIFO — lossless flow control that needs no level
//! trigger.

use core::sync::atomic::{AtomicBool, Ordering};

use tairix_arch_x86_64::irqmask::{RflagsIrqControl, RflagsState};
use tairix_arch_x86_64::serial;
use tairix_kernel_core::{ConsoleInputQueue, ConsoleRead};
use tairix_sync::once::OnceCell;
use tairix_sync::{InterruptControl, IrqSafeSpinLock};

/// Mask this CPU's interrupts for a kernel-heap-allocator critical section,
/// returning the prior `RFLAGS.IF` as an opaque token.
///
/// The `fn`-pointer adapter the boot path installs into the global heap
/// (`tairix_kalloc::install_irq_control`) so the
/// allocator's lock is interrupt-safe: an interrupt taken on a CPU already
/// holding the lock can no longer reenter `alloc`/`dealloc` and spin forever
/// on the lock its own interrupted mainline holds. It masks through the port's
/// one masking primitive, so the discipline is defined once; the token exists
/// only because a `fn` pointer cannot carry the state type.
///
/// Dropping the token loses the saved interrupt state, so it must be paired
/// with [`kalloc_irq_restore`].
#[must_use]
pub fn kalloc_irq_disable() -> usize {
    <RflagsIrqControl as InterruptControl>::disable().as_token()
}

/// Restore this CPU's interrupt state from a token
/// [`kalloc_irq_disable`] returned, closing the allocator critical section.
pub fn kalloc_irq_restore(token: usize) {
    // SAFETY: `token` is the `RFLAGS.IF` state a paired `kalloc_irq_disable`
    // captured on this CPU; restoring it re-enables interrupts only if they
    // were enabled before.
    unsafe {
        <RflagsIrqControl as InterruptControl>::restore(RflagsState::from_token(token));
    }
}

/// COM1's receive type-ahead queue — the software RX ring the
/// interrupt-driven receive path fills and the console read half drains.
///
/// The same `'static` backs both the console's
/// [`tairix_kernel_core::ConsoleRead`] half (drained by the reader's
/// `stream_read`) and its [`tairix_kernel_core::ConsoleInput`] half (the
/// receive drain's push target), so a push wakes a reader parked in
/// `BlockingConsoleRead`. The x86_64 analogue of the aarch64 `UART_INPUT`.
pub static COM1_INPUT: ConsoleInputQueue = ConsoleInputQueue::new();

/// Serialises every access to COM1's receive path — the destructive
/// hardware-FIFO reads and the [`COM1_INPUT`] ring they feed — across the
/// receive ISR and the reader's own poll-and-read. See the module docs.
static COM1_RX_GATE: IrqSafeSpinLock<(), RflagsIrqControl> = IrqSafeSpinLock::new(());

/// `true` while the receive drain has braked the device (cleared IER bit 0)
/// on a full [`COM1_INPUT`] queue. The reader re-opens the brake once it
/// frees queue space ([`rearm_com1_rx_if_masked`]).
static COM1_RX_MASKED: AtomicBool = AtomicBool::new(false);

/// The IO-APIC GSI COM1's interrupt is routed to, resolved from the MADT at
/// boot (ISA IRQ 4 through any interrupt-source override, else identity) and
/// published by [`set_com1_console_gsi`]. The external-IRQ dispatch consults
/// it to recognise the console line, and [`enable_uart_console_irq`] unmasks
/// its IO-APIC pin. Empty until boot resolves it — then the console receive
/// stays disabled and the reader falls back to the poll-backed path.
static COM1_GSI: OnceCell<u32> = OnceCell::new();

/// Publish the resolved COM1 receive GSI. Called once from the boot
/// pipeline after the IO-APIC layout is programmed. A second publish is a
/// benign no-op (the boot path calls it once).
pub fn set_com1_console_gsi(gsi: u32) {
    let _ = COM1_GSI.set(gsi);
}

/// The published COM1 receive GSI, or `None` before boot resolves it.
#[must_use]
pub fn com1_console_gsi() -> Option<u32> {
    COM1_GSI.get().ok().flatten().copied()
}

/// Drain COM1's hardware receive FIFO into the console queue under the
/// receive gate, waking the parked reader.
///
/// Invoked from interrupt context by the external-IRQ dispatch
/// ([`crate::x86_64::arch_wrapper::production_external_irq_dispatch`]) when
/// the fired GSI is [`com1_console_gsi`]. Lossless and bounded: it moves at
/// most one queue-capacity per call and leaves any surplus in the FIFO,
/// braking the device (IER) when the queue is full.
pub fn drain_com1_into_console() {
    let _gate = COM1_RX_GATE.lock();
    drain_com1_locked();
}

/// The shared FIFO→queue drain body. Callers **must** hold [`COM1_RX_GATE`]:
/// the FIFO reads are destructive, so two concurrent drains reorder or
/// duplicate input.
fn drain_com1_locked() {
    let queue = &COM1_INPUT;
    // Push through the installed console *device* so its cooked-mode line
    // discipline sees each byte at arrival time (a `^C`/`^Z` reaches the
    // foreground job even with no reader).
    let console = crate::x86_64::serial_sink::com1_console_device();
    crate::console_uart::drain_fifo_into_console(
        console,
        queue,
        serial::read_console_bytes,
        // The 16550 has no receive-timeout latch to clear (unlike the PL011):
        // emptying the FIFO deasserts the line, so the recheck read alone
        // closes the last-byte race.
        || {},
        || {
            // Flow-control brake at the device: disable the receive
            // interrupt and leave the surplus in the FIFO. The reader
            // re-enables it once space frees ([`rearm_com1_rx_if_masked`]),
            // and the 16550 raises a fresh edge for the bytes still queued.
            serial::disable_rx_interrupt();
            COM1_RX_MASKED.store(true, Ordering::Release);
        },
    );
}

/// Synchronously drain COM1's FIFO into the queue and read from the queue,
/// all from the **reader's** own context under one [`COM1_RX_GATE`] hold.
///
/// Makes console input **poll-backed**: the reader pulls any byte already in
/// the hardware FIFO directly, so it only parks when the FIFO *and* the
/// queue are empty, closing every residual interrupt-delivery race. The
/// interrupt remains only the wake that unparks a genuinely-parked reader.
///
/// # Errors
///
/// Propagates the queue read's error (the queue read is infallible; the
/// `Result` mirrors the [`ConsoleRead`] contract).
pub fn poll_and_read_com1(buf: &mut [u8]) -> Result<usize, tairix_abi::Errno> {
    let _gate = COM1_RX_GATE.lock();
    drain_com1_locked();
    COM1_INPUT.read(buf)
}

/// Re-enable COM1's receive interrupt if the drain braked the device on a
/// full queue. Called from the reader's drain path after it frees queue
/// space; a cheap flag check on the common (not-braked) path.
pub fn rearm_com1_rx_if_masked() {
    if !COM1_RX_MASKED.swap(false, Ordering::AcqRel) {
        return;
    }
    serial::enable_rx_interrupt();
}

/// Enable COM1's interrupt-driven receive: arm the device's
/// Received-Data-Available interrupt and unmask its IO-APIC pin, so a
/// keystroke wakes the parked reader.
///
/// Idempotent, and called at the start of the interactive session (the
/// unlock kthread before its passphrase prompt, and again at the `login`
/// handoff). A boot that could not resolve the console GSI leaves the slot
/// empty and this a no-op — the reader stays on the poll-backed path rather
/// than failing (fail closed).
pub fn enable_uart_console_irq() {
    let Some(gsi) = com1_console_gsi() else {
        return;
    };
    // Enable the receive interrupt at the device first, then unmask its
    // IO-APIC pin, so the first delivered edge already has a drain target.
    serial::enable_rx_interrupt();
    if let Some(controller) = crate::x86_64::ioapic_controller::published_typed() {
        // The pin was programmed (masked) at boot with its vector; unmask it
        // so the line reaches the CPU. A GSI no block owns is a fail-closed
        // no-op.
        let _ = controller.unmask(gsi);
    }
}

/// COM1's console **read** half: drains the interrupt-fed [`COM1_INPUT`]
/// queue (poll-backed via [`poll_and_read_com1`]) and, after freeing space,
/// re-opens the receive brake if the drain braked the device on a full
/// queue. The x86_64 analogue of the aarch64 `UartConsoleRead`.
#[derive(Debug, Default, Copy, Clone)]
pub struct Com1ConsoleRead;

impl ConsoleRead for Com1ConsoleRead {
    fn read(&self, buf: &mut [u8]) -> Result<usize, tairix_abi::Errno> {
        let read = poll_and_read_com1(buf)?;
        // Draining the queue may have freed the space the drain braked on:
        // re-enable the receive interrupt so input resumes (flow control
        // released). A cheap flag check on the common path.
        rearm_com1_rx_if_masked();
        Ok(read)
    }
}

/// The single `'static` [`Com1ConsoleRead`] the console list installs as
/// COM1's read half (wrapped, for console 0, in the unlock ownership gate).
pub static COM1_CONSOLE_READ: Com1ConsoleRead = Com1ConsoleRead;
