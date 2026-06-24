//! Secondary-CPU bring-up surface of the Arch HAL (
//! "SMP secondary-core bring-up", `plans/WIRING.md` §2 / Stage W14).
//!
//! the charter mandates SMP from day one. Bringing a machine's other
//! logical CPUs online is the one architecture primitive that was still
//! ad-hoc inside each port after Stage W13: every port owned a `smp`
//! module — x86_64 INIT-SIPI-SIPI, aarch64 PSCI `CPU_ON`, riscv64 SBI
//! HSM `hart_start`, wasm32 a Web Worker spawn — but the rest of the
//! kernel could not reach those through one neutral surface. This module
//! closes the burn-down: secondary bring-up becomes the
//! object-safe [`SecondaryBringup`] HAL trait, implemented once per port
//! on the port's [`crate::SchedulerArch`] handle (the owner of the dense
//! [`crate::CpuId`] ↔ native-id topology map).
//!
//! # What the trait does *not* cover
//!
//! The *directed inter-processor interrupt* half of SMP already lives on
//! [`crate::SchedulerArch::send_ipi`]; this slice is purely about
//! **starting** a parked CPU, so it does not duplicate the IPI surface
//! (no interface creep).
//!
//! The set-once *secondary entry* a freshly-started CPU runs is
//! deliberately **not** part of this trait. On the bare-metal ports it is
//! an `extern "C" fn(CpuId) -> !` function pointer the port installs once
//! at boot; on wasm32 a secondary is a *fresh module instance* whose
//! entry is the fixed `rustos_arch_wasm32_main` export, not a runtime
//! pointer one instance can hand another. Forcing a settable-entry method
//! onto the HAL would make wasm32 fake one it could never honour
//! (no fakes), so entry installation stays the
//! genuinely port-shaped concern it is, performed once before
//! [`SecondaryBringup::start_secondary`] is first called.
//!
//! # Per-arch shape (the modularity carve-out)
//!
//! Each port implements the *same* trait its own way; these parallel
//! implementations are the deliberate shape of the HAL, never collapsed
//! behind `cfg` (carve-out):
//!
//! * **x86_64** owns a low-memory trampoline frame, a per-AP stack pool,
//!   and the boot PML4; `start_secondary` installs the trampoline, writes
//!   the per-AP boot slot, runs the SDM INIT-SIPI-SIPI handshake, and
//!   spins on the AP's long-mode `ready` flag before returning (so the
//!   shared trampoline frame is safe to reuse for the next CPU).
//! * **aarch64** issues PSCI `CPU_ON` over the firmware conduit
//!   (`hvc`/`smc`) discovered from the device tree, targeting the CPU's
//!   `MPIDR_EL1` affinity.
//! * **riscv64** issues the SBI HSM `hart_start` firmware call targeting
//!   the CPU's hart id.
//! * **wasm32** asks its JavaScript host to spawn a Web Worker that
//!   instantiates the same module as a new logical CPU.
//!
//! # Why the host conformance vertical proves only the observable half
//!
//! Exactly as for [`crate::xtlb`] and [`crate::mmu::AddressSpace::activate`],
//! the *effect* of a bring-up (a second CPU actually running) is not
//! observable from a single-threaded host test, and the
//! INIT-SIPI-SIPI / PSCI / SBI / Web Worker machinery only exists on the
//! freestanding/wasm target. The host [`conformance`] vertical therefore
//! proves the contract that *is* observable on the host — the call is
//! object-safe and **fails closed** rather than
//! panicking for a CPU that cannot be started (out of range / the boot
//! CPU / unmapped) — while the real cross-core round-trip is exercised
//! end-to-end by the multi-core `ipi_smp_qemu_*` /
//! `cross_cpu_tlb_shootdown_qemu_*` QEMU verticals and the wasm32 browser
//! vertical.

use crate::CpuId;

/// Why a [`SecondaryBringup::start_secondary`] request was refused.
///
/// The neutral failure surface every port maps its native error onto, so
/// the architecture-neutral kernel handles one set of outcomes. A port's
/// richer detail (the raw PSCI / SBI / host status) is preserved in
/// [`SmpError::StartRejected`] for the audit log.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum SmpError {
    /// `cpu` did not name a startable secondary CPU: it was out of
    /// range, named the boot CPU (which is already running), or was
    /// absent from the port's topology map. The request is refused
    /// before any platform action is taken (fail
    /// closed, no out-of-bounds stack selection).
    InvalidCpu,
    /// The port's secondary entry has not been installed yet, so a
    /// freshly-started CPU would have nowhere to run. Refused at the
    /// call site so the failure is loud here, not silent on the new CPU.
    NotReady,
    /// The platform's start mechanism (INIT-SIPI-SIPI / PSCI `CPU_ON` /
    /// SBI HSM `hart_start` / Web Worker spawn) refused or failed. The
    /// payload carries the port's raw status (a PSCI / SBI return code,
    /// or `0` where the mechanism reports only success/failure).
    StartRejected(i64),
}

impl SmpError {
    /// Stable cause string for audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCpu => "smp_invalid_cpu",
            Self::NotReady => "smp_secondary_entry_not_installed",
            Self::StartRejected(_) => "smp_start_rejected",
        }
    }
}

