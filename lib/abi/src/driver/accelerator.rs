//! Accelerator driver class (`drivers/accelerator/*`).
//!
//! An accelerator is a device that performs bounded units of work off-CPU on
//! the driver's behalf: a symmetric-cipher offload, an NPU, a media engine.
//! What distinguishes the class from every other one here is that it computes
//! rather than moves — it neither presents pixels, carries frames, nor
//! addresses blocks — so the class surface is the device's own capability and
//! occupancy plus the workload families it offers.
//!
//! # The one workload family
//!
//! [`Accelerator::cipher`] is the only work operation, because a symmetric
//! cipher is the only workload family a device in this tree offers. Its
//! algorithm set is read from the device
//! ([`AcceleratorDeviceReport::ciphers`]) rather than assumed, so a device
//! offering none refuses the call closed instead of submitting work it cannot
//! do. A device that genuinely offers another family brings that family's
//! operation with it.
//!
//! # Keys are the caller's, and the driver holds none
//!
//! A [`CipherJob`] carries its key by reference for the duration of the call.
//! The driver stages it into device-visible memory to create the device's
//! session and scrubs that staging when the session is destroyed; it keeps no
//! copy of its own, so a compromised accelerator driver cannot replay a key it
//! was handed after the call that handed it returns.

use super::DriverError;

/// Symmetric block-cipher algorithms this class names.
///
/// A closed set: an algorithm is added when a device in the tree offers it
/// *and* the offer can be proven against that device, never so the vocabulary
/// looks complete.
#[repr(u16)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CipherAlgorithm {
    /// AES in cipher-block-chaining mode, with a 128-, 192- or 256-bit key
    /// and a 16-byte initialisation vector.
    AesCbc = 0,
}

impl CipherAlgorithm {
    /// Bit this algorithm occupies in a [`CipherAlgorithms`] set.
    const fn bit(self) -> u32 {
        1 << (self as u16)
    }

    /// The algorithm's initialisation-vector length in bytes.
    #[must_use]
    pub const fn iv_len(self) -> usize {
        match self {
            Self::AesCbc => 16,
        }
    }

    /// The algorithm's block length in bytes: an input length that is not a
    /// whole number of blocks has no cipher-text.
    #[must_use]
    pub const fn block_len(self) -> usize {
        match self {
            Self::AesCbc => 16,
        }
    }

    /// Whether `bytes` is a key length this algorithm accepts.
    #[must_use]
    pub const fn accepts_key_len(self, bytes: usize) -> bool {
        match self {
            Self::AesCbc => matches!(bytes, 16 | 24 | 32),
        }
    }
}

/// The set of [`CipherAlgorithm`]s a device offers.
///
/// A set rather than one algorithm because a device advertises its whole
/// offer in one register read, and a driver must be able to answer "can you
/// do this one?" before it stages a key.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct CipherAlgorithms(u32);

impl CipherAlgorithms {
    /// The empty set: a device that offers no symmetric cipher.
    pub const NONE: Self = Self(0);

    /// This set with `algorithm` added.
    #[must_use]
    pub const fn with(self, algorithm: CipherAlgorithm) -> Self {
        Self(self.0 | algorithm.bit())
    }

    /// Whether `algorithm` is in the set.
    #[must_use]
    pub const fn contains(self, algorithm: CipherAlgorithm) -> bool {
        self.0 & algorithm.bit() != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many algorithms the set names.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.0.count_ones()
    }
}

/// Which way a [`CipherJob`] runs.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum CipherDirection {
    /// Plain text in, cipher text out.
    Encrypt = 1,
    /// Cipher text in, plain text out.
    Decrypt = 2,
}

/// One unit of symmetric-cipher work.
///
/// The buffers are the caller's for the duration of the call; nothing here
/// outlives it. `output` is written only on success, so a refused job leaves
/// the caller's buffer as it found it rather than half-transformed.
pub struct CipherJob<'a> {
    /// Which algorithm to run. Refused [`DriverError::Unsupported`] unless
    /// [`AcceleratorDeviceReport::ciphers`] offers it.
    pub algorithm: CipherAlgorithm,
    /// Which way to run it.
    pub direction: CipherDirection,
    /// The key, of a length [`CipherAlgorithm::accepts_key_len`] admits.
    pub key: &'a [u8],
    /// The initialisation vector, of exactly [`CipherAlgorithm::iv_len`]
    /// bytes.
    pub iv: &'a [u8],
    /// The input, a whole number of [`CipherAlgorithm::block_len`] blocks.
    pub input: &'a [u8],
    /// Where the result goes. Must be exactly as long as `input`.
    pub output: &'a mut [u8],
}

