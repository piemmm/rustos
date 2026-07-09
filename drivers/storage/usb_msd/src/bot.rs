//! The Bulk-Only Transport (BOT) + SCSI engine (`plans/DEVICES.md` D2).
//!
//! USB mass-storage "bulk-only" devices (USB Mass Storage Class Bulk-Only
//! Transport 1.0) wrap every SCSI command in a 31-byte Command Block
//! Wrapper (CBW) on the bulk-OUT endpoint, move the data phase over the
//! bulk pair, and answer with a 13-byte Command Status Wrapper (CSW) on
//! bulk-IN. This module is that protocol plus the SCSI transparent subset
//! a disk needs, generic over the [`MsdTransport`] seam so it is proven
//! host-side against a scripted device and runs unchanged over the URB
//! transport in the `Run` binary.
//!
//! Every device-supplied field — CSW signature/tag/residue/status, INQUIRY
//! and capacity payloads, sense data, the LUN count — is bounds-checked
//! fail-closed: the device is hostile input. A tag mismatch, a corrupt
//! CSW, or a phase error runs the spec's reset recovery (Bulk-Only Mass
//! Storage Reset; the transport below already recovers a halted endpoint
//! per transfer) and fails the command rather than trusting the frame.

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::BufferClass;
use rustos_abi::{DriverError, Errno};

/// One logical bulk transfer seam the engine drives.
///
/// The `Run` binary implements it over the URB transport client (splitting
/// a transfer into per-URB chunks and bounce-copying through the shared
/// buffer); a host test implements it as a scripted device. A transfer
/// moves *up to* `data.len()` bytes and reports the honest count (a short
/// bulk-IN ends the data phase early); a halted endpoint surfaces as
/// [`Errno::EndpointStalled`] with the endpoint already recovered below,
/// so the engine may transfer again immediately.
pub trait MsdTransport {
    /// Run a control-IN transfer (SETUP + IN data stage) into `data`,
    /// returning the bytes the device delivered.
    ///
    /// # Errors
    ///
    /// An [`Errno`] from the transport/device.
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, Errno>;

    /// Run a no-data control transfer (SETUP + status stage only).
    ///
    /// # Errors
    ///
    /// An [`Errno`] from the transport/device.
    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), Errno>;

    /// Read up to `data.len()` bytes from the bulk-IN endpoint, returning
    /// the bytes delivered (a short transfer ends the phase early).
    ///
    /// # Errors
    ///
    /// [`Errno::EndpointStalled`] for a device STALL (already recovered),
    /// or any other transport [`Errno`].
    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize, Errno>;

    /// Write `data` to the bulk-OUT endpoint, returning the bytes the
    /// device accepted.
    ///
    /// # Errors
    ///
    /// As [`Self::bulk_in`].
    fn bulk_out(&mut self, data: &[u8]) -> Result<usize, Errno>;

    /// Zero every byte of the transport's shared data window, so a
    /// sensitive payload does not outlive the block operation that moved
    /// it (the zero-on-free discipline applied to the bounce window).
    fn scrub(&mut self);
}

/// CBW length (USB MSC BOT 1.0 §5.1).
pub const CBW_LEN: usize = 31;
/// CBW signature `USBC` (little-endian).
const CBW_SIGNATURE: u32 = 0x4342_5355;
/// CBW flags: data phase from device to host.
const CBW_FLAGS_IN: u8 = 0x80;
/// CSW length (BOT §5.2).
pub const CSW_LEN: usize = 13;
/// CSW signature `USBS` (little-endian).
const CSW_SIGNATURE: u32 = 0x5342_5355;
/// CSW status: command passed.
const CSW_STATUS_PASSED: u8 = 0;
/// CSW status: command failed (sense data pending).
const CSW_STATUS_FAILED: u8 = 1;
/// CSW status: phase error — the transport is out of step; reset it.
const CSW_STATUS_PHASE_ERROR: u8 = 2;

