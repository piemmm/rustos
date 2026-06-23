//! Console-backed [`rustos_log::Sink`] for the freestanding aarch64 kernel.
//!
//! Mirrors the riscv64 SBI-console sink (`kernel/arch/riscv64::serial`)
//! and the x86_64 COM1 sink: one formatted line per event, in the
//! canonical format the `kernel/core` audit consumers and the QEMU
//! serial scraper expect —
//!
//! ```text
//! [t=<ms>ms] [<level>] id=<id> <message> <key>=<value> ...
//! ```
//!
//! The leading `[t=<ms>ms]` is a monotonic, `CNTPCT_EL0`-derived
//! millisecond timestamp (epoch unspecified — only differences matter)
//! so a serial capture reads off the real wall time between two lines.
//! Consumers match on the `id=<id>` token, never the line start, so the
//! prefix does not disturb them (`AGENTS.md` §15.7 / §2.16).
//!
//! # Two consoles: the display and the UART (`plans/PI.md` P7b / P11)
//!
//! The **boot-log** path (`ConsoleWriter`, and through it
//! `SerialSink`) routes by build profile (`AGENTS.md` §10 — the
//! screen is the user-facing console, the serial line is the
//! diagnostic one):
//!
//! - A **release build** renders every log line on the attached
//!   display when the framebuffer boot console is configured
//!   ([`crate::video::is_active`]) and carries it on the UART only
//!   when no video output exists.
//! - A **debug build** (`cfg(debug_assertions)`) routes the whole
//!   log/debug stream to the **UART instead** — even while a login
//!   session owns the UART — so a serial capture of a development boot
//!   carries the full diagnostic stream and the screen stays clear for
//!   the user-facing session. With no UART discovered the bounded
//!   transmit simply drops the bytes; the screen is never the debug
//!   log's sink.
//!
//! "Debug build" here is the Cargo `dev` profile, and it lines up with
//! the **image** profile because `tools/xtask` compiles the kernel in
//! the matching profile: the non-shippable `debug` SD image is built
//! from a `dev`-profile (`debug_assertions`-on) kernel, while the
//! shippable `installer` image is built `--release` (assertions off, log
//! on screen). The single release kernel cannot observe the image it is
//! planted in, so this build-profile pairing is what makes the routing
//! above match the operator's `--profile` choice (`tools/xtask`
//! `kernel_build_profile`).
//!
//! The **stream** path is different: the video console and the UART are
//! two *separate* `abi-v1` stream backings with their own session
//! contexts (`plans/PI.md` P11 — one login each). `write_console_bytes`
//! / `read_console_bytes` are the **UART console's** halves and never
//! touch the display; the video console's write half goes straight to
//! [`crate::video::write_bytes`] in the kernel binary's console device,
//! and its input comes from a keyboard source, never the UART.
//!
//! # Board-discovered base and model (`plans/PI.md` P2)
//!
//! The UART transmit path is **board-independent**: it reads the current
//! MMIO base and register layout from [`crate::console`] on every byte.
//! The QEMU `virt` board's PrimeCell PL011 is the pre-discovery default;
//! a board whose console lives elsewhere (the Raspberry Pi — a PL011 or a
//! BCM2835 AUX mini-UART at the SoC's high peripheral window) overrides
//! both by calling [`crate::console::configure_from_fdt`] early in boot.
//! There is one console abstraction with two register backends, not two
//! consoles (`AGENTS.md` §2.2).
//!
//! The sink is a zero-sized type exposed through the `SERIAL_SINK`
//! `'static` so a bin can hand the same reference to `BootInfo`'s
//! `log_sink` / `audit_sink` slots without a mutable static (`AGENTS.md`
//! §2 — no global mutable state beyond the per-CPU bootstrap area). The
//! underlying shared mutable state is the UART itself plus the console
//! base/model cell in [`crate::console`], not this wrapper.
//!
//! # Buffered, non-blocking transmit (`AGENTS.md` §2.16 / §20)
//!
//! Both UART producers — the diagnostic log `SerialSink` and raw
//! console output (`write_console_bytes`, the `stream_write` backing) —
//! copy their bytes into one shared in-memory ring (`SerialRing`) and
//! return, instead of spinning in [`crate::console::tx_wait`] until the
//! FIFO accepts every byte. A flow-blocked PL011 (the Pi 4's BT-attached
//! UART) transmits one line in tens to hundreds of milliseconds, and this
//! boot is effectively single-CPU and cooperative, so a *synchronous*
//! transmit froze the whole CPU for that line — starving the keyboard
//! report pump and making the `Root passphrase:` prompt take or drop
//! keystrokes. Buffering decouples the producer from the slow device: at
//! the end of each producer write the ring drains opportunistically
//! (whatever the FIFO accepts now, no spin) and then arms the UART's
//! **transmit interrupt** to whatever the FIFO could not take.
//!
//! That transmit interrupt is the primary drain (`service_uart_tx_irq`):
//! the device raises it whenever the transmit FIFO has room, so the ISR
//! refills the FIFO from the ring as it drains — at the UART's real
//! throughput, regardless of what the scheduler is running, and it even
//! wakes the CPU out of `wfi`. The drain is therefore not coupled to the
//! scheduler ever reaching its idle wait, which is what made earlier
//! attempts dribble the backlog out one FIFO-load per incidental wake (the
//! "10–20 characters, then a multi-second pause"). The single shared GIC
//! line carries both directions; reading the masked interrupt status
//! (`ConsoleModel::tx_interrupt_fired` / `rx_interrupt_fired`) keeps the ISR
//! from ever draining receive bytes the passphrase poll still owns. When the
//! ring fills, a producer block-flushes (`flush_blocking`) to bound memory;
//! when the UART is genuinely wedged that flush drops bytes rather than
//! hanging (lossy — §2.1). The idle wait keeps a bounded opportunistic
//! `drain_serial` as defence in depth for the narrow pre-GIC window before
//! the transmit interrupt is live. The boot beacons stay on the direct,
//! lock-free path (`beacon`) because they run with the MMU off, where the
//! ring's lock is unusable, and must trace a hang *immediately*. This is the
//! design `lib/log` always documented ("sinks copy the event into a ring
//! buffer consumed by an async drainer").

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_log::{Event, Level, Sink};

