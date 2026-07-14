//! Multi-core (SMP) bring-up primitives for the aarch64 `virt` board.
//!
//! This module owns the architecture side of starting a secondary core
//! and recovering a running core's identity, mirroring riscv64's
//! [`crate::smp`] (the parity reference, `plans/WIRING.md` Stage W6):
//!
//! * `current_cpu_index` reads the running core's affinity out of
//!   `MPIDR_EL1`. On the `virt` board the linear core index is the low
//!   affinity field (`Aff0`), so this is the per-core identity the IRQ
//!   path forwards to the timer / IPI callbacks — the aarch64 analogue
//!   of riscv64 reading the hartid from `tp`. The dense
//!   `CpuId`↔`MPIDR` reconciliation for the scheduler lives in
//!   [`crate::kernel_arch::Aarch64Arch`].
//! * `set_secondary_entry` installs the set-once `extern "C"
//!   fn(CpuId) -> !` a freshly-started secondary core runs, and
//!   `start_secondary` asks PSCI to power on a parked core at the
//!   `smp.s` trampoline, which sets up that core's stack before invoking
//!   the installed entry. Storing a `fn` (not a closure) keeps the
//!   hand-off lock-free and free of a captured environment, exactly as
//!   [`crate::preempt`] does for the timer callback.
//!
//! # Why a set-once callback rather than an `extern` symbol
//!
//! The secondary trampoline must call *something* Rust-side, but binding
//! a mandatory `extern "C" fn secondary_main` would force every consumer
//! that links this crate (including the single-core boot pipeline and the
//! freestanding test bins) to define that symbol or fail to link. A
//! set-once callback (parking until installed) keeps secondary bring-up
//! opt-in without a Cargo feature gate (no hacks).
//!
//! # Bring-up methods
//!
//! The board's device tree declares how a parked secondary is started,
//! and the port implements both declared mechanisms:
//!
//! * **PSCI `CPU_ON`** (`start_secondary`) — the QEMU `virt` board and
//!   every PSCI-firmware platform — with the conduit (`hvc`/`smc`)
//!   discovered from the `/psci` node ([`crate::fdt`]). The firmware
//!   enters the started core at the `smp.s` PSCI trampoline with the
//!   dense id in `x0`.
//! * **Devicetree spin-table** (`start_secondary_spintable`) — the
//!   Raspberry Pi 4's stock firmware, whose `cpu@*` nodes declare
//!   `enable-method = "spin-table"` and a per-core `cpu-release-addr`.
//!   Releasing a core writes the spin-table trampoline's physical
//!   address to **both** release channels — the firmware's declared
//!   release word (for a core parked in the firmware stub) and the
//!   kernel's own `SECONDARY_KERNEL_RELEASE` word (for a core the
//!   firmware released straight into `boot.s`'s `_start`, which parks
//!   it polling that word) — publishes the released core's affinity in
//!   `SECONDARY_KERNEL_RELEASE_TARGET`, sweeps all three to the point of
//!   coherency, and signals `sev`. The kernel word is *shared*, so one
//!   `sev` wakes every core parked in `_start`; the target affinity is
//!   the gate that lets only the one core being released proceed while
//!   the rest re-park, so secondaries are brought up strictly one at a
//!   time (never a concurrent MMU-adopt / GIC-init race). A spin-table
//!   release carries no context register, so the released core recovers
//!   its dense id by matching its own `MPIDR_EL1` affinity against the
//!   table published by `register_secondary_affinities`; entry may be at
//!   EL2 (the Pi firmware hand-off), which the trampoline drops exactly
//!   as `_start` does (the shared `_el2_establish_and_drop` routine).
//!
//! A tree that declares neither mechanism leaves the handle without a
//! start method and every start fails closed rather than guessing.
//!
//! # Host testability
//!
//! `SecondaryStackPool`, `is_valid_cpu`, the callback slot, and the
//! `StartCpuError` decode build and are unit-tested on the host. The
//! `MPIDR_EL1` read, the PSCI call, and the secondary trampoline are
//! gated to the freestanding aarch64 target.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

