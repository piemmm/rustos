//! A complete, fail-closed WEBP container decoder (WebP Container
//! Specification).
//!
//! A WEBP file is a RIFF form holding one or both of the two bitstreams
//! [`crate::vp8`] and [`crate::vp8l`] decode, plus the container's own
//! alpha plane and animation. Two forms exist: the simple one, a bare
//! `VP8 ` or `VP8L` chunk, and the extended one, a `VP8X` header declaring
//! a canvas over an optional alpha chunk and either one still bitstream or a
//! chain of `ANMF` frames.
//!
//! # Five readings the specification leaves to the decoder
//!
//! **The canvas is authoritative and a disagreeing frame is refused.** A
//! `VP8X` canvas is a 24-bit declaration while a bitstream carries its own
//! 14-bit one, so the two can disagree; reconciling them would mean
//! cropping, padding, or scaling, none of which either declaration asks
//! for. A simple-format file has no container geometry, so there the
//! bitstream's own size *is* the canvas.
//!
//! **Nothing is sized from an `ANMF`'s own declaration.** Its frame extent
//! is checked to lie inside the canvas, which allocates nothing, and the
//! frame is then decoded at the size its payload declares and refused
//! unless the two agree.
//!
//! **Which kind a file is comes from the file.** One carrying `ANIM` is an
//! animation; one without is a still picture, because there is no loop count
//! to report and answering "for ever" would fabricate a declaration.
//!
//! **The canvas clears and disposes to fully transparent.** The
//! specification makes `ANIM`'s background colour explicitly optional, and a
//! straight-alpha decoder's job is to carry a file's transparency out to its
//! consumer rather than pre-flatten it against a colour the viewer will draw
//! its own backdrop behind.
//!
//! **`VP8X`'s alpha and metadata flags are hints; the chunks present are the
//! fact.** They say what a file "contains", and the decode is driven by the
//! chunks actually found — so a set alpha flag with no alpha chunk decodes,
//! and an alpha chunk with a clear flag decodes, rather than either being
//! refused over a disagreement that costs nothing. The **animation** flag is
//! not a hint: it is what says whether the file is an animation at all, so it
//! and the chunks must agree. The reserved bits and reserved field values are
//! refused either way.
//!
//! Colour profiles and metadata are read past: `ICCP`, `EXIF`, and `XMP `
//! are skipped like any unknown chunk, because nothing in this crate
//! colour-manages and the output is RGBA8 in the file's own primaries.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::frames::{Animation, FrameSource};
use crate::{DecodeError, DecodeLimits, RasterImage, MAX_ANIMATION_FRAMES, RGBA_BYTES};

/// The form identifiers a WEBP file opens with, four bytes apart.
pub(crate) const RIFF_MAGIC: [u8; 4] = *b"RIFF";
pub(crate) const WEBP_FORM: [u8; 4] = *b"WEBP";

/// Where the form identifier sits: after `RIFF` and the size of everything
/// that follows it.
const FORM_AT: usize = 8;

/// Bytes a chunk header occupies: its identifier and its payload size.
const CHUNK_HEADER: usize = 8;

/// The chunk identifiers the container defines.
const VP8_LOSSY: [u8; 4] = *b"VP8 ";
const VP8_LOSSLESS: [u8; 4] = *b"VP8L";
const EXTENDED: [u8; 4] = *b"VP8X";
const ALPHA: [u8; 4] = *b"ALPH";
const ANIMATION: [u8; 4] = *b"ANIM";
const ANIMATION_FRAME: [u8; 4] = *b"ANMF";

/// Bytes the extended header occupies: a flag word and two 24-bit
/// dimensions.
const EXTENDED_LEN: usize = 10;

/// Bytes the animation header occupies: a background colour and a loop
/// count.
const ANIMATION_LEN: usize = 6;

/// Bytes an animation frame's own header occupies before its payload.
const FRAME_HEADER: usize = 16;

/// Bytes an alpha chunk spends on its method and filter declaration.
const ALPHA_HEADER: usize = 1;

/// The extended header's flag bits this decoder reads. Every other bit is
/// reserved and refused.
const FLAG_ANIMATION: u8 = 0x02;
const FLAG_RESERVED: u8 = 0x01;

/// The widest canvas the extended header can declare, as a 24-bit
/// dimension plus one.
const MAX_CANVAS: u32 = 1 << 24;

