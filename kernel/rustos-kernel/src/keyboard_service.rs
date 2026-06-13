//! aarch64 in-kernel USB-keyboard service (`plans/PI.md` P10/P11).
//!
//! The boot-path counterpart of the architecture-neutral
//! [`crate::usb_keyboard`] composition engine: it supplies the concrete
//! in-kernel [`DriverHost`](rustos_abi::DriverHost) halves the chain needs
//! on the Raspberry Pi 4 — a capability-gated [`MmioMapper`] over the
//! identity map and a per-driver DMA host over the kernel frame allocator
//! — plus a generic-timer-backed [`Delay`](rustos_drv_bus_pcie_brcm::Delay),
//! and runs the chain as a kernel-only service kthread that brings the
//! VL805 up once and then polls it forever, feeding decoded key presses to
//! the kernel input-focus arbiter.
//!
//! # Why the identity map
//!
//! The service kthread runs under PID 1's address-space root, whose
//! identity map covers every gigapage the boot path's Device/RAM masks
//! name (`boot_aarch64`). The boot path folds the discovered PCIe
//! controller-register and outbound-MMIO-window gigapages into that Device
//! mask **before** enabling the MMU, so the controller block and the
//! enumerated VL805 BAR are already identity-mapped Device memory. The
//! [`IdentityMmioMapper`] therefore mints a [`RegisterWindow`] at the
//! window's *own* CPU-physical address (`phys == virt`) after checking the
//! capability and that the window lies within one of the two
//! discovered-and-mapped regions — it never edits a live page table at
//! driver time (`AGENTS.md` §5.4 — the check is mandatory and kernel-side;
//! §2.16 — no per-call mapping work on a path the identity map already
//! satisfies).
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe/USB (`AGENTS.md` §0.4), so the bring-up itself is
//! a metal-acceptance item; the host tests cover the capability/bounds
//! decisions of the two `DriverHost` halves (the security-relevant logic,
//! §5.4) without touching real device memory.

use core::ptr::NonNull;

use rustos_abi::driver::dma::{DmaSlab, PoolId};
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::{CapabilityId, DriverError, MmioMapError, MmioMapper, RegisterWindow};
use rustos_caps::CapabilitySet;
use rustos_kernel_mem::{FrameAllocator, PAGE_SIZE};

/// The capabilities the in-kernel keyboard bus-driver task holds: map the
/// PCIe controller register block and the VL805 BAR
/// ([`CapabilityId::MMIO_MAP`]) and carve the xHCI device-shared DMA region
/// ([`CapabilityId::MEM_DMA`]). No more — every map/alloc is re-checked
/// against this set (`AGENTS.md` §5.4); the service has no ambient
/// authority (`AGENTS.md` §4).
#[must_use]
pub fn service_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::MMIO_MAP);
    caps.insert(CapabilityId::MEM_DMA);
    caps
}

/// Non-reserved [`PoolId`] tagging the keyboard service's DMA region
/// ([`PoolId::MOCK`] = 0 is reserved for the in-process mock host).
const KEYBOARD_DMA_POOL: PoolId = PoolId::from_raw(0x5742); // "WB"

/// A capability-gated [`MmioMapper`] that maps a register window at its own
/// CPU-physical address, valid because the boot path identity-maps the PCIe
/// controller and outbound-MMIO gigapages as Device memory (see the module
/// docs).
///
/// It admits a `map_window` only when the caller holds
/// [`CapabilityId::MMIO_MAP`] **and** the requested `[base, base+len)` lies
/// wholly within one of the two discovered, identity-mapped regions — the
/// controller register block or the outbound MMIO window. Any other request
/// fails closed (`AGENTS.md` §5.4 / §2.9), so the mapper can never hand out
/// a window the identity map does not back.
pub struct IdentityMmioMapper {
    caps: CapabilitySet,
    regs_base: u64,
    regs_len: u64,
    outbound_base: u64,
    outbound_len: u64,
}