/// Per-secondary-core kernel stack size, in bytes (64 KiB).
///
/// This is a fixed *per-stack bound* (like the kthread kernel stack), not
/// a CPU-count capacity, so it is a constant and not subject to the
/// "no fixed ceiling" rule — that rule governs the *number* of cores, not
/// the size of one core's stack. The number of cores
/// the pool covers is the caller-sized `N` of [`SecondaryStackPool`].
pub const SECONDARY_STACK_BYTES: usize = 1 << 16;

/// Pool base address published to the `smp.s` trampoline (`0` until a
/// [`SecondaryStackPool`] is registered). Read by the MMU-off secondary
/// stub by symbol; written once by [`SecondaryStackPool::register`].
#[no_mangle]
#[used]
static SECONDARY_STACK_BASE: AtomicU64 = AtomicU64::new(0);

/// Per-core slice stride (bytes) published to the `smp.s` trampoline
/// (`0` until a pool is registered). Read by the secondary stub by
/// symbol; written once by [`SecondaryStackPool::register`].
#[no_mangle]
#[used]
static SECONDARY_STACK_STRIDE: AtomicU64 = AtomicU64::new(0);

/// Number of logical CPUs the registered pool covers (`0` until a pool
/// is registered, so an unstarted system fails closed — every id is
/// invalid).
static SECONDARY_STACK_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Set-once guard so a second [`SecondaryStackPool::register`] is refused
/// rather than silently re-pointing the trampoline at a different pool.
static SECONDARY_STACKS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`SecondaryStackPool::register`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SecondaryStackError {
    /// A pool was already registered; the slot is set-once per boot
    /// (no silent re-pointing of the live trampoline).
    AlreadyRegistered,
}

/// One secondary core's kernel stack: a [`SECONDARY_STACK_BYTES`] buffer
/// aligned for an AArch64 stack pointer (16 bytes).
///
/// Public so an allocator-having boot path can size a **runtime** pool
/// to the discovered core count (a zeroed, leaked `[SecondaryStack]`
/// registered through [`register_secondary_stacks`]) instead of a
/// compile-time `SecondaryStackPool<N>` — the per-core stack count is
/// derived from discovery, never a baked-in ceiling.
#[repr(C, align(16))]
pub struct SecondaryStack {
    bytes: [u8; SECONDARY_STACK_BYTES],
}

impl SecondaryStack {
    // A 64 KiB zero array is a deliberately large backing (a secondary
    // core's whole kernel stack), const-evaluated into a `static`
    // `SecondaryStackPool` or written directly into a zeroed heap
    // allocation — never materialised on a runtime stack — so the
    // large-array lint does not apply (a per-stack size is a fixed
    // bound, not a runtime stack allocation).
    #[allow(clippy::large_stack_arrays)]
    const fn new() -> Self {
        Self {
            bytes: [0u8; SECONDARY_STACK_BYTES],
        }
    }
}

/// Caller-owned, `&'static` secondary-core stack pool, sized by the
/// constructing caller for its machine (the
/// secondary-bring-up stack count is derived from the-discovered core
/// count, never a fixed `const` ceiling baked into the arch crate).
///
/// The const parameter `N` is the number of logical CPUs the caller
/// intends to bring up: a single-CPU boot path needs no pool, a two-core
/// vertical uses `SecondaryStackPool<2>`, and a multi-core boot path sizes
/// `N` from the device-tree CPU count. The arch crate stays allocator-free
/// (watch-out — no `alloc` in a bare-metal arch crate,
/// which would force a heap into every freestanding bin that links it), so
/// the caller provides the storage as a `static` (allocator-free bins) or
/// a leaked allocation (allocator-having callers) and registers it through
/// [`SecondaryStackPool::register`].
///
/// Each secondary core `c` runs on `c`'s [`SECONDARY_STACK_BYTES`] slice;
/// the `smp.s` trampoline computes the slice top from the registered base
/// and stride, so the pool memory is reached only by the freshly-started
/// core that owns it.
#[repr(C, align(16))]
pub struct SecondaryStackPool<const N: usize> {
    stacks: [SecondaryStack; N],
}

