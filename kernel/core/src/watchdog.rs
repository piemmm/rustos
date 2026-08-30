//! First-class CPU-lockup watchdog: detect, diagnose, and try to recover
//! from both *soft* and *hard* CPU lockups, and make each one loud on the
//! debug console with enough context to explain the *why*.
//!
//! # What it detects
//!
//! * **Soft lockup** — a CPU that keeps taking interrupts but stops
//!   returning to the scheduler for far longer than any bounded operation
//!   should take: a runaway in-kernel loop, a task that never yields, a
//!   lock held across a wedged access. The CPU is alive at the trap level,
//!   so its own timer path can observe it.
//! * **Hard lockup** — a CPU that has stopped taking even the
//!   non-maskable watchdog sample while it is running work: wedged with
//!   maskable interrupts masked, an interrupt storm, or a dead core.
//!   The victim never runs its own tick, so **another** CPU must observe
//!   it over a channel the victim cannot mask (the port's pseudo-NMI: the
//!   aarch64 FIQ). This is the classic hard lockup a soft detector is
//!   structurally blind to.
//!
//! # The two heartbeats and the activity state
//!
//! Every CPU keeps, in its per-CPU `CpuState`:
//!
//! * a **progress** heartbeat ([`note_progress`]), stamped once per
//!   dispatch-loop iteration — "the scheduler ran here"; the soft-lockup
//!   basis;
//! * a **liveness** heartbeat ([`on_watchdog_tick`]), stamped by the
//!   port's non-maskable cadence sample (~[`tairix_arch_api::WATCHDOG_CADENCE_NS`])
//!   — "this CPU is still taking the pseudo-NMI"; the hard-lockup basis;
//! * an **activity** class ([`set_activity`]) — `Active` while it runs the
//!   dispatch loop or a task, `Idle` while parked in `wfi`, `Offline`
//!   before it comes online or after it leaves. Only an `Active` CPU owes
//!   progress, so a legitimately parked or not-yet-online CPU is never
//!   judged (fail closed).
//!
//! # How a lockup is judged
//!
//! * The port's cadence sample calls [`on_watchdog_tick`] on its own CPU:
//!   it stamps liveness, records what it interrupted (PC, task, processor
//!   state) as that CPU's last-known context — the raw "why" — then runs a
//!   **cross-CPU scan** of every *other* CPU:
//!   - liveness frozen past the hard threshold while `Active` → **hard
//!     lockup** (only this path can see it);
//!   - otherwise, progress frozen past the soft threshold while `Active`
//!     **and last seen in the kernel** → **soft lockup** (a CPU wedged in
//!     the kernel even when it is the only runnable task; a lone,
//!     preemptible *user* task owes no progress and is never flagged).
//! * The CPU's own armed timer tick also calls [`check_stall`], the
//!   same-CPU soft check. Both share the per-episode latch, so a lockup is
//!   reported exactly once whichever path sees it first.
//!
//! # Breaking a monopoly
//!
//! Detection alone leaves a wedged machine wedged, so a CPU that has not
//! returned to the dispatch loop within
//! [`DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS`] is *forced* back to it
//! ([`crate::preempt::request_forced_yield`]). Both per-CPU interrupt paths
//! request it — the non-maskable cadence and the maskable timer tick — for
//! the same reason the two detectors exist: whichever channel a given wedge
//! leaves running is the one that has to break it. The tick matters most,
//! because a CPU whose cadence has stopped is precisely the CPU whose
//! cadence-driven guard has stopped with it.
//!
//! # Diagnosis and recovery
//!
//! A detection renders a rich, allocation-free record — the locked CPU,
//! the observer, how long it has been silent, the last-known interrupted
//! PC and processor state, and the running task — then asks the port to
//! break it out best-effort ([`tairix_arch_api::WatchdogArch`]): a
//! reschedule for a soft lockup, a directed attention interrupt for a
//! hard one — as forceful as the port's controller allows, which on
//! GICv2 non-secure is a maskable SGI a wedged core may never take. The
//! recovery attempt and its honest outcome are themselves on the audit
//! trail; a genuinely wedged core is reported `Unrecoverable`, never
//! silently.
//!
//! # Cost and safety
//!
//! The hot-path additions are single relaxed atomic stores (one per
//! dispatch, one per cadence sample), so normal execution is unperturbed.
//! Detection and reporting are lock-free and allocation-free, safe to run
//! from the non-maskable sample path even while a target CPU holds
//! arbitrary locks. Before the report sink / clock / recovery handles are
//! installed, or on a never-armed CPU, the watchdog emits nothing and
//! never panics (fail closed).

use core::sync::atomic::{AtomicBool, Ordering};
// `AtomicU64` backs only the debug-diagnostics kernel-image-base seam, so
// it is imported only when that facility is compiled in.
#[cfg(feature = "watchdog-diagnostics")]
use core::sync::atomic::AtomicU64;

use tairix_arch_api::{
    CpuId, RecoveryOutcome, StuckInterrupt, WatchdogArch, WatchdogKind, WatchdogSample,
};
// The fresh cross-core PC sample is rendered only into the debug-only
// detail record, so its type is imported only with that facility.
#[cfg(feature = "watchdog-diagnostics")]
use tairix_arch_api::RemotePcSample;
// The non-maskable self-sample deliverability verdict is reported only by
// the debug-only diagnostic facility, so its type is imported only with it.
#[cfg(feature = "watchdog-diagnostics")]
use tairix_arch_api::cpufeatures::FeatureSupport;
// The victim-published in-flight interrupt reaches only the debug detail
// record, so its type is imported only with that facility.
#[cfg(feature = "watchdog-diagnostics")]
use tairix_arch_api::InFlightInterrupt;
use tairix_log::{Level, Sink};
use tairix_sync::once::OnceCell;
use tairix_util::fmt::format_hex_u64;

use crate::audit::{emit, AuditEvent};
use crate::cpu_state::{self, CpuState};

/// How long an `Active` CPU may run without any scheduler progress before
/// it is reported **soft**-locked, in nanoseconds (10 seconds).
///
/// A diagnostic policy value, not a resource capacity: no correct bounded
/// kernel operation withholds the CPU from the scheduler for ten seconds,
/// so a gap this large is a genuine soft lockup rather than a
/// long-but-legitimate wait. Deliberately generous so a heavily loaded but
/// healthy machine is never reported.
pub const DEFAULT_SOFT_LOCKUP_THRESHOLD_NS: u64 = 10_000_000_000;

/// How long an `Active` CPU may go without taking its non-maskable
/// watchdog sample before it is reported **hard**-locked, in nanoseconds
/// (10 seconds).
///
/// The cadence sample fires ~once per second, so ten seconds is ~ten
/// missed samples — well beyond any jitter, so a crossing means the CPU
/// genuinely stopped taking the pseudo-NMI rather than merely running
/// late. Diagnostic policy, not a capacity.
pub const DEFAULT_HARD_LOCKUP_THRESHOLD_NS: u64 = 10_000_000_000;

/// How long an `Active` CPU may run without returning to the scheduler
/// before it is forced to yield, in nanoseconds (1 second).
///
/// A lone CPU-bound task has no competitor, so the ordinary
/// competitor-gated preemption tick deliberately leaves it running; without
/// this guard it would withhold the CPU from the dispatch loop
/// indefinitely, stalling per-dispatch housekeeping and the progress
/// heartbeat (a runnable task monopolising a CPU by refusing to yield).
/// That housekeeping includes the deferred-wake drain, so a CPU held out of
/// the loop strands every interrupt-flagged wake queued against it — the
/// device interrupts still arrive and still flag, but nothing unparks their
/// waiters, and on a machine that routes device lines to one CPU that is
/// indistinguishable from a dead system.
///
/// A task that returns to the scheduler normally re-stamps progress long
/// before this window elapses, so a healthy task never triggers it; only a
/// genuine monopoliser does. Well below the 10-second soft/hard thresholds,
/// so the guard forces a housekeeping yield many times over before a stall
/// could ever be misjudged. A diagnostic/policy value, not a resource
/// capacity, and not a scheduler quantum: it is evaluated on interrupts
/// already firing on the CPU (the ~1 Hz watchdog cadence and the preemption
/// tick) and arms no new timer, so the tickless invariant is preserved.
pub const DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS: u64 = 1_000_000_000;

/// Nanoseconds per millisecond, for rendering the human-facing duration.
const NS_PER_MS: u64 = 1_000_000;

/// The watchdog activity class a CPU publishes so a cross-CPU check can
/// tell a CPU that *owes* progress apart from one that legitimately does
/// not. Encoded as the `u8` in `CpuState::wd_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogActivity {
    /// Not yet online, or has left the dispatch loop for good. Never
    /// judged.
    Offline = 0,
    /// Parked in `wfi` with nothing to run. Legitimately makes no
    /// progress and takes no ticks; never judged.
    Idle = 1,
    /// Running the dispatch loop or a task; owes forward progress.
    Active = 2,
}

impl WatchdogActivity {
    /// Recover the activity class from its stored `u8`, defaulting to
    /// [`Self::Offline`] for any unrecognised value (fail closed — an
    /// unknown state is never judged).
    #[must_use]
    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Idle,
            2 => Self::Active,
            _ => Self::Offline,
        }
    }
}

/// The installed report sink, or `None` before the boot path wires it.
static REPORT_SINK: OnceCell<&'static (dyn Sink + Sync)> = OnceCell::new();

/// The installed non-maskable recovery handle, or `None` before boot wires
/// it (a build with no pseudo-NMI channel never installs one).
static RECOVERY: OnceCell<&'static (dyn WatchdogArch + Sync)> = OnceCell::new();

/// The installed **diagnostic** sink the debug-only lockup detail is
/// emitted through, or `None` before the boot path wires it. Distinct from
/// [`REPORT_SINK`]: the always-on lockup *summary* goes to the report sink
/// (the persistent hash-chained audit log), while the address-bearing
/// *detail* goes here, to the diagnostic (log/UART) stream — so no kernel
/// address ever lands on the tamper-evident audit trail. Compiled in only
/// with the debug-diagnostics facility.
#[cfg(feature = "watchdog-diagnostics")]
static DIAG_SINK: OnceCell<&'static (dyn Sink + Sync)> = OnceCell::new();

/// The boot probe's non-maskable self-sample deliverability verdict,
/// recorded by the port ([`report_self_sample`]) before the diagnostic
/// sink exists, so it can be logged the instant that sink is installed.
/// `None` until the port reports it. Present only in a debug-diagnostics
/// build.
#[cfg(feature = "watchdog-diagnostics")]
static SELF_SAMPLE: OnceCell<FeatureSupport> = OnceCell::new();

/// Latches once the self-sample verdict has been logged, so the record is
/// emitted exactly once whichever of the record/install order wins the
/// race to a ready sink. Present only in a debug-diagnostics build.
#[cfg(feature = "watchdog-diagnostics")]
static SELF_SAMPLE_LOGGED: AtomicBool = AtomicBool::new(false);

/// The kernel image's runtime base address, registered once by the port
/// ([`set_kernel_image_base`]) from its linker `__kernel_start` symbol.
/// Kernel program counters in the debug detail are rendered *relative* to
/// this (`pc - base`), never absolute, so a capture resolves against the
/// debug kernel ELF without disclosing the runtime (KASLR-relocatable) load
/// base. `0` means "not yet registered": the detail then omits every
/// kernel-address field rather than emit a raw one (fail closed).
#[cfg(feature = "watchdog-diagnostics")]
static KERNEL_IMAGE_BASE: AtomicU64 = AtomicU64::new(0);

/// Resolves a stuck controller line to the task that owns its IRQ binding.
///
/// The watchdog reads a raw interrupt id from the shared controller when it
/// catches a hard lockup ([`WatchdogArch::stuck_interrupt`]); that id alone
/// does not say *whose* device it is. This seam lets the arch-neutral
/// watchdog attribute the line to the driver that bound it — turning
/// `stuck_irq=<id>` into `stuck_irq=<id> stuck_owner=<task>` for a bound
/// line, or `unbound` for a spurious/contained line no driver owns — without
/// the watchdog naming the kernel IRQ table type. The kernel binary installs
/// one backed by the live `IrqTable` ([`install_irq_owner`]).
pub trait StuckOwnerResolver: Sync {
    /// The task id that owns the binding for `line`, or `None` if `line` is
    /// bound to no task. A read-only lookup: it grants no authority and
    /// mutates nothing.
    fn owner_of_line(&self, line: u32) -> Option<u64>;
}

/// Names a stuck controller line that belongs to a kernel-internal source
/// rather than to a task's `irq_wait` binding.
///
/// Some enabled interrupt lines have no task owner by construction: the
/// kernel services them itself through a chained handler (the platform
/// message-signalled-interrupt multiplexer) or a bespoke path (the console
/// UART). The task-owner resolver ([`StuckOwnerResolver`]) rightly finds
/// no binding for these, so a hard-lockup report would render them as a bare
/// `unbound` — hiding that the pending line is, say, the USB/PCIe MSI line a
/// wedged CPU cannot service. This seam lets the port that *discovered* those
/// lines at runtime attribute one to a short, stable category name — turning
/// `stuck_irq=<id> stuck_owner=unbound` into `stuck_owner=<name>` — without
/// the arch-neutral watchdog naming any board, device, or line number itself:
/// it renders only whatever `&'static str` the port returns.
pub trait KernelInternalLines: Sync {
    /// A stable category name for the kernel-internal source that owns
    /// `line` (for example the platform MSI multiplexer or the console
    /// UART), or `None` when `line` is not a kernel-internal line this port
    /// recognises. A read-only lookup: it grants no authority and mutates
    /// nothing.
    fn name_of_line(&self, line: u32) -> Option<&'static str>;
}

/// The installed stuck-line owner resolver, or `None` before boot wires it.
static IRQ_OWNER: OnceCell<&'static (dyn StuckOwnerResolver + Sync)> = OnceCell::new();

/// The installed kernel-internal line-name resolver, or `None` before boot
/// wires it (or on a port with no kernel-internal enabled lines to name).
static KERNEL_LINE_NAMES: OnceCell<&'static (dyn KernelInternalLines + Sync)> = OnceCell::new();

/// Install the sink the watchdog reports lockups through. Idempotent by
/// policy: the boot path installs exactly one; a later call is a benign
/// no-op.
pub fn install_report_sink(sink: &'static (dyn Sink + Sync)) {
    let _ = REPORT_SINK.set(sink);
}

/// Install the architecture recovery handle the watchdog drives to break a
/// locked-up CPU out of its lockup. Idempotent; a port with no pseudo-NMI
/// channel simply never calls this and hard-lockup recovery stays inert.
pub fn install_recovery(arch: &'static (dyn WatchdogArch + Sync)) {
    let _ = RECOVERY.set(arch);
}

/// The report sink the watchdog currently emits through, if installed.
fn report_sink() -> Option<&'static (dyn Sink + Sync)> {
    REPORT_SINK.get().ok().flatten().copied()
}

/// The installed recovery handle, if any.
fn recovery() -> Option<&'static (dyn WatchdogArch + Sync)> {
    RECOVERY.get().ok().flatten().copied()
}

/// Install the **diagnostic** sink the debug-only lockup detail is emitted
/// through — the log/UART stream, kept separate from the persistent audit
/// [`REPORT_SINK`] so no kernel address ever lands on the tamper-evident
/// audit trail. Idempotent; the boot path installs exactly one. Present
/// only in a debug-diagnostics build.
#[cfg(feature = "watchdog-diagnostics")]
pub(crate) fn install_diagnostic_sink(sink: &'static (dyn Sink + Sync)) {
    let _ = DIAG_SINK.set(sink);
    // A self-sample verdict recorded before the sink existed is flushed the
    // moment the channel is ready, so the capability line is never lost to
    // boot ordering.
    try_log_self_sample();
}

/// The diagnostic sink the debug detail currently emits through, if any.
#[cfg(feature = "watchdog-diagnostics")]
fn diag_sink() -> Option<&'static (dyn Sink + Sync)> {
    DIAG_SINK.get().ok().flatten().copied()
}

/// Record the boot probe's verdict on whether the lockup watchdog's
/// non-maskable self-sample is deliverable on this hardware, and log it
/// once the diagnostic sink is ready.
///
/// The port probes this on the boot CPU during interrupt bring-up, *before*
/// the diagnostic sink is installed, so the verdict is stashed and flushed
/// by [`install_diagnostic_sink`] the moment the channel exists. Recording
/// it after the sink is already up logs immediately instead. Either way the
/// line is emitted exactly once. The whole self-sample discipline gates on
/// this verdict, so a reader of a later `sampled=pre_silence` hard-lockup
/// record needs it to judge whether that record names a real wedge or a
/// healthy core the watchdog simply could not observe. Present only in a
/// debug-diagnostics build; a shippable image never links it.
#[cfg(feature = "watchdog-diagnostics")]
pub fn report_self_sample(support: FeatureSupport) {
    let _ = SELF_SAMPLE.set(support);
    try_log_self_sample();
}

/// Emit the recorded self-sample verdict through the diagnostic sink, at
/// most once, once both the verdict and the sink are available. A no-op
/// while either is missing (fail-safe: the record is retried on the next
/// call), and after the one-shot latch has fired.
#[cfg(feature = "watchdog-diagnostics")]
fn try_log_self_sample() {
    let Some(support) = SELF_SAMPLE.get().ok().flatten().copied() else {
        return;
    };
    let Some(sink) = diag_sink() else {
        return;
    };
    if SELF_SAMPLE_LOGGED.swap(true, Ordering::AcqRel) {
        return;
    }
    emit_self_sample(sink, support);
}

