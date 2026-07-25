//! riscv64 CPU feature detection and cycle counter.
//!
//! Implements the Arch HAL
//! [`CpuFeatures`](tairix_arch_api::CpuFeatures) and
//! [`CpuCycles`](tairix_arch_api::CpuCycles) surfaces for riscv64.
//!
//! RISC-V splits its extension advertisement in two: the base ISA and
//! the single-letter standard extensions (including the `V` vector
//! extension) are reported in the `misa` CSR, while the multi-letter
//! standard extensions (`Zbb`, `Zbc`, `Zbkc`, …) are advertised **only**
//! in the device-tree `riscv,isa` string (RISC-V privileged spec; the
//! device-tree CPU binding). This port reads the `V` bit from `misa` at
//! bring-up and parses the multi-letter extensions from the ISA string
//! the platform-discovery pass already reads via [`tairix_fdt`], so the
//! handle carries the string-derived set and ORs it with the live `misa`
//! read.
//!
//! The cycle counter is the architectural `time` CSR (`rdtime`) — a
//! fixed-rate, monotonically-increasing counter always available to
//! S-mode on the QEMU `virt` platform. Like the aarch64 `CNTVCT_EL0`
//! choice it is a wall-time proxy the `lib/cpuops` harness measures
//! deltas over, which is sufficient to rank two equally-correct
//! routines and needs no `rdcycle` counter-enable wiring.
//!
//! Only the architecture port reads `misa` and the ISA string, so
//! detection lives here. The decoders (`features_from_misa`,
//! `features_from_isa_string`) are pure and host-tested; the `misa`
//! read executes only on the freestanding target (the host build reports
//! the string-derived set alone).

use tairix_arch_api::{
    CoreType, CpuCycles, CpuFeature, CpuFeatureSet, CpuFeatures, CpuId, FeatureProfile,
    FeatureSupport,
};

/// Bit position of the `V` (vector) extension in `misa` (`'V' - 'A'`).
const MISA_V_BIT: u32 = 21;

/// Decode the single-letter extensions this port tracks from a raw
/// `misa` value.
///
/// Pure and host-testable: only the `V` vector extension is a
/// single-letter extension in this port's [`CpuFeature`] set; the base
/// integer/atomic/compressed letters are the build-time floor and need
/// no runtime bit.
#[must_use]
pub fn features_from_misa(misa: u64) -> CpuFeatureSet {
    let mut set = CpuFeatureSet::EMPTY;
    if (misa >> MISA_V_BIT) & 1 == 1 {
        set = set.with(CpuFeature::VectorV);
    }
    set
}

/// Decode the multi-letter standard extensions this port tracks from a
/// device-tree `riscv,isa` string (e.g. `"rv64imafdc_zba_zbb_zbc"`).
///
/// The RISC-V ISA-string grammar is lowercase, with the base and
/// single-letter extensions in the first `_`-separated token and each
/// multi-letter extension in its own subsequent token. This parser reads
/// the `V` bit from the first token (so a vector-capable core is
/// recognised from the string even before the `misa` read) and the
/// `Zbb`/`Zbc`/`Zbkc` tokens after it. An unrecognised token is ignored
/// (an honest "not one we gate on"), never guessed.
#[must_use]
pub fn features_from_isa_string(isa: &str) -> CpuFeatureSet {
    let mut set = CpuFeatureSet::EMPTY;
    let mut tokens = isa.split('_');
    if let Some(base) = tokens.next() {
        let single = base
            .strip_prefix("rv64")
            .or_else(|| base.strip_prefix("rv32"))
            .unwrap_or(base);
        if single.contains('v') {
            set = set.with(CpuFeature::VectorV);
        }
    }
    for token in tokens {
        match token {
            "zbb" => set = set.with(CpuFeature::Zbb),
            "zbc" => set = set.with(CpuFeature::Zbc),
            "zbkc" => set = set.with(CpuFeature::Zbkc),
            _ => {}
        }
    }
    set
}

/// riscv64 implementation of the Arch HAL CPU-feature surface.
///
/// Carries the multi-letter extension set parsed from the device-tree
/// ISA string (the only place those extensions are advertised); the
/// single-letter `V` bit is read live from `misa`. A default-constructed
/// handle carries no string-derived extensions — the honest state before
/// the device tree is read.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuFeatureDetect {
    from_isa_string: CpuFeatureSet,
}