/// Whether `bytes` opens with the WEBP form identifiers.
///
/// Unlike every other format here the signature is in two parts with the
/// RIFF size between them, so it cannot be a prefix comparison against one
/// constant.
pub(crate) fn has_signature(bytes: &[u8]) -> bool {
    bytes.starts_with(&RIFF_MAGIC)
        && bytes
            .get(FORM_AT..FORM_AT + WEBP_FORM.len())
            .is_some_and(|form| form == WEBP_FORM)
}

/// A rectangle of the canvas.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Which bitstream a frame carries.
#[derive(Copy, Clone)]
enum Bitstream<'a> {
    Lossy(&'a [u8]),
    Lossless(&'a [u8]),
}

impl Bitstream<'_> {
    fn geometry(self) -> Result<(u32, u32), DecodeError> {
        match self {
            Self::Lossy(bytes) => crate::vp8::probe(bytes),
            Self::Lossless(bytes) => crate::vp8l::probe(bytes),
        }
    }

    fn decode(self, limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
        match self {
            Self::Lossy(bytes) => crate::vp8::decode(bytes, limits),
            Self::Lossless(bytes) => crate::vp8l::decode(bytes, limits),
        }
    }
}

/// One picture the container holds: a bitstream and, for a lossy one, the
/// alpha plane beside it.
#[derive(Copy, Clone)]
struct Picture<'a> {
    alpha: Option<&'a [u8]>,
    bitstream: Bitstream<'a>,
}

impl<'a> Picture<'a> {
    /// Read the `ALPH` and bitstream chunks one picture is made of.
    fn read(chunks: &mut Chunks<'a>, first: Chunk<'a>) -> Result<Self, DecodeError> {
        let mut alpha = None;
        let mut chunk = Some(first);
        while let Some(current) = chunk {
            match current.id {
                ALPHA => {
                    if alpha.is_some() {
                        return Err(DecodeError::WebpInvalidChunkLayout);
                    }
                    alpha = Some(current.payload);
                }
                VP8_LOSSY => {
                    return Ok(Self {
                        alpha,
                        bitstream: Bitstream::Lossy(current.payload),
                    })
                }
                VP8_LOSSLESS => {
                    // A lossless stream carries its own alpha channel, so an
                    // alpha chunk beside one is two answers to one question.
                    if alpha.is_some() {
                        return Err(DecodeError::WebpInvalidChunkLayout);
                    }
                    return Ok(Self {
                        alpha,
                        bitstream: Bitstream::Lossless(current.payload),
                    });
                }
                _ => {}
            }
            chunk = chunks.next()?;
        }
        Err(DecodeError::WebpInvalidChunkLayout)
    }

    fn decode(self, limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
        let (width, height) = self.bitstream.geometry()?;
        let mut image = self.bitstream.decode(limits)?;
        if let Some(chunk) = self.alpha {
            apply_alpha(&mut image, chunk, width, height)?;
        }
        Ok(image)
    }
}

/// One animation frame: where it goes, how long it shows, and how it meets
/// what is already on the canvas.
struct Frame<'a> {
    rect: Rect,
    duration_ms: u32,
    blend: bool,
    dispose: bool,
    picture: Picture<'a>,
}

/// A chunk of the RIFF form.
#[derive(Copy, Clone)]
struct Chunk<'a> {
    id: [u8; 4],
    payload: &'a [u8],
}

/// The chunks of one RIFF region, walked in order.
struct Chunks<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Chunks<'a> {
    /// Walk the chunk region of a whole file, refusing one whose declared
    /// RIFF region is not present.
    fn open(bytes: &'a [u8]) -> Result<Self, DecodeError> {
        if !has_signature(bytes) {
            return Err(DecodeError::WebpBadSignature);
        }
        let declared = usize::try_from(
            crate::le_u32(bytes, RIFF_MAGIC.len()).ok_or(DecodeError::WebpTruncated)?,
        )
        .unwrap_or(usize::MAX);
        // The size covers the form identifier and every chunk after it.
        let end = declared
            .checked_add(FORM_AT)
            .ok_or(DecodeError::WebpTruncated)?;
        if declared < WEBP_FORM.len() || end > bytes.len() {
            return Err(DecodeError::WebpTruncated);
        }
        Ok(Self {
            bytes: bytes.get(..end).ok_or(DecodeError::WebpTruncated)?,
            pos: FORM_AT + WEBP_FORM.len(),
        })
    }

    /// Walk a chunk's own payload as a chunk region, which is what an
    /// animation frame's payload is.
    fn nested(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn next(&mut self) -> Result<Option<Chunk<'a>>, DecodeError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let header = self
            .bytes
            .get(self.pos..self.pos + CHUNK_HEADER)
            .ok_or(DecodeError::WebpTruncated)?;
        let id = <[u8; 4]>::try_from(&header[..4]).map_err(|_| DecodeError::WebpTruncated)?;
        let size = usize::try_from(crate::le_u32(header, 4).ok_or(DecodeError::WebpTruncated)?)
            .unwrap_or(usize::MAX);
        let start = self.pos + CHUNK_HEADER;
        let end = start.checked_add(size).ok_or(DecodeError::WebpTruncated)?;
        let payload = self
            .bytes
            .get(start..end)
            .ok_or(DecodeError::WebpTruncated)?;
        // A chunk's payload is padded to an even length, and the pad byte is
        // outside the payload the size declares.
        self.pos = end
            .checked_add(size % 2)
            .ok_or(DecodeError::WebpTruncated)?;
        Ok(Some(Chunk { id, payload }))
    }
}

