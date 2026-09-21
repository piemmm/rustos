//! The font-service IPC protocol (`plans/FONT-SERVICE.md` FS-1): the
//! reserved rendezvous the sandboxed OS font service (`fontd`) binds, and
//! the fixed-width, fail-closed requests a text-drawing client presents to
//! obtain a run of glyphs' coverage bitmaps, a family's line metrics, or the
//! set of installed families.
//!
//! Text rendering is a single, sandboxed OS resource: no process but `fontd`
//! holds a font face or an outline rasteriser, and a client draws by asking
//! this endpoint for the 8-bit coverage of a bounded *run* of Unicode scalars
//! at a chosen pixel height. The transport carries no font bytes and no
//! outlines — only the small coverage bitmaps the client blits — so a
//! malformed face can fault only the service's sandbox, never the compositor
//! or a terminal.
//!
//! # A run, not a glyph, because the caller is drawing a frame
//!
//! A client asks per *run* of text rather than per character: the surface
//! that draws a label owes the user a frame, and one round trip per
//! not-yet-cached character turned a newly-opened window into a burst of
//! them. A reply answers as many of the run as its frame holds, in order,
//! and states how many — one glyph at the extreme of the coverage bound
//! fills a frame on its own, so a batch is a *prefix* and the client asks
//! again for any remainder. At the sizes a desktop draws, one round trip
//! covers any realistic run.
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
//! text is not a security boundary, so the endpoint requires no capability of
//! its own; the reply nonetheless validates every field and fails closed on a
//! corrupt frame. Every request and reply is versioned and hashed under the
//! same ABI discipline as the syscall table and frozen on the first release —
//! mutable now, `abi-v1` is not frozen.

use crate::le::{put_i32, put_u32, read_i32, read_u32};
use crate::Errno;

/// Reserved well-known call-endpoint id of the font service (`"FNT"`
/// hex-spelled prefix, mirroring [`crate::mailbox_ipc::MAILBOX_ENDPOINT`]'s
/// convention). Binding it requires `CAP_IPC_BIND_PRIVILEGED`
/// ([`crate::ipc::is_reserved_endpoint`]): a squatter claiming the
/// rendezvous first would feed forged glyph coverage to the compositor and
/// every app, so only the trusted `fontd` service may bind it. One endpoint
/// serves every client — requests carry the family, scalars and pixel height
/// in-protocol.
pub const FONT_ENDPOINT: u64 = 0x464E_5400;

/// The service-manager name of the font service, as a client asks for it.
///
/// A client reaches [`FONT_ENDPOINT`] only after the manager has activated
/// the service behind it, and the manager names a service by its bundle
/// directory. One definition, shared by the client that asks and the boot
/// description that registers it, so the two cannot drift into a connect for
/// a name no service answers to.
pub const FONT_SERVICE_NAME: &str = "fontd";

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

/// Most Unicode scalars one [`FontRequest::Glyphs`] may name.
///
/// A request is one fixed width for every operation, so the run costs its
/// four bytes a slot whether they are used or not; 32 covers a whole
/// newly-appeared window's worth of text in a single round trip and leaves
/// the frame a few hundred bytes. A validation bound, not a capacity: longer
/// text is drawn as consecutive runs, each its own request.
pub const FONT_MAX_GLYPH_RUN: usize = 32;

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

/// Lightest weight the `wght` design axis is defined over.
pub const FONT_MIN_WEIGHT: u16 = 1;

/// Heaviest weight the `wght` design axis is defined over.
pub const FONT_MAX_WEIGHT: u16 = 1000;

/// The weight a run of text is set in: the OpenType `wght` design-axis
/// coordinate, [`FONT_MIN_WEIGHT`]..=[`FONT_MAX_WEIGHT`].
///
/// A number rather than a closed set of names because every variable face
/// carries the whole axis and CSS may ask for any point on it — a document
/// setting `font-weight: 250` means 250, and rounding it to the nearest
/// named weight would draw a picture nobody wrote. [`FontWeight::REGULAR`]
/// and friends name the common points without making them the only ones.
///
/// A variable face is instanced at this coordinate, so the glyph *and* its
/// advance are the ones the designer drew. A face with no `wght` axis is
/// thickened instead by a bounded synthetic stroke that leaves its advance
/// alone. Either way a reply states what it was rendered with, so layout
/// never has to assume.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FontWeight(u16);

impl Default for FontWeight {
    fn default() -> Self {
        Self::REGULAR
    }
}

impl FontWeight {
    /// Normal weight: body text, secondary detail, terminal text.
    pub const REGULAR: Self = Self(400);
    /// A medium weight for titling text — an item's primary line, a window
    /// title, a panel heading.
    pub const MEDIUM: Self = Self(500);
    /// Bold weight: column headers and metric readouts.
    pub const BOLD: Self = Self(700);

    /// The weight `axis` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] outside
    /// [`FONT_MIN_WEIGHT`]..=[`FONT_MAX_WEIGHT`], so a coordinate the axis
    /// is not defined over is refused rather than clamped into range.
    pub const fn new(axis: u16) -> Result<Self, Errno> {
        if axis < FONT_MIN_WEIGHT || axis > FONT_MAX_WEIGHT {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(axis))
    }

    /// The OpenType `wght` design-axis coordinate this weight names.
    #[must_use]
    pub const fn axis_value(self) -> u16 {
        self.0
    }

    /// This weight's wire value.
    #[must_use]
    pub const fn to_wire(self) -> u16 {
        self.0
    }

    /// The weight `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] outside the axis's range.
    pub const fn from_wire(wire: u16) -> Result<Self, Errno> {
        Self::new(wire)
    }
}

/// The posture a run of text is set in.
///
/// A face declaring an `ital` or `slnt` axis is instanced at it; one that
/// declares neither cannot furnish a posture at all, and the service says so
/// through [`Synthesis`] rather than substituting the upright face in
/// silence.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum FontStyle {
    /// Upright.
    #[default]
    Normal,
    /// The designer's own cursive letterforms.
    Italic,
    /// Upright letterforms slanted, with no change of shape.
    Oblique,
}

impl FontStyle {
    /// This posture's wire discriminant.
    #[must_use]
    pub const fn to_wire(self) -> u16 {
        match self {
            Self::Normal => 1,
            Self::Italic => 2,
            Self::Oblique => 3,
        }
    }

