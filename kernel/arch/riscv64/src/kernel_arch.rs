//! [`RiscvArch`] — the riscv64 implementation of the Arch HAL
//! ([`rustos_arch_api::SchedulerArch`]).
//!
//! Like x86_64, the riscv64 port is a pure Arch HAL implementation
//! (`AGENTS.md` §17.2): it implements [`SchedulerArch`] and exposes the
//! monotonic clock and the hart-park primitive, but it does **not**
//! name `kernel/core` or implement its `KernelArch` super-trait. The
//! downstream boot consumer wraps [`RiscvArch`] in a local `KernelArch`
//! type (orphan rules), constructs it from the device-tree timebase,
//! and hands it to `kernel_core::kernel_main`.
//!
//! # Clock
//!
//! The monotonic clock reads the architectural `time` CSR via the
//! `rdtime` instruction (always available to S-mode on the QEMU `virt`
//! board). [`RiscvArch::monotonic_ns`] converts those ticks to
//! nanoseconds using the `timebase-frequency` the boot pipeline read
//! from the device tree (`fdt`), so the conversion and the tick source
//! share one frequency (`AGENTS.md` §2.4 — no parallel measurement).
//!
//! # Host testability
//!
//! The struct and its trait wiring build on the host so the unit tests
//! in `kernel_arch_tests.rs` run under `cargo test` without a riscv64
//! target. The instruction-level primitives are gated: the riscv64
//! build reads the `time` CSR via `rdtime` and parks on `wfi`, and the
//! host build substitutes a monotonic atomic counter *solely* so the
//! host tests can exercise the ns conversion. The hart park
//! (`halt_current_hart`) is freestanding-only; the production path is
//! the `target_arch = "riscv64"` cfg and the host shims are never
//! linked into a kernel image (`AGENTS.md` §1 — no fake primitives in
//! production).

use core::sync::atomic::AtomicU64;
// `Ordering` is only referenced on the host path (bare-metal `send_ipi`
// issues an SBI call without an `Ordering`). Scoping the import avoids a
// `dead_code`/`unused_imports` warning on `target_os = "none"`.
#[cfg(any(test, not(target_os = "none")))]
use core::sync::atomic::Ordering;

use rustos_arch_api::{CpuId, CrossCpuTlbShootdown, SchedulerArch};

use crate::smp::MAX_HARTS;

/// riscv64 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
///
/// Carries the dense [`CpuId`] → hart-id map the SMP scheduler reaches
/// through: [`SchedulerArch::current_cpu`] recovers the running hart id
/// from `tp` and reverse-maps it, and [`SchedulerArch::send_ipi`]
/// forward-maps a target `CpuId` to the hart id the SBI IPI extension
/// addresses. Stable for the lifetime of the kernel image (the map is
/// populated once at construction; the host-only counters exist solely
/// for deterministic unit tests, mirroring `X86_64Arch`).
#[derive(Debug)]
pub struct RiscvArch {
    boot_cpu: CpuId,
    timebase_hz: u64,

    /// Forward map: dense `CpuId` index → hart id of that CPU. `None`
    /// for unpopulated slots. Set once at construction.
    cpu_to_hartid: [Option<CpuId>; MAX_HARTS],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range, mapped target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "riscv64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; MAX_HARTS],

    /// Host-only stray-IPI counter for unmapped / out-of-range targets.
    #[cfg_attr(all(target_arch = "riscv64", target_os = "none"), allow(dead_code))]
    host_stray_ipi: AtomicU64,
}

impl RiscvArch {
    /// Construct a single-hart handle for `boot_cpu` running on a hart
    /// whose `time` CSR advances at `timebase_hz` ticks per second.
    ///
    /// Maps `boot_cpu` to the hart id of the same numeric value (the
    /// boot-to-`BootCompleted` slice runs logical CPU 0 on hart 0). Use
    /// [`Self::with_harts`] to register a multi-hart map.
    ///
    /// `timebase_hz` must be non-zero; the boot pipeline reads it from
    /// the device tree's `/cpus` `timebase-frequency` and refuses to
    /// boot when it is absent, so [`Self::monotonic_ns`] never
    /// divides by zero.
    #[must_use]
    pub fn new(boot_cpu: CpuId, timebase_hz: u64) -> Self {
        let mut cpu_to_hartid = [None; MAX_HARTS];
        if (boot_cpu as usize) < MAX_HARTS {
            cpu_to_hartid[boot_cpu as usize] = Some(boot_cpu);
        }
        Self::from_map(boot_cpu, timebase_hz, cpu_to_hartid)
    }

