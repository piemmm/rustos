//! Post-mortem CPU-state capture and stack-unwind surface of the Arch HAL.
//!
//! A kernel panic is fatal, non-recoverable, and halts the offending CPU,
//! so its dump should carry everything a post-mortem needs: a read-only
//! snapshot of the general-purpose registers and a bounded walk back up
//! the call stack. Capturing the register file and knowing how one stack
//! frame is laid out is genuinely target-divergent (the register set, the
//! ABI frame-pointer convention, the privileged reads), so it is a closed
//! trait set on the Arch HAL; this module is that set, modelled on the
//! [`super::memtag`] and [`super::sidechannel`] surfaces.
//!
//! # What lives here
//!
//! * [`CpuStateCapture`] — the per-port handle the panic path reaches
//!   through. It [captures](CpuStateCapture::capture) the registers, states
//!   the port's [frame layout](CpuStateCapture::frame_layout), reports the
//!   calling CPU's [kernel-stack bounds](CpuStateCapture::stack_bounds),
//!   and declares its honest [profile](CpuStateCapture::profile).
//! * [`RegisterSnapshot`] / [`NamedReg`] — the architecture-neutral,
//!   allocation-free register-file snapshot. `pc`/`sp`/`fp` are explicit
//!   because the neutral unwinder needs them; the rest are named pairs.
//! * [`FrameLayout`] — the *only* arch-specific unwinding datum: the byte
//!   offsets from a frame pointer to the saved caller frame pointer and
//!   the saved return address. The actual (bounds-checked, monotonic,
//!   depth-capped) walk is arch-neutral and lives once in `kernel/core`,
//!   reading memory only through a [`StackReader`] — so the dangerous
//!   dereference has exactly one audited site, never one per port.
//! * [`Backtrace`] / [`BacktraceProfile`] — the honest declaration,
//!   exactly like [`super::memtag::Tagging`] / [`super::sidechannel::Mitigation`]:
//!   a feature is [`Backtrace::Supported`] or [`Backtrace::Unsupported`]
//!   (with a justification — the port genuinely cannot do it).
//! * [`walk`] — the arch-neutral frame-pointer unwinder every port shares.
//! * [`conformance`] — the conformance vertical every port runs against
//!   its handle.

/// Hard cap on the number of stack frames the unwinder emits.
///
/// A corrupt-but-plausible frame chain must terminate; the monotonic and
/// bounds checks already kill cycles and off-stack pointers, and this cap
/// bounds the walk even on a chain that stays in-bounds and strictly
/// ascending for pathological inputs. 64 is deep enough for any real
/// kernel call stack and small enough to dump without flooding.
pub const MAX_FRAMES: usize = 64;

/// Maximum number of named general-purpose registers a snapshot carries.
///
/// Sized for the widest Tier-1 register file (riscv64's 31 GP registers);
/// a port with fewer simply fills fewer slots.
pub const MAX_NAMED_REGS: usize = 32;

/// One named register value in a [`RegisterSnapshot`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NamedReg {
    /// Stable, human-readable register name (e.g. `"rax"`, `"x0"`, `"ra"`).
    pub name: &'static str,
    /// The register's value at capture time.
    pub value: u64,
}

/// A read-only, allocation-free snapshot of the CPU register file.
///
/// Built by [`CpuStateCapture::capture`]. `pc`, `sp`, and `fp` are the
/// three the neutral unwinder needs and are always explicit; the named
/// general-purpose registers are extra post-mortem context. The struct is
/// `Copy` and fixed-size so the panic path never touches the heap.
#[derive(Copy, Clone, Debug)]
pub struct RegisterSnapshot {
    /// Program counter at capture (the instruction after the `capture`
    /// call site — the top of the frame chain).
    pub pc: u64,
    /// Stack pointer at capture.
    pub sp: u64,
    /// Frame pointer at capture (the register named by
    /// [`FrameLayout`]; `rbp` / `x29` / `s0`).
    pub fp: u64,
    regs: [NamedReg; MAX_NAMED_REGS],
    len: usize,
}

impl RegisterSnapshot {
    /// Start a snapshot with the three unwinder-critical registers and no
    /// named registers yet. Add named registers with [`Self::with`].
    #[must_use]
    pub const fn new(pc: u64, sp: u64, fp: u64) -> Self {
        Self {
            pc,
            sp,
            fp,
            regs: [NamedReg { name: "", value: 0 }; MAX_NAMED_REGS],
            len: 0,
        }
    }

    /// Append a named general-purpose register, consuming and returning
    /// `self` (builder style so a port's `capture` is one expression).
    ///
    /// A register beyond [`MAX_NAMED_REGS`] is silently dropped — the
    /// snapshot is best-effort post-mortem context and must never panic
    /// (fail closed, never fault mid-panic).
    #[must_use]
    pub const fn with(mut self, name: &'static str, value: u64) -> Self {
        if self.len < MAX_NAMED_REGS {
            self.regs[self.len] = NamedReg { name, value };
            self.len += 1;
        }
        self
    }

