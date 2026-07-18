//! TAIRiX Raspberry Pi 4 (BCM2711) EMMC2 SD-host block driver.
//!
//! The Pi 4's EMMC2 controller is an Arasan / SDHCI-5.1 SD host. This
//! driver brings an SD card up over the standard SDHCI register block and
//! exposes it through [`tairix_abi::driver::block::Block`].
//!
//! # Transfer paths
//!
//! The fast path is **32-bit ADMA2 DMA**: the controller masters a whole
//! transfer chunk over the DAT lines through a one-entry descriptor table
//! the engine stages in a device-shared bounce region ([`adma`],
//! [`DMA_STAGE_BLOCKS`] blocks per chunk), so a multi-block transfer
//! completes with a **single** completion interrupt instead of a per-block
//! buffer handshake, and the CPU never copies data word-by-word through the
//! slow uncached buffer data port. A larger request is split into
//! successive chunks. The engine selects this path at bring-up when the
//! host grants a device-shared DMA staging region (through
//! [`SdhciHost::dma_region`]).
//!
//! The fallback path is **programmed I/O** (PIO): blocks move one 512-byte
//! block at a time through the buffer data port (`CMD17`/`CMD18` reads,
//! `CMD24`/`CMD25` writes). It needs no DMA capability and is used when the
//! host grants no DMA region — DMA where possible, correct everywhere
//! (`plans/PI.md` P8).
//!
//! # Layered seam
//!
//! The SDHCI command/response and block-transfer state machine
//! ([`Emmc2`]) is written against the [`SdhciHost`] register seam, not a
//! concrete memory mapping. Metal drives it over a capability-gated
//! [`RegisterWindow`] ([`SdhciHost`] is implemented for it); host tests
//! drive it over a register-level mock controller. This mirrors the
//! `rpi_hvs` mailbox seam: the protocol layer is
//! proven host-side, the doorbell below it on metal.
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`Emmc2`] is a public *type* the driver host instantiates through
//! [`wiring::open_discovered`]; the host never reaches into it beyond the
//! [`Block`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; mapping the discovered
//! register window additionally requires [`CapabilityId::MMIO_MAP`]
//! (checked in [`wiring`]). The driver runs in user space and does not
//! request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::dma::DmaSlab;
use tairix_abi::driver::mmio::WindowError;
use tairix_abi::{
    CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey, RegisterWindow,
};
use tairix_dma_barrier::{dma_rmb, dma_wmb};

pub mod adma;
pub mod command;
pub mod regs;
pub mod wiring;

#[cfg(test)]
mod tests;

use command::{ResponseKind, SdCommand, BLOCK_SIZE, BLOCK_WORDS};

/// Per-driver `DriverHandle` marker returned by [`register`].
///
/// The bytes spell `"SDP"` (SD PIO) with a version nibble, matching the
/// other drivers' marker convention.
const REGISTER_HANDLE_MARKER: u64 = 0x5344_5000_0000_0001;

/// The bind priority [`BIND_KEYS`] carries.
///
/// An exact `compatible`-string match: it ranks at the exact-match tier
/// (higher matched priority binds; an unbroken tie is
/// a packaging defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: the BCM2711
/// EMMC2 SD host, matched by the device-tree `compatible` string
/// `brcm,bcm2711-emmc2` the aarch64 `FdtDiscovery` emits on the Storage
/// node (`wiring`). The single source of truth the signed-manifest bind
/// table is authored from and a discovered node is resolved against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(b"brcm,bcm2711-emmc2") {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

/// Upper bound on register polls while waiting for a controller event.
///
/// A bound on a *defence* against an unresponsive or absent controller,
/// not a scalable capacity: an SD command or block
/// transfer completes in microseconds, so a million polls is orders of
/// magnitude past any honest completion. Exceeding it fails closed with
/// [`DriverError::DeviceFault`] rather than spinning forever.
pub const DEFAULT_POLL_BUDGET: u32 = 1_000_000;

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// The SDHCI register-access seam.
///
/// Every controller access the [`Emmc2`] engine makes goes through this
/// trait, so the command/response and block-transfer state machine is
/// proven host-side against a register-level mock.
/// Both methods take `&mut self` so a model can represent registers with
/// read side-effects (the buffer data port advances; write-1-to-clear
/// status bits).
pub trait SdhciHost {
    /// Read the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError>;

    /// Write `value` to the 32-bit register at byte `offset`.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] if `offset` is outside the mapped
    /// register window.
    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError>;

    /// Park the calling task until the controller raises its interrupt
    /// line (or the wait's bounded budget elapses), then return so the
    /// engine re-reads `INTERRUPT`.
    ///
    /// This is the seam that keeps the engine off the CPU while a slow SD
    /// completion is outstanding (a driver poll
    /// must never busy-spin a status register and monopolise the CPU). The
    /// metal host ([`IrqSdhci`]) parks on the controller's bound GIC line
    /// through a [`CompletionWait`]; the host-test register mock returns
    /// immediately because its completions appear inline in the model, so
    /// the engine's next `INTERRUPT` read already observes the bit.
    ///
    /// [`CompletionSignal::TimedOut`] reports a wait that elapsed with no
    /// interrupt at all; the engine fails the transfer closed on it (see
    /// [`CompletionWait::await_irq`]).
    fn await_irq(&mut self) -> CompletionSignal;

    /// The device-shared DMA staging region this host provides, or `None`
    /// for a programmed-I/O-only host.
    ///
    /// A host that returns `Some` lets the engine move whole transfer
    /// chunks by ADMA2: the engine stages the descriptor table and the
    /// block data in the returned [`DmaRegion`] and hands the controller
    /// its device-visible base, so the CPU never copies data through the
    /// slow buffer data port. A host that returns `None` (no DMA grant, or
    /// an interconnect the platform cannot DMA over) makes the engine fall
    /// back to the block-at-a-time programmed-I/O path — DMA where
    /// possible, correct everywhere.
    ///
    /// The region is re-borrowed per call so it never aliases a concurrent
    /// [`read32`](Self::read32) / [`write32`](Self::write32): the engine
    /// stages into it, drops the borrow, then programs the controller.
    /// The default implementation is PIO-only.
    fn dma_region(&mut self) -> Option<DmaRegion<'_>> {
        None
    }

    /// Synchronize a byte range of the DMA staging region between the CPU
    /// cache and the controller.
    ///
    /// The engine calls this after publishing bytes the controller will read
    /// and before consuming bytes the controller wrote. A coherent or
    /// Normal-Non-Cacheable host needs no maintenance and keeps this default
    /// no-op; a cacheable host forwards the range to its DMA slab's
    /// coherency operation. Out-of-range requests fail closed inside the
    /// slab and are never issued by the engine.
    fn sync_dma_range(&mut self, _offset: usize, _len: usize) {}
}

