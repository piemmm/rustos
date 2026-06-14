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
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

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

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
const MAILBOX_PROPERTY_BYTES: usize = 32 * core::mem::size_of::<u32>();

#[repr(align(16))]
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
struct MailboxPropertyBuffer(core::cell::UnsafeCell<[u8; MAILBOX_PROPERTY_BYTES]>);

// SAFETY: the keyboard service owns this static property buffer for its
// one-shot firmware fallback. The service reads the discovered doorbell once
// before spawning and only the spawned kthread touches the buffer.
#[cfg(all(freestanding, kernel_isa = "aarch64"))]
unsafe impl Sync for MailboxPropertyBuffer {}

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
static MAILBOX_PROPERTY_BUFFER: MailboxPropertyBuffer =
    MailboxPropertyBuffer(core::cell::UnsafeCell::new([0; MAILBOX_PROPERTY_BYTES]));

/// Audit event: an [`IdentityMmioMapper`] map-window decision (the
/// security-relevant accept/deny of `AGENTS.md` §5.4.4). Logged once per
/// bring-up map — the controller register block and the VL805 BAR, never
/// on the poll path (`AGENTS.md` §2.16) — so a metal capture shows the
/// exact `[base, len)` the PCI driver asked the bridge to map and whether
/// it resolved to a backed CPU address. A BAR base outside the discovered
/// outbound window resolves to the `ffff_ffff_ffff_ffff` sentinel and is
/// the missing register fact behind a `length_out_of_range` xHCI bring-up.
/// Bin-crate id alongside the boot pipeline's `4100`/`4101`/`4104`; part
/// of the audit contract (`AGENTS.md` §5.4.4).
const MMIO_MAP_DECISION: EventId = EventId(4105);

/// The `resolved_cpu_hex` value logged for a refused map: no CPU address
/// resolved, so the window lies outside every region the bridge maps.
const MMIO_MAP_REJECTED: u64 = u64::MAX;

/// A capability-gated, bridge-aware [`MmioMapper`] for a device behind the
/// Pi 4's PCIe root complex.
///
/// The boot path identity-maps the PCIe controller block and the outbound
/// MMIO gigapages as Device memory (see the module docs), so a CPU-physical
/// address inside either is reachable directly. It admits a `map_window`
/// only when the caller holds [`CapabilityId::MMIO_MAP`] **and** the
/// requested `[base, base+len)` resolves to one of two regions, failing
/// closed otherwise (`AGENTS.md` §5.4 / §2.9):
///
/// * the controller register block `[regs_base, regs_base+regs_len)` — a
///   CPU-physical address, mapped identity; or
/// * a BAR inside the bridge's **outbound PCIe-bus** window
///   `[outbound_pcie_base, outbound_pcie_base+outbound_size)` — the address
///   a device's BAR decodes (and the value PCI configuration space holds).
///   The mapper applies the bridge's outbound `ranges` translation
///   (`outbound_cpu_base + (bus - outbound_pcie_base)`) to reach the
///   identity-mapped CPU address. Without this translation the VL805's
///   firmware-assigned BAR (a PCIe-bus address ≈ `0xc000_0000`) is rejected
///   even though the window the bridge maps it onto is backed.
///
/// The generic PCI driver knows only bus addresses (what the BAR register
/// holds); resolving them to a CPU mapping is the host bridge's job, so it
/// lives here in the platform mapper (mirroring how Linux's host bridge
/// applies `ranges`), never in the architecture-neutral PCI walk.
pub struct IdentityMmioMapper {
    caps: CapabilitySet,
    regs_base: u64,
    regs_len: u64,
    outbound_cpu_base: u64,
    outbound_pcie_base: u64,
    outbound_size: u64,
    /// Optional one-shot diagnostic sink for the map decision
    /// ([`MMIO_MAP_DECISION`]). `None` on the host (the security-logic
    /// tests assert the resolution directly); the metal service attaches
    /// the serial sink via [`Self::with_diag`] so a Pi capture shows the
    /// BAR base the bridge was asked to map.
    diag: Option<&'static (dyn Sink + Sync)>,
}

impl IdentityMmioMapper {
    /// Build a mapper permitting the controller register block
    /// `[regs_base, regs_base+regs_len)` (CPU-physical, identity) and the
    /// bridge's outbound window — CPU base `outbound_cpu_base`, PCIe-bus
    /// base `outbound_pcie_base`, size `outbound_size` — under `caps`.
    #[must_use]
    pub fn new(
        caps: CapabilitySet,
        regs_base: u64,
        regs_len: u64,
        outbound_cpu_base: u64,
        outbound_pcie_base: u64,
        outbound_size: u64,
    ) -> Self {
        Self {
            caps,
            regs_base,
            regs_len,
            outbound_cpu_base,
            outbound_pcie_base,
            outbound_size,
            diag: None,
        }
    }

