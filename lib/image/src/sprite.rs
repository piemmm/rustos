//! A complete, fail-closed decoder for RISC OS sprite area files (Acorn
//! filetype `&FF9`).
//!
//! A sprite area is a container of independent, named pictures — an
//! application's whole icon set in one file — so it decodes as a page
//! container rather than as one picture. It carries **no signature**: its
//! first word is the sprite count, and RISC OS types a file from its
//! directory entry rather than from its content. A caller therefore names
//! this format rather than having it recognised ([`crate::probe_as`],
//! [`crate::decode_as`], [`crate::Sequence::open_as`]); recognising one from
//! a structural coincidence would be a false-positive machine, and one this
//! crate would then act on.
//!
//! # Readings the format's own text does not settle
//!
//! **A sprite with no palette does not state its colours.** RISC OS resolves
//! those against whatever palette the display holds, so a file decoded away
//! from a display has none to resolve against, and this decoder adopts the
//! palette the OS itself assigns on entering a mode of that depth. At eight
//! bits that is not a table but the arrangement the Programmer's Reference
//! Manual gives for a screen-memory byte — four bits per channel, the low
//! two shared as the tint — so an 8bpp sprite needs no palette of its own to
//! decode exactly.
//!
//! **A short palette is the VIDC1 arrangement, not a truncated one.** VIDC
//! holds sixteen palette registers, so most 256-colour sprites carry sixteen
//! entries and those written by `*ScreenSave` carry sixty-four; RISC OS
//! passes the *last* sixteen to the hardware, and a pixel's top four bits
//! then override supremacy bits of the entry its low four selected. A
//! palette long enough for the depth is read straight through instead, which
//! is what a full 256-entry palette is for.
//!
//! **A mask supersedes a pixel's own alpha.** The mask is what the format
//! calls a sprite's transparency, so a file carrying both it and an alpha
//! channel is contradicting itself; taking the mask keeps one answer rather
//! than inventing arithmetic over two.
//!
//! Sprite names are read over rather than reported: nothing addresses a
//! sprite by name yet, and a page index is what the shared sequence shape
//! offers. CMYK, JPEG-data, and YCbCr sprite types are refused by name
//! rather than half-read, because none is a depth this decoder claims.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::channel::{Channel, Sampler};
use crate::pages::{PageSource, Pages};
use crate::{DecodeError, DecodeLimits, RasterImage, RGBA_BYTES};

/// A file holds the sprite area control block without its first word — the
/// area's total size, which the file's own length already gives — so every
/// offset the file states is four greater than the position it names.
const AREA_HEADER_LEN: u32 = 12;
const AREA_OFFSET_BIAS: u32 = 4;

/// One sprite's control block, and the offsets within it this decoder reads.
/// Its name occupies bytes 4 to 15 and is not one of them.
const SPRITE_HEADER_LEN: u32 = 44;
const NEXT_AT: u32 = 0;
const WIDTH_WORDS_AT: u32 = 16;
const HEIGHT_AT: u32 = 20;
const FIRST_BIT_AT: u32 = 24;
const LAST_BIT_AT: u32 = 28;
const IMAGE_AT: u32 = 32;
const MASK_AT: u32 = 36;
const MODE_AT: u32 = 40;

/// Bytes one palette entry occupies: the pair of words `OS_ReadPalette`
/// returns, which differ only for a flashing colour.
const PALETTE_ENTRY_LEN: u32 = 8;

/// Palette entries VIDC itself holds, which is what a sprite carrying fewer
/// than its depth needs is supplying.
const VIDC_ENTRIES: u32 = 16;