/// Most logical units a BOT device can expose (`GET MAX LUN` returns
/// `0..=15`, BOT §3.2). A fixed protocol bound, not a scalable capacity.
pub const MAX_LUNS: usize = 16;

/// Most bytes one SCSI READ/WRITE issued by [`LunBlock`] covers: the
/// block-service data window ([`rustos_abi::blkio::BLK_DATA_LEN`] — one
/// definition), so a Block call of any size splits into bounded commands
/// and per-device cost never scales with request length.
pub const MSD_MAX_TRANSFER_LEN: usize = rustos_abi::blkio::BLK_DATA_LEN;

/// Fixed-format sense data (SPC-4 §4.5.3): the why of a failed command.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Sense {
    /// Sense key (low nibble of byte 2).
    pub key: u8,
    /// Additional sense code.
    pub asc: u8,
    /// Additional sense code qualifier.
    pub ascq: u8,
}

/// Sense key: the addressed unit is not ready.
const SENSE_KEY_NOT_READY: u8 = 0x02;
/// Sense key: the command was illegal for this device.
const SENSE_KEY_ILLEGAL_REQUEST: u8 = 0x05;
/// Sense key: unit attention (media change / reset notification).
const SENSE_KEY_UNIT_ATTENTION: u8 = 0x06;
/// Sense key: the medium is write-protected.
const SENSE_KEY_DATA_PROTECT: u8 = 0x07;

/// What a LUN's standard INQUIRY reported.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Inquiry {
    /// Peripheral device type (byte 0 low five bits); `0x00` is a
    /// direct-access block device (a disk).
    pub device_type: u8,
    /// Whether the medium is removable (byte 1 bit 7).
    pub removable: bool,
}

/// Peripheral device type of a direct-access block device.
pub const DEVICE_TYPE_DIRECT_ACCESS: u8 = 0x00;

/// A logical unit's brought-up state: its geometry and write policy.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LunState {
    /// The unit's block geometry, read from `READ CAPACITY`.
    pub geometry: BlockGeometry,
    /// Whether the medium is write-protected (`MODE SENSE(6)` WP bit);
    /// writes are refused driver-side before touching the device.
    pub write_protected: bool,
}

/// The outcome of one BOT command: whether the device reported success and
/// how many data-phase bytes actually moved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct CommandOutcome {
    passed: bool,
    transferred: usize,
}

/// The data phase of one BOT command.
enum DataPhase<'a> {
    /// Device-to-host data of the given length.
    In(&'a mut [u8]),
    /// Host-to-device data.
    Out(&'a [u8]),
    /// No data phase.
    None,
}

impl DataPhase<'_> {
    fn len(&self) -> usize {
        match self {
            DataPhase::In(buf) => buf.len(),
            DataPhase::Out(buf) => buf.len(),
            DataPhase::None => 0,
        }
    }

    const fn is_in(&self) -> bool {
        matches!(self, DataPhase::In(_) | DataPhase::None)
    }
}

/// One bulk-only mass-storage device: the BOT engine over a transport.
pub struct Msd<T: MsdTransport> {
    transport: T,
    /// `bInterfaceNumber` — the `wIndex` of the class requests.
    interface_number: u8,
    /// The next CBW tag; incremented per command so a stale CSW can never
    /// match the command in flight.
    next_tag: u32,
}

impl<T: MsdTransport> Msd<T> {
    /// Wrap a transport for the mass-storage interface `interface_number`.
    pub fn new(transport: T, interface_number: u8) -> Self {
        Self {
            transport,
            interface_number,
            next_tag: 1,
        }
    }