impl<const N: usize> SecondaryStackPool<N> {
    /// A zeroed pool of `N` per-core stacks. `const` so the
    /// allocator-free bins can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stacks: [const { SecondaryStack::new() }; N],
        }
    }

    /// Publish this pool to the secondary-core trampoline and the
    /// [`is_valid_cpu`] bound, then return the covered CPU count `N`.
    ///
    /// Must be called on the boot core, exactly once, before any
    /// `start_secondary`. The trampoline reads the published base and
    /// stride with the MMU off, so the pool must stay mapped at its
    /// physical address for the lifetime of the kernel — the `&'static`
    /// receiver pins that.
    ///
    /// # Errors
    ///
    /// [`SecondaryStackError::AlreadyRegistered`] on the second publish
    /// (set-once per boot — never silently re-points the live trampoline).
    pub fn register(&'static self) -> Result<usize, SecondaryStackError> {
        register_secondary_stacks(&self.stacks)
    }
}

/// Publish `stacks` (one [`SecondaryStack`] slot per dense CPU id, slot
/// 0 belonging to the never-started boot CPU) to the secondary-core
/// trampoline and the [`is_valid_cpu`] bound, returning the covered CPU
/// count.
///
/// This is the runtime-sized registration path: an allocator-having
/// boot path leaks a zeroed `[SecondaryStack]` sized to the discovered
/// core count and registers it here; the `const`-generic
/// [`SecondaryStackPool::register`] delegates to the same set-once
/// body. Must be called on the boot core, exactly once, before any
/// `start_secondary`; the slice must stay mapped at its physical
/// address for the kernel's lifetime (the `&'static` pins that).
///
/// A freshly-started core reads the published globals — and writes its
/// first stack frames — with the MMU (and therefore its data cache)
/// **off**, while this boot-CPU write path runs cacheable. The publish
/// therefore clean+invalidates the globals *and* the whole stack region
/// to the point of coherency: a dirty line left over a stack slot would
/// later evict on the boot CPU and overwrite the started core's live
/// stack — real-silicon corruption cache-less QEMU cannot show.
///
/// # Errors
///
/// [`SecondaryStackError::AlreadyRegistered`] on the second publish
/// (set-once per boot — never silently re-points the live trampoline).
pub fn register_secondary_stacks(
    stacks: &'static [SecondaryStack],
) -> Result<usize, SecondaryStackError> {
    if SECONDARY_STACKS_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SecondaryStackError::AlreadyRegistered);
    }
    let base = stacks.as_ptr() as u64;
    SECONDARY_STACK_BASE.store(base, Ordering::Release);
    SECONDARY_STACK_STRIDE.store(SECONDARY_STACK_BYTES as u64, Ordering::Release);
    SECONDARY_STACK_COUNT.store(stacks.len(), Ordering::Release);
    // Make the publish observable to a core that starts with its cache
    // off: sweep the globals and the stack region to the point of
    // coherency, then `dsb sy` (inside the sweep) so the maintenance
    // completes before any `CPU_ON`. On the host the sweep is vacuous.
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_STACK_BASE) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_STACK_STRIDE) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    crate::paging::clean_invalidate_range_to_poc(base, core::mem::size_of_val(stacks) as u64);
    Ok(stacks.len())
}

