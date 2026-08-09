//! The font-service IPC protocol (`plans/FONT-SERVICE.md` FS-1): the
//! reserved rendezvous the sandboxed OS font service (`fontd`) binds, and
//! the fixed-width, fail-closed requests a text-drawing client presents to
//! obtain a glyph's coverage bitmap, a family's line metrics, or the set of
//! installed families.
//!
//! Text rendering is a single, sandboxed OS resource (§16.4, §19.5): no
//! process but `fontd` holds a font face or an outline rasteriser, and a
//! client draws by asking this endpoint for the 8-bit coverage of one
//! Unicode scalar at a chosen pixel height. The transport carries no font
//! bytes and no outlines — only the small coverage bitmap the client
//! blits — so a malformed face can fault only the service's sandbox, never
//! the compositor or a terminal.
//!
//! # Proportional and monospace families are one protocol
//!
//! A request names the **family** it wants, and every glyph reply carries
//! that glyph's own pen advance and left side bearing. A monospace family
//! simply reports the same advance for every glyph and a
//! [`FontMetrics::monospace_advance`] a caller can lay a character grid out
//! with; a proportional family reports zero there, and its callers advance
//! the pen per glyph. There is one drawing path for both. A monospace
//! reply is also *shaped* like its cell — one cell wide, two for a
//! double-width scalar, with a zero bearing — so a grid blits it at the cell
//! origin.
//!
//! The protocol is modelled on [`crate::display_ipc`] / [`crate::mailbox_ipc`]:
//! a fixed-width [`FontRequest`] in, and a status-framed reply out. Drawing
//! text is not a security boundary, so the endpoint requires no capability
//! of its own (§5.2); the reply nonetheless validates every field and fails
//! closed on a corrupt frame. Every request and reply is versioned and
//! hashed under the same ABI discipline as the syscall table (§9) and
//! frozen on the first release — mutable now, `abi-v1` is not frozen.

use crate::le::{put_i32, put_u32, read_i32, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the font service (`"FNT"`
/// hex-spelled prefix, mirroring [`crate::mailbox_ipc::MAILBOX_ENDPOINT`]'s
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would feed forged glyph coverage to the compositor and
/// every app, so only the trusted `fontd` service may bind it. One endpoint
/// serves every client — requests carry the family, scalar and pixel height
/// in-protocol.
pub const FONT_ENDPOINT: u64 = 0x464E_5400;

/// Magic number identifying a font-service request (`"FNT1"` little-endian).
pub const FONT_REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"FNT1");

/// The `font-v1` protocol version.
pub const FONT_VERSION_V1: u16 = 1;

/// Smallest text height, in physical pixels, a client may request.
///
/// Below this a glyph loses the strokes that keep it legible; this mirrors
/// the client-side clamp and bounds the reply so a hostile caller cannot
/// demand a degenerate raster. A validation bound, not a capacity.
pub const FONT_MIN_PIXEL_HEIGHT: u32 = 8;

/// Largest text height, in physical pixels, a client may request.
///
/// Text this tall is already a large heading; the bound caps the coverage
/// bitmap a single request can force the service to rasterise and return, so
/// a pathological request cannot demand an unbounded raster. A validation
/// bound, not a capacity.
pub const FONT_MAX_PIXEL_HEIGHT: u32 = 512;

/// Largest glyph-bitmap width, in physical pixels, a reply may carry.
///
/// A glyph is at most about two ems wide (a full-width ideograph, a wide
/// ligature-like outline with overhang), and an em is never taller than the
/// permitted text height, so bounding against twice the maximum height caps
/// the reply independently of the requested size.
pub const FONT_MAX_GLYPH_WIDTH: u32 = 2 * FONT_MAX_PIXEL_HEIGHT;

/// Largest coverage payload, in bytes, a glyph reply may carry: one 8-bit
/// alpha sample per pixel of the widest, tallest permitted bitmap.
pub const FONT_MAX_COVERAGE_LEN: usize =
    (FONT_MAX_GLYPH_WIDTH as usize) * (FONT_MAX_PIXEL_HEIGHT as usize);

/// Bytes a family key occupies on the wire, NUL-padded.
pub const FONT_FAMILY_KEY_LEN: usize = 16;

/// Bytes a family's human-readable label occupies on the wire, NUL-padded.
pub const FONT_FAMILY_LABEL_LEN: usize = 32;

/// Most selectable families a [`FontRequest::Families`] reply may list.
///
/// The installed store is a curated OS set, not a user-extensible directory,
/// so this bounds the reply a client must be prepared to receive. A
/// validation bound, not a capacity.
pub const FONT_MAX_FAMILIES: usize = 16;

/// The key naming one installed font family — the directory name under
/// `/System/Fonts`, as a validated fixed-width wire value.
///
/// A key is 1..=[`FONT_FAMILY_KEY_LEN`] bytes of lowercase ASCII letters,
/// digits, and `-`, starting with a letter or digit. Constraining the
/// spelling here makes a key that could escape its directory — a `/`, a
/// `..`, a NUL in the middle — unrepresentable in an accepted request, so
/// the service never has to defend a path built from one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FamilyKey {
    /// The key's bytes, NUL-padded to the fixed wire width.
    bytes: [u8; FONT_FAMILY_KEY_LEN],
}

