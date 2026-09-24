//! `dmaengine-v1`: the DMA-engine seam (`plans/SOUND.md` SND5).
//!
//! A control block holds bus addresses and nothing between a DMA controller
//! and RAM checks them, so whoever writes one can read and write all of
//! memory. The controller's driver is therefore the only process that maps
//! its registers or writes its control blocks. It serves one endpoint per
//! controller node ([`DMA_CONTROLLER_ENDPOINTS`]), bound under the node's
//! [`DmaControllerDuty`], and a consumer driver calls it naming only claims
//! the kernel attests: the [`DmaRequestLine`] discovery granted it and a
//! FIFO inside its own register windows. The buffer comes back as a
//! shared-memory grant the controller carved, never as an address.
//!
//! A transfer is periodic and cyclic: [`DmaEngineRequest::Prepare`] builds a
//! chain of one interrupting block per period that loops until stopped, and
//! [`DmaEngineRequest::Wait`] is a posted call answered at the first period
//! boundary past the position it names.
//!
//! Every decode is total and exact: a frame of the wrong length, an unknown
//! magic, version, operation or direction, a dirty reserved field, or a value
//! outside its range refuses with one typed [`Errno`].

use core::num::NonZeroU32;

use crate::hwtree::{HwResource, NodeEndpointBlock};
use crate::le::{put_i32, put_u16, put_u32, put_u64, read_i32, read_u16, read_u32, read_u64};
use crate::time::Duration64;
use crate::Errno;

/// The endpoints DMA controllers serve, indexed by the controller's node id.
/// Binding one takes the node's [`HwResourceKind::DmaController`] duty.
///
/// [`HwResourceKind::DmaController`]: crate::hwtree::HwResourceKind::DmaController
pub const DMA_CONTROLLER_ENDPOINTS: NodeEndpointBlock = NodeEndpointBlock::tagged(*b"DM");

/// The most channels one controller node describes: its channel mask is a
/// `u64`.
pub const DMA_MAX_CHANNELS: u8 = 64;

/// The most specifier cells a request line carries; discovery drops a wider
/// binding's entry rather than truncating it.
pub const DMA_SPECIFIER_MAX_CELLS: usize = 2;

/// The longest `dma-names` entry a request line carries; a longer one leaves
/// the line unnamed.
pub const DMA_REQUEST_NAME_MAX: usize = 8;

/// A DMA controller node's duty: the endpoint it serves and, when the tree
/// stated them, the channels this system may use, numbered from the node's
/// own first channel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaControllerDuty {
    endpoint: u64,
    channels: Option<u64>,
}

impl DmaControllerDuty {
    /// A duty to serve `endpoint`.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] if `endpoint` is outside
    /// [`DMA_CONTROLLER_ENDPOINTS`].
    pub fn new(endpoint: u64, channels: Option<u64>) -> Result<Self, Errno> {
        if !DMA_CONTROLLER_ENDPOINTS.contains(endpoint) {
            return Err(Errno::OutOfRange);
        }
        Ok(Self { endpoint, channels })
    }

    /// The endpoint the controller serves.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The usable channels, bit `n` for the node's channel `n`, or [`None`]
    /// when the tree stated no mask.
    #[must_use]
    pub const fn channels(&self) -> Option<u64> {
        self.channels
    }
}

/// One entry of a consumer's `dmas`: the controller endpoint serving it, the
/// request line's specifier in that controller's own binding, the entry's
/// position in the list, and its `dma-names` string.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaRequestLine {
    endpoint: u64,
    index: u8,
    cells: u8,
    specifier: [u32; DMA_SPECIFIER_MAX_CELLS],
    name: [u8; DMA_REQUEST_NAME_MAX],
}