    /// Attach a `'static` diagnostic sink so every [`Self::map_window`]
    /// decision is logged once (`MMIO_MAP_DECISION`). The metal service
    /// passes the serial sink; the resolution itself is unchanged, so this
    /// adds observability without widening authority (`AGENTS.md` §5.4.4).
    #[must_use]
    pub fn with_diag(mut self, sink: &'static (dyn Sink + Sync)) -> Self {
        self.diag = Some(sink);
        self
    }

    /// Log one map-window decision through the attached diagnostic sink, if
    /// any. A no-op when no sink is attached (the host build) — the
    /// resolution is identical either way (`AGENTS.md` §2.16 — one-shot,
    /// off the poll path).
    fn log_decision(&self, phys_base: u64, len: usize, resolved: Option<u64>) {
        let Some(sink) = self.diag else {
            return;
        };
        let regs_end = self.regs_base.saturating_add(self.regs_len);
        let outbound_end = self.outbound_pcie_base.saturating_add(self.outbound_size);
        let mut phys_buf = [0u8; 16];
        let mut len_buf = [0u8; 16];
        let mut resolved_buf = [0u8; 16];
        let mut regs_base_buf = [0u8; 16];
        let mut regs_end_buf = [0u8; 16];
        let mut outbound_base_buf = [0u8; 16];
        let mut outbound_end_buf = [0u8; 16];
        log(
            sink,
            &Event {
                level: if resolved.is_some() {
                    Level::Info
                } else {
                    Level::Error
                },
                id: MMIO_MAP_DECISION,
                message: "usb-keyboard: mmio map decision",
                fields: &[
                    Field {
                        key: "phys_base_hex",
                        value: format_hex_u64(phys_base, &mut phys_buf),
                    },
                    Field {
                        key: "len_hex",
                        value: format_hex_u64(len as u64, &mut len_buf),
                    },
                    Field {
                        key: "resolved_cpu_hex",
                        value: format_hex_u64(
                            resolved.unwrap_or(MMIO_MAP_REJECTED),
                            &mut resolved_buf,
                        ),
                    },
                    Field {
                        key: "regs_base_hex",
                        value: format_hex_u64(self.regs_base, &mut regs_base_buf),
                    },
                    Field {
                        key: "regs_end_hex",
                        value: format_hex_u64(regs_end, &mut regs_end_buf),
                    },
                    Field {
                        key: "outbound_pcie_base_hex",
                        value: format_hex_u64(self.outbound_pcie_base, &mut outbound_base_buf),
                    },
                    Field {
                        key: "outbound_pcie_end_hex",
                        value: format_hex_u64(outbound_end, &mut outbound_end_buf),
                    },
                ],
            },
        );
    }

    /// Resolve `[phys, phys+len)` to the identity-mapped **CPU-physical**
    /// base the window is opened over, or `None` if it lies in neither
    /// permitted region. Every bound is overflow-checked and never wraps
    /// (`AGENTS.md` §2.9 / §5.4 — fail closed).
    ///
    /// A request fully inside the controller register block is CPU-physical
    /// and maps identity; a request fully inside the outbound PCIe-bus
    /// window is a BAR address and is translated through the bridge's
    /// outbound viewport to its CPU address.
    ///
    /// The two regions live in *different* address spaces that collapse to
    /// one `u64` here, and on the Pi 4 they overlap numerically — the
    /// `SoC`'s controller-register island (`0xfd50_0000…`) falls inside the
    /// outbound PCIe-bus window (`0xc000_0000…0x1_0000_0000`). The regs
    /// block is resolved first (so the exact register window always maps
    /// identity), and a request that lands in the outbound window but
    /// *overlaps* the regs island without being contained by it is
    /// ambiguous and refused rather than mis-translated (`AGENTS.md` §5.4).
    #[must_use]
    fn resolve_cpu(&self, phys: u64, len: usize) -> Option<u64> {
        let req_end = phys.checked_add(len as u64)?;
        let regs_end = self.regs_base.checked_add(self.regs_len)?;
        if phys >= self.regs_base && req_end <= regs_end {
            return Some(phys);
        }
        let outbound_end = self.outbound_pcie_base.checked_add(self.outbound_size)?;
        if phys >= self.outbound_pcie_base && req_end <= outbound_end {
            // Refuse a BAR window that straddles the numerically-overlapping
            // controller-register island: its address space is ambiguous,
            // so fail closed instead of translating it as a bus address.
            if phys < regs_end && req_end > self.regs_base {
                return None;
            }
            let offset = phys - self.outbound_pcie_base;
            return self.outbound_cpu_base.checked_add(offset);
        }
        None
    }
}

