//! The Control/Bulk/Interrupt (CBI) wire transport (`plans/DEVICES.md` D5)
//! — the classic USB floppy-drive transport.
//!
//! A CBI device (USB Mass Storage Class Control/Bulk/Interrupt Transport
//! 1.1, protocol `0x00`) receives each command block over a class-specific
//! control-OUT request (ADSC — Accept Device-Specific Command), moves the
//! data phase over the bulk endpoint pair, and signals completion with a
//! two-byte block on its interrupt-IN endpoint. [`Cbi`] is that framing as
//! a [`ScsiTransport`], generic over the same [`MsdTransport`] seam as the
//! BOT transport, so it is proven host-side against a scripted device and
//! runs unchanged over the URB transport in the `Run` binary.
//!
//! The completion block's meaning depends on the device's command set
//! (CBI 1.1 §3.4.3.1): a UFI device reports the ASC/ASCQ of the completed
//! command (both zero = success), while every other command set reports a
//! typed status value. Both spellings are implemented, chosen by
//! [`CbiStatus`]; a phase error or persistent failure runs the spec's
//! Command Block Reset before the command fails. The device is hostile
//! input: a completion block of the wrong shape fails closed, never
//! guessed at.
//!
//! CBI addresses exactly one logical unit: the transport carries no LUN
//! field (a UFI command embeds its LUN in the command block, and CBI
//! floppy drives are single-unit devices), so any LUN other than `0` is
//! refused.

use rustos_abi::Errno;

use crate::bot::MsdTransport;
use crate::scsi::{CommandOutcome, DataPhase, ScsiTransport, Sense};

/// CBI command blocks are fixed 12-byte frames (CBI 1.1 §2.2 for the UFI
/// and SFF-8070i sets this transport serves).
pub const CBI_COMMAND_LEN: usize = 12;

/// The two-byte command-completion interrupt block (CBI 1.1 §3.4.3.1).
const CBI_STATUS_LEN: usize = 2;

/// How the device spells its command-completion interrupt block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CbiStatus {
    /// A UFI device reports `(ASC, ASCQ)` of the completed command; both
    /// zero means the command passed (CBI 1.1 §3.4.3.1.1).
    UfiSense,
    /// Every other command set reports a typed block: byte 0 must be zero
    /// (Command Completion Interrupt) and byte 1's low two bits carry the
    /// status value (CBI 1.1 §3.4.3.1.2).
    CommandStatus,
}

/// Typed status values of the [`CbiStatus::CommandStatus`] spelling
/// (values 2 and 3 — phase error and persistent failure — both run the
/// reset recovery and are matched as the remaining masked states).
const STATUS_VALUE_PASSED: u8 = 0;
const STATUS_VALUE_FAILED: u8 = 1;

/// One CBI mass-storage device: the ADSC/bulk/interrupt framing over a
/// transport.
pub struct Cbi<T: MsdTransport> {
    transport: T,
    /// `bInterfaceNumber` — the `wIndex` of the ADSC class request.
    interface_number: u8,
    /// The completion-block spelling the device's command set uses.
    status: CbiStatus,
}

impl<T: MsdTransport> Cbi<T> {
    /// Wrap a transport for the CBI interface `interface_number`, whose
    /// completion blocks are spelled per `status`.
    pub fn new(transport: T, interface_number: u8, status: CbiStatus) -> Self {
        Self {
            transport,
            interface_number,
            status,
        }
    }

    /// Borrow the underlying transport (the serve loop observes
    /// transport-level state through it).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Send one 12-byte command block over ADSC (CBI 1.1 §2.2): a
    /// class-specific control-OUT whose data stage is the command block.
    fn adsc(&mut self, block: &[u8; CBI_COMMAND_LEN]) -> Result<(), Errno> {
        let len = u16::try_from(CBI_COMMAND_LEN).map_err(|_| Errno::LengthOutOfRange)?;
        let setup = [
            0x21,
            0x00,
            0,
            0,
            self.interface_number,
            0,
            len.to_le_bytes()[0],
            len.to_le_bytes()[1],
        ];
        self.transport.control_out(setup, block)
    }