    /// Number of logical units the device exposes (`GET MAX LUN + 1`).
    ///
    /// A device that refuses the request (many single-LUN sticks STALL it,
    /// BOT §3.2) reports one LUN — the spec's meaning of the refusal. A
    /// delivered value above 15 is a protocol violation and fails closed.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a LUN count outside the protocol bound.
    pub fn lun_count(&mut self) -> Result<u8, Errno> {
        // GET MAX LUN: class-specific IN, bRequest 0xFE, one data byte.
        let setup = [0xA1, 0xFE, 0, 0, self.interface_number, 0, 1, 0];
        let mut data = [0u8; 1];
        match self.transport.control_in(setup, &mut data) {
            Ok(1) => {
                if data[0] as usize >= MAX_LUNS {
                    return Err(Errno::OutOfRange);
                }
                Ok(data[0] + 1)
            }
            // A refusal (or a zero-byte answer) means "no multi-LUN
            // support": exactly one LUN, per the BOT spec.
            Ok(_) | Err(_) => Ok(1),
        }
    }

    /// Run the spec's reset recovery — a Bulk-Only Mass Storage Reset on
    /// the interface — and surface `err` (or the reset's own failure) so a
    /// broken exchange never silently "passes".
    fn recover(&mut self, err: Errno) -> Errno {
        // Bulk-Only Mass Storage Reset: class-specific OUT, bRequest 0xFF,
        // no data. The transport below already recovers halted endpoints
        // per transfer, so no separate clear-halt step is issued here.
        let setup = [0x21, 0xFF, 0, 0, self.interface_number, 0, 0, 0];
        match self.transport.control_no_data(setup) {
            Ok(()) => err,
            Err(reset_err) => reset_err,
        }
    }

    /// Run one BOT command: CBW, data phase, CSW, with the spec's
    /// stall/retry/reset handling and every device field validated.
    fn command(
        &mut self,
        lun: u8,
        cb: &[u8],
        mut data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno> {
        if cb.is_empty() || cb.len() > 16 || lun as usize >= MAX_LUNS {
            return Err(Errno::OutOfRange);
        }
        let data_len = u32::try_from(data.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let tag = self.next_tag;
        self.next_tag = self.next_tag.wrapping_add(1);

        // Build and send the CBW. A STALL here means the device rejected
        // the wrapper itself: reset the transport and fail.
        let mut cbw = [0u8; CBW_LEN];
        cbw[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&data_len.to_le_bytes());
        cbw[12] = if data.is_in() { CBW_FLAGS_IN } else { 0 };
        cbw[13] = lun;
        cbw[14] = u8::try_from(cb.len()).map_err(|_| Errno::OutOfRange)?;
        cbw[15..15 + cb.len()].copy_from_slice(cb);
        match self.transport.bulk_out(&cbw) {
            Ok(CBW_LEN) => {}
            Ok(_) => return Err(self.recover(Errno::NotImplemented)),
            Err(err) => return Err(self.recover(err)),
        }

        // Data phase. A STALL ends the phase early (the device will report
        // the residue in the CSW); any other fault is a hard error.
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

        // CSW, retried once across a STALL (BOT §6.7.2 figure 2), then
        // validated field by field before anything is believed.
        let mut csw = [0u8; CSW_LEN];
        let received = match self.transport.bulk_in(&mut csw) {
            Ok(n) => n,
            Err(Errno::EndpointStalled) => match self.transport.bulk_in(&mut csw) {
                Ok(n) => n,
                Err(err) => return Err(self.recover(err)),
            },
            Err(err) => return Err(err),
        };
        if received != CSW_LEN {
            return Err(self.recover(Errno::LengthOutOfRange));
        }
        let signature = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
        let csw_tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
        let residue = u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]);
        let status = csw[12];
        if signature != CSW_SIGNATURE || csw_tag != tag || residue > data_len {
            return Err(self.recover(Errno::BadMagic));
        }
        // The device's honest data count: never more than the bytes that
        // actually moved, never more than the CSW says remained unmoved.
        let reported = (data_len - residue) as usize;
        let transferred = transferred.min(reported);
        match status {
            CSW_STATUS_PASSED => Ok(CommandOutcome {
                passed: true,
                transferred,
            }),
            CSW_STATUS_FAILED => Ok(CommandOutcome {
                passed: false,
                transferred,
            }),
            CSW_STATUS_PHASE_ERROR => Err(self.recover(Errno::NotImplemented)),
            _ => Err(self.recover(Errno::BadMagic)),
        }
    }

