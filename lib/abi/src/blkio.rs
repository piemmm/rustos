//! The block-service transport IPC protocol (`plans/DEVICES.md` D2).
//!
//! A user-space block driver (the first is the USB mass-storage class
//! driver, `drivers/storage/usb_msd`) exposes each logical unit it brings
//! up as a **block-service call endpoint** plus a shared-memory data
//! window, both forwarded as grants on the storage-class hardware-tree
//! node it emits. A consumer (the volume manager, `plans/DEVICES.md` D3)
//! inherits those grants, maps the window, and drives the device with the
//! fixed-size request/completion frames defined here — the same
//! request-reply IPC shape as the URB transport ([`crate::usb_urb`]).
//!
//! Only the request and the completion cross the endpoint; the data being
//! read or written lives in the separately-mapped shared window
//! ([`BLK_DATA_LEN`] bytes, always at offset zero). The serving driver
//! validates every field against the live device geometry before touching
//! hardware and fails closed; a request larger than the window is refused,
//! and a consumer moves larger transfers as multiple bounded requests, so
//! the per-device cost is fixed rather than a function of request size.

use crate::driver::block::Block;
use crate::le::{put_i32, put_u32, put_u64, read_i32, read_u32, read_u64};
use crate::{DriverError, Errno};

/// Length of the shared-memory data window a block-service endpoint
/// serves transfers through, and therefore the largest single transfer
/// one request may name. A multiple of every supported block size, sized
/// to amortise per-request cost without scaling per-device memory with
/// request length (the `virtio_blk` staging-window precedent).
pub const BLK_DATA_LEN: usize = 32 * 1024;

/// One block-service operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum BlkOp {
    /// Query the device geometry (block size, block count, flags). Carries
    /// no data and names no blocks.
    Geometry = 0,
    /// Read [`BlkRequest::blocks`] blocks starting at [`BlkRequest::lba`]
    /// into the shared data window (offset zero).
    Read = 1,
    /// Write [`BlkRequest::blocks`] blocks from the shared data window
    /// (offset zero) starting at [`BlkRequest::lba`].
    Write = 2,
    /// Commit every completed write to the medium (the SCSI
    /// `SYNCHRONIZE CACHE` / virtio flush equivalent). Carries no data and
    /// names no blocks.
    Flush = 3,
}

impl BlkOp {
    /// The wire byte for this operation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover an operation from its wire byte.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known operation (fail
    /// closed; any future opcode is refused here).
    pub const fn from_u8(value: u8) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Geometry),
            1 => Ok(Self::Read),
            2 => Ok(Self::Write),
            3 => Ok(Self::Flush),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// One block-service request: a single bounded operation against the
/// logical unit the endpoint serves.
///
/// The transfer's payload lives in the shared data window; the request
/// carries only the operation's shape. The serving driver validates every
/// field against the live geometry (range, alignment, the window bound)
/// before it touches the device and fails closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlkRequest {
    /// The operation.
    pub op: BlkOp,
    /// First logical block of a [`BlkOp::Read`] / [`BlkOp::Write`]; zero
    /// for the data-less operations.
    pub lba: u64,
    /// Number of blocks a [`BlkOp::Read`] / [`BlkOp::Write`] covers
    /// (`blocks * block_size` bytes in the shared window, never more than
    /// [`BLK_DATA_LEN`]); zero for the data-less operations.
    pub blocks: u32,
}

/// Encoded length of a [`BlkRequest`]: `op(1) || pad(3) || blocks(4) ||
/// lba(8)`. Fixed — every request encodes to the same size, so this is
/// both the encoding length and the endpoint's maximum request size.
pub const BLK_REQUEST_LEN: usize = 1 + 3 + 4 + 8;

impl BlkRequest {
    /// Encode `self` into `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold [`BLK_REQUEST_LEN`]
    /// bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.len() < BLK_REQUEST_LEN {
            return Err(Errno::BufferTooSmall);
        }
        buf[0] = self.op.as_u8();
        buf[1] = 0;
        buf[2] = 0;
        buf[3] = 0;
        put_u32(buf, 4, self.blocks);
        put_u64(buf, 8, self.lba);
        Ok(BLK_REQUEST_LEN)
    }

    /// Decode a block-service request from `bytes`, validating every field.
    ///
    /// This rejects a malformed *encoding* (truncation, an unknown opcode,
    /// a data-less operation naming blocks); the serving driver performs
    /// the further *semantic* checks that need the live device (range
    /// against the geometry, the window bound) before it queues anything.
    ///
    /// # Errors
    ///
    /// * [`Errno::LengthOutOfRange`] if `bytes` is shorter than
    ///   [`BLK_REQUEST_LEN`] — a truncated request is never read past its
    ///   bytes.
    /// * [`Errno::OutOfRange`] if the opcode byte is unknown, or a
    ///   data-less operation ([`BlkOp::Geometry`] / [`BlkOp::Flush`])
    ///   carries a non-zero `lba` or `blocks` (fail closed on a frame that
    ///   cannot mean what it says).
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < BLK_REQUEST_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        let op = BlkOp::from_u8(bytes[0])?;
        let blocks = read_u32(bytes, 4);
        let lba = read_u64(bytes, 8);
        if matches!(op, BlkOp::Geometry | BlkOp::Flush) && (lba != 0 || blocks != 0) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { op, lba, blocks })
    }
}

/// The explicit health/outcome axis of a block-service completion.
///
/// Every completion leads with one of these words, so a consumer can tell a
/// device that will come back (transient, timeout, reset) from one that is
/// dead (medium error, offline, fatal) and make an isolation decision — the
/// distinction the bare success-or-`-errno` frame could not express. The
/// serving driver emits the status from what it already knows (SCSI sense →
/// [`BlkStatus::MediumError`], surprise removal → [`BlkStatus::Offline`] /
/// [`BlkStatus::Removed`], a recovered error → [`BlkStatus::TransientError`],
/// device self-report → [`BlkStatus::Degraded`]); [`BlkStatus::Timeout`] is
/// synthesised kernel-side by the deadlined reap path (`plans/FIX-IO.md` IO1)
/// when the driver never answers.
///
/// Decoding is fail-closed: an unknown status word is [`BlkStatus::Fatal`],
/// never silently [`BlkStatus::Ok`].
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum BlkStatus {
    /// The transfer completed; the geometry payload (for a
    /// [`BlkOp::Geometry`]) is valid and the medium moved exactly the
    /// requested blocks.
    #[default]
    Ok = 0,
    /// The transfer completed and its data is valid, but the device reports
    /// itself unhealthy (a recovered ECC beyond threshold, a pending
    /// reallocation): the caller may keep using it while noting the warning.
    Degraded = 1,
    /// A transient, retryable error (a recovered communications glitch): the
    /// caller may reissue the request.
    TransientError = 2,
    /// The request did not complete within its deadline. Synthesised by the
    /// deadlined reap path; safe to reissue.
    Timeout = 3,
    /// The transfer was aborted by a device, endpoint, or hub reset. The
    /// path has already recovered, so the request is safe to reissue.
    Reset = 4,
    /// A permanent, unrecoverable medium error (a bad sector): the data at
    /// the named blocks is gone. Not retryable.
    MediumError = 5,
    /// The device is present but unresponsive. Not retryable until it
    /// demonstrably recovers.
    Offline = 6,
    /// The device has been surprise-removed. Not retryable.
    Removed = 7,
    /// An unrecoverable, unclassified failure (also the fail-closed decode of
    /// an unknown status word or a corrupt frame). Not retryable.
    Fatal = 8,
}

impl BlkStatus {
    /// The wire value for this status.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a status from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known status; the decoder
    /// maps that to [`BlkStatus::Fatal`] so a corrupt frame fails closed
    /// rather than reading as success.
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::Degraded),
            2 => Ok(Self::TransientError),
            3 => Ok(Self::Timeout),
            4 => Ok(Self::Reset),
            5 => Ok(Self::MediumError),
            6 => Ok(Self::Offline),
            7 => Ok(Self::Removed),
            8 => Ok(Self::Fatal),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// Whether the completion's geometry/data payload is valid and may be
    /// consumed. True for [`BlkStatus::Ok`] and [`BlkStatus::Degraded`]
    /// (served, if unhealthy); false for every error status.
    #[must_use]
    pub const fn data_valid(self) -> bool {
        matches!(self, Self::Ok | Self::Degraded)
    }

    /// Whether a caller may safely reissue the request. True for the
    /// transient/timeout/reset classes; false for a valid completion, a
    /// permanent medium error, or a gone device.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::TransientError | Self::Timeout | Self::Reset)
    }

    /// The [`Errno`] this status implies when the completion carries no more
    /// specific error code (the single mapping the block layer acts on:
    /// transient/reset/timeout → reissue, medium/offline → hard I/O error).
    #[must_use]
    pub const fn default_errno(self) -> Errno {
        match self {
            // A valid completion is not an error; callers gate on
            // `data_valid` before ever reading this, but a total mapping
            // keeps the function honest.
            Self::Ok | Self::Degraded => Errno::OutOfRange,
            Self::TransientError => Errno::WouldBlock,
            Self::Timeout => Errno::TimedOut,
            Self::Reset => Errno::EndpointStalled,
            Self::MediumError => Errno::MediumError,
            Self::Offline | Self::Removed => Errno::DeviceOffline,
            Self::Fatal => Errno::DeviceFault,
        }
    }

    /// The health status that best classifies `err`, so a serving driver
    /// that only has an [`Errno`] to report still emits a coherent health
    /// axis. Non-health errors (a refused write, a malformed request) map to
    /// [`BlkStatus::Fatal`]; the frame's errno word preserves the specific
    /// code.
    #[must_use]
    pub const fn for_errno(err: Errno) -> Self {
        match err {
            Errno::TimedOut => Self::Timeout,
            Errno::EndpointStalled => Self::Reset,
            Errno::WouldBlock => Self::TransientError,
            Errno::MediumError => Self::MediumError,
            Errno::DeviceOffline => Self::Offline,
            _ => Self::Fatal,
        }
    }

    /// The device-health status a [`Block`] result's [`DriverError`] carries,
    /// or [`None`] when the error is *request-level* rather than a
    /// device-health signal.
    ///
    /// A serving driver's device call can fail for two very different
    /// reasons, and only one of them is about the device's health. A bad
    /// sector ([`DriverError::MediumError`]), a gone/unresponsive device
    /// ([`DriverError::DeviceOffline`]), a busy device or a recovered stall
    /// ([`DriverError::Busy`] / [`DriverError::EndpointStalled`]), and an
    /// unrecoverable hardware fault ([`DriverError::DeviceFault`]) all speak
    /// to the *device* and are folded into [`BlkHealth`]. A rejected request
    /// — an out-of-range LBA, a write to a read-only unit, an unsupported op
    /// — is about the *request*, not the device, and returns [`None`] so it
    /// can never drive a healthy device toward [`BlkHealthState::Faulted`].
    #[must_use]
    pub const fn for_driver_health(err: DriverError) -> Option<Self> {
        match err {
            DriverError::MediumError => Some(Self::MediumError),
            DriverError::DeviceOffline => Some(Self::Offline),
            DriverError::Busy => Some(Self::TransientError),
            DriverError::EndpointStalled => Some(Self::Reset),
            DriverError::DeviceFault => Some(Self::Fatal),
            _ => None,
        }
    }

    /// How fail-closed this status is, as a rank a consumer can order two
    /// statuses by: the higher the rank, the less the completion can be
    /// trusted and the more conservatively it must be treated. The order is
    /// healthy → served-but-unwell → reissuable → permanent → gone:
    /// `Ok` < `Degraded` < `TransientError` < `Timeout` < `Reset` <
    /// `MediumError` < `Offline` < `Removed` < `Fatal`.
    ///
    /// This is the single, explicit definition of "which health signal wins"
    /// when more than one applies to one request — a leaf device's own outcome
    /// and each interior fault domain above it in the tree
    /// ([`FaultDomain::child_status`]) — so [`combine`](Self::combine) and
    /// every consumer that folds statuses cannot order them differently. It is
    /// kept deliberately independent of the wire value [`as_u32`](Self::as_u32)
    /// so the transport encoding and the recovery precedence are separate
    /// concerns that can never silently couple.
    #[must_use]
    pub const fn severity(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Degraded => 1,
            Self::TransientError => 2,
            Self::Timeout => 3,
            Self::Reset => 4,
            Self::MediumError => 5,
            Self::Offline => 6,
            Self::Removed => 7,
            Self::Fatal => 8,
        }
    }

    /// The single status that most conservatively describes a request to which
    /// both `self` and `other` apply: the more fail-closed of the two by
    /// [`severity`](Self::severity).
    ///
    /// This is how a serving driver folds a leaf device's own completion
    /// outcome with what each interior fault domain above it imposes
    /// ([`FaultDomain::child_status`]). It is the reason the fold is *correct*
    /// rather than ad-hoc: a hub mid-reset ([`BlkStatus::Reset`]) overriding a
    /// device's own [`BlkStatus::Ok`] means the aborted transfer is answered
    /// reissuably and its data is *not* consumed (`Reset` outranks `Ok`),
    /// while a device's own definitive [`BlkStatus::MediumError`] still wins
    /// over a concurrent `Reset` (a bad sector is real and must not be retried
    /// into the same failure). Because it is a total order, the fold is
    /// associative and commutative, so the chain can be walked in any order
    /// with the same result.
    #[must_use]
    pub const fn combine(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }
}

/// Byte offsets of the completion frame: `status(4) || errno(4) ||
/// block_size(4) || block_count(8) || flags(4)`.
const COMPLETION_STATUS_OFF: usize = 0;
const COMPLETION_ERRNO_OFF: usize = 4;
const COMPLETION_GEOMETRY_OFF: usize = 8;
/// Smallest prefix that carries the health axis (`status || errno`); a frame
/// shorter than this cannot be classified and fails closed.
const COMPLETION_HEADER_LEN: usize = COMPLETION_GEOMETRY_OFF;

/// Encoded length of a block-service completion: the health/status word, an
/// [`Errno`] detail word (`0` when none), then the geometry payload
/// (`block_size(4) || block_count(8) || flags(4)`, zero-filled for the
/// non-geometry operations). Also the endpoint's maximum reply size.
pub const BLK_COMPLETION_LEN: usize = COMPLETION_HEADER_LEN + 4 + 8 + 4;

/// [`BlkCompletion::flags`] bit: the logical unit is write-protected; a
/// [`BlkOp::Write`] will be refused [`Errno::PermissionDenied`].
pub const BLK_FLAG_READ_ONLY: u32 = 1;

/// A successful block-service completion.
///
/// Only a [`BlkOp::Geometry`] reply carries meaningful fields; the other
/// operations complete with a zero-filled payload (a read or write either
/// moved exactly the requested blocks or failed — there are no partial
/// completions on this seam).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BlkCompletion {
    /// Size of one logical block, in bytes.
    pub block_size: u32,
    /// Total number of logical blocks.
    pub block_count: u64,
    /// Device flags ([`BLK_FLAG_READ_ONLY`]).
    pub flags: u32,
}

