//! The pre-MMU `VideoCore` firmware channel, and the boot-time requests the
//! kernel makes over it.
//!
//! On a Raspberry Pi several things the kernel needs before it has drivers
//! belong to the firmware rather than to any register the ARM cores can
//! reach: the scan-out surface the boot console renders into, and the rate
//! the ARM cores run at. Both are property exchanges over the same mailbox
//! doorbell, so the doorbell, the DMA-visible property buffer, and the
//! transport built over them live here once and every early consumer borrows
//! them (`with_transport`).
//!
//! Each exchange is split into a pure half that takes a transport
//! (`raise_over`) and a wrapper that builds one, so the protocol sequence is
//! host-testable against the mock firmware — QEMU models no `VideoCore`, so
//! that is the only way to prove it before it runs on metal.
//!
//! # Boot CPU, before the MMU
//!
//! Every exchange here runs from the boot CPU before `enable_mmu_and_vectors`:
//! with the data caches still off the CPU↔firmware exchange is coherent by
//! construction, so no cache maintenance is needed, and there is no other
//! thread to serialise against. That is also why the property buffer is a
//! plain cell rather than a lock — an atomic read-modify-write on MMU-off
//! Device-typed memory is architecturally UNPREDICTABLE, the constraint that
//! orders the whole aarch64 boot (`plans/PI.md` P6c-2).
//!
//! After boot the doorbell belongs to the autoloaded user-space `vcmailbox`
//! service, which is the only thing that speaks to the firmware from then on.
//! Nothing here runs again.
//!
//! # Fail closed
//!
//! A board whose tree carries no mailbox (QEMU `virt`), a doorbell window
//! shorter than the register block, an unreachable firmware, or a malformed
//! answer all yield `None`. The console then keeps the UART and the clock
//! keeps whatever rate the firmware had chosen; nothing is guessed at.

use tairix_fdt::Fdt;

/// Compatible string of the BCM283x/BCM2711 firmware mailbox doorbell.
///
/// The single source of the match identity is the device's own client crate,
/// so this discovery and the `vcmailbox` service driver's `BIND_KEYS` cannot
/// diverge.
use tairix_vcmailbox::{
    query_clock_rate, set_clock_rate, ClockRateQuery, FirmwareClock, MailboxTransport,
    MAILBOX_COMPATIBLE,
};

/// A firmware mailbox doorbell located in a flattened device tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DiscoveredMailbox {
    /// CPU-physical MMIO base of the doorbell register block (the node's
    /// first `reg` entry, decoded with its parent bus's cell counts and
    /// translated through the ancestor buses' `ranges`).
    pub base: u64,
    /// Length in bytes of the register window.
    pub len: u64,
}

/// Find the `VideoCore` firmware mailbox doorbell in `fdt`.
///
/// The walk early-returns at the matched node
/// ([`tairix_fdt::scan_translated`]), so it stays safe with the MMU off.
/// Returns `None` when the tree carries no mailbox (e.g. QEMU `virt`) or the
/// node's `reg` cannot be decoded or translated.
#[must_use]
pub fn find_mailbox(fdt: &Fdt<'_>) -> Option<DiscoveredMailbox> {
    tairix_fdt::scan_translated(fdt, |node, levels, depth| {
        let compatible = node.property("compatible")?;
        if !compatible.iter_strings().any(|s| s == MAILBOX_COMPATIBLE) {
            return None;
        }
        let (base, len) = tairix_fdt::translated_reg(node, depth, levels, 0)?;
        Some(DiscoveredMailbox { base, len })
    })
}

