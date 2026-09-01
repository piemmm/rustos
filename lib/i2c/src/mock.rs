//! A protocol-faithful register-file part double for host tests.
//!
//! Every I²C chip driver needs the same test scaffold — a part whose
//! registers a test seeds and inspects, with the pointer auto-increment the
//! real chip performs — so it is defined once here rather than copied into
//! each driver's tests.
//!
//! Faithful in the ways that catch real defects: a write sticks, so a
//! driver's set-then-read round trip is real; the pointer wraps within the
//! register file exactly as the chip's does; and a programmed fault surfaces
//! from the transfer rather than from a later decode.

use core::cell::RefCell;

use tairix_abi::driver::i2c::{I2cPort, MAX_TRANSFER_LEN};
use tairix_abi::DriverError;

/// Registers the modelled part exposes. Covers every register file this
/// class of chip has (the DS3231's `0x00..=0x12` is the longest) with room
/// to spare, and makes the pointer wrap observable.
pub const REGISTER_COUNT: usize = 32;

/// A single I²C part: one register file behind one transfer port.
#[derive(Default)]
pub struct MockPart {
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    registers: [u8; REGISTER_COUNT],
    fault: Option<DriverError>,
    transfers: usize,
}

impl MockPart {
    /// A part with every register zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed `bytes` starting at register `first`.
    ///
    /// # Panics
    ///
    /// If the block runs past the register file — a test asking for a
    /// register the modelled part does not have is a broken test.
    pub fn seed(&self, first: u8, bytes: &[u8]) {
        let at = usize::from(first);
        self.state.borrow_mut().registers[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// The current contents of register `at`.
    ///
    /// # Panics
    ///
    /// If `at` is past the register file.
    #[must_use]
    pub fn register(&self, at: u8) -> u8 {
        self.state.borrow().registers[usize::from(at)]
    }

    /// Make every subsequent transfer fail with `error`, as a bus fault or
    /// an absent part would.
    pub fn fail_with(&self, error: DriverError) {
        self.state.borrow_mut().fault = Some(error);
    }

    /// How many transfers the part has been asked to run — the count a test
    /// asserts a read-modify-write did not pay twice.
    #[must_use]
    pub fn transfers(&self) -> usize {
        self.state.borrow().transfers
    }
}

impl I2cPort for MockPart {
    fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError> {
        if write.len() > MAX_TRANSFER_LEN || read.len() > MAX_TRANSFER_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut state = self.state.borrow_mut();
        state.transfers += 1;
        if let Some(fault) = state.fault {
            return Err(fault);
        }
        // The first written byte is the register pointer; the rest lands at
        // successive registers, wrapping within the file as the chip's own
        // pointer does.
        let first = usize::from(*write.first().unwrap_or(&0));
        for (step, byte) in write.iter().skip(1).enumerate() {
            state.registers[(first + step) % REGISTER_COUNT] = *byte;
        }
        // A read phase resumes at the pointer the write phase named, which
        // is what a register read's repeated start does.
        for (step, byte) in read.iter_mut().enumerate() {
            *byte = state.registers[(first + step) % REGISTER_COUNT];
        }
        Ok(())
    }
}
