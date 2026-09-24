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
    walk, CpuStateCapture, StackReader, Translation, MAX_FRAMES as BACKTRACE_MAX_FRAMES,
    MAX_NAMED_REGS, MAX_TABLE_LEVELS,
};
use tairix_arch_api::fatal::{format_hex_word, KernelFault};
use tairix_arch_api::quiesce_stop_others_best_effort;
use tairix_arch_api::{BootStackGuard, CpuId, KernelStackRegion};
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

/// Production [`StackReader`] over the kernel stack the faulting CPU is
/// running on.
///
/// Holds the region rather than just its bounds, so every read is *derived*
/// from the root whoever vouched for the stack minted — the port for its
/// boot stack, the dispatcher's publication for a kthread stack. A reader
/// that rebuilt a pointer from the walk's address instead would carry no
/// provenance for the bytes it touches, leaving the compiler free to reorder
/// or elide the reads the unwinder depends on, and would be unverifiable by
/// the undefined-behaviour oracle.
struct RawStackReader {
    region: KernelStackRegion,
}

impl StackReader for RawStackReader {
    fn read_word(&self, addr: u64) -> Option<u64> {
        let word = self.region.word_ptr(addr)?;
        // SAFETY: `word_ptr` proved the whole word lies inside the region
        // its own root vouches for and is 8-byte aligned, so the read is of
        // live, mapped kernel stack and cannot fault. Volatile so the
        // compiler cannot elide or reorder it on the panic path.
        Some(unsafe { word.read_volatile() })
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
/// Total field slots: the four base fields, the three that report the
/// stop-the-world outcome, the six that report the active translation regime,
/// the translation descriptors, and the register and frame blocks.
const FIELD_CAP: usize = 13 + MAX_TABLE_LEVELS + REG_CAP + FRAME_CAP;

/// Stack storage the register and backtrace fields are formatted into.
///
/// Declared in [`dump`]'s frame, like [`CauseBufs`], so the formatted
/// strings outlive the assembled field list.
struct CaptureBufs {
    regs: [[u8; 18]; REG_CAP],
    reg_names: [&'static str; REG_CAP],
    frames: [[u8; 18]; FRAME_CAP],
    frame_keys: [[u8; 16]; FRAME_CAP],
    frame_key_lens: [usize; FRAME_CAP],
}

impl CaptureBufs {
    const fn new() -> Self {
        Self {
            regs: [[0; 18]; REG_CAP],
            reg_names: [""; REG_CAP],
            frames: [[0; 18]; FRAME_CAP],
            frame_keys: [[0; 16]; FRAME_CAP],
            frame_key_lens: [0; FRAME_CAP],
        }
    }
}

/// The kernel stack the faulting CPU is running on: the stack of the task
/// currently switched in there, else the port's own boot stack.
///
/// The task publication is consulted first because a fatal fault almost
/// always lands on a kthread stack, which no port can identify; the
/// dispatcher and the pre-scheduler boot path run on the port's stack, which
/// it vouches for itself. Neither source is believed blind — each answers
/// only when the captured `sp` is inside the region it names, so a walk is
/// never pointed at memory nothing vouches for (fail closed).
fn walk_region(bt: &dyn CpuStateCapture, cpu: CpuId, sp: u64) -> Option<KernelStackRegion> {
    crate::kthread::running_stack(cpu, sp).or_else(|| bt.boot_stack())
}

/// Capture the register snapshot and walk the backtrace into `bufs`,
/// returning `(n_regs, n_frames, boot-stack-guard verdict)`.
///
/// Allocation-free, and reads stack memory only through the rooted,
/// bounds-checked [`RawStackReader`], so the walk never faults on a corrupt
/// chain.
fn capture_into(
    bt: &dyn CpuStateCapture,
    cpu: CpuId,
    bufs: &mut CaptureBufs,
) -> (usize, usize, Option<BootStackGuard>) {
    let snap = bt.capture();

    // Judged from the same snapshot the registers are reported from, so the
    // stack pointer the verdict rests on is the one the record shows.
    let guard = bt.boot_stack_guard().map(|region| region.assess(snap.sp));

    // Explicit unwinder-critical registers first, then the named GP
    // registers the port captured.
    let mut n_regs = 0usize;
    let mut push_reg = |name: &'static str, value: u64| {
        if n_regs < REG_CAP {
            let _ = format_hex_word(value, &mut bufs.regs[n_regs]);
            bufs.reg_names[n_regs] = name;
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
    // reads only within the region whoever vouched for the stack rooted, and
    // is depth-capped, so a corrupt chain terminates without faulting.
    let mut frame_addrs = [0u64; FRAME_CAP];
    let mut n_frames = 0usize;
    if snap.pc != 0 {
        frame_addrs[0] = snap.pc;
        n_frames = 1;
    }
    if let (Some(layout), Some(region)) = (bt.frame_layout(), walk_region(bt, cpu, snap.sp)) {
        let reader = RawStackReader { region };
        walk(&reader, snap.fp, layout, region.into(), |ra| {
            if n_frames < FRAME_CAP {
                frame_addrs[n_frames] = ra;
                n_frames += 1;
            }
        });
    }

    // Format each frame address as hex and its `frame_N` key.
    for (i, addr) in frame_addrs.iter().take(n_frames).enumerate() {
        let _ = format_hex_word(*addr, &mut bufs.frames[i]);
        bufs.frame_key_lens[i] = format_frame_key(i, &mut bufs.frame_keys[i]);
    }

    (n_regs, n_frames, guard)
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
                value: FieldValue::Str(format_hex_word(fault.syndrome, &mut bufs.syndrome)),
            },
            Field {
                key: "fault_addr",
                value: FieldValue::Str(format_hex_word(fault.address, &mut bufs.address)),
            },
            Field {
                key: "fault_pc",
                value: FieldValue::Str(format_hex_word(fault.pc, &mut bufs.pc)),
            },
        ],
    }
}

/// Take the display surface back from a graphical session that holds a seat.
///
/// Done once, ahead of the re-entrancy guard, so a nested terse record is
/// visible too. Deliberately *not* repeated after the world is stopped: a
/// repaint can fault (a scan-out the active root does not map), and nothing
/// that can fault may sit between capturing this core's state and writing the
/// record — losing the screen copy of a report is a far smaller failure than
/// losing the report.
fn reclaim_surfaces(consoles: &[crate::console::ConsoleDevice]) {
    for device in consoles {
        device.reclaim_surface();
    }
}

/// Regime fields a fault report can carry: the active root, the re-probe
/// verdict and its raw result, the hole extent, and the post-flush verdict
/// and its raw result.
const REGIME_FIELDS: usize = 6;

/// Stack storage for the active-translation-regime fields.
struct RegimeBufs {
    root: [u8; 18],
    detail: [u8; 18],
    flushed: [u8; 18],
}

impl RegimeBufs {
    const fn new() -> Self {
        Self {
            root: [0; 18],
            detail: [0; 18],
            flushed: [0; 18],
        }
    }
}

/// Coarsest granule containing an unmapped address that still translates.
///
/// A translation fault says an address is absent; it does not say *how much*
/// is absent, and that is the difference between two unrelated defects — a
/// leaf someone unmapped versus a region never mapped at all. Probing the
/// containing 2 MiB and 1 GiB bases separates them without dereferencing
/// anything.
const HOLE_GRANULES: [(u64, &str); 2] = [(1 << 21, "block"), (1 << 30, "gigapage")];

/// Describe how much around `addr` is unmapped, by probing its containing
/// granules with the port's non-faulting probe.
///
/// `"page"` when the enclosing 2 MiB block translates (so only finer leaves
/// are absent), `"block"` when the gigapage translates but that block does
/// not, `"gigapage"` when neither does, and `"unsupported"` on a port with no
/// probe.
fn hole_extent(bt: &dyn CpuStateCapture, addr: u64) -> &'static str {
    let mut absent = "page";
    for (size, name) in HOLE_GRANULES {
        match bt.translation(addr & !(size - 1), true) {
            Translation::Mapped(_) => return absent,
            Translation::Unmapped { .. } => absent = name,
            Translation::Unsupported => return "unsupported",
        }
    }
    absent
}

