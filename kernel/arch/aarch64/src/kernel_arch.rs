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

use rustos_arch_api::{
    CoreClass, CpuId, CrossCpuTlbShootdown, SchedulerArch, SecondaryBringup, SmpError,
};

use crate::fdt::PsciMethod;

/// Sentinel stored in an [`Aarch64ArchStorage::cpu_to_mpidr`] slot that
/// no CPU maps to — the encoded `None` (`AGENTS.md` §2.9 — an unmapped
/// slot is unambiguously absent, never a guessed affinity).
///
/// A real `MPIDR_EL1` affinity can never be `u64::MAX`: bits `[63:40]`
/// are `RES0` (Arm ARM, `MPIDR_EL1`), so an all-ones value never
/// collides with a populated entry.
const NO_MPIDR: u64 = u64::MAX;

/// Caller-owned, `&'static` per-CPU backing for an [`Aarch64Arch`]
/// handle (`AGENTS.md` §24.1 — per-CPU bookkeeping is sized by the
/// caller from discovered hardware, never a fixed `const` ceiling baked
/// into the arch crate).
///
/// The const parameter `N` is the number of logical-CPU slots the
/// constructing caller sizes for its machine: a single-CPU boot path or
/// vertical uses `Aarch64ArchStorage<1>`, a two-core vertical
/// `Aarch64ArchStorage<2>`, and a multi-core boot path sizes `N` from the
/// device-tree CPU count. The arch crate stays allocator-free (`AGENTS.md`
/// §24.1 watch-out — no `alloc` in a bare-metal arch crate, which would
/// force a heap into every freestanding bin linking it), so the caller
/// provides the storage as a `static` (allocator-free bins) or a leaked
/// allocation (allocator-having callers); [`Aarch64Arch`] borrows it as
/// `&'static` slices.
#[derive(Debug)]
pub struct Aarch64ArchStorage<const N: usize> {
    /// Forward map: dense `CpuId` index → `MPIDR_EL1` affinity,
    /// `NO_MPIDR` for an unpopulated slot. Written once by the
    /// constructor through the shared `&'static` borrow (atomically, so
    /// no `&'static mut` is needed) and read-only thereafter.
    cpu_to_mpidr: [AtomicU64; N],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; N],

    /// Static [`CoreClass`] of each CPU, indexed by dense [`CpuId`].
    /// Initialised to [`CoreClass::Performance`] (a homogeneous machine);
    /// [`Aarch64Arch::classify_from_fdt`] rewrites it from the device
    /// tree's per-core `capacity-dmips-mhz` ratings at boot.
    core_classes: [AtomicU8; N],
}

impl<const N: usize> Aarch64ArchStorage<N> {
    /// A backing in which every map slot is the `NO_MPIDR` unmapped
    /// sentinel, every IPI counter is `0`, and every core defaults to
    /// [`CoreClass::Performance`]. `const` so the allocator-free bins can
    /// place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_to_mpidr: [const { AtomicU64::new(NO_MPIDR) }; N],
            host_ipi_count: [const { AtomicU64::new(0) }; N],
            core_classes: [const { AtomicU8::new(CoreClass::Performance.as_u8()) }; N],
        }
    }
}

impl<const N: usize> Default for Aarch64ArchStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// aarch64 architecture handle the downstream boot consumer wraps for
/// `kernel_core::kernel_main`.
///
/// Stable for the lifetime of the kernel image. The host-only counters
/// exist solely for deterministic unit tests, mirroring `X86_64Arch` and
/// `RiscvArch`.
///
/// The per-CPU bookkeeping is borrowed from a caller-provided
/// [`Aarch64ArchStorage`] (`AGENTS.md` §24.1), so the handle itself holds
/// no fixed-size array and imposes no compile-time CPU ceiling.
#[derive(Debug)]
pub struct Aarch64Arch {
    boot_cpu: CpuId,
    timer_hz: u64,