// The console MMIO seam is only reached by the freestanding transmit /
// receive primitives below; the host build uses inert stubs, so importing
// it there would be an unused import (`AGENTS.md` §7 — the module is
// host-compiled so its pure ring logic is unit-tested).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use crate::console;
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
use crate::console::{tx_wait, TxOutcome, TX_POLL_BUDGET};

/// Capacity of the in-memory serial ring ([`SerialRing`]).
///
/// A producer copies its bytes into this RAM buffer and returns,
/// instead of spinning in [`crate::console::tx_wait`] until the FIFO accepts every byte —
/// so a producer never blocks the calling task on the slow UART transmit
/// (`AGENTS.md` §2.16 / §20: logging must not stall the keyboard report
/// pump). ~4 KiB per operator direction: large enough that ordinary boots
/// never fill it, small enough to bound the kernel `.bss` footprint.
const SERIAL_RING_CAP: usize = 4096;

/// A bounded byte FIFO buffering outbound serial bytes (log lines and
/// raw console output alike).
///
/// Pure (no MMIO), so the queue discipline is host-unit-tested; the
/// freestanding glue in this module drains it to the UART. The ring never
/// silently discards bytes: when it is full the producer drains it to the
/// device first ([`flush_blocking`]), which becomes lossy only when the
/// UART is wedged ([`crate::console::tx_wait`] drops a byte a dead transmitter can never
/// accept — `AGENTS.md` §2.1, never hang).
struct SerialRing {
    buf: [u8; SERIAL_RING_CAP],
    head: usize,
    len: usize,
}

impl SerialRing {
    /// An empty ring. `const` so it can back a `static`.
    const fn new() -> Self {
        Self {
            buf: [0; SERIAL_RING_CAP],
            head: 0,
            len: 0,
        }
    }

    /// Whether no bytes are queued.
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the ring cannot accept another byte without draining.
    fn is_full(&self) -> bool {
        self.len == SERIAL_RING_CAP
    }

    /// Enqueue one byte, returning `false` (storing nothing) when full.
    fn push_byte(&mut self, byte: u8) -> bool {
        if self.is_full() {
            return false;
        }
        let tail = (self.head + self.len) % SERIAL_RING_CAP;
        self.buf[tail] = byte;
        self.len += 1;
        true
    }

    /// Dequeue the oldest byte (zeroing its slot), or `None` when empty.
    fn pop_byte(&mut self) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let byte = self.buf[self.head];
        self.buf[self.head] = 0;
        self.head = (self.head + 1) % SERIAL_RING_CAP;
        self.len -= 1;
        Some(byte)
    }
}

/// A minimal try-lock guarding a [`SerialRing`].
///
/// `lib/sync::SpinLock` is the workspace lock primitive, but `lib/sync`
/// also carries the `epoch` reclamation module, which pulls in `alloc`
/// (`Box`/`Vec`); Cargo feature-unifies a dependency across the whole
/// target build, so *any* edge from this crate to `rustos-sync` would
/// force a global allocator onto this port's minimal, deliberately
/// allocator-less QEMU test bins (they link only this crate). This tiny
/// guard avoids that. It is the one place the serial ring needs mutual
/// exclusion and is **try-lock only** (no blocking acquire), so it does
/// not re-create the general lock library's surface — the §2.2 carve-out
/// for a constrained primitive the shared crate cannot supply here.
struct RingLock {
    locked: AtomicBool,
    ring: UnsafeCell<SerialRing>,
}

// SAFETY: every path to the inner `SerialRing` goes through `try_with`, which
// hands out the `&mut SerialRing` only after a successful `compare_exchange`
// on `locked` and releases it before returning — so at most one reference
// exists at a time. The contents are plain bytes (no `Drop`), and the
// kernel does not unwind, so a (forbidden, §2.9) panic inside the closure
// cannot leave a dangling borrow; it halts.
unsafe impl Sync for RingLock {}

impl RingLock {
    /// An empty, unlocked ring. `const` so it can back a `static`.
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            ring: UnsafeCell::new(SerialRing::new()),
        }
    }

    /// Run `f` with exclusive access to the ring **if** the lock is free,
    /// returning `Some(f(...))`; returns `None` without blocking when the
    /// lock is contended (another CPU) or re-entrant (an interrupt that
    /// logged while a producer held it), so the caller falls back to a
    /// direct, bounded write rather than risking a deadlock.
    fn try_with<R>(&self, f: impl FnOnce(&mut SerialRing) -> R) -> Option<R> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        // SAFETY: the `compare_exchange` above succeeded, so this CPU holds
        // the lock and no other reference to `ring` can exist; the store
        // below releases it after `f` returns.
        let ring = unsafe { &mut *self.ring.get() };
        let result = f(ring);
        self.locked.store(false, Ordering::Release);
        Some(result)
    }
}

/// The single ring through which **all** UART output is buffered — the
/// diagnostic log [`SerialSink`] and raw console output
/// ([`write_console_bytes`]) alike — so the two share one ordered stream
/// on the wire (`AGENTS.md` §2.2). Producers never interleave a line
/// because [`RingLock::try_with`] serialises access; a contended or
/// re-entrant caller falls back to a direct, bounded write.
static SERIAL_RING: RingLock = RingLock::new();

