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

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tairix_abi::{Errno, InputMode, TerminalSize};
use tairix_kernel_sched_api::SchedulerArch;
use tairix_kernel_sec::TaskId;
use tairix_sync::SpinLock;
use tairix_vt::control;
use tairix_vt::line::EraseSeq;
use tairix_vt::secret::{SecretIndicator, SecretInput};

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A byte sink for the privileged system console.
///
/// Implemented by the architecture-port-installed console device (a
/// UART or a framebuffer text console). The two write operations distinguish
/// verbatim bytes from program output that needs terminal newline processing,
/// so a retained console can apply that processing without fragmenting one
/// batch into repeated repaints. `kernel/core` stays free of device knowledge,
/// and the syscall handler owns the user-memory copy and capability check,
/// never the device implementation.
///
/// Implementations must be [`Sync`]: the single installed console is
/// shared by the per-CPU syscall handlers, exactly like the audit
/// [`Sink`](tairix_log::Sink).
pub trait ConsoleWrite: Sync {
    /// Write `bytes` to the console, returning the number actually
    /// written.
    ///
    /// The caller has already copied `bytes` out of user memory through
    /// the validated `copy_from_user` boundary and
    /// checked the caller's [`CapabilityId::CONSOLE_WRITE`](tairix_abi::CapabilityId::CONSOLE_WRITE);
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

    /// Write program-output `bytes` with the console line discipline's
    /// `LF` → `CR LF` translation, returning the number of input bytes
    /// consumed.
    ///
    /// Byte-stream devices keep this default implementation, which preserves
    /// exact short-write accounting across the expanded line-feed pair. A
    /// retained framebuffer console may override it to apply the translation
    /// while updating its cell grid and repaint once for the whole batch.
    ///
    /// # Errors
    ///
    /// Returns the backing device's [`Errno`] when it rejects the first byte.
    /// Once some input has been consumed, a later device error is reported as
    /// a short write so retrying cannot duplicate bytes.
    fn write_output(&self, bytes: &[u8]) -> Result<usize, Errno> {
        tairix_tty::write_cooked(bytes, |run| self.write(run))
    }

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
    /// [`CapabilityId::CONSOLE_READ`](tairix_abi::CapabilityId::CONSOLE_READ);
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

    /// Read available console input into `buf`, waiting at most
    /// `timeout_ns` nanoseconds for input to arrive.
    ///
    /// Backs the `stream_read` syscall's non-zero `timeout_ns` argument: a
    /// full-screen program refreshing a clock or status figure waits on the
    /// console with a bound instead of a busy poll. Only a backing that can
    /// park honours the bound — [`BlockingConsoleRead`] parks the caller on
    /// the console wait queue with a one-shot deadline and reports
    /// [`Errno::TimedOut`] when it elapses with no input. The default
    /// delegates to [`ConsoleRead::read`]: a non-blocking device never
    /// waits at all, so it trivially honours any bound by returning the
    /// pending bytes (possibly zero) immediately.
    ///
    /// # Errors
    ///
    /// [`Errno::TimedOut`] when a parking backing's bound elapses with no
    /// input; otherwise exactly as [`ConsoleRead::read`].
    fn read_timeout(&self, buf: &mut [u8], timeout_ns: u64) -> Result<usize, Errno> {
        let _ = timeout_ns;
        self.read(buf)
    }