impl<const N: usize> Default for SecondaryStackPool<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// `true` iff `cpu` indexes a slot inside the registered secondary-stack
/// pool, so the `smp.s` trampoline selects a stack slice that lies inside
/// it. Returns `false` for every id until a [`SecondaryStackPool`] is
/// registered (fail closed).
#[must_use]
pub fn is_valid_cpu(cpu: CpuId) -> bool {
    (cpu as usize) < SECONDARY_STACK_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
fn reset_secondary_stacks_for_tests() {
    SECONDARY_STACKS_REGISTERED.store(false, Ordering::Release);
    SECONDARY_STACK_COUNT.store(0, Ordering::Release);
    SECONDARY_STACK_BASE.store(0, Ordering::Release);
    SECONDARY_STACK_STRIDE.store(0, Ordering::Release);
}

/// The secondary-core entry the trampoline runs, packed into a `usize`
/// (the size of a `fn` pointer) so the trampoline reads it without a
/// lock. `0` until [`set_secondary_entry`] installs it.
static SECONDARY_ENTRY_FN: AtomicUsize = AtomicUsize::new(0);

/// Base address of the published dense-id → `MPIDR_EL1`-affinity table
/// (`0` until [`register_secondary_affinities`] publishes it). Read by
/// symbol, MMU-off, by the `smp.s` spin-table trampoline: a released
/// core carries no context register, so it linearly matches its own
/// affinity against this table to recover its dense [`CpuId`] (entry
/// `i` is dense id `i`'s affinity).
#[no_mangle]
#[used]
static SECONDARY_AFFINITY_BASE: AtomicU64 = AtomicU64::new(0);

/// Number of entries in the published affinity table (`0` until
/// published). Read by symbol, MMU-off, by the `smp.s` spin-table
/// trampoline; an affinity not found within this bound parks the core
/// (fail closed).
#[no_mangle]
#[used]
static SECONDARY_AFFINITY_COUNT: AtomicU64 = AtomicU64::new(0);

/// The kernel's own spin-table release word, polled MMU-off by the
/// `wfe` park loop in `boot.s`'s `_start` (the park every non-boot core
/// enters when firmware releases all cores straight to the kernel).
/// `0` parks; a non-zero value is the physical entry address a *released*
/// core branches to. Written only by `start_secondary_spintable`.
///
/// This word gates *whether* a release is in progress; it does not name
/// *which* core — [`SECONDARY_KERNEL_RELEASE_TARGET`] does. Both cores
/// parked here poll both words: a woken core branches only when the
/// release word is non-zero **and** the target affinity is its own.
#[no_mangle]
#[used]
static SECONDARY_KERNEL_RELEASE: AtomicU64 = AtomicU64::new(0);

/// The `MPIDR_EL1` affinity (`Aff0`–`Aff2`, [`MPIDR_AFFINITY_MASK`]) of
/// the single secondary the boot CPU is releasing *right now*, polled
/// MMU-off alongside [`SECONDARY_KERNEL_RELEASE`] by the `boot.s` park
/// loop. Written only by `start_secondary_spintable`, immediately before
/// the release word, and swept to the point of coherency before the
/// `sev`.
///
/// This is what serialises secondary bring-up to one core at a time, and
/// it gates **both** release channels because both converge on the same
/// entry: the `boot.s` `_start` park loop (a core the firmware released
/// straight into the kernel) and the `smp.s`
/// `_start_secondary_spintable_aarch64` trampoline (a core the firmware
/// released from its own spin table). Each polls this word MMU-off and
/// proceeds only when it equals the core's own masked affinity; the rest
/// re-park. The gate lives at the trampoline too because a firmware
/// spin-table release must **not** be trusted to wake only one core — a
/// shared/aliased `cpu-release-addr`, or one `sev` waking every parked
/// core, can deliver several cores into the trampoline at once. Without
/// the gate they would race *all* released secondaries through the
/// concurrent MMU-adopt / GIC-init path (observed on a Raspberry Pi 4 as
/// the last-released core deterministically faulting mid-bring-up and
/// never checking in). The exact predicate is [`release_gate_open`]. The
/// all-ones initial value matches no real masked affinity, so the gate is
/// closed until a deliberate release opens it (fail closed).
#[no_mangle]
#[used]
static SECONDARY_KERNEL_RELEASE_TARGET: AtomicU64 = AtomicU64::new(u64::MAX);

/// The per-core release-gate predicate the `boot.s` park loop and the
/// `smp.s` spin-table trampoline both implement in assembly: a woken
/// secondary may leave its `wfe` park and begin bring-up only when the
/// published release target (`SECONDARY_KERNEL_RELEASE_TARGET`) names
/// its own masked ([`MPIDR_AFFINITY_MASK`]) `MPIDR_EL1` affinity.
///
/// This is the single, host-tested definition of the gate decision — the
/// two assembly sites compile the same `cmp target, affinity` / `b.eq`
/// against it, so the rule can never drift between them. It is a pure
/// comparison with no ambient state, so it is exhaustively testable off
/// the metal even though the parked-core polling itself is not.
///
/// `target` is the value read from `SECONDARY_KERNEL_RELEASE_TARGET`
/// and `own_affinity` the core's masked affinity; the all-ones initial
/// target names no real affinity, so the gate stays shut until a
/// deliberate release opens it for exactly one core.
#[must_use]
pub const fn release_gate_open(target: u64, own_affinity: u64) -> bool {
    target == own_affinity
}

/// Set-once guard for [`register_secondary_affinities`].
static SECONDARY_AFFINITIES_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Failure mode of [`register_secondary_affinities`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SecondaryAffinityError {
    /// A table was already published; the slot is set-once per boot (no
    /// silent re-pointing of the live trampoline's identity source).
    AlreadyRegistered,
}

/// Publish the dense-id → affinity table the spin-table trampoline
/// recovers a released core's dense [`CpuId`] from (entry `i` is dense
/// id `i`'s `MPIDR_EL1` affinity, `Aff0`–`Aff2` masked), returning the
/// covered CPU count.
///
/// Must be called on the boot core, exactly once, before any
/// spin-table release; the slice must stay mapped at its physical
/// address for the kernel's lifetime (the `&'static` pins that). The
/// released core reads the table with the MMU (and cache) off, so the
/// publish sweeps the pointer, the count, and the table itself to the
/// point of coherency.
///
/// # Errors
///
/// [`SecondaryAffinityError::AlreadyRegistered`] on the second publish.
pub fn register_secondary_affinities(
    affinities: &'static [u64],
) -> Result<usize, SecondaryAffinityError> {
    if SECONDARY_AFFINITIES_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(SecondaryAffinityError::AlreadyRegistered);
    }
    let base = affinities.as_ptr() as u64;
    // Publish the count only after the base so an MMU-off reader that
    // observes a non-zero base sees a bounded, coherent table; the PoC
    // sweeps below order both ahead of any release.
    SECONDARY_AFFINITY_BASE.store(base, Ordering::Release);
    SECONDARY_AFFINITY_COUNT.store(affinities.len() as u64, Ordering::Release);
    crate::paging::clean_invalidate_range_to_poc(base, core::mem::size_of_val(affinities) as u64);
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_AFFINITY_BASE) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_AFFINITY_COUNT) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    Ok(affinities.len())
}

