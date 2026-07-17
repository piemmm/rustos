//! Multi-hart (SMP) bring-up primitives for the riscv64 `virt` board.
//!
//! This module owns the architecture side of starting a secondary hart
//! and recovering a running hart's identity:
//!
//! * [`current_hartid`] reads the hart id back out of the `tp` register,
//!   which both the boot trampoline (`boot.s`) and the secondary stub
//!   (`smp.s`) seed with the SBI-handed hartid. The architecture-neutral
//!   kernel never reads `tp`; it works in dense [`CpuId`]s, and the
//!   [`crate::kernel_arch::RiscvArch`] handle maps between the two.
//! * [`set_secondary_entry`] installs the set-once `extern "C" fn(CpuId)
//!   -> !` a freshly-started secondary hart runs, and `start_secondary`
//!   asks SBI HSM to start a parked hart at the `smp.s` trampoline,
//!   which sets up that hart's stack and `tp` before invoking the
//!   installed entry. Storing a `fn` (not a closure) keeps
//!   the hand-off lock-free and free of a captured environment, exactly
//!   as [`crate::preempt`] does for the timer callback.
//!
//! # Why a set-once callback rather than an `extern` symbol
//!
//! The secondary trampoline must call *something* Rust-side, but binding
//! a mandatory `extern "C" fn secondary_main` would force every consumer
//! that links this crate (including the single-hart boot pipeline and
//! the Stage-2 freestanding test bins) to define that symbol or fail to
//! link. A set-once callback (parking until installed) keeps secondary
//! bring-up opt-in without a Cargo feature gate (no
//! hacks — no feature-flag silencing).
//!
//! # Host testability
//!
//! [`SecondaryStackPool`], [`is_valid_hartid`], the callback slot, and
//! the [`StartHartError`] decode build and are unit-tested on the host.
//! The `tp` read, the SBI HSM call, and the secondary trampoline are
//! gated to the freestanding riscv64 target.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tairix_arch_api::CpuId;

/// Base-2 logarithm of the per-secondary-hart kernel stack size, so the
/// `smp.s` trampoline can index a hart's slice with a left shift rather
/// than the `M` multiply extension (which the freestanding stub avoids).
///
/// A per-stack size is a fixed *bound* (like the kthread kernel stack),
/// not a hart-count capacity, so it is a constant and not subject to the
/// "no fixed ceiling" rule — that rule governs the *number* of
/// harts, not the size of one hart's stack. The
/// number of harts the pool covers is the caller-sized `N` of
/// [`SecondaryStackPool`].
pub const SECONDARY_STACK_SHIFT: u32 = 14;

/// Per-secondary-hart kernel stack size, in bytes (16 KiB). A power of
/// two so the `smp.s` slice index is a shift (see [`SECONDARY_STACK_SHIFT`]).
pub const SECONDARY_STACK_BYTES: usize = 1 << SECONDARY_STACK_SHIFT;

/// Pool base address published to the `smp.s` trampoline (`0` until a
/// [`SecondaryStackPool`] is registered). Read by the paging-off
/// secondary stub by symbol; written once by
/// [`SecondaryStackPool::register`].
#[no_mangle]
#[used]
static SECONDARY_STACK_BASE: AtomicU64 = AtomicU64::new(0);

/// Per-hart slice log2 byte size published to the `smp.s` trampoline
/// (`0` until a pool is registered). Read by the secondary stub by
/// symbol; written once by [`SecondaryStackPool::register`] (always
/// [`SECONDARY_STACK_SHIFT`]).
#[no_mangle]
#[used]
static SECONDARY_STACK_SHIFT_BITS: AtomicU64 = AtomicU64::new(0);

/// Number of harts the registered pool covers (`0` until a pool is
/// registered, so an unstarted system fails closed — every id is
/// invalid).
static SECONDARY_STACK_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`SecondaryStackPool::register`] is refused
/// rather than silently re-pointing the live trampoline at a different
/// pool.
static SECONDARY_STACKS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`SecondaryStackPool::register`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SecondaryStackError {
    /// A pool was already registered; the slot is set-once per boot
    /// (no silent re-pointing of the live trampoline).
    AlreadyRegistered,
}

/// One secondary hart's kernel stack: a [`SECONDARY_STACK_BYTES`] buffer
/// aligned for a riscv64 stack pointer (16 bytes).
#[repr(C, align(16))]
struct SecondaryStackSlot {
    bytes: [u8; SECONDARY_STACK_BYTES],
}

impl SecondaryStackSlot {
    // A 16 KiB zero array is a deliberately large *static* backing (a
    // secondary hart's whole kernel stack), only ever const-evaluated
    // into a `static SecondaryStackPool` — never materialised on a
    // runtime stack — so the large-array lint does not apply (: a per-stack size is a fixed bound, not a runtime stack
    // allocation).
    #[allow(clippy::large_stack_arrays)]
    const fn new() -> Self {
        Self {
            bytes: [0u8; SECONDARY_STACK_BYTES],
        }
    }
}