    /// Construct a multi-hart handle from a dense `CpuId` → hart-id
    /// slice (`hartids[cpu] == hartid`).
    ///
    /// Entries beyond [`MAX_HARTS`] are ignored — the secondary-stack
    /// pool only covers that many harts (`crate::smp`). `boot_cpu` names
    /// the logical CPU of the boot hart.
    #[must_use]
    pub fn with_harts(boot_cpu: CpuId, timebase_hz: u64, hartids: &[CpuId]) -> Self {
        let mut cpu_to_hartid = [None; MAX_HARTS];
        let mut cpu = 0;
        while cpu < hartids.len() && cpu < MAX_HARTS {
            cpu_to_hartid[cpu] = Some(hartids[cpu]);
            cpu += 1;
        }
        Self::from_map(boot_cpu, timebase_hz, cpu_to_hartid)
    }

    fn from_map(
        boot_cpu: CpuId,
        timebase_hz: u64,
        cpu_to_hartid: [Option<CpuId>; MAX_HARTS],
    ) -> Self {
        Self {
            boot_cpu,
            timebase_hz,
            cpu_to_hartid,
            host_ipi_count: [const { AtomicU64::new(0) }; MAX_HARTS],
            host_stray_ipi: AtomicU64::new(0),
        }
    }

    /// The `time` CSR frequency this handle converts against.
    #[must_use]
    pub const fn timebase_hz(&self) -> u64 {
        self.timebase_hz
    }

    /// Hart id mapped to `cpu`, or `None` for an unpopulated slot.
    #[must_use]
    pub fn hartid_of(&self, cpu: CpuId) -> Option<CpuId> {
        let idx = usize::try_from(cpu).ok()?;
        self.cpu_to_hartid.get(idx).copied().flatten()
    }

    /// Dense `CpuId` whose mapped hart id is `hartid`, or `None` if no
    /// CPU maps to it.
    #[must_use]
    pub fn cpu_for_hartid(&self, hartid: CpuId) -> Option<CpuId> {
        let mut cpu = 0;
        while cpu < MAX_HARTS {
            if self.cpu_to_hartid[cpu] == Some(hartid) {
                #[allow(clippy::cast_possible_truncation)]
                return Some(cpu as CpuId);
            }
            cpu += 1;
        }
        None
    }

    /// Host-test accessor: total IPIs dispatched to `target`.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_ipi_count(&self, target: CpuId) -> u64 {
        let idx = match usize::try_from(target) {
            Ok(i) if i < MAX_HARTS => i,
            _ => return 0,
        };
        self.host_ipi_count[idx].load(Ordering::Relaxed)
    }

    /// Host-test accessor: IPIs whose target was unmapped / out of range.
    #[must_use]
    #[cfg(any(test, not(target_os = "none")))]
    pub fn host_stray_ipi_count(&self) -> u64 {
        self.host_stray_ipi.load(Ordering::Relaxed)
    }

    /// Monotonic nanoseconds since the `time` CSR's epoch.
    ///
    /// Reads the architectural `time` CSR and converts ticks to
    /// nanoseconds against this handle's `timebase_hz`, so the tick
    /// source and the conversion factor share one frequency
    /// (`AGENTS.md` §2.4 — no parallel measurement). The downstream
    /// `KernelArch` wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        let ticks = u128::from(read_time());
        let hz = u128::from(self.timebase_hz.max(1));
        // `ticks * 1e9 / hz` in 128-bit space cannot overflow for any
        // realistic uptime, and the `max(1)` defends a malformed
        // frequency from a division trap (`AGENTS.md` §2.9).
        let ns = ticks.saturating_mul(1_000_000_000) / hz;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