impl FamilyKey {
    /// The fixed-pitch family every image ships.
    ///
    /// The console atlas is generated from this family's primary face, so it
    /// is present on every image including a headless one — which makes it
    /// the family a surface falls back to when a stored preference names one
    /// the store does not hold.
    pub const MONO: Self = Self {
        bytes: *b"mono\0\0\0\0\0\0\0\0\0\0\0\0",
    };

    /// The key `name` spells.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when `name` is empty, longer than
    /// [`FONT_FAMILY_KEY_LEN`], starts with `-`, or carries a byte outside
    /// lowercase ASCII alphanumerics and `-`.
    pub const fn new(name: &str) -> Result<Self, Errno> {
        let source = name.as_bytes();
        if source.is_empty() || source.len() > FONT_FAMILY_KEY_LEN {
            return Err(Errno::OutOfRange);
        }
        let mut bytes = [0u8; FONT_FAMILY_KEY_LEN];
        let mut i = 0;
        while i < source.len() {
            let byte = source[i];
            let alphanumeric = byte.is_ascii_lowercase() || byte.is_ascii_digit();
            if !(alphanumeric || (byte == b'-' && i > 0)) {
                return Err(Errno::OutOfRange);
            }
            bytes[i] = byte;
            i += 1;
        }
        Ok(Self { bytes })
    }

    /// The key `bytes` carries, NUL-padded, validated as [`new`](Self::new)
    /// validates a name.
    ///
    /// # Errors
    ///
    /// [`Errno::BadMagic`] for any non-NUL byte after the first NUL — a
    /// smuggled second field in the padding, never silently ignored;
    /// [`Errno::OutOfRange`] for an empty key or a byte outside the
    /// permitted spelling.
    pub fn from_wire(bytes: [u8; FONT_FAMILY_KEY_LEN]) -> Result<Self, Errno> {
        let len = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(FONT_FAMILY_KEY_LEN);
        if bytes[len..].iter().any(|&byte| byte != 0) {
            return Err(Errno::BadMagic);
        }
        // `new` re-checks the spelling over exactly the non-padding bytes.
        let name = core::str::from_utf8(&bytes[..len]).map_err(|_| Errno::OutOfRange)?;
        Self::new(name)
    }

    /// The key's fixed-width NUL-padded wire bytes.
    #[must_use]
    pub const fn to_wire(self) -> [u8; FONT_FAMILY_KEY_LEN] {
        self.bytes
    }

    /// The key as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let len = self
            .bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(FONT_FAMILY_KEY_LEN);
        // The constructors admit only ASCII, so every prefix is valid UTF-8.
        core::str::from_utf8(&self.bytes[..len]).unwrap_or("")
    }
}

/// Whether a family lays text out on a fixed grid or by per-glyph advances.
///
/// A caller that needs a character grid — a terminal, a hex view, the
/// framebuffer console — requires a [`Monospace`](Self::Monospace) family;
/// desktop chrome uses whichever family the user chose.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum FamilyKind {
    /// Every glyph advances by the family's own single cell width.
    Monospace,
    /// Each glyph advances by its own width.
    Proportional,
}

