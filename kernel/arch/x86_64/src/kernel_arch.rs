//! Stage 3a (c7-arch): the x86_64 [`SchedulerArch`] implementation.
//!
//! [`X86_64Arch`] is the concrete handle the architecture-neutral
//! kernel reaches for through [`SchedulerArch`]. It is the only
//! production implementation of that trait inside the workspace
//! (the host-side `TestArch` shipped by `kernel/sched` is feature-
//! gated to `test-arch` per `AGENTS.md` §1).
//!
//! # Surface
//!
//! - [`X86_64Arch::new`] — validates a `(boot_cpu_id, boot_apic_id,
//!   cpu_to_lapic)` triple parsed from the ACPI MADT and returns the
//!   handle ready to be wrapped in `Arc`.
//! - [`SchedulerArch::current_cpu`] — on bare metal, reads the LAPIC
//!   ID register and consults
//!   [`crate::preempt::cpu_id_for_lapic`]; on host builds, returns the
//!   boot CPU's dense `CpuId` so host tests of the scheduler remain
//!   deterministic (`AGENTS.md` §7 — no flaky tests).
//! - [`SchedulerArch::ticks_now`] — on bare metal, reads `RDTSC` (the
//!   invariant TSC modern x86_64 CPUs expose; QEMU advertises it); on
//!   host builds, returns a monotonically-increasing per-instance
//!   counter so [`SchedulerArch`]'s "monotonically non-decreasing"
//!   contract holds in tests too.
//! - [`SchedulerArch::send_ipi`] — on bare metal, issues a directed
//!   IPI through an ephemeral [`crate::apic::Lapic`] over
//!   [`crate::apic::VolatileLapicMmio`] at [`crate::preempt::LAPIC_BASE_PHYS`];
//!   on host builds, records the IPI in an in-instance counter so
//!   host tests can assert preemption was requested.
//! - [`halt`] — a free function that masks interrupts and parks the
//!   CPU forever on `hlt`. The companion `rustos-kernel` bin crate
//!   (Stage 3a (c7-bin)) uses it to satisfy
//!   `rustos_kernel_core::KernelArch::halt`. The trait impl lives in
//!   the bin crate because pulling `rustos-kernel-core` into the arch
//!   crate would transitively force a `#[global_allocator]` into the
//!   two pre-existing freestanding Stage-2 QEMU test bins — see the
//!   note in `kernel/arch/x86_64/Cargo.toml`.

use core::sync::atomic::AtomicU64;
// `Ordering` is only referenced on the host path (bare-metal `ticks_now`
// reads `RDTSC` and `send_ipi` writes LAPIC MMIO — neither uses an
// `Ordering`). Scoping the import avoids a `dead_code`/`unused_imports`
// warning on `target_os = "none"` without introducing a fake user.
#[cfg(any(test, not(target_os = "none")))]
use core::sync::atomic::Ordering;

use rustos_kernel_sched::{CpuId, SchedulerArch};

use crate::percpu::MAX_CPUS;

/// Failure modes of [`X86_64Arch::new`].
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ArchInitError {
    /// `boot_cpu_id` was outside `0..MAX_CPUS`.
    BootCpuOutOfRange,
    /// The slot `cpu_to_lapic[boot_cpu_id as usize]` was `None`,
    /// implying the caller forgot to populate the boot CPU's entry
    /// before constructing the handle.
    BootCpuMissingFromLapicMap,
    /// `cpu_to_lapic[boot_cpu_id as usize]` disagreed with the
    /// caller-supplied `boot_apic_id`.
    BootCpuLapicMismatch,
}

impl ArchInitError {
    /// Stable cause string for audit records (`AGENTS.md` §5.4.4).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BootCpuOutOfRange => "boot_cpu_out_of_range",
            Self::BootCpuMissingFromLapicMap => "boot_cpu_missing_from_lapic_map",
            Self::BootCpuLapicMismatch => "boot_cpu_lapic_mismatch",
        }
    }
}

/// Architecture handle for the x86_64 Tier-1 port.
///
/// Constructed once by the kernel binary's `kernel_main` from the
/// ACPI MADT, wrapped in `Arc`, and shared between every CPU through
/// `kernel_core::BootInfo`. Stable for the lifetime of the kernel
/// image — there is no mutable internal state on the bare-metal path
/// (the host-only fields exist solely to support deterministic unit
/// tests, see the module docs).
#[derive(Debug)]
pub struct X86_64Arch {
    /// Forward mapping: dense `CpuId` index → LAPIC ID of that CPU.
    ///
    /// `None` for unallocated slots above the configured CPU count.
    /// Populated once at construction; never mutated thereafter.
    cpu_to_lapic: [Option<u8>; MAX_CPUS],