/// A borrowed view of a host's device-shared DMA staging region.
///
/// `bytes` is the CPU-accessible staging buffer (the engine writes the
/// ADMA2 descriptor table and, for a write, the outbound block data into
/// it, and reads inbound block data out of it after the transfer);
/// `device_base` is the device-visible address of `bytes[0]` — the base
/// the ADMA2 descriptor's data pointer and the controller's descriptor-
/// table register are expressed relative to. The engine brackets ownership
/// hand-offs with [`SdhciHost::sync_dma_range`] and orders them against the
/// controller doorbell/completion with [`dma_wmb`] / [`dma_rmb`], so the host
/// may provide either coherent/Normal-Non-Cacheable memory or cacheable memory
/// with explicit maintenance.
pub struct DmaRegion<'a> {
    /// CPU-accessible staging bytes (descriptor table + data area).
    pub bytes: &'a mut [u8],
    /// Device-visible base address of `bytes[0]`.
    pub device_base: u64,
}

/// Park-until-completion seam the metal [`IrqSdhci`] host drives
/// ([`SdhciHost::await_irq`]).
///
/// The eMMC2 driver is generic over `lib/abi` only,
/// so it cannot name the kernel's IRQ-wait machinery. This one-method trait
/// is the inversion point: the kernel binary supplies an implementation that
/// blocks the calling task on the controller's bound interrupt line and is
/// resumed by its ISR (mirroring the virtio host's `notify_wait`), while a
/// host test supplies an inline no-wait. A spurious wake-up cannot be
/// mistaken for a retriable failure — the engine re-reads the status
/// register on every [`CompletionSignal::Fired`] return.
pub trait CompletionWait {
    /// Block until the controller signals a completion on its interrupt
    /// line or the implementation's bounded budget elapses; the caller
    /// re-reads `INTERRUPT` on a fire and fails closed on a timeout.
    fn await_irq(&self) -> CompletionSignal;
}

/// Outcome of one [`CompletionWait::await_irq`] wait.
///
/// The SDHCI controller raises its interrupt for every started operation —
/// a completion *or* an error status — so a wait that elapses with no
/// interrupt at all means the controller (or its interrupt routing) is
/// dead. The engine maps [`Self::TimedOut`] to
/// [`DriverError::DeviceFault`] immediately rather than re-polling a
/// silent device: the caller gets a loud, typed error and the system keeps
/// running, instead of a task parked forever holding its mount's lock.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CompletionSignal {
    /// The interrupt line fired (or may have fired); re-read `INTERRUPT`.
    Fired,
    /// The bounded wait elapsed with no interrupt — a dead controller.
    TimedOut,
}

/// The metal SDHCI host: the capability-gated [`RegisterWindow`] paired with
/// a [`CompletionWait`] that parks on the controller's GIC interrupt line.
///
/// Splitting register access from the completion wait mirrors the virtio
/// driver's transport/host split: `read32`/`write32` go to
/// the mapped window, and [`await_irq`](SdhciHost::await_irq) parks the task
/// on the controller's interrupt rather than busy-spinning. Built by [`open_discovered`](crate::wiring::open_discovered)
/// from the discovered register window and a kernel-supplied waiter.
pub struct IrqSdhci<W: CompletionWait> {
    window: RegisterWindow,
    waiter: W,
    /// The device-shared DMA staging slab, when the host granted one.
    /// `Some` selects the ADMA2 transfer path; `None` keeps the driver on
    /// the programmed-I/O path (`plans/PI.md` P8 — DMA where possible).
    dma: Option<DmaSlab>,
}

impl<W: CompletionWait> IrqSdhci<W> {
    /// Pair a mapped register `window` with the completion `waiter` that
    /// parks on the controller's interrupt line, on the programmed-I/O
    /// path (no DMA staging region).
    #[must_use]
    pub fn new(window: RegisterWindow, waiter: W) -> Self {
        Self {
            window,
            waiter,
            dma: None,
        }
    }

    /// As [`Self::new`], but with a device-shared DMA staging `slab` so the
    /// engine moves transfers by ADMA2 rather than the buffer data port.
    ///
    /// The slab must be at least [`DMA_REGION_BYTES`] long (a shorter one
    /// yields fewer staged blocks, and a zero-length data area falls back
    /// to programmed I/O). On a non-coherent interconnect the slab must
    /// either be Normal-Non-Cacheable or carry the cache-maintenance shim
    /// [`DmaSlab::sync_range`] invokes.
    #[must_use]
    pub fn with_dma(window: RegisterWindow, waiter: W, slab: DmaSlab) -> Self {
        Self {
            window,
            waiter,
            dma: Some(slab),
        }
    }
}

impl<W: CompletionWait> SdhciHost for IrqSdhci<W> {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        self.window
            .read_u32(offset)
            .map_err(WindowError::as_driver_error)
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        self.window
            .write_u32(offset, value)
            .map_err(WindowError::as_driver_error)
    }

    fn await_irq(&mut self) -> CompletionSignal {
        self.waiter.await_irq()
    }

    fn dma_region(&mut self) -> Option<DmaRegion<'_>> {
        let slab = self.dma.as_mut()?;
        let device_base = slab.phys();
        Some(DmaRegion {
            bytes: slab.as_bytes_mut(),
            device_base,
        })
    }

    fn sync_dma_range(&mut self, offset: usize, len: usize) {
        if let Some(slab) = self.dma.as_ref() {
            slab.sync_range(offset, len);
        }
    }
}