/// `true` iff a dense-id → affinity table has been published for the
/// spin-table trampoline. Test/diagnostic observer.
#[must_use]
pub fn secondary_affinities_registered() -> bool {
    SECONDARY_AFFINITY_BASE.load(Ordering::Acquire) != 0
}

#[cfg(test)]
fn reset_secondary_affinities_for_tests() {
    SECONDARY_AFFINITIES_REGISTERED.store(false, Ordering::Release);
    SECONDARY_AFFINITY_BASE.store(0, Ordering::Release);
    SECONDARY_AFFINITY_COUNT.store(0, Ordering::Release);
}

/// Failure modes of [`set_secondary_entry`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetEntryError {
    /// An entry was already installed; the slot is set-once per boot.
    AlreadyInstalled,
}

/// Failure modes of `start_secondary`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartCpuError {
    /// `cpu` was outside the registered [`SecondaryStackPool`]'s core
    /// count, so the trampoline would select a stack slice outside it.
    CpuIdOutOfRange,
    /// No secondary entry was installed via [`set_secondary_entry`];
    /// starting a core that would immediately park is refused so the
    /// failure is loud at the call site, not silent on the new core.
    NoEntryInstalled,
    /// No dense-id → affinity table was published via
    /// [`register_secondary_affinities`]; a spin-table-released core
    /// could not recover its dense id, so the release is refused at the
    /// call site rather than parking the core in the trampoline.
    NoAffinityTable,
    /// The PSCI `CPU_ON` call returned an error (the core was already on,
    /// the MPIDR is invalid, the entry address was rejected, etc.); the
    /// payload is the raw PSCI status (`crate::psci::error`).
    Psci(i32),
}

