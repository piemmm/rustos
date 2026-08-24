//! The on-disk log segment: an append-only, hash-chained, self-verifying
//! container for one stream's records.
//!
//! A segment is the unit TAIRiX writes, seals, rotates, and verifies. It is
//! **payload-agnostic**: the record bytes are opaque here, so the same
//! container serves every record shape. Integrity comes from three layers:
//!
//! * a fixed [`SegmentHeader`], self-checked by a trailing checksum so a
//!   corrupt header is detected before any record is trusted;
//! * per-record framing whose stored link hash chains each record to its
//!   predecessor via the one [`crate::chain`] hash chain — a torn or tampered
//!   record fails the chain and stops a forward scan;
//! * a segment footer written when the segment closes, carrying the record
//!   count, sequence and time bounds, the segment hash over everything before
//!   it, an optional seal MAC (mandatory for audit/security streams), and a
//!   footer checksum.
//!
//! ## Layout
//!
//! ```text
//! SegmentHeader (fixed)
//! RecordBlock*        (append-only, in commit order)
//! SegmentFooter (fixed, present once the segment is closed)
//! ```
//!
//! Every multi-byte scalar is little-endian. All hashing is over *contiguous*
//! byte ranges of the encoded segment, so no streaming hash is needed.

use tairix_abi::{BootId, Duration64, WallClockReading, BOOT_ID_LEN};
use tairix_crypto::{sha256, MacTag, Sha256Digest, MAC_TAG_LEN, SHA256_OUTPUT_LEN};

use crate::attest::LogAttestationKey;
use crate::chain::{ChainedEntry, LogChain};
use crate::stream::Stream;

/// Segment magic (`"RLOGSEG" || 0x01`).
pub const SEGMENT_MAGIC: [u8; 8] = *b"RLOGSEG\x01";

/// On-disk segment format version.
pub const SEGMENT_FORMAT_VERSION: u16 = 1;

/// Largest record payload a segment accepts, in bytes. A security bound
/// (fixed): it caps the per-record buffer a reader must trust from
/// untrusted bytes, independent of any higher-layer record-size limit.
pub const MAX_RECORD_PAYLOAD: usize = 64 * 1024;

/// Leading tag of a committed record block.
const BLOCK_RECORD: u8 = 0x01;
/// Leading tag of the segment footer.
const BLOCK_FOOTER: u8 = 0x02;

/// Fixed size, in bytes, of an encoded [`SegmentHeader`].
pub const SEGMENT_HEADER_LEN: usize = 8   // magic
    + 2   // format_version
    + 1   // stream
    + 1   // reserved
    + 8   // segment_id
    + SHA256_OUTPUT_LEN // machine_id_hash
    + BOOT_ID_LEN
    + 8   // first_seq
    + SHA256_OUTPUT_LEN // prev_segment_hash
    + Duration64::WIRE_LEN
    + WallClockReading::WIRE_LEN
    + SHA256_OUTPUT_LEN; // header_checksum

/// Fixed per-record framing overhead:
/// `tag(1) || payload_len(4) || cpu(4) || seq(8) || entry_hash(32) || monotonic(12)`.
///
/// `monotonic` is the record's own ordering time within the boot (SYSLOG
/// §5.1). It is covered
/// by the segment hash (like `seq`), not folded into the per-record chain link,
/// which binds only the originating CPU and the payload digest.
pub const RECORD_PREFIX_LEN: usize = 1 + 4 + 4 + 8 + SHA256_OUTPUT_LEN + Duration64::WIRE_LEN;

/// Byte length of the footer summary (everything the segment hash covers of
/// the footer).
const FOOTER_SUMMARY_LEN: usize = 1   // tag
    + 8   // record_count
    + 8   // first_seq
    + 8   // last_seq
    + Duration64::WIRE_LEN  // first_monotonic
    + Duration64::WIRE_LEN  // last_monotonic
    + SHA256_OUTPUT_LEN     // prev_segment_hash
    + SHA256_OUTPUT_LEN; // last_record_hash

/// Fixed size, in bytes, of an encoded segment footer.
pub const SEGMENT_FOOTER_LEN: usize = FOOTER_SUMMARY_LEN
    + SHA256_OUTPUT_LEN // segment_hash
    + 1                 // seal_present
    + MAC_TAG_LEN       // seal_tag
    + SHA256_OUTPUT_LEN; // footer_checksum

/// Why a segment operation failed. Every variant is a fail-closed refusal;
/// nothing is partially applied.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SegmentError {
    /// The output buffer cannot hold what the writer was asked to write.
    BufferTooSmall,
    /// A record payload exceeds [`MAX_RECORD_PAYLOAD`].
    PayloadTooLarge,
    /// The stream requires a seal (audit/security) but no key was supplied on
    /// close, or a sealed segment was verified without a key.
    SealKeyRequired,
    /// The bytes are shorter than a structure they must contain.
    Truncated,
    /// The segment magic is wrong.
    BadMagic,
    /// The format version is not supported.
    UnsupportedVersion,
    /// A stored discriminant (stream, tag, seal flag) is out of range.
    BadField,
    /// The header checksum does not match its contents.
    BadHeaderChecksum,
    /// A record's stored link hash does not chain to its predecessor: the
    /// record was tampered with, reordered, or the segment is truncated.
    ChainBroken,
    /// A footer summary field disagrees with the scanned records.
    FooterMismatch,
    /// The recomputed segment hash does not match the stored one.
    SegmentHashMismatch,
    /// The footer checksum does not match its contents.
    BadFooterChecksum,
    /// The seal MAC is missing where required, or does not verify.
    SealInvalid,
    /// A complete, footer-terminated segment was expected but the bytes end
    /// (or tear) before a valid footer.
    MissingFooter,
}

