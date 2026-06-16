//! Production `mem_map` / `mmio_map` producers over the per-task retained
//! live address space (`plans/PI.md` P10 chunk 5d-0-ii (b′)).
//!
//! [`crate::memmap::MemMap`] and [`crate::devres::MmioMapFacility`] are the
//! object-safe seams the `mem_map` and `mmio_map` syscall handlers reach.
//! Before this module the only implementations were the fail-closed
//! `NULL_*` defaults, because no live, mutable address space was retained
//! per task (the spawn path froze the space into a read-only snapshot and
//! dropped the live one). With per-task retention now wired through the
//! kthread runtime ([`crate::kthread::with_current_live_space`]) these two
//! producers route each call to the **caller's own** live space:
//!
//! * a syscall handler runs on the CPU servicing the trap, on which the
//!   calling task is the one currently switched in, so its live space is
//!   exactly the per-CPU slot for [`SchedulerArch::current_cpu`]; and
//! * the access is exclusive — the task is suspended in its own syscall
//!   trap for the whole call (see [`crate::kthread::with_current_live_space`]).
//!
//! Both producers are generic over the arch (`A: SchedulerArch`) and hold a
//! `&'static A`, mirroring [`crate::procwait::KernelProcessWait`], so
//! `kernel/core` reads the current CPU without naming a concrete port
//! (`AGENTS.md` §17.4). A call on a CPU with no published live space (a
//! task spawned without a retained space) fails closed with
//! [`Errno::NotImplemented`] rather than touching another task's memory
//! (`AGENTS.md` §2.9 / §5.4).

use rustos_abi::{Errno, MapFlags};
use rustos_kernel_mem::{page_count_for, AnonError, DmaError, LiveSpaceError, MmioError};
use rustos_kernel_sched_api::SchedulerArch;

use crate::devres::{DmaAllocFacility, DmaCarve, MmioMapFacility};
use crate::kthread::with_current_live_space;
use crate::memmap::MemMap;

/// Fold an [`AnonError`] onto a stable [`Errno`] (`AGENTS.md` §2.9):
/// allocator exhaustion is [`Errno::OutOfMemory`] (§4), a not-mapped range
/// is [`Errno::NotFound`] (fail closed, §5.4), and a misalignment/overflow
/// is [`Errno::OutOfRange`].
fn anon_errno(err: AnonError) -> Errno {
    match err {
        AnonError::ZeroLength => Errno::LengthOutOfRange,
        AnonError::Unaligned | AnonError::Overflow => Errno::OutOfRange,
        AnonError::OutOfMemory => Errno::OutOfMemory,
        AnonError::NotMapped => Errno::NotFound,
        // `PhysUnmapped`, `Map(_)`, and any future (`#[non_exhaustive]`)
        // variant fold to the generic bad-address error, failing closed
        // rather than being silently dropped (`AGENTS.md` §2.9).
        _ => Errno::BadAddress,
    }
}

/// Fold an [`MmioError`] onto a stable [`Errno`]: no free virtual slot is
/// [`Errno::OutOfMemory`] (deterministic exhaustion, §4), a malformed
/// region or mapper config is [`Errno::OutOfRange`], and a page-table or
/// direct-map failure is [`Errno::BadAddress`].
fn mmio_errno(err: MmioError) -> Errno {
    match err {
        MmioError::NoVirtualSpace => Errno::OutOfMemory,
        MmioError::InvalidRegion | MmioError::InvalidMapConfig => Errno::OutOfRange,
        MmioError::UnknownRegion => Errno::NotFound,
        // `PageTable`, `DirectMap`, and any future (`#[non_exhaustive]`)
        // kind fail closed to a generic bad-address error (`AGENTS.md` §2.9).
        _ => Errno::BadAddress,
    }
}

/// Fold a [`DmaError`] onto a stable [`Errno`]: a contiguous-block or
/// page-table-frame exhaustion is [`Errno::OutOfMemory`] (deterministic OOM,
/// §4); a request beyond the max buddy order or the granted addressing limit
/// is [`Errno::OutOfRange`]; a zero-length request is
/// [`Errno::LengthOutOfRange`]; and a not-reachable frame or page-table
/// refusal is [`Errno::BadAddress`] (fail closed, `AGENTS.md` §2.9).
fn dma_errno(err: DmaError) -> Errno {
    match err {
        DmaError::Alloc(_) => Errno::OutOfMemory,
        DmaError::ZeroSize => Errno::LengthOutOfRange,
        DmaError::SizeUnsupported | DmaError::AddrLimitExceeded => Errno::OutOfRange,
        // `PageTable`, `DirectMap`, `UnknownBuffer`, `InvalidPoolConfig`, and
        // any future (`#[non_exhaustive]`) variant fail closed to a generic
        // bad-address error (`AGENTS.md` §2.9).
        _ => Errno::BadAddress,
    }
}

