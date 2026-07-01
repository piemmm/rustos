//! The kernel-side system console seam the `stream_write` (`abi-v1`
//! number 11) and `stream_read` (`abi-v1` number 13) syscalls use.
//!
//! [`ConsoleWrite`] is the output half and [`ConsoleRead`] the input
//! half; an arch port installs its discovered [`ConsoleDevice`] list
//! through the `BootInfo::with_consoles` seam (one entry per text
//! console — the video console and the UART are independent entries,
//! `plans/PI.md` P11), and the syscall handlers own the user-memory copy
//! and the capability check, never the device.
//!
//! `stream_write` lets the privileged early bring-up principals (PID 1
//! `init`, login, getty) write a byte buffer to the *hardware* console;
//! `stream_read` lets them read input back from it (the shell REPL).
//! Which device that is — the detected framebuffer when one is present,
//! else the first discovered UART (`plans/PI.md` P6) — is a boot-time
//! decision the architecture port makes from the normalised hardware
//! tree. `kernel/core` does not know how to talk to a
//! PL011, a 16550, or a framebuffer; it only knows it needs *a* byte
//! sink. [`ConsoleWrite`] is that seam: the boot path installs the
//! concrete device, and the syscall handler writes the copied-in bytes
//! through it.
//!
//! Until a console is installed the handler holds [`NULL_CONSOLE`],
//! which fails closed with [`Errno::NotImplemented`] rather than
//! silently swallowing the bytes. A build with no
//! console device wired (a headless target with no UART, an early-boot
//! state before discovery) therefore announces an intentionally inert
//! interface instead of pretending the write succeeded.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rustos_abi::{Errno, TerminalSize};
use rustos_kernel_sched_api::SchedulerArch;
use rustos_sync::SpinLock;
use rustos_vt::control;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A byte sink for the privileged system console.
///
/// Implemented by the architecture-port-installed console device (a
/// UART or a framebuffer text console). The trait is deliberately
/// minimal — one method that takes already-copied-in kernel bytes —
/// so `kernel/core` stays free of any device knowledge and the syscall handler owns the user-memory copy and the
/// capability check, never the device implementation.
///
/// Implementations must be [`Sync`]: the single installed console is
/// shared by the per-CPU syscall handlers, exactly like the audit
/// [`Sink`](rustos_log::Sink).
pub trait ConsoleWrite: Sync {
    /// Write `bytes` to the console, returning the number actually
    /// written.
    ///
    /// The caller has already copied `bytes` out of user memory through
    /// the validated `copy_from_user` boundary and
    /// checked the caller's [`CapabilityId::CONSOLE_WRITE`](rustos_abi::CapabilityId::CONSOLE_WRITE);
    /// the implementation only moves bytes to the device. A short write
    /// (fewer than `bytes.len()`) is permitted and reported through the
    /// return value, exactly as POSIX `write` allows; the caller loops.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] when the device cannot accept the
    /// bytes. The default sink ([`NullConsole`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn write(&self, bytes: &[u8]) -> Result<usize, Errno>;

    /// This console's character-cell grid, when the device knows it
    /// (`terminal_size`).
    ///
    /// A framebuffer text console has a grid that is a function of the panel
    /// resolution and the font, so it overrides this to report its live
    /// geometry. A byte-stream console (a UART) keeps the default [`None`]:
    /// the true size of the remote terminal is a property of whatever
    /// emulator sits at the far end of the wire, unknowable to the kernel, so
    /// `terminal_size` fails closed for it and the client applies the
    /// conventional fallback rather than the kernel fabricating a grid.
    fn geometry(&self) -> Option<TerminalSize> {
        None
    }
}

/// The console sink installed before any real device exists.
///
/// Every write fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default require, so a
/// `stream_write` issued before the boot path installs a device (or on
/// a target that genuinely has no console) announces an inert interface
/// rather than silently discarding the bytes.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullConsole;

