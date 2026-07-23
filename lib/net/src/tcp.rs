//! TCP (RFC 9293) segment codec and sequence-space arithmetic.
//!
//! This module is the pure, dual-stack wire layer the TCP state machine
//! (a later increment) is built on: the segment header and its options,
//! the checksum over the family-appropriate pseudo-header
//! ([`crate::checksum::Pseudo`]), and the modulo-2³² sequence arithmetic
//! ([`SeqNumber`]) every window and acknowledgement comparison uses.
//!
//! Like every other decoder in this crate it is total (never panics, for
//! any bytes), bounded (fixed validation bounds — [`MAX_SACK_BLOCKS`],
//! the 60-byte header ceiling — never attacker-sized work), and
//! fail-closed: a truncated header, an out-of-range data offset, a
//! malformed option, or a checksum mismatch rejects the whole segment
//! (`None`); nothing partial is surfaced. TCP checksums are mandatory in
//! both families (RFC 9293 §3.1, RFC 8200 §8.1): a segment whose checksum
//! does not verify is rejected, and there is no all-zero "no checksum"
//! sentinel (unlike UDP over IPv4).

use crate::checksum::{ChecksumCheck, Pseudo};

#[path = "tcp_conn.rs"]
pub mod conn;

#[path = "tcp_cc.rs"]
pub mod cc;

#[path = "tcp_listen.rs"]
pub mod listen;

/// IP protocol number (IPv4) and next-header value (IPv6) for TCP.
pub const PROTOCOL_TCP: u8 = 6;

/// Length of the fixed TCP header, before any options.
pub const TCP_HEADER_LEN: usize = 20;

/// Smallest legal value of the 4-bit data-offset field, in 32-bit words
/// (a header with no options).
const DATA_OFFSET_MIN_WORDS: u8 = 5;

/// Largest value the 4-bit data-offset field can encode, in 32-bit words.
const DATA_OFFSET_MAX_WORDS: u8 = 15;

/// The largest a TCP header (fixed header plus options) can be, from the
/// 4-bit data-offset field: `DATA_OFFSET_MAX_WORDS` (15) words × 4
/// bytes. The `max_header_len_matches_data_offset` test pins this to the
/// word bound so the two cannot drift.
pub const MAX_HEADER_LEN: usize = 60;

/// The largest options region a segment can carry (the header ceiling
/// less the fixed header). A fixed security bound, defined once.
pub const MAX_OPTIONS_LEN: usize = MAX_HEADER_LEN - TCP_HEADER_LEN;

/// The maximum number of SACK blocks a single segment may carry
/// (RFC 2018 §3). A fixed validation bound: a segment claiming more is
/// rejected rather than allocating for an attacker-chosen count.
pub const MAX_SACK_BLOCKS: usize = 4;

/// Half the sequence space (2³¹). A sequence number's forward gap to
/// another lands below this iff it is at or after the other in the
/// modular ordering.
const SEQ_HALF: u32 = 0x8000_0000;

/// A TCP sequence or acknowledgement number: a point in the 32-bit
/// sequence space with wrapping arithmetic and the RFC 1982 / RFC 9293
/// §3.4 modular ordering.
///
/// Sequence space is cyclic, so there is no total order: `<` is defined
/// only for numbers within 2³¹ of each other (which is all TCP ever
/// compares). The comparison helpers implement that windowed ordering;
/// `Ord`/`PartialOrd` are deliberately **not** derived so a caller can
/// never accidentally use a linear comparison on a wrapping value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SeqNumber(u32);

impl SeqNumber {
    /// The sequence number with raw wire value `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw 32-bit wire value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// `self + n`, wrapping through the 32-bit sequence space.
    #[must_use]
    pub const fn add(self, n: u32) -> Self {
        Self(self.0.wrapping_add(n))
    }

    /// `self - n`, wrapping through the 32-bit sequence space.
    #[must_use]
    pub const fn sub(self, n: u32) -> Self {
        Self(self.0.wrapping_sub(n))
    }

    /// The forward distance from `earlier` to `self` (`self - earlier`)
    /// as an unsigned count of sequence numbers, wrapping.
    ///
    /// Meaningful when `self` is known to be at or ahead of `earlier`
    /// within a window (the usual TCP case: bytes acknowledged, bytes in
    /// flight). For an unordered pair use [`Self::lt`] and friends.
    #[must_use]
    pub const fn distance_from(self, earlier: Self) -> u32 {
        self.0.wrapping_sub(earlier.0)
    }