/// Write one completion frame (`status || errno || geometry`) into `buf`.
///
/// The single encoder every public entry point funnels through, so the wire
/// layout lives in exactly one place. `errno_val` is `0` for a valid
/// completion and the positive [`Errno`] discriminant otherwise.
fn encode_frame(
    buf: &mut [u8],
    status: BlkStatus,
    errno_val: i32,
    geometry: BlkCompletion,
) -> Result<usize, Errno> {
    if buf.len() < BLK_COMPLETION_LEN {
        return Err(Errno::BufferTooSmall);
    }
    put_u32(buf, COMPLETION_STATUS_OFF, status.as_u32());
    put_i32(buf, COMPLETION_ERRNO_OFF, errno_val);
    put_u32(buf, COMPLETION_GEOMETRY_OFF, geometry.block_size);
    put_u64(buf, COMPLETION_GEOMETRY_OFF + 4, geometry.block_count);
    put_u32(buf, COMPLETION_GEOMETRY_OFF + 12, geometry.flags);
    Ok(BLK_COMPLETION_LEN)
}

impl BlkCompletion {
    /// Encode `self` as an [`BlkStatus::Ok`] completion into `buf`,
    /// returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold
    /// [`BLK_COMPLETION_LEN`] bytes.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        encode_frame(buf, BlkStatus::Ok, 0, *self)
    }

    /// Encode `self` as a completion carrying `status` into `buf`.
    ///
    /// For a data-valid status ([`BlkStatus::Ok`] / [`BlkStatus::Degraded`])
    /// the geometry payload is carried and the errno word is `0`; for an
    /// error status the geometry is meaningless and the status's implied
    /// [`Errno`] is recorded, so a serving driver reporting a health
    /// transition (a self-reported [`BlkStatus::Degraded`], a
    /// [`BlkStatus::Offline`] on surprise removal) uses one call.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `buf` cannot hold
    /// [`BLK_COMPLETION_LEN`] bytes.
    pub fn encode_status(&self, status: BlkStatus, buf: &mut [u8]) -> Result<usize, Errno> {
        if status.data_valid() {
            encode_frame(buf, status, 0, *self)
        } else {
            encode_frame(
                buf,
                status,
                status.default_errno().as_i32(),
                BlkCompletion::default(),
            )
        }
    }
}

/// Encode a fail-closed error completion into `buf`: the health status that
/// classifies `err` ([`BlkStatus::for_errno`]) plus `err` itself, with a
/// zero geometry payload.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold [`BLK_COMPLETION_LEN`]
/// bytes.
pub fn encode_error_completion(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    encode_frame(
        buf,
        BlkStatus::for_errno(err),
        err.as_i32(),
        BlkCompletion::default(),
    )
}

/// The fully-decoded outcome of a block-service completion: its health
/// [`BlkStatus`], the geometry payload (valid only when
/// [`BlkStatus::data_valid`]), and the specific [`Errno`] an error carries.
///
/// Decoding never fails: a truncated or corrupt frame — or an unknown status
/// word — resolves to [`BlkStatus::Fatal`] with [`Errno::DeviceFault`], so a
/// consumer can always make an isolation decision and never reads a corrupt
/// frame as success.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlkOutcome {
    /// The device-health/outcome axis.
    pub status: BlkStatus,
    /// The geometry payload, valid iff `status.data_valid()`.
    pub geometry: BlkCompletion,
    /// The specific error, meaningful iff `!status.data_valid()`.
    pub error: Errno,
}

impl BlkOutcome {
    /// The data-path view: the geometry on a valid completion, else the
    /// carried [`Errno`]. The one place a consumer that only wants
    /// success-or-error (not the full health axis) collapses the outcome.
    ///
    /// # Errors
    ///
    /// The carried [`Errno`] when the status is not data-valid.
    pub const fn data(&self) -> Result<BlkCompletion, Errno> {
        if self.status.data_valid() {
            Ok(self.geometry)
        } else {
            Err(self.error)
        }
    }
}

/// Decode a block-service completion into its full [`BlkOutcome`], fail
/// closed. A frame too short to classify, one whose geometry is truncated
/// under a data-valid status, or one bearing an unknown status word all
/// resolve to [`BlkStatus::Fatal`] / [`Errno::DeviceFault`] — never a
/// spurious success.
#[must_use]
pub fn decode_outcome(reply: &[u8]) -> BlkOutcome {
    let fatal = BlkOutcome {
        status: BlkStatus::Fatal,
        geometry: BlkCompletion::default(),
        error: Errno::DeviceFault,
    };
    if reply.len() < COMPLETION_HEADER_LEN {
        return fatal;
    }
    let Ok(status) = BlkStatus::from_u32(read_u32(reply, COMPLETION_STATUS_OFF)) else {
        return fatal;
    };
    if status.data_valid() {
        // A valid frame we cannot fully read cannot be trusted as valid.
        if reply.len() < BLK_COMPLETION_LEN {
            return fatal;
        }
        BlkOutcome {
            status,
            geometry: BlkCompletion {
                block_size: read_u32(reply, COMPLETION_GEOMETRY_OFF),
                block_count: read_u64(reply, COMPLETION_GEOMETRY_OFF + 4),
                flags: read_u32(reply, COMPLETION_GEOMETRY_OFF + 12),
            },
            error: status.default_errno(),
        }
    } else {
        // An unknown or absent errno word falls back to the status's implied
        // code, so an error is never read as an unrelated `Errno`.
        let error = Errno::from_i32(read_i32(reply, COMPLETION_ERRNO_OFF))
            .unwrap_or(status.default_errno());
        BlkOutcome {
            status,
            geometry: BlkCompletion::default(),
            error,
        }
    }
}

/// Decode a block-service completion for the data path: the geometry on a
/// valid completion, else the carried [`Errno`]. A thin view over
/// [`decode_outcome`] for consumers that do not need the health axis.
///
/// # Errors
///
/// The carried [`Errno`] for any non-valid completion (a device fault, a
/// [`Errno::PermissionDenied`] refused write, or a fail-closed
/// [`Errno::DeviceFault`] for a corrupt or truncated frame).
pub fn decode_completion(reply: &[u8]) -> Result<BlkCompletion, Errno> {
    decode_outcome(reply).data()
}

/// The broad performance/behaviour class of a block device, discovered from
/// its hardware-tree node, that its per-device I/O budget is derived from.
///
/// The class is *discovered*, never a compile-time board constant: a
/// rotational SATA disk, an `NVMe` namespace, a removable USB unit, and a
/// paravirtual device have genuinely different reset/spin-up/latency
/// envelopes, so a single global deadline would either punish a slow disk
/// that is merely spinning up or let a wedged fast device stall far too long.
/// The budget is *policy* sized per class, not a security/validation bound.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum BlkDeviceClass {
    /// A rotational disk (spinning SATA/SAS): a large reset/spin-up budget.
    Rotational = 0,
    /// A solid-state device (`NVMe` / SATA SSD): low latency, deep queue.
    SolidState = 1,
    /// A removable unit (USB mass storage): moderate latency, shallow queue,
    /// prone to bus resets and surprise removal.
    Removable = 2,
    /// A paravirtual device (virtio-blk) or other software-backed unit: fast,
    /// but bounded so a wedged host backend never stalls forever. The
    /// fail-closed default for an unclassified node.
    #[default]
    Virtual = 3,
}

impl BlkDeviceClass {
    /// The wire value for this class.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Recover a class from its wire value.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `value` is not a known class (fail closed).
    pub const fn from_u32(value: u32) -> Result<Self, Errno> {
        match value {
            0 => Ok(Self::Rotational),
            1 => Ok(Self::SolidState),
            2 => Ok(Self::Removable),
            3 => Ok(Self::Virtual),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The per-device I/O budget this class is served with — the one place
    /// both the serving driver and the consumer read it, so the deadline,
    /// retry count, and queue depth can never silently diverge between them.
    #[must_use]
    pub const fn budget(self) -> IoBudget {
        match self {
            // A spinning disk may legitimately take tens of seconds to spin
            // up or complete an internal reset before it answers, and its
            // grace window is wider still so a full spin-up plus reset is
            // ridden out rather than punished.
            Self::Rotational => IoBudget {
                deadline_ns: 30_000_000_000,
                max_retries: 3,
                queue_depth: 4,
                grace_ns: 60_000_000_000,
            },
            // An SSD that has not answered in a few seconds is wedged, not
            // busy; a deep queue keeps it fed.
            Self::SolidState => IoBudget {
                deadline_ns: 5_000_000_000,
                max_retries: 2,
                queue_depth: 32,
                grace_ns: 8_000_000_000,
            },
            // A removable unit tolerates a bus glitch/reset (which may
            // re-enumerate the bus) but must fail closed promptly once
            // genuinely gone.
            Self::Removable => IoBudget {
                deadline_ns: 15_000_000_000,
                max_retries: 3,
                queue_depth: 2,
                grace_ns: 20_000_000_000,
            },
            Self::Virtual => IoBudget {
                deadline_ns: 10_000_000_000,
                max_retries: 2,
                queue_depth: 16,
                grace_ns: 12_000_000_000,
            },
        }
    }
}

/// The per-device I/O budget derived from a device's [`BlkDeviceClass`]: the
/// deadline a single request is given before it fails closed, the number of
/// times a *retryable* failure may be reissued, and how many requests may be
/// in flight to the device at once.
///
/// These are scaling *policy* defaults sized for both desktop and server, not
/// fixed security ceilings: a larger machine or a faster device gets its own
/// budget from its class, never a hand-picked global constant.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct IoBudget {
    /// Per-request deadline in nanoseconds. A request unanswered for this
    /// long is reaped as [`BlkStatus::Timeout`] (`u64::MAX` = no deadline).
    pub deadline_ns: u64,
    /// How many times a retryable ([`BlkStatus::is_retryable`]) failure may
    /// be reissued before the device is failed closed.
    pub max_retries: u32,
    /// How many requests may be outstanding to the device at once.
    pub queue_depth: u32,
    /// The recovery **grace window** in nanoseconds: how long a device that
    /// has started stalling/resetting is held [`BlkHealthState::Recovering`]
    /// (its requests answered reissuably) before it is failed closed, so a
    /// transient blip is ridden out rather than punished. `u64::MAX` means
    /// "ride out indefinitely" (never used by a real class). Sized wider than
    /// `deadline_ns` so a single reset/spin-up cannot exhaust the window.
    pub grace_ns: u64,
}

impl IoBudget {
    /// Whether a block consumer should reissue a completion of `status` given
    /// it has already made `attempts` attempts against this device.
    ///
    /// This is the single shared **bounded-reissue policy** every consumer of
    /// a served block device obeys, so the kernel filesystem client and the
    /// volume manager's probe can never drift apart in when they retry versus
    /// fail closed. A completion is reissued only while it is *reissuable*
    /// ([`BlkStatus::is_retryable`] — the serving driver's "I am recovering,
    /// try again" answer under its own grace window) and only until this
    /// device's [`max_retries`](Self::max_retries) is reached: a device that
    /// keeps answering reissuably still fails closed deterministically rather
    /// than retrying forever (`AGENTS.md`'s ban on retry-until-it-works).
    #[must_use]
    pub const fn should_reissue(self, status: BlkStatus, attempts: u32) -> bool {
        status.is_retryable() && attempts < self.max_retries
    }
}

/// Encoded length of a [`BlkHealthCounters`] snapshot: ten little-endian
/// `u64` tallies.
pub const BLK_HEALTH_COUNTERS_LEN: usize = 10 * 8;

/// Cumulative, monotonic tallies of the device-level outcomes a block
/// consumer has observed from one served device since it was attached, plus
/// the number of reissues (retries) the consumer itself performed.
///
/// This is the single shared definition of the storage-health *counters*, so
/// every consumer of a served block device — the kernel filesystem client
/// today, a future user-space serve loop — folds outcomes into the same
/// buckets and cannot drift apart in what a "reset" or a "medium error" counts
/// as (`plans/FIX-IO.md` IO5). It is a pure value type: the live counters are
/// held as atomics by whichever component owns the I/O path, and a
/// [`BlkHealthCounters`] is a point-in-time snapshot the System Information
/// API reports (`AGENTS.md` §16.6) — never `/proc`-style scraped.
///
/// The per-status buckets partition every folded completion exactly once, so
/// `ok + degraded + transient + timeouts + resets + medium_errors + offline +
/// faults == completions` always holds; `reissues` is orthogonal (a single
/// logical request that is reissued twice folds three completions). Every
/// bucket saturates rather than wrapping: a tally is operational observability,
/// never a security or format bound (`AGENTS.md` §24.4), so an
/// implausibly-long-lived device pins a counter at [`u64::MAX`] instead of
/// silently wrapping to a smaller, misleading value.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct BlkHealthCounters {
    /// Total device-level completions folded (the sum of the buckets below).
    pub completions: u64,
    /// Reissuable completions the consumer retried under the shared
    /// bounded-reissue policy ([`IoBudget::should_reissue`]).
    pub reissues: u64,
    /// Completions the device answered healthy ([`BlkStatus::Ok`]).
    pub ok: u64,
    /// Completions served but with the device self-reporting unhealthy
    /// ([`BlkStatus::Degraded`]).
    pub degraded: u64,
    /// Recovered-error / comms-glitch completions ([`BlkStatus::TransientError`]).
    pub transient: u64,
    /// Completions reaped on the per-request deadline ([`BlkStatus::Timeout`]).
    pub timeouts: u64,
    /// Completions aborted by a device/hub reset ([`BlkStatus::Reset`]).
    pub resets: u64,
    /// Permanent bad-sector completions ([`BlkStatus::MediumError`]).
    pub medium_errors: u64,
    /// Completions where the device was unresponsive or surprise-removed
    /// ([`BlkStatus::Offline`] / [`BlkStatus::Removed`]).
    pub offline: u64,
    /// Unclassifiable / hard-fault completions ([`BlkStatus::Fatal`]).
    pub faults: u64,
}

impl BlkHealthCounters {
    /// Number of `u64` tallies a snapshot carries.
    pub const FIELD_COUNT: usize = 10;

    /// Index of the `completions` tally in wire order.
    pub const COMPLETIONS: usize = 0;
    /// Index of the `reissues` tally in wire order.
    pub const REISSUES: usize = 1;

    /// The wire-order index of the bucket a device-level `status` folds into.
    ///
    /// This is the single definition of the status → bucket assignment, so a
    /// pure [`fold`](Self::fold) and any lock-free counter that mirrors these
    /// tallies (the kernel filesystem client's atomics) place a completion in
    /// the same bucket and cannot diverge.
    #[must_use]
    pub const fn bucket_index(status: BlkStatus) -> usize {
        match status {
            BlkStatus::Ok => 2,
            BlkStatus::Degraded => 3,
            BlkStatus::TransientError => 4,
            BlkStatus::Timeout => 5,
            BlkStatus::Reset => 6,
            BlkStatus::MediumError => 7,
            BlkStatus::Offline | BlkStatus::Removed => 8,
            BlkStatus::Fatal => 9,
        }
    }