/// A bounds-checked little-endian byte writer over a caller-owned buffer.
struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn at(buf: &'a mut [u8], pos: usize) -> Self {
        Self { buf, pos }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), SegmentError> {
        let end = self
            .pos
            .checked_add(bytes.len())
            .ok_or(SegmentError::BufferTooSmall)?;
        if end > self.buf.len() {
            return Err(SegmentError::BufferTooSmall);
        }
        self.buf[self.pos..end].copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    fn put_u8(&mut self, v: u8) -> Result<(), SegmentError> {
        self.put(&[v])
    }

    fn put_u16(&mut self, v: u16) -> Result<(), SegmentError> {
        self.put(&v.to_le_bytes())
    }

    fn put_u32(&mut self, v: u32) -> Result<(), SegmentError> {
        self.put(&v.to_le_bytes())
    }

    fn put_u64(&mut self, v: u64) -> Result<(), SegmentError> {
        self.put(&v.to_le_bytes())
    }
}

/// A bounds-checked little-endian byte reader over an untrusted slice.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], SegmentError> {
        let end = self.pos.checked_add(n).ok_or(SegmentError::Truncated)?;
        let slice = self.buf.get(self.pos..end).ok_or(SegmentError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, SegmentError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, SegmentError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, SegmentError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, SegmentError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn digest(&mut self) -> Result<Sha256Digest, SegmentError> {
        let mut out = [0u8; SHA256_OUTPUT_LEN];
        out.copy_from_slice(self.take(SHA256_OUTPUT_LEN)?);
        Ok(out)
    }
}

/// A parsed, validated segment header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SegmentHeader {
    /// Stream this segment belongs to.
    pub stream: Stream,
    /// Monotonic segment id within the stream.
    pub segment_id: u64,
    /// Non-secret machine-id hash binding the segment to this installation.
    pub machine_id_hash: Sha256Digest,
    /// Boot this segment was created in.
    pub boot_id: BootId,
    /// Append sequence of this segment's first record.
    pub first_seq: u64,
    /// The record chain's start hash: the previous segment's segment hash,
    /// or the stream genesis for a stream's first segment.
    pub prev_segment_hash: Sha256Digest,
    /// Monotonic time the segment was created.
    pub creation_monotonic: Duration64,
    /// Wall-clock reading (and its trust state) at creation.
    pub creation_wall: WallClockReading,
}

impl SegmentHeader {
    /// Encode the header (its final field is a checksum over the rest).
    fn encode(&self, buf: &mut [u8]) -> Result<(), SegmentError> {
        let mut w = Writer::at(buf, 0);
        w.put(&SEGMENT_MAGIC)?;
        w.put_u16(SEGMENT_FORMAT_VERSION)?;
        w.put_u8(self.stream.as_u8())?;
        w.put_u8(0)?; // reserved
        w.put_u64(self.segment_id)?;
        w.put(&self.machine_id_hash)?;
        w.put(self.boot_id.as_bytes())?;
        w.put_u64(self.first_seq)?;
        w.put(&self.prev_segment_hash)?;
        w.put(&self.creation_monotonic.to_le_bytes())?;
        w.put(&self.creation_wall.to_le_bytes())?;
        let checksum = sha256(&w.buf[..w.pos]);
        w.put(&checksum)?;
        Ok(())
    }

    /// Parse and validate a header from the front of `bytes`, failing closed.
    ///
    /// # Errors
    ///
    /// [`SegmentError::Truncated`], [`SegmentError::BadMagic`],
    /// [`SegmentError::UnsupportedVersion`], [`SegmentError::BadField`], or
    /// [`SegmentError::BadHeaderChecksum`].
    pub fn parse(bytes: &[u8]) -> Result<Self, SegmentError> {
        if bytes.len() < SEGMENT_HEADER_LEN {
            return Err(SegmentError::Truncated);
        }
        let mut r = Reader::new(bytes);
        if r.take(8)? != SEGMENT_MAGIC {
            return Err(SegmentError::BadMagic);
        }
        if r.u16()? != SEGMENT_FORMAT_VERSION {
            return Err(SegmentError::UnsupportedVersion);
        }
        let stream = Stream::from_u8(r.u8()?).map_err(|_| SegmentError::BadField)?;
        if r.u8()? != 0 {
            return Err(SegmentError::BadField);
        }
        let segment_id = r.u64()?;
        let machine_id_hash = r.digest()?;
        let mut boot_raw = [0u8; BOOT_ID_LEN];
        boot_raw.copy_from_slice(r.take(BOOT_ID_LEN)?);
        let boot_id = BootId::from_raw(boot_raw);
        let first_seq = r.u64()?;
        let prev_segment_hash = r.digest()?;
        let creation_monotonic = Duration64::from_bytes(r.take(Duration64::WIRE_LEN)?)
            .map_err(|_| SegmentError::BadField)?;
        let creation_wall = WallClockReading::from_bytes(r.take(WallClockReading::WIRE_LEN)?)
            .map_err(|_| SegmentError::BadField)?;
        let stored_checksum = r.digest()?;
        if sha256(&bytes[..SEGMENT_HEADER_LEN - SHA256_OUTPUT_LEN]) != stored_checksum {
            return Err(SegmentError::BadHeaderChecksum);
        }
        Ok(Self {
            stream,
            segment_id,
            machine_id_hash,
            boot_id,
            first_seq,
            prev_segment_hash,
            creation_monotonic,
            creation_wall,
        })
    }
}