    /// Mark the reads that follow as secret (password) entry (`secret ==
    /// true`) or ordinary echoed entry (`secret == false`).
    ///
    /// A backing that draws a secret-entry activity marker (the
    /// `[input active…]` indicator, [`SecretFeedback`]) arms it only across a
    /// secret read and shows nothing otherwise, so a prompt that reads a
    /// passphrase brackets that one read with `set_secret(true)` … `false`
    /// while ordinary line reads (a command prompt that echoes what it reads,
    /// such as the pre-boot Supervisor REPL) leave it clear and never paint
    /// the marker over their own echo. It is the read-half analogue of
    /// [`ConsoleDevice::set_input_mode`]'s `Secret` arming.
    ///
    /// The default is a no-op: a backing with no secret feedback (the host
    /// test consoles, [`NullConsoleRead`]) never draws the marker regardless.
    fn set_secret(&self, secret: bool) {
        let _ = secret;
    }
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
/// [`CapabilityId::INPUT_INJECT`](tairix_abi::CapabilityId::INPUT_INJECT)
/// — hands it to the kernel seat registry
/// ([`crate::seat`]). While the seat is unowned the
/// registry encodes a key press to its console (tty) bytes and pushes them
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
    /// [`CapabilityId::INPUT_INJECT`](tairix_abi::CapabilityId::INPUT_INJECT);
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
/// [`ConsoleInput`] half (the seat registry's text sink) of a
/// keyboard-backed console (`plans/PI.md` P11).
///
/// The video console installs one of these so a directly attached
/// keyboard's decoded bytes — encoded and pushed by the seat
/// registry (`crate::seat`) while the seat is unowned —
/// are drained by the login reading that console, instead of the inert
/// `Ok(0)` poll a display with no keyboard would otherwise return. The
/// arch port holds it in a `'static` and references it as both halves of
/// the console's [`ConsoleDevice`] (and as the registry's text sink); the
/// same `'static` is therefore shared by the producer (the registry) and
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
    /// P11). Defaults to **on** (the cooked mode); the `stream_input_mode`
    /// syscall selects the discipline (login selects the secret mode
    /// around a password read so a credential is never rendered; a
    /// full-screen program selects raw so nothing is drawn at all).
    /// Interior mutability because the single
    /// installed console is shared `&'static`.
    echo: AtomicBool,
    /// The echo half's per-line editing state (the shared
    /// [`tairix_tty::EchoLine`]): the rendered-column bound for the erase
    /// rub-out and the held Delete escape-sequence prefix. One lock per
    /// console; a single console carries a single session (`plans/PI.md`
    /// P11), so the lock is uncontended and held only across one
    /// `echo_bytes` call.
    line: SpinLock<tairix_tty::EchoLine>,
    /// The secret-entry activity feedback for this console
    /// ([`SecretFeedback`]), armed while echo is suppressed so a password
    /// read still gives the operator visible progress. [`None`] on a
    /// console built without one (host tests of unrelated paths).
    secret: Option<&'static SecretFeedback>,
    /// The current read line discipline, as its [`InputMode`] wire
    /// discriminant (`plans/SPAWN.md` SP9): the input filter maps `^C`/`^Z`
    /// to foreground signals only in the **cooked** mode, so a raw-mode
    /// full-screen program still receives the literal bytes. Mirrors what
    /// [`Self::set_input_mode`] installed; atomic for the same shared
    /// `&'static` reason as `echo`.
    mode: AtomicU32,
    /// This console's controlling (foreground) ownership
    /// (`console_foreground`, `plans/DISPLAY.md` D5): the task that alone may
    /// drain the input queue and change the line discipline, and the target
    /// the cooked-mode input filter delivers `^C`/`^Z` to. The shared
    /// [`crate::foreground::ForegroundOwnership`] the pseudo-terminal slave
    /// also uses (one definition of the ownership rules); its lock-free
    /// [`current`] read is what the UART RX interrupt handler calls, so the
    /// filter never spins on a lock the interrupted task holds.
    ///
    /// [`current`]: crate::foreground::ForegroundOwnership::current
    fg: crate::foreground::ForegroundOwnership,
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
            line: SpinLock::new(tairix_tty::EchoLine::new()),
            secret: None,
            mode: AtomicU32::new(InputMode::Cooked.as_u32()),
            fg: crate::foreground::ForegroundOwnership::new(),
        }
    }

    /// Attach the console's [`SecretFeedback`], so [`Self::set_input_mode`]
    /// arms it around a secret (password) read. Builder-style, for the init
    /// pipeline that assembles the installed console list.
    #[must_use]
    pub const fn with_secret(mut self, secret: &'static SecretFeedback) -> Self {
        self.secret = Some(secret);
        self
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

    /// Select the read line discipline of this console
    /// (`stream_input_mode`): cooked echoes, secret suppresses echo and
    /// shows the activity indicator, raw suppresses echo and draws nothing
    /// (a full-screen program paints its own display, so even the indicator
    /// would corrupt it). The relaxed ordering is sufficient: echo is a
    /// per-console interactive flag with no other state ordered against
    /// it, and a single console carries a single session (`plans/PI.md`
    /// P11).
    ///
    /// Changing the mode also resets the line-discipline state (column and
    /// held Delete prefix): a secret password read and the prompt that
    /// follows it start a fresh edited line, so a later Backspace must not
    /// rub out into a line the column was last counting before the change.
    ///
    /// Only the **secret** mode arms the console's [`SecretFeedback`] (when
    /// one is attached): the feedback marker is the visible progress a
    /// password read shows instead of the characters. Every other mode
    /// disarms it, removing any in-progress marker still on screen.
    pub fn set_input_mode(&self, mode: InputMode) {
        self.echo.store(mode.echoes(), Ordering::Relaxed);
        self.mode.store(mode.as_u32(), Ordering::Relaxed);
        self.line.lock().reset();
        if let Some(secret) = self.secret {
            if mode == InputMode::Secret {
                secret.arm();
            } else {
                secret.disarm();
            }
        }
    }

    /// The currently selected read line discipline.
    ///
    /// Decoded fail-closed: the stored value is only ever a defined
    /// [`InputMode`] discriminant, but an undecodable value reports the
    /// **raw** discipline — the mode with no line-discipline behaviour at
    /// all — so corruption can never enable byte interception.
    #[must_use]
    pub fn input_mode(&self) -> InputMode {
        InputMode::from_u32(self.mode.load(Ordering::Relaxed)).unwrap_or(InputMode::Raw)
    }

    /// Hand this console's controlling (foreground) ownership to `owner`,
    /// recording `caller` as the granter (`plans/DISPLAY.md` D5).
    ///
    /// The `console_foreground` handler has already authorised `owner` as a
    /// live child of `caller`, so the ownership only ever moves down the
    /// spawn chain — inherited and intersected, never widened. The
    /// transition itself is permitted only from a position of authority
    /// over the slot: the console is unowned, or `caller` is the recorded
    /// granter (re-targeting between its own children), or `caller` is the
    /// current owner (delegating onward to its own child). Anyone else is
    /// refused, so a background task can never take the drain right.
    ///
    /// # Errors
    ///
    /// [`Errno::NotForeground`] when another task's ownership is in place
    /// and `caller` is neither its granter nor the owner.
    pub fn grant_foreground(&self, caller: TaskId, owner: TaskId) -> Result<(), Errno> {
        self.fg.grant(caller, owner)
    }

    /// Release this console's foreground ownership (the granting shell back
    /// at its prompt), returning the console to the open, unowned state.
    ///
    /// Only the recorded granter or the owner itself may release; anyone
    /// else is refused, so a background task cannot open the console by
    /// clearing the slot and then draining it. Releasing an already-unowned
    /// console is an idempotent success: the granter legitimately clears
    /// after its child exited (the exit path already cleared the slot), and
    /// there is nothing an unauthorised caller could gain from the no-op.
    ///
    /// # Errors
    ///
    /// [`Errno::NotForeground`] when another task's ownership is in place
    /// and `caller` is neither its granter nor the owner.
    pub fn release_foreground(&self, caller: TaskId) -> Result<(), Errno> {
        self.fg.release(caller)
    }

    /// Clear the foreground slot if `dead` is its recorded owner.
    ///
    /// The exit path calls this for every console when a task ends, and the
    /// read gate calls it when it proves a recorded owner dead, so a
    /// console is never wedged behind a task that can no longer read it.
    /// Task ids are never reused, so clearing on a proven-dead owner can
    /// never displace a live one. A slot naming any other task is left
    /// untouched (idempotent).
    pub fn clear_dead_foreground(&self, dead: TaskId) {
        self.fg.clear_dead(dead);
    }

    /// This console's current controlling (foreground) owner, if any.
    #[must_use]
    pub fn foreground(&self) -> Option<TaskId> {
        self.fg.current()
    }

    /// This console's shared controlling-ownership object, so the one
    /// foreground-owner gate the console and the pseudo-terminal share can
    /// operate on either terminal through a single definition.
    #[must_use]
    pub fn foreground_ownership(&self) -> &crate::foreground::ForegroundOwnership {
        &self.fg
    }

    /// Echo `bytes` (the bytes a `stream_read` just consumed) back to the
    /// console output when echo is enabled, so an interactive user sees
    /// what they type (terminal local echo).
    ///
    /// The cooking itself — `CR`/`LF` echoed as the `CR LF` pair, the
    /// Backspace/Delete single-byte and `CSI 3 ~` rub-out bounded by the
    /// current input column — is the shared [`tairix_tty::EchoLine`]
    /// discipline, the one definition the pseudo-terminal slave also runs;
    /// this method only supplies the enable gate, the per-console line lock,
    /// and the device sink. The line state persists across calls because the
    /// reader drains the console a byte (or a few) at a time, so one logical
    /// input line — and one split Delete sequence — spans many `echo_bytes`
    /// calls. Echo is part of the kernel's read line discipline, so it does
    /// not require the reader to also hold `CAP_CONSOLE_WRITE`.
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
        let mut line = self.line.lock();
        line.echo(bytes, |echoed| write_best_effort(self.write, echoed));
    }

    /// Write program `bytes` to the console output, cooking output line
    /// feeds: a bare line feed (`LF`) is emitted as the `CR LF` pair (the
    /// ONLCR output translation an interactive terminal applies) so the
    /// cursor returns to column zero as it advances a line, instead of
    /// dropping a line beneath the current column — the "staircase" a raw
    /// `LF` produces on a terminal whose line feed is a pure line feed. A
    /// carriage return passes through unchanged.
    ///
    /// Newline cooking is the output half of the console line discipline,
    /// the counterpart to the input [`Self::echo_bytes`] half (which cooks
    /// the Return key into `CR LF` the same way), so every program that
    /// writes `\n` to its standard output — the shell, the login prompts,
    /// every tool — renders correctly through the one console on every
    /// architecture, without each program emitting `\r\n` itself.
    ///
    /// Returns the number of **input** bytes consumed, which is not the
    /// count written to the device when a line feed expanded to two bytes:
    /// a short device write maps back to a short `stream_write` the caller
    /// loops on, preserving the POSIX short-write contract. A device error
    /// before any byte is consumed surfaces as `Err`; once some input has
    /// been consumed, a later device stall reports the partial count so no
    /// byte is lost or double-written on retry.
    ///
    /// # Errors
    ///
    /// Returns the backing device's [`Errno`] when it rejects the first
    /// byte (an inert [`NullConsole`] returns [`Errno::NotImplemented`]).
    pub fn write_output(&self, bytes: &[u8]) -> Result<usize, Errno> {
        self.write.write_output(bytes)
    }
}