/// The log line's plain fields for a self-sample verdict: the record
/// `Level`, whether the non-maskable sample is `live` or `inactive`, the
/// honesty term, and the non-live verdict's reason note (`None` when live).
/// A `live` sample makes a `sampled=pre_silence` lockup report credible; an
/// `inactive` one means the whole self-sample discipline is inert on this
/// hardware, so a hard-lockup report against a lone task is suspect — worth
/// a `Warn`.
#[cfg(feature = "watchdog-diagnostics")]
fn self_sample_labels(
    support: FeatureSupport,
) -> (Level, &'static str, &'static str, Option<&'static str>) {
    match support {
        FeatureSupport::Supported => (Level::Info, "live", "supported", None),
        FeatureSupport::Unsupported(reason) => {
            (Level::Warn, "inactive", "unsupported", Some(reason))
        }
        FeatureSupport::Pending(note) => (Level::Warn, "inactive", "pending", Some(note)),
    }
}

/// Render one [`AuditEvent::CpuWatchdogSelfSample`] record through `sink`.
/// Split out so a host test drives it against a recording sink. Carries no
/// kernel address and no secret — a capability statement, not a detail.
#[cfg(feature = "watchdog-diagnostics")]
fn emit_self_sample(sink: &dyn Sink, support: FeatureSupport) {
    let (level, self_sample, verdict, reason) = self_sample_labels(support);
    // Repeat-fill then overwrite, so the trailing slot is a real field until
    // a reason (when present) replaces it — the same construction the lockup
    // records use.
    let mut fields: [tairix_log::Field<'_>; 3] = [tairix_log::Field {
        key: "self_sample",
        value: tairix_log::FieldValue::Str(self_sample),
    }; 3];
    fields[1] = tairix_log::Field {
        key: "verdict",
        value: tairix_log::FieldValue::Str(verdict),
    };
    let mut n = 2;
    if let Some(reason) = reason {
        fields[n] = tairix_log::Field {
            key: "reason",
            value: tairix_log::FieldValue::Str(reason),
        };
        n += 1;
    }
    emit(sink, level, AuditEvent::CpuWatchdogSelfSample, &fields[..n]);
}

/// Register the kernel image's runtime base address (the port's linker
/// `__kernel_start`) so the debug detail can render kernel program counters
/// *relative* to it (`pc - base`) rather than absolute — the `%pK`-style
/// discipline that keeps the runtime (KASLR-relocatable) load base secret
/// while a capture still resolves against the debug kernel ELF. Idempotent
/// in effect (the base does not change for a boot). Present only in a
/// debug-diagnostics build; a shippable image never links it.
#[cfg(feature = "watchdog-diagnostics")]
pub fn set_kernel_image_base(base: u64) {
    KERNEL_IMAGE_BASE.store(base, Ordering::Relaxed);
}

/// The offset of `addr` from the registered kernel image base, or `None`
/// when the base is unregistered (`0`) or `addr` lies *below* it (not a
/// kernel-image address). Fail closed: the caller omits the field rather
/// than emit a raw absolute address that would disclose the load base.
#[cfg(feature = "watchdog-diagnostics")]
fn image_relative(addr: u64) -> Option<u64> {
    let base = KERNEL_IMAGE_BASE.load(Ordering::Relaxed);
    if base != 0 && addr >= base {
        Some(addr - base)
    } else {
        None
    }
}

// --- Debug-only lock-site observation -------------------------------

/// The installed current-CPU resolver for the lock-diagnostics observer, as
/// a thin `fn() -> Option<CpuId>` stored as a `usize` (`0` = none). The port
/// installs it ([`install_lock_diagnostics`]) with a lock-free register read
/// of the running CPU's dense id; the observer needs it because a
/// `tairix_sync` lock has no CPU argument.
#[cfg(feature = "watchdog-diagnostics")]
static LOCK_DIAG_CPU_FN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Install the debug-only lock-site diagnostics: register `current_cpu` (a
/// lock-free resolver of the running CPU's dense id) and wire the
/// `tairix_sync` lock observer so each CPU's current spinlock is recorded
/// into its per-CPU [`crate::cpu_state`] slot. After this a hard-lockup
/// report names the exact spinlock a wedged core is stuck on (`k_lock`) —
/// the one culprit the maskable liveness sample cannot observe when the
/// core wedges with interrupts off inside the critical section.
///
/// Called once per boot by the port, after the per-CPU state table exists.
/// Present only in a debug-diagnostics build; a shippable image never links
/// it and every lock stays a bare compare-and-swap.
#[cfg(feature = "watchdog-diagnostics")]
pub fn install_lock_diagnostics(current_cpu: fn() -> Option<CpuId>) {
    LOCK_DIAG_CPU_FN.store(current_cpu as usize, Ordering::Relaxed);
    tairix_sync::lockwatch::install(lock_observer);
}

/// Resolve the running CPU's dense id through the installed resolver, or
/// `None` before one is installed (fail-safe: record nothing).
#[cfg(feature = "watchdog-diagnostics")]
fn lock_diag_current_cpu() -> Option<CpuId> {
    let raw = LOCK_DIAG_CPU_FN.load(Ordering::Relaxed);
    if raw == 0 {
        return None;
    }
    // SAFETY: `install_lock_diagnostics` only ever stores a value produced
    // by `(fn() -> Option<CpuId>) as usize`; a non-zero slot is therefore a
    // valid such function pointer with no captured environment.
    let f: fn() -> Option<CpuId> =
        unsafe { core::mem::transmute::<usize, fn() -> Option<CpuId>>(raw) };
    f()
}

/// The `tairix_sync` lock observer: record `event` for the running CPU's
/// current spinlock into its per-CPU lock-site stack.
///
/// Lock-free and allocation-free — it runs *inside* the lock primitives, so
/// it must never take a lock. It only resolves the CPU id (a register read)
/// and stores into per-CPU atomics. A context with no resolvable CPU or no
/// per-CPU slot (pre-init, or a stray id) records nothing (fail-safe).
#[cfg(feature = "watchdog-diagnostics")]
fn lock_observer(event: tairix_sync::lockwatch::LockEvent, site_ptr: usize) {
    use tairix_sync::lockwatch::LockEvent;
    let Some(cpu) = lock_diag_current_cpu() else {
        return;
    };
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    match event {
        LockEvent::Acquiring => lock_push(state, site_ptr, true),
        LockEvent::TryAcquired => lock_push(state, site_ptr, false),
        // Promote the innermost record from acquiring to held; its site was
        // pushed by the preceding `Acquiring` for the same lock.
        LockEvent::Acquired => state.lock_top_acquiring.store(false, Ordering::Relaxed),
        LockEvent::Released => lock_pop(state),
    }
}

/// Push a lock record for `state`: store `site_ptr` at the current depth
/// (when within [`cpu_state::LOCK_STACK_MAX`]) and mark whether the top is
/// still acquiring. The depth is bumped last (release) so a reader that
/// loads it (acquire) first sees the matching site. Depth counts true
/// nesting (saturating) so [`lock_pop`] stays balanced even past the cap.
#[cfg(feature = "watchdog-diagnostics")]
fn lock_push(state: &CpuState, site_ptr: usize, acquiring: bool) {
    let depth = state.lock_depth.load(Ordering::Relaxed);
    if depth < cpu_state::LOCK_STACK_MAX {
        state.lock_sites[depth].store(site_ptr, Ordering::Relaxed);
    }
    state.lock_top_acquiring.store(acquiring, Ordering::Relaxed);
    state
        .lock_depth
        .store(depth.saturating_add(1), Ordering::Release);
}

/// Pop the innermost lock record for `state`. The new top (if any) is a
/// held lock by construction — a deeper lock is only taken from inside an
/// already-held section — so the acquiring flag clears.
#[cfg(feature = "watchdog-diagnostics")]
fn lock_pop(state: &CpuState) {
    let depth = state.lock_depth.load(Ordering::Relaxed);
    if depth > 0 {
        state.lock_depth.store(depth - 1, Ordering::Release);
    }
    state.lock_top_acquiring.store(false, Ordering::Relaxed);
}

/// Install the resolver that attributes a stuck controller line to the task
/// that owns its IRQ binding (see [`StuckOwnerResolver`]). Idempotent; a
/// build that never installs one simply omits the `stuck_owner` field.
pub fn install_irq_owner(resolver: &'static (dyn StuckOwnerResolver + Sync)) {
    let _ = IRQ_OWNER.set(resolver);
}

/// The installed stuck-line owner resolver, if any.
fn irq_owner() -> Option<&'static (dyn StuckOwnerResolver + Sync)> {
    IRQ_OWNER.get().ok().flatten().copied()
}

/// Install the resolver that names a stuck line belonging to a
/// kernel-internal source (see [`KernelInternalLines`]). Idempotent; a port
/// with no kernel-internal enabled lines simply never installs one and such
/// a line keeps rendering as `unbound`.
pub fn install_kernel_line_names(names: &'static (dyn KernelInternalLines + Sync)) {
    let _ = KERNEL_LINE_NAMES.set(names);
}

/// The installed kernel-internal line-name resolver, if any.
fn kernel_line_names() -> Option<&'static (dyn KernelInternalLines + Sync)> {
    KERNEL_LINE_NAMES.get().ok().flatten().copied()
}

/// A coarse tag for the in-kernel region a CPU last entered, published by
/// the CPU *itself* (through [`note_kernel_breadcrumb`]) as it runs.
///
/// This is the diagnosis a hard-locked CPU can still give on a board with
/// no non-maskable interrupt channel (the Raspberry Pi 4's GICv2 in the
/// non-secure world): a CPU wedged with maskable interrupts off cannot be
/// sampled by its own watchdog IRQ, nor interrupted by a buddy's IPI, so
/// its last watchdog-sampled context (`wd_ctx_pc`) is stale — the innocent
/// syscall-entry PC it last returned to (`sampled=pre_silence`). The
/// breadcrumb, in contrast, is written by the CPU on the way *into* the
/// region it is now stuck in, so the buddy observer's report names the real
/// activity: which syscall, which user-fault resolver phase, or the
/// scheduler dispatch loop. Ordered before the more expensive locking work
/// of each region, so a wedge inside that work is attributed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KernelBreadcrumb {
    /// No region recorded yet (the boot default, and the value a slot
    /// carries before its CPU has run any instrumented kernel region).
    None = 0,
    /// The scheduler dispatch loop between task runs (detail: unused, `0`).
    Dispatch = 1,
    /// A syscall handler body (detail: the raw syscall number).
    Syscall = 2,
    /// The user-fault resolver, before any phase decided (detail: the
    /// faulting virtual address).
    FaultEntry = 3,
    /// The foreground direct-reclaim step of the fault resolver (detail:
    /// the faulting virtual address).
    FaultReclaim = 4,
    /// The stack-growth phase of the fault resolver (detail: the faulting
    /// virtual address).
    FaultStack = 5,
    /// The compressed-memory (`ramzip`) restore phase (detail: the faulting
    /// virtual address).
    FaultRamzip = 6,
    /// The demand-paged anonymous-memory phase (detail: the faulting
    /// virtual address).
    FaultAnon = 7,
    /// The demand-paged file-backing phase (detail: the faulting virtual
    /// address).
    FaultFile = 8,
    /// The fatal (task-kill) phase of the fault resolver (detail: the
    /// faulting virtual address).
    FaultFatal = 9,
    /// The task-body shim ([`crate::kthread`] `dispatch_step`) the CFQ
    /// dispatch handed control to, *before* the context switch into the
    /// task — the `pending_upgrade` install and, for a user kthread, the
    /// address-space reactivation (`pre_resume`) and resume/live-space
    /// publication (detail: the dispatched task id). Distinguishes a wedge
    /// in the hand-off/address-space-activation prologue from one in the
    /// scheduler's own pick/steal machinery (which stays [`Self::Dispatch`],
    /// set before the body closure runs).
    TaskBody = 10,
    /// The context switch into a **user** task ([`crate::kthread`]
    /// `dispatch_step`, immediately before `ContextSwitch::switch`), that
    /// task's EL0 execution up to its first syscall/fault (which re-stamps
    /// the breadcrumb), and the arch switch *back* to the dispatcher — the
    /// crumb held for the whole `ContextSwitch::switch` call. Distinguishes a
    /// wedge in the arch switch or early user-entry from one in the shim
    /// prologue ([`Self::TaskBody`]) below it, or in the post-switch
    /// dispatcher teardown ([`Self::SwitchReturn`]) above it (detail: unused,
    /// `0` — the task id is carried by the preceding [`Self::TaskBody`]
    /// crumb).
    UserSwitch = 11,
    /// The dispatcher-side teardown that runs immediately after
    /// `ContextSwitch::switch` returns control from the task
    /// ([`crate::kthread`] `dispatch_step`): retiring the task's resume
    /// handle, clearing its live-space publication, and — for a user
    /// kthread — parking this CPU's translation off the task's user root (a
    /// translation-register write) and checking the kernel-stack guard. It
    /// runs with device interrupts still masked (inherited from the
    /// suspending task's exception entry), so a wedge here sits in an
    /// IRQ-masked section the maskable liveness sample cannot observe.
    /// Distinguishes such a wedge — notably in the user-root translation
    /// park — from one in the arch context switch or EL0 execution
    /// ([`Self::UserSwitch`]) that precedes it, before the post-run
    /// accounting tail ([`Self::DispatchTail`]) that follows once the body
    /// shim returns (detail: unused, `0` — the task id is carried by the
    /// preceding [`Self::TaskBody`] crumb).
    SwitchReturn = 12,
    /// The CFQ post-run accounting tail that runs after the task body
    /// returned to the shim ([`crate::kthread`] `dispatch_step` returned):
    /// run-accounting, vruntime charge, and re-enqueue/retire — the section
    /// that runs with device interrupts still masked (inherited from the
    /// suspending task's exception entry) until the dispatch loop restores
    /// them. Distinguishes a wedge in that accounting from one in the arch
    /// switch ([`Self::UserSwitch`]) or the post-switch dispatcher teardown
    /// ([`Self::SwitchReturn`]) that precede it (detail: the dispatched task
    /// id).
    DispatchTail = 13,
    /// The context switch into a **kernel** kthread and that kthread's body
    /// running kernel code — the [`Self::UserSwitch`] counterpart for a task
    /// that has no user address space and never leaves EL1, so no syscall or
    /// fault ever re-stamps the crumb and this one is held for the entire
    /// body run (detail: unused, `0` — the task id is carried by the
    /// preceding [`Self::TaskBody`] crumb).
    ///
    /// Separate from [`Self::UserSwitch`] because a stall reported against a
    /// kernel-context sample means something quite different in each case: a
    /// long in-kernel service body here, versus a task executing user code
    /// there. One tag covering both sent a reader looking for a misbehaving
    /// user program when the CPU was in fact inside a kernel service.
    KernelBody = 14,
}

// These decode/render helpers are consumed only by the debug-diagnostics
// snapshot and render, so they compile in only with the facility; the enum
// itself stays defined unconditionally so the breadcrumb call sites on the
// syscall/dispatch/fault paths need no `cfg` and stay one clean seam.
#[cfg(feature = "watchdog-diagnostics")]
impl KernelBreadcrumb {
    /// Decode a stored `u8` tag, treating any unknown value as
    /// [`KernelBreadcrumb::None`] (fail closed — a corrupt slot never
    /// fabricates a region).
    #[must_use]
    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Dispatch,
            2 => Self::Syscall,
            3 => Self::FaultEntry,
            4 => Self::FaultReclaim,
            5 => Self::FaultStack,
            6 => Self::FaultRamzip,
            7 => Self::FaultAnon,
            8 => Self::FaultFile,
            9 => Self::FaultFatal,
            10 => Self::TaskBody,
            11 => Self::UserSwitch,
            12 => Self::SwitchReturn,
            13 => Self::DispatchTail,
            14 => Self::KernelBody,
            _ => Self::None,
        }
    }

    /// A short, stable tag for the lockup record's `k_site` field.
    #[must_use]
    fn tag(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dispatch => "dispatch",
            Self::Syscall => "syscall",
            Self::FaultEntry => "fault_entry",
            Self::FaultReclaim => "fault_reclaim",
            Self::FaultStack => "fault_stack",
            Self::FaultRamzip => "fault_ramzip",
            Self::FaultAnon => "fault_anon",
            Self::FaultFile => "fault_file",
            Self::FaultFatal => "fault_fatal",
            Self::TaskBody => "task_body",
            Self::UserSwitch => "user_switch",
            Self::SwitchReturn => "switch_return",
            Self::DispatchTail => "dispatch_tail",
            Self::KernelBody => "kernel_body",
        }
    }
}