    /// Fold one device-level completion of `status` into the tallies: bump
    /// [`completions`](Self::completions) and the single bucket `status`
    /// belongs to (via [`bucket_index`](Self::bucket_index), the one shared
    /// assignment). Every bucket saturates, so a long-lived device never wraps
    /// a counter to a misleadingly-small value.
    pub const fn fold(&mut self, status: BlkStatus) {
        let mut fields = self.fields();
        fields[Self::COMPLETIONS] = fields[Self::COMPLETIONS].saturating_add(1);
        let bucket = Self::bucket_index(status);
        fields[bucket] = fields[bucket].saturating_add(1);
        *self = Self::from_fields(fields);
    }

    /// Record that the consumer reissued a reissuable completion. Orthogonal
    /// to [`fold`](Self::fold): the reissued attempt folds its own completion
    /// when it lands.
    pub const fn note_reissue(&mut self) {
        self.reissues = self.reissues.saturating_add(1);
    }

    /// The ten tallies in wire order, for encode/decode.
    const fn fields(&self) -> [u64; Self::FIELD_COUNT] {
        [
            self.completions,
            self.reissues,
            self.ok,
            self.degraded,
            self.transient,
            self.timeouts,
            self.resets,
            self.medium_errors,
            self.offline,
            self.faults,
        ]
    }

    /// Rebuild a snapshot from the wire-order tallies — the inverse of
    /// [`to_le_bytes`](Self::to_le_bytes)'s field order, so a lock-free counter
    /// can snapshot its atomics into one value without duplicating that order.
    #[must_use]
    pub const fn from_fields(fields: [u64; Self::FIELD_COUNT]) -> Self {
        Self {
            completions: fields[0],
            reissues: fields[1],
            ok: fields[2],
            degraded: fields[3],
            transient: fields[4],
            timeouts: fields[5],
            resets: fields[6],
            medium_errors: fields[7],
            offline: fields[8],
            faults: fields[9],
        }
    }

    /// Encode the snapshot as [`BLK_HEALTH_COUNTERS_LEN`] little-endian bytes.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; BLK_HEALTH_COUNTERS_LEN] {
        let mut out = [0u8; BLK_HEALTH_COUNTERS_LEN];
        for (i, value) in self.fields().iter().enumerate() {
            put_u64(&mut out, i * 8, *value);
        }
        out
    }

    /// Decode a snapshot from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `bytes` is shorter than
    /// [`BLK_HEALTH_COUNTERS_LEN`]. Every field value is valid (a tally is any
    /// `u64`), so there is no further shape to fail closed on.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < BLK_HEALTH_COUNTERS_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            completions: read_u64(bytes, 0),
            reissues: read_u64(bytes, 8),
            ok: read_u64(bytes, 16),
            degraded: read_u64(bytes, 24),
            transient: read_u64(bytes, 32),
            timeouts: read_u64(bytes, 40),
            resets: read_u64(bytes, 48),
            medium_errors: read_u64(bytes, 56),
            offline: read_u64(bytes, 64),
            faults: read_u64(bytes, 72),
        })
    }
}

/// The shared recovery **grace-window** timer: a bounded, event-timed window a
/// stalling storage element — a single served device ([`BlkHealth`]) or a
/// whole interior fault domain ([`FaultDomain`]) — is held open before it is
/// failed closed. Both drive their recovery window through this one definition,
/// so the arm / elapsed / one-shot-deadline arithmetic can never diverge
/// between the per-device and fault-domain machines.
///
/// It holds no clock: the caller supplies the monotonic reading on every
/// query, so it is pure and provable host-side and there is nothing to spin on
/// (event-timed via [`GraceWindow::deadline_ns`], never a busy-poll).
#[derive(Copy, Clone, Debug)]
struct GraceWindow {
    /// The window duration in nanoseconds. `u64::MAX` means "ride out
    /// indefinitely" and never reads as elapsed (no real policy uses it).
    grace_ns: u64,
    /// The monotonic reading the current window opened at; meaningful only
    /// while `open`.
    opened_at_ns: u64,
    /// Whether a window is currently open.
    open: bool,
}

impl GraceWindow {
    /// A closed window that, once opened, lasts `grace_ns`.
    const fn new(grace_ns: u64) -> Self {
        Self {
            grace_ns,
            opened_at_ns: 0,
            open: false,
        }
    }

    /// Open the window at monotonic `now_ns` unless one is already open. A
    /// window already open keeps its original start, so a *continuing* blip
    /// cannot extend the window indefinitely and postpone the fail-closed.
    fn open_at(&mut self, now_ns: u64) {
        if !self.open {
            self.open = true;
            self.opened_at_ns = now_ns;
        }
    }

    /// Close the window (the element recovered or was failed closed).
    fn close(&mut self) {
        self.open = false;
        self.opened_at_ns = 0;
    }

    /// Whether an open window has elapsed at monotonic `now_ns`.
    /// `saturating_sub` keeps a non-monotonic reading from wrapping the elapsed
    /// time; a `u64::MAX` window never reads as elapsed.
    const fn elapsed(&self, now_ns: u64) -> bool {
        self.open
            && self.grace_ns != u64::MAX
            && now_ns.saturating_sub(self.opened_at_ns) >= self.grace_ns
    }

    /// The absolute monotonic time an open window closes at — the deadline a
    /// driver arms a **one-shot** timer for. `None` when closed or when the
    /// window rides out indefinitely (`grace_ns == u64::MAX`), so no timer is
    /// armed.
    const fn deadline_ns(&self) -> Option<u64> {
        if !self.open || self.grace_ns == u64::MAX {
            return None;
        }
        Some(self.opened_at_ns.saturating_add(self.grace_ns))
    }
}

/// The health of one served block device: an explicit state distinguishing a
/// device that is fine from one that is riding out a blip, from one that has
/// been failed closed, from one that is gone.
///
/// A returning device is a normal transition (`Recovering`/`Faulted`/
/// `Offline` back to `Healthy`), so a disk that flaps is never mistaken for a
/// steadily-healthy one yet always recovers without a reboot. The values are
/// ordered from healthiest to most-failed only for readability; nothing reads
/// the discriminant.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BlkHealthState {
    /// The device is answering normally.
    #[default]
    Healthy,
    /// The device is answering with valid data but reports itself unhealthy
    /// (a recovered-error threshold, a pending reallocation): still usable.
    Degraded,
    /// The device has started stalling/resetting and is inside its grace
    /// window: its requests are answered reissuably ([`BlkStatus::Reset`])
    /// while it is given a bounded chance to come back.
    Recovering,
    /// The grace window elapsed without the device coming back: it is failed
    /// closed to its consumers, but stays *recoverable* — a later successful
    /// answer returns it to [`BlkHealthState::Healthy`].
    Faulted,
    /// The device is present but persistently unresponsive. Sticky until it
    /// demonstrably answers again.
    Offline,
    /// The device has been surprise-removed. Sticky until it answers again
    /// (a verified re-insert).
    Removed,
    /// An unclassified, unrecoverable failure was reported. Sticky; only a
    /// demonstrated successful answer (e.g. after a driver restart) clears it.
    Failed,
}

impl BlkHealthState {
    /// Whether the device is currently answering with valid data (the only
    /// states a consumer should treat as live for new work).
    #[must_use]
    pub const fn is_operational(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

/// The per-device health state machine and recovery **grace window**, owned
/// by the serving block-driver process (one per served logical unit).
///
/// It turns the raw per-request outcome the device produced into the health
/// status the consumer is told, riding out a transient blip for a bounded,
/// wall-clock-timed grace window ([`IoBudget::grace_ns`]) before failing
/// closed. It is pure and event-timed: the caller supplies the monotonic
/// `now_ns` (the kernel `clock_get` reading) on each observation, so there is
/// no timer to spin on and the whole machine is provable host-side.
///
/// The recovery model is the reply-reissuable one: while `Recovering` the
/// device's requests complete with a reissuable [`BlkStatus::Reset`] within
/// their own per-request deadline, so the serve loop never parks on one
/// device's blip and starve the others (head-of-line freedom). Only
/// *device-level* outcomes are observed here; a request-level refusal (a
/// write to a read-only unit, an out-of-range LBA) is health-neutral and is
/// never fed in, so a hostile or malformed request can never drive a healthy
/// device to `Faulted`.
#[derive(Copy, Clone, Debug)]
pub struct BlkHealth {
    state: BlkHealthState,
    budget: IoBudget,
    /// The recovery grace-window timer, open only while `state ==
    /// Recovering`; the one shared with [`FaultDomain`] so the timing cannot
    /// diverge.
    grace: GraceWindow,
}

impl BlkHealth {
    /// A freshly-`Healthy` device served with `class`'s I/O budget.
    #[must_use]
    pub const fn new(class: BlkDeviceClass) -> Self {
        Self {
            state: BlkHealthState::Healthy,
            budget: class.budget(),
            grace: GraceWindow::new(class.budget().grace_ns),
        }
    }

    /// The current health state.
    #[must_use]
    pub const fn state(&self) -> BlkHealthState {
        self.state
    }

    /// The I/O budget (including the grace window) this device is served with.
    #[must_use]
    pub const fn budget(&self) -> IoBudget {
        self.budget
    }

    /// The absolute monotonic time (the kernel `clock_get` reading) at which
    /// this device's open recovery grace window expires, if one is open.
    ///
    /// While `Recovering`, a serving driver that has no further request to
    /// fold through [`observe`](Self::observe) arms a **one-shot** timer for
    /// this deadline and calls [`poll`](Self::poll) when it fires, so the
    /// window still expires on an otherwise-quiet device without a busy-poll
    /// (event-timed, never a spin). Returns `None` when no window is open (any
    /// non-`Recovering` state) or when the class rides out indefinitely
    /// (`grace_ns == u64::MAX`), so the caller arms no timer.
    #[must_use]
    pub const fn grace_deadline_ns(&self) -> Option<u64> {
        self.grace.deadline_ns()
    }

    /// Advance the grace window on a pure time tick at monotonic `now_ns`,
    /// returning the (possibly unchanged) state.
    ///
    /// This is the time-driven counterpart to [`observe`](Self::observe): it
    /// folds *no* request outcome, only the passage of time. A device left
    /// `Recovering` because it received no further request still fails closed
    /// to [`BlkHealthState::Faulted`] once its grace window elapses, driven by
    /// the one-shot timer [`grace_deadline_ns`](Self::grace_deadline_ns) names
    /// rather than a busy-poll. It is idempotent and side-effect-free in every
    /// state but an expired `Recovering` one, so a driver may call it whenever
    /// its grace timer fires without tracking whether a request already
    /// advanced the machine.
    pub fn poll(&mut self, now_ns: u64) -> BlkHealthState {
        if matches!(self.state, BlkHealthState::Recovering) && self.grace.elapsed(now_ns) {
            self.state = BlkHealthState::Faulted;
            self.grace.close();
        }
        self.state
    }

    /// Fold one device-level outcome into the health state at monotonic time
    /// `now_ns`, returning the [`BlkStatus`] the consumer should be told.
    ///
    /// `raw` is what the device/transport produced for this request
    /// ([`BlkStatus::Ok`] on success, else the classified error). The return
    /// value may differ from `raw`: a transient error inside the grace window
    /// is reported as a reissuable [`BlkStatus::Reset`], while the same error
    /// after the window has elapsed is reported as [`BlkStatus::Offline`]
    /// (failed closed). A valid answer always recovers the device.
    pub fn observe(&mut self, raw: BlkStatus, now_ns: u64) -> BlkStatus {
        match raw {
            // Any valid answer from the device demonstrates it is alive and
            // clears a fault/recovery episode (sticky-but-recoverable).
            BlkStatus::Ok => {
                self.recover(BlkHealthState::Healthy);
                BlkStatus::Ok
            }
            BlkStatus::Degraded => {
                self.recover(BlkHealthState::Degraded);
                BlkStatus::Degraded
            }
            // A definitive per-sector verdict is still an answer: the device
            // is reachable (only its media is bad), so recover the device
            // while surfacing the medium error for this one request.
            BlkStatus::MediumError => {
                if !self.state.is_operational() {
                    self.recover(BlkHealthState::Healthy);
                }
                BlkStatus::MediumError
            }
            BlkStatus::TransientError | BlkStatus::Timeout | BlkStatus::Reset => {
                self.on_transient(now_ns)
            }
            // A gone device is sticky until it answers again; it does not pass
            // through the grace window (there is nothing to ride out).
            BlkStatus::Offline => {
                self.state = BlkHealthState::Offline;
                self.grace.close();
                BlkStatus::Offline
            }
            BlkStatus::Removed => {
                self.state = BlkHealthState::Removed;
                self.grace.close();
                BlkStatus::Removed
            }
            BlkStatus::Fatal => {
                self.state = BlkHealthState::Failed;
                self.grace.close();
                BlkStatus::Fatal
            }
        }
    }

    /// Drive the grace window for a retryable (transient/timeout/reset)
    /// outcome.
    fn on_transient(&mut self, now_ns: u64) -> BlkStatus {
        match self.state {
            // Already known gone/dead: a further non-answer changes nothing,
            // and the device stays failed closed until it demonstrably
            // answers (handled by the valid-answer arms of `observe`).
            BlkHealthState::Offline | BlkHealthState::Faulted => BlkStatus::Offline,
            BlkHealthState::Removed => BlkStatus::Removed,
            BlkHealthState::Failed => BlkStatus::Fatal,
            // First sign of trouble on a live device: open the grace window.
            BlkHealthState::Healthy | BlkHealthState::Degraded => {
                self.state = BlkHealthState::Recovering;
                self.grace.open_at(now_ns);
                BlkStatus::Reset
            }
            // Inside an open window: keep answering reissuably until the
            // window elapses, then fail closed.
            BlkHealthState::Recovering => {
                if self.grace.elapsed(now_ns) {
                    self.state = BlkHealthState::Faulted;
                    self.grace.close();
                    BlkStatus::Offline
                } else {
                    BlkStatus::Reset
                }
            }
        }
    }

    /// Return the device to a live state, clearing any recovery episode.
    fn recover(&mut self, to: BlkHealthState) {
        self.state = to;
        self.grace.close();
    }
}

/// The relative timeout a block serve loop should arm its wait on so an
/// otherwise-quiet `Recovering` device still has its grace window expired
/// promptly — the soonest armed grace deadline across `healths`, expressed
/// relative to monotonic `now_ns`.
///
/// A serving driver owns one [`BlkHealth`] per logical unit it serves and
/// parks on a wait-set between requests. A device that went `Recovering` but
/// then receives no further request would, without this, stay `Recovering`
/// forever — its grace window is only advanced by [`BlkHealth::observe`] on a
/// request or [`BlkHealth::poll`] on a time tick. This gives the loop the
/// single relative timeout to pass to `waitset_wait`/`irq_wait` so it wakes
/// exactly when the nearest window is due and drives every LUN's
/// [`BlkHealth::poll`], failing an unrecovered device closed on time without a
/// busy-poll (event-timed, never a spin).
///
/// Returns:
/// - `Some(0)` if a window has already elapsed at `now_ns` (poll immediately);
/// - `Some(ns)` for the soonest window still `ns` nanoseconds away;
/// - `None` if no device has an armed window (the loop waits with no timeout,
///   the `u64::MAX` `waitset_wait` convention), which covers both the
///   all-operational case and a class that rides out indefinitely
///   (`grace_ns == u64::MAX`, [`BlkHealth::grace_deadline_ns`] is `None`).
///
/// It is pure and reused by every block serve loop so the idle-timer arithmetic
/// exists once, never copied per driver.
pub fn recovery_wait_timeout<'a, I>(healths: I, now_ns: u64) -> Option<u64>
where
    I: IntoIterator<Item = &'a BlkHealth>,
{
    nearest_relative_deadline(
        healths.into_iter().filter_map(BlkHealth::grace_deadline_ns),
        now_ns,
    )
}

/// The relative timeout a serve loop should arm so the soonest of a set of
/// absolute one-shot `deadlines` elapses promptly, expressed relative to
/// monotonic `now_ns`.
///
/// This is the shared arithmetic behind both [`recovery_wait_timeout`] (over
/// per-device [`BlkHealth`] windows) and [`fault_domain_wait_timeout`] (over
/// interior [`FaultDomain`] windows), so the "nearest armed grace window,
/// measured from now, saturating at zero" rule lives in exactly one place and
/// the two seams cannot drift apart. Returns `Some(0)` for a window already
/// due (poll at once, never an underflowed timeout), `Some(ns)` for the
/// soonest still `ns` away, and `None` when the set arms no window (park with
/// no timeout, the `u64::MAX` `waitset_wait` convention).
fn nearest_relative_deadline<I>(deadlines: I, now_ns: u64) -> Option<u64>
where
    I: IntoIterator<Item = u64>,
{
    deadlines
        .into_iter()
        .min()
        .map(|deadline| deadline.saturating_sub(now_ns))
}

/// The recovery action a serving block driver takes for one device before its
/// next attempt, decided by the bounded escalation ladder [`RecoveryLadder`].
///
/// The ladder escalates from the least disruptive action to the most, so a
/// one-off comms glitch is not punished with a needless bus reset while a
/// genuinely stuck data path is still cleared before the workload gives up.
/// A driver maps each action to the concrete mechanism it has (a USB
/// mass-storage driver clears its bulk pipes; a virtio/NVMe driver resets its
/// queue/controller); an action a driver's hardware cannot express is a no-op
/// that still advances the ladder, so the escalation is honest on every
/// transport.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecoveryAction {
    /// The device is operational: there is nothing to recover and the ladder
    /// is back at the bottom. Emitted for a `Healthy`/`Degraded` device.
    None,
    /// Reissue the transfer without disturbing the device — the first response
    /// to a transient blip, since a one-off glitch (a recovered ECC error, a
    /// single dropped packet) often clears itself and a reset would only add
    /// latency.
    Retry,
    /// Reset the device's data path (clear stalled endpoints / re-init the
    /// queue) before the next attempt — the escalation once a blip has
    /// recurred rather than cleared on its own.
    Reset,
    /// The bounded ladder is exhausted (the device's per-class retry budget is
    /// spent, or it has already been failed closed): stop escalating and let
    /// the grace window ([`BlkHealth`]) fail the device closed on time. Never
    /// an unbounded retry.
    GiveUp,
}

/// The bounded recovery-**escalation** ladder a serving block driver climbs
/// while one of its devices is riding out a blip, owned per served logical
/// unit alongside that unit's [`BlkHealth`].
///
/// It is the driver-action counterpart of [`BlkHealth`]: where `BlkHealth`
/// decides *what the consumer is told* (reissuable while `Recovering`, failed
/// closed once the grace window elapses), the ladder decides *what the driver
/// does to the hardware* between reissued attempts — reissue as-is, reset the
/// data path, or give up escalating — bounded by the same per-class
/// [`IoBudget::max_retries`] both this and the consumer's
/// [`IoBudget::should_reissue`] read, so the driver's escalation and the
/// consumer's reissue budget derive from one policy and cannot drift apart
/// (§2.2). A device that keeps stalling therefore climbs a *finite* ladder and
/// is never reset forever (the charter's ban on retry-until-it-works).
///
/// It is pure and holds no clock or timer: the escalation advances one rung
/// per reissued attempt, and reissued attempts are already spaced by the
/// consumer's own reissue cadence and per-request deadline, so the driver
/// neither spins nor parks a backoff timer of its own — which is *stronger*
/// than a driver-side backoff for head-of-line freedom, since the serve loop
/// never sleeps on one recovering device while its siblings wait. The whole
/// ladder is therefore provable host-side.
#[derive(Copy, Clone, Debug)]
pub struct RecoveryLadder {
    budget: IoBudget,
    /// How many recovery attempts have been taken in the current episode; zero
    /// whenever the device is operational.
    attempts: u32,
}

impl RecoveryLadder {
    /// A ladder at the bottom rung for a device of `class`, reading the same
    /// per-class budget its [`BlkHealth`] does.
    #[must_use]
    pub const fn new(class: BlkDeviceClass) -> Self {
        Self {
            budget: class.budget(),
            attempts: 0,
        }
    }