impl StartCpuError {
    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuIdOutOfRange => "cpu_id_out_of_range",
            Self::NoEntryInstalled => "no_secondary_entry_installed",
            Self::NoAffinityTable => "no_secondary_affinity_table",
            Self::Psci(_) => "psci_cpu_on_failed",
        }
    }
}

/// Install the entry a secondary core runs once started.
///
/// The function must be `-> !`: a secondary core has nowhere to return
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
        .map_err(|_| SetEntryError::AlreadyInstalled)?;
    // The freshly-started core reads this slot before it enables its
    // MMU/cache; push the cacheable write to the point of coherency so
    // the MMU-off read observes it (vacuous on the host).
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_ENTRY_FN) as u64,
        core::mem::size_of::<AtomicUsize>() as u64,
    );
    Ok(())
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

/// Mask isolating the affinity fields (`Aff0`–`Aff2`) of `MPIDR_EL1`.
/// The reserved bits (`RES1` at 31, `U` at 30, `MT` at 24) sit above the
/// `Aff2` byte and are excluded, so the masked value is the pure core
/// affinity the `virt` board assigns linearly.
pub const MPIDR_AFFINITY_MASK: u64 = 0x00FF_FFFF;

/// Read the calling core's dense id from its `MPIDR_EL1` affinity.
///
/// On the QEMU `virt` board the boot loader assigns each core an affinity
/// equal to its linear index (`Aff0 = index` for the small core counts
/// RustOS' tests use), so the low affinity byte is the dense [`CpuId`].
/// This is the aarch64 analogue of riscv64's `current_hartid` and is the
/// id the IRQ path forwards to the per-core timer / IPI callbacks.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn current_cpu_index() -> CpuId {
    #[allow(clippy::cast_possible_truncation)]
    {
        current_affinity() as CpuId
    }
}

/// Read the calling core's full `MPIDR_EL1` affinity (`Aff0`–`Aff2`,
/// [`MPIDR_AFFINITY_MASK`]) — the value a device tree's `/cpus/cpu@*`
/// `reg` carries, so the boot path can locate the running boot core
/// inside the discovered CPU list.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn current_affinity() -> u64 {
    let mpidr: u64;
    // SAFETY: `mrs x, MPIDR_EL1` reads the multiprocessor-affinity
    // register; it is side-effect-free and readable at EL1.
    unsafe {
        core::arch::asm!("mrs {}, MPIDR_EL1", out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    mpidr & MPIDR_AFFINITY_MASK
}

/// Host substitute for the `MPIDR_EL1` affinity read: the single-core
/// host build always reports core `0`. Never linked into a kernel image
/// (the aarch64 build uses the `mrs` reader above).
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
#[must_use]
pub fn current_cpu_index() -> CpuId {
    0
}

/// Power on the parked secondary core `cpu` (whose firmware identity is
/// `target_mpidr`) at the `smp.s` trampoline, via PSCI `CPU_ON`.
///
/// Validates the id against the stack pool and confirms an entry is
/// installed, then issues the PSCI call through the `method` conduit. On
/// success the target core runs the trampoline (which sets up its stack
/// and tail-calls the [`set_secondary_entry`] callback with `cpu`).
///
/// # Errors
///
/// See [`StartCpuError`]. The launcher fails closed
/// rather than assuming the core came up.
///
/// # Safety
///
/// Must be called from the boot core after `boot.s` has zeroed `.bss`
/// (so the secondary stack pool is clear) and after the secondary entry
/// is installed. `target_mpidr` must name a real, parked core distinct
/// from the caller, and `cpu` must be the dense id the rest of the kernel
/// uses for it.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn start_secondary(
    method: crate::fdt::PsciMethod,
    cpu: CpuId,
    target_mpidr: u64,
) -> Result<(), StartCpuError> {
    if !is_valid_cpu(cpu) {
        return Err(StartCpuError::CpuIdOutOfRange);
    }
    if secondary_entry_addr() == 0 {
        return Err(StartCpuError::NoEntryInstalled);
    }
    let entry = secondary_trampoline_addr() as u64;
    // SAFETY: `entry` is the physical address of the `smp.s` trampoline
    // (the image runs with the MMU off), `cpu` is in range, and the
    // caller's contract guarantees `target_mpidr` names a real parked
    // core. `cpu` is handed back as the trampoline's `context_id`.
    let ret = unsafe { crate::psci::cpu_on(method, target_mpidr, entry, u64::from(cpu)) };
    if ret.is_success() {
        Ok(())
    } else {
        Err(StartCpuError::Psci(ret.status))
    }
}