impl FamilyKind {
    /// This kind's wire discriminant.
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Monospace => 1,
            Self::Proportional => 2,
        }
    }

    /// The kind `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a discriminant outside the closed set.
    pub const fn from_wire(wire: u8) -> Result<Self, Errno> {
        match wire {
            1 => Ok(Self::Monospace),
            2 => Ok(Self::Proportional),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The weight a run of text is set in.
///
/// A variable face carries a `wght` design axis, so these are *real*
/// weights: the service instantiates the outline at the axis value the
/// weight names and the advance changes with it, exactly as the type
/// designer drew. A face with no such axis is thickened instead, by a
/// bounded sub-pixel stroke that leaves its advance alone. Either way a
/// glyph reply states the advance it was rendered with, so layout never has
/// to assume one.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum FontWeight {
    /// Normal weight: body text, secondary detail, terminal text.
    #[default]
    Regular,
    /// A medium weight for titling text — an item's primary line, a window
    /// title, a panel heading.
    Medium,
    /// Bold weight: column headers and metric readouts.
    Bold,
}

impl FontWeight {
    /// This weight's wire discriminant.
    #[must_use]
    pub const fn to_wire(self) -> u16 {
        match self {
            Self::Regular => 1,
            Self::Medium => 2,
            Self::Bold => 3,
        }
    }

    /// The weight `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a discriminant outside the closed set, so an
    /// unknown weight is refused rather than silently rendered as Regular.
    pub const fn from_wire(wire: u16) -> Result<Self, Errno> {
        match wire {
            1 => Ok(Self::Regular),
            2 => Ok(Self::Medium),
            3 => Ok(Self::Bold),
            _ => Err(Errno::OutOfRange),
        }
    }

    /// The OpenType `wght` design-axis coordinate this weight names.
    ///
    /// These are the standard CSS/OpenType numeric weights, so a variable
    /// face instantiated here renders the weight its designer drew rather
    /// than an interpolation the protocol invented.
    #[must_use]
    pub const fn axis_value(self) -> u16 {
        match self {
            Self::Regular => 400,
            Self::Medium => 500,
            Self::Bold => 700,
        }
    }
}

/// One font-service operation (`plans/FONT-SERVICE.md` FS-1).
///
/// Carrying the scalar as a [`char`] and the family as a [`FamilyKey`] makes
/// an illegal request unrepresentable once decoded: a surrogate, an
/// out-of-range code point, or a key that could escape its directory is
/// rejected before the request is ever built.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FontRequest {
    /// Render the coverage of `scalar` from `family`, sized so the line box
    /// is `pixel_height` pixels tall.
    ///
    /// The service resolves the scalar to the covering face and returns the
    /// 8-bit coverage bitmap with the advance and bearing to draw it by; a
    /// scalar no face covers renders the U+FFFD replacement glyph rather
    /// than being refused.
    Glyph {
        /// The family to render from.
        family: FamilyKey,
        /// The Unicode scalar to render.
        scalar: char,
        /// The line-box height in physical pixels
        /// ([`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`]).
        pixel_height: u32,
        /// The weight to render at.
        weight: FontWeight,
    },
    /// Report `family`'s line metrics at `pixel_height`, so the client can
    /// lay text out without holding any font data.
    Metrics {
        /// The family to measure.
        family: FamilyKey,
        /// The line-box height in physical pixels
        /// ([`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`]).
        pixel_height: u32,
        /// The weight to measure, whose advances a variable face varies.
        weight: FontWeight,
    },
    /// List the installed selectable families, so a settings surface can
    /// offer exactly what the store holds rather than a compiled-in list.
    Families,
}

/// Wire operation discriminant of [`FontRequest::Glyph`].
const OP_GLYPH: u16 = 1;
/// Wire operation discriminant of [`FontRequest::Metrics`].
const OP_METRICS: u16 = 2;
/// Wire operation discriminant of [`FontRequest::Families`].
const OP_FAMILIES: u16 = 3;

/// Offset of the weight field in a request frame.
const REQUEST_WEIGHT: usize = 8;
/// Offset of the reserved halfword that follows the weight.
const REQUEST_RESERVED: usize = 10;
/// Offset of the pixel-height field in a request frame.
const REQUEST_HEIGHT: usize = 12;
/// Offset of the scalar field in a request frame.
const REQUEST_SCALAR: usize = 16;
/// Offset of the family key in a request frame.
const REQUEST_FAMILY: usize = 20;