impl DmaRequestLine {
    /// The request line `specifier` on the controller serving `endpoint`,
    /// entry `index` of its consumer's list, named `name`.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — `endpoint` outside
    ///   [`DMA_CONTROLLER_ENDPOINTS`], or a NUL in `name`, which would make
    ///   its padding ambiguous.
    /// * [`Errno::LengthOutOfRange`] — more than [`DMA_SPECIFIER_MAX_CELLS`]
    ///   cells, or a name longer than [`DMA_REQUEST_NAME_MAX`].
    pub fn new(endpoint: u64, index: u8, specifier: &[u32], name: &[u8]) -> Result<Self, Errno> {
        if !DMA_CONTROLLER_ENDPOINTS.contains(endpoint) || name.contains(&0) {
            return Err(Errno::OutOfRange);
        }
        let cells = u8::try_from(specifier.len()).map_err(|_| Errno::LengthOutOfRange)?;
        if usize::from(cells) > DMA_SPECIFIER_MAX_CELLS || name.len() > DMA_REQUEST_NAME_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut padded_specifier = [0u32; DMA_SPECIFIER_MAX_CELLS];
        padded_specifier[..specifier.len()].copy_from_slice(specifier);
        let mut padded_name = [0u8; DMA_REQUEST_NAME_MAX];
        padded_name[..name.len()].copy_from_slice(name);
        Ok(Self {
            endpoint,
            index,
            cells,
            specifier: padded_specifier,
            name: padded_name,
        })
    }

    /// The endpoint of the controller that serves this line.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The entry's position in its consumer's `dmas` list.
    #[must_use]
    pub const fn index(&self) -> u8 {
        self.index
    }

    /// The specifier cells, in the controller's own binding.
    #[must_use]
    pub fn specifier(&self) -> &[u32] {
        &self.specifier[..usize::from(self.cells)]
    }

    /// The entry's `dma-names` string, empty when it had none or it did not
    /// fit.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let len = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(DMA_REQUEST_NAME_MAX);
        &self.name[..len]
    }

    pub(crate) const fn cell_count(&self) -> u8 {
        self.cells
    }

    pub(crate) const fn specifier_cells(&self) -> [u32; DMA_SPECIFIER_MAX_CELLS] {
        self.specifier
    }

    pub(crate) const fn name_bytes(&self) -> [u8; DMA_REQUEST_NAME_MAX] {
        self.name
    }
}

/// Magic number opening every request frame (`"DMAR"`).
pub const DMA_ENGINE_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"DMAR");

/// The `dmaengine-v1` protocol version.
pub const DMA_ENGINE_VERSION_V1: u16 = 1;

/// Request header: magic (4), version (2), operation (1), reserved (1).
const HEADER_LEN: usize = 8;

/// Reply header: status (4), operation (1), reserved (3). The status is `0`
/// or the negated [`Errno`]; a success names the operation it answers, so a
/// reply can never be read as another operation's.
const REPLY_HEADER_LEN: usize = 8;

/// The operation a frame carries.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DmaEngineOp {
    /// Claim a channel for a request line.
    Open = 1,
    /// Carve the buffer and build the cyclic chain.
    Prepare = 2,
    /// Start the chain.
    Start = 3,
    /// Abort the chain and reset the channel.
    Stop = 4,
    /// Read the live memory-side offset.
    Position = 5,
    /// Release the channel and its buffer.
    Close = 6,
    /// Wait for a period boundary.
    Wait = 7,
}

impl DmaEngineOp {
    const fn from_u8(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Open,
            2 => Self::Prepare,
            3 => Self::Start,
            4 => Self::Stop,
            5 => Self::Position,
            6 => Self::Close,
            7 => Self::Wait,
            _ => return None,
        })
    }

    const fn request_body_len(self) -> usize {
        match self {
            Self::Open => HwResource::WIRE_LEN,
            Self::Prepare => prepare::LEN,
            Self::Start | Self::Stop | Self::Position | Self::Close => channel_body::LEN,
            Self::Wait => wait::LEN,
        }
    }

    const fn reply_body_len(self) -> usize {
        match self {
            Self::Open => open_reply::LEN,
            Self::Prepare => prepare_reply::LEN,
            Self::Start | Self::Stop | Self::Close => 0,
            Self::Position => position_reply::LEN,
            Self::Wait => wait_reply::LEN,
        }
    }
}

mod channel_body {
    pub const CHANNEL: usize = 0;
    pub const LEN: usize = 8;
}

mod prepare {
    pub const CHANNEL: usize = 0;
    pub const DIRECTION: usize = 1;
    pub const PERIOD_BYTES: usize = 4;
    pub const PERIODS: usize = 8;
    pub const FIFO: usize = 16;
    pub const LEN: usize = 24;
}

mod wait {
    pub const CHANNEL: usize = 0;
    pub const AFTER: usize = 8;
    pub const LEN: usize = 16;
}

mod open_reply {
    pub const CHANNEL: usize = 0;
    pub const LEN: usize = 8;
}

mod prepare_reply {
    pub const GRANT: usize = 0;
    pub const LEN: usize = 8;
}