/// Writes one append-only segment into a caller-owned buffer.
///
/// The buffer holds the whole segment; on close it is `[..len]` where `len`
/// is [`Self::finish`]'s return. No allocation occurs, so a fixed backing
/// buffer bounds the segment size and a full buffer fails closed with
/// [`SegmentError::BufferTooSmall`] rather than growing without limit.
pub struct SegmentWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
    stream: Stream,
    first_seq: u64,
    prev_segment_hash: Sha256Digest,
    chain: LogChain,
    record_count: u64,
    first_monotonic: Option<Duration64>,
    last_monotonic: Duration64,
}

impl<'a> SegmentWriter<'a> {
    /// Open a segment, writing its header into the front of `buf`.
    ///
    /// The record chain starts at `header.prev_segment_hash` (the previous
    /// segment's hash, or the stream genesis for a stream's first segment).
    ///
    /// # Errors
    ///
    /// [`SegmentError::BufferTooSmall`] if `buf` cannot hold the header.
    pub fn begin(buf: &'a mut [u8], header: &SegmentHeader) -> Result<Self, SegmentError> {
        header.encode(buf)?;
        let chain = LogChain::resume(header.first_seq, header.prev_segment_hash);
        Ok(Self {
            buf,
            pos: SEGMENT_HEADER_LEN,
            stream: header.stream,
            first_seq: header.first_seq,
            prev_segment_hash: header.prev_segment_hash,
            chain,
            record_count: 0,
            first_monotonic: None,
            last_monotonic: Duration64::ZERO,
        })
    }

    /// Append one committed record produced on `cpu` at monotonic time
    /// `monotonic`, returning its chain entry.
    ///
    /// The chain advances only after the buffer is confirmed to have room, so
    /// a rejected append leaves the writer's chain and position unchanged.
    ///
    /// # Errors
    ///
    /// [`SegmentError::PayloadTooLarge`] if `payload` exceeds
    /// [`MAX_RECORD_PAYLOAD`]; [`SegmentError::BufferTooSmall`] if the block
    /// does not fit.
    pub fn append_record(
        &mut self,
        cpu: u32,
        monotonic: Duration64,
        payload: &[u8],
    ) -> Result<ChainedEntry, SegmentError> {
        if payload.len() > MAX_RECORD_PAYLOAD {
            return Err(SegmentError::PayloadTooLarge);
        }
        let payload_len =
            u32::try_from(payload.len()).map_err(|_| SegmentError::PayloadTooLarge)?;
        let block_len = RECORD_PREFIX_LEN + payload.len();
        // Confirm room *before* advancing the chain, so a full buffer never
        // desynchronises the chain head from the bytes written.
        let end = self
            .pos
            .checked_add(block_len)
            .ok_or(SegmentError::BufferTooSmall)?;
        if end + SEGMENT_FOOTER_LEN > self.buf.len() {
            return Err(SegmentError::BufferTooSmall);
        }

        let payload_digest = sha256(payload);
        let entry = self.chain.append(cpu, &payload_digest);

        let mut w = Writer::at(self.buf, self.pos);
        w.put_u8(BLOCK_RECORD)?;
        w.put_u32(payload_len)?;
        w.put_u32(cpu)?;
        w.put_u64(entry.seq)?;
        w.put(&entry.entry_hash)?;
        w.put(&monotonic.to_le_bytes())?;
        w.put(payload)?;
        self.pos = w.pos;

        self.record_count += 1;
        if self.first_monotonic.is_none() {
            self.first_monotonic = Some(monotonic);
        }
        self.last_monotonic = monotonic;
        Ok(entry)
    }

    /// Number of records committed so far.
    #[must_use]
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// The record chain head (root over every record so far). This is what a
    /// successor segment resumes from and what an anchor signs.
    #[must_use]
    pub fn head_hash(&self) -> Sha256Digest {
        self.chain.head_hash()
    }