/// What the container declares itself to be.
enum Layout<'a> {
    Still {
        canvas: Option<(u32, u32)>,
        picture: Picture<'a>,
    },
    Animation {
        canvas: (u32, u32),
        loop_count: Option<u32>,
        frames: Vec<Frame<'a>>,
    },
}

/// Read the canvas an extended header declares.
fn read_canvas(payload: &[u8]) -> Result<(u32, u32), DecodeError> {
    let head = payload
        .get(..EXTENDED_LEN)
        .ok_or(DecodeError::WebpTruncated)?;
    if head[0] & FLAG_RESERVED != 0 || head[0] >> 6 != 0 || head[1..4] != [0, 0, 0] {
        return Err(DecodeError::WebpInvalidCanvas);
    }
    let width = le_u24(head, 4) + 1;
    let height = le_u24(head, 7) + 1;
    if width > MAX_CANVAS || height > MAX_CANVAS {
        return Err(DecodeError::WebpInvalidCanvas);
    }
    Ok((width, height))
}

/// A 24-bit little-endian field, which only this container uses.
fn le_u24(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 3).map_or(0, |field| {
        u32::from(field[0]) | (u32::from(field[1]) << 8) | (u32::from(field[2]) << 16)
    })
}

/// Read one animation frame's header and its payload's chunks.
fn read_frame(payload: &[u8], canvas: (u32, u32)) -> Result<Frame<'_>, DecodeError> {
    let head = payload
        .get(..FRAME_HEADER)
        .ok_or(DecodeError::WebpTruncated)?;
    // The offsets are in units of two pixels, so an odd placement cannot be
    // expressed and none is read.
    let rect = Rect {
        x: le_u24(head, 0) * 2,
        y: le_u24(head, 3) * 2,
        width: le_u24(head, 6) + 1,
        height: le_u24(head, 9) + 1,
    };
    if head[15] >> 2 != 0 {
        return Err(DecodeError::WebpInvalidChunkLayout);
    }
    let right = rect
        .x
        .checked_add(rect.width)
        .ok_or(DecodeError::WebpFrameOutsideCanvas)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .ok_or(DecodeError::WebpFrameOutsideCanvas)?;
    if right > canvas.0 || bottom > canvas.1 {
        return Err(DecodeError::WebpFrameOutsideCanvas);
    }
    let mut chunks = Chunks::nested(
        payload
            .get(FRAME_HEADER..)
            .ok_or(DecodeError::WebpTruncated)?,
    );
    let first = chunks.next()?.ok_or(DecodeError::WebpInvalidChunkLayout)?;
    Ok(Frame {
        rect,
        duration_ms: le_u24(head, 12),
        blend: head[15] & 0x02 == 0,
        dispose: head[15] & 0x01 != 0,
        picture: Picture::read(&mut chunks, first)?,
    })
}