impl CpuFeatureDetect {
    /// Construct a handle with no device-tree-derived extensions.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            from_isa_string: CpuFeatureSet::EMPTY,
        }
    }

    /// Construct a handle carrying the multi-letter extensions parsed
    /// from the device-tree `riscv,isa` string.
    #[must_use]
    pub fn from_isa_string(isa: &str) -> Self {
        Self {
            from_isa_string: features_from_isa_string(isa),
        }
    }

    /// The honest declaration for riscv64: `misa` plus the device-tree
    /// ISA string cover the extensions this port gates on.
    #[must_use]
    pub const fn declared_profile() -> FeatureProfile {
        FeatureProfile {
            isa_features: FeatureSupport::Supported,
            core_identity: FeatureSupport::Supported,
        }
    }

    /// Read the live `misa` CSR (freestanding target only; `0` on host).
    fn read_misa() -> u64 {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            let misa: u64;
            // SAFETY: `misa` is a read-only S-mode CSR readable with no
            // side effect; the read cannot fault at S-mode.
            unsafe {
                core::arch::asm!(
                    "csrr {misa}, misa",
                    misa = out(reg) misa,
                    options(nomem, nostack, preserves_flags),
                );
            }
            misa
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            0
        }
    }
}

impl CpuFeatures for CpuFeatureDetect {
    fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
        let from_misa = features_from_misa(Self::read_misa());
        CpuFeatureSet::from_bits(from_misa.bits() | self.from_isa_string.bits())
    }

    fn core_type(&self, _cpu: CpuId) -> CoreType {
        // riscv64 has no compact single-register model identity like MIDR
        // or the CPUID signature; `mvendorid`/`marchid`/`mimpid` are
        // often zero on virtual platforms. Report the honest unknown —
        // the ops-table key falls back to the (homogeneous) default,
        // which is correct for the single-core-type targets today.
        CoreType::UNKNOWN
    }

    fn profile(&self) -> FeatureProfile {
        Self::declared_profile()
    }
}

/// riscv64 implementation of the Arch HAL cycle-counter surface (the
/// architectural `time` CSR read via `rdtime`).
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuCycleCounter;

impl CpuCycleCounter {
    /// Construct the riscv64 cycle-counter handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CpuCycles for CpuCycleCounter {
    fn cpu_cycles(&self) -> u64 {
        // Reuse the one `time`-CSR reader the monotonic clock uses, so the
        // benchmark and the clock can never disagree on the counter (the
        // host substitute is a strictly-increasing stub).
        crate::kernel_arch::read_time()
    }

    fn cycles_monotonic_hint(&self) -> bool {
        // The `time` CSR is the architectural fixed-frequency monotonic
        // counter: a reliable constant-rate time base by definition.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::{cpucycles, cpufeatures};

    #[test]
    fn misa_v_bit_decodes() {
        assert!(features_from_misa(1 << MISA_V_BIT).contains(CpuFeature::VectorV));
        // A misa without the V bit (e.g. rv64imafdc) has no vector bit.
        assert!(!features_from_misa(0).contains(CpuFeature::VectorV));
    }

    #[test]
    fn isa_string_multi_letter_extensions_decode() {
        let set = features_from_isa_string("rv64imafdc_zba_zbb_zbc_zbkc");
        assert!(set.contains(CpuFeature::Zbb));
        assert!(set.contains(CpuFeature::Zbc));
        assert!(set.contains(CpuFeature::Zbkc));
        // `zba` is not one we gate on, so it is silently ignored.
        assert!(!set.contains(CpuFeature::VectorV));
    }

    #[test]
    fn isa_string_single_letter_vector_decodes() {
        // The single-letter `v` in the base cluster is recognised.
        let set = features_from_isa_string("rv64imafdcv_zbb");
        assert!(set.contains(CpuFeature::VectorV));
        assert!(set.contains(CpuFeature::Zbb));
    }

    #[test]
    fn isa_string_without_gated_extensions_is_empty() {
        assert_eq!(features_from_isa_string("rv64imafdc"), CpuFeatureSet::EMPTY);
        assert_eq!(features_from_isa_string("rv64gc"), CpuFeatureSet::EMPTY);
    }

    #[test]
    fn handle_ors_string_derived_extensions() {
        // On the host `misa` reads 0, so `detect` reflects exactly the
        // string-derived set the handle was built with.
        let handle = CpuFeatureDetect::from_isa_string("rv64imafdc_zbb_zbc");
        let set = handle.detect(0);
        assert!(set.contains(CpuFeature::Zbb));
        assert!(set.contains(CpuFeature::Zbc));
        // A default handle carries nothing.
        assert_eq!(CpuFeatureDetect::new().detect(0), CpuFeatureSet::EMPTY);
    }

    #[test]
    fn declared_profile_is_honest_and_release_ready() {
        let profile = CpuFeatureDetect::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        assert!(profile.is_release_ready());
    }

    #[test]
    fn passes_cpufeatures_conformance() {
        cpufeatures::conformance::run_all(&CpuFeatureDetect::new());
        // A handle with string-derived extensions is also conformant.
        cpufeatures::conformance::run_all(&CpuFeatureDetect::from_isa_string("rv64gc_zbb"));
        let dynamic: &dyn CpuFeatures = &CpuFeatureDetect::new();
        cpufeatures::conformance::run_all(dynamic);
    }

    #[test]
    fn passes_cpucycles_conformance() {
        cpucycles::conformance::run_all(&CpuCycleCounter::new());
        assert!(CpuCycleCounter::new().cycles_monotonic_hint());
    }
}