impl FontRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), weight (2),
    /// a reserved halfword, pixel height (4), scalar (4), and the
    /// [`FONT_FAMILY_KEY_LEN`]-byte family key. Every field an operation
    /// does not use is zero.
    pub const WIRE_LEN: usize = REQUEST_FAMILY + FONT_FAMILY_KEY_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        put_u32(&mut out, 0, FONT_REQUEST_MAGIC);
        out[4..6].copy_from_slice(&FONT_VERSION_V1.to_le_bytes());
        let mut write = |op: u16, family: Option<FamilyKey>, height: u32, scalar: u32, weight| {
            out[6..8].copy_from_slice(&op.to_le_bytes());
            out[REQUEST_WEIGHT..REQUEST_RESERVED].copy_from_slice(&u16::to_le_bytes(weight));
            put_u32(&mut out, REQUEST_HEIGHT, height);
            put_u32(&mut out, REQUEST_SCALAR, scalar);
            if let Some(family) = family {
                out[REQUEST_FAMILY..Self::WIRE_LEN].copy_from_slice(&family.to_wire());
            }
        };
        match *self {
            Self::Glyph {
                family,
                scalar,
                pixel_height,
                weight,
            } => write(
                OP_GLYPH,
                Some(family),
                pixel_height,
                scalar as u32,
                weight.to_wire(),
            ),
            Self::Metrics {
                family,
                pixel_height,
                weight,
            } => write(OP_METRICS, Some(family), pixel_height, 0, weight.to_wire()),
            Self::Families => write(OP_FAMILIES, None, 0, 0, 0),
        }
        out
    }

    /// Decode a request from `bytes`, failing closed on any malformed input.
    ///
    /// Every bound a decoder can already see — the pixel-height range, the
    /// scalar's validity, the family key's spelling, and the zeroing of
    /// every field the operation does not use — is enforced here, so no
    /// accepted request ever carries a value the service would have to
    /// re-reject structurally.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, or a non-zero byte in a field
    ///   the operation does not use.
    /// * [`Errno::AbiVersionUnsupported`] — not `font-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set, a
    ///   `scalar` that is not a Unicode scalar value, or a weight outside
    ///   [`FontWeight`]'s closed set.
    /// * [`Errno::LengthOutOfRange`] — a pixel height outside
    ///   [`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        if read_u32(bytes, 0) != FONT_REQUEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        if u16::from_le_bytes([bytes[4], bytes[5]]) != FONT_VERSION_V1 {
            return Err(Errno::AbiVersionUnsupported);
        }
        if bytes[REQUEST_RESERVED..REQUEST_HEIGHT]
            .iter()
            .any(|&b| b != 0)
        {
            return Err(Errno::BadMagic);
        }
        let op = u16::from_le_bytes([bytes[6], bytes[7]]);
        let weight_wire = u16::from_le_bytes([bytes[REQUEST_WEIGHT], bytes[REQUEST_WEIGHT + 1]]);
        let scalar_wire = read_u32(bytes, REQUEST_SCALAR);
        let height_wire = read_u32(bytes, REQUEST_HEIGHT);
        match op {
            OP_GLYPH => Ok(Self::Glyph {
                family: family_field(bytes)?,
                scalar: char::from_u32(scalar_wire).ok_or(Errno::OutOfRange)?,
                pixel_height: validate_pixel_height(height_wire)?,
                weight: FontWeight::from_wire(weight_wire)?,
            }),
            OP_METRICS => {
                if scalar_wire != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Metrics {
                    family: family_field(bytes)?,
                    pixel_height: validate_pixel_height(height_wire)?,
                    weight: FontWeight::from_wire(weight_wire)?,
                })
            }
            OP_FAMILIES => {
                let unused_zero = weight_wire == 0
                    && scalar_wire == 0
                    && height_wire == 0
                    && bytes[REQUEST_FAMILY..Self::WIRE_LEN]
                        .iter()
                        .all(|&b| b == 0);
                if unused_zero {
                    Ok(Self::Families)
                } else {
                    Err(Errno::BadMagic)
                }
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The family key a request frame carries.
fn family_field(bytes: &[u8]) -> Result<FamilyKey, Errno> {
    let mut key = [0u8; FONT_FAMILY_KEY_LEN];
    key.copy_from_slice(&bytes[REQUEST_FAMILY..FontRequest::WIRE_LEN]);
    FamilyKey::from_wire(key)
}

/// Accept a text height only within
/// [`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`].
fn validate_pixel_height(height: u32) -> Result<u32, Errno> {
    if (FONT_MIN_PIXEL_HEIGHT..=FONT_MAX_PIXEL_HEIGHT).contains(&height) {
        Ok(height)
    } else {
        Err(Errno::LengthOutOfRange)
    }
}

/// Fixed prefix of a [`FontRequest::Glyph`] reply: a status word (`0` on
/// success, else the negated [`Errno`] discriminant) followed by the
/// bitmap's width and height, the pen advance, and the left side bearing,
/// each a little-endian 32-bit value. The 8-bit coverage samples follow,
/// `width * height` of them.
pub const FONT_GLYPH_REPLY_HEADER_LEN: usize = 20;

/// Largest [`FontRequest::Glyph`] reply, in bytes: the header plus the
/// widest, tallest permitted coverage bitmap. A client sizes its receive
/// buffer to this.
pub const FONT_MAX_GLYPH_REPLY: usize = FONT_GLYPH_REPLY_HEADER_LEN + FONT_MAX_COVERAGE_LEN;

/// A decoded glyph-coverage reply: where the glyph sits relative to the pen,
/// how far the pen then moves, and a borrowed view of its `width * height`
/// 8-bit alpha samples, row-major, that the client blits.
///
/// A glyph with no ink — a space, a zero-width mark with an empty outline —
/// carries `width == 0` and no samples, and is drawn by advancing the pen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphCoverage<'a> {
    /// Bitmap width in pixels (`0..=FONT_MAX_GLYPH_WIDTH`).
    pub width: u32,
    /// Bitmap height in pixels
    /// ([`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`]).
    pub height: u32,
    /// The pen advance for this glyph in pixels. Zero for a combining mark
    /// that occupies no space of its own.
    pub advance: u32,
    /// The bitmap's left edge relative to the pen, in pixels. Negative when
    /// the outline reaches back over the preceding glyph.
    pub left: i32,
    /// The `width * height` row-major 8-bit coverage samples.
    pub coverage: &'a [u8],
}