/// Whether the console transmitter was declared wedged by a budget
/// expiry ([`crate::console::tx_wait`]). While set, each byte costs a
/// single readiness poll (dropped if not ready) instead of a full budget,
/// so a dead/flow-blocked UART cannot crawl the boot; the first poll that
/// finds the FIFO draining clears it and transmission resumes.
///
/// Freestanding-only: it tracks the state of the real MMIO transmit
/// ([`putchar`]); the host build's transmit is an inert stub, so the flag
/// would be dead there.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
static TX_WEDGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Transmit one byte through the currently-configured console UART,
/// waiting **boundedly** while the transmitter cannot yet accept it
/// ([`tx_wait`]): a transmitter that never drains is declared wedged
/// and the byte dropped rather than hanging the kernel (`AGENTS.md`
/// §2.1 — on the Pi 4 a flow-blocked, BT-attached PL011 never drains).
///
/// Freestanding-only: the MMIO access is meaningful solely on the target.
/// The host build omits it (the host tests cover the `Sink` formatting
/// through a capturing writer and the register/[`tx_wait`] helpers in
/// [`crate::console`] instead).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn putchar(byte: u8) {
    use core::sync::atomic::Ordering;

    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *mut u32;
    let wedged = TX_WEDGED.load(Ordering::Relaxed);
    // SAFETY: `base` is the console UART's MMIO base — the `virt` PL011
    // default or the value discovered from the firmware device tree
    // (`console::configure_from_fdt`). The reads/writes are
    // naturally-aligned 32-bit accesses to device registers at the
    // model's documented offsets and touch no Rust-managed memory.
    let (outcome, now_wedged) = tx_wait(
        || unsafe { model.tx_ready(core::ptr::read_volatile(status_reg)) },
        wedged,
        TX_POLL_BUDGET,
    );
    if now_wedged != wedged {
        TX_WEDGED.store(now_wedged, Ordering::Relaxed);
    }
    if outcome == TxOutcome::Send {
        // SAFETY: as above — a naturally-aligned 32-bit store to the
        // model's documented data register, confirmed ready by `tx_wait`.
        unsafe {
            core::ptr::write_volatile(data_reg, u32::from(byte));
        }
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn putchar(_byte: u8) {}

/// Whether the console transmitter can accept a byte **right now**, polled
/// exactly once (no spin, no budget). The non-blocking counterpart of the
/// readiness check inside [`putchar`]: [`drain_ready`] uses it to push only
/// the bytes the FIFO has room for and stop, so a buffered drain never
/// waits on the device (`AGENTS.md` §2.1 / §2.16).
///
/// Freestanding-only: the host build reports "not ready" so the host
/// [`drain_ready`] is a no-op (the queue discipline is host-tested through
/// [`SerialRing`] directly).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn tx_ready_now() -> bool {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `status_offset()` the model's documented status register; a
    // naturally-aligned 32-bit volatile read of a device register that
    // touches no Rust-managed memory.
    unsafe { model.tx_ready(core::ptr::read_volatile(status_reg)) }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn tx_ready_now() -> bool {
    false
}

/// Write one byte to the console transmitter the caller has **already
/// confirmed ready** with [`tx_ready_now`]. Unlike [`putchar`] it performs
/// no readiness wait, so it is only correct immediately after a true
/// [`tx_ready_now`].
///
/// Freestanding-only: inert on the host build.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn tx_send(byte: u8) {
    let (base, model) = console::current();
    let data_reg = (base + model.data_offset()) as *mut u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `data_offset()` the model's documented data register; a
    // naturally-aligned 32-bit volatile store to a device register that
    // touches no Rust-managed memory. The caller confirmed readiness via
    // `tx_ready_now`.
    unsafe {
        core::ptr::write_volatile(data_reg, u32::from(byte));
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn tx_send(_byte: u8) {}

/// Drain the **whole** ring to the UART, blocking (boundedly) per byte
/// through [`putchar`]. Used only when the ring is full, to make room for
/// a new line (operator-directed: block when full). A healthy UART
/// transmits every byte (the producer waits as long as the FIFO needs);
/// a wedged UART makes [`putchar`] drop each byte after a single poll, so
/// the ring still empties without hanging (lossy — `AGENTS.md` §2.1).
fn flush_blocking(ring: &mut SerialRing) {
    flush_n(ring, SERIAL_RING_CAP);
}

/// Pop up to `max` queued bytes and transmit each through the budgeted,
/// wedged-aware [`putchar`].
///
/// The single per-byte drain both the whole-ring [`flush_blocking`] and
/// the bounded idle [`drain_serial`] chunk share, so the wedged-aware policy
/// lives in one place (`AGENTS.md` §2.2). Unlike the opportunistic
/// [`drain_ready`] (which pushes only what the FIFO has room for *right
/// now* and never updates the wedged state), each [`putchar`] here waits
/// **boundedly** for FIFO room — so a healthy-but-slow UART makes
/// guaranteed forward progress, while a genuinely wedged transmitter is
/// detected once (a single budget expiry) and every remaining byte is then
/// dropped after a single poll, emptying the ring without hanging (lossy —
/// `AGENTS.md` §2.1). `SERIAL_RING_CAP` bounds the ring, so a `max` of
/// `SERIAL_RING_CAP` drains it in full.
fn flush_n(ring: &mut SerialRing, max: usize) {
    let mut sent = 0;
    while sent < max {
        match ring.pop_byte() {
            Some(byte) => {
                putchar(byte);
                sent += 1;
            }
            None => break,
        }
    }
}

/// Enqueue one byte into the ring, draining the ring to the device first
/// (blocking / lossy-if-wedged) when it is full so there is always room.
/// The single per-byte enqueue both UART producers share — the buffered
/// log line ([`RingWriter`]) and raw console output ([`write_console_bytes`])
/// — so the full-ring policy lives in one place (`AGENTS.md` §2.2).
fn enqueue_byte(ring: &mut SerialRing, byte: u8) {
    if !ring.push_byte(byte) {
        flush_blocking(ring);
        let _ = ring.push_byte(byte);
    }
}

/// Append `bytes` **verbatim** to the buffered serial ring and push out
/// whatever the FIFO will accept now, without spinning — the non-blocking
/// console-output path. Falls back to a direct, bounded [`putchar`] write
/// only when the ring is momentarily locked (another CPU / a re-entrant
/// interrupt), never spinning on the lock.
fn buffered_uart_write(bytes: &[u8]) {
    let buffered = SERIAL_RING.try_with(|ring| {
        for &byte in bytes {
            enqueue_byte(ring, byte);
        }
        drain_ready(ring);
        // Arm the transmit interrupt to whatever the FIFO could not take, so
        // the transmit ISR drains the remainder at the UART's real throughput
        // without this caller spinning on the device (`AGENTS.md` §2.16).
        sync_tx_irq_to_backlog(ring);
    });
    if buffered.is_none() {
        for &byte in bytes {
            putchar(byte);
        }
    }
}

/// Push as many queued bytes to the UART as the transmitter will accept
/// **right now**, without spinning: stop at the first byte the FIFO is not
/// ready for (or when the ring empties). Never blocks the caller, so it is
/// safe both on the hot path (end of each [`SerialSink::write_event`]) and
/// at the tickless idle point ([`drain_serial`]).
fn drain_ready(ring: &mut SerialRing) {
    while !ring.is_empty() && tx_ready_now() {
        if let Some(byte) = ring.pop_byte() {
            tx_send(byte);
        }
    }
}

/// One idle-drain chunk: the number of buffered bytes [`drain_serial`] pushes
/// per call before returning so the dispatch loop can re-step and service
/// interrupts.
///
/// Small enough that interrupt delivery (a keypress echo, a timed wake) is
/// never delayed by more than a chunk's transmit time, large enough that the
/// per-call loop overhead is negligible — a few PL011 FIFO-loads
/// (`AGENTS.md` §2.16).
const IDLE_DRAIN_CHUNK: usize = 64;

/// Opportunistically push a bounded chunk (`IDLE_DRAIN_CHUNK`) of buffered
/// serial bytes to the UART from the tickless idle wait.
///
/// This is **defence in depth**, not the primary drain: buffered output now
/// flows through the transmit interrupt ([`service_uart_tx_irq`]), which
/// refills the FIFO as it drains at the UART's real throughput regardless of
/// scheduler state, and wakes the CPU out of `wfi` when there is room. This
/// idle drain only covers the narrow window before the GIC console line is
/// live (the early boot phases, where no transmit interrupt is delivered yet)
/// and is harmless once it is — it pushes a bounded chunk, blocking boundedly
/// per byte through the wedged-aware `flush_n`, and returns so the dispatch
/// loop re-steps. A no-op when the ring is momentarily locked by a producer
/// (its own `drain_ready` / transmit interrupt covers that line).
pub fn drain_serial() {
    SERIAL_RING.try_with(|ring| flush_n(ring, IDLE_DRAIN_CHUNK));
}

/// Block until every buffered serial byte has been pushed to the UART
/// (or dropped, if the transmitter is wedged — `flush_blocking`).
///
/// Unlike [`drain_serial`] this waits for the FIFO, so it is reserved for
/// terminal paths that are about to stop running the dispatch loop — the
/// panic bridge (`crate::panic::handle_panic_via_serial`) — where the
/// buffered diagnostic context that led up to the panic must reach a
/// serial capture *before* the CPU parks, and blocking can no longer
/// starve anything. A no-op when the ring is momentarily locked (a panic
/// mid-line); the direct panic record still prints.
pub fn flush_serial_blocking() {
    SERIAL_RING.try_with(flush_blocking);
}

/// `core::fmt::Write` adapter that appends bytes to the locked [`SerialRing`],
/// translating `\n` → `\r\n` (so a serial capture renders line breaks) and
/// block-flushing the ring to the UART when it fills so a long line always
/// makes progress (operator-directed: block when full).
struct RingWriter<'a> {
    ring: &'a mut SerialRing,
}

impl RingWriter<'_> {
    /// Enqueue one byte, draining the ring to the device first when it is
    /// full so there is always room (the ring is empty afterwards).
    fn push(&mut self, byte: u8) {
        enqueue_byte(self.ring, byte);
    }
}

