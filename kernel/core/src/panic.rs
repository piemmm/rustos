//! Architecture-neutral kernel panic policy.
//!
//! The architecture port owns the `#[panic_handler]` attribute itself
//! (Stage 3) — `kernel/core` cannot, because in a host-test build
//! `std` already supplies a handler and registering a second one is a
//! link error. The arch port's `#[panic_handler]` is therefore a
//! one-liner that delegates here:
//!
//! ```ignore
//! #[panic_handler]
//! fn tairix_panic(info: &core::panic::PanicInfo<'_>) -> ! {
//!     tairix_kernel_core::handle_panic(info, &PANIC_CTX)
//! }
//! ```
//!
//! `PANIC_CTX` is the [`PanicContext`] the arch port builds at boot
//! and stores in a once-initialised `static` (the per-CPU bootstrap
//! exception called out by — *"No global mutable static
//! beyond the per-CPU bootstrap area"*).
//!
//! [`handle_panic`] does the rest: it logs a structured
//! [`AuditEvent::Panic`] record with the failing file, line, column,
//! and the current CPU id, then calls [`KernelArch::halt`]. It
//! **never** silently resets — the `!` return type and the `halt`
//! contract together encode that.
//!
//! A fatal CPU exception taken in kernel mode enters the same reporting
//! path through [`fault_dump`], carrying a [`KernelFault`] instead of a
//! source location: the port's synchronous-exception vector has no fix-up
//! for it, so it is as fatal as a `panic!` and deserves the same register
//! snapshot and backtrace. Only the three cause fields and the audit event
//! id differ, so there is one dump, not two.
//!
//! # Testability
//!
//! `core::panic::PanicInfo` has no public constructor on stable Rust,
//! so [`handle_panic`] forwards the location into [`panic_dump`], an
//! inner function that takes `Option<&core::panic::Location<'_>>`
//! directly. Host-side tests drive [`panic_dump`] with a
//! [`core::panic::Location::caller`]; integration tests cover the
//! `panic!`-to-`handle_panic` round-trip end-to-end.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use tairix_arch_api::backtrace::{
    walk, CpuStateCapture, StackReader, MAX_FRAMES as BACKTRACE_MAX_FRAMES, MAX_NAMED_REGS,
};
use tairix_log::{log, Event, Field, FieldValue, Level, Sink};

use crate::audit::AuditEvent;
use crate::bootinfo::KernelArch;

/// Set on entry to [`panic_dump`] so a panic taken *inside* the panic
/// handler cannot recurse into the register/backtrace machinery (a
/// fault-in-fault-handler would be a triple fault). The first entry does
/// the full dump; any re-entry emits one terse record and halts. It is
/// never cleared in production — the handler halts the CPU and never
/// returns — so a live `true` always means "already dumping".
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Reset the re-entrancy guard. Test-only: production never clears it
/// (the handler halts and never returns), but host tests drive
/// [`panic_dump`] repeatedly and must start each from a clean state.
#[cfg(test)]
fn reset_panic_guard() {
    PANICKING.store(false, Ordering::Release);
}

/// Production [`StackReader`]: reads one kernel-stack word by raw address.
struct RawStackReader;

impl StackReader for RawStackReader {
    fn read_word(&self, addr: u64) -> Option<u64> {
        // SAFETY: the neutral `walk` validated `addr` is 8-byte aligned and
        // that its whole word lies within the current CPU's kernel-stack
        // bounds before calling. Such an address is live, mapped kernel
        // stack, so the read cannot fault. The read is volatile so the
        // compiler cannot elide or reorder it on the panic path. This
        // reader is over the kernel's own trusted stack, so it always
        // yields a value — the fallible return exists for the user-stack
        // reader, which reads an untrusted address space.
        Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
    }
}

/// Context the architecture port hands to [`handle_panic`].
///
/// The struct is borrowed read-only by the panic path, so the arch
/// port can store it inside a `kernel/sync::Once`-protected `static`
/// without taking any locks at panic time (the panic path never
/// blocks).
pub struct PanicContext<'a, A: KernelArch + 'static> {
    /// Architecture port instance.
    pub arch: &'a A,
    /// Sink that receives the [`AuditEvent::Panic`] record.
    ///
    /// In production this is the same `audit_sink` passed to
    /// [`crate::kernel_main`]; host tests reuse `TestSink`.
    pub audit_sink: &'a (dyn Sink + Sync),
    /// The port's post-mortem CPU-state handle, when it has published one.
    ///
    /// When `Some`, [`panic_dump`] adds a register snapshot and a bounded
    /// frame-pointer backtrace to the [`AuditEvent::Panic`] record. When
    /// `None` (a port that has not published a handle, or the pre-init
    /// window before it is available) the dump carries only the base
    /// `cpu`/`file`/`line`/`column` fields — never a faked backtrace.
    pub backtrace: Option<&'a dyn CpuStateCapture>,