/// Caller-owned, `&'static` secondary-hart stack pool, sized by the
/// constructing caller for its machine (the
/// secondary-bring-up stack count is derived from the-discovered hart
/// count, never a fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of harts the caller intends to
/// bring up: a single-hart boot path needs no pool, a two-hart vertical
/// uses `SecondaryStackPool<2>`, and a multi-hart boot path sizes `N`
/// from the device-tree hart count. The arch crate stays allocator-free
/// (watch-out — no `alloc` in a bare-metal arch crate,
/// which would force a heap into every freestanding bin that links it),
/// so the caller provides the storage as a `static` (allocator-free bins)
/// or a leaked allocation (allocator-having callers) and registers it
/// through [`SecondaryStackPool::register`].
///
/// Each secondary hart `h` runs on `h`'s [`SECONDARY_STACK_BYTES`] slice;
/// the `smp.s` trampoline computes the slice top from the registered base
/// and log2 size, so the pool memory is reached only by the freshly-
/// started hart that owns it.
#[repr(C, align(16))]
pub struct SecondaryStackPool<const N: usize> {
    stacks: [SecondaryStackSlot; N],
}

impl<const N: usize> SecondaryStackPool<N> {
    /// A zeroed pool of `N` per-hart stacks. `const` so the
    /// allocator-free bins can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stacks: [const { SecondaryStackSlot::new() }; N],
        }
    }

    /// Publish this pool to the secondary-hart trampoline and the
    /// [`is_valid_hartid`] bound, then return the covered hart count `N`.
    ///
    /// Must be called on the boot hart, exactly once, before any
    /// `start_secondary`. The trampoline reads the published base and
    /// log2 size with paging off, so the pool must stay mapped at its
    /// physical address for the lifetime of the kernel — the `&'static`
    /// receiver pins that.
    ///
    /// # Errors
    ///
    /// [`SecondaryStackError::AlreadyRegistered`] on the second publish
    /// (set-once per boot — never silently re-points the live trampoline).
    pub fn register(&'static self) -> Result<usize, SecondaryStackError> {
        if SECONDARY_STACKS_REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SecondaryStackError::AlreadyRegistered);
        }
        let base = self.stacks.as_ptr() as u64;
        SECONDARY_STACK_BASE.store(base, Ordering::Release);
        SECONDARY_STACK_SHIFT_BITS.store(u64::from(SECONDARY_STACK_SHIFT), Ordering::Release);
        SECONDARY_STACK_COUNT.store(N, Ordering::Release);
        // Order the pool publish ahead of any secondary hart's paging-off
        // read of the globals. The SBI `hart_start` firmware call that
        // starts a hart is itself a barrier, but the explicit `fence`
        // makes the ordering local and unconditional (explicit synchronisation for cross-CPU shared state).
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        // SAFETY: `fence rw, rw` orders prior stores ahead of later
        // memory accesses; it has no operands and no effect beyond
        // ordering, and is always valid in S-mode.
        unsafe {
            core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
        }
        Ok(N)
    }
}