/// Fold a [`LiveSpaceError`] onto a stable [`Errno`].
fn live_errno(err: LiveSpaceError) -> Errno {
    match err {
        LiveSpaceError::Anon(anon) => anon_errno(anon),
        LiveSpaceError::Mmio(mmio) => mmio_errno(mmio),
        LiveSpaceError::Dma(dma) => dma_errno(dma),
        // `LiveSpaceError` is `#[non_exhaustive]`; fail closed.
        _ => Errno::BadAddress,
    }
}

/// The production anonymous-memory producer: maps/unmaps `RW` anonymous
/// pages in the **calling task's own** live address space (`plans/SPAWN.md`
/// `SP5b` production form).
pub struct LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle the CPU id is read
    /// from (the boot-leaked `KernelState` arch, exactly as
    /// [`crate::procwait::KernelProcessWait`]).
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> MemMap for LiveMemMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn map(&self, len: usize, flags: MapFlags, addr_hint: u64) -> Result<u64, Errno> {
        let page_count = page_count_for(len).map_err(anon_errno)?;
        let cpu = self.arch.current_cpu();
        // `FIXED` names its own base (`addr_hint`); a non-`FIXED` request asks
        // the live space's per-task heap-window allocator to choose one out of
        // this task's own free user-VA — never a base guessed here that might
        // collide with the image, stack, or a granted device window
        // (`AGENTS.md` §2.9 / `plans/PI.md` 5d-0-ii (c)).
        if flags.is_fixed() {
            with_current_live_space(cpu, |space| space.map_anonymous(addr_hint, page_count))
        } else {
            with_current_live_space(cpu, |space| space.map_anonymous_placed(page_count))
        }
        .ok_or(Errno::NotImplemented)?
        .map_err(live_errno)
    }

    fn unmap(&self, base: u64, len: usize) -> Result<(), Errno> {
        let page_count = page_count_for(len).map_err(anon_errno)?;
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.unmap_anonymous(base, page_count))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }
}

/// The production MMIO-map facility: maps a validated, **granted** device
/// window into the calling driver task's own live address space
/// (`plans/PI.md` P10 chunk 5d-0). The handler has already resolved and
/// owner-checked the grant (`AGENTS.md` §5.4 / §18.3); this performs only
/// the page-table mechanism, guard-bracketed and caching-disabled.
pub struct LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> MmioMapFacility for LiveMmioMap<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn map_window(&self, phys_base: u64, len: usize) -> Result<u64, Errno> {
        let cpu = self.arch.current_cpu();
        with_current_live_space(cpu, |space| space.map_device_window(phys_base, len))
            .ok_or(Errno::NotImplemented)?
            .map_err(live_errno)
    }
}

/// The production DMA-alloc facility: carves a coherent, guard-bracketed DMA
/// buffer into the calling driver task's own live address space
/// (`plans/PI.md` P10 chunk 5d-0). The handler has already resolved and
/// owner-checked the grant and validated its DMA constraint (`AGENTS.md`
/// §5.4 / §18.3); this performs only the carve mechanism, bounded by the
/// grant's `addr_limit`.
pub struct LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    arch: &'static A,
}

impl<A> LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    /// Build the producer over the `'static` arch handle.
    #[must_use]
    pub const fn new(arch: &'static A) -> Self {
        Self { arch }
    }
}