    /// Dense `CpuId` of the boot processor.
    boot_cpu_id: CpuId,

    /// LAPIC ID of the boot processor — must equal
    /// `cpu_to_lapic[boot_cpu_id as usize].unwrap()`.
    boot_cpu_lapic_id: u8,

    /// Host-only monotonic counter backing [`SchedulerArch::ticks_now`].
    ///
    /// Production builds (`target_os = "none"`) read `RDTSC` instead
    /// and never touch this field; it is kept unconditionally to keep
    /// the struct layout identical between targets (`AGENTS.md` §1 —
    /// no hacks).
    //
    // Allow `dead_code` only on the bare-metal target: on the host
    // target the field is read by `ticks_now`. The justification is
    // the struct-layout invariant called out above (AGENTS.md §15.10
    // — `#[allow]` is paired with a justifying comment).
    #[cfg_attr(all(target_arch = "x86_64", target_os = "none"), allow(dead_code))]
    host_tick_counter: AtomicU64,

    /// Host-only IPI accounting — incremented on every `send_ipi`
    /// with an in-range target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "x86_64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; MAX_CPUS],

    /// Host-only stray-IPI counter for out-of-range targets.
    #[cfg_attr(all(target_arch = "x86_64", target_os = "none"), allow(dead_code))]
    host_stray_ipi: AtomicU64,
}

impl X86_64Arch {
    /// Build a validated arch handle.
    ///
    /// # Errors
    ///
    /// See [`ArchInitError`]. The constructor refuses to silently
    /// repair caller mistakes — fail closed per `AGENTS.md` §5.4.5.
    pub fn new(
        boot_cpu_id: CpuId,
        boot_cpu_lapic_id: u8,
        cpu_to_lapic: [Option<u8>; MAX_CPUS],
    ) -> Result<Self, ArchInitError> {
        let idx = usize::try_from(boot_cpu_id).map_err(|_| ArchInitError::BootCpuOutOfRange)?;
        if idx >= MAX_CPUS {
            return Err(ArchInitError::BootCpuOutOfRange);
        }
        let recorded = cpu_to_lapic[idx].ok_or(ArchInitError::BootCpuMissingFromLapicMap)?;
        if recorded != boot_cpu_lapic_id {
            return Err(ArchInitError::BootCpuLapicMismatch);
        }
        Ok(Self {
            cpu_to_lapic,
            boot_cpu_id,
            boot_cpu_lapic_id,
            host_tick_counter: AtomicU64::new(0),
            host_ipi_count: [const { AtomicU64::new(0) }; MAX_CPUS],
            host_stray_ipi: AtomicU64::new(0),
        })
    }

    /// Boot CPU's dense `CpuId`.
    #[must_use]
    pub const fn boot_cpu_id(&self) -> CpuId {
        self.boot_cpu_id
    }

    /// Boot CPU's LAPIC ID.
    #[must_use]
    pub const fn boot_cpu_lapic_id(&self) -> u8 {
        self.boot_cpu_lapic_id
    }

    /// LAPIC ID of `cpu`, or `None` for unallocated slots.
    #[must_use]
    pub fn lapic_id_of(&self, cpu: CpuId) -> Option<u8> {
        let idx = usize::try_from(cpu).ok()?;
        self.cpu_to_lapic.get(idx).copied().flatten()
    }

    /// Host-test accessor: total IPIs dispatched to `target`.
    ///
    /// Only meaningful on host builds — on bare metal the counter is
    /// never written to (the real LAPIC absorbs the IPI). Exposed so
    /// the (c7-bin) follow-up's host-side smoke tests can assert
    /// scheduler-driven preemption requested the right CPU.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_ipi_count(&self, target: CpuId) -> u64 {
        let idx = match usize::try_from(target) {
            Ok(i) if i < MAX_CPUS => i,
            _ => return 0,
        };
        self.host_ipi_count[idx].load(Ordering::Relaxed)
    }

    /// Host-test accessor: IPIs whose target was out of range.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_stray_ipi_count(&self) -> u64 {
        self.host_stray_ipi.load(Ordering::Relaxed)
    }
}

// --- SchedulerArch ----------------------------------------------------