/// Raise the ARM core clock to the highest rate the firmware will deliver,
/// returning that rate in Hz.
///
/// The firmware leaves the ARM clock wherever it last put it, which after the
/// boot window is `arm_freq_min` — 600 MHz on a Pi 4B whose parts are rated
/// at 1.5 GHz. Everything between here and the autoloaded frequency driver
/// (mounting the root volume, unlocking it, reaching a login) would otherwise
/// run at 40% of the machine's speed, so the kernel asks for full speed while
/// it is still the only thing that can.
///
/// This is a one-shot floor, not a policy: it takes no view of utilisation
/// and never lowers the clock. Once `devmgr` autoloads the frequency driver,
/// the kernel governor's targets take over.
///
/// The ceiling is read from the firmware rather than assumed, so a board
/// whose `config.txt` raises `arm_freq` is raised to *its* ceiling. Returns
/// `None` on any failure — no mailbox, a short doorbell window, an
/// unreachable or unhelpful firmware — leaving the clock as it was.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
#[must_use]
pub fn raise_cpu_clock(fdt: &Fdt<'_>) -> Option<u32> {
    let mailbox = find_mailbox(fdt)?;
    with_transport(mailbox, raise_over).flatten()
}

/// Ask the firmware behind `transport` for the ARM clock's ceiling and set
/// the clock to it, returning the rate it applied.
///
/// The exchange half of `raise_cpu_clock`, separated so it is host-testable
/// against the protocol-faithful mock firmware — QEMU models no `VideoCore`,
/// so this is the only way to prove the sequence before it runs on metal.
///
/// A zero ceiling is how the property interface spells "no such clock", so it
/// yields `None` rather than a request for a stopped clock.
#[must_use]
pub fn raise_over(transport: &mut dyn MailboxTransport) -> Option<u32> {
    let ceiling = query_clock_rate(transport, FirmwareClock::Arm, ClockRateQuery::Max).ok()?;
    if ceiling == 0 {
        return None;
    }
    set_clock_rate(transport, FirmwareClock::Arm, ceiling).ok()
}

/// Run `body` against a transport over `mailbox`'s doorbell and the shared
/// property buffer.
///
/// `None` when the doorbell window is shorter than the register block, its
/// base cannot be addressed, or the buffer's bus address cannot be formed —
/// in which case `body` never runs and no exchange is attempted.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub fn with_transport<R>(
    mailbox: DiscoveredMailbox,
    body: impl FnOnce(&mut dyn MailboxTransport) -> R,
) -> Option<R> {
    metal::with_transport(mailbox, body)
}

/// The freestanding half: the doorbell window, the DMA-visible property
/// buffer, and the transport over them. Target-only — it addresses MMIO and
/// SDRAM through boot-identity addresses.
#[cfg(all(target_arch = "aarch64", target_os = "none"))]
mod metal {
    use core::cell::UnsafeCell;
    use core::ptr::NonNull;

    use tairix_abi::RegisterWindow;
    use tairix_vcmailbox::{
        arm_physical_to_bus, MailboxTransport, MmioMailbox, DEFAULT_BUS_ALIAS, DEFAULT_POLL_BUDGET,
        MAILBOX_REGS_LEN_BYTES, PROPERTY_LEN_BYTES,
    };

    use super::DiscoveredMailbox;

    /// The DMA-visible mailbox property message, 16-byte aligned as the
    /// doorbell protocol requires.
    #[repr(align(16))]
    struct PropertyBuffer(UnsafeCell<[u8; PROPERTY_LEN_BYTES]>);

    // SAFETY: written only by the single-threaded boot CPU inside the
    // pre-MMU discovery sequence, before SMP bring-up and before any other
    // user of the doorbell exists; never touched again afterwards, since the
    // user-space `vcmailbox` service owns the doorbell from then on.
    unsafe impl Sync for PropertyBuffer {}

    static PROPERTY_BUFFER: PropertyBuffer =
        PropertyBuffer(UnsafeCell::new([0; PROPERTY_LEN_BYTES]));