impl core::fmt::Write for RingWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.push(b'\r');
            }
            self.push(byte);
        }
        Ok(())
    }
}

/// Format one event into `w` in the canonical serial-log line shape.
///
/// The single definition shared by the buffered UART path ([`RingWriter`])
/// and the direct fallback / video path ([`ConsoleWriter`]) so the line
/// format lives in one place (`AGENTS.md` §2.2). The leading `[t=<ms>ms]`
/// is a monotonic `CNTPCT_EL0`-derived timestamp; consumers match on the
/// `id=<id>` token, never the line start (§15.7 / §2.16). Write errors are
/// ignored: the sinks are infallible MMIO/RAM writes and the logging path
/// must not panic (§2.9).
fn write_formatted<W: core::fmt::Write>(w: &mut W, event: &Event<'_>) {
    let _ = write!(
        w,
        "[t={}ms] [{}] id={} {}",
        crate::kernel_arch::uptime_ms(),
        level_str(event.level),
        event.id.0,
        event.message
    );
    for field in event.fields {
        let _ = write!(w, " {}={}", field.key, field.value);
    }
    let _ = writeln!(w);
}

/// Read one byte from the currently-configured console UART **without
/// blocking**, returning `None` when the receive FIFO holds no byte.
///
/// This is the non-blocking counterpart of [`putchar`]: it polls the
/// model's receive-ready bit once and, if a byte is waiting, reads the
/// data register. It never busy-waits for input (`AGENTS.md` §2.1) — the
/// caller drains what is available and returns; waiting for input is the
/// stream layer's job, not the device's (kernel-core's
/// `BlockingConsoleRead` parks an empty-handed `stream_read` caller on
/// the scheduler, `AGENTS.md` §20).
///
/// Freestanding-only: the host build returns `None` (no device), so the
/// consuming [`read_console_bytes`] reports a zero-length read there.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn getchar() -> Option<u8> {
    let (base, model) = console::current();
    let status_reg = (base + model.status_offset()) as *const u32;
    let data_reg = (base + model.data_offset()) as *const u32;
    // SAFETY: `base` is the console UART's MMIO base — the `virt` PL011
    // default or the value discovered from the firmware device tree
    // (`console::configure_from_fdt`). The reads are naturally-aligned
    // 32-bit accesses to device registers at the model's documented
    // offsets and touch no Rust-managed memory. The data register is read
    // only after `rx_ready` confirms a byte is present, so it never pops
    // an empty receive FIFO.
    unsafe {
        if !model.rx_ready(core::ptr::read_volatile(status_reg)) {
            return None;
        }
        // The received byte is in the low 8 bits of the data register;
        // the upper bits carry framing/parity error flags this bootstrap
        // backing does not surface.
        Some((core::ptr::read_volatile(data_reg) & 0xff) as u8)
    }
}

