//! The one live, mutable user address space of a process, shared by every
//! thread of its thread group (`plans/THREADS.md` decision 4).
//!
//! A process's address space is process-scoped state, but the tasks that
//! mutate it are threads. [`ProcessSpace`] is therefore refcounted and
//! internally locked: each thread's kthread control block holds an
//! [`Arc`](alloc::sync::Arc) clone, and the per-CPU publication the syscall
//! producers reach through [`crate::kthread::with_current_live_space`] takes
//! the lock for the duration of one operation. Ownership no longer rests
//! with a single task, so a second thread of the same process is a matter of
//! cloning the handle rather than of finding a new owner.
//!
//! # Why a spin lock
//!
//! The demand-paging fault resolver mutates the space with interrupts
//! masked and cannot park, so the lock must be a spin lock rather than a
//! [`SleepLock`](crate::SleepLock). The discipline that makes that safe is
//! **never park while holding it**: every [`LiveUserSpace`] method is
//! park-free (frame allocation, a page-table write, a TLB flush), and file
//! content is read into a kernel buffer *before* the mapping call, so the
//! critical section stays bounded and short.
//!
//! The space is never touched from an interrupt handler, so a plain
//! [`SpinLock`] is correct — an `IrqSafeSpinLock` would mask interrupts for
//! page-table work no ISR can contend on.
//!
//! # Lock order
//!
//! `ProcessSpace` is acquired **before** the address-space registry
//! ([`crate::AddressSpaceRegistry`]), never after: the snapshot publication
//! reads each freshly mapped page's translation out of the live space while
//! it holds the registry write guard. No path takes the registry first and
//! the live space second.

use alloc::boxed::Box;

use tairix_kernel_mem::LiveUserSpace;
use tairix_sync::SpinLock;

/// A process's live, mutable user address space.
///
/// Constructed by the spawn path from the same architecture address space
/// the registry snapshot was frozen from, so the snapshot and the live
/// space describe one set of mappings. Dropped when the last thread of the
/// process releases its handle, which returns the space's frames — image,
/// stack, anonymous, device and page-table alike — to the allocator.
pub struct ProcessSpace {
    space: SpinLock<Box<dyn LiveUserSpace + Send>>,
}

impl ProcessSpace {
    /// Wrap `space` as the process's shared live address space.
    #[must_use]
    pub fn new(space: Box<dyn LiveUserSpace + Send>) -> Self {
        Self {
            space: SpinLock::new(space),
        }
    }

    /// Run `f` against the space under the lock.
    ///
    /// `f` must not park (see the module docs): it holds a spin lock the
    /// demand-paging fault path also takes with interrupts masked.
    pub fn with<R>(&self, f: impl FnOnce(&mut dyn LiveUserSpace) -> R) -> R {
        let mut guard = self.space.lock();
        f(&mut **guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::boxed::Box as StdBox;

    use tairix_kernel_mem::{
        AddressSpace, BootMemoryMap, FrameAllocator, HostPageTable, LiveSpace, MemoryRegion,
        PhysAddr, RegionKind, SimPhysMap, VirtAddr, PAGE_SIZE,
    };

    /// A host `LiveSpace` over simulated RAM — the same double the
    /// `live_producer` tests use, so the lock wrapper is exercised over a
    /// real `LiveUserSpace` rather than a bespoke stub.
    fn host_space() -> Box<dyn LiveUserSpace + Send> {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new((PAGE_SIZE * 16) as u64),
            length: (256 * PAGE_SIZE) as u64,
        });
        let frames: &'static FrameAllocator =
            StdBox::leak(StdBox::new(FrameAllocator::new(&map).expect("allocator")));
        let sim = SimPhysMap::new(PhysAddr::new((PAGE_SIZE * 16) as u64), 256 * PAGE_SIZE);
        let live = LiveSpace::new(
            AddressSpace::new(HostPageTable::new()),
            sim,
            frames,
            VirtAddr::new(0x4000_0000),
            8,
            VirtAddr::new(0x5000_0000),
            8,
            VirtAddr::new(0x6000_0000),
            8,
            VirtAddr::new(0x7000_0000),
            8,
            VirtAddr::new(0x8000_0000),
            8,
        )
        .expect("windows are valid");
        Box::new(live)
    }

    #[test]
    fn with_hands_the_closure_one_shared_space() {
        let shared = ProcessSpace::new(host_space());
        let first = shared
            .with(|space| space.reserve_anonymous(2))
            .expect("reservation fits the window");
        let second = shared
            .with(|space| space.reserve_anonymous(2))
            .expect("second reservation fits");
        // Distinct windows prove the second call observed the first's
        // reservation, so both reached one space rather than a copy.
        assert_ne!(first, second);
    }

    #[test]
    fn the_lock_excludes_a_second_borrow_while_one_is_live() {
        let shared = ProcessSpace::new(host_space());
        shared.with(|_| {
            assert!(
                shared.space.try_lock().is_none(),
                "the space must not be reachable twice at once"
            );
        });
        assert!(
            shared.space.try_lock().is_some(),
            "the lock is released when the closure returns"
        );
    }

    #[test]
    fn the_handle_is_shareable_across_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        // Every thread of a process holds an `Arc<ProcessSpace>` clone, and
        // a thread may run on any CPU, so the handle must be both.
        assert_send_sync::<ProcessSpace>();
    }

    /// Two CPUs may publish the *same* handle at once — one per running
    /// thread of the process — and both must reach the one space. This is
    /// what the former unique `&mut` publication could not express.
    #[test]
    fn two_published_cpus_reach_one_space() {
        // CPU indices used by no other test in this crate, so parallel runs
        // never share the global per-CPU slots.
        const FIRST_CPU: u32 = 43;
        const SECOND_CPU: u32 = 44;

        let shared: &'static ProcessSpace =
            StdBox::leak(StdBox::new(ProcessSpace::new(host_space())));
        let _first = crate::kthread::publish_live_space_for_test(FIRST_CPU, shared);
        let _second = crate::kthread::publish_live_space_for_test(SECOND_CPU, shared);

        let from_first =
            crate::kthread::with_current_live_space(FIRST_CPU, |space| space.reserve_anonymous(2))
                .expect("the slot is published")
                .expect("reservation fits the window");
        let from_second =
            crate::kthread::with_current_live_space(SECOND_CPU, |space| space.reserve_anonymous(2))
                .expect("the slot is published")
                .expect("reservation fits the window");
        assert_ne!(
            from_first, from_second,
            "the second CPU observed the first CPU's reservation"
        );
    }
}