impl IdentityMmioMapper {
    /// Build a mapper permitting the controller register block
    /// `[regs_base, regs_base+regs_len)` and the outbound MMIO window
    /// `[outbound_base, outbound_base+outbound_len)`, under `caps`.
    #[must_use]
    pub fn new(
        caps: CapabilitySet,
        regs_base: u64,
        regs_len: u64,
        outbound_base: u64,
        outbound_len: u64,
    ) -> Self {
        Self {
            caps,
            regs_base,
            regs_len,
            outbound_base,
            outbound_len,
        }
    }

    /// Whether `[phys, phys+len)` lies wholly within one of the two
    /// permitted regions. Overflow in either bound fails the check
    /// (`AGENTS.md` §2.9), never wraps.
    #[must_use]
    fn permits(&self, phys: u64, len: usize) -> bool {
        let within = |base: u64, span: u64| -> bool {
            let Some(region_end) = base.checked_add(span) else {
                return false;
            };
            let Some(req_end) = phys.checked_add(len as u64) else {
                return false;
            };
            phys >= base && req_end <= region_end
        };
        within(self.regs_base, self.regs_len) || within(self.outbound_base, self.outbound_len)
    }
}

impl MmioMapper for IdentityMmioMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        // Capability before state (`AGENTS.md` §5.4).
        if !self.caps.contains(CapabilityId::MMIO_MAP) {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 || !self.permits(phys_base, len) {
            return Err(MmioMapError::InvalidRegion);
        }
        let addr = usize::try_from(phys_base).map_err(|_| MmioMapError::InvalidRegion)?;
        let base = NonNull::new(addr as *mut u8).ok_or(MmioMapError::InvalidRegion)?;
        // SAFETY: `permits` confirmed `[phys_base, phys_base+len)` lies
        // within a region the boot path identity-mapped as Device memory
        // (the PCIe controller block or the outbound MMIO window), so the
        // CPU-virtual address equals `phys_base` and the whole window is a
        // valid, exclusively-owned MMIO mapping for the driver's lifetime
        // (`AGENTS.md` §4 — reclaimed when the kernel tears the service
        // down). No other live reference aliases device MMIO.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

/// A per-driver DMA host that carves the xHCI device-shared region from the
/// kernel frame allocator, within the bridge's inbound aperture.
///
/// The allocated block is identity-mapped RAM, so the CPU pointer is the
/// frame's own physical address. The device-visible (PCIe-space) address is
/// the frame's physical address translated through the bridge's inbound
/// viewport (`inbound_pcie_base + (phys - inbound_cpu_base)`), and it is
/// rejected unless it lies wholly below the inbound aperture top
/// (`AGENTS.md` §5.4 — a device must never be handed an address outside the
/// window the bridge grants it). The region is held for the controller's
/// lifetime (a permanent device DMA mapping, `AGENTS.md` §4), so it is
/// allocated once and never freed.
pub struct FrameDmaHost {
    caps: CapabilitySet,
    frames: &'static FrameAllocator,
    inbound_cpu_base: u64,
    inbound_pcie_base: u64,
    aperture_top: u64,
}

impl FrameDmaHost {
    /// Build a DMA host over the kernel frame allocator `frames`, with the
    /// bridge's inbound viewport (`inbound_cpu_base` → `inbound_pcie_base`)
    /// and the exclusive aperture top `aperture_top`, under `caps`.
    #[must_use]
    pub fn new(
        caps: CapabilitySet,
        frames: &'static FrameAllocator,
        inbound_cpu_base: u64,
        inbound_pcie_base: u64,
        aperture_top: u64,
    ) -> Self {
        Self {
            caps,
            frames,
            inbound_cpu_base,
            inbound_pcie_base,
            aperture_top,
        }
    }

    /// Translate a CPU-physical frame base into the device-visible
    /// (PCIe-space) address a device behind the bridge uses, rejecting any
    /// address (or `size` span) that falls outside the inbound aperture
    /// (`AGENTS.md` §5.4 — fail closed, never wrap).
    fn device_addr(&self, phys: u64, size: usize) -> Result<u64, DriverError> {
        let offset = phys
            .checked_sub(self.inbound_cpu_base)
            .ok_or(DriverError::OutOfRange)?;
        let device = self
            .inbound_pcie_base
            .checked_add(offset)
            .ok_or(DriverError::OutOfRange)?;
        let end = device
            .checked_add(size as u64)
            .ok_or(DriverError::OutOfRange)?;
        if end > self.aperture_top {
            return Err(DriverError::OutOfRange);
        }
        Ok(device)
    }
}

impl VirtioHost for FrameDmaHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        // Capability before state (`AGENTS.md` §5.4).
        if !self.caps.contains(CapabilityId::MEM_DMA) {
            return Err(DriverError::PermissionDenied);
        }
        if size == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        // A power-of-two contiguous block covering `size` (the buddy
        // allocator allocates `2^order` frames, aligned).
        let pages = size.div_ceil(PAGE_SIZE);
        let order = pages.next_power_of_two().trailing_zeros();
        let frame = self
            .frames
            .alloc_order(order)
            .map_err(|_| DriverError::DeviceFault)?;
        let phys = frame.start().as_u64();
        let device = self.device_addr(phys, size)?;
        let addr = usize::try_from(phys).map_err(|_| DriverError::OutOfRange)?;
        let ptr = NonNull::new(addr as *mut u8).ok_or(DriverError::OutOfRange)?;
        // SAFETY: the frame allocator just handed us `2^order` contiguous
        // frames (≥ `size` bytes) that are identity-mapped RAM, so `ptr`
        // (= `phys`) is a valid, exclusively-owned, writable region for
        // `size` bytes; zeroing it satisfies the `alloc_dma_zeroed`
        // contract. No other reference aliases the freshly allocated block.
        unsafe {
            core::ptr::write_bytes(ptr.as_ptr(), 0, size);
        }
        // SAFETY: `ptr` covers `size` zeroed bytes valid for the whole
        // kernel lifetime (the block is never freed — a permanent device
        // DMA mapping, `AGENTS.md` §4), unaliased, and `device` is its
        // verified-in-aperture device-visible base. The leaked drop is a
        // no-op, matching the permanent ownership.
        Ok(unsafe { DmaSlab::from_leaked(device, ptr, size, KEYBOARD_DMA_POOL, 0) })
    }

    fn notify_wait(&self, _queue_index: u16) {}
}