impl<const N: usize> Default for SecondaryStackPool<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// `true` iff `hartid` indexes a slot inside the registered secondary-
/// stack pool, so the `smp.s` trampoline selects a stack slice that lies
/// inside it. Returns `false` for every id until a [`SecondaryStackPool`]
/// is registered (fail closed).
#[must_use]
pub fn is_valid_hartid(hartid: CpuId) -> bool {
    (hartid as usize) < SECONDARY_STACK_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
fn reset_secondary_stacks_for_tests() {
    SECONDARY_STACKS_REGISTERED.store(false, Ordering::Release);
    SECONDARY_STACK_COUNT.store(0, Ordering::Release);
    SECONDARY_STACK_BASE.store(0, Ordering::Release);
    SECONDARY_STACK_SHIFT_BITS.store(0, Ordering::Release);
}

/// The secondary-hart entry the trampoline runs, packed into a `usize`
/// (the size of a `fn` pointer) so the trampoline reads it without a
/// lock. `0` until [`set_secondary_entry`] installs it.
static SECONDARY_ENTRY_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_secondary_entry`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetEntryError {
    /// An entry was already installed; the slot is set-once per boot.
    AlreadyInstalled,
}

/// Failure modes of `start_secondary`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartHartError {
    /// `hartid` was outside the registered [`SecondaryStackPool`]'s hart
    /// count, so the trampoline would select a stack slice outside it.
    HartIdOutOfRange,
    /// No secondary entry was installed via [`set_secondary_entry`];
    /// starting a hart that would immediately park is refused so the
    /// failure is loud at the call site, not silent on the new hart.
    NoEntryInstalled,
    /// The SBI HSM `hart_start` call returned an error (the hart was
    /// already started, the id is invalid to SBI, etc.).
    Sbi(isize),
}

impl StartHartError {
    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HartIdOutOfRange => "hartid_out_of_range",
            Self::NoEntryInstalled => "no_secondary_entry_installed",
            Self::Sbi(_) => "sbi_hart_start_failed",
        }
    }
}

/// Install the entry a secondary hart runs once started.
///
/// The function must be `-> !`: a secondary hart has nowhere to return
/// to (the trampoline's only fallback is a `wfi` park). Encoding the
/// bottom type in the signature pins that at the call site.
///
/// # Errors
///
/// [`SetEntryError::AlreadyInstalled`] on the second publish.
pub fn set_secondary_entry(entry: extern "C" fn(CpuId) -> !) -> Result<(), SetEntryError> {
    let raw = entry as usize;
    SECONDARY_ENTRY_FN
        .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| SetEntryError::AlreadyInstalled)
}

/// Address of the installed secondary entry (`0` if none).
/// Test/diagnostic observer.
#[must_use]
pub fn secondary_entry_addr() -> usize {
    SECONDARY_ENTRY_FN.load(Ordering::Acquire)
}

#[cfg(test)]
fn clear_secondary_entry_for_tests() {
    SECONDARY_ENTRY_FN.store(0, Ordering::Release);
}

/// Read the calling hart's id from the `tp` register.
///
/// Both `boot.s` (boot hart) and `smp.s` (secondary harts) seed `tp`
/// with the SBI-handed hartid before entering Rust, so this is a
/// side-effect-free per-CPU identity read.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[must_use]
pub fn current_hartid() -> CpuId {
    let tp: u64;
    // SAFETY: reading `tp` has no side effects. The boot/secondary
    // trampolines guarantee it holds this hart's id (inside the
    // registered pool's hart count), which fits a `CpuId` (`u32`).
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp, options(nomem, nostack, preserves_flags));
    }
    #[allow(clippy::cast_possible_truncation)]
    let id = tp as u32;
    id
}

/// Host substitute for the `tp` hart-identity read: the single-hart
/// host build always reports hart `0`. Never linked into a kernel image
/// (the riscv64 build uses the `tp` read above).
#[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
#[must_use]
pub fn current_hartid() -> CpuId {
    0
}

/// Start the parked secondary hart `hartid` at the `smp.s` trampoline.
///
/// Validates the id against the stack pool and confirms an entry is
/// installed, then issues the SBI HSM `hart_start` call. On success the
/// target hart runs the trampoline, which sets up its stack and `tp`
/// and tail-calls the [`set_secondary_entry`] callback.
///
/// # Errors
///
/// See [`StartHartError`]. The launcher fails closed rather than assuming the hart came up.
///
/// # Safety
///
/// Must be called from the boot hart after a [`SecondaryStackPool`] has
/// been registered (the trampoline reads its published base/shift) and
/// after the secondary entry is installed. `hartid` must name a real,
/// parked hart distinct from the caller.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub unsafe fn start_secondary(hartid: CpuId) -> Result<(), StartHartError> {
    if !is_valid_hartid(hartid) {
        return Err(StartHartError::HartIdOutOfRange);
    }
    if secondary_entry_addr() == 0 {
        return Err(StartHartError::NoEntryInstalled);
    }
    let entry = secondary_trampoline_addr();
    let ret = crate::sbi::hart_start(hartid, entry, 0);
    if ret.is_success() {
        Ok(())
    } else {
        Err(StartHartError::Sbi(ret.error))
    }
}

/// Address of the `_start_secondary` trampoline published by `smp.s`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn secondary_trampoline_addr() -> usize {
    extern "C" {
        fn _start_secondary();
    }
    _start_secondary as *const () as usize
}

/// Rust side of the secondary trampoline.
///
/// `smp.s` jumps here, once per secondary hart, after seeding `tp` and a
/// private stack. It runs the installed [`set_secondary_entry`]
/// callback; with none installed it parks the hart (the
/// [`start_secondary`] guard makes this branch unreachable in practice,
/// but a freshly-started hart must never fall through to undefined
/// instructions, fail closed).
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
#[no_mangle]
extern "C" fn tairix_arch_riscv64_secondary_main(hartid: CpuId) -> ! {
    let raw = SECONDARY_ENTRY_FN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: every store into the slot round-trips a valid
        // `extern "C" fn(CpuId) -> !` pointer through
        // `set_secondary_entry`; the callback is a `fn` with no captured
        // environment, safe to invoke on this hart.
        let entry: extern "C" fn(CpuId) -> ! =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId) -> !>(raw) };
        entry(hartid);
    }
    crate::kernel_arch::halt_current_hart()
}

#[cfg(test)]
#[path = "smp_tests.rs"]
mod tests;