    /// The named general-purpose registers captured, in push order.
    #[must_use]
    pub fn named(&self) -> &[NamedReg] {
        &self.regs[..self.len]
    }
}

/// How one stack frame is laid out relative to its frame pointer.
///
/// This is the whole of the arch-specific unwinding knowledge: given a
/// frame pointer `fp`, the caller's saved frame pointer lives at
/// `fp + saved_fp_offset` and the return address at
/// `fp + return_addr_offset` (offsets are signed — riscv64's are
/// negative). Everything else about the walk — validation, bounds
/// checks, monotonicity, the depth cap, the reads — is arch-neutral and
/// lives in [`walk`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    /// Signed byte offset from `fp` to the word holding the caller's
    /// saved frame pointer.
    pub saved_fp_offset: i16,
    /// Signed byte offset from `fp` to the word holding the return
    /// address into the caller.
    pub return_addr_offset: i16,
}

/// A self-describing, arch-neutral snapshot of a *faulting user thread's*
/// register state, captured by the architecture port at trap entry.
///
/// Where [`RegisterSnapshot`] alone is what the kernel-panic path needs
/// (it walks its own trusted kernel stack, whose [`FrameLayout`] and
/// [`StackBounds`] the port supplies through [`CpuStateCapture`]), a
/// *user*-fault crash record must carry everything needed to walk the
/// crashing task's **user** stack from kernel context without any live
/// access to the port handle: the register file, the port's frame
/// layout, and an honest statement of whether the frame pointer is
/// usable. Bundling the three makes the value fully self-describing, so
/// the user-fault resolver can thread it by shared reference through the
/// (architecture-neutral) resolver ABI and the kernel core never needs to
/// know which port it came from.
///
/// The value is `Copy` and fixed-size (it embeds only `Copy`, fixed-size
/// members), so the fault path threads it without touching the heap — the
/// fault path must never allocate.
#[derive(Copy, Clone, Debug)]
pub struct UserRegisterFrame {
    /// The faulting thread's general-purpose register file: `pc`/`sp`/`fp`
    /// plus the named GP set, exactly as [`RegisterSnapshot`] carries them.
    pub snapshot: RegisterSnapshot,
    /// The architecture's frame-pointer layout ([`CpuStateCapture::frame_layout`]),
    /// so the neutral [`walk`] can follow the user frame chain without a
    /// port handle.
    pub layout: FrameLayout,
    /// Whether `snapshot.fp` is a usable frame pointer for an fp-walk.
    ///
    /// A port that saves the whole GP frame (incl. the fp register) at
    /// trap entry sets this `true`. A port that does not yet save the fp
    /// register sets it `false`, and the user-stack walk honestly degrades
    /// to reporting `pc`/`sp` only rather than following a frame pointer it
    /// does not actually hold (fail closed — never a fabricated chain).
    pub fp_valid: bool,
}

impl UserRegisterFrame {
    /// Bundle a captured user register file with the port's frame layout
    /// and an honest `fp_valid` flag.
    #[must_use]
    pub const fn new(snapshot: RegisterSnapshot, layout: FrameLayout, fp_valid: bool) -> Self {
        Self {
            snapshot,
            layout,
            fp_valid,
        }
    }
}

/// A half-open kernel-stack address range `[low, high)` the unwinder is
/// permitted to read while walking, in ascending address order.
///
/// The walk never dereferences an address outside this range, so a
/// corrupt frame pointer ends the walk cleanly instead of faulting.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct StackBounds {
    /// Inclusive lowest address the unwinder may read.
    pub low: u64,
    /// Exclusive highest address the unwinder may read.
    pub high: u64,
}

impl StackBounds {
    /// Construct bounds, normalising an inverted or empty range to an
    /// empty one (`low >= high`), which the unwinder treats as "no
    /// readable stack" and refuses to walk (fail closed).
    #[must_use]
    pub const fn new(low: u64, high: u64) -> Self {
        Self { low, high }
    }

    /// The bounds of a known stack region `[low, high)`, but only when the
    /// captured `sp` actually lies inside it — otherwise `None`.
    ///
    /// The one shared definition every port uses to turn its boot-stack
    /// symbols into walk bounds: a port hands the region it knows and the
    /// captured stack pointer, and gets real bounds when the CPU is on
    /// that stack, or `None` (fail closed — degrade to registers + pc)
    /// when it is on a stack the port cannot vouch for. Keeping this here
    /// means the containment rule is not re-derived in each arch crate.
    #[must_use]
    pub const fn enclosing(sp: u64, low: u64, high: u64) -> Option<StackBounds> {
        if low < high && sp >= low && sp < high {
            Some(StackBounds::new(low, high))
        } else {
            None
        }
    }

    /// `true` if the whole 8-byte word at `addr` lies within `[low, high)`.
    #[must_use]
    pub const fn contains_word(&self, addr: u64) -> bool {
        match addr.checked_add(8) {
            Some(end) => addr >= self.low && end <= self.high,
            None => false,
        }
    }
}