impl SchedulerArch for X86_64Arch {
    fn current_cpu(&self) -> CpuId {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: `LAPIC_BASE_PHYS` is identity-mapped (boot.s
            // SAFETY-INVARIANT 4 — 0..4 GiB identity map). The ID
            // register is read-only and side-effect-free per Intel
            // SDM Vol 3A §11.4.6. The pointer is never aliased
            // mutably from any other context.
            let lapic_id = unsafe {
                let id_reg = (crate::preempt::LAPIC_BASE_PHYS + 0x20) as *const u32;
                (core::ptr::read_volatile(id_reg) >> 24) as u8
            };
            let mapped = crate::preempt::cpu_id_for_lapic(lapic_id);
            if mapped == u32::MAX {
                // Mapping table not yet populated — fall back to the
                // boot CPU. The bin crate populates the table before
                // unmasking interrupts, so this branch is only
                // reachable during the very first instructions after
                // the trampoline hand-off (`AGENTS.md` §5.4.5 — fail
                // closed, do not invent a CpuId).
                self.boot_cpu_id
            } else {
                mapped
            }
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            self.boot_cpu_id
        }
    }

    fn ticks_now(&self) -> u64 {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: `RDTSC` is unconditionally available on every
            // x86_64 CPU (it predates the architecture) and reads the
            // monotonically-non-decreasing time-stamp counter into
            // EDX:EAX. The instruction has no side effects and
            // touches no memory.
            let lo: u32;
            let hi: u32;
            unsafe {
                core::arch::asm!(
                    "rdtsc",
                    out("eax") lo,
                    out("edx") hi,
                    options(nomem, nostack, preserves_flags),
                );
            }
            (u64::from(hi) << 32) | u64::from(lo)
        }
        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            // Per-instance counter so parallel `cargo test` runs
            // don't share state. `fetch_add(1, Relaxed) + 1` is
            // monotonically non-decreasing.
            self.host_tick_counter.fetch_add(1, Ordering::Relaxed) + 1
        }
    }

    fn send_ipi(&self, target: CpuId) {
        // Resolve the destination LAPIC ID first. Out-of-range or
        // unallocated targets are dropped rather than panicking — the
        // architecture-neutral contract documents `send_ipi` as
        // best-effort, and stray IPIs are recorded for tests on the
        // host path only.
        let Some(target_apic_id) = self.lapic_id_of(target) else {
            #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
            self.host_stray_ipi.fetch_add(1, Ordering::Relaxed);
            return;
        };

        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            // SAFETY: `LAPIC_BASE_PHYS` is identity-mapped (boot.s
            // SAFETY-INVARIANT 4). Each CPU sees its own per-CPU
            // LAPIC at that physical address; concurrent
            // `send_ipi` calls on different CPUs therefore access
            // independent registers and do not race. Within a single
            // CPU the call is not re-entrant — the kernel-side
            // contract documents `send_ipi` as callable from process
            // context only (the timer ISR uses
            // `kernel/arch/x86_64::preempt`'s own EOI path).
            let mmio = unsafe {
                crate::apic::VolatileLapicMmio::new(crate::preempt::LAPIC_BASE_PHYS as *mut u32)
            };
            let mut lapic = crate::apic::Lapic::new(mmio);
            lapic.send_ipi(
                target_apic_id,
                crate::apic::DeliveryMode::Fixed,
                crate::preempt::TIMER_VECTOR,
            );
        }

        #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
        {
            // Host: count the IPI; ignore the resolved APIC ID.
            let _ = target_apic_id;
            // `lapic_id_of` already validated the index.
            let idx = target as usize;
            self.host_ipi_count[idx].fetch_add(1, Ordering::Relaxed);
        }
    }
}

// --- Halt -------------------------------------------------------------

