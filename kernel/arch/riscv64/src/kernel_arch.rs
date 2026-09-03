//! [`RiscvArch`] — the riscv64 implementation of the Arch HAL
//! ([`tairix_arch_api::SchedulerArch`]).
//!
//! Like x86_64, the riscv64 port is a pure Arch HAL implementation: it implements [`SchedulerArch`] and exposes the
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
//! share one frequency (no parallel measurement).
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
//! linked into a kernel image (no fake primitives in
//! production).

// Both are referenced on every target now: the constructor populates the
// `&'static` per-CPU map with atomic stores, so
// `Ordering` is live on the bare-metal path too.
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_arch_api::{CpuId, CrossCpuTlbShootdown, SchedulerArch, SecondaryBringup, SmpError};

/// Sentinel stored in a [`RiscvArchStorage::cpu_to_hartid`] slot that no
/// CPU maps to. A real hart id is a [`CpuId`] (a `u32`), so `u64::MAX`
/// can never collide with a populated entry — it is the encoded `None`
/// (an unmapped slot is unambiguously absent, never a
/// guessed id).
const NO_HARTID: u64 = u64::MAX;

/// Caller-owned, `&'static` per-CPU backing for a [`RiscvArch`] handle
/// (per-CPU bookkeeping is sized by the caller from
/// discovered hardware, never a fixed `const` ceiling baked into the
/// arch crate).
///
/// The const parameter `N` is the number of logical-CPU slots the
/// constructing caller sizes for its machine: a single-hart vertical
/// uses `RiscvArchStorage<1>`, a two-hart vertical `RiscvArchStorage<2>`,
/// and a multi-hart boot path sizes `N` from the device-tree hart count.
/// The arch crate stays allocator-free (watch-out — no
/// `alloc` in a bare-metal arch crate), so the caller provides the
/// storage as a `static` (allocator-free bins) or a leaked allocation
/// (allocator-having callers); [`RiscvArch`] borrows it as `&'static`
/// slices.
#[derive(Debug)]
pub struct RiscvArchStorage<const N: usize> {
    /// Forward map: dense `CpuId` index → hart id, [`NO_HARTID`] for an
    /// unpopulated slot. Written once by the constructor through the
    /// shared `&'static` borrow (atomically, so no `&'static mut` is
    /// needed) and read-only thereafter.
    cpu_to_hartid: [AtomicU64; N],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range, mapped target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "riscv64", target_os = "none"), allow(dead_code))]
    host_ipi_count: [AtomicU64; N],
}

impl<const N: usize> RiscvArchStorage<N> {
    /// A zeroed backing: every map slot is the `u64::MAX` unmapped
    /// sentinel and every IPI counter is `0`. `const` so the
    /// allocator-free bins can place it in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_to_hartid: [const { AtomicU64::new(NO_HARTID) }; N],
            host_ipi_count: [const { AtomicU64::new(0) }; N],
        }
    }
}

impl<const N: usize> Default for RiscvArchStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

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
///
/// The per-CPU bookkeeping is borrowed from a caller-provided
/// [`RiscvArchStorage`], so the handle itself holds
/// no fixed-size array and imposes no compile-time CPU ceiling.
#[derive(Debug)]
pub struct RiscvArch {
    boot_cpu: CpuId,
    timebase_hz: u64,

    /// Forward map: dense `CpuId` index → hart id, [`NO_HARTID`] for an
    /// unpopulated slot. Borrowed from the caller's
    /// [`RiscvArchStorage`]; its length is the caller's CPU count.
    cpu_to_hartid: &'static [AtomicU64],

    /// Host-only IPI accounting — incremented on every `send_ipi` with
    /// an in-range, mapped target. Bare-metal builds never touch it.
    #[cfg_attr(all(target_arch = "riscv64", target_os = "none"), allow(dead_code))]
    host_ipi_count: &'static [AtomicU64],

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
    pub fn new<const N: usize>(
        storage: &'static RiscvArchStorage<N>,
        boot_cpu: CpuId,
        timebase_hz: u64,
    ) -> Self {
        let this = Self::from_storage(boot_cpu, timebase_hz, storage);
        // A slot beyond the caller's `N` cannot be mapped; the handle
        // then simply has no entry for `boot_cpu` (the conformance
        // suites never index it) — fail closed, never panic.
        this.store_hartid(boot_cpu, boot_cpu);
        this
    }

    /// Construct a multi-hart handle from a dense `CpuId` → hart-id
    /// slice (`hartids[cpu] == hartid`).
    ///
    /// Entries beyond the caller's storage capacity `N` are ignored —
    /// the caller sizes `N` to its discovered hart count. `boot_cpu` names the logical CPU of the boot hart.
    #[must_use]
    pub fn with_harts<const N: usize>(
        storage: &'static RiscvArchStorage<N>,
        boot_cpu: CpuId,
        timebase_hz: u64,
        hartids: &[CpuId],
    ) -> Self {
        let this = Self::from_storage(boot_cpu, timebase_hz, storage);
        for (cpu, &hartid) in hartids.iter().enumerate() {
            if let Ok(cpu) = CpuId::try_from(cpu) {
                this.store_hartid(cpu, hartid);
            }
        }
        this
    }

