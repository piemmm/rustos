//! The transport-neutral SCSI engine (`plans/DEVICES.md` D5).
//!
//! USB mass storage carries SCSI commands over one of several wire
//! transports — Bulk-Only Transport ([`crate::bot`]), Control/Bulk/Interrupt
//! ([`crate::cbi`]), and USB Attached SCSI ([`crate::uas`]). The *commands*
//! are the same; only the framing differs. This module is the one home of
//! the command layer: it builds each CDB, validates every device-supplied
//! payload fail-closed, and maps sense data onto errors, generic over the
//! [`ScsiTransport`] seam each wire transport implements.
//!
//! The command *set* also varies by device: a disk speaks the SCSI
//! transparent set, a USB floppy speaks UFI (12-byte fixed CDBs and
//! `MODE SENSE(10)`, USB Mass Storage UFI 1.0). [`CommandSet`] carries that
//! per-device spelling so the logic exists once.

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::BufferClass;
use tairix_abi::{DriverError, Errno};

/// One SCSI command execution seam: a wire transport (BOT, CBI, UAS) runs
/// the command block, moves the data phase, and reports the device's honest
/// verdict. A host test implements it as a scripted device.
pub trait ScsiTransport {
    /// Execute one SCSI command for `lun`: send `cdb`, move `data`, and
    /// return whether the device reported success plus the bytes that
    /// actually moved (never more than the data phase's length).
    ///
    /// # Errors
    ///
    /// An [`Errno`] for a transport fault or protocol violation (the
    /// command's *failure* is an answer, not an error). Starting a new
    /// execution invalidates any in-band sense capture a previous command
    /// left behind ([`Self::take_sense`]), so stale sense can never
    /// explain a later failure.
    fn execute(
        &mut self,
        lun: u8,
        cdb: &[u8],
        data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno>;

    /// Number of logical units the device exposes, by the transport's own
    /// discovery (BOT `GET MAX LUN`, UAS `REPORT LUNS`, CBI exactly one).
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a count outside the protocol bound, or a
    /// transport fault.
    fn lun_count(&mut self) -> Result<u8, Errno>;

    /// Take the sense data the transport captured with the last failed
    /// command, if its protocol delivers sense in-band (UAS autosense).
    /// `None` means the caller must issue `REQUEST SENSE`.
    fn take_sense(&mut self) -> Option<Sense>;

    /// Zero every byte of the transport's shared data window, so a
    /// sensitive payload does not outlive the block operation that moved
    /// it (the zero-on-free discipline applied to the bounce window).
    fn scrub(&mut self);
}

/// The command-set spelling a device speaks.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CommandSet {
    /// The SCSI transparent command set (interface sub-class `0x06`).
    Transparent,
    /// UFI, the USB floppy command set (sub-class `0x04`): every CDB is
    /// padded to 12 bytes, write protection is read with `MODE SENSE(10)`,
    /// and the set carries no `SYNCHRONIZE CACHE` (the medium is written
    /// through).
    Ufi,
}

/// Fixed UFI command-block length (UFI 1.0 §3.2: all commands are 12-byte
/// blocks, zero-padded).
const UFI_CDB_LEN: usize = 12;

/// The data phase of one SCSI command.
pub enum DataPhase<'a> {
    /// Device-to-host data of the given length.
    In(&'a mut [u8]),
    /// Host-to-device data.
    Out(&'a [u8]),
    /// No data phase.
    None,
}

impl DataPhase<'_> {
    /// Bytes the phase covers.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            DataPhase::In(buf) => buf.len(),
            DataPhase::Out(buf) => buf.len(),
            DataPhase::None => 0,
        }
    }

    /// Whether the phase moves no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the phase is device-to-host (a no-data phase counts as IN,
    /// matching the BOT CBW direction flag convention).
    #[must_use]
    pub const fn is_in(&self) -> bool {
        matches!(self, DataPhase::In(_) | DataPhase::None)
    }
}

/// The outcome of one executed command: whether the device reported success
/// and how many data-phase bytes actually moved.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    /// The device reported the command passed.
    pub passed: bool,
    /// Data-phase bytes that actually moved.
    pub transferred: usize,
}

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