/// SD-clock frequency-select divisor used during card identification.
///
/// SD identification must run at or below 400 kHz. The exact base clock
/// is board-specific, so a conservative divisor keeps the identification
/// clock in range on the Pi 4's EMMC2 base clock.
const IDENT_CLOCK_DIVISOR: u32 = 0x80;

/// SD-clock frequency-select divisor used for data transfers, once the
/// card has been identified and selected.
///
/// The SDHCI 8-bit divided-clock relation is `SDCLK = base / (2 · divisor)`,
/// so this divisor is `IDENT_CLOCK_DIVISOR / 32` and the data clock is
/// therefore exactly **32× the identification clock**. The identification
/// clock is held at or below 400 kHz (the SD spec ceiling encoded by
/// [`IDENT_CLOCK_DIVISOR`]), so the data clock stays at or below
/// `32 · 400 kHz = 12.8 MHz` for *any* base clock at which identification
/// was in range — comfortably within SD Default Speed's 25 MHz limit, so no
/// high-speed mode switch or tuning is required. It is
/// derived from the identification divisor rather than a base-clock constant
/// precisely so it carries no board assumption of its own: whatever base makes identification legal makes this legal too.
const DATA_CLOCK_DIVISOR: u32 = IDENT_CLOCK_DIVISOR / 32;

/// Data-timeout-counter value (`CONTROL1[19:16]`): the controller's
/// maximum data-line timeout, the conservative bring-up setting.
const DATA_TIMEOUT_VALUE: u32 = 0x0E;

/// Largest number of blocks one transfer command may carry. The SDHCI
/// 16-bit block-count field bounds a single transfer (a format-fixed bound, not a scalable capacity); a caller
/// asking for more is rejected fail-closed.
const MAX_BLOCKS_PER_TRANSFER: usize = 0xFFFF;

/// Number of 512-byte blocks the ADMA2 DMA path stages per transfer chunk.
///
/// The staging buffer is a bounce region the whole chunk moves through in
/// one DMA command, so a transfer larger than this is split into
/// successive chunks by the engine — the value caps the *staging window*,
/// not the total transfer, so it is not a scaling ceiling. 128 blocks is
/// 64 KiB, the largest a single 32-bit ADMA2 descriptor's 16-bit length
/// field can carry ([`adma::MAX_DESC_BYTES`]), so one descriptor covers a
/// full chunk. A read/write of an arbitrary number of blocks loops over
/// this window; each 64 KiB chunk completes with a single command and a
/// single completion interrupt instead of 128 per-block PIO buffer
/// handshakes.
pub const DMA_STAGE_BLOCKS: usize = 128;

/// Byte length of the DMA data-staging area (`DMA_STAGE_BLOCKS` blocks).
const DMA_DATA_BYTES: usize = DMA_STAGE_BLOCKS * BLOCK_SIZE as usize;

/// Byte offset of the one-entry ADMA2 descriptor table within the DMA
/// region: immediately after the data-staging area.
const DMA_DESC_OFFSET: usize = DMA_DATA_BYTES;

/// Minimum byte length of the DMA staging slab a host must grant for the
/// ADMA2 path: the data-staging area plus the one-entry descriptor table.
///
/// The metal wiring sizes its DMA carve to exactly this. A host that
/// grants a shorter region is treated as programmed-I/O-only (the engine
/// falls back rather than staging past the region — fail closed).
pub const DMA_REGION_BYTES: usize = DMA_DATA_BYTES + adma::DESC_BYTES;

/// The step of the SD identification sequence a bring-up reached before
/// failing.
///
/// Carried by [`BringUpFault`] so an in-kernel / metal caller can log
/// *which* step of [`Emmc2::open`] the controller stalled at, rather than
/// a single opaque error. `raspi4b` cannot model EMMC2 (`plans/PI.md`
/// §0.4), so on a real Raspberry Pi 4 this stage is the only signal that
/// localises an SD bring-up failure (`plans/PI.md` P8/B4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BringUpStage {
    /// Mapping the discovered SDHCI register window (the [`wiring`]
    /// pre-step, before any controller access).
    MapWindow,
    /// Controller reset and SD-clock stabilisation (`reset_and_clock`).
    ResetClock,
    /// `CMD0` `GO_IDLE_STATE`.
    GoIdle,
    /// `CMD8` `SEND_IF_COND` (v2 voltage / check-pattern echo).
    SendIfCond,
    /// `ACMD41` `SD_SEND_OP_COND` power-up polling.
    OpCond,
    /// `CMD2` `ALL_SEND_CID`.
    AllSendCid,
    /// `CMD3` `SEND_RELATIVE_ADDR`.
    SendRelativeAddr,
    /// `CMD9` `SEND_CSD` and the CSD geometry derivation.
    SendCsd,
    /// `CMD7` `SELECT_CARD`.
    SelectCard,
    /// `CMD16` `SET_BLOCKLEN`.
    SetBlockLen,
    /// `ACMD6` `SET_BUS_WIDTH` and the controller-side 4-bit width bit.
    SetBusWidth,
    /// Raising the SD clock from the identification to the data rate.
    RaiseClock,
}

impl BringUpStage {
    /// A stable, terse, human-readable name for the stage.
    ///
    /// Logged as a structured field on the failing-stage audit line so the
    /// metal UART log names the exact SD command that stalled. The strings
    /// are part of the operator-facing diagnostic contract; treat them as
    /// stable.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BringUpStage::MapWindow => "map register window",
            BringUpStage::ResetClock => "reset + SD clock",
            BringUpStage::GoIdle => "CMD0 GO_IDLE_STATE",
            BringUpStage::SendIfCond => "CMD8 SEND_IF_COND",
            BringUpStage::OpCond => "ACMD41 SD_SEND_OP_COND",
            BringUpStage::AllSendCid => "CMD2 ALL_SEND_CID",
            BringUpStage::SendRelativeAddr => "CMD3 SEND_RELATIVE_ADDR",
            BringUpStage::SendCsd => "CMD9 SEND_CSD",
            BringUpStage::SelectCard => "CMD7 SELECT_CARD",
            BringUpStage::SetBlockLen => "CMD16 SET_BLOCKLEN",
            BringUpStage::SetBusWidth => "ACMD6 SET_BUS_WIDTH",
            BringUpStage::RaiseClock => "raise SD clock to data rate",
        }
    }
}