/// Publish `cpu`'s kernel-activity breadcrumb: the region it is entering
/// (`site`) and its datum (`detail` — a syscall number or faulting
/// address). Written by the CPU itself so a later hard-lockup report can
/// name the region even when the CPU can no longer be sampled.
///
/// Three relaxed stores plus one release bump — the same order of cost as
/// the progress/liveness heartbeats already on these paths, and the price
/// of a diagnosable hard lockup on a board without a non-maskable channel.
/// The `detail` and `site` are stored *before* the release bump of the
/// sequence, so a reader that observes a fresh sequence (acquire) sees a
/// matching site and detail. An out-of-range `cpu` is a no-op (fail
/// closed).
///
/// `detail` is restricted by construction to non-secret diagnostic data:
/// the callers pass only a raw syscall number, a faulting virtual address,
/// or a task id — never a syscall *argument value*, buffer contents, key,
/// credential, or capability token, and the type cannot carry one.
///
/// This is part of the debug-diagnostics facility. In a shippable image the
/// `watchdog-diagnostics` feature is off, the body below is compiled out,
/// and the ~12 breadcrumb call sites on the syscall / scheduler-dispatch /
/// user-fault hot paths inline to nothing — no atomics, no branch, no
/// stored region — so the release kernel pays exactly zero for a breadcrumb
/// it can never emit. The signature stays so those call sites need no `cfg`.
#[cfg(feature = "watchdog-diagnostics")]
pub fn note_kernel_breadcrumb(cpu: CpuId, site: KernelBreadcrumb, detail: u64) {
    if let Some(state) = cpu_state::get(cpu) {
        state.kbc_detail.store(detail, Ordering::Relaxed);
        state.kbc_site.store(site as u8, Ordering::Relaxed);
        state.kbc_seq.fetch_add(1, Ordering::Release);
    }
}

/// Compiled-out no-op form of [`note_kernel_breadcrumb`] for a shippable
/// image (the `watchdog-diagnostics` feature off): it records nothing and
/// inlines away, so the syscall / dispatch / fault call sites cost nothing.
#[cfg(not(feature = "watchdog-diagnostics"))]
#[inline(always)]
pub fn note_kernel_breadcrumb(_cpu: CpuId, _site: KernelBreadcrumb, _detail: u64) {}

/// The maximum number of pre-silence backtrace frames the watchdog keeps
/// per CPU — the fixed buffer a port fills through [`note_watchdog_backtrace`]
/// and the report renders. Re-exported so the port sizes its capture buffer
/// to exactly what will be stored (never a private second copy).
///
/// Part of the debug-diagnostics facility: it and [`note_watchdog_backtrace`]
/// exist only with the `watchdog-diagnostics` feature, so a shippable image
/// compiles the whole pre-silence backtrace path — capture, storage, and
/// render — out entirely.
#[cfg(feature = "watchdog-diagnostics")]
pub const WATCHDOG_BACKTRACE_MAX: usize = cpu_state::WD_BT_MAX;

/// Record `cpu`'s **pre-silence backtrace**: the return-address chain
/// (`frames`, innermost first, starting at the interrupted PC) the port
/// unwound from the context this CPU's latest non-maskable watchdog sample
/// interrupted.
///
/// The port captures this on every cadence sample, so on a hard lockup —
/// where the CPU can no longer be sampled and its `pc` is a stale
/// `pre_silence` single word — the observer's report can still render the
/// whole call nest the CPU was in ~1 s before it went silent, which a lone
/// address cannot give. At most [`WATCHDOG_BACKTRACE_MAX`] frames are kept;
/// the length is stored (release) *after* the frames so a reader that loads
/// it (acquire) first sees a consistent set. An empty `frames` clears the
/// record (the report then omits it, never fabricating one); an
/// out-of-range `cpu` is a no-op (fail closed).
#[cfg(feature = "watchdog-diagnostics")]
pub fn note_watchdog_backtrace(cpu: CpuId, frames: &[u64]) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    let len = frames.len().min(cpu_state::WD_BT_MAX);
    for (slot, &pc) in state.wd_bt.iter().zip(frames.iter()).take(len) {
        slot.store(pc, Ordering::Relaxed);
    }
    // Publish the length last (release) so a consumer that reads it first
    // (acquire) observes the frames stored above. `len` is bounded by
    // `WD_BT_MAX` (a small const), so the conversion never truncates; the
    // saturating fallback keeps the store total without an `unwrap`.
    state
        .wd_bt_len
        .store(u32::try_from(len).unwrap_or(u32::MAX), Ordering::Release);
}

/// The outcome of one heartbeat evaluation on a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    /// The heartbeat has never been stamped — fail closed, no judgement.
    Unarmed,
    /// The heartbeat is recent (within the threshold): healthy.
    Healthy,
    /// The threshold was crossed and this sample first observed the
    /// episode: report it. Carries the elapsed gap in nanoseconds.
    Onset(u64),
    /// The threshold is crossed but the episode was already reported.
    Still,
}

/// Evaluate one heartbeat `last_ts` against `now`/`threshold`, latching
/// `reported` so a crossed threshold reports exactly once per episode.
/// Pure but for the latch swap; the caller does the I/O.
fn evaluate(last_ts: u64, reported: &AtomicBool, now: u64, threshold: u64) -> Sample {
    if last_ts == 0 {
        return Sample::Unarmed;
    }
    let elapsed = now.saturating_sub(last_ts);
    if elapsed < threshold {
        return Sample::Healthy;
    }
    if reported.swap(true, Ordering::AcqRel) {
        Sample::Still
    } else {
        Sample::Onset(elapsed)
    }
}

/// Publish `cpu`'s watchdog activity class so the cross-CPU scan can tell
/// a CPU that owes progress apart from one that legitimately does not.
/// An out-of-range `cpu` is a no-op (fail closed).
pub fn set_activity(cpu: CpuId, activity: WatchdogActivity) {
    if let Some(state) = cpu_state::get(cpu) {
        state.wd_activity.store(activity as u8, Ordering::Release);
    }
}

// --- Heartbeats -----------------------------------------------------

/// Stamp `state`'s progress heartbeat at `now_ns`, clearing the soft
/// episode latch. Returns `Some(stalled_ns)` when the CPU was in a
/// *reported* soft lockup and has now recovered (the caller emits a
/// recovery record); `None` on the ordinary healthy path. A `0` reading is
/// stamped as `1` so a stamped heartbeat is never mistaken for the
/// "unarmed" sentinel.
fn record_progress(state: &CpuState, now_ns: u64) -> Option<u64> {
    let prev = state
        .last_progress_ns
        .swap(now_ns.max(1), Ordering::Release);
    if state.stall_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

/// Record that the scheduler made progress on `cpu` at monotonic time
/// `now_ns` (the dispatch loop calls this once per iteration).
///
/// Pure accounting on the common path (one atomic store); it emits only on
/// the rare edge where a previously-reported soft lockup clears, so it is
/// cheap enough for the hot dispatch path. An out-of-range `cpu` is a
/// no-op (fail closed).
pub fn note_progress(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Some(stalled_ns) = record_progress(state, now_ns) {
        report_lockup(
            AuditEvent::CpuStallCleared,
            Level::Warn,
            cpu,
            None,
            stalled_ns,
            &Diag::EMPTY,
        );
    }
}

/// Stamp `state`'s liveness heartbeat at `now_ns` *without* a sampled
/// context, returning `Some(stalled_ns)` when the CPU was in a *reported*
/// hard lockup and has now recovered.
///
/// This is the liveness analogue of [`record_progress`]: reaching the
/// dispatch loop is itself proof the CPU is alive and takes interrupts (it
/// either just woke from `wfi` by taking one, or is running continuously),
/// so the dispatcher stamps liveness here for the same reason it stamps
/// progress. Without it a CPU returning to `Active` after a long idle park
/// would carry the stale liveness heartbeat from *before* the park — its
/// non-maskable sample is only taken while running, not while parked — and
/// a cross-CPU scan would falsely report it hard-locked the instant it
/// republishes `Active`. Unlike [`record_liveness`] it leaves the recorded
/// sample context untouched: the dispatcher has no interrupted PC/PSTATE to
/// record, and the context only feeds a report's "why".
fn record_alive(state: &CpuState, now_ns: u64) -> Option<u64> {
    let prev = state.last_seen_ns.swap(now_ns.max(1), Ordering::Release);
    if state.hard_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

/// Record that `cpu` is alive at monotonic time `now_ns` from a context
/// that proves liveness without a sampled interrupt — the dispatch loop
/// calls this once per iteration, alongside [`note_progress`], before it
/// republishes the CPU as `Active`.
///
/// Pure accounting on the common path (one atomic store); it emits only on
/// the rare edge where a previously-reported hard lockup clears, so it is
/// cheap enough for the hot dispatch path. An out-of-range `cpu` is a
/// no-op (fail closed).
pub fn note_alive(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    if let Some(stalled_ns) = record_alive(state, now_ns) {
        report_lockup(
            AuditEvent::CpuHardLockupCleared,
            Level::Warn,
            cpu,
            None,
            stalled_ns,
            &Diag::EMPTY,
        );
    }
}

/// Stamp `state`'s liveness heartbeat at `now_ns` and record `sample` as
/// its last-known context. Returns `Some(stalled_ns)` when the CPU was in
/// a *reported* hard lockup and has now recovered.
fn record_liveness(state: &CpuState, now_ns: u64, sample: &WatchdogSample) -> Option<u64> {
    // Record the context first so a buddy that observes the fresh liveness
    // heartbeat also sees a matching (or newer) context.
    state.wd_ctx_pc.store(sample.pc, Ordering::Relaxed);
    state.wd_ctx_task.store(sample.task, Ordering::Relaxed);
    state.wd_ctx_aux.store(sample.aux, Ordering::Relaxed);
    state
        .wd_ctx_in_kernel
        .store(sample.in_kernel, Ordering::Relaxed);
    let prev = state.last_seen_ns.swap(now_ns.max(1), Ordering::Release);
    if state.hard_reported.swap(false, Ordering::AcqRel) {
        Some(now_ns.saturating_sub(prev))
    } else {
        None
    }
}

// --- The non-maskable cadence sample --------------------------------

/// Handle one non-maskable watchdog cadence sample taken *on* `cpu` at
/// monotonic time `now_ns`, with `sample` describing what it interrupted.
///
/// The port's pseudo-NMI cadence path (the aarch64 FIQ watchdog) calls
/// this on its own CPU. It stamps this CPU's liveness heartbeat, records
/// the sample as its last-known context (the raw "why"), reports a
/// recovery if this CPU was previously hard-locked, and then runs the
/// cross-CPU scan — the only place a *hard* lockup on another CPU becomes
/// observable. Lock-free and allocation-free, safe to call from the
/// non-maskable path. An out-of-range `cpu` still runs the scan (the CPU
/// is simply not itself sampled), so a stray id never blinds the detector.
pub fn on_watchdog_tick(cpu: CpuId, now_ns: u64, sample: &WatchdogSample) {
    if let Some(state) = cpu_state::get(cpu) {
        if let Some(stalled_ns) = record_liveness(state, now_ns, sample) {
            report_lockup(
                AuditEvent::CpuHardLockupCleared,
                Level::Warn,
                cpu,
                None,
                stalled_ns,
                &Diag::EMPTY,
            );
        }
        // A lone CPU-bound *user* task that never returns to the scheduler
        // keeps taking this very cadence sample (so it is not wedged), but
        // it withholds the CPU from the dispatch loop, stalling housekeeping
        // and the progress heartbeat. Force it back to the dispatcher once:
        // the return-to-user preempt point this same interrupt runs honours
        // the request, so the task cannot monopolise the CPU by refusing to
        // yield. Nothing is armed here beyond the cadence already firing.
        if monopolises_cpu(state, now_ns, sample.in_kernel) {
            crate::preempt::request_forced_yield(cpu);
        }
    }
    scan(cpu, now_ns);
}

/// Whether `state`'s CPU is an `Active` core that has withheld the CPU
/// from the scheduler past [`DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS`].
///
/// Pure predicate (no I/O, no latch), so the monopoly policy is unit-tested
/// directly. Only a CPU that is `Active` and has an *armed* progress
/// heartbeat older than the guard qualifies; an unarmed heartbeat (`0`) or
/// a clock that went backwards never does (fail closed — no phantom yield).
///
/// Deliberately takes no sampled context: the recorded kernel/user field is
/// refreshed only by a cadence sample, so on a CPU whose cadence has
/// stopped it rots at whatever it last read and would suppress the guard
/// exactly when it is needed. Callers that *do* hold a fresh context apply
/// it themselves ([`monopolises_cpu`]).
fn progress_overdue(state: &CpuState, now_ns: u64) -> bool {
    if WatchdogActivity::from_u8(state.wd_activity.load(Ordering::Acquire))
        != WatchdogActivity::Active
    {
        return false;
    }
    let last_progress = state.last_progress_ns.load(Ordering::Acquire);
    if last_progress == 0 {
        return false;
    }
    now_ns.saturating_sub(last_progress) >= DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS
}

/// [`progress_overdue`] for a caller holding a **fresh** sample of what the
/// CPU was running: the cadence path, whose `in_kernel` reading was taken
/// by the very sample now calling in. Kernel code is never preempted (the
/// kernel is non-preemptible), so an in-kernel sample owes no yield.
fn monopolises_cpu(state: &CpuState, now_ns: u64, in_kernel: bool) -> bool {
    !in_kernel && progress_overdue(state, now_ns)
}

/// Whether `state`'s recorded context is older than the port's cadence
/// interval — the CPU stopped sampling itself, so pc/aux/kernel-or-user
/// name the code it last returned to rather than what it is running now.
///
/// A never-sampled heartbeat (`0`) counts as stale: nothing vouches for the
/// context at all. One cadence interval of slack, so an ordinary sample
/// that merely straddles a report is not cried down as stale.
fn context_stale(state: &CpuState, now_ns: u64) -> bool {
    let last_seen = state.last_seen_ns.load(Ordering::Acquire);
    last_seen == 0 || now_ns.saturating_sub(last_seen) > tairix_arch_api::WATCHDOG_CADENCE_NS
}

/// The verdict of classifying one CPU during the cross-CPU scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Healthy, idle, offline, or an already-reported ongoing episode.
    Quiet,
    /// A newly detected hard lockup, carrying the silent gap in ns.
    HardOnset(u64),
    /// A newly detected soft lockup, carrying the no-progress gap in ns.
    SoftOnset(u64),
}

/// Classify one CPU's `state` at `now_ns`, latching its per-episode flag so
/// a crossing is reported exactly once. Pure but for the latch swap; the
/// caller does the I/O. Only an `Active` CPU is judged; a hard lockup takes
/// precedence over a soft one, and a soft lockup is only reported for a CPU
/// last seen *in the kernel* (a lone preemptible user task owes no
/// progress).
fn classify(state: &CpuState, now_ns: u64) -> Verdict {
    if WatchdogActivity::from_u8(state.wd_activity.load(Ordering::Acquire))
        != WatchdogActivity::Active
    {
        return Verdict::Quiet;
    }
    match evaluate(
        state.last_seen_ns.load(Ordering::Acquire),
        &state.hard_reported,
        now_ns,
        DEFAULT_HARD_LOCKUP_THRESHOLD_NS,
    ) {
        Sample::Onset(elapsed) => return Verdict::HardOnset(elapsed),
        // Still hard-locked (already reported): do not also raise a soft
        // report for the same wedged CPU.
        Sample::Still => return Verdict::Quiet,
        Sample::Healthy | Sample::Unarmed => {}
    }
    if !state.wd_ctx_in_kernel.load(Ordering::Acquire) {
        return Verdict::Quiet;
    }
    match evaluate(
        state.last_progress_ns.load(Ordering::Acquire),
        &state.stall_reported,
        now_ns,
        DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
    ) {
        Sample::Onset(elapsed) => Verdict::SoftOnset(elapsed),
        _ => Verdict::Quiet,
    }
}

/// Scan every CPU other than the `observer` for a lockup, reporting and
/// attempting recovery for each newly detected episode.
///
/// Runs from the observer's non-maskable sample, so it can see a CPU that
/// has stopped taking maskable interrupts entirely (a hard lockup) —
/// exactly the case the victim's own timer path is blind to.
fn scan(observer: CpuId, now_ns: u64) {
    for (index, state) in cpu_state::states().iter().enumerate() {
        // The observer is, by definition, alive — skip it.
        if observer as usize == index {
            continue;
        }
        let Ok(target) = CpuId::try_from(index) else {
            continue;
        };
        match classify(state, now_ns) {
            Verdict::HardOnset(elapsed) => {
                // The victim's own last-known sample is stale (it went
                // silent), so mark it pre-silence and read the shared
                // controller live for the device line actually wedging it —
                // the "why" the stale sample cannot give.
                let mut diag = Diag::snapshot(state);
                diag.sample_stale = true;
                diag.stuck = recovery().and_then(WatchdogArch::stuck_interrupt);
                diag.stuck_owner = resolve_stuck_owner(diag.stuck);
                // The observer reads the wedged core's PC over the port's
                // non-maskable external-debug channel — the fresh "why" the
                // stale pre-silence sample cannot give. A port with no such
                // channel (or none discovered for the target) answers
                // `Unsupported`, leaving `live_pc` empty so the report falls
                // back to the stale sample. Debug-diagnostics only: the field
                // is rendered solely into the address-bearing detail record.
                #[cfg(feature = "watchdog-diagnostics")]
                if let Some(RemotePcSample::Sampled { pc, context }) =
                    recovery().map(|r| r.remote_pc_sample(target))
                {
                    diag.live_pc = Some(pc);
                    diag.live_context = context;
                }
                // What the victim itself published as still in flight. The
                // shared-controller read above cannot see a banked SGI or
                // PPI, so without this a core wedged inside one is reported
                // against whatever device line happened to be pending.
                #[cfg(feature = "watchdog-diagnostics")]
                if let Some(in_flight) = recovery().map(|r| r.in_flight_interrupt(target)) {
                    diag.in_flight = in_flight;
                }
                report_lockup(
                    AuditEvent::CpuHardLockupDetected,
                    Level::Error,
                    target,
                    Some(observer),
                    elapsed,
                    &diag,
                );
                drive_recovery(target, WatchdogKind::Hard);
            }
            Verdict::SoftOnset(elapsed) => {
                // A soft-locked CPU is normally still taking its watchdog
                // sample, so its context is fresh — but that is read, not
                // assumed, because a context older than the cadence names
                // innocent code. There is no stuck-line story to tell.
                let mut diag = Diag::snapshot(state);
                diag.sample_stale = context_stale(state, now_ns);
                report_lockup(
                    AuditEvent::CpuStallDetected,
                    Level::Error,
                    target,
                    Some(observer),
                    elapsed,
                    &diag,
                );
                drive_recovery(target, WatchdogKind::Soft);
            }
            Verdict::Quiet => {}
        }
    }
}

/// Break a CPU's monopoly and check it for a **soft** lockup, from its own
/// armed timer-tick path.
///
/// The same-CPU complement of the cross-CPU `scan`'s check, and the
/// watchdog's *last* working channel on a CPU whose non-maskable cadence
/// has stopped: the maskable tick keeps firing there, so this runs when
/// [`on_watchdog_tick`] — and with it the cadence-driven monopoly guard —
/// no longer does. It therefore does two things: request the forced yield
/// that returns a CPU withholding itself from the dispatcher, and report a
/// soft lockup once the no-progress gap crosses the threshold.
///
/// Reads the installed monotonic clock; before it, or for an out-of-range
/// `cpu`, it is a fail-safe no-op. Shares the soft-episode latch with
/// `scan`, so a soft lockup is reported exactly once whichever path sees
/// it first — but the yield request deliberately does *not* share it.
pub fn check_stall(cpu: CpuId) {
    let Some(now_ns) = crate::waitq::wait_now_ns() else {
        return;
    };
    check_stall_at(cpu, now_ns);
}

/// [`check_stall`] at an explicit `now_ns`. Split from the public entry
/// point so host tests can drive both halves against a chosen instant
/// without the installed clock.
pub(crate) fn check_stall_at(cpu: CpuId, now_ns: u64) {
    let Some(state) = cpu_state::get(cpu) else {
        return;
    };
    // Break the monopoly first, and on every tick: this is the one channel
    // still running on a CPU whose non-maskable cadence has stopped, and
    // that CPU's cadence-driven guard is dead with it. Read unlatched, so a
    // core that stays out of the dispatch loop is pushed back at every tick
    // rather than once per episode.
    if progress_overdue(state, now_ns) {
        crate::preempt::request_forced_yield(cpu);
    }
    let soft = evaluate(
        state.last_progress_ns.load(Ordering::Acquire),
        &state.stall_reported,
        now_ns,
        DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
    );
    if let Sample::Onset(elapsed) = soft {
        let mut diag = Diag::snapshot(state);
        // This path runs from the maskable tick, which keeps firing on a
        // CPU whose non-maskable cadence has died — so the context it
        // renders can be arbitrarily old and must say so.
        diag.sample_stale = context_stale(state, now_ns);
        report_lockup(
            AuditEvent::CpuStallDetected,
            Level::Error,
            cpu,
            None,
            elapsed,
            &diag,
        );
    }
}

// --- Recovery -------------------------------------------------------

/// A short, fixed tag for a [`RecoveryOutcome`], for the audit record.
fn outcome_tag(outcome: RecoveryOutcome) -> &'static str {
    match outcome {
        RecoveryOutcome::Rescheduled => "rescheduled",
        RecoveryOutcome::AttentionRaised => "attention",
        RecoveryOutcome::Unrecoverable => "unrecoverable",
        RecoveryOutcome::Unsupported => "unsupported",
    }
}

