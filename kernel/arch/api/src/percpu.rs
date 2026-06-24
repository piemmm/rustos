//! Per-CPU storage surface of the Arch HAL (
//! "per-CPU storage").
//!
//! Every CPU needs a private word it can reach with no lock and no
//! cross-CPU coordination — the anchor the rest of the kernel resolves
//! its CPU-local state (the current task, the per-CPU run queue, the
//! syscall stack) from. On real silicon that word is a dedicated
//! register the CPU reads in a single instruction: the GS base on
//! x86_64, `TPIDR_EL1` on aarch64, `tp` on riscv64. The wasm32 port has
//! no such register, so the per-worker module instance owns the word
//! directly. The charter makes the architecture surface a closed set of traits
//! on the HAL; this module is the "per-CPU storage" member of that set,
//! so the register read/write lives in exactly one place per port
//! instead of being copied into every call site.
//!
//! # What lives here
//!
//! * [`PerCpu`] — the per-port handle the kernel reaches through. It
//!   reads and writes the calling CPU's per-CPU base word: the kernel
//!   seeds it once as a CPU comes online (with the address of that CPU's
//!   control block, or a dense CPU index — the port and the kernel agree
//!   on the meaning) and then reads it on every CPU-local access.
//! * [`conformance`] — the conformance vertical: a host-run
//!   [`conformance::run_all`] round-trip check every port runs over its
//!   handle, plus a two-handle [`conformance::run_isolation`] check that
//!   pins the per-CPU word of one CPU is independent of another's.
//!
//! # The base word is opaque
//!
//! This trait does not interpret the stored word. The kernel chooses its
//! meaning (a pointer to a per-CPU control block, a dense [`crate::CpuId`],
//! …) and every port stores and returns it byte-for-byte at native
//! pointer width. Keeping it opaque is what lets the one trait serve
//! every port without naming a per-CPU layout it has no business owning.
//!
//! # Why the host backing is per-handle
//!
//! On the bare-metal targets the read/write hit the real per-CPU
//! register, so a single shared handle is correct: the register is
//! genuinely per-CPU. A host (or wasm32) build has no such register, so
//! each [`PerCpu`] handle owns its word. A host test therefore models
//! "two CPUs" as two handles, which is exactly what
//! [`conformance::run_isolation`] exercises. This mirrors the host-only
//! backing the other slices carry (the `host_tick_counter` in the
//! scheduler handle); it is never linked into a kernel image
//! (no fake primitives in production).

/// The per-CPU storage handle an architecture port exposes.
///
/// The kernel seeds the calling CPU's per-CPU base word with
/// [`Self::write_self_base`] as the CPU comes online, then resolves its
/// CPU-local state through [`Self::read_self_base`] on every access. The
/// stored word is opaque to this surface (see the module docs).
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU. A port's handle is typically zero-sized on the
/// bare-metal target — the per-CPU word lives in a register, not in the
/// handle.
pub trait PerCpu: Send + Sync {
    /// Read the calling CPU's per-CPU base word.
    ///
    /// On the bare-metal targets this is a single side-effect-free
    /// register read (`rdmsr IA32_GS_BASE` / `mrs TPIDR_EL1` /
    /// `mv _, tp`). Before [`Self::write_self_base`] has run on this CPU
    /// the word reads back as `0` (the architecture-neutral "unset"
    /// value), never an undefined or invented address (fail closed).
    fn read_self_base(&self) -> usize;

    /// Install `base` as the calling CPU's per-CPU base word.
    ///
    /// After this returns, [`Self::read_self_base`] on the same CPU
    /// returns `base` until the next write.
    ///
    /// # Safety
    ///
    /// On a bare-metal port the per-CPU base register is the anchor the
    /// kernel resolves *all* of its CPU-local state from. The caller
    /// must guarantee that
    ///
    /// * the write runs on the CPU whose word is being set (the register
    ///   is per-CPU; writing it changes only the calling CPU's word), and
    /// * `base` is the value the kernel's per-CPU resolution expects for
    ///   this CPU (typically the address of a live, correctly-aligned
    ///   per-CPU control block reserved for this CPU). Installing a
    ///   bogus word makes every subsequent CPU-local access read through
    ///   it.
    unsafe fn write_self_base(&self, base: usize);
}

/// The per-CPU storage conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`PerCpu`] handle. The suite is portable — it names only the trait —
/// and runs on the host, exactly like the sibling
/// [`crate::memtag::conformance`] and [`crate::sidechannel::conformance`]
/// verticals: it is the trait-level "the word round-trips" check.
/// [`conformance::run_isolation`] additionally pins the per-CPU property
/// that one CPU's word is independent of another's, which a single
/// handle cannot express; each port drives it over two handles.
pub mod conformance {
    use super::PerCpu;

    /// The round-trip probe values, including the "unset" sentinel, a
    /// pointer-like value, and the full-width edge value, so a port that
    /// truncated the word would be caught.
    const PROBES: [usize; 5] = [0, 1, 0x1000, 0xdead_beef, usize::MAX];