    /// The posture `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a discriminant outside the closed set, so an
    /// unknown posture is refused rather than silently set upright.
    pub const fn from_wire(wire: u16) -> Result<Self, Errno> {
        match wire {
            1 => Ok(Self::Normal),
            2 => Ok(Self::Italic),
            3 => Ok(Self::Oblique),
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// Hundredths of a percent per [`FontStretch`] wire step, so the half-percent
/// widths CSS names (`extra-condensed` is 62.5%) are exact.
pub const FONT_STRETCH_SCALE: u16 = 100;

/// Narrowest width the `wdth` design axis is defined over, in
/// [`FONT_STRETCH_SCALE`] units (50%).
pub const FONT_MIN_STRETCH: u16 = 50 * FONT_STRETCH_SCALE;

/// Widest width the `wdth` design axis is defined over, in
/// [`FONT_STRETCH_SCALE`] units (200%).
pub const FONT_MAX_STRETCH: u16 = 200 * FONT_STRETCH_SCALE;

/// The width a run of text is set at: the OpenType `wdth` design-axis
/// coordinate as a percentage of normal, in [`FONT_STRETCH_SCALE`] units.
///
/// Like the weight it is a number rather than a keyword set, because the
/// axis is continuous and the committed Noto faces carry it. A face with no
/// `wdth` axis is set at its own width — a width cannot be synthesised
/// without distorting the letterforms, so the protocol does not pretend it
/// can.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FontStretch(u16);

impl Default for FontStretch {
    fn default() -> Self {
        Self::NORMAL
    }
}

impl FontStretch {
    /// Unstretched: the width the face was drawn at.
    pub const NORMAL: Self = Self(100 * FONT_STRETCH_SCALE);

    /// The width `hundredths` of a percent names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] outside
    /// [`FONT_MIN_STRETCH`]..=[`FONT_MAX_STRETCH`].
    pub const fn new(hundredths: u16) -> Result<Self, Errno> {
        if hundredths < FONT_MIN_STRETCH || hundredths > FONT_MAX_STRETCH {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(hundredths))
    }

    /// This width in hundredths of a percent.
    #[must_use]
    pub const fn hundredths(self) -> u16 {
        self.0
    }

    /// This width's wire value.
    #[must_use]
    pub const fn to_wire(self) -> u16 {
        self.0
    }

    /// The width `wire` names.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] outside the axis's range.
    pub const fn from_wire(wire: u16) -> Result<Self, Errno> {
        Self::new(wire)
    }
}

/// The run of Unicode scalars one [`FontRequest::Glyphs`] asks for: bounded,
/// non-empty, and a fixed width on the wire.
///
/// Holding the scalars as [`char`] makes a surrogate or an out-of-range code
/// point unrepresentable once decoded. Slots past the run's length are
/// `'\0'`, and a frame carrying anything else there is refused, so the
/// padding can never smuggle a scalar the count does not admit — U+0000 is
/// itself a legal scalar, and the count is what distinguishes one asked for
/// from the padding.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphRun {
    /// The run's scalars, `'\0'`-padded to the fixed wire width.
    scalars: [char; FONT_MAX_GLYPH_RUN],
    /// How many leading `scalars` the run asks for.
    len: u32,
}

impl GlyphRun {
    /// The run `scalars` spells.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] when `scalars` is empty — a request that
    /// asks for nothing is malformed, not a no-op, since a reply must always
    /// answer at least one glyph for the client to make progress — or longer
    /// than [`FONT_MAX_GLYPH_RUN`].
    pub fn new(scalars: &[char]) -> Result<Self, Errno> {
        if scalars.is_empty() || scalars.len() > FONT_MAX_GLYPH_RUN {
            return Err(Errno::LengthOutOfRange);
        }
        let len = u32::try_from(scalars.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let mut run = Self {
            scalars: ['\0'; FONT_MAX_GLYPH_RUN],
            len,
        };
        run.scalars[..scalars.len()].copy_from_slice(scalars);
        Ok(run)
    }

    /// The scalars the run asks for, in the order a reply answers them.
    #[must_use]
    pub fn scalars(&self) -> &[char] {
        &self.scalars[..self.len as usize]
    }
}

/// One font-service operation (`plans/FONT-SERVICE.md` FS-1).
///
/// Carrying the scalars as [`char`] and the family as a [`FamilyKey`] makes
/// an illegal request unrepresentable once decoded: a surrogate, an
/// out-of-range code point, or a key that could escape its directory is
/// rejected before the request is ever built.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FontRequest {
    /// Render the coverage of every scalar in `scalars` from `family`, sized
    /// so the line box is `pixel_height` pixels tall.
    ///
    /// The reply answers a **prefix** of the run — as many as its frame
    /// holds, in order — and states how many, so a client asks again for any
    /// remainder. The service resolves each scalar to the covering face and
    /// returns its 8-bit coverage bitmap with the advance and bearing to draw
    /// it by; a scalar no face covers renders the U+FFFD replacement glyph
    /// rather than being refused.
    Glyphs {
        /// The family to render from.
        family: FamilyKey,
        /// The Unicode scalars to render.
        scalars: GlyphRun,
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
    /// Hand back every scalar in `scalars` from `family` as **contours in
    /// font units**, for a caller drawing text as geometry rather than
    /// blitting a cell.
    ///
    /// There is deliberately no pixel height: the picture a vector consumer
    /// produces has no resolution, so baking a size in at this point would
    /// fix an accuracy the caller has not chosen yet. The caller scales by
    /// `font-size / units_per_em` and flattens the quadratics at whatever
    /// tolerance its own placement resolves.
    ///
    /// Like [`Glyphs`](Self::Glyphs) the reply answers a **prefix** of the
    /// run and states how many, so the client asks again for any remainder.
    Outlines {
        /// The family to outline from.
        family: FamilyKey,
        /// The Unicode scalars to outline.
        scalars: GlyphRun,
        /// The weight to instance the face at.
        weight: FontWeight,
        /// The posture to instance the face at.
        style: FontStyle,
        /// The width to instance the face at.
        stretch: FontStretch,
    },
}

/// Wire operation discriminant of [`FontRequest::Glyphs`].
const OP_GLYPHS: u16 = 1;
/// Wire operation discriminant of [`FontRequest::Metrics`].
const OP_METRICS: u16 = 2;
/// Wire operation discriminant of [`FontRequest::Families`].
const OP_FAMILIES: u16 = 3;
/// Wire operation discriminant of [`FontRequest::Outlines`].
const OP_OUTLINES: u16 = 4;

/// Offset of the weight field in a request frame.
const REQUEST_WEIGHT: usize = 8;
/// Offset of the posture field in a request frame.
const REQUEST_STYLE: usize = 10;
/// Offset of the width field in a request frame.
const REQUEST_STRETCH: usize = 12;
/// Offset of the reserved halfword that follows the width.
const REQUEST_RESERVED: usize = 14;
/// Offset of the pixel-height field in a request frame.
const REQUEST_HEIGHT: usize = 16;
/// Offset of the run-length field in a request frame.
const REQUEST_COUNT: usize = 20;
/// Offset of the family key in a request frame.
const REQUEST_FAMILY: usize = 24;
/// Offset of the inline scalar run in a request frame.
const REQUEST_RUN: usize = REQUEST_FAMILY + FONT_FAMILY_KEY_LEN;

/// Bytes one scalar occupies in a request's run.
const REQUEST_SCALAR_LEN: usize = 4;

impl FontRequest {
    /// Encoded size on the wire: magic (4), version (2), op (2), weight (2),
    /// posture (2), width (2), a reserved halfword, pixel height (4), run
    /// length (4), the [`FONT_FAMILY_KEY_LEN`]-byte family key, and the
    /// [`FONT_MAX_GLYPH_RUN`]-slot inline scalar run. Every field an
    /// operation does not use is zero.
    pub const WIRE_LEN: usize = REQUEST_RUN + FONT_MAX_GLYPH_RUN * REQUEST_SCALAR_LEN;

    /// Encode `self` little-endian.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        let (op, family, height, weight, run) = match *self {
            Self::Glyphs {
                family,
                scalars,
                pixel_height,
                weight,
            } => (
                OP_GLYPHS,
                Some(family),
                pixel_height,
                weight.to_wire(),
                Some(scalars),
            ),
            Self::Metrics {
                family,
                pixel_height,
                weight,
            } => (
                OP_METRICS,
                Some(family),
                pixel_height,
                weight.to_wire(),
                None,
            ),
            Self::Families => (OP_FAMILIES, None, 0, 0, None),
            Self::Outlines {
                family,
                scalars,
                weight,
                style,
                stretch,
            } => {
                out[REQUEST_STYLE..REQUEST_STRETCH].copy_from_slice(&style.to_wire().to_le_bytes());
                out[REQUEST_STRETCH..REQUEST_RESERVED]
                    .copy_from_slice(&stretch.to_wire().to_le_bytes());
                (
                    OP_OUTLINES,
                    Some(family),
                    0,
                    weight.to_wire(),
                    Some(scalars),
                )
            }
        };
        put_u32(&mut out, 0, FONT_REQUEST_MAGIC);
        out[4..6].copy_from_slice(&FONT_VERSION_V1.to_le_bytes());
        out[6..8].copy_from_slice(&op.to_le_bytes());
        out[REQUEST_WEIGHT..REQUEST_STYLE].copy_from_slice(&weight.to_le_bytes());
        put_u32(&mut out, REQUEST_HEIGHT, height);
        if let Some(family) = family {
            out[REQUEST_FAMILY..REQUEST_RUN].copy_from_slice(&family.to_wire());
        }
        if let Some(run) = run {
            put_u32(&mut out, REQUEST_COUNT, run.len);
            for (index, &scalar) in run.scalars().iter().enumerate() {
                put_u32(
                    &mut out,
                    REQUEST_RUN + index * REQUEST_SCALAR_LEN,
                    scalar as u32,
                );
            }
        }
        out
    }

    /// Decode a request from `bytes`, failing closed on any malformed input.
    ///
    /// Every bound a decoder can already see — the pixel-height range, the
    /// run length, every scalar's validity, the family key's spelling, and
    /// the zeroing of every field and run slot the operation does not use —
    /// is enforced here, so no accepted request ever carries a value the
    /// service would have to re-reject structurally.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `bytes` cannot hold a whole request.
    /// * [`Errno::BadMagic`] — wrong magic, or a non-zero byte in a field or
    ///   run slot the operation does not use.
    /// * [`Errno::AbiVersionUnsupported`] — not `font-v1`.
    /// * [`Errno::OutOfRange`] — an operation outside the closed set, a run
    ///   slot that is not a Unicode scalar value, or a weight, posture, or
    ///   width outside the design axis it names.
    /// * [`Errno::LengthOutOfRange`] — a pixel height outside
    ///   [`FONT_MIN_PIXEL_HEIGHT`]..=[`FONT_MAX_PIXEL_HEIGHT`], or a run
    ///   length outside `1..=`[`FONT_MAX_GLYPH_RUN`].
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
        let style_wire = u16::from_le_bytes([bytes[REQUEST_STYLE], bytes[REQUEST_STYLE + 1]]);
        let stretch_wire = u16::from_le_bytes([bytes[REQUEST_STRETCH], bytes[REQUEST_STRETCH + 1]]);
        let count_wire = read_u32(bytes, REQUEST_COUNT);
        let height_wire = read_u32(bytes, REQUEST_HEIGHT);
        // Only the outline op is set in a posture and a width; the coverage
        // path draws neither, so a frame carrying one there is malformed
        // rather than a request whose extra fields go unread.
        let posture_unused = style_wire == 0 && stretch_wire == 0;
        match op {
            OP_GLYPHS => {
                if !posture_unused {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Glyphs {
                    family: family_field(bytes)?,
                    scalars: run_field(bytes, count_wire)?,
                    pixel_height: validate_pixel_height(height_wire)?,
                    weight: FontWeight::from_wire(weight_wire)?,
                })
            }
            OP_METRICS => {
                if count_wire != 0 || !run_slots_zero(bytes, 0) || !posture_unused {
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
                    && posture_unused
                    && count_wire == 0
                    && height_wire == 0
                    && bytes[REQUEST_FAMILY..REQUEST_RUN].iter().all(|&b| b == 0)
                    && run_slots_zero(bytes, 0);
                if unused_zero {
                    Ok(Self::Families)
                } else {
                    Err(Errno::BadMagic)
                }
            }
            // An outline has no resolution, so the pixel height is the field
            // this operation does not use and must be zero.
            OP_OUTLINES => {
                if height_wire != 0 {
                    return Err(Errno::BadMagic);
                }
                Ok(Self::Outlines {
                    family: family_field(bytes)?,
                    scalars: run_field(bytes, count_wire)?,
                    weight: FontWeight::from_wire(weight_wire)?,
                    style: FontStyle::from_wire(style_wire)?,
                    stretch: FontStretch::from_wire(stretch_wire)?,
                })
            }
            _ => Err(Errno::OutOfRange),
        }
    }
}

/// The family key a request frame carries.
fn family_field(bytes: &[u8]) -> Result<FamilyKey, Errno> {
    let mut key = [0u8; FONT_FAMILY_KEY_LEN];
    key.copy_from_slice(&bytes[REQUEST_FAMILY..REQUEST_RUN]);
    FamilyKey::from_wire(key)
}

/// The scalar run a request frame carries, given its wire run length.
fn run_field(bytes: &[u8], count: u32) -> Result<GlyphRun, Errno> {
    let count = usize::try_from(count).map_err(|_| Errno::LengthOutOfRange)?;
    if count == 0 || count > FONT_MAX_GLYPH_RUN {
        return Err(Errno::LengthOutOfRange);
    }
    if !run_slots_zero(bytes, count) {
        return Err(Errno::BadMagic);
    }
    let mut scalars = ['\0'; FONT_MAX_GLYPH_RUN];
    for (index, slot) in scalars.iter_mut().take(count).enumerate() {
        let wire = read_u32(bytes, REQUEST_RUN + index * REQUEST_SCALAR_LEN);
        *slot = char::from_u32(wire).ok_or(Errno::OutOfRange)?;
    }
    GlyphRun::new(&scalars[..count])
}

/// Whether every run slot from `from` onward is zero: the padding no
/// operation reads, which may never smuggle a scalar the run length excludes.
fn run_slots_zero(bytes: &[u8], from: usize) -> bool {
    bytes[REQUEST_RUN + from * REQUEST_SCALAR_LEN..FontRequest::WIRE_LEN]
        .iter()
        .all(|&byte| byte == 0)
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

/// Fixed prefix every batch reply opens with: a status word (`0` on success,
/// else the negated [`Errno`] discriminant) followed by how many glyphs of
/// the run the batch answers, each a little-endian 32-bit value.
pub const FONT_BATCH_PREFIX_LEN: usize = 8;

/// Fixed prefix of a [`FontRequest::Glyphs`] reply: the shared batch prefix
/// and nothing more. That many records follow.
pub const FONT_GLYPHS_REPLY_HEADER_LEN: usize = FONT_BATCH_PREFIX_LEN;

/// Frames a batch reply as its producer emits it, one record at a time.
///
/// Both reply kinds answer a **prefix** of the requested run — the service
/// appends records until the next will not fit and states how many it
/// answered — so the fill rule lives here once rather than in each writer.
/// A coverage batch and an outline batch therefore cannot come to disagree
/// about what a well-formed prefix reply looks like, and each decoder
/// enforces the bound its own producer stopped at.
struct BatchWriter<'a> {
    /// The reply frame, already trimmed to the protocol maximum for its
    /// kind, so a frame this writer seals is always one the decoder accepts.
    buf: &'a mut [u8],
    /// Where the first record begins.
    header: usize,
    /// Bytes written, header included.
    at: usize,
    /// Records appended.
    count: u32,
}

impl<'a> BatchWriter<'a> {
    /// A writer over `buf` with a `header`-byte header reserved, capped at
    /// `max_reply`.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `buf` cannot hold the header.
    fn new(buf: &'a mut [u8], header: usize, max_reply: usize) -> Result<Self, Errno> {
        if buf.len() < header {
            return Err(Errno::BufferTooSmall);
        }
        let usable = buf.len().min(max_reply);
        Ok(Self {
            buf: &mut buf[..usable],
            header,
            at: header,
            count: 0,
        })
    }