    /// The installed system consoles, so the dump takes the display surface
    /// back before it writes
    /// ([`crate::console::ConsoleWrite::reclaim_surface`]).
    ///
    /// A graphical session that holds a seat owns the scan-out surface, and
    /// the text console hands it over rather than scribbling on the
    /// composited frame. That must never hide a kernel panic: on a port whose
    /// log sink renders to the framebuffer, the report would otherwise land
    /// in a hidden console's retained screen while the user stares at a
    /// frozen frame. Reclaiming first is the `console_unblank` a fatal fault
    /// has always deserved.
    ///
    /// Defaults to [`crate::console::NO_CONSOLES`]: a port that wires no
    /// console has no surface to reclaim, and the report reaches its log sink
    /// exactly as before.
    pub consoles: &'a [crate::console::ConsoleDevice],
}

impl<'a, A: KernelArch> PanicContext<'a, A> {
    /// Construct a panic context with no post-mortem handle.
    ///
    /// A port that can capture registers and unwind installs its handle
    /// with [`Self::with_backtrace`]; until then the dump is the base
    /// record.
    #[must_use]
    pub fn new(arch: &'a A, audit_sink: &'a (dyn Sink + Sync)) -> Self {
        Self {
            arch,
            audit_sink,
            backtrace: None,
            consoles: &crate::console::NO_CONSOLES,
        }
    }

    /// Attach the port's [`CpuStateCapture`] handle, consuming and
    /// returning `self`, so the dump carries registers + a backtrace.
    #[must_use]
    pub fn with_backtrace(mut self, backtrace: &'a dyn CpuStateCapture) -> Self {
        self.backtrace = Some(backtrace);
        self
    }

    /// Attach the installed console list, so the dump reclaims the display
    /// surface before it writes (see [`Self::consoles`]).
    #[must_use]
    pub fn with_consoles(mut self, consoles: &'a [crate::console::ConsoleDevice]) -> Self {
        self.consoles = consoles;
        self
    }
}

/// Dump the panic context to the audit sink and halt the boot CPU.
///
/// The function never returns: the `!` return type and the
/// [`KernelArch::halt`] contract together guarantee that the kernel
/// does not silently reset (Stage 2 deliverables).
///
/// # Emitted fields
///
/// | Key      | Value                                                    |
/// | -------- | -------------------------------------------------------- |
/// | `cpu`    | Decimal CPU id returned by `arch.current_cpu()`.         |
/// | `file`   | `info.location().file()` or `"<unknown>"`.               |
/// | `line`   | Decimal `info.location().line()` or `"0"`.               |
/// | `column` | Decimal `info.location().column()` or `"0"`.             |
///
/// When the panic context carries a [`CpuStateCapture`] handle, the record
/// additionally carries a register snapshot and a bounded backtrace:
///
/// | Key             | Value                                                       |
/// | --------------- | ----------------------------------------------------------- |
/// | `pc` `sp` `fp`  | `0x`-prefixed 64-bit hex of the captured PC / SP / FP.      |
/// | `<reg>`         | One per captured named GP register (e.g. `rax`, `x0`, `ra`).|
/// | `frame_0`       | The captured program counter (top of the call chain).      |
/// | `frame_1..`     | Return addresses recovered by the frame-pointer walk.      |
///
/// Kernel addresses are printed deliberately: a kernel panic is fatal,
/// non-recoverable, and halting, so its dump carries the addresses a
/// post-mortem needs (resolved offline against the unstripped kernel ELF —
/// see `docs/src/architecture/panic-diagnostics.md`). This is distinct
/// from [`AuditEvent::TaskFaultKilled`], which still omits the raw *user*
/// faulting address (no ASLR/layout leak from a survivable per-task event).
///
/// The format is part of the audit contract and
/// is asserted by the integration tests.
pub fn handle_panic<A: KernelArch>(info: &PanicInfo<'_>, ctx: &PanicContext<'_, A>) -> ! {
    panic_dump(info.location(), ctx)
}

/// A fatal CPU exception taken in **kernel** mode, as the port's
/// synchronous-exception vector saw it.
///
/// The same three words on every port, spelled differently per
/// architecture: `ESR_EL1` / `FAR_EL1` / `ELR_EL1` on aarch64, the `#PF`
/// error code / faulting linear address / `RIP` on x86_64, and `scause` /
/// `stval` / `sepc` on riscv64. The port's shim names them; everything
/// above it reads the neutral triple.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct KernelFault {
    /// The port's exception syndrome — why the CPU trapped.
    pub syndrome: u64,
    /// The address the faulting access could not reach.
    pub address: u64,
    /// The faulting instruction.
    pub pc: u64,
}

impl core::fmt::Display for KernelFault {
    /// One line, hex, in the field order the audit record uses, so the
    /// pre-init console path and the structured dump read alike.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "syndrome={:#018x} fault_addr={:#018x} fault_pc={:#018x}",
            self.syndrome, self.address, self.pc
        )
    }
}

