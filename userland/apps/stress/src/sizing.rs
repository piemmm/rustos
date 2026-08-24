//! Sizing policy: how a run's byte targets are derived
//! (`plans/STRESSTEST.md` §7.3).
//!
//! Targets are **policies over discovered hardware, never frozen scalars**:
//! the memory target is sized from the boot-attested installed RAM
//! (`boot_facts`) and the disk target from the scratch volume's live free
//! space (the unprivileged `MOUNT_LIST` query), so the same command loads a
//! 1 GiB board and a 128 GiB server proportionally. An explicit
//! `--vm-bytes`/`--hdd-bytes` always wins; `--overcommit P` rescales the
//! *discovered* targets to `P` percent of the resource (over 100 pushes
//! into pressure — the resulting typed refusals are expected outcomes).
//!
//! Documented fallbacks: when a discovery is unavailable (a refused query,
//! an unlisted scratch volume) the policy falls back to a fixed
//! conservative per-worker figure — small enough to be safe on any
//! machine, honest enough to still generate load — rather than failing a
//! run whose purpose is the load itself.

use crate::command::RunSpec;

/// What the controller discovered about the machine before sizing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Discovered {
    /// Installed physical RAM (`boot_facts.memory_bytes`), when the query
    /// answered.
    pub ram_bytes: Option<u64>,
    /// Free space on the volume backing the scratch directory, when the
    /// mount walk found it.
    pub scratch_free_bytes: Option<u64>,
}

/// The per-worker byte targets a run dispatches with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Targets {
    /// Each vm worker's allocation target.
    pub vm_bytes: u64,
    /// Each hdd worker's file-size budget.
    pub hdd_bytes: u64,
    /// Each io worker's file-size budget (small by design: the io worker
    /// exercises the write/sync path, not capacity).
    pub io_bytes: u64,
    /// Each cache worker's scratch-tree budget.
    pub cache_bytes: u64,
}

/// Fallback per-worker memory target when installed RAM is unknown.
const VM_FALLBACK: u64 = 32 * MIB;
/// Fallback per-worker disk budget when the scratch free space is unknown.
const HDD_FALLBACK: u64 = 16 * MIB;
/// The io worker's fixed file budget: small buffers and frequent syncs are
/// the load, not capacity.
const IO_BYTES: u64 = 4 * MIB;
/// The cache worker's per-worker tree budget cap.
const CACHE_CAP: u64 = 8 * MIB;
/// Fallback per-worker cache-tree budget when free space is unknown.
const CACHE_FALLBACK: u64 = 4 * MIB;
/// No target sinks below this floor: a sub-64-KiB unit generates no
/// meaningful load and rounds to nothing on a large-block volume.
const FLOOR: u64 = 64 * KIB;
/// The default (no `--overcommit`) share of a discovered resource the
/// combined workers of a kind target, in percent: half the resource loads
/// the machine hard while leaving the system itself room to run.
const DEFAULT_SHARE_PERCENT: u32 = 50;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// Derive the per-worker byte targets for `spec` from `discovered`.
///
/// Explicit `--vm-bytes`/`--hdd-bytes` are used verbatim. Otherwise each
/// kind's workers share a slice of the discovered resource — half of it by
/// default, `--overcommit P` percent of it when given — split evenly, with
/// the documented fallbacks when discovery is unavailable and a 64 KiB
/// floor under every result.
#[must_use]
pub fn size_targets(spec: &RunSpec, discovered: &Discovered) -> Targets {
    let percent = spec.overcommit.unwrap_or(DEFAULT_SHARE_PERCENT);
    let vm_bytes = spec.vm_bytes.unwrap_or_else(|| {
        share_per_worker(discovered.ram_bytes, percent, spec.workers.vm, VM_FALLBACK)
    });
    let hdd_bytes = spec.hdd_bytes.unwrap_or_else(|| {
        share_per_worker(
            discovered.scratch_free_bytes,
            percent,
            spec.workers.hdd,
            HDD_FALLBACK,
        )
    });
    let cache_bytes = share_per_worker(
        discovered.scratch_free_bytes.map(|free| free / 16),
        100,
        spec.workers.cache,
        CACHE_FALLBACK,
    )
    .min(CACHE_CAP);
    Targets {
        vm_bytes,
        hdd_bytes,
        io_bytes: IO_BYTES,
        cache_bytes: cache_bytes.max(FLOOR),
    }
}

/// One worker's even share of `percent` percent of `resource`, floored at
/// [`FLOOR`]; `fallback` when the resource is undiscovered. The product is
/// computed in `u128`, so a 100 TB+ resource at a large overcommit cannot wrap
/// (sizes are 64-bit, intermediates wider).
fn share_per_worker(resource: Option<u64>, percent: u32, workers: u32, fallback: u64) -> u64 {
    let Some(resource) = resource else {
        return fallback;
    };
    let workers = u128::from(workers.max(1));
    let share = u128::from(resource) * u128::from(percent) / 100 / workers;
    u64::try_from(share).unwrap_or(u64::MAX).max(FLOOR)
}