#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
fn getchar() -> Option<u8> {
    None
}

/// Switch the currently-configured console UART from poll-only receive to
/// **receive-interrupt-driven**: apply [`ConsoleModel::rx_interrupt_sequence`]
/// to the device so a received byte raises an interrupt.
///
/// The kernel binary calls this once, when console 0 is handed to `login`
/// (`crate::aarch64::gic_irq::enable_uart_console_irq`): before that the
/// in-kernel root-unlock kthread reads the passphrase by *polling* the FIFO
/// (it cooperatively yields, keeping the CPU busy), so the receive interrupt
/// must stay masked or it would drain those bytes out from under the poll.
/// After the handoff `login` parks off the run queue, the CPU idles, and this
/// interrupt is what wakes it when a byte arrives (`AGENTS.md` §2.1 / §20 —
/// the stream backing owns blocking, and it does so by parking, not polling).
///
/// Freestanding-only: the register writes are meaningful solely on the
/// target. The host build is a no-op (the sequence itself is host-tested in
/// [`crate::console`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn enable_rx_interrupt() {
    let (base, model) = console::current();
    for step in model.rx_interrupt_sequence() {
        apply_reg_rmw(base, step);
    }
}

/// Apply one [`console::RegRmw`] to the live console MMIO:
/// `*reg = (*reg & !step.clear) | step.set`, skipping a no-op step. The
/// single register read-modify-write the receive- and transmit-interrupt
/// enable/disable helpers share (`AGENTS.md` §2.2).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn apply_reg_rmw(base: usize, step: console::RegRmw) {
    if step.is_noop() {
        return;
    }
    let reg = (base + step.offset) as *mut u32;
    // SAFETY: `base` is the discovered console UART's MMIO base (the `virt`
    // PL011 default or the device-tree value) and `step.offset` the model's
    // documented register offset, so `reg` is a naturally-aligned 32-bit
    // device register. The read-modify-write preserves every bit outside the
    // step's masks and touches no Rust-managed memory.
    unsafe {
        let cur = core::ptr::read_volatile(reg);
        core::ptr::write_volatile(reg, (cur & !step.clear) | step.set);
    }
}

/// Inert on the host build: there is no UART MMIO to switch into
/// receive-interrupt mode (the sequence itself is host-tested in
/// [`crate::console`]).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn enable_rx_interrupt() {}

/// Clear the console UART's latched receive / receive-timeout interrupt.
///
/// A receive ISR ([`crate::aarch64::gic_irq`]) calls this after it has
/// drained the hardware FIFO **empty**: on the PL011 the receive-timeout
/// latch is not cleared merely by emptying the FIFO, so without the
/// `UARTICR` write the line stays asserted and the ISR re-fires forever (an
/// interrupt storm that starves every other task). A model whose latch
/// clears on a data-register read ([`ConsoleModel::rx_interrupt_clear`]
/// returns [`None`], the mini-UART) needs nothing here.
///
/// Freestanding-only: the register write is meaningful solely on the target;
/// the host build is a no-op (the clear policy is host-tested in
/// [`crate::console`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn clear_rx_interrupt() {
    let (base, model) = console::current();
    if let Some((offset, value)) = model.rx_interrupt_clear() {
        let reg = (base + offset) as *mut u32;
        // SAFETY: `base` is the discovered console UART's MMIO base and
        // `offset` is the model's documented write-1-to-clear interrupt-clear
        // register; a naturally-aligned 32-bit store of the clear mask touches
        // no Rust-managed memory. The register is write-1-to-clear, so a plain
        // store of exactly the bits to clear is correct (no read-modify-write).
        unsafe {
            core::ptr::write_volatile(reg, value);
        }
    }
}

/// Inert on the host build: there is no latched UART interrupt to clear
/// (the clear policy is host-tested in [`crate::console`]).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn clear_rx_interrupt() {}

