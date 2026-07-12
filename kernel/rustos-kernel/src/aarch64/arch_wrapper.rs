//! In-crate [`KernelArch`] wrapper around
//! [`rustos_arch_aarch64::Aarch64Arch`], plus the [`UartConsole`]
//! [`ConsoleWrite`] device the aarch64 boot path installs on
//! [`rustos_kernel_core::BootInfo`].
//!
//! # Why a wrapper
//!
//! `rustos_kernel_core::KernelArch` is a foreign trait and
//! `rustos_arch_aarch64::Aarch64Arch` is a foreign type, so Rust's
//! coherence rules forbid implementing the trait for the type directly.
//! [`Aarch64BinArch`] is the smallest local type that owns an
//! `Aarch64Arch`, delegates the [`SchedulerArch`] super-trait, and
//! implements [`KernelArch::halt`] / [`KernelArch::monotonic_ns`] by
//! forwarding to the arch port — the orphan-rule sibling of the x86_64
//! `crate::BinArch` and the riscv64 `RiscvBinArch` (`plans/PI.md`
//! P6c-2).
//!
//! The aarch64 port wires the [`KernelArch`] interrupt-routing surface to
//! the GICv2 through [`crate::aarch64::gic_irq`]: `irq_routing` returns the
//! GICv2-backed [`rustos_kernel_core::IrqRouting`] (freestanding) and
//! `install_irq_dispatch` publishes the kernel `IrqTable` into the arch
//! crate's EL1 IRQ-vector seam, so a discovered device SPI can be bound and
//! a parked task is woken when the line fires (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (1)). On a non-freestanding host build the routing stays the
//! conservative fail-closed [`rustos_kernel_core::IrqRouting::unsupported`]
//! default (no `VolatileGicMmio` exists off the bare-metal target).

use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::entropy::PlatformRng as Aarch64PlatformEntropy;
use rustos_arch_aarch64::{halt_current_cpu, serial, Aarch64Arch};
use rustos_arch_api::{CpuId, PlatformEntropy, SchedulerArch};
use rustos_kernel_core::{ConsoleRead, ConsoleWrite, IrqRouting, KernelArch, SeatRegistry};
use rustos_kernel_irq::IrqTable;

/// Local [`KernelArch`] wrapper around the arch port's
/// [`Aarch64Arch`].
///
/// Owns the concrete arch handle and delegates every trait method to
/// it. Constructed once by the boot path (`boot_aarch64`) and handed to
/// `kernel_core::kernel_main` inside an `Arc` — the single
/// concrete-arch selection point for the
/// aarch64 kernel image.
#[derive(Debug)]
pub struct Aarch64BinArch {
    arch: Aarch64Arch,
}

impl Aarch64BinArch {
    /// Wrap `arch` so it can be handed to `kernel_core::kernel_main`.
    #[must_use]
    pub const fn new(arch: Aarch64Arch) -> Self {
        Self { arch }
    }

    /// Borrow the wrapped [`Aarch64Arch`].
    #[must_use]
    pub const fn arch(&self) -> &Aarch64Arch {
        &self.arch
    }
}

impl SchedulerArch for Aarch64BinArch {
    fn current_cpu(&self) -> CpuId {
        self.arch.current_cpu()
    }

    fn ticks_now(&self) -> u64 {
        self.arch.ticks_now()
    }

    fn send_ipi(&self, target: CpuId) {
        self.arch.send_ipi(target);
    }

    fn set_preemption(&self, armed: bool) {
        // Tickless preemption: forward the scheduler's
        // arm/disarm decision to the arch port, which programs the EL1
        // generic-timer one-shot. The default no-op would silently drop
        // preemption, so the delegation is required, not optional.
        self.arch.set_preemption(armed);
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        // Forward the nearest blocking-wait deadline to the arch port,
        // which combines it with the quantum and arms the single EL1
        // generic-timer one-shot to the earlier. The
        // default no-op would silently drop timed wakes, so the delegation
        // is required.
        self.arch.set_wakeup(deadline_ns);
    }
}

