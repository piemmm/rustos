//! The kernel-side system console seam the `stream_write` (`abi-v1`
//! number 11) and `stream_read` (`abi-v1` number 13) syscalls use
//! (`AGENTS.md` §10 / §16.4).
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
//! tree (`AGENTS.md` §18). `kernel/core` does not know how to talk to a
//! PL011, a 16550, or a framebuffer; it only knows it needs *a* byte
//! sink. [`ConsoleWrite`] is that seam: the boot path installs the
//! concrete device, and the syscall handler writes the copied-in bytes
//! through it.
//!
//! Until a console is installed the handler holds [`NULL_CONSOLE`],
//! which fails closed with [`Errno::NotImplemented`] rather than
//! silently swallowing the bytes (`AGENTS.md` §2.9). A build with no
//! console device wired (a headless target with no UART, an early-boot
//! state before discovery) therefore announces an intentionally inert
//! interface instead of pretending the write succeeded.

use core::sync::atomic::{AtomicBool, Ordering};

use rustos_abi::Errno;
use rustos_kernel_sched_api::SchedulerArch;

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;

/// A byte sink for the privileged system console.
///
/// Implemented by the architecture-port-installed console device (a
/// UART or a framebuffer text console). The trait is deliberately
/// minimal — one method that takes already-copied-in kernel bytes —
/// so `kernel/core` stays free of any device knowledge (`AGENTS.md`
/// §17.4) and the syscall handler owns the user-memory copy and the
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
    /// the validated `copy_from_user` boundary (`AGENTS.md` §5.4) and
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
}

/// The console sink installed before any real device exists.
///
/// Every write fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default `AGENTS.md` §2.9 / §5.4 require, so a
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
/// device knowledge (`AGENTS.md` §17.4) and the syscall handler owns
/// the user-memory copy and the capability check, never the device
/// implementation.
///
/// Implementations must be [`Sync`]: the single installed console is
/// shared by the per-CPU syscall handlers, exactly like
/// [`ConsoleWrite`].
pub trait ConsoleRead: Sync {
    /// Read available console input into `buf`, returning the number of
    /// bytes actually read.
    ///
    /// The caller copies the filled prefix out to user memory through
    /// the validated `copy_to_user` boundary (`AGENTS.md` §5.4) and has
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
/// fail-closed default `AGENTS.md` §2.9 / §5.4 require, so a
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

/// One installed system text console: the output sink and input source
/// of a single console the per-process descriptor table can attach a
/// standard stream to (`AGENTS.md` §20, `plans/PI.md` P11).
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
/// park rather than borrowing another console's input (`AGENTS.md`
/// §2.9 / §5.4).
pub struct ConsoleDevice {
    /// The console's byte sink (`stream_write`).
    pub write: &'static (dyn ConsoleWrite + 'static),
    /// The console's byte source (`stream_read`).
    pub read: &'static (dyn ConsoleRead + 'static),
    /// Whether a `stream_read` of this console echoes the bytes it
    /// consumes back to [`Self::write`] — the terminal local-echo of the
    /// console's read line discipline (`AGENTS.md` §20, `plans/PI.md`
    /// P11). Defaults to **on** so an interactive user sees what they
    /// type; the `stream_echo` syscall toggles it (login disables it
    /// around a password read so a credential is never rendered,
    /// `AGENTS.md` §5.4). Interior mutability because the single
    /// installed console is shared `&'static`.
    echo: AtomicBool,
}

