//! Context-switch surface of the Arch HAL ("context
//! switch").
//!
//! Suspending the running task and resuming another on a CPU is a
//! privilege-neutral but deeply architecture-specific operation: it
//! saves the outgoing task's callee-saved registers onto its kernel
//! stack, records the resulting stack pointer, loads the inbound task's
//! stack pointer, and restores its registers with the port's native
//! prologue/epilogue assembly. The charter makes the architecture surface a
//! closed set of traits on the HAL; this module is the "context switch"
//! member of that set, so the per-task save area and the switch
//! primitive live behind one vocabulary instead of being re-described
//! at every call site. The parallel per-arch
//! implementations of this one trait are the deliberate shape of
//! modularity, never collapsed behind `cfg` (carve-out).
//!
//! # What lives here
//!
//! * [`TaskContext`] — the architecture-neutral per-task save area the
//!   scheduler parks in its task table. Every bare-metal port persists
//!   exactly one word across a switch — the kernel-stack pointer at
//!   suspension — and keeps the callee-saved registers *on the stack
//!   itself* in a fixed frame owned by the switch assembly. So the
//!   neutral save area is a single `#[repr(C)]` `u64`, layout-identical
//!   to each port's native `TaskCtx` (one definition).
//! * [`TaskEntry`] — the entry point a freshly prepared task first runs.
//!   A plain `unsafe extern "C" fn(usize) -> !` (not a closure): it is
//!   reached via the port's resume assembly, which has no Rust frame to
//!   drop a captured environment in, and a task body never returns to
//!   its synthesised frame.
//! * [`KernelStackRegion`] — a kernel stack, as a pointer and a length
//!   rather than an address, because a port writes the initial frame
//!   through it and the panic unwinder reads words through it, and an
//!   address carries no provenance to do either with. Building one is the
//!   `unsafe` step, which is what lets [`ContextSwitch::prepare`] be safe.
//! * [`PrepareError`] — the fail-closed result of seeding a task's
//!   initial frame ([`ContextSwitch::prepare`]). A bad stack is rejected,
//!   never silently truncated.
//! * [`ContextSwitch`] — the per-port handle the kernel reaches through.
//!   It seeds a never-run task's first frame ([`ContextSwitch::prepare`],
//!   host-testable pointer/layout math) and performs the bare-metal
//!   switch ([`ContextSwitch::switch`], the port's assembly).
//! * [`conformance`] — the conformance vertical: a host-run
//!   [`conformance::run_all`] check every bare-metal port runs over its
//!   [`ContextSwitch`] handle, proving the `prepare` contract (an empty
//!   context is not runnable, a bad stack is rejected fail-closed, and a
//!   good stack yields a runnable in-bounds frame).
//!
//! # Why `prepare` is host-tested but `switch` is not
//!
//! [`ContextSwitch::prepare`] is pure pointer/layout arithmetic over a
//! caller-supplied stack buffer, so it runs and is asserted on the host
//! exactly like the [`crate::timer::conformance`] vertical. The switch
//! itself is only meaningful on the bare-metal target — it returns into
//! a *different* task's stack and cannot be observed from `cargo test` —
//! so, like [`crate::EnterUser::enter_user`], it carries no host
//! conformance check; it is proven end-to-end by each port's QEMU
//! scheduler-drive vertical (a real task switch round-trips). Inventing
//! a host stub that "switches" would be a fake primitive.

use core::ptr::NonNull;

/// The entry point a freshly prepared [`TaskContext`] first runs.
///
/// `unsafe extern "C" fn(usize) -> !` rather than a closure: the port's
/// resume assembly jumps to it with the first-argument register set to
/// the task's argument, and a task body never returns to its synthesised
/// frame, so there is no captured environment to drop and nothing for
/// the frame's return address to land on.
pub type TaskEntry = unsafe extern "C" fn(usize) -> !;

