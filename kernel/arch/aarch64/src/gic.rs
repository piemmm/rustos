//! GICv2 (ARM Generic Interrupt Controller) driver, with the
//! distributor / CPU-interface MMIO bases **discovered from the device
//! tree** (`plans/PI.md` P3).
//!
//! The aarch64 port boots on two boards whose GICv2 lives at different
//! addresses: the QEMU `virt` board ([`DEFAULT_GICD_BASE`] /
//! [`DEFAULT_GICC_BASE`]) and the Raspberry Pi 4's GIC-400
//! (`0xFF84_1000` / `0xFF84_2000`). Per the
//! difference is **discovered device-tree data**, never a `cfg(board)`
//! fork: this module holds the runtime base pair (an atomic, default =
//! the `virt` GICv2) the freestanding MMIO accessor reads, plus the
//! [`find_gic`] / [`configure_from_fdt`] discovery the boot path runs
//! over the firmware tree. The register layout is identical across the
//! two (GIC-400 *is* a GICv2), so there is one driver, two discovered
//! bases.
//!
//! This module owns the minimum surface the aarch64
//! Stage-3 primitives need:
//!
//! * `init` — enable the distributor and this CPU's interface, open the
//!   priority mask, and route interrupts to the IRQ exception.
//! * `enable_ppi` — enable one private-peripheral interrupt (the EL1
//!   physical-timer PPI, [`crate::preempt::TIMER_PPI`], is the one the
//!   timer path arms).
//! * `acknowledge` / `end_of_interrupt` — the IAR/EOIR handshake the
//!   IRQ handler runs per interrupt.
//! * `send_sgi` — raise a software-generated interrupt on a target CPU
//!   (the GICv2 mechanism behind [`crate::kernel_arch::Aarch64Arch`]'s
//!   `send_ipi`).
//!
//! # Interrupt-id classes (GICv2 spec §2.2.1)
//!
//! INTIDs `0..16` are SGIs (inter-processor), `16..32` are PPIs
//! (per-CPU peripherals — the timers live here), and `32..1020` are SPIs
//! (shared peripherals). Enabling and priority for SGIs/PPIs is
//! per-CPU through the banked first `GICD_ISENABLER` / `GICD_IPRIORITYR`
//! registers.
//!
//! # Host testability
//!
//! The register-offset math and the `GICD_SGIR` word encoding are pure
//! functions, unit-tested on the host; the MMIO reads/writes are gated
//! to the freestanding aarch64 target.

use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_arch_api::{CpuId, StuckInterrupt};
use tairix_fdt::Fdt;

/// MMIO base the distributor points at before any discovery runs: the
/// QEMU `virt` board's GICv2 distributor. A board with a different GIC
/// (the Pi 4's GIC-400) replaces this at boot from its device tree
/// ([`configure_from_fdt`]); the default is the `virt` value, never a
/// fabricated per-board constant (`plans/PI.md` §3 — no fresh
/// `PI_*_BASE`).
pub const DEFAULT_GICD_BASE: usize = 0x0800_0000;

/// MMIO base the CPU interface points at before any discovery runs: the
/// QEMU `virt` board's GICv2 CPU interface. See [`DEFAULT_GICD_BASE`].
pub const DEFAULT_GICC_BASE: usize = 0x0801_0000;

/// Currently-selected GICv2 distributor MMIO base. Defaults to the
/// `virt` board; overwritten by [`configure`] / [`configure_from_fdt`]
/// once discovery resolves the board's GIC.
static GICD_BASE: AtomicUsize = AtomicUsize::new(DEFAULT_GICD_BASE);
/// Currently-selected GICv2 CPU-interface MMIO base. Defaults to the
/// `virt` board.
static GICC_BASE: AtomicUsize = AtomicUsize::new(DEFAULT_GICC_BASE);

/// Point the GICv2 driver at the distributor base `gicd` and CPU-
/// interface base `gicc`.
///
/// Called once early in a board's boot path (after device-tree discovery
/// resolves the GIC, before `init`). `Release`/`Acquire` ordering pairs
/// these stores with [`current`]'s loads so the freestanding MMIO path —
/// and any secondary CPU that brings up its interface afterwards —
/// observes a consistent `(gicd, gicc)` pair.
pub fn configure(distributor: usize, cpu_iface: usize) {
    GICD_BASE.store(distributor, Ordering::Release);
    GICC_BASE.store(cpu_iface, Ordering::Release);
}

/// The GICv2 distributor and CPU-interface MMIO bases currently in
/// effect, as `(gicd, gicc)`.
#[must_use]
pub fn current() -> (usize, usize) {
    (
        GICD_BASE.load(Ordering::Acquire),
        GICC_BASE.load(Ordering::Acquire),
    )
}

/// `compatible` strings that name a GICv2-class interrupt controller this
/// driver speaks. GIC-400 (`arm,gic-400`) is a GICv2, so it shares the
/// register layout; the QEMU `virt` board advertises `arm,cortex-a15-gic`.
/// An unrecognised controller is not matched, so the boot path keeps the
/// fail-safe default rather than driving an unknown layout.
const GIC_COMPATIBLES: &[&[u8]] = &[
    b"arm,gic-400",
    b"arm,cortex-a15-gic",
    b"arm,cortex-a7-gic",
    b"arm,cortex-a9-gic",
    b"arm,gic-v2",
];

/// `true` if `compatible` names a GICv2-class controller this driver can
/// drive (one of the recognised GICv2 `compatible` strings).
#[must_use]
pub fn is_gic_compatible(compatible: &[u8]) -> bool {
    GIC_COMPATIBLES.contains(&compatible)
}

/// A GICv2 distributor + CPU-interface pair located in a flattened
/// device tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredGic<'a> {
    /// The `compatible` string that selected this controller (one of the
    /// recognised GICv2 `compatible` strings); borrowed from the
    /// device-tree blob. Used as the hardware-tree node's bind key
    /// (`crate::platform`).
    pub compatible: &'a [u8],
    /// CPU-physical MMIO base of the distributor: the GIC node's first
    /// `reg` region, decoded with its parent bus's cell counts and
    /// translated through the ancestor buses' `ranges`
    /// ([`crate::fdt::translated_reg`]).
    pub gicd_base: u64,
    /// CPU-physical MMIO base of the CPU interface (the node's second
    /// `reg` region, decoded and translated likewise).
    pub gicc_base: u64,
}