    /// Forward map: dense `CpuId` index → `MPIDR_EL1` affinity of that
    /// CPU, `NO_MPIDR` for unpopulated slots. Borrowed from the
    /// caller's [`Aarch64ArchStorage`]; its length is the caller's CPU
    /// count. [`SchedulerArch::current_cpu`] reverse-maps the running
    /// core's affinity through it, and the SMP launcher forward-maps a
    /// dense id to the MPIDR PSCI `CPU_ON` addresses.
    cpu_to_mpidr: &'static [AtomicU64],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_ipi_count: &'static [AtomicU64],

    /// Host-only stray-IPI counter for out-of-range targets.
    #[cfg_attr(all(target_arch = "aarch64", target_os = "none"), allow(dead_code))]
    host_stray_ipi: AtomicU64,

    /// Static [`CoreClass`] of each CPU, indexed by dense [`CpuId`].
    /// Borrowed from the caller's [`Aarch64ArchStorage`].
    ///
    /// Initialised to [`CoreClass::Performance`] (a homogeneous machine).
    /// [`Self::classify_from_fdt`] rewrites the table from the device
    /// tree's per-core `capacity-dmips-mhz` ratings at boot, and the
    /// scheduler reads it through the [`SchedulerArch::core_class`]
    /// override so it can place background work on the efficiency cores of
    /// a `big.LITTLE` part (`AGENTS.md` §17.2 — static per-CPU identity
    /// discovered by the arch port).
    core_classes: &'static [AtomicU8],

    /// PSCI conduit (`hvc`/`smc`) the [`SecondaryBringup`] path calls
    /// firmware through, discovered from the `/psci` device-tree node
    /// (`crate::fdt::psci_method`) and installed via
    /// [`Self::with_psci_method`]. `None` on a single-core / headless
    /// handle that never starts a secondary; starting one then fails
    /// closed with [`SmpError::NotReady`] (`AGENTS.md` §5.4.5).
    psci_method: Option<PsciMethod>,
}

impl Aarch64Arch {
    /// Construct a single-CPU handle for `boot_cpu` running on a CPU
    /// whose physical counter advances at `timer_hz` ticks per second
    /// (the value `CNTFRQ_EL0` reports).
    ///
    /// `timer_hz` must be non-zero; the boot pipeline reads it from
    /// `CNTFRQ_EL0` (see `read_cntfrq`) and refuses to boot when it is
    /// zero, so [`Self::monotonic_ns`] never divides by zero.
    ///
    /// The per-CPU bookkeeping is borrowed from the caller-provided
    /// `storage` (`AGENTS.md` §24.1); a single-CPU caller sizes it
    /// `Aarch64ArchStorage<1>`. Maps `boot_cpu` to an affinity of the
    /// same numeric value (the boot/timer slice runs the boot core);
    /// [`Self::with_cpus`] registers a full multi-core map.
    #[must_use]
    pub fn new<const N: usize>(
        storage: &'static Aarch64ArchStorage<N>,
        boot_cpu: CpuId,
        timer_hz: u64,
    ) -> Self {
        let this = Self::from_storage(boot_cpu, timer_hz, storage);
        // A slot beyond the caller's `N` cannot be mapped; the handle
        // then simply has no entry for `boot_cpu` — fail closed, never
        // panic (`AGENTS.md` §2.9).
        this.store_mpidr(boot_cpu, u64::from(boot_cpu));
        this
    }

    /// Construct a multi-core handle from a dense `CpuId` → `MPIDR_EL1`
    /// affinity slice (`mpidrs[cpu] == affinity`).
    ///
    /// Entries beyond the caller's storage capacity `N` are ignored — the
    /// caller sizes `N` to its discovered core count (`AGENTS.md` §24.1).
    /// `boot_cpu` names the dense id of the boot core.
    #[must_use]
    pub fn with_cpus<const N: usize>(
        storage: &'static Aarch64ArchStorage<N>,
        boot_cpu: CpuId,
        timer_hz: u64,
        mpidrs: &[u64],
    ) -> Self {
        let this = Self::from_storage(boot_cpu, timer_hz, storage);
        for (cpu, &mpidr) in mpidrs.iter().enumerate() {
            if let Ok(cpu) = CpuId::try_from(cpu) {
                this.store_mpidr(cpu, mpidr);
            }
        }
        this
    }

