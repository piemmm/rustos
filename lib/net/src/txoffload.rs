//! Transmit-offload support a NIC driver needs above the shared-memory ring
//! (`plans/NETWORK.md` N18): the driver-side split of a
//! [`FrameOffload::TxSegment`] super-frame, and the one frame accessor a
//! transmit-checksum engine that must be told the transport reads.
//!
//! A NIC that negotiates
//! [`NetOffloads::TX_SEGMENT_TCP`](tairix_abi::driver::net::NetOffloads::TX_SEGMENT_TCP)
//! is handed one over-MTU TCP segment and must put MTU-sized packets on the
//! wire. Where the silicon has a segmentation engine the driver forwards the
//! request (the `virtio-net` path); where it has none — the Broadcom GENET,
//! and the same case Linux's `net/core/tso.c` serves for `mvneta`, `mvpp2`,
//! and `fec` — the driver splits the super-frame itself and still gets the
//! win the offload exists for: one ring slot and one stack transmit path for
//! tens of wire packets.
//!
//! The arithmetic is device-independent, so it lives here once rather than in
//! each such driver.
//!
//! # What a segment carries
//!
//! Each emitted frame is the super-frame's `hdr_len`-byte header followed by
//! at most `gso_size` payload bytes, with the header fixed up per segment:
//! the IPv4 total length and identification (or the IPv6 payload length), the
//! IPv4 header checksum, the TCP sequence number, `FIN`/`PSH` only on the
//! last segment and `CWR` only on the first (RFC 3168 §6.1.2), and the TCP
//! checksum field advanced from the super-frame's length-zero pseudo-header
//! partial to this segment's own.
//!
//! The transport checksum is left **partial**: the caller completes it, which
//! for its intended consumer means handing each segment to the device's
//! transmit-checksum engine — the offload the segmenter's
//! [`checksum_offload`](TcpSegmenter::checksum_offload) names. The IPv4
//! header checksum is not offloadable that way and is computed here.
//!
//! # Security
//!
//! The super-frame arrives over a shared-memory ring, so every field is
//! re-validated against the frame length before a byte is read or written and
//! a frame that does not describe itself consistently is refused whole
//! ([`GsoError`]) rather than partially transmitted.

use tairix_abi::driver::net_ring::FrameOffload;

use crate::checksum::{internet_checksum, Checksum};
use crate::eth::ETHERNET_HEADER_LEN;
use crate::ipv4::{IPV4_HEADER_LEN, IPV4_MAX_HEADER_LEN};
use crate::ipv6::IPV6_HEADER_LEN;
use crate::tcp::{TcpFlags, CHECKSUM_OFFSET, TCP_HEADER_LEN};

/// Byte offset of the total-length field within an IPv4 header.
const IPV4_TOTAL_LENGTH: usize = 2;
/// Byte offset of the identification field within an IPv4 header.
const IPV4_IDENTIFICATION: usize = 4;
/// Byte offset of the header-checksum field within an IPv4 header.
const IPV4_CHECKSUM: usize = 10;
/// Byte offset of the payload-length field within an IPv6 header.
const IPV6_PAYLOAD_LENGTH: usize = 4;
/// Byte offset of the sequence number within a TCP header.
const TCP_SEQ: usize = 4;
/// Byte offset of the data-offset nibble within a TCP header.
const TCP_DATA_OFFSET: usize = 12;
/// Byte offset of the control-bit byte within a TCP header.
const TCP_FLAGS: usize = 13;

/// Why a super-frame could not be segmented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GsoError {
    /// The offload was not [`FrameOffload::TxSegment`], so there is nothing
    /// to segment.
    NotSegmented,
    /// A length, offset, or header field is inconsistent with the frame: the
    /// header does not fit, the checksum field falls outside it, the IP
    /// version or header length disagrees with `csum_start`, the TCP data
    /// offset disagrees with `hdr_len`, or `gso_size` is zero.
    Malformed,
    /// The destination buffer cannot hold the next segment.
    BufferTooSmall,
}

