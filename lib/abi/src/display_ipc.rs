//! The display-service IPC protocol (`plans/DISPLAY.md` D7b): the reserved
//! rendezvous a display-driver service binds and the fixed-width,
//! fail-closed requests a seat owner presents frames through.
//!
//! The transport is deliberately zero-copy: a client shares one
//! `shm_create` region holding its frames, hands the service the
//! endpoint-directed `shm_grant` handle once (`Configure`), and thereafter
//! presents by **frame index** (`Present`) — no pixel bytes ever cross the
//! IPC. Every request that acts *for a seat* carries that seat's id, and the
//! service gates each one (including `Query`) on the caller's live seat lease
//! through the kernel's `call_peer_seat` oracle-free check, so only the seat
//! owner can learn the mode, configure frames, or scan out.
//!
//! [`DisplayRequest::QueryStats`] is the one operation that acts for no seat:
//! it describes the *device* this service drives, for the monitor that draws
//! graphics utilisation. It is gated on the caller's kernel-attested
//! `CAP_SYSINFO_HW` instead — a reader that holds no lease, and never will,
//! must still be able to read hardware utilisation, and that is the authority
//! the hardware inventory it details is already read under.
//!
//! A `Present` names a whole frame's damage in one call: it carries a
//! [`DamageList`] of up to [`MAX_DAMAGE_RECTS`] rectangles inline, so a
//! frame that changed two far-apart places costs one round trip and two
//! rectangle-sized blits rather than a round trip each and a blit of the
//! box spanning them.
//!
//! Requests are the fixed-width [`DisplayRequest`]. `Configure` and
//! `Present` answer with the shared status frame
//! ([`crate::reply::encode_status_reply`] /
//! [`crate::reply::decode_status_reply`]); `Query` answers with the
//! [`DISPLAY_MODE_REPLY_LEN`]-byte mode reply ([`encode_mode_reply`] /
//! [`decode_mode_reply`]); `QueryStats` with the
//! [`DISPLAY_STATS_REPLY_LEN`]-byte statistics reply ([`encode_stats_reply`] /
//! [`decode_stats_reply`]). Every decode fails closed: an unknown magic,
//! version, operation, format, an out-of-bounds frame count, an empty
//! damage rectangle, or a dirty reserved field refuses rather than
//! guessing.

use crate::driver::display::{
    AccelCaps, DamageRect, DisplayDeviceReport, DisplayFormat, DisplayMode, MAX_DAMAGE_RECTS,
};
use crate::le::{put_u16, put_u32, put_u64, read_u16, read_u32, read_u64};
use crate::Errno;

/// Reserved well-known call-endpoint id of the display service (`"DIS"`
/// hex-spelled prefix, mirroring [`crate::seat::SEATMGR_ENDPOINT`]'s
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would receive the desktop session's frames and learn
/// the seat owner's shared-memory grant. One endpoint serves every seat —
/// requests carry the seat id in-protocol, so a later multi-GPU broker is
/// additive, never a second protocol.
pub const DISPLAY_ENDPOINT: u64 = 0x0D15_1001;

/// Magic number identifying a display-service request (`"DSP1"`
/// little-endian).
pub const DISPLAY_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"DSP1");

/// The `display-v1` protocol version.
pub const DISPLAY_VERSION_V1: u16 = 1;

/// Most frames one `Configure` may lay out in its shared region. A
/// validation bound, not a capacity: two frames are the double-buffer
/// steady state, three serve a mailbox-style presenter, and anything
/// beyond four buys no latency while letting a hostile client reserve
/// unbounded pinned memory.
pub const DISPLAY_MAX_FRAMES: u32 = 4;

/// Maximum request, in bytes, the [`DISPLAY_ENDPOINT`] accepts: exactly
/// one fixed-width [`DisplayRequest`].
pub const DISPLAY_MAX_REQUEST: usize = DisplayRequest::WIRE_LEN;

/// One display-service operation (`plans/DISPLAY.md` D7b).
///
/// Every variant that acts for a seat names it, and the service derives the
/// right to perform it from the caller's **live lease** on that seat
/// (`call_peer_seat`), never from a claimed handle. [`Self::QueryStats`] acts
/// for no seat and carries its own authority.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisplayRequest {
    /// Report the active mode of `seat_id`'s display.
    Query {
        /// The seat whose display mode is requested.
        seat_id: u64,
    },
    /// Adopt the caller's shared frame region for `seat_id`: map the
    /// granted region once and validate the frame layout against the
    /// active mode. Replaces any earlier configuration by the same lease.
    Configure {
        /// The seat the frames will be presented on.
        seat_id: u64,
        /// The `shm_grant` handle minted to this endpoint's serving task.
        shm_handle: u64,
        /// Frames laid out back-to-back in the region
        /// (`1..=DISPLAY_MAX_FRAMES`).
        frame_count: u32,
        /// Frame width in pixels; must equal the active mode's.
        width_px: u32,
        /// Frame height in pixels; must equal the active mode's.
        height_px: u32,
        /// Bytes between consecutive scanlines; must equal the active
        /// mode's stride, so a configured frame is bit-compatible with
        /// the scan-out surface.
        stride_bytes: u32,
        /// Pixel encoding; must equal the active mode's.
        format: DisplayFormat,
    },
    /// Report the device's own graphics statistics: how long it has been
    /// driven, the memory it owns, what its compositor can do, and the mode
    /// it scans out.
    ///
    /// The one operation that names **no seat**: it describes the device this
    /// service drives, not an act on anybody's screen, so a monitor that
    /// holds no lease can read it. Its authority is `CAP_SYSINFO_HW` — the
    /// same the hardware inventory it details is read under — checked against
    /// the caller's kernel-attested capabilities.
    QueryStats,
    /// Scan out configured frame `frame_index`, of which only `damage`
    /// changed since the previously presented frame.
    Present {
        /// The seat the frame is presented on.
        seat_id: u64,
        /// Index of the frame inside the configured region.
        frame_index: u32,
        /// The changed rectangles; a single [`DamageRect::full`] presents
        /// the whole frame. Never empty.
        damage: DamageList,
    },
}