/// The `'static` aarch64 platform-entropy handle the kernel seeds its CSPRNG
/// reserve from. Zero-sized; the `FEAT_RNG` `RNDR` register is addressed
/// directly, so no per-instance state is needed.
static AARCH64_PLATFORM_ENTROPY: Aarch64PlatformEntropy = Aarch64PlatformEntropy::new();

impl KernelArch for Aarch64BinArch {
    type Cs = ContextSwitchHal;

    fn context_switch(&self) -> Self::Cs {
        ContextSwitchHal::new()
    }

    fn halt(&self) -> ! {
        halt_current_cpu()
    }

    fn platform_entropy(&self) -> Option<&'static dyn PlatformEntropy> {
        // aarch64 seeds the kernel CSPRNG reserve from the ARMv8.5 `RNDR`
        // register. The handle is zero-sized; whether `FEAT_RNG` is present
        // is decided at runtime by the port.
        Some(&AARCH64_PLATFORM_ENTROPY)
    }

    fn monotonic_ns(&self, _cpu: CpuId) -> u64 {
        self.arch.monotonic_ns()
    }

    fn arch_id(&self) -> Option<rustos_abi::Arch> {
        Some(rustos_abi::Arch::Aarch64)
    }

    fn ticks_to_ns(&self, ticks: u64) -> u64 {
        // `ticks_now` is raw `CNTPCT_EL0`, so the identity default would
        // misreport CPU time; convert against the same discovered timer
        // frequency `monotonic_ns` uses.
        self.arch.ticks_to_ns(ticks)
    }

    fn park_translation(&self) -> Option<fn()> {
        // Re-installs the boot space's `TTBR0_EL1` root (published by the
        // boot `switch()`) so no user root stays active after its task
        // suspends — the invariant a dead task's page-table reclamation
        // relies on.
        fn park() {
            // Fire-and-forget from the dispatcher: with no park root
            // published yet there is nothing to leave (fail closed), so
            // the `bool` outcome is deliberately discarded.
            let _ = rustos_arch_aarch64::paging::park_kernel_root();
        }
        Some(park)
    }

    fn wait_for_interrupt(&self) {
        // The tickless idle park. The dispatch loop
        // calls this with device IRQs already **masked** (it masked them to
        // close the park/wake race and drained any already-flagged wake), so
        // `wfi` parks the CPU until an interrupt becomes pending — it wakes
        // on a *pending-but-masked* interrupt, so an edge that asserts after
        // the drain but before this call is not lost. The loop
        // re-enables IRQs after we return, *taking* the pending interrupt
        // then (its lock-free handler flags the deferred wake the next
        // `drain_pending_wakes` consumes). On a host build there is no EL1,
        // so this is a benign no-op (the loop re-steps immediately).
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            use rustos_arch_aarch64::exceptions;
            // The dispatch loop topped up the buffered serial transmit
            // through `pump_console_tx` (below) just before calling this, so
            // `TXIM` is already armed against any backlog — the event that
            // wakes this `wfi`. This method only parks.
            //
            // SAFETY: `wfi` parks until a pending interrupt and has no other
            // architectural effect; it is called with `DAIF.I` masked, where
            // it still wakes on a pending-but-masked interrupt. The loop
            // re-enables IRQs (`set_device_irqs(true)`) after we return, so
            // the interrupt is taken then. The mask state is left exactly as
            // found (masked).
            unsafe {
                exceptions::wait_for_interrupt();
            }
        }
    }

    fn pump_console_tx(&self) {
        // Non-blocking top-up of the buffered serial transmit ring: push
        // only what the PL011 FIFO accepts right now and arm the console
        // transmit interrupt (`TXIM`) for the rest (`serial::pump_tx`), never
        // a per-byte spin. The dispatch loop calls
        // this on **every** iteration — after each dispatched task and again
        // before the idle `wfi` — so the log drains at the loop's rate even
        // while a perpetually-runnable in-kernel kthread (the polled
        // USB-keyboard report pump) keeps the loop from ever idling, and
        // independent of whether the PL011 transmit interrupt self-sustains
        // the drain on real silicon (it does not reliably on the Pi 4's
        // flow-blocked UART — the metal stall this fixes). On a host build
        // there is no PL011, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            serial::pump_tx();
        }
    }

    fn set_device_irqs(&self, enabled: bool) {
        // Toggle this CPU's PE-level IRQ taking (`DAIF.I`) so the dispatch
        // loop runs in-kernel tasks/kthreads with device interrupts enabled
        // (the fully preemptive kernel), and masks them
        // only around the idle park and before halt. On a host build there
        // is no `DAIF`, so this is a benign no-op.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            use rustos_arch_aarch64::exceptions;
            // SAFETY: `enable_irq`/`mask_irq` only toggle `DAIF.I`; the
            // vector table + GICv2 are installed by the time the dispatch
            // loop runs (`install_irq_dispatch` ran in the boot `irq`
            // phase), so a taken interrupt dispatches through a valid EL1
            // handler. A device IRQ taken in EL1 services its source and
            // returns without rescheduling the current task (the kernel is
            // non-preemptible); only the EL0 timer tick preempts.
            unsafe {
                if enabled {
                    exceptions::enable_irq();
                } else {
                    exceptions::mask_irq();
                }
            }
        }
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        {
            let _ = enabled;
        }
    }

    fn irq_routing(&self) -> IrqRouting {
        // The GICv2-backed routing the kernel core builds the `IrqTable`
        // against. On the bare-metal target this names the `'static`
        // `GIC_IRQ_CONTROLLER` over the discovered GICv2 windows; on a host
        // build there is no `VolatileGicMmio`, so the routing stays the
        // conservative fail-closed default.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            crate::aarch64::gic_irq::gic_irq_routing()
        }
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        {
            IrqRouting::unsupported()
        }
    }

    fn msi_alloc_facility(
        &self,
    ) -> Option<&'static (dyn rustos_kernel_core::MsiAllocFacility + 'static)> {
        // The Pi 4's BCM2711 root-complex MSI controller backs `msi_alloc` so
        // a user-space bus driver can wire the VL805 xHCI for message-signalled
        // interrupts. On a host build there is no `VolatileMsiMmio`, so no
        // facility is offered and `msi_alloc` stays fail-closed.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            Some(&crate::aarch64::gic_irq::BRCM_MSI_ALLOC_FACILITY)
        }
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        {
            None
        }
    }

    fn direct_phys_map(&self) -> Option<&'static (dyn rustos_kernel_mem::PhysMap + Sync)> {
        // The configured-identity direct map (`virtual == physical` over the
        // discovered RAM/Device gigapages) the shared-memory facility scrubs
        // region frames through. On a host build there is no real RAM to map,
        // so none is offered and `shm_*` stays fail-closed.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        {
            Some(&crate::aarch64::spawn_producer::SPAWN_TABLE_PHYSMAP)
        }
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        {
            None
        }
    }

    fn install_irq_dispatch(&self, table: &'static IrqTable) {
        // Publish the freshly built `IrqTable` and register the production
        // device-IRQ dispatcher with the arch crate's EL1 IRQ-vector seam,
        // so an acknowledged non-timer GIC INTID is translated into an
        // `IrqTable::fire` (mask-before-wake, `docs/src/security/irq.md`).
        crate::aarch64::gic_irq::install_device_irq_dispatch(table);

        // Arm timer-driven preemption now that the GICv2 is up (P-1,
        // `plans/PI.md` D2b-2b-A): register the per-CPU preempt storage,
        // install the EL0-preemption callback, and start the periodic
        // generic timer. PE IRQs stay masked until EL0 runs preemptibly /
        // the unlock kthread unmasks, so the armed timer is inert until a
        // handler context exists — additive and non-regressing. A tick taken in EL1 never preempts (non-preemptible
        // kernel); a tick taken in EL0 round-robin-preempts the running
        // user task back to the scheduler.
        crate::aarch64::gic_irq::arm_preemption();
    }
}

