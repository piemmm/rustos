//! aarch64 in-kernel USB-keyboard service (`plans/PI.md` P10/P11).
//!
//! The boot-path counterpart of the architecture-neutral
//! [`crate::usb_keyboard`] engine: it supplies the concrete in-kernel
//! [`DriverHost`](rustos_abi::DriverHost) halves the chain needs on the Pi
//! 4 — a capability-gated [`MmioMapper`] over the identity map and a
//! per-driver DMA host over the frame allocator — plus a
//! generic-timer-backed [`Delay`](rustos_drv_bus_pcie_brcm::Delay), and
//! runs the chain as a kthread that brings the VL805 up once then polls it.
//!
//! # Why the identity map
//!
//! The boot path folds the discovered PCIe register and outbound-MMIO
//! gigapages into PID 1's identity-mapped Device memory before enabling the
//! MMU, so [`IdentityMmioMapper`] mints a [`RegisterWindow`] at the
//! window's own CPU-physical address (`phys == virt`) after checking the
//! capability and that it lies within one of the two discovered regions —
//! it never edits a live page table at driver time (`AGENTS.md` §5.4).
//!
//! # No QEMU vertical
//!
//! QEMU models no Pi PCIe/USB (`AGENTS.md` §0.4), so the bring-up is a
//! metal-acceptance item; host tests cover the capability/bounds decisions
//! of the two `DriverHost` halves.

use core::ptr::NonNull;

use rustos_abi::driver::dma::{DmaHost, DmaSlab, PoolId, SlabCoherencyFn};
use rustos_abi::driver::virtio::VirtioHost;
use rustos_abi::{CapabilityId, DriverError, MmioMapError, MmioMapper, RegisterWindow};
use rustos_caps::CapabilitySet;
use rustos_kernel_mem::{FrameAllocator, PAGE_SIZE};
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

/// The capabilities the keyboard bus-driver task holds: [`CapabilityId::MMIO_MAP`]
/// (the PCIe register block and VL805 BAR) and [`CapabilityId::MEM_DMA`]
/// (the xHCI DMA region). No more — every map/alloc is re-checked against
/// this set (`AGENTS.md` §5.4); no ambient authority.
#[must_use]
pub fn service_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::MMIO_MAP);
    caps.insert(CapabilityId::MEM_DMA);
    caps
}