/// The architecture-neutral per-task register-save area.
///
/// One [`TaskContext`] per scheduler task. The only persisted field is
/// the kernel-stack pointer at the moment the task was last suspended;
/// the callee-saved registers live on the task's own stack in a fixed
/// frame the port's switch assembly owns. The layout is identical on
/// every bare-metal port (a single `#[repr(C)]` `u64`), so the neutral
/// type *is* the port's `TaskCtx` rather than a parallel definition.
///
/// A freshly constructed [`TaskContext`] has [`stack_pointer`] zero and
/// is **not runnable** ([`Self::is_runnable`]): callers must seed an
/// initial frame with [`ContextSwitch::prepare`] before the first
/// switch into it.
///
/// [`stack_pointer`]: Self::stack_pointer
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskContext {
    /// Kernel-stack pointer at suspension. Read by the resume half of
    /// [`ContextSwitch::switch`]; written by the suspend half and by
    /// [`ContextSwitch::prepare`]. Zero on a never-prepared task.
    pub stack_pointer: u64,
}

impl TaskContext {
    /// Build an empty context. [`Self::stack_pointer`] is zero; the task
    /// is not runnable until [`ContextSwitch::prepare`] seeds a frame.
    #[must_use]
    pub const fn empty() -> Self {
        Self { stack_pointer: 0 }
    }

    /// `true` once an initial frame has been seeded (a non-zero stack
    /// pointer). A switch into a non-runnable context is a kernel bug
    /// the port's `switch` contract forbids.
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        self.stack_pointer != 0
    }
}

/// The widest ABI stack alignment any Tier-1 port requires, and therefore
/// the alignment [`KernelStackRegion::seed_frame`] demands of a region's
/// top. Every stack source aligns to this, so it has one definition rather
/// than one per port and one per source.
pub const STACK_ALIGN: usize = 16;

/// The fail-closed result of seeding a task's first frame
/// ([`ContextSwitch::prepare`]).
///
/// A stack that cannot hold a valid initial frame is rejected, never
/// silently truncated or wrapped. There is no "null stack" variant:
/// [`KernelStackRegion`] cannot name one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PrepareError {
    /// The region's top was not [`STACK_ALIGN`]-aligned.
    Misaligned,
    /// The region was smaller than the port's initial frame.
    TooSmall,
}

/// A kernel stack, as [`ContextSwitch::prepare`] receives it and as the
/// panic unwinder reads it.
///
/// Carries the pointer its bytes are touched *through*, not merely their
/// address: a consumer handed a bare integer would have to invent a pointer
/// from it, and one invented that way carries no provenance for what it
/// addresses, so the compiler may reorder or elide accesses through it. The
/// length is the other half — it makes [`PrepareError::TooSmall`] a question
/// about the stack rather than about address zero, and it bounds
/// [`Self::word_ptr`]'s derivation.
///
/// The two consumers differ only in which end they use: `prepare` writes the
/// initial frame at [`Self::seed_frame`], the unwinder reads words through
/// [`Self::word_ptr`]. Both derive from the one root whoever established the
/// stack minted.
#[derive(Copy, Clone, Debug)]
pub struct KernelStackRegion {
    /// Lowest byte of the usable stack.
    base: NonNull<u8>,
    /// Usable bytes above [`Self::base`].
    len: usize,
}

// SAFETY: the descriptor is immutable and hands out no exclusive access of
// its own — `seed_frame` and `word_ptr` return raw pointers, so the
// exclusivity the constructor's contract demands travels with the pointer
// and is the minter's obligation, not a property sharing the descriptor
// could violate. Sharing one therefore grants nothing the bare
// `(address, length)` pair it replaced did not.
unsafe impl Send for KernelStackRegion {}
// SAFETY: as `Send` above.
unsafe impl Sync for KernelStackRegion {}

impl KernelStackRegion {
    /// Name the usable stack `[base, base + len)`.
    ///
    /// # Safety
    ///
    /// `base` must be the lowest byte of a region of `len` bytes that is
    /// mapped, writable, exclusive to the task, and stays valid for as long
    /// as the region is used. Discharging this here is what lets
    /// [`ContextSwitch::prepare`] be a safe function: the proof travels with
    /// the value instead of living in prose at every call site.
    #[must_use]
    pub const unsafe fn new(base: NonNull<u8>, len: usize) -> Self {
        Self { base, len }
    }