// SAFETY-INVARIANT: `Aarch64BinArch::halt` returns the bottom type. The
// coercion fails to type-check if the impl ever loses `-> !`, pinning
// the contract at compile time, exactly as the
// x86_64 `BinArch` and riscv64 `RiscvBinArch` wrappers do.
const _AARCH64_BIN_ARCH_HALT_RETURNS_NEVER: fn(&Aarch64BinArch) -> ! =
    <Aarch64BinArch as KernelArch>::halt;

/// The system console device the aarch64 boot path installs on
/// [`rustos_kernel_core::BootInfo`].
///
/// A zero-sized [`ConsoleWrite`] + [`ConsoleRead`] adapter over the
/// discovered console UART: every `stream_write` byte is forwarded
/// verbatim through [`rustos_arch_aarch64::serial::write_console_bytes`]
/// and every `stream_read` drains pending input through
/// [`rustos_arch_aarch64::serial::read_console_bytes`], both targeting
/// the board-discovered UART base (`plans/PI.md` P2 / P6). It is the
/// "first discovered UART" half of the console seam; the framebuffer
/// path (`plans/PI.md` P7) replaces it with a text-console device once a
/// display driver loads.
///
/// This is the bootstrap stream **backing** the spawner attaches to fd
/// 0/1, not a program-facing interface.
#[derive(Debug, Default, Copy, Clone)]
pub struct UartConsole;

