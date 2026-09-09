//! A complete, fail-closed GIF decoder (`GIF89a` specification, and the
//! `GIF87a` subset).
//!
//! GIF is a *sequence* format: its content is a chain of blocks over one
//! logical screen, and every frame composites onto whatever its predecessors
//! left there under the disposal method declared for each. A per-index decode
//! would therefore be both wrong and quadratic, so the decoder holds the
//! composition canvas and steps forward through the chain, yielding the
//! composited canvas once per frame.
//!
//! Opening a stream makes one structural pass: it validates the block chain,
//! counts the frames, and reads the `NETSCAPE2.0` loop count, decoding no
//! pixels. Stepping then walks forward from where the last step stopped, so a
//! step costs its own frame and nothing more.
//!
//! # Two places this decoder does not read the specification literally
//!
//! *Restore to background* (disposal `2`) clears the frame's area to fully
//! transparent rather than to the logical screen's background colour. Every
//! producer means "clear it" and every other decoder does this; restoring an
//! opaque background colour would flash a coloured box through nearly every
//! real animation. The background-colour index is therefore read and skipped.
//!
//! A frame's declared delay is reported exactly as the file gives it,
//! including zero. Clamping a too-fast animation to a minimum interval is a
//! *playback* decision, and the thing that plays frames is the one that knows
//! how fast its screen can show them.

use alloc::vec::Vec;
use core::ops::Range;

use tairix_util::fallible;

use crate::{DecodeError, DecodeLimits, RasterImage, PROBE_LIMITS, RGBA_BYTES};

/// The three magic bytes every GIF opens with.
const MAGIC: [u8; 3] = *b"GIF";

/// The two versions the format defines. `GIF87a` simply carries none of the
/// `GIF89a` extension blocks, so one parser reads both.
const VERSIONS: [[u8; 3]; 2] = [*b"87a", *b"89a"];

/// The Logical Screen Descriptor's fixed length (`GIF89a` §18).
const SCREEN_DESCRIPTOR_LEN: usize = 7;

/// The Image Descriptor's length after its separator (`GIF89a` §20).
const IMAGE_DESCRIPTOR_LEN: usize = 9;

const EXTENSION_INTRODUCER: u8 = 0x21;
const IMAGE_SEPARATOR: u8 = 0x2C;
const TRAILER: u8 = 0x3B;

const LABEL_PLAIN_TEXT: u8 = 0x01;
const LABEL_GRAPHIC_CONTROL: u8 = 0xF9;
const LABEL_APPLICATION: u8 = 0xFF;

/// The Graphic Control Extension's fixed data-sub-block length (`GIF89a` §23).
const GRAPHIC_CONTROL_LEN: u8 = 4;

/// The Application Extension's fixed identifier + authentication-code length
/// (`GIF89a` §26).
const APPLICATION_ID_LEN: u8 = 11;

/// The identifier and authentication code of the de-facto animation-loop
/// extension, the length of the sub-block carrying its count, and the
/// introducer byte that sub-block opens with.
const NETSCAPE_ID: [u8; 11] = *b"NETSCAPE2.0";
const NETSCAPE_LOOP_LEN: u8 = 3;
const NETSCAPE_LOOP_SUB_BLOCK: u8 = 0x01;

/// Bounds on the LZW minimum code size (`GIF89a` Appendix F). A one-bit code
/// size leaves no room for the clear and end codes the format requires, and
/// nine would exceed the 256 entries an index byte can address.
const MIN_CODE_SIZE_MIN: u8 = 2;
const MIN_CODE_SIZE_MAX: u8 = 8;

/// The widest LZW code the format allows, and the resulting table size.
const MAX_CODE_BITS: u32 = 12;
const MAX_CODES: usize = 1 << MAX_CODE_BITS;

/// The prefix stored for a root code, which has none. Outside the code space,
/// so it can never be mistaken for a code.
const NO_PREFIX: u16 = u16::MAX;

/// Bytes per colour-table entry: one each of red, green, and blue.
const PALETTE_ENTRY_LEN: usize = 3;

/// Nanoseconds in the hundredth of a second a GIF delay counts in.
const DELAY_TICK_NS: u64 = 10_000_000;