    fn from_storage<const N: usize>(
        boot_cpu: CpuId,
        timer_hz: u64,
        storage: &'static Aarch64ArchStorage<N>,
    ) -> Self {
        Self {
            boot_cpu,
            timer_hz,
            cpu_to_mpidr: &storage.cpu_to_mpidr,
            host_ipi_count: &storage.host_ipi_count,
            host_stray_ipi: AtomicU64::new(0),
            core_classes: &storage.core_classes,
            psci_method: None,
        }
    }

    /// Populate dense `cpu`'s map slot with `mpidr`. An out-of-range
    /// `cpu` (beyond the caller-sized capacity) is silently ignored, so a
    /// sparse or undersized storage cannot corrupt memory (`AGENTS.md`
    /// §5.4 — fail closed). Called only at construction.
    fn store_mpidr(&self, cpu: CpuId, mpidr: u64) {
        if let Some(slot) = usize::try_from(cpu)
            .ok()
            .and_then(|idx| self.cpu_to_mpidr.get(idx))
        {
            slot.store(mpidr, Ordering::Relaxed);
        }
    }

    /// Install the PSCI conduit the [`SecondaryBringup`] path calls
    /// firmware through, returning the updated handle (a builder so the
    /// existing constructors keep their signatures).
    ///
    /// The downstream boot consumer reads the conduit from the device
    /// tree (`crate::fdt::psci_method`) once on the boot core and installs
    /// it here before bringing secondaries up.
    #[must_use]
    pub fn with_psci_method(mut self, method: PsciMethod) -> Self {
        self.psci_method = Some(method);
        self
    }

    /// The PSCI conduit installed via [`Self::with_psci_method`], or
    /// `None` on a handle that never starts a secondary.
    #[must_use]
    pub const fn psci_method(&self) -> Option<PsciMethod> {
        self.psci_method
    }

    /// Record the [`CoreClass`] discovered for dense `cpu`.
    ///
    /// An out-of-range `cpu` is ignored — the table is bounded to the
    /// caller-sized [`Aarch64ArchStorage`] length, so a stray call cannot
    /// corrupt memory (`AGENTS.md` §5.4 fail-closed). Mirrors
    /// `X86_64Arch::record_core_class`.
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
    /// [`CpuId`] through this handle's affinity map, and classifies each
    /// core's `capacity-dmips-mhz` rating against the peak rating with
    /// [`crate::hetcore::class_for_capacity`]. A malformed tree, or a
    /// CPU node whose affinity is not in the map, leaves that core at the
    /// [`CoreClass::Performance`] default rather than guessing
    /// (`AGENTS.md` §2.9 — fail conservative). The downstream boot
    /// consumer calls this once on the boot core after building the
    /// affinity map.
    ///
    /// Two device-tree passes (find the peak, then classify) carry no
    /// fixed-size buffer, so the classification scales to the caller's
    /// storage length (`AGENTS.md` §24.1) rather than a fixed
    /// compile-time CPU ceiling.
    pub fn classify_from_fdt(&self, fdt: &crate::fdt::Fdt<'_>) {
        // Reset to the homogeneous default so a re-classification leaves
        // no stale efficiency class behind (idempotent, `AGENTS.md` §2.9).
        for slot in self.core_classes {
            slot.store(CoreClass::Performance.as_u8(), Ordering::Relaxed);
        }
        // Pass 1: the peak capacity over every CPU node whose affinity
        // maps to an in-range dense id. A malformed tree yields no peak
        // (the homogeneous default).
        let mut peak: Option<u64> = None;
        let _ = fdt.each_cpu(|mpidr, capacity| {
            if let (Some(cap), Some(_)) = (capacity, self.cpu_for_mpidr(mpidr)) {
                peak = Some(peak.map_or(cap, |p| p.max(cap)));
            }
        });
        // Pass 2: a core rated strictly below the peak is an efficiency
        // core; a peak-rated or unrated core stays the performance
        // default already stored above.
        let _ = fdt.each_cpu(|mpidr, capacity| {
            if let Some(cpu) = self.cpu_for_mpidr(mpidr) {
                if crate::hetcore::class_for_capacity(capacity, peak).is_efficiency() {
                    self.record_core_class(cpu, CoreClass::Efficiency);
                }
            }
        });
    }

