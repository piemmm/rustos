//! The UniMAC MDIO master and the clause-22 PHY it drives.
//!
//! The BCM2711 GENET embeds its own MDIO master in the UniMAC register block
//! (`MDIO_CMD`), so the Pi 4's external gigabit PHY — a BCM54213PE at
//! [`PHY_ADDRESS`] — is reached without a separate MDIO bus driver. The PHY
//! itself is driven purely through the IEEE 802.3 clause-22 register set
//! (BMCR/BMSR/ANAR/ANLPAR and the 1000BASE-T control/status pair), never a
//! vendor register, so the link bring-up is the standard autonegotiation
//! sequence rather than part-specific magic.
//!
//! Every transaction is bounded and fails closed: the `START_BUSY` handshake
//! is polled against a wall-clock deadline ([`MDIO_TIMEOUT_US`]) and a PHY
//! that never answers yields [`DriverError::DeviceFault`] instead of spinning.
//! The delay between polls comes from the injected [`Delay`], so the CPU
//! sleeps rather than burning a core.

use tairix_abi::driver::timing::Delay;
use tairix_abi::DriverError;

use crate::regs;
use crate::GenetRegs;

/// MDIO address of the Pi 4's external gigabit PHY.
///
/// The board strapping puts the BCM54213PE at address 1, which the device
/// tree states as the `ethernet-phy@1` child of the GENET's MDIO bus. This
/// driver reaches the PHY through the MAC's own embedded MDIO master rather
/// than binding that child node, so the address is stated here — the one
/// board fact the PHY path needs.
pub const PHY_ADDRESS: u8 = 1;

/// Microseconds a single MDIO transaction may take before it is refused. A
/// bounded defence against an absent or wedged PHY, not a capacity: a
/// clause-22 transaction at the standard 2.5 MHz MDC completes in tens of
/// microseconds, so 20 ms is orders of magnitude past any honest answer.
pub const MDIO_TIMEOUT_US: u64 = 20_000;

/// Microseconds between `START_BUSY` polls.
const MDIO_POLL_INTERVAL_US: u32 = 10;

/// Microseconds allowed for autonegotiation to complete.
///
/// IEEE 802.3 clause 28 bounds a link-negotiation attempt at roughly 1.6 s
/// and permits several attempts; 4 s covers a slow partner without leaving
/// bring-up open-ended. Expiry is **not** an error — it means "no link
/// partner yet", so the interface comes up link-down and the PHY's link-up
/// interrupt re-resolves it later.
pub const AUTONEG_TIMEOUT_US: u64 = 4_000_000;

/// Microseconds between autonegotiation-completion polls.
const AUTONEG_POLL_INTERVAL_US: u32 = 10_000;

/// Microseconds allowed for the PHY to clear its self-clearing reset bit.
/// Clause 22 requires the reset to complete within 500 ms.
const PHY_RESET_TIMEOUT_US: u64 = 500_000;

/// Microseconds between PHY-reset-completion polls.
const PHY_RESET_POLL_INTERVAL_US: u32 = 1_000;

/// Clause-22 register: basic mode control.
const BMCR: u8 = 0x00;
/// Clause-22 register: basic mode status.
const BMSR: u8 = 0x01;
/// Clause-22 register: autonegotiation advertisement.
const ANAR: u8 = 0x04;
/// Clause-22 register: link-partner ability.
const ANLPAR: u8 = 0x05;
/// Clause-22 register: 1000BASE-T control.
const GBCR: u8 = 0x09;
/// Clause-22 register: 1000BASE-T status.
const GBSR: u8 = 0x0A;

/// `BMCR`: self-clearing PHY reset.
const BMCR_RESET: u16 = 1 << 15;
/// `BMCR`: enable autonegotiation.
const BMCR_ANEG_ENABLE: u16 = 1 << 12;
/// `BMCR`: restart autonegotiation (self-clearing).
const BMCR_ANEG_RESTART: u16 = 1 << 9;
/// `BMCR`: electrical isolation / power down.
const BMCR_POWER_DOWN: u16 = 1 << 11;

/// `BMSR`: autonegotiation complete.
const BMSR_ANEG_COMPLETE: u16 = 1 << 5;
/// `BMSR`: link is up.
const BMSR_LINK_UP: u16 = 1 << 2;

/// `ANAR`/`ANLPAR`: 10BASE-T half duplex.
const ADV_10_HALF: u16 = 1 << 5;
/// `ANAR`/`ANLPAR`: 10BASE-T full duplex.
const ADV_10_FULL: u16 = 1 << 6;
/// `ANAR`/`ANLPAR`: 100BASE-TX half duplex.
const ADV_100_HALF: u16 = 1 << 7;
/// `ANAR`/`ANLPAR`: 100BASE-TX full duplex.
const ADV_100_FULL: u16 = 1 << 8;