/// Ask the installed port to break `target` out of a `kind` lockup,
/// best-effort, and record the attempt and its honest outcome. A build
/// with no recovery handle records the attempt as `unsupported` rather
/// than silently doing nothing.
fn drive_recovery(target: CpuId, kind: WatchdogKind) {
    let outcome = recovery().map_or(RecoveryOutcome::Unsupported, |arch| {
        arch.request_recovery(target, kind)
    });
    if let Some(sink) = report_sink() {
        recovery_to(sink, target, kind, outcome);
    }
}

/// Render one recovery-attempt record through `sink`. Split from
/// [`drive_recovery`] so host tests can drive the render against a
/// recording sink without touching the process-wide install seam.
fn recovery_to(sink: &dyn Sink, target: CpuId, kind: WatchdogKind, outcome: RecoveryOutcome) {
    let fields = [
        tairix_log::Field {
            key: "cpu",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(target)),
        },
        tairix_log::Field {
            key: "kind",
            value: tairix_log::FieldValue::Str(kind.tag()),
        },
        tairix_log::Field {
            key: "outcome",
            value: tairix_log::FieldValue::Str(outcome_tag(outcome)),
        },
    ];
    emit(sink, Level::Warn, AuditEvent::CpuLockupRecovery, &fields);
}

// --- Diagnostics ("why") --------------------------------------------

/// A snapshot of a CPU's last-known interrupted context — the raw "why" a
/// lockup diagnosis renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Diag {
    /// Last-known interrupted program counter (`0` = none).
    pc: u64,
    /// Last-known running task id ([`WatchdogSample::NO_TASK`] = none).
    task: u64,
    /// Last-known port-defined processor-state word (`0` = none). An
    /// architecture register (aarch64 `SPSR_EL1`), not a kernel address,
    /// but a raw internal-state value nonetheless, so it is confined to the
    /// debug-only detail record and never the always-on summary.
    #[cfg(feature = "watchdog-diagnostics")]
    aux: u64,
    /// Whether the last-known context was **kernel** code (`true`) or a
    /// **user** task (`false`). Rendered as the `context` field so a report
    /// says at a glance whether a wedged CPU was last executing in the
    /// kernel (a held lock / in-flight syscall / masked in-kernel spin) or a
    /// user task — the single most decisive clue for the "why".
    in_kernel: bool,
    /// Whether the recorded sample is *stale* — the last one taken before
    /// the CPU went silent (a hard lockup), so pc/aux/context name the
    /// innocent code the CPU last returned to, not the wedge. Rendered as
    /// `sampled=pre_silence`. A soft lockup's sample is live, so this is
    /// `false` and no marker is rendered.
    sample_stale: bool,
    /// The device interrupt the **observer** read live as stuck in the
    /// shared controller (active over pending), or `None`. Rendered as
    /// `stuck_irq=<id>` plus `stuck_state=<active|pending>` — the "why" the
    /// stale sample cannot give. Only a line that could still be delivered
    /// is reported (active, or enabled-and-pending); a masked line cannot
    /// reach a CPU, so it is never blamed. Filled only on the hard-lockup
    /// path (the observer's cross-CPU read); a snapshot of the CPU's own
    /// context leaves it `None`.
    stuck: Option<StuckInterrupt>,
    /// Who owns the [`Self::stuck`] line, resolved against the kernel IRQ
    /// table on the hard-lockup path. It disambiguates the raw `stuck_irq`
    /// id — the recurring "which device is this line?" question — by naming
    /// the driver that bound it (`stuck_owner=<task>`), or reporting that no
    /// driver owns it (`stuck_owner=unbound`, a spurious/contained line, so
    /// the wedge is elsewhere). [`StuckOwner::Unknown`] (no stuck line, or no
    /// resolver installed) renders nothing, so a record never claims an
    /// attribution it does not have.
    stuck_owner: StuckOwner,
    /// The self-published kernel-activity breadcrumb the target CPU last
    /// recorded ([`KernelBreadcrumb`]). Unlike [`Self::pc`] this stays fresh
    /// through a hard lockup — the CPU wrote it on the way into the region
    /// it is now wedged in — so it names the real in-kernel activity even
    /// when the sampled context is `pre_silence`. Rendered as
    /// `k_site=<tag>`; [`KernelBreadcrumb::None`] renders nothing.
    ///
    /// The breadcrumb / backtrace fields below are part of the
    /// debug-diagnostics facility and exist only with the
    /// `watchdog-diagnostics` feature; they are rendered into the debug-only
    /// detail record, never the always-on summary.
    #[cfg(feature = "watchdog-diagnostics")]
    breadcrumb: KernelBreadcrumb,
    /// The datum for [`Self::breadcrumb`] (a syscall number or faulting
    /// address). Rendered as `k_detail=<hex>` for any site that carries
    /// one.
    #[cfg(feature = "watchdog-diagnostics")]
    breadcrumb_detail: u64,
    /// The breadcrumb sequence at snapshot time. Rendered as `k_seq=<n>`
    /// so two successive reports distinguish a frozen breadcrumb (the CPU
    /// is stuck in exactly this region) from an advancing one.
    #[cfg(feature = "watchdog-diagnostics")]
    breadcrumb_seq: u64,
    /// The pre-silence backtrace the port unwound from the last watchdog
    /// sample's interrupted context (innermost first). Only the first
    /// [`Self::bt_len`] entries are valid. Rendered as a single
    /// `k_bt=<pc0>,<pc1>,…` field so a hard lockup names the whole call
    /// nest the CPU was in ~1 s before it went silent, which the lone
    /// `pre_silence` `pc` cannot; a zero length renders nothing.
    #[cfg(feature = "watchdog-diagnostics")]
    bt: [u64; cpu_state::WD_BT_MAX],
    /// Number of valid frames in [`Self::bt`] (`0` = none captured).
    #[cfg(feature = "watchdog-diagnostics")]
    bt_len: usize,
    /// The innermost spinlock this CPU was holding or spinning to acquire
    /// when sampled, as a `&'static Location` (`usize`, `0` = none). On a
    /// GICv2 hard lockup the maskable liveness sample cannot observe a CPU
    /// wedged with interrupts off inside a spinlock section, so this
    /// self-published record names the exact lock — rendered
    /// `k_lock=<file>:<line>` from the acquiring call's source location,
    /// never a runtime address.
    #[cfg(feature = "watchdog-diagnostics")]
    lock_site: usize,
    /// Whether [`Self::lock_site`] was still being *acquired* (spinning,
    /// contended/deadlocked) rather than *held* (wedged inside its critical
    /// section). Rendered as the `k_lock_state` tag.
    #[cfg(feature = "watchdog-diagnostics")]
    lock_acquiring: bool,
    /// A **fresh** program counter of the hard-locked CPU, read by the
    /// observer over the port's non-maskable external-debug channel
    /// ([`WatchdogArch::remote_pc_sample`], aarch64 CoreSight `EDPCSR`).
    /// `None` when no such channel is wired/discovered for the target or the
    /// read produced no valid sample — the ordinary case, in which the
    /// report falls back to the stale [`Self::pc`]. Unlike that stale value
    /// this names the instruction the core is *actually* wedged on, so it is
    /// rendered as its own `live_pc=+0x…` field (image-relative) in the
    /// debug detail. Set on the hard-lockup path after the snapshot; the
    /// observer's cross-CPU read, not a self-sample.
    #[cfg(feature = "watchdog-diagnostics")]
    live_pc: Option<u64>,
    /// The port-defined context word sampled with [`Self::live_pc`]
    /// (aarch64 `EDVIDSR`: security state / exception level / mode), `0`
    /// when none. A register value, not an address, so it is rendered
    /// verbatim as `live_ctx`.
    #[cfg(feature = "watchdog-diagnostics")]
    live_context: u64,
    /// The interrupt the wedged CPU published as acknowledged-but-not-yet
    /// completed ([`WatchdogArch::in_flight_interrupt`]), rendered as
    /// `in_flight`. This is the only way a *banked* line shows up: the
    /// observer's [`Self::stuck`] read sees shared device lines alone, so a
    /// never-completed SGI or PPI leaves that scan to fall through to an
    /// innocent pending device line. An id here means the core is still
    /// inside that interrupt and owes it a completion. Set on the
    /// hard-lockup path after the snapshot.
    #[cfg(feature = "watchdog-diagnostics")]
    in_flight: InFlightInterrupt,
}

/// Attribution of a stuck controller line to the task that owns its IRQ
/// binding, resolved by the watchdog for a hard-lockup report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StuckOwner {
    /// No stuck line, or no owner resolver installed: nothing is rendered,
    /// so a record never claims an attribution it could not make.
    Unknown,
    /// The stuck line is bound to no task — a spurious/contained line no
    /// driver owns (rendered `stuck_owner=unbound`), which says the wedge is
    /// elsewhere, not this line.
    Unbound,
    /// The stuck line is bound to this task (rendered `stuck_owner=<hex>`) —
    /// the driver whose device is asserting it.
    Task(u64),
    /// The stuck line belongs to a kernel-internal source with no task
    /// binding (rendered `stuck_owner=<name>`) — a chained/bespoke line the
    /// kernel services itself (the platform MSI multiplexer, the console
    /// UART), named by the port that discovered it so a reader is not left
    /// with a bare `unbound` for a line the kernel does in fact own.
    Named(&'static str),
}

impl Diag {
    /// The [`Self::in_flight`] reading before the observer has taken one.
    /// Only the hard-lockup path reads it, so every other record carries
    /// this and renders no `in_flight` field at all rather than implying
    /// the CPU was inside no interrupt.
    #[cfg(feature = "watchdog-diagnostics")]
    const IN_FLIGHT_UNREAD: InFlightInterrupt = InFlightInterrupt::Unsupported("not read");

    /// An empty diagnosis (a recovery/clear record carries no context).
    const EMPTY: Self = Self {
        pc: 0,
        task: WatchdogSample::NO_TASK,
        #[cfg(feature = "watchdog-diagnostics")]
        aux: 0,
        in_kernel: false,
        sample_stale: false,
        stuck: None,
        stuck_owner: StuckOwner::Unknown,
        #[cfg(feature = "watchdog-diagnostics")]
        breadcrumb: KernelBreadcrumb::None,
        #[cfg(feature = "watchdog-diagnostics")]
        breadcrumb_detail: 0,
        #[cfg(feature = "watchdog-diagnostics")]
        breadcrumb_seq: 0,
        #[cfg(feature = "watchdog-diagnostics")]
        bt: [0; cpu_state::WD_BT_MAX],
        #[cfg(feature = "watchdog-diagnostics")]
        bt_len: 0,
        #[cfg(feature = "watchdog-diagnostics")]
        lock_site: 0,
        #[cfg(feature = "watchdog-diagnostics")]
        lock_acquiring: false,
        #[cfg(feature = "watchdog-diagnostics")]
        live_pc: None,
        #[cfg(feature = "watchdog-diagnostics")]
        live_context: 0,
        #[cfg(feature = "watchdog-diagnostics")]
        in_flight: Self::IN_FLIGHT_UNREAD,
    };

    /// Whether this diagnosis carries a real captured sample (as opposed to
    /// the empty recovery/clear record). Only then is the kernel/user
    /// `context` meaningful.
    fn has_sample(&self) -> bool {
        self.pc != 0 || self.task != WatchdogSample::NO_TASK
    }

    /// Read the innermost recorded lock site for `state` (the debug-only
    /// per-CPU lock-site stack), as `(site_ptr, acquiring)`. `(0, false)`
    /// when the CPU holds no recorded lock. The depth is loaded first
    /// (acquire) so the site read after it is the one the observer stored
    /// before its release bump; an index past the recorded cap is clamped
    /// to the deepest stored entry (a nesting that deep is pathological and
    /// the outer entries still name a real held lock).
    #[cfg(feature = "watchdog-diagnostics")]
    fn lock_snapshot(state: &CpuState) -> (usize, bool) {
        let depth = state.lock_depth.load(Ordering::Acquire);
        if depth == 0 {
            return (0, false);
        }
        let top = depth - 1;
        let idx = top.min(cpu_state::LOCK_STACK_MAX - 1);
        let site = state.lock_sites[idx].load(Ordering::Relaxed);
        // The acquiring flag names the true top; if that top is past the
        // recorded cap it was never stored, so the clamped entry we render
        // is an outer *held* lock, not the (unknown) acquiring one.
        let acquiring =
            top < cpu_state::LOCK_STACK_MAX && state.lock_top_acquiring.load(Ordering::Relaxed);
        (site, acquiring)
    }

    /// Read a CPU's recorded last-known context. The observer-supplied
    /// `sample_stale` / `stuck` / `stuck_owner` fields default off; the
    /// hard-lockup path sets them after the snapshot.
    fn snapshot(state: &CpuState) -> Self {
        // The breadcrumb + pre-silence backtrace are read only when the
        // debug-diagnostics facility is compiled in; a shippable build has
        // no such per-CPU storage to read.
        #[cfg(feature = "watchdog-diagnostics")]
        let (breadcrumb, breadcrumb_detail, breadcrumb_seq, bt, bt_len) = {
            // Read the breadcrumb sequence first (acquire) so the site and
            // detail loaded after it are the ones the writer stored before
            // its release bump — a consistent (site, detail, seq) triple.
            let breadcrumb_seq = state.kbc_seq.load(Ordering::Acquire);
            let breadcrumb = KernelBreadcrumb::from_u8(state.kbc_site.load(Ordering::Relaxed));
            let breadcrumb_detail = state.kbc_detail.load(Ordering::Relaxed);
            // Read the backtrace length first (acquire) so the frames loaded
            // after it are the ones the port stored before its release store
            // — a consistent set, never a torn mix of a new length with old
            // frames. A length past the array is clamped (fail closed).
            let bt_len =
                (state.wd_bt_len.load(Ordering::Acquire) as usize).min(cpu_state::WD_BT_MAX);
            let mut bt = [0u64; cpu_state::WD_BT_MAX];
            for (out, slot) in bt.iter_mut().zip(state.wd_bt.iter()).take(bt_len) {
                *out = slot.load(Ordering::Relaxed);
            }
            (breadcrumb, breadcrumb_detail, breadcrumb_seq, bt, bt_len)
        };
        #[cfg(feature = "watchdog-diagnostics")]
        let (lock_site, lock_acquiring) = Self::lock_snapshot(state);
        Self {
            pc: state.wd_ctx_pc.load(Ordering::Acquire),
            task: state.wd_ctx_task.load(Ordering::Acquire),
            #[cfg(feature = "watchdog-diagnostics")]
            aux: state.wd_ctx_aux.load(Ordering::Acquire),
            in_kernel: state.wd_ctx_in_kernel.load(Ordering::Acquire),
            sample_stale: false,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb_detail,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb_seq,
            #[cfg(feature = "watchdog-diagnostics")]
            bt,
            #[cfg(feature = "watchdog-diagnostics")]
            bt_len,
            #[cfg(feature = "watchdog-diagnostics")]
            lock_site,
            #[cfg(feature = "watchdog-diagnostics")]
            lock_acquiring,
            // A fresh cross-core sample is an observer action, not part of a
            // CPU's self-recorded context, so the snapshot leaves it empty;
            // the hard-lockup path fills it after this (like `stuck`).
            #[cfg(feature = "watchdog-diagnostics")]
            live_pc: None,
            #[cfg(feature = "watchdog-diagnostics")]
            live_context: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            in_flight: Self::IN_FLIGHT_UNREAD,
        }
    }
}