/// Frames one stream may declare.
///
/// A fixed containment bound rather than a capacity: a frame block costs about
/// ten bytes, so a small file can declare enormous numbers of them, and no
/// viewer has use for an animation longer than this. It bounds the count the
/// structural pass accepts; nothing is allocated per frame.
const MAX_FRAMES: u32 = 16_384;

/// A forward byte reader over the whole stream, refusing every read that
/// would run past the end.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8], pos: usize) -> Self {
        Self { bytes, pos }
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.bytes.get(self.pos).ok_or(DecodeError::GifTruncated)?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(len).ok_or(DecodeError::GifTruncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(DecodeError::GifTruncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Walk a chain of data sub-blocks to just past its zero-length
    /// terminator, discarding the contents.
    fn skip_sub_blocks(&mut self) -> Result<(), DecodeError> {
        loop {
            let len = self.byte()?;
            if len == 0 {
                return Ok(());
            }
            self.take(usize::from(len))?;
        }
    }
}

/// The little-endian 16-bit field at `at` of a record whose length the
/// caller has already proved, so an absent field cannot arise.
fn word(fields: &[u8], at: usize) -> u32 {
    u32::from(crate::le_u16(fields, at).unwrap_or_default())
}

/// What happens to a frame's area once it has been shown (`GIF89a` §23).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Disposal {
    /// `0` (none specified) and `1` (do not dispose): the frame stays and the
    /// next composites over it. Both spellings mean the same to a decoder, so
    /// they are one variant.
    Keep,
    /// `2`: the frame's area is cleared.
    Clear,
    /// `3`: the frame's area is restored to what it held before the frame was
    /// drawn.
    Previous,
}

impl Disposal {
    fn from_bits(bits: u8) -> Result<Self, DecodeError> {
        match bits {
            0 | 1 => Ok(Self::Keep),
            2 => Ok(Self::Clear),
            3 => Ok(Self::Previous),
            // 4..=7 are reserved by the specification. Guessing a composition
            // the author never declared would be a fabricated picture.
            _ => Err(DecodeError::GifReservedDisposal),
        }
    }
}

/// The Graphic Control Extension in force for the next rendered block, or the
/// format's defaults where none precedes it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Control {
    disposal: Disposal,
    delay_ns: u64,
    transparent: Option<u8>,
}

impl Control {
    const DEFAULT: Self = Self {
        disposal: Disposal::Keep,
        delay_ns: 0,
        transparent: None,
    };
}

/// A rectangle of the logical screen, in pixels.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Rect {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

/// One frame's Image Descriptor, validated against the logical screen.
struct Descriptor<'a> {
    rect: Rect,
    interlaced: bool,
    palette: Option<&'a [u8]>,
    min_code_size: u8,
    /// Offset of the first LZW data sub-block's length byte.
    data: usize,
}

/// The logical screen every frame composites onto (`GIF89a` §18).
struct Screen<'a> {
    width: u32,
    height: u32,
    palette: Option<&'a [u8]>,
}

/// Read the signature and Logical Screen Descriptor, answering the screen and
/// the offset of the first block after the global colour table.
fn read_screen(bytes: &[u8]) -> Result<(Screen<'_>, usize), DecodeError> {
    let mut reader = Reader::new(bytes, 0);
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(DecodeError::GifBadSignature);
    }
    let version = reader.take(3)?;
    if !VERSIONS.iter().any(|known| known == version) {
        return Err(DecodeError::GifUnknownVersion);
    }
    let descriptor = reader.take(SCREEN_DESCRIPTOR_LEN)?;
    let width = word(descriptor, 0);
    let height = word(descriptor, 2);
    let packed = descriptor[4];
    // The background-colour index (byte 5) and pixel aspect ratio (byte 6)
    // are read and skipped: disposal clears to transparent, and the desktop
    // scales by its own geometry rather than a stored ratio.
    let palette = if packed & 0x80 == 0 {
        None
    } else {
        Some(reader.take(palette_len(packed))?)
    };
    Ok((
        Screen {
            width,
            height,
            palette,
        },
        reader.pos,
    ))
}

/// Bytes a colour table of the size `packed`'s low three bits declare
/// occupies: `3 * 2^(size + 1)`, so 6 through 768.
const fn palette_len(packed: u8) -> usize {
    PALETTE_ENTRY_LEN << ((packed & 0x07) + 1)
}