    /// How many records have been appended.
    const fn count(&self) -> u32 {
        self.count
    }

    /// The header bytes past the shared status word and count, for a batch
    /// whose header states more than those two. Sealing only ever writes the
    /// shared prefix, so what a producer puts here survives it.
    fn header_mut(&mut self) -> &mut [u8] {
        &mut self.buf[FONT_BATCH_PREFIX_LEN..self.header]
    }

    /// Reserve `len` bytes for the next record, or `None` once the batch is
    /// full — the frame cannot hold another, or it already answers the
    /// longest run a request may name.
    fn open(&mut self, len: usize) -> Option<&mut [u8]> {
        if self.count as usize >= FONT_MAX_GLYPH_RUN {
            return None;
        }
        let end = self
            .at
            .checked_add(len)
            .filter(|&end| end <= self.buf.len())?;
        let at = self.at;
        self.at = end;
        self.count += 1;
        Some(&mut self.buf[at..end])
    }

    /// Seal the batch — the success status and the answered count — and
    /// return the framed reply's length.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when no record fitted at all. A batch that
    /// answers nothing leaves a client with no way forward, so it is never a
    /// successful reply: the caller frames the refusal instead.
    fn finish(self) -> Result<usize, Errno> {
        if self.count == 0 {
            return Err(Errno::BufferTooSmall);
        }
        put_i32(self.buf, 0, 0);
        put_u32(self.buf, 4, self.count);
        Ok(self.at)
    }
}

/// Read a batch reply's shared prefix: the refusal it carries, or how many
/// records follow.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — `reply` is shorter than the prefix.
/// * [`Errno::OutOfRange`] — a positive or undefined status word (wire
///   corruption — fail closed).
/// * [`Errno::LengthOutOfRange`] — a count outside
///   `1..=`[`FONT_MAX_GLYPH_RUN`].
fn batch_count(reply: &[u8]) -> Result<usize, Errno> {
    if reply.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    let status = read_i32(reply, 0);
    if status != 0 {
        let errno = Errno::try_from_status(status).ok_or(Errno::OutOfRange)?;
        return Err(errno);
    }
    if reply.len() < FONT_BATCH_PREFIX_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let count = usize::try_from(read_u32(reply, 4)).map_err(|_| Errno::LengthOutOfRange)?;
    if count == 0 || count > FONT_MAX_GLYPH_RUN {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(count)
}

/// Fixed prefix of one glyph record within a batch: the bitmap's width and
/// height, the pen advance, and the left side bearing, each a little-endian
/// 32-bit value. The 8-bit coverage samples follow, `width * height` of them,
/// so records are walked in sequence rather than indexed.
pub const FONT_GLYPH_RECORD_HEADER_LEN: usize = 16;

/// Largest [`FontRequest::Glyphs`] reply, in bytes: the batch header plus one
/// widest, tallest permitted record. A client sizes its receive buffer to
/// this.
///
/// It is also why a batch answers a *prefix* of the run rather than all of
/// it: one glyph at the extreme of [`FONT_MAX_COVERAGE_LEN`] fills the frame
/// on its own. At the sizes a desktop draws text a glyph is a few hundred
/// bytes, so one round trip covers any realistic run and the paging exists to
/// keep the bound honest.
pub const FONT_MAX_GLYPH_REPLY: usize =
    FONT_GLYPHS_REPLY_HEADER_LEN + FONT_GLYPH_RECORD_HEADER_LEN + FONT_MAX_COVERAGE_LEN;

/// One decoded glyph record: where the glyph sits relative to the pen,
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

/// Frames a [`FontRequest::Glyphs`] reply as the service produces it, one
/// record at a time.
///
/// The service rasterises from a cache it can borrow only one entry of at a
/// time, so it appends records rather than handing over a collected batch.
/// The fill rule is the shared one every batch reply obeys; this writer adds
/// only the record's own layout, beside the decoder that enforces the same
/// bound.
pub struct GlyphBatchWriter<'a> {
    inner: BatchWriter<'a>,
}

impl<'a> GlyphBatchWriter<'a> {
    /// A writer over `buf`, with the batch header reserved.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when `buf` cannot hold the header.
    pub fn new(buf: &'a mut [u8]) -> Result<Self, Errno> {
        Ok(Self {
            inner: BatchWriter::new(buf, FONT_GLYPHS_REPLY_HEADER_LEN, FONT_MAX_GLYPH_REPLY)?,
        })
    }