/// The full 10/100 advertisement this driver publishes.
const ANAR_ADVERTISE: u16 = ADV_10_HALF | ADV_10_FULL | ADV_100_HALF | ADV_100_FULL;

/// `GBCR`: advertise 1000BASE-T half duplex.
const GBCR_ADV_1000_HALF: u16 = 1 << 8;
/// `GBCR`: advertise 1000BASE-T full duplex.
const GBCR_ADV_1000_FULL: u16 = 1 << 9;

/// The full gigabit advertisement this driver publishes.
const GBCR_ADVERTISE: u16 = GBCR_ADV_1000_HALF | GBCR_ADV_1000_FULL;

/// `GBSR`: link partner is 1000BASE-T half-duplex capable.
const GBSR_LP_1000_HALF: u16 = 1 << 10;
/// `GBSR`: link partner is 1000BASE-T full-duplex capable.
const GBSR_LP_1000_FULL: u16 = 1 << 11;

/// A negotiated link rate.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LinkSpeed {
    /// 10 Mb/s.
    Ten,
    /// 100 Mb/s.
    Hundred,
    /// 1000 Mb/s.
    Thousand,
}

impl LinkSpeed {
    /// The [`regs::UMAC_CMD`] speed selector for this rate.
    #[must_use]
    pub const fn umac_selector(self) -> u32 {
        match self {
            Self::Ten => regs::UMAC_SPEED_10,
            Self::Hundred => regs::UMAC_SPEED_100,
            Self::Thousand => regs::UMAC_SPEED_1000,
        }
    }
}

/// The resolved outcome of autonegotiation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Link {
    /// The negotiated rate.
    pub speed: LinkSpeed,
    /// Whether the negotiated mode is full duplex.
    pub full_duplex: bool,
}

/// Read clause-22 register `reg` of the PHY at `phy` over the UniMAC's MDIO
/// master.
///
/// # Errors
///
/// [`DriverError::OutOfRange`] if a register access falls outside the mapped
/// aperture, or [`DriverError::DeviceFault`] if the transaction does not
/// complete within [`MDIO_TIMEOUT_US`] or the controller reports that no PHY
/// answered.
pub fn read<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    phy: u8,
    reg: u8,
) -> Result<u16, DriverError> {
    let command = regs::MDIO_RD
        | (u32::from(phy) & regs::MDIO_PMD_MASK) << regs::MDIO_PMD_SHIFT
        | (u32::from(reg) & regs::MDIO_REG_MASK) << regs::MDIO_REG_SHIFT;
    let done = transact(regs_io, delay, command)?;
    if done & regs::MDIO_READ_FAIL != 0 {
        return Err(DriverError::DeviceFault);
    }
    // The 16-bit result is returned in the low half of the command register.
    Ok(u16::try_from(done & regs::MDIO_DATA_MASK).unwrap_or(0))
}

/// Write `value` to clause-22 register `reg` of the PHY at `phy`.
///
/// # Errors
///
/// As [`read`], less the no-PHY report (a write returns no read status).
pub fn write<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    phy: u8,
    reg: u8,
    value: u16,
) -> Result<(), DriverError> {
    let command = regs::MDIO_WR
        | (u32::from(phy) & regs::MDIO_PMD_MASK) << regs::MDIO_PMD_SHIFT
        | (u32::from(reg) & regs::MDIO_REG_MASK) << regs::MDIO_REG_SHIFT
        | u32::from(value);
    transact(regs_io, delay, command)?;
    Ok(())
}

/// Post `command` to `MDIO_CMD`, start the transaction, and poll
/// `START_BUSY` to completion against a wall-clock deadline. Returns the
/// register's final value (which carries a read's data and status bits).
fn transact<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    command: u32,
) -> Result<u32, DriverError> {
    regs_io.write(regs::MDIO_CMD, command)?;
    regs_io.write(regs::MDIO_CMD, command | regs::MDIO_START_BUSY)?;
    let deadline = delay.now_us().saturating_add(MDIO_TIMEOUT_US);
    loop {
        let status = regs_io.read(regs::MDIO_CMD)?;
        if status & regs::MDIO_START_BUSY == 0 {
            return Ok(status);
        }
        if delay.now_us() >= deadline {
            return Err(DriverError::DeviceFault);
        }
        delay.delay_us(MDIO_POLL_INTERVAL_US);
    }
}