/// Read one Image Descriptor, its colour table, and its code size, leaving
/// the reader at the first data sub-block.
fn read_descriptor<'a>(
    reader: &mut Reader<'a>,
    screen: &Screen<'_>,
) -> Result<Descriptor<'a>, DecodeError> {
    let fields = reader.take(IMAGE_DESCRIPTOR_LEN)?;
    let rect = Rect {
        left: word(fields, 0),
        top: word(fields, 2),
        width: word(fields, 4),
        height: word(fields, 6),
    };
    let packed = fields[8];
    if rect.width == 0 || rect.height == 0 {
        return Err(DecodeError::GifZeroFrame);
    }
    // A frame reaching outside the logical screen is a structural
    // inconsistency; clipping it would silently drop the author's pixels.
    let fits = rect.left.saturating_add(rect.width) <= screen.width
        && rect.top.saturating_add(rect.height) <= screen.height;
    if !fits {
        return Err(DecodeError::GifFrameOutsideScreen);
    }
    let palette = if packed & 0x80 == 0 {
        None
    } else {
        Some(reader.take(palette_len(packed))?)
    };
    let min_code_size = reader.byte()?;
    if !(MIN_CODE_SIZE_MIN..=MIN_CODE_SIZE_MAX).contains(&min_code_size) {
        return Err(DecodeError::GifInvalidCodeSize);
    }
    Ok(Descriptor {
        rect,
        interlaced: packed & 0x40 != 0,
        palette,
        min_code_size,
        data: reader.pos,
    })
}

/// Read a Graphic Control Extension's fixed payload.
fn read_graphic_control(reader: &mut Reader<'_>) -> Result<Control, DecodeError> {
    if reader.byte()? != GRAPHIC_CONTROL_LEN {
        return Err(DecodeError::GifMalformedExtension);
    }
    let fields = reader.take(usize::from(GRAPHIC_CONTROL_LEN))?;
    let packed = fields[0];
    let delay = u64::from(word(fields, 1));
    let transparent = (packed & 0x01 != 0).then_some(fields[3]);
    if reader.byte()? != 0 {
        return Err(DecodeError::GifMalformedExtension);
    }
    Ok(Control {
        disposal: Disposal::from_bits((packed >> 2) & 0x07)?,
        delay_ns: delay * DELAY_TICK_NS,
        transparent,
    })
}

/// What an Application Extension contributed.
enum Application {
    /// The animation-loop extension, carrying how many times to play the
    /// sequence — `None` for ever, so the distinction cannot be lost to a
    /// magic number.
    Loops(Option<u32>),
    /// Any other application's extension, read and skipped.
    Other,
}

/// Read an Application Extension (`GIF89a` §26).
fn read_application(reader: &mut Reader<'_>) -> Result<Application, DecodeError> {
    if reader.byte()? != APPLICATION_ID_LEN {
        return Err(DecodeError::GifMalformedExtension);
    }
    let identifier = reader.take(usize::from(APPLICATION_ID_LEN))?;
    if identifier != NETSCAPE_ID {
        reader.skip_sub_blocks()?;
        return Ok(Application::Other);
    }
    let len = reader.byte()?;
    if len != NETSCAPE_LOOP_LEN {
        // The buffering-size variant, or an empty chain: no loop count here.
        if len != 0 {
            reader.take(usize::from(len))?;
            reader.skip_sub_blocks()?;
        }
        return Ok(Application::Other);
    }
    let payload = reader.take(usize::from(NETSCAPE_LOOP_LEN))?;
    let loops = word(payload, 1);
    let introducer = payload[0];
    reader.skip_sub_blocks()?;
    if introducer != NETSCAPE_LOOP_SUB_BLOCK {
        return Ok(Application::Other);
    }
    Ok(Application::Loops((loops != 0).then_some(loops)))
}

/// What one structural pass over the block chain found.
struct Layout {
    frames: u32,
    loop_count: Option<u32>,
}

