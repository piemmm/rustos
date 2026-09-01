//! TAIRiX I²C register-transaction protocol.
//!
//! I²C parts are register files: a transaction names a register and the part
//! auto-increments the pointer across the bytes that follow. Composing that
//! onto the [`I2cPort`] transfer seam is the same three lines for every chip,
//! so it is written once here and each chip driver contributes only its own
//! register map and quirks — the split `lib/usb` and `lib/virtio` already
//! make between a bus protocol and the devices on it.
//!
//! # Why one indivisible transfer
//!
//! Reading a register block is a *write-then-read*: the pointer write and the
//! read-back are one request, so no other transfer can be interleaved between
//! them and return some other register's contents — a wrong clock rather than
//! an error. [`Device`] therefore never splits them.
//!
//! # What a caller holds
//!
//! A [`Device`] is one [`I2cPort`], which is the whole authority a chip driver
//! has: on the real bus that port is the transfer endpoint its bus driver
//! serves it on, and no address crosses that wire, so a chip driver cannot
//! reach a neighbour however it is compromised. The same [`Device`] runs
//! unchanged against a mock part in a host test.
//!
//! Reference: the I²C-bus specification, NXP UM10204.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub use tairix_abi::driver::i2c::{I2cPort, MAX_TRANSFER_LEN};
use tairix_abi::DriverError;

/// The largest register block one transaction can carry.
///
/// One byte of the write phase is spent on the register pointer, so a block
/// write reaches one byte less than the phase bound. Derived rather than
/// restated, so it cannot drift from the seam's own bound.
pub const MAX_BLOCK_LEN: usize = MAX_TRANSFER_LEN - 1;

/// One register-addressed part, reached over its own transfer port.
///
/// Copy-cheap over a borrowed port, so a driver can hold one per functional
/// block of a chip that answers on several ports.
#[derive(Copy, Clone, Debug)]
pub struct Device<P> {
    port: P,
}

impl<P: I2cPort> Device<P> {
    /// Bind the register protocol to `port`.
    pub const fn new(port: P) -> Self {
        Self { port }
    }

    /// Read `out.len()` bytes starting at register `first`, letting the
    /// chip's pointer auto-increment across them.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if `out` is longer than
    /// [`MAX_TRANSFER_LEN`]; otherwise whatever the port reports
    /// ([`I2cPort::transfer`]). `out` is left untouched on every failure.
    pub fn read(&self, first: u8, out: &mut [u8]) -> Result<(), DriverError> {
        if out.len() > MAX_TRANSFER_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        self.port.transfer(&[first], out)
    }

    /// Read one register.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read).
    pub fn read_one(&self, register: u8) -> Result<u8, DriverError> {
        let mut byte = [0u8];
        self.read(register, &mut byte)?;
        Ok(byte[0])
    }

    /// Write `bytes` starting at register `first`.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if `bytes` is longer than
    /// [`MAX_BLOCK_LEN`] (the pointer byte shares the write phase);
    /// otherwise whatever the port reports ([`I2cPort::transfer`]).
    pub fn write(&self, first: u8, bytes: &[u8]) -> Result<(), DriverError> {
        if bytes.len() > MAX_BLOCK_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut frame = [0u8; MAX_TRANSFER_LEN];
        frame[0] = first;
        frame[1..=bytes.len()].copy_from_slice(bytes);
        self.port.transfer(&frame[..=bytes.len()], &mut [])
    }

    /// Write one register.
    ///
    /// # Errors
    ///
    /// As [`write`](Self::write).
    pub fn write_one(&self, register: u8, value: u8) -> Result<(), DriverError> {
        self.write(register, &[value])
    }

    /// Read one register, apply `update` to it, and write the result back
    /// only if it changed.
    ///
    /// The read-modify-write every status/control flag clear needs, in one
    /// place: writing a whole control register back from a driver's own idea
    /// of its contents would clobber bits the chip owns.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read) and [`write`](Self::write).
    pub fn update_one<F: FnOnce(u8) -> u8>(
        &self,
        register: u8,
        update: F,
    ) -> Result<(), DriverError> {
        let current = self.read_one(register)?;
        let next = update(current);
        if next == current {
            return Ok(());
        }
        self.write_one(register, next)
    }
}

#[cfg(any(test, feature = "mock-bus"))]
pub mod mock;

#[cfg(test)]
mod tests;