    /// How many records have been appended.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.inner.count()
    }

    /// Append `glyph`, reporting `false` when it does not fit or the batch
    /// already holds the longest run a request may name — the caller stops
    /// there, and the client asks again for the remainder.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] — a geometry outside the permitted bounds
    /// (see [`GlyphCoverage`]), or a `coverage` whose length is not exactly
    /// `width * height`. Checked ahead of the fit, so a malformed record is
    /// reported however full the batch is.
    pub fn push(&mut self, glyph: &GlyphCoverage<'_>) -> Result<bool, Errno> {
        let len = glyph_coverage_len(glyph.width, glyph.height, glyph.advance, glyph.left)?;
        if glyph.coverage.len() != len {
            return Err(Errno::LengthOutOfRange);
        }
        let Some(record) = self.inner.open(FONT_GLYPH_RECORD_HEADER_LEN + len) else {
            return Ok(false);
        };
        put_u32(record, 0, glyph.width);
        put_u32(record, 4, glyph.height);
        put_u32(record, 8, glyph.advance);
        put_i32(record, 12, glyph.left);
        record[FONT_GLYPH_RECORD_HEADER_LEN..].copy_from_slice(glyph.coverage);
        Ok(true)
    }

    /// Seal the batch — the success status and the answered count — and
    /// return the framed reply's length.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when no record fitted at all. A batch that
    /// answers nothing leaves a client with no way forward, so it is never a
    /// successful reply: the caller frames the refusal instead.
    pub fn finish(self) -> Result<usize, Errno> {
        self.inner.finish()
    }
}

/// Encode a fail-closed batch refusal (a status word only) into `buf`.
///
/// Both batch kinds refuse the same way, so there is one refusal frame.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] if `buf` cannot hold the status word.
pub fn encode_batch_error_reply(buf: &mut [u8], err: Errno) -> Result<usize, Errno> {
    if buf.len() < 4 {
        return Err(Errno::BufferTooSmall);
    }
    // A negative status carries `-errno`; `Errno` discriminants are positive.
    put_i32(buf, 0, -err.as_i32());
    Ok(4)
}

/// The glyphs a [`FontRequest::Glyphs`] reply answered: a prefix of the
/// requested run, in order, each record borrowing its coverage from the
/// frame.
///
/// Held inline so decoding one allocates nothing, and validated whole at
/// decode — every record a batch exposes has already been bounds-checked, so
/// walking one cannot fail part-way through a drawn run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphBatch<'a> {
    glyphs: [GlyphCoverage<'a>; FONT_MAX_GLYPH_RUN],
    len: usize,
}