impl ConsoleWrite for NullConsole {
    fn write(&self, _bytes: &[u8]) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullConsole`] instance a write-less console entry
/// carries.
///
/// A [`ConsoleDevice`] whose console has no output device points its
/// `write` half here so the direction stays fail-closed without an
/// `Option` branch on the hot path.
pub static NULL_CONSOLE: NullConsole = NullConsole;

/// A byte source for the privileged system console input.
///
/// The read counterpart of [`ConsoleWrite`], implemented by the same
/// architecture-port-installed console device (a UART or a keyboard
/// input source). The trait is deliberately minimal — one method that
/// fills a kernel-owned buffer — so `kernel/core` stays free of any
/// device knowledge and the syscall handler owns
/// the user-memory copy and the capability check, never the device
/// implementation.
///
/// The trait itself carries **no** [`Sync`] bound: a transient,
/// single-kthread reader (the in-kernel root-unlock kthread's cooperative
/// blocking reader, [`crate::kthread`]) holds a `!Sync` yield handle and
/// must implement [`ConsoleRead`] without being shareable across CPUs.
/// [`Sync`] is instead required at the **sharing sites** — the
/// `'static` console list ([`ConsoleDevice::read`]) and the
/// [`BlockingConsoleRead`] adapter both store `&'static (dyn ConsoleRead +
/// Sync)` because that list is shared by the per-CPU syscall handlers,
/// exactly like [`ConsoleWrite`]. Constraining at the storage site rather
/// than the trait keeps the shared path `Sync` without forcing every
/// transient reader to be (do not over-constrain a
/// trait).
pub trait ConsoleRead {
    /// Read available console input into `buf`, returning the number of
    /// bytes actually read.
    ///
    /// The caller copies the filled prefix out to user memory through
    /// the validated `copy_to_user` boundary and has
    /// already checked the caller's
    /// [`CapabilityId::CONSOLE_READ`](rustos_abi::CapabilityId::CONSOLE_READ);
    /// the implementation only moves bytes from the device. A short
    /// read (fewer than `buf.len()`, including zero when no input is
    /// pending) is permitted and reported through the return value; the
    /// caller loops. The implementation must never report more bytes
    /// than it wrote into `buf`.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] when the device cannot be read. The
    /// default source ([`NullConsoleRead`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface.
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno>;
}

/// The console input source installed before any real device exists.
///
/// Every read fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default require, so a
/// `stream_read` issued before the boot path installs a device (or on
/// a target that genuinely has no console input) announces an inert
/// interface rather than fabricating input.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullConsoleRead;

impl ConsoleRead for NullConsoleRead {
    fn read(&self, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullConsoleRead`] instance a read-less console entry
/// carries.
///
/// A [`ConsoleDevice`] whose console has no input device (a write-only
/// serial port) points its `read` half here so the direction stays
/// fail-closed without an `Option` branch on the hot path.
pub static NULL_CONSOLE_READ: NullConsoleRead = NullConsoleRead;

/// A sink that accepts decoded keystroke bytes injected into a console
/// from a user-space input driver (`plans/PI.md` P11 —
/// keyboard input for the video console).
///
/// The producer counterpart of [`ConsoleRead`]. A keyboard-input driver
/// that has decoded a directly attached keyboard (USB-HID / PS-2) into a
/// key edge injects it through the `key_inject` syscall (`abi-v1` number
/// 22), which — after checking
/// [`CapabilityId::INPUT_INJECT`](rustos_abi::CapabilityId::INPUT_INJECT)
/// — hands it to the kernel input-focus arbiter
/// ([`crate::input_focus`]). While the desktop does not hold focus the
/// arbiter encodes a key press to its console (tty) bytes and pushes them
/// here (its *text sink*); the matching [`ConsoleRead`] half then drains
/// them for a `stream_read` consumer (login), so the video console's
/// session reads its own keyboard rather than the UART's bytes.
///
/// Implementations must be [`Sync`]: the single installed console is
/// shared by the per-CPU syscall handlers, exactly like [`ConsoleWrite`]
/// and [`ConsoleRead`].
pub trait ConsoleInput: Sync {
    /// Enqueue up to `bytes.len()` decoded console bytes, returning the
    /// number actually accepted.
    ///
    /// The caller (the input-focus arbiter's text sink, fed by the
    /// `key_inject` handler) has already decoded the key edge and checked
    /// [`CapabilityId::INPUT_INJECT`](rustos_abi::CapabilityId::INPUT_INJECT);
    /// the implementation only moves the encoded bytes into its queue. A
    /// short push
    /// (fewer than `bytes.len()`, including zero when the bounded queue
    /// is full) is permitted and reported through the return value; the
    /// producer retries the remainder and never blocks.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] when the console accepts no injected
    /// input. The default sink ([`NullConsoleInput`]) returns
    /// [`Errno::NotImplemented`] to mark a console (a UART reading its
    /// own hardware FIFO) that has no injectable input queue.
    fn push(&self, bytes: &[u8]) -> Result<usize, Errno>;
}

/// The console input sink installed for a console that accepts no
/// injected input.
///
/// Every push fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default require, so the input-focus
/// arbiter's text sink, when it is a console with no injectable queue (a
/// UART, which reads its own hardware FIFO), announces an inert interface
/// rather than silently dropping the keystrokes.
#[derive(Debug, Default, Copy, Clone)]
pub struct NullConsoleInput;

impl ConsoleInput for NullConsoleInput {
    fn push(&self, _bytes: &[u8]) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullConsoleInput`] instance a console with no injectable
/// input queue carries.
///
/// A [`ConsoleDevice`] whose console reads its own hardware (a UART)
/// points its `input` half here so a `console_input` targeting it fails
/// closed without an `Option` branch on the hot path.
pub static NULL_CONSOLE_INPUT: NullConsoleInput = NullConsoleInput;

/// Capacity, in bytes, of a [`ConsoleInputQueue`]'s type-ahead ring.
///
/// This is a **fixed bound**, not a scaling capacity: a console type-ahead buffer is the software analogue of a
/// UART's hardware receive FIFO. A human types a handful of characters
/// per second, so 256 bytes absorbs realistic type-ahead between
/// `stream_read` drains; a bound rather than an unbounded queue means a
/// wedged or absent consumer can never make the keyboard driver's pushes
/// grow kernel memory without limit. Overflow drops the
/// excess as a short push (the producer retries) — a
/// dropped surplus keystroke is preferable to unbounded growth.
pub const CONSOLE_INPUT_QUEUE_CAPACITY: usize = 256;

/// The fixed-capacity byte ring behind a [`ConsoleInputQueue`].
struct InputRing {
    buf: [u8; CONSOLE_INPUT_QUEUE_CAPACITY],
    /// Index of the next byte to drain.
    head: usize,
    /// Number of bytes currently queued.
    len: usize,
}

impl InputRing {
    const fn new() -> Self {
        Self {
            buf: [0; CONSOLE_INPUT_QUEUE_CAPACITY],
            head: 0,
            len: 0,
        }
    }
}

/// A bounded, lock-protected type-ahead queue that is both the
/// [`ConsoleRead`] half (drained by `stream_read`) and the
/// [`ConsoleInput`] half (the input-focus arbiter's text sink) of a
/// keyboard-backed console (`plans/PI.md` P11).
///
/// The video console installs one of these so a directly attached
/// keyboard's decoded bytes — encoded and pushed by the input-focus
/// arbiter (`crate::input_focus`) while the desktop does not hold focus —
/// are drained by the login reading that console, instead of the inert
/// `Ok(0)` poll a display with no keyboard would otherwise return. The
/// arch port holds it in a `'static` and references it as both halves of
/// the console's [`ConsoleDevice`] (and as the arbiter's text sink); the
/// same `'static` is therefore shared by the producer (the arbiter) and
/// the consumer (`stream_read`), so a push wakes a reader parked in
/// [`BlockingConsoleRead`].
///
/// A drained byte is **zeroed in place** as it leaves the ring: a typed
/// password transits this queue between the keyboard driver and login,
/// so the buffer must not retain the cleartext after the consumer has
/// taken it (zero-on-free for memory that held a
/// credential; — secret hygiene).
pub struct ConsoleInputQueue {
    ring: SpinLock<InputRing>,
}

impl Default for ConsoleInputQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleInputQueue {
    /// Construct an empty queue. `const` so the arch port can place it in
    /// a `'static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ring: SpinLock::new(InputRing::new()),
        }
    }