impl ConsoleInput for ConsoleDevice {
    /// Push produced input through this console's line discipline
    /// (`plans/SPAWN.md` SP9): in the **cooked** mode, with a foreground
    /// job set and the delivery producer installed, `^C`/`^Z` are consumed
    /// and queued as [`tairix_abi::Signal::Interrupt`]/[`tairix_abi::Signal::Stop`] for the
    /// foreground task; every other byte — and every byte in the raw or
    /// secret modes, or with no foreground — flows to the underlying input
    /// sink unchanged. Every input producer (a UART RX handler, the seat
    /// registry's keyboard sink) pushes through the device, so the mapping
    /// works even while no task is reading — exactly when a foreground job
    /// is running and the shell is blocked in `wait`.
    ///
    /// The queueing side is interrupt-safe (an atomic store); the actual
    /// scheduler-driving delivery runs at the next dispatcher-context
    /// drain, mirroring the deferred console wakes.
    fn push(&self, bytes: &[u8]) -> Result<usize, Errno> {
        // Gate the interception narrowly: cooked mode, a foreground job,
        // and an installed producer. Anything else passes through — a
        // missing producer must not swallow bytes no one will act on.
        let target = if self.input_mode() == InputMode::Cooked {
            self.foreground()
        } else {
            None
        };
        let Some(target) = target else {
            return self.input.push(bytes);
        };
        if !crate::procsignal::foreground_signal_installed() {
            return self.input.push(bytes);
        }

        let mut accepted = 0usize;
        let mut rest = bytes;
        while !rest.is_empty() {
            // The shared discipline classifies the leading byte; the kernel
            // owns the policy (only here, in cooked mode with a foreground
            // job) and the delivery (queue the signal, nudge the dispatcher).
            if let Some(signal) = tairix_tty::job_control_signal(rest[0]) {
                // Consume the job-control byte and queue the signal for
                // the foreground task instead of buffering it.
                crate::procsignal::queue_foreground_signal(target, signal);
                // Nudge the dispatch loop out of its idle park so the
                // deferred delivery drain runs promptly even with every
                // task parked.
                crate::waitq::console_wake();
                accepted += 1;
                rest = &rest[1..];
            } else {
                // Forward the verbatim run up to the next control byte
                // (at least `rest[0]`, so progress is guaranteed).
                let run = rest
                    .iter()
                    .position(|&b| tairix_tty::job_control_signal(b).is_some())
                    .unwrap_or(rest.len());
                match self.input.push(&rest[..run]) {
                    Ok(pushed) => {
                        accepted += pushed;
                        if pushed < run {
                            // Short push (full queue): report what was
                            // taken, exactly as the queue itself would.
                            return Ok(accepted);
                        }
                    }
                    // An inner error with bytes already accepted is a
                    // short push; with nothing accepted it propagates.
                    Err(err) if accepted == 0 => return Err(err),
                    Err(_) => return Ok(accepted),
                }
                rest = &rest[run..];
            }
        }
        Ok(accepted)
    }
}

/// Write every byte of `bytes` to the console output, best-effort: echo and
/// secret feedback are cosmetic, so a short write or device error is
/// swallowed rather than failing the read the user asked for. Loops over
/// short writes and stops on a closed/erroring device (never spin).
fn write_best_effort(write: &dyn ConsoleWrite, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        match write.write(bytes) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes = &bytes[n.min(bytes.len())..],
        }
    }
}

/// The secret-entry activity feedback of one console: the kernel-side host
/// of the shared [`SecretIndicator`] marker (`tairix_vt::secret`), giving
/// the operator visible progress while a password is typed with echo
/// suppressed.
///
/// One per console, created beside the console's blocking read adapter by
/// the init pipeline. It is **armed** by [`ConsoleDevice::set_input_mode`]
/// whenever the secret mode is selected (a password read) — or
/// directly by an in-kernel secret prompt such as the root-unlock
/// passphrase read — and disarmed by every other mode. While armed, the
/// console's blocking reader feeds it every consumed byte
/// ([`Self::consumed`]) and drives its one-shot animation deadline
/// ([`Self::deadline_ns`] / [`Self::tick`]) from its park loop.
///
/// It tracks the secret line's length itself, through the same
/// [`EraseSeq`]-based classification every reader's `LineEditor` applies to
/// the same bytes, so "the input was erased back to empty" is decided
/// exactly as the reader's buffer decides it. Only the *count* is tracked —
/// no secret byte is ever stored or rendered.
pub struct SecretFeedback {
    /// The console output the marker is drawn to.
    write: &'static (dyn ConsoleWrite + 'static),
    /// Whether a secret read is in progress (echo suppressed). Feeding and
    /// ticking are no-ops while disarmed, so an echoed (non-secret) read
    /// never draws the marker.
    armed: AtomicBool,
    /// The marker state machine plus the line-length tracking that drives
    /// its events. Uncontended (one console carries one session); held only
    /// across one feed/tick.
    state: SpinLock<SecretState>,
}