/// Bits per pixel each numbered screen mode holds, indexed by mode number;
/// zero for mode 7, whose Teletext cells are character codes rather than
/// pixels. Numbers past this table are third-party extension modes, whose
/// depth only the module that defined them knows.
const MODE_BITS: [u8; 54] = [
    1, 2, 4, 1, 1, 2, 1, 0, 2, 4, 8, 2, 4, 8, 4, 8, 4, 4, 1, 2, 4, 8, 4, 1, 8, 1, 2, 4, 8, 1, 2, 4,
    8, 1, 2, 4, 8, 1, 2, 4, 8, 1, 2, 4, 1, 2, 4, 8, 4, 8, 1, 2, 4, 8,
];

/// The colours RISC OS assigns on entering a mode, in the order a logical
/// colour indexes them. A sixteen-colour mode's palette is these eight
/// twice, the second eight being the flashing pairs' first colour.
const DEFAULT_COLOURS: [[u8; 3]; 8] = [
    [0x00, 0x00, 0x00],
    [0xFF, 0x00, 0x00],
    [0x00, 0xFF, 0x00],
    [0xFF, 0xFF, 0x00],
    [0x00, 0x00, 0xFF],
    [0xFF, 0x00, 0xFF],
    [0x00, 0xFF, 0xFF],
    [0xFF, 0xFF, 0xFF],
];

/// A two-colour mode's own assignment. Neither it nor the four-colour one
/// below is a prefix of the eight.
const DEFAULT_COLOURS_2: [[u8; 3]; 2] = [[0x00, 0x00, 0x00], [0xFF, 0xFF, 0xFF]];

/// A four-colour mode's own assignment.
const DEFAULT_COLOURS_4: [[u8; 3]; 4] = [
    [0x00, 0x00, 0x00],
    [0xFF, 0x00, 0x00],
    [0xFF, 0xFF, 0x00],
    [0xFF, 0xFF, 0xFF],
];

/// The word at `at`, or a refusal where the input holds no word there.
fn word(bytes: &[u8], at: u32) -> Result<u32, DecodeError> {
    usize::try_from(at)
        .ok()
        .and_then(|at| crate::le_u32(bytes, at))
        .ok_or(DecodeError::SpriteTruncated)
}

/// The `len` bytes at `base + offset`.
fn slice_at(bytes: &[u8], base: u32, offset: u32, len: u32) -> Result<&[u8], DecodeError> {
    let start = base
        .checked_add(offset)
        .ok_or(DecodeError::SpriteTruncated)?;
    let end = start.checked_add(len).ok_or(DecodeError::SpriteTruncated)?;
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return Err(DecodeError::SpriteTruncated);
    };
    bytes.get(start..end).ok_or(DecodeError::SpriteTruncated)
}

/// Which colour a packed pixel's lowest field carries, and whether its
/// highest is alpha rather than an unused byte.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Order {
    red_lowest: bool,
    alpha: bool,
}

/// The channel order every sprite mode word but a RISC OS 5 one implies.
const VIDC_ORDER: Order = Order {
    red_lowest: true,
    alpha: false,
};

/// How one pixel's bits sit in a row.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Layout {
    /// A palette index of 1, 2, 4, or 8 bits, least significant pixel
    /// leftmost.
    Indexed { bits: u32 },
    /// A little-endian value of 2, 3, or 4 bytes, its channels cut out by
    /// fields the sprite type fixes.
    Packed {
        bytes: u32,
        channels: [Channel; RGBA_BYTES],
    },
}

impl Layout {
    const fn bits(self) -> u32 {
        match self {
            Self::Indexed { bits } => bits,
            Self::Packed { bytes, .. } => bytes * 8,
        }
    }
}

/// How wide one pixel of a sprite's mask is, and so how it is read.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum MaskDepth {
    /// An old-format sprite's mask is the image's own depth and shares its
    /// row layout; only whether a pixel's bits are all clear is read.
    Image,
    /// A new-format sprite's mask is one bit per pixel, starting at bit zero
    /// of rows of its own.
    Bit,
    /// A wide mask is eight bits per pixel, read as alpha.
    Alpha,
}

/// What a sprite mode word says about the pixels it describes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Mode {
    layout: Layout,
    mask: MaskDepth,
    /// A numbered mode, the only kind whose rows may begin part way into
    /// their first word.
    numbered: bool,
}