/// One backtrace-capability feature's status on a given port.
///
/// Mirrors [`super::memtag::Tagging`] / [`super::sidechannel::Mitigation`]:
/// a port takes exactly one honest position per feature.
/// [`Backtrace::Unsupported`] is permitted **only** where the port
/// genuinely cannot provide the feature, and the payload must record why
/// (so the conformance suite can refuse an unjustified claim).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Backtrace {
    /// The port provides this feature honestly on its silicon.
    Supported,
    /// The port cannot provide this feature. The payload is the
    /// justification recorded in the port's `README.md`; it must be
    /// non-empty.
    Unsupported(&'static str),
}

impl Backtrace {
    /// `true` if this feature is [`Backtrace::Supported`].
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// The explanatory note for an [`Backtrace::Unsupported`] decision, or
    /// `None` when supported.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of the two post-mortem capabilities.
///
/// Two genuinely distinct properties, so two slots (no slot the kernel
/// does not need):
///
/// * [`Self::register_capture`] — the port can snapshot the register file
///   ([`CpuStateCapture::capture`] returns real register values).
/// * [`Self::frame_unwind`] — the port can describe its frame layout
///   ([`CpuStateCapture::frame_layout`] returns `Some`), so the neutral
///   [`walk`] can follow the frame-pointer chain.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BacktraceProfile {
    /// The register-file snapshot is available.
    pub register_capture: Backtrace,
    /// The frame-pointer unwind layout is available.
    pub frame_unwind: Backtrace,
}

/// A single named slot of a [`BacktraceProfile`], yielded by
/// [`BacktraceProfile::entries`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BacktraceEntry {
    /// Stable, human-readable name of the slot.
    pub name: &'static str,
    /// The port's decision for this slot.
    pub backtrace: Backtrace,
}

/// Reason a [`BacktraceProfile`] failed [`BacktraceProfile::validate`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// An [`Backtrace::Unsupported`] decision carried an empty (or
    /// whitespace-only) justification; `field` names the offending slot.
    EmptyJustification {
        /// The [`BacktraceEntry::name`] of the unjustified slot.
        field: &'static str,
    },
}

impl BacktraceProfile {
    /// The two capability slots, in a stable order, each paired with its
    /// name.
    #[must_use]
    pub const fn entries(&self) -> [BacktraceEntry; 2] {
        [
            BacktraceEntry {
                name: "register_capture",
                backtrace: self.register_capture,
            },
            BacktraceEntry {
                name: "frame_unwind",
                backtrace: self.frame_unwind,
            },
        ]
    }

    /// Validate the honesty rule: every [`Backtrace::Unsupported`] slot
    /// must carry a non-empty justification.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::EmptyJustification`] naming the first slot
    /// whose [`Backtrace::detail`] is present but empty or whitespace-only.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for entry in self.entries() {
            if let Some(reason) = entry.backtrace.detail() {
                if reason.trim().is_empty() {
                    return Err(ProfileError::EmptyJustification { field: entry.name });
                }
            }
        }
        Ok(())
    }
}

/// A bounds-validated, **fallible** reader of one 64-bit stack word.
///
/// [`walk`] validates every address (non-null, aligned, within the
/// supplied [`StackBounds`]) **before** calling [`Self::read_word`]. That
/// is sufficient for a reader over memory the caller *knows* is mapped —
/// the kernel-panic reader dereferences its own trusted, in-bounds kernel
/// stack and can always return [`Some`]. It is **not** sufficient for a
/// reader over an *untrusted* address space: a user-fault backtrace walks
/// the crashing task's user stack from kernel context, where an address
/// can pass every structural check ([`StackBounds`], alignment,
/// monotonicity) and still be unmapped, freshly reclaimed, or a
/// deliberately corrupt pointer. Such a reader copies the word in through
/// the capability-checked user-access path and returns [`None`] when the
/// copy faults, so the unwinder ends the walk cleanly instead of the
/// kernel taking a fault inside the fault handler.
///
/// The return type is therefore [`Option`]: [`Some`] is the word, [`None`]
/// ends the walk. A reader must **never** itself fault, panic, or block —
/// it fails closed by returning [`None`].
pub trait StackReader {
    /// Read the 64-bit word at `addr`, or return [`None`] if it cannot be
    /// read.
    ///
    /// `addr` is guaranteed by [`walk`] to be 8-byte aligned and to have
    /// its whole 8-byte extent inside the walk's [`StackBounds`]. A reader
    /// over trusted, known-mapped memory always returns [`Some`]; a reader
    /// over an untrusted address space returns [`None`] rather than
    /// dereferencing a pointer it cannot prove is safe.
    fn read_word(&self, addr: u64) -> Option<u64>;
}

/// The post-mortem CPU-state handle an architecture port exposes.
///
/// The panic path reaches it read-only: [`Self::capture`] snapshots the
/// registers, [`Self::frame_layout`] and [`Self::stack_bounds`] feed the
/// neutral [`walk`], and [`Self::profile`] is the honest declaration.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from any CPU. No method allocates or blocks — they run on the
/// panic path, which must never touch the heap (the panic may itself be a
/// heap failure) and never fault.
pub trait CpuStateCapture: Send + Sync {
    /// The port's honest declaration of which post-mortem capabilities it
    /// provides. Must satisfy [`BacktraceProfile::validate`].
    fn profile(&self) -> BacktraceProfile;

