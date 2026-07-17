//! The kernel-held introspection source the `sysinfo_introspect` syscall
//! serves (`PREREQUISITES.md` P-C).
//!
//! The live data — the process table, kernel memory accounting, the mount
//! table, machine identity, uptime, and any task's resource limits — lives
//! in the binding kernel (`tairix-kernel`), which owns the `CapTable`, the
//! scheduler, the frame allocator, and the mount table. This trait is the
//! seam `kernel/core` reaches it through, exactly as [`crate::hwtree`]'s
//! [`HwTreeSource`](crate::hwtree::HwTreeSource) is the seam for the
//! discovered hardware tree. Each method returns an *already wire-encoded*
//! snapshot, so `kernel/core` stays ignorant of both the data's storage and
//! the `lib/abi` wire layout — the single encoder lives beside the state it
//! serialises.
//!
//! **The source always answers with the whole system's state and never
//! narrows by principal.** The `sysinfo_introspect` syscall is held only by
//! the user-space `sysinfod` broker, which re-derives every per-client scope
//! against each requester's kernel-attested `Origin`. Keeping the kernel
//! primitive global-only holds the ring-0 attack surface down while the
//! kernel stays the identity authority (every record field is filled from
//! attested task state, never a caller claim).
//!
//! Every method fails closed: a build with no source installed answers
//! [`Errno::NotImplemented`] so an early `sysinfo_introspect` announces an
//! inert interface rather than fabricating data.

use alloc::vec::Vec;

use tairix_abi::{Errno, ProcId};

/// The kernel-held live system state the `sysinfo_introspect` syscall serves.
///
/// The boot path installs an implementation backed by the binding kernel's
/// authoritative `CapTable` + scheduler + allocators + mount table; the
/// handler in [`crate::syscalls`] copies the encoded bytes out to the
/// (capability-gated, `CAP_SYSINFO_INTROSPECT`) caller.
///
/// `Sync` because the single installed source is shared by the per-CPU
/// syscall handlers, exactly like [`crate::hwtree::HwTreeSource`].
pub trait IntrospectSource: Sync {
    /// Encode up to `max_records` live [`tairix_abi::sysinfo::ProcessRecord`]s
    /// beginning at record index `offset`, in a stable order, packed
    /// little-endian back-to-back.
    ///
    /// The order must be stable across paged calls so a broker walking the
    /// list never skips or repeats a record. Every field is filled from
    /// kernel-attested task state, never a caller claim. An `offset` past the
    /// end returns an empty `Vec` (the paging terminator), never an error.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn processes(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current
    /// [`tairix_abi::sysinfo::KernelMemoryStats`].
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn kernel_memory(&self) -> Result<Vec<u8>, Errno>;

    /// Encode up to `max_records` [`tairix_abi::sysinfo::MountRecord`]s
    /// beginning at record index `offset`, in a stable order, packed
    /// little-endian back-to-back. An `offset` past the end returns an empty
    /// `Vec`.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn mounts(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current
    /// [`tairix_abi::sysinfo::SystemIdentity`] (machine id, OS version,
    /// hostname).
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn identity(&self) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current [`tairix_abi::sysinfo::Uptime`].
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn uptime(&self) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current
    /// [`tairix_abi::sysinfo::LoadAverage`]: the damped 1/5/15-minute
    /// run-queue averages plus the live runnable/total-task and
    /// logged-in-user censuses, all read from kernel-attested state.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn load_average(&self) -> Result<Vec<u8>, Errno>;

    /// The wire image of the `[ResourceLimitRecord; LimitKind::COUNT]` report
    /// for the task whose kernel-attested process-instance identity is
    /// `proc_id`.
    ///
    /// The target is named by its unforgeable [`ProcId`] (stable across PID
    /// reuse), resolved against the authoritative `CapTable`; the broker
    /// passes the *client's own* attested `ProcId` so a client reads only its
    /// own limits.
    ///
    /// # Errors
    ///
    /// * [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    /// * [`Errno::NotFound`] if no live task carries `proc_id` (fail closed).
    fn task_limits(&self, proc_id: ProcId) -> Result<Vec<u8>, Errno>;

    /// Encode up to `max_records`
    /// [`tairix_abi::sysinfo::UserDirectoryRecord`]s beginning at record
    /// index `offset`, in a stable order, packed little-endian
    /// back-to-back.
    ///
    /// The directory pairs each account's uid with its username and
    /// carries **no** credential material — password records stay behind
    /// the capability-gated `users_db_read` syscall. A kernel holding no
    /// user database answers with an empty directory (never a fabricated
    /// account); an `offset` past the end returns an empty `Vec` (the
    /// paging terminator), never an error.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn user_directory(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;

    /// Encode up to `max_records` [`tairix_abi::sysinfo::CpuTimeRecord`]s
    /// beginning at CPU index `offset`, in ascending CPU order, packed
    /// little-endian back-to-back.
    ///
    /// Each record carries the CPU's cumulative busy time (accounted on
    /// the scheduler's dispatch bracket) and the idle remainder of the
    /// same monotonic sample, all from kernel-attested state. An `offset`
    /// past the last CPU returns an empty `Vec` (the paging terminator),
    /// never an error.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn cpu_times(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current
    /// [`tairix_abi::sysinfo::MemoryPressureStats`]: the live band (a
    /// fresh sample), the derived watermarks in force, the reserve
    /// floor, the free/total readings, and the per-band entry counters
    /// since boot (`plans/STRESSTEST.md` ST1).
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn memory_pressure(&self) -> Result<Vec<u8>, Errno>;

    /// Encode up to `max_records`
    /// [`tairix_abi::sysinfo::ReclaimClassRecord`]s beginning at class
    /// index `offset`, in class-id order, packed little-endian
    /// back-to-back — the reclaimable-cache ledger aggregated across
    /// every registered live cache. An `offset` past the last class
    /// returns an empty `Vec` (the paging terminator), never an error.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn reclaim(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;

    /// The wire image of the current
    /// [`tairix_abi::sysinfo::RamzipStats`]: counters only, never page
    /// contents or key material; an undriven tier truthfully reports
    /// idle zeros.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn ramzip(&self) -> Result<Vec<u8>, Errno>;

    /// Encode up to `max_records`
    /// [`tairix_abi::sysinfo::CpuLoadRecord`]s beginning at CPU index
    /// `offset`, in ascending CPU order, packed little-endian
    /// back-to-back: the run-queue depth sample plus the context-switch
    /// and preemption counters (the busy/idle time split stays in
    /// [`cpu_times`](Self::cpu_times) — no figure is served twice). An
    /// `offset` past the last CPU returns an empty `Vec` (the paging
    /// terminator), never an error.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullIntrospectSource`].
    fn cpu_load(&self, offset: u64, max_records: usize) -> Result<Vec<u8>, Errno>;
}

/// The fail-closed default installed before the binding kernel wires the real
/// source: every domain answers [`Errno::NotImplemented`], so an early
/// `sysinfo_introspect` announces an inert interface rather than fabricating
/// data.
pub struct NullIntrospectSource;

impl IntrospectSource for NullIntrospectSource {
    fn processes(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn kernel_memory(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn mounts(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn identity(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn uptime(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn load_average(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn task_limits(&self, _proc_id: ProcId) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn user_directory(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn cpu_times(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn memory_pressure(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn reclaim(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn ramzip(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn cpu_load(&self, _offset: u64, _max_records: usize) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared fail-closed default, referenced by
/// [`KernelSyscallHandlers`](crate::syscalls) until the binding kernel
/// installs the real source.
pub static NULL_INTROSPECT: NullIntrospectSource = NullIntrospectSource;