/// Unmask the console UART's **transmit** interrupt so the device raises an
/// interrupt while its transmit FIFO has room, driving
/// [`service_uart_tx_irq`] to refill the FIFO from the buffered ring at the
/// UART's real throughput — independent of the scheduler ever reaching its
/// idle wait (`AGENTS.md` §2.16 / §20: logging must never stall a task).
///
/// Touches only the transmit-interrupt-enable bit (PL011 `UARTIMSC.TXIM`,
/// mini-UART `AUX_MU_IER_REG` bit 1), never the receive bits, so it is safe
/// to call during the passphrase FIFO-poll window when the receive interrupt
/// is deliberately masked. Idempotent. Freestanding-only; the host build is a
/// no-op (the register policy is host-tested in [`crate::console`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn enable_tx_interrupt() {
    let (base, model) = console::current();
    apply_reg_rmw(base, model.tx_interrupt_enable());
}

/// Inert on the host build (no UART MMIO).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn enable_tx_interrupt() {}

/// Mask the console UART's transmit interrupt (and clear its latch where the
/// model needs it) — the inverse of [`enable_tx_interrupt`], applied once the
/// ring drains so an empty FIFO does not re-fire the ISR forever.
///
/// Freestanding-only; the host build is a no-op.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn disable_tx_interrupt() {
    let (base, model) = console::current();
    apply_reg_rmw(base, model.tx_interrupt_disable());
    if let Some((offset, value)) = model.tx_interrupt_clear() {
        let reg = (base + offset) as *mut u32;
        // SAFETY: `base` is the discovered console UART's MMIO base and
        // `offset` the model's documented write-1-to-clear transmit
        // interrupt-clear register; a naturally-aligned 32-bit store of the
        // clear mask touches no Rust-managed memory.
        unsafe {
            core::ptr::write_volatile(reg, value);
        }
    }
}

/// Inert on the host build (no UART MMIO).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub fn disable_tx_interrupt() {}

/// Set the transmit interrupt to exactly track "the ring still owes the UART
/// bytes": unmask it while a backlog remains, mask it once the ring drains.
///
/// Called under the [`SERIAL_RING`] lock by both the producer (after its
/// opportunistic [`drain_ready`]) and the transmit ISR ([`service_uart_tx_irq`]),
/// so `TXIM` is armed exactly when there is buffered output to push and
/// disarmed the instant there is not — no empty-FIFO interrupt storm, no
/// stranded backlog (`AGENTS.md` §2.2 — one definition of the policy).
fn sync_tx_irq_to_backlog(ring: &SerialRing) {
    if ring.is_empty() {
        disable_tx_interrupt();
    } else {
        enable_tx_interrupt();
    }
}

/// Service a console UART interrupt from interrupt context: read the masked
/// interrupt status **once**, push buffered bytes to the transmit FIFO if the
/// transmit interrupt fired, and report whether a receive interrupt is also
/// pending so the caller drains the receive path.
///
/// Reading the masked status (PL011 `UARTMIS`, mini-UART `AUX_MU_IIR_REG`) is
/// what lets the one shared UART interrupt line carry both directions safely:
/// while the receive interrupt is masked (the passphrase FIFO-poll window) it
/// never appears, so this returns `false` and the receive bytes are left for
/// the poll (`AGENTS.md` §5.4 — fail closed by construction). On a transmit
/// interrupt it pushes whatever the FIFO accepts now (`drain_ready`, no
/// spin) and re-syncs `TXIM` to the remaining backlog (`sync_tx_irq_to_backlog`),
/// so the FIFO is refilled as it drains until the ring empties and the
/// interrupt then disarms itself. Wait-free and allocation-free. A momentary
/// ring-lock contention is a no-op: the lock holder is the producer (EL1, IRQs
/// masked, so it cannot actually be interrupted here) which re-syncs `TXIM`
/// itself on exit.
///
/// Freestanding-only; the host build reports no receive interrupt.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn service_uart_tx_irq() -> bool {
    let (base, model) = console::current();
    let status_reg = (base + model.interrupt_status_offset()) as *const u32;
    // SAFETY: `base` is the discovered console UART's MMIO base and
    // `interrupt_status_offset()` the model's documented masked-status
    // register; a naturally-aligned 32-bit volatile read of a device register
    // that touches no Rust-managed memory.
    let status = unsafe { core::ptr::read_volatile(status_reg) };
    if model.tx_interrupt_fired(status) {
        SERIAL_RING.try_with(|ring| {
            drain_ready(ring);
            sync_tx_irq_to_backlog(ring);
        });
    }
    model.rx_interrupt_fired(status)
}

/// Host stub: there is no UART, so no receive interrupt is ever pending.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn service_uart_tx_irq() -> bool {
    false
}

/// Kick off interrupt-driven transmit for any output already buffered when
/// the GIC console line is first brought up: arm `TXIM` if the ring holds a
/// backlog (else leave it masked).
///
/// Called once from the GIC bring-up (`crate::aarch64::gic_irq`) after the
/// console line is routed and unmasked, so log lines produced before the GIC
/// was live (the early boot phases) start draining through the transmit ISR
/// without waiting for the next producer to arm it. A no-op when the ring is
/// momentarily locked; the next producer arms it.
pub fn prime_tx_irq() {
    SERIAL_RING.try_with(|ring| sync_tx_irq_to_backlog(ring));
}

/// Fill `buf` with whatever console input is **immediately available**,
/// returning the number of bytes read (`0..=buf.len()`).
///
/// Non-blocking: it drains the receive FIFO into `buf` and stops at the
/// first byte that is not yet available (or when `buf` is full), so it
/// never busy-waits for input (`AGENTS.md` §2.1). A read with no pending
/// input returns `0` — a valid short read kernel-core's
/// `BlockingConsoleRead` turns into a scheduler park, re-polling when the
/// caller is next dispatched, so user space only ever sees a read with
/// bytes (`AGENTS.md` §20 — the backing owns blocking). This is the
/// device-side **backing** the stream layer attaches to fd 0
/// (`plans/PI.md` P6e-2 / `AGENTS.md` §20); it is not a program-facing
/// interface.
///
/// Freestanding-only receive (the host build's `getchar` yields
/// `None`), so the host tests of the consuming `ConsoleRead` adapter
/// observe a zero-length read without touching MMIO.
pub fn read_console_bytes(buf: &mut [u8]) -> usize {
    let mut read = 0;
    for slot in buf.iter_mut() {
        match getchar() {
            Some(byte) => {
                *slot = byte;
                read += 1;
            }
            None => break,
        }
    }
    read
}