    /// Snapshot the calling CPU's registers.
    ///
    /// Read-only, allocation-free, side-effect-free. On a port whose
    /// [`BacktraceProfile::register_capture`] is [`Backtrace::Unsupported`]
    /// the returned snapshot's `pc`/`sp`/`fp` are `0` and it carries no
    /// named registers (an honest empty snapshot, never faked values).
    fn capture(&self) -> RegisterSnapshot;

    /// The port's frame-pointer layout, or `None` when
    /// [`BacktraceProfile::frame_unwind`] is [`Backtrace::Unsupported`].
    ///
    /// `Some` is the signed offsets from a frame pointer to the saved
    /// caller frame pointer and return address; the neutral [`walk`]
    /// follows the chain using them. `None` means the neutral walker emits
    /// the captured `pc` and stops (fail closed).
    fn frame_layout(&self) -> Option<FrameLayout>;

    /// The calling CPU's current kernel-stack bounds, or `None` when the
    /// port cannot vouch for them.
    ///
    /// The unwinder reads memory only within these bounds, so an honest
    /// `None` (or an empty range) degrades the dump to registers plus the
    /// captured `pc` — it never widens the walk to memory the port cannot
    /// guarantee is mapped. A port derives them from the stack it knows the
    /// calling CPU is on (its boot stack, a per-CPU stack); when the
    /// captured `sp` is on a stack it cannot identify it returns `None`
    /// rather than guessing (fail closed).
    fn stack_bounds(&self) -> Option<StackBounds>;
}

/// Walk the frame-pointer chain, emitting each return address, and always
/// terminate without ever reading outside `bounds`.
///
/// This is the one arch-neutral unwinder every port shares (the arch
/// contributes only [`FrameLayout`], [`StackBounds`], and the initial
/// `fp`). It is the crux of a safe panic backtrace: a naive `*(fp)` walk
/// over a corrupt chain is a fault inside the fault handler — a
/// triple-fault. Every candidate frame pointer is therefore validated
/// before either word is read:
///
/// * both the saved-fp and return-address words lie wholly within
///   `bounds` (kills off-stack pointers),
/// * the frame pointer is 8-byte aligned (kills misaligned reads),
/// * each successive frame pointer is **strictly greater** than the last
///   (kills cycles and non-progressing chains — the stack grows down, so
///   caller frames are at higher addresses),
///
/// and the walk is hard-capped at [`MAX_FRAMES`]. Any failed check ends
/// the walk cleanly. `emit` is called once per resolved return address
/// (frame 1 upward); the caller emits the captured `pc` itself as frame 0.
///
/// Returns the number of return addresses emitted.
pub fn walk<R: StackReader + ?Sized>(
    reader: &R,
    start_fp: u64,
    layout: FrameLayout,
    bounds: StackBounds,
    mut emit: impl FnMut(u64),
) -> usize {
    let mut fp = start_fp;
    let mut prev_fp: u64 = 0;
    let mut count = 0usize;

    for _ in 0..MAX_FRAMES {
        // A frame pointer must be non-null, 8-byte aligned, and strictly
        // above the previous one (monotonic — kills cycles). `prev_fp`
        // starts at 0 so the first iteration's only lower bound is
        // non-null.
        if fp == 0 || fp % 8 != 0 || fp <= prev_fp {
            break;
        }

        let Some(saved_fp_addr) = offset_addr(fp, layout.saved_fp_offset) else {
            break;
        };
        let Some(ret_addr_addr) = offset_addr(fp, layout.return_addr_offset) else {
            break;
        };
        // Both words must be fully in-bounds *and* aligned before any read.
        if !bounds.contains_word(saved_fp_addr)
            || !bounds.contains_word(ret_addr_addr)
            || saved_fp_addr % 8 != 0
            || ret_addr_addr % 8 != 0
        {
            break;
        }

        // A read may fail even for a structurally valid address when the
        // reader is over an untrusted address space (an unmapped or
        // reclaimed user page): end the walk cleanly, never fault.
        let Some(ret_addr) = reader.read_word(ret_addr_addr) else {
            break;
        };
        let Some(caller_fp) = reader.read_word(saved_fp_addr) else {
            break;
        };

        // A zero return address is the conventional chain terminator
        // (the outermost frame's saved return address); stop cleanly.
        if ret_addr == 0 {
            break;
        }
        emit(ret_addr);
        count += 1;

        prev_fp = fp;
        fp = caller_fp;
    }

    count
}

/// Add a signed byte offset to a frame pointer, rejecting wrap.
#[must_use]
fn offset_addr(fp: u64, offset: i16) -> Option<u64> {
    // Widen to `i64` first so negating `i16::MIN` cannot overflow, then
    // convert the (now non-negative) magnitude with a checked `try_from`
    // rather than a sign-losing `as` cast.
    let off = i64::from(offset);
    if off >= 0 {
        fp.checked_add(u64::try_from(off).ok()?)
    } else {
        fp.checked_sub(u64::try_from(-off).ok()?)
    }
}