/// The [`SecretFeedback`] state: the shared marker state machine, the held
/// Delete-sequence prefix, and the secret line's current length.
struct SecretState {
    indicator: SecretIndicator,
    seq: EraseSeq,
    len: usize,
}

impl SecretFeedback {
    /// A fresh, disarmed feedback for the console whose output is `write`.
    #[must_use]
    pub const fn new(write: &'static (dyn ConsoleWrite + 'static)) -> Self {
        Self {
            write,
            armed: AtomicBool::new(false),
            state: SpinLock::new(SecretState {
                indicator: SecretIndicator::new(),
                seq: EraseSeq::new(),
                len: 0,
            }),
        }
    }

    /// Arm the feedback for one secret read, starting from a fresh line.
    pub fn arm(&self) {
        let mut state = self.state.lock();
        state.indicator = SecretIndicator::new();
        state.seq = EraseSeq::new();
        state.len = 0;
        drop(state);
        self.armed.store(true, Ordering::Release);
    }

    /// Disarm the feedback (the secret read is over), removing an in-progress
    /// marker still on screen — an aborted secret read must not leave the
    /// animated marker painted over the next prompt. A *completed* marker
    /// (the operator pressed Enter, so `[input complete]` is showing) is
    /// deliberate final feedback and is left in place.
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
        let mut state = self.state.lock();
        let render = state.indicator.abort();
        state.seq = EraseSeq::new();
        state.len = 0;
        write_best_effort(self.write, render.bytes());
    }

    /// Feed the bytes a blocking console read just consumed, at monotonic
    /// time `now_ns`, rendering whatever marker transitions they cause.
    /// A no-op while disarmed.
    pub fn consumed(&self, bytes: &[u8], now_ns: u64) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock();
        for &byte in bytes {
            let step = state.seq.feed(byte);
            if step.erase() {
                let render = state.erase(now_ns);
                write_best_effort(self.write, render.bytes());
                continue;
            }
            for &literal in step.literal() {
                let render = if literal == control::CR || literal == control::LF {
                    state.len = 0;
                    state.indicator.input(SecretInput::Submitted, now_ns)
                } else if control::is_line_erase(literal) {
                    state.erase(now_ns)
                } else {
                    state.len += 1;
                    state.indicator.input(SecretInput::Typed, now_ns)
                };
                write_best_effort(self.write, render.bytes());
            }
        }
    }

    /// The one-shot deadline the marker animation currently needs, or
    /// [`None`] while disarmed or hidden. The console's blocking reader
    /// parks with this deadline and calls [`Self::tick`] when it passes.
    #[must_use]
    pub fn deadline_ns(&self) -> Option<u64> {
        if !self.armed.load(Ordering::Acquire) {
            return None;
        }
        self.state.lock().indicator.deadline_ns()
    }

    /// The armed animation deadline passed: advance the marker's dots one
    /// frame. A no-op while disarmed.
    pub fn tick(&self, now_ns: u64) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock();
        let render = state.indicator.tick(now_ns);
        write_best_effort(self.write, render.bytes());
    }
}

impl SecretState {
    /// Apply one erase to the tracked line length and feed the resulting
    /// event to the marker.
    fn erase(&mut self, now_ns: u64) -> tairix_vt::secret::Render {
        self.len = self.len.saturating_sub(1);
        self.indicator.input(
            SecretInput::Erased {
                line_empty: self.len == 0,
            },
            now_ns,
        )
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
    /// The console's secret-entry feedback: fed the consumed bytes while a
    /// secret read is armed, and ticked from the park loop when its
    /// animation deadline passes. [`None`] on a console built without one.
    secret: Option<&'static SecretFeedback>,
}

impl<A> BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Wrap `inner` so empty polls park the caller until input arrives,
    /// feeding and ticking the console's `secret` feedback while a secret
    /// read is armed.
    ///
    /// `arch` supplies the current-CPU read the park needs, exactly as it
    /// does for [`KernelProcessWait`](crate::procwait::KernelProcessWait).
    #[must_use]
    pub const fn new(
        arch: &'static A,
        inner: &'static (dyn ConsoleRead + Sync + 'static),
        secret: Option<&'static SecretFeedback>,
    ) -> Self {
        Self {
            arch,
            inner,
            secret,
        }
    }
}