/// Attribute a stuck line to the driver that bound it (via the installed
/// owner resolver) or to a kernel-internal source (via the installed
/// name resolver). `None` (no stuck line) or an uninstalled owner resolver
/// yields [`StuckOwner::Unknown`] so nothing is rendered — a report never
/// claims an attribution it could not make. A bound line names its owning
/// task; an otherwise-unowned line the kernel services itself is named
/// ([`StuckOwner::Named`]); only a line neither owns is
/// [`StuckOwner::Unbound`] (a spurious/contained line, so the wedge is
/// elsewhere).
fn resolve_stuck_owner(stuck: Option<StuckInterrupt>) -> StuckOwner {
    resolve_stuck_owner_with(
        stuck,
        irq_owner().map(|r| r as &dyn StuckOwnerResolver),
        kernel_line_names().map(|r| r as &dyn KernelInternalLines),
    )
}

/// The pure core of [`resolve_stuck_owner`]: attribute `stuck` against the
/// given `resolver` and kernel-internal `names`, split out so the mapping is
/// host-tested with fakes rather than the process-global install seams.
///
/// A task binding wins: a line a driver bound is attributed to that task. A
/// line with no task binding is then offered to the kernel-internal name
/// resolver — so a chained/bespoke line the kernel owns (the platform MSI
/// multiplexer, the console UART) is *named* rather than dismissed as
/// `unbound`. Only a line neither a task nor the kernel owns is `Unbound`
/// (a genuinely spurious/contained line, so the wedge is elsewhere). An
/// uninstalled owner resolver still yields `Unknown` so a report never
/// claims an attribution it could not make.
fn resolve_stuck_owner_with(
    stuck: Option<StuckInterrupt>,
    resolver: Option<&dyn StuckOwnerResolver>,
    names: Option<&dyn KernelInternalLines>,
) -> StuckOwner {
    let Some(stuck) = stuck else {
        return StuckOwner::Unknown;
    };
    match resolver {
        None => StuckOwner::Unknown,
        Some(resolver) => match resolver.owner_of_line(stuck.intid) {
            Some(task) => StuckOwner::Task(task),
            None => match names.and_then(|n| n.name_of_line(stuck.intid)) {
                Some(name) => StuckOwner::Named(name),
                None => StuckOwner::Unbound,
            },
        },
    }
}

/// A short, stable tag for a stuck line's state: whether it is actively
/// storming (`active`) — a handler in flight, the signature of a live
/// wedge — or merely `pending` (enabled and asserted, but not yet taken).
/// Only deliverable lines are ever reported, so a masked line never
/// reaches this tag.
fn stuck_state_tag(stuck: StuckInterrupt) -> &'static str {
    if stuck.active {
        "active"
    } else {
        "pending"
    }
}

/// Render `value` into `buf` as `0x`-prefixed 16-nibble lowercase hex.
fn hex0x(value: u64, buf: &mut [u8; 18]) -> &str {
    buf[0] = b'0';
    buf[1] = b'x';
    let mut hex = [0u8; 16];
    let rendered = format_hex_u64(value, &mut hex);
    let bytes = rendered.as_bytes();
    buf[2..2 + bytes.len()].copy_from_slice(bytes);
    core::str::from_utf8(&buf[..2 + bytes.len()]).unwrap_or("0x")
}

/// Emit one lockup record.
///
/// Splits into two independent records, each allocation-free (stack buffers
/// only) and lock-free — safe on the non-maskable sample path and the
/// dispatch hot path alike:
///
/// * The **always-on summary** goes to the persistent audit [`report_sink`]
///   and carries only non-disclosing state (`cpu`, `observer`,
///   `stalled_ms`, `task`, `context`, `sampled`, `stuck_*`) — never a
///   kernel address, so the tamper-evident audit trail records *that* a
///   lockup happened and roughly where, with zero disclosure.
/// * The **debug-only detail** (a `watchdog-diagnostics` build only) goes to
///   the separate diagnostic `diag_sink` and carries the address-bearing
///   developer aids (`pc`/`pstate`/`k_site`/`k_detail`/`k_seq`/`k_bt`), with
///   every kernel address rendered image-base-relative (`+0x…`), never
///   absolute — the `%pK`-style discipline.
///
/// `observer` names the CPU that caught a cross-CPU lockup; `None` for a
/// same-CPU detection or a recovery/clear record.
fn report_lockup(
    event: AuditEvent,
    level: Level,
    cpu: CpuId,
    observer: Option<CpuId>,
    elapsed_ns: u64,
    diag: &Diag,
) {
    if let Some(sink) = report_sink() {
        report_summary_to(sink, event, level, cpu, observer, elapsed_ns, diag);
    }
    #[cfg(feature = "watchdog-diagnostics")]
    report_diagnostic_detail(level, cpu, observer, diag);
}

/// Render the always-on, non-disclosing lockup **summary** through `sink`.
/// Split out so host tests can drive it against a recording sink without
/// touching the process-wide install seam. Carries no kernel address: the
/// address-bearing detail is a separate diagnostic record
/// (`report_detail_to`, debug builds only).
fn report_summary_to(
    sink: &dyn Sink,
    event: AuditEvent,
    level: Level,
    cpu: CpuId,
    observer: Option<CpuId>,
    elapsed_ns: u64,
    diag: &Diag,
) {
    let mut owner_buf = [0u8; 18];

    // Build the field list on the stack. The order is stable so a reader
    // and a parser see the same shape every time.
    let mut fields: [tairix_log::Field<'_>; 9] = [tairix_log::Field {
        key: "cpu",
        value: tairix_log::FieldValue::UnsignedInt(u64::from(cpu)),
    }; 9];
    let mut n = 1;
    if let Some(obs) = observer {
        fields[n] = tairix_log::Field {
            key: "observer",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(obs)),
        };
        n += 1;
    }
    fields[n] = tairix_log::Field {
        key: "stalled_ms",
        value: tairix_log::FieldValue::UnsignedInt(elapsed_ns / NS_PER_MS),
    };
    n += 1;
    // The running-task id (not an address) names the culprit task where one
    // was captured.
    if diag.task != WatchdogSample::NO_TASK {
        fields[n] = tairix_log::Field {
            key: "task",
            value: tairix_log::FieldValue::UnsignedInt(diag.task),
        };
        n += 1;
    }
    // The kernel/user distinction is the single most decisive non-disclosing
    // clue for the "why", distilled from the sampled processor state (never
    // the raw state word, which is the debug detail's job).
    if diag.has_sample() {
        fields[n] = tairix_log::Field {
            key: "context",
            value: tairix_log::FieldValue::Str(if diag.in_kernel { "kernel" } else { "user" }),
        };
        n += 1;
        // A hard lockup's context is the last sample taken *before* the CPU
        // went silent (~`stalled_ms` old), not a live reading — mark it so
        // a reader does not mistake the innocent code the CPU last returned
        // to for the actual wedge.
        if diag.sample_stale {
            fields[n] = tairix_log::Field {
                key: "sampled",
                value: tairix_log::FieldValue::Str("pre_silence"),
            };
            n += 1;
        }
    }
    // The device interrupt currently stuck in the shared controller, read
    // live by the observer — the "why" the stale sample cannot give. Only
    // a line that can still reach a CPU is reported, so `stuck_state` just
    // says whether it is a live storm (`active`) or an enabled line
    // asserted but not yet taken (`pending`). The id and its owning task id
    // are diagnostic identifiers, not addresses.
    if let Some(stuck) = diag.stuck {
        fields[n] = tairix_log::Field {
            key: "stuck_irq",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(stuck.intid)),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "stuck_state",
            value: tairix_log::FieldValue::Str(stuck_state_tag(stuck)),
        };
        n += 1;
        // Who owns the stuck line — the driver whose device is asserting it
        // (`<task>`), or `unbound` for a spurious/contained line no driver
        // owns (so the wedge is elsewhere, not this line). Only rendered
        // when the owner was actually resolved, never a claim we cannot make.
        match diag.stuck_owner {
            StuckOwner::Task(task) => {
                fields[n] = tairix_log::Field {
                    key: "stuck_owner",
                    value: tairix_log::FieldValue::Str(hex0x(task, &mut owner_buf)),
                };
                n += 1;
            }
            StuckOwner::Named(name) => {
                fields[n] = tairix_log::Field {
                    key: "stuck_owner",
                    value: tairix_log::FieldValue::Str(name),
                };
                n += 1;
            }
            StuckOwner::Unbound => {
                fields[n] = tairix_log::Field {
                    key: "stuck_owner",
                    value: tairix_log::FieldValue::Str("unbound"),
                };
                n += 1;
            }
            StuckOwner::Unknown => {}
        }
    }
    emit(sink, level, event, &fields[..n]);
}

// --- Debug-only address-bearing detail ------------------------------

/// Emit the debug-only lockup **detail** record through the diagnostic
/// sink, if there is anything address-bearing to say and a diagnostic sink
/// is installed. A recovery/clear record (empty diag) has no detail and is
/// skipped, so the diagnostic stream carries a detail line only for an
/// actual detection. Compiled in only with the debug-diagnostics facility.
#[cfg(feature = "watchdog-diagnostics")]
fn report_diagnostic_detail(level: Level, cpu: CpuId, observer: Option<CpuId>, diag: &Diag) {
    let has_detail = diag.pc != 0
        || diag.breadcrumb != KernelBreadcrumb::None
        || diag.bt_len != 0
        || diag.lock_site != 0
        || diag.live_pc.is_some()
        || in_flight_field(diag.in_flight).is_some();
    if !has_detail {
        return;
    }
    if let Some(sink) = diag_sink() {
        report_detail_to(sink, level, cpu, observer, diag);
    }
}

/// The `in_flight` field value for a published reading, or `None` when the
/// port published none.
///
/// An unread/unsupported reading renders nothing at all: a record must
/// never let "the port cannot tell us" read as "the core is inside no
/// interrupt". Both other readings are load-bearing — `none` clears a
/// missed completion as the cause, an id names the interrupt the core
/// still owes a completion to.
#[cfg(feature = "watchdog-diagnostics")]
fn in_flight_field(in_flight: InFlightInterrupt) -> Option<tairix_log::FieldValue<'static>> {
    match in_flight {
        InFlightInterrupt::Idle => Some(tairix_log::FieldValue::Str("none")),
        InFlightInterrupt::Acknowledged { intid } => {
            Some(tairix_log::FieldValue::UnsignedInt(u64::from(intid)))
        }
        InFlightInterrupt::Unsupported(_) => None,
    }
}

/// Render `offset` into `buf` as the image-relative marker `+0x`-prefixed
/// 16-nibble lowercase hex. The leading `+` makes unmistakable that the
/// value is an offset from the kernel image base, never an absolute
/// runtime address, so a reader can never confuse the two.
#[cfg(feature = "watchdog-diagnostics")]
fn hex_off(offset: u64, buf: &mut [u8; 19]) -> &str {
    buf[0] = b'+';
    buf[1] = b'0';
    buf[2] = b'x';
    let mut hex = [0u8; 16];
    let rendered = format_hex_u64(offset, &mut hex);
    let bytes = rendered.as_bytes();
    buf[3..3 + bytes.len()].copy_from_slice(bytes);
    core::str::from_utf8(&buf[..3 + bytes.len()]).unwrap_or("+0x")
}