    /// Close the segment: write the footer and return the closed segment.
    ///
    /// `seal_key` is required for audit/security streams and MACs the segment
    /// hash; for other streams it is optional.
    ///
    /// The returned [`FinishedSegment`] hands the caller-owned backing buffer
    /// back (the segment image is `buf[..len]`) together with the `segment_hash`
    /// a successor segment chains from, so a writer of a stream can persist the
    /// closed bytes and reopen a fresh segment over the same buffer without a
    /// second allocation.
    ///
    /// # Errors
    ///
    /// [`SegmentError::SealKeyRequired`] if the stream requires a seal but no
    /// key was given; [`SegmentError::BufferTooSmall`] if the footer does not
    /// fit.
    pub fn finish(
        self,
        seal_key: Option<&LogAttestationKey>,
    ) -> Result<FinishedSegment<'a>, SegmentError> {
        if self.stream.requires_seal() && seal_key.is_none() {
            return Err(SegmentError::SealKeyRequired);
        }
        let footer_start = self.pos;
        let next_seq = self.first_seq + self.record_count;
        let first_monotonic = self.first_monotonic.unwrap_or(Duration64::ZERO);
        let last_record_hash = self.chain.head_hash();

        let mut w = Writer::at(self.buf, footer_start);
        w.put_u8(BLOCK_FOOTER)?;
        w.put_u64(self.record_count)?;
        w.put_u64(self.first_seq)?;
        w.put_u64(next_seq)?;
        w.put(&first_monotonic.to_le_bytes())?;
        w.put(&self.last_monotonic.to_le_bytes())?;
        w.put(&self.prev_segment_hash)?;
        w.put(&last_record_hash)?;
        let summary_end = w.pos;
        // header + records + footer summary are contiguous in `buf`.
        let segment_hash = sha256(&w.buf[..summary_end]);
        w.put(&segment_hash)?;
        let (seal_present, seal_tag): (u8, MacTag) = match seal_key {
            Some(key) => (1, key.seal(&[&segment_hash])),
            None => (0, [0u8; MAC_TAG_LEN]),
        };
        w.put_u8(seal_present)?;
        w.put(&seal_tag)?;
        let footer_checksum = sha256(&w.buf[footer_start..w.pos]);
        w.put(&footer_checksum)?;
        let len = w.pos;
        Ok(FinishedSegment {
            len,
            segment_hash,
            next_seq,
            buf: self.buf,
        })
    }
}

/// The result of closing a [`SegmentWriter`].
///
/// Carries the reclaimed backing buffer (the closed segment image is
/// `buf[..len]`), the `segment_hash` a successor segment chains from, and the
/// stream's next append sequence, so a per-stream writer can persist the bytes
/// and reopen a fresh segment over the same buffer.
pub struct FinishedSegment<'a> {
    /// Total length of the closed segment image within [`Self::buf`].
    pub len: usize,
    /// The segment hash: the value a successor segment uses as its
    /// `prev_segment_hash` to continue the stream's record chain.
    pub segment_hash: Sha256Digest,
    /// The stream's next append sequence (this segment's last `seq + 1`).
    pub next_seq: u64,
    /// The reclaimed backing buffer; the closed segment occupies `[..len]`.
    pub buf: &'a mut [u8],
}

/// A validated, borrowed view of one committed record block.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RecordBlockRef<'a> {
    /// The record's originating CPU id.
    pub cpu: u32,
    /// The record's append sequence.
    pub seq: u64,
    /// The record's chain link hash.
    pub entry_hash: Sha256Digest,
    /// The record's monotonic ordering time within the boot (SYSLOG §5.1).
    pub monotonic: Duration64,
    /// The opaque record payload bytes.
    pub payload: &'a [u8],
}

/// How a forward scan of a segment body terminated.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ScanEnd {
    /// A footer block begins at this offset within the body.
    Footer {
        /// Offset of the footer within the body (records occupy `[0, offset)`).
        offset: usize,
    },
    /// The records ended cleanly on a block boundary with no footer: an open
    /// segment still being written.
    Open,
    /// Trailing bytes could not be validated as a record or footer — a torn
    /// (partial or corrupt) tail. Every record before it is committed.
    Torn,
}

/// One move of the shared forward scan. Both the recovery reader and
/// [`verify_segment`] drive this, so the record-framing rules live in one
/// place.
enum Step<'a> {
    Record {
        block: RecordBlockRef<'a>,
        next_offset: usize,
        new_head: Sha256Digest,
    },
    Footer,
    Open,
    /// A record could not be validated; the specific reason lets a strict
    /// verifier report precisely while recovery just stops.
    Stop(SegmentError),
}