impl<A> BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// The shared poll-and-park core behind [`ConsoleRead::read`] and
    /// [`ConsoleRead::read_timeout`]: block until the inner device yields
    /// bytes, fails, or — when `limit_ns` is `Some` — the caller's absolute
    /// deadline passes with no input ([`Errno::TimedOut`]).
    fn read_until(&self, buf: &mut [u8], limit_ns: Option<u64>) -> Result<usize, Errno> {
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
            // The nearest one-shot wake this wait needs: the secret
            // feedback's animation frame (armed only while a password
            // marker is on screen) and/or the caller's own read deadline.
            // An ordinary unbounded read has neither, parks with no
            // deadline, and takes no timer wake-ups at all (tickless).
            let deadline = self
                .secret
                .and_then(SecretFeedback::deadline_ns)
                .unwrap_or(crate::waitq::NO_DEADLINE)
                .min(limit_ns.unwrap_or(crate::waitq::NO_DEADLINE));
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
                crate::waitq::CONSOLE_WAITQ.register(task, deadline);
            }
            let read = match self.inner.read(buf) {
                Ok(read) => read,
                Err(e) => {
                    // An inner-device error propagates immediately, fail
                    // closed; leave the wait set first so
                    // no stale registration lingers.
                    if let Some(task) = parkable {
                        crate::waitq::console_deregister(task, deadline);
                    }
                    return Err(e);
                }
            };
            if read > 0 {
                if let Some(task) = parkable {
                    crate::waitq::console_deregister(task, deadline);
                }
                // Feed the consumed bytes to the armed secret feedback so
                // the operator sees typing progress on a no-echo read; a
                // no-op while disarmed. The clock is zero before the wait
                // hook is installed, but such a build cannot park, so no
                // animation deadline is ever awaited against it.
                if let Some(secret) = self.secret {
                    let now = crate::waitq::wait_now_ns().unwrap_or(0);
                    secret.consumed(&buf[..read.min(buf.len())], now);
                }
                return Ok(read);
            }
            // The caller's bound elapsed with no input: report the timeout
            // rather than parking again. Checked only against the wait
            // clock the deadline was computed from; a build with no such
            // clock has no parkable caller and fails closed below instead.
            if let Some(limit) = limit_ns {
                if crate::waitq::wait_now_ns().unwrap_or(0) >= limit {
                    if let Some(task) = parkable {
                        crate::waitq::console_deregister(task, deadline);
                    }
                    return Err(Errno::TimedOut);
                }
            }
            // Empty poll: **park** the caller off the run queue until input
            // arrives (never a busy-yield). A re-enqueuing
            // yield here would loop in EL1 with IRQs masked, so the dispatch
            // loop could never reach its idle `wait_for_interrupt` and a
            // device IRQ (e.g. the interrupt-driven keyboard or UART driver
            // whose edge *produces* this console's next byte) would be
            // starved.
            //
            // The wait is **event-driven**: a [`crate::waitq::console_wake`]
            // from a keyboard- or UART-backed console's input push unparks
            // the reader the instant a byte lands. There is no timed re-poll
            // of the *device*; the only finite deadlines ever registered
            // here are the secret feedback's animation tick and the
            // caller's own read bound, so an ordinary unbounded read still
            // arms no one-shot at all.
            //
            // With no waker hook there is no scheduler to park on, so fail
            // closed rather than busy-spin.
            let Some(task) = parkable else {
                return Err(Errno::NotImplemented);
            };
            // Arm the timed-wake one-shot to the nearest pending deadline so
            // the animation tick fires even on an otherwise-idle CPU (the
            // nearest armed wakeup). Only an animated wait pays this; an
            // untimed read parks with no arming work at all.
            if deadline != crate::waitq::NO_DEADLINE {
                crate::waitq::rearm_timed_wakeup();
            }
            let parked = reschedule_current(cpu, RescheduleAction::Park);
            crate::waitq::console_deregister(task, deadline);
            // A `false` means no resumable user kthread is published on this
            // CPU — fail closed rather than busy-spin, as the process-wait
            // producer does.
            if !parked {
                return Err(Errno::NotImplemented);
            }
            // A doomed waiter never re-parks: a termination deferred against
            // this task unwinds the read so the kill lands at the syscall
            // boundary (the errno never reaches user space).
            if crate::procsignal::kill_pending(task) {
                return Err(Errno::Interrupted);
            }
            // A timed wake for the animation: advance the marker's dots one
            // frame, then loop back to re-poll and re-park. Input arriving
            // concurrently is handled by the re-poll, never lost.
            if let Some(secret) = self.secret {
                if let Some(tick) = secret.deadline_ns() {
                    let now = crate::waitq::wait_now_ns().unwrap_or(0);
                    if now >= tick {
                        secret.tick(now);
                    }
                }
            }
            // Loop: re-register, then re-poll. A push in the narrow window
            // between this `deregister` and the next `register` is not lost —
            // the immediate re-poll drains it before any further park.
        }
    }
}

impl<A> ConsoleRead for BlockingConsoleRead<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_until(buf, None)
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ns: u64) -> Result<usize, Errno> {
        // The absolute deadline on the same monotonic clock the park's
        // one-shot is armed against, saturating so a hostile bound can
        // never wrap below `now`. With no wait clock installed there is
        // no scheduler to park on either; the unbounded core then fails
        // closed on an empty poll exactly as `read` does.
        let limit = crate::waitq::wait_now_ns().map(|now| now.saturating_add(timeout_ns));
        self.read_until(buf, limit)
    }
}