// The metal half: the generic-timer delay, the boot→seam discovery hand-off,
// and the service-kthread spawn. Tied to the aarch64 freestanding build
// because it names the arch port (`busy_delay_us`, the discovered
// `PcieDiscovery`) and the aarch64 input-focus arbiter; the host build
// compiles and tests the two `DriverHost` halves above instead
// (`AGENTS.md` §17.4 — the arch dependency is target-only).
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
mod metal {
    use super::{service_caps, FrameDmaHost, IdentityMmioMapper};

    use alloc::boxed::Box;

    use rustos_arch_aarch64::kernel_arch::busy_delay_us;
    use rustos_arch_aarch64::platform::PcieDiscovery;
    use rustos_drv_bus_pcie_brcm::{Delay, PcieWindows};
    use rustos_drv_input_usb_hid::{pump_once, KeyboardConsole};
    use rustos_kernel_core::{InitSpawnCtx, YieldHandle};
    use rustos_sync::SpinLock;

    use crate::arch_wrapper_aarch64::INPUT_FOCUS;
    use crate::usb_keyboard::{bring_up_keyboard, ArbiterConsoleSink, ChainHost, PcieBringup};

    /// A [`Delay`] backed by the architectural physical counter
    /// (`CNTPCT_EL0`), for the BCM2711 PCIe link-training settle waits.
    struct GenericTimerDelay;

    impl Delay for GenericTimerDelay {
        fn delay_us(&self, us: u32) {
            busy_delay_us(us);
        }
    }

    /// The PCIe windows the boot path discovered (pre-MMU), handed to the
    /// init seam after the scheduler is up.
    ///
    /// Set once on the boot CPU before user mode and read once when PID 1
    /// is spawned, so the `SpinLock` never contends; `None` on a board with
    /// no `brcm,bcm2711-pcie` node (the QEMU `virt` shape), where the
    /// keyboard service is simply not started (`AGENTS.md` §18.4).
    static DISCOVERED: SpinLock<Option<PcieDiscovery>> = SpinLock::new(None);

    /// Record the PCIe windows the boot path discovered, for the init seam
    /// to consume. Called once on the boot CPU (`boot_aarch64`).
    pub fn record_discovery(discovery: PcieDiscovery) {
        *DISCOVERED.lock() = Some(discovery);
    }

