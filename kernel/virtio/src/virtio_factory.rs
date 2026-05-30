//! Kernel-binary [`VirtioHostFactory`] (Stage 4.D Item 2-tail.4).
//!
//! The userland driver host (`userland/system/drvhost`) calls
//! [`VirtioHostFactory::mint`] just before invoking a driver's
//! `register()` entry point. The production kernel binary plugs the
//! [`KernelVirtioFactory`] defined here into the host's
//! `virtio_host_factory` slot so that every loaded virtio-class driver
//! receives a fresh, capability-checked [`KernelVirtioHost`] backed by
//! its own per-process [`DmaPool`] (`AGENTS.md` §4 — per-process heaps,
//! never a shared global pool).
//!
//! # Why this lives in the kernel binary
//!
//! The factory trait [`VirtioHostFactory`] is owned by the bus-agnostic
//! `lib/virtio` host seam, but a concrete implementation has to mention
//! the kernel-side generics (`P: PageTableOps`, the audit [`Sink`], the
//! [`IrqWaiter`] seam) and depend on the `kernel-host` build of
//! `drivers/bus/virtio`. Because both `drvhost` and this crate depend
//! only on the `lib/virtio` seam — never on each other — the userland
//! host stays free of any `kernel/*` dependency and the kernel stays
//! free of any `userland/*` dependency (`AGENTS.md` §17.4). The factory
//! is exposed from the crate's library half so the production binary
//! and the QEMU integration tests share one implementation
//! (`AGENTS.md` §2.2 — no duplication).
//!
//! # Per-driver freshness
//!
//! [`KernelVirtioFactory::mint`] builds a brand-new [`AddressSpace`]
//! and [`DmaPool`] on every call. Because [`KernelVirtioHost`] owns its
//! pool, the host (and therefore the pool, with all of its mappings)
//! is reclaimed when `drvhost` drops the boxed host at the end of
//! `register()` — no driver can retain DMA-mapped memory past its own
//! load call.

use alloc::boxed::Box;

use crate::kernel_host::KernelVirtioHost;
use rustos_abi::driver::VirtioHost;
use rustos_abi::{CapabilityId, CapabilityQuery, IrqHandle};
use rustos_kernel_irq::{IrqTable, IrqWaiter};
use rustos_kernel_mem::{AddressSpace, DmaPool, FrameAllocator, PageTableOps, PhysMap, VirtAddr};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_log::Sink;
use rustos_virtio::{PoolId, VirtioHostFactory};

/// Borrowed kernel resources and per-device parameters a
/// [`KernelVirtioFactory`] needs to mint a [`KernelVirtioHost`].
///
/// Every field is borrowed for `'k`; the factory (and the hosts it
/// mints) outlive nothing beyond this borrow. The shape mirrors the
/// driver host's per-load config: one config value is built per loaded
/// driver and kept alive for the duration of its `register()` call.
pub struct KernelVirtioFactoryConfig<'k> {
    /// Physical frame allocator the per-driver [`DmaPool`] draws from.
    pub frames: &'k FrameAllocator,
    /// Kernel direct physical map the per-driver [`DmaPool`] reaches a
    /// buffer's frames through, so the CPU sees exactly the frames the
    /// device DMAs to (the boot identity map in production).
    pub phys: &'k dyn PhysMap,
    /// Capabilities of the task that owns the driver's per-process
    /// pool. Every allocation and free is audited against this set,
    /// and [`KernelVirtioHost::notify_wait`] waits on the line bound by
    /// `caller.task()` (`AGENTS.md` §5.4 — forgery defence).
    pub caller: &'k TaskCapabilities,
    /// Audit sink every DMA grant/denial and IRQ decision is logged to.
    pub audit: &'k dyn Sink,
    /// Kernel IRQ table the device's interrupt line is bound in.
    pub irq: &'k IrqTable,
    /// Handle the bus driver minted when it bound the device's line
    /// (Stage 4.D Item 3 supplies the GSI alongside the register
    /// window).
    pub irq_handle: IrqHandle,
    /// Clock + cooperative-yield seam the blocking wait loop drives
    /// (wraps the scheduler + architecture monotonic clock).
    pub waiter: &'k dyn IrqWaiter,
    /// Base virtual address of the per-driver DMA window inside the
    /// freshly-minted address space.
    pub pool_base: VirtAddr,
    /// Capacity, in pages, of the per-driver DMA window.
    pub pool_pages: usize,
}