    /// Modular "before": `self` precedes `other` in sequence space.
    ///
    /// Defined via the unsigned forward gap `self - other`: `self` is
    /// before `other` exactly when that gap lands in the upper half of
    /// the space (equivalently, the signed offset is negative), which
    /// avoids a `u32`-to-`i32` reinterpretation.
    #[must_use]
    pub const fn lt(self, other: Self) -> bool {
        self.0.wrapping_sub(other.0) >= SEQ_HALF
    }

    /// Modular "before or equal".
    #[must_use]
    pub const fn le(self, other: Self) -> bool {
        let gap = self.0.wrapping_sub(other.0);
        gap == 0 || gap >= SEQ_HALF
    }

    /// Modular "after": `self` follows `other` in sequence space.
    #[must_use]
    pub const fn gt(self, other: Self) -> bool {
        let gap = self.0.wrapping_sub(other.0);
        gap != 0 && gap < SEQ_HALF
    }

    /// Modular "after or equal".
    #[must_use]
    pub const fn ge(self, other: Self) -> bool {
        self.0.wrapping_sub(other.0) < SEQ_HALF
    }

    /// Whether `self` lies in the half-open window `[start, start + len)`
    /// (mod 2³²) — the RFC 9293 §3.4 receive-window acceptance test.
    ///
    /// An empty window (`len == 0`) contains nothing.
    #[must_use]
    pub const fn in_window(self, start: Self, len: u32) -> bool {
        self.distance_from(start) < len
    }
}

/// The 8 control bits of the TCP header (byte 13). The reserved bits and
/// the historic NS bit (byte 12 low nibble) are ignored on receive and
/// zero on emit (RFC 9293 §3.1).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpFlags(u8);

const FLAG_FIN: u8 = 0x01;
const FLAG_SYN: u8 = 0x02;
const FLAG_RST: u8 = 0x04;
const FLAG_PSH: u8 = 0x08;
const FLAG_ACK: u8 = 0x10;
const FLAG_URG: u8 = 0x20;
const FLAG_ECE: u8 = 0x40;
const FLAG_CWR: u8 = 0x80;

impl TcpFlags {
    /// The FIN control bit.
    pub const FIN: Self = Self(FLAG_FIN);
    /// The SYN control bit.
    pub const SYN: Self = Self(FLAG_SYN);
    /// The RST control bit.
    pub const RST: Self = Self(FLAG_RST);
    /// The PSH control bit.
    pub const PSH: Self = Self(FLAG_PSH);
    /// The ACK control bit.
    pub const ACK: Self = Self(FLAG_ACK);
    /// The URG control bit.
    pub const URG: Self = Self(FLAG_URG);
    /// The ECE control bit.
    pub const ECE: Self = Self(FLAG_ECE);
    /// The CWR control bit.
    pub const CWR: Self = Self(FLAG_CWR);

    /// No control bits set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The flags encoded by raw byte `bits`.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The raw control-bit byte.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit set in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The SYN bit is set.
    #[must_use]
    pub const fn syn(self) -> bool {
        self.contains(Self::SYN)
    }

    /// The ACK bit is set.
    #[must_use]
    pub const fn ack(self) -> bool {
        self.contains(Self::ACK)
    }

    /// The FIN bit is set.
    #[must_use]
    pub const fn fin(self) -> bool {
        self.contains(Self::FIN)
    }

    /// The RST bit is set.
    #[must_use]
    pub const fn rst(self) -> bool {
        self.contains(Self::RST)
    }

    /// The PSH bit is set.
    #[must_use]
    pub const fn psh(self) -> bool {
        self.contains(Self::PSH)
    }

    /// The URG bit is set.
    #[must_use]
    pub const fn urg(self) -> bool {
        self.contains(Self::URG)
    }

    /// The ECE bit is set.
    #[must_use]
    pub const fn ece(self) -> bool {
        self.contains(Self::ECE)
    }

    /// The CWR bit is set.
    #[must_use]
    pub const fn cwr(self) -> bool {
        self.contains(Self::CWR)
    }
}