impl<'a> GlyphBatch<'a> {
    /// The value an unanswered slot holds.
    ///
    /// A batch only ever exposes the records it decoded, so this never
    /// reaches a caller; it exists so the fixed-capacity batch can be built
    /// without allocating and without an `Option` per slot.
    const UNSET: GlyphCoverage<'static> = GlyphCoverage {
        width: 0,
        height: FONT_MIN_PIXEL_HEIGHT,
        advance: 0,
        left: 0,
        coverage: &[],
    };

    /// The glyphs the batch answered, in the order the run asked for them.
    #[must_use]
    pub fn glyphs(&self) -> &[GlyphCoverage<'a>] {
        &self.glyphs[..self.len]
    }
}

/// Decode a glyph-batch reply, borrowing each record's coverage bytes from
/// `reply`.
///
/// A successful batch always answers at least one glyph, so a client walking
/// a run always makes progress: a reply claiming none is malformed rather
/// than an empty answer to ask again for.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — `reply` is shorter than the status word, or
///   shorter than the records its counts and geometries imply (a truncated
///   frame is refused, never read past its bytes).
/// * [`Errno::OutOfRange`] — a positive or undefined status word (wire
///   corruption — fail closed).
/// * [`Errno::LengthOutOfRange`] — a count outside
///   `1..=`[`FONT_MAX_GLYPH_RUN`], a record geometry outside the permitted
///   bounds, or a frame longer than [`FONT_MAX_GLYPH_REPLY`].
pub fn decode_glyphs_reply(reply: &[u8]) -> Result<GlyphBatch<'_>, Errno> {
    let count = batch_count(reply)?;
    let mut batch = GlyphBatch {
        glyphs: [GlyphBatch::UNSET; FONT_MAX_GLYPH_RUN],
        len: count,
    };
    let mut at = FONT_GLYPHS_REPLY_HEADER_LEN;
    for slot in batch.glyphs.iter_mut().take(count) {
        let samples = at + FONT_GLYPH_RECORD_HEADER_LEN;
        if samples > reply.len() {
            return Err(Errno::BufferTooSmall);
        }
        let width = read_u32(reply, at);
        let height = read_u32(reply, at + 4);
        let advance = read_u32(reply, at + 8);
        let left = read_i32(reply, at + 12);
        let len = glyph_coverage_len(width, height, advance, left)?;
        let end = samples.checked_add(len).ok_or(Errno::LengthOutOfRange)?;
        if end > FONT_MAX_GLYPH_REPLY {
            return Err(Errno::LengthOutOfRange);
        }
        if end > reply.len() {
            return Err(Errno::BufferTooSmall);
        }
        *slot = GlyphCoverage {
            width,
            height,
            advance,
            left,
            coverage: &reply[samples..end],
        };
        at = end;
    }
    Ok(batch)
}

/// Validate a glyph geometry and return the coverage length (`width * height`)
/// it implies. The bounds are the same on both the encode and decode sides, so
/// producer and consumer can never disagree on what a well-formed reply looks
/// like.
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

/// The most outline points one glyph may decode, across every component of a
/// composite.
///
/// A validation bound on a hostile face, not a capacity: the heaviest glyph
/// in the committed faces is 584 points, in a 31k-glyph CJK face. The
/// outline reply's own maximum is derived from it, and the engine that
/// decodes a face enforces the same number, so a glyph the parser admits is
/// always one a reply can carry.
pub const FONT_MAX_OUTLINE_POINTS: u32 = 8192;

/// One coordinate of a glyph outline: **font units in 26.6 fixed point**,
/// the convention font engines carry sub-unit precision in.
///
/// Font units are the face's own, so a caller divides by the record's
/// `units_per_em` for em fractions. Sub-unit precision is real rather than
/// decorative — a composite component's transform and a variable face's
/// `gvar`/IUP deltas both land between units — and an integer representation
/// makes NaN and infinity *unrepresentable*, so "no non-finite reaches the
/// geometry" holds by construction rather than by a check at every consumer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FontUnits(i32);

impl FontUnits {
    /// Fractional bits: 26.6.
    pub const FRACTION_BITS: u32 = 6;

    /// One whole font unit, in raw steps.
    pub const ONE: i32 = 1 << Self::FRACTION_BITS;

    /// The coordinate `raw` 1/64ths of a font unit spell.
    #[must_use]
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// This coordinate in raw 1/64ths of a font unit.
    #[must_use]
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// The nearest representable coordinate to `value` font units.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a non-finite value, or one past the
    /// representable range — a corrupt outline is refused rather than
    /// saturated into a coordinate the face does not state.
    pub fn from_f64(value: f64) -> Result<Self, Errno> {
        if !value.is_finite() {
            return Err(Errno::OutOfRange);
        }
        let scaled = value * f64::from(Self::ONE);
        let rounded = if scaled < 0.0 {
            scaled - 0.5
        } else {
            scaled + 0.5
        };
        if rounded <= f64::from(i32::MIN) || rounded >= f64::from(i32::MAX) {
            return Err(Errno::OutOfRange);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "held strictly inside the i32 range on the line above, so the \
                      truncation the lint warns about cannot occur"
        )]
        Ok(Self(rounded as i32))
    }

    /// This coordinate in font units.
    #[must_use]
    pub fn to_f64(self) -> f64 {
        f64::from(self.0) / f64::from(Self::ONE)
    }
}

/// Raw steps per em in a [`Synthesis`] bold stroke width.
pub const FONT_SYNTH_BOLD_SCALE: u32 = 1 << 16;

/// The widest synthetic bold stroke a reply may ask for: a quarter of the em.
///
/// A validation bound, not a capacity — a stroke heavier than this has
/// stopped being a weight and become a blot, and the number exists so a
/// corrupt reply cannot make a consumer stroke a glyph into a solid block.
pub const FONT_MAX_SYNTH_BOLD: u16 = 1 << 14;

/// Raw steps per unit in a [`Synthesis`] oblique shear: 2.14 fixed point.
pub const FONT_SYNTH_SHEAR_SCALE: i32 = 1 << 14;

/// The steepest oblique shear a reply may ask for: 45°.
///
/// A validation bound like the bold stroke above: past it the letterforms
/// lie down rather than lean.
pub const FONT_MAX_SYNTH_SHEAR: i16 = 1 << 14;

// The two bounds above are quarters and units of the scales they are
// measured in; asserting it here is what keeps a later edit to one of them
// from silently moving the other's meaning.
const _: () = assert!(FONT_SYNTH_BOLD_SCALE == 4 * (FONT_MAX_SYNTH_BOLD as u32));
const _: () = assert!(FONT_SYNTH_SHEAR_SCALE == FONT_MAX_SYNTH_SHEAR as i32);

/// What the resolved face could **not** furnish, so the caller completes it
/// exactly rather than guessing or silently drawing the upright regular.
///
/// A variable face carrying the axis renders the weight and posture its
/// designer drew and this report is empty. A face carrying neither cannot,
/// and the honest answer is to say what is missing and by how much: the
/// service owns the synthesis policy (it is the thing that knows the face),
/// and the caller owns the geometry it is applied to.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Synthesis {
    /// Stroke width to thicken the outline by, in `1/`[`FONT_SYNTH_BOLD_SCALE`]
    /// of the em. Zero when the face furnished the weight itself.
    bold_em: u16,
    /// Horizontal shear to slant the outline by, in
    /// `1/`[`FONT_SYNTH_SHEAR_SCALE`] steps — the `tan` of the oblique angle,
    /// because a shear is what a caller applies and an angle is what every
    /// caller would then have to convert identically. Zero when the face
    /// furnished the posture itself.
    oblique_shear: i16,
}