    /// Run the spec's Command Block Reset (CBI 1.1 §2.3): an ADSC whose
    /// command block is `SEND DIAGNOSTIC` with both reset pad patterns,
    /// and surface `err` (or the reset's own failure) so a broken exchange
    /// never silently "passes". The bulk endpoints' halts are recovered
    /// per-transfer by the transport below.
    fn recover(&mut self, err: Errno) -> Errno {
        let mut block = [0xFFu8; CBI_COMMAND_LEN];
        block[0] = 0x1D; // SEND DIAGNOSTIC
        block[1] = 0x04; // Self-test — the reset spelling the spec fixes.
        match self.adsc(&block) {
            Ok(()) => err,
            Err(reset_err) => reset_err,
        }
    }

    /// Read and interpret the command-completion interrupt block,
    /// returning whether the command passed.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for a block of the wrong size,
    /// [`Errno::BadMagic`] for a typed block of the wrong shape, and the
    /// reset-first surfaced error for a phase error / persistent failure.
    fn completion(&mut self) -> Result<bool, Errno> {
        let mut block = [0u8; CBI_STATUS_LEN];
        let received = self.transport.interrupt_in(&mut block)?;
        if received != CBI_STATUS_LEN {
            return Err(self.recover(Errno::LengthOutOfRange));
        }
        match self.status {
            CbiStatus::UfiSense => Ok(block[0] == 0 && block[1] == 0),
            CbiStatus::CommandStatus => {
                if block[0] != 0 {
                    return Err(self.recover(Errno::BadMagic));
                }
                match block[1] & 0x03 {
                    STATUS_VALUE_PASSED => Ok(true),
                    STATUS_VALUE_FAILED => Ok(false),
                    // Phase error / persistent failure: the transport is
                    // out of step; reset it and fail the command. The
                    // masked value cannot exceed 3, so these two arms
                    // cover every remaining state.
                    _ => Err(self.recover(Errno::NotImplemented)),
                }
            }
        }
    }
}

impl<T: MsdTransport> ScsiTransport for Cbi<T> {
    /// Run one CBI command: ADSC, data phase, completion interrupt.
    fn execute(
        &mut self,
        lun: u8,
        cb: &[u8],
        mut data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno> {
        // CBI carries no transport-level LUN; only unit 0 exists.
        if lun != 0 {
            return Err(Errno::OutOfRange);
        }
        if cb.is_empty() || cb.len() > CBI_COMMAND_LEN {
            return Err(Errno::OutOfRange);
        }
        let mut block = [0u8; CBI_COMMAND_LEN];
        block[..cb.len()].copy_from_slice(cb);

        match self.adsc(&block) {
            Ok(()) => {}
            // A protocol STALL on ADSC is the device's "command not
            // accepted" answer (CBI 1.1 §2.2); the control endpoint
            // recovers on the next SETUP, so this is a failed command,
            // not a transport fault.
            Err(Errno::EndpointStalled) => {
                return Ok(CommandOutcome {
                    passed: false,
                    transferred: 0,
                })
            }
            Err(err) => return Err(err),
        }

        // Data phase over the bulk pair. A STALL ends the phase early —
        // the completion interrupt still reports the command's verdict.
        let mut transferred = 0usize;
        match &mut data {
            DataPhase::In(buf) if !buf.is_empty() => match self.transport.bulk_in(buf) {
                Ok(n) => transferred = n,
                Err(Errno::EndpointStalled) => {}
                Err(err) => return Err(err),
            },
            DataPhase::Out(buf) if !buf.is_empty() => match self.transport.bulk_out(buf) {
                Ok(n) => transferred = n,
                Err(Errno::EndpointStalled) => {}
                Err(err) => return Err(err),
            },
            _ => {}
        }

        let passed = self.completion()?;
        Ok(CommandOutcome {
            passed,
            transferred,
        })
    }

    /// CBI exposes exactly one logical unit (the transport has no LUN
    /// discovery request).
    fn lun_count(&mut self) -> Result<u8, Errno> {
        Ok(1)
    }

    /// The UFI completion block carries only ASC/ASCQ, not a full sense
    /// triple; the command layer issues `REQUEST SENSE` for the whole
    /// story.
    fn take_sense(&mut self) -> Option<Sense> {
        None
    }

    fn scrub(&mut self) {
        self.transport.scrub();
    }
}

#[cfg(test)]
#[path = "cbi_tests.rs"]
mod tests;
