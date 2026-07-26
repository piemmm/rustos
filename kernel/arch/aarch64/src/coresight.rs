//! ARMv8-A CoreSight external-debug cross-core PC sampling.
//!
//! A hard-locked core that stopped taking maskable interrupts cannot
//! sample its own PC, and on a GIC whose non-maskable interrupt (FIQ,
//! Group 0) belongs to the secure world (the Raspberry Pi 4 GIC-400) no
//! interrupt-driven observer can reach it either. The one observation that
//! survives is a read of the victim's PC by *another* master over the
//! memory-mapped **external debug** interface: the PC Sample-based
//! Profiling registers (`EDPCSR`/`EDCIDSR`/`EDVIDSR`, Arm ARM DDI 0487
//! chapter H9). Reading `EDPCSR` returns a sampled program counter of the
//! target PE **without halting it** and over a channel `DAIF` cannot mask
//! — exactly the fresh datum the stale pre-silence sample cannot give.
//!
//! This module is the register-level access for that read, in the shape the
//! rest of the port already uses ([`crate::brcm_msi`], [`crate::gic`]): the
//! register offsets, the unlock/validity/assembly logic, and the honest
//! "did we get a valid sample" decision are **pure functions unit-tested on
//! the host** through the `DebugMmio` seam, while the real MMIO reads are
//! the freestanding `VolatileDebugMmio` over a **discovered** per-CPU
//! debug-component base — never a baked-in board constant. Per-CPU bases
//! are installed once at boot from the device tree (the Linux
//! `arm,coresight-cpu-debug` binding); a platform whose tree does not
//! describe the debug components installs none and the sampler honestly
//! reports [`tairix_arch_api::RemotePcSample::Unsupported`] (fail closed).
//!
//! The addresses are data threaded from discovery, so this logic is
//! platform-neutral; only *whether* a given board's tree exposes the nodes
//! differs, which is discovery's concern, not this module's.

use tairix_arch_api::RemotePcSample;

/// The debug-component base in effect for a CPU before discovery installs
/// one: `0`, an invalid base. There is no fail-safe default debug window —
/// a read before a real base is installed must report no sample rather than
/// poke a fabricated address, so a zero base is the "no channel" sentinel
/// [`sample_from`] fails closed on.
pub const NO_BASE: usize = 0;

// --- External-debug register offsets (from the debug-component base) -----
//
// Arm ARM DDI 0487, "External debug registers" (chapter H9 / the memory
// map in section I5): the PC Sample-based Profiling block and the
// software-lock / status registers. Offsets are within the 4 KiB debug
// component the ROM table (or the DT `reg`) points at.

/// `EDPCSR` low word (the sampled PC's low 32 bits). **Reading this word
/// captures the sample**: it latches `EDPCSRhi`, `EDCIDSR`, and `EDVIDSR`
/// for the following reads, so it must be read *first*.
pub const EDPCSR_LO: usize = 0x0A0;

/// `EDCIDSR` — the Context ID sampled together with `EDPCSR` (captured by
/// the `EDPCSR_LO` read). Diagnostic context; not required for the PC.
pub const EDCIDSR: usize = 0x0A4;

/// `EDVIDSR` — the VMID / security-state / exception-level / mode sampled
/// with `EDPCSR` (captured by the `EDPCSR_LO` read). Carried as the sample
/// `context` word so a reader can tell EL0/EL1 and secure/non-secure.
pub const EDVIDSR: usize = 0x0A8;

/// `EDPCSR` high word (the sampled PC's high 32 bits), valid only after
/// the `EDPCSR_LO` read latched it.
pub const EDPCSR_HI: usize = 0x0AC;

/// `EDPRSR` — Processor Status Register. Read to prove the target PE is
/// powered up and not in reset before trusting a sample (bits below).
pub const EDPRSR: usize = 0x314;

/// `EDLAR` — Lock Access Register. Writing [`EDLAR_UNLOCK_KEY`] clears the
/// software lock so the memory-mapped debug registers are writable/usable
/// from this master; any other value re-locks.
pub const EDLAR: usize = 0xFB0;

/// `EDDEVID` — Debug Device ID. Its low nibble encodes whether PC
/// Sample-based Profiling (the `EDPCSR` block) is implemented.
pub const EDDEVID: usize = 0xFC8;

/// The magic value written to [`EDLAR`] to clear the software lock (the
/// architectural CoreSight unlock key `0xC5ACCE55`).
pub const EDLAR_UNLOCK_KEY: u32 = 0xC5AC_CE55;

/// `EDPRSR.PU` (bit 0): the PE is powered up. Clear means the core is in a
/// low-power/off state and any sample is meaningless.
pub const EDPRSR_PU: u32 = 1 << 0;

/// `EDPRSR.R` (bit 2): the PE is in reset. Set means no valid sample.
pub const EDPRSR_R: u32 = 1 << 2;