    /// How many recovery attempts have been taken in the current episode.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The next recovery action for a device now in health `state`, advancing
    /// the ladder.
    ///
    /// The single entry point, so the escalation policy lives in one place and
    /// a driver cannot re-derive it differently:
    /// - An operational device (`Healthy`/`Degraded`) resets the ladder and
    ///   yields [`RecoveryAction::None`] — the prior episode, if any, is over.
    /// - A `Recovering` device escalates: the first attempt is a plain
    ///   [`RecoveryAction::Retry`]; every subsequent attempt is a
    ///   [`RecoveryAction::Reset`], up to the class's
    ///   [`IoBudget::max_retries`], after which it is [`RecoveryAction::GiveUp`]
    ///   and the grace window is left to fail the device closed on time.
    /// - A device already failed closed (`Faulted`/`Offline`/`Removed`/
    ///   `Failed`) yields [`RecoveryAction::GiveUp`]: the episode ended, so the
    ///   driver stops escalating resets; a later genuine answer returns the
    ///   device to `Healthy` and re-arms the ladder.
    pub fn next_action(&mut self, state: BlkHealthState) -> RecoveryAction {
        match state {
            BlkHealthState::Healthy | BlkHealthState::Degraded => {
                self.attempts = 0;
                RecoveryAction::None
            }
            BlkHealthState::Recovering => {
                let taken = self.attempts;
                if taken >= self.budget.max_retries {
                    RecoveryAction::GiveUp
                } else {
                    self.attempts = taken + 1;
                    if taken == 0 {
                        RecoveryAction::Retry
                    } else {
                        RecoveryAction::Reset
                    }
                }
            }
            BlkHealthState::Faulted
            | BlkHealthState::Offline
            | BlkHealthState::Removed
            | BlkHealthState::Failed => RecoveryAction::GiveUp,
        }
    }
}

/// The recovery state of one interior **fault domain** — the bus, hub, USB
/// controller, SAS/JBOD expander, or PCIe root complex that owns a group of
/// block devices beneath it.
///
/// A blip in an *owner* (a hub reset, a controller re-init) is one
/// fault-domain event, not N independent disk failures: the whole subtree
/// rides out a single shared grace window together. The three states mirror
/// the device-level [`BlkHealthState`] machine, collapsed to what an interior
/// node needs (an interior node has no "medium error" or "degraded-but-
/// serving" of its own — those are per-device).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum FaultDomainState {
    /// The owner is up; children answer on their own per-device health.
    #[default]
    Healthy,
    /// The owner is resetting / mid-blip: every child is held reissuable under
    /// one shared grace window while the owner is given a bounded chance to
    /// come back.
    Recovering,
    /// The grace window elapsed without the owner returning: the whole subtree
    /// is failed closed to consumers, but stays *recoverable* — a demonstrated
    /// owner recovery returns it to [`FaultDomainState::Healthy`] with no
    /// reboot.
    Offline,
}

/// The recovery state machine of one fault-domain *owner* — a group of served
/// block devices that a single upstream element fails and recovers together.
///
/// The owner is usually an interior node of the discovered hardware tree
/// ([`crate::hwtree`]) — a bus, hub, controller, expander, or root complex —
/// read from the tree, never hard-coded, so a USB hub, a SAS expander, and a
/// PCIe root complex are all just interior nodes. It need not be a *bus* node,
/// though: a leaf driver's own shared transport that fans out to several
/// logical units (a USB mass-storage device's Bulk-Only pipe pair over its
/// LUNs) is equally a fault-domain owner of those units — a reset of that one
/// transport hits all of them at once — identified by the driver's own
/// discovered transport grant rather than a tree node. Either way the owner id
/// is opaque here and never a board constant.
///
/// It turns an *owner-level* event — the owner began a reset/blip
/// ([`quiesce`](Self::quiesce)), the owner demonstrably returned
/// ([`resume`](Self::resume)), or the shared grace window elapsed
/// ([`poll`](Self::poll)) — into the coherent [`BlkStatus`] every child in the
/// subtree is told ([`child_status`](Self::child_status)). One hub reset is
/// therefore one recovery episode across all its disks, not N spurious
/// failures.
///
/// It reuses the same `GraceWindow` primitive as the per-device
/// [`BlkHealth`], so an interior node and a leaf device time their recovery
/// window identically. It holds no clock and no children: the caller supplies
/// the monotonic reading and drives its children's own [`BlkHealth`], so the
/// machine is pure, event-timed (never a busy-poll), and provable host-side.
/// Which nodes are children is a property of the hardware tree the caller
/// walks, not of this state machine — keeping it platform-neutral (it stores
/// only the owner's opaque node id).
///
/// Sticky-but-recoverable: once `Offline`, only a demonstrated owner recovery
/// ([`resume`](Self::resume)) clears it, so a flapping hub cannot masquerade
/// as healthy, yet a genuine return always recovers the subtree.
#[derive(Copy, Clone, Debug)]
pub struct FaultDomain {
    /// The owner's opaque id — an interior hardware-tree node id (the caller
    /// reads parenthood from [`crate::hwtree`]) or a leaf driver's own shared
    /// transport grant. Identity only; never dereferenced here.
    owner: u32,
    state: FaultDomainState,
    /// The shared recovery window for the whole subtree, open only while
    /// `state == Recovering`.
    grace: GraceWindow,
}

impl FaultDomain {
    /// A freshly-`Healthy` fault domain owned by `owner` (an interior
    /// hardware-tree node id, or a leaf driver's own shared-transport id),
    /// whose recovery window lasts `grace_ns`.
    ///
    /// `grace_ns` is **policy**, derived by the caller from the owner's
    /// discovered class (e.g. the widest [`IoBudget::grace_ns`] among the
    /// children it fans out to, so a subtree of spinning disks rides out a
    /// longer reset than one of removable units) — never a hand-picked global
    /// constant, and never a security/validation bound.
    #[must_use]
    pub const fn new(owner: u32, grace_ns: u64) -> Self {
        Self {
            owner,
            state: FaultDomainState::Healthy,
            grace: GraceWindow::new(grace_ns),
        }
    }

    /// The owner's opaque id (an interior hardware-tree node, or a leaf
    /// driver's shared transport).
    #[must_use]
    pub const fn owner(&self) -> u32 {
        self.owner
    }

    /// The current fault-domain state.
    #[must_use]
    pub const fn state(&self) -> FaultDomainState {
        self.state
    }

    /// Begin (or continue) an owner reset/blip at monotonic `now_ns`, opening
    /// the shared grace window, and return the resulting state.
    ///
    /// A `Healthy` owner enters `Recovering` and arms the window; a continuing
    /// blip keeps the *original* window start, so an owner that keeps resetting
    /// cannot postpone its fail-closed indefinitely. An owner already `Offline`
    /// stays `Offline` — a further reset is not a recovery (only
    /// [`resume`](Self::resume) clears a failed subtree).
    pub fn quiesce(&mut self, now_ns: u64) -> FaultDomainState {
        match self.state {
            FaultDomainState::Healthy | FaultDomainState::Recovering => {
                self.state = FaultDomainState::Recovering;
                self.grace.open_at(now_ns);
            }
            FaultDomainState::Offline => {}
        }
        self.state
    }

    /// Record that the owner has demonstrably returned: the whole subtree
    /// recovers to `Healthy` and children resume on their own per-device
    /// health, whatever state the domain was in.
    ///
    /// This is the only transition that clears an `Offline` subtree (no
    /// reboot), so recovery is always a demonstrated event, never a guess.
    pub fn resume(&mut self) -> FaultDomainState {
        self.state = FaultDomainState::Healthy;
        self.grace.close();
        self.state
    }

    /// Advance the shared grace window on a pure time tick at monotonic
    /// `now_ns`: a `Recovering` subtree whose owner has not returned by the
    /// window's close fails closed to `Offline`.
    ///
    /// This is the time-driven counterpart to [`quiesce`](Self::quiesce),
    /// driven by the one-shot timer [`grace_deadline_ns`](Self::grace_deadline_ns)
    /// names rather than a busy-poll. Idempotent and side-effect-free off the
    /// open-recovery path.
    pub fn poll(&mut self, now_ns: u64) -> FaultDomainState {
        if matches!(self.state, FaultDomainState::Recovering) && self.grace.elapsed(now_ns) {
            self.state = FaultDomainState::Offline;
            self.grace.close();
        }
        self.state
    }

    /// The absolute monotonic time the open recovery window closes at — the
    /// deadline a driver arms a **one-shot** timer for to call
    /// [`poll`](Self::poll). `None` when no window is open.
    #[must_use]
    pub const fn grace_deadline_ns(&self) -> Option<u64> {
        self.grace.deadline_ns()
    }

    /// The [`BlkStatus`] a child device's in-flight request should be told
    /// given the domain state at monotonic `now_ns`, or `None` when the domain
    /// imposes nothing and the child answers on its own per-device
    /// [`BlkHealth`].
    ///
    /// While `Recovering` a child's request is answered reissuably
    /// ([`BlkStatus::Reset`]) so the blip is invisible to the workload if the
    /// owner returns inside the window; once the window has elapsed (or the
    /// domain is already `Offline`) the child is failed closed
    /// ([`BlkStatus::Offline`]). This is a pure query — the fail-closed
    /// *transition* happens in [`poll`](Self::poll) / [`quiesce`](Self::quiesce)
    /// — so reading it never mutates the machine.
    #[must_use]
    pub const fn child_status(&self, now_ns: u64) -> Option<BlkStatus> {
        match self.state {
            FaultDomainState::Healthy => None,
            FaultDomainState::Recovering => {
                if self.grace.elapsed(now_ns) {
                    Some(BlkStatus::Offline)
                } else {
                    Some(BlkStatus::Reset)
                }
            }
            FaultDomainState::Offline => Some(BlkStatus::Offline),
        }
    }
}