    /// `MPIDR_EL1` affinity mapped to dense `cpu`, or `None` for an
    /// unpopulated slot. The SMP launcher hands this to PSCI `CPU_ON`.
    #[must_use]
    pub fn mpidr_of(&self, cpu: CpuId) -> Option<u64> {
        let idx = usize::try_from(cpu).ok()?;
        match self.cpu_to_mpidr.get(idx)?.load(Ordering::Relaxed) {
            NO_MPIDR => None,
            raw => Some(raw),
        }
    }

    /// Dense `CpuId` whose mapped affinity is `mpidr`, or `None` if no
    /// CPU maps to it.
    #[must_use]
    pub fn cpu_for_mpidr(&self, mpidr: u64) -> Option<CpuId> {
        // The all-ones sentinel (`NO_MPIDR`) is never a real affinity
        // (MPIDR_EL1[63:40] are RES0), so it matches no populated slot.
        if mpidr == NO_MPIDR {
            return None;
        }
        self.cpu_to_mpidr
            .iter()
            .position(|slot| slot.load(Ordering::Relaxed) == mpidr)
            .and_then(|cpu| u32::try_from(cpu).ok())
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
        usize::try_from(target)
            .ok()
            .and_then(|idx| self.host_ipi_count.get(idx))
            .map_or(0, |c| c.load(Ordering::Relaxed))
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
        // Bound by the caller-sized CPU count, not a fixed ceiling
        // (`AGENTS.md` §24.1). An out-of-range target is dropped rather
        // than panicking — `send_ipi` is best-effort, and strays are
        // recorded for host tests.
        if usize::try_from(target).map_or(true, |i| i >= self.cpu_to_mpidr.len()) {
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
            // The range check above proved the index is in the
            // same-length ledger; `get` keeps the access total.
            if let Some(counter) = usize::try_from(target)
                .ok()
                .and_then(|idx| self.host_ipi_count.get(idx))
            {
                counter.fetch_add(1, Ordering::Relaxed);
            }
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

impl SecondaryBringup for Aarch64Arch {
    unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError> {
        // Fail closed before any PSCI call: the boot core is already
        // running, and an unmapped dense id has no `MPIDR` to power on
        // (`AGENTS.md` §5.4.5).
        if cpu == self.boot_cpu {
            return Err(SmpError::InvalidCpu);
        }
        let Some(mpidr) = self.mpidr_of(cpu) else {
            return Err(SmpError::InvalidCpu);
        };
        // Bring-up needs the firmware conduit; a handle without one
        // (single-core / headless) cannot start a secondary.
        let Some(method) = self.psci_method else {
            return Err(SmpError::NotReady);
        };

        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            // SAFETY: the caller of this HAL method guarantees `.bss` is
            // zeroed (clear secondary-stack pool), the secondary entry is
            // installed, and `mpidr` names a real, parked core distinct
            // from the caller — exactly `crate::smp::start_secondary`'s
            // contract. `cpu` was range-checked via `mpidr_of`.
            match unsafe { crate::smp::start_secondary(method, cpu, mpidr) } {
                Ok(()) => Ok(()),
                Err(crate::smp::StartCpuError::CpuIdOutOfRange) => Err(SmpError::InvalidCpu),
                Err(crate::smp::StartCpuError::NoEntryInstalled) => Err(SmpError::NotReady),
                Err(crate::smp::StartCpuError::Psci(status)) => {
                    Err(SmpError::StartRejected(i64::from(status)))
                }
            }
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            // Host: there is no PSCI firmware to call. Mirror the
            // bare-metal precondition so the observable contract holds —
            // refuse when no secondary entry is installed, otherwise
            // report the (range-checked) request as accepted. The real
            // `CPU_ON` is proven by the QEMU verticals.
            let _ = (method, mpidr);
            if crate::smp::secondary_entry_addr() == 0 {
                return Err(SmpError::NotReady);
            }
            Ok(())
        }
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

/// The generic-timer frequency (Hz) to drive preemption with, honouring
/// the board's device tree: the `/timer` `clock-frequency` override when
/// the firmware tree declares one ([`crate::fdt::timer_clock_frequency`]),
/// otherwise the `CNTFRQ_EL0` register ([`read_cntfrq`]).
///
/// This is the boot path's single source of the counter rate
/// (`plans/PI.md` P4): the Raspberry Pi 4's 54 MHz crystal and the QEMU
/// `virt` board's host-derived rate both flow through here without a
/// `cfg(board)` fork (`AGENTS.md` §17.2 / §2.2). The selection logic is
/// the host-tested pure [`crate::fdt::effective_timer_hz`]; only the
/// register read is target-gated.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn timer_frequency_hz(fdt: &crate::fdt::Fdt<'_>) -> u64 {
    crate::fdt::effective_timer_hz(crate::fdt::timer_clock_frequency(fdt), read_cntfrq())
}

/// Enable Advanced SIMD / floating-point at EL1 (`CPACR_EL1.FPEN = 0b11`,
/// do-not-trap), followed by an `isb` so the change is in effect before
/// the next instruction.
///
/// The boot trampoline (`boot.s`) leaves FP/SIMD trapping, so any code
/// the compiler lowers to NEON — a vectorised `memcpy`/`memcmp`, the
/// `rxe` decoder, the log formatter — would otherwise take an
/// undefined-instruction synchronous exception (`ESR_EL1.EC = 0x07`)
/// with no vectors installed and hang the core. Every freestanding
/// aarch64 kernel calls this once on the boot CPU before running any
/// code that may use FP. This is the single definition of the enable
/// sequence (`AGENTS.md` §2.2 — no duplication); the boot consumers and
/// the QEMU verticals all call it.
///
/// # Safety
///
/// Must run on the boot CPU once, before any FP/SIMD instruction
/// executes. It writes one architectural EL1 control register and
/// grants no cross-privilege authority; it is safe to call from EL1
/// kernel context and a no-op to call again.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_fp_el1() {
    // `CPACR_EL1.FPEN` is bits [21:20]; `0b11` means "do not trap FP/SIMD
    // at EL0 or EL1".
    const FPEN_NO_TRAP: u64 = 0b11 << 20;
    // SAFETY: read-modify-write of `CPACR_EL1` (the EL1 FP/SIMD trap
    // control) followed by `isb`. Per this function's contract it runs
    // on the boot CPU before any FP instruction; the write only relaxes
    // a trap and confers no authority.
    unsafe {
        core::arch::asm!(
            "mrs {t}, CPACR_EL1",
            "orr {t}, {t}, {fpen}",
            "msr CPACR_EL1, {t}",
            "isb",
            t = out(reg) _,
            fpen = in(reg) FPEN_NO_TRAP,
            options(nostack, preserves_flags),
        );
    }
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
        static S: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 0, 1_000_000_000);
        let a = arch.monotonic_ns();
        let b = arch.monotonic_ns();
        assert!(b > a, "clock must be monotonically increasing");
    }