    /// Name the stack `[low, high)` a port vouches for, but only when `sp`
    /// is actually on it — otherwise `None`.
    ///
    /// The one shared definition a port turns its boot-stack linker symbols
    /// into a walkable region with: the region exists because the linker
    /// reserved it, which is a fact only the port holds, so the pointer is
    /// minted here rather than rebuilt by every consumer that wants to read
    /// a word of it. A `sp` outside the range means the CPU is on a stack
    /// this region cannot vouch for, and the caller degrades rather than
    /// guessing. An unrepresentable or inverted range is refused.
    ///
    /// # Safety
    ///
    /// `[low, high)` must be a mapped, writable stack region that stays
    /// valid for as long as the returned value is used.
    #[must_use]
    pub unsafe fn enclosing(sp: u64, low: u64, high: u64) -> Option<Self> {
        let (addr, len) = Self::enclosing_span(sp, low, high)?;
        // The one int-to-pointer step, stated where the fact that these
        // bytes exist is known rather than re-derived per read.
        let base = NonNull::new(core::ptr::with_exposed_provenance_mut::<u8>(addr))?;
        Some(Self { base, len })
    }

    /// The base address and length [`Self::enclosing`] resolves `[low, high)`
    /// to, or `None` on the refusals it makes.
    ///
    /// Split out so the rule can be tested: minting a pointer from an address
    /// is an operation no interpreter can follow, so a host test that called
    /// [`Self::enclosing`] for a range it accepts would take the whole crate's
    /// undefined-behaviour stage down with it. The mint is exercised by the
    /// ports, on target.
    fn enclosing_span(sp: u64, low: u64, high: u64) -> Option<(usize, usize)> {
        // A region holding address zero would make a null pointer one of its
        // bytes, which its `NonNull` root cannot name; refusing it is part of
        // the rule rather than a by-product of building the pointer.
        if low == 0 || low >= high || sp < low || sp >= high {
            return None;
        }
        let addr = usize::try_from(low).ok()?;
        let len = usize::try_from(high - low).ok()?;
        Some((addr, len))
    }

    /// Lowest address in the region.
    #[must_use]
    pub fn base_addr(self) -> u64 {
        self.base.addr().get() as u64
    }

    /// The region's root, so a consumer can republish the pointer rather
    /// than an address it would have to rebuild one from.
    #[must_use]
    pub const fn base_ptr(self) -> NonNull<u8> {
        self.base
    }

    /// Exclusive upper bound of the region (one past its last byte).
    #[must_use]
    pub fn top_addr(self) -> u64 {
        self.base.addr().get() as u64 + self.len as u64
    }

    /// `true` if `addr` is one of this region's bytes.
    #[must_use]
    pub fn contains_addr(self, addr: u64) -> bool {
        addr >= self.base_addr() && addr < self.top_addr()
    }

    /// A pointer to the 64-bit word at `addr`, derived from the region's own
    /// root, or `None` when the word is not wholly inside it or is
    /// misaligned.
    ///
    /// The only way to read a word of the region: an address that survives
    /// the range check is expressed as an offset from the root the
    /// constructor vouched for, so the read carries provenance for the
    /// bytes it touches instead of aliasing nothing.
    #[must_use]
    pub fn word_ptr(self, addr: u64) -> Option<NonNull<u64>> {
        if !addr.is_multiple_of(8) {
            return None;
        }
        let end = addr.checked_add(8)?;
        if addr < self.base_addr() || end > self.top_addr() {
            return None;
        }
        let offset = usize::try_from(addr - self.base_addr()).ok()?;
        // SAFETY: `offset < len`, so the step stays inside the region the
        // constructor's contract vouches for, and the whole word above it
        // was proved in-bounds.
        Some(unsafe { self.base.add(offset) }.cast::<u64>())
    }

