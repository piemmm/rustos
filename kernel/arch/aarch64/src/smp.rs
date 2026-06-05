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
//! opt-in without a Cargo feature gate (`AGENTS.md` §2.1 — no hacks).
//!
//! # Bring-up method
//!
//! The `virt` board (and every platform RustOS' aarch64 QEMU tests run
//! on) brings secondary cores up through PSCI `CPU_ON`, with the conduit
//! (`hvc`/`smc`) discovered from the device tree ([`crate::fdt`]).
//! Non-PSCI spin-table boot (e.g. a bare Raspberry Pi 3) is a tracked
//! follow-up documented in the port `README.md`; `start_secondary`
//! fails closed for an absent PSCI method rather than faking it
//! (`AGENTS.md` §2.1 / §5.4.5).
//!
//! # Host testability
//!
//! `MAX_CPUS`, `is_valid_cpu`, the callback slot, and the
//! `StartCpuError` decode build and are unit-tested on the host. The
//! `MPIDR_EL1` read, the PSCI call, and the secondary trampoline are
//! gated to the freestanding aarch64 target.

use core::sync::atomic::{AtomicUsize, Ordering};

use rustos_arch_api::CpuId;

pub use crate::kernel_arch::MAX_CPUS;

/// `true` iff `cpu` indexes a reserved secondary-stack slot (and so the
/// `smp.s` trampoline selects a stack slice inside the reserved pool).
/// Must agree with the `SECONDARY_MAX_CPUS` `.equ` in `smp.s`.
#[must_use]
pub const fn is_valid_cpu(cpu: CpuId) -> bool {
    (cpu as usize) < MAX_CPUS
}

/// The secondary-core entry the trampoline runs, packed into a `usize`
/// (the size of a `fn` pointer) so the trampoline reads it without a
/// lock. `0` until [`set_secondary_entry`] installs it.
static SECONDARY_ENTRY_FN: AtomicUsize = AtomicUsize::new(0);

/// Failure modes of [`set_secondary_entry`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SetEntryError {
    /// An entry was already installed; the slot is set-once per boot
    /// (`AGENTS.md` §2.1).
    AlreadyInstalled,
}

/// Failure modes of `start_secondary`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum StartCpuError {
    /// `cpu` was outside `0..MAX_CPUS`, so the trampoline would select a
    /// stack slice outside the reserved pool.
    CpuIdOutOfRange,
    /// No secondary entry was installed via [`set_secondary_entry`];
    /// starting a core that would immediately park is refused so the
    /// failure is loud at the call site, not silent on the new core.
    NoEntryInstalled,
    /// The PSCI `CPU_ON` call returned an error (the core was already on,
    /// the MPIDR is invalid, the entry address was rejected, etc.); the
    /// payload is the raw PSCI status (`crate::psci::error`).
    Psci(i32),
}

impl StartCpuError {
    /// Stable cause string for audit records (`AGENTS.md` §5.4.4).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuIdOutOfRange => "cpu_id_out_of_range",
            Self::NoEntryInstalled => "no_secondary_entry_installed",
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
    let mpidr: u64;
    // SAFETY: `mrs x, MPIDR_EL1` reads the multiprocessor-affinity
    // register; it is side-effect-free and readable at EL1.
    unsafe {
        core::arch::asm!("mrs {}, MPIDR_EL1", out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    #[allow(clippy::cast_possible_truncation)]
    let id = (mpidr & MPIDR_AFFINITY_MASK) as u32;
    id
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
/// See [`StartCpuError`]. The launcher fails closed (`AGENTS.md` §5.4.5)
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

/// Address of the `_start_secondary_aarch64` trampoline published by
/// `smp.s`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn secondary_trampoline_addr() -> usize {
    extern "C" {
        fn _start_secondary_aarch64();
    }
    _start_secondary_aarch64 as *const () as usize
}

/// Rust side of the secondary trampoline.
///
/// `smp.s` jumps here, once per secondary core, after seeding a private
/// stack. It runs the installed [`set_secondary_entry`] callback; with
/// none installed it parks the core (the `start_secondary` guard makes
/// this branch unreachable in practice, but a freshly-started core must
/// never fall through to undefined instructions — `AGENTS.md` §2.9, fail
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