    /// Standard INQUIRY: what kind of unit this LUN is.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] if the device fails the command,
    /// [`Errno::LengthOutOfRange`] for a response too short to carry the
    /// identity bytes, or a transport error.
    pub fn inquiry(&mut self, lun: u8) -> Result<Inquiry, Errno> {
        // INQUIRY (SPC-4 §6.6): standard data, 36-byte allocation.
        let mut data = [0u8; 36];
        let cb = [0x12, 0, 0, 0, 36, 0];
        let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
        if !outcome.passed {
            return Err(Errno::NotImplemented);
        }
        if outcome.transferred < 2 {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Inquiry {
            device_type: data[0] & 0x1F,
            removable: data[1] & 0x80 != 0,
        })
    }

    /// TEST UNIT READY: whether the LUN can accept media commands now.
    ///
    /// A failed command is an answer ("not ready"), not an error; the
    /// caller reads the sense data to learn why.
    ///
    /// # Errors
    ///
    /// A transport error.
    pub fn test_unit_ready(&mut self, lun: u8) -> Result<bool, Errno> {
        let cb = [0x00, 0, 0, 0, 0, 0];
        let outcome = self.command(lun, &cb, DataPhase::None)?;
        Ok(outcome.passed)
    }

    /// REQUEST SENSE: why the previous command failed.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] if the device fails the command itself,
    /// [`Errno::LengthOutOfRange`] for a response too short to carry the
    /// key/ASC/ASCQ triple, or a transport error.
    pub fn request_sense(&mut self, lun: u8) -> Result<Sense, Errno> {
        // REQUEST SENSE (SPC-4 §6.39): fixed format, 18-byte allocation.
        let mut data = [0u8; 18];
        let cb = [0x03, 0, 0, 0, 18, 0];
        let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
        if !outcome.passed {
            return Err(Errno::NotImplemented);
        }
        if outcome.transferred < 14 {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Sense {
            key: data[2] & 0x0F,
            asc: data[12],
            ascq: data[13],
        })
    }