impl CipherJob<'_> {
    /// Check the job against `ciphers` and against its own algorithm's
    /// shape, before any of it reaches a device.
    ///
    /// Every relation is checked here, once, so no driver re-derives the
    /// rules and none can admit a job another would refuse.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] — `ciphers` does not offer this
    ///   algorithm.
    /// * [`DriverError::OutOfRange`] — the key or the initialisation vector
    ///   is not a length the algorithm admits.
    /// * [`DriverError::BufferTooSmall`] — the input is empty, is not a
    ///   whole number of blocks, or the output is not the input's length.
    pub fn validate(&self, ciphers: CipherAlgorithms) -> Result<(), DriverError> {
        if !ciphers.contains(self.algorithm) {
            return Err(DriverError::Unsupported);
        }
        if !self.algorithm.accepts_key_len(self.key.len())
            || self.iv.len() != self.algorithm.iv_len()
        {
            return Err(DriverError::OutOfRange);
        }
        if self.input.is_empty()
            || !self.input.len().is_multiple_of(self.algorithm.block_len())
            || self.output.len() != self.input.len()
        {
            return Err(DriverError::BufferTooSmall);
        }
        Ok(())
    }
}

/// What an accelerator reports about *itself*: the memory it owns, the
/// workloads it offers, and the most work one job may carry.
///
/// The device half of the accelerator reading a monitor draws. Every field is
/// the driver's to state — nothing above it can measure them — and the
/// occupancy a reader wants beside them is the *hosting service's*, measured
/// by bracketing the work call, exactly as the display service measures its
/// device's.
///
/// An in-process driver-trait value, not a wire record.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AcceleratorDeviceReport {
    /// Bytes of the device's own memory currently holding job state.
    /// Necessarily `0` when [`Self::mem_total_bytes`] is.
    pub mem_resident_bytes: u64,
    /// Bytes of memory the device owns. `0` means the device has **no memory
    /// of its own** — it works out of system RAM — not that none is free.
    pub mem_total_bytes: u64,
    /// The symmetric-cipher algorithms it offers, empty where it offers none.
    pub ciphers: CipherAlgorithms,
    /// The most input bytes one job may carry. Never `0` for a device
    /// offering a workload: a driver that cannot learn the device's own
    /// ceiling publishes its own staging bound instead, so a caller always
    /// has a figure to plan against rather than a job size to discover by
    /// refusal.
    pub max_job_bytes: u64,
}

/// Trait every accelerator driver implements.
///
/// # Capabilities
///
/// Every method is gated by ownership of the
/// [`DriverHandle`](crate::driver::DriverHandle) returned from the driver's
/// `register` entry point. The load-time grant of
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD) is what permits
/// the host to issue that handle; the per-method dispatcher re-verifies the
/// handle on every call.
pub trait Accelerator {
    /// Report what the device owns and what it can be asked to do.
    fn device_report(&self) -> AcceleratorDeviceReport;

    /// Run one symmetric-cipher job to completion, writing the result into
    /// `job.output`.
    ///
    /// # Errors
    ///
    /// * Everything [`CipherJob::validate`] returns, checked before any of
    ///   the job reaches the device.
    /// * [`DriverError::LengthOutOfRange`] if the input exceeds the device's
    ///   own [`AcceleratorDeviceReport::max_job_bytes`] ceiling.
    /// * [`DriverError::DeviceFault`] if the device refused or failed the
    ///   job. `job.output` is untouched in that case.
    fn cipher(&mut self, job: CipherJob<'_>) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::{
        Accelerator, AcceleratorDeviceReport, CipherAlgorithm, CipherAlgorithms, CipherDirection,
        CipherJob,
    };
    use crate::driver::DriverError;

    fn aes() -> CipherAlgorithms {
        CipherAlgorithms::NONE.with(CipherAlgorithm::AesCbc)
    }