    /// Spawn the USB-keyboard service kthread if the boot path discovered a
    /// PCIe bridge, returning whether it was started.
    ///
    /// Called by the PID 1 spawn seam **before** it drives the dispatch
    /// loop, so the service is admitted onto the boot CPU's run queue and
    /// runs alongside PID 1. With no discovered bridge (the `virt` shape)
    /// or no `'static` frame allocator it starts nothing and returns
    /// `false` — fail closed (`AGENTS.md` §2.9 / §18.4), the video login
    /// simply parks with no keyboard.
    ///
    /// The service owns its driver resources for the kernel's lifetime: the
    /// capability-gated MMIO mapper and DMA host are leaked to `'static`
    /// (kernel state is never freed, `AGENTS.md` §4), and the body runs the
    /// full bring-up chain once, then polls the keyboard forever, yielding
    /// between polls so PID 1 also runs. Every map/alloc is re-checked
    /// kernel-side (`AGENTS.md` §5.4); this adds no ambient authority
    /// (`AGENTS.md` §4).
    #[must_use]
    pub fn spawn_if_present(ctx: &dyn InitSpawnCtx) -> bool {
        let Some(discovery) = *DISCOVERED.lock() else {
            return false;
        };
        let Some(frames) = ctx.static_frames() else {
            return false;
        };

        let caps = service_caps();
        let mapper: &'static IdentityMmioMapper = Box::leak(Box::new(IdentityMmioMapper::new(
            caps,
            discovery.regs_phys,
            discovery.regs_len,
            discovery.outbound_cpu_base,
            discovery.outbound_size,
        )));
        let inbound_cpu_base = discovery
            .dma_aperture_top
            .saturating_sub(discovery.inbound_size);
        let dma: &'static FrameDmaHost = Box::leak(Box::new(FrameDmaHost::new(
            caps,
            frames,
            inbound_cpu_base,
            discovery.inbound_pcie_base,
            discovery.dma_aperture_top,
        )));
        let bringup = PcieBringup {
            regs_phys: discovery.regs_phys,
            windows: PcieWindows {
                inbound_pcie_base: discovery.inbound_pcie_base,
                inbound_size: discovery.inbound_size,
                outbound_cpu_base: discovery.outbound_cpu_base,
                outbound_pcie_base: discovery.outbound_pcie_base,
                outbound_size: discovery.outbound_size,
            },
            dma_aperture_top: discovery.dma_aperture_top,
        };

        let body = move |yielder: &mut dyn YieldHandle| {
            let host = ChainHost::new(caps, mapper, dma);
            let delay = GenericTimerDelay;
            // Bring the VL805 up once. A failure (no link, no device, a
            // refused map) ends the service fail-closed: the video login
            // parks with no keyboard rather than the kernel hanging
            // (`AGENTS.md` §2.9). The chain logs its own progress.
            let mut keyboard = match bring_up_keyboard(&host, &bringup, &delay) {
                Ok(keyboard) => keyboard,
                Err(_) => return,
            };
            let mut console = KeyboardConsole::new();
            let mut sink = ArbiterConsoleSink::new(&INPUT_FOCUS);
            // Poll the keyboard forever, yielding between polls so PID 1
            // and its sessions keep running (`AGENTS.md` §2.1 — never spin
            // a CPU). A `pump_once` error is non-fatal: drop the poll and
            // retry on the next dispatch.
            loop {
                let _ = pump_once(&mut keyboard, &mut console, &mut sink);
                yielder.yield_now();
            }
        };

        ctx.spawn_kernel_service(Box::new(body))
    }
}

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub use metal::{record_discovery, spawn_if_present};

#[cfg(test)]
mod tests {
    use super::*;

    use rustos_kernel_mem::{
        bootinfo::{BootMemoryMap, MemoryRegion, RegionKind},
        PhysAddr,
    };