/// A card bring-up failure: the [`BringUpStage`] reached and the
/// underlying [`DriverError`].
///
/// [`Emmc2::open`] returns this so the failing step is recoverable for
/// diagnostics (`plans/PI.md` P8/B4). A consumer that only needs the
/// `DriverError` (the driver-ABI shape) converts with
/// `DriverError::from` / `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BringUpFault {
    /// The step the bring-up reached before failing.
    pub stage: BringUpStage,
    /// The underlying driver error at that step.
    pub error: DriverError,
}

impl BringUpFault {
    /// Pair `stage` with the `error` that ended the bring-up there.
    #[must_use]
    const fn new(stage: BringUpStage, error: DriverError) -> Self {
        Self { stage, error }
    }
}

impl From<BringUpFault> for DriverError {
    fn from(fault: BringUpFault) -> Self {
        fault.error
    }
}

/// An SD card brought up over the SDHCI register seam.
///
/// `H` is the register backing: a capability-gated [`RegisterWindow`] on
/// metal, a register-level mock in host tests. Dropping the [`Emmc2`]
/// drops `H`; for the metal window that releases the mapping the kernel
/// reclaims on unload.
pub struct Emmc2<H: SdhciHost> {
    host: H,
    geometry: BlockGeometry,
    /// The card's Relative Card Address, already positioned in bits
    /// `[31:16]` for use as an addressed-command argument.
    rca: u32,
    poll_budget: u32,
    /// Blocks the ADMA2 DMA path stages per chunk, or `0` when the host
    /// granted no DMA region (the engine then uses programmed I/O). Set
    /// once during [`Emmc2::init`] from the host's [`SdhciHost::dma_region`].
    dma_stage_blocks: usize,
}

impl<H: SdhciHost> Emmc2<H> {
    /// Bring the card up over `host` with the default poll budget.
    ///
    /// Runs the full SD identification sequence (reset → clock → `CMD0`,
    /// `CMD8`, `ACMD41`, `CMD2`, `CMD3`, `CMD9`, `CMD7`, `CMD16`) and
    /// derives the block geometry from the card's CSD.
    ///
    /// # Errors
    ///
    /// Returns a [`BringUpFault`] naming the [`BringUpStage`] that failed
    /// and the underlying error:
    ///
    /// * [`DriverError::Unsupported`] if the card is not a v2 high-
    ///   capacity (block-addressed) SD card.
    /// * [`DriverError::DeviceFault`] if the controller never completes
    ///   a command or the card never finishes power-up within the poll
    ///   budget.
    ///
    /// Convert to a bare [`DriverError`] with `?` / `DriverError::from`.
    pub fn open(host: H) -> Result<Self, BringUpFault> {
        Self::open_with_budget(host, DEFAULT_POLL_BUDGET)
    }

    /// Bring the card up over `host`, bounding every controller wait by
    /// `poll_budget` (used by host tests to assert the fail-closed
    /// timeout path with a small budget).
    ///
    /// # Errors
    ///
    /// As [`Emmc2::open`].
    pub fn open_with_budget(host: H, poll_budget: u32) -> Result<Self, BringUpFault> {
        let mut dev = Self {
            host,
            geometry: BlockGeometry {
                block_size: BLOCK_SIZE,
                block_count: 0,
            },
            rca: 0,
            poll_budget,
            dma_stage_blocks: 0,
        };
        dev.init()?;
        Ok(dev)
    }

    /// Borrow the underlying register backing (host-test inspection).
    #[must_use]
    pub fn host(&self) -> &H {
        &self.host
    }

    /// Poll `register` until every bit in `mask` clears, within the
    /// budget; fail closed on a stuck controller.
    fn wait_clear(&mut self, register: usize, mask: u32) -> Result<(), DriverError> {
        for _ in 0..self.poll_budget {
            if self.host.read32(register)? & mask == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DriverError::DeviceFault)
    }