/// What brought the kernel down — the only thing that differs between the
/// two entries into [`dump`].
enum Fatal<'a> {
    /// A Rust `panic!`, with its source location when one is available.
    Panic(Option<&'a core::panic::Location<'a>>),
    /// A fatal kernel-mode CPU exception.
    Fault(KernelFault),
}

impl Fatal<'_> {
    /// The audit event this cause is recorded under.
    fn event(&self) -> AuditEvent {
        match self {
            Self::Panic(_) => AuditEvent::Panic,
            Self::Fault(_) => AuditEvent::KernelFault,
        }
    }

    /// The terse message the re-entrancy guard emits, naming the cause so a
    /// re-entered report is still attributable.
    fn nested_message(&self) -> &'static str {
        match self {
            Self::Panic(_) => "kernel panic (nested — re-entered the fatal-report path)",
            Self::Fault(_) => "fatal kernel fault (nested — re-entered the fatal-report path)",
        }
    }
}

/// Number of register fields the dump can carry: `pc`/`sp`/`fp` plus the
/// port's named general-purpose registers.
const REG_CAP: usize = MAX_NAMED_REGS + 3;
/// Number of backtrace frames the dump can carry (matches the walker cap).
const FRAME_CAP: usize = BACKTRACE_MAX_FRAMES;
/// Total field slots: the four base fields plus the register and frame
/// blocks.
const FIELD_CAP: usize = 4 + REG_CAP + FRAME_CAP;

/// Capture the register snapshot and walk the backtrace into the caller's
/// stack buffers, returning `(n_regs, n_frames)`.
///
/// Split out of [`panic_dump`] only to keep that function flat; it is
/// still allocation-free and reads stack memory only through the
/// bounds-checked [`RawStackReader`] (the walk never faults on a corrupt
/// chain). The buffers are borrowed from the panic frame so their contents
/// outlive the assembled field list.
fn capture_into(
    bt: &dyn CpuStateCapture,
    reg_bufs: &mut [[u8; 18]; REG_CAP],
    reg_names: &mut [&'static str; REG_CAP],
    frame_bufs: &mut [[u8; 18]; FRAME_CAP],
    frame_keys: &mut [[u8; 16]; FRAME_CAP],
    frame_key_lens: &mut [usize; FRAME_CAP],
) -> (usize, usize) {
    let snap = bt.capture();

    // Explicit unwinder-critical registers first, then the named GP
    // registers the port captured.
    let mut n_regs = 0usize;
    let mut push_reg = |name: &'static str, value: u64| {
        if n_regs < REG_CAP {
            let _ = format_hex_u64(value, &mut reg_bufs[n_regs]);
            reg_names[n_regs] = name;
            n_regs += 1;
        }
    };
    push_reg("pc", snap.pc);
    push_reg("sp", snap.sp);
    push_reg("fp", snap.fp);
    for reg in snap.named() {
        push_reg(reg.name, reg.value);
    }

    // Frame 0 is the captured program counter (the fault site's frame);
    // the frame-pointer walk appends the caller return addresses. The walk
    // reads memory only within the port's vouched stack bounds and is
    // depth-capped, so a corrupt chain terminates without faulting.
    let mut frame_addrs = [0u64; FRAME_CAP];
    let mut n_frames = 0usize;
    if snap.pc != 0 {
        frame_addrs[0] = snap.pc;
        n_frames = 1;
    }
    if let (Some(layout), Some(bounds)) = (bt.frame_layout(), bt.stack_bounds()) {
        walk(&RawStackReader, snap.fp, layout, bounds, |ra| {
            if n_frames < FRAME_CAP {
                frame_addrs[n_frames] = ra;
                n_frames += 1;
            }
        });
    }

    // Format each frame address as hex and its `frame_N` key.
    for (i, addr) in frame_addrs.iter().take(n_frames).enumerate() {
        let _ = format_hex_u64(*addr, &mut frame_bufs[i]);
        frame_key_lens[i] = format_frame_key(i, &mut frame_keys[i]);
    }

    (n_regs, n_frames)
}

/// Audit-and-halt path shared by [`handle_panic`] and the host-side
/// tests.
///
/// Split out so tests can drive the full code path on stable Rust
/// without needing to construct a [`core::panic::PanicInfo`] (which
/// has no public constructor).
pub fn panic_dump<A: KernelArch>(
    location: Option<&core::panic::Location<'_>>,
    ctx: &PanicContext<'_, A>,
) -> ! {
    dump(&Fatal::Panic(location), ctx)
}

