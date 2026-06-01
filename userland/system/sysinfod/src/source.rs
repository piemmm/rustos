//! The data-provider seam: everything `sysinfod` knows about live system
//! state is read through [`SysinfoSource`].
//!
//! The dispatcher in [`crate::service`] owns *policy* — request decoding,
//! capability enforcement (`AGENTS.md` §16.6), paging bounds, and audit
//! logging — and nothing else. The *data* is supplied by an implementation
//! of [`SysinfoSource`] injected by `init` when it starts the service: on a
//! running kernel this is a thin shim over the kernel's process table and
//! memory accounting; in tests it is an in-memory fixture. Splitting the
//! two keeps the security-relevant code free of any particular kernel
//! plumbing.

use rustos_abi::sysinfo::{KernelMemoryStats, ProcessRecord, SystemIdentity, Uptime};
use rustos_abi::{CapabilityQuery, Errno};

/// The authenticated principal on whose behalf a request is served.
///
/// The identity is supplied by the IPC layer, never by the caller's own
/// payload (`AGENTS.md` §5.4.1): `sysinfod` trusts the kernel-provided
/// `uid` and capability view, not bytes on the wire.
pub struct Caller<'a> {
    /// Owning user identifier of the requesting task.
    pub uid: u32,
    /// The caller's effective capability set, queried through the
    /// object-safe [`CapabilityQuery`] seam so `sysinfod` never names a
    /// concrete `CapabilitySet` type.
    pub capabilities: &'a dyn CapabilityQuery,
}

/// Which processes a process-list query should observe.
///
/// The scope is decided by the dispatcher from the query identifier — never
/// by the caller — so a self-scoped request can never be widened into a
/// global one without the `CAP_SYSINFO_GLOBAL` gate that the global query
/// carries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProcessScope {
    /// Only processes owned by [`Caller::uid`]. The self-scoped, ungated
    /// observer.
    Caller,
    /// Every process on the system. Reached only after the
    /// `CAP_SYSINFO_GLOBAL` check has passed.
    Global,
}

/// Read-only access to the live system state exposed by the `sysinfo` API.
///
/// Every method is fallible and returns a [`rustos_abi::Errno`]; a source
/// must never panic (`AGENTS.md` §2.9). Methods that answer a query whose
/// response is a sequence — the process lists — return a borrowed slice and
/// leave paging to the dispatcher, so the bounds logic lives in exactly one
/// place.
pub trait SysinfoSource {
    /// Return the records visible to `caller` under `scope`.
    ///
    /// The slice is returned whole; [`crate::serve`] applies the
    /// `offset`/`limit` paging from the request. Ordering is the source's
    /// responsibility and must be stable across paged calls so a client
    /// walking the list never skips or repeats a record.
    fn process_records(
        &self,
        caller: &Caller<'_>,
        scope: ProcessScope,
    ) -> Result<&[ProcessRecord], Errno>;

    /// Return kernel memory statistics.
    ///
    /// Reached only after the `CAP_SYSINFO_KERNEL` gate has passed.
    fn kernel_memory_stats(&self, caller: &Caller<'_>) -> Result<KernelMemoryStats, Errno>;

    /// Return the encoded detected hardware tree.
    ///
    /// Reached only after the `CAP_SYSINFO_HW` gate has passed. The bytes
    /// are passed through verbatim: the hardware-tree wire format is owned
    /// by `lib/abi` (`AGENTS.md` §18.1), not by this service, so `sysinfod`
    /// frames them without interpreting them.
    fn hardware_tree(&self, caller: &Caller<'_>) -> Result<&[u8], Errno>;

    /// Return the machine identity (machine ID, OS version, hostname).
    fn system_identity(&self, caller: &Caller<'_>) -> Result<SystemIdentity, Errno>;

    /// Return system uptime and boot wall-clock time.
    fn uptime(&self, caller: &Caller<'_>) -> Result<Uptime, Errno>;
}