/// Locate the GICv2 interrupt controller in `fdt`.
///
/// Walks the tree for the first node whose `compatible` names a
/// GICv2-class controller ([`is_gic_compatible`]) and reads its `reg`:
/// region 0 is the distributor, region 1 the CPU interface. Each region
/// is decoded with the node's parent bus's cell counts and translated
/// through the ancestor buses' `ranges`
/// ([`crate::fdt::translated_reg`]) — on the QEMU `virt` board the GIC
/// sits at the root with 2+2 cells, while the Pi 4's GIC-400 sits under
/// `/soc` with one-cell *bus* `reg` values (`0x4004_1000`) remapped to
/// the CPU-physical bases (`0xFF84_1000`). The walk early-returns at
/// the matched controller, so it stays safe with the MMU off
/// ([`crate::fdt::scan_translated`]). Returns `None` if the tree is
/// malformed or carries no recognised, translatable GIC (the caller
/// then keeps the [`DEFAULT_GICD_BASE`] / [`DEFAULT_GICC_BASE`] default
/// — fail closed).
#[must_use]
pub fn find_gic<'a>(fdt: &Fdt<'a>) -> Option<DiscoveredGic<'a>> {
    crate::fdt::scan_translated(fdt, |node, levels, depth| {
        let matched = node
            .property("compatible")?
            .iter_strings()
            .find(|s| is_gic_compatible(s))?;
        let (distributor, _) = crate::fdt::translated_reg(node, depth, levels, 0)?;
        let (cpu_iface, _) = crate::fdt::translated_reg(node, depth, levels, 1)?;
        Some(DiscoveredGic {
            compatible: matched,
            gicd_base: distributor,
            gicc_base: cpu_iface,
        })
    })
}

/// Discover the GIC in `fdt` and point the driver at it.
///
/// Returns the [`DiscoveredGic`] that was applied, or `None` (leaving the
/// previous configuration untouched) when the tree carries no recognised
/// GIC or a base does not fit a `usize`.
#[must_use]
pub fn configure_from_fdt<'a>(fdt: &Fdt<'a>) -> Option<DiscoveredGic<'a>> {
    let found = find_gic(fdt)?;
    // A device MMIO base always fits a `usize` on the 64-bit targets this
    // port serves; `try_from` keeps the conversion honest — fail closed
    // rather than truncate.
    let distributor = usize::try_from(found.gicd_base).ok()?;
    let cpu_iface = usize::try_from(found.gicc_base).ok()?;
    configure(distributor, cpu_iface);
    Some(found)
}

/// `GICD_CTLR` — distributor control (offset 0x000). Bit 0 enables
/// forwarding of pending interrupts to the CPU interfaces.
const GICD_CTLR: usize = 0x000;
/// `GICD_ISENABLER<n>` — set-enable, one bit per interrupt (base 0x100).
const GICD_ISENABLER: usize = 0x100;
/// `GICD_ICENABLER<n>` — clear-enable, one bit per interrupt (base
/// 0x180). Writing a `1` disables (masks) the corresponding interrupt.
const GICD_ICENABLER: usize = 0x180;
/// `GICD_ISPENDR<n>` — set-pending status, one bit per interrupt (base
/// 0x200). A set bit means the interrupt is pending delivery.
const GICD_ISPENDR: usize = 0x200;
/// `GICD_ISACTIVER<n>` — set-active status, one bit per interrupt (base
/// 0x300). A set bit means the interrupt has been acknowledged but not
/// yet completed (its handler is in flight); a bit stuck set is the
/// signature of a line whose handler never returns or that re-fires
/// faster than it is serviced (an interrupt storm).
const GICD_ISACTIVER: usize = 0x300;
/// `GICD_IPRIORITYR<n>` — priority, one byte per interrupt (base 0x400).
const GICD_IPRIORITYR: usize = 0x400;
/// `GICD_ITARGETSR<n>` — interrupt processor targets, one byte per
/// interrupt (base 0x800). The byte is a CPU-interface bitmask: bit `c`
/// routes the interrupt to CPU `c`. The first 32 bytes (SGIs/PPIs) are
/// read-only and banked per CPU; only the SPI bytes (INTID `>= 32`) are
/// writable, which is why [`Gicv2::route_spi`] is the SPI-only routing
/// primitive (GICv2 spec §4.3.12).
const GICD_ITARGETSR: usize = 0x800;
/// `GICD_SGIR` — software-generated interrupt control (offset 0xF00).
const GICD_SGIR: usize = 0xF00;

/// `GICC_CTLR` — CPU-interface control (offset 0x000). Bit 0 enables
/// signalling of interrupts to the CPU.
const GICC_CTLR: usize = 0x000;
/// `GICC_PMR` — interrupt priority mask (offset 0x004). Only interrupts
/// of higher priority (numerically lower) than this are signalled.
const GICC_PMR: usize = 0x004;
/// `GICC_IAR` — interrupt acknowledge (offset 0x00C). A read returns the
/// INTID of the highest-priority pending interrupt and activates it.
const GICC_IAR: usize = 0x00C;
/// `GICC_EOIR` — end of interrupt (offset 0x010). Writing the INTID read
/// from `GICC_IAR` deactivates it.
const GICC_EOIR: usize = 0x010;

/// Highest addressable GICv2 INTID. INTIDs `1020..=1023` are reserved by
/// the spec (1023 is [`SPURIOUS_INTID`]); a controller rejects anything
/// above this as out of range (fail closed).
pub const MAX_INTID: u32 = 1019;

/// Mask isolating the INTID from a `GICC_IAR` read (bits `[9:0]`; the
/// upper bits carry the source CPU for SGIs).
pub const IAR_INTID_MASK: u32 = 0x3FF;