/// Convert a `CNTPCT_EL0` tick span to microseconds at the counter rate
/// `freq` (`CNTFRQ_EL0`). One conversion shared by the bring-up `now_us`
/// clock and the firmware-reload wait measurement (`AGENTS.md` §2.2). A
/// zero `freq` returns `0` (never a divisor); `saturating_*` keeps a
/// wrapped/reordered sample bounded.
#[cfg(any(test, all(freestanding, kernel_isa = "aarch64")))]
fn counter_elapsed_us(start_ticks: u64, end_ticks: u64, freq: u64) -> u64 {
    if freq == 0 {
        return 0;
    }
    end_ticks
        .saturating_sub(start_ticks)
        .saturating_mul(1_000_000)
        / freq
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

/// Audit event: an [`IdentityMmioMapper`] map-window accept/deny
/// (`AGENTS.md` §5.4.4), logged once per bring-up map. Shows the `[base,
/// len)` the PCI driver asked to map and whether it resolved to a backed
/// CPU address; a BAR base outside the outbound window resolves to the
/// all-ones sentinel.
const MMIO_MAP_DECISION: EventId = EventId(4105);

/// The `resolved_cpu_hex` value logged for a refused map: no CPU address
/// resolved, so the window lies outside every region the bridge maps.
const MMIO_MAP_REJECTED: u64 = u64::MAX;

/// A capability-gated, bridge-aware [`MmioMapper`] for a device behind the
/// Pi 4's PCIe root complex.
///
/// Admits a `map_window` only with [`CapabilityId::MMIO_MAP`] and when
/// `[base, base+len)` resolves to one of two regions, failing closed
/// otherwise (`AGENTS.md` §5.4 / §2.9):
///
/// * the controller register block `[regs_base, regs_base+regs_len)` —
///   CPU-physical, identity-mapped; or
/// * a BAR inside the bridge's outbound PCIe-bus window
///   `[outbound_pcie_base, +outbound_size)`, translated to its CPU address
///   via the bridge's `ranges` (`outbound_cpu_base + (bus -
///   outbound_pcie_base)`).
///
/// Resolving bus addresses to a CPU mapping is the host bridge's job, so it
/// lives here, never in the architecture-neutral PCI walk.
pub struct IdentityMmioMapper {
    caps: CapabilitySet,
    regs_base: u64,
    regs_len: u64,
    outbound_cpu_base: u64,
    outbound_pcie_base: u64,
    outbound_size: u64,
    /// Optional diagnostic sink for the map decision
    /// ([`MMIO_MAP_DECISION`]); `None` on the host, the serial sink on
    /// metal (via [`Self::with_diag`]).
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
    /// decision is logged once; the resolution is unchanged, so this adds
    /// observability without widening authority.
    #[must_use]
    pub fn with_diag(mut self, sink: &'static (dyn Sink + Sync)) -> Self {
        self.diag = Some(sink);
        self
    }

    /// Log one map-window decision through the attached diagnostic sink, if
    /// any (a no-op on the host build).
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

    /// Resolve `[phys, phys+len)` to the identity-mapped CPU-physical base,
    /// or `None` if it lies in neither permitted region. Every bound is
    /// overflow-checked (fail closed). A request inside the register block
    /// maps identity; one inside the outbound window is translated through
    /// the bridge viewport. The two regions overlap numerically on the Pi
    /// 4, so the regs block resolves first and a BAR that straddles it
    /// without being contained is refused as ambiguous.
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
        // within a region the boot path identity-mapped as Device memory,
        // so the CPU-virtual address equals `cpu_base` and the window is a
        // valid, exclusively-owned MMIO mapping for the driver's lifetime.
        // The window records the device-visible base `phys_base`, not
        // `cpu_base`. No other live reference aliases device MMIO.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

/// A per-driver DMA host that carves the xHCI device-shared region from the
/// kernel frame allocator, within the bridge's inbound aperture.
///
/// The frame is identity-mapped RAM (CPU pointer == physical address). It
/// is rejected unless its whole span lies inside the inbound CPU window
/// `[inbound_cpu_base, aperture_top)`, then translated to the
/// device-visible address (`inbound_pcie_base + (phys - inbound_cpu_base)`).
/// The bound is checked in CPU-physical space, not on the translated device
/// address, since the viewport offsets it far above the CPU window. Held
/// for the controller's lifetime: allocated once, never freed.
pub struct FrameDmaHost {
    caps: CapabilitySet,
    frames: &'static FrameAllocator,
    inbound_cpu_base: u64,
    inbound_pcie_base: u64,
    aperture_top: u64,
    /// Cache-maintenance shim for a non-coherent DMA master. `None` on a
    /// coherent interconnect or the host build; the aarch64
    /// clean/invalidate primitive on metal (the BCM2711 PCIe master does
    /// not snoop the CPU caches).
    coherency: Option<SlabCoherencyFn>,
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
        coherency: Option<SlabCoherencyFn>,
    ) -> Self {
        Self {
            caps,
            frames,
            inbound_cpu_base,
            inbound_pcie_base,
            aperture_top,
            coherency,
        }
    }

    /// Translate a CPU-physical frame base into the device-visible
    /// (PCIe-space) address, rejecting any frame whose CPU-physical span
    /// falls outside the inbound CPU window `[inbound_cpu_base,
    /// aperture_top)` (fail closed, never wrap). The bound is applied in
    /// CPU-physical space, not on the translated device address (which the
    /// viewport lifts far above the CPU window).
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

impl DmaHost for FrameDmaHost {
    fn alloc_dma_zeroed(&self, size: usize) -> Result<DmaSlab, DriverError> {
        // Capability before state (`AGENTS.md` §5.4).
        if !self.caps.contains(CapabilityId::MEM_DMA) {
            return Err(DriverError::PermissionDenied);
        }
        if size == 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        // A power-of-two contiguous block covering `size`.
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
        let slab = unsafe { DmaSlab::from_leaked(device, ptr, size, KEYBOARD_DMA_POOL, 0) };
        // Cache maintenance on a non-coherent interconnect: the BCM2711
        // PCIe master does not snoop the CPU caches, so without it the
        // controller reads a stale command ring and every command times
        // out (`AGENTS.md` §4).
        Ok(match self.coherency {
            Some(coherency) => slab.with_coherency(coherency),
            None => slab,
        })
    }
}

impl VirtioHost for FrameDmaHost {
    fn notify_wait(&self, _queue_index: u16) {}
}

// The metal half: the generic-timer delay, the boot→seam discovery hand-off,
// and the service-kthread spawn. Tied to the aarch64 build because it names
// the arch port and input-focus arbiter; the host build tests the two
// `DriverHost` halves above instead (`AGENTS.md` §17.4).
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
    use rustos_arch_aarch64::platform::{PcieDiscovery, PCIE_COMPATIBLE};
    use rustos_arch_aarch64::SERIAL_SINK;
    use rustos_devmatch::MatchResolution;
    use rustos_drv_bus_pcie_brcm::{Delay, PcieWindows};
    use rustos_hid::{pump_once, KeyboardConsole};
    use rustos_kernel_core::{InitSpawnCtx, YieldHandle};
    use rustos_log::{log, Event, EventId, Field, Level};
    use rustos_sync::SpinLock;
    use rustos_util::fmt::format_hex_u64;
    use rustos_vcmailbox::{
        notify_xhci_reset, query_firmware_revision, BufferCoherency, ExchangeStats, MailboxError,
        MmioMailbox, TimeoutStage, DEFAULT_BUS_ALIAS, DEFAULT_POLL_BUDGET, MAILBOX_REGS_LEN_BYTES,
    };

    use rustos_abi::{CapabilityId, HwNode};
    use rustos_caps::CapabilitySet;

    use crate::aarch64::arch_wrapper::INPUT_FOCUS;
    use crate::driver_catalog;
    use crate::driver_loader::KernelDriverLoader;
    use crate::usb_keyboard::{
        bring_up_keyboard, ArbiterConsoleSink, ChainHost, FirmwareReset, FirmwareResetFailure,
        FirmwareResetOutcome, KeyboardChain, KeyboardPumpDiagnostics, PcieBringup,
        VL805_FIRMWARE_DEV_ADDR,
    };

    /// Audit event: the USB-keyboard service kthread's lifecycle (started,
    /// or skipped because a prerequisite was absent), logged once at the
    /// PID 1 spawn seam before its bring-up diagnostics run.
    const KEYBOARD_SERVICE: EventId = EventId(4103);

    /// Audit event: the §18.3 autoload decision for the discovered PCIe
    /// root complex (`AGENTS.md` §5.4.4). The bring-up runs because the
    /// driver-candidate catalogue *bound* a driver to the discovered node,
    /// never because a bridge address exists: a bound node logs the winning
    /// driver path, an unmatched node is left unbound (`AGENTS.md` §18.4),
    /// and a packaging tie or an unrepresentable identity fails closed.
    const KEYBOARD_AUTOLOAD: EventId = EventId(4112);

    /// Audit event: the signed driver-load gate decision for an in-kernel
    /// driver (`plans/PI.md` P10 5c-ii). Each driver is *admitted*
    /// through `drvhost::Host::load` — Ed25519 signature verification
    /// against the build's embedded key plus the `CAP_DRV_LOAD` /
    /// `CAP_DRV_KERNEL` gates — before it operates; a refused admission
    /// fails closed (`AGENTS.md` §5.4 / §23.1). The detailed reject reason
    /// is logged by the host itself (its sink is this serial sink); this
    /// event records the higher-level start/skip decision keyed by path.
    const KEYBOARD_LOAD_GATE: EventId = EventId(4132);

    /// Audit event: diagnostics from the VL805 firmware-reload mailbox
    /// exchange ([`ExchangeStats`]), logged after the `NOTIFY_XHCI_RESET`
    /// call. Adds *where* it timed out (`timeout_stage`: `1` inbox write
    /// room, `2` completion post), the posted word, poll counts, foreign
    /// completions, and the last status — distinguishing a transport fault
    /// from `VideoCore` dropping the tag.
    const MAILBOX_EXCHANGE: EventId = EventId(4121);

    /// Audit event: the runtime mailbox liveness probe
    /// ([`query_firmware_revision`]), issued before the `NOTIFY_XHCI_RESET`
    /// reload over the same transport. It mutates no hardware, so it
    /// isolates whether the mailbox path works: `probe_outcome=ok` pins a
    /// later reload timeout on `VideoCore` dropping the tag, while a probe
    /// timeout pins it on the mailbox environment.
    const MAILBOX_PROBE: EventId = EventId(4122);

    /// The poll budget the VL805 firmware-**reload** mailbox waits the
    /// `NOTIFY_XHCI_RESET` completion out against — ten times the quick
    /// property-read budget ([`DEFAULT_POLL_BUDGET`]). The reload is a
    /// multi-hundred-ms `VideoCore` operation (≈4 s of wall time here), so a
    /// shorter budget could report a still-in-progress reload as dropped.
    /// Bounded and fails closed; `4121`'s `wait_elapsed_us_hex` reports the
    /// real span.
    const FIRMWARE_RELOAD_POLL_BUDGET: u32 = 10 * DEFAULT_POLL_BUDGET;

    static MAILBOX_DOORBELL: SpinLock<Option<u64>> = SpinLock::new(None);

    /// Cache-maintenance shim handed to every xHCI DMA slab
    /// ([`FrameDmaHost`]'s [`rustos_abi::driver::dma::SlabCoherencyFn`]).
    /// The BCM2711 PCIe root complex is not cache-coherent, so cleaning and
    /// invalidating the range to the point of coherency (`dc civac` + `dsb`)
    /// keeps the command/event rings consistent in both directions.
    fn dma_coherency(base: *const u8, len: usize) {
        clean_invalidate_dcache_range(base as usize, len);
    }

    /// Stable, allocation-free name for the wait a timed-out mailbox
    /// exchange gave up in.
    const fn timeout_stage_name(stage: TimeoutStage) -> &'static str {
        match stage {
            TimeoutStage::None => "none",
            TimeoutStage::PostRoom => "post_room",
            TimeoutStage::Response => "response",
        }
    }

    /// Stack buffers for rendering an [`ExchangeStats`] into hex fields,
    /// shared by the `4121` reload and `4122` probe loggers (`AGENTS.md`
    /// §2.2).
    struct ExchangeStatBufs {
        posted: [u8; 16],
        post_polls: [u8; 16],
        response_reads: [u8; 16],
        foreign: [u8; 16],
    }

    impl ExchangeStatBufs {
        const fn new() -> Self {
            Self {
                posted: [0u8; 16],
                post_polls: [0u8; 16],
                response_reads: [0u8; 16],
                foreign: [0u8; 16],
            }
        }
    }

    /// Render the five [`ExchangeStats`] fields shared by the `4121` reload
    /// and `4122` probe diagnostics into `bufs`. The five buffers are
    /// disjoint struct fields, so each borrow is independent; the returned
    /// fields borrow `bufs` for the duration of the log call (`AGENTS.md`
    /// §2.2 — one field layout, two consumers).
    fn exchange_stat_fields(stats: ExchangeStats, bufs: &mut ExchangeStatBufs) -> [Field<'_>; 5] {
        [
            Field {
                key: "timeout_stage",
                value: timeout_stage_name(stats.timeout_stage),
            },
            Field {
                key: "posted_word_hex",
                value: format_hex_u64(u64::from(stats.posted_word), &mut bufs.posted),
            },
            Field {
                key: "post_room_polls_hex",
                value: format_hex_u64(u64::from(stats.post_room_polls), &mut bufs.post_polls),
            },
            Field {
                key: "response_reads_hex",
                value: format_hex_u64(u64::from(stats.response_reads), &mut bufs.response_reads),
            },
            Field {
                key: "foreign_channel_reads_hex",
                value: format_hex_u64(u64::from(stats.foreign_channel_reads), &mut bufs.foreign),
            },
        ]
    }

    /// Stable, allocation-free name for a [`MailboxError`], reusing the
    /// [`FirmwareResetFailure`] reason table so the probe-outcome field and
    /// the `4108` reload reason never diverge (`AGENTS.md` §2.2).
    fn mailbox_error_name(err: MailboxError) -> &'static str {
        VideoCoreFirmwareReset::mailbox_failure(err).as_str()
    }

    /// Log the VL805 firmware-reload mailbox exchange diagnostics (`4121`)
    /// one-shot, so a metal capture localises a timed-out reload. Rendered
    /// on the stack, never on the poll path (`AGENTS.md` §2.9 / §2.16).
    ///
    /// `wait_elapsed_us` is the `CNTPCT_EL0`-measured wall time the reload
    /// exchange actually took, and `poll_budget_hex` the iteration budget it
    /// was allowed ([`FIRMWARE_RELOAD_POLL_BUDGET`]): a `response`-stage
    /// timeout whose elapsed already exceeds the 1 s budget is a
    /// genuinely dropped tag, not a wait cut short (`AGENTS.md` §15.7).
    fn log_mailbox_exchange(stats: ExchangeStats, wait_elapsed_us: u64) {
        let mut bufs = ExchangeStatBufs::new();
        let mut last_status_buf = [0u8; 16];
        let mut elapsed_buf = [0u8; 16];
        let mut budget_buf = [0u8; 16];
        let stat_fields = exchange_stat_fields(stats, &mut bufs);
        let fields = [
            stat_fields[0],
            stat_fields[1],
            stat_fields[2],
            stat_fields[3],
            stat_fields[4],
            Field {
                key: "last_status_hex",
                value: format_hex_u64(u64::from(stats.last_status), &mut last_status_buf),
            },
            Field {
                key: "wait_elapsed_us_hex",
                value: format_hex_u64(wait_elapsed_us, &mut elapsed_buf),
            },
            Field {
                key: "poll_budget_hex",
                value: format_hex_u64(u64::from(FIRMWARE_RELOAD_POLL_BUDGET), &mut budget_buf),
            },
        ];
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: MAILBOX_EXCHANGE,
                message: "usb-keyboard: vl805 firmware reload mailbox exchange diagnostics",
                fields: &fields,
            },
        );
    }

    /// Log the runtime mailbox liveness-probe diagnostics (`4122`) one-shot,
    /// before the reload, so a metal capture separates a broken post-MMU
    /// mailbox path from `VideoCore` specifically dropping the xHCI tag.
    /// Rendered on the stack, never on the poll path (`AGENTS.md` §2.9 /
    /// §2.16 / §15.7).
    fn log_mailbox_probe(outcome: Result<u32, MailboxError>, stats: ExchangeStats) {
        let mut bufs = ExchangeStatBufs::new();
        let mut revision_buf = [0u8; 16];
        let (outcome_name, revision) = match outcome {
            Ok(revision) => ("ok", revision),
            Err(err) => (mailbox_error_name(err), 0),
        };
        let stat_fields = exchange_stat_fields(stats, &mut bufs);
        let fields = [
            Field {
                key: "probe_outcome",
                value: outcome_name,
            },
            Field {
                key: "firmware_revision_hex",
                value: format_hex_u64(u64::from(revision), &mut revision_buf),
            },
            stat_fields[0],
            stat_fields[1],
            stat_fields[2],
            stat_fields[3],
            stat_fields[4],
        ];
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: MAILBOX_PROBE,
                message: "usb-keyboard: runtime mailbox liveness probe diagnostics",
                fields: &fields,
            },
        );
    }

    /// Record the boot-discovered `VideoCore` mailbox doorbell base for the
    /// optional VL805 firmware fallback. Called once by the boot CPU after
    /// the MMU is live.
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
                FIRMWARE_RELOAD_POLL_BUDGET,
                coherency,
            ) else {
                return Self::failed(FirmwareResetFailure::BadGeometry);
            };
            // Liveness probe first: a benign firmware-revision read over the
            // same transport, separating a broken mailbox path (probe times
            // out) from `VideoCore` dropping the xHCI tag (probe ok, reload
            // times out). It mutates no device state. Read the stats now —
            // the reload below overwrites them.
            let probe = query_firmware_revision(&mut mailbox);
            log_mailbox_probe(probe, mailbox.last_exchange_stats());
            // Measure the reload's real wall time across `CNTPCT_EL0`, so
            // `4121` reports the genuine wait, not an assumed iteration→time
            // mapping of the budget.
            let reload_start = read_cntpct();
            let outcome = notify_xhci_reset(&mut mailbox, VL805_FIRMWARE_DEV_ADDR);
            let reload_elapsed_us =
                super::counter_elapsed_us(reload_start, read_cntpct(), read_cntfrq());
            // Log the exchange diagnostics (`4121`) regardless of outcome,
            // read before mapping it so the stats reflect this call.
            log_mailbox_exchange(mailbox.last_exchange_stats(), reload_elapsed_us);
            match outcome {
                Ok(response_value) => FirmwareResetOutcome::Reloaded { response_value },
                Err(err) => Self::failed(Self::mailbox_failure(err)),
            }
        }
    }

    /// A [`Delay`] backed by the architectural physical counter
    /// (`CNTPCT_EL0`), for the BCM2711 PCIe link-training settle waits. It
    /// also accumulates how long the bring-up *asked* to wait
    /// (`requested_us` over `calls`), so the `4116` measurement can compare
    /// the requested span against the counter-measured one and the
    /// operator's wall clock. Diagnostic only; never gates behaviour.
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
            // Scale the counter at the same rate `busy_delay_us` spins
            // against, so a `now_us`-bounded poll measures the same wall
            // time a delay produces. Fails closed on a zero `CNTFRQ_EL0`.
            super::counter_elapsed_us(0, read_cntpct(), read_cntfrq())
        }
    }

    /// Audit event: the bring-up timing measurement, logged once after the
    /// chain returns. `requested_us_hex` is what the code asked
    /// [`GenericTimerDelay`] to wait; `counter_elapsed_us_hex` is the
    /// `CNTPCT_EL0` span against `CNTFRQ_EL0` (`timer_hz_hex`). Both small
    /// while the wall clock shows ~10 s means a timer-rate fault (the
    /// counter runs slower than advertised); a large counter span means the
    /// code genuinely spun.
    const BRINGUP_TIMING: EventId = EventId(4116);

    /// Log the bring-up timing measurement (`4116`): requested vs
    /// counter-measured span and the counter rate.
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
    /// init seam. Set once on the boot CPU and read once at PID 1 spawn, so
    /// the `SpinLock` never contends; `None` with no `brcm,bcm2711-pcie`
    /// node (the QEMU `virt` shape), where the service is not started.
    static DISCOVERED: SpinLock<Option<PcieDiscovery>> = SpinLock::new(None);

    /// Record the PCIe windows the boot path discovered, for the init seam.
    /// MUST be called **after** the MMU is enabled: the `SpinLock`'s atomic
    /// read-modify-write is UNPREDICTABLE on the MMU-off Device memory the
    /// boot CPU runs on (`plans/PI.md` P6c-2).
    pub fn record_discovery(discovery: PcieDiscovery) {
        *DISCOVERED.lock() = Some(discovery);
    }

    /// Resolve the discovered PCIe root complex against the in-kernel
    /// driver-candidate catalogue ([`crate::driver_catalog`]), returning the
    /// winning driver's image path when a driver binds the node.
    ///
    /// This is the §18 data-driven gate (`plans/PI.md` P10 5c): the bring-up
    /// proceeds because a driver's bind table matched the discovered node's
    /// identity, **not** because a bridge address was found — the kernel no
    /// longer hunts for a keyboard (`AGENTS.md` §18.5). Every outcome is
    /// audited as [`KEYBOARD_AUTOLOAD`]: a bound node logs the winning
    /// driver path, an unmatched node is left unbound and logged (§18.4),
    /// and a packaging tie or an identity that does not fit a match key
    /// fails closed (`AGENTS.md` §2.9 / §5.4).
    fn resolve_discovered_bridge() -> Option<&'static str> {
        let Some(key) = crate::driver_catalog::bridge_compatible_key(PCIE_COMPATIBLE) else {
            log(
                &SERIAL_SINK,
                &Event {
                    level: Level::Error,
                    id: KEYBOARD_AUTOLOAD,
                    message:
                        "usb-keyboard autoload: discovered pcie identity unrepresentable; unbound",
                    fields: &[],
                },
            );
            return None;
        };
        match crate::driver_catalog::resolve_driver(&[key]) {
            MatchResolution::Winner {
                candidate,
                priority,
            } => {
                let path = crate::driver_catalog::driver_candidates()[candidate].path;
                let mut pbuf = [0u8; 16];
                log(
                    &SERIAL_SINK,
                    &Event {
                        level: Level::Info,
                        id: KEYBOARD_AUTOLOAD,
                        message: "usb-keyboard autoload: discovered pcie node bound to driver",
                        fields: &[
                            Field {
                                key: "driver",
                                value: path,
                            },
                            Field {
                                key: "priority_hex",
                                value: format_hex_u64(u64::from(priority), &mut pbuf),
                            },
                        ],
                    },
                );
                Some(path)
            }
            MatchResolution::Unmatched => {
                log(
                    &SERIAL_SINK,
                    &Event {
                        level: Level::Info,
                        id: KEYBOARD_AUTOLOAD,
                        message: "usb-keyboard autoload: discovered pcie node unmatched; unbound",
                        fields: &[],
                    },
                );
                None
            }
            MatchResolution::Tie { priority } => {
                let mut pbuf = [0u8; 16];
                log(
                    &SERIAL_SINK,
                    &Event {
                        level: Level::Error,
                        id: KEYBOARD_AUTOLOAD,
                        message:
                            "usb-keyboard autoload: discovered pcie node tie (packaging defect); \
                             failing closed",
                        fields: &[Field {
                            key: "priority_hex",
                            value: format_hex_u64(u64::from(priority), &mut pbuf),
                        }],
                    },
                );
                None
            }
        }
    }

    /// The capability set the in-kernel driver loader presents when
    /// admitting a driver: `CAP_DRV_LOAD` (the universal load gate) plus
    /// `CAP_DRV_KERNEL` (every in-kernel manifest is `kind = InKernel`).
    /// Each driver is granted only the intersection of this set with its
    /// manifest's request (`AGENTS.md` §5.2).
    fn loader_caps() -> CapabilitySet {
        let mut caps = CapabilitySet::empty();
        caps.insert(CapabilityId::DRV_LOAD);
        caps.insert(CapabilityId::DRV_KERNEL);
        caps
    }

    /// Log one in-kernel driver's admission outcome at [`KEYBOARD_LOAD_GATE`].
    fn log_admission(level: Level, path: &str, message: &'static str) {
        log(
            &SERIAL_SINK,
            &Event {
                level,
                id: KEYBOARD_LOAD_GATE,
                message,
                fields: &[Field {
                    key: "driver",
                    value: path,
                }],
            },
        );
    }

    /// Admit the two head bus drivers (`pcie_brcm`, `bus_usb`) through the
    /// signed `drvhost::Host::load` gate before the chain is brought up.
    ///
    /// Returns whether **both** were admitted. Fails closed (`AGENTS.md`
    /// §5.4 / §23.1): if either is refused — bad signature, syscall-hash
    /// mismatch, missing capability — the chain is not brought up. The
    /// host logs the detailed reject reason itself (its sink is the serial
    /// sink); this records the higher-level decision keyed by path.
    fn admit_bus_drivers(loader: &KernelDriverLoader<'_>) -> bool {
        let caps = loader_caps();
        for path in [driver_catalog::PCIE_BRCM_PATH, driver_catalog::BUS_USB_PATH] {
            if loader.admit(path, &caps).is_ok() {
                log_admission(
                    Level::Info,
                    path,
                    "usb-keyboard: in-kernel driver admitted through the signed load gate",
                );
            } else {
                log_admission(
                    Level::Error,
                    path,
                    "usb-keyboard: in-kernel driver refused at the signed load gate; not starting",
                );
                return false;
            }
        }
        true
    }

    /// Re-match the enumerated HID child node against the driver catalogue
    /// and admit the winning HID driver through the signed gate before the
    /// report pump feeds the input arbiter (`plans/PI.md` P10 5c-ii — the
    /// §18 growable-runtime-tree re-autoload step).
    ///
    /// Returns whether the HID driver was admitted. Fails closed: an
    /// unmatched child, a packaging tie, or a refused admission means no
    /// key edge is ever injected — an unadmitted device must not drive
    /// input (`AGENTS.md` §5.4 / §23.1).
    fn admit_hid_child(loader: &KernelDriverLoader<'_>, hid_node: &HwNode) -> bool {
        let path = match driver_catalog::resolve_driver(hid_node.match_keys()) {
            MatchResolution::Winner { candidate, .. } => {
                driver_catalog::driver_candidates()[candidate].path
            }
            MatchResolution::Unmatched => {
                log_admission(
                    Level::Error,
                    "(enumerated-hid)",
                    "usb-keyboard: enumerated HID child matched no driver; not pumping",
                );
                return false;
            }
            MatchResolution::Tie { .. } => {
                log_admission(
                    Level::Error,
                    "(enumerated-hid)",
                    "usb-keyboard: enumerated HID child tie (packaging defect); not pumping",
                );
                return false;
            }
        };
        if loader.admit(path, &loader_caps()).is_ok() {
            log_admission(
                Level::Info,
                path,
                "usb-keyboard: HID driver admitted through the signed load gate",
            );
            true
        } else {
            log_admission(
                Level::Error,
                path,
                "usb-keyboard: HID driver refused at the signed load gate; not pumping",
            );
            false
        }
    }

    /// The enumerated boot keyboard handed from the synchronous floor
    /// bring-up ([`bring_up_keyboard_into_tree`]) to the report-pump kthread
    /// ([`spawn_pump`]).
    ///
    /// A [`KeyboardChain`] owns its mapped xHCI register window and DMA
    /// region (raw device pointers), so it is not auto-`Send`. The hand-off
    /// is a one-shot transfer of *exclusive* ownership from the boot CPU
    /// (where the controller was brought up) to the single pump kthread that
    /// is thereafter its sole owner — never aliased, never concurrently
    /// accessed — and the device memory it points at is identity-mapped for
    /// the kernel's lifetime, so moving it across that boundary is sound.
    pub struct SendKeyboard(KeyboardChain);

    // SAFETY: `SendKeyboard` is the sole, exclusive owner of the keyboard's
    // mapped MMIO/DMA resources (no aliasing), transferred exactly once from
    // the boot CPU to the one pump kthread; there is never concurrent or
    // shared access, so moving the handle between those execution contexts
    // cannot race (`AGENTS.md` §2.10).
    unsafe impl Send for SendKeyboard {}

    impl SendKeyboard {
        /// Borrow the wrapped keyboard for the pump loop.
        fn keyboard_mut(&mut self) -> &mut KeyboardChain {
            &mut self.0
        }
    }

    /// Bring the VL805 USB-HID keyboard online **once** on the boot CPU and
    /// emit its enumerated identity as a discovered child [`HwNode`]
    /// (`AGENTS.md` §18.2), returning that node together with the live
    /// keyboard the report-pump kthread drives ([`spawn_pump`]).
    ///
    /// This is design B's floor ownership of USB enumeration (`plans/PI.md`
    /// B3): the bootstrap-floor bus drivers bring the controller up and
    /// enumerate the keyboard behind it, so the §18 discovery path sees the
    /// device like every other one. The caller attaches the returned node to
    /// the discovered tree ([`crate::unlock_service::augment_boot_tree`])
    /// before the pre-unlock autoload runs, and hands the keyboard to
    /// [`spawn_pump`]; the controller is brought up exactly once here, never
    /// a second time (`AGENTS.md` §2.16 / §2.17 — the in-kernel pump keeps
    /// driving the keyboard until the B5 flip, with no redundant bring-up).
    ///
    /// Returns [`None`], failing closed (`AGENTS.md` §18.4 / §2.9 / §5.4),
    /// when: no PCIe bridge was discovered (the QEMU `virt` shape); the
    /// discovered node binds no in-kernel bus driver
    /// ([`resolve_discovered_bridge`]); the driver trust anchor is
    /// unavailable; a bus or HID driver is refused at the signed load gate;
    /// there is no `'static` frame allocator; or the bring-up / enumeration
    /// fails. The capability-gated MMIO mapper and DMA host are leaked to
    /// `'static` — a one-shot boot publish, never a per-event allocation.
    #[must_use]
    pub fn bring_up_keyboard_into_tree(ctx: &dyn InitSpawnCtx) -> Option<(HwNode, SendKeyboard)> {
        // No discovered bridge is the QEMU `virt` shape, not an error: stay
        // silent and bring nothing up (`AGENTS.md` §18.4).
        let discovery = (*DISCOVERED.lock())?;
        // §18 data-driven gate (`plans/PI.md` P10 5c): bring the chain up
        // only because the driver-candidate catalogue *binds* a driver to the
        // discovered node — not because a bridge address exists. An unmatched
        // node (no in-kernel driver claims it), a packaging tie, or an
        // unrepresentable identity leaves the node unbound, logged, and
        // nothing brought up (`AGENTS.md` §18.4 / §2.9). The catalogue binds
        // `brcm,bcm2711-pcie` to `pcie_brcm`, so on the Pi 4 this proceeds.
        resolve_discovered_bridge()?;
        // §8 / §9 load gate (`plans/PI.md` P10 5c-ii): admit the head bus
        // drivers (`pcie_brcm`, `bus_usb`) through the signed
        // `drvhost::Host::load` path — Ed25519 signature verification
        // against the build's embedded key plus the `CAP_DRV_LOAD` /
        // `CAP_DRV_KERNEL` gates — *before* the chain touches hardware, not
        // a bare `register()` call. A refused admission fails closed: the
        // chain is not brought up (`AGENTS.md` §5.4 / §23.1). The HID driver
        // is admitted below, once enumeration discovers the device
        // (`admit_hid_child`).
        let Some(loader) = KernelDriverLoader::new(&SERIAL_SINK) else {
            log_admission(
                Level::Error,
                "(trust-anchor)",
                "usb-keyboard: driver trust anchor unavailable; not starting",
            );
            return None;
        };
        if !admit_bus_drivers(&loader) {
            return None;
        }
        // A discovered bridge with no `'static` frame allocator is worth
        // logging: the keyboard cannot be brought up. Fail closed.
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
            return None;
        };

        let caps = service_caps();
        // Attach the serial sink so each map decision is logged once on
        // metal: a refused BAR shows the base it asked for.
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
            Some(dma_coherency),
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

        let host = ChainHost::new(caps, mapper, dma);
        let firmware_reset = VideoCoreFirmwareReset {
            doorbell_base: *MAILBOX_DOORBELL.lock(),
        };
        let delay = GenericTimerDelay::new();
        // Bring the VL805 up once on the boot CPU. A failure fails closed
        // (login then parks with no keyboard); the chain logs its own staged
        // progress to the serial sink.
        //
        // Bracket the chain with `CNTPCT_EL0` and compare against the delay's
        // requested-microsecond tally (`4116`): a multi-second pause whose
        // requested/elapsed are only a few hundred ms is a timer-rate fault,
        // not a code-side spin.
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
        let brought_up = result.ok()?;
        // §18 re-autoload (`plans/PI.md` P10 5c-ii): re-match the enumerated
        // HID child against the driver catalogue and admit the winning HID
        // driver through the signed gate before any key edge can reach the
        // input arbiter. Fail closed: an unmatched/refused child means the
        // keyboard is not pumped (`AGENTS.md` §5.4 / §23.1 — an unadmitted
        // device must not drive input).
        let loader = KernelDriverLoader::new(&SERIAL_SINK)?;
        if !admit_hid_child(&loader, &brought_up.hid_node) {
            return None;
        }
        Some((brought_up.hid_node, SendKeyboard(brought_up.keyboard)))
    }

    /// Spawn the report-pump kthread that drives the already-brought-up
    /// `keyboard` ([`bring_up_keyboard_into_tree`]) into the input-focus
    /// arbiter, returning whether it was admitted.
    ///
    /// The kthread polls the keyboard forever, yielding between polls so
    /// PID 1 keeps running; a `pump_once` error is non-fatal and folded into
    /// the bounded pump diagnostics (`4129` first report, `4130` pump error,
    /// `4131` heartbeat). The controller is **not** brought up here — that
    /// happened once on the boot CPU — so this never touches the VL805 a
    /// second time (`plans/PI.md` B3 / `AGENTS.md` §2.16).
    #[must_use]
    pub fn spawn_pump(ctx: &dyn InitSpawnCtx, mut keyboard: SendKeyboard) -> bool {
        let body = move |yielder: &mut dyn YieldHandle| {
            let mut console = KeyboardConsole::new();
            let mut sink = ArbiterConsoleSink::new(&INPUT_FOCUS);
            let mut diagnostics = KeyboardPumpDiagnostics::new();
            // Poll the keyboard forever, yielding between polls so PID 1
            // keeps running. A `pump_once` error is non-fatal. The result is
            // folded into `diagnostics`, which emits bounded audit events
            // (`4129` first report, `4130` pump error, `4131` heartbeat).
            loop {
                let result = pump_once(keyboard.keyboard_mut(), &mut console, &mut sink);
                diagnostics.record(result, &SERIAL_SINK);
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
                    "usb-keyboard report-pump kthread admitted"
                } else {
                    "usb-keyboard report-pump kthread could not be admitted"
                },
                fields: &[],
            },
        );
        started
    }
}

