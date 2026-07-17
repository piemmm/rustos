//! Pure-Rust EL0 fixture for the syscall-continuation regression vertical.
//!
//! The program issues one ordinary `clock_get` syscall. Its test kernel
//! deliberately suspends the caller after computing the successful result,
//! dispatches a competing child that parks, and only then resumes this frame.
//! Returning zero proves the result register survived that round trip; any
//! other value returns a distinct non-zero process status.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

#[cfg(freestanding)]
mod program {
    /// Sentinel the test kernel returns from the ordinary syscall.
    const EXPECTED_READING: u64 = 0x51a7_c011_71a0_0001;

    /// Issue the ordinary syscall and report whether its result survived the
    /// scheduler round trip.
    fn main() -> i32 {
        if rustos_rt::clock_get() == EXPECTED_READING {
            0
        } else {
            1
        }
    }

    rustos_rt::entry!(main);
}

#[cfg(not(freestanding))]
fn main() {}