    fn from_storage<const N: usize>(
        boot_cpu: CpuId,
        timebase_hz: u64,
        storage: &'static RiscvArchStorage<N>,
    ) -> Self {
        Self {
            boot_cpu,
            timebase_hz,
            cpu_to_hartid: &storage.cpu_to_hartid,
            host_ipi_count: &storage.host_ipi_count,
            host_stray_ipi: AtomicU64::new(0),
        }
    }

    /// Populate dense `cpu`'s map slot with `hartid`. An out-of-range
    /// `cpu` (beyond the caller-sized capacity) is silently ignored, so
    /// a sparse or undersized storage cannot corrupt memory (fail
    /// closed). Called only at construction.
    fn store_hartid(&self, cpu: CpuId, hartid: CpuId) {
        if let Some(slot) = usize::try_from(cpu)
            .ok()
            .and_then(|idx| self.cpu_to_hartid.get(idx))
        {
            slot.store(u64::from(hartid), Ordering::Relaxed);
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
        match self.cpu_to_hartid.get(idx)?.load(Ordering::Relaxed) {
            NO_HARTID => None,
            raw => u32::try_from(raw).ok(),
        }
    }

    /// Dense `CpuId` whose mapped hart id is `hartid`, or `None` if no
    /// CPU maps to it.
    #[must_use]
    pub fn cpu_for_hartid(&self, hartid: CpuId) -> Option<CpuId> {
        // `hartid` is a `u32`, so its `u64` form is always below the
        // `NO_HARTID` (`u64::MAX`) sentinel — an unmapped slot never
        // matches.
        let target = u64::from(hartid);
        self.cpu_to_hartid
            .iter()
            .position(|slot| slot.load(Ordering::Relaxed) == target)
            .and_then(|cpu| u32::try_from(cpu).ok())
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
    /// (no parallel measurement). The downstream
    /// `KernelArch` wrapper forwards `monotonic_ns` here.
    #[must_use]
    pub fn monotonic_ns(&self) -> u64 {
        self.ticks_to_ns(read_time())
    }

    /// Convert a `time`-CSR tick span into nanoseconds against this
    /// handle's `timebase_hz` — the same frequency [`Self::monotonic_ns`]
    /// converts through (one conversion definition, shared with the
    /// aarch64 port via `tairix_arch_api::ticks_to_ns`). The downstream
    /// `KernelArch` wrapper forwards its `ticks_to_ns` here so the
    /// scheduler's per-task tick accounting reads in real time.
    #[must_use]
    pub fn ticks_to_ns(&self, ticks: u64) -> u64 {
        tairix_arch_api::ticks_to_ns(ticks, self.timebase_hz)
    }
}

impl SchedulerArch for RiscvArch {
    fn current_cpu(&self) -> CpuId {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // Recover the running hart id from `tp` (seeded by `boot.s` /
            // `smp.s`) and reverse-map it to a dense `CpuId`. An unmapped
            // hart falls back to the boot CPU rather than inventing an id
            // (fail closed).
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
            // Host: count the IPI against the target CPU. `hartid_of`
            // already validated the slot is populated, so the index is
            // in range of the same-length ledger; `get` keeps it total.
            let _ = hartid;
            if let Some(counter) = usize::try_from(target)
                .ok()
                .and_then(|idx| self.host_ipi_count.get(idx))
            {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn set_preemption(&self, armed: bool) {
        // Tickless preemption: `armed` records the
        // running task's quantum deadline (now + the per-hart interval
        // recorded by `init_local_preempt`, the single stored copy);
        // `!armed` clears it. The deadline combiner then programs the
        // single supervisor-timer one-shot to the *earlier* of this quantum
        // and any pending blocking-wait wakeup ([`Self::set_wakeup`]), so
        // neither suppresses the other. Off the freestanding target there
        // is no SBI timer, so the arming is inert.
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            let deadline = if armed {
                let quantum = crate::preempt::timer_interval_ticks();
                Some(read_time().wrapping_add(quantum.max(1)))
            } else {
                None
            };
            crate::preempt::record_quantum_deadline(deadline);
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            let _ = armed;
        }
    }

    fn set_wakeup(&self, deadline_ns: Option<u64>) {
        // The timed half of the tickless one-shot: a
        // blocking wait with a finite timeout records its soonest waiter
        // deadline here so the parked waiter is woken on time even when the
        // hart has no runnable task to preempt. Convert the absolute
        // monotonic-ns deadline to an absolute `time`-CSR tick against this
        // handle's `timebase_hz` (the same frequency `monotonic_ns`
        // converts the other way), then record it; the combiner arms
        // the one-shot to the earlier of this wakeup and any quantum. Off
        // the freestanding target the arming is inert.
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            let deadline =
                deadline_ns.map(|ns| tairix_arch_api::wakeup::ns_to_ticks(ns, self.timebase_hz));
            crate::preempt::record_wakeup_deadline(deadline);
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            let _ = deadline_ns;
        }
    }
}

impl CrossCpuTlbShootdown for RiscvArch {
    fn shootdown_page(&self, vaddr: u64) {
        self.shootdown_range(vaddr, 1);
    }

    fn publish_needs_remote(&self) -> bool {
        // Sv39 permits an implementation to cache an invalid PTE, so a
        // hart that already walked an absent leaf keeps faulting on it
        // until an `sfence.vma` discards the cached absence — the local
        // fence `paging::publish_mappings` issues covers only the
        // publishing hart. A space active on several harts (the kernel
        // remap window, the boot root) therefore owes the other harts a
        // fence as well, which the SBI RFENCE below delivers.
        true
    }

    fn shootdown_range(&self, start_vaddr: u64, page_count: usize) {
        if page_count == 0 {
            return;
        }
        // Invalidate the calling hart locally first: the SBI remote fence
        // below covers only the *other* harts, never the caller. Both the
        // local flush and this share the one sequence.
        crate::paging::invalidate_range_local(start_vaddr, page_count);

        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // Reach every *other* online hart through the SBI RFENCE
            // firmware call. `remote_sfence_vma` takes a byte *range* and
            // returns only once those harts have fenced, so one call
            // covers the whole run and the firmware performs the remote
            // acknowledge — there is no software ack loop (cf. the x86_64
            // IPI path). A malformed mask returns an SBI error the caller
            // cannot act on, so it is dropped (over-/under-fencing the
            // *remote* set cannot corrupt the local map).
            let me = SchedulerArch::current_cpu(self);
            // `usize::try_from` rather than `as`: an address never exceeds
            // `usize` on riscv64, so the `Err` arm is unreachable, but the
            // checked conversion keeps the cast lint-clean without an
            // `#[allow]`.
            let Ok(page) = usize::try_from(start_vaddr & !(crate::paging::PAGE_SIZE as u64 - 1))
            else {
                return;
            };
            let Some(size) = page_count.checked_mul(crate::paging::PAGE_SIZE) else {
                return;
            };
            // Iterate the caller-sized per-CPU map, not a fixed ceiling.
            for cpu in 0..self.cpu_to_hartid.len() {
                let Ok(cpu) = u32::try_from(cpu) else { break };
                if cpu == me {
                    continue;
                }
                if let Some(hartid) = self.hartid_of(cpu) {
                    let (mask, base) = crate::sbi::hart_mask_for(hartid);
                    let _ = crate::sbi::remote_sfence_vma(mask, base, page, size);
                }
            }
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            // Host: the local helper above was a vacuous no-op and there
            // is no firmware to call; the conformance vertical asserts
            // only that the call is total and panic-free.
            let _ = start_vaddr;
        }
    }
}

impl SecondaryBringup for RiscvArch {
    unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError> {
        // Fail closed before any firmware call: the boot hart is already
        // running, and an unmapped / out-of-range dense id has no hart to
        // target.
        if cpu == self.boot_cpu {
            return Err(SmpError::InvalidCpu);
        }
        let Some(hartid) = self.hartid_of(cpu) else {
            return Err(SmpError::InvalidCpu);
        };
        if !crate::smp::is_valid_hartid(hartid) {
            return Err(SmpError::InvalidCpu);
        }

        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            // SAFETY: the caller of this HAL method guarantees the
            // secondary entry is installed, `.bss` is zeroed, and the
            // target hart is real and parked — exactly the contract
            // `crate::smp::start_secondary` requires. The id was just
            // range-checked against the stack pool above.
            match unsafe { crate::smp::start_secondary(hartid) } {
                Ok(()) => Ok(()),
                Err(crate::smp::StartHartError::HartIdOutOfRange) => Err(SmpError::InvalidCpu),
                Err(crate::smp::StartHartError::NoEntryInstalled) => Err(SmpError::NotReady),
                Err(crate::smp::StartHartError::Sbi(status)) => {
                    Err(SmpError::StartRejected(status as i64))
                }
            }
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            // Host: there is no SBI firmware to call. Mirror the
            // bare-metal precondition so the observable contract holds —
            // refuse when no secondary entry is installed, otherwise
            // report the (range-checked) request as accepted. The real
            // hart_start is proven by the QEMU verticals.
            if crate::smp::secondary_entry_addr() == 0 {
                return Err(SmpError::NotReady);
            }
            Ok(())
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
    // TAIRiX targets (QEMU `virt` delegates it).
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
    // a spurious wake. This is the riscv64 form of the
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