impl Synthesis {
    /// Nothing to synthesise: the face furnished both axes.
    pub const NONE: Self = Self {
        bold_em: 0,
        oblique_shear: 0,
    };

    /// A report asking for `bold_em` of stroke and `oblique_shear` of slant.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] past [`FONT_MAX_SYNTH_BOLD`] or
    /// [`FONT_MAX_SYNTH_SHEAR`].
    pub const fn new(bold_em: u16, oblique_shear: i16) -> Result<Self, Errno> {
        if bold_em > FONT_MAX_SYNTH_BOLD
            || oblique_shear > FONT_MAX_SYNTH_SHEAR
            || oblique_shear < -FONT_MAX_SYNTH_SHEAR
        {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            bold_em,
            oblique_shear,
        })
    }

    /// Whether the face furnished everything asked of it.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.bold_em == 0 && self.oblique_shear == 0
    }

    /// The bold stroke width as a fraction of the em.
    #[must_use]
    pub fn bold_fraction(self) -> f64 {
        f64::from(self.bold_em) / f64::from(FONT_SYNTH_BOLD_SCALE)
    }

    /// The oblique shear: the horizontal offset per unit of height.
    #[must_use]
    pub fn shear(self) -> f64 {
        f64::from(self.oblique_shear) / f64::from(FONT_SYNTH_SHEAR_SCALE)
    }

    /// This report's wire value.
    #[must_use]
    pub fn to_wire(self) -> u32 {
        (u32::from(self.oblique_shear.cast_unsigned()) << 16) | u32::from(self.bold_em)
    }

    /// The report `wire` carries.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a synthesis outside the bounds above, or
    /// for a word whose halves do not fit the fields they name.
    pub fn from_wire(wire: u32) -> Result<Self, Errno> {
        let bold = u16::try_from(wire & 0xFFFF).map_err(|_| Errno::OutOfRange)?;
        let shear = u16::try_from(wire >> 16).map_err(|_| Errno::OutOfRange)?;
        Self::new(bold, shear.cast_signed())
    }
}

/// Fixed prefix of a [`FontRequest::Outlines`] reply: the shared batch
/// prefix, then the *requested family's primary face* geometry every record
/// is laid out against — its `units_per_em`, ascent, descent and line gap in
/// that face's own font units.
pub const FONT_OUTLINE_REPLY_HEADER_LEN: usize = FONT_BATCH_PREFIX_LEN + 16;

/// Fixed prefix of one outline record: the resolved face's own
/// `units_per_em`, the pen advance in 26.6 font units, how many contours and
/// segments follow, and the [`Synthesis`] that face could not furnish.
pub const FONT_OUTLINE_RECORD_HEADER_LEN: usize = 20;

/// Fixed prefix of one contour within a record: its start point in 26.6 font
/// units, and how many segments close it back onto that point.
pub const FONT_OUTLINE_CONTOUR_HEADER_LEN: usize = 12;

/// A straight segment on the wire: its kind and its end point.
pub const FONT_OUTLINE_LINE_LEN: usize = 12;

/// A quadratic segment on the wire: its kind, its end point, and the
/// off-curve control point between.
pub const FONT_OUTLINE_QUADRATIC_LEN: usize = 20;

/// Largest single outline record, in bytes.
///
/// One contour of [`FONT_MAX_OUTLINE_POINTS`]` - 1` quadratics is the
/// worst case a face the engine admits can produce: a quadratic is the wider
/// segment, and the contour and its segments share the one point bound.
pub const FONT_MAX_OUTLINE_RECORD: usize = FONT_OUTLINE_RECORD_HEADER_LEN
    + FONT_OUTLINE_CONTOUR_HEADER_LEN
    + (FONT_MAX_OUTLINE_POINTS as usize - 1) * FONT_OUTLINE_QUADRATIC_LEN;

/// Largest [`FontRequest::Outlines`] reply, in bytes: the batch header plus
/// one worst-case record. A client sizes its receive buffer to this.
///
/// It is comfortably under [`FONT_MAX_GLYPH_REPLY`], so serving geometry
/// moves no existing bound and grows no existing receive buffer — the
/// coverage of one glyph at the extreme of [`FONT_MAX_COVERAGE_LEN`] is
/// still the larger frame by some way.
pub const FONT_MAX_OUTLINE_REPLY: usize = FONT_OUTLINE_REPLY_HEADER_LEN + FONT_MAX_OUTLINE_RECORD;

/// Wire discriminant of a straight outline segment.
const SEGMENT_LINE: u32 = 1;
/// Wire discriminant of a quadratic outline segment.
const SEGMENT_QUADRATIC: u32 = 2;

/// One segment of a glyph contour, continuing from the previous point.
///
/// Quadratics come through whole: how finely a curve must be flattened
/// depends on the size it is finally drawn at, which only the caller knows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GlyphSegment {
    /// A straight line to `to`.
    Line {
        /// Where the segment ends.
        to: (FontUnits, FontUnits),
    },
    /// A quadratic Bézier through `control` to `to`.
    Quadratic {
        /// The off-curve control point.
        control: (FontUnits, FontUnits),
        /// Where the segment ends.
        to: (FontUnits, FontUnits),
    },
}

impl GlyphSegment {
    /// Bytes this segment occupies on the wire.
    const fn wire_len(self) -> usize {
        match self {
            Self::Line { .. } => FONT_OUTLINE_LINE_LEN,
            Self::Quadratic { .. } => FONT_OUTLINE_QUADRATIC_LEN,
        }
    }
}

/// One closed contour a producer hands [`OutlineBatchWriter::push`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ContourSource<'a> {
    /// Where the contour begins, and where its last segment returns to.
    pub start: (FontUnits, FontUnits),
    /// The segments, in order.
    pub segments: &'a [GlyphSegment],
}

/// One glyph's outline as a producer holds it, ready to be framed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OutlineSource<'a> {
    /// The **resolved** face's em, which a per-scalar fallback may differ in
    /// from the family's primary.
    pub units_per_em: u32,
    /// The pen advance in that face's font units.
    pub advance: FontUnits,
    /// What *this* face could not furnish and the caller must complete.
    pub synth: Synthesis,
    /// The contours, filled together under the non-zero rule.
    pub contours: &'a [ContourSource<'a>],
}

/// Frames a [`FontRequest::Outlines`] reply as the service produces it, one
/// glyph at a time, under the same prefix-batch fill rule the coverage reply
/// obeys.
pub struct OutlineBatchWriter<'a> {
    inner: BatchWriter<'a>,
}

impl<'a> OutlineBatchWriter<'a> {
    /// A writer over `buf` whose header states the requested family's
    /// primary-face geometry and the synthesis the caller must complete.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] when `buf` cannot hold the header.
    /// * [`Errno::OutOfRange`] for a `units_per_em` outside the range
    ///   TrueType defines (`16..=16384`).
    pub fn new(
        buf: &'a mut [u8],
        units_per_em: u32,
        ascent: i32,
        descent: i32,
        line_gap: i32,
    ) -> Result<Self, Errno> {
        validate_units_per_em(units_per_em)?;
        let mut inner =
            BatchWriter::new(buf, FONT_OUTLINE_REPLY_HEADER_LEN, FONT_MAX_OUTLINE_REPLY)?;
        let header = inner.header_mut();
        put_u32(header, 0, units_per_em);
        put_i32(header, 4, ascent);
        put_i32(header, 8, descent);
        put_i32(header, 12, line_gap);
        Ok(Self { inner })
    }