/// Dump a fatal **kernel-mode CPU exception** and halt the CPU.
///
/// The port's synchronous-exception vector reaches this through its
/// `extern "C"` shim for an exception it has no fix-up for — a same-EL
/// abort, a supervisor page fault, an illegal instruction. Resuming would
/// re-trap forever, so it is exactly as fatal as a `panic!` and takes the
/// same path: the same register snapshot, the same bounded backtrace, the
/// same re-entrancy guard, the same halt. Only the three cause fields and
/// the audit event id differ.
///
/// # Emitted fields
///
/// | Key          | Value                                                  |
/// | ------------ | ------------------------------------------------------ |
/// | `cpu`        | Decimal CPU id returned by `arch.current_cpu()`.       |
/// | `syndrome`   | 64-bit hex of the port's exception syndrome.           |
/// | `fault_addr` | 64-bit hex of the address the access could not reach.  |
/// | `fault_pc`   | 64-bit hex of the faulting instruction.                |
///
/// followed by the register and `frame_N` blocks [`panic_dump`] documents.
/// `fault_pc` is the *interrupted* instruction; the register block's `pc`
/// is where the shim itself was captured, so the two are deliberately
/// distinct keys.
pub fn fault_dump<A: KernelArch>(fault: KernelFault, ctx: &PanicContext<'_, A>) -> ! {
    dump(&Fatal::Fault(fault), ctx)
}

/// Stack storage the three cause-specific fields are formatted into.
///
/// Declared in [`dump`]'s frame so the formatted strings outlive the
/// assembled field list; a panic report allocates nothing.
struct CauseBufs {
    line: [u8; 11],
    column: [u8; 11],
    syndrome: [u8; 18],
    address: [u8; 18],
    pc: [u8; 18],
}

impl CauseBufs {
    const fn new() -> Self {
        Self {
            line: [0; 11],
            column: [0; 11],
            syndrome: [0; 18],
            address: [0; 18],
            pc: [0; 18],
        }
    }
}

/// Format the three cause-specific fields: a panic's source position, or
/// the hardware syndrome of a kernel-mode fault. Never both, and neither is
/// fabricated for the other.
fn cause_fields<'b>(fatal: &Fatal<'b>, bufs: &'b mut CauseBufs) -> [Field<'b>; 3] {
    match *fatal {
        Fatal::Panic(location) => {
            let (file_str, line_str, col_str) = match location {
                Some(loc) => (
                    loc.file(),
                    format_u32(loc.line(), &mut bufs.line),
                    format_u32(loc.column(), &mut bufs.column),
                ),
                None => ("<unknown>", "0", "0"),
            };
            [
                Field {
                    key: "file",
                    value: FieldValue::Str(file_str),
                },
                Field {
                    key: "line",
                    value: FieldValue::Str(line_str),
                },
                Field {
                    key: "column",
                    value: FieldValue::Str(col_str),
                },
            ]
        }
        Fatal::Fault(fault) => [
            Field {
                key: "syndrome",
                value: FieldValue::Str(format_hex_u64(fault.syndrome, &mut bufs.syndrome)),
            },
            Field {
                key: "fault_addr",
                value: FieldValue::Str(format_hex_u64(fault.address, &mut bufs.address)),
            },
            Field {
                key: "fault_pc",
                value: FieldValue::Str(format_hex_u64(fault.pc, &mut bufs.pc)),
            },
        ],
    }
}

/// Emit the terse record for a report re-entered while one was already
/// being written, under the re-entering cause's own event id.
fn report_nested(fatal: &Fatal<'_>, cpu: u32, sink: &(dyn Sink + Sync)) {
    let mut cpu_buf = [0u8; 11];
    let fields = [Field {
        key: "cpu",
        value: FieldValue::Str(format_u32(cpu, &mut cpu_buf)),
    }];
    log(
        sink,
        &Event {
            level: Level::Error,
            id: fatal.event().id(),
            message: fatal.nested_message(),
            fields: &fields,
        },
    );
}