impl Sense {
    /// Reconstruct fixed-format sense from a UFI command-completion
    /// interrupt block, which carries only the additional sense code and
    /// qualifier (USB Mass Storage CBI 1.1 §3.4.3.1.1) rather than the full
    /// sense triple. The sense key is recovered from the ASC using the SCSI
    /// additional-sense-code assignments (SPC-4 Annex F / SBC-3): each of
    /// these codes is only ever reported under one key — write protection
    /// under DATA PROTECT, a not-ready or no-medium state under NOT READY, a
    /// media-change or power-on/reset notification under UNIT ATTENTION, and
    /// a rejected command or CDB field under ILLEGAL REQUEST. Any other code
    /// carries no recovered key (`0`), so it surfaces as an unclassified
    /// device fault rather than a guessed category (fail closed).
    #[must_use]
    pub fn from_ufi_completion(asc: u8, ascq: u8) -> Self {
        let key = match asc {
            0x27 => SENSE_KEY_DATA_PROTECT,
            0x04 | 0x3A => SENSE_KEY_NOT_READY,
            0x28 | 0x29 => SENSE_KEY_UNIT_ATTENTION,
            0x20 | 0x21 | 0x24 | 0x26 => SENSE_KEY_ILLEGAL_REQUEST,
            _ => 0,
        };
        Self { key, asc, ascq }
    }
}

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
    /// Whether the medium is write-protected (`MODE SENSE` WP bit);
    /// writes are refused driver-side before touching the device.
    pub write_protected: bool,
}

/// Most logical units a served device can expose (the BOT `GET MAX LUN`
/// bound, `0..=15`; UAS units above it are not served). A fixed protocol
/// bound, not a scalable capacity.
pub const MAX_LUNS: usize = 16;

/// Most bytes one SCSI READ/WRITE issued by [`LunBlock`] covers: the
/// block-service data window ([`tairix_abi::blkio::BLK_DATA_LEN`] — one
/// definition), so a Block call of any size splits into bounded commands
/// and per-device cost never scales with request length.
pub const MSD_MAX_TRANSFER_LEN: usize = tairix_abi::blkio::BLK_DATA_LEN;

/// One SCSI device: the command layer over a wire transport.
pub struct ScsiDevice<T: ScsiTransport> {
    transport: T,
    set: CommandSet,
}

impl<T: ScsiTransport> ScsiDevice<T> {
    /// The command layer over `transport`, speaking `set`.
    pub fn new(transport: T, set: CommandSet) -> Self {
        Self { transport, set }
    }

    /// Number of logical units the device exposes.
    ///
    /// # Errors
    ///
    /// As [`ScsiTransport::lun_count`].
    pub fn lun_count(&mut self) -> Result<u8, Errno> {
        self.transport.lun_count()
    }

    /// Execute `cdb` with the command set's spelling applied: a UFI device
    /// receives every command as a 12-byte zero-padded block.
    fn command(
        &mut self,
        lun: u8,
        cdb: &[u8],
        data: DataPhase<'_>,
    ) -> Result<CommandOutcome, Errno> {
        if cdb.is_empty() || cdb.len() > 16 {
            return Err(Errno::OutOfRange);
        }
        let mut padded = [0u8; UFI_CDB_LEN];
        let cdb = match self.set {
            CommandSet::Ufi if cdb.len() < UFI_CDB_LEN => {
                padded[..cdb.len()].copy_from_slice(cdb);
                &padded[..]
            }
            _ => cdb,
        };
        self.transport.execute(lun, cdb, data)
    }