    /// How many records have been appended.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.inner.count()
    }

    /// Append `glyph`, reporting `false` when it does not fit or the batch
    /// already holds the longest run a request may name.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] for a `units_per_em` outside TrueType's range.
    /// * [`Errno::LengthOutOfRange`] when the contours and segments together
    ///   pass [`FONT_MAX_OUTLINE_POINTS`]. Checked ahead of the fit, so an
    ///   over-large glyph is reported however full the batch is.
    pub fn push(&mut self, glyph: &OutlineSource<'_>) -> Result<bool, Errno> {
        validate_units_per_em(glyph.units_per_em)?;
        let contours = u32::try_from(glyph.contours.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let mut segments = 0_u32;
        let mut body = 0_usize;
        for contour in glyph.contours {
            let count =
                u32::try_from(contour.segments.len()).map_err(|_| Errno::LengthOutOfRange)?;
            segments = segments.checked_add(count).ok_or(Errno::LengthOutOfRange)?;
            body += FONT_OUTLINE_CONTOUR_HEADER_LEN;
            for segment in contour.segments {
                body += segment.wire_len();
            }
        }
        if contours
            .checked_add(segments)
            .is_none_or(|points| points > FONT_MAX_OUTLINE_POINTS)
        {
            return Err(Errno::LengthOutOfRange);
        }
        let Some(record) = self.inner.open(FONT_OUTLINE_RECORD_HEADER_LEN + body) else {
            return Ok(false);
        };
        put_u32(record, 0, glyph.units_per_em);
        put_i32(record, 4, glyph.advance.raw());
        put_u32(record, 8, contours);
        put_u32(record, 12, segments);
        put_u32(record, 16, glyph.synth.to_wire());
        let mut at = FONT_OUTLINE_RECORD_HEADER_LEN;
        for contour in glyph.contours {
            put_i32(record, at, contour.start.0.raw());
            put_i32(record, at + 4, contour.start.1.raw());
            // The measuring pass above summed every contour's length into
            // `segments` without overflowing, so each fits. A count that
            // somehow did not would make the decoder refuse the frame
            // rather than read a record shorter than it claims.
            let count = u32::try_from(contour.segments.len()).unwrap_or(u32::MAX);
            put_u32(record, at + 8, count);
            at += FONT_OUTLINE_CONTOUR_HEADER_LEN;
            for segment in contour.segments {
                at += put_segment(record, at, *segment);
            }
        }
        Ok(true)
    }

    /// Seal the batch and return the framed reply's length.
    ///
    /// # Errors
    ///
    /// [`Errno::BufferTooSmall`] when no record fitted at all.
    pub fn finish(self) -> Result<usize, Errno> {
        self.inner.finish()
    }
}

/// Write one segment at `at`, returning the bytes it took.
fn put_segment(record: &mut [u8], at: usize, segment: GlyphSegment) -> usize {
    match segment {
        GlyphSegment::Line { to } => {
            put_u32(record, at, SEGMENT_LINE);
            put_i32(record, at + 4, to.0.raw());
            put_i32(record, at + 8, to.1.raw());
            FONT_OUTLINE_LINE_LEN
        }
        GlyphSegment::Quadratic { control, to } => {
            put_u32(record, at, SEGMENT_QUADRATIC);
            put_i32(record, at + 4, to.0.raw());
            put_i32(record, at + 8, to.1.raw());
            put_i32(record, at + 12, control.0.raw());
            put_i32(record, at + 16, control.1.raw());
            FONT_OUTLINE_QUADRATIC_LEN
        }
    }
}

/// Accept only an em TrueType's `head` table defines.
const fn validate_units_per_em(units_per_em: u32) -> Result<(), Errno> {
    if units_per_em < 16 || units_per_em > 16384 {
        return Err(Errno::OutOfRange);
    }
    Ok(())
}

/// One glyph's outline as a decoded reply exposes it: borrowed from the
/// frame, its shape already validated whole, so walking it cannot fail
/// part-way through a drawn run.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphOutline<'a> {
    /// The **resolved** face's em. Carried per record rather than per batch
    /// because a per-scalar fallback crosses faces, and two faces of one
    /// family need not share an em — which a coverage reply hides by
    /// answering in pixels and an outline reply cannot.
    pub units_per_em: u32,
    /// The pen advance in this face's font units.
    pub advance: FontUnits,
    /// How many contours the glyph has. A glyph with no ink — a space — has
    /// none, and is drawn by advancing the pen.
    pub contours: u32,
    /// How many segments the glyph has across every contour.
    pub segments: u32,
    /// What the face this glyph came from could not furnish, so the caller
    /// completes it exactly.
    ///
    /// Per record rather than per batch for the same reason
    /// [`units_per_em`](Self::units_per_em) is: a per-scalar fallback crosses
    /// faces, and a family's primary may carry the `wght` axis where its
    /// Chinese companion does not. One report for the batch would leave the
    /// companion's glyphs unbolded beside bold Latin, or stroke a face that
    /// had already rendered the weight.
    pub synth: Synthesis,
    /// The contour bytes, validated at decode.
    body: &'a [u8],
}

impl<'a> GlyphOutline<'a> {
    /// The glyph's closed contours, in order.
    ///
    /// Contours fill by **non-zero winding**, the TrueType rule: a counter is
    /// a contour wound against the one enclosing it rather than a shape in
    /// its own right, so they are filled *together* under that one rule.
    #[must_use]
    pub const fn contours(&self) -> ContourIter<'a> {
        ContourIter {
            body: self.body,
            at: 0,
            left: self.contours,
        }
    }
}

/// Walks the contours of one [`GlyphOutline`].
#[derive(Clone, Debug)]
pub struct ContourIter<'a> {
    body: &'a [u8],
    at: usize,
    left: u32,
}

impl<'a> Iterator for ContourIter<'a> {
    type Item = GlyphContour<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 || self.at + FONT_OUTLINE_CONTOUR_HEADER_LEN > self.body.len() {
            return None;
        }
        let start = (
            FontUnits::from_raw(read_i32(self.body, self.at)),
            FontUnits::from_raw(read_i32(self.body, self.at + 4)),
        );
        let segments = read_u32(self.body, self.at + 8);
        let from = self.at + FONT_OUTLINE_CONTOUR_HEADER_LEN;
        let to = segment_span(self.body, from, segments).ok()?;
        self.at = to;
        self.left -= 1;
        Some(GlyphContour {
            start,
            count: segments,
            body: &self.body[from..to],
        })
    }
}

/// One closed contour of a glyph outline.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GlyphContour<'a> {
    /// Where the contour begins, and where its last segment returns to.
    pub start: (FontUnits, FontUnits),
    /// How many segments close it.
    count: u32,
    /// The segment bytes, validated at decode.
    body: &'a [u8],
}

impl<'a> GlyphContour<'a> {
    /// How many segments close the contour.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.count
    }

    /// Whether the contour states no segments at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The segments, in order from [`start`](Self::start).
    #[must_use]
    pub const fn segments(&self) -> SegmentIter<'a> {
        SegmentIter {
            body: self.body,
            at: 0,
        }
    }
}

/// Walks the segments of one [`GlyphContour`].
#[derive(Clone, Debug)]
pub struct SegmentIter<'a> {
    body: &'a [u8],
    at: usize,
}

impl Iterator for SegmentIter<'_> {
    type Item = GlyphSegment;

    fn next(&mut self) -> Option<Self::Item> {
        let (segment, len) = read_segment(self.body, self.at)?;
        self.at += len;
        Some(segment)
    }
}