/// Release the **single** parked secondary core `cpu` (whose masked
/// `MPIDR_EL1` affinity is `target_affinity`) through the Devicetree
/// spin-table protocol: publish `target_affinity` as the released
/// target, write the spin-table trampoline's physical address to the
/// firmware-declared release word `release_addr` *and* to the kernel's
/// own `SECONDARY_KERNEL_RELEASE` park word, sweep all three to the
/// point of coherency, and signal `sev`.
///
/// Two release channels because the parked core's location depends on
/// the firmware: a core still spinning in the firmware stub polls its
/// own per-core `release_addr`, while a core the firmware released
/// straight into the kernel image polls the shared kernel word in
/// `boot.s`'s `_start` park loop. Whichever loop the core is in, it
/// branches to the same trampoline, which recovers its dense id from the
/// published affinity table.
///
/// **The release is one core at a time.** The firmware `release_addr` is
/// already per-core, but the kernel park word is *shared* — a single
/// `sev` wakes every core parked in `_start`. So the boot CPU also
/// publishes `target_affinity` into [`SECONDARY_KERNEL_RELEASE_TARGET`],
/// and the park loop only lets the core whose affinity matches proceed;
/// the rest re-park. This is a correctness requirement, not a nicety:
/// waking all secondaries at once would race them through the concurrent
/// MMU-adopt / GIC-init path simultaneously, which on a Raspberry Pi 4
/// intermittently faults the last-released core mid-bring-up so it never
/// checks in. The target is stored *before* the release word and swept
/// ahead of the `sev`, so a woken core that observes the open gate also
/// observes the correct target. A core not described in the affinity
/// table parks again in the trampoline (fail closed).
///
/// Validates the id against the stack pool and confirms the entry and
/// the affinity table are installed **before** any write, so a released
/// core can never wake into a half-published hand-off.
///
/// # Errors
///
/// See [`StartCpuError`]. The launcher fails closed rather than
/// assuming the core came up (a spin-table release has no firmware
/// status to inspect; a core that never polls simply stays offline and
/// the system continues on the cores that are online).
///
/// # Safety
///
/// Must be called from the boot core after `boot.s` has zeroed `.bss`
/// (so the secondary stack pool is clear) and after the secondary entry
/// and affinity table are installed. `release_addr` must be the
/// firmware-declared `cpu-release-addr` word for `cpu` — an
/// identity-mapped, writable physical word the kernel image does not
/// occupy — `cpu` must be the dense id the rest of the kernel uses for
/// the core parked on it, and `target_affinity` must be that core's
/// masked ([`MPIDR_AFFINITY_MASK`]) `MPIDR_EL1` affinity (the value the
/// park loop compares its own affinity against).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn start_secondary_spintable(
    cpu: CpuId,
    release_addr: u64,
    target_affinity: u64,
) -> Result<(), StartCpuError> {
    if !is_valid_cpu(cpu) {
        return Err(StartCpuError::CpuIdOutOfRange);
    }
    if secondary_entry_addr() == 0 {
        return Err(StartCpuError::NoEntryInstalled);
    }
    if !secondary_affinities_registered() {
        return Err(StartCpuError::NoAffinityTable);
    }
    let entry = spintable_trampoline_addr() as u64;
    // Name the core being released *before* opening the gate, so a core
    // that wakes on the `sev` and sees the release word non-zero also
    // sees the affinity it must match. Only the matching core proceeds;
    // the rest re-park (the park loop polls both words each iteration).
    SECONDARY_KERNEL_RELEASE_TARGET.store(target_affinity, Ordering::Release);
    // SAFETY: the caller's contract guarantees `release_addr` is the
    // firmware's declared release word — identity-mapped RAM outside
    // the kernel image — so the volatile store writes only the word the
    // parked core polls.
    unsafe {
        core::ptr::write_volatile(release_addr as *mut u64, entry);
    }
    SECONDARY_KERNEL_RELEASE.store(entry, Ordering::Release);
    // The parked cores poll with their MMU (and cache) off: push the
    // target, the firmware release word, and the kernel release word to
    // the point of coherency, then wake the `wfe` loops. The last
    // sweep's own `dsb sy` orders every store ahead of `sev`.
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_KERNEL_RELEASE_TARGET) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    crate::paging::clean_invalidate_range_to_poc(release_addr, core::mem::size_of::<u64>() as u64);
    crate::paging::clean_invalidate_range_to_poc(
        core::ptr::addr_of!(SECONDARY_KERNEL_RELEASE) as u64,
        core::mem::size_of::<AtomicU64>() as u64,
    );
    // SAFETY: `sev` is a hint instruction with no operands or side
    // effects beyond waking `wfe` waiters.
    unsafe {
        core::arch::asm!("sev", options(nomem, nostack, preserves_flags));
    }
    Ok(())
}