/// Spurious-interrupt INTID the CPU interface returns from `GICC_IAR`
/// when no interrupt is pending (GICv2 spec §3.2.4). The handler must
/// not `EOI` it.
pub const SPURIOUS_INTID: u32 = 1023;

/// Word written to `GICD_SGIR` to raise SGI `intid` on the CPUs named in
/// `target_list` (one bit per CPU, bits `[23:16]`), with target-list
/// filter `0b00` ("forward to the listed CPUs").
#[must_use]
pub const fn sgir_value(intid: u32, target_list: u8) -> u32 {
    ((target_list as u32) << 16) | (intid & 0xF)
}

/// Byte offset of the 32-bit status word covering interrupt `intid`
/// within a one-bit-per-interrupt register bank at `base` (the shared
/// layout of `GICD_ISENABLER`/`ICENABLER`/`ISPENDR`/`ISACTIVER`).
#[must_use]
const fn gicd_bit_word_offset(base: usize, intid: u32) -> usize {
    base + ((intid / 32) as usize) * 4
}

/// Byte offset of the `GICD_ISENABLER` word covering interrupt `intid`.
#[must_use]
pub const fn isenabler_offset(intid: u32) -> usize {
    gicd_bit_word_offset(GICD_ISENABLER, intid)
}

/// Bit position within the `GICD_ISENABLER` word for interrupt `intid`.
#[must_use]
pub const fn isenabler_bit(intid: u32) -> u32 {
    1 << (intid % 32)
}

/// Lowest GICv2 INTID that is a shared peripheral interrupt (SPI). INTIDs
/// `0..32` are SGIs/PPIs whose `GICD_ITARGETSR` bytes are read-only and
/// banked per CPU; only `>= MIN_SPI_INTID` may be routed with
/// [`Gicv2::route_spi`] (GICv2 spec §2.2.1).
pub const MIN_SPI_INTID: u32 = 32;

/// Byte offset of the `GICD_ITARGETSR` register for interrupt `intid`
/// (one byte per INTID).
#[must_use]
pub const fn itargetsr_offset(intid: u32) -> usize {
    GICD_ITARGETSR + intid as usize
}

/// Byte offset of the `GICD_ICENABLER` word covering interrupt `intid`.
/// Parallel layout to [`isenabler_offset`]; the bit position is shared
/// ([`isenabler_bit`]).
#[must_use]
pub const fn icenabler_offset(intid: u32) -> usize {
    gicd_bit_word_offset(GICD_ICENABLER, intid)
}

/// Scan the SPI range `[MIN_SPI_INTID, max_intid]` for the lowest
/// interrupt id currently stuck in the distributor *and able to reach a
/// CPU*, reading each 32-bit status word through `read` (`read(off)`
/// returns the word at byte offset `off`).
///
/// **Only a deliverable line can wedge a CPU, so only a deliverable line
/// is reported.** A masked line cannot be signalled to any CPU, so an
/// asserted-but-masked line is contained noise that can never be the
/// cause of a lockup — reporting it blames an innocent line (the recurring
/// spurious `stuck_irq=111`). Two banks are therefore weighed:
/// - **active** (`GICD_ISACTIVER`): a handler is in flight (or the line is
///   re-firing faster than it is serviced). A line only becomes active by
///   being delivered, so an active line is a genuine suspect regardless of
///   its current mask, and this the stronger signature — it wins.
/// - **pending** (`GICD_ISPENDR`): merely asserted. Reported *only* when
///   the line is still **enabled** at `GICD_ISENABLER`; a masked-pending
///   line is skipped and the scan continues to the next set bit.
///
/// Within a bank the lowest qualifying id wins (`trailing_zeros` gives the
/// lowest set bit; masked bits are cleared and the search continues). Only
/// SPIs (`>= MIN_SPI_INTID`) are scanned: SGI/PPI status is banked per CPU,
/// so an observer reading it would see its *own* lines, not the wedged
/// CPU's.
///
/// Returns the lowest deliverable stuck SPI as a [`StuckInterrupt`] — its
/// id and whether it is `active` (a live storm) or merely `pending` — or
/// `None` when no SPI is active or enabled-and-pending. Pure over `read`,
/// so it is host-tested against the mock distributor without any MMIO.
#[must_use]
fn first_stuck_spi(max_intid: u32, read: impl Fn(usize) -> u32) -> Option<StuckInterrupt> {
    // Active first: a line with a handler in flight is the stronger
    // hard-lockup signal, and being active proves it was delivered.
    if let Some(intid) = first_matching_spi(GICD_ISACTIVER, max_intid, &read, |_| true) {
        return Some(StuckInterrupt {
            intid,
            active: true,
        });
    }
    // Pending only when still enabled: a masked-pending line cannot reach a
    // CPU, so it can never be the wedge — skip it rather than blame it.
    let enabled = |id: u32| read(isenabler_offset(id)) & isenabler_bit(id) != 0;
    if let Some(intid) = first_matching_spi(GICD_ISPENDR, max_intid, &read, enabled) {
        return Some(StuckInterrupt {
            intid,
            active: false,
        });
    }
    None
}

/// The lowest SPI in `[MIN_SPI_INTID, max_intid]` whose bit is set in the
/// one-bit-per-interrupt register bank at `base` **and** which `accept`
/// admits, reading each 32-bit word through `read`.
///
/// Iterates every set bit in ascending id order (clearing the lowest set
/// bit each step) so a rejected candidate — e.g. a masked-pending line —
/// does not stop the search; the next qualifying line is found instead. A
/// candidate beyond `max_intid` ends the scan (every higher bit and word
/// exceeds the range too). Pure over `read`, so it is host-tested.
fn first_matching_spi(
    base: usize,
    max_intid: u32,
    read: &impl Fn(usize) -> u32,
    accept: impl Fn(u32) -> bool,
) -> Option<u32> {
    let mut intid = MIN_SPI_INTID;
    while intid <= max_intid {
        let mut word = read(gicd_bit_word_offset(base, intid));
        while word != 0 {
            let candidate = intid + word.trailing_zeros();
            if candidate > max_intid {
                return None;
            }
            if accept(candidate) {
                return Some(candidate);
            }
            word &= word - 1;
        }
        intid += 32;
    }
    None
}