/// The wire length of the segment at `at`.
///
/// One place decides how far a segment reaches, so the walk that validates
/// a frame and the iterator that reads it back cannot disagree.
///
/// # Errors
///
/// [`Errno::BufferTooSmall`] when the bytes run out inside the segment, and
/// [`Errno::OutOfRange`] for a kind outside the closed set.
fn segment_len(body: &[u8], at: usize) -> Result<usize, Errno> {
    if at + FONT_OUTLINE_LINE_LEN > body.len() {
        return Err(Errno::BufferTooSmall);
    }
    let len = match read_u32(body, at) {
        SEGMENT_LINE => FONT_OUTLINE_LINE_LEN,
        SEGMENT_QUADRATIC => FONT_OUTLINE_QUADRATIC_LEN,
        _ => return Err(Errno::OutOfRange),
    };
    if at + len > body.len() {
        return Err(Errno::BufferTooSmall);
    }
    Ok(len)
}

/// Read one segment at `at`, with its wire length.
fn read_segment(body: &[u8], at: usize) -> Option<(GlyphSegment, usize)> {
    let len = segment_len(body, at).ok()?;
    let to = (
        FontUnits::from_raw(read_i32(body, at + 4)),
        FontUnits::from_raw(read_i32(body, at + 8)),
    );
    if len == FONT_OUTLINE_LINE_LEN {
        return Some((GlyphSegment::Line { to }, len));
    }
    let control = (
        FontUnits::from_raw(read_i32(body, at + 12)),
        FontUnits::from_raw(read_i32(body, at + 16)),
    );
    Some((GlyphSegment::Quadratic { control, to }, len))
}

/// Where `segments` segments starting at `from` end.
///
/// # Errors
///
/// Whatever [`segment_len`] refuses one of them with.
fn segment_span(body: &[u8], from: usize, segments: u32) -> Result<usize, Errno> {
    let mut at = from;
    for _ in 0..segments {
        at += segment_len(body, at)?;
    }
    Ok(at)
}

/// The glyph outlines a [`FontRequest::Outlines`] reply answered: a prefix of
/// the requested run, in order, each record borrowing its geometry from the
/// frame.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OutlineBatch<'a> {
    /// The requested family's primary face em — what a run's layout and
    /// baselines are measured in, whichever faces the individual scalars
    /// resolved to.
    pub units_per_em: u32,
    /// The primary face's ascender in its own font units.
    pub ascent: i32,
    /// The primary face's descender magnitude in its own font units.
    pub descent: i32,
    /// The primary face's extra leading between lines, in its own font units.
    pub line_gap: i32,
    glyphs: [GlyphOutline<'a>; FONT_MAX_GLYPH_RUN],
    len: usize,
}

impl<'a> OutlineBatch<'a> {
    /// The value an unanswered slot holds. A batch only ever exposes the
    /// records it decoded, so this never reaches a caller.
    const UNSET: GlyphOutline<'static> = GlyphOutline {
        units_per_em: 16,
        advance: FontUnits::from_raw(0),
        contours: 0,
        segments: 0,
        synth: Synthesis::NONE,
        body: &[],
    };

    /// The glyphs the batch answered, in the order the run asked for them.
    #[must_use]
    pub fn glyphs(&self) -> &[GlyphOutline<'a>] {
        &self.glyphs[..self.len]
    }
}

/// Decode an outline-batch reply, borrowing each record's geometry from
/// `reply`.
///
/// The whole frame is validated here — every count, every segment kind, and
/// the agreement between a record's stated segment total and its contours'
/// own counts — so a caller walking a decoded batch cannot fail part-way
/// through a drawn run.
///
/// # Errors
///
/// * The carried [`Errno`] when the service refused the request.
/// * [`Errno::BufferTooSmall`] — `reply` is shorter than its header, or
///   shorter than the records its counts imply.
/// * [`Errno::OutOfRange`] — a positive or undefined status word, an em
///   outside TrueType's range, a synthesis past its bounds, or a segment
///   kind outside the closed set.
/// * [`Errno::LengthOutOfRange`] — a count outside
///   `1..=`[`FONT_MAX_GLYPH_RUN`], a glyph past
///   [`FONT_MAX_OUTLINE_POINTS`], a record whose contours do not account for
///   its stated segments, or a frame longer than [`FONT_MAX_OUTLINE_REPLY`].
pub fn decode_outlines_reply(reply: &[u8]) -> Result<OutlineBatch<'_>, Errno> {
    let count = batch_count(reply)?;
    if reply.len() < FONT_OUTLINE_REPLY_HEADER_LEN {
        return Err(Errno::BufferTooSmall);
    }
    let units_per_em = read_u32(reply, FONT_BATCH_PREFIX_LEN);
    validate_units_per_em(units_per_em)?;
    let mut batch = OutlineBatch {
        units_per_em,
        ascent: read_i32(reply, FONT_BATCH_PREFIX_LEN + 4),
        descent: read_i32(reply, FONT_BATCH_PREFIX_LEN + 8),
        line_gap: read_i32(reply, FONT_BATCH_PREFIX_LEN + 12),
        glyphs: [OutlineBatch::UNSET; FONT_MAX_GLYPH_RUN],
        len: count,
    };
    let mut at = FONT_OUTLINE_REPLY_HEADER_LEN;
    for slot in batch.glyphs.iter_mut().take(count) {
        let body_at = at + FONT_OUTLINE_RECORD_HEADER_LEN;
        if body_at > reply.len() {
            return Err(Errno::BufferTooSmall);
        }
        let record_em = read_u32(reply, at);
        validate_units_per_em(record_em)?;
        let advance = FontUnits::from_raw(read_i32(reply, at + 4));
        let contours = read_u32(reply, at + 8);
        let segments = read_u32(reply, at + 12);
        if contours
            .checked_add(segments)
            .is_none_or(|points| points > FONT_MAX_OUTLINE_POINTS)
        {
            return Err(Errno::LengthOutOfRange);
        }
        let end = contour_span(reply, body_at, contours, segments)?;
        if end > FONT_MAX_OUTLINE_REPLY {
            return Err(Errno::LengthOutOfRange);
        }
        *slot = GlyphOutline {
            units_per_em: record_em,
            advance,
            contours,
            segments,
            synth: Synthesis::from_wire(read_u32(reply, at + 16))?,
            body: &reply[body_at..end],
        };
        at = end;
    }
    Ok(batch)
}

/// Validate `contours` contours starting at `from` and return where they
/// end, refusing a record whose contours do not account for exactly
/// `segments` segments.
fn contour_span(reply: &[u8], from: usize, contours: u32, segments: u32) -> Result<usize, Errno> {
    let mut at = from;
    let mut seen = 0_u32;
    for _ in 0..contours {
        if at + FONT_OUTLINE_CONTOUR_HEADER_LEN > reply.len() {
            return Err(Errno::BufferTooSmall);
        }
        let count = read_u32(reply, at + 8);
        seen = seen.checked_add(count).ok_or(Errno::LengthOutOfRange)?;
        if seen > segments {
            return Err(Errno::LengthOutOfRange);
        }
        at = segment_span(reply, at + FONT_OUTLINE_CONTOUR_HEADER_LEN, count)?;
    }
    if seen != segments {
        return Err(Errno::LengthOutOfRange);
    }
    Ok(at)
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
        Err(err) => return encode_batch_error_reply(buf, err),
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
