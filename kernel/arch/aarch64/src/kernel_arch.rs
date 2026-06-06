//! [`Aarch64Arch`] — the aarch64 implementation of the Arch HAL
//! ([`rustos_arch_api::SchedulerArch`]).
//!
//! Like x86_64 and riscv64, the aarch64 port is a pure Arch HAL
//! implementation (`AGENTS.md` §17.2): it implements [`SchedulerArch`]
//! and exposes the monotonic clock and a CPU-park primitive, but it does
//! **not** name `kernel/core` or implement its `KernelArch` super-trait.
//! The downstream boot consumer wraps [`Aarch64Arch`] in a local
//! `KernelArch` type (orphan rules) and hands it to
//! `kernel_core::kernel_main`.
//!
//! # Clock
//!
//! The monotonic clock reads the architectural physical counter
//! `CNTPCT_EL0`; [`Aarch64Arch::monotonic_ns`] converts those ticks to
//! nanoseconds using the counter frequency `CNTFRQ_EL0` reports (passed
//! to the constructor), so the conversion factor and the tick source
//! share one frequency (`AGENTS.md` §2.4 — no parallel measurement).
//!
//! # Inter-processor interrupts
//!
//! [`SchedulerArch::send_ipi`] raises a GICv2 software-generated
//! interrupt (SGI) on the target CPU through [`crate::gic`]. Sending to
//! the calling CPU is permitted (a self-reschedule). The boot/timer
//! slice runs a single CPU; the host build records the request in an
//! in-memory ledger so unit tests can assert preemption was requested.
//!
//! # Host testability
//!
//! The struct and its trait wiring build on the host so the unit tests
//! run under `cargo test`. The instruction-level primitives are gated:
//! the aarch64 build reads `CNTPCT_EL0` and parks on `wfi`, and the host
//! build substitutes a monotonic atomic counter *solely* so the host
//! tests can exercise the ns conversion (`AGENTS.md` §1 — no fake
//! primitives in production).

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use rustos_arch_api::{CoreClass, CpuId, CrossCpuTlbShootdown, SchedulerArch};

/// Maximum number of logical CPUs the per-CPU accounting arrays cover.
/// The boot/timer slice brings up one; the bound is headroom for the
/// SMP follow-up and keeps the host IPI ledger fixed-size.
pub const MAX_CPUS: usize = 8;

/// aarch64 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
///
/// Stable for the lifetime of the kernel image. The host-only counters
/// exist solely for deterministic unit tests, mirroring `X86_64Arch` and
/// `RiscvArch`.
#[derive(Debug)]
pub struct Aarch64Arch {
    boot_cpu: CpuId,
    timer_hz: u64,

    /// Forward map: dense `CpuId` index → `MPIDR_EL1` affinity of that
    /// CPU. `None` for unpopulated slots. Set once at construction.
    /// [`SchedulerArch::current_cpu`] reverse-maps the running core's
    /// affinity through it, and the SMP launcher forward-maps a dense id
    /// to the MPIDR PSCI `CPU_ON` addresses.
    cpu_to_mpidr: [Option<u64>; MAX_CPUS],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; MAX_CPUS],

    /// Host-only stray-IPI counter for out-of-range targets.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_stray_ipi: AtomicU64,

    /// Static [`CoreClass`] of each CPU, indexed by dense [`CpuId`].
    ///
    /// Initialised to [`CoreClass::Performance`] (a homogeneous machine).
    /// [`Self::classify_from_fdt`] rewrites the table from the device
    /// tree's per-core `capacity-dmips-mhz` ratings at boot, and the
    /// scheduler reads it through the [`SchedulerArch::core_class`]
    /// override so it can place background work on the efficiency cores of
    /// a `big.LITTLE` part (`AGENTS.md` §17.2 — static per-CPU identity
    /// discovered by the arch port).
    core_classes: [AtomicU8; MAX_CPUS],
}

impl Aarch64Arch {
    /// Construct a single-CPU handle for `boot_cpu` running on a CPU
    /// whose physical counter advances at `timer_hz` ticks per second
    /// (the value `CNTFRQ_EL0` reports).
    ///
    /// `timer_hz` must be non-zero; the boot pipeline reads it from
    /// `CNTFRQ_EL0` (see `read_cntfrq`) and refuses to boot when it is
    /// zero, so [`Self::monotonic_ns`] never divides by zero.
    #[must_use]
    pub fn new(boot_cpu: CpuId, timer_hz: u64) -> Self {
        let mut cpu_to_mpidr = [None; MAX_CPUS];
        if (boot_cpu as usize) < MAX_CPUS {
            // On the `virt` board the boot core's affinity equals its
            // dense index; `with_cpus` registers a full multi-core map.
            cpu_to_mpidr[boot_cpu as usize] = Some(u64::from(boot_cpu));
        }
        Self::from_map(boot_cpu, timer_hz, cpu_to_mpidr)
    }

