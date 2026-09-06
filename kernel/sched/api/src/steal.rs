//! Where a CPU starts its work-stealing scan.
//!
//! Every policy that owns per-CPU run queues steals from a victim when its
//! own queues run dry, and every one of them walks the whole CPU list
//! circularly from some offset. If that offset were fixed, each idle CPU
//! would probe CPU 0 first and they would convoy on one queue's lock; a
//! rotating start spreads them out.
//!
//! That decorrelation is not policy — the policies differ in vruntime,
//! deadline, and band, not in where a scan begins — so it lives here once
//! rather than three times.

use alloc::boxed::Box;
use alloc::vec::Vec;

use tairix_rng::{NonCryptoRng, RandU64};
use tairix_sync::SpinLock;

use crate::arch::CpuId;

/// Per-CPU generators supplying each CPU's scan start.
///
/// One generator per CPU, indexed by the *calling* CPU, so the draw is always
/// uncontended: an idle CPU advances its own stream instead of serialising on
/// a shared one. `start` takes `&self`, so each generator's `&mut self`
/// stepping sits behind a [`SpinLock`] whose only ever contender would be the
/// same CPU.
///
/// The generator is deliberately the *predictable* one
/// ([`NonCryptoRng`]): what a scan start needs is decorrelation, and an
/// observer who can predict which CPU is probed first learns nothing they
/// could act on. Seeds are the CPU index, which is why no seed material is
/// threaded in from the boot path. **If a scan start ever feeds a security
/// decision — a placement an adversary could steer, an identifier that
/// leaves the kernel — that reasoning stops holding and this needs both an
/// unpredictable generator and a real per-boot seed.**
pub struct StealScan {
    per_cpu: Box<[SpinLock<NonCryptoRng>]>,
}

impl StealScan {
    /// Build one generator per CPU, each on its own stream.
    ///
    /// `SplitMix64` avalanches the seed, so seeding with the bare CPU index
    /// gives unrelated streams and no shared constant has to be agreed on.
    #[must_use]
    pub fn new(cpus: u32) -> Self {
        let mut per_cpu = Vec::with_capacity(cpus as usize);
        for cpu in 0..cpus {
            per_cpu.push(SpinLock::new(NonCryptoRng::seed_from_u64(u64::from(cpu))));
        }
        Self {
            per_cpu: per_cpu.into_boxed_slice(),
        }
    }

    /// The index in `0..cpus` at which `cpu` should begin its scan.
    ///
    /// Unbiased for any `cpus`, power of two or not. A `cpu` outside the
    /// range this was built for, or a zero `cpus`, starts the scan at 0 —
    /// which is a correct start for a circular walk, so a caller never has an
    /// error to handle on a path that is only choosing where to look first.
    #[must_use]
    pub fn start(&self, cpu: CpuId, cpus: usize) -> usize {
        let Some(rng) = self.per_cpu.get(cpu as usize) else {
            return 0;
        };
        let bound = u64::try_from(cpus).unwrap_or(u64::MAX);
        usize::try_from(rng.lock().next_below(bound)).unwrap_or(0)
    }
}

impl core::fmt::Debug for StealScan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StealScan")
            .field("cpus", &self.per_cpu.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::StealScan;
    use alloc::vec::Vec;

    #[test]
    fn a_start_is_always_a_real_cpu_index() {
        for cpus in [1usize, 2, 3, 4, 7, 8, 64] {
            let scan = StealScan::new(u32::try_from(cpus).expect("a small cpu count"));
            for cpu in 0..cpus {
                for _ in 0..256 {
                    let start = scan.start(u32::try_from(cpu).expect("a small cpu id"), cpus);
                    assert!(start < cpus, "{start} is not a CPU of {cpus}");
                }
            }
        }
    }

    /// The whole point: two CPUs must not walk the same order, or they would
    /// contend on the same victim's queue lock every time.
    #[test]
    fn each_cpu_draws_from_its_own_stream() {
        let cpus = 8usize;
        let scan = StealScan::new(8);
        let streams: Vec<Vec<usize>> = (0..8u32)
            .map(|cpu| (0..64).map(|_| scan.start(cpu, cpus)).collect())
            .collect();
        for (i, a) in streams.iter().enumerate() {
            for b in &streams[i + 1..] {
                assert_ne!(a, b, "two CPUs share a scan-start stream");
            }
        }
    }

    /// A start must cover every CPU, including on counts that are not powers
    /// of two — where a hand-rolled modulo would bias the low indices.
    #[test]
    fn starts_cover_every_cpu_without_bias() {
        for cpus in [3usize, 5, 6, 7, 10] {
            let scan = StealScan::new(u32::try_from(cpus).expect("a small cpu count"));
            let mut counts = alloc::vec![0u32; cpus];
            for _ in 0..(cpus * 4_000) {
                counts[scan.start(0, cpus)] += 1;
            }
            for (cpu, count) in counts.iter().enumerate() {
                assert!(
                    *count > 3_000 && *count < 5_000,
                    "cpus {cpus}: CPU {cpu} drawn {count} times"
                );
            }
        }
    }

    /// A count or CPU the table was not built for must answer with a usable
    /// start rather than an out-of-range index.
    #[test]
    fn out_of_range_inputs_start_at_zero() {
        let scan = StealScan::new(2);
        assert_eq!(scan.start(9, 2), 0, "an unknown CPU has no stream");
        assert_eq!(scan.start(0, 0), 0, "no CPUs means nothing to scan");
    }
}