/// The `EDPCSR_LO` value that means "no valid sample": the architecture
/// returns all-ones when PC sampling cannot supply a PC (the PE is in a
/// prohibited region, or profiling is not yet valid).
pub const EDPCSR_NONE: u32 = 0xFFFF_FFFF;

/// `EDDEVID.PCSample` field (the low nibble): `0` means PC Sample-based
/// Profiling is not implemented; a non-zero value means the `EDPCSR` block
/// (and, for `>= 3`, `EDVIDSR`/`EDCIDSR`) is present.
#[must_use]
pub const fn pc_sampling_implemented(eddevid: u32) -> bool {
    (eddevid & 0xF) != 0
}

/// `true` iff `EDPRSR` says the PE is in a state where a PC sample is
/// meaningful: powered up and not in reset.
#[must_use]
pub const fn pe_sampleable(edprsr: u32) -> bool {
    (edprsr & EDPRSR_PU) != 0 && (edprsr & EDPRSR_R) == 0
}

/// Assemble a 64-bit sampled PC from the `EDPCSR` low/high words.
#[must_use]
pub const fn assemble_pc(lo: u32, hi: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

// --- The MMIO seam + the pure sampling sequence -------------------------

/// Access to one PE's memory-mapped external-debug register block.
///
/// The seam the sampling sequence is written against: the freestanding
/// `VolatileDebugMmio` performs real reads/writes at a discovered
/// debug-component base, while host tests drive an in-memory mock, so the
/// unlock → capability → validity → capture ordering is unit-tested without
/// hardware (the same shape as [`crate::brcm_msi::MsiMmio`]).
pub trait DebugMmio {
    /// Read the 32-bit external-debug register at byte `offset` from this
    /// PE's debug-component base.
    fn read32(&self, offset: usize) -> u32;
    /// Write `value` to the 32-bit external-debug register at byte `offset`
    /// from this PE's debug-component base.
    fn write32(&self, offset: usize, value: u32);
}

/// Read a fresh PC sample of the PE whose debug block `mmio` addresses, or
/// report honestly why one is not available.
///
/// The sequence follows Arm ARM DDI 0487's PC Sample-based Profiling
/// contract, fail-closed at every step:
///
/// 1. Clear the software lock ([`EDLAR`]) so this master may use the block.
/// 2. Confirm PC sampling is implemented ([`EDDEVID`]); else
///    [`RemotePcSample::Unsupported`] — the silicon has no such channel.
/// 3. Confirm the target PE is powered up and not in reset ([`EDPRSR`]);
///    else [`RemotePcSample::Unavailable`] — a sample now would be
///    meaningless (an honest transient, not "no channel").
/// 4. Read [`EDPCSR_LO`] **first** (this latches the high word and the
///    context registers); an all-ones value means the PE was in a region
///    where sampling is prohibited, reported [`RemotePcSample::Unavailable`].
/// 5. Read the latched [`EDVIDSR`] context and [`EDPCSR_HI`], assemble the
///    64-bit PC, and return [`RemotePcSample::Sampled`].
///
/// A pure function over the seam: it neither halts the target nor blocks,
/// so it is safe on the observer's non-maskable sample path.
#[must_use]
pub fn sample_from<M: DebugMmio>(mmio: &M) -> RemotePcSample {
    // Clear the software lock so the memory-mapped block is usable from this
    // master. On a block with no software lock this is harmless.
    mmio.write32(EDLAR, EDLAR_UNLOCK_KEY);

    if !pc_sampling_implemented(mmio.read32(EDDEVID)) {
        return RemotePcSample::Unsupported(
            "PC Sample-based Profiling not implemented in this debug component",
        );
    }
    if !pe_sampleable(mmio.read32(EDPRSR)) {
        return RemotePcSample::Unavailable("target PE is powered down or in reset");
    }

    // The low-word read captures the sample and latches the high/context
    // words for the reads that follow, so it must come first.
    let lo = mmio.read32(EDPCSR_LO);
    if lo == EDPCSR_NONE {
        return RemotePcSample::Unavailable(
            "no valid PC sample (PE in a sampling-prohibited region)",
        );
    }
    let context = u64::from(mmio.read32(EDVIDSR));
    let hi = mmio.read32(EDPCSR_HI);
    RemotePcSample::Sampled {
        pc: assemble_pc(lo, hi),
        context,
    }
}

// --- Discovered per-CPU debug-base registry -----------------------------

use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// The installed per-CPU debug-component base table, as a raw pointer +
/// length pair (a fat pointer cannot be stored atomically). The slice is
/// owned by the boot path, sized to the discovered CPU count and leaked to
/// `'static` — never a fixed compile-time ceiling — so the registry scales
/// to the machine. Both stay at their empty defaults until
/// [`install_debug_bases`] runs, so a build that never discovers debug
/// components reports every CPU [`NO_BASE`] (fail closed).
static BASES_PTR: AtomicPtr<usize> = AtomicPtr::new(core::ptr::null_mut());
static BASES_LEN: AtomicUsize = AtomicUsize::new(0);

/// Install the discovered per-CPU debug-component base table (set-once).
///
/// `bases[cpu]` is the memory-mapped external-debug base for dense
/// [`tairix_arch_api::CpuId`] `cpu`, or [`NO_BASE`] for a CPU with no
/// discovered debug component. The slice is sized to the discovered CPU
/// count by the boot path (no fixed ceiling) and must be `'static` and
/// immutable after this call, since [`debug_base`] reads it lock-free from
/// the watchdog sample path. A second call is ignored — one boot installs
/// one table.
pub fn install_debug_bases(bases: &'static [usize]) {
    // Publish the pointer, then the length with `Release`; a reader that
    // observes a non-zero length (`Acquire`) is guaranteed to see the
    // pointer store. `compare_exchange` on the length makes it set-once.
    BASES_PTR.store(bases.as_ptr().cast_mut(), Ordering::Relaxed);
    let _ = BASES_LEN.compare_exchange(0, bases.len(), Ordering::Release, Ordering::Relaxed);
}

/// The discovered debug-component base for `cpu`, or [`NO_BASE`] when none
/// was installed for it (no table installed, `cpu` out of range, or that
/// CPU's slot is [`NO_BASE`]).
#[must_use]
pub fn debug_base(cpu: tairix_arch_api::CpuId) -> usize {
    let len = BASES_LEN.load(Ordering::Acquire);
    let idx = cpu as usize;
    if idx >= len {
        return NO_BASE;
    }
    let ptr = BASES_PTR.load(Ordering::Relaxed);
    if ptr.is_null() {
        return NO_BASE;
    }
    // SAFETY: `idx < len` and `ptr` is the base of the `'static` slice
    // `install_debug_bases` published (length `len`), immutable after
    // install, so `ptr.add(idx)` is in bounds and points at a live `usize`.
    unsafe { *ptr.add(idx) }
}

/// Read a fresh external-debug PC sample of `target` over its discovered
/// debug component, or report why one is not available. The
/// [`tairix_arch_api::WatchdogArch::remote_pc_sample`] body for this port.
///
/// Fails closed to [`RemotePcSample::Unsupported`] when no debug base was
/// discovered for `target` (the common case on a tree that does not
/// describe the debug components), so the caller keeps the stale sample
/// rather than a fabricated one.
#[must_use]
pub fn remote_pc_sample(target: tairix_arch_api::CpuId) -> RemotePcSample {
    let base = debug_base(target);
    if base == NO_BASE {
        return RemotePcSample::Unsupported("no external-debug component discovered for this CPU");
    }
    // The register read is a real MMIO access, so it exists only on the
    // freestanding target. A discovered base on the host build (no such
    // build installs one) has no window to read, so it reports an honest
    // transient rather than a fabricated PC.
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        sample_from(&VolatileDebugMmio::at(base))
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        let _ = base;
        RemotePcSample::Unavailable("external-debug MMIO is not accessible off metal")
    }
}