/// Encode a successful glyph-coverage reply into `buf`, returning the number
/// of bytes written.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — a geometry outside the permitted bounds
///   (see [`GlyphCoverage`]), or a `coverage` whose length is not exactly
///   `width * height`.
/// * [`Errno::BufferTooSmall`] — `buf` cannot hold the header plus coverage.
pub fn encode_glyph_reply(buf: &mut [u8], glyph: &GlyphCoverage<'_>) -> Result<usize, Errno> {
    let len = glyph_coverage_len(glyph.width, glyph.height, glyph.advance, glyph.left)?;
    if glyph.coverage.len() != len {
        return Err(Errno::LengthOutOfRange);
    }
    let total = FONT_GLYPH_REPLY_HEADER_LEN + len;
    if buf.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    put_u32(buf, 4, glyph.width);
    put_u32(buf, 8, glyph.height);
    put_u32(buf, 12, glyph.advance);
    put_i32(buf, 16, glyph.left);
    buf[FONT_GLYPH_REPLY_HEADER_LEN..total].copy_from_slice(glyph.coverage);
    Ok(total)
}

/// Encode a fail-closed glyph-reply refusal (a status word only) into `buf`.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the status word.
pub fn encode_glyph_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(4)
}

/// Decode a glyph-coverage reply, borrowing its coverage bytes from `reply`.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — `reply` is shorter than the status word, or
///   shorter than the header plus the coverage its geometry implies (a
///   truncated frame is refused, never read past its bytes).
/// * [`Errno::OutOfRange`] — a positive or undefined status word (wire
///   corruption — fail closed).
/// * [`Errno::LengthOutOfRange`] — a geometry outside the permitted bounds.
pub fn decode_glyph_reply(reply: &[u8]) -> Result<GlyphCoverage<'_>, Errno> {
    if reply.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    let status = read_i32(reply, 0);
    if status != 0 {
        let errno = status
            .checked_neg()
            .and_then(Errno::from_i32)
            .ok_or(Errno::OutOfRange)?;
        return Err(errno);
    }
    if reply.len() < FONT_GLYPH_REPLY_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let width = read_u32(reply, 4);
    let height = read_u32(reply, 8);
    let advance = read_u32(reply, 12);
    let left = read_i32(reply, 16);
    let len = glyph_coverage_len(width, height, advance, left)?;
    let total = FONT_GLYPH_REPLY_HEADER_LEN + len;
    if reply.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    Ok(GlyphCoverage {
        width,
        height,
        advance,
        left,
        coverage: &reply[FONT_GLYPH_REPLY_HEADER_LEN..total],
    })
}

