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

    /// The device-health status a [`Block`](crate::driver::block::Block)
    /// result's [`DriverError`] carries, or [`None`] when the error is
    /// *request-level* rather than a device-health signal.
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
    /// The monotonic reading at which the current `Recovering` episode began;
    /// meaningful only while `state == Recovering`.
    recovering_since_ns: u64,
}

impl BlkHealth {
    /// A freshly-`Healthy` device served with `class`'s I/O budget.
    #[must_use]
    pub const fn new(class: BlkDeviceClass) -> Self {
        Self {
            state: BlkHealthState::Healthy,
            budget: class.budget(),
            recovering_since_ns: 0,
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
        if !matches!(self.state, BlkHealthState::Recovering) || self.budget.grace_ns == u64::MAX {
            return None;
        }
        Some(
            self.recovering_since_ns
                .saturating_add(self.budget.grace_ns),
        )
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
        if matches!(self.state, BlkHealthState::Recovering) && self.grace_elapsed(now_ns) {
            self.state = BlkHealthState::Faulted;
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
                BlkStatus::Offline
            }
            BlkStatus::Removed => {
                self.state = BlkHealthState::Removed;
                BlkStatus::Removed
            }
            BlkStatus::Fatal => {
                self.state = BlkHealthState::Failed;
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
                self.recovering_since_ns = now_ns;
                BlkStatus::Reset
            }
            // Inside an open window: keep answering reissuably until the
            // window elapses, then fail closed.
            BlkHealthState::Recovering => {
                if self.grace_elapsed(now_ns) {
                    self.state = BlkHealthState::Faulted;
                    BlkStatus::Offline
                } else {
                    BlkStatus::Reset
                }
            }
        }
    }

    /// Whether an open recovery grace window has elapsed at monotonic
    /// `now_ns`. Meaningful only while `Recovering`; `saturating_sub` keeps a
    /// non-monotonic clock reading from wrapping the elapsed time, and a
    /// `u64::MAX` grace window (ride out indefinitely) never reads as elapsed.
    const fn grace_elapsed(&self, now_ns: u64) -> bool {
        now_ns.saturating_sub(self.recovering_since_ns) >= self.budget.grace_ns
    }

    /// Return the device to a live state, clearing any recovery episode.
    fn recover(&mut self, to: BlkHealthState) {
        self.state = to;
        self.recovering_since_ns = 0;
    }
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
    fn window_holds_whole_blocks_of_every_supported_size() {
        for block_size in [512usize, 4096] {
            assert_eq!(BLK_DATA_LEN % block_size, 0);
        }
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
}