/// The post-mortem-capture conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`CpuStateCapture`] handle. The suite is portable — it names only the
/// trait — and runs on the host, exactly like the [`super::memtag`]
/// vertical: it is the trait-level "profile is honest" / "capture is
/// allocation-free and total" / "the declared frame layout drives the
/// neutral walker correctly" check. Each port's own host tests
/// additionally pin the concrete profile its silicon requires, and the
/// end-to-end QEMU panic vertical proves the *real* on-target capture is
/// non-trivial.
pub mod conformance {
    use super::{walk, Backtrace, CpuStateCapture, StackBounds, StackReader, MAX_FRAMES};

    /// Number of 64-bit words in the [`MockStack`] backing store. Large
    /// enough for the synthetic chain the layout check plants; fixed so
    /// the conformance vertical allocates nothing (it runs on the panic-
    /// free host path but shares the crate's `no_std`, alloc-free bar).
    const MOCK_WORDS: usize = 64;

    /// A fixed-size host stack image the neutral walker reads through, laid
    /// out to a port's real [`super::FrameLayout`]. Word index `k` is at address
    /// `base + 8*k`; out-of-range reads return `0` (the walker's bounds
    /// check prevents them in practice).
    struct MockStack {
        base: u64,
        words: [u64; MOCK_WORDS],
    }

    impl MockStack {
        fn index_of(&self, addr: u64) -> usize {
            usize::try_from((addr - self.base) / 8).unwrap_or(usize::MAX)
        }
        fn set(&mut self, addr: u64, value: u64) {
            let i = self.index_of(addr);
            self.words[i] = value;
        }
    }

    impl StackReader for MockStack {
        fn read_word(&self, addr: u64) -> Option<u64> {
            let i = self.index_of(addr);
            Some(self.words.get(i).copied().unwrap_or(0))
        }
    }

    /// Run the entire post-mortem-capture conformance suite against `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold:
    /// the profile fails [`super::BacktraceProfile::validate`], the profile
    /// and the `frame_layout`/`capture` methods disagree, `capture` is not
    /// total, or the declared frame layout does not drive the neutral
    /// walker to recover a synthetic chain.
    pub fn run_all<C: CpuStateCapture + ?Sized>(port: &C) {
        profile_is_honest(port);
        profile_matches_methods(port);
        capture_is_total(port);
        layout_drives_the_walker(port);
    }

    /// The profile validates and every `Unsupported` slot is justified.
    fn profile_is_honest<C: CpuStateCapture + ?Sized>(port: &C) {
        let profile = port.profile();
        assert!(
            profile.validate().is_ok(),
            "backtrace profile must justify every Unsupported feature: {:?}",
            profile.validate()
        );
        for entry in profile.entries() {
            if let Some(reason) = entry.backtrace.detail() {
                assert!(
                    !reason.trim().is_empty(),
                    "Unsupported feature `{}` must carry a non-empty explanation",
                    entry.name
                );
            }
        }
    }

    /// `frame_layout()` is `Some` exactly when the profile says frame
    /// unwinding is supported, so a port cannot claim one and provide the
    /// other.
    fn profile_matches_methods<C: CpuStateCapture + ?Sized>(port: &C) {
        let profile = port.profile();
        match profile.frame_unwind {
            Backtrace::Supported => assert!(
                port.frame_layout().is_some(),
                "frame_unwind Supported but frame_layout() is None"
            ),
            Backtrace::Unsupported(_) => assert!(
                port.frame_layout().is_none(),
                "frame_unwind Unsupported but frame_layout() is Some"
            ),
        }
    }

    /// `capture()` never panics and, on an Unsupported-register port,
    /// returns the honest empty snapshot rather than faked values.
    fn capture_is_total<C: CpuStateCapture + ?Sized>(port: &C) {
        let snap = port.capture();
        if let Backtrace::Unsupported(_) = port.profile().register_capture {
            assert_eq!(snap.pc, 0, "Unsupported register capture must report pc=0");
            assert_eq!(snap.sp, 0, "Unsupported register capture must report sp=0");
            assert_eq!(snap.fp, 0, "Unsupported register capture must report fp=0");
            assert!(
                snap.named().is_empty(),
                "Unsupported register capture must carry no named registers"
            );
        }
    }