/// The damage a [`DisplayRequest::Present`] names: one to
/// [`MAX_DAMAGE_RECTS`] rectangles, held inline in the fixed-width frame.
///
/// Constructing one is the only way to name a present's damage, so the count
/// bound and the "no empty rectangle" rule hold before a byte is encoded as
/// well as after one is decoded — a decoded list is exactly as trustworthy as
/// a locally built one. Rectangles beyond the live count are zero, which is
/// what makes the frame's reserved tail checkable.
///
/// Equality compares the rectangles named, not the dead slots behind them.
#[derive(Copy, Clone, Debug)]
pub struct DamageList {
    rects: [DamageRect; MAX_DAMAGE_RECTS],
    count: u8,
}

/// The zero rectangle filling a [`DamageList`]'s unused slots. Not a valid
/// damage rectangle — it is never inside the live prefix.
const NO_RECT: DamageRect = DamageRect {
    x: 0,
    y: 0,
    width_px: 0,
    height_px: 0,
};

impl DamageList {
    /// The list naming `rects`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] if `rects` is empty, holds more than
    /// [`MAX_DAMAGE_RECTS`] entries, or holds an empty rectangle.
    pub fn new(rects: &[DamageRect]) -> Result<Self, Errno> {
        let count = u8::try_from(rects.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if rects.is_empty() || rects.len() > MAX_DAMAGE_RECTS {
            return Err(Errno::LengthOutOfRange);
        }
        let mut slots = [NO_RECT; MAX_DAMAGE_RECTS];
        for (slot, rect) in slots.iter_mut().zip(rects) {
            if rect.width_px == 0 || rect.height_px == 0 {
                return Err(Errno::LengthOutOfRange);
            }
            *slot = *rect;
        }
        Ok(Self {
            rects: slots,
            count,
        })
    }

    /// The rectangles the present names.
    #[must_use]
    pub fn rects(&self) -> &[DamageRect] {
        &self.rects[..usize::from(self.count)]
    }
}

impl PartialEq for DamageList {
    fn eq(&self, other: &Self) -> bool {
        self.rects() == other.rects()
    }
}

impl Eq for DamageList {}

/// Wire operation discriminant of [`DisplayRequest::Query`].
const OP_QUERY: u16 = 1;
/// Wire operation discriminant of [`DisplayRequest::Configure`].
const OP_CONFIGURE: u16 = 2;
/// Wire operation discriminant of [`DisplayRequest::Present`].
const OP_PRESENT: u16 = 3;
/// Wire operation discriminant of [`DisplayRequest::QueryStats`].
const OP_QUERY_STATS: u16 = 4;

/// Offset of a `Present`'s first damage rectangle.
const PRESENT_RECTS_AT: usize = 24;

/// Encoded size of one [`DamageRect`]: x, y, width, height.
const DAMAGE_RECT_LEN: usize = 16;

impl DisplayRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), seat id
    /// (8), and an operation block whose unused tail must be zero. The
    /// widest block is `Present`'s frame index, rectangle count and its
    /// [`MAX_DAMAGE_RECTS`] inline rectangles.
    pub const WIRE_LEN: usize = PRESENT_RECTS_AT + MAX_DAMAGE_RECTS * DAMAGE_RECT_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, DISPLAY_REQUEST_MAGIC);
        put_u16(&mut out, 4, DISPLAY_VERSION_V1);
        match *self {
            Self::Query { seat_id } => {
                put_u16(&mut out, 6, OP_QUERY);
                put_u64(&mut out, 8, seat_id);
            }
            Self::Configure {
                seat_id,
                shm_handle,
                frame_count,
                width_px,
                height_px,
                stride_bytes,
                format,
            } => {
                put_u16(&mut out, 6, OP_CONFIGURE);
                put_u64(&mut out, 8, seat_id);
                put_u64(&mut out, 16, shm_handle);
                put_u32(&mut out, 24, frame_count);
                put_u32(&mut out, 28, width_px);
                put_u32(&mut out, 32, height_px);
                put_u32(&mut out, 36, stride_bytes);
                out[40] = format.as_u8();
            }
            Self::QueryStats => put_u16(&mut out, 6, OP_QUERY_STATS),
            Self::Present {
                seat_id,
                frame_index,
                damage,
            } => {
                put_u16(&mut out, 6, OP_PRESENT);
                put_u64(&mut out, 8, seat_id);
                put_u32(&mut out, 16, frame_index);
                put_u32(&mut out, 20, u32::from(damage.count));
                for (index, rect) in damage.rects().iter().enumerate() {
                    put_rect(&mut out, PRESENT_RECTS_AT + index * DAMAGE_RECT_LEN, rect);
                }
            }
        }
        out
    }

    /// Decode from `bytes`, failing closed on any malformed input.
    ///
    /// Semantic bounds a decoder can already see are enforced here — the
    /// frame count within `1..=DISPLAY_MAX_FRAMES`, a non-empty damage
    /// rectangle, a plausible `Configure` geometry (no zero extent, a
    /// stride that holds one scanline) — so no accepted request ever
    /// carries a value the server would have to re-reject structurally.
    /// Bounds only the server knows (the active mode, the configured
    /// frame count) stay server-side.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic or a dirty reserved tail.
    /// * [`Errno::AbiVersionUnsupported`] — not `display-v1`.
    /// * [`Errno::OutOfRange`] — an operation or pixel format outside the
    ///   closed set.
    /// * [`Errno::LengthOutOfRange`] — a frame count outside
    ///   `1..=DISPLAY_MAX_FRAMES`, a zero-extent geometry, a stride too
    ///   small for one scanline, or an empty damage rectangle.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != DISPLAY_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != DISPLAY_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = read_u16(bytes, 6);
        let seat_id = read_u64(bytes, 8);
        match op {
            OP_QUERY => {
                reserved_zero(bytes, 16)?;
                Ok(Self::Query { seat_id })
            }
            OP_CONFIGURE => {
                reserved_zero(bytes, 41)?;
                let shm_handle = read_u64(bytes, 16);
                let frame_count = read_u32(bytes, 24);
                if frame_count == 0 || frame_count > DISPLAY_MAX_FRAMES {
                    return Err(Errno::LengthOutOfRange);
                }
                let width_px = read_u32(bytes, 28);
                let height_px = read_u32(bytes, 32);
                let stride_bytes = read_u32(bytes, 36);
                let format = DisplayFormat::from_u8(bytes[40])?;
                if width_px == 0 || height_px == 0 {
                    return Err(Errno::LengthOutOfRange);
                }
                let min_stride = u64::from(width_px) * u64::from(format.bytes_per_pixel());
                if u64::from(stride_bytes) < min_stride {
                    return Err(Errno::LengthOutOfRange);
                }
                Ok(Self::Configure {
                    seat_id,
                    shm_handle,
                    frame_count,
                    width_px,
                    height_px,
                    stride_bytes,
                    format,
                })
            }
            OP_QUERY_STATS => {
                // The common seat slot is part of this operation's reserved
                // tail: a stats read names no seat, so a seat smuggled into
                // one is refused rather than ignored.
                reserved_zero(bytes, 8)?;
                Ok(Self::QueryStats)
            }
            OP_PRESENT => {
                let frame_index = read_u32(bytes, 16);
                let count = usize::try_from(read_u32(bytes, 20))
                    .ok()
                    .filter(|count| *count <= MAX_DAMAGE_RECTS)
                    .ok_or(Errno::LengthOutOfRange)?;
                // The slots past the live count are part of the reserved
                // tail: a rectangle smuggled behind the count is refused,
                // never ignored.
                reserved_zero(bytes, PRESENT_RECTS_AT + count * DAMAGE_RECT_LEN)?;
                let mut rects = [NO_RECT; MAX_DAMAGE_RECTS];
                for (index, slot) in rects.iter_mut().take(count).enumerate() {
                    *slot = read_rect(bytes, PRESENT_RECTS_AT + index * DAMAGE_RECT_LEN);
                }
                Ok(Self::Present {
                    seat_id,
                    frame_index,
                    damage: DamageList::new(&rects[..count])?,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Write one damage rectangle at `at`.
fn put_rect(out: &mut [u8; DisplayRequest::WIRE_LEN], at: usize, rect: &DamageRect) {
    put_u32(out, at, rect.x);
    put_u32(out, at + 4, rect.y);
    put_u32(out, at + 8, rect.width_px);
    put_u32(out, at + 12, rect.height_px);
}

/// Read one damage rectangle from `at`. The result is bounds-checked by
/// [`DamageList::new`] (non-empty) and by the server (inside the mode).
fn read_rect(bytes: &[u8], at: usize) -> DamageRect {
    DamageRect {
        x: read_u32(bytes, at),
        y: read_u32(bytes, at + 4),
        width_px: read_u32(bytes, at + 8),
        height_px: read_u32(bytes, at + 12),
    }
}

/// Refuse a request whose reserved tail (from `from` to the end of the
/// fixed frame) carries any non-zero byte — wire corruption or a smuggled
/// field, never silently ignored.
fn reserved_zero(bytes: &[u8], from: usize) -> Result<(), Errno> {
    if bytes[from..DisplayRequest::WIRE_LEN]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    Ok(())
}

/// Reply length, in bytes, of a successful `Query`: the status word
/// followed by the mode record — width (4), height (4), stride (4),
/// format (1), and a reserved tail (3) that must be zero.
pub const DISPLAY_MODE_REPLY_LEN: usize = 20;

/// Encode a `Query` outcome: the [`DISPLAY_MODE_REPLY_LEN`]-byte mode
/// reply on success, the shared status frame (a negative [`Errno`]
/// discriminant, zero-padded to the same length) on refusal. Padding the
/// refusal keeps the reply length constant, so a client can always issue
/// one fixed-size receive.
#[must_use]
pub fn encode_mode_reply(result: Result<DisplayMode, Errno>) -> [u8; DISPLAY_MODE_REPLY_LEN] {
    let mut out = [0u8; DISPLAY_MODE_REPLY_LEN];
    match result {
        Ok(mode) => {
            put_u32(&mut out, 4, mode.width_px);
            put_u32(&mut out, 8, mode.height_px);
            put_u32(&mut out, 12, mode.stride_bytes);
            out[16] = mode.format.as_u8();
        }
        Err(err) => {
            out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err)));
        }
    }
    out
}

/// Decode a `Query` reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word or an unknown pixel
///   format (fail closed on a malformed frame).
/// * [`Errno::BadMagic`] — a dirty reserved tail.
/// * [`Errno::LengthOutOfRange`] — a mode with a zero extent or a stride
///   too small for one scanline (a nonsensical mode is refused, never
///   handed to a renderer).
/// * The decoded [`Errno`] itself, when the service refused the query.
pub fn decode_mode_reply(bytes: &[u8]) -> Result<DisplayMode, Errno> {
    if bytes.len() < DISPLAY_MODE_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let width_px = read_u32(bytes, 4);
    let height_px = read_u32(bytes, 8);
    let stride_bytes = read_u32(bytes, 12);
    let format = DisplayFormat::from_u8(bytes[16])?;
    if bytes[17..DISPLAY_MODE_REPLY_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    if width_px == 0 || height_px == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let min_stride = u64::from(width_px) * u64::from(format.bytes_per_pixel());
    if u64::from(stride_bytes) < min_stride {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(DisplayMode {
        width_px,
        height_px,
        stride_bytes,
        format,
    })
}

/// A display device's own graphics statistics, as
/// [`DisplayRequest::QueryStats`] answers them.
///
/// Composed of the two types that already define its parts — the driver's own
/// [`DisplayDeviceReport`] and the [`DisplayMode`] it scans out — plus the
/// service-measured occupancy, so no field here is a second spelling of one
/// defined elsewhere.
///
/// `busy_ns` and `idle_ns` partition the window since the device was first
/// driven: the same busy/idle vocabulary the CPU reading uses, so utilisation
/// derives the same way and no new averaging convention appears. Both are
/// cumulative and never reset, so a reader takes a two-sample delta over its
/// own interval; a first sample therefore yields no utilisation, and a device
/// nothing has presented to reports both as `0` rather than an idle share of a
/// window that never opened.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DisplayStats {
    /// The seat whose frames the device is currently configured for, or `0`
    /// when nothing is configured. A fact about the device, not a key.
    pub seat_id: u64,
    /// Cumulative nanoseconds the device had a present in flight.
    pub busy_ns: u64,
    /// Cumulative nanoseconds it did not, over the same window.
    pub idle_ns: u64,
    /// What the driver reports about the device itself.
    pub device: DisplayDeviceReport,
    /// The mode being scanned out.
    pub mode: DisplayMode,
}

/// Set in a statistics reply when the device has a hardware compositor, so
/// the accelerated-capability fields carry a reading rather than a nought.
const STATS_FLAG_ACCELERATED: u16 = 1 << 0;
/// Set when that compositor can scale a whole layer by a constant opacity
/// ([`AccelCaps::per_layer_opacity`]).
const STATS_FLAG_PER_LAYER_OPACITY: u16 = 1 << 1;
/// Every flag bit this ABI defines; any other set bit fails the decode
/// closed rather than being ignored.
const STATS_FLAGS_KNOWN: u16 = STATS_FLAG_ACCELERATED | STATS_FLAG_PER_LAYER_OPACITY;

impl DisplayStats {
    /// Encoded size on the wire, and the size of one packed
    /// `GPU_DEVICE_STATS` record.
    ///
    /// Layout: `busy_ns` (8), `idle_ns` (8), `mem_resident_bytes` (8),
    /// `mem_total_bytes` (8), `seat_id` (8), `max_layers` (4),
    /// `max_width_px` (4), `max_height_px` (4), mode width (4), height (4),
    /// stride (4), format (1), reserved (1), flags (2), reserved (4). Every
    /// field is naturally aligned within the record and every reserved byte
    /// must be zero on the wire.
    pub const WIRE_LEN: usize = 72;

    /// Byte offset of the accelerated-capability block.
    const ACCEL_AT: usize = 40;
    /// Byte offset of the mode block.
    const MODE_AT: usize = 52;
    /// Byte offset of the capability flags.
    const FLAGS_AT: usize = 66;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        let mut flags = 0u16;
        if let Some(caps) = self.device.accel {
            flags |= STATS_FLAG_ACCELERATED;
            if caps.per_layer_opacity {
                flags |= STATS_FLAG_PER_LAYER_OPACITY;
            }
            put_u32(&mut out, Self::ACCEL_AT, caps.max_layers);
            put_u32(&mut out, Self::ACCEL_AT + 4, caps.max_width_px);
            put_u32(&mut out, Self::ACCEL_AT + 8, caps.max_height_px);
        }
        put_u16(&mut out, Self::FLAGS_AT, flags);
        put_u64(&mut out, 0, self.busy_ns);
        put_u64(&mut out, 8, self.idle_ns);
        put_u64(&mut out, 16, self.device.mem_resident_bytes);
        put_u64(&mut out, 24, self.device.mem_total_bytes);
        put_u64(&mut out, 32, self.seat_id);
        put_u32(&mut out, Self::MODE_AT, self.mode.width_px);
        put_u32(&mut out, Self::MODE_AT + 4, self.mode.height_px);
        put_u32(&mut out, Self::MODE_AT + 8, self.mode.stride_bytes);
        out[Self::MODE_AT + 12] = self.mode.format.as_u8();
        out
    }

    /// Decode from `bytes`.
    ///
    /// Every relation the producer must have honoured is re-checked here, so
    /// a reader never renders a service's arithmetic: a device with no memory
    /// of its own cannot claim resident bytes, an unaccelerated device cannot
    /// carry compositor limits, and an accelerated one must be able to
    /// composite at least one layer of at least one pixel.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` is shorter than
    ///   [`Self::WIRE_LEN`].
    /// * [`Errno::OutOfRange`] — an unknown pixel format.
    /// * [`Errno::BadMagic`] — an unknown flag bit or a dirty reserved field.
    /// * [`Errno::LengthOutOfRange`] — a nonsensical mode (zero extent, or a
    ///   stride too small for one scanline), resident bytes above the memory
    ///   the device owns, or an accelerated-capability block that contradicts
    ///   its flag.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let flags = read_u16(bytes, Self::FLAGS_AT);
        if flags & !STATS_FLAGS_KNOWN != 0
            || bytes[Self::MODE_AT + 13] != 0
            || bytes[Self::FLAGS_AT + 2..Self::WIRE_LEN]
                .iter()
                .any(|&b| b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let mem_resident_bytes = read_u64(bytes, 16);
        let mem_total_bytes = read_u64(bytes, 24);
        if mem_resident_bytes > mem_total_bytes {
            return Err(Errno::LengthOutOfRange);
        }
        let max_layers = read_u32(bytes, Self::ACCEL_AT);
        let max_width_px = read_u32(bytes, Self::ACCEL_AT + 4);
        let max_height_px = read_u32(bytes, Self::ACCEL_AT + 8);
        let accel = if flags & STATS_FLAG_ACCELERATED != 0 {
            if max_layers == 0 || max_width_px == 0 || max_height_px == 0 {
                return Err(Errno::LengthOutOfRange);
            }
            Some(AccelCaps {
                max_layers,
                max_width_px,
                max_height_px,
                per_layer_opacity: flags & STATS_FLAG_PER_LAYER_OPACITY != 0,
            })
        } else {
            if max_layers != 0
                || max_width_px != 0
                || max_height_px != 0
                || flags & STATS_FLAG_PER_LAYER_OPACITY != 0
            {
                return Err(Errno::LengthOutOfRange);
            }
            None
        };
        let format = DisplayFormat::from_u8(bytes[Self::MODE_AT + 12])?;
        let mode = DisplayMode {
            width_px: read_u32(bytes, Self::MODE_AT),
            height_px: read_u32(bytes, Self::MODE_AT + 4),
            stride_bytes: read_u32(bytes, Self::MODE_AT + 8),
            format,
        };
        if mode.width_px == 0
            || mode.height_px == 0
            || u64::from(mode.stride_bytes)
                < u64::from(mode.width_px) * u64::from(format.bytes_per_pixel())
        {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Self {
            seat_id: read_u64(bytes, 32),
            busy_ns: read_u64(bytes, 0),
            idle_ns: read_u64(bytes, 8),
            device: DisplayDeviceReport {
                mem_resident_bytes,
                mem_total_bytes,
                accel,
            },
            mode,
        })
    }
}

/// Reply length, in bytes, of a `QueryStats`: the status word followed by one
/// [`DisplayStats`] record.
pub const DISPLAY_STATS_REPLY_LEN: usize = 4 + DisplayStats::WIRE_LEN;

/// Encode a `QueryStats` outcome: the status word plus the statistics record
/// on success, the status frame zero-padded to the same length on refusal, so
/// a client always issues one fixed-size receive.
#[must_use]
pub fn encode_stats_reply(result: Result<DisplayStats, Errno>) -> [u8; DISPLAY_STATS_REPLY_LEN] {
    let mut out = [0u8; DISPLAY_STATS_REPLY_LEN];
    out[..4].copy_from_slice(&crate::reply::encode_status_reply(result.map(|_| ())));
    if let Ok(stats) = result {
        out[4..].copy_from_slice(&stats.to_le_bytes());
    }
    out
}

/// Decode a `QueryStats` reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word.
/// * The decoded [`Errno`] itself, when the service refused the read.
/// * Everything [`DisplayStats::from_bytes`] can return.
pub fn decode_stats_reply(bytes: &[u8]) -> Result<DisplayStats, Errno> {
    if bytes.len() < DISPLAY_STATS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    DisplayStats::from_bytes(&bytes[4..DISPLAY_STATS_REPLY_LEN])
}

#[cfg(test)]
mod tests {
    use super::{
        decode_mode_reply, decode_stats_reply, encode_mode_reply, encode_stats_reply, DamageList,
        DisplayRequest, DisplayStats, DAMAGE_RECT_LEN, DISPLAY_MAX_FRAMES, DISPLAY_MODE_REPLY_LEN,
        DISPLAY_REQUEST_MAGIC, DISPLAY_STATS_REPLY_LEN, NO_RECT, PRESENT_RECTS_AT,
    };
    use crate::driver::display::{
        AccelCaps, DamageRect, DisplayDeviceReport, DisplayFormat, DisplayMode, MAX_DAMAGE_RECTS,
    };
    use crate::Errno;

    /// Byte offset of the statistics record inside its reply frame: the
    /// status word precedes it, so a test that dirties a record field aims
    /// past that word.
    const STATS_BODY_AT: usize = 4;

    /// The bound as a `u32`, for the tests that count rectangles.
    fn max_rects() -> u32 {
        u32::try_from(MAX_DAMAGE_RECTS).expect("the bound fits a u32")
    }

    /// The rectangle `index` pixels down the surface, so a list's entries
    /// are distinguishable at a glance.
    fn rect(index: u32) -> DamageRect {
        DamageRect {
            x: 10,
            y: 20 + index,
            width_px: 30,
            height_px: 40,
        }
    }

    /// One rectangle more than the bound allows, so a test can slice out any
    /// length it needs (this crate allocates nowhere, tests included).
    fn rects() -> [DamageRect; MAX_DAMAGE_RECTS + 1] {
        let mut all = [NO_RECT; MAX_DAMAGE_RECTS + 1];
        for (index, slot) in all.iter_mut().enumerate() {
            *slot = rect(u32::try_from(index).expect("a small index"));
        }
        all
    }

    fn damage(count: u32) -> DamageList {
        let count = usize::try_from(count).expect("a small count");
        DamageList::new(&rects()[..count]).expect("a list within the bound")
    }

    fn sample_configure() -> DisplayRequest {
        DisplayRequest::Configure {
            seat_id: 0,
            shm_handle: 7,
            frame_count: 2,
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        }
    }

    fn sample_present() -> DisplayRequest {
        DisplayRequest::Present {
            seat_id: 0,
            frame_index: 1,
            damage: damage(1),
        }
    }

    #[test]
    fn requests_round_trip() {
        for request in [
            DisplayRequest::Query { seat_id: 3 },
            sample_configure(),
            sample_present(),
        ] {
            let bytes = request.to_le_bytes();
            assert_eq!(DisplayRequest::from_bytes(&bytes), Ok(request));
        }
    }

    /// A whole frame's damage travels in one request: every list length the
    /// bound allows survives the wire, rectangles and order intact.
    #[test]
    fn a_present_carries_a_whole_damage_list() {
        for count in 1..=max_rects() {
            let request = DisplayRequest::Present {
                seat_id: 7,
                frame_index: 1,
                damage: damage(count),
            };
            let decoded = DisplayRequest::from_bytes(&request.to_le_bytes());
            assert_eq!(decoded, Ok(request), "{count} rectangles");
            let Ok(DisplayRequest::Present { damage, .. }) = decoded else {
                panic!("a present decodes as one");
            };
            assert_eq!(u32::try_from(damage.rects().len()), Ok(count));
            assert_eq!(damage.rects().last(), Some(&rect(count - 1)));
        }
    }

    /// The list's bound and its "no empty rectangle" rule hold at
    /// construction, so a caller cannot build one the wire would refuse.
    #[test]
    fn a_damage_list_is_bounded_and_never_empty() {
        assert_eq!(DamageList::new(&[]).err(), Some(Errno::LengthOutOfRange));
        assert_eq!(
            DamageList::new(&rects()).err(),
            Some(Errno::LengthOutOfRange),
            "one rectangle past the bound"
        );
        let with_empty = [
            rect(0),
            DamageRect {
                x: 0,
                y: 0,
                width_px: 0,
                height_px: 1,
            },
        ];
        assert_eq!(
            DamageList::new(&with_empty).err(),
            Some(Errno::LengthOutOfRange)
        );
        // Equality is the rectangles named, not the slots behind them.
        assert_eq!(damage(2), damage(2));
        assert_ne!(damage(2), damage(3));
    }

    /// A count past the bound, and a rectangle hidden in a slot the count
    /// does not reach, are both refused rather than partly honoured.
    #[test]
    fn a_present_refuses_a_smuggled_rectangle() {
        let encoded = DisplayRequest::Present {
            seat_id: 0,
            frame_index: 0,
            damage: damage(2),
        }
        .to_le_bytes();

        let mut over = encoded;
        over[20..24].copy_from_slice(&(max_rects() + 1).to_le_bytes());
        assert_eq!(
            DisplayRequest::from_bytes(&over),
            Err(Errno::LengthOutOfRange)
        );
        let mut none = encoded;
        none[20..24].copy_from_slice(&0u32.to_le_bytes());
        none[PRESENT_RECTS_AT..].fill(0);
        assert_eq!(
            DisplayRequest::from_bytes(&none),
            Err(Errno::LengthOutOfRange),
            "a present that names nothing changed"
        );
        // A rectangle the count does not reach is a dirty reserved slot,
        // whether it sits behind a live one or behind a zeroed count.
        for at in [PRESENT_RECTS_AT + 2 * DAMAGE_RECT_LEN, PRESENT_RECTS_AT] {
            let mut behind = encoded;
            if at == PRESENT_RECTS_AT {
                behind[20..24].copy_from_slice(&0u32.to_le_bytes());
            }
            behind[at] = 1;
            assert_eq!(DisplayRequest::from_bytes(&behind), Err(Errno::BadMagic));
        }
    }

    #[test]
    fn magic_is_the_ascii_tag() {
        assert_eq!(DISPLAY_REQUEST_MAGIC, u32::from_le_bytes(*b"DSP1"));
    }

    #[test]
    fn decode_fails_closed_on_malformed_framing() {
        let good = sample_configure().to_le_bytes();

        assert_eq!(
            DisplayRequest::from_bytes(&good[..DisplayRequest::WIRE_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        let mut bad_magic = good;
        bad_magic[0] ^= 0xFF;
        assert_eq!(DisplayRequest::from_bytes(&bad_magic), Err(Errno::BadMagic));
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            DisplayRequest::from_bytes(&bad_version),
            Err(Errno::AbiVersionUnsupported)
        );
        let mut bad_op = good;
        bad_op[6] = 9;
        assert_eq!(DisplayRequest::from_bytes(&bad_op), Err(Errno::OutOfRange));
    }

    #[test]
    fn decode_refuses_dirty_reserved_tails() {
        // A query must carry nothing beyond its seat id.
        let mut query = DisplayRequest::Query { seat_id: 0 }.to_le_bytes();
        query[16] = 1;
        assert_eq!(DisplayRequest::from_bytes(&query), Err(Errno::BadMagic));
        // A configure's tail past the format byte must be zero.
        let mut configure = sample_configure().to_le_bytes();
        configure[47] = 1;
        assert_eq!(DisplayRequest::from_bytes(&configure), Err(Errno::BadMagic));
        // A present's tail past its last damage rectangle must be zero.
        let mut present = sample_present().to_le_bytes();
        present[PRESENT_RECTS_AT + DAMAGE_RECT_LEN] = 1;
        assert_eq!(DisplayRequest::from_bytes(&present), Err(Errno::BadMagic));
    }

    #[test]
    fn configure_bounds_are_enforced() {
        let encode = |frame_count: u32, width: u32, height: u32, stride: u32, format: u8| {
            let mut bytes = sample_configure().to_le_bytes();
            bytes[24..28].copy_from_slice(&frame_count.to_le_bytes());
            bytes[28..32].copy_from_slice(&width.to_le_bytes());
            bytes[32..36].copy_from_slice(&height.to_le_bytes());
            bytes[36..40].copy_from_slice(&stride.to_le_bytes());
            bytes[40] = format;
            DisplayRequest::from_bytes(&bytes)
        };
        // Frame count outside 1..=DISPLAY_MAX_FRAMES.
        assert_eq!(encode(0, 640, 480, 2560, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(
            encode(DISPLAY_MAX_FRAMES + 1, 640, 480, 2560, 2),
            Err(Errno::LengthOutOfRange)
        );
        // Zero extents and a stride too small for one scanline.
        assert_eq!(encode(2, 0, 480, 2560, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 640, 0, 2560, 2), Err(Errno::LengthOutOfRange));
        assert_eq!(encode(2, 640, 480, 2559, 2), Err(Errno::LengthOutOfRange));
        // An unknown pixel format.
        assert_eq!(encode(2, 640, 480, 2560, 9), Err(Errno::OutOfRange));
        // The widest accepted count still round-trips.
        assert!(encode(DISPLAY_MAX_FRAMES, 640, 480, 2560, 2).is_ok());
    }

    #[test]
    fn present_refuses_an_empty_damage_rectangle() {
        // The width and height of the first rectangle, at its own offset.
        let width_at = PRESENT_RECTS_AT + 8;
        let height_at = PRESENT_RECTS_AT + 12;
        let mut zero_width = sample_present().to_le_bytes();
        zero_width[width_at..width_at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            DisplayRequest::from_bytes(&zero_width),
            Err(Errno::LengthOutOfRange)
        );
        let mut zero_height = sample_present().to_le_bytes();
        zero_height[height_at..height_at + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            DisplayRequest::from_bytes(&zero_height),
            Err(Errno::LengthOutOfRange)
        );
        // An empty rectangle behind a live one is refused too: the whole
        // list is checked, never just its first entry.
        let mut second = DisplayRequest::Present {
            seat_id: 0,
            frame_index: 0,
            damage: damage(2),
        }
        .to_le_bytes();
        let second_width = PRESENT_RECTS_AT + DAMAGE_RECT_LEN + 8;
        second[second_width..second_width + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            DisplayRequest::from_bytes(&second),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn mode_replies_round_trip_ok_and_error() {
        let mode = DisplayMode {
            width_px: 1920,
            height_px: 1080,
            stride_bytes: 7680,
            format: DisplayFormat::Rgba8888,
        };
        assert_eq!(decode_mode_reply(&encode_mode_reply(Ok(mode))), Ok(mode));
        assert_eq!(
            decode_mode_reply(&encode_mode_reply(Err(Errno::SeatNotOwner))),
            Err(Errno::SeatNotOwner)
        );
        assert_eq!(
            decode_mode_reply(&encode_mode_reply(Err(Errno::SeatRevoked))),
            Err(Errno::SeatRevoked)
        );
    }

    #[test]
    fn mode_reply_decode_fails_closed() {
        let mode = DisplayMode {
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        };
        let good = encode_mode_reply(Ok(mode));

        assert_eq!(
            decode_mode_reply(&good[..DISPLAY_MODE_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A corrupt (positive) status word.
        let mut bad_status = good;
        bad_status[0] = 1;
        assert_eq!(decode_mode_reply(&bad_status), Err(Errno::OutOfRange));
        // An unknown format byte.
        let mut bad_format = good;
        bad_format[16] = 9;
        assert_eq!(decode_mode_reply(&bad_format), Err(Errno::OutOfRange));
        // A dirty reserved tail.
        let mut dirty = good;
        dirty[19] = 1;
        assert_eq!(decode_mode_reply(&dirty), Err(Errno::BadMagic));
        // A nonsensical mode: zero extents or an undersized stride.
        let mut zero_width = good;
        zero_width[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_mode_reply(&zero_width), Err(Errno::LengthOutOfRange));
        let mut thin_stride = good;
        thin_stride[12..16].copy_from_slice(&2559u32.to_le_bytes());
        assert_eq!(
            decode_mode_reply(&thin_stride),
            Err(Errno::LengthOutOfRange)
        );
    }
    /// The mode every statistics test reports, so a decode failure is always
    /// about the field the test dirtied.
    fn stats_mode() -> DisplayMode {
        DisplayMode {
            width_px: 640,
            height_px: 480,
            stride_bytes: 2560,
            format: DisplayFormat::Bgra8888,
        }
    }

    /// An accelerated device with memory of its own: every optional part of
    /// the reply populated, so a round trip exercises all of them.
    fn accelerated_stats() -> DisplayStats {
        DisplayStats {
            seat_id: 3,
            busy_ns: 4_000_000,
            idle_ns: 96_000_000,
            device: DisplayDeviceReport {
                mem_resident_bytes: 8 << 20,
                mem_total_bytes: 256 << 20,
                accel: Some(AccelCaps {
                    max_layers: 6,
                    max_width_px: 1920,
                    max_height_px: 1080,
                    per_layer_opacity: true,
                }),
            },
            mode: stats_mode(),
        }
    }

    #[test]
    fn query_stats_round_trips_and_names_no_seat() {
        let encoded = DisplayRequest::QueryStats.to_le_bytes();
        assert_eq!(
            DisplayRequest::from_bytes(&encoded),
            Ok(DisplayRequest::QueryStats)
        );
        // The common seat slot is this operation's reserved tail: a seat
        // smuggled into a stats read is refused, never ignored.
        let mut with_seat = encoded;
        with_seat[8] = 1;
        assert_eq!(DisplayRequest::from_bytes(&with_seat), Err(Errno::BadMagic));
    }

    #[test]
    fn stats_reply_round_trips_both_device_shapes() {
        let accelerated = accelerated_stats();
        assert_eq!(
            decode_stats_reply(&encode_stats_reply(Ok(accelerated))),
            Ok(accelerated)
        );
        // A firmware framebuffer: no memory of its own, no hardware
        // compositor. `mem_total_bytes == 0` is the statement "none of its
        // own", and it round trips as `accel: None` rather than as zeroed
        // capabilities.
        let software = DisplayStats {
            seat_id: 0,
            busy_ns: 0,
            idle_ns: 0,
            device: DisplayDeviceReport::SOFTWARE,
            mode: stats_mode(),
        };
        let decoded = decode_stats_reply(&encode_stats_reply(Ok(software)));
        assert_eq!(decoded, Ok(software));
        assert!(decoded.expect("decoded").device.accel.is_none());
    }

    #[test]
    fn stats_reply_carries_a_refusal_at_full_length() {
        let refused = encode_stats_reply(Err(Errno::PermissionDenied));
        assert_eq!(refused.len(), DISPLAY_STATS_REPLY_LEN);
        assert_eq!(decode_stats_reply(&refused), Err(Errno::PermissionDenied));
    }

    #[test]
    fn stats_reply_decode_fails_closed() {
        let good = encode_stats_reply(Ok(accelerated_stats()));

        assert_eq!(
            decode_stats_reply(&good[..DISPLAY_STATS_REPLY_LEN - 1]),
            Err(Errno::BufferTooSmall)
        );
        // A corrupt (positive) status word.
        let mut bad_status = good;
        bad_status[0] = 1;
        assert_eq!(decode_stats_reply(&bad_status), Err(Errno::OutOfRange));
        // An undefined flag bit.
        let mut unknown_flag = good;
        unknown_flag[STATS_BODY_AT + 66] |= 1 << 4;
        assert_eq!(decode_stats_reply(&unknown_flag), Err(Errno::BadMagic));
        // A dirty reserved word, and a dirty reserved tail.
        let mut dirty_word = good;
        dirty_word[STATS_BODY_AT + 65] = 1;
        assert_eq!(decode_stats_reply(&dirty_word), Err(Errno::BadMagic));
        let mut dirty_tail = good;
        dirty_tail[DISPLAY_STATS_REPLY_LEN - 1] = 1;
        assert_eq!(decode_stats_reply(&dirty_tail), Err(Errno::BadMagic));
        // More resident than the device owns.
        let mut over_resident = good;
        over_resident[STATS_BODY_AT + 16..STATS_BODY_AT + 24]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(
            decode_stats_reply(&over_resident),
            Err(Errno::LengthOutOfRange)
        );
        // Compositor limits on a device whose flag says it has none.
        let mut unflagged_caps = good;
        unflagged_caps[STATS_BODY_AT + 66] = 0;
        assert_eq!(
            decode_stats_reply(&unflagged_caps),
            Err(Errno::LengthOutOfRange)
        );
        // An accelerated device that can composite no layer.
        let mut no_layers = good;
        no_layers[STATS_BODY_AT + 40..STATS_BODY_AT + 44].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_stats_reply(&no_layers), Err(Errno::LengthOutOfRange));
        // An unknown format byte, and a nonsensical mode.
        let mut bad_format = good;
        bad_format[STATS_BODY_AT + 64] = 9;
        assert_eq!(decode_stats_reply(&bad_format), Err(Errno::OutOfRange));
        let mut thin_stride = good;
        thin_stride[STATS_BODY_AT + 60..STATS_BODY_AT + 64].copy_from_slice(&2559u32.to_le_bytes());
        assert_eq!(
            decode_stats_reply(&thin_stride),
            Err(Errno::LengthOutOfRange)
        );
    }
}