/// Splits one [`FrameOffload::TxSegment`] super-frame into wire segments.
///
/// Construct with [`TcpSegmenter::new`], which validates the whole frame, then
/// call [`next_segment`](TcpSegmenter::next_segment) until it yields `None`.
/// Nothing is allocated: each segment is written into a caller-supplied
/// buffer, which for a NIC is the transmit slot's own DMA buffer.
#[derive(Debug)]
pub struct TcpSegmenter<'a> {
    frame: &'a [u8],
    csum_start: usize,
    csum_offset: usize,
    hdr_len: usize,
    gso_size: usize,
    ipv6: bool,
    /// The sequence number of the super-frame's first payload byte.
    base_seq: u32,
    /// The IPv4 identification of the super-frame; each segment takes the
    /// next value, as a hardware segmentation engine does.
    base_ident: u16,
    /// The super-frame's control bits, from which each segment's are masked.
    flags: TcpFlags,
    /// The length-zero pseudo-header partial the stack left in the checksum
    /// field, to which each segment adds its own transport length.
    pseudo_partial: u16,
    /// Payload bytes already emitted.
    emitted: usize,
    /// Every segment has been emitted. Tracked separately from `emitted`
    /// because a payload-free super-frame still produces one segment.
    finished: bool,
}

impl<'a> TcpSegmenter<'a> {
    /// Validate `frame` against `offload` and prepare to segment it.
    ///
    /// # Errors
    ///
    /// [`GsoError::NotSegmented`] for any other offload, or
    /// [`GsoError::Malformed`] when the frame does not describe itself
    /// consistently (fail closed — nothing is transmitted).
    pub fn new(frame: &'a [u8], offload: FrameOffload) -> Result<Self, GsoError> {
        Self::resume(frame, offload, 0)
    }

    /// As [`new`](Self::new), but with the first `emitted` payload bytes
    /// already on the wire.
    ///
    /// A driver whose transmit ring fills part-way through a split keeps the
    /// super-frame and picks it up at its next doorbell, rather than dropping
    /// the segments it had no room for. `emitted` must name a segment
    /// boundary — a multiple of `gso_size` no larger than the payload — so
    /// the resumed split is the same one the interrupted run was making.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), plus [`GsoError::Malformed`] for an `emitted`
    /// that is not such a boundary.
    pub fn resume(
        frame: &'a [u8],
        offload: FrameOffload,
        emitted: usize,
    ) -> Result<Self, GsoError> {
        let FrameOffload::TxSegment {
            csum_start,
            csum_offset,
            gso_size,
            hdr_len,
            ipv6,
        } = offload
        else {
            return Err(GsoError::NotSegmented);
        };
        let csum_start = usize::from(csum_start);
        let csum_offset = usize::from(csum_offset);
        let hdr_len = usize::from(hdr_len);
        let gso_size = usize::from(gso_size);
        if gso_size == 0 || hdr_len > frame.len() {
            return Err(GsoError::Malformed);
        }
        // The TCP header must start past the link header, hold its checksum
        // field, and end exactly where the payload begins.
        let tcp_header_len = hdr_len
            .checked_sub(csum_start)
            .filter(|len| (TCP_HEADER_LEN..=crate::tcp::MAX_HEADER_LEN).contains(len))
            .ok_or(GsoError::Malformed)?;
        // The checksum field sits at TCP's one fixed position; a
        // super-frame naming another is not the frame it claims to be.
        if csum_start < ETHERNET_HEADER_LEN || csum_offset != CHECKSUM_OFFSET {
            return Err(GsoError::Malformed);
        }
        // A resume point must be a segment boundary: the payload split, the
        // sequence numbers, and the per-segment identification are all
        // derived from it.
        if emitted > frame.len() - hdr_len || !emitted.is_multiple_of(gso_size) {
            return Err(GsoError::Malformed);
        }
        let ip_header_len = csum_start - ETHERNET_HEADER_LEN;
        let header = frame.get(..hdr_len).ok_or(GsoError::Malformed)?;
        check_ip_header(header, ip_header_len, ipv6)?;
        // The 4-bit data offset counts 32-bit words and must name exactly the
        // header the offload declared, so the payload split is unambiguous.
        let data_offset = *header
            .get(csum_start + TCP_DATA_OFFSET)
            .ok_or(GsoError::Malformed)?;
        if usize::from(data_offset >> 4) * 4 != tcp_header_len {
            return Err(GsoError::Malformed);
        }
        let flags = *header
            .get(csum_start + TCP_FLAGS)
            .ok_or(GsoError::Malformed)?;
        Ok(Self {
            frame,
            csum_start,
            csum_offset,
            hdr_len,
            gso_size,
            ipv6,
            base_seq: read_u32(header, csum_start + TCP_SEQ),
            base_ident: if ipv6 {
                0
            } else {
                read_u16(header, ETHERNET_HEADER_LEN + IPV4_IDENTIFICATION)
            },
            flags: TcpFlags::from_bits(flags),
            pseudo_partial: read_u16(header, csum_start + csum_offset),
            emitted,
            finished: emitted > 0 && emitted >= frame.len() - hdr_len,
        })
    }