#[cfg(all(freestanding, kernel_isa = "aarch64"))]
pub use metal::{
    bring_up_keyboard_into_tree, record_discovery, record_mailbox_doorbell, spawn_pump,
    SendKeyboard,
};

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
        let host = FrameDmaHost::new(with_caps(&[]), frame_allocator(), 0, 0, APERTURE_TOP, None);
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
            None,
        );
        assert_eq!(host.device_addr(0x10_0000, 0x4000), Ok(0x10_0000));
        // A nonzero inbound viewport offsets the device address.
        let shifted = FrameDmaHost::new(
            with_caps(&[CapabilityId::MEM_DMA]),
            frame_allocator(),
            0x8000_0000,
            0x4000_0000,
            APERTURE_TOP,
            None,
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
            None,
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
            None,
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
            None,
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

    #[test]
    fn counter_elapsed_us_converts_a_tick_span_at_the_counter_rate() {
        // The Pi 4 architectural counter runs at 54 MHz (the metal capture's
        // `timer_hz_hex=0x337_f980`): one second of ticks is one million µs.
        const PI4_HZ: u64 = 54_000_000;
        assert_eq!(counter_elapsed_us(0, PI4_HZ, PI4_HZ), 1_000_000);
        // A span measured from a non-zero start is the difference only.
        assert_eq!(
            counter_elapsed_us(PI4_HZ, PI4_HZ + PI4_HZ / 1000, PI4_HZ),
            1_000
        );
    }

    #[test]
    fn counter_elapsed_us_fails_closed_on_a_zero_or_reordered_sample() {
        // A zero `CNTFRQ_EL0` is a firmware fault, never a divisor — report
        // 0 rather than trapping (`AGENTS.md` §2.9).
        assert_eq!(counter_elapsed_us(0, 1_000_000, 0), 0);
        // A reordered/wrapped sample (end before start) saturates to 0, not
        // a panic or a huge span.
        assert_eq!(counter_elapsed_us(1_000, 500, 54_000_000), 0);
    }
}