impl<A> DmaAllocFacility for LiveDmaAlloc<A>
where
    A: SchedulerArch + Send + Sync + 'static,
{
    fn alloc(&self, len: usize, addr_limit: u64) -> Result<DmaCarve, Errno> {
        let cpu = self.arch.current_cpu();
        // The coherent (and QEMU `virt`) device-visible address is the
        // CPU-physical base; a translating inbound viewport is refused
        // earlier in the handler (it rides the metal item), so here the
        // device address is exactly the carved physical base (§18.1).
        with_current_live_space(cpu, |space| space.alloc_dma(len, addr_limit))
            .ok_or(Errno::NotImplemented)?
            .map(|mapping| DmaCarve {
                cpu_va: mapping.cpu_va,
                device_addr: mapping.phys_base,
            })
            .map_err(live_errno)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::boxed::Box;
    use std::vec::Vec;

    use rustos_kernel_mem::{DmaMapping, LiveUserSpace};

    use crate::kthread::publish_live_space_for_test;
    use crate::test_arch::TestArch;

    /// A recording [`LiveUserSpace`] double: it logs each call and returns a
    /// configurable result, so the producer's routing + error fold are
    /// exercised without a real page table (the real [`LiveUserSpace`] is
    /// covered in `kernel/mem`). `&mut self` methods mean plain fields
    /// suffice — no interior mutability — so it stays `Send`.
    #[derive(Default)]
    struct FakeLive {
        anon_maps: Vec<(u64, u64)>,
        anon_placed: Vec<u64>,
        anon_unmaps: Vec<(u64, u64)>,
        device_maps: Vec<(u64, usize)>,
        dma_allocs: Vec<(usize, u64)>,
        next: Option<LiveSpaceError>,
    }

    /// The physical base a DMA carve reports back from the fake, so the
    /// producer test can assert the device address flows through unchanged.
    const DMA_PHYS: u64 = 0x4001_0000;

    /// The base a placed (non-`FIXED`) map reports back from the fake, so the
    /// producer test can assert the returned value flows through unchanged.
    const PLACED_BASE: u64 = 0xC000_0000;

    impl LiveUserSpace for FakeLive {
        fn map_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.anon_maps.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(base_va),
            }
        }

        fn map_anonymous_placed(&mut self, page_count: u64) -> Result<u64, LiveSpaceError> {
            self.anon_placed.push(page_count);
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(PLACED_BASE),
            }
        }

        fn unmap_anonymous(&mut self, base_va: u64, page_count: u64) -> Result<(), LiveSpaceError> {
            self.anon_unmaps.push((base_va, page_count));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn map_device_window(&mut self, phys_base: u64, len: usize) -> Result<u64, LiveSpaceError> {
            self.device_maps.push((phys_base, len));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(0x9000_1000),
            }
        }

        fn alloc_dma(&mut self, len: usize, addr_limit: u64) -> Result<DmaMapping, LiveSpaceError> {
            self.dma_allocs.push((len, addr_limit));
            match self.next.take() {
                Some(err) => Err(err),
                None => Ok(DmaMapping {
                    cpu_va: 0xD000_2000,
                    phys_base: DMA_PHYS,
                }),
            }
        }
    }

    /// A `TestArch` reporting `cpu`, leaked to the `'static` shape the
    /// producers hold (mirroring the boot-global arch handle).
    ///
    /// Each test uses a **distinct** `cpu` so the global per-CPU
    /// [`with_current_live_space`] slot is never shared between tests running
    /// in parallel (`AGENTS.md` §7 — no flaky tests).
    fn arch_at(cpu: u32) -> &'static TestArch {
        let arch = Box::leak(Box::new(TestArch::with_cpus(cpu + 1)));
        arch.set_current_cpu(cpu);
        arch
    }

    /// Leak `fake` to the `'static` lifetime [`publish_live_space_for_test`]
    /// requires (the production live space is owned for the task's life),
    /// returning the `&'static mut` to publish and a raw pointer to inspect
    /// the recording after the producer call (the producer's `&mut` has ended
    /// by then; single-threaded). A test leak is bounded by the process.
    fn leak_fake() -> (&'static mut FakeLive, *const FakeLive) {
        leak_fake_with(FakeLive::default())
    }

    fn leak_fake_with(fake: FakeLive) -> (&'static mut FakeLive, *const FakeLive) {
        let leaked: &'static mut FakeLive = Box::leak(Box::new(fake));
        let ptr: *const FakeLive = leaked;
        (leaked, ptr)
    }

    const PAGE: usize = 4096;

    #[test]
    fn mem_map_routes_a_fixed_request_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(1, fake);

        let producer = LiveMemMap::new(arch_at(1));
        let base = 0x4000;
        let got = producer.map(2 * PAGE, MapFlags::FIXED, base);
        assert_eq!(got, Ok(base));
        // The producer rounded the byte length to a page count and forwarded
        // the FIXED base unchanged.
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_maps, std::vec![(base, 2)]);
    }

    #[test]
    fn mem_map_unmap_routes_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(2, fake);

        let producer = LiveMemMap::new(arch_at(2));
        assert_eq!(producer.unmap(0x4000, PAGE), Ok(()));
        // SAFETY: see above.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_unmaps, std::vec![(0x4000, 1)]);
    }

    #[test]
    fn mem_map_non_fixed_routes_to_the_placement_allocator() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(3, fake);

        let producer = LiveMemMap::new(arch_at(3));
        // A non-`FIXED` request asks the live space to choose the base; the
        // `addr_hint` is ignored, and the placed base flows back unchanged.
        let got = producer.map(2 * PAGE, MapFlags::empty(), 0xDEAD_0000);
        assert_eq!(got, Ok(PLACED_BASE));
        // The producer routed to `map_anonymous_placed` (page count only),
        // never the `FIXED` `map_anonymous`.
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.anon_placed, std::vec![2]);
        assert!(recorded.anon_maps.is_empty());
    }

    #[test]
    fn mem_map_with_no_published_space_fails_closed_for_a_non_fixed_request() {
        // No live space published on this CPU: a non-`FIXED` placement must
        // also fail closed rather than fabricating a base.
        let producer = LiveMemMap::new(arch_at(9));
        assert_eq!(
            producer.map(PAGE, MapFlags::empty(), 0),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mem_map_with_no_published_space_fails_closed() {
        // No live space published on this CPU: the producer must not map
        // anything (a task spawned without a retained space).
        let producer = LiveMemMap::new(arch_at(4));
        assert_eq!(
            producer.map(PAGE, MapFlags::FIXED, 0x4000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mem_map_folds_an_out_of_memory_error() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Anon(AnonError::OutOfMemory)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(5, fake);

        let producer = LiveMemMap::new(arch_at(5));
        assert_eq!(
            producer.map(PAGE, MapFlags::FIXED, 0x4000),
            Err(Errno::OutOfMemory)
        );
    }

    #[test]
    fn mmio_map_routes_a_granted_window_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(6, fake);

        let producer = LiveMmioMap::new(arch_at(6));
        let va = producer.map_window(0xFE98_0000, 0x4000);
        assert_eq!(va, Ok(0x9000_1000));
        // SAFETY: see above.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.device_maps, std::vec![(0xFE98_0000, 0x4000)]);
    }

    #[test]
    fn mmio_map_with_no_published_space_fails_closed() {
        let producer = LiveMmioMap::new(arch_at(7));
        assert_eq!(
            producer.map_window(0xFE98_0000, 0x4000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mmio_map_folds_a_no_virtual_space_error() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Mmio(MmioError::NoVirtualSpace)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(8, fake);

        let producer = LiveMmioMap::new(arch_at(8));
        assert_eq!(
            producer.map_window(0xFE98_0000, 0x4000),
            Err(Errno::OutOfMemory)
        );
    }

    #[test]
    fn dma_alloc_routes_a_carve_to_the_current_live_space() {
        let (fake, ptr) = leak_fake();
        let _guard = publish_live_space_for_test(10, fake);

        let producer = LiveDmaAlloc::new(arch_at(10));
        let carve = producer.alloc(2 * PAGE, 0x4000_0000);
        // The CPU VA and the physical-base-as-device-address flow back from
        // the live space unchanged.
        assert_eq!(
            carve,
            Ok(DmaCarve {
                cpu_va: 0xD000_2000,
                device_addr: DMA_PHYS,
            })
        );
        // SAFETY: the producer's `&mut` has ended; single-threaded read.
        let recorded = unsafe { &*ptr };
        assert_eq!(recorded.dma_allocs, std::vec![(2 * PAGE, 0x4000_0000)]);
    }

    #[test]
    fn dma_alloc_with_no_published_space_fails_closed() {
        let producer = LiveDmaAlloc::new(arch_at(11));
        assert_eq!(producer.alloc(PAGE, 0), Err(Errno::NotImplemented));
    }

    #[test]
    fn dma_alloc_folds_an_addressing_limit_error_to_out_of_range() {
        let (fake, _ptr) = leak_fake_with(FakeLive {
            next: Some(LiveSpaceError::Dma(DmaError::AddrLimitExceeded)),
            ..FakeLive::default()
        });
        let _guard = publish_live_space_for_test(12, fake);

        let producer = LiveDmaAlloc::new(arch_at(12));
        assert_eq!(producer.alloc(PAGE, 0x1000), Err(Errno::OutOfRange));
    }
}