/// Read a file's chunk chain and answer the form it declares.
fn layout(bytes: &[u8]) -> Result<Layout<'_>, DecodeError> {
    let mut chunks = Chunks::open(bytes)?;
    let first = chunks.next()?.ok_or(DecodeError::WebpInvalidChunkLayout)?;
    if first.id != EXTENDED {
        // The simple form is one bitstream chunk and nothing else, so the
        // animation and alpha chunks the extended header defines cannot
        // appear here.
        if first.id == ALPHA || first.id == ANIMATION || first.id == ANIMATION_FRAME {
            return Err(DecodeError::WebpInvalidChunkLayout);
        }
        return Ok(Layout::Still {
            canvas: None,
            picture: Picture::read(&mut chunks, first)?,
        });
    }
    let canvas = read_canvas(first.payload)?;
    let animated = first
        .payload
        .first()
        .is_some_and(|flags| flags & FLAG_ANIMATION != 0);
    let mut loop_count = None;
    let mut frames: Vec<Frame<'_>> = Vec::new();
    while let Some(chunk) = chunks.next()? {
        match chunk.id {
            ANIMATION => {
                let head = chunk
                    .payload
                    .get(..ANIMATION_LEN)
                    .ok_or(DecodeError::WebpTruncated)?;
                let declared = crate::le_u16(head, 4).ok_or(DecodeError::WebpTruncated)?;
                loop_count = (declared != 0).then(|| u32::from(declared));
            }
            ANIMATION_FRAME => {
                if u32::try_from(frames.len()).unwrap_or(u32::MAX) >= MAX_ANIMATION_FRAMES {
                    return Err(DecodeError::WebpTooManyFrames);
                }
                if !fallible::reserve(&mut frames, 1) {
                    return Err(DecodeError::OutOfMemory);
                }
                frames.push(read_frame(chunk.payload, canvas)?);
            }
            ALPHA | VP8_LOSSY | VP8_LOSSLESS => {
                if animated {
                    return Err(DecodeError::WebpInvalidChunkLayout);
                }
                return Ok(Layout::Still {
                    canvas: Some(canvas),
                    picture: Picture::read(&mut chunks, chunk)?,
                });
            }
            _ => {}
        }
    }
    if !animated {
        return Err(DecodeError::WebpInvalidChunkLayout);
    }
    if frames.is_empty() {
        return Err(DecodeError::WebpNoFrames);
    }
    Ok(Layout::Animation {
        canvas,
        loop_count,
        frames,
    })
}

/// Undo the row filtering an alpha plane declares, in place.
fn unfilter(plane: &mut [u8], width: usize, height: usize, method: u8) {
    for row in 0..height {
        let start = row * width;
        // The first row has no row above it, so every method predicts along
        // it; later rows predict from the row above as the method says.
        let vertical = method == 2 && row > 0;
        let gradient = method == 3 && row > 0;
        let mut left = if row == 0 {
            0
        } else {
            plane.get(start - width).copied().unwrap_or(0)
        };
        let mut above_left = left;
        for column in 0..width {
            let at = start + column;
            let above = if row == 0 {
                0
            } else {
                plane.get(at - width).copied().unwrap_or(0)
            };
            let predicted = if vertical {
                above
            } else if gradient {
                let value = i32::from(left) + i32::from(above) - i32::from(above_left);
                u8::try_from(value.clamp(0, 255)).unwrap_or(0)
            } else if method == 0 {
                0
            } else {
                left
            };
            let Some(sample) = plane.get_mut(at) else {
                continue;
            };
            *sample = sample.wrapping_add(predicted);
            above_left = above;
            left = *sample;
        }
    }
}

/// Decode an alpha chunk and write its plane into `image`'s alpha channel.
fn apply_alpha(
    image: &mut RasterImage,
    chunk: &[u8],
    width: u32,
    height: u32,
) -> Result<(), DecodeError> {
    let declaration = *chunk.first().ok_or(DecodeError::WebpTruncated)?;
    let method = declaration & 0x03;
    let filter = (declaration >> 2) & 0x03;
    let preprocessing = (declaration >> 4) & 0x03;
    // The pre-processing field is informative — it says what an encoder did,
    // not what a decoder must undo — so it is validated and then not acted
    // on: smoothing the stored values would invent pixels.
    if method > 1 || preprocessing > 1 || declaration >> 6 != 0 {
        return Err(DecodeError::WebpUnsupportedAlpha);
    }
    let body = chunk
        .get(ALPHA_HEADER..)
        .ok_or(DecodeError::WebpTruncated)?;
    let count = usize::try_from(u64::from(width) * u64::from(height))
        .map_err(|_| DecodeError::OutOfMemory)?;
    let mut plane = if method == 0 {
        let raw = body
            .get(..count)
            .ok_or(DecodeError::WebpAlphaGeometryMismatch)?;
        fallible::collected(count, raw.iter().copied()).ok_or(DecodeError::OutOfMemory)?
    } else {
        crate::vp8l::decode_alpha(body, width, height)?
    };
    if plane.len() != count {
        return Err(DecodeError::WebpAlphaGeometryMismatch);
    }
    unfilter(
        &mut plane,
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
        filter,
    );
    let pixels = image.pixels_mut();
    if pixels.len() != count * RGBA_BYTES {
        return Err(DecodeError::WebpAlphaGeometryMismatch);
    }
    for (pixel, &alpha) in pixels
        .as_chunks_mut::<RGBA_BYTES>()
        .0
        .iter_mut()
        .zip(&plane)
    {
        pixel[3] = alpha;
    }
    Ok(())
}