/// Cut a packed pixel's channels out of the widths a sprite type fixes,
/// lowest field first.
fn packed_layout(widths: [u32; RGBA_BYTES], bytes: u32, order: Order) -> Layout {
    let mid = Channel::fixed(widths[0], widths[1]);
    let low = Channel::fixed(0, widths[0]);
    let high = Channel::fixed(widths[0] + widths[1], widths[2]);
    let (red, blue) = if order.red_lowest {
        (low, high)
    } else {
        (high, low)
    };
    let top = if order.alpha {
        Channel::fixed(widths[0] + widths[1] + widths[2], widths[3])
    } else {
        Channel::ABSENT
    };
    Layout::Packed {
        bytes,
        channels: [red, mid, blue, top],
    }
}

/// The pixel format a sprite type names.
fn layout_for(sprite_type: u32, order: Order) -> Result<Layout, DecodeError> {
    let (widths, bytes) = match sprite_type {
        1 => return Ok(Layout::Indexed { bits: 1 }),
        2 => return Ok(Layout::Indexed { bits: 2 }),
        3 => return Ok(Layout::Indexed { bits: 4 }),
        4 => return Ok(Layout::Indexed { bits: 8 }),
        5 => ([5, 5, 5, 1], 2),
        6 => ([8, 8, 8, 8], 4),
        8 => ([8, 8, 8, 0], 3),
        10 => ([5, 6, 5, 0], 2),
        16 => ([4, 4, 4, 4], 2),
        _ => return Err(DecodeError::SpriteUnsupportedType),
    };
    // Asking for alpha where the format has no fourth field is a mode word
    // contradicting itself, and dropping the flag would hide that.
    if order.alpha && widths[3] == 0 {
        return Err(DecodeError::SpriteInvalidModeWord);
    }
    Ok(packed_layout(widths, bytes, order))
}

/// The channel order mode-flags bits 12 to 15 name. Only the RGB family is a
/// depth this decoder claims; the CMYK and YCbCr families are colour spaces
/// of their own.
fn data_format(flags: u32) -> Result<Order, DecodeError> {
    if flags >> 4 & 0x3 != 0 {
        return Err(DecodeError::SpriteUnsupportedType);
    }
    Ok(Order {
        red_lowest: flags >> 6 & 1 == 0,
        alpha: flags >> 7 & 1 == 1,
    })
}

/// What a numbered screen mode says about its pixels.
fn numbered_mode(mode: u32) -> Result<Mode, DecodeError> {
    let bits = usize::try_from(mode)
        .ok()
        .and_then(|mode| MODE_BITS.get(mode))
        .copied()
        .filter(|&bits| bits != 0)
        .ok_or(DecodeError::SpriteUnknownMode)?;
    Ok(Mode {
        layout: Layout::Indexed {
            bits: u32::from(bits),
        },
        mask: MaskDepth::Image,
        numbered: true,
    })
}

/// Decode a sprite mode word.
///
/// A word under 256 is a screen mode number; one with bit 0 clear is a
/// pointer to a mode selector block, which no file can hold; the rest are
/// the two sprite mode word formats, told apart by whether bits 27 to 30 are
/// all set.
fn read_mode(value: u32) -> Result<Mode, DecodeError> {
    if value < 256 {
        return numbered_mode(value);
    }
    if value & 1 == 0 {
        return Err(DecodeError::SpriteInvalidModeWord);
    }
    let (sprite_type, order) = if value >> 27 & 0xF == 0xF {
        // A RISC OS 5 word: a fixed pattern in bits 27-30, 16-19, and 0-3,
        // a seven-bit type, and mode-flags bits 8-15.
        if value & 0x000F_000F != 1 {
            return Err(DecodeError::SpriteInvalidModeWord);
        }
        (value >> 20 & 0x7F, data_format(value >> 8 & 0xFF)?)
    } else {
        // A RISC OS 3.5 word: a four-bit type over two 13-bit DPI fields,
        // neither of which a valid word leaves zero.
        if (value >> 1).trailing_zeros() >= 13 || (value >> 14).trailing_zeros() >= 13 {
            return Err(DecodeError::SpriteInvalidModeWord);
        }
        (value >> 27 & 0xF, VIDC_ORDER)
    };
    Ok(Mode {
        layout: layout_for(sprite_type, order)?,
        // Bit 31 widens the mask from one bit per pixel to eight.
        mask: if value >> 31 & 1 == 1 {
            MaskDepth::Alpha
        } else {
            MaskDepth::Bit
        },
        numbered: false,
    })
}