    /// Construct a multi-core handle from a dense `CpuId` → `MPIDR_EL1`
    /// affinity slice (`mpidrs[cpu] == affinity`).
    ///
    /// Entries beyond [`MAX_CPUS`] are ignored — the secondary-stack pool
    /// (`crate::smp`) only covers that many cores. `boot_cpu` names the
    /// dense id of the boot core.
    #[must_use]
    pub fn with_cpus(boot_cpu: CpuId, timer_hz: u64, mpidrs: &[u64]) -> Self {
        let mut cpu_to_mpidr = [None; MAX_CPUS];
        let mut cpu = 0;
        while cpu < mpidrs.len() && cpu < MAX_CPUS {
            cpu_to_mpidr[cpu] = Some(mpidrs[cpu]);
            cpu += 1;
        }
        Self::from_map(boot_cpu, timer_hz, cpu_to_mpidr)
    }

    fn from_map(boot_cpu: CpuId, timer_hz: u64, cpu_to_mpidr: [Option<u64>; MAX_CPUS]) -> Self {
        Self {
            boot_cpu,
            timer_hz,
            cpu_to_mpidr,
            host_ipi_count: [const { AtomicU64::new(0) }; MAX_CPUS],
            host_stray_ipi: AtomicU64::new(0),
            core_classes: [const { AtomicU8::new(CoreClass::Performance.as_u8()) }; MAX_CPUS],
        }
    }

    /// Record the [`CoreClass`] discovered for dense `cpu`.
    ///
    /// An out-of-range `cpu` is ignored — the table is bounded to
    /// [`MAX_CPUS`], so a stray call cannot corrupt memory (`AGENTS.md`
    /// §5.4 fail-closed). Mirrors `X86_64Arch::record_core_class`.
    pub fn record_core_class(&self, cpu: CpuId, class: CoreClass) {
        if let Some(slot) = usize::try_from(cpu)
            .ok()
            .and_then(|idx| self.core_classes.get(idx))
        {
            slot.store(class.as_u8(), Ordering::Relaxed);
        }
    }

    /// Discover the per-CPU [`CoreClass`] table from the device tree.
    ///
    /// Walks every `/cpus/cpu@*` node (via [`rustos_fdt::Fdt::each_cpu`]),
    /// maps each node's `reg` (its `MPIDR_EL1` affinity) to a dense
    /// [`CpuId`] through this handle's affinity map, and classifies the
    /// collected `capacity-dmips-mhz` ratings with
    /// [`crate::hetcore::classify_by_capacity`]. A malformed tree, or a
    /// CPU node whose affinity is not in the map, leaves that core at the
    /// [`CoreClass::Performance`] default rather than guessing
    /// (`AGENTS.md` §2.9 — fail conservative). The downstream boot
    /// consumer calls this once on the boot core after building the
    /// affinity map.
    pub fn classify_from_fdt(&self, fdt: &crate::fdt::Fdt<'_>) {
        let mut capacities: [Option<u64>; MAX_CPUS] = [None; MAX_CPUS];
        // A malformed tree yields the all-`None` homogeneous default.
        let _ = fdt.each_cpu(|mpidr, capacity| {
            if let Some(idx) = self
                .cpu_for_mpidr(mpidr)
                .and_then(|cpu| usize::try_from(cpu).ok())
            {
                if let Some(slot) = capacities.get_mut(idx) {
                    *slot = capacity;
                }
            }
        });
        for (idx, class) in crate::hetcore::classify_by_capacity(&capacities)
            .into_iter()
            .enumerate()
        {
            if let Ok(cpu) = CpuId::try_from(idx) {
                self.record_core_class(cpu, class);
            }
        }
    }

    /// `MPIDR_EL1` affinity mapped to dense `cpu`, or `None` for an
    /// unpopulated slot. The SMP launcher hands this to PSCI `CPU_ON`.
    #[must_use]
    pub fn mpidr_of(&self, cpu: CpuId) -> Option<u64> {
        let idx = usize::try_from(cpu).ok()?;
        self.cpu_to_mpidr.get(idx).copied().flatten()
    }