    /// Drain up to `buf.len()` queued bytes into `buf`, zeroing each
    /// drained slot in the ring (a transited credential
    /// is not retained), and return the number drained.
    fn drain(&self, buf: &mut [u8]) -> usize {
        let mut ring = self.ring.lock();
        let take = core::cmp::min(ring.len, buf.len());
        for slot in buf.iter_mut().take(take) {
            let idx = ring.head % CONSOLE_INPUT_QUEUE_CAPACITY;
            *slot = ring.buf[idx];
            ring.buf[idx] = 0;
            ring.head = (ring.head + 1) % CONSOLE_INPUT_QUEUE_CAPACITY;
            ring.len -= 1;
        }
        take
    }

    /// Free space, in bytes, currently available in the ring.
    ///
    /// An interrupt-driven producer (a UART receive ISR draining a hardware
    /// FIFO into this queue) reads it to apply **lossless backpressure**:
    /// dequeue from the FIFO only what the ring can accept, leaving the rest
    /// in the FIFO for the next interrupt rather than reading bytes it would
    /// have to drop (the software analogue of the FIFO's
    /// own flow control). A snapshot: with a concurrent drain the true free
    /// space can only grow, so a producer that trusts this never overfills.
    #[must_use]
    pub fn free_capacity(&self) -> usize {
        let ring = self.ring.lock();
        CONSOLE_INPUT_QUEUE_CAPACITY - ring.len
    }

    /// Enqueue as many of `bytes` as fit, returning the number accepted
    /// (a short push when the ring fills; the producer retries the
    /// remainder and never blocks).
    fn enqueue(&self, bytes: &[u8]) -> usize {
        let mut ring = self.ring.lock();
        let mut pushed = 0;
        for &byte in bytes {
            if ring.len == CONSOLE_INPUT_QUEUE_CAPACITY {
                break;
            }
            let idx = (ring.head + ring.len) % CONSOLE_INPUT_QUEUE_CAPACITY;
            ring.buf[idx] = byte;
            ring.len += 1;
            pushed += 1;
        }
        pushed
    }
}

impl ConsoleRead for ConsoleInputQueue {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        // An empty queue is a zero-length read, exactly like a UART with
        // an empty RX FIFO; `BlockingConsoleRead` parks the caller and
        // re-polls, so a later arbiter push wakes it (the backing owns blocking).
        Ok(self.drain(buf))
    }
}

impl ConsoleInput for ConsoleInputQueue {
    fn push(&self, bytes: &[u8]) -> Result<usize, Errno> {
        let pushed = self.enqueue(bytes);
        // Wake any reader parked in `BlockingConsoleRead` the instant input
        // lands, so a keyboard-backed console delivers without waiting for
        // the bounded re-poll deadline (event-driven
        // where a wake source exists; the timed re-poll is only the polled
        // UART fallback). A fail-safe no-op before the wait-arch hook is
        // installed.
        if pushed > 0 {
            crate::waitq::console_wake();
        }
        Ok(pushed)
    }
}