/// Address of the `_start_secondary_aarch64` trampoline published by
/// `smp.s`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn secondary_trampoline_addr() -> usize {
    extern "C" {
        fn _start_secondary_aarch64();
    }
    _start_secondary_aarch64 as *const () as usize
}

/// Address of the `_start_secondary_spintable_aarch64` trampoline
/// published by `smp.s` — the argument-free entry a spin-table release
/// branches a parked core to.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn spintable_trampoline_addr() -> usize {
    extern "C" {
        fn _start_secondary_spintable_aarch64();
    }
    _start_secondary_spintable_aarch64 as *const () as usize
}

/// Rust side of the secondary trampoline.
///
/// `smp.s` jumps here, once per secondary core, after seeding a private
/// stack. It runs the installed [`set_secondary_entry`] callback; with
/// none installed it parks the core (the `start_secondary` guard makes
/// this branch unreachable in practice, but a freshly-started core must
/// never fall through to undefined instructions, fail
/// closed).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[no_mangle]
extern "C" fn rustos_arch_aarch64_secondary_main(cpu: CpuId) -> ! {
    let raw = SECONDARY_ENTRY_FN.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: every store into the slot round-trips a valid
        // `extern "C" fn(CpuId) -> !` pointer through
        // `set_secondary_entry`; the callback is a `fn` with no captured
        // environment, safe to invoke on this core.
        let entry: extern "C" fn(CpuId) -> ! =
            unsafe { core::mem::transmute::<usize, extern "C" fn(CpuId) -> !>(raw) };
        entry(cpu);
    }
    crate::kernel_arch::halt_current_cpu()
}

#[cfg(test)]
#[path = "smp_tests.rs"]
mod tests;