impl ConsoleWrite for UartConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
        // The busy-wait transmit path accepts every byte, so the write
        // is total and never short. It performs no `\n` translation:
        // the bytes reach the device exactly as the program wrote them.
        Ok(serial::write_console_bytes(bytes))
    }
}

impl ConsoleRead for UartConsole {
    fn read(&self, buf: &mut [u8]) -> Result<usize, rustos_abi::Errno> {
        // The non-blocking receive path drains whatever input is
        // immediately available and never busy-waits;
        // a read with no pending input is a valid zero-length read.
        // Kernel-core wraps this device in its `BlockingConsoleRead`
        // adapter, which turns that empty poll into a scheduler park so
        // a `stream_read` caller waits for input rather than seeing a
        // spurious end-of-input.
        Ok(serial::read_console_bytes(buf))
    }
}

/// The single `'static` [`UartConsole`] the boot path lists as the UART
/// console's **write** half (and the in-kernel root-unlock kthread's
/// passphrase **poll** source). Zero-sized, so it has no `.bss`/`.data`
/// footprint — mirroring `rustos_arch_aarch64::SERIAL_SINK`.
pub static UART_CONSOLE: UartConsole = UartConsole;

/// The UART console's receive type-ahead queue — the software RX ring the
/// interrupt-driven serial path fills.
///
/// The PL011 receive interrupt is unmasked once for the whole interactive
/// session — by the root-unlock kthread at the start of its passphrase
/// prompt, and (idempotently) again at the `login` handoff
/// (`crate::aarch64::gic_irq::enable_uart_console_irq`). Its handler
/// (`crate::aarch64::gic_irq::production_device_irq_dispatch`) drains the
/// hardware FIFO and `push`es the bytes here, which wakes the reader parked
/// in kernel-core's `BlockingConsoleRead` (the `login` reader) or the
/// root-unlock kthread's `KthreadConsoleRead` the instant input arrives
/// (the backing parks rather than polls). It is the
/// UART analogue of [`VIDEO_KEYBOARD`]: the same `'static` backs both the
/// console's [`rustos_kernel_core::ConsoleRead`] half (drained by the
/// reader's `stream_read`) and its [`rustos_kernel_core::ConsoleInput`] half
/// (the interrupt's push target), so the push reaches the parked reader.
///
/// Both the unlock kthread and `login` therefore read this interrupt-fed
/// queue and **park** for input rather than busy-polling the raw FIFO; the
/// console-0 gate keeps `login` from reading until the unlock resolves, so
/// the two never contend (`plans/PI.md` P11).
pub static UART_INPUT: rustos_kernel_core::ConsoleInputQueue =
    rustos_kernel_core::ConsoleInputQueue::new();

