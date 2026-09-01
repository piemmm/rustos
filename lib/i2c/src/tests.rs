//! Host tests for the register-transaction protocol.

use core::cell::RefCell;

use tairix_abi::driver::i2c::{I2cPort, MAX_TRANSFER_LEN};
use tairix_abi::DriverError;

use crate::mock::{MockPart, REGISTER_COUNT};
use crate::{Device, MAX_BLOCK_LEN};

fn device() -> Device<MockPart> {
    Device::new(MockPart::new())
}

#[test]
fn a_block_read_is_one_transfer_naming_the_first_register() {
    /// Records the phases so the test can assert the transaction's *shape*,
    /// not merely its result.
    struct Recorder(RefCell<Option<([u8; MAX_TRANSFER_LEN], usize, usize)>>);

    impl I2cPort for Recorder {
        fn transfer(&self, write: &[u8], read: &mut [u8]) -> Result<(), DriverError> {
            let mut copy = [0u8; MAX_TRANSFER_LEN];
            copy[..write.len()].copy_from_slice(write);
            assert!(
                self.0.borrow().is_none(),
                "a register read must not be split into two transactions"
            );
            *self.0.borrow_mut() = Some((copy, write.len(), read.len()));
            Ok(())
        }
    }

    let recorder = Device::new(Recorder(RefCell::new(None)));
    let mut out = [0u8; 7];
    recorder.read(0x00, &mut out).expect("reads");
    let (write, write_len, read_len) = recorder.port.0.borrow().expect("the port was driven");
    assert_eq!(write_len, 1, "the write phase is the pointer alone");
    assert_eq!(write[0], 0x00);
    assert_eq!(read_len, 7);
}

#[test]
fn a_block_read_walks_the_registers_from_the_named_one() {
    let d = device();
    d.port.seed(0x00, &[1, 2, 3, 4, 5, 6, 7]);
    let mut out = [0u8; 4];
    d.read(0x02, &mut out).expect("reads");
    assert_eq!(out, [3, 4, 5, 6]);
    assert_eq!(d.read_one(0x06), Ok(7));
}

#[test]
fn a_block_write_lands_at_successive_registers_and_reads_back() {
    let d = device();
    d.write(0x03, &[0xAA, 0xBB, 0xCC]).expect("writes");
    assert_eq!(d.port.register(0x03), 0xAA);
    assert_eq!(d.port.register(0x04), 0xBB);
    assert_eq!(d.port.register(0x05), 0xCC);
    let mut out = [0u8; 3];
    d.read(0x03, &mut out).expect("reads");
    assert_eq!(out, [0xAA, 0xBB, 0xCC]);
    d.write_one(0x03, 0x11).expect("writes");
    assert_eq!(d.read_one(0x03), Ok(0x11));
}

#[test]
fn a_block_longer_than_one_transaction_can_carry_is_refused() {
    let d = device();
    // The write phase spends one byte on the pointer, so the block bound is
    // one below the phase bound.
    assert_eq!(MAX_BLOCK_LEN, MAX_TRANSFER_LEN - 1);
    assert_eq!(
        d.write(0x00, &[0u8; MAX_BLOCK_LEN + 1]),
        Err(DriverError::LengthOutOfRange)
    );
    d.write(0x00, &[0u8; MAX_BLOCK_LEN])
        .expect("the bound fits");
    assert_eq!(
        d.read(0x00, &mut [0u8; MAX_TRANSFER_LEN + 1]),
        Err(DriverError::LengthOutOfRange)
    );
}

#[test]
fn a_refusal_leaves_the_callers_buffer_untouched() {
    let d = device();
    d.port.seed(0x00, &[9, 9, 9]);
    d.port.fail_with(DriverError::DeviceFault);
    let mut out = [0xEEu8; 3];
    assert_eq!(d.read(0x00, &mut out), Err(DriverError::DeviceFault));
    assert_eq!(out, [0xEE; 3], "no half-read is mistaken for data");
    assert_eq!(d.read_one(0x00), Err(DriverError::DeviceFault));
    assert_eq!(d.write_one(0x00, 1), Err(DriverError::DeviceFault));
}

#[test]
fn a_read_modify_write_reads_once_and_writes_only_a_change() {
    let d = device();
    d.port.seed(0x0E, &[0b1000_0000]);

    // Clearing a bit that is set costs a read and a write.
    d.update_one(0x0E, |v| v & !0b1000_0000).expect("updates");
    assert_eq!(d.port.register(0x0E), 0);
    assert_eq!(d.port.transfers(), 2);

    // Clearing it again reads and stops: no needless bus traffic, and no
    // write-back of bits the chip may have changed under us.
    d.update_one(0x0E, |v| v & !0b1000_0000).expect("updates");
    assert_eq!(d.port.transfers(), 3);

    // The update sees the chip's own current value, not a driver's guess.
    d.port.seed(0x0E, &[0b0000_0011]);
    d.update_one(0x0E, |v| v | 0b0001_0000).expect("updates");
    assert_eq!(d.port.register(0x0E), 0b0001_0011);
}

#[test]
fn a_failing_update_never_writes() {
    let d = device();
    d.port.seed(0x0E, &[0x55]);
    d.port.fail_with(DriverError::NotFound);
    assert_eq!(
        d.update_one(0x0E, |_| 0xFF),
        Err(DriverError::NotFound),
        "the read must fail closed"
    );
    d.port.fail_with(DriverError::DeviceFault);
    assert_eq!(d.port.register(0x0E), 0x55);
}

#[test]
fn the_mock_pointer_wraps_within_the_register_file() {
    let d = device();
    d.port.seed(0x00, &[0x77]);
    let mut out = [0u8; 2];
    let last = u8::try_from(REGISTER_COUNT - 1).expect("small");
    d.read(last, &mut out).expect("reads");
    assert_eq!(out[1], 0x77, "the pointer wraps as the chip's does");
}