/// Start a parked secondary logical CPU.
///
/// The kernel calls [`Self::start_secondary`] once per detected secondary
/// CPU during bring-up. The trait is implemented on the port's
/// [`crate::SchedulerArch`] handle — the owner of the dense
/// [`CpuId`] ↔ native-id (APIC id / `MPIDR_EL1` / hart id / worker
/// index) topology map and the platform start path — and is object-safe
/// so the architecture-neutral kernel can hold it behind a
/// `&dyn SecondaryBringup`.
///
/// # Required semantics
///
/// * A `cpu` the port cannot start — out of range, the boot CPU, or
///   unmapped — must return [`SmpError::InvalidCpu`] **before** touching
///   any platform state (fail closed).
/// * If no secondary entry has been installed, the call must return
///   [`SmpError::NotReady`] rather than starting a CPU that would park.
/// * On success the call must have issued the platform start request and
///   completed any port-internal handshake required before the *next*
///   secondary can be started safely (on x86_64 that means waiting for
///   the AP's long-mode `ready` flag, because the trampoline frame is
///   reused; the other ports have nothing to wait on and return as soon
///   as the firmware/host has accepted the request).
/// * Implementations must never panic for any `cpu`.
pub trait SecondaryBringup {
    /// Start the secondary logical CPU `cpu`.
    ///
    /// # Errors
    ///
    /// See [`SmpError`]. The call fails closed rather than assuming the
    /// CPU came up.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the port's secondary entry is
    /// installed, that early boot has zeroed the secondary-stack pool,
    /// and that `cpu` names a real, parked logical CPU distinct from the
    /// caller. The implementation still validates `cpu` against its
    /// topology and refuses an unstartable id ([`SmpError::InvalidCpu`]);
    /// the `unsafe` marker reflects the genuine bring-up preconditions a
    /// safe wrapper cannot check (a real, parked, distinct CPU).
    unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError>;
}

/// The secondary-bring-up conformance vertical.
///
/// Like [`crate::xtlb::conformance`] it names only the trait and runs on
/// the host: there is no privileged INIT-SIPI-SIPI / PSCI / SBI / Web
/// Worker machinery on the host build, and a bring-up's cross-core
/// *effect* is not observable from a single-threaded test. It proves the
/// observable half of the contract — the call is object-safe and **fails
/// closed** for a CPU that cannot be started, never panicking. The real
/// multi-core round-trip is proven by the `ipi_smp_qemu_*` /
/// `cross_cpu_tlb_shootdown_qemu_*` verticals and the wasm32 browser
/// vertical.
pub mod conformance {
    use super::{SecondaryBringup, SmpError};
    use crate::CpuId;

    /// Run the [`SecondaryBringup`] conformance suite against `bringup`.
    ///
    /// `unstartable` must be a [`CpuId`] the port cannot start (the
    /// canonical choice is [`CpuId::MAX`], which no real topology maps).
    /// The suite asserts that starting it fails closed with
    /// [`SmpError::InvalidCpu`] and that the call is total (no panic),
    /// both directly and behind the object-safe erasure the kernel holds
    /// the handle through.
    ///
    /// # Panics
    ///
    /// Panics (failing the conformance test) if the port does not refuse
    /// `unstartable` with [`SmpError::InvalidCpu`].
    pub fn run_all<T: SecondaryBringup + ?Sized>(bringup: &T, unstartable: CpuId) {
        // SAFETY: the conformance vertical only ever starts an
        // `unstartable` CPU id, which the contract requires the port to
        // refuse before taking any platform action — so the bring-up
        // preconditions of `start_secondary` are vacuously satisfied.
        let refused = unsafe { bringup.start_secondary(unstartable) };
        assert_eq!(
            refused,
            Err(SmpError::InvalidCpu),
            "start_secondary must fail closed for an unstartable CPU id",
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{SecondaryBringup, SmpError};
        use super::run_all;
        use crate::CpuId;
        use core::sync::atomic::{AtomicUsize, Ordering};

        /// A faithful host double: it refuses any CPU at or above a small
        /// bound (mimicking a port topology) and records how many starts
        /// it accepted so the suite has something observable to assert.
        /// The counter is interior-mutable because [`SecondaryBringup`]
        /// takes `&self` — the real handle is shared (`&dyn`) between
        /// CPUs exactly like [`crate::SchedulerArch`].
        #[derive(Default)]
        struct CountingBringup {
            started: AtomicUsize,
        }

        const BOUND: CpuId = 4;

        impl SecondaryBringup for CountingBringup {
            unsafe fn start_secondary(&self, cpu: CpuId) -> Result<(), SmpError> {
                if cpu == 0 || cpu >= BOUND {
                    return Err(SmpError::InvalidCpu);
                }
                self.started.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        #[test]
        fn suite_requires_fail_closed_on_an_unstartable_id() {
            let bringup = CountingBringup::default();
            run_all(&bringup, CpuId::MAX);
            assert_eq!(
                bringup.started.load(Ordering::Relaxed),
                0,
                "the suite must not have started any real CPU",
            );

            // And over the object-safe erasure the kernel holds it behind.
            let dynamic = CountingBringup::default();
            let erased: &dyn SecondaryBringup = &dynamic;
            run_all(erased, CpuId::MAX);
            assert_eq!(dynamic.started.load(Ordering::Relaxed), 0);
        }

        #[test]
        fn faithful_double_accepts_a_real_secondary() {
            // Positive coverage the generic suite cannot give (it does
            // not know a startable id): a valid secondary is accepted.
            let bringup = CountingBringup::default();
            // SAFETY: the double takes no platform action; CPU 1 is a
            // valid secondary in its synthetic topology.
            let ok = unsafe { bringup.start_secondary(1) };
            assert_eq!(ok, Ok(()));
            assert_eq!(bringup.started.load(Ordering::Relaxed), 1);
            // The boot CPU (0) is refused, never started.
            // SAFETY: as above; CPU 0 is refused before any action.
            assert_eq!(
                unsafe { bringup.start_secondary(0) },
                Err(SmpError::InvalidCpu)
            );
        }
    }
}
