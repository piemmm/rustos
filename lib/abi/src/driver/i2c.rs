//! I²C transfer seam (`abi-v1`) — the driver-class half of the bus
//! (`plans/TIMESYNC.md` TS-4).
//!
//! I²C carries no enumeration protocol: a controller cannot ask the bus what
//! is on it, and probing every address is forbidden and can be destructive on
//! a write-only register block. Discovery therefore names each child in the
//! platform's device tree, and the two halves of a child's existence are
//! split so neither side can overreach — the **duty** to serve it goes to the
//! bus driver ([`crate::hwtree::HwResourceKind::BusChild`], carrying the
//! endpoint id *and* the bus address), while the **authority** to use it goes
//! to the chip driver as a plain endpoint grant naming only that id.
//!
//! [`I2cPort`] is that authority's shape, and the reason it carries **no
//! address**: a port *is* one part, so a chip driver has no field in which it
//! could name a neighbour, whatever it believes about the bus. The bus driver
//! supplies the address from its own duty grant, on the endpoint the request
//! arrived on. A chip driver therefore cannot reach another chip even if it
//! is compromised outright.
//!
//! [`I2cAddress`] exists for the *controller* side of that split: it is what
//! a bus driver validates a duty grant's raw address into before it drives
//! the bus.

use crate::{DriverError, Errno};

/// Largest payload one phase of a transfer may carry, in bytes.
///
/// A fixed validation bound, not a capacity: the `SMBus` block protocol caps
/// a block at 32 bytes, no register block this class reaches is longer, and the
/// wire frame is one fixed size so an endpoint needs one bound. Widening it
/// to fit a caller is a defect.
pub const MAX_TRANSFER_LEN: usize = 32;

/// Lowest address a device may answer to: everything below is reserved by the
/// I²C specification (general call, START byte, CBUS, Hs-mode master codes).
const FIRST_DEVICE_ADDRESS: u8 = 0x08;

/// Highest address a device may answer to: `1111 0xx` is the 10-bit
/// addressing escape and `1111 1xx` is the device-ID prefix.
const LAST_DEVICE_ADDRESS: u8 = 0x77;

/// A validated 7-bit I²C device address.
///
/// Construction refuses every address the I²C-bus specification reserves
/// (NXP UM10204 §3.1.12), so a transfer can never be aimed at the general
/// call, a Hs-mode master code, or the 10-bit escape — each of which would
/// address something other than the part the discovery data meant. 10-bit
/// addressing is deliberately out of scope: no part this class reaches uses
/// it, and admitting the escape prefix as an ordinary address would silently
/// mis-address the bus.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct I2cAddress(u8);

impl I2cAddress {
    /// Validate `raw` as a 7-bit device address.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a reserved address or one wider than seven
    /// bits.
    pub const fn new(raw: u8) -> Result<Self, Errno> {
        if raw < FIRST_DEVICE_ADDRESS || raw > LAST_DEVICE_ADDRESS {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Validate the raw address a [`BusChild`](crate::hwtree::HwResourceKind::BusChild)
    /// duty grant carries.
    ///
    /// The grant's field is 64 bits wide because the kind serves every
    /// addressed bus; an I²C bus driver narrows it here rather than
    /// truncating, so a malformed tree leaves a child unserved instead of
    /// pointing the controller at some other part.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if the value is not a usable 7-bit address.
    pub const fn from_bus_address(raw: u64) -> Result<Self, Errno> {
        if raw > u8::MAX as u64 {
            return Err(Errno::OutOfRange);
        }
        // The bound above makes the narrowing total; `as` keeps this usable
        // in a `const fn`, where `u8::try_from` is not.
        #[allow(clippy::cast_possible_truncation)]
        Self::new(raw as u8)
    }

    /// The validated 7-bit address.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// The transfer path to **one** addressed part.
///
/// One write phase then one read phase, without releasing the transaction
/// between them: that is what every register-addressed part needs, because
/// the write carries the register pointer and the read takes the block back.
/// Either phase may be empty.
///
/// There is deliberately no address argument. On the real bus a port is the
/// per-child transfer endpoint the bus driver serves, and the address lives
/// only in that driver's duty grant — so the authority a chip driver holds is
/// exactly one part.
pub trait I2cPort {
    /// Run one write-then-read transfer against this part.
    ///
    /// An empty `write` skips the write phase and an empty `read` skips the
    /// read phase; both empty addresses the part and stops, which is how a
    /// caller checks that it answers at all. On `Ok` the whole of `read` has
    /// been filled — a short answer is a failure, never a partially-filled
    /// buffer.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if either phase exceeds
    ///   [`MAX_TRANSFER_LEN`].
    /// * [`DriverError::NotFound`] if the part did not acknowledge its
    ///   address.
    /// * [`DriverError::DeviceFault`] for a data acknowledge failure, a
    ///   clock-stretch timeout, a short transfer, or any other bus fault.
    /// * [`DriverError::PermissionDenied`] if the caller may not reach the
    ///   transfer endpoint.
    fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError>;
}

/// A borrowed port is a port, so a caller can keep the part and hand the
/// driver a reference to it.
impl<T: I2cPort + ?Sized> I2cPort for &T {
    fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError> {
        (**self).transfer(write, read)
    }
}

#[cfg(test)]
mod tests {
    use super::{I2cAddress, FIRST_DEVICE_ADDRESS, LAST_DEVICE_ADDRESS, MAX_TRANSFER_LEN};
    use crate::Errno;

    #[test]
    fn only_the_unreserved_seven_bit_addresses_are_accepted() {
        for raw in 0u8..=u8::MAX {
            let usable = (FIRST_DEVICE_ADDRESS..=LAST_DEVICE_ADDRESS).contains(&raw);
            assert_eq!(
                I2cAddress::new(raw).is_ok(),
                usable,
                "{raw:#04x} acceptance"
            );
        }
        // The parts this class reaches: the DS3231/PCF8523 pair and the
        // PCF85063A.
        for raw in [0x68u8, 0x51] {
            assert_eq!(I2cAddress::new(raw).map(I2cAddress::get), Ok(raw));
        }
    }

    #[test]
    fn a_duty_grants_wide_address_field_is_narrowed_rather_than_truncated() {
        assert_eq!(
            I2cAddress::from_bus_address(0x68),
            I2cAddress::new(0x68),
            "an ordinary address survives the narrowing"
        );
        // A value wider than an address, one that would truncate onto a
        // usable address, and a reserved one all fail closed rather than
        // pointing the controller at some other part.
        for raw in [0x168u64, 0x1_0000_0068, u64::MAX, 0x00, 0x7F] {
            assert_eq!(
                I2cAddress::from_bus_address(raw),
                Err(Errno::OutOfRange),
                "{raw:#x} must not become an address"
            );
        }
    }

    #[test]
    fn the_phase_bound_is_the_smbus_block_maximum() {
        assert_eq!(MAX_TRANSFER_LEN, 32);
    }
}
