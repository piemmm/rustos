//! The bounded, kernel-held **crash-record store** and the safe user-stack
//! reader the user-fault kill path fills it from
//! (`plans/FIX-WILD.md` Stage 2).
//!
//! When a user task is killed by an unresolvable memory fault the resolver
//! records a post-mortem [`CrashRecord`] here — the faulting identity, the
//! cause class, the load-relative program counter and backtrace, and the
//! register file. A privileged debugger reads them back through the
//! capability-gated `CAP_SYSINFO_KERNEL` System Information query
//! ([`tairix_abi::sysinfo::SysinfoQueryId::CRASH_RECORD`]); the store is
//! served directly from this crate, exactly like the seat registry and the
//! IRQ table, because the state it exposes lives here.
//!
//! # Why the store is bounded
//!
//! The store is a fixed-capacity newest-first ring of the most recent
//! [`MAX_CRASH_RECORDS`] crashes — the `dmesg`-class rolling diagnostic
//! buffer, not an unbounded log that a crash loop could grow without
//! limit. It is written only on the fault path of a task that is already
//! dying (never on any running program's hot path), and eviction of the
//! oldest record keeps the footprint constant regardless of how many tasks
//! crash.
//!
//! # The user-stack reader is the safety linchpin
//!
//! [`UserStackReader`] walks the crashing task's **user** stack from
//! **kernel** context, so it dereferences untrusted, possibly-corrupt user
//! pointers. It reads every word through the capability-checked
//! [`copy_in`] path and returns [`None`] the instant a read faults, so the
//! shared arch-neutral unwinder ([`tairix_arch_api::backtrace::walk`]) ends
//! the walk cleanly and the kernel never takes a fault inside the fault
//! handler.

use alloc::vec::Vec;

use tairix_abi::sysinfo::CrashRecord;
use tairix_arch_api::backtrace::StackReader;
use tairix_kernel_mem::{copy_in, PhysMap, UserAddressSpace, VirtAddr};
use tairix_sync::RwLock;

/// Maximum number of crash records retained.
///
/// A rolling diagnostic buffer, sized to hold plenty of recent crashes for
/// a debugger to page through while keeping the footprint fixed. When the
/// ring is full the oldest record is evicted so a crash loop can never grow
/// the store without bound. This is a deliberately bounded diagnostic
/// buffer, like the kernel's other rolling logs, not a resource capacity
/// that must scale with the machine.
pub const MAX_CRASH_RECORDS: usize = 64;

/// The bounded, newest-first store of recent user-fault crash records.
///
/// Interior-mutable so a `&CrashStore` borrow suffices for both the write
/// on the (dying-task) fault path and the read on the introspect path — the
/// same shape the kernel's other kernel-held introspection sources use. All
/// access is serialised by one [`RwLock`]; a read never blocks a read.
pub struct CrashStore {
    log: RwLock<Vec<CrashRecord>>,
}

impl CrashStore {
    /// Construct an empty store.
    ///
    /// `const` so the kernel can declare its single store as a `static`
    /// without lazy initialisation.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            log: RwLock::new(Vec::new()),
        }
    }

    /// Record `rec` as the most recent crash, evicting the oldest record
    /// when the ring is already at [`MAX_CRASH_RECORDS`].
    ///
    /// Newest-first: the just-recorded crash is index `0`, which is the
    /// order a debugger most often wants. Called only on the fault path of a
    /// task that will never execute another instruction.
    pub fn record(&self, rec: CrashRecord) {
        let mut log = self.log.write();
        log.insert(0, rec);
        log.truncate(MAX_CRASH_RECORDS);
    }

    /// Encode up to `max_records` [`CrashRecord`]s beginning at record index
    /// `offset` (newest first), packed little-endian back-to-back.
    ///
    /// An `offset` past the end returns an empty `Vec` — the paging
    /// terminator, never an error.
    #[must_use]
    pub fn page(&self, offset: u64, max_records: usize) -> Vec<u8> {
        let log = self.log.read();
        let mut out = Vec::new();
        for rec in log
            .iter()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
            .take(max_records)
        {
            out.extend_from_slice(&rec.to_le_bytes());
        }
        out
    }

    /// Number of records currently held. Test-only observer.
    #[cfg(test)]
    #[must_use]
    pub fn len(&self) -> usize {
        self.log.read().len()
    }

    /// Whether the store holds no records. Test-only observer.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.log.read().is_empty()
    }

    /// The most recently recorded crash, decoded back. Test-only observer.
    #[cfg(test)]
    #[must_use]
    pub fn latest(&self) -> Option<CrashRecord> {
        self.log.read().first().copied()
    }
}