/// One installed system text console: the output sink and input source
/// of a single console the per-process descriptor table can attach a
/// standard stream to (`plans/PI.md` P11).
///
/// The boot path installs a `'static` **list** of these through
/// `BootInfo::with_consoles` — index 0 is the primary console (the
/// detected display, else the first discovered UART), and each further
/// entry is an independent console with its own session context (the
/// UART beside an active video console). A spawner attaches a child's
/// standard streams to exactly one entry; the video console and the
/// UART therefore carry **separate login processes** rather than
/// sharing input (`plans/PI.md` P11 — two concurrent session contexts).
///
/// A console with no input device (a write-only serial port, a display
/// with no keyboard yet) carries [`NULL_CONSOLE_READ`] (or an
/// empty-polling source) as its `read` half, so reads fail closed or
/// park rather than borrowing another console's input.
pub struct ConsoleDevice {
    /// The console's byte sink (`stream_write`).
    pub write: &'static (dyn ConsoleWrite + 'static),
    /// The console's byte source (`stream_read`). [`Sync`] is required
    /// here, at the shared-`'static`-list storage site, rather than on the
    /// [`ConsoleRead`] trait itself.
    pub read: &'static (dyn ConsoleRead + Sync + 'static),
    /// The console's injected-input sink (`console_input`).
    ///
    /// A keyboard-backed console (the video console) points this and
    /// [`Self::read`] at the **same** [`ConsoleInputQueue`], so a
    /// keyboard-input driver's pushes are drained by the login reading
    /// this console (`plans/PI.md` P11). A console that reads its own
    /// hardware (a UART) points this at [`NULL_CONSOLE_INPUT`], so a
    /// `console_input` targeting it fails closed.
    pub input: &'static (dyn ConsoleInput + 'static),
    /// Whether a `stream_read` of this console echoes the bytes it
    /// consumes back to [`Self::write`] — the terminal local-echo of the
    /// console's read line discipline (`plans/PI.md`
    /// P11). Defaults to **on** so an interactive user sees what they
    /// type; the `stream_echo` syscall toggles it (login disables it
    /// around a password read so a credential is never rendered). Interior mutability because the single
    /// installed console is shared `&'static`.
    echo: AtomicBool,
    /// Column of the line-discipline cursor since the last line terminator
    /// (or echo toggle): the count of characters the user has typed and the
    /// echo has rendered on the current input line. Bounds the **erase**
    /// (rub-out): a Backspace rubs out one rendered character only while this
    /// is non-zero, so a Backspace at the start of the input line never walks
    /// the cursor back into the prompt the program wrote.
    /// Reset to zero on a `CR`/`LF` echo and on every [`Self::set_echo`]
    /// toggle (each starts a fresh edited line). Relaxed ordering for the same
    /// reason as [`Self::echo`]: a single console carries a single session
    /// (`plans/PI.md` P11), so there is no cross-CPU race to order against.
    echo_col: AtomicUsize,
}