mod position_reply {
    pub const OFFSET: usize = 0;
    pub const LEN: usize = 8;
}

mod wait_reply {
    use crate::time::Duration64;

    pub const END: usize = 0;
    pub const ERRORS: usize = 4;
    pub const POSITION: usize = 8;
    pub const SERVICED: usize = 16;
    pub const LEN: usize = SERVICED + Duration64::WIRE_LEN;
}

/// Largest request frame: an [`DmaEngineOp::Open`] quoting a whole record.
pub const DMA_ENGINE_MAX_REQUEST: usize = HEADER_LEN + HwResource::WIRE_LEN;

/// Largest reply frame: a [`DmaEngineOp::Wait`] report.
pub const DMA_ENGINE_MAX_REPLY: usize = REPLY_HEADER_LEN + wait_reply::LEN;

/// Which way a transfer moves data.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DmaDirection {
    /// From the buffer to the device's FIFO.
    MemoryToDevice = 1,
    /// From the device's FIFO into the buffer.
    DeviceToMemory = 2,
}

impl DmaDirection {
    const fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::MemoryToDevice),
            2 => Some(Self::DeviceToMemory),
            _ => None,
        }
    }
}

/// The shape of a cyclic transfer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CyclicParams {
    /// CPU-physical address of the device FIFO, which must lie inside one of
    /// the caller's own register windows.
    pub fifo: u64,
    /// Which way the data moves.
    pub direction: DmaDirection,
    /// Bytes between two period interrupts.
    pub period_bytes: u32,
    /// Periods in the buffer, which the chain loops over.
    pub periods: u32,
}

impl CyclicParams {
    /// Bytes the buffer spans: the periods laid end to end from offset `0`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] for an empty period or buffer, or a buffer
    /// of four GiB or more.
    pub const fn buffer_bytes(&self) -> Result<u32, Errno> {
        if self.period_bytes == 0 || self.periods == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        match self.period_bytes.checked_mul(self.periods) {
            Some(bytes) => Ok(bytes),
            None => Err(Errno::LengthOutOfRange),
        }
    }
}

/// A request to a DMA controller's endpoint.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DmaEngineRequest {
    /// Claim the lowest free channel able to serve the quoted request line,
    /// which the caller must hold as its own grant.
    Open(DmaRequestLine),
    /// Carve `channel`'s buffer and build its cyclic chain.
    Prepare {
        /// The channel [`Open`](Self::Open) returned.
        channel: u8,
        /// The transfer's shape.
        params: CyclicParams,
    },
    /// Start `channel`'s chain from its first period.
    Start {
        /// The channel.
        channel: u8,
    },
    /// Abort `channel`'s chain and reset the channel.
    Stop {
        /// The channel.
        channel: u8,
    },
    /// Read `channel`'s live memory-side offset.
    Position {
        /// The channel.
        channel: u8,
    },
    /// Stop `channel` if it runs, release its buffer, and free it.
    Close {
        /// The channel.
        channel: u8,
    },
    /// Answer at the first period boundary past byte position `after`.
    Wait {
        /// The channel.
        channel: u8,
        /// The monotone byte position already seen.
        after: u64,
    },
}

impl DmaEngineRequest {
    /// The operation this request carries.
    #[must_use]
    pub const fn op(&self) -> DmaEngineOp {
        match self {
            Self::Open(_) => DmaEngineOp::Open,
            Self::Prepare { .. } => DmaEngineOp::Prepare,
            Self::Start { .. } => DmaEngineOp::Start,
            Self::Stop { .. } => DmaEngineOp::Stop,
            Self::Position { .. } => DmaEngineOp::Position,
            Self::Close { .. } => DmaEngineOp::Close,
            Self::Wait { .. } => DmaEngineOp::Wait,
        }
    }