impl SchedulerArch for RiscvArch {
    fn current_cpu(&self) -> CpuId {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // Recover the running hart id from `tp` (seeded by `boot.s` /
            // `smp.s`) and reverse-map it to a dense `CpuId`. An unmapped
            // hart falls back to the boot CPU rather than inventing an id
            // (`AGENTS.md` §5.4.5 — fail closed).
            let hartid = crate::smp::current_hartid();
            self.cpu_for_hartid(hartid).unwrap_or(self.boot_cpu)
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            self.boot_cpu
        }
    }

    fn ticks_now(&self) -> u64 {
        read_time()
    }

    fn send_ipi(&self, target: CpuId) {
        // Resolve the destination hart id first. Sending to the calling
        // CPU is permitted (a self-reschedule). An unmapped / out-of-range
        // target is dropped rather than panicking — `send_ipi` is
        // best-effort, and stray IPIs are recorded for host tests.
        let Some(hartid) = self.hartid_of(target) else {
            #[cfg(any(test, not(target_os = "none")))]
            self.host_stray_ipi.fetch_add(1, Ordering::Relaxed);
            return;
        };

        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // Raise a supervisor software interrupt on the target hart
            // via the SBI IPI extension; the target's trap handler runs
            // `crate::preempt::on_software_interrupt`. The result is
            // best-effort — a malformed mask returns an SBI error that
            // the scheduler cannot act on, so it is dropped here.
            let (mask, base) = crate::sbi::hart_mask_for(hartid);
            let _ = crate::sbi::send_ipi(mask, base);
        }

        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            // Host: count the IPI against the target CPU; `hartid_of`
            // already validated the index is in range.
            let _ = hartid;
            self.host_ipi_count[target as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl CrossCpuTlbShootdown for RiscvArch {
    fn shootdown_page(&self, vaddr: u64) {
        // Invalidate the calling hart locally first: the SBI remote
        // fence below covers only the *other* harts, never the caller.
        // Both the local flush and this share the one sequence (§2.2).
        crate::paging::invalidate_page_local(vaddr);

        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // Reach every *other* online hart through the SBI RFENCE
            // firmware call. `remote_sfence_vma` returns only once those
            // harts have fenced, so the firmware performs the remote
            // acknowledge — there is no software ack loop (cf. the
            // x86_64 IPI path). A malformed mask returns an SBI error
            // the caller cannot act on, so it is dropped (over-/under-
            // fencing the *remote* set cannot corrupt the local map).
            let me = SchedulerArch::current_cpu(self);
            let page = vaddr & !(crate::paging::PAGE_SIZE as u64 - 1);
            for cpu in 0..MAX_HARTS as u32 {
                if cpu == me {
                    continue;
                }
                if let Some(hartid) = self.hartid_of(cpu) {
                    let (mask, base) = crate::sbi::hart_mask_for(hartid);
                    let _ = crate::sbi::remote_sfence_vma(
                        mask,
                        base,
                        page as usize,
                        crate::paging::PAGE_SIZE,
                    );
                }
            }
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            // Host: the local helper above was a vacuous no-op and there
            // is no firmware to call; the conformance vertical asserts
            // only that the call is total and panic-free.
            let _ = vaddr;
        }
    }
}

/// Read the architectural `time` CSR (nanosecond-resolution monotonic
/// tick source on the `virt` board).
#[cfg(target_arch = "riscv64")]
pub(crate) fn read_time() -> u64 {
    let ticks: u64;
    // SAFETY: `rdtime` reads the unprivileged `time` CSR; it has no
    // side effects and is available to S-mode on every riscv64 platform
    // RustOS targets (QEMU `virt` delegates it).
    unsafe {
        core::arch::asm!("rdtime {}", out(reg) ticks, options(nomem, nostack, preserves_flags));
    }
    ticks
}

/// Host substitute for the `time` CSR: a strictly increasing counter so
/// the unit tests below observe a monotonic clock. Never linked into a
/// kernel image (the riscv64 build uses [`read_time`] above).
#[cfg(not(target_arch = "riscv64"))]
pub(crate) fn read_time() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// Park the calling hart forever on `wfi` with interrupts disabled.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
fn park() -> ! {
    // SAFETY: clearing `sstatus.SIE` masks S-mode interrupts; `wfi` is
    // a well-defined wait-for-interrupt hint. The loop defends against
    // a spurious wake. This is the riscv64 form of the `AGENTS.md` §2
    // "never silently reset" contract.
    unsafe {
        core::arch::asm!("csrci sstatus, 2", options(nomem, nostack));
    }
    loop {
        // SAFETY: `wfi` is a hint with no architectural side effects.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Park the calling hart forever (the panic bridge and the downstream
/// `KernelArch` wrapper's `halt` both forward here). Masks S-mode
/// interrupts and spins on `wfi`.
#[cfg(all(target_arch = "riscv64", target_os = "none"))]
pub fn halt_current_hart() -> ! {
    park()
}

#[cfg(test)]
#[path = "kernel_arch_tests.rs"]
mod tests;