impl ConsoleDevice {
    /// Pair `write` and `read` as one installed console that accepts no
    /// injected input, with terminal echo on by default (
    /// — interactive consoles echo).
    ///
    /// The console's `input` half is [`NULL_CONSOLE_INPUT`], so a
    /// `console_input` targeting it fails closed — the right default for
    /// a console that reads its own hardware (a UART). A keyboard-backed
    /// console uses [`Self::with_input`] instead.
    #[must_use]
    pub const fn new(
        write: &'static (dyn ConsoleWrite + 'static),
        read: &'static (dyn ConsoleRead + Sync + 'static),
    ) -> Self {
        Self::with_input(write, read, &NULL_CONSOLE_INPUT)
    }

    /// Pair `write`, `read`, and an injected-input sink `input` as one
    /// installed console, with terminal echo on by default.
    ///
    /// A keyboard-backed console (the video console) passes the same
    /// [`ConsoleInputQueue`] as both `read` and `input`, so the
    /// keyboard-input driver's `console_input` pushes are drained by the
    /// login's `stream_read` of this console (`plans/PI.md` P11).
    #[must_use]
    pub const fn with_input(
        write: &'static (dyn ConsoleWrite + 'static),
        read: &'static (dyn ConsoleRead + Sync + 'static),
        input: &'static (dyn ConsoleInput + 'static),
    ) -> Self {
        Self {
            write,
            read,
            input,
            echo: AtomicBool::new(true),
            echo_col: AtomicUsize::new(0),
        }
    }

    /// This console's character-cell geometry, if the backing device knows it
    /// (`terminal_size`).
    ///
    /// Delegates to the console's write device: [`Some`] for a framebuffer
    /// text console reporting its live grid, [`None`] for a byte-stream
    /// console (a UART) whose remote-terminal size the kernel cannot attest.
    #[must_use]
    pub fn geometry(&self) -> Option<TerminalSize> {
        self.write.geometry()
    }

    /// Whether terminal local echo is currently enabled for this console.
    #[must_use]
    pub fn echo_enabled(&self) -> bool {
        self.echo.load(Ordering::Relaxed)
    }

    /// Enable or disable terminal local echo for this console
    /// (`stream_echo`). The relaxed ordering is sufficient: echo is a
    /// per-console interactive flag with no other state ordered against
    /// it, and a single console carries a single session (`plans/PI.md`
    /// P11).
    ///
    /// Toggling echo also resets the line-discipline column to zero: a
    /// suppressed password read (`login` disables echo around it) and the prompt that follows it start a fresh edited
    /// line, so a later Backspace must not rub out into a line the column was
    /// last counting before the toggle.
    pub fn set_echo(&self, enabled: bool) {
        self.echo.store(enabled, Ordering::Relaxed);
        self.echo_col.store(0, Ordering::Relaxed);
    }

    /// Echo `bytes` (the bytes a `stream_read` just consumed) back to the
    /// console output when echo is enabled, so an interactive user sees
    /// what they type (terminal local echo).
    ///
    /// A carriage return or line feed is echoed as the CR-LF pair so the
    /// cursor both returns to column zero *and* advances a line — a bare
    /// CR (what a serial terminal sends for the Return key) would
    /// otherwise overwrite the current line. An **erase** (rub-out) byte —
    /// Backspace or Delete, [`control::is_line_erase`] — is *not* echoed
    /// verbatim (that would paint a stray control glyph); instead it rubs
    /// out the previous character with the `BS SP BS`
    /// [`control::ERASE_ECHO`] sequence, but only while a character on the
    /// current input line remains to erase (the per-console `echo_col`
    /// column). A Backspace at the start of the line is a no-op,
    /// so it never walks the cursor back over the prompt. This is the echo
    /// half of the read line discipline; the reader's line buffer applies
    /// the matching erase to the bytes it keeps (`plans/PI.md` P11). Echo is
    /// part of the kernel's read line discipline, so it does not require the
    /// reader to also hold `CAP_CONSOLE_WRITE`.
    ///
    /// Echo is purely cosmetic, so it is **best-effort**: a short write or
    /// a device error is swallowed rather than failing the read the user
    /// asked for (never let a cosmetic side effect
    /// abort the real operation). With echo disabled this is a no-op, so
    /// a suppressed password read touches the output device not at all.
    pub fn echo_bytes(&self, bytes: &[u8]) {
        if !self.echo_enabled() {
            return;
        }
        // The column persists across calls because the reader drains the
        // console a byte (or a few) at a time: one logical input line spans
        // many `echo_bytes` calls, so the rub-out bound must be carried in
        // the console, not recomputed per call.
        let mut col = self.echo_col.load(Ordering::Relaxed);
        // Batch consecutive printable bytes into one device write (fewer
        // device round-trips); flush the pending run when
        // a control byte needs separate handling.
        let mut run_start = 0;
        for i in 0..bytes.len() {
            let byte = bytes[i];
            if byte == control::CR || byte == control::LF {
                self.echo_run(&bytes[run_start..i]);
                let _ = self.write.write(b"\r\n");
                col = 0;
                run_start = i + 1;
            } else if control::is_line_erase(byte) {
                self.echo_run(&bytes[run_start..i]);
                col += i - run_start;
                if col > 0 {
                    let _ = self.write.write(&control::ERASE_ECHO);
                    col -= 1;
                }
                run_start = i + 1;
            }
        }
        self.echo_run(&bytes[run_start..]);
        col += bytes.len() - run_start;
        self.echo_col.store(col, Ordering::Relaxed);
    }

    /// Write one run of non-line-break bytes to the console output,
    /// looping over short writes and stopping on a closed/erroring device
    /// (never spin). Best-effort, for [`Self::echo_bytes`].
    fn echo_run(&self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.write.write(bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) => bytes = &bytes[n.min(bytes.len())..],
            }
        }
    }
}

/// The empty console list the syscall handler defaults to.
///
/// With no console installed every console-backed stream access fails
/// closed with [`Errno::NotImplemented`] — the same inert-interface
/// announcement [`NULL_CONSOLE`] / [`NULL_CONSOLE_READ`] make — and
/// `console_count` reports zero. The boot path replaces it through
/// `KernelSyscallHandlers::with_consoles`.
pub static NO_CONSOLES: [ConsoleDevice; 0] = [];

/// A [`ConsoleRead`] adapter that **blocks** the calling task until input
/// arrives — the stream backing owning the wait, exactly as assigns it ("the backing owns blocking", never the program).
///
/// The installed console devices are deliberately non-blocking pollers (a
/// UART RX drain must never busy-wait inside the device), so a bare device read with an empty FIFO is a zero-length read.
/// Reported to user space, that zero is indistinguishable from end of
/// input — an interactive session reading its first keystroke would exit
/// instantly. This adapter closes that gap at the seam between the device
/// and the `stream_read` handler: a zero-length inner read parks the
/// calling task back on the scheduler through [`reschedule_current`]
/// (the same poll-and-park loop the `wait` syscall's
/// [`KernelProcessWait`](crate::procwait::KernelProcessWait) producer
/// uses — cooperative, never a busy-spin) and re-polls
/// the device when next dispatched, returning only once the device
/// yields bytes or fails.
///
/// A caller that cannot be parked (no resumable user kthread is published
/// on this CPU — a kernel-context read, or a dispatch path outside the
/// user-kthread protocol) fails closed with [`Errno::NotImplemented`]
/// rather than busy-spinning or fabricating an end-of-input, mirroring the process-wait producer's contract.
///
/// Built and installed by the kernel-core init pipeline (phase `Syscall`)
/// around whatever [`ConsoleRead`] the boot path provided; an inner
/// device error (including [`NullConsoleRead`]'s fail-closed
/// [`Errno::NotImplemented`]) propagates immediately and never parks.
pub struct BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
    inner: &'static (dyn ConsoleRead + Sync + 'static),
}

impl<A> BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Wrap `inner` so empty polls park the caller until input arrives.
    ///
    /// `arch` supplies the current-CPU read the park needs, exactly as it
    /// does for [`KernelProcessWait`](crate::procwait::KernelProcessWait).
    #[must_use]
    pub const fn new(arch: &'static A, inner: &'static (dyn ConsoleRead + Sync + 'static)) -> Self {
        Self { arch, inner }
    }
}