impl core::ops::BitOr for TcpFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// RFC 7323 timestamps option payload: the sender's timestamp and the
/// most recent timestamp it echoes back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamps {
    /// `TSval` — the sender's current timestamp clock.
    pub value: u32,
    /// `TSecr` — the timestamp being echoed (meaningful only when ACK is
    /// set; zero otherwise).
    pub echo: u32,
}

/// One RFC 2018 selective-acknowledgement block: a contiguous range of
/// received sequence space, `[left, right)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SackBlock {
    /// The first sequence number of the block.
    pub left: SeqNumber,
    /// The sequence number just past the block's last byte.
    pub right: SeqNumber,
}

impl SackBlock {
    /// A zero block, used only to initialise the fixed-size store.
    const ZERO: Self = Self {
        left: SeqNumber::new(0),
        right: SeqNumber::new(0),
    };
}

/// The recognised TCP options of one segment, parsed into typed fields.
///
/// Unrecognised (but well-formed) options are skipped on parse; a
/// malformed option — a bad length, a body of the wrong size, or a SACK
/// block count over [`MAX_SACK_BLOCKS`] — rejects the whole segment.
/// The SACK blocks are held in a fixed-size store (never an
/// attacker-sized allocation); read them with [`TcpOptions::sack`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpOptions {
    /// Maximum segment size (RFC 9293 §3.2), a SYN-only option.
    pub mss: Option<u16>,
    /// Window scale shift count (RFC 7323 §2), a SYN-only option. The raw
    /// value is surfaced; the RFC 7323 §2.3 clamp to 14 is the state
    /// machine's policy, not the codec's.
    pub window_scale: Option<u8>,
    /// The SACK-permitted option (RFC 2018 §2) was present (SYN only).
    pub sack_permitted: bool,
    /// The timestamps option (RFC 7323 §3), if present.
    pub timestamps: Option<Timestamps>,
    /// SACK blocks (RFC 2018 §3), held in a fixed store; `sack_count`
    /// bounds the live prefix.
    sack_blocks: [SackBlock; MAX_SACK_BLOCKS],
    /// Number of live entries in `sack_blocks` (`0..=MAX_SACK_BLOCKS`).
    sack_count: usize,
}

impl Default for TcpOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl TcpOptions {
    /// No options.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mss: None,
            window_scale: None,
            sack_permitted: false,
            timestamps: None,
            sack_blocks: [SackBlock::ZERO; MAX_SACK_BLOCKS],
            sack_count: 0,
        }
    }

    /// The live SACK blocks.
    #[must_use]
    pub fn sack(&self) -> &[SackBlock] {
        &self.sack_blocks[..self.sack_count]
    }

    /// Replace the SACK blocks. Returns `false` (leaving `self`
    /// unchanged) when `blocks` exceeds [`MAX_SACK_BLOCKS`].
    #[must_use]
    pub fn set_sack(&mut self, blocks: &[SackBlock]) -> bool {
        if blocks.len() > MAX_SACK_BLOCKS {
            return false;
        }
        for (slot, block) in self.sack_blocks.iter_mut().zip(blocks) {
            *slot = *block;
        }
        self.sack_count = blocks.len();
        true
    }

    /// Whether no options are set (a bare header on emit).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.mss.is_none()
            && self.window_scale.is_none()
            && !self.sack_permitted
            && self.timestamps.is_none()
            && self.sack_count == 0
    }

    /// Bytes these options occupy on the wire once serialised and padded
    /// to a 4-byte boundary — the overhead a segment carrying them adds
    /// beyond the 20-byte fixed header.
    ///
    /// The sender uses this to keep header + options + payload within the
    /// path MTU (RFC 6691): a data segment's payload is the negotiated MSS
    /// minus this overhead, so a full-size segment never overflows the link.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        // Reuse the one serialiser so the length can never disagree with
        // the bytes actually emitted. `encode_options` only fails when the
        // options exceed the 40-byte region, impossible for a self-built
        // set; the ceiling fallback keeps this total and panic-free.
        let mut buf = [0u8; MAX_OPTIONS_LEN];
        encode_options(self, &mut buf).unwrap_or(MAX_OPTIONS_LEN)
    }
}