/// One sprite's control block, every field already validated against the
/// others.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Header {
    mode: Mode,
    width: u32,
    height: u32,
    /// Bits of the first word of a row that precede its first pixel.
    left_wastage: u32,
    /// Bytes one row of the image occupies, rows being word-aligned.
    stride: u32,
    image_at: u32,
    /// Where the mask sits, or `None` where the sprite has none.
    mask_at: Option<u32>,
    palette_entries: u32,
    /// Bytes this sprite occupies, control block included.
    length: u32,
}

impl Header {
    fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// The pixel width the wastage fields leave, refusing a row that does not
/// hold a whole number of pixels.
///
/// A pixel never straddles a word at a depth that divides 32, so wastage
/// there has to land on a pixel boundary; at 24 bits, which does not, a row
/// simply holds whole pixels and starts at bit zero.
fn pixel_width(words_less_one: u32, left: u32, last: u32, bits: u32) -> Result<u32, DecodeError> {
    if left > 31 || last > 31 {
        return Err(DecodeError::SpriteInvalidWastage);
    }
    let boundary = if 32_u32.is_multiple_of(bits) { bits } else { 8 };
    if !left.is_multiple_of(boundary) {
        return Err(DecodeError::SpriteInvalidWastage);
    }
    let used = (u64::from(words_less_one) * 32 + u64::from(last) + 1)
        .checked_sub(u64::from(left))
        .filter(|used| *used > 0 && used.is_multiple_of(u64::from(bits)))
        .ok_or(DecodeError::SpriteInvalidWastage)?;
    u32::try_from(used / u64::from(bits)).map_err(|_| DecodeError::DimensionsOverflow)
}

/// Read the control block at `at`, and with it the sprite's true geometry.
fn read_header(bytes: &[u8], at: u32) -> Result<Header, DecodeError> {
    let field = |offset: u32| {
        at.checked_add(offset)
            .ok_or(DecodeError::SpriteTruncated)
            .and_then(|at| word(bytes, at))
    };
    let length = field(NEXT_AT)?;
    if length < SPRITE_HEADER_LEN {
        return Err(DecodeError::SpriteBadArea);
    }
    let mode = read_mode(field(MODE_AT)?)?;
    let bits = mode.layout.bits();
    let words_less_one = field(WIDTH_WORDS_AT)?;
    let height = field(HEIGHT_AT)?
        .checked_add(1)
        .ok_or(DecodeError::DimensionsOverflow)?;
    let left_wastage = field(FIRST_BIT_AT)?;
    // Only a numbered mode's rows may begin part way into their first word;
    // every sprite mode word requires the first pixel at bit zero.
    if !mode.numbered && left_wastage != 0 {
        return Err(DecodeError::SpriteInvalidWastage);
    }
    let width = pixel_width(words_less_one, left_wastage, field(LAST_BIT_AT)?, bits)?;
    let image_at = field(IMAGE_AT)?;
    let mask_field = field(MASK_AT)?;
    let palette_bytes = image_at
        .min(mask_field)
        .checked_sub(SPRITE_HEADER_LEN)
        .ok_or(DecodeError::SpriteBadArea)?;
    Ok(Header {
        mode,
        width,
        height,
        left_wastage,
        stride: words_less_one
            .checked_add(1)
            .and_then(|words| words.checked_mul(4))
            .ok_or(DecodeError::DimensionsOverflow)?,
        image_at,
        mask_at: (mask_field != image_at).then_some(mask_field),
        // A palette whose length is not a whole number of entries states
        // nothing this decoder can read, so the depth's own colours stand.
        palette_entries: if palette_bytes.is_multiple_of(PALETTE_ENTRY_LEN) {
            palette_bytes / PALETTE_ENTRY_LEN
        } else {
            0
        },
        length,
    })
}

/// Bytes one row of a mask occupies at `bits` per pixel, word-aligned.
fn mask_stride(width: u32, bits: u32) -> Result<u32, DecodeError> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(bits))
        .and_then(|total| total.checked_add(31))
        .ok_or(DecodeError::DimensionsOverflow)?
        / 32
        * 4;
    u32::try_from(bytes).map_err(|_| DecodeError::DimensionsOverflow)
}