    /// The port's declared [`super::FrameLayout`], driven through the neutral
    /// [`walk`] over a synthetic in-bounds ascending chain, recovers
    /// exactly the planted return addresses and terminates. A port whose
    /// frame unwinding is Unsupported has no layout and is skipped.
    fn layout_drives_the_walker<C: CpuStateCapture + ?Sized>(port: &C) {
        // Three ascending frames, one 64-byte stride apart, starting one
        // stride above `base` so a negative saved-fp/return-address offset
        // never underflows below `base`. Word index `k` is at
        // `base + 8*k`.
        const FRAMES: usize = 3;
        let Some(layout) = port.frame_layout() else {
            return;
        };
        let base: u64 = 0x8000;
        let stride: u64 = 64;
        let fp_of = |i: u64| base + (i + 1) * stride;

        let mut stack = MockStack {
            base,
            words: [0u64; MOCK_WORDS],
        };
        let mut expected = [0u64; FRAMES];
        for (i, slot) in expected.iter_mut().enumerate() {
            let iu = i as u64;
            let fp = fp_of(iu);
            let caller_fp = if i + 1 < FRAMES { fp_of(iu + 1) } else { 0 };
            let ret = 0xffff_0000_0000_0000 + iu + 1;
            let sfp_addr = super::offset_addr(fp, layout.saved_fp_offset)
                .expect("saved-fp slot address must not wrap");
            let ra_addr = super::offset_addr(fp, layout.return_addr_offset)
                .expect("return-address slot address must not wrap");
            stack.set(sfp_addr, caller_fp);
            stack.set(ra_addr, ret);
            *slot = ret;
        }

        let bounds = StackBounds::new(base, base + (MOCK_WORDS as u64) * 8);
        let mut got = [0u64; FRAMES];
        let mut idx = 0usize;
        let n = walk(&stack, fp_of(0), layout, bounds, |ra| {
            if idx < FRAMES {
                got[idx] = ra;
            }
            idx += 1;
        });
        assert_eq!(n, FRAMES, "walker frame count mismatch");
        assert_eq!(idx, FRAMES, "walker emitted the wrong number of frames");
        assert_eq!(got, expected, "walker did not recover the planted chain");
        assert!(n <= MAX_FRAMES, "walker exceeded the frame cap");
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// x86_64 / aarch64 layout: saved caller fp at `[fp]`, return address
    /// at `[fp + 8]`.
    const FP_HIGH_LAYOUT: FrameLayout = FrameLayout {
        saved_fp_offset: 0,
        return_addr_offset: 8,
    };

    /// riscv64 layout: caller fp at `[fp - 16]`, return address at
    /// `[fp - 8]`.
    const FP_LOW_LAYOUT: FrameLayout = FrameLayout {
        saved_fp_offset: -16,
        return_addr_offset: -8,
    };

    /// A dense host stack image addressed from `base`.
    struct Mem {
        base: u64,
        words: std::vec::Vec<u64>,
    }

    impl Mem {
        fn new(base: u64, len: usize) -> Self {
            Self {
                base,
                words: std::vec![0u64; len],
            }
        }
        fn idx(&self, addr: u64) -> usize {
            ((addr - self.base) / 8) as usize
        }
        fn set(&mut self, addr: u64, v: u64) {
            let i = self.idx(addr);
            self.words[i] = v;
        }
        fn bounds(&self) -> StackBounds {
            StackBounds::new(self.base, self.base + (self.words.len() as u64) * 8)
        }
    }

    impl StackReader for Mem {
        fn read_word(&self, addr: u64) -> Option<u64> {
            // Fail loudly in tests if the walker ever reads out of bounds —
            // that would be the very fault-in-fault-handler bug we defend
            // against. `get` returns None rather than panicking on the
            // host, but the assertion documents the contract.
            let i = self.idx(addr);
            assert!(
                i < self.words.len(),
                "walker read out of bounds at {addr:#x}"
            );
            Some(self.words[i])
        }
    }

    fn collect(mem: &Mem, start_fp: u64, layout: FrameLayout) -> std::vec::Vec<u64> {
        let mut got = std::vec::Vec::new();
        walk(mem, start_fp, layout, mem.bounds(), |ra| got.push(ra));
        got
    }

    #[test]
    fn walks_a_normal_chain_high_layout() {
        // frame i at fp = base + (i+1)*64; caller above; ret = 0xAA0i.
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp = |i: u64| base + (i + 1) * 64;
        for i in 0..3u64 {
            let caller = if i < 2 { fp(i + 1) } else { 0 };
            mem.set(fp(i), caller);
            mem.set(fp(i) + 8, 0xAA00 + i + 1);
        }
        let got = collect(&mem, fp(0), FP_HIGH_LAYOUT);
        assert_eq!(got, std::vec![0xAA01, 0xAA02, 0xAA03]);
    }

    #[test]
    fn user_register_frame_bundles_snapshot_layout_and_fp_validity() {
        // A `UserRegisterFrame` is a self-describing bundle: the neutral
        // walk can follow the frame chain using only the frame's own
        // embedded `layout` and `snapshot.fp`, with no port handle.
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp = |i: u64| base + (i + 1) * 64;
        for i in 0..2u64 {
            let caller = if i < 1 { fp(i + 1) } else { 0 };
            mem.set(fp(i), caller);
            mem.set(fp(i) + 8, 0xD00D + i);
        }
        let snapshot = RegisterSnapshot::new(0xF000, fp(0) + 8, fp(0)).with("x0", 7);
        let frame = UserRegisterFrame::new(snapshot, FP_HIGH_LAYOUT, true);
        assert!(frame.fp_valid);
        assert_eq!(frame.snapshot.fp, fp(0));
        assert_eq!(frame.snapshot.named()[0].value, 7);

        let mut got = std::vec::Vec::new();
        walk(&mem, frame.snapshot.fp, frame.layout, mem.bounds(), |ra| {
            got.push(ra);
        });
        assert_eq!(got, std::vec![0xD00D, 0xD00E]);

        // An honest `fp_valid = false` is preserved so a consumer degrades
        // to `pc`/`sp` only rather than trusting a frame pointer the port
        // did not actually save.
        let no_fp = UserRegisterFrame::new(snapshot, FP_HIGH_LAYOUT, false);
        assert!(!no_fp.fp_valid);
    }

    #[test]
    fn walks_a_normal_chain_low_layout() {
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp = |i: u64| base + (i + 2) * 64;
        for i in 0..3u64 {
            let caller = if i < 2 { fp(i + 1) } else { 0 };
            mem.set(fp(i) - 16, caller);
            mem.set(fp(i) - 8, 0xBB00 + i + 1);
        }
        let got = collect(&mem, fp(0), FP_LOW_LAYOUT);
        assert_eq!(got, std::vec![0xBB01, 0xBB02, 0xBB03]);
    }

    #[test]
    fn cycle_terminates_via_monotonicity() {
        // A frame whose saved fp points back to itself must not loop.
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp0 = base + 64;
        mem.set(fp0, fp0); // caller fp == self → not strictly greater
        mem.set(fp0 + 8, 0xC001);
        let got = collect(&mem, fp0, FP_HIGH_LAYOUT);
        // Emits the first return address, then the self-referential caller
        // fails the strict-monotonic check and the walk stops.
        assert_eq!(got, std::vec![0xC001]);
    }

    #[test]
    fn descending_chain_terminates() {
        // caller fp below current fp (non-monotonic) stops the walk.
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp1 = base + 128;
        let fp0 = base + 64;
        mem.set(fp1, fp0); // caller is lower → rejected
        mem.set(fp1 + 8, 0xD001);
        let got = collect(&mem, fp1, FP_HIGH_LAYOUT);
        assert_eq!(got, std::vec![0xD001]);
    }

    #[test]
    fn unaligned_start_fp_emits_nothing() {
        let base = 0x1_0000u64;
        let mem = Mem::new(base, 64);
        let got = collect(&mem, base + 65, FP_HIGH_LAYOUT); // not 8-aligned
        assert!(got.is_empty());
    }

    #[test]
    fn null_start_fp_emits_nothing() {
        let base = 0x1_0000u64;
        let mem = Mem::new(base, 64);
        let got = collect(&mem, 0, FP_HIGH_LAYOUT);
        assert!(got.is_empty());
    }

    #[test]
    fn out_of_bounds_fp_emits_nothing_and_never_reads() {
        let base = 0x1_0000u64;
        let mem = Mem::new(base, 64);
        // A frame pointer far outside the mapped window: the bounds check
        // must reject it *before* any read (Mem::read_word would assert).
        let got = collect(&mem, base + 0x10_0000, FP_HIGH_LAYOUT);
        assert!(got.is_empty());
    }

    /// A reader over an untrusted address space signals an unreadable
    /// word (an unmapped/reclaimed user page) by returning `None`. The
    /// walk must emit the frames read so far and end cleanly — it must
    /// never treat `None` as a value, and never fault. This is the crux
    /// of the user-stack unwind safety contract.
    struct FailingAt {
        mem: Mem,
        fail_addr: u64,
    }

    impl StackReader for FailingAt {
        fn read_word(&self, addr: u64) -> Option<u64> {
            if addr == self.fail_addr {
                None
            } else {
                self.mem.read_word(addr)
            }
        }
    }

    #[test]
    fn unreadable_word_ends_the_walk_cleanly() {
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp = |i: u64| base + (i + 1) * 64;
        for i in 0..3u64 {
            let caller = if i < 2 { fp(i + 1) } else { 0 };
            mem.set(fp(i), caller);
            mem.set(fp(i) + 8, 0xAA00 + i + 1);
        }
        // Make the second frame's return-address word unreadable: the
        // first frame is emitted, then the walk stops on the failed read.
        let bounds = mem.bounds();
        let reader = FailingAt {
            mem,
            fail_addr: fp(1) + 8,
        };
        let mut got = std::vec::Vec::new();
        walk(&reader, fp(0), FP_HIGH_LAYOUT, bounds, |ra| got.push(ra));
        assert_eq!(got, std::vec![0xAA01]);
    }

    #[test]
    fn depth_is_capped() {
        // A very long strictly-ascending in-bounds chain must stop at
        // MAX_FRAMES rather than run to the end of memory. Frames are one
        // 16-byte stride apart so a frame's saved-fp word (`fp+0`) and its
        // return-address word (`fp+8`) never overlap the next frame.
        let base = 0x1_0000u64;
        let len = (MAX_FRAMES + 10) * 2 + 16;
        let mut mem = Mem::new(base, len);
        let fp = |i: u64| base + (i + 1) * 16;
        for i in 0..(MAX_FRAMES as u64 + 5) {
            mem.set(fp(i), fp(i + 1));
            mem.set(fp(i) + 8, 0xE000 + i + 1);
        }
        let mut count = 0usize;
        let n = walk(&mem, fp(0), FP_HIGH_LAYOUT, mem.bounds(), |_| count += 1);
        assert_eq!(n, MAX_FRAMES);
        assert_eq!(count, MAX_FRAMES);
    }

    #[test]
    fn zero_return_address_terminates() {
        let base = 0x1_0000u64;
        let mut mem = Mem::new(base, 64);
        let fp0 = base + 64;
        mem.set(fp0, fp0 + 64);
        mem.set(fp0 + 8, 0); // zero ret → terminator before emit
        let got = collect(&mem, fp0, FP_HIGH_LAYOUT);
        assert!(got.is_empty());
    }

    #[test]
    fn offset_addr_handles_signed_offsets_and_rejects_wrap() {
        assert_eq!(offset_addr(0x1000, 8), Some(0x1008));
        assert_eq!(offset_addr(0x1000, -16), Some(0x0FF0));
        assert_eq!(offset_addr(0, -8), None);
        assert_eq!(offset_addr(u64::MAX, 8), None);
    }

    #[test]
    fn register_snapshot_builder_caps_and_reads_back() {
        let mut snap = RegisterSnapshot::new(0x10, 0x20, 0x30);
        for i in 0..(MAX_NAMED_REGS + 4) {
            snap = snap.with("r", i as u64);
        }
        assert_eq!(snap.pc, 0x10);
        assert_eq!(snap.named().len(), MAX_NAMED_REGS);
        assert_eq!(snap.named()[0].value, 0);
        assert_eq!(
            snap.named()[MAX_NAMED_REGS - 1].value,
            (MAX_NAMED_REGS - 1) as u64
        );
    }

    #[test]
    fn stack_bounds_word_containment() {
        let b = StackBounds::new(0x100, 0x200);
        assert!(b.contains_word(0x100));
        assert!(b.contains_word(0x1F8));
        assert!(!b.contains_word(0x1F9)); // 0x1F9+8 = 0x201 > high
        assert!(!b.contains_word(0x0F8)); // below low
        assert!(!b.contains_word(u64::MAX)); // wrap
    }

    #[test]
    fn profile_validate_rejects_empty_justification() {
        let p = BacktraceProfile {
            register_capture: Backtrace::Unsupported("  "),
            frame_unwind: Backtrace::Supported,
        };
        assert_eq!(
            p.validate(),
            Err(ProfileError::EmptyJustification {
                field: "register_capture"
            })
        );
    }

    #[test]
    fn profile_validate_accepts_justified() {
        let p = BacktraceProfile {
            register_capture: Backtrace::Supported,
            frame_unwind: Backtrace::Unsupported("host-managed stack"),
        };
        assert_eq!(p.validate(), Ok(()));
        assert_eq!(
            p.entries().map(|e| e.name),
            ["register_capture", "frame_unwind"]
        );
    }

    /// A host stub port exercising the conformance vertical end-to-end.
    struct StubCapture {
        layout: Option<FrameLayout>,
        profile: BacktraceProfile,
    }

    impl CpuStateCapture for StubCapture {
        fn profile(&self) -> BacktraceProfile {
            self.profile
        }
        fn capture(&self) -> RegisterSnapshot {
            if self.profile.register_capture.is_supported() {
                RegisterSnapshot::new(0x1000, 0x2000, 0x3000).with("r0", 1)
            } else {
                RegisterSnapshot::new(0, 0, 0)
            }
        }
        fn frame_layout(&self) -> Option<FrameLayout> {
            self.layout
        }
        fn stack_bounds(&self) -> Option<StackBounds> {
            None
        }
    }

    #[test]
    fn conformance_accepts_a_supported_high_layout_port() {
        let port = StubCapture {
            layout: Some(FP_HIGH_LAYOUT),
            profile: BacktraceProfile {
                register_capture: Backtrace::Supported,
                frame_unwind: Backtrace::Supported,
            },
        };
        conformance::run_all(&port);
        let dynamic: &dyn CpuStateCapture = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    fn conformance_accepts_a_supported_low_layout_port() {
        let port = StubCapture {
            layout: Some(FP_LOW_LAYOUT),
            profile: BacktraceProfile {
                register_capture: Backtrace::Supported,
                frame_unwind: Backtrace::Supported,
            },
        };
        conformance::run_all(&port);
    }

    #[test]
    fn conformance_accepts_an_unsupported_port() {
        let port = StubCapture {
            layout: None,
            profile: BacktraceProfile {
                register_capture: Backtrace::Unsupported("host-managed stack"),
                frame_unwind: Backtrace::Unsupported("host-managed stack"),
            },
        };
        conformance::run_all(&port);
    }

    #[test]
    #[should_panic(expected = "frame_unwind Supported but frame_layout() is None")]
    fn conformance_rejects_inconsistent_profile() {
        let port = StubCapture {
            layout: None,
            profile: BacktraceProfile {
                register_capture: Backtrace::Supported,
                frame_unwind: Backtrace::Supported,
            },
        };
        conformance::run_all(&port);
    }
}