/// Walk the whole block chain, validating its framing and counting its frames
/// without decoding a pixel.
///
/// Every declared length is checked against the bytes actually present, so a
/// stream that lies about a block's size is refused here rather than at the
/// step that would have read it.
fn scan(bytes: &[u8], screen: &Screen<'_>, first_block: usize) -> Result<Layout, DecodeError> {
    let mut reader = Reader::new(bytes, first_block);
    let mut frames = 0u32;
    // Absent, the format plays a sequence exactly once.
    let mut loop_count = Some(1);
    loop {
        match reader.byte()? {
            TRAILER => break,
            IMAGE_SEPARATOR => {
                let descriptor = read_descriptor(&mut reader, screen)?;
                reader.pos = descriptor.data;
                reader.skip_sub_blocks()?;
                frames = frames.saturating_add(1);
                if frames > MAX_FRAMES {
                    return Err(DecodeError::GifTooManyFrames);
                }
            }
            EXTENSION_INTRODUCER => match reader.byte()? {
                LABEL_GRAPHIC_CONTROL => {
                    read_graphic_control(&mut reader)?;
                }
                LABEL_APPLICATION => {
                    if let Application::Loops(declared) = read_application(&mut reader)? {
                        loop_count = declared;
                    }
                }
                // A comment, a plain text block, and any extension a
                // later specification adds all frame their payload the same
                // way, so one arm walks past every one of them.
                _ => reader.skip_sub_blocks()?,
            },
            _ => return Err(DecodeError::GifUnknownBlock),
        }
    }
    if frames == 0 {
        return Err(DecodeError::GifNoFrames);
    }
    Ok(Layout { frames, loop_count })
}

/// Read the logical screen's declared size, decoding nothing.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let (screen, _) = read_screen(bytes)?;
    PROBE_LIMITS.check(screen.width, screen.height)?;
    Ok((screen.width, screen.height))
}

/// The LZW dictionary and output stack, allocated once and reused by every
/// frame of a stream.
struct Lzw {
    prefix: Vec<u16>,
    suffix: Vec<u8>,
    /// One slot deeper than the longest possible string, for the reserved
    /// leading byte the not-yet-defined-code case fills in.
    stack: Vec<u8>,
}

impl Lzw {
    fn new() -> Option<Self> {
        Some(Self {
            prefix: fallible::filled(MAX_CODES, NO_PREFIX)?,
            suffix: fallible::filled(MAX_CODES, 0u8)?,
            stack: fallible::filled(MAX_CODES + 1, 0u8)?,
        })
    }

    /// Walk `code`'s string onto the stack in reverse from `depth`, answering
    /// its first byte.
    ///
    /// A dictionary entry's prefix is always a code that already existed when
    /// the entry was defined, so the walk strictly decreases and terminates.
    /// The index and depth are checked anyway, so a table this decoder could
    /// not have built still cannot overrun either buffer.
    fn walk(&mut self, code: u16, roots: u16, depth: &mut usize) -> Result<u8, DecodeError> {
        let mut cursor = code;
        while cursor >= roots {
            let index = usize::from(cursor);
            if index >= self.suffix.len() || *depth >= self.stack.len() {
                return Err(DecodeError::GifInvalidCode);
            }
            self.stack[*depth] = self.suffix[index];
            *depth += 1;
            cursor = self.prefix[index];
        }
        if *depth >= self.stack.len() {
            return Err(DecodeError::GifInvalidCode);
        }
        let root = u8::try_from(cursor).map_err(|_| DecodeError::GifInvalidCode)?;
        self.stack[*depth] = root;
        *depth += 1;
        Ok(root)
    }
}

/// The LZW code stream of one frame, read across its data sub-blocks.
///
/// The sub-blocks are a framing layer only: the bit stream runs continuously
/// across their boundaries, so the reader pulls the next block whenever it
/// runs out of bits rather than resynchronising.
struct CodeReader<'a> {
    bytes: &'a [u8],
    /// Offset of the next sub-block's length byte.
    next_block: usize,
    /// Bytes of the current sub-block still unread.
    block: &'a [u8],
    accumulator: u32,
    held: u32,
    ended: bool,
}

impl<'a> CodeReader<'a> {
    const fn new(bytes: &'a [u8], start: usize) -> Self {
        Self {
            bytes,
            next_block: start,
            block: &[],
            accumulator: 0,
            held: 0,
            ended: false,
        }
    }

    /// Pull the next sub-block, answering whether it carried any bytes.
    fn advance(&mut self) -> Result<bool, DecodeError> {
        if self.ended {
            return Ok(false);
        }
        let len = usize::from(
            *self
                .bytes
                .get(self.next_block)
                .ok_or(DecodeError::GifTruncated)?,
        );
        let start = self.next_block + 1;
        let end = start.checked_add(len).ok_or(DecodeError::GifTruncated)?;
        self.block = self
            .bytes
            .get(start..end)
            .ok_or(DecodeError::GifTruncated)?;
        self.next_block = end;
        if len == 0 {
            self.ended = true;
            return Ok(false);
        }
        Ok(true)
    }