    /// READ CAPACITY: the LUN's block geometry, via `READ CAPACITY(10)`
    /// and — for a unit past the 32-bit LBA horizon — `READ CAPACITY(16)`
    /// (SBC-3 §5.15/§5.16).
    ///
    /// Every device-supplied field is validated: the block size must be a
    /// power of two in `512..=4096` (so whole blocks tile the shared data
    /// window), and the block count must be non-zero and not overflow.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] if the device fails the command,
    /// [`Errno::OutOfRange`] / [`Errno::LengthOutOfRange`] for a geometry
    /// that cannot be honest, or a transport error.
    pub fn read_capacity(&mut self, lun: u8) -> Result<BlockGeometry, Errno> {
        let mut data = [0u8; 8];
        let cb = [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
        if !outcome.passed {
            return Err(Errno::NotImplemented);
        }
        if outcome.transferred != 8 {
            return Err(Errno::LengthOutOfRange);
        }
        let max_lba_32 = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let (max_lba, block_size) = if max_lba_32 == u32::MAX {
            // The unit is larger than READ CAPACITY(10) can express; ask
            // with the 16-byte form.
            let mut data = [0u8; 32];
            let mut cb = [0u8; 16];
            cb[0] = 0x9E; // SERVICE ACTION IN(16)
            cb[1] = 0x10; // READ CAPACITY(16)
            cb[10..14].copy_from_slice(&32u32.to_be_bytes());
            let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
            if !outcome.passed {
                return Err(Errno::NotImplemented);
            }
            if outcome.transferred < 12 {
                return Err(Errno::LengthOutOfRange);
            }
            (
                u64::from_be_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]),
                u32::from_be_bytes([data[8], data[9], data[10], data[11]]),
            )
        } else {
            (u64::from(max_lba_32), block_size)
        };
        if !block_size.is_power_of_two() || !(512..=4096).contains(&block_size) {
            return Err(Errno::OutOfRange);
        }
        let block_count = max_lba.checked_add(1).ok_or(Errno::OutOfRange)?;
        Ok(BlockGeometry {
            block_size,
            block_count,
        })
    }

    /// MODE SENSE(6): whether the medium is write-protected (the WP bit of
    /// the mode-parameter header's device-specific byte, SBC-3 §6.4.1).
    ///
    /// A device that fails the command is reported write-enabled — the
    /// established meaning of the refusal (many sticks implement no mode
    /// pages); the *enforcement* of the bit stays fail-closed in
    /// [`LunBlock::write_blocks`].
    ///
    /// # Errors
    ///
    /// A transport error.
    pub fn write_protected(&mut self, lun: u8) -> Result<bool, Errno> {
        // MODE SENSE(6), all pages, header-only allocation.
        let mut data = [0u8; 4];
        let cb = [0x1A, 0, 0x3F, 0, 4, 0];
        let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
        Ok(outcome.passed && outcome.transferred >= 3 && data[2] & 0x80 != 0)
    }

    /// SYNCHRONIZE CACHE(10): commit every completed write to the medium.
    ///
    /// A unit that answers ILLEGAL REQUEST has no cache to flush — the
    /// data is already on the medium — so that refusal is success; every
    /// other failure surfaces.
    ///
    /// # Errors
    ///
    /// The sense-mapped [`Errno`] of a genuine flush failure, or a
    /// transport error.
    pub fn synchronize_cache(&mut self, lun: u8) -> Result<(), Errno> {
        let cb = [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let outcome = self.command(lun, &cb, DataPhase::None)?;
        if outcome.passed {
            return Ok(());
        }
        let sense = self.request_sense(lun)?;
        if sense.key == SENSE_KEY_ILLEGAL_REQUEST {
            return Ok(());
        }
        Err(sense_errno(sense))
    }

    /// Read `buf.len()` bytes (a whole number of `block_size`-byte
    /// blocks) starting at `lba`. The caller ([`LunBlock`]) has validated
    /// the range and chunked the transfer; a short delivery is a failure,
    /// never a silent partial read.
    ///
    /// # Errors
    ///
    /// The sense-mapped [`Errno`] of a failed read, `NotImplemented` for a
    /// short delivery the device called a success, or a transport error.
    pub fn read(
        &mut self,
        lun: u8,
        lba: u64,
        block_size: u32,
        buf: &mut [u8],
    ) -> Result<(), Errno> {
        let blocks = blocks_spanned(lba, buf.len(), block_size)?;
        let cb = rw_command(0x28, 0x88, lba, blocks);
        let expected = buf.len();
        let outcome = self.command(lun, cb.bytes(), DataPhase::In(buf))?;
        if !outcome.passed {
            let sense = self.request_sense(lun)?;
            return Err(sense_errno(sense));
        }
        if outcome.transferred != expected {
            return Err(Errno::NotImplemented);
        }
        Ok(())
    }

    /// Write `buf` (a whole number of blocks) starting at `lba`. The
    /// write-protect policy is enforced by [`LunBlock`] before this is
    /// reached; a device-side DATA PROTECT still surfaces as
    /// [`Errno::PermissionDenied`].
    ///
    /// # Errors
    ///
    /// As [`Self::read`].
    pub fn write(&mut self, lun: u8, lba: u64, block_size: u32, buf: &[u8]) -> Result<(), Errno> {
        let blocks = blocks_spanned(lba, buf.len(), block_size)?;
        let cb = rw_command(0x2A, 0x8A, lba, blocks);
        let expected = buf.len();
        let outcome = self.command(lun, cb.bytes(), DataPhase::Out(buf))?;
        if !outcome.passed {
            let sense = self.request_sense(lun)?;
            return Err(sense_errno(sense));
        }
        if outcome.transferred != expected {
            return Err(Errno::NotImplemented);
        }
        Ok(())
    }

    /// Zero the transport's shared data window (sensitive-payload hygiene).
    pub fn scrub_window(&mut self) {
        self.transport.scrub();
    }

    /// Borrow the underlying transport, so the serve loop can observe
    /// transport-level state (e.g. that the interface disappeared and the
    /// per-LUN nodes must be retracted).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }
}