/// Mask interrupts and park the CPU forever.
///
/// The bin crate's `KernelArch::halt` impl forwards here. Kept as a
/// free function (not a method) so the bin crate can call it without
/// owning an [`X86_64Arch`] (e.g. from the panic handler before
/// `BootInfo` has been constructed).
///
/// # SAFETY-INVARIANT
///
/// This function never returns. The `!` return type encodes the
/// invariant at the type level (`AGENTS.md` §2.10).
pub fn halt() -> ! {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // SAFETY: `cli` and `hlt` are unprivileged-w.r.t.-the-kernel
        // serialising instructions documented in Intel SDM Vol 2A
        // (HLT) and Vol 2B (CLI). They touch no memory, have no
        // calling-convention side effects, and the surrounding `loop`
        // guarantees the `!` return type.
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
        loop {
            // SAFETY: same justification as the `cli` above; `hlt`
            // blocks the CPU until the next external interrupt.
            // Because we masked `IF` with `cli` above, the only
            // wakeups possible are NMI and SMI, both of which return
            // here and re-execute `hlt` on the next loop iteration.
            unsafe {
                core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        // Host fallback: spin-wait forever. Host tests never invoke
        // this function — the compile-time `const _` assertion in the
        // test module proves the `-> !` signature without calling it
        // (`AGENTS.md` §7 — no flaky tests, no host-side blocking).
        loop {
            core::hint::spin_loop();
        }
    }
}

// --- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn live_map(entries: &[(usize, u8)]) -> [Option<u8>; MAX_CPUS] {
        let mut m = [None; MAX_CPUS];
        for &(idx, lapic) in entries {
            m[idx] = Some(lapic);
        }
        m
    }

    #[test]
    fn new_accepts_consistent_mapping() {
        let arch = X86_64Arch::new(0, 0xA0, live_map(&[(0, 0xA0), (1, 0xA1)]))
            .expect("valid construction");
        assert_eq!(arch.boot_cpu_id(), 0);
        assert_eq!(arch.boot_cpu_lapic_id(), 0xA0);
        assert_eq!(arch.lapic_id_of(0), Some(0xA0));
        assert_eq!(arch.lapic_id_of(1), Some(0xA1));
        assert_eq!(arch.lapic_id_of(2), None);
    }

    #[test]
    fn new_rejects_boot_cpu_out_of_range() {
        let err =
            X86_64Arch::new(u32::try_from(MAX_CPUS).unwrap(), 0, live_map(&[(0, 0)])).unwrap_err();
        assert_eq!(err, ArchInitError::BootCpuOutOfRange);
        assert_eq!(err.as_str(), "boot_cpu_out_of_range");
    }

    #[test]
    fn new_rejects_missing_boot_cpu_slot() {
        let err = X86_64Arch::new(2, 0, live_map(&[(0, 0)])).unwrap_err();
        assert_eq!(err, ArchInitError::BootCpuMissingFromLapicMap);
        assert_eq!(err.as_str(), "boot_cpu_missing_from_lapic_map");
    }

    #[test]
    fn new_rejects_lapic_id_mismatch() {
        let err = X86_64Arch::new(0, 0xAA, live_map(&[(0, 0xBB)])).unwrap_err();
        assert_eq!(err, ArchInitError::BootCpuLapicMismatch);
        assert_eq!(err.as_str(), "boot_cpu_lapic_mismatch");
    }

    #[test]
    fn current_cpu_on_host_returns_boot_cpu_id() {
        let arch = X86_64Arch::new(3, 0xC3, live_map(&[(3, 0xC3)])).unwrap();
        assert_eq!(arch.current_cpu(), 3);
    }

    #[test]
    fn ticks_now_is_monotonic_on_host() {
        let arch = X86_64Arch::new(0, 0, live_map(&[(0, 0)])).unwrap();
        let a = arch.ticks_now();
        let b = arch.ticks_now();
        let c = arch.ticks_now();
        assert!(b > a, "expected b > a, got a={a} b={b}");
        assert!(c > b, "expected c > b, got b={b} c={c}");
    }

    #[test]
    fn send_ipi_records_in_range_target_on_host() {
        let arch = X86_64Arch::new(0, 0xA0, live_map(&[(0, 0xA0), (1, 0xA1)])).unwrap();
        arch.send_ipi(1);
        arch.send_ipi(1);
        arch.send_ipi(0);
        assert_eq!(arch.host_ipi_count(1), 2);
        assert_eq!(arch.host_ipi_count(0), 1);
        assert_eq!(arch.host_stray_ipi_count(), 0);
    }

    #[test]
    fn send_ipi_drops_unmapped_target_into_stray_counter() {
        let arch = X86_64Arch::new(0, 0, live_map(&[(0, 0)])).unwrap();
        // CPU 5 is unmapped — no entry in `cpu_to_lapic`.
        arch.send_ipi(5);
        // CPU u32::MAX is out of range — `usize::try_from` succeeds
        // (u32 → usize) on 64-bit hosts but the index is OOB, so
        // `lapic_id_of` returns None and the stray counter ticks.
        arch.send_ipi(u32::MAX);
        assert_eq!(arch.host_stray_ipi_count(), 2);
        assert_eq!(arch.host_ipi_count(5), 0);
    }

    /// Compile-time proof that [`halt`] has the `-> !` signature
    /// required by `KernelArch::halt`. Calling it would block the
    /// test runner; coercing the function pointer is enough to
    /// surface a mismatched return type at build time
    /// (`AGENTS.md` §2.10 — encode the invariant in the type system).
    const _HALT_RETURNS_NEVER: fn() -> ! = halt;

    /// Compile-time proof that `X86_64Arch` implements
    /// [`SchedulerArch`]. If the impl ever regresses, this line
    /// fails to type-check.
    const _IS_SCHED_ARCH: fn(&X86_64Arch) -> u32 = <X86_64Arch as SchedulerArch>::current_cpu;
}