    /// Poll `register` until every bit in `mask` is set, within the
    /// budget; fail closed on a stuck controller.
    fn wait_set(&mut self, register: usize, mask: u32) -> Result<(), DriverError> {
        for _ in 0..self.poll_budget {
            if self.host.read32(register)? & mask == mask {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(DriverError::DeviceFault)
    }

    /// Wait for the `INTERRUPT` register to assert `wanted`, **parking the
    /// task on the controller's interrupt** between checks rather than
    /// busy-spinning the CPU, and failing closed on any error bit.
    ///
    /// The wanted bits are cleared (write-1-to-clear) before returning so
    /// the next wait starts from a clean status word — which also de-asserts
    /// the controller's level-sensitive interrupt line, so the following
    /// [`SdhciHost::await_irq`] re-arms cleanly for the next completion.
    ///
    /// Each iteration reads the status once and, if neither `wanted` nor an
    /// error bit is set yet, parks via [`SdhciHost::await_irq`] until the
    /// controller signals — never a busy-spin (: a
    /// driver poll must not monopolise the CPU and starve interrupt-driven
    /// work). `poll_budget` bounds the number of parks as a fail-closed
    /// backstop against a storm of spurious wake-ups; the metal completion
    /// itself arrives in one or two iterations. A wait that reports
    /// [`CompletionSignal::TimedOut`] fails closed at once: the controller
    /// signals every started operation (completion or error), so a silent
    /// budget-long wait means the device is dead and re-polling it would
    /// only stretch the outage.
    fn wait_interrupt(&mut self, wanted: u32) -> Result<(), DriverError> {
        for _ in 0..self.poll_budget {
            let status = self.host.read32(regs::REG_INTERRUPT)?;
            if status & regs::INT_ERROR_MASK != 0 {
                // Clear the latched error and fail closed — never retry-until-it-works.
                self.host.write32(regs::REG_INTERRUPT, status)?;
                return Err(DriverError::DeviceFault);
            }
            if status & wanted == wanted {
                self.host.write32(regs::REG_INTERRUPT, wanted)?;
                return Ok(());
            }
            if self.host.await_irq() == CompletionSignal::TimedOut {
                return Err(DriverError::DeviceFault);
            }
        }
        Err(DriverError::DeviceFault)
    }

    /// Reset the controller and bring the SD clock up to the
    /// identification frequency.
    fn reset_and_clock(&mut self) -> Result<(), DriverError> {
        self.host
            .write32(regs::REG_CONTROL1, regs::CONTROL1_SRST_HC)?;
        self.wait_clear(regs::REG_CONTROL1, regs::CONTROL1_SRST_HC)?;

        // Power the card rail before clocking it. The full host-controller
        // reset above clears SD Bus Power, and the standard register block
        // gates all command/data activity on it, so without this write the
        // very first command (`CMD0`) never completes — the bus is dark.
        // 3.3 V is the EMMC2-fed card supply (Linux's Pi 4 EMMC2 brings the
        // power register up to the same `0x0F`).
        self.host.write32(
            regs::REG_CONTROL0,
            regs::CONTROL0_BUS_VOLTAGE_3V3 | regs::CONTROL0_BUS_POWER,
        )?;

        let control1 = (IDENT_CLOCK_DIVISOR << regs::CONTROL1_CLK_FREQ_SHIFT)
            | (DATA_TIMEOUT_VALUE << regs::CONTROL1_TIMEOUT_SHIFT)
            | regs::CONTROL1_CLK_INTLEN;
        self.host.write32(regs::REG_CONTROL1, control1)?;
        self.wait_set(regs::REG_CONTROL1, regs::CONTROL1_CLK_STABLE)?;

        let with_sd_clock = self.host.read32(regs::REG_CONTROL1)? | regs::CONTROL1_CLK_EN;
        self.host.write32(regs::REG_CONTROL1, with_sd_clock)?;

        // Latch every status bit in the status-enable register so the
        // engine can read them back, and enable the completion sources the
        // engine parks on (plus every error bit) in the signal-enable
        // register so the controller raises its CPU interrupt line on each
        // completion — the engine waits on the interrupt rather than
        // busy-spinning. The shared GIC line is
        // routed + unmasked, and the parked task woken, by the kernel-side
        // [`CompletionWait`] the metal host carries.
        self.host.write32(regs::REG_IRPT_MASK, regs::INT_ALL)?;
        self.host
            .write32(regs::REG_IRPT_EN, regs::INT_SIGNAL_ENABLE)?;
        Ok(())
    }

    /// Issue `cmd` with `arg` and `transfer_mode`, returning the response
    /// words (`RESP0..3`; only `RESP0` is meaningful for short
    /// responses).
    fn issue(
        &mut self,
        cmd: SdCommand,
        arg: u32,
        transfer_mode: u32,
    ) -> Result<[u32; 4], DriverError> {
        let mut inhibit = regs::STATUS_CMD_INHIBIT;
        if cmd.transfers_data || cmd.response == ResponseKind::ShortBusy {
            inhibit |= regs::STATUS_DAT_INHIBIT;
        }
        self.wait_clear(regs::REG_STATUS, inhibit)?;
        self.host.write32(regs::REG_INTERRUPT, regs::INT_ALL)?;
        self.host.write32(regs::REG_ARG1, arg)?;
        self.host
            .write32(regs::REG_CMDTM, transfer_mode | cmd.cmd_word())?;
        self.wait_interrupt(regs::INT_CMD_DONE)?;

        let r0 = self.host.read32(regs::REG_RESP0)?;
        if cmd.response == ResponseKind::Long {
            Ok([
                r0,
                self.host.read32(regs::REG_RESP1)?,
                self.host.read32(regs::REG_RESP2)?,
                self.host.read32(regs::REG_RESP3)?,
            ])
        } else {
            Ok([r0, 0, 0, 0])
        }
    }

    /// Issue an application command: `CMD55` (`APP_CMD`) addressed to the
    /// card at `rca`, then `acmd`.
    fn issue_app(&mut self, acmd: SdCommand, arg: u32, rca: u32) -> Result<[u32; 4], DriverError> {
        self.issue(command::APP_CMD, rca, 0)?;
        self.issue(acmd, arg, 0)
    }

    /// Switch the selected card and the controller to the 4-bit DAT bus.
    ///
    /// `ACMD6` puts the card on its four DAT lines, then the controller's
    /// [`regs::CONTROL0_DATA_WIDTH_4BIT`] bit is set so the host drives the
    /// same width — a 4× transfer-rate gain over the 1-bit reset default.
    /// The controller bit is set with a read-modify-write so the SD-bus
    /// power and voltage bits the same register holds are preserved.
    fn set_bus_width_4bit(&mut self) -> Result<(), DriverError> {
        self.issue_app(
            command::SET_BUS_WIDTH,
            command::BUS_WIDTH_4BIT_ARG,
            self.rca,
        )?;
        let control0 = self.host.read32(regs::REG_CONTROL0)?;
        self.host.write32(
            regs::REG_CONTROL0,
            control0 | regs::CONTROL0_DATA_WIDTH_4BIT,
        )?;
        Ok(())
    }

    /// Raise the SD clock from the identification rate to the data rate
    /// ([`DATA_CLOCK_DIVISOR`]) now that the card is identified and
    /// selected.
    ///
    /// Follows the SDHCI clock-change sequence: stop `SDCLK` (clear
    /// [`regs::CONTROL1_CLK_EN`]) before reprogramming the frequency-select
    /// divisor — changing it while the clock runs is undefined — re-arm the
    /// internal clock and wait for [`regs::CONTROL1_CLK_STABLE`], then
    /// re-enable `SDCLK` at the new frequency. The timeout field is kept at
    /// the bring-up value.
    fn raise_data_clock(&mut self) -> Result<(), DriverError> {
        let running = self.host.read32(regs::REG_CONTROL1)?;
        self.host
            .write32(regs::REG_CONTROL1, running & !regs::CONTROL1_CLK_EN)?;

        let control1 = (DATA_CLOCK_DIVISOR << regs::CONTROL1_CLK_FREQ_SHIFT)
            | (DATA_TIMEOUT_VALUE << regs::CONTROL1_TIMEOUT_SHIFT)
            | regs::CONTROL1_CLK_INTLEN;
        self.host.write32(regs::REG_CONTROL1, control1)?;
        self.wait_set(regs::REG_CONTROL1, regs::CONTROL1_CLK_STABLE)?;

        let with_sd_clock = self.host.read32(regs::REG_CONTROL1)? | regs::CONTROL1_CLK_EN;
        self.host.write32(regs::REG_CONTROL1, with_sd_clock)?;
        Ok(())
    }

    /// Run the SD identification sequence and derive the geometry.
    ///
    /// Each fallible step tags its [`DriverError`] with the
    /// [`BringUpStage`] it failed at, so a metal caller logs the exact SD
    /// command that stalled (`plans/PI.md` P8/B4).
    fn init(&mut self) -> Result<(), BringUpFault> {
        self.reset_and_clock()
            .map_err(|e| BringUpFault::new(BringUpStage::ResetClock, e))?;

        self.issue(command::GO_IDLE_STATE, 0, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::GoIdle, e))?;

        let if_cond = self
            .issue(command::SEND_IF_COND, command::IF_COND_ARG, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::SendIfCond, e))?;
        if if_cond[0] & 0xFF != command::IF_COND_CHECK_PATTERN {
            // The card did not echo the check pattern: not a v2 card or a
            // voltage mismatch. Fail closed.
            return Err(BringUpFault::new(
                BringUpStage::SendIfCond,
                DriverError::Unsupported,
            ));
        }

        // Poll ACMD41 until the card finishes power-up. The poll budget
        // bounds the wait so an absent or wedged card fails closed rather
        // than spinning forever.
        let mut powered_up = false;
        for _ in 0..self.poll_budget {
            let ocr = self
                .issue_app(command::SD_SEND_OP_COND, command::OP_COND_ARG, 0)
                .map_err(|e| BringUpFault::new(BringUpStage::OpCond, e))?;
            if ocr[0] & command::OCR_READY != 0 {
                if ocr[0] & command::OCR_CCS == 0 {
                    // A byte-addressed standard-capacity card. The read
                    // path is block-addressed; reject rather than
                    // mis-address (`plans/PI.md` P8).
                    return Err(BringUpFault::new(
                        BringUpStage::OpCond,
                        DriverError::Unsupported,
                    ));
                }
                powered_up = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !powered_up {
            return Err(BringUpFault::new(
                BringUpStage::OpCond,
                DriverError::DeviceFault,
            ));
        }

        self.issue(command::ALL_SEND_CID, 0, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::AllSendCid, e))?;
        let rca = self
            .issue(command::SEND_RELATIVE_ADDR, 0, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::SendRelativeAddr, e))?;
        // RCA occupies bits [31:16] of the R6 response and is reused, in
        // the same position, as the argument of every addressed command.
        self.rca = rca[0] & 0xFFFF_0000;

        let csd = self
            .issue(command::SEND_CSD, self.rca, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::SendCsd, e))?;
        self.geometry = command::geometry_from_csd(csd)
            .map_err(|e| BringUpFault::new(BringUpStage::SendCsd, e))?;