    /// Usable bytes the region spans.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len
    }

    /// Whether the region spans no bytes at all.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The port's initial frame: the topmost `frame_bytes` of the region,
    /// as a pointer to its lowest byte — the stack pointer the first switch
    /// into the task resumes from.
    ///
    /// The two refusals below are the same on every port, so they are
    /// checked once here rather than restated in each one.
    ///
    /// # Errors
    ///
    /// [`PrepareError::Misaligned`] if the region's top does not meet
    /// [`STACK_ALIGN`]; [`PrepareError::TooSmall`] if the frame does not
    /// fit in the region.
    pub fn seed_frame(self, frame_bytes: usize) -> Result<NonNull<u8>, PrepareError> {
        // The top's own address: alignment is a question about the address,
        // so it is asked in the width an address has.
        if !(self.base.addr().get() + self.len).is_multiple_of(STACK_ALIGN) {
            return Err(PrepareError::Misaligned);
        }
        let Some(offset) = self.len.checked_sub(frame_bytes) else {
            return Err(PrepareError::TooSmall);
        };
        // SAFETY: `offset <= self.len`, so the result is within the region
        // the constructor's contract vouches for (one-past-the-end at worst,
        // when the frame is empty).
        Ok(unsafe { self.base.add(offset) })
    }
}

/// The context-switch handle an architecture port exposes.
///
/// The kernel seeds a never-run task's first frame once with
/// [`Self::prepare`], then suspends/resumes tasks with [`Self::switch`]
/// at every yield/preemption point. Both operate on the neutral
/// [`TaskContext`]; the port forwards them to its native `TaskCtx` save
/// area and switch assembly (the layouts are identical, so the forward
/// is a reinterpretation, not a copy).
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU's scheduler path. A port's handle is typically
/// zero-sized — the per-task state lives in the [`TaskContext`] the
/// caller owns, not in the handle — exactly like the [`crate::Timer`]
/// and [`crate::EnterUser`] handles.
pub trait ContextSwitch: Send + Sync {
    /// Seed `ctx`'s initial frame so the first [`Self::switch`] *into* it
    /// lands at `entry` with the first-argument register set to `arg`.
    ///
    /// `stack` is the task's usable kernel stack; the frame is written in
    /// its topmost bytes. On success `ctx.stack_pointer` points at the
    /// bottom of the synthesised frame and [`TaskContext::is_runnable`]
    /// becomes `true`.
    ///
    /// # Errors
    ///
    /// Returns a [`PrepareError`] (and leaves `ctx` unchanged) if the
    /// region's top is misaligned for the port's ABI or the region is too
    /// small to hold the port's initial frame. The port fails closed
    /// rather than seed a corrupt frame.
    fn prepare(
        &self,
        ctx: &mut TaskContext,
        stack: KernelStackRegion,
        entry: TaskEntry,
        arg: usize,
    ) -> Result<(), PrepareError>;

    /// Switch from `prev` to `next` on the calling CPU.
    ///
    /// Saves the running task's callee-saved registers onto its stack,
    /// records the resulting stack pointer in `*prev`, loads
    /// `(*next).stack_pointer`, and restores the inbound task's
    /// registers. Control returns to the call site of the inbound task's
    /// previous switch — or, for a never-run task seeded by
    /// [`Self::prepare`], to that task's `entry`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that:
    ///
    /// * `prev` and `next` are non-null, aligned [`TaskContext`] pointers
    ///   the kernel owns;
    /// * `prev` belongs to the currently-running task and is exclusive to
    ///   this CPU;
    /// * `(*next)` is runnable ([`TaskContext::is_runnable`]) — its
    ///   `stack_pointer` is a value [`Self::prepare`] or a prior
    ///   [`Self::switch`] wrote for the inbound task;
    /// * the inbound kernel stack is mapped and exclusive to this CPU.
    ///
    /// Switching into a non-runnable `next` is a kernel bug.
    unsafe fn switch(&self, prev: *mut TaskContext, next: *mut TaskContext);