    /// Encode `self` into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] if `out` cannot hold the frame.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Errno> {
        let op = self.op();
        let len = HEADER_LEN + op.request_body_len();
        let frame = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
        frame.fill(0);
        put_u32(frame, 0, DMA_ENGINE_REQUEST_MAGIC);
        put_u16(frame, 4, DMA_ENGINE_VERSION_V1);
        frame[6] = op as u8;
        let body = &mut frame[HEADER_LEN..];
        match self {
            Self::Open(line) => body.copy_from_slice(&HwResource::dma_request(line).to_le_bytes()),
            Self::Prepare { channel, params } => {
                body[prepare::CHANNEL] = *channel;
                body[prepare::DIRECTION] = params.direction as u8;
                put_u32(body, prepare::PERIOD_BYTES, params.period_bytes);
                put_u32(body, prepare::PERIODS, params.periods);
                put_u64(body, prepare::FIFO, params.fifo);
            }
            Self::Start { channel }
            | Self::Stop { channel }
            | Self::Position { channel }
            | Self::Close { channel } => body[channel_body::CHANNEL] = *channel,
            Self::Wait { channel, after } => {
                body[wait::CHANNEL] = *channel;
                put_u64(body, wait::AFTER, *after);
            }
        }
        Ok(len)
    }

    /// Decode a request frame.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — shorter than its operation's frame.
    /// * [`Errno::LengthOutOfRange`] — longer than its operation's frame, or
    ///   an empty or oversized buffer.
    /// * [`Errno::BadMagic`] — a wrong magic, a dirty reserved field, or a
    ///   quoted record that is not a canonical request line.
    /// * [`Errno::AbiVersionUnsupported`] — not [`DMA_ENGINE_VERSION_V1`].
    /// * [`Errno::OutOfRange`] — an unknown operation or direction, or a
    ///   channel past [`DMA_MAX_CHANNELS`].
    pub fn decode(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < HEADER_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != DMA_ENGINE_REQUEST_MAGIC || bytes[7] != 0 {
            return Err(Errno::BadMagic);
        }
        if read_u16(bytes, 4) != DMA_ENGINE_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        let op = DmaEngineOp::from_u8(bytes[6]).ok_or(Errno::OutOfRange)?;
        let body = exact_body(bytes, HEADER_LEN, op.request_body_len())?;
        Ok(match op {
            DmaEngineOp::Open => {
                let record = HwResource::from_bytes(body).map_err(|_| Errno::BadMagic)?;
                Self::Open(record.dma_request_line()?)
            }
            DmaEngineOp::Prepare => decode_prepare(body)?,
            DmaEngineOp::Start => Self::Start {
                channel: decode_channel_body(body, channel_body::LEN)?,
            },
            DmaEngineOp::Stop => Self::Stop {
                channel: decode_channel_body(body, channel_body::LEN)?,
            },
            DmaEngineOp::Position => Self::Position {
                channel: decode_channel_body(body, channel_body::LEN)?,
            },
            DmaEngineOp::Close => Self::Close {
                channel: decode_channel_body(body, channel_body::LEN)?,
            },
            DmaEngineOp::Wait => Self::Wait {
                channel: decode_channel_body(body, wait::AFTER)?,
                after: read_u64(body, wait::AFTER),
            },
        })
    }
}

/// The body of a frame whose header is `header` bytes and whose body must be
/// exactly `len`.
fn exact_body(bytes: &[u8], header: usize, len: usize) -> Result<&[u8], Errno> {
    match bytes.len().cmp(&(header + len)) {
        core::cmp::Ordering::Less => Err(Errno::BufferTooSmall),
        core::cmp::Ordering::Greater => Err(Errno::LengthOutOfRange),
        core::cmp::Ordering::Equal => Ok(&bytes[header..]),
    }
}

/// A channel byte followed by reserved bytes up to `reserved_end`.
fn decode_channel_body(body: &[u8], reserved_end: usize) -> Result<u8, Errno> {
    if body[1..reserved_end].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    checked_channel(body[0])
}

const fn checked_channel(channel: u8) -> Result<u8, Errno> {
    if channel >= DMA_MAX_CHANNELS {
        return Err(Errno::OutOfRange);
    }
    Ok(channel)
}

fn decode_prepare(body: &[u8]) -> Result<DmaEngineRequest, Errno> {
    if read_u16(body, 2) != 0 || read_u32(body, 12) != 0 {
        return Err(Errno::BadMagic);
    }
    let channel = checked_channel(body[prepare::CHANNEL])?;
    let direction = DmaDirection::from_u8(body[prepare::DIRECTION]).ok_or(Errno::OutOfRange)?;
    let params = CyclicParams {
        fifo: read_u64(body, prepare::FIFO),
        direction,
        period_bytes: read_u32(body, prepare::PERIOD_BYTES),
        periods: read_u32(body, prepare::PERIODS),
    };
    params.buffer_bytes()?;
    Ok(DmaEngineRequest::Prepare { channel, params })
}