    fn job<'a>(
        key: &'a [u8],
        iv: &'a [u8],
        input: &'a [u8],
        output: &'a mut [u8],
    ) -> CipherJob<'a> {
        CipherJob {
            algorithm: CipherAlgorithm::AesCbc,
            direction: CipherDirection::Encrypt,
            key,
            iv,
            input,
            output,
        }
    }

    #[test]
    fn algorithm_set_holds_only_what_was_inserted() {
        assert!(CipherAlgorithms::NONE.is_empty());
        assert_eq!(CipherAlgorithms::NONE.len(), 0);
        assert!(!CipherAlgorithms::NONE.contains(CipherAlgorithm::AesCbc));
        assert!(aes().contains(CipherAlgorithm::AesCbc));
        assert_eq!(aes().len(), 1);
        assert!(!aes().is_empty());
        // Idempotent: inserting the same algorithm twice is still one.
        assert_eq!(aes().with(CipherAlgorithm::AesCbc), aes());
    }

    #[test]
    fn aes_cbc_admits_only_its_own_key_lengths() {
        for len in [16usize, 24, 32] {
            assert!(CipherAlgorithm::AesCbc.accepts_key_len(len));
        }
        for len in [0usize, 1, 15, 17, 20, 31, 33, 64] {
            assert!(!CipherAlgorithm::AesCbc.accepts_key_len(len));
        }
        assert_eq!(CipherAlgorithm::AesCbc.iv_len(), 16);
        assert_eq!(CipherAlgorithm::AesCbc.block_len(), 16);
    }

    #[test]
    fn a_job_is_refused_by_an_algorithm_the_device_does_not_offer() {
        let mut out = [0u8; 16];
        assert_eq!(
            job(&[0u8; 16], &[0u8; 16], &[0u8; 16], &mut out).validate(CipherAlgorithms::NONE),
            Err(DriverError::Unsupported)
        );
    }

    #[test]
    fn a_job_is_refused_on_a_key_or_iv_length_the_algorithm_rejects() {
        let mut out = [0u8; 16];
        assert_eq!(
            job(&[0u8; 17], &[0u8; 16], &[0u8; 16], &mut out).validate(aes()),
            Err(DriverError::OutOfRange)
        );
        let mut out = [0u8; 16];
        assert_eq!(
            job(&[0u8; 16], &[0u8; 15], &[0u8; 16], &mut out).validate(aes()),
            Err(DriverError::OutOfRange)
        );
    }

    #[test]
    fn a_job_is_refused_on_a_partial_block_or_a_mismatched_output() {
        let mut out = [0u8; 8];
        assert_eq!(
            job(&[0u8; 16], &[0u8; 16], &[0u8; 8], &mut out).validate(aes()),
            Err(DriverError::BufferTooSmall)
        );
        let mut out = [0u8; 32];
        assert_eq!(
            job(&[0u8; 16], &[0u8; 16], &[0u8; 16], &mut out).validate(aes()),
            Err(DriverError::BufferTooSmall)
        );
        let mut out = [0u8; 0];
        assert_eq!(
            job(&[0u8; 16], &[0u8; 16], &[], &mut out).validate(aes()),
            Err(DriverError::BufferTooSmall)
        );
    }

    #[test]
    fn a_well_formed_job_validates() {
        let mut out = [0u8; 32];
        assert_eq!(
            job(&[0u8; 32], &[0u8; 16], &[0u8; 32], &mut out).validate(aes()),
            Ok(())
        );
    }

    #[test]
    fn an_unbrought_up_device_reports_nothing_it_cannot_do() {
        struct Unbound;
        impl Accelerator for Unbound {
            fn device_report(&self) -> AcceleratorDeviceReport {
                AcceleratorDeviceReport {
                    mem_resident_bytes: 0,
                    mem_total_bytes: 0,
                    ciphers: CipherAlgorithms::NONE,
                    max_job_bytes: 0,
                }
            }
            fn cipher(&mut self, job: CipherJob<'_>) -> Result<(), DriverError> {
                job.validate(self.device_report().ciphers)
            }
        }
        let report = Unbound.device_report();
        assert!(report.ciphers.is_empty());
        assert_eq!(report.mem_total_bytes, 0);
        assert_eq!(report.mem_resident_bytes, 0);
        assert_eq!(report.max_job_bytes, 0);
        let mut out = [0u8; 16];
        assert_eq!(
            Unbound.cipher(job(&[0u8; 16], &[0u8; 16], &[0u8; 16], &mut out)),
            Err(DriverError::Unsupported)
        );
    }
}