/// The UART console's **read** half: drains the interrupt-fed [`UART_INPUT`]
/// queue and, after freeing space, re-enables the receive line if the ISR
/// masked it on a full queue
/// (`crate::aarch64::gic_irq::rearm_uart_rx_if_masked` — the consumer side
/// of the receive flow control). A zero-sized adapter; the queue's
/// [`rustos_kernel_core::ConsoleInput`] (push) half stays the raw
/// [`UART_INPUT`], which the interrupt fills.
#[derive(Debug, Default, Copy, Clone)]
pub struct UartConsoleRead;

impl ConsoleRead for UartConsoleRead {
    fn read(&self, buf: &mut [u8]) -> Result<usize, rustos_abi::Errno> {
        // Pull any byte already in the hardware FIFO into the queue and read
        // the queue in one atomic step under the receive gate
        // (`gic_irq::poll_and_read_uart`), so console input is
        // **poll-backed** rather than solely interrupt-driven: the reader
        // sees a byte that is physically present even if the CPU has not yet
        // taken its receive interrupt, and only ever parks when the FIFO
        // *and* the queue are genuinely empty. The gate serialises this
        // whole step against the RX ISR — reader-context code runs with
        // IRQs deliverable, so without it the two destructive FIFO drains
        // race and can reorder or duplicate typed bytes (the corrupted
        // login-line defect).
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        let read = crate::aarch64::gic_irq::poll_and_read_uart(buf)?;
        #[cfg(not(all(freestanding, kernel_isa = "aarch64")))]
        let read = UART_INPUT.read(buf)?;
        // Draining the queue may have freed the space the ISR was blocked on:
        // re-enable the receive line if it masked itself on a full queue, so
        // input resumes (flow control released). A cheap flag check on the
        // common path; freestanding-only because the GIC re-enable is
        // meaningful solely on the target.
        #[cfg(all(freestanding, kernel_isa = "aarch64"))]
        crate::aarch64::gic_irq::rearm_uart_rx_if_masked();
        Ok(read)
    }
}

/// The single `'static` [`UartConsoleRead`] the console list installs as the
/// UART console's read half (wrapped, for console 0, in the unlock gate).
pub static UART_CONSOLE_READ: UartConsoleRead = UartConsoleRead;

/// The video (framebuffer) console device — the **primary** console the
/// boot path lists when the P7b framebuffer boot console is configured
/// (`plans/PI.md` P11).
///
/// A zero-sized [`ConsoleWrite`] adapter over the framebuffer text
/// renderer: every `stream_write` byte is rendered on screen through
/// [`rustos_arch_aarch64::video::write_bytes`]. The video console's
/// **input** half is not this type but the shared [`VIDEO_KEYBOARD`]
/// queue: the display's session reads a directly attached keyboard
/// (USB HID / PS-2 — the P10 input wiring), whose decoded key edges the
/// keyboard-input driver hands to the [`SEAT_REGISTRY`], which (while
/// the seat is unowned) encodes a press and enqueues it here, never on
/// the UART, which is the session-free debug log line while a display is
/// active (`plans/PI.md` P11). Until a keyboard driver injects anything
/// the queue is empty, so
/// kernel-core's `BlockingConsoleRead` parks a reader until a keystroke
/// arrives — the prompt waits instead of exiting or borrowing the serial
/// line.
#[derive(Debug, Default, Copy, Clone)]
pub struct VideoConsole;

impl ConsoleWrite for VideoConsole {
    fn write(&self, bytes: &[u8]) -> Result<usize, rustos_abi::Errno> {
        // The boot path lists this device only when the framebuffer
        // console came up, but fail closed rather than silently dropping
        // bytes if that invariant is ever violated.
        if !rustos_arch_aarch64::video::is_active() {
            return Err(rustos_abi::Errno::NotImplemented);
        }
        rustos_arch_aarch64::video::write_bytes(bytes);
        Ok(bytes.len())
    }

