//! The five worker load units (`plans/STRESSTEST.md` §7.2).
//!
//! Each unit is one **bounded, restartable** piece of work: the worker's
//! outer loop re-runs its unit until the controller ends it, so the load
//! is continuous while every individual unit terminates. Deliberately
//! tight loops are the entire point here — the workers *are* the load —
//! and they run as ordinary preemptible user tasks for a bounded run.
//!
//! Disk-touching units act only beneath the worker's scratch directory,
//! through the injected [`Scratch`] seam: the program half backs it with
//! the real `fs_*` syscalls; host tests inject an in-memory fake. A typed
//! refusal (a resource limit, a full volume, a capability denial) is an
//! **expected outcome** reported once and exited with
//! [`crate::worker::REFUSED_EXIT`], never retried until it works.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// How one load unit ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnitOutcome {
    /// The unit completed; the worker's loop re-runs it.
    Done,
    /// A typed resource refusal ended the unit: reported once, exit
    /// [`crate::worker::REFUSED_EXIT`], counted by the controller as an
    /// expected outcome.
    Refused(&'static str),
    /// Anything else went wrong (an I/O fault, corrupt read-back): the
    /// worker states it and exits 1, failing the run.
    Failed(&'static str),
}

/// Why a scratch operation could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScratchError {
    /// A typed resource refusal (`ENOSPC`, a limit, a capability denial).
    Refused(&'static str),
    /// A genuine failure (device fault, unexpected error).
    Failed(&'static str),
}

/// The scratch-directory filesystem seam a disk-touching unit drives.
///
/// Path-based and minimal: exactly the operations the units need, backed
/// by the real syscalls in the program half and by an in-memory fake in
/// host tests.
pub trait Scratch {
    /// Create (or truncate) the file at `path`, ready for writing.
    ///
    /// # Errors
    ///
    /// [`ScratchError`], classified by the implementation.
    fn create(&mut self, path: &str) -> Result<(), ScratchError>;
    /// Write `data` at `offset` of the file at `path`, completely.
    ///
    /// # Errors
    ///
    /// [`ScratchError`], classified by the implementation.
    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<(), ScratchError>;
    /// Read exactly `buf.len()` bytes at `offset` of the file at `path`.
    ///
    /// # Errors
    ///
    /// [`ScratchError`], classified by the implementation.
    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(), ScratchError>;
    /// Flush the file at `path` to its backing store.
    ///
    /// # Errors
    ///
    /// [`ScratchError`], classified by the implementation.
    fn sync(&mut self, path: &str) -> Result<(), ScratchError>;
    /// Remove the file at `path`.
    ///
    /// # Errors
    ///
    /// [`ScratchError`], classified by the implementation.
    fn remove(&mut self, path: &str) -> Result<(), ScratchError>;
}

/// The write/read granule of the io unit: small buffers and frequent
/// syncs exercise the write path, not throughput.
const IO_CHUNK: usize = 4 << 10;
/// The io unit syncs after this many chunks.
const IO_SYNC_EVERY: u64 = 16;
/// The write granule of the hdd unit: large sequential chunks exercise
/// throughput and free-space accounting.
const HDD_CHUNK: usize = 64 << 10;
/// Files a cache unit spreads its tree budget over.
const CACHE_FILES: u64 = 16;

/// The scratch file a worker of `kind` and `index` owns. One deterministic
/// name per worker: workers never contend on one file, and the
/// controller's teardown can remove exactly what the run created.
#[must_use]
pub fn scratch_file(scratch: &str, kind: crate::worker::WorkerKind, index: u32) -> String {
    format!("{scratch}/stress-{}-{index}.dat", kind.as_str())
}

/// The `n`-th file of a cache worker's re-read tree.
#[must_use]
pub fn cache_file(scratch: &str, index: u32, n: u64) -> String {
    format!("{scratch}/stress-cache-{index}-{n}.dat")
}

/// One CPU unit: a syscall-free integer/float arithmetic mix over
/// `iterations` rounds, folded into a value the caller must consume (via
/// `core::hint::black_box`) so the work cannot be optimised away.
/// Exercises preemption: a worker running this never yields voluntarily.
#[must_use]
pub fn cpu_unit(seed: u64, iterations: u64) -> u64 {
    let mut acc = seed | 1;
    let mut float = 1.000_000_1_f64;
    let mut i = 0;
    while i < iterations {
        acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        acc ^= acc >> 29;
        float = (float * 1.000_000_119) % 3.5;
        i += 1;
    }
    acc ^ (float.to_bits())
}

/// One vm unit: ensure `buf` holds `target` bytes (allocation refusal is
/// the typed vm refusal), then touch every page in a rotating pattern so
/// the pass faults each page and rewrites its first byte — driving
/// allocation, fault-in, and (once the tier is live for running tasks)
/// compress-out and fault-in of the swappable worker's pages.
pub fn vm_unit(buf: &mut Vec<u8>, target: u64, pass: u64) -> UnitOutcome {
    /// The touch stride: one cache-line-sized step per page keeps the
    /// unit's cost proportional to pages, not bytes.
    const PAGE: usize = 4 << 10;
    let target = usize::try_from(target).unwrap_or(usize::MAX);
    if buf.len() < target {
        let grow = target - buf.len();
        if buf.try_reserve_exact(grow).is_err() {
            return UnitOutcome::Refused("memory allocation refused");
        }
        buf.resize(target, 0);
    }
    let rotate = u8::try_from(pass % 251).unwrap_or(0);
    let mut offset = 0;
    while offset < buf.len() {
        let value = buf[offset].wrapping_add(1);
        buf[offset] = value ^ rotate;
        offset += PAGE;
    }
    UnitOutcome::Done
}

/// One io unit: stream-write `budget` bytes through small chunks with a
/// periodic sync every few chunks, then read the head chunk back and
/// rewrite it — the write path and the block cache, not capacity. The
/// file persists between units (the next unit rewrites it); the
/// controller's teardown removes it.
pub fn io_unit(fs: &mut dyn Scratch, path: &str, budget: u64) -> UnitOutcome {
    let mut chunk = [0u8; IO_CHUNK];
    if let Err(err) = fs.create(path) {
        return err.into();
    }
    let chunks = budget.div_ceil(IO_CHUNK as u64).max(1);
    let mut n = 0;
    while n < chunks {
        fill_pattern(&mut chunk, n);
        if let Err(err) = fs.write(path, n * IO_CHUNK as u64, &chunk) {
            return err.into();
        }
        n += 1;
        if n % IO_SYNC_EVERY == 0 {
            if let Err(err) = fs.sync(path) {
                return err.into();
            }
        }
    }
    if let Err(err) = fs.sync(path) {
        return err.into();
    }
    let mut back = [0u8; IO_CHUNK];
    if let Err(err) = fs.read(path, 0, &mut back) {
        return err.into();
    }
    fill_pattern(&mut chunk, 0);
    if back != chunk {
        return UnitOutcome::Failed("io read-back mismatch");
    }
    UnitOutcome::Done
}

/// One hdd unit: write `budget` bytes in large sequential patterned
/// chunks, sync, verify a head/middle/tail sample, and delete the file —
/// throughput and free-space accounting, self-cleaning per unit.
pub fn hdd_unit(fs: &mut dyn Scratch, path: &str, budget: u64) -> UnitOutcome {
    // Heap-allocated: two 64 KiB buffers belong on the worker's heap, not
    // its stack. Allocation refusal is the typed vm-style refusal.
    let mut chunk = alloc::vec::Vec::new();
    if chunk.try_reserve_exact(HDD_CHUNK).is_err() {
        return UnitOutcome::Refused("memory allocation refused");
    }
    chunk.resize(HDD_CHUNK, 0);
    let mut back = alloc::vec::Vec::new();
    if back.try_reserve_exact(HDD_CHUNK).is_err() {
        return UnitOutcome::Refused("memory allocation refused");
    }
    back.resize(HDD_CHUNK, 0);
    if let Err(err) = fs.create(path) {
        return err.into();
    }
    let chunks = budget.div_ceil(HDD_CHUNK as u64).max(1);
    let mut n = 0;
    while n < chunks {
        fill_pattern(&mut chunk, n);
        if let Err(err) = fs.write(path, n * HDD_CHUNK as u64, &chunk) {
            return err.into();
        }
        n += 1;
    }
    if let Err(err) = fs.sync(path) {
        return err.into();
    }
    // Verify a bounded sample — first, middle, last chunk — rather than
    // re-reading the whole budget: corruption is detected, the unit stays
    // write-dominated.
    for probe in [0, chunks / 2, chunks - 1] {
        if let Err(err) = fs.read(path, probe * HDD_CHUNK as u64, &mut back) {
            return err.into();
        }
        fill_pattern(&mut chunk, probe);
        if back != chunk {
            return UnitOutcome::Failed("hdd verify mismatch");
        }
    }
    if let Err(err) = fs.remove(path) {
        return err.into();
    }
    UnitOutcome::Done
}

/// One cache unit: ensure the worker's re-read tree exists (its budget
/// spread over a small fixed set of files), then walk it re-reading
/// every file — repeated cold walks churn the filesystem and block caches
/// so their ledgers move.
pub fn cache_unit(fs: &mut dyn Scratch, scratch: &str, index: u32, budget: u64) -> UnitOutcome {
    let per_file = (budget / CACHE_FILES).clamp(1, 64 << 10);
    let mut chunk = [0u8; 4 << 10];
    let mut n = 0;
    while n < CACHE_FILES {
        let path = cache_file(scratch, index, n);
        // Probe with a read; a missing file is (re)built. Any refusal is
        // the typed outcome.
        let mut probe = [0u8; 1];
        if fs.read(&path, 0, &mut probe).is_err() {
            if let Err(err) = fs.create(&path) {
                return err.into();
            }
            let mut written = 0u64;
            while written < per_file {
                fill_pattern(&mut chunk, n.wrapping_add(written));
                let take = usize::try_from((per_file - written).min(chunk.len() as u64))
                    .unwrap_or(chunk.len());
                if let Err(err) = fs.write(&path, written, &chunk[..take]) {
                    return err.into();
                }
                written += take as u64;
            }
            if let Err(err) = fs.sync(&path) {
                return err.into();
            }
        }
        // The re-read walk: pull the whole file back through the caches.
        let mut back = [0u8; 4 << 10];
        let mut offset = 0u64;
        while offset < per_file {
            let take =
                usize::try_from((per_file - offset).min(chunk.len() as u64)).unwrap_or(chunk.len());
            if let Err(err) = fs.read(&path, offset, &mut back[..take]) {
                return err.into();
            }
            offset += take as u64;
        }
        n += 1;
    }
    UnitOutcome::Done
}

/// Every scratch file a run's workers could have created, for the
/// controller's teardown removal: the run cleans up after itself on every
/// exit path — completion, timeout, and the signal ends alike — removing
/// exactly the deterministic names its own workers own, never anything
/// else in a user-supplied `--temp-path` directory.
#[must_use]
pub fn run_scratch_paths(workers: &crate::command::Workers, scratch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for index in 0..workers.io {
        paths.push(scratch_file(scratch, crate::worker::WorkerKind::Io, index));
    }
    for index in 0..workers.hdd {
        paths.push(scratch_file(scratch, crate::worker::WorkerKind::Hdd, index));
    }
    for index in 0..workers.cache {
        for n in 0..CACHE_FILES {
            paths.push(cache_file(scratch, index, n));
        }
    }
    paths
}

/// Deterministic chunk pattern: byte `i` of chunk `n` is a function of
/// both, so a misplaced or stale block never verifies.
fn fill_pattern(chunk: &mut [u8], n: u64) {
    // Byte 3 of the multiplied value is exactly bits 24..32 — the mix the
    // pattern keys on — extracted without a truncating cast.
    let tag = n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_le_bytes()[3];
    let mut low: u8 = 0;
    for byte in chunk.iter_mut() {
        *byte = tag ^ low;
        low = low.wrapping_add(1);
    }
}

impl From<ScratchError> for UnitOutcome {
    fn from(err: ScratchError) -> Self {
        match err {
            ScratchError::Refused(reason) => Self::Refused(reason),
            ScratchError::Failed(reason) => Self::Failed(reason),
        }
    }
}