/// The colour every logical colour of an indexed sprite resolves to.
///
/// Resolved once per sprite rather than per pixel: at eight bits the VIDC
/// arrangement makes a pixel's colour a pure function of its byte, so
/// tabulating it leaves the row loop one indexed load.
struct IndexedPalette {
    entries: [[u8; RGBA_BYTES]; 256],
}

impl IndexedPalette {
    /// Resolve `bits`-deep logical colours against the sprite's own palette,
    /// or against the depth's default where it carries none this decoder can
    /// use.
    fn new(bits: u32, palette: &[u8], entries: u32) -> Self {
        let count = 1usize << bits;
        let mut table = [[0, 0, 0, u8::MAX]; 256];
        // Fewer entries than the depth needs is the VIDC1 arrangement rather
        // than a short palette: the last sixteen are the hardware registers,
        // and a pixel's top four bits override supremacy bits of the entry
        // its low four select.
        let vidc = entries < u32::try_from(count).unwrap_or(u32::MAX);
        let base = entries.saturating_sub(VIDC_ENTRIES);
        for (index, slot) in table.iter_mut().enumerate().take(count) {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let (rgb, supremacy) = match (vidc, entries >= VIDC_ENTRIES) {
                (false, _) => (Self::stored(palette, index), false),
                (true, true) => (Self::stored(palette, base + (index & 0xF)), bits == 8),
                (true, false) => (Self::default_for(bits, index), bits == 8),
            };
            *slot = if supremacy {
                Self::supremacy(rgb, index)
            } else {
                [rgb[0], rgb[1], rgb[2], u8::MAX]
            };
        }
        Self { entries: table }
    }

    /// The colour stored at `index`. An entry is `&BBGGRR00`, so it lands in
    /// memory as an unused byte and then red, green, and blue.
    fn stored(palette: &[u8], index: u32) -> [u8; 3] {
        let at = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(PALETTE_ENTRY_LEN as usize));
        let Some(entry) = at
            .and_then(|at| palette.get(at..))
            .and_then(<[u8]>::first_chunk::<4>)
        else {
            return [0, 0, 0];
        };
        [entry[1], entry[2], entry[3]]
    }

    /// The colour RISC OS assigns a logical colour on entering a mode of
    /// this depth. At eight bits the sixteen VIDC registers hold the tint in
    /// every channel's low two bits, red bit two, and blue bit two.
    fn default_for(bits: u32, index: u32) -> [u8; 3] {
        match bits {
            1 => DEFAULT_COLOURS_2[(index & 0x1) as usize],
            2 => DEFAULT_COLOURS_4[(index & 0x3) as usize],
            4 => DEFAULT_COLOURS[(index & 0x7) as usize],
            _ => {
                let tint = index & 0x3;
                let widen = |high: u32| u8::try_from((high << 2 | tint) * 0x11).unwrap_or(u8::MAX);
                [widen(index >> 2 & 1), widen(0), widen(index >> 3 & 1)]
            }
        }
    }

    /// Override the supremacy bits a 256-colour pixel's top four carry: red
    /// bit three, green bits two and three, and blue bit three.
    fn supremacy(rgb: [u8; 3], pixel: u32) -> [u8; RGBA_BYTES] {
        let widen = |nibble: u32| u8::try_from(nibble * 0x11).unwrap_or(u8::MAX);
        [
            widen((u32::from(rgb[0]) >> 4 & 0x7) | (pixel >> 4 & 1) << 3),
            widen((u32::from(rgb[1]) >> 4 & 0x3) | (pixel >> 5 & 1) << 2 | (pixel >> 6 & 1) << 3),
            widen((u32::from(rgb[2]) >> 4 & 0x7) | (pixel >> 7 & 1) << 3),
            u8::MAX,
        ]
    }
}