/// The relative timeout a serving/bus driver should arm its wait on so an
/// otherwise-quiet `Recovering` interior [`FaultDomain`] still has its shared
/// grace window expired promptly — the soonest armed deadline across
/// `domains`, expressed relative to monotonic `now_ns`.
///
/// This is the interior-node counterpart of [`recovery_wait_timeout`]: a
/// serve loop that owns fault domains (a hub/controller reset holds its whole
/// subtree reissuable under one window) parks between events exactly as it
/// does for per-device windows, and would, without this, leave a quiesced
/// domain `Recovering` forever if no child request arrived to fold — its
/// window is only advanced by [`FaultDomain::poll`] on a time tick or a fresh
/// [`FaultDomain::quiesce`]. It gives the loop the single relative timeout to
/// pass to `waitset_wait`/`irq_wait` so it wakes when the nearest subtree
/// window is due and drives every domain's `poll`, failing an unrecovered
/// subtree closed on time without a busy-poll (event-timed, never a spin).
///
/// Its return convention matches [`recovery_wait_timeout`] exactly — both
/// delegate to the same `nearest_relative_deadline` core (§2.2) — so a serve
/// loop that owns *both* per-device and fault-domain windows takes the min of
/// the two and cannot compute them by different rules: `Some(0)` for a window
/// already due, `Some(ns)` for the soonest still `ns` away, and `None` when no
/// domain has an armed window (park with no timeout).
pub fn fault_domain_wait_timeout<'a, I>(domains: I, now_ns: u64) -> Option<u64>
where
    I: IntoIterator<Item = &'a FaultDomain>,
{
    nearest_relative_deadline(
        domains
            .into_iter()
            .filter_map(FaultDomain::grace_deadline_ns),
        now_ns,
    )
}

/// The effective [`BlkStatus`] one child device's completion should carry,
/// given the device's own `device_status` and the chain of interior fault
/// [`domains`](FaultDomain) above it in the discovered hardware tree
/// (nearest-owner first, up to the root — resolved by re-applying
/// [`hwtree::fault_domain_owner`](crate::hwtree::fault_domain_owner)), at
/// monotonic `now_ns`.
///
/// This is the single definition of how a serving driver folds a leaf
/// device's own outcome with what each ancestor bus/hub/controller imposes,
/// so the leaf-device machine ([`BlkHealth`]) and the interior-node machine
/// ([`FaultDomain`]) compose coherently rather than through a rule each serve
/// loop re-derives. A hub mid-reset ([`FaultDomain::child_status`] returns
/// [`BlkStatus::Reset`]) turns a child's `Ok` into a reissuable `Reset` so the
/// blip is invisible if the owner returns inside the window; an ancestor whose
/// window has elapsed fails the child closed to [`BlkStatus::Offline`]; and a
/// device's own definitive verdict (a bad-sector [`BlkStatus::MediumError`])
/// still wins over a less-severe ancestor status. The precedence is
/// [`BlkStatus::combine`]'s total order, folded over the device status and
/// every domain that imposes one, so the chain composes associatively and a
/// deeper failing domain can never be masked by a shallower healthy one.
///
/// It is a pure query over the domains' states (each domain's fail-closed
/// *transition* still happens in its own [`FaultDomain::poll`] /
/// [`quiesce`](FaultDomain::quiesce)), so reading the effective status never
/// mutates a machine.
#[must_use]
pub fn effective_child_status<'a, I>(device_status: BlkStatus, domains: I, now_ns: u64) -> BlkStatus
where
    I: IntoIterator<Item = &'a FaultDomain>,
{
    domains
        .into_iter()
        .filter_map(|domain| domain.child_status(now_ns))
        .fold(device_status, BlkStatus::combine)
}

/// One validated request's outcome, kept separate at the source so device
/// health is only ever driven by the *device's* own behaviour. A request the
/// driver refuses up front, or that the device rejects as out-of-range, is a
/// [`Served::Refused`] the recovery arm frames verbatim without ever counting
/// it against the grace window — a hostile or malformed request can never
/// drive a healthy device toward [`BlkHealthState::Faulted`].
enum Served {
    /// The request was validated and handed to the device; this is what the
    /// device call returned (its [`DriverError`] carries the health signal).
    Device(Result<BlkCompletion, DriverError>),
    /// The request itself was refused (malformed, read-only, or larger than
    /// the window). Health-neutral.
    Refused(Errno),
}

/// A data request's extent could not be resolved: either the request was
/// refused (health-neutral) or the device's own geometry call failed
/// (device-level, and so health-relevant).
enum Extent {
    Refused(Errno),
    Device(DriverError),
}

/// Serve one block-service request over `device` and the endpoint's shared
/// data `window`, folding the device's outcome into `health` and framing the
/// resulting completion into `reply`, returning its length.
///
/// This is the one shared block-service request engine every serving driver
/// reuses (`usb_msd`, and the virtio/eMMC serve paths as they are brought up),
/// so the validation, the fail-closed refusals, the success paths, and the
/// grace-window recovery model live in exactly one place and cannot diverge
/// between drivers. It is pure and alloc-free, so the whole request surface is
/// proven host-side over an in-memory [`Block`] double.
///
/// The caller is untrusted: every request field is validated against the live
/// geometry and the mapped window before the device is touched, and a request
/// that cannot mean what it says is answered with an in-band error completion,
/// never a partial application.
///
/// `read_only` is the device's write policy (e.g. a MODE SENSE WP bit); a
/// write against it is refused before the device is touched, and the geometry
/// reply carries it as [`BLK_FLAG_READ_ONLY`] so a consumer can present the
/// volume honestly.
///
/// `now_ns` is the monotonic clock reading (the kernel monotonic clock the
/// driver reads before serving each request) that times the recovery grace
/// window. A device-level transient stall inside the window is answered with a
/// reissuable [`BlkStatus::Reset`]; the same stall after the window elapses is
/// failed closed as [`BlkStatus::Offline`]. A valid answer recovers the
/// device. A request-level refusal is framed verbatim and never touches
/// `health`, so head-of-line freedom holds: the serve loop never parks on one
/// device's blip.
///
/// Framing a completion cannot fail (the buffer is exactly the sized
/// destination); a defensive `unwrap_or(0)` keeps this panic-free on any path
/// rather than relying on that invariant.
pub fn serve_request_recovering<B: Block>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
    reply: &mut [u8; BLK_COMPLETION_LEN],
    health: &mut BlkHealth,
    now_ns: u64,
) -> usize {
    match classify(device, read_only, request, window) {
        Served::Refused(err) => encode_error_completion(reply, err).unwrap_or(0),
        Served::Device(Ok(completion)) => {
            let status = health.observe(BlkStatus::Ok, now_ns);
            completion.encode_status(status, reply).unwrap_or(0)
        }
        Served::Device(Err(err)) => match BlkStatus::for_driver_health(err) {
            // A device-health error drives the recovery state machine: the
            // status the consumer is told may be softened to reissuable while
            // inside the grace window or hardened to offline once it elapses.
            Some(raw) => {
                let status = health.observe(raw, now_ns);
                BlkCompletion::default()
                    .encode_status(status, reply)
                    .unwrap_or(0)
            }
            // A request-level rejection the device raised (an out-of-range
            // LBA) is not about the device's health: frame it verbatim.
            None => encode_error_completion(reply, err.as_errno()).unwrap_or(0),
        },
    }
}

/// Decode, validate, and execute one request on `device`, classifying the
/// outcome as a device call or a health-neutral request refusal.
fn classify<B: Block>(
    device: &mut B,
    read_only: bool,
    request: &[u8],
    window: &mut [u8],
) -> Served {
    let request = match BlkRequest::decode(request) {
        Ok(request) => request,
        Err(err) => return Served::Refused(err),
    };
    match request.op {
        BlkOp::Geometry => Served::Device(device.geometry().map(|geometry| BlkCompletion {
            block_size: geometry.block_size,
            block_count: geometry.block_count,
            flags: if read_only { BLK_FLAG_READ_ONLY } else { 0 },
        })),
        BlkOp::Read => match data_extent(device, request.blocks, window.len()) {
            Ok(len) => Served::Device(
                device
                    .read_blocks(request.lba, &mut window[..len])
                    .map(|()| BlkCompletion::default()),
            ),
            Err(Extent::Refused(err)) => Served::Refused(err),
            Err(Extent::Device(err)) => Served::Device(Err(err)),
        },
        BlkOp::Write => {
            // The write policy is enforced here as well as in the Block
            // implementation, so no request shape can route around it.
            if read_only {
                return Served::Refused(Errno::PermissionDenied);
            }
            match data_extent(device, request.blocks, window.len()) {
                Ok(len) => Served::Device(
                    device
                        .write_blocks(request.lba, &window[..len])
                        .map(|()| BlkCompletion::default()),
                ),
                Err(Extent::Refused(err)) => Served::Refused(err),
                Err(Extent::Device(err)) => Served::Device(Err(err)),
            }
        }
        BlkOp::Flush => Served::Device(device.flush().map(|()| BlkCompletion::default())),
    }
}