    /// Restore this CPU's *between-handler* privilege-entry convention
    /// immediately **before** a user task cooperatively parks
    /// mid-syscall-handler (the [`Self::leave_cooperative_park`] partner
    /// runs the instant the task is switched back in).
    ///
    /// A resumable user kthread can suspend itself from inside its own
    /// syscall handler (a `yield`/`wait` that reschedules), so the kernel
    /// switches *away* from a CPU that is mid-handler and may later switch a
    /// *different* task in. On ports whose syscall entry leaves a per-CPU
    /// register convention flipped for the duration of the handler — x86_64,
    /// where `swapgs` makes `%gs` the kernel TLS only between entry and exit
    /// — that flipped state must be balanced back to the convention the
    /// dispatcher and the next user-entry path expect before the park, or the
    /// next ring-3 entry of another task would observe an unbalanced `swapgs`
    /// and fault (`plans/PI.md` X2). This pair brackets exactly the park so
    /// the two `swapgs` always pair on the *same* task's control flow.
    ///
    /// The default is a no-op: ports with no such per-handler convention
    /// (aarch64 saves `SP_EL0`/`ELR_EL1`/`SPSR_EL1` in the trap frame;
    /// riscv64 has no cooperative mid-handler park yet) need nothing here.
    /// Only the cooperative-park path
    /// ([`reschedule_current`](../../tairix_kernel_core/index.html)) calls it;
    /// the first trampoline→user entry never does, so it stays balanced.
    ///
    /// # Safety
    ///
    /// Must be called only from the running user task's own syscall-handler
    /// control flow, exactly once before each cooperative park, paired with
    /// [`Self::leave_cooperative_park`] on resume. Calling it elsewhere would
    /// leave the CPU's privilege-entry convention unbalanced.
    unsafe fn enter_cooperative_park(&self) {}

    /// Re-establish this CPU's *in-handler* privilege-entry convention
    /// immediately **after** a parked user task is switched back in, undoing
    /// [`Self::enter_cooperative_park`].
    ///
    /// The default is a no-op. See [`Self::enter_cooperative_park`] for the
    /// full contract; this is its exact inverse and the two must be paired.
    ///
    /// # Safety
    ///
    /// Must be called only on resume from a cooperative park, paired with a
    /// prior [`Self::enter_cooperative_park`] on the same task's control flow.
    unsafe fn leave_cooperative_park(&self) {}
}

/// The context-switch conformance vertical.
///
/// Every bare-metal architecture port runs [`conformance::run_all`]
/// against its [`ContextSwitch`] handle. The suite is portable — it
/// names only the trait — and runs on the host, exactly like the sibling
/// [`crate::timer::conformance`] and [`crate::percpu::conformance`]
/// verticals. It exercises only [`ContextSwitch::prepare`] (pure
/// pointer/layout math); [`ContextSwitch::switch`] is proven by each
/// port's QEMU scheduler-drive vertical (see the module docs).
///
/// It is driven per port (not folded into [`crate::conformance::run_all`])
/// because the suite seeds a frame into a caller-supplied stack and runs
/// over the port's real handle in that port's crate, the same precedent
/// as [`crate::irq::conformance`] and [`crate::timer::conformance`].
pub mod conformance {
    use super::{ContextSwitch, KernelStackRegion, PrepareError, TaskContext, TaskEntry};
    use core::ptr::NonNull;

    /// A divergent host entry used only for its address. The conformance
    /// suite never switches into a prepared frame on the host (that is
    /// the bare-metal-only operation), so this is never invoked; it
    /// exists so [`ContextSwitch::prepare`] has a valid [`TaskEntry`] to
    /// encode. The body diverges via `panic!` rather than `loop {}` so
    /// clippy's `empty_loop` lint does not fire on the host build.
    unsafe extern "C" fn probe_entry(_arg: usize) -> ! {
        panic!("probe_entry is address-only; never invoked by the conformance suite")
    }

    /// A stack buffer large enough for any port's initial frame, aligned
    /// to the widest ABI stack alignment the targets require (16 bytes).
    /// Sized at 512 bytes — comfortably above every port's frame — so the
    /// success case has a valid, in-bounds top to seed.
    ///
    /// A real buffer rather than a literal address: the suite hands its
    /// top to a port that genuinely writes a frame there, so the bytes
    /// have to exist and the pointer has to carry provenance for them.
    #[repr(C, align(16))]
    struct ConformanceStack([u8; 512]);