    #[test]
    fn zero_frequency_does_not_divide_by_zero() {
        static S: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 0, 0);
        // Must not panic; `max(1)` guards the divide.
        let _ = arch.monotonic_ns();
    }

    #[test]
    fn current_cpu_reports_the_boot_cpu_on_host() {
        static S: Aarch64ArchStorage<4> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 3, 1_000);
        assert_eq!(arch.current_cpu(), 3);
    }

    #[test]
    fn send_ipi_counts_in_range_targets_and_strays() {
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 0, 1_000);
        arch.send_ipi(1);
        arch.send_ipi(1);
        assert_eq!(arch.host_ipi_count(1), 2);
        // A target beyond the caller-sized CPU count is recorded as a
        // stray, never panics.
        arch.send_ipi(2);
        assert_eq!(arch.host_stray_ipi_count(), 1);
    }

    #[test]
    fn with_cpus_round_trips_the_mpidr_map_both_ways() {
        // A two-core `virt` board: dense CpuId 0/1 → affinity 0/1.
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0, 1]);
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
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0x000, 0x100]);
        assert_eq!(arch.mpidr_of(1), Some(0x100));
        assert_eq!(arch.cpu_for_mpidr(0x100), Some(1));
    }

    #[test]
    fn new_maps_the_boot_cpu_to_its_own_affinity() {
        static S: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 0, 1_000);
        assert_eq!(arch.mpidr_of(0), Some(0));
        assert_eq!(arch.cpu_for_mpidr(0), Some(0));
    }

    #[test]
    fn core_class_defaults_to_performance_before_discovery() {
        // No FDT discovery has run: every CPU, and any out-of-range id,
        // is the safe homogeneous default.
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0, 1]);
        assert_eq!(arch.core_class(0), CoreClass::Performance);
        assert_eq!(arch.core_class(1), CoreClass::Performance);
        assert_eq!(arch.core_class(CpuId::MAX), CoreClass::Performance);
    }

    #[test]
    fn classify_from_fdt_reports_big_little_cores() {
        // A 2+2 big.LITTLE `virt`-shaped tree: affinities 0/1 are the big
        // cores (cap 1024), 0x100/0x101 the LITTLE cores (cap 512). The
        // affinity map places them at dense ids 0..=3.
        static S: Aarch64ArchStorage<4> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0x0, 0x1, 0x100, 0x101]);
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
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0x0, 0x1]);
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
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0, 1]);
        rustos_arch_api::xtlb::conformance::run_all(&arch, 64u64 << 30);
        let erased: &dyn CrossCpuTlbShootdown = &arch;
        rustos_arch_api::xtlb::conformance::run_all(erased, 64u64 << 30);
    }

    /// §17.2 / W14: the port passes the secondary-bring-up conformance
    /// vertical over its real `Aarch64Arch` handle. On the host there is
    /// no PSCI firmware, so the vertical asserts the observable half —
    /// starting an unstartable id fails closed and never panics. The real
    /// PSCI `CPU_ON` round-trip is proven by the two-core QEMU verticals
    /// (`ipi_smp_qemu_aarch64`, `cross_cpu_tlb_shootdown_qemu_aarch64`).
    #[test]
    fn passes_secondary_bringup_conformance() {
        static S: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&S, 0, 1_000, &[0, 1]).with_psci_method(PsciMethod::Hvc);
        rustos_arch_api::smp::conformance::run_all(&arch, CpuId::MAX);
        let erased: &dyn SecondaryBringup = &arch;
        rustos_arch_api::smp::conformance::run_all(erased, CpuId::MAX);
    }

    /// The boot core and any unmapped dense id are refused before any
    /// PSCI call, and a handle with no PSCI conduit cannot start a
    /// secondary — the fail-closed contract (`AGENTS.md` §5.4.5). (The
    /// set-once secondary-entry slot is a process-global shared with
    /// `crate::smp`'s own tests, so the accepted path is exercised there,
    /// not re-driven here — no flaky cross-test state, `AGENTS.md` §7.)
    #[test]
    fn start_secondary_fails_closed_on_unstartable_ids_and_missing_conduit() {
        // SAFETY: every call below is refused before any PSCI action, so
        // the test takes no platform action and touches no shared global.
        static WITH: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        static WITHOUT: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        unsafe {
            let with_conduit =
                Aarch64Arch::with_cpus(&WITH, 0, 1_000, &[0, 1]).with_psci_method(PsciMethod::Hvc);
            // Boot core: already running.
            assert_eq!(with_conduit.start_secondary(0), Err(SmpError::InvalidCpu));
            // Unmapped dense id.
            assert_eq!(with_conduit.start_secondary(2), Err(SmpError::InvalidCpu));
            assert_eq!(
                with_conduit.start_secondary(CpuId::MAX),
                Err(SmpError::InvalidCpu)
            );
            // A mapped secondary on a handle with no PSCI conduit is
            // refused as not-ready, before any firmware call.
            let no_conduit = Aarch64Arch::with_cpus(&WITHOUT, 0, 1_000, &[0, 1]);
            assert_eq!(no_conduit.start_secondary(1), Err(SmpError::NotReady));
        }
    }

    /// §17.2 / W0: the port passes the shared Arch HAL conformance
    /// vertical over its real `SchedulerArch`, `SideChannel`,
    /// `MemoryTags`, discovery, and per-CPU storage handles
    /// (`plans/WIRING.md` Stage W0 / W2).
    #[test]
    fn passes_arch_hal_conformance_suite() {
        static S: Aarch64ArchStorage<1> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::new(&S, 0, 1_000);
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