/// Byte length a data request covers, validated against the geometry and the
/// mapped window (the device's own range check still applies inside the
/// [`Block`] call). A zero-block or oversized request is a health-neutral
/// refusal; a failing geometry call is a device-level fault.
fn data_extent<B: Block>(device: &B, blocks: u32, window_len: usize) -> Result<usize, Extent> {
    if blocks == 0 {
        return Err(Extent::Refused(Errno::OutOfRange));
    }
    let geometry = device.geometry().map_err(Extent::Device)?;
    let len = (blocks as usize)
        .checked_mul(geometry.block_size as usize)
        .ok_or(Extent::Refused(Errno::LengthOutOfRange))?;
    if len > window_len {
        return Err(Extent::Refused(Errno::LengthOutOfRange));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let req = BlkRequest {
            op: BlkOp::Read,
            lba: 0x1_0000_0001,
            blocks: 64,
        };
        let mut buf = [0u8; BLK_REQUEST_LEN];
        let n = req.encode(&mut buf).expect("encodes");
        assert_eq!(n, BLK_REQUEST_LEN);
        assert_eq!(BlkRequest::decode(&buf[..n]), Ok(req));
    }

    #[test]
    fn request_encode_rejects_small_buffer() {
        let req = BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        };
        let mut buf = [0u8; BLK_REQUEST_LEN - 1];
        assert_eq!(req.encode(&mut buf), Err(Errno::BufferTooSmall));
    }

    #[test]
    fn request_decode_rejects_truncated() {
        let buf = [0u8; BLK_REQUEST_LEN - 1];
        assert_eq!(BlkRequest::decode(&buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn request_decode_rejects_unknown_op() {
        let mut buf = [0u8; BLK_REQUEST_LEN];
        buf[0] = 4;
        assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));
    }

    #[test]
    fn request_decode_rejects_dataless_op_naming_blocks() {
        for op in [BlkOp::Geometry, BlkOp::Flush] {
            let req = BlkRequest {
                op,
                lba: 0,
                blocks: 1,
            };
            let mut buf = [0u8; BLK_REQUEST_LEN];
            req.encode(&mut buf).expect("encodes");
            assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));

            let req = BlkRequest {
                op,
                lba: 1,
                blocks: 0,
            };
            req.encode(&mut buf).expect("encodes");
            assert_eq!(BlkRequest::decode(&buf), Err(Errno::OutOfRange));
        }
    }

    #[test]
    fn completion_round_trips() {
        let completion = BlkCompletion {
            block_size: 512,
            block_count: 0x2_0000_0000,
            flags: BLK_FLAG_READ_ONLY,
        };
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        let n = completion.encode(&mut buf).expect("encodes");
        assert_eq!(n, BLK_COMPLETION_LEN);
        assert_eq!(decode_completion(&buf[..n]), Ok(completion));
    }

    #[test]
    fn completion_encode_rejects_small_buffer() {
        let mut buf = [0u8; BLK_COMPLETION_LEN - 1];
        assert_eq!(
            BlkCompletion::default().encode(&mut buf),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn error_completion_round_trips() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        let n = encode_error_completion(&mut buf, Errno::PermissionDenied).expect("encodes");
        // The specific errno is preserved, and a non-health error classifies
        // as `Fatal` on the health axis with the errno carried through.
        assert_eq!(decode_completion(&buf[..n]), Err(Errno::PermissionDenied));
        let outcome = decode_outcome(&buf[..n]);
        assert_eq!(outcome.status, BlkStatus::Fatal);
        assert_eq!(outcome.error, Errno::PermissionDenied);
    }

    #[test]
    fn health_statuses_round_trip_on_the_wire() {
        // Every health error carries its distinct errno; a `Degraded`
        // completion still delivers valid geometry.
        for status in [
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
            BlkStatus::MediumError,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ] {
            let mut buf = [0u8; BLK_COMPLETION_LEN];
            BlkCompletion::default()
                .encode_status(status, &mut buf)
                .expect("encodes");
            let outcome = decode_outcome(&buf);
            assert_eq!(outcome.status, status);
            assert_eq!(outcome.error, status.default_errno());
            assert!(!outcome.status.data_valid());
            assert_eq!(decode_completion(&buf), Err(status.default_errno()));
        }

        let geometry = BlkCompletion {
            block_size: 4096,
            block_count: 7,
            flags: BLK_FLAG_READ_ONLY,
        };
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        geometry
            .encode_status(BlkStatus::Degraded, &mut buf)
            .expect("encodes");
        let outcome = decode_outcome(&buf);
        assert_eq!(outcome.status, BlkStatus::Degraded);
        assert!(outcome.status.data_valid());
        assert_eq!(outcome.data(), Ok(geometry));
    }

    #[test]
    fn blk_status_from_u32_round_trips_and_fails_closed() {
        for status in [
            BlkStatus::Ok,
            BlkStatus::Degraded,
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
            BlkStatus::MediumError,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ] {
            assert_eq!(BlkStatus::from_u32(status.as_u32()), Ok(status));
        }
        assert_eq!(BlkStatus::from_u32(9), Err(Errno::OutOfRange));
        assert_eq!(BlkStatus::from_u32(u32::MAX), Err(Errno::OutOfRange));
        assert!(BlkStatus::Timeout.is_retryable());
        assert!(BlkStatus::Reset.is_retryable());
        assert!(BlkStatus::TransientError.is_retryable());
        assert!(!BlkStatus::MediumError.is_retryable());
        assert!(!BlkStatus::Offline.is_retryable());
    }

    /// The statuses in ascending fail-closed severity — the one order
    /// `combine` folds by. Kept here so the precedence is asserted, not
    /// assumed from the enum declaration.
    const SEVERITY_ORDER: [BlkStatus; 9] = [
        BlkStatus::Ok,
        BlkStatus::Degraded,
        BlkStatus::TransientError,
        BlkStatus::Timeout,
        BlkStatus::Reset,
        BlkStatus::MediumError,
        BlkStatus::Offline,
        BlkStatus::Removed,
        BlkStatus::Fatal,
    ];

    #[test]
    fn blk_status_severity_is_a_strict_total_order() {
        // Severity is strictly increasing across the whole vocabulary, so
        // `combine` has a well-defined winner for every pair — healthy least,
        // an unclassified fatal most.
        for pair in SEVERITY_ORDER.windows(2) {
            let [lower, higher] = [pair[0], pair[1]];
            assert!(
                lower.severity() < higher.severity(),
                "{lower:?} must rank below {higher:?}"
            );
        }
        // The two data-valid statuses rank below every error status, so a
        // completion whose data may be consumed can never be "worse" than one
        // that must be rejected.
        assert!(BlkStatus::Ok.severity() < BlkStatus::TransientError.severity());
        assert!(BlkStatus::Degraded.severity() < BlkStatus::TransientError.severity());
        // Every reissuable status ranks below every permanent/gone one, so a
        // fold never downgrades a definitive verdict to "retry".
        for retryable in [
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
        ] {
            for permanent in [
                BlkStatus::MediumError,
                BlkStatus::Offline,
                BlkStatus::Removed,
                BlkStatus::Fatal,
            ] {
                assert!(retryable.severity() < permanent.severity());
            }
        }
    }

    #[test]
    fn blk_status_combine_is_the_more_fail_closed_and_forms_a_lattice() {
        // `combine` picks the higher-severity status, and does so as a proper
        // fold: commutative, associative, and idempotent, so a chain of
        // statuses can be folded in any order with the same result.
        for &a in &SEVERITY_ORDER {
            assert_eq!(a.combine(a), a, "idempotent");
            for &b in &SEVERITY_ORDER {
                let winner = if a.severity() >= b.severity() { a } else { b };
                assert_eq!(a.combine(b), winner);
                assert_eq!(a.combine(b), b.combine(a), "commutative");
                for &c in &SEVERITY_ORDER {
                    assert_eq!(
                        a.combine(b).combine(c),
                        a.combine(b.combine(c)),
                        "associative"
                    );
                }
            }
        }
        // The two precedence decisions the fold exists to make: a hub reset
        // overrides a child's Ok (the aborted transfer is answered reissuably,
        // its data not consumed)...
        assert_eq!(BlkStatus::Ok.combine(BlkStatus::Reset), BlkStatus::Reset);
        assert!(!BlkStatus::Ok.combine(BlkStatus::Reset).data_valid());
        // ...but a device's own definitive medium error still wins over a
        // concurrent reset (a bad sector is real; do not retry into it).
        assert_eq!(
            BlkStatus::Reset.combine(BlkStatus::MediumError),
            BlkStatus::MediumError
        );
        assert!(!BlkStatus::Reset
            .combine(BlkStatus::MediumError)
            .is_retryable());
    }

    #[test]
    fn truncated_or_corrupt_completion_fails_closed_to_fatal() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        BlkCompletion::default().encode(&mut buf).expect("encodes");
        // A truncated *valid* frame cannot be trusted as valid.
        assert_eq!(
            decode_outcome(&buf[..BLK_COMPLETION_LEN - 1]).status,
            BlkStatus::Fatal
        );
        // A frame too short to even classify fails closed, never a spurious Ok.
        let outcome = decode_outcome(&buf[..2]);
        assert_eq!(outcome.status, BlkStatus::Fatal);
        assert_eq!(outcome.error, Errno::DeviceFault);
        assert_eq!(decode_completion(&buf[..2]), Err(Errno::DeviceFault));
    }

    #[test]
    fn unknown_status_word_fails_closed_to_fatal() {
        let mut buf = [0u8; BLK_COMPLETION_LEN];
        BlkCompletion::default().encode(&mut buf).expect("encodes");
        put_u32(&mut buf, 0, 9999);
        let outcome = decode_outcome(&buf);
        assert_eq!(outcome.status, BlkStatus::Fatal);
        assert_eq!(outcome.error, Errno::DeviceFault);
    }

    #[test]
    fn device_class_budgets_differ_and_scale_by_class() {
        for class in [
            BlkDeviceClass::Rotational,
            BlkDeviceClass::SolidState,
            BlkDeviceClass::Removable,
            BlkDeviceClass::Virtual,
        ] {
            assert_eq!(BlkDeviceClass::from_u32(class.as_u32()), Ok(class));
            let b = class.budget();
            assert!(b.deadline_ns > 0 && b.queue_depth > 0);
            // The grace window rides out at least one full deadline's worth of
            // stall/reset, so a single reset cannot exhaust it.
            assert!(
                b.grace_ns > b.deadline_ns,
                "{class:?} grace must exceed its per-request deadline"
            );
        }
        assert_eq!(BlkDeviceClass::from_u32(4), Err(Errno::OutOfRange));
        // A rotational disk is given a longer spin-up/reset budget than an SSD,
        // and the SSD a deeper queue — no single global constant fits both.
        assert!(
            BlkDeviceClass::Rotational.budget().deadline_ns
                > BlkDeviceClass::SolidState.budget().deadline_ns
        );
        assert!(
            BlkDeviceClass::SolidState.budget().queue_depth
                > BlkDeviceClass::Rotational.budget().queue_depth
        );
    }

    #[test]
    fn the_bounded_reissue_policy_retries_only_reissuable_statuses_within_budget() {
        let budget = BlkDeviceClass::Rotational.budget();
        assert!(budget.max_retries > 0);
        // A reissuable status is reissued only up to the budget, then fails
        // closed — never forever.
        for status in [
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
        ] {
            for attempts in 0..budget.max_retries {
                assert!(budget.should_reissue(status, attempts), "{status:?}");
            }
            assert!(
                !budget.should_reissue(status, budget.max_retries),
                "{status:?} must stop at the budget"
            );
        }
        // A valid completion or a definitive/gone verdict is never reissued,
        // even on the very first attempt.
        for status in [
            BlkStatus::Ok,
            BlkStatus::Degraded,
            BlkStatus::MediumError,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ] {
            assert!(!budget.should_reissue(status, 0), "{status:?}");
        }
    }

    #[test]
    fn window_holds_whole_blocks_of_every_supported_size() {
        for block_size in [512usize, 4096] {
            assert_eq!(BLK_DATA_LEN % block_size, 0);
        }
    }

    #[test]
    fn health_counters_fold_every_status_into_exactly_one_bucket() {
        let mut counters = BlkHealthCounters::default();
        // Fold one of every device-level status, plus a couple of repeats so
        // the buckets are distinguishable.
        let statuses = [
            BlkStatus::Ok,
            BlkStatus::Ok,
            BlkStatus::Degraded,
            BlkStatus::TransientError,
            BlkStatus::Timeout,
            BlkStatus::Reset,
            BlkStatus::Reset,
            BlkStatus::MediumError,
            BlkStatus::Offline,
            BlkStatus::Removed,
            BlkStatus::Fatal,
        ];
        for status in statuses {
            counters.fold(status);
        }
        assert_eq!(counters.completions, statuses.len() as u64);
        assert_eq!(counters.ok, 2);
        assert_eq!(counters.degraded, 1);
        assert_eq!(counters.transient, 1);
        assert_eq!(counters.timeouts, 1);
        assert_eq!(counters.resets, 2);
        assert_eq!(counters.medium_errors, 1);
        // Offline and Removed share one bucket.
        assert_eq!(counters.offline, 2);
        assert_eq!(counters.faults, 1);
        // The buckets partition every completion exactly once.
        let bucket_sum = counters.ok
            + counters.degraded
            + counters.transient
            + counters.timeouts
            + counters.resets
            + counters.medium_errors
            + counters.offline
            + counters.faults;
        assert_eq!(bucket_sum, counters.completions);
        // Reissues are orthogonal to folded completions.
        assert_eq!(counters.reissues, 0);
        counters.note_reissue();
        counters.note_reissue();
        assert_eq!(counters.reissues, 2);
        assert_eq!(counters.completions, statuses.len() as u64);
    }

    #[test]
    fn health_counters_round_trip_on_the_wire() {
        let counters = BlkHealthCounters {
            completions: 100,
            reissues: 7,
            ok: 80,
            degraded: 3,
            transient: 4,
            timeouts: 2,
            resets: 5,
            medium_errors: 1,
            offline: 3,
            faults: 2,
        };
        let bytes = counters.to_le_bytes();
        assert_eq!(bytes.len(), BLK_HEALTH_COUNTERS_LEN);
        assert_eq!(BlkHealthCounters::from_bytes(&bytes), Ok(counters));
        // A trailing surplus is ignored (the record embeds the snapshot as a
        // fixed prefix); a short slice fails closed.
        let mut padded = bytes.to_vec();
        padded.push(0xAB);
        assert_eq!(BlkHealthCounters::from_bytes(&padded), Ok(counters));
        assert_eq!(
            BlkHealthCounters::from_bytes(&bytes[..BLK_HEALTH_COUNTERS_LEN - 1]),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn health_counters_saturate_rather_than_wrap() {
        // A tally is operational observability, never a wrapping ring: a
        // counter pinned at the ceiling stays pinned rather than rolling to a
        // misleadingly-small value.
        let mut counters = BlkHealthCounters {
            completions: u64::MAX,
            ok: u64::MAX,
            reissues: u64::MAX,
            ..BlkHealthCounters::default()
        };
        counters.fold(BlkStatus::Ok);
        counters.note_reissue();
        assert_eq!(counters.completions, u64::MAX);
        assert_eq!(counters.ok, u64::MAX);
        assert_eq!(counters.reissues, u64::MAX);
    }

    #[test]
    fn only_device_level_errors_classify_as_health_signals() {
        // Device-health errors fold into the state machine...
        for (err, status) in [
            (DriverError::MediumError, BlkStatus::MediumError),
            (DriverError::DeviceOffline, BlkStatus::Offline),
            (DriverError::Busy, BlkStatus::TransientError),
            (DriverError::EndpointStalled, BlkStatus::Reset),
            (DriverError::DeviceFault, BlkStatus::Fatal),
        ] {
            assert_eq!(BlkStatus::for_driver_health(err), Some(status), "{err:?}");
        }
        // ...while request-level rejections are health-neutral (a hostile or
        // malformed request must never be able to fault a healthy device).
        for err in [
            DriverError::OutOfRange,
            DriverError::LengthOutOfRange,
            DriverError::PermissionDenied,
            DriverError::Unsupported,
            DriverError::NotImplemented,
            DriverError::BadMagic,
            DriverError::NotFound,
            DriverError::NoSpace,
        ] {
            assert_eq!(BlkStatus::for_driver_health(err), None, "{err:?}");
        }
    }

    #[test]
    fn a_healthy_device_reports_its_answers_verbatim() {
        let mut health = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(health.state(), BlkHealthState::Healthy);
        assert_eq!(health.observe(BlkStatus::Ok, 0), BlkStatus::Ok);
        assert_eq!(health.state(), BlkHealthState::Healthy);
        // A self-reported unhealthy device stays usable and its data valid.
        assert_eq!(health.observe(BlkStatus::Degraded, 1), BlkStatus::Degraded);
        assert_eq!(health.state(), BlkHealthState::Degraded);
        assert!(health.state().is_operational());
        // A live device recovers straight back to healthy on its next good read.
        assert_eq!(health.observe(BlkStatus::Ok, 2), BlkStatus::Ok);
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_medium_error_is_a_live_answer_not_a_device_fault() {
        let mut health = BlkHealth::new(BlkDeviceClass::Rotational);
        // A bad sector surfaces to the request but never faults the device.
        assert_eq!(
            health.observe(BlkStatus::MediumError, 0),
            BlkStatus::MediumError
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_blip_that_returns_inside_the_grace_window_is_ridden_out() {
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        let grace = health.budget().grace_ns;
        // First stall opens the window; the request is answered reissuably,
        // never hard-failed.
        assert_eq!(health.observe(BlkStatus::Reset, 1_000), BlkStatus::Reset);
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // A reissue still inside the window is still reissuable.
        assert_eq!(
            health.observe(BlkStatus::TransientError, 1_000 + grace / 2),
            BlkStatus::Reset
        );
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // The device comes back before the window elapses: fully recovered,
        // no reboot, the episode invisible to the workload's data.
        assert_eq!(
            health.observe(BlkStatus::Ok, 1_000 + grace - 1),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_blip_that_outlasts_the_grace_window_fails_closed() {
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let grace = health.budget().grace_ns;
        assert_eq!(health.observe(BlkStatus::Timeout, 100), BlkStatus::Reset);
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // Exactly at the window boundary the device is failed closed: the
        // consumer is told the device is offline, not reissuable.
        assert_eq!(
            health.observe(BlkStatus::Timeout, 100 + grace),
            BlkStatus::Offline
        );
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // Faulted is sticky: further non-answers keep failing closed...
        assert_eq!(
            health.observe(BlkStatus::Reset, 100 + grace + 1),
            BlkStatus::Offline
        );
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // ...but a genuine return still recovers it without a reboot.
        assert_eq!(
            health.observe(BlkStatus::Ok, 100 + grace + 2),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_gone_device_is_sticky_until_it_demonstrably_returns() {
        for gone in [BlkStatus::Offline, BlkStatus::Removed] {
            let mut health = BlkHealth::new(BlkDeviceClass::Removable);
            assert_eq!(health.observe(gone, 0), gone);
            let expected = if gone == BlkStatus::Removed {
                BlkHealthState::Removed
            } else {
                BlkHealthState::Offline
            };
            assert_eq!(health.state(), expected);
            assert!(!health.state().is_operational());
            // A gone device does not enter the grace window: a further stall
            // keeps it gone (there is nothing present to ride out).
            assert!(!health.observe(BlkStatus::Reset, 10).data_valid());
            assert_eq!(health.state(), expected);
            // Only a real answer (a verified re-insert) clears it.
            assert_eq!(health.observe(BlkStatus::Ok, 20), BlkStatus::Ok);
            assert_eq!(health.state(), BlkHealthState::Healthy);
        }
    }

    #[test]
    fn a_fatal_outcome_is_sticky_but_still_recoverable() {
        let mut health = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(health.observe(BlkStatus::Fatal, 0), BlkStatus::Fatal);
        assert_eq!(health.state(), BlkHealthState::Failed);
        // A transient after a fatal keeps failing closed, never re-opening a
        // grace window on a device declared dead.
        assert_eq!(health.observe(BlkStatus::Reset, 1), BlkStatus::Fatal);
        assert_eq!(health.state(), BlkHealthState::Failed);
        // A demonstrated good answer (e.g. after a driver restart) recovers it.
        assert_eq!(health.observe(BlkStatus::Ok, 2), BlkStatus::Ok);
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_flapping_device_re_opens_a_fresh_grace_window_each_episode() {
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        let grace = health.budget().grace_ns;
        // Episode one: stall, recover.
        assert_eq!(health.observe(BlkStatus::Reset, 0), BlkStatus::Reset);
        assert_eq!(health.observe(BlkStatus::Ok, 10), BlkStatus::Ok);
        // Episode two starts a *new* window measured from its own start, so
        // the earlier episode's elapsed time cannot prematurely fault it.
        let base = 1_000_000;
        assert_eq!(health.observe(BlkStatus::Reset, base), BlkStatus::Reset);
        assert_eq!(health.state(), BlkHealthState::Recovering);
        assert_eq!(
            health.observe(BlkStatus::Reset, base + grace - 1),
            BlkStatus::Reset,
            "the new window is measured from this episode's own start"
        );
        assert_eq!(
            health.observe(BlkStatus::Reset, base + grace),
            BlkStatus::Offline
        );
    }

    #[test]
    fn an_idle_recovering_device_expires_its_window_on_a_time_poll() {
        // A device that stalls once and then goes *quiet* (no further request
        // to fold through `observe`) must still fail closed when its grace
        // window elapses, driven by the one-shot timer `grace_deadline_ns`
        // names rather than a busy-poll.
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let grace = health.budget().grace_ns;
        assert_eq!(health.observe(BlkStatus::Timeout, 100), BlkStatus::Reset);
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // The driver arms its one-shot for exactly this deadline.
        assert_eq!(health.grace_deadline_ns(), Some(100 + grace));

        // A poll before the deadline leaves the window open (the timer would
        // not have fired yet); it is idempotent.
        assert_eq!(health.poll(100 + grace - 1), BlkHealthState::Recovering);
        assert_eq!(health.poll(100 + grace - 1), BlkHealthState::Recovering);
        assert_eq!(health.state(), BlkHealthState::Recovering);

        // At the deadline the poll fails the device closed, with no request
        // ever observed in the interim.
        assert_eq!(health.poll(100 + grace), BlkHealthState::Faulted);
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // Faulted has no open window, so no further timer is armed...
        assert_eq!(health.grace_deadline_ns(), None);
        // ...and a repeat poll is a sticky no-op (never re-faults or resets).
        assert_eq!(health.poll(u64::MAX), BlkHealthState::Faulted);
    }

    #[test]
    fn a_time_poll_is_a_no_op_off_the_open_recovery_path() {
        // A healthy device is never faulted by the passage of time alone.
        let mut healthy = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(healthy.grace_deadline_ns(), None);
        assert_eq!(healthy.poll(u64::MAX), BlkHealthState::Healthy);
        assert_eq!(healthy.state(), BlkHealthState::Healthy);

        // A device reported degraded (usable) is likewise time-poll-inert.
        let mut degraded = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(
            degraded.observe(BlkStatus::Degraded, 0),
            BlkStatus::Degraded
        );
        assert_eq!(degraded.grace_deadline_ns(), None);
        assert_eq!(degraded.poll(u64::MAX), BlkHealthState::Degraded);

        // A device already known gone stays gone: a time poll never rewrites a
        // sticky terminal state into `Faulted`.
        let mut gone = BlkHealth::new(BlkDeviceClass::Removable);
        assert_eq!(gone.observe(BlkStatus::Removed, 0), BlkStatus::Removed);
        assert_eq!(gone.grace_deadline_ns(), None);
        assert_eq!(gone.poll(u64::MAX), BlkHealthState::Removed);
    }

    #[test]
    fn a_time_poll_expiry_matches_an_observed_expiry() {
        // The time-driven and request-driven paths agree: a device failed
        // closed by `poll` behaves exactly as one failed closed by a stall
        // that outlasted the window — sticky-offline, yet still recoverable.
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        let grace = health.budget().grace_ns;
        assert_eq!(health.observe(BlkStatus::Reset, 0), BlkStatus::Reset);
        assert_eq!(health.poll(grace), BlkHealthState::Faulted);
        // A subsequent stall is failed closed (sticky), as on the observed
        // path...
        assert_eq!(
            health.observe(BlkStatus::Reset, grace + 1),
            BlkStatus::Offline
        );
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // ...and a genuine return recovers it without a reboot.
        assert_eq!(health.observe(BlkStatus::Ok, grace + 2), BlkStatus::Ok);
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn a_serve_loop_with_no_recovering_device_arms_no_idle_timer() {
        // Every LUN operational (or none at all): the loop parks with no
        // timeout rather than waking spuriously.
        assert_eq!(recovery_wait_timeout(core::iter::empty(), 100), None);

        let healthy = BlkHealth::new(BlkDeviceClass::Rotational);
        let degraded = {
            let mut h = BlkHealth::new(BlkDeviceClass::SolidState);
            assert_eq!(h.observe(BlkStatus::Degraded, 10), BlkStatus::Degraded);
            h
        };
        assert_eq!(
            recovery_wait_timeout([&healthy, &degraded], 100),
            None,
            "an operational set arms no window"
        );
    }

    #[test]
    fn a_serve_loop_arms_the_soonest_recovering_windows_deadline() {
        // Two LUNs recovering with different-length windows: the loop waits
        // for the *soonest* to elapse, relative to now.
        let mut early = BlkHealth::new(BlkDeviceClass::Removable);
        let mut late = BlkHealth::new(BlkDeviceClass::Rotational);
        assert_eq!(early.observe(BlkStatus::Reset, 100), BlkStatus::Reset);
        assert_eq!(late.observe(BlkStatus::Reset, 100), BlkStatus::Reset);
        let early_deadline = early.grace_deadline_ns().expect("early is recovering");
        let late_deadline = late.grace_deadline_ns().expect("late is recovering");
        assert!(
            early_deadline < late_deadline,
            "a removable unit's grace window is shorter than a rotational disk's"
        );
        // A healthy sibling contributes no deadline and is ignored.
        let healthy = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(
            recovery_wait_timeout([&early, &late, &healthy], 150),
            Some(early_deadline - 150),
            "the relative wait targets the soonest window, measured from now"
        );
    }

    #[test]
    fn a_serve_loop_polls_immediately_for_an_already_elapsed_window() {
        // A window whose deadline has already passed at `now` yields a zero
        // wait, so the loop polls at once and never over-waits (and never a
        // negative/underflowed timeout).
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        assert_eq!(health.observe(BlkStatus::Reset, 0), BlkStatus::Reset);
        let deadline = health.grace_deadline_ns().expect("recovering");
        assert_eq!(recovery_wait_timeout([&health], deadline), Some(0));
        assert_eq!(recovery_wait_timeout([&health], deadline + 1_000), Some(0));
    }

    // ---- The driver-side recovery escalation ladder (`RecoveryLadder`) ----
    //
    // The ladder decides what the *driver* does to the hardware between
    // reissued attempts while a device is `Recovering`. These prove the
    // escalation is bounded, escalates least-disruptive-first, and derives its
    // cap from the same per-class budget the consumer reissues against.

    #[test]
    fn an_operational_device_needs_no_recovery_action() {
        // A healthy or degraded (working-but-unwell) device is not recovering:
        // the ladder yields `None` and stays at the bottom rung.
        let mut ladder = RecoveryLadder::new(BlkDeviceClass::Removable);
        assert_eq!(
            ladder.next_action(BlkHealthState::Healthy),
            RecoveryAction::None
        );
        assert_eq!(ladder.attempts(), 0);
        assert_eq!(
            ladder.next_action(BlkHealthState::Degraded),
            RecoveryAction::None
        );
        assert_eq!(ladder.attempts(), 0);
    }

    #[test]
    fn a_recovering_device_escalates_retry_then_reset_then_gives_up_at_budget() {
        // The first recovering attempt is a plain reissue (a one-off glitch may
        // clear itself); each subsequent attempt escalates to a data-path
        // reset; once the class's retry budget is spent the ladder gives up and
        // leaves the grace window to fail the device closed.
        let budget = BlkDeviceClass::Rotational.budget();
        assert_eq!(budget.max_retries, 3);
        let mut ladder = RecoveryLadder::new(BlkDeviceClass::Rotational);
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Retry
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Reset
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Reset
        );
        // The budget (3) is now spent: no more escalation, deterministically.
        assert_eq!(ladder.attempts(), 3);
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::GiveUp
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::GiveUp,
            "give-up is sticky while recovering: never an unbounded reset loop"
        );
        assert_eq!(ladder.attempts(), 3);
    }

    #[test]
    fn the_ladders_cap_scales_with_the_device_class_budget() {
        // A shallower-budget class (an SSD: max_retries 2) exhausts its ladder
        // in fewer rungs than a deeper one, straight from the shared per-class
        // budget — never a hand-picked constant in the ladder.
        assert_eq!(BlkDeviceClass::SolidState.budget().max_retries, 2);
        let mut ladder = RecoveryLadder::new(BlkDeviceClass::SolidState);
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Retry
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Reset
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::GiveUp
        );
    }

    #[test]
    fn a_device_that_recovers_mid_ladder_re_arms_from_the_bottom() {
        // A blip that clears returns the device to `Healthy`; the ladder resets
        // so the next, unrelated episode starts from a gentle retry rather than
        // an immediate reset (no scar from the prior episode).
        let mut ladder = RecoveryLadder::new(BlkDeviceClass::Removable);
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Retry
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Reset
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Healthy),
            RecoveryAction::None
        );
        assert_eq!(ladder.attempts(), 0);
        // A fresh episode starts from the gentle rung again.
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Retry
        );
    }

    #[test]
    fn a_device_already_failed_closed_stops_escalating() {
        // Once the device has been failed closed (grace window elapsed, gone,
        // or unclassified-fatal), the recovering episode is over: the driver
        // stops resetting and waits for a demonstrated return, which re-arms the
        // ladder from the bottom.
        let mut ladder = RecoveryLadder::new(BlkDeviceClass::Virtual);
        for state in [
            BlkHealthState::Faulted,
            BlkHealthState::Offline,
            BlkHealthState::Removed,
            BlkHealthState::Failed,
        ] {
            assert_eq!(ladder.next_action(state), RecoveryAction::GiveUp);
            assert_eq!(ladder.attempts(), 0, "a failed-closed device takes no rung");
        }
        assert_eq!(
            ladder.next_action(BlkHealthState::Healthy),
            RecoveryAction::None
        );
        assert_eq!(
            ladder.next_action(BlkHealthState::Recovering),
            RecoveryAction::Retry,
            "a demonstrated return re-arms the ladder"
        );
    }

    // ---- The interior fault-domain machine (`FaultDomain`) ----
    //
    // A hub/controller blip is *one* fault-domain event across its whole
    // subtree, not N spurious disk failures. These prove that coherent
    // quiesce/resume over the shared grace window, host-side.

    /// A representative interior-node grace budget (an owning bus/hub's reset
    /// envelope). A literal here stands in for the policy the caller derives
    /// from the owner's discovered class at the wiring site.
    const DOMAIN_GRACE_NS: u64 = 1_000;
    /// A representative owner node id from the hardware tree.
    const OWNER_ID: u32 = 7;

    #[test]
    fn a_healthy_domain_imposes_nothing_on_its_children() {
        // A live owner does not override its children: each child answers on
        // its own per-device health, so the domain returns `None`.
        let domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        assert_eq!(domain.state(), FaultDomainState::Healthy);
        assert_eq!(domain.owner(), OWNER_ID);
        assert_eq!(domain.child_status(0), None);
        assert_eq!(domain.grace_deadline_ns(), None);
    }

    #[test]
    fn an_owner_reset_holds_the_whole_subtree_reissuable_under_one_window() {
        // One owner reset opens one shared window; every child in the subtree
        // is told the same reissuable `Reset` while it is open.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        assert_eq!(domain.quiesce(0), FaultDomainState::Recovering);
        assert_eq!(domain.grace_deadline_ns(), Some(DOMAIN_GRACE_NS));
        // Any child, at any point inside the window, reissues rather than fails.
        assert_eq!(domain.child_status(0), Some(BlkStatus::Reset));
        assert_eq!(
            domain.child_status(DOMAIN_GRACE_NS - 1),
            Some(BlkStatus::Reset)
        );
    }

    #[test]
    fn an_owner_that_returns_inside_the_window_recovers_the_whole_subtree() {
        // The owner comes back before its window closes: the whole subtree
        // resumes to `Healthy` and children go back to their own health,
        // leaving no scar (the ride-out-the-blip behaviour).
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(10);
        assert_eq!(domain.resume(), FaultDomainState::Healthy);
        assert_eq!(domain.child_status(20), None);
        assert_eq!(domain.grace_deadline_ns(), None);
    }

    #[test]
    fn an_owner_that_outlasts_the_window_fails_the_subtree_closed() {
        // The window elapses with no return: the subtree fails closed to
        // `Offline` and every child is told `Offline`.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(0);
        // A child observing at/after the deadline is failed closed even before
        // a `poll` mutates the machine (the pure query agrees with the state).
        assert_eq!(
            domain.child_status(DOMAIN_GRACE_NS),
            Some(BlkStatus::Offline)
        );
        assert_eq!(domain.poll(DOMAIN_GRACE_NS), FaultDomainState::Offline);
        assert_eq!(
            domain.child_status(DOMAIN_GRACE_NS),
            Some(BlkStatus::Offline)
        );
        // Once failed closed the window is shut: no one-shot timer is re-armed.
        assert_eq!(domain.grace_deadline_ns(), None);
    }

    #[test]
    fn a_quiet_domain_expires_its_window_on_a_time_poll() {
        // An owner that resets and then goes silent (no child request to fold)
        // still fails closed on the one-shot grace timer, never sitting
        // `Recovering` forever — and a poll before the deadline is a no-op.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(100);
        assert_eq!(domain.grace_deadline_ns(), Some(100 + DOMAIN_GRACE_NS));
        assert_eq!(
            domain.poll(100 + DOMAIN_GRACE_NS - 1),
            FaultDomainState::Recovering
        );
        assert_eq!(
            domain.poll(100 + DOMAIN_GRACE_NS),
            FaultDomainState::Offline
        );
    }

    #[test]
    fn an_offline_subtree_is_sticky_until_a_demonstrated_owner_return() {
        // A failed-closed subtree cannot masquerade as healthy: a further reset
        // leaves it `Offline`, and only a demonstrated owner recovery clears it
        // — without a reboot.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(0);
        domain.poll(DOMAIN_GRACE_NS);
        assert_eq!(domain.state(), FaultDomainState::Offline);
        // A fresh reset while already offline changes nothing.
        assert_eq!(
            domain.quiesce(DOMAIN_GRACE_NS + 5),
            FaultDomainState::Offline
        );
        assert_eq!(domain.grace_deadline_ns(), None);
        // A demonstrated return recovers the whole subtree.
        assert_eq!(domain.resume(), FaultDomainState::Healthy);
        assert_eq!(domain.child_status(DOMAIN_GRACE_NS + 10), None);
    }

    #[test]
    fn a_continuing_reset_cannot_postpone_the_fail_closed() {
        // A blip that keeps re-asserting must not extend its own window: the
        // window is measured from the *first* quiesce, so a flapping owner
        // still fails closed on schedule.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(0);
        // Re-quiescing partway through keeps the original deadline.
        domain.quiesce(DOMAIN_GRACE_NS / 2);
        assert_eq!(domain.grace_deadline_ns(), Some(DOMAIN_GRACE_NS));
        assert_eq!(domain.poll(DOMAIN_GRACE_NS), FaultDomainState::Offline);
    }

    #[test]
    fn a_flapping_owner_reopens_a_fresh_window_each_episode() {
        // A distinct new episode (after a genuine recovery) opens a fresh
        // window from its own start, so an owner that recovers and blips again
        // gets a full grace window for the new episode.
        let mut domain = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        domain.quiesce(0);
        domain.resume();
        domain.quiesce(5_000);
        assert_eq!(domain.grace_deadline_ns(), Some(5_000 + DOMAIN_GRACE_NS));
        // The new episode rides out on its own clock.
        assert_eq!(
            domain.poll(5_000 + DOMAIN_GRACE_NS - 1),
            FaultDomainState::Recovering
        );
    }

    #[test]
    fn a_ride_out_forever_domain_never_fails_closed() {
        // A `u64::MAX` grace (ride out indefinitely) arms no one-shot timer and
        // never elapses, so children stay reissuable until a demonstrated
        // return — the honest "no deadline" convention.
        let mut domain = FaultDomain::new(OWNER_ID, u64::MAX);
        domain.quiesce(0);
        assert_eq!(domain.grace_deadline_ns(), None);
        assert_eq!(domain.poll(u64::MAX), FaultDomainState::Recovering);
        assert_eq!(domain.child_status(u64::MAX), Some(BlkStatus::Reset));
    }

    // ---- Fault-domain composition (`fault_domain_wait_timeout`,
    // `effective_child_status`) ----
    //
    // A serve loop that owns fault domains must time their shared windows and
    // fold what each imposes onto its children exactly as it does per-device.
    // These prove the interior-node timing mirrors `recovery_wait_timeout` and
    // that a device's own health composes coherently with its ancestor chain.

    #[test]
    fn a_serve_loop_with_no_recovering_domain_arms_no_idle_timer() {
        // No domain, and an all-`Healthy` set, both arm no window: the loop
        // parks with no timeout rather than waking spuriously — the same
        // convention as the per-device timer.
        assert_eq!(fault_domain_wait_timeout(core::iter::empty(), 100), None);
        let healthy = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        assert_eq!(fault_domain_wait_timeout([&healthy], 100), None);
    }

    #[test]
    fn a_serve_loop_arms_the_soonest_recovering_domain_deadline() {
        // Two owners recovering under different windows: the loop waits for the
        // soonest to elapse, relative to now; a healthy sibling is ignored.
        let mut early = FaultDomain::new(1, DOMAIN_GRACE_NS);
        let mut late = FaultDomain::new(2, DOMAIN_GRACE_NS * 4);
        early.quiesce(50);
        late.quiesce(50);
        let healthy = FaultDomain::new(3, DOMAIN_GRACE_NS);
        assert_eq!(
            fault_domain_wait_timeout([&early, &late, &healthy], 100),
            Some((50 + DOMAIN_GRACE_NS) - 100),
            "targets the soonest subtree window, measured from now"
        );
        // A window already past at `now` yields a zero wait (poll at once,
        // never an underflow), matching `recovery_wait_timeout`.
        assert_eq!(
            fault_domain_wait_timeout([&early], 50 + DOMAIN_GRACE_NS),
            Some(0)
        );
        assert_eq!(
            fault_domain_wait_timeout([&early], 50 + DOMAIN_GRACE_NS + 5_000),
            Some(0)
        );
    }

    #[test]
    fn a_child_with_no_fault_domain_answers_on_its_own_health() {
        // With no interior domain imposing anything, the effective status is
        // exactly the device's own outcome — the fold is the identity.
        for status in SEVERITY_ORDER {
            assert_eq!(
                effective_child_status(status, core::iter::empty(), 0),
                status
            );
        }
        // A healthy ancestor imposes nothing, so it too leaves the child's own
        // status untouched.
        let healthy = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        assert_eq!(
            effective_child_status(BlkStatus::Ok, [&healthy], 0),
            BlkStatus::Ok
        );
    }

    #[test]
    fn a_recovering_ancestor_makes_a_healthy_child_reissuable() {
        // A hub mid-reset turns a child's `Ok` into a reissuable `Reset`, so
        // the aborted transfer is retried rather than its stale data consumed
        // — the ride-out-the-blip behaviour, seen from the child.
        let mut hub = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        hub.quiesce(0);
        let effective = effective_child_status(BlkStatus::Ok, [&hub], 10);
        assert_eq!(effective, BlkStatus::Reset);
        assert!(!effective.data_valid());
        assert!(effective.is_retryable());
    }

    #[test]
    fn a_devices_own_medium_error_wins_over_a_recovering_ancestor() {
        // A definitive bad-sector verdict from the device itself is not masked
        // by a concurrent hub reset: the child fails closed to the medium
        // error rather than being told to retry into the same failure.
        let mut hub = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        hub.quiesce(0);
        let effective = effective_child_status(BlkStatus::MediumError, [&hub], 10);
        assert_eq!(effective, BlkStatus::MediumError);
        assert!(!effective.is_retryable());
    }

    #[test]
    fn a_failed_ancestor_fails_the_child_closed_regardless_of_its_own_health() {
        // An ancestor whose grace window has elapsed (or is already offline)
        // fails the whole subtree closed: even a child reporting `Ok` is told
        // `Offline`.
        let mut hub = FaultDomain::new(OWNER_ID, DOMAIN_GRACE_NS);
        hub.quiesce(0);
        // Observed at/after the window: the domain imposes `Offline`.
        let effective = effective_child_status(BlkStatus::Ok, [&hub], DOMAIN_GRACE_NS);
        assert_eq!(effective, BlkStatus::Offline);
        assert!(!effective.data_valid());
    }

    #[test]
    fn a_deep_failing_domain_is_never_masked_by_a_shallow_healthy_one() {
        // Nested domains (a healthy hub under a recovering root complex): the
        // most fail-closed status in the whole chain wins, so a healthy nearer
        // owner cannot hide a failing further one. Order-independent, since the
        // fold is associative/commutative.
        let nearest_healthy = FaultDomain::new(1, DOMAIN_GRACE_NS);
        let mut root_recovering = FaultDomain::new(2, DOMAIN_GRACE_NS);
        root_recovering.quiesce(0);
        assert_eq!(
            effective_child_status(BlkStatus::Ok, [&nearest_healthy, &root_recovering], 10),
            BlkStatus::Reset
        );
        assert_eq!(
            effective_child_status(BlkStatus::Ok, [&root_recovering, &nearest_healthy], 10),
            BlkStatus::Reset
        );
        // If both a nearer hub is recovering and a further root has failed
        // closed, the child is `Offline` (the strictest ancestor wins).
        let mut hub_recovering = FaultDomain::new(1, DOMAIN_GRACE_NS);
        hub_recovering.quiesce(0);
        let mut root_offline = FaultDomain::new(2, DOMAIN_GRACE_NS);
        root_offline.quiesce(0);
        root_offline.poll(DOMAIN_GRACE_NS);
        assert_eq!(root_offline.state(), FaultDomainState::Offline);
        assert_eq!(
            effective_child_status(BlkStatus::Ok, [&hub_recovering, &root_offline], 10),
            BlkStatus::Offline
        );
    }

    // ---- The shared block-service request engine (`serve_request_recovering`)
    // ----
    //
    // These prove the one serve engine every block driver reuses, over
    // in-memory [`Block`] doubles. The 32 KiB data window is heap-backed in the
    // test (like the real driver's mapped window) rather than a large local
    // array; the engine itself is alloc-free and runs unchanged inside a
    // freestanding driver.
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::driver::block::BlockGeometry;

    const BLOCK_SIZE: usize = 512;
    const BLOCK_COUNT: u64 = 64;
    /// `BLOCK_SIZE` as the wire-width type the geometry carries.
    const BLOCK_SIZE_U32: u32 = 512;

    /// An in-memory 512-byte-block device with a flush counter.
    struct MemBlock {
        data: Vec<u8>,
        flushes: usize,
    }

    impl MemBlock {
        fn new() -> Self {
            Self {
                data: vec![0u8; BLOCK_SIZE * usize::try_from(BLOCK_COUNT).expect("64 fits usize")],
                flushes: 0,
            }
        }
    }

    impl Block for MemBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let start =
                usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::LengthOutOfRange)?;
            if !buf.len().is_multiple_of(BLOCK_SIZE) || end > self.data.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            buf.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let start =
                usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
            let end = start
                .checked_add(buf.len())
                .ok_or(DriverError::LengthOutOfRange)?;
            if !buf.len().is_multiple_of(BLOCK_SIZE) || end > self.data.len() {
                return Err(DriverError::LengthOutOfRange);
            }
            self.data[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            self.flushes += 1;
            Ok(())
        }
    }

    fn encode_request(request: &BlkRequest) -> [u8; BLK_REQUEST_LEN] {
        let mut bytes = [0u8; BLK_REQUEST_LEN];
        request.encode(&mut bytes).expect("encodes");
        bytes
    }

    /// Serve one request against a fresh-`Healthy` device at time zero: the
    /// success and request-refusal paths these tests exercise are independent
    /// of the recovery state, so the health tracking is transparent here (its
    /// own transitions are proven by the recovery machine tests above and the
    /// serve-recovery tests below).
    fn serve<B: Block>(
        device: &mut B,
        read_only: bool,
        request: &BlkRequest,
        window: &mut [u8],
    ) -> Result<BlkCompletion, Errno> {
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let len = serve_request_recovering(
            device,
            read_only,
            &encode_request(request),
            window,
            &mut reply,
            &mut health,
            0,
        );
        decode_completion(&reply[..len])
    }

    #[test]
    fn serve_geometry_reports_the_device_and_the_write_policy() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let request = BlkRequest {
            op: BlkOp::Geometry,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &request, &mut window),
            Ok(BlkCompletion {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
                flags: 0,
            })
        );
        assert_eq!(
            serve(&mut device, true, &request, &mut window),
            Ok(BlkCompletion {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
                flags: BLK_FLAG_READ_ONLY,
            })
        );
    }

    #[test]
    fn serve_write_then_read_round_trips_through_the_window() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        window[..BLOCK_SIZE].copy_from_slice(&[0xA5u8; BLOCK_SIZE]);
        let write = BlkRequest {
            op: BlkOp::Write,
            lba: 3,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &write, &mut window),
            Ok(BlkCompletion::default())
        );

        window.fill(0);
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 3,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Ok(BlkCompletion::default())
        );
        assert_eq!(&window[..BLOCK_SIZE], &[0xA5u8; BLOCK_SIZE]);
    }

    #[test]
    fn serve_a_write_to_a_read_only_device_is_refused_before_the_device() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        window[..BLOCK_SIZE].fill(0xFF);
        let write = BlkRequest {
            op: BlkOp::Write,
            lba: 0,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, true, &write, &mut window),
            Err(Errno::PermissionDenied)
        );
        // Nothing reached the medium.
        assert!(device.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn serve_a_transfer_larger_than_the_window_is_refused() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let blocks = u32::try_from(BLK_DATA_LEN / BLOCK_SIZE + 1).expect("fits");
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn serve_a_zero_block_transfer_is_refused() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn serve_an_out_of_range_read_surfaces_the_device_refusal() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let read = BlkRequest {
            op: BlkOp::Read,
            lba: BLOCK_COUNT,
            blocks: 1,
        };
        assert_eq!(
            serve(&mut device, false, &read, &mut window),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn serve_a_malformed_request_is_answered_in_band() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let len = serve_request_recovering(
            &mut device,
            false,
            &[0u8; BLK_REQUEST_LEN - 1],
            &mut window,
            &mut reply,
            &mut health,
            0,
        );
        assert_eq!(
            decode_completion(&reply[..len]),
            Err(Errno::LengthOutOfRange)
        );
        // A malformed request never counts against the device's health.
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn serve_flush_reaches_the_device_exactly_once() {
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let flush = BlkRequest {
            op: BlkOp::Flush,
            lba: 0,
            blocks: 0,
        };
        assert_eq!(
            serve(&mut device, false, &flush, &mut window),
            Ok(BlkCompletion::default())
        );
        assert_eq!(device.flushes, 1);
    }

    /// A device that answers geometry from a fixed [`BlockGeometry`] but
    /// injects a chosen [`DriverError`] into every data transfer — a
    /// stand-in for a disk that is stalling, has a bad sector, or has gone
    /// offline. `fault = None` means it serves reads normally.
    struct FaultyBlock {
        fault: Option<DriverError>,
    }

    impl Block for FaultyBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(BlockGeometry {
                block_size: BLOCK_SIZE_U32,
                block_count: BLOCK_COUNT,
            })
        }

        fn read_blocks(&mut self, _lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            // A healthy read serves deterministic zeroes; an injected fault is
            // returned as the device's own error.
            if let Some(err) = self.fault {
                return Err(err);
            }
            buf.fill(0);
            Ok(())
        }

        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            self.fault.map_or(Ok(()), Err)
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// Serve one read against `device`, returning the framed completion's
    /// [`BlkStatus`] so a test can assert the health axis the consumer sees.
    fn serve_read_status(
        device: &mut FaultyBlock,
        health: &mut BlkHealth,
        now_ns: u64,
    ) -> BlkStatus {
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let read = encode_request(&BlkRequest {
            op: BlkOp::Read,
            lba: 0,
            blocks: 1,
        });
        let len = serve_request_recovering(
            device,
            false,
            &read,
            &mut window,
            &mut reply,
            health,
            now_ns,
        );
        decode_outcome(&reply[..len]).status
    }

    #[test]
    fn serve_a_transient_stall_is_ridden_out_then_fails_closed_at_the_window() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::EndpointStalled),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::Removable);
        let grace = health.budget().grace_ns;
        // Inside the grace window the stall is answered reissuably, and the
        // device is held Recovering — never hard-failed on the first blip.
        assert_eq!(
            serve_read_status(&mut device, &mut health, 0),
            BlkStatus::Reset
        );
        assert_eq!(health.state(), BlkHealthState::Recovering);
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace / 2),
            BlkStatus::Reset
        );
        // Still stalling once the window elapses: only now is it failed closed.
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace),
            BlkStatus::Offline
        );
        assert_eq!(health.state(), BlkHealthState::Faulted);
        // The device comes back: the next good read recovers it, no reboot.
        device.fault = None;
        assert_eq!(
            serve_read_status(&mut device, &mut health, grace + 1),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn serve_a_blip_that_returns_inside_the_window_leaves_no_scar() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::Busy),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::SolidState);
        assert_eq!(
            serve_read_status(&mut device, &mut health, 10),
            BlkStatus::Reset
        );
        assert_eq!(health.state(), BlkHealthState::Recovering);
        // Returns well inside the window: fully recovered, invisible to data.
        device.fault = None;
        assert_eq!(
            serve_read_status(&mut device, &mut health, 100),
            BlkStatus::Ok
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn serve_a_bad_sector_surfaces_without_faulting_the_device() {
        let mut device = FaultyBlock {
            fault: Some(DriverError::MediumError),
        };
        let mut health = BlkHealth::new(BlkDeviceClass::Rotational);
        // A permanent medium error is surfaced to the request, but the device
        // itself stays Healthy — a bad block is not a dead disk.
        assert_eq!(
            serve_read_status(&mut device, &mut health, 0),
            BlkStatus::MediumError
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }

    #[test]
    fn serve_a_device_out_of_range_rejection_is_health_neutral() {
        // The device rejects an out-of-range LBA (a request-level fault it
        // raised): it must not count against the grace window.
        let mut device = MemBlock::new();
        let mut window = vec![0u8; BLK_DATA_LEN];
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let mut health = BlkHealth::new(BlkDeviceClass::Virtual);
        let read = encode_request(&BlkRequest {
            op: BlkOp::Read,
            lba: BLOCK_COUNT,
            blocks: 1,
        });
        let len = serve_request_recovering(
            &mut device,
            false,
            &read,
            &mut window,
            &mut reply,
            &mut health,
            0,
        );
        assert_eq!(
            decode_completion(&reply[..len]),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(health.state(), BlkHealthState::Healthy);
    }
}