/// Write `bytes` verbatim to the configured **UART**, returning the
/// number written (always `bytes.len()`; the buffered transmit accepts
/// every byte into the ring and drops only a wedged transmitter's).
///
/// Unlike [`ConsoleWriter`] this performs **no** `\n` → `\r\n`
/// translation: it is the raw byte sink the `stream_write` syscall
/// (`abi-v1` number 11) emits a user program's output through, so the
/// bytes reach the device exactly as the program wrote them
/// (`plans/PI.md` P6c-2, `AGENTS.md` §10 / §16.4). It deliberately
/// never routes to the video console: the UART is its own stream
/// backing with its own login session (`plans/PI.md` P11), so a
/// program attached to the UART console writes the serial line and
/// nothing else. The downstream boot pipeline wraps this in a
/// `kernel_core::ConsoleWrite` device and installs it on `BootInfo`.
///
/// Buffered, like the log sink: the bytes are copied into the shared
/// serial ring (`buffered_uart_write`) and the call returns without
/// spinning on the slow UART, so a program writing fd 1 to the serial
/// console never blocks the CPU on the transmit (`AGENTS.md` §2.16 /
/// §20). Routing both the log line and console output through the one
/// ring also keeps them in a single ordered stream on the wire (§2.2).
///
/// Freestanding-only transmit (the host build's `putchar` is inert),
/// so the host tests of the consuming `ConsoleWrite` adapter observe the
/// byte count without touching MMIO.
#[must_use]
pub fn write_console_bytes(bytes: &[u8]) -> usize {
    buffered_uart_write(bytes);
    bytes.len()
}

/// Emit a short, ordered boot **beacon** — `tag` followed by `\r\n` — so a
/// boot that wedges *before* the consolidated boot-log line
/// (`KERNEL_BOOT_AARCH64_REACHED`, emitted only after the MMU is on) still
/// leaves a trail on the serial line whose last printed tag localises the
/// hang.
///
/// The beacon writes the **UART only**, byte-at-a-time through `putchar`
/// **directly** — never through the buffered ring [`write_console_bytes`]
/// now uses. A beacon must stay lock-free and MMU-off-safe: it fires
/// before the MMU is on, where the ring lock's (`RingLock`) atomic
/// compare-exchange is UNPREDICTABLE (the same reason the allocator and
/// scheduler wait for the
/// MMU, `plans/PI.md` P6c-2), and its whole purpose is an *immediate*
/// trace of a hang, so deferring it into a ring that may never drain would
/// defeat it. It performs no allocation and deliberately never touches the
/// video console (its render lock has the same MMU-off hazard), so a
/// screen mirror could itself wedge the very boot a beacon exists to
/// trace. The serial line is the single, always-safe bisection channel
/// (`AGENTS.md` §2.9 — never hang).
///
/// Freestanding-only (the whole module is gated to the bare-metal
/// target): the transmit is meaningful solely on the Pi's UART, so it
/// carries no host unit test.
pub fn beacon(tag: &str) {
    for &byte in tag.as_bytes() {
        putchar(byte);
    }
    putchar(b'\r');
    putchar(b'\n');
}

/// [`core::fmt::Write`] adapter for the **log/debug** line path that
/// routes by build profile. A release build emits each byte through the
/// user-facing console — the video console when configured (its
/// renderer interprets `\n` itself), else the UART. A debug build
/// (`cfg(debug_assertions)`) sends the whole stream to the **UART
/// instead**, so a serial capture of a development boot carries the
/// full diagnostic stream while the screen stays clear for the session.
///
/// UART bytes get `\n` → `\r\n` translation so terminals capturing the
/// serial line render the boot log with proper line breaks; the video
/// renderer needs no such translation.
pub struct ConsoleWriter;

impl core::fmt::Write for ConsoleWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Debug builds divert the log/debug stream to the UART (never
        // the screen); release builds render it on the display when one
        // is configured and fall back to the UART otherwise.
        if !cfg!(debug_assertions) && crate::video::is_active() {
            crate::video::write_bytes(s.as_bytes());
            return Ok(());
        }
        for byte in s.bytes() {
            if byte == b'\n' {
                putchar(b'\r');
            }
            putchar(byte);
        }
        Ok(())
    }
}

/// `Sink` that emits one formatted line per event through the console.
#[derive(Debug)]
pub struct SerialSink;