/// Bare-metal [`DebugMmio`] over one PE's discovered debug-component base.
///
/// Holds the base as a value (unlike the zero-sized MSI/GIC handles, a
/// different base per target CPU), so the sampler constructs one per read
/// from [`debug_base`]. Compiled only for the freestanding aarch64 target;
/// host builds use the test mock.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub struct VolatileDebugMmio {
    base: usize,
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl VolatileDebugMmio {
    /// A handle over the debug-component window based at `base`.
    #[must_use]
    pub const fn at(base: usize) -> Self {
        Self { base }
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl DebugMmio for VolatileDebugMmio {
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: `offset` addresses a register within the discovered 4 KiB
        // external-debug window `base` points at, mapped device memory the
        // kernel owns. External-debug registers are readable by any master;
        // the read has no side effect beyond the architected sample capture.
        unsafe { core::ptr::read_volatile((self.base + offset) as *const u32) }
    }
    fn write32(&self, offset: usize, value: u32) {
        // SAFETY: as `read32`, but a 32-bit store — used only for the
        // [`EDLAR`] software-lock clear, which the architecture defines as a
        // side-effect-free unlock of the memory-mapped interface.
        unsafe { core::ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// An in-memory [`DebugMmio`] with settable registers that records the
    /// read order, so the unlock, the capability/validity gating, and the
    /// EDPCSR capture-first sequencing are asserted without hardware.
    #[derive(Default)]
    struct MockDebug {
        eddevid: u32,
        edprsr: u32,
        edpcsr_lo: u32,
        edpcsr_hi: u32,
        edvidsr: u32,
        lock: RefCell<u32>,
        reads: RefCell<alloc::vec::Vec<usize>>,
    }

    extern crate alloc;

    impl DebugMmio for MockDebug {
        fn read32(&self, offset: usize) -> u32 {
            self.reads.borrow_mut().push(offset);
            match offset {
                EDDEVID => self.eddevid,
                EDPRSR => self.edprsr,
                EDPCSR_LO => self.edpcsr_lo,
                EDPCSR_HI => self.edpcsr_hi,
                EDVIDSR => self.edvidsr,
                _ => 0,
            }
        }
        fn write32(&self, offset: usize, value: u32) {
            if offset == EDLAR {
                *self.lock.borrow_mut() = value;
            }
        }
    }

    fn a_working_block() -> MockDebug {
        MockDebug {
            eddevid: 0x3,      // PC sampling implemented (non-zero nibble)
            edprsr: EDPRSR_PU, // powered up, not in reset
            edpcsr_lo: 0x8000_1234,
            edpcsr_hi: 0x0000_00FF,
            edvidsr: 0x0000_0001,
            ..MockDebug::default()
        }
    }

    #[test]
    fn offsets_match_the_arm_external_debug_map() {
        // Pinned against DDI 0487 so a refactor cannot silently move a
        // register out from under the metal read.
        assert_eq!(EDPCSR_LO, 0x0A0);
        assert_eq!(EDCIDSR, 0x0A4);
        assert_eq!(EDVIDSR, 0x0A8);
        assert_eq!(EDPCSR_HI, 0x0AC);
        assert_eq!(EDPRSR, 0x314);
        assert_eq!(EDLAR, 0xFB0);
        assert_eq!(EDDEVID, 0xFC8);
        assert_eq!(EDLAR_UNLOCK_KEY, 0xC5AC_CE55);
    }

    #[test]
    fn pc_sampling_capability_reads_the_low_nibble() {
        assert!(!pc_sampling_implemented(0));
        assert!(!pc_sampling_implemented(0xFFFF_FFF0));
        assert!(pc_sampling_implemented(0x1));
        assert!(pc_sampling_implemented(0x3));
    }

    #[test]
    fn pe_sampleable_needs_powered_up_and_not_in_reset() {
        assert!(pe_sampleable(EDPRSR_PU));
        assert!(!pe_sampleable(0)); // powered down
        assert!(!pe_sampleable(EDPRSR_PU | EDPRSR_R)); // in reset
    }

    #[test]
    fn assemble_pc_joins_high_and_low_words() {
        assert_eq!(assemble_pc(0x8000_1234, 0x0000_00FF), 0x0000_00FF_8000_1234);
    }

    #[test]
    fn a_valid_block_yields_the_assembled_pc_and_context() {
        let m = a_working_block();
        let sample = sample_from(&m);
        assert_eq!(
            sample,
            RemotePcSample::Sampled {
                pc: 0x0000_00FF_8000_1234,
                context: 1,
            }
        );
        // The software lock was cleared first.
        assert_eq!(*m.lock.borrow(), EDLAR_UNLOCK_KEY);
        // EDPCSR_LO was read before EDPCSR_HI (capture-first ordering).
        let reads = m.reads.borrow();
        let lo_at = reads.iter().position(|&o| o == EDPCSR_LO).unwrap();
        let hi_at = reads.iter().position(|&o| o == EDPCSR_HI).unwrap();
        assert!(lo_at < hi_at);
    }

    #[test]
    fn an_unimplemented_block_is_unsupported() {
        let m = MockDebug {
            eddevid: 0,
            ..a_working_block()
        };
        assert!(matches!(sample_from(&m), RemotePcSample::Unsupported(_)));
    }

    #[test]
    fn a_powered_down_pe_is_unavailable_not_unsupported() {
        // A transient state (the core may power back up), so the honest
        // answer is Unavailable — distinct from "no channel".
        let m = MockDebug {
            edprsr: 0,
            ..a_working_block()
        };
        assert!(matches!(sample_from(&m), RemotePcSample::Unavailable(_)));
    }

    #[test]
    fn an_all_ones_sample_is_unavailable() {
        let m = MockDebug {
            edpcsr_lo: EDPCSR_NONE,
            ..a_working_block()
        };
        assert!(matches!(sample_from(&m), RemotePcSample::Unavailable(_)));
    }

    #[test]
    fn the_registry_is_empty_until_installed_and_fails_closed() {
        // Before any install (and for an out-of-range CPU), every base is
        // NO_BASE, so `remote_pc_sample` reports Unsupported — the fail-closed
        // default on a tree with no debug components.
        assert_eq!(debug_base(0), NO_BASE);
        assert_eq!(debug_base(9999), NO_BASE);
        assert!(matches!(
            remote_pc_sample(0),
            RemotePcSample::Unsupported(_)
        ));
    }
}