/// Validate a glyph geometry and return the coverage length (`width *
/// height`) it implies. The bounds are the same on both the encode and
/// decode sides, so producer and consumer can never disagree on what a
/// well-formed reply looks like.
fn glyph_coverage_len(width: u32, height: u32, advance: u32, left: i32) -> Result<usize, Errno> {
    let span = i32::try_from(FONT_MAX_GLYPH_WIDTH).map_err(|_| Errno::LengthOutOfRange)?;
    if width > FONT_MAX_GLYPH_WIDTH
        || advance > FONT_MAX_GLYPH_WIDTH
        || left < -span
        || left > span
        || !(FONT_MIN_PIXEL_HEIGHT..=FONT_MAX_PIXEL_HEIGHT).contains(&height)
    {
        return Err(Errno::LengthOutOfRange);
    }
    // Each factor is bounded well under `u32::MAX`, so the product fits a
    // `usize` on every target (including 32-bit `wasm32`).
    Ok((width as usize) * (height as usize))
}

/// The line metrics a client needs to lay text out at a chosen pixel height,
/// obtained through [`FontRequest::Metrics`].
///
/// Glyphs sit on a baseline `baseline` rows below the top of a
/// `pixel_height`-tall box, and successive lines step by `line_height`. A
/// monospace family also reports the one advance every glyph shares, which
/// is what a character grid is built from; a proportional family reports
/// zero there and its callers advance the pen by each glyph's own advance.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FontMetrics {
    /// Height of a glyph bitmap, in pixels: the ascent plus the descent.
    pub pixel_height: u32,
    /// Baseline row within that box, in pixels below its top.
    pub baseline: u32,
    /// Distance between successive baselines, in pixels.
    pub line_height: u32,
    /// The advance every glyph of a monospace family shares, in pixels, or
    /// `0` for a proportional family.
    pub monospace_advance: u32,
}

/// Reply length, in bytes, of a [`FontRequest::Metrics`]: the status word
/// followed by the four [`FontMetrics`] fields, each a little-endian `u32`.
pub const FONT_METRICS_REPLY_LEN: usize = 20;

/// The tallest line box a family may report relative to its glyph box, as a
/// multiple. A face's line gap is a fraction of its em; anything beyond this
/// is a corrupt reply rather than a typographic choice.
const MAX_LINE_HEIGHT_FACTOR: u32 = 4;

/// Encode a metrics outcome: the [`FONT_METRICS_REPLY_LEN`]-byte reply on
/// success, the status word (a negative [`Errno`] discriminant, zero-padded
/// to the same length) on refusal. Padding the refusal keeps the reply
/// length constant, so a client always issues one fixed-size receive.
#[must_use]
pub fn encode_metrics_reply(result: Result<FontMetrics, Errno>) -> [u8; FONT_METRICS_REPLY_LEN] {
    let mut out = [0u8; FONT_METRICS_REPLY_LEN];
    match result {
        Ok(metrics) => {
            put_u32(&mut out, 4, metrics.pixel_height);
            put_u32(&mut out, 8, metrics.baseline);
            put_u32(&mut out, 12, metrics.line_height);
            put_u32(&mut out, 16, metrics.monospace_advance);
        }
        Err(err) => {
            out[..4].copy_from_slice(&crate::reply::encode_status_reply(Err(err)));
        }
    }
    out
}