/// The SCSI transfer length (in logical blocks of the unit's own block
/// size) a byte length spans, bounded by the per-command ceiling
/// ([`MSD_MAX_TRANSFER_LEN`]) and re-validated block-aligned so a caller
/// bug cannot issue a torn command.
fn blocks_spanned(lba: u64, len: usize, block_size: u32) -> Result<u32, Errno> {
    let block_size = usize::try_from(block_size).map_err(|_| Errno::OutOfRange)?;
    if block_size == 0 || len == 0 || len > MSD_MAX_TRANSFER_LEN || len % block_size != 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let blocks = u32::try_from(len / block_size).map_err(|_| Errno::LengthOutOfRange)?;
    // Guard the end-of-range sum here too, so a caller bug cannot wrap.
    lba.checked_add(u64::from(blocks))
        .ok_or(Errno::LengthOutOfRange)?;
    Ok(blocks)
}

/// A READ/WRITE command block: the 10-byte form when the range fits it,
/// else the 16-byte form (SBC-3 §5.11/§5.13).
struct RwCommand {
    bytes: [u8; 16],
    len: usize,
}

impl RwCommand {
    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Build the READ/WRITE command block for `lba`/`blocks` in **512-byte
/// units scaled by the caller** — `blocks` here is the SCSI transfer
/// length in logical blocks, computed by [`Msd::read`]/[`Msd::write`]
/// from the byte length and the unit's block size.
fn rw_command(op10: u8, op16: u8, lba: u64, blocks: u32) -> RwCommand {
    let mut bytes = [0u8; 16];
    if let (Ok(lba32), Ok(blocks16)) = (u32::try_from(lba), u16::try_from(blocks)) {
        bytes[0] = op10;
        bytes[2..6].copy_from_slice(&lba32.to_be_bytes());
        bytes[7..9].copy_from_slice(&blocks16.to_be_bytes());
        return RwCommand { bytes, len: 10 };
    }
    bytes[0] = op16;
    bytes[2..10].copy_from_slice(&lba.to_be_bytes());
    bytes[10..14].copy_from_slice(&blocks.to_be_bytes());
    RwCommand { bytes, len: 16 }
}

/// Map fixed-format sense data onto the [`Errno`] a failed media command
/// surfaces: write-protection is a permission refusal, a not-ready or
/// attention state is distinct from a hard medium fault.
fn sense_errno(sense: Sense) -> Errno {
    match sense.key {
        SENSE_KEY_DATA_PROTECT => Errno::PermissionDenied,
        SENSE_KEY_NOT_READY | SENSE_KEY_UNIT_ATTENTION => Errno::WouldBlock,
        SENSE_KEY_ILLEGAL_REQUEST => Errno::OutOfRange,
        _ => Errno::NotImplemented,
    }
}

/// Map a transport/engine [`Errno`] onto the [`DriverError`] the
/// [`Block`] surface reports.
fn driver_error(err: Errno) -> DriverError {
    match err {
        Errno::PermissionDenied => DriverError::PermissionDenied,
        Errno::LengthOutOfRange => DriverError::LengthOutOfRange,
        Errno::OutOfRange => DriverError::OutOfRange,
        Errno::BufferTooSmall => DriverError::BufferTooSmall,
        Errno::EndpointStalled => DriverError::EndpointStalled,
        Errno::NotFound => DriverError::NotFound,
        Errno::WouldBlock => DriverError::Busy,
        Errno::BadMagic => DriverError::BadMagic,
        _ => DriverError::DeviceFault,
    }
}

/// Commit-to-medium seam the block service drives for
/// [`rustos_abi::blkio::BlkOp::Flush`]; the [`Block`] trait carries no
/// flush, so the per-LUN handle adds it explicitly.
pub trait Flush {
    /// Commit every completed write to the medium.
    ///
    /// # Errors
    ///
    /// A [`DriverError`] from the device.
    fn flush(&mut self) -> Result<(), DriverError>;
}

/// One logical unit exposed as a [`Block`] device: the per-LUN view the
/// block service serves (`plans/DEVICES.md` D2 target shape).
pub struct LunBlock<'a, T: MsdTransport> {
    msd: &'a mut Msd<T>,
    lun: u8,
    state: LunState,
}