/// Reset the PHY, publish this driver's full 10/100/1000 advertisement, and
/// start autonegotiation.
///
/// # Errors
///
/// As [`read`]: a PHY that never answers, or never clears its self-clearing
/// reset within the clause-22 500 ms bound, is refused with
/// [`DriverError::DeviceFault`] rather than driven blind.
pub fn start_autoneg<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    phy: u8,
) -> Result<(), DriverError> {
    write(regs_io, delay, phy, BMCR, BMCR_RESET)?;
    let deadline = delay.now_us().saturating_add(PHY_RESET_TIMEOUT_US);
    loop {
        // `BMCR_RESET` is self-clearing: the PHY drops it when its internal
        // reset completes and its registers are readable again.
        if read(regs_io, delay, phy, BMCR)? & BMCR_RESET == 0 {
            break;
        }
        if delay.now_us() >= deadline {
            return Err(DriverError::DeviceFault);
        }
        delay.delay_us(PHY_RESET_POLL_INTERVAL_US);
    }

    // Advertise everything the MAC can carry, then (re)start negotiation.
    // The advertisement registers keep their reset-default reserved bits, so
    // the ability bits are merged in rather than written wholesale.
    let anar = read(regs_io, delay, phy, ANAR)? | ANAR_ADVERTISE;
    write(regs_io, delay, phy, ANAR, anar)?;
    let gbcr = read(regs_io, delay, phy, GBCR)? | GBCR_ADVERTISE;
    write(regs_io, delay, phy, GBCR, gbcr)?;
    let bmcr = (read(regs_io, delay, phy, BMCR)? & !BMCR_POWER_DOWN)
        | BMCR_ANEG_ENABLE
        | BMCR_ANEG_RESTART;
    write(regs_io, delay, phy, BMCR, bmcr)
}

/// Wait — bounded by [`AUTONEG_TIMEOUT_US`] — for autonegotiation to
/// complete, then resolve the link.
///
/// [`None`] means there is no link: either negotiation did not finish inside
/// the bound (no partner on the wire) or it finished with the link down.
/// That is a normal state, not a failure — the interface comes up link-down
/// and the PHY's link-change interrupt drives [`resolve`] again later.
///
/// # Errors
///
/// As [`read`].
pub fn await_link<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    phy: u8,
) -> Result<Option<Link>, DriverError> {
    let deadline = delay.now_us().saturating_add(AUTONEG_TIMEOUT_US);
    loop {
        let status = read(regs_io, delay, phy, BMSR)?;
        if status & BMSR_ANEG_COMPLETE != 0 && status & BMSR_LINK_UP != 0 {
            return resolve(regs_io, delay, phy);
        }
        if delay.now_us() >= deadline {
            return Ok(None);
        }
        delay.delay_us(AUTONEG_POLL_INTERVAL_US);
    }
}

/// Resolve the current link from the PHY's status registers, without
/// waiting.
///
/// Returns [`None`] when the link is down or negotiation has not completed.
/// Otherwise the highest-common-denominator mode of this driver's
/// advertisement and the partner's abilities: gigabit from the 1000BASE-T
/// status register first, then 100, then 10, full duplex preferred at each
/// rate. Because the advertisement published by [`start_autoneg`] is the
/// complete set, the partner's abilities alone decide the outcome.
///
/// # Errors
///
/// As [`read`].
pub fn resolve<R: GenetRegs, D: Delay>(
    regs_io: &mut R,
    delay: &D,
    phy: u8,
) -> Result<Option<Link>, DriverError> {
    let status = read(regs_io, delay, phy, BMSR)?;
    if status & BMSR_LINK_UP == 0 || status & BMSR_ANEG_COMPLETE == 0 {
        return Ok(None);
    }
    let gigabit = read(regs_io, delay, phy, GBSR)?;
    if gigabit & GBSR_LP_1000_FULL != 0 {
        return Ok(Some(Link {
            speed: LinkSpeed::Thousand,
            full_duplex: true,
        }));
    }
    if gigabit & GBSR_LP_1000_HALF != 0 {
        return Ok(Some(Link {
            speed: LinkSpeed::Thousand,
            full_duplex: false,
        }));
    }
    let partner = read(regs_io, delay, phy, ANLPAR)?;
    for (bit, speed, full_duplex) in [
        (ADV_100_FULL, LinkSpeed::Hundred, true),
        (ADV_100_HALF, LinkSpeed::Hundred, false),
        (ADV_10_FULL, LinkSpeed::Ten, true),
        (ADV_10_HALF, LinkSpeed::Ten, false),
    ] {
        if partner & bit != 0 {
            return Ok(Some(Link { speed, full_duplex }));
        }
    }
    // Negotiation completed and the link is up, but the partner advertised
    // no mode this MAC can carry: refuse to guess a rate.
    Ok(None)
}