    /// The sense data explaining the last failed command: the transport's
    /// in-band capture where its protocol carries one (UAS), else a
    /// `REQUEST SENSE` round trip.
    fn sense_for_failure(&mut self, lun: u8) -> Result<Sense, Errno> {
        if let Some(sense) = self.transport.take_sense() {
            return Ok(sense);
        }
        self.request_sense(lun)
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

    /// Whether the LUN becomes ready within `attempts` TEST UNIT READY
    /// round trips — the bring-up drain of the start-of-day UNIT
    /// ATTENTION / not-ready states. Each failed attempt consumes the
    /// unit's sense state (the transport's in-band capture, or a
    /// `REQUEST SENSE` that clears it device-side), so this is a fixed
    /// number of real round trips, never a hot spin.
    ///
    /// # Errors
    ///
    /// A transport error, or a sense round trip failing outright.
    pub fn ready_after_drain(&mut self, lun: u8, attempts: usize) -> Result<bool, Errno> {
        for _ in 0..attempts {
            if self.test_unit_ready(lun)? {
                return Ok(true);
            }
            let _ = self.sense_for_failure(lun)?;
        }
        Ok(false)
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

    /// MODE SENSE: whether the medium is write-protected (the WP bit of
    /// the mode-parameter header's device-specific byte, SBC-3 §6.4.1 /
    /// UFI 1.0 §4.10).
    ///
    /// The transparent set asks with `MODE SENSE(6)`; UFI carries only the
    /// 10-byte form, whose header stores the device-specific byte at
    /// offset 3. A device that fails the command is reported write-enabled
    /// — the established meaning of the refusal (many devices implement no
    /// mode pages); the *enforcement* of the bit stays fail-closed in
    /// [`LunBlock::write_blocks`].
    ///
    /// # Errors
    ///
    /// A transport error.
    pub fn write_protected(&mut self, lun: u8) -> Result<bool, Errno> {
        let protected = match self.set {
            CommandSet::Transparent => {
                // MODE SENSE(6), all pages, header-only allocation.
                let mut data = [0u8; 4];
                let cb = [0x1A, 0, 0x3F, 0, 4, 0];
                let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
                outcome.passed && outcome.transferred >= 3 && data[2] & 0x80 != 0
            }
            CommandSet::Ufi => {
                // MODE SENSE(10), all pages, header-only allocation.
                let mut data = [0u8; 8];
                let cb = [0x5A, 0, 0x3F, 0, 0, 0, 0, 0, 8, 0];
                let outcome = self.command(lun, &cb, DataPhase::In(&mut data))?;
                outcome.passed && outcome.transferred >= 4 && data[3] & 0x80 != 0
            }
        };
        Ok(protected)
    }

    /// SYNCHRONIZE CACHE(10): commit every completed write to the medium.
    ///
    /// UFI carries no such command — the floppy medium is written through —
    /// so a UFI flush succeeds without a wire round trip. For the
    /// transparent set, a unit that answers ILLEGAL REQUEST has no cache to
    /// flush (the data is already on the medium), so that refusal is
    /// success; every other failure surfaces.
    ///
    /// # Errors
    ///
    /// The sense-mapped [`Errno`] of a genuine flush failure, or a
    /// transport error.
    pub fn synchronize_cache(&mut self, lun: u8) -> Result<(), Errno> {
        if self.set == CommandSet::Ufi {
            return Ok(());
        }
        let cb = [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let outcome = self.command(lun, &cb, DataPhase::None)?;
        if outcome.passed {
            return Ok(());
        }
        let sense = self.sense_for_failure(lun)?;
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
            let sense = self.sense_for_failure(lun)?;
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
            let sense = self.sense_for_failure(lun)?;
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
    if block_size == 0 || len == 0 || len > MSD_MAX_TRANSFER_LEN || !len.is_multiple_of(block_size)
    {
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

/// Build the READ/WRITE command block for `lba`/`blocks` — `blocks` is the
/// SCSI transfer length in logical blocks, computed by
/// [`ScsiDevice::read`]/[`ScsiDevice::write`] from the byte length and the
/// unit's block size.
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

/// One logical unit exposed as a [`Block`] device: the per-LUN view the
/// block service serves (`plans/DEVICES.md` D2 target shape).
pub struct LunBlock<'a, T: ScsiTransport> {
    device: &'a mut ScsiDevice<T>,
    lun: u8,
    state: LunState,
}

impl<'a, T: ScsiTransport> LunBlock<'a, T> {
    /// A [`Block`] view of `lun` with its brought-up `state`.
    pub fn new(device: &'a mut ScsiDevice<T>, lun: u8, state: LunState) -> Self {
        Self { device, lun, state }
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
        if len == 0 || !len.is_multiple_of(block_size) {
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

impl<T: ScsiTransport> Block for LunBlock<'_, T> {
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
                self.device
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
            self.device.scrub_window();
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
                self.device
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
            self.device.scrub_window();
        }
        result
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // Commit the device's volatile write cache via SCSI SYNCHRONIZE
        // CACHE (a no-op on a UFI unit that has none). The block service
        // drives this for `BlkOp::Flush`.
        self.device
            .synchronize_cache(self.lun)
            .map_err(driver_error)
    }
}

#[cfg(test)]
#[path = "scsi_tests.rs"]
mod tests;