/// The one fatal-report body: reclaim the display, guard against
/// re-entry, emit a single audit record describing `fatal` with a register
/// snapshot and a bounded backtrace, then halt.
fn dump<A: KernelArch>(fatal: &Fatal<'_>, ctx: &PanicContext<'_, A>) -> ! {
    let cpu = ctx.arch.current_cpu();

    // Take the display surface back before anything is written. A graphical
    // session holding a seat owns the scan-out, and the text console hands it
    // over rather than drawing on the composited frame — but a panic must
    // never be invisible, so the report gets the screen whatever was on it.
    // Best-effort and idempotent, and ahead of the re-entrancy guard so a
    // nested panic's terse record is visible too.
    for device in ctx.consoles {
        device.reclaim_surface();
    }

    // Re-entrancy guard: a panic taken *inside* this handler (e.g. the
    // sink or a register read faulting) must not recurse into the walk.
    // The first entry does the full dump; any re-entry emits one terse
    // record and halts immediately.
    if PANICKING.swap(true, Ordering::AcqRel) {
        report_nested(fatal, cpu, ctx.audit_sink);
        ctx.arch.halt();
    }

    // Stack-resident formatting buffers. No allocation on the panic path —
    // it must not depend on the heap, which may itself be the source of
    // the panic (the OOM case).
    let mut cpu_buf = [0u8; 11];
    let mut cause_bufs = CauseBufs::new();

    let cpu_str = format_u32(cpu, &mut cpu_buf);
    let cause = cause_fields(fatal, &mut cause_bufs);

    // --- Register snapshot + bounded backtrace (when the port published a
    // handle). Everything below is stack-buffered and allocation-free.
    //
    // Registers are emitted as `pc`/`sp`/`fp` plus each named GP register,
    // each a fixed 18-byte `0x`-prefixed 64-bit hex string. The backtrace
    // is emitted as `frame_0` (the captured program counter) followed by
    // `frame_1..` (the return addresses the frame-pointer walk recovers),
    // each a hex string. Kernel addresses are deliberately printed here: a
    // kernel panic is fatal, non-recoverable, and halting, so its dump
    // carries the addresses a post-mortem needs (resolved offline against
    // the unstripped kernel ELF with addr2line — see the panic-diagnostics
    // doc). This is distinct from `AuditEvent::TaskFaultKilled`, which
    // still omits the raw *user* faulting address (no ASLR/layout leak from
    // a survivable, per-task event).
    let mut reg_bufs = [[0u8; 18]; REG_CAP];
    let mut reg_names = [""; REG_CAP];
    let mut frame_bufs = [[0u8; 18]; FRAME_CAP];
    let mut frame_keys = [[0u8; 16]; FRAME_CAP];
    let mut frame_key_lens = [0usize; FRAME_CAP];
    let (n_regs, n_frames) = match ctx.backtrace {
        Some(bt) => capture_into(
            bt,
            &mut reg_bufs,
            &mut reg_names,
            &mut frame_bufs,
            &mut frame_keys,
            &mut frame_key_lens,
        ),
        None => (0, 0),
    };

    // Assemble the single record's field list. Every referenced buffer
    // (`reg_bufs`, `reg_names`, `frame_bufs`, `frame_keys`) is a local
    // declared above and so outlives `fields`; the borrows below are
    // ordinary shared borrows, no lifetime laundering needed.
    let mut fields = [Field {
        key: "cpu",
        value: FieldValue::Str(cpu_str),
    }; FIELD_CAP];
    fields[0] = Field {
        key: "cpu",
        value: FieldValue::Str(cpu_str),
    };
    fields[1..4].copy_from_slice(&cause);
    let mut n = 4usize;
    for i in 0..n_regs {
        let value = core::str::from_utf8(&reg_bufs[i]).unwrap_or("0x?");
        fields[n] = Field {
            key: reg_names[i],
            value: FieldValue::Str(value),
        };
        n += 1;
    }
    for i in 0..n_frames {
        let key = core::str::from_utf8(&frame_keys[i][..frame_key_lens[i]]).unwrap_or("frame_?");
        let value = core::str::from_utf8(&frame_bufs[i]).unwrap_or("0x?");
        fields[n] = Field {
            key,
            value: FieldValue::Str(value),
        };
        n += 1;
    }

    let event = fatal.event();
    log(
        ctx.audit_sink,
        &Event {
            level: Level::Error,
            id: event.id(),
            message: event.message(),
            fields: &fields[..n],
        },
    );

    ctx.arch.halt();
}

/// Format a `u32` into `buf` as decimal ASCII and return a borrowed
/// `&str` over the populated suffix.
///
/// Allocation-free; the buffer must be at least 11 bytes (the longest
/// `u32` decimal is `"4294967295"`). The function is total: every
/// `u32` input yields a valid ASCII string. Used only by the panic
/// path so the panic handler cannot itself panic on an allocator that
/// is already wedged.
fn format_u32(value: u32, buf: &mut [u8; 11]) -> &str {
    if value == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut n = value;
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("0")
}

/// Format a `u64` into `buf` as a fixed-width `0x`-prefixed 16-digit
/// lowercase hex string and return a borrowed `&str` over the whole
/// buffer.
///
/// Allocation-free and total; the buffer is exactly 18 bytes (`"0x"` plus
/// 16 hex digits), so every `u64` fills it completely and the returned
/// `&str` is always the full buffer. Fixed width (rather than minimal)
/// keeps a column of register/frame addresses aligned in the dump and
/// means no length bookkeeping. Used only by the panic path.
fn format_hex_u64(value: u64, buf: &mut [u8; 18]) -> &str {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    buf[0] = b'0';
    buf[1] = b'x';
    let mut v = value;
    let mut i = 18;
    while i > 2 {
        i -= 1;
        buf[i] = HEX[(v & 0xf) as usize];
        v >>= 4;
    }
    core::str::from_utf8(&buf[..]).unwrap_or("0x0000000000000000")
}