    /// The next `width`-bit code, or `None` once the stream has run out.
    fn code(&mut self, width: u32) -> Result<Option<u16>, DecodeError> {
        while self.held < width {
            let block = self.block;
            let Some((&byte, rest)) = block.split_first() else {
                if !self.advance()? {
                    return Ok(None);
                }
                continue;
            };
            self.block = rest;
            self.accumulator |= u32::from(byte) << self.held;
            self.held += 8;
        }
        let mask = (1u32 << width) - 1;
        let code =
            u16::try_from(self.accumulator & mask).map_err(|_| DecodeError::GifInvalidCode)?;
        self.accumulator >>= width;
        self.held -= width;
        Ok(Some(code))
    }

    /// Drain to just past the terminating zero-length sub-block, answering
    /// the offset of the block that follows.
    fn finish(&mut self) -> Result<usize, DecodeError> {
        while !self.ended {
            self.block = &[];
            self.advance()?;
        }
        Ok(self.next_block)
    }
}

/// Expand one frame's LZW stream into `out`, which is exactly the frame's
/// pixel count, and answer the offset of the block after it.
///
/// The table grows in lockstep with the encoder: the entry that fills the
/// current width's code space widens the next read by one bit, to twelve. A
/// stream that fills the table and never clears it keeps decoding at twelve
/// bits against the table as it stands — the deferred clear real encoders
/// rely on — rather than being refused.
///
/// A stream producing fewer pixels than the frame declares is refused, since
/// a partly-filled frame would be a fabricated picture. One producing more
/// stops at the frame's last pixel, because the rest is not part of the frame.
fn expand(
    bytes: &[u8],
    data: usize,
    min_code_size: u8,
    lzw: &mut Lzw,
    out: &mut [u8],
) -> Result<usize, DecodeError> {
    let root_bits = u32::from(min_code_size);
    let roots = 1u16 << root_bits;
    let end = roots + 1;
    for index in 0..usize::from(roots) {
        lzw.prefix[index] = NO_PREFIX;
        lzw.suffix[index] = u8::try_from(index).map_err(|_| DecodeError::GifInvalidCode)?;
    }
    let mut next = end + 1;
    let mut width = root_bits + 1;
    let mut previous: Option<u16> = None;
    let mut written = 0usize;
    let mut reader = CodeReader::new(bytes, data);
    while let Some(code) = reader.code(width)? {
        if code == roots {
            next = end + 1;
            width = root_bits + 1;
            previous = None;
            continue;
        }
        if code == end {
            break;
        }
        let mut depth = 0usize;
        let first = match code.cmp(&next) {
            core::cmp::Ordering::Less => lzw.walk(code, roots, &mut depth)?,
            // The code this very step defines: its string is the previous one
            // followed by that string's own first byte. The stack fills in
            // reverse, so the trailing byte takes the slot below the walk —
            // which is why the stack carries one extra.
            core::cmp::Ordering::Equal => {
                let Some(prev) = previous else {
                    return Err(DecodeError::GifInvalidCode);
                };
                depth = 1;
                let first = lzw.walk(prev, roots, &mut depth)?;
                lzw.stack[0] = first;
                first
            }
            core::cmp::Ordering::Greater => return Err(DecodeError::GifInvalidCode),
        };
        for slot in (0..depth).rev() {
            if written == out.len() {
                break;
            }
            out[written] = lzw.stack[slot];
            written += 1;
        }
        if written == out.len() {
            break;
        }
        if let Some(prev) = previous {
            if usize::from(next) < MAX_CODES {
                lzw.prefix[usize::from(next)] = prev;
                lzw.suffix[usize::from(next)] = first;
                next += 1;
                if u32::from(next) >= (1u32 << width) && width < MAX_CODE_BITS {
                    width += 1;
                }
            }
        }
        previous = Some(code);
    }
    if written != out.len() {
        return Err(DecodeError::GifTruncatedImageData);
    }
    reader.finish()
}

/// Rows an interlace pass starting at `start` and stepping by `step` covers
/// in a frame `height` rows tall.
const fn pass_rows(height: u32, start: u32, step: u32) -> u32 {
    if height <= start {
        0
    } else {
        (height - start).div_ceil(step)
    }
}