impl<A> ConsoleRead for BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        // A zero-length destination can never receive a byte; report the
        // empty read instead of parking a caller no input could ever wake
        // (the handler already screens this, defence in
        // depth here).
        if buf.is_empty() {
            return Ok(0);
        }
        let cpu = self.arch.current_cpu();
        // The current caller to register on `CONSOLE_WAITQ` so a producer can
        // `unpark` it, when a scheduler waker hook is installed. [`None`] on a
        // host build of an unrelated path (no scheduler): such a caller can
        // still drain immediately-available bytes, but an empty poll fails
        // closed rather than busy-spinning, since it cannot park, exactly as the process-wait producer does.
        let parkable: Option<_> = crate::waitq::wait_arch().and_then(|hook| hook.current_task(cpu));
        loop {
            // Register **before** polling so a push arriving in the window
            // between the empty poll and the park is not lost: the producer's
            // [`crate::waitq::console_wake`] then `unpark`s this task and the
            // scheduler's wake-pending token converts a concurrent park commit
            // into a re-ready (this mirrors `irq_wait` / `hw_tree_wait`
            // exactly, one park discipline). A bare
            // register-after-poll would lose the wake for the final bytes of a
            // fast input burst (a producer pushing the tail of a line between
            // the reader's last empty poll and its park), wedging the
            // line-oriented reader; registering first closes that race.
            if let Some(task) = parkable {
                crate::waitq::CONSOLE_WAITQ.register(task, crate::waitq::NO_DEADLINE);
            }
            let read = match self.inner.read(buf) {
                Ok(read) => read,
                Err(e) => {
                    // An inner-device error propagates immediately, fail
                    // closed; leave the wait set first so
                    // no stale registration lingers.
                    if let Some(task) = parkable {
                        crate::waitq::CONSOLE_WAITQ.deregister(task);
                    }
                    return Err(e);
                }
            };
            if read > 0 {
                if let Some(task) = parkable {
                    crate::waitq::CONSOLE_WAITQ.deregister(task);
                }
                return Ok(read);
            }
            // Empty poll: **park** the caller off the run queue until input
            // arrives (never a busy-yield). A re-enqueuing
            // yield here would loop in EL1 with IRQs masked, so the dispatch
            // loop could never reach its idle `wait_for_interrupt` and a
            // device IRQ (e.g. the interrupt-driven keyboard or UART driver
            // whose edge *produces* this console's next byte) would be
            // starved.
            //
            // The wait is **event-driven, with no timed re-poll**: a
            // [`crate::waitq::console_wake`] from a keyboard- or UART-backed
            // console's input push unparks the reader the instant a byte
            // lands. A bounded timed re-poll was deliberately *not* used:
            // arming the per-CPU one-shot here perturbs the transitional
            // in-kernel block waiter's `wfi` (`crate::aarch64::root_unlock` —
            // it re-arms its GIC line on every wake and a spurious timer wake
            // corrupts a multi-block read), which livelocked the one-time
            // driver-store bundle read. Registering with [`NO_DEADLINE`] arms
            // no one-shot.
            //
            // With no waker hook there is no scheduler to park on, so fail
            // closed rather than busy-spin.
            let Some(task) = parkable else {
                return Err(Errno::NotImplemented);
            };
            let parked = reschedule_current(cpu, RescheduleAction::Park);
            crate::waitq::CONSOLE_WAITQ.deregister(task);
            // A `false` means no resumable user kthread is published on this
            // CPU — fail closed rather than busy-spin, as the process-wait
            // producer does.
            if !parked {
                return Err(Errno::NotImplemented);
            }
            // Loop: re-register, then re-poll. A push in the narrow window
            // between this `deregister` and the next `register` is not lost —
            // the immediate re-poll drains it before any further park.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_console_fails_closed() {
        assert_eq!(NULL_CONSOLE.write(b"hello"), Err(Errno::NotImplemented));
        // Even an empty write announces the inert interface rather than
        // pretending success.
        assert_eq!(NullConsole.write(&[]), Err(Errno::NotImplemented));
    }

    extern crate std;

    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::test_arch::TestArch;

    /// A scripted inner device: hands out a fixed byte string once, then
    /// reports empty polls (or a scripted error), recording how many
    /// times it was polled.
    struct ScriptedRead {
        bytes: &'static [u8],
        error: Option<Errno>,
        polls: AtomicUsize,
    }

    impl ScriptedRead {
        const fn with_bytes(bytes: &'static [u8]) -> Self {
            Self {
                bytes,
                error: None,
                polls: AtomicUsize::new(0),
            }
        }

        const fn with_error(error: Errno) -> Self {
            Self {
                bytes: &[],
                error: Some(error),
                polls: AtomicUsize::new(0),
            }
        }
    }

    impl ConsoleRead for ScriptedRead {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            if let Some(err) = self.error {
                return Err(err);
            }
            let take = core::cmp::min(self.bytes.len(), buf.len());
            buf[..take].copy_from_slice(&self.bytes[..take]);
            Ok(take)
        }
    }

    fn leaked_arch() -> &'static TestArch {
        std::boxed::Box::leak(std::boxed::Box::new(TestArch::with_cpus(1)))
    }

    #[test]
    fn blocking_read_returns_pending_bytes_without_parking() {
        static INNER: ScriptedRead = ScriptedRead::with_bytes(b"hi");
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read(&mut buf), Ok(2));
        assert_eq!(&buf[..2], b"hi");
        // Exactly one device poll: a read with pending input never
        // reschedules.
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocking_read_fails_closed_when_caller_cannot_park() {
        // The host test thread publishes no resumable user kthread, so
        // the park is refused and the adapter must fail closed rather
        // than busy-spin on the empty device.
        static INNER: ScriptedRead = ScriptedRead::with_bytes(&[]);
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read(&mut buf), Err(Errno::NotImplemented));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocking_read_propagates_inner_errors_without_parking() {
        static INNER: ScriptedRead = ScriptedRead::with_error(Errno::PermissionDenied);
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read(&mut buf), Err(Errno::PermissionDenied));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocking_read_reports_an_empty_request_without_touching_the_device() {
        static INNER: ScriptedRead = ScriptedRead::with_bytes(b"unseen");
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER);
        // No byte could ever satisfy a zero-length destination; the
        // adapter reports the empty read without polling or parking.
        assert_eq!(blocking.read(&mut []), Ok(0));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn null_console_read_fails_closed() {
        let mut buf = [0u8; 8];
        assert_eq!(NULL_CONSOLE_READ.read(&mut buf), Err(Errno::NotImplemented));
        // Even a zero-length read announces the inert interface rather
        // than reporting a successful empty read.
        assert_eq!(NullConsoleRead.read(&mut []), Err(Errno::NotImplemented));
    }

    /// A `ConsoleWrite` that records every byte handed to it, so the echo
    /// tests can assert exactly what the line discipline emitted.
    struct EchoRecorder {
        written: std::sync::Mutex<std::vec::Vec<u8>>,
    }

    impl EchoRecorder {
        const fn new() -> Self {
            Self {
                written: std::sync::Mutex::new(std::vec::Vec::new()),
            }
        }

        fn taken(&self) -> std::vec::Vec<u8> {
            self.written.lock().unwrap().clone()
        }
    }

    impl ConsoleWrite for EchoRecorder {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            self.written.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    fn echo_device(write: &'static EchoRecorder) -> ConsoleDevice {
        ConsoleDevice::new(write, &NULL_CONSOLE_READ)
    }

    #[test]
    fn console_device_echo_is_on_by_default() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        assert!(device.echo_enabled());
    }

    #[test]
    fn echo_bytes_writes_printable_bytes_verbatim_when_enabled() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        device.echo_bytes(b"root");
        assert_eq!(W.taken(), b"root");
    }

    #[test]
    fn echo_bytes_translates_cr_and_lf_to_crlf() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A bare CR (the Return key on a serial terminal) and a bare LF
        // both echo as the CR-LF pair so the cursor returns to column zero
        // *and* advances a line.
        device.echo_bytes(b"ab\rcd\n");
        assert_eq!(W.taken(), b"ab\r\ncd\r\n");
    }

    #[test]
    fn echo_bytes_is_a_no_op_when_echo_is_disabled() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        device.set_echo(false);
        // A suppressed password read must not render the secret at all.
        device.echo_bytes(b"hunter2");
        assert!(W.taken().is_empty());
        // Re-enabling restores echo.
        device.set_echo(true);
        device.echo_bytes(b"x");
        assert_eq!(W.taken(), b"x");
    }

    #[test]
    fn echo_bytes_rubs_out_the_previous_character_on_erase() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // Type "ab", then a Backspace (DEL): the rendered "ab" stays, and
        // the erase paints `BS SP BS` to wipe the last glyph — never the raw
        // control byte.
        device.echo_bytes(b"ab\x7f");
        assert_eq!(W.taken(), b"ab\x08 \x08");
    }

    #[test]
    fn echo_bytes_accepts_bs_as_an_erase_too() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A serial terminal sends BS (`^H`) rather than DEL for Backspace;
        // both rub out.
        device.echo_bytes(b"x\x08");
        assert_eq!(W.taken(), b"x\x08 \x08");
    }

    #[test]
    fn echo_bytes_erase_at_line_start_is_a_no_op() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A Backspace with nothing typed must not rub out into the prompt:
        // it writes nothing at all.
        device.echo_bytes(b"\x7f");
        assert!(W.taken().is_empty());
    }

    #[test]
    fn echo_bytes_column_persists_across_calls() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // The reader drains a byte at a time, so each character is its own
        // `echo_bytes` call; the rub-out must still know a character was
        // rendered on an earlier call.
        device.echo_bytes(b"a");
        device.echo_bytes(b"\x7f");
        assert_eq!(W.taken(), b"a\x08 \x08");
        // The line is now empty again, so a second Backspace is a no-op.
        device.echo_bytes(b"\x7f");
        assert_eq!(W.taken(), b"a\x08 \x08");
    }

    #[test]
    fn echo_bytes_line_terminator_resets_the_erase_bound() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // After a line is submitted the next line starts empty, so a
        // Backspace at its head rubs nothing out.
        device.echo_bytes(b"ab\n");
        assert_eq!(W.taken(), b"ab\r\n");
        device.echo_bytes(b"\x7f");
        assert_eq!(W.taken(), b"ab\r\n");
    }

    #[test]
    fn echo_bytes_set_echo_resets_the_erase_bound() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // Typing, then a password read (echo off → on), then a Backspace:
        // the toggle started a fresh line, so the Backspace rubs nothing out
        // — it can never walk back into the characters typed before the
        // password.
        device.echo_bytes(b"ab");
        device.set_echo(false);
        device.set_echo(true);
        device.echo_bytes(b"\x7f");
        assert_eq!(W.taken(), b"ab");
    }

    #[test]
    fn null_console_input_fails_closed() {
        // A console with no injectable queue (a UART) refuses every push
        // rather than silently dropping the keystrokes.
        assert_eq!(NULL_CONSOLE_INPUT.push(b"abc"), Err(Errno::NotImplemented));
        assert_eq!(NullConsoleInput.push(&[]), Err(Errno::NotImplemented));
    }

    #[test]
    fn input_queue_pushed_bytes_are_drained_in_order() {
        let queue = ConsoleInputQueue::new();
        // The producer (keyboard driver) pushes; the consumer (login)
        // drains, in FIFO order.
        assert_eq!(queue.push(b"root"), Ok(4));
        let mut buf = [0u8; 8];
        assert_eq!(queue.read(&mut buf), Ok(4));
        assert_eq!(&buf[..4], b"root");
        // Drained dry, a further read reports the empty poll (which
        // `BlockingConsoleRead` turns into a park).
        assert_eq!(queue.read(&mut buf), Ok(0));
    }

    #[test]
    fn input_queue_drains_only_what_fits_and_keeps_the_rest() {
        let queue = ConsoleInputQueue::new();
        assert_eq!(queue.push(b"abcdef"), Ok(6));
        // A short destination drains a prefix; the remainder stays queued
        // for the next read (POSIX short-read semantics).
        let mut small = [0u8; 4];
        assert_eq!(queue.read(&mut small), Ok(4));
        assert_eq!(&small, b"abcd");
        let mut rest = [0u8; 4];
        assert_eq!(queue.read(&mut rest), Ok(2));
        assert_eq!(&rest[..2], b"ef");
    }

    #[test]
    fn input_queue_wraps_around_the_ring() {
        let queue = ConsoleInputQueue::new();
        // Push, drain a prefix, push again: the second push wraps past the
        // ring's physical end, and the FIFO order is preserved.
        assert_eq!(queue.push(b"aaaa"), Ok(4));
        let mut buf = [0u8; 3];
        assert_eq!(queue.read(&mut buf), Ok(3));
        assert_eq!(&buf, b"aaa");
        assert_eq!(queue.push(b"bcd"), Ok(3));
        let mut out = [0u8; 8];
        assert_eq!(queue.read(&mut out), Ok(4));
        assert_eq!(&out[..4], b"abcd");
    }

    #[test]
    fn input_queue_overflow_is_a_short_push() {
        let queue = ConsoleInputQueue::new();
        // Filling to capacity accepts exactly the capacity; the surplus is
        // a short push the producer retries, never an
        // unbounded allocation.
        let full = [b'x'; CONSOLE_INPUT_QUEUE_CAPACITY];
        assert_eq!(queue.push(&full), Ok(CONSOLE_INPUT_QUEUE_CAPACITY));
        assert_eq!(queue.push(b"y"), Ok(0));
        // Draining one byte frees exactly one slot.
        let mut one = [0u8; 1];
        assert_eq!(queue.read(&mut one), Ok(1));
        assert_eq!(queue.push(b"y"), Ok(1));
    }

    #[test]
    fn input_queue_free_capacity_tracks_fill_and_drain() {
        let queue = ConsoleInputQueue::new();
        // Empty: the whole capacity is free.
        assert_eq!(queue.free_capacity(), CONSOLE_INPUT_QUEUE_CAPACITY);
        // A push shrinks the free space by exactly the bytes accepted, so an
        // interrupt-driven producer can dequeue only `free_capacity` bytes
        // from a hardware FIFO and never overfill (lossless backpressure).
        assert_eq!(queue.push(b"abcd"), Ok(4));
        assert_eq!(queue.free_capacity(), CONSOLE_INPUT_QUEUE_CAPACITY - 4);
        // A drain restores it.
        let mut two = [0u8; 2];
        assert_eq!(queue.read(&mut two), Ok(2));
        assert_eq!(queue.free_capacity(), CONSOLE_INPUT_QUEUE_CAPACITY - 2);
        // Full: no free space, so the producer leaves bytes in the FIFO.
        let full = [b'x'; CONSOLE_INPUT_QUEUE_CAPACITY];
        let _ = queue.read(&mut [0u8; CONSOLE_INPUT_QUEUE_CAPACITY]);
        assert_eq!(queue.push(&full), Ok(CONSOLE_INPUT_QUEUE_CAPACITY));
        assert_eq!(queue.free_capacity(), 0);
    }

    #[test]
    fn input_queue_empty_destination_reads_nothing() {
        let queue = ConsoleInputQueue::new();
        assert_eq!(queue.push(b"data"), Ok(4));
        // A zero-length destination drains nothing and leaves the queue
        // intact.
        assert_eq!(queue.read(&mut []), Ok(0));
        let mut buf = [0u8; 8];
        assert_eq!(queue.read(&mut buf), Ok(4));
    }
}