    impl ConformanceStack {
        /// The whole buffer as a region.
        fn region(&mut self) -> KernelStackRegion {
            self.sub_region(0, self.0.len())
        }

        /// `len` bytes of the buffer starting `offset` in, so the suite can
        /// present a misaligned top or an undersized stack without naming an
        /// address it does not own.
        fn sub_region(&mut self, offset: usize, len: usize) -> KernelStackRegion {
            let base = &mut self.0[offset..offset + len];
            let ptr = NonNull::from(base).cast::<u8>();
            // SAFETY: `ptr` addresses `len` bytes of this live, uniquely
            // borrowed buffer, which outlives the region's use inside the
            // one check that consumes it.
            unsafe { KernelStackRegion::new(ptr, len) }
        }
    }

    /// Run the entire [`ContextSwitch`] conformance suite against `cs`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if an empty context reports runnable, if
    /// a misaligned/too-small stack is *not* rejected fail-closed, or if a
    /// good stack does not yield a runnable, in-bounds frame.
    pub fn run_all<C: ContextSwitch + ?Sized>(cs: &C) {
        empty_context_is_not_runnable();
        rejects_misaligned_stack(cs);
        rejects_too_small_stack(cs);
        prepares_a_runnable_in_bounds_frame(cs);
    }

    /// A freshly built context carries a zero stack pointer and is not
    /// runnable until a frame is seeded.
    fn empty_context_is_not_runnable() {
        let ctx = TaskContext::empty();
        assert_eq!(ctx.stack_pointer, 0, "an empty context must be zeroed");
        assert!(
            !ctx.is_runnable(),
            "an empty context must not be runnable before prepare"
        );
        assert_eq!(
            TaskContext::default(),
            TaskContext::empty(),
            "the derived default must agree with empty()"
        );
    }