    /// Run the entire single-handle per-CPU conformance suite against
    /// `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if the word does not round-trip: a read
    /// after a write returns a different value, or the full pointer width
    /// is not preserved.
    pub fn run_all<P: PerCpu + ?Sized>(port: &P) {
        word_round_trips(port);
    }

    /// Every written word reads back unchanged, at full pointer width,
    /// and the last write wins.
    fn word_round_trips<P: PerCpu + ?Sized>(port: &P) {
        for probe in PROBES {
            // SAFETY: this suite only ever runs on the host (it is
            // invoked from `#[cfg(test)]` modules), where a port's word
            // is a plain in-handle cell, so any `usize` is a valid
            // "base" and the write has no wider effect. The real
            // register write is `cfg`-gated to the bare-metal target,
            // which never reaches this host-run code.
            unsafe {
                port.write_self_base(probe);
            }
            assert_eq!(
                port.read_self_base(),
                probe,
                "per-CPU base word must round-trip unchanged (wrote {probe:#x})"
            );
            // A second read agrees: the word is stable between writes.
            assert_eq!(
                port.read_self_base(),
                probe,
                "per-CPU base word must be stable between writes (wrote {probe:#x})"
            );
        }
    }

    /// Run the two-handle isolation check: writing one CPU's per-CPU word
    /// never disturbs another's.
    ///
    /// A single [`PerCpu`] handle cannot express the per-CPU property —
    /// on the bare-metal target the register *is* per-CPU, and on the
    /// host each handle models one CPU's word (see the module docs). A
    /// port therefore drives this over two distinct handles (two
    /// "CPUs").
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if a write to one handle is observed
    /// through the other.
    pub fn run_isolation<P: PerCpu>(cpu_a: &P, cpu_b: &P) {
        // SAFETY: host-only execution (see `word_round_trips`); each
        // handle's word is an independent in-handle cell.
        unsafe {
            cpu_a.write_self_base(0xAAAA);
            cpu_b.write_self_base(0xBBBB);
        }
        assert_eq!(
            cpu_a.read_self_base(),
            0xAAAA,
            "CPU A's per-CPU word must not be disturbed by CPU B's write"
        );
        assert_eq!(
            cpu_b.read_self_base(),
            0xBBBB,
            "CPU B's per-CPU word must not be disturbed by CPU A's write"
        );
        // Re-writing A leaves B untouched.
        // SAFETY: as above.
        unsafe {
            cpu_a.write_self_base(0xCCCC);
        }
        assert_eq!(cpu_a.read_self_base(), 0xCCCC);
        assert_eq!(
            cpu_b.read_self_base(),
            0xBBBB,
            "CPU B's per-CPU word must survive a later write to CPU A"
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::PerCpu;
        use super::{run_all, run_isolation};
        use core::sync::atomic::{AtomicUsize, Ordering};

        /// A faithful host double: an in-handle cell standing in for one
        /// CPU's per-CPU register.
        #[derive(Default)]
        struct CellPerCpu {
            base: AtomicUsize,
        }

        impl PerCpu for CellPerCpu {
            fn read_self_base(&self) -> usize {
                self.base.load(Ordering::Relaxed)
            }
            unsafe fn write_self_base(&self, base: usize) {
                self.base.store(base, Ordering::Relaxed);
            }
        }

        #[test]
        fn suite_accepts_a_faithful_cell() {
            let port = CellPerCpu::default();
            run_all(&port);
            let dynamic: &dyn PerCpu = &port;
            run_all(dynamic);
        }

        #[test]
        fn isolation_holds_across_two_handles() {
            run_isolation(&CellPerCpu::default(), &CellPerCpu::default());
        }

        /// A broken port that truncates the word to 32 bits must be
        /// rejected by the round-trip check.
        #[derive(Default)]
        struct TruncatingPerCpu {
            base: AtomicUsize,
        }

        impl PerCpu for TruncatingPerCpu {
            fn read_self_base(&self) -> usize {
                self.base.load(Ordering::Relaxed)
            }
            unsafe fn write_self_base(&self, base: usize) {
                self.base.store(base & 0xFFFF_FFFF, Ordering::Relaxed);
            }
        }

        #[test]
        #[should_panic(expected = "must round-trip unchanged")]
        fn suite_rejects_a_truncating_port() {
            run_all(&TruncatingPerCpu::default());
        }

        /// A broken port whose word leaks across handles (a shared
        /// static) must be rejected by the isolation check.
        struct SharedPerCpu;

        static SHARED_WORD: AtomicUsize = AtomicUsize::new(0);

        impl PerCpu for SharedPerCpu {
            fn read_self_base(&self) -> usize {
                SHARED_WORD.load(Ordering::Relaxed)
            }
            unsafe fn write_self_base(&self, base: usize) {
                SHARED_WORD.store(base, Ordering::Relaxed);
            }
        }

        #[test]
        #[should_panic(expected = "must not be disturbed")]
        fn isolation_rejects_a_shared_word() {
            run_isolation(&SharedPerCpu, &SharedPerCpu);
        }
    }
}