/// Decode a [`FontRequest::Metrics`] reply frame.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole reply.
/// * [`Errno::OutOfRange`] — a corrupt status word (fail closed).
/// * [`Errno::LengthOutOfRange`] — nonsensical metrics: a pixel height out
///   of range, a baseline below the box, a line height of zero or
///   implausibly tall, or a monospace advance wider than a glyph may be.
/// * The decoded [`Errno`] itself, when the service refused the request.
pub fn decode_metrics_reply(bytes: &[u8]) -> Result<FontMetrics, Errno> {
    if bytes.len() < FONT_METRICS_REPLY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&bytes[..4])?;
    let metrics = FontMetrics {
        pixel_height: read_u32(bytes, 4),
        baseline: read_u32(bytes, 8),
        line_height: read_u32(bytes, 12),
        monospace_advance: read_u32(bytes, 16),
    };
    if !(FONT_MIN_PIXEL_HEIGHT..=FONT_MAX_PIXEL_HEIGHT).contains(&metrics.pixel_height) {
        return Err(Errno::LengthOutOfRange);
    }
    if metrics.baseline > metrics.pixel_height
        || metrics.monospace_advance > FONT_MAX_GLYPH_WIDTH
        || metrics.line_height == 0
        || metrics.line_height > metrics.pixel_height.saturating_mul(MAX_LINE_HEIGHT_FACTOR)
    {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(metrics)
}

/// One installed selectable family, as a [`FontRequest::Families`] reply
/// lists it: the key a request names it by, the label a settings surface
/// shows, and how it lays text out.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FamilyEntry {
    /// The key a [`FontRequest`] names this family by.
    pub key: FamilyKey,
    /// The label a font picker shows, NUL-padded on the wire.
    label: [u8; FONT_FAMILY_LABEL_LEN],
    /// Whether the family is fixed-pitch.
    pub kind: FamilyKind,
}

impl FamilyEntry {
    /// The value an undecoded slot of a [`FamilyList`] holds.
    ///
    /// A list only ever exposes the entries it decoded, so this never
    /// reaches a caller; it exists so the fixed-capacity list can be built
    /// without allocating and without an `Option` per slot.
    const UNSET: Self = Self {
        key: FamilyKey::MONO,
        label: [0u8; FONT_FAMILY_LABEL_LEN],
        kind: FamilyKind::Monospace,
    };

    /// The entry for `key`, shown as `label`, laid out as `kind`.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when `label` is empty or longer than
    /// [`FONT_FAMILY_LABEL_LEN`] bytes; [`Errno::OutOfRange`] when it
    /// carries a control byte, which a picker must never be asked to draw.
    pub fn new(key: FamilyKey, label: &str, kind: FamilyKind) -> Result<Self, Errno> {
        if label.is_empty() || label.len() > FONT_FAMILY_LABEL_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if label.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
            return Err(Errno::OutOfRange);
        }
        let mut padded = [0u8; FONT_FAMILY_LABEL_LEN];
        padded[..label.len()].copy_from_slice(label.as_bytes());
        Ok(Self {
            key,
            label: padded,
            kind,
        })
    }

    /// The label a font picker shows.
    #[must_use]
    pub fn label(&self) -> &str {
        let len = self
            .label
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(FONT_FAMILY_LABEL_LEN);
        core::str::from_utf8(&self.label[..len]).unwrap_or("")
    }
}

/// Bytes one [`FamilyEntry`] occupies on the wire: the key, the label, the
/// kind discriminant, and three bytes of zero padding that keep the record
/// four-byte aligned.
pub const FONT_FAMILY_ENTRY_LEN: usize = FONT_FAMILY_KEY_LEN + FONT_FAMILY_LABEL_LEN + 4;

/// Fixed prefix of a [`FontRequest::Families`] reply: the status word and
/// the entry count, each a little-endian 32-bit value.
pub const FONT_FAMILIES_REPLY_HEADER_LEN: usize = 8;

/// Largest [`FontRequest::Families`] reply, in bytes. A client sizes its
/// receive buffer to this.
pub const FONT_MAX_FAMILIES_REPLY: usize =
    FONT_FAMILIES_REPLY_HEADER_LEN + FONT_MAX_FAMILIES * FONT_FAMILY_ENTRY_LEN;