    /// Dense `CpuId` whose mapped affinity is `mpidr`, or `None` if no
    /// CPU maps to it.
    #[must_use]
    pub fn cpu_for_mpidr(&self, mpidr: u64) -> Option<CpuId> {
        let mut cpu = 0;
        while cpu < MAX_CPUS {
            if self.cpu_to_mpidr[cpu] == Some(mpidr) {
                #[allow(clippy::cast_possible_truncation)]
                return Some(cpu as CpuId);
            }
            cpu += 1;
        }
        None
    }

    /// The counter frequency this handle converts against.
    #[must_use]
    pub const fn timer_hz(&self) -> u64 {
        self.timer_hz
    }

    /// Host-test accessor: total IPIs dispatched to `target`.
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

    /// Monotonic nanoseconds since the physical counter's epoch.
    ///
    /// Reads `CNTPCT_EL0` and converts ticks to nanoseconds against this
    /// handle's `timer_hz`, so the tick source and the conversion factor
    /// share one frequency (`AGENTS.md` §2.4). The downstream
    /// `KernelArch` wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        let ticks = u128::from(read_cntpct());
        let hz = u128::from(self.timer_hz.max(1));
        // `ticks * 1e9 / hz` in 128-bit space cannot overflow for any
        // realistic uptime, and the `max(1)` defends a malformed
        // frequency from a division trap (`AGENTS.md` §2.9).
        let ns = ticks.saturating_mul(1_000_000_000) / hz;
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

impl SchedulerArch for Aarch64Arch {
    fn current_cpu(&self) -> CpuId {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // Recover the running core's affinity (`crate::smp` reads
            // `MPIDR_EL1`) and reverse-map it to a dense `CpuId`. An
            // unmapped core falls back to the boot CPU rather than
            // inventing an id (`AGENTS.md` §5.4.5 — fail closed).
            let affinity = u64::from(crate::smp::current_cpu_index());
            self.cpu_for_mpidr(affinity).unwrap_or(self.boot_cpu)
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            self.boot_cpu
        }
    }

    fn ticks_now(&self) -> u64 {
        read_cntpct()
    }

    fn core_class(&self, cpu: CpuId) -> CoreClass {
        // Out-of-range CPUs report the safe homogeneous default per the
        // Arch HAL contract; a stored byte is always a valid encoding
        // because `record_core_class` only writes `CoreClass::as_u8`.
        usize::try_from(cpu)
            .ok()
            .and_then(|idx| self.core_classes.get(idx))
            .map_or(CoreClass::Performance, |slot| {
                CoreClass::from_u8(slot.load(Ordering::Relaxed)).unwrap_or(CoreClass::Performance)
            })
    }

    fn send_ipi(&self, target: CpuId) {
        if usize::try_from(target).map_or(true, |i| i >= MAX_CPUS) {
            #[cfg(any(test, not(target_os = "none")))]
            self.host_stray_ipi.fetch_add(1, Ordering::Relaxed);
            return;
        }

        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // Raise a GICv2 software-generated interrupt on the target
            // CPU; its IRQ exception path runs the scheduler entry. The
            // result is best-effort — a single-CPU image targets itself.
            crate::gic::send_sgi(target);
        }

        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            self.host_ipi_count[target as usize].fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl CrossCpuTlbShootdown for Aarch64Arch {
    fn shootdown_page(&self, vaddr: u64) {
        // aarch64 needs no IPI or software acknowledge: `tlbi vaae1is`
        // is the *inner-shareable broadcast* invalidation, so the same
        // instruction the local flush issues already reaches every PE in
        // the domain. Both paths funnel through the one shared sequence
        // (`AGENTS.md` §2.2); the `dsb ish` + `isb` inside it provide the
        // ordering the cross-CPU contract requires.
        crate::paging::invalidate_page_inner_shareable(vaddr);
    }
}

/// Read the architectural physical counter `CNTPCT_EL0` (the monotonic
/// tick source on the `virt` board).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub(crate) fn read_cntpct() -> u64 {
    let ticks: u64;
    // SAFETY: `CNTPCT_EL0` is the unprivileged physical counter; reading
    // it has no side effects and is accessible at EL1 (and at EL0/EL1
    // after `boot.s` enables `CNTHCTL_EL2.EL1PCTEN` when entered at EL2).
    unsafe {
        core::arch::asm!("mrs {}, CNTPCT_EL0", out(reg) ticks, options(nomem, nostack, preserves_flags));
    }
    ticks
}