/// Volatile access to a GICv2 distributor + CPU-interface register pair.
///
/// The production implementation is `VolatileGicMmio` (freestanding
/// only); host tests substitute an in-memory mock. Modelled on riscv64's
/// `PlicMmio` seam so the whole controller control-flow is host-testable
/// (one MMIO path, no duplicate register logic).
pub trait GicMmio {
    /// Read the distributor register at byte offset `off`.
    fn gicd_read(&self, off: usize) -> u32;
    /// Write the distributor 32-bit register at byte offset `off`.
    fn gicd_write(&self, off: usize, val: u32);
    /// Write the distributor byte register at byte offset `off` (the
    /// byte-addressable priority registers).
    fn gicd_write_byte(&self, off: usize, val: u8);
    /// Read the CPU-interface register at byte offset `off`.
    fn gicc_read(&self, off: usize) -> u32;
    /// Write the CPU-interface register at byte offset `off`.
    fn gicc_write(&self, off: usize, val: u32);
    /// Publish every store this CPU has issued so far to the
    /// inner-shareable domain, so another PE observes them before it acts
    /// on a subsequently-raised interrupt.
    ///
    /// [`Gicv2::send_sgi`] calls this immediately before the `GICD_SGIR`
    /// write. Raising a reschedule IPI is a cross-CPU hand-off: the waker
    /// enqueues the woken task (a normal-memory store) and *then* signals
    /// the target. On a weakly-ordered PE the enqueue is not guaranteed
    /// observable to the target before the target takes the SGI, so
    /// without this barrier the target's next dispatch can read an empty
    /// run queue and re-park — stranding the woken task (a lost wake-up
    /// that hangs the system). The barrier is the store analogue of the
    /// mask-before-wake fence [`GicController`]'s `mask` already issues:
    /// on the freestanding target it is a `dsb ishst`; the host mock
    /// records it so the publish-before-signal ordering is unit-tested.
    fn publish_barrier(&self);
}

/// Low-level GICv2 register driver over a [`GicMmio`] seam.
///
/// Holds no policy: it exposes the raw enable/disable/priority/ack/EOI
/// operations and leaves range validation and the mask-before-wake fence
/// to [`GicController`].
pub struct Gicv2<M: GicMmio> {
    mmio: M,
}

impl<M: GicMmio> Gicv2<M> {
    /// Bind a driver to `mmio`.
    pub const fn new(mmio: M) -> Self {
        Self { mmio }
    }

    /// Enable the distributor and this CPU's interface and open the
    /// priority mask so every priority is signalled.
    pub fn init(&self) {
        self.mmio.gicc_write(GICC_PMR, 0xFF);
        self.mmio.gicc_write(GICC_CTLR, 1);
        self.mmio.gicd_write(GICD_CTLR, 1);
    }

    /// Give `intid` a mid-range priority and set its enable bit.
    pub fn enable_intid(&self, intid: u32) {
        self.mmio
            .gicd_write_byte(GICD_IPRIORITYR + intid as usize, 0x80);
        self.mmio
            .gicd_write(isenabler_offset(intid), isenabler_bit(intid));
    }

    /// Clear `intid`'s enable bit, masking it at the distributor.
    pub fn disable_intid(&self, intid: u32) {
        self.mmio
            .gicd_write(icenabler_offset(intid), isenabler_bit(intid));
    }

    /// Route shared-peripheral interrupt `intid` to the CPU interfaces
    /// named in `cpu_targets` (a bitmask: bit `c` selects CPU `c`).
    ///
    /// SPIs reset to *no* target on the GICv2, so a device's SPI is
    /// never delivered until its `GICD_ITARGETSR` byte names a CPU;
    /// this is the SPI analogue of the x86_64 IO-APIC redirection
    /// entry's destination field. INTIDs below [`MIN_SPI_INTID`] are
    /// SGIs/PPIs whose target bytes are read-only and banked per CPU, so
    /// the routing write is skipped for them (a no-op rather than a
    /// silently-ignored read-only store).
    pub fn route_spi(&self, intid: u32, cpu_targets: u8) {
        if intid >= MIN_SPI_INTID {
            self.mmio
                .gicd_write_byte(itargetsr_offset(intid), cpu_targets);
        }
    }

    /// Read `GICC_IAR` to acknowledge the highest-priority pending
    /// interrupt, returning its INTID (which may be [`SPURIOUS_INTID`]).
    #[must_use]
    pub fn acknowledge(&self) -> u32 {
        self.mmio.gicc_read(GICC_IAR) & IAR_INTID_MASK
    }

    /// Write `GICC_EOIR` to deactivate `intid`.
    pub fn end_of_interrupt(&self, intid: u32) {
        self.mmio.gicc_write(GICC_EOIR, intid);
    }

    /// The lowest shared-peripheral interrupt (SPI) currently stuck in the
    /// distributor **and still able to reach a CPU** — active (handler in
    /// flight and not completed) in preference to merely pending-and-
    /// enabled — scanning up to `max_intid`, or `None` when no such SPI is
    /// stuck. The returned [`StuckInterrupt`] carries whether the line is
    /// active (a live storm) or pending. A masked line is skipped: it
    /// cannot reach a CPU, so it can never be the wedge.
    ///
    /// A pure read of the distributor's globally-shared status, safe to
    /// call from any CPU (the watchdog observer uses it to name the line
    /// wedging a hard-locked core).
    #[must_use]
    pub fn stuck_spi(&self, max_intid: u32) -> Option<StuckInterrupt> {
        first_stuck_spi(max_intid, |off| self.mmio.gicd_read(off))
    }

    /// Raise SGI INTID 0 on the CPUs named in `target`'s target-list bit.
    ///
    /// Publishes this CPU's prior stores ([`GicMmio::publish_barrier`])
    /// before the `GICD_SGIR` write, so the run-queue enqueue the waker
    /// performed before raising the reschedule IPI is observable to the
    /// target PE before it takes the SGI — otherwise the target could
    /// dispatch against a stale, empty run queue and strand the woken task
    /// (a lost wake-up).
    pub fn send_sgi(&self, target: CpuId) {
        let bit = u8::try_from(target)
            .ok()
            .and_then(|c| 1u8.checked_shl(u32::from(c)));
        if let Some(target_list) = bit {
            self.mmio.publish_barrier();
            self.mmio.gicd_write(GICD_SGIR, sgir_value(0, target_list));
        }
    }
}