/// The four interlace passes, as (first row, row step) (`GIF89a` §20).
const INTERLACE_PASSES: [(u32, u32); 4] = [(0, 8), (4, 8), (2, 4), (1, 2)];

/// The frame row the `stream_row`-th row of an interlaced frame's data
/// belongs to.
fn interlaced_row(stream_row: u32, height: u32) -> u32 {
    let mut row = stream_row;
    for (start, step) in INTERLACE_PASSES {
        let rows = pass_rows(height, start, step);
        if row < rows {
            return start + row * step;
        }
        row -= rows;
    }
    // The four passes cover every row inside the frame, so this is reached
    // only for a row past its end, which belongs to the last.
    height.saturating_sub(1)
}

/// A GIF stream's frames, composited in order.
pub(crate) struct Frames<'a> {
    bytes: &'a [u8],
    width: u32,
    height: u32,
    global_palette: Option<&'a [u8]>,
    first_block: usize,
    cursor: usize,
    index: u32,
    count: u32,
    loop_count: Option<u32>,
    /// The composition canvas: straight-alpha RGBA8, screen-sized.
    canvas: Vec<u8>,
    /// One frame's palette indices, grown to the largest frame seen.
    indices: Vec<u8>,
    /// The canvas pixels a restore-to-previous disposal puts back. Allocated
    /// only if a stream asks for one.
    saved: Vec<u8>,
    /// The disposal the last frame drawn asks for, applied before the next.
    pending: Option<(Disposal, Rect)>,
    /// The refusal a step stopped at, if one did.
    ///
    /// A part-way refusal leaves the previous frame's disposal applied, part
    /// of the refused frame's pixels on the canvas, and its restore snapshot
    /// taken from that state, so nothing there describes a whole frame. Every
    /// refusal this decoder raises is deterministic, so a retry would re-read
    /// and re-refuse the same frame anyway; recording it makes stepping after
    /// a refusal fail closed by construction rather than by that argument —
    /// which is what a format allocating per frame, and so able to refuse
    /// mid-composite and then succeed, will need.
    failed: Option<DecodeError>,
    lzw: Lzw,
    delay_ns: u64,
}

impl<'a> Frames<'a> {
    /// Validate a stream's structure and prepare its canvas, decoding no
    /// pixels.
    ///
    /// The screen geometry is weighed against `limits` before the canvas is
    /// allocated, so a stream that lies about its size cannot make this
    /// reserve memory proportional to the lie.
    pub(crate) fn open(bytes: &'a [u8], limits: &DecodeLimits) -> Result<Self, DecodeError> {
        let (screen, first_block) = read_screen(bytes)?;
        limits.check(screen.width, screen.height)?;
        let layout = scan(bytes, &screen, first_block)?;
        let canvas_len = usize::try_from(
            u64::from(screen.width)
                .checked_mul(u64::from(screen.height))
                .and_then(|pixels| pixels.checked_mul(RGBA_BYTES as u64))
                .ok_or(DecodeError::DimensionsOverflow)?,
        )
        .map_err(|_| DecodeError::DimensionsOverflow)?;
        Ok(Self {
            bytes,
            width: screen.width,
            height: screen.height,
            global_palette: screen.palette,
            first_block,
            cursor: first_block,
            index: 0,
            count: layout.frames,
            loop_count: layout.loop_count,
            canvas: fallible::filled(canvas_len, 0u8).ok_or(DecodeError::OutOfMemory)?,
            indices: Vec::new(),
            saved: Vec::new(),
            pending: None,
            failed: None,
            lzw: Lzw::new().ok_or(DecodeError::OutOfMemory)?,
            delay_ns: 0,
        })
    }

    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    pub(crate) const fn count(&self) -> u32 {
        self.count
    }

    /// How many frames have been stepped to, which is the next one's index.
    pub(crate) const fn index(&self) -> u32 {
        self.index
    }

    pub(crate) const fn loop_count(&self) -> Option<u32> {
        self.loop_count
    }

    /// The delay the frame most recently stepped to declares, in nanoseconds.
    pub(crate) const fn delay_ns(&self) -> u64 {
        self.delay_ns
    }

    /// The composited canvas as it stands.
    pub(crate) fn canvas(&self) -> &[u8] {
        &self.canvas
    }