/// Read the canvas a file declares, decoding no pixels.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    match layout(bytes)? {
        Layout::Still {
            canvas: Some(canvas),
            picture,
        } => {
            check_canvas(picture.bitstream.geometry()?, canvas)?;
            Ok(canvas)
        }
        Layout::Still {
            canvas: None,
            picture,
        } => picture.bitstream.geometry(),
        Layout::Animation { canvas, .. } => Ok(canvas),
    }
}

/// Refuse a picture whose own geometry differs from the rectangle the
/// container gives it.
fn check_canvas(picture: (u32, u32), declared: (u32, u32)) -> Result<(), DecodeError> {
    if picture == declared {
        Ok(())
    } else {
        Err(DecodeError::WebpFrameGeometryMismatch)
    }
}

/// Decode a WEBP file's picture: the still one, or an animation's first
/// composited frame.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    match layout(bytes)? {
        Layout::Still { canvas, picture } => {
            let geometry = picture.bitstream.geometry()?;
            if let Some(canvas) = canvas {
                check_canvas(geometry, canvas)?;
            }
            limits.check(geometry.0, geometry.1)?;
            picture.decode(limits)
        }
        Layout::Animation {
            canvas,
            loop_count,
            frames,
        } => {
            let mut animation = Animation::new(Chain::new(canvas, loop_count, frames, limits)?);
            if !animation.step()? {
                return Err(DecodeError::WebpNoFrames);
            }
            let pixels =
                fallible::collected(animation.canvas().len(), animation.canvas().iter().copied())
                    .ok_or(DecodeError::OutOfMemory)?;
            Ok(RasterImage::from_parts(canvas.0, canvas.1, pixels))
        }
    }
}

/// What [`open`] found: a still picture's geometry, or an animation ready to
/// step.
pub(crate) enum Opened<'a> {
    Still { width: u32, height: u32 },
    Animation(Animation<Chain<'a>>),
}

/// Validate a file's structure and prepare whichever kind it is, decoding no
/// pixels.
pub(crate) fn open<'a>(bytes: &'a [u8], limits: &DecodeLimits) -> Result<Opened<'a>, DecodeError> {
    match layout(bytes)? {
        Layout::Still { canvas, picture } => {
            let (width, height) = picture.bitstream.geometry()?;
            if let Some(canvas) = canvas {
                check_canvas((width, height), canvas)?;
            }
            Ok(Opened::Still { width, height })
        }
        Layout::Animation {
            canvas,
            loop_count,
            frames,
        } => Ok(Opened::Animation(Animation::new(Chain::new(
            canvas, loop_count, frames, limits,
        )?))),
    }
}

/// An animation's frames, composited onto the canvas the container declares.
pub(crate) struct Chain<'a> {
    frames: Vec<Frame<'a>>,
    limits: DecodeLimits,
    width: u32,
    height: u32,
    loop_count: Option<u32>,
    /// The composition canvas: straight-alpha RGBA8, canvas-sized.
    canvas: Vec<u8>,
    /// The rectangle the last frame shown asks to be cleared before the
    /// next is drawn.
    pending: Option<Rect>,
}

impl<'a> Chain<'a> {
    fn new(
        canvas: (u32, u32),
        loop_count: Option<u32>,
        frames: Vec<Frame<'a>>,
        limits: &DecodeLimits,
    ) -> Result<Self, DecodeError> {
        limits.check(canvas.0, canvas.1)?;
        let len = usize::try_from(
            u64::from(canvas.0)
                .checked_mul(u64::from(canvas.1))
                .and_then(|pixels| pixels.checked_mul(RGBA_BYTES as u64))
                .ok_or(DecodeError::DimensionsOverflow)?,
        )
        .map_err(|_| DecodeError::DimensionsOverflow)?;
        Ok(Self {
            frames,
            limits: *limits,
            width: canvas.0,
            height: canvas.1,
            loop_count,
            canvas: fallible::filled(len, 0u8).ok_or(DecodeError::OutOfMemory)?,
            pending: None,
        })
    }