impl SerialSink {
    /// Construct a sink. `const` so a binary can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for SerialSink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for SerialSink {
    fn write_event(&self, event: &Event<'_>) {
        // Release build with a live framebuffer console: the user-facing
        // screen is the log's sink, and a framebuffer write is a fast
        // memory store, not a slow serial transmit — render directly,
        // unbuffered. The ring exists to decouple the *UART* (`AGENTS.md`
        // §10 — the screen is the user console; §2.16 — don't buffer what
        // isn't slow).
        if !cfg!(debug_assertions) && crate::video::is_active() {
            let mut w = ConsoleWriter;
            write_formatted(&mut w, event);
            return;
        }

        // UART path (always, on a debug build): copy the formatted line
        // into the in-memory ring and return, so the calling task is never
        // blocked spinning on the slow UART transmit — the defect that let
        // logging starve the keyboard report pump (`AGENTS.md` §2.16 / §20;
        // `lib/log`: "sinks copy the event into a ring buffer consumed by
        // an async drainer"). `RingLock::try_with` — try-lock only, never a
        // blocking acquire — keeps this re-entrancy- and deadlock-free: if
        // the ring is momentarily held (another CPU, or an interrupt that
        // logged while a task held it) the line falls back to the direct,
        // bounded `ConsoleWriter` UART path rather than spinning on the lock.
        let buffered = SERIAL_RING.try_with(|ring| {
            let mut w = RingWriter { ring };
            write_formatted(&mut w, event);
            // Push what the FIFO will accept right now, without spinning, so
            // lines flow out promptly between bursts, then arm the transmit
            // interrupt to whatever the FIFO could not take: the transmit ISR
            // ([`service_uart_tx_irq`]) refills the FIFO as it drains, at the
            // UART's real throughput, without this task ever blocking on the
            // device or the drain waiting for the scheduler to idle
            // (`AGENTS.md` §2.16 / §20).
            drain_ready(ring);
            sync_tx_irq_to_backlog(ring);
        });
        if buffered.is_none() {
            let mut w = ConsoleWriter;
            write_formatted(&mut w, event);
        }
    }
}

const fn level_str(level: Level) -> &'static str {
    match level {
        Level::Trace => "TRACE",
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

/// Single `'static` [`SerialSink`] handle the bin installs in
/// `BootInfo`'s `log_sink` / `audit_sink` slots. Zero-sized, so no
/// `.bss` or `.data` footprint.
pub static SERIAL_SINK: SerialSink = SerialSink::new();

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt::Write as _;

    #[test]
    fn serial_ring_starts_empty() {
        let ring = SerialRing::new();
        assert!(ring.is_empty());
        assert!(!ring.is_full());
        assert_eq!(ring.len, 0);
    }

    #[test]
    fn serial_ring_is_fifo() {
        let mut ring = SerialRing::new();
        for &byte in b"abc" {
            assert!(ring.push_byte(byte));
        }
        assert_eq!(ring.pop_byte(), Some(b'a'));
        assert_eq!(ring.pop_byte(), Some(b'b'));
        assert_eq!(ring.pop_byte(), Some(b'c'));
        assert_eq!(ring.pop_byte(), None);
        assert!(ring.is_empty());
    }

    #[test]
    fn serial_ring_refuses_a_byte_when_full() {
        let mut ring = SerialRing::new();
        for _ in 0..SERIAL_RING_CAP {
            assert!(ring.push_byte(b'x'));
        }
        assert!(ring.is_full());
        // A full ring stores nothing and reports the refusal; the existing
        // bytes are untouched.
        assert!(!ring.push_byte(b'y'));
        assert_eq!(ring.len, SERIAL_RING_CAP);
    }

    #[test]
    fn serial_ring_wraps_around_the_buffer() {
        let mut ring = SerialRing::new();
        // Advance head/tail near the wrap, then push a run that straddles
        // the physical end of the backing array, and confirm FIFO order is
        // preserved across the wrap.
        for _ in 0..SERIAL_RING_CAP - 2 {
            assert!(ring.push_byte(b'.'));
            assert_eq!(ring.pop_byte(), Some(b'.'));
        }
        for &byte in b"WRAP" {
            assert!(ring.push_byte(byte));
        }
        let mut out = [0u8; 4];
        for slot in &mut out {
            *slot = ring.pop_byte().expect("byte queued");
        }
        assert_eq!(&out, b"WRAP");
        assert_eq!(ring.pop_byte(), None);
    }

    #[test]
    fn drain_ready_keeps_bytes_when_no_transmitter_is_ready() {
        // On the host build `tx_ready_now` reports "not ready" (no device),
        // so an opportunistic drain must leave the queued bytes intact for
        // a later drain rather than dropping them.
        let mut ring = SerialRing::new();
        for &byte in b"queued" {
            assert!(ring.push_byte(byte));
        }
        drain_ready(&mut ring);
        assert_eq!(ring.len, b"queued".len());
        assert_eq!(ring.pop_byte(), Some(b'q'));
    }

    #[test]
    fn write_console_bytes_buffers_then_drains_the_shared_ring() {
        // Console output copies into the shared ring and returns (the
        // producer never blocks on the slow UART); a blocking flush empties
        // it. On the host build `tx_ready_now` is always "not ready", so the
        // opportunistic `drain_ready` is a no-op and the bytes stay buffered
        // until `flush_serial_blocking` pops them. No other test touches the
        // `SERIAL_RING` static, so this observation is stable.
        assert!(
            SERIAL_RING.try_with(|ring| ring.is_empty()).unwrap(),
            "ring starts empty"
        );
        let n = write_console_bytes(b"backlog");
        assert_eq!(n, b"backlog".len());
        assert!(
            !SERIAL_RING.try_with(|ring| ring.is_empty()).unwrap(),
            "console output is buffered into the ring, not spun out"
        );
        flush_serial_blocking();
        assert!(
            SERIAL_RING.try_with(|ring| ring.is_empty()).unwrap(),
            "a blocking flush drains the ring"
        );
    }

    #[test]
    fn service_uart_tx_irq_reports_no_receive_on_the_host() {
        // The freestanding service reads the masked status and drains the
        // transmit ring; the host stub has no device, so it reports "no
        // receive interrupt pending" and the caller skips the receive path.
        assert!(!service_uart_tx_irq());
    }

    #[test]
    fn ring_writer_translates_newline_to_crlf() {
        let mut ring = SerialRing::new();
        let mut w = RingWriter { ring: &mut ring };
        w.write_str("x\n").expect("ring write is infallible");
        assert_eq!(ring.pop_byte(), Some(b'x'));
        assert_eq!(ring.pop_byte(), Some(b'\r'));
        assert_eq!(ring.pop_byte(), Some(b'\n'));
        assert_eq!(ring.pop_byte(), None);
    }
}