/// Host substitute for `CNTPCT_EL0`: a strictly increasing counter so
/// the unit tests observe a monotonic clock. Never linked into a kernel
/// image (the bare-metal aarch64 build uses the `mrs` reader above).
///
/// Gated on "not bare-metal aarch64" rather than "not aarch64" so a
/// hosted aarch64 development machine (e.g. an Apple-silicon or ARM
/// Linux host running `cargo test`) also uses this deterministic
/// substitute instead of the real `CNTPCT_EL0`, whose coarse tick can
/// read identically across two adjacent calls. Mirrors the gating in
/// the x86_64 and riscv64 backends.
#[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
pub(crate) fn read_cntpct() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

/// Read the counter frequency `CNTFRQ_EL0` reports (Hz). On the `virt`
/// board QEMU programs this to the host timer frequency.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn read_cntfrq() -> u64 {
    let hz: u64;
    // SAFETY: `CNTFRQ_EL0` is readable at EL1; the read has no side
    // effects.
    unsafe {
        core::arch::asm!("mrs {}, CNTFRQ_EL0", out(reg) hz, options(nomem, nostack, preserves_flags));
    }
    hz
}

/// Park the calling CPU forever on `wfi` with interrupts disabled.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
fn park() -> ! {
    // SAFETY: `msr DAIFSet, #0xf` masks all interrupts; `wfi` is a
    // well-defined wait-for-interrupt hint. The loop defends against a
    // spurious wake. This is the aarch64 form of the `AGENTS.md` §2
    // "never silently reset" contract.
    unsafe {
        core::arch::asm!("msr DAIFSet, #0xf", options(nomem, nostack));
    }
    loop {
        // SAFETY: `wfi` is a hint with no architectural side effects.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Park the calling CPU forever (the panic bridge and the downstream
/// `KernelArch` wrapper's `halt` both forward here). Masks interrupts
/// and spins on `wfi`.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn halt_current_cpu() -> ! {
    park()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_ns_uses_the_handle_frequency() {
        // Host `read_cntpct` increments by one per call; with a 1 GHz
        // frequency one tick is one nanosecond, so successive reads are
        // strictly increasing and scale by the frequency.
        let arch = Aarch64Arch::new(0, 1_000_000_000);
        let a = arch.monotonic_ns();
        let b = arch.monotonic_ns();
        assert!(b > a, "clock must be monotonically increasing");
    }

    #[test]
    fn zero_frequency_does_not_divide_by_zero() {
        let arch = Aarch64Arch::new(0, 0);
        // Must not panic; `max(1)` guards the divide.
        let _ = arch.monotonic_ns();
    }

    #[test]
    fn current_cpu_reports_the_boot_cpu_on_host() {
        let arch = Aarch64Arch::new(3, 1_000);
        assert_eq!(arch.current_cpu(), 3);
    }

    #[test]
    fn send_ipi_counts_in_range_targets_and_strays() {
        let arch = Aarch64Arch::new(0, 1_000);
        arch.send_ipi(1);
        arch.send_ipi(1);
        assert_eq!(arch.host_ipi_count(1), 2);
        // Out-of-range target is recorded as a stray, never panics.
        arch.send_ipi(u32::try_from(MAX_CPUS).unwrap());
        assert_eq!(arch.host_stray_ipi_count(), 1);
    }

    #[test]
    fn with_cpus_round_trips_the_mpidr_map_both_ways() {
        // A two-core `virt` board: dense CpuId 0/1 → affinity 0/1.
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0, 1]);
        assert_eq!(arch.mpidr_of(0), Some(0));
        assert_eq!(arch.mpidr_of(1), Some(1));
        assert_eq!(arch.cpu_for_mpidr(0), Some(0));
        assert_eq!(arch.cpu_for_mpidr(1), Some(1));
        // An unpopulated slot / unmapped affinity is `None`, not a guess.
        assert_eq!(arch.mpidr_of(2), None);
        assert_eq!(arch.cpu_for_mpidr(7), None);
    }

    #[test]
    fn with_cpus_supports_a_sparse_affinity_layout() {
        // Affinity need not equal the dense index (a clustered MPIDR):
        // dense 1 maps to affinity 0x100 (Aff1 = 1).
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0x000, 0x100]);
        assert_eq!(arch.mpidr_of(1), Some(0x100));
        assert_eq!(arch.cpu_for_mpidr(0x100), Some(1));
    }

    #[test]
    fn new_maps_the_boot_cpu_to_its_own_affinity() {
        let arch = Aarch64Arch::new(0, 1_000);
        assert_eq!(arch.mpidr_of(0), Some(0));
        assert_eq!(arch.cpu_for_mpidr(0), Some(0));
    }

    #[test]
    fn core_class_defaults_to_performance_before_discovery() {
        // No FDT discovery has run: every CPU, and any out-of-range id,
        // is the safe homogeneous default.
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0, 1]);
        assert_eq!(arch.core_class(0), CoreClass::Performance);
        assert_eq!(arch.core_class(1), CoreClass::Performance);
        assert_eq!(arch.core_class(CpuId::MAX), CoreClass::Performance);
    }

    #[test]
    fn classify_from_fdt_reports_big_little_cores() {
        // A 2+2 big.LITTLE `virt`-shaped tree: affinities 0/1 are the big
        // cores (cap 1024), 0x100/0x101 the LITTLE cores (cap 512). The
        // affinity map places them at dense ids 0..=3.
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0x0, 0x1, 0x100, 0x101]);
        let blob = rustos_fdt::fixture::arm_with_cpus(
            0x4000_0000,
            0x2000_0000,
            &[
                (0x0, Some(1024)),
                (0x1, Some(1024)),
                (0x100, Some(512)),
                (0x101, Some(512)),
            ],
        );
        let fdt = crate::fdt::Fdt::new(&blob).expect("valid fdt");
        arch.classify_from_fdt(&fdt);
        assert_eq!(arch.core_class(0), CoreClass::Performance);
        assert_eq!(arch.core_class(1), CoreClass::Performance);
        assert_eq!(arch.core_class(2), CoreClass::Efficiency);
        assert_eq!(arch.core_class(3), CoreClass::Efficiency);
        // An out-of-range id stays the safe default; totality holds.
        assert_eq!(arch.core_class(CpuId::MAX), CoreClass::Performance);
    }

    #[test]
    fn classify_from_fdt_is_homogeneous_without_capacities() {
        // A tree whose cpu nodes advertise no capacity leaves every core
        // a performance core (a homogeneous machine).
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0x0, 0x1]);
        let blob = rustos_fdt::fixture::arm_with_cpus(
            0x4000_0000,
            0x2000_0000,
            &[(0x0, None), (0x1, None)],
        );
        let fdt = crate::fdt::Fdt::new(&blob).expect("valid fdt");
        arch.classify_from_fdt(&fdt);
        assert_eq!(arch.core_class(0), CoreClass::Performance);
        assert_eq!(arch.core_class(1), CoreClass::Performance);
    }

    /// §17.2 / W6: the port passes the cross-CPU TLB-shootdown
    /// conformance vertical over its real `Aarch64Arch` handle. On the
    /// host the broadcast `tlbi` is a vacuous no-op (no TLB), so the
    /// vertical asserts the observable half — the call is total and
    /// panic-free for any address. The real inner-shareable broadcast is
    /// proven by `cross_cpu_tlb_shootdown_qemu_aarch64`.
    #[test]
    fn passes_cross_cpu_tlb_shootdown_conformance() {
        let arch = Aarch64Arch::with_cpus(0, 1_000, &[0, 1]);
        rustos_arch_api::xtlb::conformance::run_all(&arch, 64u64 << 30);
        let erased: &dyn CrossCpuTlbShootdown = &arch;
        rustos_arch_api::xtlb::conformance::run_all(erased, 64u64 << 30);
    }

    /// §17.2 / W0: the port passes the shared Arch HAL conformance
    /// vertical over its real `SchedulerArch`, `SideChannel`,
    /// `MemoryTags`, discovery, and per-CPU storage handles
    /// (`plans/WIRING.md` Stage W0 / W2).
    #[test]
    fn passes_arch_hal_conformance_suite() {
        let arch = Aarch64Arch::new(0, 1_000);
        let blob = rustos_fdt::fixture::virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = crate::fdt::Fdt::new(&blob).expect("valid fdt");
        let discovery = crate::platform::FdtDiscovery::new(fdt);
        rustos_arch_api::conformance::run_all(
            &arch,
            &crate::sidechannel::SideChannel::new(),
            &crate::memtag::MemoryTags::new(),
            &discovery,
            &crate::percpu_hal::PerCpuStorage::new(),
        );
    }
}