/// Encode a family-list outcome into `buf`, returning the number of bytes
/// written. A refusal is the status word alone.
///
/// # Errors
///
/// * [`Errno::LengthOutOfRange`] — more than [`FONT_MAX_FAMILIES`] entries.
/// * [`Errno::BufferTooSmall`] — `buf` cannot hold the framed reply.
pub fn encode_families_reply(
    buf: &mut [u8],
    result: Result<&[FamilyEntry], Errno>,
) -> Result<usize, Errno> {
    let entries = match result {
        Ok(entries) => entries,
        Err(err) => return encode_glyph_error_reply(buf, err),
    };
    if entries.len() > FONT_MAX_FAMILIES {
        return Err(Errno::LengthOutOfRange);
    }
    let total = FONT_FAMILIES_REPLY_HEADER_LEN + entries.len() * FONT_FAMILY_ENTRY_LEN;
    if buf.len() < total {
        return Err(Errno::BufferTooSmall);
    }
    put_i32(buf, 0, 0);
    let count = u32::try_from(entries.len()).map_err(|_| Errno::LengthOutOfRange)?;
    put_u32(buf, 4, count);
    let mut at = FONT_FAMILIES_REPLY_HEADER_LEN;
    for entry in entries {
        let key_end = at + FONT_FAMILY_KEY_LEN;
        let label_end = key_end + FONT_FAMILY_LABEL_LEN;
        buf[at..key_end].copy_from_slice(&entry.key.to_wire());
        buf[key_end..label_end].copy_from_slice(&entry.label);
        buf[label_end] = entry.kind.to_wire();
        buf[label_end + 1..label_end + 4].fill(0);
        at = label_end + 4;
    }
    Ok(total)
}

/// The installed selectable families a [`FontRequest::Families`] reply
/// listed, held inline so decoding one allocates nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FamilyList {
    entries: [FamilyEntry; FONT_MAX_FAMILIES],
    len: usize,
}

impl FamilyList {
    /// The families the reply listed, in the order the service reported.
    #[must_use]
    pub fn entries(&self) -> &[FamilyEntry] {
        &self.entries[..self.len]
    }
}

/// Decode a [`FontRequest::Families`] reply.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — a truncated frame.
/// * [`Errno::OutOfRange`] — a corrupt status word, an unknown kind, or a
///   malformed key or label.
/// * [`Errno::LengthOutOfRange`] — a count past [`FONT_MAX_FAMILIES`].
/// * [`Errno::BadMagic`] — a dirty padding tail in a record.
pub fn decode_families_reply(reply: &[u8]) -> Result<FamilyList, Errno> {
    if reply.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    crate::reply::decode_status_reply(&reply[..4])?;
    if reply.len() < FONT_FAMILIES_REPLY_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let count = read_u32(reply, 4) as usize;
    if count > FONT_MAX_FAMILIES {
        return Err(Errno::LengthOutOfRange);
    }
    if reply.len() < FONT_FAMILIES_REPLY_HEADER_LEN + count * FONT_FAMILY_ENTRY_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let mut list = FamilyList {
        entries: [FamilyEntry::UNSET; FONT_MAX_FAMILIES],
        len: count,
    };
    for (index, slot) in list.entries.iter_mut().take(count).enumerate() {
        let at = FONT_FAMILIES_REPLY_HEADER_LEN + index * FONT_FAMILY_ENTRY_LEN;
        let key_end = at + FONT_FAMILY_KEY_LEN;
        let label_end = key_end + FONT_FAMILY_LABEL_LEN;
        let mut key = [0u8; FONT_FAMILY_KEY_LEN];
        key.copy_from_slice(&reply[at..key_end]);
        let key = FamilyKey::from_wire(key)?;
        let label = decode_label(&reply[key_end..label_end])?;
        let kind = FamilyKind::from_wire(reply[label_end])?;
        if reply[label_end + 1..label_end + 4].iter().any(|&b| b != 0) {
            return Err(Errno::BadMagic);
        }
        *slot = FamilyEntry::new(key, label, kind)?;
    }
    Ok(list)
}

/// The label a NUL-padded wire field carries, refusing a dirty padding tail
/// or a non-UTF-8 spelling.
fn decode_label(field: &[u8]) -> Result<&str, Errno> {
    let len = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    if field[len..].iter().any(|&byte| byte != 0) {
        return Err(Errno::BadMagic);
    }
    core::str::from_utf8(&field[..len]).map_err(|_| Errno::OutOfRange)
}

#[cfg(test)]
mod tests;