/// Validate the next block at `offset` in `body` against the running chain.
fn step<'a>(
    body: &'a [u8],
    offset: usize,
    expected_seq: u64,
    chain_head: &Sha256Digest,
) -> Step<'a> {
    if offset == body.len() {
        return Step::Open;
    }
    let Some(&tag) = body.get(offset) else {
        return Step::Stop(SegmentError::Truncated);
    };
    if tag == BLOCK_FOOTER {
        return Step::Footer;
    }
    if tag != BLOCK_RECORD {
        return Step::Stop(SegmentError::Truncated);
    }

    let mut r = Reader::new(body);
    r.pos = offset;
    // Tag already inspected; consume it and the fixed prefix.
    if r.u8().is_err() {
        return Step::Stop(SegmentError::Truncated);
    }
    let Ok(payload_len) = r.u32() else {
        return Step::Stop(SegmentError::Truncated);
    };
    let payload_len = payload_len as usize;
    if payload_len > MAX_RECORD_PAYLOAD {
        return Step::Stop(SegmentError::BadField);
    }
    let Ok(cpu) = r.u32() else {
        return Step::Stop(SegmentError::Truncated);
    };
    let Ok(seq) = r.u64() else {
        return Step::Stop(SegmentError::Truncated);
    };
    let Ok(entry_hash) = r.digest() else {
        return Step::Stop(SegmentError::Truncated);
    };
    let Ok(monotonic_bytes) = r.take(Duration64::WIRE_LEN) else {
        return Step::Stop(SegmentError::Truncated);
    };
    let Ok(monotonic) = Duration64::from_bytes(monotonic_bytes) else {
        return Step::Stop(SegmentError::BadField);
    };
    let Ok(payload) = r.take(payload_len) else {
        return Step::Stop(SegmentError::Truncated);
    };
    let next_offset = r.pos;

    if seq != expected_seq {
        return Step::Stop(SegmentError::ChainBroken);
    }
    let payload_digest = sha256(payload);
    let candidate = ChainedEntry {
        cpu,
        seq,
        prev_hash: *chain_head,
        payload_digest,
        entry_hash,
    };
    if !candidate.is_self_consistent() {
        return Step::Stop(SegmentError::ChainBroken);
    }
    Step::Record {
        block: RecordBlockRef {
            cpu,
            seq,
            entry_hash,
            monotonic,
            payload,
        },
        next_offset,
        new_head: entry_hash,
    }
}

/// A forward-scanning, self-verifying reader over a segment's committed
/// records.
///
/// Each [`Iterator::next`] validates the next record against the running
/// chain and yields it, stopping at the footer, a clean open end, or a torn
/// tail. It never trusts a record it did not re-chain, and it never panics on
/// malformed bytes — the terminal [`Self::end`] state reports how the scan
/// finished, which is exactly the power-loss recovery contract (recover to
/// the last complete, chain-valid committed record).
pub struct SegmentReader<'a> {
    header: SegmentHeader,
    body: &'a [u8],
    offset: usize,
    expected_seq: u64,
    chain_head: Sha256Digest,
    end: Option<ScanEnd>,
}

impl<'a> SegmentReader<'a> {
    /// Open a segment for reading, validating and parsing its header.
    ///
    /// # Errors
    ///
    /// Any [`SegmentHeader::parse`] error.
    pub fn open(bytes: &'a [u8]) -> Result<Self, SegmentError> {
        let header = SegmentHeader::parse(bytes)?;
        Ok(Self {
            header,
            body: &bytes[SEGMENT_HEADER_LEN..],
            offset: 0,
            expected_seq: header.first_seq,
            chain_head: header.prev_segment_hash,
            end: None,
        })
    }

    /// The validated segment header.
    #[must_use]
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    /// How the scan terminated, available once iteration is exhausted.
    #[must_use]
    pub fn end(&self) -> Option<ScanEnd> {
        self.end
    }

    /// The running chain head — after iteration, the hash over every
    /// committed record read.
    #[must_use]
    pub fn head_hash(&self) -> Sha256Digest {
        self.chain_head
    }
}

impl<'a> Iterator for SegmentReader<'a> {
    type Item = RecordBlockRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.end.is_some() {
            return None;
        }
        match step(self.body, self.offset, self.expected_seq, &self.chain_head) {
            Step::Record {
                block,
                next_offset,
                new_head,
            } => {
                self.offset = next_offset;
                self.expected_seq += 1;
                self.chain_head = new_head;
                Some(block)
            }
            Step::Footer => {
                self.end = Some(ScanEnd::Footer {
                    offset: self.offset,
                });
                None
            }
            Step::Open => {
                self.end = Some(ScanEnd::Open);
                None
            }
            Step::Stop(_) => {
                self.end = Some(ScanEnd::Torn);
                None
            }
        }
    }
}

/// A verified segment's summary: the facts a caller can trust once
/// [`verify_segment`] returns `Ok`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SegmentSummary {
    /// The validated header.
    pub header: SegmentHeader,
    /// Number of committed records.
    pub record_count: u64,
    /// First record's append sequence (equals `header.first_seq`).
    pub first_seq: u64,
    /// One past the last record's sequence (`first_seq + record_count`).
    pub next_seq: u64,
    /// Monotonic time of the first record (zero for an empty segment).
    pub first_monotonic: Duration64,
    /// Monotonic time of the last record (zero for an empty segment).
    pub last_monotonic: Duration64,
    /// Chain head over every record (what a successor segment resumes from).
    pub last_record_hash: Sha256Digest,
    /// The segment hash the footer commits to.
    pub segment_hash: Sha256Digest,
    /// Whether the segment carries a verified seal MAC.
    pub sealed: bool,
}