/// Read the `bits`-wide field at bit offset `bit` of a row, pixels never
/// straddling a byte at any depth this reads.
fn field_at(src: &[u8], bit: u64, bits: u32) -> u32 {
    let mask = u32::MAX >> (u32::BITS - bits);
    let byte = usize::try_from(bit / 8)
        .ok()
        .and_then(|at| src.get(at))
        .copied()
        .unwrap_or(0);
    (u32::from(byte) >> (bit % 8)) & mask
}

/// Expand one row of indexed pixels, least significant pixel leftmost.
fn expand_indexed(bits: u32, left: u32, palette: &IndexedPalette, src: &[u8], dst: &mut [u8]) {
    let mut bit = u64::from(left);
    for pixel in dst.as_chunks_mut::<RGBA_BYTES>().0 {
        *pixel = palette.entries[field_at(src, bit, bits) as usize];
        bit += u64::from(bits);
    }
}

/// Expand one row of packed pixels.
fn expand_packed(bytes: usize, samplers: &[Sampler; RGBA_BYTES], src: &[u8], dst: &mut [u8]) {
    for (x, pixel) in dst.as_chunks_mut::<RGBA_BYTES>().0.iter_mut().enumerate() {
        let raw = src
            .get(x * bytes..)
            .and_then(|rest| rest.get(..bytes))
            .unwrap_or_default()
            .iter()
            .rev()
            .fold(0u32, |value, &byte| value << 8 | u32::from(byte));
        for (channel, sampler) in pixel.iter_mut().zip(samplers) {
            *channel = sampler.sample(raw);
        }
    }
}

/// Apply a sprite's transparency mask over its already-expanded pixels.
fn apply_mask(
    header: &Header,
    mask: &[u8],
    row_bytes: usize,
    out: &mut [u8],
) -> Result<(), DecodeError> {
    // Only an old-format mask shares the image's row layout; a new-format
    // one starts at bit zero of rows of its own width. Either way a mask
    // pixel is at most a byte wide, so it never straddles one.
    let (bits, stride, left) = match header.mode.mask {
        MaskDepth::Image => (
            header.mode.layout.bits(),
            header.stride,
            header.left_wastage,
        ),
        MaskDepth::Bit => (1, mask_stride(header.width, 1)?, 0),
        MaskDepth::Alpha => (8, mask_stride(header.width, 8)?, 0),
    };
    let rows = stride
        .checked_mul(header.height)
        .ok_or(DecodeError::DimensionsOverflow)?;
    let mask = slice_at(mask, 0, 0, rows)?;
    let alpha = header.mode.mask == MaskDepth::Alpha;
    for (src, dst) in mask
        .chunks_exact(stride as usize)
        .zip(out.chunks_exact_mut(row_bytes))
    {
        let mut bit = u64::from(left);
        for pixel in dst.as_chunks_mut::<RGBA_BYTES>().0 {
            let value = field_at(src, bit, bits);
            // Every bit of a binary mask pixel is set or clear together, so
            // any set bit means the pixel is solid.
            pixel[3] = if alpha {
                u8::try_from(value).unwrap_or(u8::MAX)
            } else if value == 0 {
                0
            } else {
                u8::MAX
            };
            bit += u64::from(bits);
        }
    }
    Ok(())
}