    fn geometry(&self) -> Option<rustos_abi::TerminalSize> {
        // The framebuffer console's grid is a function of the firmware
        // panel resolution and the font, known at runtime; report it so a
        // full-screen program (`top`) draws to the real display extents. A
        // UART keeps the trait default (`None`).
        rustos_arch_aarch64::video::text_grid()
    }
}

/// The single `'static` [`VideoConsole`] the boot path lists first when
/// the framebuffer boot console is active. Zero-sized, like
/// [`UART_CONSOLE`].
pub static VIDEO_CONSOLE: VideoConsole = VideoConsole;

/// The video console's keyboard type-ahead queue (`plans/PI.md` P11 —
/// keyboard input for the video console).
///
/// It is both the video console's [`rustos_kernel_core::ConsoleRead`]
/// half (drained by a `stream_read` from the video login) and its
/// [`rustos_kernel_core::ConsoleInput`] half — the [`SEAT_REGISTRY`]'s
/// *text sink*, into which an injected key press is encoded
/// while the seat is unowned. The same `'static` backs both
/// halves, so the registry's push wakes the reader parked in kernel-core's
/// `BlockingConsoleRead`. The video login takes input only from its own
/// keyboard, never the serial line — the UART carries no console at all
/// while a display is active (`plans/PI.md` P11).
pub static VIDEO_KEYBOARD: rustos_kernel_core::ConsoleInputQueue =
    rustos_kernel_core::ConsoleInputQueue::new();

/// The kernel seat registry the boot path installs through
/// [`rustos_kernel_core::KernelSyscallHandlers::with_seat_registry`]
/// (`plans/DISPLAY.md` D2; `plans/PI.md` P11 — input follows the
/// surface owner).
///
/// Its *text sink* is the **video console device**
/// (`VIDEO_ONLY_CONSOLES[0]`), whose line discipline forwards to
/// [`VIDEO_KEYBOARD`]: while the seat is unowned (the default for a freshly
/// booted text login) the registry encodes each injected key *press* to the
/// video console's tty bytes and pushes them through the console's input
/// filter (`plans/SPAWN.md` SP9 — a cooked-mode `^C`/`^Z` reaches the
/// foreground job instead of the queue), landing in the keyboard queue
/// where the video login's `stream_read` drains them. When the
/// window manager acquires the seat (`display_acquire`, owner-checked) the
/// registry routes whole [`rustos_abi::input::KeyInput`] records to its
/// desktop keyboard channel instead, drained by the owner's
/// `keyboard_read`. The same `'static`
/// is shared by the in-kernel keyboard driver's `ArbiterConsoleSink` (which
/// injects key edges) and the `key_inject` / `display_acquire` /
/// `display_release` / `keyboard_read` syscall handlers.
pub static SEAT_REGISTRY: SeatRegistry = SeatRegistry::new(&VIDEO_ONLY_CONSOLES[0]);

/// The console-0 read half of the video console, gated on the in-kernel
/// root-unlock service's ownership latch (`plans/PI.md` P11 Chunk B-2 item
/// 5): a `stream_read` from the primary console's `login` is withheld
/// (parked) until the unlock kthread has finished reading the root
/// passphrase off the same queue and opened the gate, so the two never
/// race for console-0 input. Wraps [`VIDEO_KEYBOARD`]; the injected-input
/// (`console_input`) half stays the raw queue.
static GATED_VIDEO_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &VIDEO_KEYBOARD,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The console-0 read half of the UART console, gated on the same unlock
/// ownership latch as [`GATED_VIDEO_READ`] for the UART-only (QEMU `virt`,
/// headless Pi) layout where the UART *is* the primary console.
///
/// It reads the interrupt-fed [`UART_INPUT`] queue (not the raw FIFO): once
/// the gate opens the receive interrupt is unmasked and a parked `login`
/// reader is woken by the queue push, never left polling.
static GATED_UART_READ: crate::unlock_service::GatedConsoleRead =
    crate::unlock_service::GatedConsoleRead::new(
        &UART_CONSOLE_READ,
        &crate::unlock_service::CONSOLE0_GATE,
    );