    /// A region whose top is not 16-byte aligned is rejected. Every
    /// target's ABI requires at least 16-byte stack alignment, so a region
    /// one byte short of the aligned buffer's end is misaligned on all of
    /// them.
    fn rejects_misaligned_stack<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut stack = ConformanceStack([0; 512]);
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        assert_eq!(
            cs.prepare(&mut ctx, stack.sub_region(0, 511), entry, 0),
            Err(PrepareError::Misaligned),
            "a misaligned stack top must be rejected"
        );
        assert!(
            !ctx.is_runnable(),
            "a rejected prepare must leave the context non-runnable"
        );
    }

    /// An aligned region far too small to hold any port's initial frame is
    /// rejected. Sixteen bytes is below every port's frame size yet keeps
    /// the top 16-byte aligned, so only the size can be what refuses it.
    fn rejects_too_small_stack<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut stack = ConformanceStack([0; 512]);
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        assert_eq!(
            cs.prepare(&mut ctx, stack.sub_region(0, 16), entry, 0),
            Err(PrepareError::TooSmall),
            "a too-small stack must be rejected"
        );
        assert!(!ctx.is_runnable());
    }

    /// A good region yields a runnable context whose seeded stack pointer
    /// lies strictly inside the supplied stack (below the top, at or above
    /// the base).
    fn prepares_a_runnable_in_bounds_frame<C: ContextSwitch + ?Sized>(cs: &C) {
        let mut stack = ConformanceStack([0; 512]);
        let region = stack.region();
        let (base, top) = (region.top_addr() - region.len() as u64, region.top_addr());
        let mut ctx = TaskContext::empty();
        let entry: TaskEntry = probe_entry;
        cs.prepare(&mut ctx, region, entry, 0x00C0_FFEE)
            .expect("a 512-byte aligned stack must seed a frame");
        assert!(ctx.is_runnable(), "a prepared context must be runnable");
        assert!(
            ctx.stack_pointer < top,
            "the seeded stack pointer must lie below the stack top"
        );
        assert!(
            ctx.stack_pointer >= base,
            "the seeded stack pointer must lie within the supplied stack"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{
            ContextSwitch, KernelStackRegion, PrepareError, TaskContext, TaskEntry,
        };
        use super::run_all;

        /// A faithful host double: it seeds a plausible frame through the
        /// shared region check, exactly as a real port does. `switch` is
        /// never exercised on the host, so its body is empty (the suite
        /// calls only `prepare`).
        struct CellContextSwitch;

        /// A frame size below the 512-byte conformance stack but above the
        /// 16-byte too-small probe.
        const DOUBLE_FRAME: usize = 64;

        impl ContextSwitch for CellContextSwitch {
            fn prepare(
                &self,
                ctx: &mut TaskContext,
                stack: KernelStackRegion,
                _entry: TaskEntry,
                _arg: usize,
            ) -> Result<(), PrepareError> {
                let frame = stack.seed_frame(DOUBLE_FRAME)?;
                ctx.stack_pointer = frame.addr().get() as u64;
                Ok(())
            }

            unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {}
        }

        #[test]
        fn suite_accepts_a_faithful_context_switch() {
            let cs = CellContextSwitch;
            run_all(&cs);
            let dynamic: &dyn ContextSwitch = &cs;
            run_all(dynamic);
        }

        /// A broken `prepare` that seeds whatever it is handed must be
        /// rejected by the fail-closed check, so the suite is not vacuous.
        struct LenientContextSwitch;

        impl ContextSwitch for LenientContextSwitch {
            fn prepare(
                &self,
                ctx: &mut TaskContext,
                stack: KernelStackRegion,
                _entry: TaskEntry,
                _arg: usize,
            ) -> Result<(), PrepareError> {
                // Bug: bypasses the region's alignment and size checks.
                ctx.stack_pointer = stack.top_addr().wrapping_sub(DOUBLE_FRAME as u64);
                Ok(())
            }

            unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {}
        }

        #[test]
        #[should_panic(expected = "a misaligned stack top must be rejected")]
        fn suite_rejects_a_context_switch_that_seeds_an_unchecked_stack() {
            run_all(&LenientContextSwitch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16-byte-aligned buffer to carve regions out of.
    #[repr(C, align(16))]
    struct Buf([u8; 256]);

    /// The word derivation stays inside the region and refuses anything it
    /// cannot prove is in it — the check that lets the unwinder dereference
    /// a walked address at all.
    #[test]
    fn word_ptr_derives_only_inside_the_region() {
        let mut buf = Buf([0u8; 256]);
        let r = region(&mut buf, 0, 64);
        let base = r.base_addr();

        let first = r.word_ptr(base).expect("first word is in the region");
        // SAFETY: `word_ptr` vouched for the word, and `buf` is live.
        unsafe { first.write_volatile(0xfeed_face) };
        assert_eq!(unsafe { first.read_volatile() }, 0xfeed_face);
        assert_eq!(first.addr().get() as u64, base);

        // Last whole word in, one past it out.
        assert!(r.word_ptr(base + 56).is_some());
        assert!(r.word_ptr(base + 64).is_none(), "past the top");
        assert!(r.word_ptr(base + 60).is_none(), "word straddles the top");
        assert!(r.word_ptr(base - 8).is_none(), "below the base");
        assert!(r.word_ptr(base + 4).is_none(), "misaligned");
        assert!(r.word_ptr(u64::MAX - 3).is_none(), "wrapping end");
    }

    /// A port resolves a region only when the captured `sp` is on the stack
    /// it named, and never from a range no pointer could describe.
    ///
    /// Asserted against the rule rather than the constructor: `enclosing`
    /// mints a pointer from an address for a range it accepts, which no
    /// interpreter can follow, so only the ports execute that — on target.
    #[test]
    fn enclosing_vouches_only_for_the_stack_the_cpu_is_on() {
        const LOW: u64 = 0x8000_1000;
        const HIGH: u64 = 0x8000_2000;

        let base = usize::try_from(LOW).expect("the fixture base fits a pointer");
        assert_eq!(
            KernelStackRegion::enclosing_span(LOW + 8, LOW, HIGH),
            Some((base, 0x1000)),
            "an sp on the named stack resolves to its whole span"
        );
        assert!(KernelStackRegion::enclosing_span(LOW, LOW, HIGH).is_some());
        assert!(
            KernelStackRegion::enclosing_span(HIGH - 1, LOW, HIGH).is_some(),
            "the last byte is on it"
        );

        assert!(
            KernelStackRegion::enclosing_span(LOW - 1, LOW, HIGH).is_none(),
            "below the stack"
        );
        assert!(
            KernelStackRegion::enclosing_span(HIGH, LOW, HIGH).is_none(),
            "the top is exclusive"
        );
        assert!(
            KernelStackRegion::enclosing_span(LOW, HIGH, LOW).is_none(),
            "inverted range"
        );
        assert!(
            KernelStackRegion::enclosing_span(LOW, LOW, LOW).is_none(),
            "empty range"
        );
        assert!(
            KernelStackRegion::enclosing_span(0, 0, HIGH).is_none(),
            "a null base is never a stack"
        );
    }

    /// The walk window is read back off the region's own pointer, so the
    /// range the walk validates against cannot drift from the root its
    /// reads are derived from.
    #[test]
    fn walk_bounds_come_from_the_region_itself() {
        use crate::backtrace::StackBounds;
        let mut buf = Buf([0u8; 256]);
        let r = region(&mut buf, 16, 32);
        let bounds: StackBounds = r.into();
        assert_eq!(bounds, StackBounds::new(r.base_addr(), r.top_addr()));
        assert!(bounds.contains_word(r.base_addr()));
        assert!(!bounds.contains_word(r.top_addr()));
    }

    fn region(buf: &mut Buf, offset: usize, len: usize) -> KernelStackRegion {
        let ptr = NonNull::from(&mut buf.0[offset..offset + len]).cast::<u8>();
        // SAFETY: `ptr` addresses `len` bytes of the live, uniquely borrowed
        // buffer, which outlives the region.
        unsafe { KernelStackRegion::new(ptr, len) }
    }

    #[test]
    fn a_region_reports_its_extent() {
        let mut buf = Buf([0; 256]);
        let r = region(&mut buf, 0, 256);
        assert_eq!(r.len(), 256);
        assert!(!r.is_empty());
        assert_eq!(
            r.top_addr() - 256,
            r.seed_frame(256).expect("exact fit").addr().get() as u64
        );
    }

    #[test]
    fn seed_frame_refuses_a_misaligned_top() {
        let mut buf = Buf([0; 256]);
        // Room to spare, so only the unaligned top can refuse it.
        assert_eq!(
            region(&mut buf, 0, 248).seed_frame(64),
            Err(PrepareError::Misaligned)
        );
    }

    #[test]
    fn seed_frame_refuses_a_frame_that_does_not_fit() {
        let mut buf = Buf([0; 256]);
        assert_eq!(
            region(&mut buf, 0, 64).seed_frame(65),
            Err(PrepareError::TooSmall)
        );
        // The exact fit is the boundary and is accepted.
        assert!(region(&mut buf, 0, 64).seed_frame(64).is_ok());
    }

    /// An empty region names no bytes, so every frame is too small — the
    /// fail-closed replacement for the deleted null-stack refusal.
    #[test]
    fn an_empty_region_refuses_every_frame() {
        let mut buf = Buf([0; 256]);
        let r = region(&mut buf, 0, 0);
        assert!(r.is_empty());
        assert_eq!(r.seed_frame(8), Err(PrepareError::TooSmall));
    }

    #[test]
    fn task_context_layout_is_one_word() {
        assert_eq!(core::mem::size_of::<TaskContext>(), 8);
        assert_eq!(core::mem::align_of::<TaskContext>(), 8);
        assert_eq!(core::mem::offset_of!(TaskContext, stack_pointer), 0);
    }

    #[test]
    fn empty_context_is_zero_and_not_runnable() {
        let ctx = TaskContext::empty();
        assert_eq!(ctx.stack_pointer, 0);
        assert!(!ctx.is_runnable());
        let runnable = TaskContext {
            stack_pointer: 0x8000,
        };
        assert!(runnable.is_runnable());
    }
}