/// Describe the translation regime the report is running under: which root
/// is active, and — for a fault — how its faulting address translates *now*.
///
/// Whether an address translates is a property of the active root, not of the
/// machine, so a fault report that names only the address cannot say which
/// address space refused it. The re-probe additionally separates a
/// persistently unmapped address from one that translates again by the time
/// the report runs, which says the mapping changed under the faulting access.
fn regime_fields<'b>(
    fatal: &Fatal<'_>,
    bt: Option<&dyn CpuStateCapture>,
    bufs: &'b mut RegimeBufs,
    want_descs: &mut bool,
) -> ([Field<'b>; REGIME_FIELDS], usize) {
    let mut fields = [Field {
        key: "",
        value: FieldValue::Str(""),
    }; REGIME_FIELDS];
    let Some(bt) = bt else {
        return (fields, 0);
    };
    let mut n = 0usize;
    if let Some(root) = bt.active_root() {
        fields[n] = Field {
            key: "root",
            value: FieldValue::Str(format_hex_word(root, &mut bufs.root)),
        };
        n += 1;
    }
    if let Fatal::Fault(fault) = fatal {
        // A data abort's syndrome says whether it was a write; re-probing the
        // same access kind is what makes the answer comparable to the fault.
        let (verdict, detail) = match bt.translation(fault.address, true) {
            Translation::Unsupported => ("unsupported", None),
            Translation::Mapped(phys) => ("yes", Some(phys)),
            Translation::Unmapped { status } => ("no", Some(status)),
        };
        fields[n] = Field {
            key: "fault_maps",
            value: FieldValue::Str(verdict),
        };
        n += 1;
        if let Some(detail) = detail {
            fields[n] = Field {
                key: "fault_par",
                value: FieldValue::Str(format_hex_word(detail, &mut bufs.detail)),
            };
            n += 1;
        }
        if verdict == "no" {
            fields[n] = Field {
                key: "fault_hole",
                value: FieldValue::Str(hole_extent(bt, fault.address)),
            };
            n += 1;
            // An address the tables map but the TLB refuses is a maintenance
            // defect, and it is indistinguishable from a clobbered table
            // until the cached translations are discarded and the address
            // re-probed. `yes` here means the tables were right all along.
            match bt.translation_after_tlb_flush(fault.address, true) {
                Translation::Unsupported => {}
                Translation::Mapped(phys) => {
                    fields[n] = Field {
                        key: "maps_after_tlbi",
                        value: FieldValue::Str("yes"),
                    };
                    n += 1;
                    fields[n] = Field {
                        key: "par_after_tlbi",
                        value: FieldValue::Str(format_hex_word(phys, &mut bufs.flushed)),
                    };
                    n += 1;
                }
                Translation::Unmapped { status } => {
                    fields[n] = Field {
                        key: "maps_after_tlbi",
                        value: FieldValue::Str("no"),
                    };
                    n += 1;
                    fields[n] = Field {
                        key: "par_after_tlbi",
                        value: FieldValue::Str(format_hex_word(status, &mut bufs.flushed)),
                    };
                    n += 1;
                }
            }
        }
        *want_descs = verdict == "no";
    }
    (fields, n)
}

/// Stack storage for the translation-descriptor fields.
struct DescBufs {
    values: [[u8; 18]; MAX_TABLE_LEVELS],
    keys: [[u8; 16]; MAX_TABLE_LEVELS],
    key_lens: [usize; MAX_TABLE_LEVELS],
}

impl DescBufs {
    const fn new() -> Self {
        Self {
            values: [[0; 18]; MAX_TABLE_LEVELS],
            keys: [[0; 16]; MAX_TABLE_LEVELS],
            key_lens: [0; MAX_TABLE_LEVELS],
        }
    }
}

/// Format the active regime's raw translation descriptors for `addr`,
/// root-downward, as `desc_0..`.
///
/// Only worth emitting for an address that did not translate, where they say
/// whether the hierarchy is intact with an absent entry or the table page
/// itself is arbitrary data — a page-table use-after-free.
fn desc_fields<'b>(
    bt: &dyn CpuStateCapture,
    addr: u64,
    bufs: &'b mut DescBufs,
) -> ([Field<'b>; MAX_TABLE_LEVELS], usize) {
    let mut descs = [0u64; MAX_TABLE_LEVELS];
    let read = bt.table_path(addr, &mut descs).min(MAX_TABLE_LEVELS);
    for (i, desc) in descs.iter().enumerate().take(read) {
        let _ = format_hex_word(*desc, &mut bufs.values[i]);
        bufs.key_lens[i] = format_desc_key(i, &mut bufs.keys[i]);
    }
    let mut fields = [Field {
        key: "",
        value: FieldValue::Str(""),
    }; MAX_TABLE_LEVELS];
    for (i, field) in fields.iter_mut().enumerate().take(read) {
        *field = Field {
            key: core::str::from_utf8(&bufs.keys[i][..bufs.key_lens[i]]).unwrap_or("desc_?"),
            value: FieldValue::Str(core::str::from_utf8(&bufs.values[i]).unwrap_or("0x?")),
        };
    }
    (fields, read)
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

/// The record's field list, accumulated in a stack array.
///
/// Bounded by [`FIELD_CAP`] by construction: a `push` past the end is
/// dropped rather than overflowing, so a port that grows its register set
/// beyond the cap loses a field instead of corrupting the panic frame.
struct Fields<'b> {
    slots: [Field<'b>; FIELD_CAP],
    len: usize,
}

impl<'b> Fields<'b> {
    fn new() -> Self {
        Self {
            slots: [Field {
                key: "",
                value: FieldValue::Str(""),
            }; FIELD_CAP],
            len: 0,
        }
    }

    fn push(&mut self, key: &'b str, value: &'b str) {
        if self.len < FIELD_CAP {
            self.slots[self.len] = Field {
                key,
                value: FieldValue::Str(value),
            };
            self.len += 1;
        }
    }

    fn extend(&mut self, fields: &[Field<'b>]) {
        for field in fields {
            if self.len < FIELD_CAP {
                self.slots[self.len] = *field;
                self.len += 1;
            }
        }
    }

    fn as_slice(&self) -> &[Field<'b>] {
        &self.slots[..self.len]
    }
}

/// The one fatal-report body: reclaim the display, guard against
/// re-entry, emit a single audit record describing `fatal` with a register
/// snapshot and a bounded backtrace, then halt.
fn dump<A: KernelArch>(fatal: &Fatal<'_>, ctx: &PanicContext<'_, A>) -> ! {
    let cpu = ctx.arch.current_cpu();

    // A graphical session holding a seat owns the scan-out, and the text
    // console hands it over rather than drawing on the composited frame — but
    // a panic must never be invisible, so the report takes the screen back
    // whatever was on it. Done ahead of the re-entrancy guard so a nested
    // panic's terse record is visible too.
    reclaim_surfaces(ctx.consoles);

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
    let mut regime_bufs = RegimeBufs::new();

    let cpu_str = format_u32(cpu, &mut cpu_buf);
    let cause = cause_fields(fatal, &mut cause_bufs);

    // Stop the world *before* anything about the machine is read: a kernel
    // invariant is already broken and this core cannot resume, so peers left
    // running either deadlock on a guard it abandoned or proceed over
    // half-updated state. It also has to precede the translation readings
    // below, which interrogate memory a peer can still be editing — a probe
    // and a table walk taken either side of a concurrent page-table update
    // describe two different machines and contradict each other, which is
    // worse than no reading at all. Best effort: it names an unresponsive
    // peer rather than waiting behind one, so a reader compares
    // `peers_stopped` against `peers_asked` to know how much of the machine
    // the readings actually held still.
    let stop = quiesce_stop_others_best_effort(cpu, |peer| {
        crate::sched::SchedulerArch::send_ipi(ctx.arch, peer);
    });
    let mut asked_buf = [0u8; 11];
    let mut stopped_buf = [0u8; 11];
    let mut unresponsive_buf = [0u8; 11];
    let asked_str = format_u32(stop.asked, &mut asked_buf);
    let stopped_str = format_u32(stop.stopped, &mut stopped_buf);
    let unresponsive_str = stop
        .unresponsive
        .map(|peer| format_u32(peer, &mut unresponsive_buf));

    let mut want_descs = false;
    let (regime, n_regime) = regime_fields(fatal, ctx.backtrace, &mut regime_bufs, &mut want_descs);
    // The raw descriptors only inform an address that failed to translate.
    let mut desc_bufs = DescBufs::new();
    let (descs, n_descs) = match (want_descs, ctx.backtrace, fatal) {
        (true, Some(bt), Fatal::Fault(fault)) => desc_fields(bt, fault.address, &mut desc_bufs),
        _ => (
            [Field {
                key: "",
                value: FieldValue::Str(""),
            }; MAX_TABLE_LEVELS],
            0,
        ),
    };

    // A port that published no post-mortem handle gets the base record
    // rather than a faked backtrace.
    let mut capture = CaptureBufs::new();
    let (n_regs, n_frames, guard) = match ctx.backtrace {
        Some(bt) => capture_into(bt, cpu, &mut capture),
        None => (0, 0, None),
    };
    let mut overrun_buf = [0u8; 18];
    let overrun_str = match guard {
        Some(BootStackGuard::BelowStack { bytes, .. }) => {
            Some(format_hex_word(bytes, &mut overrun_buf))
        }
        _ => None,
    };

    // Assemble the one record. Every buffer the fields borrow is a local
    // declared above, so all of them outlive the list.
    let mut fields = Fields::new();
    fields.push("cpu", cpu_str);
    fields.extend(&cause);
    fields.extend(&regime[..n_regime]);
    fields.extend(&descs[..n_descs]);
    fields.push("peers_asked", asked_str);
    fields.push("peers_stopped", stopped_str);
    if let Some(peer) = unresponsive_str {
        fields.push("peer_unresponsive", peer);
    }
    // The boot stack's guard, where the port reserves one. Without it a
    // fault whose real cause was an overrun reads as an unexplained
    // corruption of whatever happened to sit below the stack.
    if let Some(verdict) = guard {
        fields.push("boot_stack_guard", verdict.label());
    }
    if let Some(bytes) = overrun_str {
        fields.push("boot_stack_overrun_bytes", bytes);
    }
    for i in 0..n_regs {
        fields.push(
            capture.reg_names[i],
            core::str::from_utf8(&capture.regs[i]).unwrap_or("0x?"),
        );
    }
    for i in 0..n_frames {
        let key = &capture.frame_keys[i][..capture.frame_key_lens[i]];
        fields.push(
            core::str::from_utf8(key).unwrap_or("frame_?"),
            core::str::from_utf8(&capture.frames[i]).unwrap_or("0x?"),
        );
    }

    let event = fatal.event();
    log(
        ctx.audit_sink,
        &Event {
            level: Level::Error,
            id: event.id(),
            message: event.message(),
            fields: fields.as_slice(),
        },
    );

    // Wait for the record to reach the device. On a port whose console is a
    // buffered ring this is the difference between a report and a silent
    // machine: the stop above left no dispatch loop to pump the queue, no
    // transmit interrupt will be serviced, and the halt below never returns.
    ctx.arch.flush_console_blocking();

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

/// Format an indexed field key `<prefix><index>` into `buf` and return its
/// byte length.
///
/// Allocation-free. `buf` must hold the prefix plus the decimal index; both
/// callers' indices are capped well below three digits, and a prefix that
/// would not fit is truncated rather than panicking (the report never
/// panics).
fn format_indexed_key(prefix: &[u8], index: usize, buf: &mut [u8; 16]) -> usize {
    let head = prefix.len().min(buf.len());
    buf[..head].copy_from_slice(&prefix[..head]);
    let mut num_buf = [0u8; 11];
    let num = format_u32(u32::try_from(index).unwrap_or(u32::MAX), &mut num_buf);
    let num_bytes = num.as_bytes();
    let end = (head + num_bytes.len()).min(buf.len());
    buf[head..end].copy_from_slice(&num_bytes[..end - head]);
    end
}

/// The backtrace field key `frame_<index>`.
fn format_frame_key(index: usize, buf: &mut [u8; 16]) -> usize {
    format_indexed_key(b"frame_", index, buf)
}

/// The translation-descriptor field key `desc_<index>`.
fn format_desc_key(index: usize, buf: &mut [u8; 16]) -> usize {
    format_indexed_key(b"desc_", index, buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr::NonNull;
    use tairix_arch_api::BootStackGuardRegion;

    /// The process-wide quiesce liveness tables the stop-request tests
    /// publish. Set-once per process, so one shared pair — not a per-test
    /// allocation, which the undefined-behaviour oracle cannot tell from a
    /// real leak.
    static QUIESCE_ONLINE: [AtomicBool; 1] = [AtomicBool::new(false)];
    static QUIESCE_ACK: [AtomicBool; 1] = [AtomicBool::new(false)];
    use crate::test_arch::{TestArch, HALT_SENTINEL};
    use crate::test_sink::TestSink;
    use alloc::string::String;
    use core::panic::Location;
    use core::sync::atomic::AtomicU64;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use tairix_arch_api::backtrace::{
        Backtrace, BacktraceProfile, CpuStateCapture, FrameLayout, RegisterSnapshot,
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

    fn drive_panic_dump<F>(
        make_location: F,
    ) -> (TestArch, alloc::vec::Vec<crate::test_sink::CapturedEvent>)
    where
        F: FnOnce() -> &'static Location<'static>,
    {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();
        let arch = TestArch::with_cpus(2);
        arch.set_current_cpu(1);
        let sink = &TestSink::new();
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
        let records = sink.snapshot();
        (arch, records)
    }

    #[track_caller]
    fn caller_location() -> &'static Location<'static> {
        Location::caller()
    }

    #[test]
    fn panic_dump_emits_one_record_with_documented_fields() {
        let (arch, events) = drive_panic_dump(caller_location);

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
        let sink = &TestSink::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &sink.snapshot()[0];
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
    fn format_frame_key_numbers_frames() {
        let mut buf = [0u8; 16];
        let n = format_frame_key(0, &mut buf);
        assert_eq!(&buf[..n], b"frame_0");
        let mut buf = [0u8; 16];
        let n = format_frame_key(63, &mut buf);
        assert_eq!(&buf[..n], b"frame_63");
    }

    /// A host [`CpuStateCapture`] that points the walker at a real stack
    /// image the test owns, so `panic_dump`'s production `RawStackReader`
    /// derives its reads from a root over live host memory.
    ///
    /// The region is built from the backing store's own pointer rather than
    /// from its address: an address round-tripped through an integer and
    /// synthesised back reads the same bytes natively while telling the test
    /// nothing about whether the reader stayed inside the region it was
    /// given, and is refused outright by the undefined-behaviour oracle.
    struct HostCapture {
        pc: u64,
        sp: u64,
        fp: u64,
        /// What the port vouches for, so a test can also make it honestly
        /// decline and prove the walk came from elsewhere.
        boot_stack: Option<KernelStackRegion>,
    }

    /// Root a region in a word slice the caller owns and keeps alive.
    ///
    /// The slice must not be touched through its own handle afterwards:
    /// reborrowing it would retire the region's root, so the image is
    /// planted with [`plant`] — through the very pointer the reader derives
    /// from, which is the discipline under test.
    fn region_of(words: &mut [u64]) -> KernelStackRegion {
        let len = core::mem::size_of_val(words);
        let base = NonNull::from(words).cast::<u8>();
        // SAFETY: `words` is a live, writable host allocation of exactly
        // `len` bytes, and the caller holds it for as long as the region is
        // used.
        unsafe { KernelStackRegion::new(base, len) }
    }

    /// Write one word of a fixture stack through the region's own root.
    fn plant(region: KernelStackRegion, addr: u64, value: u64) {
        let word = region.word_ptr(addr).expect("word inside the fixture");
        // SAFETY: `word_ptr` proved the word lies inside the region, whose
        // backing store the caller holds live.
        unsafe { word.write_volatile(value) };
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
        fn boot_stack(&self) -> Option<KernelStackRegion> {
            self.boot_stack
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
        let region = region_of(&mut stack);
        let base = region.base_addr();
        let fp0 = base;
        let fp1 = base + 16;
        plant(region, base, fp1); // caller fp of frame 0
        plant(region, base + 8, RET1); // return address of frame 0
        plant(region, base + 16, 0); // caller fp of frame 1 (terminates)
        plant(region, base + 24, RET2); // return address of frame 1
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: fp0,
            boot_stack: Some(region),
        };

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let cap_ref: &dyn CpuStateCapture = &cap;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(cap_ref);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1, "one panic record");
        let ev = &sink.snapshot()[0];
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

    /// A panic on a kthread stack is unwound from the stack the dispatcher
    /// published, not from the boot stack the port vouches for.
    ///
    /// This is the case that matters: almost every fatal fault after boot
    /// lands on a kthread stack, which no port can identify, so a walk that
    /// consulted only the port would emit registers and no chain at all
    /// exactly when a chain is most needed. The port here honestly reports
    /// no boot stack — the CPU is not on it — so any frame beyond `frame_0`
    /// can only have come from the publication.
    #[test]
    fn panic_dump_unwinds_a_kthread_stack_the_dispatcher_published() {
        const RET1: u64 = 0xffff_8000_0000_3333;
        const CPU: CpuId = 0;
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let mut stack: alloc::vec::Vec<u64> = alloc::vec![0u64; 2];
        let region = region_of(&mut stack);
        let base = region.base_addr();
        plant(region, base, 0); // caller fp terminates the walk
        plant(region, base + 8, RET1);

        // The port declines: this CPU is not on its boot stack.
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: base,
            boot_stack: None,
        };
        let _published = crate::kthread::publish_running_stack_for_test(CPU, region);

        let arch = TestArch::with_cpus(1);
        arch.set_current_cpu(CPU);
        let sink = &TestSink::new();
        let cap_ref: &dyn CpuStateCapture = &cap;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(cap_ref);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("frame_0"), Some("0xffff800000000000"));
        assert_eq!(
            field("frame_1"),
            Some("0xffff800000003333"),
            "the published kthread stack was walked"
        );
    }

    /// With nothing published and the port declining, the report carries
    /// registers and no chain rather than a walk over memory nothing
    /// vouches for.
    #[test]
    fn panic_dump_emits_no_chain_when_no_stack_is_vouched_for() {
        const CPU: CpuId = 1;
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let mut stack: alloc::vec::Vec<u64> = alloc::vec![0u64; 2];
        let region = region_of(&mut stack);
        let base = region.base_addr();
        plant(region, base, 0);
        plant(region, base + 8, 0xffff_8000_0000_4444);
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: base,
            boot_stack: None,
        };

        let arch = TestArch::with_cpus(2);
        arch.set_current_cpu(CPU);
        let sink = &TestSink::new();
        let cap_ref: &dyn CpuStateCapture = &cap;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(cap_ref);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("pc"), Some("0xffff800000000000"));
        assert_eq!(field("frame_0"), Some("0xffff800000000000"));
        assert_eq!(field("frame_1"), None, "no region, no walk");
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

        impl Surface {
            const fn new() -> Self {
                Self {
                    reclaimed: AtomicBool::new(false),
                    reported: AtomicBool::new(false),
                    reclaimed_before_report: AtomicBool::new(false),
                }
            }

            /// Start a fresh round. An installed console is process-lifetime
            /// (`ConsoleDevice::new` takes `&'static`), so the fixture is a
            /// static the oracle can account for, cleared per round rather
            /// than reallocated.
            fn reset(&self) {
                self.reclaimed.store(false, Ordering::SeqCst);
                self.reported.store(false, Ordering::SeqCst);
                self.reclaimed_before_report.store(false, Ordering::SeqCst);
            }
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

        static SURFACE: Surface = Surface::new();
        static CONSOLE: SurfaceConsole = SurfaceConsole(&SURFACE);
        static CONSOLES: [crate::console::ConsoleDevice; 1] = [crate::console::ConsoleDevice::new(
            &CONSOLE,
            &crate::console::NULL_CONSOLE_READ,
        )];

        for nested in [false, true] {
            let _serial = TEST_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_panic_guard();
            PANICKING.store(nested, Ordering::Release);

            let surface = &SURFACE;
            surface.reset();
            let sink = &SurfaceSink(surface);
            let arch = TestArch::with_cpus(1);
            let result = catch_unwind(AssertUnwindSafe(|| {
                let ctx = PanicContext::new(&arch, sink).with_consoles(&CONSOLES);
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
        let sink = &TestSink::new();
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
        let sink = &TestSink::new();
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
        let ev = &sink.snapshot()[0];
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
        let region = region_of(&mut stack);
        let base = region.base_addr();
        plant(region, base, 0); // caller fp terminates the walk
        plant(region, base + 8, RET1);
        let cap = HostCapture {
            pc: 0xffff_8000_0000_0000,
            sp: base,
            fp: base,
            boot_stack: Some(region),
        };

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
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
        let ev = &sink.snapshot()[0];
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
        let sink = &TestSink::new();
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

    /// The world is stopped *before* the record is written, and the record
    /// says what the stop achieved.
    ///
    /// Ordering matters twice over: peers left running while the report is
    /// assembled can deadlock on a guard the dying core abandoned, and a peer
    /// still writing contends the console queue the report needs. The sink
    /// observes the latched stop request to prove the stop came first.
    ///
    /// One published table serves the whole process (the slot is set-once) and
    /// marks *no* CPU online, so nothing is ever poked and no wait ever spins.
    /// That is what keeps this test order-independent: every panic-driving
    /// test in this process reports `peers_asked=0` whether it ran before or
    /// after this one, and none of them is charged another's spin budget.
    #[test]
    fn the_world_is_stopped_before_the_record_is_written() {
        struct OrderSink {
            stopped_before_report: AtomicBool,
            reported: AtomicBool,
        }

        impl Sink for OrderSink {
            fn write_event(&self, _event: &Event<'_>) {
                // Asked as a *peer* (cpu 1), never as the reporter: the
                // reporter is deliberately exempt from its own request.
                self.stopped_before_report
                    .store(tairix_arch_api::quiesce_stop_requested(1), Ordering::SeqCst);
                self.reported.store(true, Ordering::SeqCst);
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        // A one-entry liveness table with nothing online: publishing it is what
        // lets the stop latch its request at all, while leaving no peer to
        // poke or wait for. Set-once per process, so the pair is a static the
        // whole crate's tests share rather than a per-test allocation.
        let _ = tairix_arch_api::quiesce_publish_tables(&QUIESCE_ONLINE, &QUIESCE_ACK);

        let arch = TestArch::with_cpus(1);
        let sink = &OrderSink {
            stopped_before_report: AtomicBool::new(false),
            reported: AtomicBool::new(false),
        };
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

        assert!(
            sink.reported.load(Ordering::SeqCst),
            "the record was written"
        );
        assert!(
            sink.stopped_before_report.load(Ordering::SeqCst),
            "the stop request must be latched before the record is written"
        );
        // No CPU is online, so nobody was poked and nothing was waited on.
        assert_eq!(arch.ipi_count(), 0);
        reset_panic_guard();
    }

    /// The translation readings are taken *after* the stop, so they describe
    /// one machine rather than two.
    ///
    /// `fault_par` and `desc_0..` interrogate memory a running peer can still
    /// be editing. A probe and a table walk taken either side of a concurrent
    /// page-table update disagree, and the disagreement reads as a defect in
    /// the tables rather than in the reading — a Pi 4 capture whose probe said
    /// "absent" and whose walk then produced a valid descriptor for the same
    /// address cost a diagnosis this way.
    ///
    /// This reporter is cpu 2 — no other test in this process reports as
    /// anything but cpu 0 — and `quiesce_stop_requested(2)` is false only once
    /// *this* stop has latched itself as the requester. That keeps the
    /// assertion honest whatever order the process ran its panics in: the
    /// stop request itself is a set-once global, so merely observing it
    /// latched would pass vacuously after any earlier panic test.
    #[test]
    fn the_translation_readings_are_taken_after_the_stop() {
        /// Records, at each reading, whether this reporter had already
        /// latched the stop.
        struct WhenRead {
            probe_after_stop: AtomicBool,
            walk_after_stop: AtomicBool,
        }

        /// The reporter's cpu, unique to this test among the process's panics.
        const REPORTER: u32 = 2;

        /// Any other cpu, used to read the request itself apart from who owns
        /// it.
        const PEER: u32 = 1;

        /// `true` once [`REPORTER`] is the CPU holding the stop request.
        ///
        /// The request is a set-once global, so "is it latched" alone would
        /// pass vacuously after any earlier panic test in this process, and
        /// "should REPORTER stop" alone passes vacuously before any. Read
        /// together they name the owner: latched, and exempting REPORTER.
        fn reporter_holds_the_stop() -> bool {
            tairix_arch_api::quiesce_stop_requested(PEER)
                && !tairix_arch_api::quiesce_stop_requested(REPORTER)
        }

        impl CpuStateCapture for WhenRead {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn active_root(&self) -> Option<u64> {
                Some(0x0454_0000)
            }
            fn translation(&self, _addr: u64, _write: bool) -> Translation {
                self.probe_after_stop
                    .store(reporter_holds_the_stop(), Ordering::SeqCst);
                Translation::Unmapped { status: 0x080d }
            }
            fn table_path(&self, _addr: u64, out: &mut [u64; MAX_TABLE_LEVELS]) -> usize {
                self.walk_after_stop
                    .store(reporter_holds_the_stop(), Ordering::SeqCst);
                out[0] = 0x0454_1003;
                1
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        // Publishing is what lets the stop latch a requester at all. The slot
        // is set-once per process, so this is idempotent with the other
        // publisher here and leaves no peer online to poke or wait for.
        let _ = tairix_arch_api::quiesce_publish_tables(&QUIESCE_ONLINE, &QUIESCE_ACK);

        let arch = TestArch::with_cpus(4);
        arch.set_current_cpu(REPORTER);
        let sink = &TestSink::new();
        let read = &WhenRead {
            probe_after_stop: AtomicBool::new(false),
            walk_after_stop: AtomicBool::new(false),
        };
        let handle: &dyn CpuStateCapture = read;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(handle);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0046,
                    address: 0x3e40_2000,
                    pc: 0x002e_0db8,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        // Both readings ran, and both saw this reporter already holding the
        // stop request.
        let ev = &sink.snapshot()[0];
        assert!(
            ev.fields.iter().any(|(k, _)| k == "fault_par"),
            "the probe reading reached the record"
        );
        assert!(
            ev.fields.iter().any(|(k, _)| k == "desc_0"),
            "the descriptor walk reached the record"
        );
        assert!(
            read.probe_after_stop.load(Ordering::SeqCst),
            "the faulting-address probe must run after the stop"
        );
        assert!(
            read.walk_after_stop.load(Ordering::SeqCst),
            "the descriptor walk must run after the stop"
        );
        reset_panic_guard();
    }

    /// A port that reserves a boot-stack guard has its verdict carried into
    /// the record, so a fault whose real cause was an overrun says so
    /// instead of reading as an unexplained corruption below the stack.
    #[test]
    fn a_report_carries_the_boot_stack_guard_verdict() {
        /// A capture handle whose guard is a byte array this test owns.
        ///
        /// `sp` is reported as the port captured it, so the fixture can put
        /// the stack pointer above the guard (judged on the canary) or
        /// below it (an overrun the canary cannot see).
        struct Guarded {
            guard: BootStackGuardRegion,
            sp: u64,
        }

        impl CpuStateCapture for Guarded {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, self.sp, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn boot_stack_guard(&self) -> Option<BootStackGuardRegion> {
                Some(self.guard)
            }
        }

        /// Drive one report over a guard the caller has poisoned (or not)
        /// and a chosen stack pointer, returning its guard fields.
        fn verdict_of(
            bytes: &mut [u8],
            sp_below_bottom: u64,
        ) -> (Option<std::string::String>, Option<std::string::String>) {
            let len = bytes.len();
            let base = NonNull::from(&mut *bytes).cast::<u8>();
            // SAFETY: `bytes` is a live host allocation of exactly `len`
            // bytes that the caller holds for as long as the region is used.
            let guard = unsafe { BootStackGuardRegion::from_root(base, len) };
            let bottom = guard.stack_bottom_addr();
            let handle: &dyn CpuStateCapture = &Guarded {
                guard,
                sp: bottom - sp_below_bottom,
            };

            let _serial = TEST_SERIAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reset_panic_guard();
            let arch = TestArch::with_cpus(1);
            let sink = &TestSink::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let ctx = PanicContext::new(&arch, sink).with_backtrace(handle);
                panic_dump(None, &ctx);
            }));
            assert!(result.is_err());
            let ev = &sink.snapshot()[0];
            let field = |key: &str| {
                ev.fields
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
            };
            let out = (field("boot_stack_guard"), field("boot_stack_overrun_bytes"));
            reset_panic_guard();
            out
        }

        let mut guard = [tairix_memguard::GUARD_BYTE; tairix_memguard::CANARY_BYTES * 2];

        // Poisoned, stack pointer above the guard: nothing to report but
        // that the stack stayed inside itself.
        assert_eq!(
            verdict_of(&mut guard, 0),
            (Some("intact".into()), None),
            "an untouched guard must read as intact"
        );

        // The stack pointer alone is decisive, and carries the extent —
        // this is the overrun a frame larger than the guard leaves behind
        // without disturbing a byte of it.
        assert_eq!(
            verdict_of(&mut guard, 0x20),
            (
                Some("sp_below_stack".into()),
                Some("0x0000000000000020".into())
            ),
            "a stack pointer below the stack must be reported with its extent"
        );

        // A write through the canary, with the stack pointer recovered
        // above the stack's bottom: only the poison still says so.
        guard[tairix_memguard::CANARY_BYTES * 2 - 1] = 0;
        assert_eq!(
            verdict_of(&mut guard, 0),
            (Some("disturbed".into()), None),
            "a disturbed canary must reach the record"
        );
    }

    /// With no peers to stop, the record still states so rather than leaving
    /// a reader guessing whether the machine was stopped.
    #[test]
    fn a_report_states_the_stop_outcome_even_with_no_peers() {
        let (_arch, events) = drive_panic_dump(caller_location);
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("peers_asked"), Some("0"));
        assert_eq!(field("peers_stopped"), Some("0"));
        // Nothing was unresponsive, so the field is absent rather than "none".
        assert_eq!(field("peer_unresponsive"), None);
    }

    /// A fault report names the active translation root and re-probes the
    /// faulting address, so a reader learns *which* address space refused it.
    #[test]
    fn a_fault_report_names_the_active_regime() {
        /// A capture handle that reports a root and an unmapped probe, as a
        /// port with a non-faulting probe does.
        struct Probing;

        impl CpuStateCapture for Probing {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn active_root(&self) -> Option<u64> {
                Some(0x0000_0000_0004_1000)
            }
            fn translation(&self, _addr: u64, _write: bool) -> Translation {
                Translation::Unmapped { status: 0x9 }
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let probe: &dyn CpuStateCapture = &Probing;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0046,
                    address: 0x3e40_2000,
                    pc: 0x002e_05b8,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let events = sink.snapshot();
        let ev = &events[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("root"), Some("0x0000000000041000"));
        assert_eq!(field("fault_maps"), Some("no"));
        assert_eq!(field("fault_par"), Some("0x0000000000000009"));
    }

    /// An address the tables map but the TLB refuses is reported as such,
    /// because it is otherwise indistinguishable from a clobbered table.
    ///
    /// A Pi 4 fault at a *varying* low-RAM address whose descriptors read back
    /// as a valid block is the case this separates: with a coherent walker and
    /// correct tables, a refused translation is a maintenance defect, and
    /// `maps_after_tlbi=yes` is what says so.
    #[test]
    fn a_fault_the_tlb_refuses_but_the_tables_map_is_named() {
        /// Refuses the plain probe and answers the post-flush one, as a port
        /// whose TLB held a stale entry does.
        struct StaleTlb;

        impl CpuStateCapture for StaleTlb {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn active_root(&self) -> Option<u64> {
                Some(0x0454_0000)
            }
            fn translation(&self, addr: u64, _write: bool) -> Translation {
                // The gigapage base translates and the faulting 2 MiB block
                // does not, which is the reported `fault_hole=block`.
                if addr == 0 {
                    Translation::Mapped(0)
                } else {
                    Translation::Unmapped { status: 0x080d }
                }
            }
            fn translation_after_tlb_flush(&self, _addr: u64, _write: bool) -> Translation {
                Translation::Mapped(0x02a7_cb38)
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let probe: &dyn CpuStateCapture = &StaleTlb;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0046,
                    address: 0x02a7_cb38,
                    pc: 0x002c_e0a4,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let ev = &sink.snapshot()[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("fault_maps"), Some("no"));
        assert_eq!(field("fault_hole"), Some("block"));
        assert_eq!(field("maps_after_tlbi"), Some("yes"));
        assert_eq!(field("par_after_tlbi"), Some("0x0000000002a7cb38"));
        reset_panic_guard();
    }

    /// A port with no probe reports no post-flush verdict, rather than one it
    /// cannot support.
    #[test]
    fn a_port_without_a_probe_reports_no_post_flush_verdict() {
        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0046,
                    address: 0x02a7_cb38,
                    pc: 0x002c_e0a4,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let ev = &sink.snapshot()[0];
        assert!(ev.fields.iter().all(|(k, _)| k != "maps_after_tlbi"));
        assert!(ev.fields.iter().all(|(k, _)| k != "par_after_tlbi"));
        reset_panic_guard();
    }

    /// A *panic* has no faulting address, so it names the root and stops —
    /// no probe verdict is fabricated for it.
    #[test]
    fn a_panic_report_names_the_root_but_probes_nothing() {
        struct RootOnly;

        impl CpuStateCapture for RootOnly {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn active_root(&self) -> Option<u64> {
                Some(0x2000)
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let probe: &dyn CpuStateCapture = &RootOnly;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
            panic_dump(None, &ctx);
        }));
        assert!(result.is_err());

        let ev = &sink.snapshot()[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("root"), Some("0x0000000000002000"));
        assert_eq!(field("fault_maps"), None);
        assert_eq!(field("fault_par"), None);
    }

    /// The report drains its own record to the device *after* writing it and
    /// before halting.
    ///
    /// This is the regression guard for a real metal failure: stopping the
    /// world leaves no dispatch loop to pump a buffered console queue and no
    /// transmit interrupt that will ever be serviced, so a report that halts
    /// without waiting for its own bytes truncates mid-record and the machine
    /// looks like it died in silence — the exact failure the report exists to
    /// prevent. The sink samples the flush count as it is written, so the
    /// ordering is asserted, not just the call.
    #[test]
    fn the_record_is_flushed_to_the_device_after_it_is_written() {
        struct FlushOrderSink<'a> {
            arch: &'a TestArch,
            flushes_at_write: AtomicU64,
            reported: AtomicBool,
        }

        impl Sink for FlushOrderSink<'_> {
            fn write_event(&self, _event: &Event<'_>) {
                self.flushes_at_write
                    .store(self.arch.console_flush_count(), Ordering::SeqCst);
                self.reported.store(true, Ordering::SeqCst);
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = &TestArch::with_cpus(1);
        let sink = &FlushOrderSink {
            arch,
            flushes_at_write: AtomicU64::new(u64::MAX),
            reported: AtomicBool::new(false),
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(arch, sink);
            fault_dump(
                KernelFault {
                    syndrome: 1,
                    address: 2,
                    pc: 3,
                },
                &ctx,
            );
        }));
        assert!(result.is_err(), "the report must halt");

        assert!(
            sink.reported.load(Ordering::SeqCst),
            "the record was written"
        );
        assert_eq!(
            sink.flushes_at_write.load(Ordering::SeqCst),
            0,
            "the flush must come after the record, not before it"
        );
        assert!(
            arch.console_flush_count() >= 1,
            "the report must drain its own record before halting"
        );
        assert_eq!(arch.halt_count(), 1);
    }

    /// The hole probe says how much around the faulting address is absent,
    /// which separates "a leaf was unmapped" from "a region was never
    /// mapped" — unrelated defects the syndrome alone cannot tell apart.
    #[test]
    fn the_report_characterises_the_size_of_the_unmapped_hole() {
        /// A probe whose only unmapped range is `[hole, hole + span)`.
        struct Holed {
            hole: u64,
            span: u64,
        }

        impl CpuStateCapture for Holed {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn translation(&self, addr: u64, _write: bool) -> Translation {
                if addr >= self.hole && addr < self.hole + self.span {
                    Translation::Unmapped { status: 0xd }
                } else {
                    Translation::Mapped(addr)
                }
            }
        }

        /// Drive one fault at `addr` against a probe holed at
        /// `[hole, hole + span)` and return the record's `fault_hole`.
        fn hole_field(addr: u64, hole: u64, span: u64) -> alloc::string::String {
            reset_panic_guard();
            let arch = TestArch::with_cpus(1);
            let sink = &TestSink::new();
            let probe: &dyn CpuStateCapture = &Holed { hole, span };
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
                fault_dump(
                    KernelFault {
                        syndrome: 0,
                        address: addr,
                        pc: 0,
                    },
                    &ctx,
                );
            }));
            let ev = &sink.snapshot()[0];
            ev.fields
                .iter()
                .find(|(k, _)| k == "fault_hole")
                .map(|(_, v)| alloc::string::String::from(v.as_str()))
                .unwrap_or_default()
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // One 4 KiB page absent inside a mapped 2 MiB block.
        assert_eq!(hole_field(0x3e40_2000, 0x3e40_2000, 0x1000), "page");
        // The whole 2 MiB block absent, its gigapage otherwise mapped.
        assert_eq!(hole_field(0x3e40_2000, 0x3e40_0000, 1 << 21), "block");
        // The whole gigapage absent.
        assert_eq!(hole_field(0x3e40_2000, 0, 1 << 30), "gigapage");
        reset_panic_guard();
    }

    /// An unmapped fault carries the raw translation descriptors, which say
    /// whether the hierarchy is intact with an absent entry or the table
    /// page itself is arbitrary data — a page-table use-after-free.
    #[test]
    fn an_unmapped_fault_carries_the_raw_translation_descriptors() {
        struct Walked;

        impl CpuStateCapture for Walked {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn translation(&self, _addr: u64, _write: bool) -> Translation {
                Translation::Unmapped { status: 0xd }
            }
            fn table_path(&self, _addr: u64, out: &mut [u64; MAX_TABLE_LEVELS]) -> usize {
                // A valid table descriptor, then an invalid leaf: an intact
                // hierarchy whose entry is genuinely absent.
                out[0] = 0x0000_0000_0454_1003;
                out[1] = 0;
                2
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let probe: &dyn CpuStateCapture = &Walked;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
            fault_dump(
                KernelFault {
                    syndrome: 0x9600_0046,
                    address: 0x3e40_2000,
                    pc: 0,
                },
                &ctx,
            );
        }));
        assert!(result.is_err());

        let ev = &sink.snapshot()[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("desc_0"), Some("0x0000000004541003"));
        assert_eq!(field("desc_1"), Some("0x0000000000000000"));
        // Only what the port read is emitted; nothing is invented.
        assert_eq!(field("desc_2"), None);
        reset_panic_guard();
    }

    /// A *mapped* address carries no descriptors: they exist to explain an
    /// absence, and a port with no walk emits none either.
    #[test]
    fn a_mapped_fault_and_a_walkless_port_carry_no_descriptors() {
        struct Mapped;

        impl CpuStateCapture for Mapped {
            fn profile(&self) -> BacktraceProfile {
                BacktraceProfile {
                    register_capture: Backtrace::Supported,
                    frame_unwind: Backtrace::Unsupported("no chain in this fixture"),
                }
            }
            fn capture(&self) -> RegisterSnapshot {
                RegisterSnapshot::new(0, 0, 0)
            }
            fn frame_layout(&self) -> Option<FrameLayout> {
                None
            }
            fn boot_stack(&self) -> Option<KernelStackRegion> {
                None
            }
            fn translation(&self, addr: u64, _write: bool) -> Translation {
                Translation::Mapped(addr)
            }
            fn table_path(&self, _addr: u64, out: &mut [u64; MAX_TABLE_LEVELS]) -> usize {
                out[0] = 0xdead_beef;
                1
            }
        }

        let _serial = TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_panic_guard();

        let arch = TestArch::with_cpus(1);
        let sink = &TestSink::new();
        let probe: &dyn CpuStateCapture = &Mapped;
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let ctx = PanicContext::new(&arch, sink).with_backtrace(probe);
            fault_dump(
                KernelFault {
                    syndrome: 0,
                    address: 0x1000,
                    pc: 0,
                },
                &ctx,
            );
        }));

        let ev = &sink.snapshot()[0];
        let field = |key: &str| {
            ev.fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(field("fault_maps"), Some("yes"));
        assert_eq!(field("desc_0"), None, "descriptors explain an absence only");
        assert_eq!(field("fault_hole"), None);
        reset_panic_guard();
    }
}