    /// Build the transport and hand it to `body`. See
    /// [`super::with_transport`].
    pub(super) fn with_transport<R>(
        mailbox: DiscoveredMailbox,
        body: impl FnOnce(&mut dyn MailboxTransport) -> R,
    ) -> Option<R> {
        if mailbox.len < MAILBOX_REGS_LEN_BYTES as u64 {
            return None;
        }
        // SAFETY: single-threaded boot CPU, pre-publication (see
        // `PropertyBuffer`): no other reference to the buffer exists.
        let buffer_ptr = unsafe { NonNull::new_unchecked(PROPERTY_BUFFER.0.get().cast::<u8>()) };
        let buffer_phys = buffer_ptr.as_ptr() as u64;
        let buffer_bus = arm_physical_to_bus(buffer_phys, DEFAULT_BUS_ALIAS).ok()?;
        let doorbell_ptr = NonNull::new(usize::try_from(mailbox.base).ok()? as *mut u8)?;
        // SAFETY: `mailbox.base` is the FDT-discovered, `ranges`-translated
        // CPU-physical doorbell window (boot runs identity-addressed), at
        // least `MAILBOX_REGS_LEN_BYTES` long (checked above); the buffer
        // pointer covers exactly `PROPERTY_LEN_BYTES` of the static above.
        // Both windows are accessed only through `RegisterWindow`'s checked
        // 32-bit accessors, and neither outlives this call.
        let regs = unsafe {
            RegisterWindow::from_mapping(mailbox.base, doorbell_ptr, MAILBOX_REGS_LEN_BYTES)
        };
        let buffer =
            unsafe { RegisterWindow::from_mapping(buffer_phys, buffer_ptr, PROPERTY_LEN_BYTES) };
        let mut transport = MmioMailbox::new(regs, buffer, buffer_bus, DEFAULT_POLL_BUDGET).ok()?;
        Some(body(&mut transport))
    }
}

#[cfg(test)]
mod tests {
    use super::{find_mailbox, raise_over};
    use tairix_fdt::fixture::raspi_like_arm;
    use tairix_fdt::Fdt;
    use tairix_vcmailbox::mock::MockFirmware;

    #[test]
    fn finds_the_mailbox_in_a_raspi_tree() {
        // The fixture mirrors the real Pi 4 tree: the mailbox sits under
        // `/soc` at bus address `0x7E00_B880`, remapped by `ranges` to
        // CPU-physical `0xFE00_B880`.
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mailbox = find_mailbox(&fdt).expect("mailbox present");
        assert_eq!(mailbox.base, 0xfe00_b880);
        assert_eq!(mailbox.len, 0x40);
    }

    #[test]
    fn the_clock_is_raised_to_the_ceiling_the_firmware_reports() {
        // The reported defect's boot half: the board is found at its minimum
        // and must be asked for its own ceiling, not a board constant.
        let mut firmware = MockFirmware::healthy();
        firmware.arm_clock_min_hz = 600_000_000;
        firmware.arm_clock_max_hz = 1_500_000_000;
        firmware.arm_clock_hz = 600_000_000;
        assert_eq!(raise_over(&mut firmware), Some(1_500_000_000));
        assert_eq!(firmware.arm_clock_hz, 1_500_000_000);
    }

    #[test]
    fn an_overclocked_board_is_raised_to_its_own_ceiling() {
        let mut firmware = MockFirmware::healthy();
        firmware.arm_clock_max_hz = 2_000_000_000;
        assert_eq!(raise_over(&mut firmware), Some(2_000_000_000));
    }

    #[test]
    fn a_firmware_that_does_not_know_the_clock_is_left_alone() {
        // Zero means "no such clock". Setting the clock to it would ask for a
        // stopped one, so nothing is asked at all.
        let mut firmware = MockFirmware::healthy();
        firmware.arm_clock_max_hz = 0;
        let before = firmware.arm_clock_hz;
        assert_eq!(raise_over(&mut firmware), None);
        assert_eq!(firmware.arm_clock_hz, before, "the clock is left as it was");
    }

    #[test]
    fn no_mailbox_in_a_mailboxless_tree_is_none() {
        // A virt-like tree carries no `brcm,bcm2835-mbox` node, so neither
        // the video console nor the clock raise attempts an exchange.
        let mut builder = tairix_fdt::fixture::DtbBuilder::new();
        builder.begin_node("");
        builder.begin_node("pl011@9000000");
        builder.prop_str("compatible", "arm,pl011");
        builder.end_node();
        builder.end_node();
        let blob = builder.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert!(find_mailbox(&fdt).is_none());
    }
}