        self.issue(command::SELECT_CARD, self.rca, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::SelectCard, e))?;
        self.issue(command::SET_BLOCKLEN, BLOCK_SIZE, 0)
            .map_err(|e| BringUpFault::new(BringUpStage::SetBlockLen, e))?;

        // The card is now selected in the transfer state at the slow
        // identification clock on the 1-bit bus. Widen the bus to 4-bit and
        // raise the clock to the data rate before any block transfer — both
        // are pure speed steps the read/write path then inherits, turning
        // the ~50 KB/s identification-clock 1-bit path into the ~6 MB/s
        // Default-Speed 4-bit path. Bus width is widened
        // first so the clock change (and every later transfer) runs on the
        // final width.
        self.set_bus_width_4bit()
            .map_err(|e| BringUpFault::new(BringUpStage::SetBusWidth, e))?;
        self.raise_data_clock()
            .map_err(|e| BringUpFault::new(BringUpStage::RaiseClock, e))?;

        // Select the fast path: move transfers by ADMA2 when the host
        // granted a staging region that is (a) at least DMA_REGION_BYTES
        // long and (b) wholly addressable by the 32-bit ADMA2 descriptor
        // fields. Otherwise stay on programmed I/O — DMA where possible,
        // correct everywhere. This 32-bit-addressability precondition is
        // checked once here so the transfer path never has to fail a
        // request on an out-of-range device address. Enabling ADMA2 is a
        // pure controller-mode step the read/write path then inherits, so
        // it runs once here rather than per transfer.
        self.dma_stage_blocks = match self.host.dma_region() {
            Some(region)
                if region.bytes.len() >= DMA_REGION_BYTES
                    && region
                        .device_base
                        .checked_add(DMA_REGION_BYTES as u64)
                        .is_some_and(|end| u32::try_from(end).is_ok()) =>
            {
                DMA_STAGE_BLOCKS
            }
            _ => 0,
        };
        if self.dma_stage_blocks != 0 {
            self.enable_adma2()
                .map_err(|e| BringUpFault::new(BringUpStage::RaiseClock, e))?;
        }
        Ok(())
    }

    /// Select 32-bit ADMA2 as the controller's DMA mode.
    ///
    /// A read-modify-write of `CONTROL0` (SDHCI Host Control 1) that clears
    /// the DMA-select field and sets the ADMA2 value, preserving the SD-bus
    /// power/voltage and 4-bit-width bits the same register holds. Called
    /// once at the end of bring-up when a DMA staging region is present.
    fn enable_adma2(&mut self) -> Result<(), DriverError> {
        let control0 = self.host.read32(regs::REG_CONTROL0)?;
        let control0 =
            (control0 & !regs::CONTROL0_DMA_SELECT_MASK) | regs::CONTROL0_DMA_SELECT_ADMA2;
        self.host.write32(regs::REG_CONTROL0, control0)?;
        Ok(())
    }

    /// Validate a block-transfer request (read or write) against the
    /// geometry, returning the 32-bit block address and 16-bit block
    /// count the controller takes.
    fn validate_transfer(&self, lba: u64, buf_len: usize) -> Result<(u32, u16), DriverError> {
        let bs = BLOCK_SIZE as usize;
        if buf_len == 0 || buf_len % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = buf_len / bs;
        if blocks > MAX_BLOCKS_PER_TRANSFER {
            return Err(DriverError::LengthOutOfRange);
        }
        let end = lba
            .checked_add(blocks as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.geometry.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        // SDHC/SDXC are block-addressed with a 32-bit block number.
        let block_addr = u32::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)?;
        let block_count = u16::try_from(blocks).map_err(|_| DriverError::LengthOutOfRange)?;
        Ok((block_addr, block_count))
    }

    /// Drain one 512-byte block from the buffer data port into `block`.
    fn read_block_pio(&mut self, block: &mut [u8]) -> Result<(), DriverError> {
        self.wait_interrupt(regs::INT_READ_RDY)?;
        for word in 0..BLOCK_WORDS {
            let value = self.host.read32(regs::REG_DATA)?;
            block[word * 4..word * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        Ok(())
    }

    /// Push one 512-byte block from `block` into the buffer data port.
    fn write_block_pio(&mut self, block: &[u8]) -> Result<(), DriverError> {
        self.wait_interrupt(regs::INT_WRITE_RDY)?;
        for word in 0..BLOCK_WORDS {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&block[word * 4..word * 4 + 4]);
            self.host
                .write32(regs::REG_DATA, u32::from_le_bytes(bytes))?;
        }
        Ok(())
    }

    /// The read command and `CMDTM` transfer-mode word for a `multi`-block
    /// (vs single-block) read, on the `dma` (ADMA2) or programmed-I/O path.
    ///
    /// One definition shared by both transfer paths so the direction /
    /// block-count / auto-CMD12 / DMA-enable bits cannot drift between
    /// them.
    fn read_command(multi: bool, dma: bool) -> (SdCommand, u32) {
        let dma_bit = if dma { regs::TM_DMA_EN } else { 0 };
        if multi {
            (
                command::READ_MULTIPLE_BLOCK,
                regs::TM_DAT_DIR_READ
                    | regs::TM_BLKCNT_EN
                    | regs::TM_MULTI_BLOCK
                    | regs::TM_AUTO_CMD12
                    | dma_bit,
            )
        } else {
            (command::READ_SINGLE_BLOCK, regs::TM_DAT_DIR_READ | dma_bit)
        }
    }

    /// The write command and `CMDTM` transfer-mode word for a `multi`-block
    /// (vs single-block) write, on the `dma` (ADMA2) or programmed-I/O
    /// path. Host-to-card is the cleared direction bit.
    fn write_command(multi: bool, dma: bool) -> (SdCommand, u32) {
        let dma_bit = if dma { regs::TM_DMA_EN } else { 0 };
        if multi {
            (
                command::WRITE_MULTIPLE_BLOCK,
                regs::TM_BLKCNT_EN | regs::TM_MULTI_BLOCK | regs::TM_AUTO_CMD12 | dma_bit,
            )
        } else {
            (command::WRITE_BLOCK, dma_bit)
        }
    }

    /// Read `buf` (a whole number of blocks) from the card starting at
    /// `block_addr` over the programmed-I/O buffer data port.
    fn read_blocks_pio(&mut self, block_addr: u32, buf: &mut [u8]) -> Result<(), DriverError> {
        let block_count = u32::try_from(buf.len() / BLOCK_SIZE as usize)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        self.host
            .write32(regs::REG_BLKSIZECNT, (block_count << 16) | BLOCK_SIZE)?;
        let (cmd, transfer_mode) = Self::read_command(block_count != 1, false);
        self.issue(cmd, block_addr, transfer_mode)?;
        for block in buf.chunks_mut(BLOCK_SIZE as usize) {
            self.read_block_pio(block)?;
        }
        self.wait_interrupt(regs::INT_DATA_DONE)?;
        Ok(())
    }

    /// Write `buf` (a whole number of blocks) to the card starting at
    /// `block_addr` over the programmed-I/O buffer data port.
    fn write_blocks_pio(&mut self, block_addr: u32, buf: &[u8]) -> Result<(), DriverError> {
        let block_count = u32::try_from(buf.len() / BLOCK_SIZE as usize)
            .map_err(|_| DriverError::LengthOutOfRange)?;
        self.host
            .write32(regs::REG_BLKSIZECNT, (block_count << 16) | BLOCK_SIZE)?;
        let (cmd, transfer_mode) = Self::write_command(block_count != 1, false);
        self.issue(cmd, block_addr, transfer_mode)?;
        for block in buf.chunks(BLOCK_SIZE as usize) {
            self.write_block_pio(block)?;
        }
        self.wait_interrupt(regs::INT_DATA_DONE)?;
        Ok(())
    }

    /// Stage the one-entry ADMA2 descriptor covering the first `chunk_bytes`
    /// of the data-staging area into the device-shared region, returning the
    /// device-visible base of the descriptor table.
    ///
    /// The data buffer is at region offset 0 and the descriptor table at
    /// [`DMA_DESC_OFFSET`] (both fit the region — `init` proved it is at
    /// least [`DMA_REGION_BYTES`]). Fails closed if the region is gone or a
    /// device address does not fit the 32-bit ADMA2 field.
    fn dma_stage_descriptor(&mut self, chunk_bytes: usize) -> Result<u32, DriverError> {
        let region = self.host.dma_region().ok_or(DriverError::DeviceFault)?;
        let data_dev = u32::try_from(region.device_base).map_err(|_| DriverError::DeviceFault)?;
        let desc = adma::encode_tran(data_dev, chunk_bytes);
        region.bytes[DMA_DESC_OFFSET..DMA_DESC_OFFSET + adma::DESC_BYTES].copy_from_slice(&desc);
        region
            .device_base
            .checked_add(DMA_DESC_OFFSET as u64)
            .and_then(|a| u32::try_from(a).ok())
            .ok_or(DriverError::DeviceFault)
    }

    /// Issue one staged ADMA2 chunk and wait for its single completion.
    ///
    /// The caller has already staged the descriptor (and, for a write, the
    /// outbound data) into the region. This orders those Normal-Non-
    /// Cacheable stores ahead of the MMIO doorbell with [`dma_wmb`],
    /// programs the block count and descriptor-table base, issues the
    /// `read`/write command, and parks on the **single** transfer-complete
    /// interrupt — the controller masters every block over the DAT lines,
    /// so there is no per-block buffer handshake. One definition shared by
    /// the read and write chunk loops.
    fn dma_issue_chunk(
        &mut self,
        lba: u32,
        chunk_blocks: u32,
        chunk_bytes: usize,
        table_dev: u32,
        write: bool,
    ) -> Result<(), DriverError> {
        self.host.sync_dma_range(0, chunk_bytes);
        self.host.sync_dma_range(DMA_DESC_OFFSET, adma::DESC_BYTES);
        dma_wmb();
        self.host
            .write32(regs::REG_BLKSIZECNT, (chunk_blocks << 16) | BLOCK_SIZE)?;
        self.host.write32(regs::REG_ADMA_ADDR, table_dev)?;
        let multi = chunk_blocks != 1;
        let (cmd, transfer_mode) = if write {
            Self::write_command(multi, true)
        } else {
            Self::read_command(multi, true)
        };
        self.issue(cmd, lba, transfer_mode)?;
        self.wait_interrupt(regs::INT_DATA_DONE)
    }

    /// Read `buf` (a whole number of blocks) from the card starting at
    /// `block_addr` by ADMA2, one [`DMA_STAGE_BLOCKS`]-block chunk per
    /// command.
    ///
    /// After each chunk's completion the engine orders its loads with
    /// [`dma_rmb`] and copies the device-written staging bytes into `buf`.
    fn read_blocks_dma(&mut self, block_addr: u32, buf: &mut [u8]) -> Result<(), DriverError> {
        let stage_bytes = self.dma_stage_blocks * BLOCK_SIZE as usize;
        let mut lba = block_addr;
        let mut offset = 0usize;
        while offset < buf.len() {
            let chunk_bytes = (buf.len() - offset).min(stage_bytes);
            let chunk_blocks = u32::try_from(chunk_bytes / BLOCK_SIZE as usize)
                .map_err(|_| DriverError::LengthOutOfRange)?;
            let table_dev = self.dma_stage_descriptor(chunk_bytes)?;
            self.dma_issue_chunk(lba, chunk_blocks, chunk_bytes, table_dev, false)?;
            // Order the reads of device-written data after the completion,
            // discard any stale cacheable copy through the host coherency
            // seam, then copy the staged bytes out.
            dma_rmb();
            self.host.sync_dma_range(0, chunk_bytes);
            let region = self.host.dma_region().ok_or(DriverError::DeviceFault)?;
            buf[offset..offset + chunk_bytes].copy_from_slice(&region.bytes[..chunk_bytes]);
            offset += chunk_bytes;
            lba = lba
                .checked_add(chunk_blocks)
                .ok_or(DriverError::LengthOutOfRange)?;
        }
        Ok(())
    }

    /// Write `buf` (a whole number of blocks) to the card starting at
    /// `block_addr` by ADMA2, one [`DMA_STAGE_BLOCKS`]-block chunk per
    /// command.
    ///
    /// The engine copies each chunk of `buf` into the staging region before
    /// issuing the command, so the caller's buffer is never mutated (the
    /// `Block` write contract).
    fn write_blocks_dma(&mut self, block_addr: u32, buf: &[u8]) -> Result<(), DriverError> {
        let stage_bytes = self.dma_stage_blocks * BLOCK_SIZE as usize;
        let mut lba = block_addr;
        let mut offset = 0usize;
        while offset < buf.len() {
            let chunk_bytes = (buf.len() - offset).min(stage_bytes);
            let chunk_blocks = u32::try_from(chunk_bytes / BLOCK_SIZE as usize)
                .map_err(|_| DriverError::LengthOutOfRange)?;
            let table_dev = self.dma_stage_descriptor(chunk_bytes)?;
            {
                // Stage the outbound data after the descriptor; the shared
                // issue step's `dma_wmb` orders both ahead of the doorbell.
                let region = self.host.dma_region().ok_or(DriverError::DeviceFault)?;
                region.bytes[..chunk_bytes].copy_from_slice(&buf[offset..offset + chunk_bytes]);
            }
            self.dma_issue_chunk(lba, chunk_blocks, chunk_bytes, table_dev, true)?;
            offset += chunk_bytes;
            lba = lba
                .checked_add(chunk_blocks)
                .ok_or(DriverError::LengthOutOfRange)?;
        }
        Ok(())
    }
}

impl<H: SdhciHost> Block for Emmc2<H> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (block_addr, _block_count) = self.validate_transfer(lba, buf.len())?;
        if self.dma_stage_blocks != 0 {
            self.read_blocks_dma(block_addr, buf)
        } else {
            self.read_blocks_pio(block_addr, buf)
        }
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (block_addr, _block_count) = self.validate_transfer(lba, buf.len())?;
        if self.dma_stage_blocks != 0 {
            self.write_blocks_dma(block_addr, buf)
        } else {
            self.write_blocks_pio(block_addr, buf)
        }
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // This driver never enables the eMMC/SD device write cache (the
        // EXT_CSD `CACHE_CTRL` feature stays off), so the card operates
        // write-through: a completed write has finished programming to the
        // medium before its command returns. There is therefore no
        // volatile cache to commit, and flush is a genuine no-op — not a
        // swallowed forward. Enabling the device cache would make this a
        // real `FLUSH_CACHE` command.
        Ok(())
    }
}