    /// Payload bytes already emitted — the resume point a caller stores when
    /// it must stop part-way.
    #[must_use]
    pub fn emitted(&self) -> usize {
        self.emitted
    }

    /// Segments the frame will produce (at least one, even for a payload
    /// that already fits `gso_size`).
    #[must_use]
    pub fn segment_count(&self) -> usize {
        let payload = self.frame.len() - self.hdr_len;
        payload.div_ceil(self.gso_size).max(1)
    }

    /// Largest segment [`next_segment`](Self::next_segment) can write, so a
    /// caller can check its buffer once rather than per segment.
    #[must_use]
    pub fn max_segment_len(&self) -> usize {
        self.hdr_len + self.gso_size
    }

    /// The offload each emitted segment carries: its transport checksum
    /// holds only the pseudo-header partial and must be completed over the
    /// bytes from `csum_start`.
    #[must_use]
    pub fn checksum_offload(&self) -> FrameOffload {
        // Both fit `u16` by construction: they came from one.
        FrameOffload::TxChecksum {
            csum_start: u16::try_from(self.csum_start).unwrap_or(u16::MAX),
            csum_offset: u16::try_from(self.csum_offset).unwrap_or(u16::MAX),
        }
    }

    /// Write the next segment into `out`, returning its length, or `None`
    /// once every payload byte has been emitted.
    ///
    /// # Errors
    ///
    /// [`GsoError::BufferTooSmall`] when `out` is shorter than the segment;
    /// the segmenter is left unadvanced, so nothing is lost.
    pub fn next_segment(&mut self, out: &mut [u8]) -> Result<Option<usize>, GsoError> {
        if self.finished {
            return Ok(None);
        }
        let payload_len = self.frame.len() - self.hdr_len;
        let take = self.gso_size.min(payload_len - self.emitted);
        let total = self.hdr_len + take;
        let out = out.get_mut(..total).ok_or(GsoError::BufferTooSmall)?;
        let start = self.hdr_len + self.emitted;
        out[..self.hdr_len].copy_from_slice(&self.frame[..self.hdr_len]);
        out[self.hdr_len..].copy_from_slice(&self.frame[start..start + take]);

        let first = self.emitted == 0;
        let last = self.emitted + take >= payload_len;
        let tcp_len = total - self.csum_start;
        self.fix_ip_header(out, tcp_len);
        self.fix_tcp_header(out, first, last, tcp_len);

        self.emitted += take;
        self.finished = self.emitted >= payload_len;
        Ok(Some(total))
    }

    /// Retarget the IP header at this segment's own length: the IPv4 total
    /// length, its per-segment identification, and its header checksum, or
    /// the IPv6 payload length.
    fn fix_ip_header(&self, out: &mut [u8], tcp_len: usize) {
        let ip = ETHERNET_HEADER_LEN;
        let ip_header_len = self.csum_start - ip;
        // Bounded by `gso_size` + the header, both `u16`-derived.
        let ip_payload = u16::try_from(tcp_len).unwrap_or(u16::MAX);
        if self.ipv6 {
            out[ip + IPV6_PAYLOAD_LENGTH..ip + IPV6_PAYLOAD_LENGTH + 2]
                .copy_from_slice(&ip_payload.to_be_bytes());
            return;
        }
        let total = ip_payload.saturating_add(u16::try_from(ip_header_len).unwrap_or(u16::MAX));
        out[ip + IPV4_TOTAL_LENGTH..ip + IPV4_TOTAL_LENGTH + 2]
            .copy_from_slice(&total.to_be_bytes());
        // A distinct identification per segment, exactly as a hardware
        // segmentation engine emits.
        let ident = self
            .base_ident
            .wrapping_add(u16::try_from(self.emitted / self.gso_size).unwrap_or(0));
        out[ip + IPV4_IDENTIFICATION..ip + IPV4_IDENTIFICATION + 2]
            .copy_from_slice(&ident.to_be_bytes());
        out[ip + IPV4_CHECKSUM..ip + IPV4_CHECKSUM + 2].copy_from_slice(&[0, 0]);
        let header_sum = internet_checksum(&out[ip..ip + ip_header_len]);
        out[ip + IPV4_CHECKSUM..ip + IPV4_CHECKSUM + 2].copy_from_slice(&header_sum.to_be_bytes());
    }