impl MmioMapper for IdentityMmioMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        // Capability before state (`AGENTS.md` §5.4).
        if !self.caps.contains(CapabilityId::MMIO_MAP) {
            return Err(MmioMapError::CapabilityMissing);
        }
        if len == 0 {
            return Err(MmioMapError::InvalidRegion);
        }
        let resolved = self.resolve_cpu(phys_base, len);
        self.log_decision(phys_base, len, resolved);
        let cpu_base = resolved.ok_or(MmioMapError::InvalidRegion)?;
        let addr = usize::try_from(cpu_base).map_err(|_| MmioMapError::InvalidRegion)?;
        let base = NonNull::new(addr as *mut u8).ok_or(MmioMapError::InvalidRegion)?;
        // SAFETY: `resolve_cpu` confirmed `[cpu_base, cpu_base+len)` lies
        // within a region the boot path identity-mapped as Device memory
        // (the PCIe controller block, or the outbound MMIO window a BAR's
        // bus address was translated into), so the CPU-virtual address
        // equals `cpu_base` and the whole window is a valid,
        // exclusively-owned MMIO mapping for the driver's lifetime
        // (`AGENTS.md` §4 — reclaimed when the kernel tears the service
        // down). The window records the device-visible base `phys_base`
        // (what the controller's siblings program), not `cpu_base`. No
        // other live reference aliases device MMIO.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