/// Render the debug-only lockup **detail** through `sink`: the
/// address-bearing developer aids the always-on summary deliberately
/// omits. Every kernel address is rendered image-base-relative (`+0x…`)
/// via [`image_relative`] — never absolute — and a kernel-address field is
/// omitted entirely when the base is unregistered (fail closed, never a
/// raw disclosure). Split out so host tests drive it against a recording
/// sink without the install seam.
#[cfg(feature = "watchdog-diagnostics")]
fn report_detail_to(
    sink: &dyn Sink,
    level: Level,
    cpu: CpuId,
    observer: Option<CpuId>,
    diag: &Diag,
) {
    let mut pc_buf = [0u8; 19];
    let mut live_pc_buf = [0u8; 19];
    let mut live_ctx_buf = [0u8; 18];
    let mut aux_buf = [0u8; 18];
    let mut kdetail_buf = [0u8; 18];
    let mut bt_buf = [0u8; BT_RENDER_BYTES];

    let mut fields: [tairix_log::Field<'_>; 15] = [tairix_log::Field {
        key: "cpu",
        value: tairix_log::FieldValue::UnsignedInt(u64::from(cpu)),
    }; 15];
    let mut n = 1;
    // `observer` correlates this detail line with its summary line on the
    // audit trail (both carry the same `cpu`/`observer`).
    if let Some(obs) = observer {
        fields[n] = tairix_log::Field {
            key: "observer",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(obs)),
        };
        n += 1;
    }
    // The sampled program counter, image-relative — omitted when the base
    // is unregistered or the pc is not a kernel-image address (fail closed,
    // never a raw absolute).
    if let Some(off) = image_relative(diag.pc) {
        fields[n] = tairix_log::Field {
            key: "pc",
            value: tairix_log::FieldValue::Str(hex_off(off, &mut pc_buf)),
        };
        n += 1;
    }
    // The **fresh** cross-core PC the observer read over the port's
    // non-maskable external-debug channel (CoreSight `EDPCSR`) — the
    // instruction the wedged core is *actually* on, unlike the stale `pc`
    // above. Image-relative like `pc` (a kernel wedge resolves against the
    // debug ELF; a user-EL sample is not a kernel-image address and is
    // omitted, fail closed). `live_ctx` is the sampled context register
    // (aarch64 `EDVIDSR`), a value not an address, rendered verbatim.
    if let Some(live) = diag.live_pc {
        if let Some(off) = image_relative(live) {
            fields[n] = tairix_log::Field {
                key: "live_pc",
                value: tairix_log::FieldValue::Str(hex_off(off, &mut live_pc_buf)),
            };
            n += 1;
        }
        if diag.live_context != 0 {
            fields[n] = tairix_log::Field {
                key: "live_ctx",
                value: tairix_log::FieldValue::Str(hex0x(diag.live_context, &mut live_ctx_buf)),
            };
            n += 1;
        }
    }
    // What the wedged core published as acknowledged but not completed —
    // an interrupt id, not an address. `none` is as load-bearing as an id:
    // it clears a missed completion as the cause, whereas an id names the
    // interrupt the core is still inside, *including* a banked SGI or PPI
    // the observer's `stuck_irq` scan is structurally blind to (it reads
    // shared device lines only and falls through to the first pending one).
    if let Some(value) = in_flight_field(diag.in_flight) {
        fields[n] = tairix_log::Field {
            key: "in_flight",
            value,
        };
        n += 1;
    }
    // The raw processor-state word (aarch64 `SPSR_EL1`): a register value,
    // not an address, so it is rendered verbatim.
    if diag.aux != 0 {
        fields[n] = tairix_log::Field {
            key: "pstate",
            value: tairix_log::FieldValue::Str(hex0x(diag.aux, &mut aux_buf)),
        };
        n += 1;
    }
    // The self-published kernel-activity breadcrumb — the region the CPU
    // last entered, fresh through a hard lockup where the sampled `pc` is
    // `pre_silence`. `k_detail` is a syscall number, a faulting virtual
    // address, or a task id — never a kernel-image address, so it is
    // rendered verbatim (not rebased).
    if diag.breadcrumb != KernelBreadcrumb::None {
        fields[n] = tairix_log::Field {
            key: "k_site",
            value: tairix_log::FieldValue::Str(diag.breadcrumb.tag()),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "k_detail",
            value: tairix_log::FieldValue::Str(hex0x(diag.breadcrumb_detail, &mut kdetail_buf)),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "k_seq",
            value: tairix_log::FieldValue::UnsignedInt(diag.breadcrumb_seq),
        };
        n += 1;
    }
    // The exact spinlock the CPU was on when sampled — recorded by the
    // lock observer as the acquiring call's source `file:line`. A source
    // string, never a runtime address, so it discloses no load base. On a
    // GICv2 hard lockup this names the IRQ-masked culprit lock the maskable
    // liveness sample cannot observe: `acquiring` = still spinning to take
    // it (contended/deadlocked), `held` = wedged inside its section.
    if diag.lock_site != 0 {
        // SAFETY: a non-zero `lock_site` is the `&'static Location` the
        // `tairix_sync` lock observer stored from `Location::caller()`; the
        // pointee is `'static` rodata, valid for the whole run, so forming
        // the reference and reading `file`/`line` is sound.
        let loc: &'static core::panic::Location<'static> =
            unsafe { &*(diag.lock_site as *const core::panic::Location<'static>) };
        fields[n] = tairix_log::Field {
            key: "k_lock",
            value: tairix_log::FieldValue::Str(loc.file()),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "k_lock_line",
            value: tairix_log::FieldValue::UnsignedInt(u64::from(loc.line())),
        };
        n += 1;
        fields[n] = tairix_log::Field {
            key: "k_lock_state",
            value: tairix_log::FieldValue::Str(if diag.lock_acquiring {
                "acquiring"
            } else {
                "held"
            }),
        };
        n += 1;
    }
    // The pre-silence backtrace, image-relative frames — the whole call nest
    // the CPU was in ~1 s before it went silent, which the lone `pre_silence`
    // `pc` cannot give.
    let n = push_backtrace_field(&mut fields, n, diag, &mut bt_buf);
    emit(sink, level, AuditEvent::CpuLockupDiagnostic, &fields[..n]);
}

/// Bytes needed to render the deepest backtrace as `+0x<16>,+0x<16>,…`:
/// each of [`cpu_state::WD_BT_MAX`] frames is `+0x` + 16 nibbles (19) plus a
/// separating comma (20), and the trailing comma is simply not written.
#[cfg(feature = "watchdog-diagnostics")]
const BT_RENDER_BYTES: usize = cpu_state::WD_BT_MAX * 20;

/// Append the `k_bt=+<off0>,+<off1>,…` pre-silence-backtrace field to
/// `fields` at index `n`, returning the new length. Each frame is rendered
/// image-relative (`+0x…`, [`image_relative`]); a frame that does not
/// resolve against the kernel image base is skipped rather than disclosed
/// raw. A backtrace that ends up with no renderable frame (none captured,
/// or the base is unregistered) adds nothing, so a record never fabricates
/// a stack and never leaks an absolute address. `buf` backs the rendered
/// joined string, so it must outlive the returned fields.
#[cfg(feature = "watchdog-diagnostics")]
fn push_backtrace_field<'a>(
    fields: &mut [tairix_log::Field<'a>],
    n: usize,
    diag: &Diag,
    buf: &'a mut [u8; BT_RENDER_BYTES],
) -> usize {
    if diag.bt_len == 0 {
        return n;
    }
    let mut used = 0;
    let mut rendered_any = false;
    for &pc in diag.bt.iter().take(diag.bt_len) {
        let Some(off) = image_relative(pc) else {
            continue;
        };
        if rendered_any {
            buf[used] = b',';
            used += 1;
        }
        let mut one = [0u8; 19];
        let text = hex_off(off, &mut one);
        let bytes = text.as_bytes();
        buf[used..used + bytes.len()].copy_from_slice(bytes);
        used += bytes.len();
        rendered_any = true;
    }
    if !rendered_any {
        return n;
    }
    let text = core::str::from_utf8(&buf[..used]).unwrap_or("");
    let mut n = n;
    fields[n] = tairix_log::Field {
        key: "k_bt",
        value: tairix_log::FieldValue::Str(text),
    };
    n += 1;
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;

    /// Reset a shared per-CPU test slot to the pristine state so each test
    /// starts clean. Tests use disjoint CPU indices (like the sibling
    /// `preempt` tests) so parallel threads never collide on a slot.
    fn reset(cpu: CpuId) -> &'static CpuState {
        let state = cpu_state::get(cpu).expect("test CPU slot exists");
        state.last_progress_ns.store(0, Ordering::Relaxed);
        state.stall_reported.store(false, Ordering::Relaxed);
        state.last_seen_ns.store(0, Ordering::Relaxed);
        state
            .wd_activity
            .store(WatchdogActivity::Offline as u8, Ordering::Relaxed);
        state.hard_reported.store(false, Ordering::Relaxed);
        state.wd_ctx_pc.store(0, Ordering::Relaxed);
        state.wd_ctx_task.store(u64::MAX, Ordering::Relaxed);
        state.wd_ctx_aux.store(0, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(false, Ordering::Relaxed);
        #[cfg(feature = "watchdog-diagnostics")]
        {
            state.kbc_site.store(0, Ordering::Relaxed);
            state.kbc_detail.store(0, Ordering::Relaxed);
            state.kbc_seq.store(0, Ordering::Relaxed);
            state.wd_bt_len.store(0, Ordering::Relaxed);
            for slot in &state.wd_bt {
                slot.store(0, Ordering::Relaxed);
            }
            state.lock_depth.store(0, Ordering::Relaxed);
            state.lock_top_acquiring.store(false, Ordering::Relaxed);
            for slot in &state.lock_sites {
                slot.store(0, Ordering::Relaxed);
            }
        }
        state
    }

    fn field<'a>(ev: &'a crate::test_sink::CapturedEvent, key: &str) -> Option<&'a str> {
        ev.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Build a [`Diag`] for a render test from the always-on fields; the
    /// debug-only breadcrumb / backtrace / `aux` fields are defaulted here
    /// (a test that exercises them constructs a full literal under the
    /// feature). This lets the always-on summary tests build a `Diag` in
    /// both feature configurations without listing fields that do not exist
    /// in a shippable build.
    fn diag(
        pc: u64,
        task: u64,
        in_kernel: bool,
        sample_stale: bool,
        stuck: Option<StuckInterrupt>,
        stuck_owner: StuckOwner,
    ) -> Diag {
        Diag {
            pc,
            task,
            in_kernel,
            sample_stale,
            stuck,
            stuck_owner,
            #[cfg(feature = "watchdog-diagnostics")]
            aux: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb: KernelBreadcrumb::None,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb_detail: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            breadcrumb_seq: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            bt: [0; cpu_state::WD_BT_MAX],
            #[cfg(feature = "watchdog-diagnostics")]
            bt_len: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            lock_site: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            lock_acquiring: false,
            #[cfg(feature = "watchdog-diagnostics")]
            live_pc: None,
            #[cfg(feature = "watchdog-diagnostics")]
            live_context: 0,
            #[cfg(feature = "watchdog-diagnostics")]
            in_flight: Diag::IN_FLIGHT_UNREAD,
        }
    }

    /// True iff no rendered field value contains `needle` — a regression
    /// guard that a raw absolute address never leaks into a record.
    fn no_field_contains(ev: &crate::test_sink::CapturedEvent, needle: &str) -> bool {
        ev.fields.iter().all(|(_, v)| !v.contains(needle))
    }

    // --- The pure heartbeat evaluator -------------------------------

    #[test]
    fn an_unarmed_heartbeat_is_never_judged() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(0, &latch, 1_000_000_000, 10), Sample::Unarmed);
    }

    #[test]
    fn recent_progress_is_healthy_and_the_boundary_is_inclusive() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(100, &latch, 109, 10), Sample::Healthy);
        assert_eq!(evaluate(100, &latch, 110, 10), Sample::Onset(10));
    }

    #[test]
    fn crossing_the_threshold_reports_the_episode_once() {
        let latch = AtomicBool::new(false);
        assert_eq!(evaluate(1_000, &latch, 1_010, 10), Sample::Onset(10));
        assert_eq!(evaluate(1_000, &latch, 1_050, 10), Sample::Still);
        assert_eq!(evaluate(1_000, &latch, 1_999, 10), Sample::Still);
    }

    // --- Progress heartbeat + soft recovery -------------------------

    #[test]
    fn progress_after_a_reported_soft_lockup_reports_recovery_once() {
        let state = reset(40);
        state.last_progress_ns.store(100, Ordering::Relaxed);
        assert_eq!(
            evaluate(100, &state.stall_reported, 200, 10),
            Sample::Onset(100)
        );
        assert_eq!(record_progress(state, 250), Some(150));
        assert_eq!(record_progress(state, 260), None);
    }

    #[test]
    fn progress_without_a_reported_soft_lockup_is_silent() {
        let state = reset(41);
        assert_eq!(record_progress(state, 100), None);
    }

    #[test]
    fn a_stamped_heartbeat_is_never_the_unarmed_sentinel() {
        let state = reset(42);
        record_progress(state, 0);
        assert_ne!(state.last_progress_ns.load(Ordering::Relaxed), 0);
    }

    // --- Liveness heartbeat + context capture + hard recovery -------

    #[test]
    fn liveness_records_context_and_reports_hard_recovery_once() {
        let state = reset(43);
        let sample = WatchdogSample {
            pc: 0xdead_beef,
            task: 7,
            aux: 0x3c5,
            in_kernel: true,
        };
        // No prior hard report: healthy path, context captured.
        assert_eq!(record_liveness(state, 1_000, &sample), None);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0xdead_beef);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 7);
        assert_eq!(state.wd_ctx_aux.load(Ordering::Relaxed), 0x3c5);
        assert!(state.wd_ctx_in_kernel.load(Ordering::Relaxed));
        // A latched hard episode: the next liveness clears it and reports
        // the recovery gap once.
        state.hard_reported.store(true, Ordering::Relaxed);
        assert_eq!(record_liveness(state, 1_500, &sample), Some(500));
        assert_eq!(record_liveness(state, 1_600, &sample), None);
    }

    #[test]
    fn alive_refreshes_liveness_without_touching_context() {
        let state = reset(35);
        // A stale liveness heartbeat and a captured context from an earlier
        // real sample.
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.wd_ctx_pc.store(0xabcd, Ordering::Relaxed);
        state.wd_ctx_task.store(3, Ordering::Relaxed);
        state.wd_ctx_aux.store(0x345, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Proof-of-life from the dispatcher: liveness advances, context is
        // left exactly as the last real sample recorded it.
        assert_eq!(record_alive(state, 5_000), None);
        assert_eq!(state.last_seen_ns.load(Ordering::Relaxed), 5_000);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0xabcd);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 3);
        assert_eq!(state.wd_ctx_aux.load(Ordering::Relaxed), 0x345);
        assert!(state.wd_ctx_in_kernel.load(Ordering::Relaxed));
    }

    #[test]
    fn alive_after_a_reported_hard_lockup_reports_recovery_once() {
        let state = reset(36);
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.hard_reported.store(true, Ordering::Relaxed);
        // Reaching the dispatcher clears a latched hard episode and reports
        // the recovery gap exactly once.
        assert_eq!(record_alive(state, 3_000), Some(2_000));
        assert_eq!(record_alive(state, 3_100), None);
    }

    #[test]
    fn a_stamped_liveness_heartbeat_is_never_the_unarmed_sentinel() {
        let state = reset(37);
        record_alive(state, 0);
        assert_ne!(state.last_seen_ns.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_cpu_returning_from_a_long_idle_park_is_not_hard_locked() {
        // The regression: a CPU idle-parked far longer than the hard
        // threshold (its non-maskable sample is not taken while parked)
        // carries a stale liveness heartbeat. When it wakes it must stamp
        // liveness *before* it republishes Active — exactly the order the
        // dispatch loop uses (`note_alive` then `set_activity(Active)`) — so
        // the cross-CPU scan never sees it Active with the pre-park
        // heartbeat and never reports a false hard lockup.
        let state = reset(39);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Last real sample was taken before a long idle park.
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + 20 * DEFAULT_HARD_LOCKUP_THRESHOLD_NS;
        // The dispatch loop's proof-of-life stamps at the top, before Active.
        record_alive(state, now);
        record_progress(state, now);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
        // And without the fix — Active republished over the pre-park
        // heartbeat — the very same scan would have fired.
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert!(matches!(classify(state, now), Verdict::HardOnset(_)));
    }

    // --- Classification --------------------------------------------

    #[test]
    fn a_non_active_cpu_is_never_classified() {
        let state = reset(44);
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.last_progress_ns.store(1, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Idle and Offline: quiet even with ancient heartbeats.
        for activity in [WatchdogActivity::Idle, WatchdogActivity::Offline] {
            state.wd_activity.store(activity as u8, Ordering::Relaxed);
            assert_eq!(classify(state, 1_000_000_000_000), Verdict::Quiet);
        }
    }

    #[test]
    fn an_active_cpu_that_stops_taking_the_sample_is_hard_locked() {
        let state = reset(45);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_HARD_LOCKUP_THRESHOLD_NS;
        assert_eq!(
            classify(state, now),
            Verdict::HardOnset(DEFAULT_HARD_LOCKUP_THRESHOLD_NS)
        );
        // Latched: a later scan of the same wedged CPU stays quiet.
        assert_eq!(classify(state, now + 1_000), Verdict::Quiet);
    }

    #[test]
    fn hard_lockup_takes_precedence_over_soft() {
        let state = reset(46);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Both heartbeats are ancient, but liveness (hard) wins.
        state.last_seen_ns.store(1, Ordering::Relaxed);
        state.last_progress_ns.store(1, Ordering::Relaxed);
        let now = 1 + DEFAULT_HARD_LOCKUP_THRESHOLD_NS + 5;
        assert!(matches!(classify(state, now), Verdict::HardOnset(_)));
        // The soft latch was never taken, so a subsequent same-CPU
        // `check_stall` could still report the soft condition if the CPU
        // somehow resumed taking samples but not dispatching.
        assert!(!state.stall_reported.load(Ordering::Relaxed));
    }

    #[test]
    fn an_active_in_kernel_cpu_that_stops_dispatching_is_soft_locked() {
        let state = reset(47);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_SOFT_LOCKUP_THRESHOLD_NS;
        // Liveness fresh (not hard), progress ancient.
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert_eq!(
            classify(state, now),
            Verdict::SoftOnset(DEFAULT_SOFT_LOCKUP_THRESHOLD_NS)
        );
    }

    #[test]
    fn a_lone_user_task_is_never_soft_locked() {
        let state = reset(48);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        // Last seen in *user* mode: no scheduler progress is owed.
        state.wd_ctx_in_kernel.store(false, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_SOFT_LOCKUP_THRESHOLD_NS;
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(1_000, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
    }

    #[test]
    fn a_healthy_active_cpu_is_quiet() {
        let state = reset(49);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        let now = 1_000_000;
        state.last_seen_ns.store(now, Ordering::Relaxed);
        state.last_progress_ns.store(now, Ordering::Relaxed);
        assert_eq!(classify(state, now), Verdict::Quiet);
    }

    // --- Public entry points fail closed ----------------------------

    #[test]
    fn public_entry_points_fail_closed_for_an_out_of_range_cpu() {
        note_progress(u32::MAX, 1_000);
        check_stall(u32::MAX);
        set_activity(u32::MAX, WatchdogActivity::Active);
        // An out-of-range sampling CPU still runs the scan harmlessly.
        on_watchdog_tick(u32::MAX, 1_000, &WatchdogSample::EMPTY);
    }

    #[test]
    fn on_watchdog_tick_stamps_liveness_and_context() {
        let state = reset(38);
        let sample = WatchdogSample {
            pc: 0x1234,
            task: 9,
            aux: 0x2c0,
            in_kernel: true,
        };
        on_watchdog_tick(38, 5_000, &sample);
        assert_ne!(state.last_seen_ns.load(Ordering::Relaxed), 0);
        assert_eq!(state.wd_ctx_pc.load(Ordering::Relaxed), 0x1234);
        assert_eq!(state.wd_ctx_task.load(Ordering::Relaxed), 9);
    }

    // --- Monopoly guard (a lone CPU-bound user task) ----------------

    /// Arm `state` as an Active CPU whose scheduler progress was last
    /// stamped at `progress_ns`.
    fn active_with_progress(cpu: CpuId, progress_ns: u64) -> &'static CpuState {
        let state = reset(cpu);
        state
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        state.last_progress_ns.store(progress_ns, Ordering::Relaxed);
        state
    }

    #[test]
    fn a_lone_user_task_past_the_guard_monopolises() {
        let state = active_with_progress(50, 1_000);
        let now = 1_000 + DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        assert!(monopolises_cpu(state, now, false));
        // Boundary is inclusive; a hair under is not yet a monopoly.
        assert!(!monopolises_cpu(state, now - 1, false));
    }

    #[test]
    fn kernel_code_is_never_force_yielded() {
        let state = active_with_progress(51, 1_000);
        let now = 1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        // The kernel is non-preemptible: a CPU sampled in kernel code is
        // never a monopoly candidate, however long since progress.
        assert!(!monopolises_cpu(state, now, true));
    }

    #[test]
    fn a_non_active_or_unarmed_cpu_never_monopolises() {
        // Not Active (parked/offline): owes no progress, never a monopoly.
        let idle = reset(52);
        idle.last_progress_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        assert!(!monopolises_cpu(idle, now, false));
        // Active but progress never armed (0): fail closed, no phantom yield.
        let fresh = reset(53);
        fresh
            .wd_activity
            .store(WatchdogActivity::Active as u8, Ordering::Relaxed);
        assert!(!monopolises_cpu(fresh, now, false));
    }

    #[test]
    fn on_watchdog_tick_requests_a_forced_yield_for_a_monopolising_user_cpu() {
        let state = active_with_progress(54, 1_000);
        state.force_yield.store(false, Ordering::Relaxed);
        let sample = WatchdogSample {
            pc: 0x4000_0000,
            task: 21,
            aux: 0x6000_0000,
            in_kernel: false,
        };
        on_watchdog_tick(54, 1_000 + DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS, &sample);
        assert!(state.force_yield.load(Ordering::Relaxed));
    }

    /// The tick-driven guard reads no sampled context, so a CPU whose
    /// recorded kernel/user field has rotted at `true` — the state of a core
    /// whose cadence stopped mid-dispatch — is still force-yielded. Pinning
    /// this is the point: gating on that field is what let a wedged core
    /// monopolise itself unopposed.
    #[test]
    fn a_stale_in_kernel_reading_does_not_suppress_the_tick_guard() {
        let state = active_with_progress(24, 1_000);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS;
        assert!(progress_overdue(state, now));
        // The cadence caller, which holds a *fresh* reading, still defers to
        // it: the kernel is non-preemptible.
        assert!(!monopolises_cpu(state, now, true));
    }

    /// A context no cadence sample has refreshed within the cadence interval
    /// is stale, and a never-sampled one is stale too (nothing vouches for
    /// it). A sample within the interval is not.
    #[test]
    fn a_context_older_than_the_cadence_is_stale() {
        let cadence = tairix_arch_api::WATCHDOG_CADENCE_NS;
        let state = reset(25);
        assert!(context_stale(state, 1_000));
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        assert!(!context_stale(state, 1_000 + cadence));
        assert!(context_stale(state, 1_000 + cadence + 1));
    }

    /// The same-CPU stall report renders a rotted context as `pre_silence`.
    /// It used to print a confident `context=kernel` from a field ten
    /// seconds out of date, which read as evidence the CPU was wedged in the
    /// kernel when it was only running the code it last returned to.
    #[test]
    fn the_same_cpu_stall_report_marks_a_rotted_context_stale() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let state = active_with_progress(26, 1_000);
        state.wd_ctx_pc.store(0x4000_0000, Ordering::Relaxed);
        state.wd_ctx_task.store(9, Ordering::Relaxed);
        state.wd_ctx_in_kernel.store(true, Ordering::Relaxed);
        // Last sampled at the same instant progress stopped, so by the time
        // the soft threshold elapses the context is ten seconds old.
        state.last_seen_ns.store(1_000, Ordering::Relaxed);
        let now = 1_000 + DEFAULT_SOFT_LOCKUP_THRESHOLD_NS;
        let mut d = Diag::snapshot(state);
        d.sample_stale = context_stale(state, now);
        report_summary_to(
            sink,
            AuditEvent::CpuStallDetected,
            Level::Error,
            26,
            None,
            DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
            &d,
        );
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(field(ev, "context"), Some("kernel"));
        assert_eq!(field(ev, "sampled"), Some("pre_silence"));
        // No observer: the field's absence is what identifies this as the
        // victim's own tick rather than a cross-CPU scan.
        assert_eq!(field(ev, "observer"), None);
    }

    #[test]
    fn on_watchdog_tick_does_not_force_yield_a_kernel_or_recent_cpu() {
        // Sampled in the kernel: never force-yielded.
        let in_kernel = active_with_progress(55, 1_000);
        in_kernel.force_yield.store(false, Ordering::Relaxed);
        let ksample = WatchdogSample {
            pc: 0x1000,
            task: WatchdogSample::NO_TASK,
            aux: 0x3c5,
            in_kernel: true,
        };
        on_watchdog_tick(
            55,
            1_000 + 10 * DEFAULT_MONOPOLY_YIELD_THRESHOLD_NS,
            &ksample,
        );
        assert!(!in_kernel.force_yield.load(Ordering::Relaxed));
        // A user task that returned to the scheduler recently: not a
        // monopoly, so no forced yield.
        let recent = active_with_progress(56, 1_000);
        recent.force_yield.store(false, Ordering::Relaxed);
        let usample = WatchdogSample {
            pc: 0x4000_0000,
            task: 22,
            aux: 0x6000_0000,
            in_kernel: false,
        };
        on_watchdog_tick(56, 1_500, &usample);
        assert!(!recent.force_yield.load(Ordering::Relaxed));
    }

    // --- Diagnostics rendering --------------------------------------

    #[test]
    fn hex0x_prefixes_and_pads_to_sixteen_nibbles() {
        let mut buf = [0u8; 18];
        assert_eq!(hex0x(0xabc, &mut buf), "0x0000000000000abc");
    }

    #[test]
    fn the_always_on_summary_carries_state_but_never_a_kernel_address() {
        // The always-on summary (audit trail) records *that* a hard lockup
        // happened and roughly where, with zero address disclosure: a bare
        // pc, the raw processor state, and every `k_*` breadcrumb/backtrace
        // field are the debug-only detail's job and never appear here, so
        // the persistent hash-chained log never carries a KASLR-defeating
        // raw kernel pointer.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = diag(
            0xffff_0000_1234_5678,
            12,
            true,
            true,
            Some(StuckInterrupt {
                intid: 37,
                active: true,
            }),
            StuckOwner::Task(13),
        );
        report_summary_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            2,
            Some(0),
            7_000_000_000,
            &d,
        );
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuHardLockupDetected.id());
        assert_eq!(ev.level, Level::Error);
        assert_eq!(field(ev, "cpu"), Some("2"));
        assert_eq!(field(ev, "observer"), Some("0"));
        assert_eq!(field(ev, "stalled_ms"), Some("7000"));
        // A task id (not an address) and the kernel/user context — the
        // decisive non-disclosing clues.
        assert_eq!(field(ev, "task"), Some("12"));
        assert_eq!(field(ev, "context"), Some("kernel"));
        assert_eq!(field(ev, "sampled"), Some("pre_silence"));
        assert_eq!(field(ev, "stuck_irq"), Some("37"));
        assert_eq!(field(ev, "stuck_state"), Some("active"));
        assert_eq!(field(ev, "stuck_owner"), Some("0x000000000000000d"));
        // No kernel address, ever, on the always-on record.
        assert_eq!(field(ev, "pc"), None);
        assert_eq!(field(ev, "pstate"), None);
        assert_eq!(field(ev, "k_site"), None);
        assert_eq!(field(ev, "k_detail"), None);
        assert_eq!(field(ev, "k_seq"), None);
        assert_eq!(field(ev, "k_bt"), None);
        // Regression guard: the raw sampled pc never appears in *any*
        // rendered field of the summary record.
        assert!(no_field_contains(ev, "ffff000012345678"));
    }

    /// The kernel image base the debug-detail tests rebase against. All such
    /// tests set this same value, so the process-global base store never
    /// races to a *different* value between parallel test threads.
    #[cfg(feature = "watchdog-diagnostics")]
    const TEST_IMAGE_BASE: u64 = 0xffff_0000_0000_0000;

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_debug_detail_renders_addresses_image_relative_never_absolute() {
        // With the facility on and the image base registered, the debug
        // detail record carries the developer aids the summary omits — but
        // every kernel address is an image-relative offset (`+0x…`), never
        // the absolute runtime pc, so a capture resolves against the debug
        // ELF without disclosing the (KASLR-relocatable) load base.
        set_kernel_image_base(TEST_IMAGE_BASE);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let mut bt = [0u64; cpu_state::WD_BT_MAX];
        bt[0] = TEST_IMAGE_BASE + 0x0021_d42c;
        bt[1] = TEST_IMAGE_BASE + 0x0021_e100;
        bt[2] = TEST_IMAGE_BASE + 0x0022_0f00;
        let d = Diag {
            pc: TEST_IMAGE_BASE + 0x1234_5678,
            task: 12,
            aux: 0x3c5,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            breadcrumb: KernelBreadcrumb::FaultAnon,
            breadcrumb_detail: 0x1_0000_2000,
            breadcrumb_seq: 42,
            bt,
            bt_len: 3,
            lock_site: 0,
            lock_acquiring: false,
            live_pc: Some(TEST_IMAGE_BASE + 0x0018_1dc0),
            live_context: 0x0000_2000,
            in_flight: Diag::IN_FLIGHT_UNREAD,
        };
        report_detail_to(sink, Level::Error, 2, Some(0), &d);
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuLockupDiagnostic.id());
        assert_eq!(field(ev, "cpu"), Some("2"));
        assert_eq!(field(ev, "observer"), Some("0"));
        // The pc renders as an image-relative offset with the unmistakable
        // `+` marker — never the absolute runtime address.
        assert_eq!(field(ev, "pc"), Some("+0x0000000012345678"));
        // The fresh cross-core sample renders as its own image-relative
        // `live_pc` — the instruction the wedged core is actually on — with
        // the sampled context word verbatim, alongside (never replacing) the
        // stale `pc`.
        assert_eq!(field(ev, "live_pc"), Some("+0x0000000000181dc0"));
        assert_eq!(field(ev, "live_ctx"), Some("0x0000000000002000"));
        assert_eq!(field(ev, "pstate"), Some("0x00000000000003c5"));
        assert_eq!(field(ev, "k_site"), Some("fault_anon"));
        // `k_detail` is a faulting VA (not a kernel-image address), rendered
        // verbatim.
        assert_eq!(field(ev, "k_detail"), Some("0x0000000100002000"));
        assert_eq!(field(ev, "k_seq"), Some("42"));
        // Each backtrace frame is likewise image-relative.
        assert_eq!(
            field(ev, "k_bt"),
            Some("+0x000000000021d42c,+0x000000000021e100,+0x0000000000220f00"),
        );
        // The decisive guarantee: the absolute runtime pc / frame addresses
        // never appear in the record, only their offsets.
        assert!(no_field_contains(ev, "ffff00001234"));
        assert!(no_field_contains(ev, "ffff0000002"));
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_debug_detail_omits_kernel_addresses_that_do_not_rebase() {
        // Fail closed: a pc / backtrace frame that does not resolve against
        // the registered image base (here, below it) is omitted rather than
        // disclosed raw — but the non-address breadcrumb still renders, so
        // the record stays useful.
        set_kernel_image_base(TEST_IMAGE_BASE);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let mut bt = [0u64; cpu_state::WD_BT_MAX];
        bt[0] = 0x0021_d42c; // below the image base — not rebasable
        let d = Diag {
            pc: 0x1234, // below the image base — not rebasable
            task: WatchdogSample::NO_TASK,
            aux: 0,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            breadcrumb: KernelBreadcrumb::FaultReclaim,
            breadcrumb_detail: 0x0021_d000,
            breadcrumb_seq: 5,
            bt,
            bt_len: 1,
            lock_site: 0,
            lock_acquiring: false,
            live_pc: None,
            live_context: 0,
            in_flight: Diag::IN_FLIGHT_UNREAD,
        };
        report_detail_to(sink, Level::Error, 1, None, &d);
        let ev = &sink.snapshot()[0];
        // The un-rebasable pc and backtrace are omitted (never raw).
        assert_eq!(field(ev, "pc"), None);
        assert_eq!(field(ev, "k_bt"), None);
        // The breadcrumb (no kernel-image address) still renders.
        assert_eq!(field(ev, "k_site"), Some("fault_reclaim"));
        assert_eq!(field(ev, "k_seq"), Some("5"));
        // And no raw address slipped through.
        assert!(no_field_contains(ev, "21d42c"));
        assert!(no_field_contains(ev, "1234"));
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_debug_detail_names_the_interrupt_a_wedged_core_is_still_inside() {
        // The reproduction this field exists for: the observer's shared
        // scan can only see device lines, so it falls through to an
        // innocent pending one while the real wedge is a banked PPI the
        // victim acknowledged and never completed. The victim's own record
        // names it.
        set_kernel_image_base(TEST_IMAGE_BASE);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = Diag {
            in_flight: InFlightInterrupt::Acknowledged { intid: 27 },
            stuck: Some(StuckInterrupt {
                intid: 77,
                active: false,
            }),
            ..Diag::EMPTY
        };
        report_detail_to(sink, Level::Error, 0, Some(2), &d);
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "in_flight"), Some("27"));
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_debug_detail_tells_no_interrupt_in_flight_apart_from_no_reading() {
        // `none` clears a missed completion as the cause and is worth
        // rendering; an unread reading renders nothing at all, so a record
        // can never let "the port cannot tell us" read as "the core is
        // inside no interrupt".
        set_kernel_image_base(TEST_IMAGE_BASE);
        let idle: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_detail_to(
            idle,
            Level::Error,
            0,
            None,
            &Diag {
                in_flight: InFlightInterrupt::Idle,
                ..Diag::EMPTY
            },
        );
        assert_eq!(field(&idle.snapshot()[0], "in_flight"), Some("none"));

        let unread: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_detail_to(
            unread,
            Level::Error,
            0,
            None,
            &Diag {
                pc: TEST_IMAGE_BASE + 0x10,
                ..Diag::EMPTY
            },
        );
        assert_eq!(field(&unread.snapshot()[0], "in_flight"), None);
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn a_published_in_flight_reading_is_detail_enough_on_its_own() {
        // A wedge with no pc, breadcrumb, backtrace or lock site to report
        // must still emit the detail line when the victim named the
        // interrupt it is inside — that reading is the whole diagnosis.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_detail_to(
            sink,
            Level::Error,
            0,
            Some(1),
            &Diag {
                in_flight: InFlightInterrupt::Acknowledged { intid: 0 },
                ..Diag::EMPTY
            },
        );
        assert_eq!(field(&sink.snapshot()[0], "in_flight"), Some("0"));
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_self_sample_verdict_states_whether_the_non_maskable_sample_is_live() {
        // A live sample makes a later `sampled=pre_silence` record
        // credible, so the capability is stated plainly rather than
        // discarded.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        emit_self_sample(sink, FeatureSupport::Supported);
        let ev = &sink.snapshot()[0];
        assert_eq!(ev.id, AuditEvent::CpuWatchdogSelfSample.id());
        assert_eq!(ev.level, Level::Info);
        assert_eq!(field(ev, "self_sample"), Some("live"));
        assert_eq!(field(ev, "verdict"), Some("supported"));
        // A live verdict has no reason to give.
        assert_eq!(field(ev, "reason"), None);
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn an_inactive_self_sample_is_a_warning_that_carries_its_reason() {
        // Both non-live verdicts say the discipline is inert on this
        // hardware and why, so a hard-lockup report against a lone task can
        // be judged rather than believed.
        for (support, verdict, reason) in [
            (
                FeatureSupport::Unsupported("group 0 belongs to the secure world"),
                "unsupported",
                "group 0 belongs to the secure world",
            ),
            (
                FeatureSupport::Pending("not yet probed"),
                "pending",
                "not yet probed",
            ),
        ] {
            let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
            emit_self_sample(sink, support);
            let ev = &sink.snapshot()[0];
            assert_eq!(ev.level, Level::Warn);
            assert_eq!(field(ev, "self_sample"), Some("inactive"));
            assert_eq!(field(ev, "verdict"), Some(verdict));
            assert_eq!(field(ev, "reason"), Some(reason));
        }
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_self_sample_record_carries_no_address() {
        // A capability statement, not an address-bearing detail.
        set_kernel_image_base(TEST_IMAGE_BASE);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        emit_self_sample(sink, FeatureSupport::Supported);
        let ev = &sink.snapshot()[0];
        assert!(no_field_contains(ev, "0x"));
        assert!(no_field_contains(ev, "+0"));
    }

    /// The per-CPU lock-site stack tracks the *innermost* lock and stays
    /// balanced across nesting and release, and the acquiring→held
    /// promotion is reflected. The stored value is opaque to the stack
    /// logic (a `Location` pointer), so fake distinct non-zero ids stand in
    /// (the render path is what dereferences it — tested separately).
    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_lock_site_stack_tracks_the_innermost_lock() {
        const OUTER: usize = 0x1000;
        const INNER: usize = 0x2000;
        let state = reset(60);
        // No lock held → nothing recorded.
        assert_eq!(Diag::lock_snapshot(state), (0, false));
        // Spin-acquire the outer lock: recorded as acquiring, then promoted
        // to held once the CAS wins.
        lock_push(state, OUTER, true);
        assert_eq!(Diag::lock_snapshot(state), (OUTER, true));
        state.lock_top_acquiring.store(false, Ordering::Relaxed);
        assert_eq!(Diag::lock_snapshot(state), (OUTER, false));
        // A nested (held) inner lock becomes the innermost record.
        lock_push(state, INNER, false);
        assert_eq!(Diag::lock_snapshot(state), (INNER, false));
        // Releasing the inner lock restores the outer (held) as innermost.
        lock_pop(state);
        assert_eq!(Diag::lock_snapshot(state), (OUTER, false));
        // Releasing the outer lock leaves nothing recorded.
        lock_pop(state);
        assert_eq!(Diag::lock_snapshot(state), (0, false));
        // A stray extra release underflows safely (fail-safe: no panic, no
        // wraparound into a bogus depth).
        lock_pop(state);
        assert_eq!(Diag::lock_snapshot(state), (0, false));
    }

    /// Nesting past the recorded cap still balances on release (depth counts
    /// true nesting), and once it drops back within the cap the innermost
    /// recorded site is named again — the deep excess is simply not stored.
    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_lock_site_stack_survives_nesting_past_the_cap() {
        let state = reset(61);
        // Push one more than the cap can record.
        for i in 0..=cpu_state::LOCK_STACK_MAX {
            lock_push(state, 0x1000 + i, false);
        }
        // Pop back down to a single held lock (the outermost, id 0x1000).
        for _ in 0..cpu_state::LOCK_STACK_MAX {
            lock_pop(state);
        }
        assert_eq!(Diag::lock_snapshot(state), (0x1000, false));
        lock_pop(state);
        assert_eq!(Diag::lock_snapshot(state), (0, false));
    }

    /// The debug detail names the stuck spinlock as `k_lock=<file>` +
    /// `k_lock_line` + a state tag, from the acquiring call's source
    /// `Location` — a source string, never a runtime address (no image base
    /// is even consulted). This is the field that pins an IRQ-masked hard
    /// lockup's culprit lock, which the maskable liveness sample cannot see.
    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_debug_detail_names_the_stuck_lock() {
        let site = core::panic::Location::caller();
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = Diag {
            pc: 0,
            task: WatchdogSample::NO_TASK,
            aux: 0,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            breadcrumb: KernelBreadcrumb::None,
            breadcrumb_detail: 0,
            breadcrumb_seq: 0,
            bt: [0; cpu_state::WD_BT_MAX],
            bt_len: 0,
            lock_site: core::ptr::from_ref::<core::panic::Location<'static>>(site) as usize,
            lock_acquiring: true,
            live_pc: None,
            live_context: 0,
            in_flight: Diag::IN_FLIGHT_UNREAD,
        };
        report_detail_to(sink, Level::Error, 3, Some(0), &d);
        let ev = &sink.snapshot()[0];
        assert_eq!(ev.id, AuditEvent::CpuLockupDiagnostic.id());
        assert_eq!(field(ev, "k_lock"), Some(site.file()));
        assert_eq!(field(ev, "k_lock_state"), Some("acquiring"));
        // The state tag flips to `held` for a lock the CPU had taken.
        let sink2: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let held = Diag {
            lock_acquiring: false,
            ..d
        };
        report_detail_to(sink2, Level::Error, 3, Some(0), &held);
        assert_eq!(field(&sink2.snapshot()[0], "k_lock_state"), Some("held"));
    }

    /// A lock site is address-safe: the recorded value is a source
    /// `file:line`, so a hex runtime address never appears in the rendered
    /// record even though `lock_site` is a pointer internally.
    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn the_lock_site_field_discloses_no_runtime_address() {
        let site = core::panic::Location::caller();
        let ptr = core::ptr::from_ref::<core::panic::Location<'static>>(site) as usize;
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = Diag {
            pc: 0,
            task: WatchdogSample::NO_TASK,
            aux: 0,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            breadcrumb: KernelBreadcrumb::None,
            breadcrumb_detail: 0,
            breadcrumb_seq: 0,
            bt: [0; cpu_state::WD_BT_MAX],
            bt_len: 0,
            lock_site: ptr,
            lock_acquiring: false,
            live_pc: None,
            live_context: 0,
            in_flight: Diag::IN_FLIGHT_UNREAD,
        };
        report_detail_to(sink, Level::Error, 3, None, &d);
        let ev = &sink.snapshot()[0];
        // The raw pointer value never appears as text in any field.
        let mut hex = [0u8; 16];
        let ptr_hex = format_hex_u64(ptr as u64, &mut hex);
        assert!(no_field_contains(ev, ptr_hex));
    }

    #[test]
    fn a_summary_without_a_stuck_line_omits_it() {
        // No SPI is stuck (the wedge is a pure in-kernel spin with IRQs
        // masked, not a storm), so the observer reports no line rather than
        // a fabricated one — but still marks the sample pre-silence.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = diag(
            0x0021_d42c,
            WatchdogSample::NO_TASK,
            true,
            true,
            None,
            StuckOwner::Unknown,
        );
        report_summary_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            1,
            Some(0),
            10_000_000_000,
            &d,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "context"), Some("kernel"));
        assert_eq!(field(ev, "sampled"), Some("pre_silence"));
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
        // No stuck line means no owner to attribute.
        assert_eq!(field(ev, "stuck_owner"), None);
    }

    #[test]
    fn stuck_state_tag_names_active_and_pending() {
        let tag = |active| stuck_state_tag(StuckInterrupt { intid: 37, active });
        assert_eq!(tag(true), "active");
        assert_eq!(tag(false), "pending");
    }

    #[test]
    fn a_pending_unbound_stuck_line_renders_pending_and_unbound() {
        // Only deliverable lines reach a report, so a `pending` line is an
        // enabled, asserted-but-not-yet-taken line — a real suspect. When no
        // driver owns it (`unbound`), that says the wedge is elsewhere; the
        // masked, undeliverable line the observer used to blame is never
        // reported at all now.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = diag(
            0x0021_d524,
            WatchdogSample::NO_TASK,
            true,
            true,
            Some(StuckInterrupt {
                intid: 50,
                active: false,
            }),
            StuckOwner::Unbound,
        );
        report_summary_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            1,
            Some(0),
            10_000_000_000,
            &d,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "stuck_irq"), Some("50"));
        assert_eq!(field(ev, "stuck_state"), Some("pending"));
        assert_eq!(field(ev, "stuck_owner"), Some("unbound"));
    }

    #[test]
    fn a_pending_named_stuck_line_renders_the_kernel_internal_name() {
        // A pending, enabled line that no driver bound but the kernel does
        // service itself (a chained MSI multiplexer, the console UART) is
        // named rather than dismissed as `unbound`, so a reader sees which
        // kernel-internal source a wedged CPU could not service.
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = diag(
            0x0021_d5f0,
            WatchdogSample::NO_TASK,
            true,
            true,
            Some(StuckInterrupt {
                intid: 153,
                active: false,
            }),
            StuckOwner::Named("usb-msi"),
        );
        report_summary_to(
            sink,
            AuditEvent::CpuHardLockupDetected,
            Level::Error,
            1,
            Some(0),
            10_000_000_000,
            &d,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "stuck_irq"), Some("153"));
        assert_eq!(field(ev, "stuck_state"), Some("pending"));
        assert_eq!(field(ev, "stuck_owner"), Some("usb-msi"));
    }

    #[test]
    fn a_user_context_record_says_user() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = diag(0x4000_0000, 15, false, false, None, StuckOwner::Unknown);
        report_summary_to(
            sink,
            AuditEvent::CpuStallDetected,
            Level::Error,
            1,
            None,
            10_000_000_000,
            &d,
        );
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "context"), Some("user"));
        // A soft lockup's sample is live (the CPU still takes its watchdog
        // sample), so it carries no pre-silence marker and no stuck line.
        assert_eq!(field(ev, "sampled"), None);
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
    }

    #[test]
    fn a_record_omits_context_it_does_not_have() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        report_summary_to(
            sink,
            AuditEvent::CpuStallCleared,
            Level::Warn,
            1,
            None,
            0,
            &Diag::EMPTY,
        );
        let events = sink.snapshot();
        let ev = &events[0];
        assert_eq!(field(ev, "cpu"), Some("1"));
        assert_eq!(field(ev, "stalled_ms"), Some("0"));
        assert_eq!(field(ev, "observer"), None);
        assert_eq!(field(ev, "pc"), None);
        assert_eq!(field(ev, "task"), None);
        assert_eq!(field(ev, "pstate"), None);
        // A clear record carries no sample, so neither the pre-silence
        // marker nor a stuck line is rendered.
        assert_eq!(field(ev, "sampled"), None);
        assert_eq!(field(ev, "stuck_irq"), None);
        assert_eq!(field(ev, "stuck_state"), None);
    }

    #[test]
    fn resolve_stuck_owner_attributes_a_bound_line_and_reports_unbound_otherwise() {
        // A fake resolver standing in for the live IRQ table: line 42 is
        // bound to task 7, every other line is unbound.
        struct FakeOwner;
        impl StuckOwnerResolver for FakeOwner {
            fn owner_of_line(&self, line: u32) -> Option<u64> {
                (line == 42).then_some(7)
            }
        }
        // A fake kernel-internal name table: line 153 is the platform MSI
        // multiplexer, line 33 the console UART, every other line unknown.
        struct FakeNames;
        impl KernelInternalLines for FakeNames {
            fn name_of_line(&self, line: u32) -> Option<&'static str> {
                match line {
                    153 => Some("usb-msi"),
                    33 => Some("console-uart"),
                    _ => None,
                }
            }
        }
        let fake = FakeOwner;
        let resolver: Option<&dyn StuckOwnerResolver> = Some(&fake);
        let fake_names = FakeNames;
        let names: Option<&dyn KernelInternalLines> = Some(&fake_names);

        let si = |intid| {
            Some(StuckInterrupt {
                intid,
                active: false,
            })
        };
        // No stuck line: nothing to attribute (renders no owner).
        assert_eq!(
            resolve_stuck_owner_with(None, resolver, names),
            StuckOwner::Unknown
        );
        // A stuck line bound to a driver names its task — a task binding
        // wins over any kernel-internal name.
        assert_eq!(
            resolve_stuck_owner_with(si(42), resolver, names),
            StuckOwner::Task(7)
        );
        // An otherwise-unowned line the kernel services itself is *named*
        // rather than dismissed as unbound.
        assert_eq!(
            resolve_stuck_owner_with(si(153), resolver, names),
            StuckOwner::Named("usb-msi")
        );
        assert_eq!(
            resolve_stuck_owner_with(si(33), resolver, names),
            StuckOwner::Named("console-uart")
        );
        // A stuck line neither a driver nor the kernel owns is unbound — the
        // wedge is elsewhere.
        assert_eq!(
            resolve_stuck_owner_with(si(111), resolver, names),
            StuckOwner::Unbound
        );
        // With no name resolver installed, a kernel-internal line falls back
        // to unbound (never a fabricated name).
        assert_eq!(
            resolve_stuck_owner_with(si(153), resolver, None),
            StuckOwner::Unbound
        );
        // With no owner resolver installed at all, nothing is claimed — even
        // when a name resolver is present, since attribution needs the table.
        assert_eq!(
            resolve_stuck_owner_with(si(42), None, names),
            StuckOwner::Unknown
        );
    }

    #[test]
    fn a_recovery_record_names_the_kind_and_outcome() {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        recovery_to(
            sink,
            3,
            WatchdogKind::Hard,
            RecoveryOutcome::AttentionRaised,
        );
        let events = sink.snapshot();
        let ev = &events[0];
        assert_eq!(ev.id, AuditEvent::CpuLockupRecovery.id());
        assert_eq!(field(ev, "cpu"), Some("3"));
        assert_eq!(field(ev, "kind"), Some("hard"));
        assert_eq!(field(ev, "outcome"), Some("attention"));
    }

    // --- Kernel-activity breadcrumb ---------------------------------

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn breadcrumb_tag_round_trips_and_unknown_decodes_to_none() {
        // Every named site round-trips through its `u8` encoding.
        for site in [
            KernelBreadcrumb::None,
            KernelBreadcrumb::Dispatch,
            KernelBreadcrumb::Syscall,
            KernelBreadcrumb::FaultEntry,
            KernelBreadcrumb::FaultReclaim,
            KernelBreadcrumb::FaultStack,
            KernelBreadcrumb::FaultRamzip,
            KernelBreadcrumb::FaultAnon,
            KernelBreadcrumb::FaultFile,
            KernelBreadcrumb::FaultFatal,
            KernelBreadcrumb::TaskBody,
            KernelBreadcrumb::UserSwitch,
            KernelBreadcrumb::SwitchReturn,
            KernelBreadcrumb::DispatchTail,
        ] {
            assert_eq!(KernelBreadcrumb::from_u8(site as u8), site);
            // Every named site has a distinct, non-empty tag (no accidental
            // aliasing of the finer dispatch sub-sites onto an existing one).
            assert!(!site.tag().is_empty());
        }
        // The finer dispatch sub-sites render distinct `k_site` tags, in the
        // chronological order a task hand-off walks them.
        assert_eq!(KernelBreadcrumb::TaskBody.tag(), "task_body");
        assert_eq!(KernelBreadcrumb::UserSwitch.tag(), "user_switch");
        assert_eq!(KernelBreadcrumb::SwitchReturn.tag(), "switch_return");
        assert_eq!(KernelBreadcrumb::DispatchTail.tag(), "dispatch_tail");
        // The dispatch sub-sites carry the four distinct `u8` encodings the
        // hand-off stamps in order; a renumber must keep them disjoint.
        assert_eq!(
            [
                KernelBreadcrumb::TaskBody as u8,
                KernelBreadcrumb::UserSwitch as u8,
                KernelBreadcrumb::SwitchReturn as u8,
                KernelBreadcrumb::DispatchTail as u8,
            ],
            [10, 11, 12, 13],
        );
        // A corrupt/unknown tag decodes to `None` rather than fabricating a
        // region (fail closed).
        assert_eq!(KernelBreadcrumb::from_u8(200), KernelBreadcrumb::None);
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn a_switch_return_breadcrumb_renders_the_post_switch_teardown_site() {
        // The finer post-`cs.switch` teardown crumb renders `switch_return`,
        // telling a wedge coming *back* from a task (the IRQ-masked
        // user-root translation park) apart from one going *into* it
        // (`user_switch`) on a board with no non-maskable sample.
        set_kernel_image_base(TEST_IMAGE_BASE);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let d = Diag {
            pc: TEST_IMAGE_BASE + 0x1c_7524,
            task: WatchdogSample::NO_TASK,
            aux: 0x6000_0305,
            in_kernel: true,
            sample_stale: true,
            stuck: None,
            stuck_owner: StuckOwner::Unknown,
            breadcrumb: KernelBreadcrumb::SwitchReturn,
            breadcrumb_detail: 0,
            breadcrumb_seq: 87_196,
            bt: [0u64; cpu_state::WD_BT_MAX],
            bt_len: 0,
            lock_site: 0,
            lock_acquiring: false,
            live_pc: None,
            live_context: 0,
            in_flight: Diag::IN_FLIGHT_UNREAD,
        };
        report_detail_to(sink, Level::Error, 1, Some(0), &d);
        let ev = &sink.snapshot()[0];
        assert_eq!(field(ev, "k_site"), Some("switch_return"));
        assert_eq!(field(ev, "k_detail"), Some("0x0000000000000000"));
        assert_eq!(field(ev, "k_seq"), Some("87196"));
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn note_kernel_breadcrumb_publishes_a_snapshot_readable_by_a_buddy() {
        let state = reset(44);
        // A fresh slot has no breadcrumb; a snapshot renders none.
        let before = Diag::snapshot(state);
        assert_eq!(before.breadcrumb, KernelBreadcrumb::None);
        assert_eq!(before.breadcrumb_seq, 0);

        // The CPU publishes the region it enters; the buddy observer's
        // snapshot reads a consistent (site, detail, seq) triple.
        note_kernel_breadcrumb(44, KernelBreadcrumb::Syscall, 0x2a);
        let first = Diag::snapshot(state);
        assert_eq!(first.breadcrumb, KernelBreadcrumb::Syscall);
        assert_eq!(first.breadcrumb_detail, 0x2a);
        assert_eq!(first.breadcrumb_seq, 1);

        // Each write advances the sequence, so two successive reports tell a
        // frozen breadcrumb (stuck here) from an advancing one.
        note_kernel_breadcrumb(44, KernelBreadcrumb::FaultAnon, 0x1000);
        let second = Diag::snapshot(state);
        assert_eq!(second.breadcrumb, KernelBreadcrumb::FaultAnon);
        assert_eq!(second.breadcrumb_detail, 0x1000);
        assert_eq!(second.breadcrumb_seq, 2);
    }

    #[test]
    fn note_kernel_breadcrumb_is_a_fail_closed_no_op_for_an_out_of_range_cpu() {
        // A stray id never panics and never touches a slot it does not own.
        let count = u32::try_from(cpu_state::TEST_CPUS).expect("test CPU count fits u32");
        note_kernel_breadcrumb(count, KernelBreadcrumb::Syscall, 1);
    }

    // --- Pre-silence backtrace --------------------------------------

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn note_watchdog_backtrace_publishes_a_snapshot_readable_by_a_buddy() {
        let state = reset(45);
        // A fresh slot has no backtrace; a snapshot reads none.
        assert_eq!(Diag::snapshot(state).bt_len, 0);

        // The port publishes the interrupted-context frame chain; the buddy
        // observer's snapshot reads a consistent set (length + frames).
        note_watchdog_backtrace(45, &[0x1111, 0x2222, 0x3333]);
        let snap = Diag::snapshot(state);
        assert_eq!(snap.bt_len, 3);
        assert_eq!(&snap.bt[..3], &[0x1111, 0x2222, 0x3333]);

        // A later, shorter capture replaces the previous one wholesale (the
        // published length bounds what a reader trusts).
        note_watchdog_backtrace(45, &[0xaaaa]);
        let snap = Diag::snapshot(state);
        assert_eq!(snap.bt_len, 1);
        assert_eq!(snap.bt[0], 0xaaaa);
    }

    #[cfg(feature = "watchdog-diagnostics")]
    #[test]
    fn note_watchdog_backtrace_caps_depth_and_is_a_fail_closed_no_op_out_of_range() {
        let state = reset(46);
        // More frames than the fixed diagnostic depth are truncated, never
        // overrunning the fixed per-CPU buffer.
        let deep: [u64; cpu_state::WD_BT_MAX + 4] = core::array::from_fn(|i| i as u64 + 1);
        note_watchdog_backtrace(46, &deep);
        let snap = Diag::snapshot(state);
        assert_eq!(snap.bt_len, cpu_state::WD_BT_MAX);
        assert_eq!(
            snap.bt[cpu_state::WD_BT_MAX - 1],
            cpu_state::WD_BT_MAX as u64
        );

        // An empty capture clears the record (the report then omits it).
        note_watchdog_backtrace(46, &[]);
        assert_eq!(Diag::snapshot(state).bt_len, 0);

        // A stray id never panics and never touches a slot it does not own.
        let count = u32::try_from(cpu_state::TEST_CPUS).expect("test CPU count fits u32");
        note_watchdog_backtrace(count, &[0x1234]);
    }
}