    /// Advance the TCP sequence number, mask the control bits that belong to
    /// one end of the run, and complete the pseudo-header partial with this
    /// segment's transport length.
    fn fix_tcp_header(&self, out: &mut [u8], first: bool, last: bool, tcp_len: usize) {
        let tcp = self.csum_start;
        // `emitted` is bounded by the frame length, so the widening is exact.
        let seq = self
            .base_seq
            .wrapping_add(u32::try_from(self.emitted).unwrap_or(0));
        out[tcp + TCP_SEQ..tcp + TCP_SEQ + 4].copy_from_slice(&seq.to_be_bytes());

        // Ending the stream, and pushing it, belong to the final segment;
        // the congestion-window-reduced signal belongs to the first.
        let mut flags = self.flags.bits();
        if !last {
            flags &= !(TcpFlags::FIN.bits() | TcpFlags::PSH.bits());
        }
        if !first {
            flags &= !TcpFlags::CWR.bits();
        }
        out[tcp + TCP_FLAGS] = flags;

        let mut sum = Checksum::new();
        sum.push(&self.pseudo_partial.to_be_bytes());
        sum.push(&u16::try_from(tcp_len).unwrap_or(u16::MAX).to_be_bytes());
        let partial = sum.partial();
        out[tcp + self.csum_offset..tcp + self.csum_offset + 2]
            .copy_from_slice(&partial.to_be_bytes());
    }
}

/// Refuse a header whose IP version or length disagrees with the offload's
/// `csum_start` — the offsets every later write is derived from.
fn check_ip_header(header: &[u8], ip_header_len: usize, ipv6: bool) -> Result<(), GsoError> {
    let first = *header.get(ETHERNET_HEADER_LEN).ok_or(GsoError::Malformed)?;
    if ipv6 {
        if first >> 4 != 6 || ip_header_len != IPV6_HEADER_LEN {
            return Err(GsoError::Malformed);
        }
        return Ok(());
    }
    if first >> 4 != 4 || !(IPV4_HEADER_LEN..=IPV4_MAX_HEADER_LEN).contains(&ip_header_len) {
        return Err(GsoError::Malformed);
    }
    // The IHL nibble counts 32-bit words and must name the same header the
    // offload's `csum_start` implies.
    if usize::from(first & 0x0F) * 4 != ip_header_len {
        return Err(GsoError::Malformed);
    }
    Ok(())
}

/// Read a big-endian `u16` at `off`, whose range the caller has checked.
fn read_u16(bytes: &[u8], off: usize) -> u16 {
    match bytes.get(off..off + 2) {
        Some(&[high, low]) => u16::from_be_bytes([high, low]),
        _ => 0,
    }
}

/// Read a big-endian `u32` at `off`, whose range the caller has checked.
fn read_u32(bytes: &[u8], off: usize) -> u32 {
    match bytes.get(off..off + 4) {
        Some(&[a, b, c, d]) => u32::from_be_bytes([a, b, c, d]),
        _ => 0,
    }
}

/// Byte offset of the protocol field within an IPv4 header.
const IPV4_PROTOCOL: usize = 9;
/// Byte offset of the next-header field within an IPv6 header.
const IPV6_NEXT_HEADER: usize = 6;

/// The IP protocol / next-header value the Ethernet `frame`'s IP header
/// declares, or [`None`] when the frame is not IP or is too short to say.
///
/// A transmit-checksum engine that must be told the transport reads it here
/// rather than re-deriving the header layout: the GENET applies RFC 768's
/// "a computed zero is sent as `0xFFFF`" rule only when told the datagram is
/// UDP, and getting that wrong would silently disable a UDP checksum.
///
/// The value is the transport's only for a frame whose transport header
/// follows the fixed IP header directly — which is exactly the frame a
/// transmit-checksum offload is attached to, since the stack derives its
/// `csum_start` the same way. An IPv6 frame carrying extension headers
/// reports the first of them, and such a frame never carries the offload.
#[must_use]
pub fn transport_protocol(frame: &[u8]) -> Option<u8> {
    let ethertype = u16::from_be_bytes([*frame.get(12)?, *frame.get(13)?]);
    match ethertype {
        crate::eth::ETHERTYPE_IPV4 => frame.get(ETHERNET_HEADER_LEN + IPV4_PROTOCOL).copied(),
        crate::eth::ETHERTYPE_IPV6 => frame.get(ETHERNET_HEADER_LEN + IPV6_NEXT_HEADER).copied(),
        _ => None,
    }
}

#[cfg(test)]
#[path = "txoffload_tests.rs"]
mod tests;