/// TCP option kind numbers (RFC 9293 §3.2 and the options registry).
const OPT_END: u8 = 0;
const OPT_NOP: u8 = 1;
const OPT_MSS: u8 = 2;
const OPT_WINDOW_SCALE: u8 = 3;
const OPT_SACK_PERMITTED: u8 = 4;
const OPT_SACK: u8 = 5;
const OPT_TIMESTAMPS: u8 = 8;

const OPT_LEN_MSS: u8 = 4;
const OPT_LEN_WINDOW_SCALE: u8 = 3;
const OPT_LEN_SACK_PERMITTED: u8 = 2;
const OPT_LEN_TIMESTAMPS: u8 = 10;

/// Parse the options region (`data offset × 4 − 20` bytes). Returns
/// `None` (rejecting the segment) on any malformed option.
fn parse_options(bytes: &[u8]) -> Option<TcpOptions> {
    let mut opts = TcpOptions::new();
    let mut i = 0;
    while i < bytes.len() {
        let kind = bytes[i];
        if kind == OPT_END {
            break;
        }
        if kind == OPT_NOP {
            i += 1;
            continue;
        }
        // Every other kind is length-prefixed: kind, length, body.
        let len = usize::from(*bytes.get(i + 1)?);
        if len < 2 || i + len > bytes.len() {
            return None;
        }
        let body = &bytes[i + 2..i + len];
        match kind {
            OPT_MSS => {
                let [hi, lo] = body
                    .try_into()
                    .ok()
                    .filter(|_| len == usize::from(OPT_LEN_MSS))?;
                opts.mss = Some(u16::from_be_bytes([hi, lo]));
            }
            OPT_WINDOW_SCALE => {
                if len != usize::from(OPT_LEN_WINDOW_SCALE) {
                    return None;
                }
                opts.window_scale = Some(body[0]);
            }
            OPT_SACK_PERMITTED => {
                if len != usize::from(OPT_LEN_SACK_PERMITTED) {
                    return None;
                }
                opts.sack_permitted = true;
            }
            OPT_SACK => {
                if body.is_empty() || body.len() % 8 != 0 {
                    return None;
                }
                let count = body.len() / 8;
                if count > MAX_SACK_BLOCKS {
                    return None;
                }
                for (slot, chunk) in opts.sack_blocks.iter_mut().zip(body.chunks_exact(8)) {
                    let left = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    let right = u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                    *slot = SackBlock {
                        left: SeqNumber::new(left),
                        right: SeqNumber::new(right),
                    };
                }
                opts.sack_count = count;
            }
            OPT_TIMESTAMPS => {
                if len != usize::from(OPT_LEN_TIMESTAMPS) {
                    return None;
                }
                let value = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let echo = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                opts.timestamps = Some(Timestamps { value, echo });
            }
            // A well-formed but unrecognised option is skipped.
            _ => {}
        }
        i += len;
    }
    Some(opts)
}

/// A parsed TCP segment: the header fields, the recognised options, and
/// the payload borrowed from the input buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpSegment<'a> {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub destination_port: u16,
    /// Sequence number of the first payload byte (or of the SYN/FIN
    /// control when it occupies sequence space).
    pub seq: SeqNumber,
    /// Acknowledgement number (meaningful only when [`TcpFlags::ack`]).
    pub ack: SeqNumber,
    /// The control bits.
    pub flags: TcpFlags,
    /// Advertised receive window (unscaled; scaling is the state
    /// machine's job with the negotiated [`TcpOptions::window_scale`]).
    pub window: u16,
    /// Urgent pointer (meaningful only when [`TcpFlags::urg`]).
    pub urgent: u16,
    /// The recognised options.
    pub options: TcpOptions,
    /// The segment payload (the bytes past the header).
    pub payload: &'a [u8],
}

impl<'a> TcpSegment<'a> {
    /// Parse and checksum-verify a TCP segment carried in `bytes` under
    /// the `pseudo` addressing context. `bytes` must be exactly the TCP
    /// segment (the network layer trims to the IP payload length).
    ///
    /// Returns `None` (fail closed) for a truncated header, a data-offset
    /// field outside `5..=15` words, a header longer than `bytes`, a
    /// segment longer than the 16-bit pseudo-header length can express, a
    /// malformed option, or a checksum that does not verify. TCP
    /// checksums are mandatory in both families, so there is no accepted
    /// zero-checksum form.
    #[must_use]
    pub fn parse(pseudo: Pseudo, bytes: &'a [u8]) -> Option<Self> {
        Self::parse_with(pseudo, bytes, ChecksumCheck::Verify)
    }