/// A per-driver DMA host that carves the xHCI device-shared region from the
/// kernel frame allocator, within the bridge's inbound aperture.
///
/// The allocated block is identity-mapped RAM, so the CPU pointer is the
/// frame's own physical address. That CPU-physical frame is rejected unless
/// its whole span lies inside the bridge's inbound CPU window
/// `[inbound_cpu_base, aperture_top)`; only then is it translated to the
/// device-visible (PCIe-space) address the controller DMAs through
/// (`inbound_pcie_base + (phys - inbound_cpu_base)`). The bound is checked
/// in **CPU-physical** space, not on the translated device address: the
/// inbound viewport offsets the device address far above the CPU window
/// (the Pi 4 maps PCIe `[0x4_0000_0000, …)` onto CPU `[0, …)`), so a
/// device-vs-CPU-top comparison would reject every valid carve
/// (`AGENTS.md` §5.4 — fail closed, a device is never handed an address
/// outside the window the bridge grants it). The region is held for the
/// controller's lifetime (a permanent device DMA mapping, `AGENTS.md` §4),
/// so it is allocated once and never freed.
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
    /// and the exclusive **CPU-physical** aperture top `aperture_top` (the
    /// top of the inbound CPU window `[inbound_cpu_base, aperture_top)`),
    /// under `caps`.
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
    /// frame whose CPU-physical span falls outside the bridge's inbound CPU
    /// window `[inbound_cpu_base, aperture_top)` (`AGENTS.md` §5.4 — fail
    /// closed, never wrap).
    ///
    /// The reachability bound is applied to the **CPU-physical** span
    /// (`[phys, phys + size)`), not to the translated device address: the
    /// inbound viewport lifts the device address into a PCIe-space window
    /// (`inbound_pcie_base`) that sits far above the CPU window top, so
    /// bounding the device address against the CPU top would reject every
    /// valid carve (the boot-wedge-after-discovery defect this guards).
    fn device_addr(&self, phys: u64, size: usize) -> Result<u64, DriverError> {
        let offset = phys
            .checked_sub(self.inbound_cpu_base)
            .ok_or(DriverError::OutOfRange)?;
        let cpu_end = phys
            .checked_add(size as u64)
            .ok_or(DriverError::OutOfRange)?;
        if cpu_end > self.aperture_top {
            return Err(DriverError::OutOfRange);
        }
        self.inbound_pcie_base
            .checked_add(offset)
            .ok_or(DriverError::OutOfRange)
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
    use super::{
        service_caps, FrameDmaHost, IdentityMmioMapper, MAILBOX_PROPERTY_BUFFER,
        MAILBOX_PROPERTY_BYTES,
    };

    use alloc::boxed::Box;

    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicU64, Ordering};

    use rustos_abi::RegisterWindow;
    use rustos_arch_aarch64::kernel_arch::{
        busy_delay_us, clean_invalidate_dcache_range, read_cntfrq, read_cntpct,
    };
    use rustos_arch_aarch64::platform::PcieDiscovery;
    use rustos_arch_aarch64::SERIAL_SINK;
    use rustos_drv_bus_pcie_brcm::{Delay, PcieWindows};
    use rustos_drv_input_usb_hid::{pump_once, KeyboardConsole};
    use rustos_kernel_core::{InitSpawnCtx, YieldHandle};
    use rustos_log::{log, Event, EventId, Field, Level};
    use rustos_sync::SpinLock;
    use rustos_util::fmt::format_hex_u64;
    use rustos_vcmailbox::{
        notify_xhci_reset, BufferCoherency, MailboxError, MmioMailbox, DEFAULT_BUS_ALIAS,
        DEFAULT_POLL_BUDGET, MAILBOX_REGS_LEN_BYTES,
    };

    use crate::arch_wrapper_aarch64::INPUT_FOCUS;
    use crate::usb_keyboard::{
        bring_up_keyboard, ArbiterConsoleSink, ChainHost, FirmwareReset, FirmwareResetFailure,
        FirmwareResetOutcome, PcieBringup, VL805_FIRMWARE_DEV_ADDR,
    };

    /// Audit event: the USB-keyboard service kthread's lifecycle (started,
    /// or skipped because a prerequisite was absent). Logged once at the
    /// PID 1 spawn seam so a metal capture shows whether the service was
    /// even admitted before its bring-up diagnostics
    /// ([`crate::usb_keyboard`], ids `4101`/`4102`) run. Bin-crate id
    /// alongside the boot pipeline; part of the audit contract
    /// (`AGENTS.md` §5.4.4).
    const KEYBOARD_SERVICE: EventId = EventId(4103);

    static MAILBOX_DOORBELL: SpinLock<Option<u64>> = SpinLock::new(None);

    /// Record the boot-discovered `VideoCore` mailbox doorbell base for the
    /// optional VL805 firmware fallback. Called once by the boot CPU after the
    /// MMU is live, matching [`record_discovery`]'s `SpinLock` constraint.
    #[doc = "Record the boot-discovered VideoCore mailbox doorbell base."]
    pub fn record_mailbox_doorbell(doorbell_base: u64) {
        *MAILBOX_DOORBELL.lock() = Some(doorbell_base);
    }

    struct VideoCoreFirmwareReset {
        doorbell_base: Option<u64>,
    }

    impl VideoCoreFirmwareReset {
        const fn failed(reason: FirmwareResetFailure) -> FirmwareResetOutcome {
            FirmwareResetOutcome::Failed { reason }
        }

        const fn mailbox_failure(err: MailboxError) -> FirmwareResetFailure {
            match err {
                MailboxError::Window => FirmwareResetFailure::Window,
                MailboxError::Timeout => FirmwareResetFailure::Timeout,
                MailboxError::FirmwareError => FirmwareResetFailure::FirmwareError,
                MailboxError::MalformedResponse => FirmwareResetFailure::MalformedResponse,
                MailboxError::BadAperture => FirmwareResetFailure::BadAperture,
                MailboxError::BadGeometry => FirmwareResetFailure::BadGeometry,
                _ => FirmwareResetFailure::Unknown,
            }
        }

        fn buffer_bus_addr(buffer_phys: u64) -> Option<u32> {
            if buffer_phys >= u64::from(DEFAULT_BUS_ALIAS) {
                return None;
            }
            Some(DEFAULT_BUS_ALIAS | buffer_phys as u32)
        }
    }

    impl FirmwareReset for VideoCoreFirmwareReset {
        fn reload(&self) -> FirmwareResetOutcome {
            let Some(doorbell_base) = self.doorbell_base else {
                return FirmwareResetOutcome::NotAvailable;
            };
            let Some(buffer_ptr) = NonNull::new(MAILBOX_PROPERTY_BUFFER.0.get().cast::<u8>())
            else {
                return Self::failed(FirmwareResetFailure::BadGeometry);
            };
            let buffer_phys = buffer_ptr.as_ptr() as u64;
            let Some(buffer_bus) = Self::buffer_bus_addr(buffer_phys) else {
                return Self::failed(FirmwareResetFailure::BadAperture);
            };
            let Some(doorbell_ptr) = NonNull::new(doorbell_base as usize as *mut u8) else {
                return Self::failed(FirmwareResetFailure::BadGeometry);
            };
            // SAFETY: `doorbell_base` was discovered from the firmware FDT,
            // folded into the Device identity map before MMU enable, and its
            // advertised length was checked by the video mailbox discovery;
            // the property exchange accesses it only through checked dword
            // register operations during this one-shot fallback.
            let regs = unsafe {
                RegisterWindow::from_mapping(doorbell_base, doorbell_ptr, MAILBOX_REGS_LEN_BYTES)
            };
            // SAFETY: `MAILBOX_PROPERTY_BUFFER` is a 16-byte-aligned static
            // owned by this one service for the single firmware-reload
            // fallback. The `MmioMailbox` bounds every access to the 128-byte
            // property message, and the coherency hooks publish/invalidate it
            // around the firmware DMA exchange.
            let buffer = unsafe {
                RegisterWindow::from_mapping(buffer_phys, buffer_ptr, MAILBOX_PROPERTY_BYTES)
            };
            let coherency = BufferCoherency::new(
                |base, len| clean_invalidate_dcache_range(base as usize, len),
                |base, len| clean_invalidate_dcache_range(base as usize, len),
            );
            let Ok(mut mailbox) = MmioMailbox::with_coherency(
                regs,
                buffer,
                buffer_bus,
                DEFAULT_POLL_BUDGET,
                coherency,
            ) else {
                return Self::failed(FirmwareResetFailure::BadGeometry);
            };
            match notify_xhci_reset(&mut mailbox, VL805_FIRMWARE_DEV_ADDR) {
                Ok(response_value) => FirmwareResetOutcome::Reloaded { response_value },
                Err(err) => Self::failed(Self::mailbox_failure(err)),
            }
        }
    }

    /// A [`Delay`] backed by the architectural physical counter
    /// (`CNTPCT_EL0`), for the BCM2711 PCIe link-training settle waits.
    ///
    /// It also accumulates how long the bring-up *asked* to wait
    /// (`requested_us`, over `calls` invocations) so the `4116` timing
    /// measurement can compare what the code requested against the
    /// counter-measured elapsed span and the operator's wall clock — the
    /// datapoint that localises a multi-second bring-up pause to a
    /// timer-rate mismatch rather than a genuine spin (`AGENTS.md`
    /// §15.7 / §2.16 — measure, don't guess). The counters are diagnostic
    /// only and never gate behaviour.
    struct GenericTimerDelay {
        requested_us: AtomicU64,
        calls: AtomicU64,
    }

    impl GenericTimerDelay {
        const fn new() -> Self {
            Self {
                requested_us: AtomicU64::new(0),
                calls: AtomicU64::new(0),
            }
        }

        /// The total microseconds requested and the call count observed so
        /// far (`Relaxed` is sufficient — single-kthread diagnostic totals,
        /// not a synchronisation point).
        fn totals(&self) -> (u64, u64) {
            (
                self.requested_us.load(Ordering::Relaxed),
                self.calls.load(Ordering::Relaxed),
            )
        }
    }

    impl Delay for GenericTimerDelay {
        fn delay_us(&self, us: u32) {
            self.requested_us
                .fetch_add(u64::from(us), Ordering::Relaxed);
            self.calls.fetch_add(1, Ordering::Relaxed);
            busy_delay_us(us);
        }

        fn now_us(&self) -> u64 {
            // Scale the architectural counter to microseconds with the
            // same rate `busy_delay_us` spins against, so a readiness poll
            // bounded by `now_us` measures the same wall time a delay
            // produces. A zero `CNTFRQ_EL0` would be a firmware fault
            // (`AGENTS.md` §2.9 — never divide by it); report 0 so the
            // caller's elapsed span stays non-negative and the bound trips
            // immediately rather than spinning.
            let freq = read_cntfrq();
            if freq == 0 {
                0
            } else {
                read_cntpct().saturating_mul(1_000_000) / freq
            }
        }
    }

    /// Audit event: the bring-up timing measurement. Logged once, right
    /// after the VL805 bring-up chain returns, so a metal capture pins the
    /// multi-second pause between the controller map (`4105`) and the
    /// trained-link (`4101`) lines to its cause. `requested_us_hex` is the
    /// total the code *asked* [`GenericTimerDelay`] to wait across the whole
    /// chain (bounded by the bring-up's settle/poll budgets to a few
    /// hundred milliseconds); `counter_elapsed_us_hex` is the same span as
    /// measured by `CNTPCT_EL0` against `CNTFRQ_EL0` (`timer_hz_hex`).
    ///
    /// When both read a few hundred milliseconds yet the operator's wall
    /// clock shows ~10 s, the architectural counter is running far slower
    /// than `CNTFRQ_EL0` advertises, so every `busy_delay_us` over-waits
    /// proportionally — the timer-rate fault. When `counter_elapsed_us_hex`
    /// itself reads ~10 s, the code genuinely spun that long (a delay
    /// fed an outsized value, fixable in code). One-shot at bring-up, never
    /// on the poll path (`AGENTS.md` §2.16 / §19.4); part of the audit
    /// contract (`AGENTS.md` §5.4.4).
    const BRINGUP_TIMING: EventId = EventId(4116);

    /// Log the bring-up timing measurement (`4116`): the requested vs
    /// counter-measured elapsed span and the resolved counter rate, all
    /// rendered allocation-free on the stack (`AGENTS.md` §2.9).
    fn log_bringup_timing(requested_us: u64, delay_calls: u64, counter_elapsed_us: u64) {
        let mut requested_buf = [0u8; 16];
        let mut calls_buf = [0u8; 16];
        let mut elapsed_buf = [0u8; 16];
        let mut hz_buf = [0u8; 16];
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: BRINGUP_TIMING,
                message: "usb-keyboard: bring-up delay timing measurement",
                fields: &[
                    Field {
                        key: "requested_us_hex",
                        value: format_hex_u64(requested_us, &mut requested_buf),
                    },
                    Field {
                        key: "delay_calls_hex",
                        value: format_hex_u64(delay_calls, &mut calls_buf),
                    },
                    Field {
                        key: "counter_elapsed_us_hex",
                        value: format_hex_u64(counter_elapsed_us, &mut elapsed_buf),
                    },
                    Field {
                        key: "timer_hz_hex",
                        value: format_hex_u64(read_cntfrq(), &mut hz_buf),
                    },
                ],
            },
        );
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
    ///
    /// MUST be called **after** the MMU is enabled: the `SpinLock` below
    /// uses an exclusive `compare_exchange` (an atomic read-modify-write),
    /// which is UNPREDICTABLE on the MMU-off Device-typed memory the boot
    /// CPU runs on (`plans/PI.md` P6c-2). The windows are read pre-MMU (a
    /// `Copy` value) but recording them is deferred until translation is
    /// live so this store never wedges the boot.
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
        // No discovered bridge is the QEMU `virt` shape, not an error: stay
        // silent and start nothing (`AGENTS.md` §18.4).
        let Some(discovery) = *DISCOVERED.lock() else {
            return false;
        };
        // A discovered bridge with no `'static` frame allocator *is* a
        // surprise worth logging: the keyboard cannot be brought up, so the
        // video login parks with no keyboard. Fail closed and say why
        // (`AGENTS.md` §2.9).
        let Some(frames) = ctx.static_frames() else {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Error,
                    id: KEYBOARD_SERVICE,
                    message: "usb-keyboard service not started: no kernel frame allocator",
                    fields: &[],
                },
            );
            return false;
        };

        let caps = service_caps();
        // Attach the serial sink so each map decision (the controller regs
        // block, the VL805 BAR) is logged once on metal: a refused BAR
        // (the `length_out_of_range` xHCI bring-up) shows the exact base it
        // asked for against the discovered window (`AGENTS.md` §5.4.4).
        let mapper: &'static IdentityMmioMapper = Box::leak(Box::new(
            IdentityMmioMapper::new(
                caps,
                discovery.regs_phys,
                discovery.regs_len,
                discovery.outbound_cpu_base,
                discovery.outbound_pcie_base,
                discovery.outbound_size,
            )
            .with_diag(&SERIAL_SINK),
        ));
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
        };

        let body = move |yielder: &mut dyn YieldHandle| {
            let host = ChainHost::new(caps, mapper, dma);
            let firmware_reset = VideoCoreFirmwareReset {
                doorbell_base: *MAILBOX_DOORBELL.lock(),
            };
            let delay = GenericTimerDelay::new();
            // Bring the VL805 up once. A failure (no link, no device, a
            // refused map) ends the service fail-closed: the video login
            // parks with no keyboard rather than the kernel hanging
            // (`AGENTS.md` §2.9). The chain logs its own staged progress
            // (pcie link, xhci, enumeration) to the serial sink, so a metal
            // capture pins which stage a silent keyboard stalled at.
            //
            // Bracket the chain with `CNTPCT_EL0` and compare against the
            // delay's own requested-microsecond tally: a multi-second
            // operator-visible pause whose `requested_us`/`counter_elapsed_us`
            // are only a few hundred milliseconds is a timer-rate fault
            // (every `busy_delay_us` over-waits), not a code-side spin
            // (`4116`, `AGENTS.md` §15.7).
            let start_ticks = read_cntpct();
            let result = bring_up_keyboard(&host, &bringup, &firmware_reset, &delay, &SERIAL_SINK);
            let elapsed_ticks = read_cntpct().wrapping_sub(start_ticks);
            let freq = read_cntfrq();
            let counter_elapsed_us = if freq == 0 {
                0
            } else {
                elapsed_ticks.saturating_mul(1_000_000) / freq
            };
            let (requested_us, delay_calls) = delay.totals();
            log_bringup_timing(requested_us, delay_calls, counter_elapsed_us);
            let mut keyboard = match result {
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

        let started = ctx.spawn_kernel_service(Box::new(body));
        log(
            &SERIAL_SINK,
            &Event {
                level: if started { Level::Info } else { Level::Error },
                id: KEYBOARD_SERVICE,
                message: if started {
                    "usb-keyboard service kthread admitted (bring-up runs on first dispatch)"
                } else {
                    "usb-keyboard service kthread could not be admitted"
                },
                fields: &[],
            },
        );
        started
    }
}

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub use metal::{record_discovery, record_mailbox_doorbell, spawn_if_present};

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
    /// outbound MMIO window mapping CPU `0x6_0000_0000` onto PCIe-bus
    /// `0xc000_0000`, size `1 GiB`.
    const REGS_BASE: u64 = 0xfd50_0000;
    const REGS_LEN: u64 = 0x9310;
    const OUTBOUND_CPU_BASE: u64 = 0x6_0000_0000;
    const OUTBOUND_PCIE_BASE: u64 = 0xc000_0000;
    const OUTBOUND_SIZE: u64 = 0x4000_0000;
    const APERTURE_TOP: u64 = 0xc000_0000;

    fn mapper(caps: CapabilitySet) -> IdentityMmioMapper {
        IdentityMmioMapper::new(
            caps,
            REGS_BASE,
            REGS_LEN,
            OUTBOUND_CPU_BASE,
            OUTBOUND_PCIE_BASE,
            OUTBOUND_SIZE,
        )
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
        // The controller register block is CPU-physical and maps identity.
        let regs = m.map_window(REGS_BASE, 0x1000).expect("regs block");
        assert_eq!(regs.phys_base(), REGS_BASE);
        // A BAR inside the outbound *PCIe-bus* window is admitted (it is
        // translated to its CPU address below); the CPU base `0x6_…` is
        // *not* itself a valid request — only the bus address is.
        assert!(m.map_window(OUTBOUND_PCIE_BASE + 0x1_0000, 0x1000).is_ok());
        assert_eq!(
            m.map_window(OUTBOUND_CPU_BASE, 0x1000).err(),
            Some(MmioMapError::InvalidRegion)
        );
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
            m.map_window(OUTBOUND_PCIE_BASE, usize::MAX).err(),
            Some(MmioMapError::InvalidRegion)
        );
    }

    #[test]
    fn mapper_translates_a_bar_through_the_outbound_viewport() {
        // The boot-wedge-after-discovery defect: the VL805's BAR base read
        // from PCIe config space is a *bus* address inside the outbound
        // PCIe window (firmware assigns it ≈ `0xc000_0000`), not a CPU
        // address. The mapper must apply the bridge's outbound `ranges`
        // translation (`outbound_cpu_base + (bus - outbound_pcie_base)`)
        // and open the window over the identity-mapped CPU address — the
        // old mapper compared the bus address against the CPU window and
        // rejected every valid BAR with `InvalidRegion` (→ `LengthOutOfRange`).
        let m = mapper(with_caps(&[CapabilityId::MMIO_MAP]));
        // A BAR at the bus base maps to the CPU base.
        let at_base = m.map_window(OUTBOUND_PCIE_BASE, 0x1000).expect("bus base");
        // The window records the device-visible (bus) base it was asked for.
        assert_eq!(at_base.phys_base(), OUTBOUND_PCIE_BASE);
        assert_eq!(
            m.resolve_cpu(OUTBOUND_PCIE_BASE, 0x1000),
            Some(OUTBOUND_CPU_BASE)
        );
        // A BAR offset into the window translates by the same offset.
        assert_eq!(
            m.resolve_cpu(OUTBOUND_PCIE_BASE + 0x12_3000, 0x1000),
            Some(OUTBOUND_CPU_BASE + 0x12_3000)
        );
        // The last admissible byte (a window flush against the top) still
        // resolves; one byte past the top fails closed.
        assert_eq!(
            m.resolve_cpu(OUTBOUND_PCIE_BASE + OUTBOUND_SIZE - 0x1000, 0x1000),
            Some(OUTBOUND_CPU_BASE + OUTBOUND_SIZE - 0x1000)
        );
        assert_eq!(
            m.resolve_cpu(OUTBOUND_PCIE_BASE + OUTBOUND_SIZE - 0x800, 0x1000),
            None
        );
    }

    /// A `'static`, `Sync` sink capturing the last event's id and whether
    /// it was an error, so the diagnostic test can assert the map decision
    /// was logged. Atomics make it `Sync` without interior `RefCell`
    /// (which the `&'static (dyn Sink + Sync)` bound `with_diag` requires
    /// would reject).
    struct DiagSink {
        last_id: core::sync::atomic::AtomicU32,
        last_was_error: core::sync::atomic::AtomicBool,
    }

    impl Sink for DiagSink {
        fn write_event(&self, event: &Event<'_>) {
            use core::sync::atomic::Ordering;
            self.last_id.store(event.id.0, Ordering::Relaxed);
            self.last_was_error
                .store(event.level == Level::Error, Ordering::Relaxed);
        }
    }

    static DIAG_SINK: DiagSink = DiagSink {
        last_id: core::sync::atomic::AtomicU32::new(0),
        last_was_error: core::sync::atomic::AtomicBool::new(false),
    };

    #[test]
    fn mapper_diagnostics_log_the_map_decision() {
        use core::sync::atomic::Ordering;
        let m = mapper(with_caps(&[CapabilityId::MMIO_MAP])).with_diag(&DIAG_SINK);
        // A BAR base in the outbound *CPU* window (a 64-bit BAR a firmware
        // programmed with the CPU-domain address rather than the bus
        // address) is outside the accepted PCIe-bus window and is refused —
        // exactly the `length_out_of_range` xHCI bring-up shape. The
        // decision is logged at `Error` so a metal capture shows the base.
        assert_eq!(
            m.map_window(OUTBOUND_CPU_BASE, 0x1000).err(),
            Some(MmioMapError::InvalidRegion)
        );
        assert_eq!(
            DIAG_SINK.last_id.load(Ordering::Relaxed),
            MMIO_MAP_DECISION.0
        );
        assert!(DIAG_SINK.last_was_error.load(Ordering::Relaxed));
        // An admitted map logs the same id at `Info` (not `Error`).
        let _ = m.map_window(REGS_BASE, 0x1000).expect("regs block");
        assert_eq!(
            DIAG_SINK.last_id.load(Ordering::Relaxed),
            MMIO_MAP_DECISION.0
        );
        assert!(!DIAG_SINK.last_was_error.load(Ordering::Relaxed));
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

    #[test]
    fn dma_host_admits_a_low_frame_through_a_high_pcie_viewport() {
        // The real Pi 4 shape (the boot-wedge-after-discovery defect): the
        // inbound viewport lifts CPU `[0, 0x2_0000_0000)` onto PCIe
        // `[0x4_0000_0000, 0x6_0000_0000)`, so the device-visible address of
        // any RAM frame is far *above* the CPU-physical aperture top. A
        // low-RAM frame must still be admitted — bounding the device address
        // against the CPU top (the old bug) rejected every valid carve and
        // failed the xHCI bring-up with `OutOfRange`.
        const PI4_PCIE_BASE: u64 = 0x4_0000_0000;
        const PI4_CPU_TOP: u64 = 0x2_0000_0000;
        let host = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0,
            PI4_PCIE_BASE,
            PI4_CPU_TOP,
        );
        // A frame in low RAM translates to a device address above the CPU
        // top yet inside the inbound window — accepted.
        assert_eq!(
            host.device_addr(0x3000_0000, 0x4000),
            Ok(PI4_PCIE_BASE + 0x3000_0000)
        );
        // A frame whose CPU span overruns the CPU aperture top is still
        // rejected, fail-closed.
        assert_eq!(
            host.device_addr(PI4_CPU_TOP - 0x800, 0x1000),
            Err(DriverError::OutOfRange)
        );
    }
}