/// GICv2 controller: the policy layer over [`Gicv2`].
///
/// Validates every INTID against `max_intid` and fails closed before touching a register. Implements the
/// Arch HAL [`tairix_arch_api::IrqController`] (line masking) and
/// [`tairix_arch_api::InterruptEntry`] (the claim/complete handshake).
pub struct GicController<M: GicMmio> {
    gic: Gicv2<M>,
    max_intid: u32,
}

impl<M: GicMmio> GicController<M> {
    /// Build a controller over `gic` whose highest valid INTID is
    /// `max_intid` (inclusive, clamped to [`MAX_INTID`]).
    #[must_use]
    pub const fn new(gic: Gicv2<M>, max_intid: u32) -> Self {
        let max_intid = if max_intid > MAX_INTID {
            MAX_INTID
        } else {
            max_intid
        };
        Self { gic, max_intid }
    }

    /// Inclusive upper bound on accepted INTIDs.
    #[must_use]
    pub const fn max_intid(&self) -> u32 {
        self.max_intid
    }

    const fn in_range(&self, intid: u32) -> bool {
        intid <= self.max_intid
    }
}

impl<M: GicMmio + Send + Sync> tairix_arch_api::IrqController for GicController<M> {
    /// Mask `line` by clearing its distributor enable bit, then emit a
    /// `SeqCst` fence so the masked state is globally visible before a
    /// waiter observes `ready = true` (`docs/src/security/irq.md`).
    fn mask(&self, line: u32) -> Result<(), tairix_arch_api::IrqControlError> {
        if !self.in_range(line) {
            return Err(tairix_arch_api::IrqControlError::OutOfRange);
        }
        self.gic.disable_intid(line);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Unmask `line` by setting its distributor enable bit (priority is
    /// left at the mid value [`Gicv2::enable_intid`] installs).
    fn unmask(&self, line: u32) -> Result<(), tairix_arch_api::IrqControlError> {
        if !self.in_range(line) {
            return Err(tairix_arch_api::IrqControlError::OutOfRange);
        }
        self.gic.enable_intid(line);
        Ok(())
    }
}

impl<M: GicMmio + Send + Sync> tairix_arch_api::InterruptEntry for GicController<M> {
    /// Acknowledge the active interrupt, mapping the GICv2
    /// [`SPURIOUS_INTID`] ("nothing pending") to [`None`].
    fn claim(&self) -> Option<u32> {
        match self.gic.acknowledge() {
            SPURIOUS_INTID => None,
            intid => Some(intid),
        }
    }

    /// End-of-interrupt for `line`.
    fn complete(&self, line: u32) {
        self.gic.end_of_interrupt(line);
    }
}

/// Bare-metal [`GicMmio`] over the **discovered** GICv2 windows.
///
/// A zero-sized handle: each access reads the distributor / CPU-interface
/// base [`current`] holds at that moment (an atomic load), so the driver
/// always targets the discovered base and the handle stays
/// const-constructible (it can live in a `static`, the way the IRQ-table
/// bridge does). Compiled only for the freestanding aarch64 target; host
/// builds use the in-memory mock in the test module.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub struct VolatileGicMmio;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
impl GicMmio for VolatileGicMmio {
    fn gicd_read(&self, off: usize) -> u32 {
        // SAFETY: `off` addresses a distributor register within the
        // discovered GICv2 MMIO window the kernel owns.
        unsafe { core::ptr::read_volatile((current().0 + off) as *const u32) }
    }
    fn gicd_write(&self, off: usize, val: u32) {
        // SAFETY: as `gicd_read`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((current().0 + off) as *mut u32, val) }
    }
    fn gicd_write_byte(&self, off: usize, val: u8) {
        // SAFETY: the byte-addressable priority register for the INTID
        // inside the distributor window; QEMU honours the byte store.
        unsafe { core::ptr::write_volatile((current().0 + off) as *mut u8, val) }
    }
    fn gicc_read(&self, off: usize) -> u32 {
        // SAFETY: `off` addresses a CPU-interface register within the
        // discovered GICv2 MMIO window the kernel owns.
        unsafe { core::ptr::read_volatile((current().1 + off) as *const u32) }
    }
    fn gicc_write(&self, off: usize, val: u32) {
        // SAFETY: as `gicc_read`, but a 32-bit store.
        unsafe { core::ptr::write_volatile((current().1 + off) as *mut u32, val) }
    }
    fn publish_barrier(&self) {
        // SAFETY: `dsb ishst` is an unprivileged data-synchronisation
        // barrier that completes this PE's outstanding stores to the
        // inner-shareable domain before the following `GICD_SGIR` write;
        // it touches no memory and has no effect beyond ordering.
        unsafe {
            core::arch::asm!("dsb ishst", options(nostack, preserves_flags));
        }
    }
}

/// Construct the production driver over the discovered GICv2 windows.
///
/// # Safety
///
/// The GICv2 distributor + CPU interface must be mapped at the
/// discovered bases ([`current`]), identity-mapped and exclusively owned
/// by the kernel.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
const unsafe fn volatile_gic() -> Gicv2<VolatileGicMmio> {
    Gicv2::new(VolatileGicMmio)
}

/// Enable the distributor and the calling CPU's interface, and open the
/// priority mask so every priority is signalled.
///
/// # Safety
///
/// Must be called once per CPU during bring-up, before any interrupt
/// source is enabled. It writes the fixed GICv2 MMIO windows on the
/// `virt` board; calling it elsewhere is a kernel bug.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn init() {
    // SAFETY: bring-up context — the fixed `virt`-board windows are
    // mapped and owned by the kernel.
    unsafe { volatile_gic() }.init();
}

/// Enable private-peripheral / shared interrupt `intid` and give it a
/// mid-range priority.
///
/// # Safety
///
/// The distributor must already be enabled ([`init`]). Enabling an
/// INTID lets it reach the CPU once its source is armed.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn enable_ppi(intid: u32) {
    // SAFETY: as `init` — the fixed windows are mapped and owned.
    unsafe { volatile_gic() }.enable_intid(intid);
}