/// Decode the sprite whose control block sits at `at`.
fn decode_sprite(bytes: &[u8], at: u32, limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let header = read_header(bytes, at)?;
    limits.check(header.width, header.height)?;
    // Everything a sprite states an offset for lies inside the sprite, so
    // the whole decode reads within its own extent.
    let sprite = slice_at(bytes, at, 0, header.length)?;
    let image = slice_at(
        sprite,
        0,
        header.image_at,
        header
            .stride
            .checked_mul(header.height)
            .ok_or(DecodeError::DimensionsOverflow)?,
    )?;
    let row_bytes = usize::try_from(header.width)
        .ok()
        .and_then(|width| width.checked_mul(RGBA_BYTES))
        .ok_or(DecodeError::DimensionsOverflow)?;
    let out_len = usize::try_from(header.height)
        .ok()
        .and_then(|height| height.checked_mul(row_bytes))
        .ok_or(DecodeError::DimensionsOverflow)?;
    let mut out: Vec<u8> = fallible::filled(out_len, 0u8).ok_or(DecodeError::OutOfMemory)?;

    let rows = out
        .chunks_exact_mut(row_bytes)
        .zip(image.chunks_exact(header.stride as usize));
    match header.mode.layout {
        Layout::Indexed { bits } => {
            let palette = slice_at(
                sprite,
                0,
                SPRITE_HEADER_LEN,
                header
                    .palette_entries
                    .checked_mul(PALETTE_ENTRY_LEN)
                    .ok_or(DecodeError::SpriteTruncated)?,
            )?;
            let palette = IndexedPalette::new(bits, palette, header.palette_entries);
            for (dst, src) in rows {
                expand_indexed(bits, header.left_wastage, &palette, src, dst);
            }
        }
        Layout::Packed { bytes, channels } => {
            let samplers = channels.map(Sampler::new);
            for (dst, src) in rows {
                expand_packed(bytes as usize, &samplers, src, dst);
            }
        }
    }

    if let Some(mask_at) = header.mask_at {
        let mask = usize::try_from(mask_at)
            .ok()
            .and_then(|at| sprite.get(at..))
            .ok_or(DecodeError::SpriteTruncated)?;
        apply_mask(&header, mask, row_bytes, &mut out)?;
    }
    Ok(RasterImage::from_parts(header.width, header.height, out))
}

/// A sprite area's own header: how many sprites it declares, where they
/// start, and where they end.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct AreaHeader {
    count: u32,
    first: u32,
    end: u32,
}

fn read_area(bytes: &[u8]) -> Result<AreaHeader, DecodeError> {
    let count = word(bytes, 0)?;
    if count == 0 {
        return Err(DecodeError::SpriteNoSprites);
    }
    let first = word(bytes, 4)?
        .checked_sub(AREA_OFFSET_BIAS)
        .ok_or(DecodeError::SpriteBadArea)?;
    let end = word(bytes, 8)?
        .checked_sub(AREA_OFFSET_BIAS)
        .ok_or(DecodeError::SpriteBadArea)?;
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    if first < AREA_HEADER_LEN || first > end || end > len {
        return Err(DecodeError::SpriteBadArea);
    }
    Ok(AreaHeader { count, first, end })
}

/// Where the sprite after the one at `at` begins.
///
/// Every step advances by at least a control block and stops at the area's
/// end, so a chain can neither loop nor outrun the file.
fn next_sprite(bytes: &[u8], area: &AreaHeader, at: u32) -> Result<u32, DecodeError> {
    let length = word(
        bytes,
        at.checked_add(NEXT_AT).ok_or(DecodeError::SpriteBadArea)?,
    )?;
    if length < SPRITE_HEADER_LEN {
        return Err(DecodeError::SpriteBadArea);
    }
    let next = at.checked_add(length).ok_or(DecodeError::SpriteBadArea)?;
    if next > area.end {
        return Err(DecodeError::SpriteBadArea);
    }
    Ok(next)
}