    /// The canvas byte range `rect`'s `row`-th row occupies, or `None` when
    /// any of it falls outside the canvas.
    fn row(&self, rect: Rect, row: u32) -> Option<core::ops::Range<usize>> {
        let y = usize::try_from(rect.y.checked_add(row)?).ok()?;
        let x = usize::try_from(rect.x).ok()?;
        let width = usize::try_from(rect.width).ok()?;
        let stride = usize::try_from(self.width).ok()?.checked_mul(RGBA_BYTES)?;
        let start = y
            .checked_mul(stride)?
            .checked_add(x.checked_mul(RGBA_BYTES)?)?;
        let end = start.checked_add(width.checked_mul(RGBA_BYTES)?)?;
        (end <= self.canvas.len()).then_some(start..end)
    }

    /// Clear the rectangle the previous frame asked to be disposed.
    fn dispose(&mut self) {
        let Some(rect) = self.pending.take() else {
            return;
        };
        for row in 0..rect.height {
            if let Some(span) = self.row(rect, row) {
                if let Some(pixels) = self.canvas.get_mut(span) {
                    pixels.fill(0);
                }
            }
        }
    }

    /// Draw a decoded frame into its rectangle, blending or overwriting as
    /// the frame asks.
    fn draw(&mut self, rect: Rect, blend_over: bool, picture: &RasterImage) {
        let stride = usize::try_from(rect.width).unwrap_or(0) * RGBA_BYTES;
        for row in 0..rect.height {
            let Some(span) = self.row(rect, row) else {
                continue;
            };
            let source = usize::try_from(row).unwrap_or(0) * stride;
            let (Some(from), Some(into)) = (
                picture.pixels().get(source..source + stride),
                self.canvas.get_mut(span),
            ) else {
                continue;
            };
            for (target, incoming) in into
                .as_chunks_mut::<RGBA_BYTES>()
                .0
                .iter_mut()
                .zip(from.as_chunks::<RGBA_BYTES>().0)
            {
                if blend_over {
                    blend(target, incoming);
                } else {
                    target.copy_from_slice(incoming);
                }
            }
        }
    }
}

/// Composite one straight-alpha pixel over another.
///
/// The specification's own formula, with the colour normalised against the
/// alpha actually stored so the two cannot disagree.
fn blend(target: &mut [u8], incoming: &[u8]) {
    let source_alpha = u32::from(incoming[3]);
    if source_alpha == u32::from(u8::MAX) {
        target.copy_from_slice(incoming);
        return;
    }
    if source_alpha == 0 {
        return;
    }
    let kept = u32::from(target[3]) * (u32::from(u8::MAX) - source_alpha);
    let alpha = source_alpha + kept / u32::from(u8::MAX);
    if alpha == 0 {
        target.fill(0);
        return;
    }
    for channel in 0..3 {
        let mixed = u32::from(incoming[channel]) * source_alpha * u32::from(u8::MAX)
            + u32::from(target[channel]) * kept;
        target[channel] = u8::try_from(mixed / (alpha * u32::from(u8::MAX))).unwrap_or(u8::MAX);
    }
    target[3] = u8::try_from(alpha).unwrap_or(u8::MAX);
}

impl FrameSource for Chain<'_> {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn count(&self) -> u32 {
        u32::try_from(self.frames.len()).unwrap_or(u32::MAX)
    }

    fn loop_count(&self) -> Option<u32> {
        self.loop_count
    }

    fn canvas(&self) -> &[u8] {
        &self.canvas
    }

    fn advance(&mut self, index: u32) -> Result<u64, DecodeError> {
        self.dispose();
        let frame = self
            .frames
            .get(usize::try_from(index).unwrap_or(usize::MAX))
            .ok_or(DecodeError::WebpNoFrames)?;
        let (rect, duration, dispose, blend_over, picture) = (
            frame.rect,
            frame.duration_ms,
            frame.dispose,
            frame.blend,
            frame.picture,
        );
        let decoded = picture.decode(&self.limits)?;
        if decoded.width() != rect.width || decoded.height() != rect.height {
            return Err(DecodeError::WebpFrameGeometryMismatch);
        }
        self.draw(rect, blend_over, &decoded);
        self.pending = dispose.then_some(rect);
        Ok(u64::from(duration) * 1_000_000)
    }

    fn restart(&mut self) {
        self.canvas.fill(0);
        self.pending = None;
    }
}

#[cfg(test)]
#[path = "webp_tests.rs"]
mod tests;
