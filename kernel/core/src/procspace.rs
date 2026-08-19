//! The user-execution context a process shares with every thread of its
//! thread group (`plans/THREADS.md` decision 4): its one live, mutable
//! address space, and the port's register program for resuming a thread in
//! it.
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
//! The handle also carries the process's [`ProcessResume`] hook and its
//! port's [`EnterUser`] handle, because those are the other two things
//! process-wide that creating a thread needs. Keeping all three together is
//! what makes `thread_create` arch-neutral: it reaches the caller's own
//! handle ([`crate::kthread::current_process_space`]) and builds the new
//! thread's switch-in hook and entry from it, with no new per-arch producer
//! (decision 9).
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

use tairix_arch_api::{EnterUser, UserEntry};
use tairix_kernel_mem::LiveUserSpace;
use tairix_sync::SpinLock;

use crate::spawn::{thread_pre_resume, ProcessResume, UserThreadEntry};

/// A process's live, mutable user address space.
///
/// Constructed by the spawn path from the same architecture address space
/// the registry snapshot was frozen from, so the snapshot and the live
/// space describe one set of mappings. Dropped when the last thread of the
/// process releases its handle, which returns the space's frames — image,
/// stack, anonymous, device and page-table alike — to the allocator.
pub struct ProcessSpace {
    space: SpinLock<Box<dyn LiveUserSpace + Send>>,
    /// The port's switch-in hook for this process, shared by every thread.
    resume: ProcessResume,
    /// The port's "enter user mode" handle, so a thread created later is
    /// entered through the same transition the first one was.
    port: &'static dyn EnterUser,
}

#[cfg(test)]
impl ProcessSpace {
    /// A host-test context over `space`: an inert switch-in hook and a port
    /// double whose transition is never executed, so a test whose subject is
    /// the address-space half spells neither.
    pub(crate) fn for_test(space: Box<dyn LiveUserSpace + Send>) -> Self {
        Self::new(
            space,
            crate::test_arch::inert_process_resume(),
            &crate::test_arch::NEVER_ENTER_USER,
        )
    }
}

impl ProcessSpace {
    /// Wrap `space`, the process's switch-in hook, and its port's enter-user
    /// handle as the process's shared user-execution context.
    #[must_use]
    pub fn new(
        space: Box<dyn LiveUserSpace + Send>,
        resume: ProcessResume,
        port: &'static dyn EnterUser,
    ) -> Self {
        Self {
            space: SpinLock::new(space),
            resume,
            port,
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

    /// The per-thread switch-in hook for a thread of this process whose
    /// thread pointer is `tls_base`.
    #[must_use]
    pub fn thread_pre_resume(&self, tls_base: u64) -> Box<dyn FnMut(u64) + Send> {
        thread_pre_resume(&self.resume, tls_base)
    }

    /// The entry a thread of this process is dropped into user mode with.
    #[must_use]
    pub fn thread_entry(&self, regs: UserEntry) -> UserThreadEntry {
        UserThreadEntry {
            port: self.port,
            regs,
        }
    }
}

/// A host [`LiveSpace`](tairix_kernel_mem::LiveSpace) over simulated RAM — the
/// production space over the host page-table and physical-map doubles, so a
/// test exercises the real thing rather than a bespoke stub.
///
/// Shared by this module's own tests and [`crate::kthread`]'s, which needs a
/// process space to hang off a control block.
#[cfg(test)]
pub(crate) fn host_test_space() -> Box<dyn LiveUserSpace + Send> {
    use tairix_kernel_mem::{
        AddressSpace, BootMemoryMap, FrameAllocator, HostPageTable, LiveSpace, MemoryRegion,
        PhysAddr, RegionKind, SimPhysMap, VirtAddr, PAGE_SIZE,
    };

    let mut map = BootMemoryMap::new();
    map.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new((PAGE_SIZE * 16) as u64),
        length: (256 * PAGE_SIZE) as u64,
    });
    let frames: &'static FrameAllocator =
        Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
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

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::sync::Arc;

    #[test]
    fn with_hands_the_closure_one_shared_space() {
        let shared = ProcessSpace::for_test(host_test_space());
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
        let shared = ProcessSpace::for_test(host_test_space());
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

        let shared = Arc::new(ProcessSpace::for_test(host_test_space()));
        let _first = crate::kthread::publish_live_space_for_test(FIRST_CPU, Arc::clone(&shared));
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

    /// `thread_create`'s handle comes out of the publication's **own**
    /// refcount, and taking it leaves the publication intact.
    ///
    /// The per-CPU slot holds a bare pointer so the switch path pays no
    /// refcount traffic, which makes "the pointer always addresses the inside of
    /// a live `Arc`" the invariant the reconstruction rests on: published a
    /// leaked `Box` instead and the increment writes to the neighbouring heap
    /// block below the value (`plans/OPEN-DEFECTS.md` D45). Watching the count
    /// move on the handle that was published is what pins the increment to the
    /// right allocation.
    #[test]
    fn the_published_handle_is_what_a_reconstruction_shares() {
        // A CPU no other test in this crate publishes on, so the global
        // per-CPU slot is unshared under a parallel run.
        const CPU: u32 = 47;

        let shared = Arc::new(ProcessSpace::for_test(host_test_space()));
        let published = crate::kthread::publish_live_space_for_test(CPU, Arc::clone(&shared));
        assert_eq!(
            Arc::strong_count(&shared),
            2,
            "the publication owns one share; the slot only borrows"
        );

        let reconstructed =
            crate::kthread::current_process_space(CPU).expect("the slot is published");
        assert!(
            Arc::ptr_eq(&shared, &reconstructed),
            "the reconstruction names the published allocation"
        );
        assert_eq!(
            Arc::strong_count(&shared),
            3,
            "the reconstruction took a share of this allocation's own count"
        );

        drop(reconstructed);
        assert_eq!(
            Arc::strong_count(&shared),
            2,
            "dropping it releases that share and leaves the publication intact"
        );

        drop(published);
        assert!(
            crate::kthread::current_process_space(CPU).is_none(),
            "a cleared slot fails closed rather than handing out a stale space"
        );
        assert_eq!(Arc::strong_count(&shared), 1);
    }
}