    /// Restart at the first frame, clearing the canvas.
    pub(crate) fn rewind(&mut self) {
        self.canvas.fill(0);
        self.cursor = self.first_block;
        self.index = 0;
        self.pending = None;
        self.failed = None;
        self.delay_ns = 0;
    }

    /// Composite the next frame onto the canvas, answering `false` once the
    /// chain is exhausted.
    ///
    /// A refusal is remembered and repeated until [`Self::rewind`]: see
    /// [`Self::failed`].
    pub(crate) fn step(&mut self) -> Result<bool, DecodeError> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        if self.index >= self.count {
            return Ok(false);
        }
        self.advance()
            .inspect_err(|err| self.failed = Some(err.clone()))
    }

    /// One step, with no memory of a refusal: [`Self::step`] records it.
    fn advance(&mut self) -> Result<bool, DecodeError> {
        self.dispose();
        let mut control = Control::DEFAULT;
        let mut reader = Reader::new(self.bytes, self.cursor);
        loop {
            match reader.byte()? {
                IMAGE_SEPARATOR => {
                    let screen = Screen {
                        width: self.width,
                        height: self.height,
                        palette: self.global_palette,
                    };
                    let descriptor = read_descriptor(&mut reader, &screen)?;
                    if control.disposal == Disposal::Previous {
                        self.save(descriptor.rect)?;
                    }
                    self.cursor = self.draw(&descriptor, control)?;
                    self.index += 1;
                    self.delay_ns = control.delay_ns;
                    self.pending = Some((control.disposal, descriptor.rect));
                    return Ok(true);
                }
                EXTENSION_INTRODUCER => match reader.byte()? {
                    LABEL_GRAPHIC_CONTROL => control = read_graphic_control(&mut reader)?,
                    // A Plain Text Extension is a rendered block this decoder
                    // draws nothing for, so it consumes the control in force
                    // exactly as a frame would.
                    LABEL_PLAIN_TEXT => {
                        reader.skip_sub_blocks()?;
                        control = Control::DEFAULT;
                    }
                    LABEL_APPLICATION => {
                        read_application(&mut reader)?;
                    }
                    _ => reader.skip_sub_blocks()?,
                },
                // The structural pass proved the chain reaches its trailer
                // with `count` frames in it, so nothing else can be here.
                _ => return Err(DecodeError::GifUnknownBlock),
            }
        }
    }

    /// Apply the last frame's declared disposal to the canvas.
    fn dispose(&mut self) {
        let Some((disposal, rect)) = self.pending.take() else {
            return;
        };
        match disposal {
            Disposal::Keep => {}
            Disposal::Clear => {
                for row in 0..rect.height {
                    if let Some(span) = self.row_span(rect, row) {
                        if let Some(pixels) = self.canvas.get_mut(span) {
                            pixels.fill(0);
                        }
                    }
                }
            }
            Disposal::Previous => self.restore(rect),
        }
    }

    /// Bytes one canvas row occupies.
    fn row_bytes(&self) -> usize {
        usize::try_from(self.width).unwrap_or(usize::MAX) * RGBA_BYTES
    }

    /// The canvas byte range `rect`'s `row`-th row occupies, or `None` when
    /// any part of it falls outside the canvas.
    fn row_span(&self, rect: Rect, row: u32) -> Option<Range<usize>> {
        let y = usize::try_from(rect.top.checked_add(row)?).ok()?;
        let x = usize::try_from(rect.left).ok()?;
        let width = usize::try_from(rect.width).ok()?;
        let start = y
            .checked_mul(self.row_bytes())?
            .checked_add(x.checked_mul(RGBA_BYTES)?)?;
        let end = start.checked_add(width.checked_mul(RGBA_BYTES)?)?;
        (end <= self.canvas.len()).then_some(start..end)
    }

    /// Copy `rect` of the canvas aside, so a restore-to-previous disposal can
    /// put it back.
    fn save(&mut self, rect: Rect) -> Result<(), DecodeError> {
        let row_len = usize::try_from(rect.width).unwrap_or(usize::MAX) * RGBA_BYTES;
        let total = row_len
            .checked_mul(usize::try_from(rect.height).unwrap_or(usize::MAX))
            .ok_or(DecodeError::DimensionsOverflow)?;
        if !fallible::grow_to(&mut self.saved, total, 0u8) {
            return Err(DecodeError::OutOfMemory);
        }
        for row in 0..rect.height {
            let (Some(source), Some(into)) = (
                self.row_span(rect, row),
                usize::try_from(row).ok().map(|row| row * row_len),
            ) else {
                continue;
            };
            let (Some(from), Some(target)) = (
                self.canvas.get(source),
                self.saved.get_mut(into..into + row_len),
            ) else {
                continue;
            };
            target.copy_from_slice(from);
        }
        Ok(())
    }

    /// Put back what [`Self::save`] copied aside.
    ///
    /// Every restore is preceded by the save that filled `saved` with exactly
    /// this rectangle, so a row that does not resolve is unreachable and
    /// leaves the canvas as it stands rather than inventing pixels.
    fn restore(&mut self, rect: Rect) {
        let row_len = usize::try_from(rect.width).unwrap_or(usize::MAX) * RGBA_BYTES;
        for row in 0..rect.height {
            let (Some(target), Some(from)) = (
                self.row_span(rect, row),
                usize::try_from(row).ok().map(|row| row * row_len),
            ) else {
                continue;
            };
            let Some(source) = self.saved.get(from..from + row_len) else {
                continue;
            };
            if let Some(pixels) = self.canvas.get_mut(target) {
                pixels.copy_from_slice(source);
            }
        }
    }

    /// Expand one frame's indices and composite them onto the canvas,
    /// answering the offset of the block that follows it.
    fn draw(
        &mut self,
        descriptor: &Descriptor<'_>,
        control: Control,
    ) -> Result<usize, DecodeError> {
        let palette = descriptor
            .palette
            .or(self.global_palette)
            .ok_or(DecodeError::GifMissingColourTable)?;
        let rect = descriptor.rect;
        let pixels = usize::try_from(u64::from(rect.width) * u64::from(rect.height))
            .map_err(|_| DecodeError::DimensionsOverflow)?;
        if !fallible::grow_to(&mut self.indices, pixels, 0u8) {
            return Err(DecodeError::OutOfMemory);
        }
        let after = {
            let indices = self
                .indices
                .get_mut(..pixels)
                .ok_or(DecodeError::OutOfMemory)?;
            expand(
                self.bytes,
                descriptor.data,
                descriptor.min_code_size,
                &mut self.lzw,
                indices,
            )?
        };
        let stride = usize::try_from(rect.width).unwrap_or(usize::MAX);
        let entries = palette.len() / PALETTE_ENTRY_LEN;
        for stream_row in 0..rect.height {
            let frame_row = if descriptor.interlaced {
                interlaced_row(stream_row, rect.height)
            } else {
                stream_row
            };
            let Some(span) = self.row_span(rect, frame_row) else {
                return Err(DecodeError::GifFrameOutsideScreen);
            };
            let source = usize::try_from(stream_row).unwrap_or(usize::MAX) * stride;
            let (Some(row), Some(target)) = (
                self.indices.get(source..source + stride),
                self.canvas.get_mut(span),
            ) else {
                return Err(DecodeError::GifTruncatedImageData);
            };
            let (quads, _) = target.as_chunks_mut::<RGBA_BYTES>();
            for (index, pixel) in row.iter().zip(quads) {
                if control.transparent == Some(*index) {
                    continue;
                }
                if usize::from(*index) >= entries {
                    return Err(DecodeError::GifPaletteIndexOutOfRange);
                }
                let entry = usize::from(*index) * PALETTE_ENTRY_LEN;
                pixel[0] = palette[entry];
                pixel[1] = palette[entry + 1];
                pixel[2] = palette[entry + 2];
                pixel[3] = u8::MAX;
            }
        }
        Ok(after)
    }
}

/// Decode a GIF's first frame at the logical screen's size.
///
/// A still consumer — an icon, a wallpaper — wants one picture, and the first
/// composited frame is the one the format shows first.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let mut frames = Frames::open(bytes, limits)?;
    if !frames.step()? {
        return Err(DecodeError::GifNoFrames);
    }
    let (width, height) = (frames.width(), frames.height());
    let pixels = fallible::collected(frames.canvas().len(), frames.canvas().iter().copied())
        .ok_or(DecodeError::OutOfMemory)?;
    Ok(RasterImage::from_parts(width, height, pixels))
}

#[cfg(test)]
#[path = "gif_tests.rs"]
mod tests;