/// Format the backtrace field key `frame_<index>` into `buf` and return
/// its byte length.
///
/// Allocation-free; `buf` must be at least 16 bytes (`"frame_"` plus the
/// decimal index — the index is capped at
/// [`BACKTRACE_MAX_FRAMES`], so at most two digits). Used only by the
/// panic path.
fn format_frame_key(index: usize, buf: &mut [u8; 16]) -> usize {
    const PREFIX: &[u8] = b"frame_";
    buf[..PREFIX.len()].copy_from_slice(PREFIX);
    let mut num_buf = [0u8; 11];
    // `index` is bounded by the frame cap; the cast is lossless.
    let num = format_u32(u32::try_from(index).unwrap_or(u32::MAX), &mut num_buf);
    let num_bytes = num.as_bytes();
    let end = PREFIX.len() + num_bytes.len();
    buf[PREFIX.len()..end].copy_from_slice(num_bytes);
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_arch::{TestArch, HALT_SENTINEL};
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::string::String;
    use core::panic::Location;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use tairix_arch_api::backtrace::{
        Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot, StackBounds,
    };

    /// Serialises the tests that drive [`panic_dump`]. The re-entrancy
    /// guard [`PANICKING`] is a process-global, so two panic-driving tests
    /// running in parallel would race on it; holding this lock for the
    /// duration of each such test makes the guard state deterministic
    /// (no flaky tests). `catch_unwind` swallows the inner halt-panic, so
    /// the lock is never poisoned.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn format_u32_examples() {
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(0, &mut buf), "0");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(1, &mut buf), "1");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(4_294_967_295, &mut buf), "4294967295");
        let mut buf = [0u8; 11];
        assert_eq!(format_u32(42, &mut buf), "42");
    }

    fn drive_panic_dump<F>(make_location: F) -> (TestArch, &'static TestSink)
    where
        F: FnOnce() -> &'static Location<'static>,
    {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();
        let arch = TestArch::with_cpus(2);
        arch.set_current_cpu(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let loc = make_location();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(Some(loc), &ctx);
        }));
        let err = result.expect_err("halt path must panic via TestArch");
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&'static str>().copied())
            .unwrap_or("");
        assert!(msg.contains(HALT_SENTINEL), "halt sentinel missing: {msg}");
        (arch, sink)
    }

    #[track_caller]
    fn caller_location() -> &'static Location<'static> {
        Location::caller()
    }

    #[test]
    fn panic_dump_emits_one_record_with_documented_fields() {
        let (arch, sink) = drive_panic_dump(caller_location);

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "expected exactly one panic record");
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::Panic.id());
        assert_eq!(ev.level, Level::Error);

        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("cpu"), Some("1"));
        // `track_caller` propagates through `drive_panic_dump`'s
        // `make_location()` call, so the file is whichever source
        // contains the `make_location` invocation. Asserting on a
        // specific file path is fragile across host targets; assert
        // the field exists and is non-empty instead.
        assert!(field("file").is_some_and(|s| !s.is_empty()));
        assert!(field("line").is_some_and(|s| s.parse::<u32>().is_ok()));
        assert!(field("column").is_some_and(|s| s.parse::<u32>().is_ok()));

        assert_eq!(arch.halt_count(), 1);
    }

    #[test]
    fn panic_dump_handles_missing_location() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();
        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("file"), Some("<unknown>"));
        assert_eq!(field("line"), Some("0"));
        assert_eq!(field("column"), Some("0"));
        assert_eq!(arch.halt_count(), 1);
    }

    #[test]
    fn format_hex_u64_is_fixed_width_lowercase() {
        let mut buf = [0u8; 18];
        assert_eq!(format_hex_u64(0, &mut buf), "0x0000000000000000");
        let mut buf = [0u8; 18];
        assert_eq!(format_hex_u64(0xdead_beef, &mut buf), "0x00000000deadbeef");
        let mut buf = [0u8; 18];
        assert_eq!(format_hex_u64(u64::MAX, &mut buf), "0xffffffffffffffff");
        let mut buf = [0u8; 18];
        assert_eq!(
            format_hex_u64(0xffff_8000_0000_1111, &mut buf),
            "0xffff800000001111"
        );
    }

    #[test]
    fn format_frame_key_numbers_frames() {
        let mut buf = [0u8; 16];
        let n = format_frame_key(0, &mut buf);
        assert_eq!(&buf[..n], b"frame_0");
        let mut buf = [0u8; 16];
        let n = format_frame_key(63, &mut buf);
        assert_eq!(&buf[..n], b"frame_63");
    }

    /// A host [`CpuStateCapture`] that points the walker at a real, safe
    /// stack image built in a `Vec`, so `panic_dump`'s production
    /// `RawStackReader` reads genuine mapped host memory (not a fault).
    struct HostCapture {
        pc: u64,
        sp: u64,
        fp: u64,
        bounds: StackBounds,
    }

    impl CpuStateCapture for HostCapture {
        fn profile(&self) -> BacktraceProfile {
            BacktraceProfile {
                register_capture: Backtrace::Supported,
                frame_unwind: Backtrace::Supported,
            }
        }
        fn capture(&self) -> RegisterSnapshot {
            RegisterSnapshot::new(self.pc, self.sp, self.fp)
                .with("rax", 0x1234)
                .with("rbx", 0x5678)
        }
        fn frame_layout(&self) -> Option<FrameLayout> {
            // System V / AAPCS64 layout: saved fp at [fp], ret at [fp+8].
            Some(FrameLayout {
                saved_fp_offset: 0,
                return_addr_offset: 8,
            })
        }
        fn stack_bounds(&self) -> Option<StackBounds> {
            Some(self.bounds)
        }
    }

    #[test]
    fn panic_dump_emits_registers_and_backtrace_when_handle_present() {
        // vec[0] = caller fp (fp1), vec[1] = RET1, vec[2] = 0 (terminator),
        // vec[3] = RET2. fp0 is at &vec[0]; fp1 at &vec[2].
        const RET1: u64 = 0xffff_8000_0000_1111;
        const RET2: u64 = 0xffff_8000_0000_2222;
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        // Build a two-frame chain in a real Vec the walker can read safely.
        let mut stack: alloc::vec::Vec<u64> = alloc::vec![0u64; 4];
        let base = stack.as_ptr() as u64;
        let fp0 = base;
        let fp1 = base + 16;
        stack[0] = fp1; // caller fp of frame 0
        stack[1] = RET1; // return address of frame 0
        stack[2] = 0; // caller fp of frame 1 (terminates the walk)
        stack[3] = RET2; // return address of frame 1
        let bounds = StackBounds::new(base, base + 32);
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: fp0,
            bounds,
        };

        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let cap_ref: &dyn CpuStateCapture = &cap;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(cap_ref);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "one panic record");
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };

        // Register block present.
        assert_eq!(field("pc"), Some("0xffff800000000000"));
        assert_eq!(field("rax"), Some("0x0000000000001234"));
        assert_eq!(field("rbx"), Some("0x0000000000005678"));

        // Backtrace: frame_0 = pc, frame_1 = RET1, frame_2 = RET2.
        assert_eq!(field("frame_0"), Some("0xffff800000000000"));
        assert_eq!(field("frame_1"), Some("0xffff800000001111"));
        assert_eq!(field("frame_2"), Some("0xffff800000002222"));
        // The chain terminated (no fourth frame).
        assert_eq!(field("frame_3"), None);
        assert_eq!(arch.halt_count(), 1);
    }

    /// A hidden console is shown again before the report is written, and a
    /// nested panic reclaims too: on a port whose log sink renders to the
    /// framebuffer, an oops raised under a graphical session would otherwise
    /// land in the retained screen while the user stares at a frozen frame.
    #[test]
    fn panic_dump_reclaims_the_display_surface_first() {
        /// What the dump did, and in which order. Shared by the console and
        /// the sink so the test can prove the reclaim came first.
        #[derive(Default)]
        struct Surface {
            reclaimed: AtomicBool,
            reported: AtomicBool,
            reclaimed_before_report: AtomicBool,
        }

        struct SurfaceConsole(&'static Surface);

        impl crate::console::ConsoleWrite for SurfaceConsole {
            fn write(&self, bytes: &[u8]) -> Result<usize, tairix_abi::Errno> {
                Ok(bytes.len())
            }

            fn reclaim_surface(&self) {
                self.0.reclaimed.store(true, Ordering::SeqCst);
            }
        }

        /// Stands in for a port whose log sink renders to the framebuffer
        /// (the aarch64 `SerialSink` does, on a release build with a live
        /// surface): the report only reaches the screen if the surface was
        /// reclaimed before it was written.
        struct SurfaceSink(&'static Surface);

        impl Sink for SurfaceSink {
            fn write_event(&self, _event: &Event<'_>) {
                if self.0.reclaimed.load(Ordering::SeqCst) {
                    self.0.reclaimed_before_report.store(true, Ordering::SeqCst);
                }
                self.0.reported.store(true, Ordering::SeqCst);
            }
        }

        for nested in [false, true] {
            let _serial = TEST_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_panic_guard();
            PANICKING.store(nested, Ordering::Release);

            let surface: &'static Surface = Box::leak(Box::new(Surface::default()));
            let console: &'static SurfaceConsole = Box::leak(Box::new(SurfaceConsole(surface)));
            let consoles: &'static [crate::console::ConsoleDevice] =
                Box::leak(Box::new([crate::console::ConsoleDevice::new(
                    console,
                    &crate::console::NULL_CONSOLE_READ,
                )]));
            let sink: &'static SurfaceSink = Box::leak(Box::new(SurfaceSink(surface)));
            let arch = TestArch::with_cpus(1);
            let result = catch_unwind(AssertUnwindSafe(|| {
                let ctx = PanicContext::new(&arch, sink).with_consoles(consoles);
                panic_dump(None, &ctx);
            }));
            assert!(result.is_err());

            assert!(
                surface.reported.load(Ordering::SeqCst),
                "a record is always emitted (nested: {nested})"
            );
            assert!(
                surface.reclaimed_before_report.load(Ordering::SeqCst),
                "the surface is reclaimed before the report (nested: {nested})"
            );
            reset_panic_guard();
        }
    }

    #[test]
    fn nested_panic_emits_terse_record_and_halts() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();
        // Simulate a panic taken while already inside the handler.
        PANICKING.store(true, Ordering::Release);

        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert!(
            events[0].message.contains("nested"),
            "nested panic must be terse: {}",
            events[0].message
        );
        assert_eq!(arch.halt_count(), 1);
        reset_panic_guard();
    }

    /// The three cause fields of a kernel-mode fault, and the absence of a
    /// source position it does not have.
    #[test]
    fn fault_dump_emits_one_kernel_fault_record_with_documented_fields() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(4);
        arch.set_current_cpu(3);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0045,
                    address: 0xffff_0000_dead_beef,
                    pc: 0x0000_0000_8010_1234,
                },
                &ctx,
            );
        }));
        assert!(result.is_err(), "the fault path must halt");

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "expected exactly one fault record");
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::KernelFault.id());
        assert_eq!(ev.level, Level::Error);
        assert_eq!(ev.message, AuditEvent::KernelFault.message());

        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("cpu"), Some("3"));
        assert_eq!(field("syndrome"), Some("0x0000000096000045"));
        assert_eq!(field("fault_addr"), Some("0xffff0000deadbeef"));
        assert_eq!(field("fault_pc"), Some("0x0000000080101234"));
        // A fault has no source position, and none is fabricated for it.
        assert_eq!(field("file"), None);
        assert_eq!(field("line"), None);
        assert_eq!(field("column"), None);

        assert_eq!(arch.halt_count(), 1);
    }

    /// A fault report carries the same register snapshot and bounded
    /// backtrace a panic does — one dump, two causes — and keeps the
    /// faulting instruction distinct from the captured `pc`.
    #[test]
    fn fault_dump_carries_the_shared_register_and_backtrace_block() {
        const RET1: u64 = 0xffff_8000_0000_1111;
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let mut stack: alloc::vec::Vec<u64> = alloc::vec![0u64; 2];
        let base = stack.as_ptr() as u64;
        stack[0] = 0; // caller fp terminates the walk
        stack[1] = RET1;
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: base,
            bounds: StackBounds::new(base, base + 16),
        };

        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let cap_ref: &dyn CpuStateCapture = &cap;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(cap_ref);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0045,
                    address: 0xffff_0000_dead_beef,
                    pc: 0x0000_0000_8010_1234,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(ev.id, AuditEvent::KernelFault.id());
        assert_eq!(field("pc"), Some("0xffff800000000000"));
        assert_eq!(field("rax"), Some("0x0000000000001234"));
        assert_eq!(field("frame_0"), Some("0xffff800000000000"));
        assert_eq!(field("frame_1"), Some("0xffff800000001111"));
        // The faulting instruction is not the shim's captured `pc`.
        assert_eq!(field("fault_pc"), Some("0x0000000080101234"));
    }

    /// The re-entrancy guard is shared by both causes — a fault taken while
    /// a report is already being written emits one terse record under its
    /// own event id, never recursing into the walk.
    #[test]
    fn a_nested_fault_emits_one_terse_kernel_fault_record() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();
        PANICKING.store(true, Ordering::Release);

        let arch = TestArch::with_cpus(1);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            fault_dump(
                KernelFault {
                    syndrome: 1,
                    address: 2,
                    pc: 3,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, AuditEvent::KernelFault.id());
        assert!(
            events[0].message.contains("nested"),
            "nested fault must be terse: {}",
            events[0].message
        );
        assert_eq!(events[0].fields.len(), 1, "cpu only");
        assert_eq!(arch.halt_count(), 1);
        reset_panic_guard();
    }

    /// The pre-init console line the port's bridge prints when no arch
    /// handle is published yet uses the same field names and hex width the
    /// structured record does.
    #[test]
    fn kernel_fault_displays_as_one_hex_line() {
        use core::fmt::Write as _;
        let mut out = String::new();
        let _ = write!(
            out,
            "{}",
            KernelFault {
                syndrome: 0x9600_0045,
                address: 0xffff_0000_dead_beef,
                pc: 0x8010_1234,
            }
        );
        assert_eq!(
            out,
            "syndrome=0x0000000096000045 fault_addr=0xffff0000deadbeef fault_pc=0x0000000080101234"
        );
    }
}