impl Default for CrashStore {
    fn default() -> Self {
        Self::new()
    }
}

/// The shared empty store referenced by
/// [`KernelSyscallHandlers`](crate::syscalls::KernelSyscallHandlers) until
/// the boot path installs the real one, so a crash before wiring records
/// into an inert sink rather than touching an uninstalled store.
pub static NULL_CRASH_STORE: CrashStore = CrashStore::new();

/// A [`StackReader`] over an *untrusted* user address space.
///
/// Every read goes through the capability-checked [`copy_in`] path and
/// returns [`None`] the instant the copy faults (an unmapped, reclaimed, or
/// deliberately corrupt user page), so the shared unwinder ends the walk
/// cleanly and the kernel never faults inside the fault handler. It never
/// itself faults, panics, or blocks — it fails closed.
pub struct UserStackReader<'a> {
    space: &'a dyn UserAddressSpace,
    physmap: &'a dyn PhysMap,
}

impl<'a> UserStackReader<'a> {
    /// Build a reader over the crashing task's live address space and the
    /// physical map backing it (the pair [`copy_in`] walks).
    #[must_use]
    pub fn new(space: &'a dyn UserAddressSpace, physmap: &'a dyn PhysMap) -> Self {
        Self { space, physmap }
    }
}

impl StackReader for UserStackReader<'_> {
    fn read_word(&self, addr: u64) -> Option<u64> {
        let mut buf = [0u8; 8];
        match copy_in(self.space, self.physmap, VirtAddr::new(addr), &mut buf) {
            Ok(()) => Some(u64::from_le_bytes(buf)),
            // A faulting copy over the untrusted user stack ends the walk
            // cleanly: the unwinder treats `None` as "stop here", never as a
            // value, so a corrupt or unmapped frame pointer can never take
            // the kernel down (fail closed).
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::sysinfo::{CrashFaultBucket, CrashFaultClass, CrashRecord};
    use tairix_abi::ProcId;

    fn record(pid: u64) -> CrashRecord {
        CrashRecord::new(
            ProcId::from_raw([
                u8::try_from(pid).unwrap(),
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
            ]),
            pid,
            0,
            0,
            false,
            CrashFaultClass::Wild,
            CrashFaultBucket::Wild,
            0,
            b"t",
        )
        .expect("fits")
    }

    #[test]
    fn records_are_newest_first_and_page_round_trips() {
        let store = CrashStore::new();
        store.record(record(1));
        store.record(record(2));
        assert_eq!(store.len(), 2);
        // Newest first: pid 2 then pid 1.
        let page = store.page(0, 8);
        let first = CrashRecord::from_bytes(&page[..CrashRecord::WIRE_LEN]).unwrap();
        assert_eq!(first.pid, 2);
        let second =
            CrashRecord::from_bytes(&page[CrashRecord::WIRE_LEN..2 * CrashRecord::WIRE_LEN])
                .unwrap();
        assert_eq!(second.pid, 1);
        assert_eq!(store.latest().unwrap().pid, 2);
    }

    #[test]
    fn the_ring_evicts_the_oldest_when_full() {
        let store = CrashStore::new();
        for pid in 0..(MAX_CRASH_RECORDS as u64 + 5) {
            store.record(record(pid));
        }
        assert_eq!(store.len(), MAX_CRASH_RECORDS);
        // The newest is the last recorded; the oldest five were evicted.
        assert_eq!(store.latest().unwrap().pid, MAX_CRASH_RECORDS as u64 + 4);
        let page = store.page(0, MAX_CRASH_RECORDS);
        let oldest_kept =
            CrashRecord::from_bytes(&page[(MAX_CRASH_RECORDS - 1) * CrashRecord::WIRE_LEN..])
                .unwrap();
        assert_eq!(oldest_kept.pid, 5);
    }

    #[test]
    fn an_offset_past_the_end_is_the_empty_terminator() {
        let store = CrashStore::new();
        store.record(record(1));
        assert!(store.page(5, 8).is_empty());
    }
}