/// Route shared-peripheral interrupt `intid` to the CPU interfaces named
/// in `cpu_targets` (bit `c` selects CPU `c`).
///
/// SPIs reset to no target on the GICv2, so this must be called for a
/// device SPI before it can be delivered (see [`Gicv2::route_spi`]).
///
/// # Safety
///
/// The distributor must already be enabled ([`init`]); the fixed
/// `virt`-board windows are mapped and owned by the kernel.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub unsafe fn route_spi(intid: u32, cpu_targets: u8) {
    // SAFETY: as `init` — the fixed windows are mapped and owned.
    unsafe { volatile_gic() }.route_spi(intid, cpu_targets);
}

/// Read `GICC_IAR` to acknowledge and activate the highest-priority
/// pending interrupt, returning its INTID (which may be
/// [`SPURIOUS_INTID`]).
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn acknowledge() -> u32 {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.acknowledge()
}

/// Write `GICC_EOIR` to deactivate the interrupt `intid` previously
/// returned by [`acknowledge`].
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn end_of_interrupt(intid: u32) {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.end_of_interrupt(intid);
}

/// Raise software-generated interrupt INTID 0 on `target`.
///
/// Used by [`crate::kernel_arch::Aarch64Arch`]'s `send_ipi`. A
/// single-CPU image targets itself, which the GIC delivers as a normal
/// SGI.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn send_sgi(target: CpuId) {
    // SAFETY: the fixed windows are mapped and owned by the kernel.
    unsafe { volatile_gic() }.send_sgi(target);
}