impl ConsoleDevice {
    /// Pair `write` and `read` as one installed console, with terminal
    /// echo on by default (`AGENTS.md` §20 — interactive consoles echo).
    #[must_use]
    pub const fn new(
        write: &'static (dyn ConsoleWrite + 'static),
        read: &'static (dyn ConsoleRead + 'static),
    ) -> Self {
        Self {
            write,
            read,
            echo: AtomicBool::new(true),
        }
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
    pub fn set_echo(&self, enabled: bool) {
        self.echo.store(enabled, Ordering::Relaxed);
    }

    /// Echo `bytes` (the bytes a `stream_read` just consumed) back to the
    /// console output when echo is enabled, so an interactive user sees
    /// what they type (`AGENTS.md` §20 — terminal local echo).
    ///
    /// A carriage return or line feed is echoed as the CR-LF pair so the
    /// cursor both returns to column zero *and* advances a line — a bare
    /// CR (what a serial terminal sends for the Return key) would
    /// otherwise overwrite the current line. Echo is part of the kernel's
    /// read line discipline, so it does not require the reader to also
    /// hold `CAP_CONSOLE_WRITE`.
    ///
    /// Echo is purely cosmetic, so it is **best-effort**: a short write or
    /// a device error is swallowed rather than failing the read the user
    /// asked for (`AGENTS.md` §2.16 — never let a cosmetic side effect
    /// abort the real operation). With echo disabled this is a no-op, so
    /// a suppressed password read touches the output device not at all.
    pub fn echo_bytes(&self, bytes: &[u8]) {
        if !self.echo_enabled() {
            return;
        }
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' || bytes[i] == b'\n' {
                self.echo_run(&bytes[start..i]);
                let _ = self.write.write(b"\r\n");
                start = i + 1;
            }
            i += 1;
        }
        self.echo_run(&bytes[start..]);
    }

    /// Write one run of non-line-break bytes to the console output,
    /// looping over short writes and stopping on a closed/erroring device
    /// (`AGENTS.md` §2.1 — never spin). Best-effort, for [`Self::echo_bytes`].
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
/// arrives — the stream backing owning the wait, exactly as `AGENTS.md`
/// §20 assigns it ("the backing owns blocking", never the program).
///
/// The installed console devices are deliberately non-blocking pollers (a
/// UART RX drain must never busy-wait inside the device, `AGENTS.md`
/// §2.1), so a bare device read with an empty FIFO is a zero-length read.
/// Reported to user space, that zero is indistinguishable from end of
/// input — an interactive session reading its first keystroke would exit
/// instantly. This adapter closes that gap at the seam between the device
/// and the `stream_read` handler: a zero-length inner read parks the
/// calling task back on the scheduler through [`reschedule_current`]
/// (the same poll-and-park loop the `wait` syscall's
/// [`KernelProcessWait`](crate::procwait::KernelProcessWait) producer
/// uses — cooperative, never a busy-spin, `AGENTS.md` §2.1) and re-polls
/// the device when next dispatched, returning only once the device
/// yields bytes or fails.
///
/// A caller that cannot be parked (no resumable user kthread is published
/// on this CPU — a kernel-context read, or a dispatch path outside the
/// user-kthread protocol) fails closed with [`Errno::NotImplemented`]
/// rather than busy-spinning or fabricating an end-of-input (`AGENTS.md`
/// §2.1 / §2.9), mirroring the process-wait producer's contract.
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
    inner: &'static (dyn ConsoleRead + 'static),
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
    pub const fn new(arch: &'static A, inner: &'static (dyn ConsoleRead + 'static)) -> Self {
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
        // (`AGENTS.md` §2.9 — the handler already screens this, defence in
        // depth here).
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // Poll the device first, parking only when it had nothing: a
            // read with pending input never reschedules (`AGENTS.md`
            // §2.16 — no needless work on the hot path). An inner error
            // propagates immediately, fail closed (`AGENTS.md` §2.9).
            let read = self.inner.read(buf)?;
            if read > 0 {
                return Ok(read);
            }
            // Park the caller back on the scheduler; control returns here
            // when it is next dispatched, after which we re-poll. A
            // `false` means no resumable user kthread is published on
            // this CPU — the caller is not a parkable user task, so fail
            // closed rather than busy-spin (`AGENTS.md` §2.1 / §2.9 /
            // §5.4.5), exactly as the process-wait producer does.
            let cpu = self.arch.current_cpu();
            if !reschedule_current(cpu, RescheduleAction::Yield) {
                return Err(Errno::NotImplemented);
            }
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
}