/// What a walk of the whole area measured, without decoding a pixel.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Measured {
    largest: u32,
    width: u32,
    height: u32,
}

/// A sprite area, as the pages a walk decodes.
///
/// The control blocks form a chain rather than a table, so the last one
/// located is kept: a sequential walk then costs one step per page instead
/// of re-walking the chain for each.
pub(crate) struct Area {
    header: AreaHeader,
    located: (u32, u32),
}

impl Area {
    /// Validate the area's chain and measure its sprites, decoding none.
    ///
    /// A control block that will not parse is fatal, because the chain is
    /// what finds the next sprite. A sprite whose *mode* this decoder does
    /// not claim is not: it is passed over here and refused only if it is
    /// asked for, exactly as one page of an icon file is. Where no sprite
    /// could be measured, the first refusal one raised is the answer,
    /// because that names a real reason.
    fn open(bytes: &[u8]) -> Result<(Self, Measured), DecodeError> {
        let header = read_area(bytes)?;
        let mut at = header.first;
        let mut measured: Option<Measured> = None;
        let mut refusal: Option<DecodeError> = None;
        for index in 0..header.count {
            if at >= header.end {
                return Err(DecodeError::SpriteBadArea);
            }
            match read_header(bytes, at) {
                Ok(sprite) => {
                    if measured.is_none_or(|best| {
                        sprite.area() > u64::from(best.width) * u64::from(best.height)
                    }) {
                        measured = Some(Measured {
                            largest: index,
                            width: sprite.width,
                            height: sprite.height,
                        });
                    }
                }
                Err(err) => refusal = refusal.or(Some(err)),
            }
            at = next_sprite(bytes, &header, at)?;
        }
        let measured = measured.ok_or_else(|| refusal.unwrap_or(DecodeError::SpriteNoSprites))?;
        Ok((
            Self {
                header,
                located: (0, header.first),
            },
            measured,
        ))
    }

    /// Where the control block of the sprite at `index` begins.
    fn locate(&mut self, bytes: &[u8], index: u32) -> Result<u32, DecodeError> {
        let (mut from, mut at) = if index >= self.located.0 {
            self.located
        } else {
            (0, self.header.first)
        };
        while from < index {
            at = next_sprite(bytes, &self.header, at)?;
            from += 1;
        }
        self.located = (index, at);
        Ok(at)
    }
}

impl PageSource for Area {
    fn count(&self) -> u32 {
        self.header.count
    }

    fn decode(
        &mut self,
        bytes: &[u8],
        index: u32,
        limits: &DecodeLimits,
    ) -> Result<RasterImage, DecodeError> {
        let at = self.locate(bytes, index)?;
        decode_sprite(bytes, at, limits)
    }
}

/// Read the geometry of the picture [`decode`] would answer, from control
/// blocks alone.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let (_, measured) = Area::open(bytes)?;
    Ok((measured.width, measured.height))
}

/// Decode the area's largest sprite at its natural size.
///
/// An area holds pictures rather than one picture at several sizes, so "the
/// picture" is a convention here: the largest is the only choice that never
/// silently answers a thumbnail, and it is the one a single-sprite file — a
/// saved screen, a lone icon — holds anyway. A caller that wants the others
/// walks the sequence.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let (mut area, measured) = Area::open(bytes)?;
    area.decode(bytes, measured.largest, limits)
}

/// Validate the area and measure its sprites, decoding none of them.
pub(crate) fn pages(bytes: &[u8], limits: &DecodeLimits) -> Result<Pages<Area>, DecodeError> {
    let (area, measured) = Area::open(bytes)?;
    Ok(Pages::new(area, limits, measured.width, measured.height))
}

#[cfg(test)]
#[path = "sprite_tests.rs"]
mod tests;