#[cfg(test)]
mod tests {
    use tairix_abi::Signal;

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
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
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
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read(&mut buf), Err(Errno::NotImplemented));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocking_read_propagates_inner_errors_without_parking() {
        static INNER: ScriptedRead = ScriptedRead::with_error(Errno::PermissionDenied);
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read(&mut buf), Err(Errno::PermissionDenied));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn blocking_read_reports_an_empty_request_without_touching_the_device() {
        static INNER: ScriptedRead = ScriptedRead::with_bytes(b"unseen");
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        // No byte could ever satisfy a zero-length destination; the
        // adapter reports the empty read without polling or parking.
        assert_eq!(blocking.read(&mut []), Ok(0));
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn timed_read_returns_pending_bytes_without_parking() {
        static INNER: ScriptedRead = ScriptedRead::with_bytes(b"hi");
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        let mut buf = [0u8; 8];
        assert_eq!(blocking.read_timeout(&mut buf, 1_000), Ok(2));
        assert_eq!(&buf[..2], b"hi");
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn timed_read_fails_closed_when_caller_cannot_park() {
        // With no wait clock or scheduler hook installed there is nothing
        // to park on and no deadline to honour: the bounded read fails
        // closed exactly as the unbounded one, never busy-spinning until
        // the bound elapses.
        static INNER: ScriptedRead = ScriptedRead::with_bytes(&[]);
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        let mut buf = [0u8; 8];
        assert_eq!(
            blocking.read_timeout(&mut buf, 1_000),
            Err(Errno::NotImplemented)
        );
        assert_eq!(INNER.polls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn timed_read_propagates_inner_errors() {
        static INNER: ScriptedRead = ScriptedRead::with_error(Errno::PermissionDenied);
        let blocking = BlockingConsoleRead::new(leaked_arch(), &INNER, None);
        let mut buf = [0u8; 8];
        assert_eq!(
            blocking.read_timeout(&mut buf, 1_000),
            Err(Errno::PermissionDenied)
        );
    }

    #[test]
    fn default_timed_read_delegates_to_the_plain_read() {
        // A non-blocking source never waits, so it honours any bound by
        // answering immediately with whatever is pending.
        let queue = ConsoleInputQueue::new();
        assert_eq!(queue.push(b"ab"), Ok(2));
        let mut buf = [0u8; 8];
        assert_eq!(queue.read_timeout(&mut buf, 1), Ok(2));
        assert_eq!(&buf[..2], b"ab");
        assert_eq!(queue.read_timeout(&mut buf, 1), Ok(0));
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

    /// A `ConsoleWrite` that accepts at most `cap` bytes per call (a device
    /// that short-writes), recording what it took, so the output cooking
    /// can be tested against a device that does not drain a whole buffer at
    /// once.
    struct ShortConsole {
        cap: usize,
        written: std::sync::Mutex<std::vec::Vec<u8>>,
    }

    impl ShortConsole {
        const fn new(cap: usize) -> Self {
            Self {
                cap,
                written: std::sync::Mutex::new(std::vec::Vec::new()),
            }
        }

        fn taken(&self) -> std::vec::Vec<u8> {
            self.written.lock().unwrap().clone()
        }
    }

    impl ConsoleWrite for ShortConsole {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            let take = bytes.len().min(self.cap);
            self.written
                .lock()
                .unwrap()
                .extend_from_slice(&bytes[..take]);
            Ok(take)
        }
    }

    /// A framebuffer-like sink that counts each backing write as one repaint.
    struct RepaintRecorder {
        writes: core::sync::atomic::AtomicUsize,
    }

    impl RepaintRecorder {
        const fn new() -> Self {
            Self {
                writes: core::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl ConsoleWrite for RepaintRecorder {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            Ok(bytes.len())
        }

        fn write_output(&self, bytes: &[u8]) -> Result<usize, Errno> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            Ok(bytes.len())
        }
    }

    #[test]
    fn write_output_batches_a_scrolling_burst_into_one_backing_write() {
        static W: RepaintRecorder = RepaintRecorder::new();
        let device = ConsoleDevice::new(&W, &NULL_CONSOLE_READ);

        assert_eq!(device.write_output(b"one\ntwo\nthree\nfour\n"), Ok(19));
        assert_eq!(W.writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn write_output_cooks_lf_to_crlf() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A bare line feed reaches the device as CR-LF so the cursor
        // returns to column zero as it advances a line; the reported count
        // is the *input* length, not the expanded device length.
        assert_eq!(device.write_output(b"ab\ncd\n"), Ok(6));
        assert_eq!(W.taken(), b"ab\r\ncd\r\n");
    }

    #[test]
    fn write_output_passes_a_bare_cr_through_unchanged() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // Only the line feed is cooked; a carriage return with no following
        // line feed passes through verbatim (it is not swallowed or paired).
        assert_eq!(device.write_output(b"x\ry"), Ok(3));
        assert_eq!(W.taken(), b"x\ry");
    }

    #[test]
    fn write_output_cooks_every_lf_even_after_a_cr() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // ONLCR maps every line feed to CR-LF unconditionally, so an
        // already-`\r\n`-terminated write becomes `\r\r\n`. The extra
        // carriage return is harmless (returning to column zero is
        // idempotent), which is why producers can simply emit `\n`.
        assert_eq!(device.write_output(b"x\r\ny"), Ok(4));
        assert_eq!(W.taken(), b"x\r\r\ny");
    }

    #[test]
    fn write_output_cooks_a_leading_and_only_lf() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        assert_eq!(device.write_output(b"\n"), Ok(1));
        assert_eq!(W.taken(), b"\r\n");
    }

    #[test]
    fn write_output_on_an_inert_device_fails_closed() {
        // No byte can be written, so the first attempt surfaces the
        // device's error rather than silently reporting progress.
        let device = ConsoleDevice::new(&NULL_CONSOLE, &NULL_CONSOLE_READ);
        assert_eq!(device.write_output(b"hi\n"), Err(Errno::NotImplemented));
    }

    #[test]
    fn write_output_maps_a_short_device_write_back_to_input_bytes() {
        // A device that accepts one byte per call: the run "ab" writes only
        // "a", so exactly one input byte is reported consumed and the caller
        // loops for the rest — the newline is never half-emitted.
        static W: ShortConsole = ShortConsole::new(1);
        let device = ConsoleDevice::new(&W, &NULL_CONSOLE_READ);
        assert_eq!(device.write_output(b"ab\n"), Ok(1));
        assert_eq!(W.taken(), b"a");
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
    fn echo_bytes_is_a_no_op_in_the_secret_and_raw_modes() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A suppressed password read must not render the secret at all…
        device.set_input_mode(InputMode::Secret);
        device.echo_bytes(b"hunter2");
        assert!(W.taken().is_empty());
        // …and a full-screen program's raw read draws nothing either.
        device.set_input_mode(InputMode::Raw);
        device.echo_bytes(b"q");
        assert!(W.taken().is_empty());
        // Restoring the cooked mode restores echo.
        device.set_input_mode(InputMode::Cooked);
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
    fn echo_bytes_set_input_mode_resets_the_erase_bound() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // Typing, then a password read (secret → cooked), then a Backspace:
        // the mode change started a fresh line, so the Backspace rubs
        // nothing out — it can never walk back into the characters typed
        // before the password.
        device.echo_bytes(b"ab");
        device.set_input_mode(InputMode::Secret);
        device.set_input_mode(InputMode::Cooked);
        device.echo_bytes(b"\x7f");
        assert_eq!(W.taken(), b"ab");
    }

    #[test]
    fn echo_bytes_rubs_out_on_the_delete_key_sequence() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // The Delete key arrives as `CSI 3 ~`: it must rub out the previous
        // glyph exactly like a Backspace, never paint the raw escape bytes
        // (the "weird control codes" defect).
        device.echo_bytes(b"ab\x1b[3~");
        assert_eq!(W.taken(), b"ab\x08 \x08");
    }

    #[test]
    fn echo_bytes_delete_sequence_survives_split_reads() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // The reader drains a byte at a time, so the sequence spans four
        // `echo_bytes` calls; the held prefix must carry across them.
        device.echo_bytes(b"x");
        for &byte in b"\x1b[3~" {
            device.echo_bytes(&[byte]);
        }
        assert_eq!(W.taken(), b"x\x08 \x08");
    }

    #[test]
    fn echo_bytes_delete_at_line_start_is_a_no_op() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // A Delete with nothing typed must not rub out into the prompt.
        device.echo_bytes(b"\x1b[3~");
        assert!(W.taken().is_empty());
    }

    #[test]
    fn echo_bytes_a_broken_delete_prefix_echoes_literally() {
        static W: EchoRecorder = EchoRecorder::new();
        let device = echo_device(&W);
        // `ESC [ 4 ~` (End) is not Delete: the held prefix is released and
        // echoed as ordinary bytes, exactly what the reader's line buffer
        // stored, so screen and buffer agree.
        device.echo_bytes(b"\x1b[4~");
        assert_eq!(W.taken(), b"\x1b[4~");
    }

    #[test]
    fn secret_feedback_is_inert_until_armed() {
        static W: EchoRecorder = EchoRecorder::new();
        let feedback = SecretFeedback::new(&W);
        // An echoed (non-secret) read must never draw the marker.
        feedback.consumed(b"abc", 0);
        assert!(W.taken().is_empty());
        assert_eq!(feedback.deadline_ns(), None);
    }

    #[test]
    fn secret_feedback_shows_the_marker_on_the_first_typed_byte() {
        static W: EchoRecorder = EchoRecorder::new();
        let feedback = SecretFeedback::new(&W);
        feedback.arm();
        feedback.consumed(b"s", 0);
        assert_eq!(W.taken(), b"[input active.]");
        // The animation wants its one-second tick.
        assert_eq!(
            feedback.deadline_ns(),
            Some(tairix_vt::secret::SECRET_TICK_NS)
        );
    }

    #[test]
    fn secret_feedback_shows_input_complete_on_enter() {
        static W: EchoRecorder = EchoRecorder::new();
        let feedback = SecretFeedback::new(&W);
        feedback.arm();
        feedback.consumed(b"pw\r", 0);
        let written = W.taken();
        // The active marker was drawn once, then replaced in place with the
        // `[input complete]` marker when Enter submitted the line.
        assert!(written.starts_with(b"[input active.]"));
        assert!(written.ends_with(b"[input complete]"));
        assert_eq!(feedback.deadline_ns(), None);
    }

    #[test]
    fn secret_feedback_removes_the_marker_when_the_line_is_fully_erased() {
        static W: EchoRecorder = EchoRecorder::new();
        let feedback = SecretFeedback::new(&W);
        feedback.arm();
        // One character typed, then erased with the Delete key sequence:
        // the whole marker goes away.
        feedback.consumed(b"s\x1b[3~", 0);
        let written = W.taken();
        assert!(written.starts_with(b"[input active.]"));
        assert!(written.ends_with(&marker_rubout(15)));
        assert_eq!(feedback.deadline_ns(), None);
    }

    #[test]
    fn secret_feedback_ticks_keep_the_animation_running() {
        static W: EchoRecorder = EchoRecorder::new();
        let feedback = SecretFeedback::new(&W);
        feedback.arm();
        feedback.consumed(b"s", 0);
        // Each tick advances the dots and arms the next frame — no
        // further typing is required to keep the marker animating.
        feedback.tick(tairix_vt::secret::SECRET_TICK_NS);
        assert_eq!(
            feedback.deadline_ns(),
            Some(2 * tairix_vt::secret::SECRET_TICK_NS)
        );
        feedback.tick(2 * tairix_vt::secret::SECRET_TICK_NS);
        assert_eq!(
            feedback.deadline_ns(),
            Some(3 * tairix_vt::secret::SECRET_TICK_NS)
        );
    }

    #[test]
    fn secret_mode_arms_and_cooked_mode_disarms_the_attached_feedback() {
        static W: EchoRecorder = EchoRecorder::new();
        static FEEDBACK: SecretFeedback = SecretFeedback::new(&W);
        let device = echo_device(&W).with_secret(&FEEDBACK);
        // The secret mode (a password read) arms the feedback…
        device.set_input_mode(InputMode::Secret);
        FEEDBACK.consumed(b"s", 0);
        assert_eq!(W.taken(), b"[input active.]");
        // …and restoring the cooked mode disarms it, rubbing out a marker
        // an aborted read left behind.
        device.set_input_mode(InputMode::Cooked);
        assert!(W.taken().ends_with(&marker_rubout(15)));
        FEEDBACK.consumed(b"x", 0);
        assert_eq!(feedback_tail_after_disarm(&W.taken()), 0);
    }

    #[test]
    fn raw_mode_never_arms_the_attached_feedback() {
        static W: EchoRecorder = EchoRecorder::new();
        static FEEDBACK: SecretFeedback = SecretFeedback::new(&W);
        let device = echo_device(&W).with_secret(&FEEDBACK);
        // A full-screen program's raw read draws nothing: no echo and no
        // activity marker — the program owns every cell of the display.
        device.set_input_mode(InputMode::Raw);
        FEEDBACK.consumed(b"?", 0);
        assert!(W.taken().is_empty());
        // Raw selected while the secret marker is showing removes it: the
        // mode change disarms an armed feedback exactly as cooked does.
        device.set_input_mode(InputMode::Secret);
        FEEDBACK.consumed(b"s", 0);
        assert_eq!(W.taken(), b"[input active.]");
        device.set_input_mode(InputMode::Raw);
        assert!(W.taken().ends_with(&marker_rubout(15)));
    }

    /// The bytes that rub a `width`-column marker off the screen: step the
    /// cursor back over it, blank every column, and step back again.
    fn marker_rubout(width: usize) -> alloc::vec::Vec<u8> {
        let mut bytes = alloc::vec::Vec::new();
        bytes.extend(core::iter::repeat(0x08u8).take(width));
        bytes.extend(core::iter::repeat(b' ').take(width));
        bytes.extend(core::iter::repeat(0x08u8).take(width));
        bytes
    }

    /// How many bytes were written after the disarm rub-out — zero proves a
    /// disarmed feedback stays inert.
    fn feedback_tail_after_disarm(written: &[u8]) -> usize {
        let erase = marker_rubout(15);
        match written
            .windows(erase.len())
            .rposition(|window| window == erase.as_slice())
        {
            Some(pos) => written.len() - (pos + erase.len()),
            None => written.len(),
        }
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

    /// Build a keyboard-style console (queue as both read and input halves)
    /// for the line-discipline tests, returning the device and its queue.
    fn filter_device() -> (&'static ConsoleDevice, &'static ConsoleInputQueue) {
        let queue: &'static ConsoleInputQueue =
            std::boxed::Box::leak(std::boxed::Box::new(ConsoleInputQueue::new()));
        let device: &'static ConsoleDevice = std::boxed::Box::leak(std::boxed::Box::new(
            ConsoleDevice::with_input(&NULL_CONSOLE, queue, queue),
        ));
        (device, queue)
    }

    /// Drain everything currently buffered on `queue`.
    fn drain(queue: &ConsoleInputQueue) -> std::vec::Vec<u8> {
        let mut buf = [0u8; CONSOLE_INPUT_QUEUE_CAPACITY];
        let n = queue.read(&mut buf).expect("queue read");
        buf[..n].to_vec()
    }

    /// Grant `device`'s foreground ownership of task `owner` from `granter`.
    fn grant(device: &ConsoleDevice, granter: u64, owner: u64) {
        device
            .grant_foreground(
                tairix_kernel_sec::TaskId(granter),
                tairix_kernel_sec::TaskId(owner),
            )
            .expect("grant foreground");
    }

    #[test]
    fn cooked_foreground_maps_ctrl_c_to_a_queued_interrupt() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        grant(device, 1, 9);
        // The control byte is consumed (counted as accepted) and the
        // surrounding bytes flow to the reader untouched.
        assert_eq!(device.push(b"ab\x03cd"), Ok(5));
        assert_eq!(drain(queue), b"abcd");
        assert_eq!(
            crate::procsignal::take_pending_foreground_for_test(),
            Some((tairix_kernel_sec::TaskId(9), Signal::Interrupt))
        );
    }

    #[test]
    fn cooked_foreground_maps_ctrl_z_to_a_queued_stop() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        grant(device, 1, 4);
        assert_eq!(device.push(b"\x1a"), Ok(1));
        assert_eq!(drain(queue), b"");
        assert_eq!(
            crate::procsignal::take_pending_foreground_for_test(),
            Some((tairix_kernel_sec::TaskId(4), Signal::Stop))
        );
    }

    #[test]
    fn raw_and_secret_modes_pass_control_bytes_through() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        for mode in [InputMode::Raw, InputMode::Secret] {
            let (device, queue) = filter_device();
            grant(device, 1, 9);
            device.set_input_mode(mode);
            // A full-screen program (raw) or a password read (secret) gets
            // the literal bytes; nothing is queued for delivery.
            assert_eq!(device.push(b"\x03\x1a"), Ok(2));
            assert_eq!(drain(queue), b"\x03\x1a");
            assert_eq!(crate::procsignal::take_pending_foreground_for_test(), None);
        }
    }

    #[test]
    fn cooked_without_a_foreground_passes_control_bytes_through() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        // No foreground set — the shell at its prompt — so `^C` is ordinary
        // input for the reader (elsh's raw editor never even reaches this:
        // it selects raw mode; this is the cooked default).
        assert_eq!(device.push(b"x\x03"), Ok(2));
        assert_eq!(drain(queue), b"x\x03");
        assert_eq!(crate::procsignal::take_pending_foreground_for_test(), None);
    }

    #[test]
    fn clearing_the_foreground_restores_pass_through() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        grant(device, 1, 9);
        assert_eq!(device.push(b"\x03"), Ok(1));
        assert!(crate::procsignal::take_pending_foreground_for_test().is_some());
        // The shell released the slot after its wait returned: bytes flow
        // again.
        device
            .release_foreground(tairix_kernel_sec::TaskId(1))
            .expect("granter releases");
        assert_eq!(device.push(b"\x03"), Ok(1));
        assert_eq!(drain(queue), b"\x03");
        assert_eq!(crate::procsignal::take_pending_foreground_for_test(), None);
    }

    #[test]
    fn a_later_control_byte_replaces_the_pending_one() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        grant(device, 1, 9);
        // Both are accepted; the single pending slot keeps the newest
        // request (the older one is moot once the newer lands).
        assert_eq!(device.push(b"\x03\x1a"), Ok(2));
        assert_eq!(drain(queue), b"");
        assert_eq!(
            crate::procsignal::take_pending_foreground_for_test(),
            Some((tairix_kernel_sec::TaskId(9), Signal::Stop))
        );
    }

    #[test]
    fn filter_reports_a_short_push_when_the_queue_fills() {
        let _guard = crate::procsignal::foreground_test_lock();
        crate::procsignal::ensure_foreground_hook_for_test();
        let (device, queue) = filter_device();
        grant(device, 1, 9);
        // Fill the queue completely, then push a run through the filter:
        // the short push is reported exactly as the bare queue reports it.
        let full = [b'x'; CONSOLE_INPUT_QUEUE_CAPACITY];
        assert_eq!(device.push(&full), Ok(CONSOLE_INPUT_QUEUE_CAPACITY));
        assert_eq!(device.push(b"yz"), Ok(0));
        let _ = drain(queue);
    }

    #[test]
    fn granting_an_unowned_console_records_owner_and_granter() {
        let (device, _queue) = filter_device();
        assert_eq!(device.foreground(), None);
        grant(device, 1, 9);
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(9)));
    }

    #[test]
    fn the_granter_can_retarget_between_its_children() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        // The same granter moves the ownership to another of its children
        // (a new foreground job) without releasing in between.
        grant(device, 1, 12);
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(12)));
    }

    #[test]
    fn the_owner_can_delegate_onward() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        // The foreground owner (a nested shell) hands the console to its
        // own child; it becomes the recorded granter of the new owner.
        grant(device, 9, 20);
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(20)));
        // The delegating owner can reclaim as the new grant's granter.
        device
            .release_foreground(tairix_kernel_sec::TaskId(9))
            .expect("delegating owner releases");
        assert_eq!(device.foreground(), None);
    }

    #[test]
    fn a_bystander_cannot_take_or_retarget_the_ownership() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        // A task that is neither the granter nor the owner is refused; the
        // recorded ownership is untouched.
        assert_eq!(
            device.grant_foreground(tairix_kernel_sec::TaskId(7), tairix_kernel_sec::TaskId(8)),
            Err(Errno::NotForeground)
        );
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(9)));
    }

    #[test]
    fn a_bystander_cannot_release_the_ownership() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        assert_eq!(
            device.release_foreground(tairix_kernel_sec::TaskId(7)),
            Err(Errno::NotForeground)
        );
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(9)));
    }

    #[test]
    fn the_owner_can_release_its_own_ownership() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        device
            .release_foreground(tairix_kernel_sec::TaskId(9))
            .expect("owner releases");
        assert_eq!(device.foreground(), None);
    }

    #[test]
    fn releasing_an_unowned_console_is_an_idempotent_success() {
        let (device, _queue) = filter_device();
        // The granter clears after its child exited (the exit path already
        // cleared the slot): a benign no-op for any caller.
        device
            .release_foreground(tairix_kernel_sec::TaskId(7))
            .expect("idempotent release");
        assert_eq!(device.foreground(), None);
    }

    #[test]
    fn clear_dead_foreground_clears_only_the_matching_owner() {
        let (device, _queue) = filter_device();
        grant(device, 1, 9);
        // Another task's death leaves the recorded ownership in place …
        device.clear_dead_foreground(tairix_kernel_sec::TaskId(7));
        assert_eq!(device.foreground(), Some(tairix_kernel_sec::TaskId(9)));
        // … and the owner's death clears it, so the console is never
        // wedged behind a task that can no longer read it.
        device.clear_dead_foreground(tairix_kernel_sec::TaskId(9));
        assert_eq!(device.foreground(), None);
    }
}