/// The lowest shared-peripheral interrupt currently stuck **and still able
/// to reach a CPU** (active in preference to enabled-and-pending) in the
/// distributor, or `None` when none is.
///
/// The watchdog observer calls this when it detects a hard lockup on
/// another CPU: the wedged core's own last-known sample is stale, so this
/// globally-visible read names the device line actually wedging it (an
/// interrupt storm, or a line whose handler never completes). A masked line
/// is never reported — it cannot be delivered, so it cannot be the wedge.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn stuck_spi() -> Option<StuckInterrupt> {
    // SAFETY: the discovered windows are mapped and owned by the kernel;
    // reading the distributor status words has no side effect.
    unsafe { volatile_gic() }.stuck_spi(MAX_INTID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgir_packs_target_list_and_intid() {
        // INTID 0 to CPU 0 → target list bit 0.
        assert_eq!(sgir_value(0, 0b0000_0001), 1 << 16);
        // INTID 0 to CPU 2 → target list bit 2.
        assert_eq!(sgir_value(0, 0b0000_0100), 0b0000_0100 << 16);
        // INTID field is the low 4 bits only.
        assert_eq!(sgir_value(0x1F, 1) & 0xF, 0xF);
    }

    #[test]
    fn isenabler_offset_selects_the_right_word() {
        // INTIDs 0..32 live in the first word (offset 0x100).
        assert_eq!(isenabler_offset(0), 0x100);
        assert_eq!(isenabler_offset(30), 0x100);
        // INTID 32 spills into the second word.
        assert_eq!(isenabler_offset(32), 0x104);
    }

    #[test]
    fn isenabler_bit_indexes_within_the_word() {
        assert_eq!(isenabler_bit(0), 1);
        assert_eq!(isenabler_bit(30), 1 << 30);
        assert_eq!(isenabler_bit(32), 1);
    }

    #[test]
    fn iar_mask_and_spurious_match_gicv2_spec() {
        assert_eq!(IAR_INTID_MASK, 0x3FF);
        assert_eq!(SPURIOUS_INTID, 1023);
    }

    #[test]
    fn icenabler_offset_parallels_isenabler() {
        assert_eq!(icenabler_offset(0), 0x180);
        assert_eq!(icenabler_offset(30), 0x180);
        assert_eq!(icenabler_offset(32), 0x184);
    }

    #[test]
    fn itargetsr_offset_is_one_byte_per_intid() {
        // SPI 2 on the `virt` board (the PL031 RTC) is INTID 34.
        assert_eq!(itargetsr_offset(34), 0x800 + 34);
        assert_eq!(itargetsr_offset(MIN_SPI_INTID), 0x800 + 32);
    }

    #[test]
    fn route_spi_writes_the_target_byte_for_an_spi() {
        let gic = Gicv2::new(MockGicMmio::new());
        // Route INTID 34 to CPU 0 (target-list bit 0).
        gic.route_spi(34, 0b0000_0001);
        assert_eq!(gic.mmio.gicd_read(itargetsr_offset(34)), 0b0000_0001);
    }

    #[test]
    fn route_spi_skips_sgis_and_ppis() {
        let gic = Gicv2::new(MockGicMmio::new());
        // INTID 30 is the timer PPI: its target byte is read-only and
        // banked, so `route_spi` must not write it.
        gic.route_spi(30, 0b0000_0001);
        assert_eq!(gic.mmio.gicd_read(itargetsr_offset(30)), 0);
    }

    /// One recorded mock operation, in issue order, so a test can assert
    /// the publish-before-signal ordering the SGI hand-off requires.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MockOp {
        /// A `publish_barrier` (the `dsb ishst` on metal).
        Barrier,
        /// A distributor write to the `GICD_SGIR` register (raising the SGI).
        SgirWrite,
    }

    /// In-memory GICv2 register file: distributor and CPU-interface
    /// windows are independent, so the mock keeps a map per window and
    /// serves the last value written to a register on a subsequent read.
    /// It also records the barrier / `GICD_SGIR`-write sequence in
    /// [`MockGicMmio::ops`] so the publish-before-signal ordering is
    /// unit-tested without hardware.
    struct MockGicMmio {
        gicd: std::sync::Mutex<std::collections::HashMap<usize, u32>>,
        gicc: std::sync::Mutex<std::collections::HashMap<usize, u32>>,
        ops: std::sync::Mutex<std::vec::Vec<MockOp>>,
    }

    impl MockGicMmio {
        fn new() -> Self {
            Self {
                gicd: std::sync::Mutex::new(std::collections::HashMap::new()),
                gicc: std::sync::Mutex::new(std::collections::HashMap::new()),
                ops: std::sync::Mutex::new(std::vec::Vec::new()),
            }
        }

        /// The barrier / SGIR-write operations recorded so far, in order.
        fn ops(&self) -> std::vec::Vec<MockOp> {
            self.ops.lock().unwrap().clone()
        }
    }

    impl GicMmio for MockGicMmio {
        fn gicd_read(&self, off: usize) -> u32 {
            *self.gicd.lock().unwrap().get(&off).unwrap_or(&0)
        }
        fn gicd_write(&self, off: usize, val: u32) {
            if off == GICD_SGIR {
                self.ops.lock().unwrap().push(MockOp::SgirWrite);
            }
            self.gicd.lock().unwrap().insert(off, val);
        }
        fn gicd_write_byte(&self, off: usize, val: u8) {
            self.gicd.lock().unwrap().insert(off, u32::from(val));
        }
        fn gicc_read(&self, off: usize) -> u32 {
            *self.gicc.lock().unwrap().get(&off).unwrap_or(&0)
        }
        fn gicc_write(&self, off: usize, val: u32) {
            self.gicc.lock().unwrap().insert(off, val);
        }
        fn publish_barrier(&self) {
            self.ops.lock().unwrap().push(MockOp::Barrier);
        }
    }

    #[test]
    fn send_sgi_publishes_prior_stores_before_raising_the_interrupt() {
        // The cross-CPU wake hand-off enqueues the woken task and *then*
        // raises the reschedule IPI. On a weakly-ordered PE the enqueue
        // must be published before the target can act on the SGI, or the
        // target dispatches against a stale run queue and strands the task
        // (a lost wake-up that hangs the system). `send_sgi` must issue the
        // publish barrier strictly before the `GICD_SGIR` write.
        let gic = Gicv2::new(MockGicMmio::new());
        gic.send_sgi(1);
        assert_eq!(
            gic.mmio.ops(),
            std::vec![MockOp::Barrier, MockOp::SgirWrite],
            "the publish barrier must precede the SGIR write"
        );
        // The SGI was actually raised (INTID 0 to CPU 1's target-list bit).
        assert_eq!(gic.mmio.gicd_read(GICD_SGIR), sgir_value(0, 0b0000_0010));
    }

    #[test]
    fn send_sgi_to_an_unrepresentable_target_neither_barriers_nor_writes() {
        // A target whose bit does not fit the 8-bit target list raises no
        // SGI, so it must not touch the SGIR — and, having nothing to
        // publish for, issues no barrier either (fail closed, no partial
        // hand-off).
        let gic = Gicv2::new(MockGicMmio::new());
        gic.send_sgi(8);
        assert!(gic.mmio.ops().is_empty());
    }

    #[test]
    fn enable_intid_sets_priority_and_enable_bit() {
        let gic = Gicv2::new(MockGicMmio::new());
        gic.enable_intid(42);
        assert_eq!(gic.mmio.gicd_read(GICD_IPRIORITYR + 42), 0x80);
        assert_eq!(
            gic.mmio.gicd_read(isenabler_offset(42)) & isenabler_bit(42),
            isenabler_bit(42)
        );
    }

    #[test]
    fn disable_intid_sets_clear_enable_bit() {
        let gic = Gicv2::new(MockGicMmio::new());
        gic.disable_intid(42);
        assert_eq!(
            gic.mmio.gicd_read(icenabler_offset(42)) & isenabler_bit(42),
            isenabler_bit(42)
        );
    }

    #[test]
    fn acknowledge_masks_off_the_source_cpu_bits() {
        let gic = Gicv2::new(MockGicMmio::new());
        // A real IAR carries the source CPU in the upper bits for an SGI;
        // `acknowledge` returns only the INTID field.
        gic.mmio.gicc_write(GICC_IAR, (0b101 << 10) | 0x2A);
        assert_eq!(gic.acknowledge(), 0x2A);
    }

    #[test]
    fn controller_clamps_max_intid_to_the_spec_ceiling() {
        let c = GicController::new(Gicv2::new(MockGicMmio::new()), u32::MAX);
        assert_eq!(c.max_intid(), MAX_INTID);
    }

    #[test]
    fn stuck_spi_is_none_when_nothing_is_active_or_pending() {
        let gic = Gicv2::new(MockGicMmio::new());
        assert_eq!(gic.stuck_spi(MAX_INTID), None);
    }

    #[test]
    fn stuck_spi_names_the_lowest_active_line() {
        let gic = Gicv2::new(MockGicMmio::new());
        // SPI 37 active: word covering 32..64 with bit (37-32)=5 set — a
        // handler in flight, a genuine hard-lockup suspect.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISACTIVER, 37), 1 << 5);
        assert_eq!(
            gic.stuck_spi(MAX_INTID),
            Some(StuckInterrupt {
                intid: 37,
                active: true,
            })
        );
    }

    #[test]
    fn stuck_spi_reports_an_enabled_pending_line_when_none_is_active() {
        let gic = Gicv2::new(MockGicMmio::new());
        // SPI 50 pending (bit 50-32=18 in the 32..64 pending word) and
        // still enabled: asserted, deliverable, and so a real suspect.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISPENDR, 50), 1 << 18);
        gic.mmio.gicd_write(isenabler_offset(50), isenabler_bit(50));
        assert_eq!(
            gic.stuck_spi(MAX_INTID),
            Some(StuckInterrupt {
                intid: 50,
                active: false,
            })
        );
    }

    #[test]
    fn stuck_spi_skips_a_masked_pending_line() {
        let gic = Gicv2::new(MockGicMmio::new());
        // SPI 50 pending but with no enable bit set: masked, so it cannot
        // reach a CPU and can never be the wedge. It must not be reported
        // (the recurring spurious `stuck_irq=111` this fix closes).
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISPENDR, 50), 1 << 18);
        assert_eq!(gic.stuck_spi(MAX_INTID), None);
        // A higher pending line that *is* enabled is still found, skipping
        // the lower masked one rather than stopping at it.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISPENDR, 55), 1 << 23);
        gic.mmio.gicd_write(isenabler_offset(55), isenabler_bit(55));
        assert_eq!(
            gic.stuck_spi(MAX_INTID),
            Some(StuckInterrupt {
                intid: 55,
                active: false,
            })
        );
    }

    #[test]
    fn stuck_spi_prefers_an_active_line_over_a_lower_pending_one() {
        let gic = Gicv2::new(MockGicMmio::new());
        // A higher line stuck *active* is the stronger hard-lockup signal
        // than a lower one merely pending, so active wins outright.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISACTIVER, 96), 1 << 0);
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISPENDR, 40), 1 << 8);
        gic.mmio.gicd_write(isenabler_offset(40), isenabler_bit(40));
        assert_eq!(
            gic.stuck_spi(MAX_INTID),
            Some(StuckInterrupt {
                intid: 96,
                active: true,
            })
        );
    }

    #[test]
    fn stuck_spi_ignores_sgi_ppi_status_and_out_of_range_bits() {
        let gic = Gicv2::new(MockGicMmio::new());
        // Bits in the first word (SGIs/PPIs, id 0..32) are banked per CPU
        // and must not be reported. Only the SPI range is scanned.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISACTIVER, 0), 0xF);
        assert_eq!(gic.stuck_spi(MAX_INTID), None);
        // A line above the controller's max is out of range and ignored.
        gic.mmio
            .gicd_write(gicd_bit_word_offset(GICD_ISACTIVER, 40), 1 << 8);
        assert_eq!(gic.stuck_spi(39), None);
        assert_eq!(
            gic.stuck_spi(48),
            Some(StuckInterrupt {
                intid: 40,
                active: true,
            })
        );
    }

    /// / W3: the GIC controller passes the shared Arch HAL
    /// interrupt-controller + interrupt-entry conformance verticals over
    /// its real handle (`plans/WIRING.md` Stage W3). INTID 42 is an
    /// addressable SPI; 2000 is above [`MAX_INTID`]. The mock's `GICC_IAR`
    /// is seeded with [`SPURIOUS_INTID`] so the [`InterruptEntry`] drain
    /// terminates ("nothing pending").
    #[test]
    fn gic_controller_passes_arch_hal_irq_conformance() {
        use tairix_arch_api::{InterruptEntry, IrqController};

        let c = GicController::new(Gicv2::new(MockGicMmio::new()), 1019);
        c.gic.mmio.gicc_write(GICC_IAR, SPURIOUS_INTID);

        tairix_arch_api::irq::conformance::run_controller(&c, 42, 2000);
        tairix_arch_api::irq::conformance::run_entry(&c);

        // Object-safe behind `&dyn`, the way the kernel reaches it.
        let dyn_ctrl: &dyn IrqController = &c;
        assert_eq!(dyn_ctrl.mask(42), Ok(()));
        let dyn_entry: &dyn InterruptEntry = &c;
        assert_eq!(dyn_entry.claim(), None);
    }

    #[test]
    fn gic_compatible_matches_gicv2_class_controllers() {
        // The QEMU `virt` board and the Pi 4's GIC-400 are both GICv2.
        assert!(is_gic_compatible(b"arm,cortex-a15-gic"));
        assert!(is_gic_compatible(b"arm,gic-400"));
        // A GICv3 redistributor layout is *not* this driver's; fail closed.
        assert!(!is_gic_compatible(b"arm,gic-v3"));
        assert!(!is_gic_compatible(b""));
    }

    #[test]
    fn finds_gic_400_bases_in_a_raspi_tree() {
        // The Pi-shaped fixture carries a GIC-400 under `/soc` with bus
        // `reg` values; discovery translates them through the `ranges`
        // to the BCM2711 CPU-physical bases.
        let blob = tairix_fdt::fixture::raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let gic = find_gic(&fdt).expect("a GIC is present");
        assert_eq!(gic.gicd_base, 0xff84_1000);
        assert_eq!(gic.gicc_base, 0xff84_2000);
    }

    #[test]
    fn finds_gicv2_bases_in_a_virt_tree() {
        // The `virt`-shaped fixture carries the GICv2 at the default bases.
        let blob = tairix_fdt::fixture::virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let gic = find_gic(&fdt).expect("a GIC is present");
        assert_eq!(usize::try_from(gic.gicd_base).unwrap(), DEFAULT_GICD_BASE);
        assert_eq!(usize::try_from(gic.gicc_base).unwrap(), DEFAULT_GICC_BASE);
    }

    #[test]
    fn no_gic_in_a_gicless_tree_is_none() {
        // A tree with only the two console UARTs (no `intc` node) yields
        // no GIC — the boot path then keeps the fail-safe default.
        let mut b = tairix_fdt::fixture::DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("serial@9000000");
        b.prop_str("compatible", "arm,pl011");
        let mut reg = std::vec::Vec::new();
        reg.extend_from_slice(&0x0900_0000u64.to_be_bytes());
        reg.extend_from_slice(&0x1000u64.to_be_bytes());
        b.prop("reg", &reg);
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(find_gic(&fdt), None);
    }

    #[test]
    fn configure_from_fdt_applies_the_discovered_bases() {
        // Drive the global config through a Pi-shaped FDT and read it
        // back. This test owns the global GIC base slot for its duration;
        // the other tests here either exercise pure helpers (`find_gic`)
        // or the mock MMIO, so there is no cross-test interference.
        let blob = tairix_fdt::fixture::raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let applied = configure_from_fdt(&fdt).expect("GIC discovered");
        assert_eq!(applied.gicd_base, 0xff84_1000);
        assert_eq!(current(), (0xff84_1000, 0xff84_2000));

        // Restore the default so the process-global slot is left as other
        // code expects (defence-in-depth; nothing else reads it on host).
        configure(DEFAULT_GICD_BASE, DEFAULT_GICC_BASE);
    }
}