    /// Parse a TCP segment under `pseudo`, verifying the mandatory
    /// checksum in software unless `check` reports the device already
    /// validated it ([`ChecksumCheck::DeviceValidated`], the negotiated
    /// receive checksum offload — `plans/NETWORK.md` §2.3).
    ///
    /// The offload suppresses **only** the one's-complement fold; every
    /// other validation still runs, including the data-offset range and
    /// the pseudo-header length bound (a segment too long to express in
    /// the 16-bit length is rejected regardless of offload). Trust is in
    /// the device, never the peer; the offload is never load-bearing for
    /// security.
    #[must_use]
    pub fn parse_with(pseudo: Pseudo, bytes: &'a [u8], check: ChecksumCheck) -> Option<Self> {
        let fixed = bytes.get(..TCP_HEADER_LEN)?;
        let source_port = u16::from_be_bytes([fixed[0], fixed[1]]);
        let destination_port = u16::from_be_bytes([fixed[2], fixed[3]]);
        let seq = SeqNumber::new(u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]));
        let ack = SeqNumber::new(u32::from_be_bytes([
            fixed[8], fixed[9], fixed[10], fixed[11],
        ]));
        let data_offset_words = fixed[12] >> 4;
        if !(DATA_OFFSET_MIN_WORDS..=DATA_OFFSET_MAX_WORDS).contains(&data_offset_words) {
            return None;
        }
        let header_len = usize::from(data_offset_words) * 4;
        let flags = TcpFlags::from_bits(fixed[13]);
        let window = u16::from_be_bytes([fixed[14], fixed[15]]);
        let urgent = u16::from_be_bytes([fixed[18], fixed[19]]);
        if bytes.len() < header_len {
            return None;
        }
        // The pseudo-header carries the TCP length (header + data). A
        // segment too long to express there cannot bear a valid checksum;
        // this length bound is a semantic check that holds under offload.
        let segment_len = u16::try_from(bytes.len()).ok()?;
        if check.must_verify() {
            let mut sum = pseudo.seed(PROTOCOL_TCP, segment_len);
            sum.push(bytes);
            if sum.finish() != 0 {
                return None;
            }
        }
        let options = parse_options(&bytes[TCP_HEADER_LEN..header_len])?;
        Some(Self {
            source_port,
            destination_port,
            seq,
            ack,
            flags,
            window,
            urgent,
            options,
            payload: &bytes[header_len..],
        })
    }
}

/// The header fields of a segment to emit (everything but the payload).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpSegmentMeta {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub destination_port: u16,
    /// Sequence number.
    pub seq: SeqNumber,
    /// Acknowledgement number (set the ACK flag to make it meaningful).
    pub ack: SeqNumber,
    /// Control bits.
    pub flags: TcpFlags,
    /// Advertised (unscaled) receive window.
    pub window: u16,
    /// Urgent pointer (set the URG flag to make it meaningful).
    pub urgent: u16,
    /// Options to serialise.
    pub options: TcpOptions,
}

/// Errors from [`write()`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteError {
    /// The serialised options exceed the 40-byte options region.
    OptionsTooLarge,
    /// The header plus payload exceeds the 16-bit pseudo-header length.
    TooLarge,
    /// `out` is smaller than the header plus payload.
    BufferTooSmall,
}