/// Fully verify a complete, footer-terminated segment, returning its summary.
///
/// Checks, fail-closed: the header checksum, every record's chain link and
/// contiguous sequence, the footer summary against the scanned records, the
/// footer checksum, the segment hash over the header/records/summary, and the
/// seal MAC (required for audit/security streams). `seal_key` must be supplied
/// whenever the segment is sealed or its stream requires a seal.
///
/// # Errors
///
/// The specific [`SegmentError`] for the first inconsistency found.
pub fn verify_segment(
    bytes: &[u8],
    seal_key: Option<&LogAttestationKey>,
) -> Result<SegmentSummary, SegmentError> {
    let header = SegmentHeader::parse(bytes)?;
    let body = &bytes[SEGMENT_HEADER_LEN..];

    let mut offset = 0usize;
    let mut expected_seq = header.first_seq;
    let mut chain_head = header.prev_segment_hash;
    let mut record_count = 0u64;

    let footer_offset = loop {
        match step(body, offset, expected_seq, &chain_head) {
            Step::Record {
                next_offset,
                new_head,
                ..
            } => {
                offset = next_offset;
                expected_seq += 1;
                chain_head = new_head;
                record_count += 1;
            }
            Step::Footer => break offset,
            Step::Open => return Err(SegmentError::MissingFooter),
            Step::Stop(err) => return Err(err),
        }
    };

    // The footer must be exactly the tail of the segment.
    if body.len() != footer_offset + SEGMENT_FOOTER_LEN {
        return Err(SegmentError::FooterMismatch);
    }
    let footer = &body[footer_offset..];
    let mut r = Reader::new(footer);
    r.u8()?; // footer tag (already matched)
    let footer_record_count = r.u64()?;
    let footer_first_seq = r.u64()?;
    let footer_next_seq = r.u64()?;
    let first_monotonic = Duration64::from_bytes(r.take(Duration64::WIRE_LEN)?)
        .map_err(|_| SegmentError::BadField)?;
    let last_monotonic = Duration64::from_bytes(r.take(Duration64::WIRE_LEN)?)
        .map_err(|_| SegmentError::BadField)?;
    let footer_prev_segment_hash = r.digest()?;
    let footer_last_record_hash = r.digest()?;
    let stored_segment_hash = r.digest()?;
    let seal_present = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(SegmentError::BadField),
    };
    let mut seal_tag = [0u8; MAC_TAG_LEN];
    seal_tag.copy_from_slice(r.take(MAC_TAG_LEN)?);
    let stored_footer_checksum = r.digest()?;

    // Footer checksum covers the whole footer bar its own trailing digest.
    let footer_checksum = sha256(&footer[..SEGMENT_FOOTER_LEN - SHA256_OUTPUT_LEN]);
    if footer_checksum != stored_footer_checksum {
        return Err(SegmentError::BadFooterChecksum);
    }

    if footer_record_count != record_count
        || footer_first_seq != header.first_seq
        || footer_next_seq != header.first_seq + record_count
        || footer_prev_segment_hash != header.prev_segment_hash
        || footer_last_record_hash != chain_head
    {
        return Err(SegmentError::FooterMismatch);
    }

    // Segment hash over header + records + footer summary (all contiguous).
    let summary_end = SEGMENT_HEADER_LEN + footer_offset + FOOTER_SUMMARY_LEN;
    if sha256(&bytes[..summary_end]) != stored_segment_hash {
        return Err(SegmentError::SegmentHashMismatch);
    }

    // Seal: required for audit/security; verified whenever present.
    let sealed = if header.stream.requires_seal() {
        if !seal_present {
            return Err(SegmentError::SealInvalid);
        }
        true
    } else {
        seal_present
    };
    if sealed {
        let key = seal_key.ok_or(SegmentError::SealKeyRequired)?;
        if !key.verify(&[&stored_segment_hash], &seal_tag) {
            return Err(SegmentError::SealInvalid);
        }
    }

    Ok(SegmentSummary {
        header,
        record_count,
        first_seq: header.first_seq,
        next_seq: header.first_seq + record_count,
        first_monotonic,
        last_monotonic,
        last_record_hash: chain_head,
        segment_hash: stored_segment_hash,
        sealed,
    })
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::attest::{machine_id_hash, stream_genesis, LogAttestationKey};
    use alloc::vec::Vec;
    use tairix_abi::{BootId, Duration64, Time64, WallClockReading, WallTimeState, BOOT_ID_LEN};

    const MID: [u8; 16] = [0x11; 16];

    fn boot() -> BootId {
        BootId::from_raw([0x5A; BOOT_ID_LEN])
    }

    fn header(stream: Stream, first_seq: u64, prev: Sha256Digest) -> SegmentHeader {
        SegmentHeader {
            stream,
            segment_id: 1,
            machine_id_hash: machine_id_hash(&MID),
            boot_id: boot(),
            first_seq,
            prev_segment_hash: prev,
            creation_monotonic: Duration64::from_secs(1),
            creation_wall: WallClockReading::new(
                Time64::from_secs(1_700_000_000),
                WallTimeState::Trusted,
            ),
        }
    }

    fn genesis(stream: Stream) -> Sha256Digest {
        stream_genesis(&machine_id_hash(&MID), stream.genesis_label(), &boot())
    }

    fn key() -> LogAttestationKey {
        LogAttestationKey::from_key([0x24; 32])
    }

    // Write a runtime segment with the given payloads; returns (bytes, len).
    fn build_runtime(payloads: &[&[u8]]) -> (Vec<u8>, usize) {
        let mut buf = alloc::vec![0u8; 8192];
        let len = {
            let h = header(Stream::Runtime, 100, genesis(Stream::Runtime));
            let mut w = SegmentWriter::begin(&mut buf, &h).expect("begin");
            for (i, p) in payloads.iter().enumerate() {
                let cpu = u32::try_from(i).expect("index fits u32");
                let secs = 10 + i64::try_from(i).expect("index fits i64");
                w.append_record(cpu, Duration64::from_secs(secs), p)
                    .expect("append");
            }
            w.finish(None).expect("finish").len
        };
        (buf, len)
    }

    #[test]
    fn round_trips_records_through_reader() {
        let payloads: [&[u8]; 3] = [b"alpha", b"a longer record payload", b""];
        let (buf, len) = build_runtime(&payloads);
        let reader = SegmentReader::open(&buf[..len]).expect("open");
        assert_eq!(reader.header().stream, Stream::Runtime);
        assert_eq!(reader.header().first_seq, 100);
        let got: Vec<_> = SegmentReader::open(&buf[..len]).expect("open").collect();
        assert_eq!(got.len(), 3);
        for (i, block) in got.iter().enumerate() {
            let idx = u32::try_from(i).expect("index fits u32");
            assert_eq!(block.seq, 100 + u64::from(idx));
            assert_eq!(block.cpu, idx);
            assert_eq!(block.payload, payloads[i]);
            // Each record carries its own monotonic time (SYSLOG §5.1),
            // matching the `10 + i`
            // seconds `build_runtime` stamped it with.
            let secs = 10 + i64::try_from(i).expect("index fits i64");
            assert_eq!(block.monotonic, Duration64::from_secs(secs));
        }
        // The reader terminates on the footer.
        let mut r = SegmentReader::open(&buf[..len]).expect("open");
        while r.next().is_some() {}
        assert!(matches!(r.end(), Some(ScanEnd::Footer { .. })));
    }

    #[test]
    fn verify_accepts_an_honest_segment() {
        let (buf, len) = build_runtime(&[b"one", b"two"]);
        let s = verify_segment(&buf[..len], None).expect("verifies");
        assert_eq!(s.record_count, 2);
        assert_eq!(s.first_seq, 100);
        assert_eq!(s.next_seq, 102);
        assert!(!s.sealed);
        assert_eq!(s.first_monotonic, Duration64::from_secs(10));
        assert_eq!(s.last_monotonic, Duration64::from_secs(11));
    }

    #[test]
    fn verify_accepts_an_empty_segment() {
        let (buf, len) = build_runtime(&[]);
        let s = verify_segment(&buf[..len], None).expect("empty verifies");
        assert_eq!(s.record_count, 0);
        assert_eq!(s.next_seq, 100);
        // An empty segment's last record hash is the chain start (genesis).
        assert_eq!(s.last_record_hash, genesis(Stream::Runtime));
    }

    #[test]
    fn tampering_with_a_payload_byte_breaks_the_chain() {
        let (mut buf, len) = build_runtime(&[b"hello", b"world"]);
        // Flip a byte inside the first record's payload.
        let payload_off = SEGMENT_HEADER_LEN + RECORD_PREFIX_LEN;
        buf[payload_off] ^= 0xFF;
        assert_eq!(
            verify_segment(&buf[..len], None),
            Err(SegmentError::ChainBroken)
        );
    }

    #[test]
    fn tampering_with_the_header_is_caught() {
        let (mut buf, len) = build_runtime(&[b"x"]);
        buf[12] ^= 0xFF; // inside segment_id
        assert_eq!(
            SegmentHeader::parse(&buf[..len]),
            Err(SegmentError::BadHeaderChecksum)
        );
        assert_eq!(
            verify_segment(&buf[..len], None),
            Err(SegmentError::BadHeaderChecksum)
        );
    }

    #[test]
    fn bad_magic_and_version_are_rejected() {
        let (mut buf, len) = build_runtime(&[b"x"]);
        let mut m = buf.clone();
        m[0] ^= 0xFF;
        assert_eq!(SegmentHeader::parse(&m[..len]), Err(SegmentError::BadMagic));
        buf[8] = 9; // format_version low byte
        assert_eq!(
            SegmentHeader::parse(&buf[..len]),
            Err(SegmentError::UnsupportedVersion)
        );
    }

    #[test]
    fn a_torn_tail_recovers_the_committed_records() {
        let (buf, len) = build_runtime(&[b"first", b"second", b"third"]);
        // Cut the segment inside the footer/last structures so the third
        // record is committed but the footer is incomplete.
        let torn = &buf[..len - SEGMENT_FOOTER_LEN - 2];
        let mut r = SegmentReader::open(torn).expect("open");
        let got: Vec<_> = (&mut r).collect();
        assert_eq!(got.len(), 2, "records before the torn tail are recovered");
        assert!(matches!(r.end(), Some(ScanEnd::Torn)));
        // A strict verify of an unterminated segment fails closed.
        assert!(verify_segment(torn, None).is_err());
    }

    #[test]
    fn an_open_segment_has_no_footer() {
        // Build records but never call finish().
        let mut buf = alloc::vec![0u8; 8192];
        let end = {
            let h = header(Stream::Runtime, 0, genesis(Stream::Runtime));
            let mut w = SegmentWriter::begin(&mut buf, &h).expect("begin");
            w.append_record(0, Duration64::from_secs(1), b"only")
                .expect("append");
            // Emulate "just the header + one record" by asking the writer where
            // it stopped: re-derive via a reader over the written prefix.
            SEGMENT_HEADER_LEN + RECORD_PREFIX_LEN + 4
        };
        let mut r = SegmentReader::open(&buf[..end]).expect("open");
        let got: Vec<_> = (&mut r).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(r.end(), Some(ScanEnd::Open));
        assert_eq!(
            verify_segment(&buf[..end], None),
            Err(SegmentError::MissingFooter)
        );
    }

    #[test]
    fn audit_stream_must_be_sealed_and_verifies_with_the_key() {
        // Closing an audit segment without a key is refused.
        let mut buf2 = alloc::vec![0u8; 8192];
        let h2 = header(Stream::Audit, 0, genesis(Stream::Audit));
        let mut w2 = SegmentWriter::begin(&mut buf2, &h2).expect("begin");
        w2.append_record(0, Duration64::from_secs(1), b"login denied")
            .expect("append");
        assert!(matches!(
            w2.finish(None),
            Err(SegmentError::SealKeyRequired)
        ));

        let mut buf3 = alloc::vec![0u8; 8192];
        let h3 = header(Stream::Audit, 0, genesis(Stream::Audit));
        let mut w3 = SegmentWriter::begin(&mut buf3, &h3).expect("begin");
        w3.append_record(0, Duration64::from_secs(1), b"login denied")
            .expect("append");
        let len = w3.finish(Some(&key())).expect("sealed finish").len;

        let s = verify_segment(&buf3[..len], Some(&key())).expect("sealed verify");
        assert!(s.sealed);
        // Verifying a sealed segment without the key fails closed.
        assert_eq!(
            verify_segment(&buf3[..len], None),
            Err(SegmentError::SealKeyRequired)
        );
        // A wrong key does not verify.
        let wrong = LogAttestationKey::from_key([0x99; 32]);
        assert_eq!(
            verify_segment(&buf3[..len], Some(&wrong)),
            Err(SegmentError::SealInvalid)
        );
    }

    #[test]
    fn segments_chain_across_the_stream() {
        // First segment.
        let (buf1, len1) = build_runtime(&[b"a", b"b"]);
        let s1 = verify_segment(&buf1[..len1], None).expect("seg1");
        // Second segment resumes: prev_segment_hash = seg1.segment_hash,
        // first_seq = seg1.next_seq.
        let mut buf2 = alloc::vec![0u8; 8192];
        let len2 = {
            let mut h = header(Stream::Runtime, s1.next_seq, s1.segment_hash);
            h.segment_id = 2;
            let mut w = SegmentWriter::begin(&mut buf2, &h).expect("begin2");
            w.append_record(0, Duration64::from_secs(20), b"c")
                .expect("append");
            w.finish(None).expect("finish2").len
        };
        let s2 = verify_segment(&buf2[..len2], None).expect("seg2");
        assert_eq!(s2.first_seq, s1.next_seq);
        assert_eq!(s2.header.prev_segment_hash, s1.segment_hash);
        // The record chain is continuous: seg2's first record chains onto
        // seg1's segment hash, so the seq is monotonic across the boundary.
        let first_c = SegmentReader::open(&buf2[..len2])
            .expect("open")
            .next()
            .unwrap();
        assert_eq!(first_c.seq, 102);
    }

    #[test]
    fn oversized_payload_is_refused() {
        let mut buf = alloc::vec![0u8; MAX_RECORD_PAYLOAD + 4096];
        let h = header(Stream::Runtime, 0, genesis(Stream::Runtime));
        let mut w = SegmentWriter::begin(&mut buf, &h).expect("begin");
        let big = alloc::vec![0u8; MAX_RECORD_PAYLOAD + 1];
        assert_eq!(
            w.append_record(0, Duration64::from_secs(1), &big),
            Err(SegmentError::PayloadTooLarge)
        );
    }

    #[test]
    fn a_full_buffer_fails_closed() {
        // Room for the header + footer but not a record.
        let mut buf = alloc::vec![0u8; SEGMENT_HEADER_LEN + SEGMENT_FOOTER_LEN + 4];
        let h = header(Stream::Runtime, 0, genesis(Stream::Runtime));
        let mut w = SegmentWriter::begin(&mut buf, &h).expect("begin");
        assert_eq!(
            w.append_record(0, Duration64::from_secs(1), b"too big for the room"),
            Err(SegmentError::BufferTooSmall)
        );
    }
}