/// The console list installed when the framebuffer boot console is
/// active: the video console is the **only** console (index 0, PID 1's
/// banner + the login session). The UART then carries no session at all —
/// it is the debug log line (`rustos_arch_aarch64::SERIAL_SINK`), and a
/// full-screen login drawing over the log stream would garble both — so
/// no stream backing is installed for it and its receive interrupt is
/// never enabled (`plans/PI.md` P11).
pub static VIDEO_ONLY_CONSOLES: [rustos_kernel_core::ConsoleDevice; 1] = [
    // The video console: written to the framebuffer, read (through the
    // unlock ownership gate) from the shared keyboard type-ahead queue,
    // and fed by the input-focus arbiter's text sink into that same queue.
    rustos_kernel_core::ConsoleDevice::with_input(
        &VIDEO_CONSOLE,
        &GATED_VIDEO_READ,
        &VIDEO_KEYBOARD,
    ),
];

/// The console list installed when no display came up (QEMU `virt`, a
/// headless Pi): the discovered UART is the only console, so it is the
/// primary console and its read half is gated on the unlock latch.
pub static UART_ONLY_CONSOLES: [rustos_kernel_core::ConsoleDevice; 1] =
    [rustos_kernel_core::ConsoleDevice::with_input(
        &UART_CONSOLE,
        &GATED_UART_READ,
        &UART_INPUT,
    )];

/// The **installed** UART console device — the primary (and only) console
/// on a serial-only boot. With an active video console the UART is the
/// debug log line and carries no console at all, its receive interrupt is
/// never enabled, and the RX drain that consumes this device never runs.
///
/// The UART RX drain pushes received bytes through this device rather than
/// the raw [`UART_INPUT`] queue, so the console's cooked-mode line
/// discipline sees every byte at arrival time (`plans/SPAWN.md` SP9): a
/// `^C`/`^Z` typed while a foreground job runs is delivered as a signal
/// even though no task is reading.
#[must_use]
pub fn uart_console_device() -> &'static rustos_kernel_core::ConsoleDevice {
    &UART_ONLY_CONSOLES[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_aarch64::{Aarch64Arch, Aarch64ArchStorage};

    #[test]
    fn bin_arch_delegates_scheduler_arch_to_inner() {
        static S: Aarch64ArchStorage<4> = Aarch64ArchStorage::new();
        let bin = Aarch64BinArch::new(Aarch64Arch::new(&S, 2, 1_000));
        assert_eq!(bin.current_cpu(), 2);
        // The monotonic clock is the inner handle's; two reads are
        // strictly increasing on the host substitute counter.
        let a = bin.monotonic_ns(2);
        let b = bin.monotonic_ns(2);
        assert!(b > a, "clock must be monotonically increasing");
    }

    #[test]
    fn uart_console_reports_full_write_and_is_inert_on_host() {
        // `serial::write_console_bytes` is a no-op transmit on the host
        // build but reports the full byte count, so the adapter's
        // contract (never short) holds without touching MMIO.
        assert_eq!(UART_CONSOLE.write(b"hello"), Ok(5));
        assert_eq!(UartConsole.write(&[]), Ok(0));
    }

    #[test]
    fn uart_console_read_reports_zero_and_is_inert_on_host() {
        // `serial::read_console_bytes` yields no input on the host build
        // (the device `getchar` returns `None`), so the adapter reports a
        // valid zero-length read — never an error — without touching MMIO.
        let mut buf = [0u8; 8];
        assert_eq!(UART_CONSOLE.read(&mut buf), Ok(0));
        assert_eq!(UartConsole.read(&mut []), Ok(0));
    }
}