/// Kernel-side [`VirtioHostFactory`] that mints one capability-checked
/// [`KernelVirtioHost`] per loaded driver.
///
/// Generic over the page-table backend `P` (the host test double in
/// unit tests, the architecture page table in production) and the
/// closure `F` that mints a fresh empty page table for each driver's
/// address space.
pub struct KernelVirtioFactory<'k, P, F>
where
    P: PageTableOps,
    F: Fn() -> P,
{
    config: KernelVirtioFactoryConfig<'k>,
    make_table: F,
}

impl<'k, P, F> KernelVirtioFactory<'k, P, F>
where
    P: PageTableOps,
    F: Fn() -> P,
{
    /// Build a factory from its borrowed [`KernelVirtioFactoryConfig`]
    /// and a `make_table` closure.
    ///
    /// `make_table` is invoked once per [`mint`](Self::mint) call to
    /// produce the empty page table backing that driver's private
    /// [`AddressSpace`]. It must return a fresh, empty table each time;
    /// reusing a table across drivers would breach the per-process
    /// isolation guarantee (`AGENTS.md` §4).
    #[must_use]
    pub fn new(config: KernelVirtioFactoryConfig<'k>, make_table: F) -> Self {
        Self { config, make_table }
    }
}

impl<P, F> VirtioHostFactory for KernelVirtioFactory<'_, P, F>
where
    P: PageTableOps,
    F: Fn() -> P,
{
    fn mint<'r>(&'r self, granted: &dyn CapabilityQuery) -> Option<Box<dyn VirtioHost + 'r>> {
        // Fail closed: a driver that was not granted `CAP_MEM_DMA`
        // gets no virtio host at all. The kernel DMA gate would refuse
        // every allocation anyway (`KernelVirtioHost::alloc_dma_zeroed`
        // is authoritative), but short-circuiting here avoids minting a
        // pool the driver can never use (`AGENTS.md` §5.4 —
        // capability check before touching state).
        if !granted.holds(CapabilityId::MEM_DMA) {
            return None;
        }

        let space = AddressSpace::new((self.make_table)());
        let pool = DmaPool::new(
            space,
            self.config.pool_base,
            self.config.pool_pages,
            self.config.frames,
            self.config.phys,
        )
        .ok()?;

        let host: KernelVirtioHost<'r, P, dyn Sink> = KernelVirtioHost::new(
            pool,
            self.config.caller,
            self.config.audit,
            PoolId::fresh(),
            self.config.irq,
            self.config.irq_handle,
            self.config.waiter,
        );
        Some(Box::new(host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell as StdRefCell;

    use alloc::vec::Vec;
    use rustos_caps::CapabilitySet;
    use rustos_kernel_irq::IrqWaitAbort;
    use rustos_kernel_mem::{
        bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
        HostPageTable, PhysAddr, SimPhysMap, PAGE_SIZE,
    };
    use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
    use rustos_kernel_sec::identity::UserId;
    use rustos_log::{Event, Sink};

    const OWNER: TaskId = TaskId(99);

    /// Idle waiter: the factory tests never park on `notify_wait`.
    struct IdleWaiter;
    impl IrqWaiter for IdleWaiter {
        fn now_ns(&self) -> u64 {
            0
        }
        fn yield_now(&self) -> Result<(), IrqWaitAbort> {
            Ok(())
        }
    }

    /// Minimal recording [`Sink`] capturing every event id.
    struct Recorder {
        ids: StdRefCell<Vec<u32>>,
    }
    impl Recorder {
        fn new() -> Self {
            Self {
                ids: StdRefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.ids.borrow().clone()
        }
    }
    impl Sink for Recorder {
        fn write_event(&self, event: &Event<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    /// Event id `kernel/sec::dma::alloc_dma` emits on a granted alloc.
    const DMA_ALLOCATED_ID: u32 = 1030;

    fn usable_map(pages: usize) -> BootMemoryMap {
        let mut m = BootMemoryMap::new();
        m.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 16),
            length: (PAGE_SIZE * pages) as u64,
        });
        m
    }

    /// Simulated physical RAM covering the usable region so a minted
    /// pool can reach its frames from the CPU.
    fn fresh_sim(pages: usize) -> SimPhysMap {
        SimPhysMap::new(PhysAddr::new(PAGE_SIZE as u64 * 16), pages * PAGE_SIZE)
    }

    fn task_with(caps: &[CapabilityId], sink: &Recorder) -> TaskCapabilities {
        let mut set = CapabilitySet::empty();
        for c in caps {
            set.insert(*c);
        }
        TaskCapabilities::derive(OWNER, UserId(1000), set, set, sink)
    }

    fn binding(line: u32) -> (IrqTable, IrqHandle) {
        let table = IrqTable::new(31);
        let out = table.bind(line, OWNER).expect("bind device line");
        (table, out.handle)
    }

    fn config<'k>(
        frames: &'k FrameAllocator,
        phys: &'k SimPhysMap,
        caller: &'k TaskCapabilities,
        audit: &'k Recorder,
        irq: &'k IrqTable,
        handle: IrqHandle,
        waiter: &'k IdleWaiter,
    ) -> KernelVirtioFactoryConfig<'k> {
        KernelVirtioFactoryConfig {
            frames,
            phys,
            caller,
            audit,
            irq,
            irq_handle: handle,
            waiter,
            pool_base: VirtAddr::new(0x2000_0000),
            pool_pages: 16,
        }
    }

    #[test]
    fn mint_yields_host_for_dma_capable_driver() {
        let frames = FrameAllocator::new(&usable_map(16)).unwrap();
        let sim = fresh_sim(16);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let (irq, handle) = binding(4);
        let waiter = IdleWaiter;
        let factory = KernelVirtioFactory::new(
            config(&frames, &sim, &caller, &sink, &irq, handle, &waiter),
            HostPageTable::new,
        );

        let mut granted = CapabilitySet::empty();
        granted.insert(CapabilityId::MEM_DMA);
        let host = factory.mint(&granted).expect("host minted");

        let slab = host.alloc_dma_zeroed(PAGE_SIZE).expect("granted");
        assert_eq!(slab.len(), PAGE_SIZE);
        assert!(slab.as_bytes().iter().all(|b| *b == 0));
        assert!(sink.ids().contains(&DMA_ALLOCATED_ID));
    }

    #[test]
    fn mint_refuses_driver_without_mem_dma() {
        let frames = FrameAllocator::new(&usable_map(16)).unwrap();
        let sim = fresh_sim(16);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let (irq, handle) = binding(4);
        let waiter = IdleWaiter;
        let factory = KernelVirtioFactory::new(
            config(&frames, &sim, &caller, &sink, &irq, handle, &waiter),
            HostPageTable::new,
        );

        // The driver's granted set lacks `CAP_MEM_DMA`.
        let granted = CapabilitySet::empty();
        assert!(factory.mint(&granted).is_none());
    }

    #[test]
    fn mint_builds_a_distinct_pool_each_call() {
        let frames = FrameAllocator::new(&usable_map(64)).unwrap();
        let sim = fresh_sim(64);
        let sink = Recorder::new();
        let caller = task_with(&[CapabilityId::MEM_DMA], &sink);
        let (irq, handle) = binding(4);
        let waiter = IdleWaiter;
        let factory = KernelVirtioFactory::new(
            config(&frames, &sim, &caller, &sink, &irq, handle, &waiter),
            HostPageTable::new,
        );

        let mut granted = CapabilitySet::empty();
        granted.insert(CapabilityId::MEM_DMA);

        let first = factory.mint(&granted).expect("first host");
        let a = first.alloc_dma_zeroed(PAGE_SIZE).expect("first alloc");
        let first_pool = a.pool_id();
        drop(a);
        drop(first);

        let second = factory.mint(&granted).expect("second host");
        let b = second.alloc_dma_zeroed(PAGE_SIZE).expect("second alloc");
        // A fresh pool per driver means a fresh, distinct `PoolId`.
        assert_ne!(first_pool, b.pool_id());
    }
}