/// Serialise `options` into `out` (at least [`MAX_OPTIONS_LEN`] bytes),
/// padded with end-of-list bytes to a 4-byte boundary. Returns the
/// padded length, or `None` if the options do not fit the region.
///
/// The options are laid out in a fixed, interoperable order with the
/// customary NOP alignment padding (timestamps and SACK on 4-byte
/// boundaries), so a written segment is byte-deterministic.
fn encode_options(options: &TcpOptions, out: &mut [u8; MAX_OPTIONS_LEN]) -> Option<usize> {
    fn put(out: &mut [u8; MAX_OPTIONS_LEN], n: &mut usize, bytes: &[u8]) -> Option<()> {
        let end = n.checked_add(bytes.len())?;
        out.get_mut(*n..end)?.copy_from_slice(bytes);
        *n = end;
        Some(())
    }
    let mut n = 0usize;
    if let Some(mss) = options.mss {
        let [hi, lo] = mss.to_be_bytes();
        put(out, &mut n, &[OPT_MSS, OPT_LEN_MSS, hi, lo])?;
    }
    if options.sack_permitted {
        put(out, &mut n, &[OPT_SACK_PERMITTED, OPT_LEN_SACK_PERMITTED])?;
    }
    if let Some(ts) = options.timestamps {
        // Two NOPs align the 10-byte option onto a 4-byte boundary.
        put(
            out,
            &mut n,
            &[OPT_NOP, OPT_NOP, OPT_TIMESTAMPS, OPT_LEN_TIMESTAMPS],
        )?;
        put(out, &mut n, &ts.value.to_be_bytes())?;
        put(out, &mut n, &ts.echo.to_be_bytes())?;
    }
    if let Some(shift) = options.window_scale {
        put(
            out,
            &mut n,
            &[OPT_NOP, OPT_WINDOW_SCALE, OPT_LEN_WINDOW_SCALE, shift],
        )?;
    }
    let sack = options.sack();
    if !sack.is_empty() {
        // 2 header bytes + 8 per block; bounded by MAX_SACK_BLOCKS, so
        // the length always fits the option length byte.
        let opt_len = u8::try_from(2 + sack.len() * 8).ok()?;
        put(out, &mut n, &[OPT_NOP, OPT_NOP, OPT_SACK, opt_len])?;
        for block in sack {
            put(out, &mut n, &block.left.value().to_be_bytes())?;
            put(out, &mut n, &block.right.value().to_be_bytes())?;
        }
    }
    // Pad with end-of-option-list bytes to a 4-byte boundary.
    while n % 4 != 0 {
        put(out, &mut n, &[OPT_END])?;
    }
    Some(n)
}

/// Write a TCP segment — header, options, then `payload` — into `out`,
/// computing the checksum over `pseudo`. Returns the number of bytes
/// written.
///
/// # Errors
///
/// * [`WriteError::OptionsTooLarge`] — the options do not fit 40 bytes.
/// * [`WriteError::TooLarge`] — the segment exceeds the 16-bit length.
/// * [`WriteError::BufferTooSmall`] — `out` cannot hold the segment.
pub fn write(
    pseudo: Pseudo,
    meta: &TcpSegmentMeta,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, WriteError> {
    let mut option_bytes = [0u8; MAX_OPTIONS_LEN];
    let options_len =
        encode_options(&meta.options, &mut option_bytes).ok_or(WriteError::OptionsTooLarge)?;
    let header_len = TCP_HEADER_LEN + options_len;
    // header_len is a multiple of 4 in `20..=60`; the data-offset field
    // counts 32-bit words.
    let data_offset_words =
        u8::try_from(header_len / 4).map_err(|_| WriteError::OptionsTooLarge)?;
    let total = header_len
        .checked_add(payload.len())
        .filter(|&total| u16::try_from(total).is_ok())
        .ok_or(WriteError::TooLarge)?;
    let out = out.get_mut(..total).ok_or(WriteError::BufferTooSmall)?;
    out[0..2].copy_from_slice(&meta.source_port.to_be_bytes());
    out[2..4].copy_from_slice(&meta.destination_port.to_be_bytes());
    out[4..8].copy_from_slice(&meta.seq.value().to_be_bytes());
    out[8..12].copy_from_slice(&meta.ack.value().to_be_bytes());
    out[12] = data_offset_words << 4;
    out[13] = meta.flags.bits();
    out[14..16].copy_from_slice(&meta.window.to_be_bytes());
    out[16..18].copy_from_slice(&[0, 0]);
    out[18..20].copy_from_slice(&meta.urgent.to_be_bytes());
    out[TCP_HEADER_LEN..header_len].copy_from_slice(&option_bytes[..options_len]);
    out[header_len..].copy_from_slice(payload);
    // `total <= u16::MAX` was checked above.
    let segment_len = u16::try_from(total).map_err(|_| WriteError::TooLarge)?;
    let mut sum = pseudo.seed(PROTOCOL_TCP, segment_len);
    sum.push(out);
    let checksum = sum.finish();
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    Ok(total)
}

#[cfg(test)]
#[path = "tcp_tests.rs"]
mod tests;