    /// The Pi 4 discovered windows (mirroring the `usb_keyboard` and
    /// `platform::pcie_bringup` fixtures): controller regs at
    /// `0xfd50_0000` (len `0x9310`), inbound aperture top `0xc000_0000`,
    /// outbound MMIO window CPU `0x6_0000_0000` size `1 GiB`.
    const REGS_BASE: u64 = 0xfd50_0000;
    const REGS_LEN: u64 = 0x9310;
    const OUTBOUND_BASE: u64 = 0x6_0000_0000;
    const OUTBOUND_LEN: u64 = 0x4000_0000;
    const APERTURE_TOP: u64 = 0xc000_0000;

    fn mapper(caps: CapabilitySet) -> IdentityMmioMapper {
        IdentityMmioMapper::new(caps, REGS_BASE, REGS_LEN, OUTBOUND_BASE, OUTBOUND_LEN)
    }

    fn with_caps(ids: &[CapabilityId]) -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        for id in ids {
            caps.insert(*id);
        }
        caps
    }

    #[test]
    fn mapper_refuses_without_the_mmio_capability() {
        let m = mapper(with_caps(&[CapabilityId::MEM_DMA]));
        assert_eq!(
            m.map_window(REGS_BASE, 0x1000).err(),
            Some(MmioMapError::CapabilityMissing)
        );
    }

    #[test]
    fn mapper_admits_only_the_two_discovered_windows() {
        let m = mapper(with_caps(&[CapabilityId::MMIO_MAP]));
        // Inside the controller register block and the outbound window.
        assert!(m.map_window(REGS_BASE, 0x1000).is_ok());
        assert!(m.map_window(OUTBOUND_BASE + 0x1_0000, 0x1000).is_ok());
        // Zero length, a window straddling the end of a region, an
        // out-of-range base, and an overflowing span all fail closed.
        assert_eq!(
            m.map_window(REGS_BASE, 0).err(),
            Some(MmioMapError::InvalidRegion)
        );
        assert_eq!(
            m.map_window(REGS_BASE, usize::try_from(REGS_LEN).expect("fits") + 1)
                .err(),
            Some(MmioMapError::InvalidRegion)
        );
        assert_eq!(
            m.map_window(0x1000_0000, 0x1000).err(),
            Some(MmioMapError::InvalidRegion)
        );
        assert_eq!(
            m.map_window(OUTBOUND_BASE, usize::MAX).err(),
            Some(MmioMapError::InvalidRegion)
        );
    }

    fn frame_allocator() -> &'static FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new((PAGE_SIZE as u64) * 16),
            length: (PAGE_SIZE * 64) as u64,
        });
        // Leaked so the host returns a `&'static FrameAllocator`, matching
        // the production leaked kernel allocator.
        alloc::boxed::Box::leak(alloc::boxed::Box::new(
            FrameAllocator::new(&map).expect("frames"),
        ))
    }

    #[test]
    fn dma_host_refuses_without_the_dma_capability() {
        let host = FrameDmaHost::new(with_caps(&[]), frame_allocator(), 0, 0, APERTURE_TOP);
        assert_eq!(
            host.alloc_dma_zeroed(0x1000).err(),
            Some(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn dma_host_translates_through_the_inbound_viewport() {
        // Pi 4 identity inbound (cpu base 0 → pcie base 0): the device sees
        // its own physical address.
        let host = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0,
            0,
            APERTURE_TOP,
        );
        assert_eq!(host.device_addr(0x10_0000, 0x4000), Ok(0x10_0000));
        // A nonzero inbound viewport offsets the device address.
        let shifted = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0x8000_0000,
            0x4000_0000,
            APERTURE_TOP,
        );
        assert_eq!(shifted.device_addr(0x8000_1000, 0x1000), Ok(0x4000_1000));
    }

    #[test]
    fn dma_host_rejects_a_region_outside_the_aperture() {
        let host = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0,
            0,
            APERTURE_TOP,
        );
        // A device address whose span reaches past the aperture top.
        assert_eq!(
            host.device_addr(APERTURE_TOP - 0x800, 0x1000),
            Err(DriverError::OutOfRange)
        );
        // A physical address below the inbound viewport base underflows
        // fail-closed.
        let shifted = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0x8000_0000,
            0,
            APERTURE_TOP,
        );
        assert_eq!(
            shifted.device_addr(0x1000, 0x1000),
            Err(DriverError::OutOfRange)
        );
    }
}