/// Write a success reply header for `op` and zero its body, returning the
/// body.
fn success_frame(out: &mut [u8], op: DmaEngineOp) -> Result<(&mut [u8], usize), Errno> {
    let len = REPLY_HEADER_LEN + op.reply_body_len();
    let frame = out.get_mut(..len).ok_or(Errno::BufferTooSmall)?;
    frame.fill(0);
    frame[4] = op as u8;
    Ok((&mut frame[REPLY_HEADER_LEN..], len))
}

/// The body of a success reply to `op`, or the refusal it carries.
fn success_body(bytes: &[u8], op: DmaEngineOp) -> Result<&[u8], Errno> {
    if bytes.len() < REPLY_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    match read_i32(bytes, 0) {
        0 => {}
        negative if negative < 0 => {
            return Err(Errno::try_from_status(negative).unwrap_or(Errno::BadMagic));
        }
        _ => return Err(Errno::BadMagic),
    }
    if bytes[4] != op as u8 || bytes[5..REPLY_HEADER_LEN].iter().any(|&b| b != 0) {
        return Err(Errno::BadMagic);
    }
    exact_body(bytes, REPLY_HEADER_LEN, op.reply_body_len())
}

/// Encode a refusal: the status alone, naming no operation.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` cannot hold the reply header.
pub fn encode_error_reply(out: &mut [u8], err: Errno) -> Result<usize, Errno> {
    let frame = out
        .get_mut(..REPLY_HEADER_LEN)
        .ok_or(Errno::BufferTooSmall)?;
    frame.fill(0);
    put_i32(frame, 0, -err.as_i32());
    Ok(REPLY_HEADER_LEN)
}

/// Encode the success reply to an operation that returns nothing
/// ([`DmaEngineOp::Start`], [`DmaEngineOp::Stop`], [`DmaEngineOp::Close`]).
///
/// # Errors
///
/// [`Errno::OutOfRange`] for an operation that returns a body, or
/// [`Errno::BufferTooSmall`] if `out` cannot hold the reply.
pub fn encode_done_reply(out: &mut [u8], op: DmaEngineOp) -> Result<usize, Errno> {
    if op.reply_body_len() != 0 {
        return Err(Errno::OutOfRange);
    }
    success_frame(out, op).map(|(_, len)| len)
}

/// Decode the reply to an operation that returns nothing.
///
/// # Errors
///
/// The refusal the reply carries, [`Errno::OutOfRange`] for an operation
/// that returns a body, or a framing error as [`DmaEngineRequest::decode`].
pub fn decode_done_reply(bytes: &[u8], op: DmaEngineOp) -> Result<(), Errno> {
    if op.reply_body_len() != 0 {
        return Err(Errno::OutOfRange);
    }
    success_body(bytes, op).map(|_| ())
}

/// Encode the reply to [`DmaEngineOp::Open`]: the channel claimed.
///
/// # Errors
///
/// [`Errno::OutOfRange`] for a channel past [`DMA_MAX_CHANNELS`], or
/// [`Errno::BufferTooSmall`].
pub fn encode_open_reply(out: &mut [u8], channel: u8) -> Result<usize, Errno> {
    let channel = checked_channel(channel)?;
    let (body, len) = success_frame(out, DmaEngineOp::Open)?;
    body[open_reply::CHANNEL] = channel;
    Ok(len)
}

/// Decode the reply to [`DmaEngineOp::Open`].
///
/// # Errors
///
/// The refusal the reply carries, [`Errno::OutOfRange`] for a channel past
/// [`DMA_MAX_CHANNELS`], or a framing error.
pub fn decode_open_reply(bytes: &[u8]) -> Result<u8, Errno> {
    let body = success_body(bytes, DmaEngineOp::Open)?;
    decode_channel_body(body, open_reply::LEN)
}

/// Encode the reply to [`DmaEngineOp::Prepare`]: the kernel's handle for the
/// caller's mapping of the buffer.
///
/// # Errors
///
/// [`Errno::OutOfRange`] for the reserved handle `0`, or
/// [`Errno::BufferTooSmall`].
pub fn encode_prepare_reply(out: &mut [u8], grant: u64) -> Result<usize, Errno> {
    if grant == 0 {
        return Err(Errno::OutOfRange);
    }
    let (body, len) = success_frame(out, DmaEngineOp::Prepare)?;
    put_u64(body, prepare_reply::GRANT, grant);
    Ok(len)
}