impl<'a, T: MsdTransport> LunBlock<'a, T> {
    /// A [`Block`] view of `lun` with its brought-up `state`.
    pub fn new(msd: &'a mut Msd<T>, lun: u8, state: LunState) -> Self {
        Self { msd, lun, state }
    }

    /// Whether the medium is write-protected (the serve loop reports it
    /// on the geometry reply).
    #[must_use]
    pub fn write_protected(&self) -> bool {
        self.state.write_protected
    }

    /// Validate one block operation against the unit's geometry: the
    /// buffer must be a non-empty whole number of blocks and the range
    /// must lie inside the unit (mirrors the virtio-blk validation).
    fn validate(&self, lba: u64, len: usize) -> Result<(), DriverError> {
        let block_size = self.state.geometry.block_size as usize;
        if len == 0 || len % block_size != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = u64::try_from(len / block_size).map_err(|_| DriverError::LengthOutOfRange)?;
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.state.geometry.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(())
    }
}

impl<T: MsdTransport> Block for LunBlock<'_, T> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(self.state.geometry)
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_blocks_with_class(lba, buf, BufferClass::NonSensitive)
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.write_blocks_with_class(lba, buf, BufferClass::NonSensitive)
    }

    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate(lba, buf.len())?;
        let block_size = self.state.geometry.block_size as usize;
        // Split into bounded per-command chunks so the device cost is
        // fixed, never a function of the request length.
        let result = (|| {
            let mut off = 0usize;
            let mut cur_lba = lba;
            while off < buf.len() {
                let chunk = (buf.len() - off).min(MSD_MAX_TRANSFER_LEN);
                self.msd
                    .read(
                        self.lun,
                        cur_lba,
                        self.state.geometry.block_size,
                        &mut buf[off..off + chunk],
                    )
                    .map_err(driver_error)?;
                cur_lba += (chunk / block_size) as u64;
                off += chunk;
            }
            Ok(())
        })();
        // A sensitive payload never outlives the call in the shared
        // window, success or failure.
        if class == BufferClass::Sensitive {
            self.msd.scrub_window();
        }
        result
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        // Enforce the write-protect policy driver-side, before any byte
        // reaches the device (the device's own DATA PROTECT would also
        // refuse, but the policy never depends on it).
        if self.state.write_protected {
            return Err(DriverError::PermissionDenied);
        }
        self.validate(lba, buf.len())?;
        let block_size = self.state.geometry.block_size as usize;
        let result = (|| {
            let mut off = 0usize;
            let mut cur_lba = lba;
            while off < buf.len() {
                let chunk = (buf.len() - off).min(MSD_MAX_TRANSFER_LEN);
                self.msd
                    .write(
                        self.lun,
                        cur_lba,
                        self.state.geometry.block_size,
                        &buf[off..off + chunk],
                    )
                    .map_err(driver_error)?;
                cur_lba += (chunk / block_size) as u64;
                off += chunk;
            }
            Ok(())
        })();
        if class == BufferClass::Sensitive {
            self.msd.scrub_window();
        }
        result
    }
}

impl<T: MsdTransport> Flush for LunBlock<'_, T> {
    fn flush(&mut self) -> Result<(), DriverError> {
        self.msd.synchronize_cache(self.lun).map_err(driver_error)
    }
}

#[cfg(test)]
#[path = "bot_tests.rs"]
mod tests;