/// Decode the reply to [`DmaEngineOp::Prepare`].
///
/// # Errors
///
/// The refusal the reply carries, [`Errno::OutOfRange`] for the reserved
/// handle `0`, or a framing error.
pub fn decode_prepare_reply(bytes: &[u8]) -> Result<u64, Errno> {
    let body = success_body(bytes, DmaEngineOp::Prepare)?;
    match read_u64(body, prepare_reply::GRANT) {
        0 => Err(Errno::OutOfRange),
        grant => Ok(grant),
    }
}

/// Encode the reply to [`DmaEngineOp::Position`]: the memory-side offset.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` cannot hold the reply.
pub fn encode_position_reply(out: &mut [u8], offset: u64) -> Result<usize, Errno> {
    let (body, len) = success_frame(out, DmaEngineOp::Position)?;
    put_u64(body, position_reply::OFFSET, offset);
    Ok(len)
}

/// Decode the reply to [`DmaEngineOp::Position`].
///
/// # Errors
///
/// The refusal the reply carries, or a framing error.
pub fn decode_position_reply(bytes: &[u8]) -> Result<u64, Errno> {
    success_body(bytes, DmaEngineOp::Position).map(|body| read_u64(body, position_reply::OFFSET))
}

/// How a [`DmaEngineOp::Wait`] ended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WaitEnd {
    /// A period boundary past the position the wait named.
    Boundary,
    /// The channel stopped first.
    Stopped,
    /// The channel faulted, with the controller's own error bits.
    Faulted(NonZeroU32),
}

impl WaitEnd {
    const BOUNDARY: u8 = 1;
    const STOPPED: u8 = 2;
    const FAULTED: u8 = 3;
}

/// The answer to a [`DmaEngineOp::Wait`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WaitReport {
    /// Why the wait ended.
    pub end: WaitEnd,
    /// Bytes the channel had moved since it started, counted at the event.
    pub position: u64,
    /// The controller's monotonic clock when it serviced the event.
    pub serviced: Duration64,
}

/// Encode the reply to [`DmaEngineOp::Wait`].
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `out` cannot hold the reply.
pub fn encode_wait_reply(out: &mut [u8], report: &WaitReport) -> Result<usize, Errno> {
    let (body, len) = success_frame(out, DmaEngineOp::Wait)?;
    let (end, errors) = match report.end {
        WaitEnd::Boundary => (WaitEnd::BOUNDARY, 0),
        WaitEnd::Stopped => (WaitEnd::STOPPED, 0),
        WaitEnd::Faulted(bits) => (WaitEnd::FAULTED, bits.get()),
    };
    body[wait_reply::END] = end;
    put_u32(body, wait_reply::ERRORS, errors);
    put_u64(body, wait_reply::POSITION, report.position);
    body[wait_reply::SERVICED..].copy_from_slice(&report.serviced.to_le_bytes());
    Ok(len)
}

/// Decode the reply to [`DmaEngineOp::Wait`].
///
/// # Errors
///
/// The refusal the reply carries; [`Errno::OutOfRange`] for an unknown end;
/// [`Errno::BadMagic`] for error bits on an end that carries none, none on a
/// fault, a dirty reserved field, or a non-canonical clock reading; or a
/// framing error.
pub fn decode_wait_reply(bytes: &[u8]) -> Result<WaitReport, Errno> {
    let body = success_body(bytes, DmaEngineOp::Wait)?;
    if body[wait_reply::END + 1..wait_reply::ERRORS]
        .iter()
        .any(|&b| b != 0)
    {
        return Err(Errno::BadMagic);
    }
    let errors = read_u32(body, wait_reply::ERRORS);
    let end = match (body[wait_reply::END], NonZeroU32::new(errors)) {
        (WaitEnd::BOUNDARY, None) => WaitEnd::Boundary,
        (WaitEnd::STOPPED, None) => WaitEnd::Stopped,
        (WaitEnd::FAULTED, Some(bits)) => WaitEnd::Faulted(bits),
        (WaitEnd::BOUNDARY | WaitEnd::STOPPED | WaitEnd::FAULTED, _) => {
            return Err(Errno::BadMagic)
        }
        _ => return Err(Errno::OutOfRange),
    };
    let serviced =
        Duration64::from_bytes(&body[wait_reply::SERVICED..]).map_err(|_| Errno::BadMagic)?;
    Ok(WaitReport {
        end,
        position: read_u64(body, wait_reply::POSITION),
        serviced,
    })
}

#[cfg(test)]
#[path = "dmaengine_tests.rs"]
mod tests;
