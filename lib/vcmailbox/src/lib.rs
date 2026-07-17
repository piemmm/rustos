//! TAIRiX BCM2711 `VideoCore` firmware mailbox property-channel client.
//!
//! The Raspberry Pi's GPU firmware owns the display pipeline until the
//! ARM side asks for a framebuffer over the **mailbox property
//! channel**: a 16-byte-aligned buffer of little-endian `u32` words the
//! ARM core fills with *tags*, hands to the firmware through the
//! mailbox doorbell registers, and reads back mutated in place. This
//! crate is the one definition of that protocol: it
//! encodes the display-size query and the framebuffer-allocation
//! request, validates the firmware's response fail-closed (the firmware is an external input), and translates between
//! the `VideoCore` **bus** addresses the firmware speaks and the ARM
//! **physical** addresses the kernel can map. Both consumers ride it:
//! the aarch64 port's framebuffer boot console (`plans/PI.md` P7b) and
//! the `drivers/display/rpi_hvs` HVS driver (`plans/PI.md` P7).
//!
//! Two layers, split so the protocol is host-testable without hardware:
//!
//! * The **pure framing layer** ([`FramebufferRequest::encode`],
//!   [`decode_framebuffer_response`], [`encode_display_size_query`],
//!   [`decode_display_size_response`]) operates on a `[u32;
//!   PROPERTY_WORDS]` message and never touches MMIO.
//! * The **transport seam** ([`MailboxTransport`]) submits a message
//!   and returns the firmware-mutated words. [`MmioMailbox`] is the
//!   metal implementation over two capability-gated
//!   [`RegisterWindow`]s (the doorbell register block and the DMA
//!   property buffer); emulation and host tests supply a mock
//!   transport instead — QEMU does not model the firmware, so the
//!   protocol semantics are proven here and on metal, never faked.
//!
//! The mailbox MMIO base is discovered (device tree → `hwtree`), never
//! a compiled-in constant (`plans/PI.md` §4); the caller maps it and
//! hands the windows in — in user space through the capability-gated
//! `MmioMapper`, in the kernel boot path through the port's own mapped
//! identity window. No path here grants authority of its own.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use tairix_abi::driver::display::DisplayFormat;
use tairix_abi::driver::mailbox::MAILBOX_PROPERTY_WORDS;
use tairix_abi::{DriverBindKey, DriverError, HwMatchKey, RegisterWindow};

#[cfg(test)]
mod tests;

/// Device-tree `compatible` string of the `BCM283x` / `BCM2711` `VideoCore`
/// firmware mailbox.
///
/// The Pi 4 device tree names the BCM2711 doorbell block with the original
/// BCM2835 binding. This is the single source of the mailbox match identity: the aarch64 platform discovery emits the mailbox node
/// with this `compatible` key, and the `vcmailbox` service driver's
/// [`BIND_KEYS`] match it — both name this one definition rather than each
/// spelling the string themselves.
pub const MAILBOX_COMPATIBLE: &[u8] = b"brcm,bcm2835-mbox";

/// The bind priority [`BIND_KEYS`] carries. An exact `compatible`-string
/// match ranks above a generic class-wildcard driver.
const BIND_PRIORITY: u16 = 10;

/// The `VideoCore` mailbox service driver's hardware bind table: the BCM2711 firmware mailbox, matched by its device-tree
/// [`MAILBOX_COMPATIBLE`] string. The single source of truth the signed
/// `vcmailbox` bundle's bind table is authored from and `devmgr` resolves a
/// discovered mailbox node against.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    match HwMatchKey::compatible(MAILBOX_COMPATIBLE) {
        Ok(key) => key,
        // Unreachable: the literal is well within `HW_COMPATIBLE_MAX`. A
        // too-long literal would be a compile-time const-eval error here,
        // never a runtime panic.
        Err(_) => panic!("compatible string fits HW_COMPATIBLE_MAX"),
    },
)];

#[cfg(any(test, feature = "mock-firmware"))]
pub mod mock;

/// Failure modes of the mailbox property exchange.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MailboxError {
    /// A doorbell or buffer register access was rejected (out of
    /// bounds / misaligned window access).
    Window,
    /// The doorbell poll budget was exhausted before the firmware
    /// became ready or responded on the property channel.
    Timeout,
    /// The firmware parsed the request and explicitly rejected it.
    FirmwareError,
    /// The response violates the property protocol: an unknown header
    /// code, a missing response bit, a short value length, or an
    /// echoed field that does not match the request.
    MalformedResponse,
    /// The returned buffer address falls outside the 30-bit
    /// `VideoCore` aperture, is zero, or is not page-aligned.
    BadAperture,
    /// The returned geometry is inconsistent (pitch narrower than a
    /// scanline, or a buffer smaller than `pitch * height`).
    BadGeometry,
}

impl MailboxError {
    /// Map to the closest [`DriverError`] for callers that surface
    /// mailbox failures across the driver ABI.
    #[must_use]
    pub const fn as_driver_error(self) -> DriverError {
        match self {
            Self::Window => DriverError::OutOfRange,
            Self::Timeout | Self::FirmwareError => DriverError::DeviceFault,
            Self::MalformedResponse => DriverError::BadMagic,
            Self::BadAperture | Self::BadGeometry => DriverError::LengthOutOfRange,
        }
    }
}

// --- Property message geometry ----------------------------------------

/// Fixed word count of the framebuffer property message (header,
/// six tags, end tag, padded to a 16-byte multiple).
///
/// This is the same `VideoCore` property-channel width the host↔driver
/// [`MailboxChannel`](tairix_abi::driver::mailbox::MailboxChannel) seam
/// transports, so it is defined once in `lib/abi` and re-used here rather
/// than duplicated.
pub const PROPERTY_WORDS: usize = MAILBOX_PROPERTY_WORDS;

/// Byte length of the property message ([`PROPERTY_WORDS`] words).
pub const PROPERTY_LEN_BYTES: usize = PROPERTY_WORDS * 4;

/// Request header code: "process request".
const CODE_REQUEST: u32 = 0;
/// Response header code: request processed successfully.
const CODE_RESPONSE_OK: u32 = 0x8000_0000;
/// Response header code: error parsing the request.
const CODE_RESPONSE_ERROR: u32 = 0x8000_0001;
/// Per-tag response bit in the request/response length word.
const TAG_RESPONSE_BIT: u32 = 1 << 31;

/// Tag: set the physical (display) width/height.
const TAG_SET_PHYSICAL_WH: u32 = 0x0004_8003;
/// Tag: set the virtual (buffer) width/height.
const TAG_SET_VIRTUAL_WH: u32 = 0x0004_8004;
/// Tag: set the colour depth in bits per pixel.
const TAG_SET_DEPTH: u32 = 0x0004_8005;
/// Tag: set the pixel order (`0` = BGR, `1` = RGB).
const TAG_SET_PIXEL_ORDER: u32 = 0x0004_8006;
/// Tag: allocate the framebuffer (request: alignment; response: bus
/// address + size).
const TAG_ALLOCATE: u32 = 0x0004_0001;
/// Tag: get the pitch (bytes per scanline).
const TAG_GET_PITCH: u32 = 0x0004_0008;
/// Tag: notify the `VideoCore` to (re)load the VL805 xHCI controller's
/// firmware after a PCIe reset (request: the VL805 PCI device address;
/// no response value). The BCM2711 PCIe root-complex bring-up asserts
/// `PERST#`, which resets the VL805; on a board without the SPI EEPROM
/// (Pi 4 rev 1.4 and later) that drops its firmware, and only the
/// `VideoCore` can reload it (`plans/PI.md` P10).
const TAG_NOTIFY_XHCI_RESET: u32 = 0x0003_0058;

/// Firmware pixel-order value for BGR ([`DisplayFormat::Bgra8888`]).
const PIXEL_ORDER_BGR: u32 = 0;
/// Firmware pixel-order value for RGB ([`DisplayFormat::Rgba8888`]).
const PIXEL_ORDER_RGB: u32 = 1;

/// Alignment the allocate tag requests for the framebuffer base (one
/// page, so the kernel can map it).
const ALLOC_ALIGN_BYTES: u32 = 4096;

/// Exclusive upper bound of the 30-bit `VideoCore` SDRAM aperture.
const APERTURE_LIMIT: u64 = 0x4000_0000;

/// Mask selecting the 2-bit `VideoCore` bus-alias prefix.
const BUS_ALIAS_MASK: u32 = 0xC000_0000;

/// Default `VideoCore` bus alias for L2-cached SDRAM aliasing
/// (`0xC000_0000`); the firmware and the HVS DMA through a bus address,
/// not the ARM physical address.
pub const DEFAULT_BUS_ALIAS: u32 = 0xC000_0000;

/// Map a [`DisplayFormat`] to the firmware pixel-order tag value,
/// failing closed on a format the firmware protocol has no encoding
/// for (`DisplayFormat` is `#[non_exhaustive]`).
const fn pixel_order(format: DisplayFormat) -> Result<u32, MailboxError> {
    match format {
        DisplayFormat::Rgba8888 => Ok(PIXEL_ORDER_RGB),
        DisplayFormat::Bgra8888 => Ok(PIXEL_ORDER_BGR),
        _ => Err(MailboxError::BadGeometry),
    }
}

// --- Pure framing layer -------------------------------------------------

/// The framebuffer geometry the ARM side asks the firmware for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FramebufferRequest {
    /// Requested width in pixels.
    pub width_px: u32,
    /// Requested height in pixels.
    pub height_px: u32,
    /// Requested pixel encoding (depth 32, pixel order from the
    /// format).
    pub format: DisplayFormat,
}

impl FramebufferRequest {
    /// Encode the framebuffer-allocation property message.
    ///
    /// Tag order: set physical width/height, set virtual width/height,
    /// set depth, set pixel order, allocate buffer, get pitch, end tag.
    ///
    /// # Errors
    ///
    /// [`MailboxError::BadGeometry`] if either dimension is zero or
    /// the requested surface overflows a `u32` byte count.
    pub fn encode(&self) -> Result<[u32; PROPERTY_WORDS], MailboxError> {
        if self.width_px == 0 || self.height_px == 0 {
            return Err(MailboxError::BadGeometry);
        }
        // Reject a surface no firmware response could describe: the
        // byte count must fit the aperture arithmetic downstream.
        let min_pitch = self
            .width_px
            .checked_mul(self.format.bytes_per_pixel())
            .ok_or(MailboxError::BadGeometry)?;
        min_pitch
            .checked_mul(self.height_px)
            .ok_or(MailboxError::BadGeometry)?;

        let mut words = [0u32; PROPERTY_WORDS];
        let mut at = 2; // header written last, once the length is known.
        at = push_tag(
            &mut words,
            at,
            TAG_SET_PHYSICAL_WH,
            &[self.width_px, self.height_px],
        );
        at = push_tag(
            &mut words,
            at,
            TAG_SET_VIRTUAL_WH,
            &[self.width_px, self.height_px],
        );
        at = push_tag(&mut words, at, TAG_SET_DEPTH, &[32]);
        at = push_tag(
            &mut words,
            at,
            TAG_SET_PIXEL_ORDER,
            &[pixel_order(self.format)?],
        );
        at = push_tag(&mut words, at, TAG_ALLOCATE, &[ALLOC_ALIGN_BYTES, 0]);
        at = push_tag(&mut words, at, TAG_GET_PITCH, &[0]);
        // End tag (a zero word) is already in place; account for it.
        at += 1;

        words[0] = words_to_bytes(at);
        words[1] = CODE_REQUEST;
        Ok(words)
    }
}

/// Append one tag (id, value-buffer length, request code, values) at
/// word index `at`, returning the next free index. The message layout
/// is fixed and sized by [`PROPERTY_WORDS`], so the writes are always
/// in bounds.
fn push_tag(words: &mut [u32; PROPERTY_WORDS], at: usize, tag: u32, values: &[u32]) -> usize {
    words[at] = tag;
    words[at + 1] = words_to_bytes(values.len());
    words[at + 2] = 0;
    for (i, &v) in values.iter().enumerate() {
        words[at + 3 + i] = v;
    }
    at + 3 + values.len()
}

/// Byte count of `words` message words. The message is bounded by
/// [`PROPERTY_WORDS`], so the conversion never truncates.
fn words_to_bytes(words: usize) -> u32 {
    u32::try_from(words * 4).unwrap_or(u32::MAX)
}

// --- Response decoding ---------------------------------------------------

/// The firmware's answer to a [`FramebufferRequest`]: the allocated
/// surface, still addressed by its `VideoCore` **bus** address.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FirmwareFramebuffer {
    /// `VideoCore` bus address of the allocated framebuffer.
    pub bus_addr: u32,
    /// Allocated buffer size in bytes.
    pub size_bytes: u32,
    /// Bytes per scanline the firmware chose.
    pub pitch_bytes: u32,
    /// Confirmed width in pixels.
    pub width_px: u32,
    /// Confirmed height in pixels.
    pub height_px: u32,
    /// Confirmed pixel encoding.
    pub format: DisplayFormat,
}

impl FirmwareFramebuffer {
    /// Translate [`Self::bus_addr`] to the ARM physical address the
    /// kernel can map, failing closed on a bad aperture.
    ///
    /// # Errors
    ///
    /// [`MailboxError::BadAperture`] if the address is zero, not
    /// page-aligned, or the buffer does not fit the 30-bit `VideoCore`
    /// aperture.
    pub fn arm_physical_base(&self) -> Result<u64, MailboxError> {
        bus_to_arm_physical(self.bus_addr, self.size_bytes)
    }

    /// The 2-bit `VideoCore` bus alias the firmware allocated the
    /// buffer under (the alias the HVS must DMA through).
    #[must_use]
    pub const fn bus_alias(&self) -> u32 {
        self.bus_addr & BUS_ALIAS_MASK
    }
}

/// Translate a `VideoCore` bus address to an ARM physical address,
/// failing closed when `[bus, bus + size)` does not describe a sane
/// SDRAM window.
///
/// # Errors
///
/// [`MailboxError::BadAperture`] if the address is zero after the
/// alias strip, not page-aligned, or `base + size` exceeds the 30-bit
/// aperture.
pub fn bus_to_arm_physical(bus_addr: u32, size_bytes: u32) -> Result<u64, MailboxError> {
    let base = u64::from(bus_addr & !BUS_ALIAS_MASK);
    if base == 0 || base % u64::from(ALLOC_ALIGN_BYTES) != 0 {
        return Err(MailboxError::BadAperture);
    }
    let end = base
        .checked_add(u64::from(size_bytes))
        .ok_or(MailboxError::BadAperture)?;
    if end > APERTURE_LIMIT {
        return Err(MailboxError::BadAperture);
    }
    Ok(base)
}

/// Translate an ARM *physical* address into the `VideoCore` **bus**
/// address the firmware addresses it through, under `alias` (one of the
/// 2-bit `VideoCore` alias prefixes, e.g. [`DEFAULT_BUS_ALIAS`]).
///
/// The exact inverse of [`bus_to_arm_physical`], used by callers to
/// post a carved property buffer's address on the doorbell
/// (`plans/PI.md` P7/P7b). Fails closed rather than aliasing the wrong
/// page.
///
/// # Errors
///
/// [`MailboxError::BadAperture`] if `phys` is zero or does not fit the
/// 30-bit `VideoCore` SDRAM aperture, or if `alias` carries bits outside
/// the 2-bit alias prefix.
pub fn arm_physical_to_bus(phys: u64, alias: u32) -> Result<u32, MailboxError> {
    if phys == 0 || phys >= APERTURE_LIMIT {
        return Err(MailboxError::BadAperture);
    }
    if alias & !BUS_ALIAS_MASK != 0 {
        return Err(MailboxError::BadAperture);
    }
    // `phys` is below the 30-bit aperture limit, so the cast is exact.
    let base = u32::try_from(phys).map_err(|_| MailboxError::BadAperture)?;
    Ok(base | alias)
}

/// One decoded tag: its value words within the response message.
struct TagValue<'a> {
    words: &'a [u32],
}

/// Find `tag` in the response `words` and return its value slice,
/// enforcing the per-tag protocol invariants (response bit present,
/// response length sane and within the declared value buffer).
///
/// On the `VideoCore` property protocol each tag carries a value-buffer
/// length (`words[at + 1]` — the space the ARM side provisioned) and a
/// request/response code word (`words[at + 2]`); on reply the firmware
/// sets the response bit and writes the byte length it *wanted* to send
/// into that word, which may legally exceed the value buffer, in which
/// case it actually wrote only `min(buffer_length, response_length)`
/// bytes. Every message this crate emits provisions each tag's value
/// buffer to `max(request, response)` words (see `push_tag` callers), so
/// the firmware never needs to truncate a reply for our fixed-layout
/// tags. A reply that nonetheless claims a length greater than the
/// buffer is therefore a firmware fault for these tags, not the benign
/// truncation case, so it is failed closed rather than clamped
/// (the firmware is external input).
fn find_tag(words: &[u32; PROPERTY_WORDS], tag: u32) -> Result<TagValue<'_>, MailboxError> {
    let mut at = 2;
    loop {
        if at + 3 > PROPERTY_WORDS {
            return Err(MailboxError::MalformedResponse);
        }
        let id = words[at];
        if id == 0 {
            return Err(MailboxError::MalformedResponse);
        }
        let buf_bytes = words[at + 1];
        if buf_bytes % 4 != 0 {
            return Err(MailboxError::MalformedResponse);
        }
        let buf_words =
            usize::try_from(buf_bytes / 4).map_err(|_| MailboxError::MalformedResponse)?;
        if at + 3 + buf_words > PROPERTY_WORDS {
            return Err(MailboxError::MalformedResponse);
        }
        if id == tag {
            let code = words[at + 2];
            if code & TAG_RESPONSE_BIT == 0 {
                return Err(MailboxError::MalformedResponse);
            }
            let resp_bytes = code & !TAG_RESPONSE_BIT;
            // `resp_bytes > buf_bytes` is the firmware signalling it
            // wanted to send more than we provisioned; we always size
            // every tag's value buffer to `max(request, response)`, so
            // for our fixed-layout tags this is a fault, not the benign
            // truncation case — fail closed (see the doc comment).
            if resp_bytes % 4 != 0 || resp_bytes > buf_bytes {
                return Err(MailboxError::MalformedResponse);
            }
            let resp_words =
                usize::try_from(resp_bytes / 4).map_err(|_| MailboxError::MalformedResponse)?;
            return Ok(TagValue {
                words: &words[at + 3..at + 3 + resp_words],
            });
        }
        at += 3 + buf_words;
    }
}

/// Read `tag`'s single-word response value.
fn tag_word(words: &[u32; PROPERTY_WORDS], tag: u32) -> Result<u32, MailboxError> {
    let value = find_tag(words, tag)?;
    match value.words {
        [v] => Ok(*v),
        _ => Err(MailboxError::MalformedResponse),
    }
}

/// Read `tag`'s two-word response value.
fn tag_pair(words: &[u32; PROPERTY_WORDS], tag: u32) -> Result<(u32, u32), MailboxError> {
    let value = find_tag(words, tag)?;
    match value.words {
        [a, b] => Ok((*a, *b)),
        _ => Err(MailboxError::MalformedResponse),
    }
}

/// Decode and validate the firmware's response to `request`.
///
/// Every echoed field is checked against the request and the geometry
/// is cross-validated, so a firmware that silently substituted a
/// different surface is rejected rather than scanned out
/// (validate every input; fail closed).
///
/// # Errors
///
/// * [`MailboxError::FirmwareError`] — the firmware rejected the
///   request or returned an unknown header code.
/// * [`MailboxError::MalformedResponse`] — a protocol violation or an
///   echoed field that does not match the request.
/// * [`MailboxError::BadAperture`] — the buffer address is unusable
///   (see [`bus_to_arm_physical`]).
/// * [`MailboxError::BadGeometry`] — the pitch or size is inconsistent
///   with the confirmed geometry.
pub fn decode_framebuffer_response(
    request: &FramebufferRequest,
    words: &[u32; PROPERTY_WORDS],
) -> Result<FirmwareFramebuffer, MailboxError> {
    match words[1] {
        CODE_RESPONSE_OK => {}
        // The firmware's explicit rejection of the request.
        CODE_RESPONSE_ERROR => return Err(MailboxError::FirmwareError),
        // Anything else is a protocol breach, not a firmware verdict.
        _ => return Err(MailboxError::MalformedResponse),
    }
    if words[0] % 4 != 0 || words[0] > words_to_bytes(PROPERTY_WORDS) {
        return Err(MailboxError::MalformedResponse);
    }

    let (phys_w, phys_h) = tag_pair(words, TAG_SET_PHYSICAL_WH)?;
    let (virt_w, virt_h) = tag_pair(words, TAG_SET_VIRTUAL_WH)?;
    let depth = tag_word(words, TAG_SET_DEPTH)?;
    let order = tag_word(words, TAG_SET_PIXEL_ORDER)?;
    let (bus_addr, size_bytes) = tag_pair(words, TAG_ALLOCATE)?;
    let pitch_bytes = tag_word(words, TAG_GET_PITCH)?;

    // The firmware may legally confirm different values; this driver
    // requires its exact geometry (the desktop owns mode selection),
    // so any substitution fails closed.
    if (phys_w, phys_h) != (request.width_px, request.height_px)
        || (virt_w, virt_h) != (request.width_px, request.height_px)
        || depth != 32
        || order != pixel_order(request.format)?
    {
        return Err(MailboxError::MalformedResponse);
    }

    let min_pitch = request
        .width_px
        .checked_mul(request.format.bytes_per_pixel())
        .ok_or(MailboxError::BadGeometry)?;
    if pitch_bytes < min_pitch {
        return Err(MailboxError::BadGeometry);
    }
    let need = u64::from(pitch_bytes)
        .checked_mul(u64::from(request.height_px))
        .ok_or(MailboxError::BadGeometry)?;
    if u64::from(size_bytes) < need {
        return Err(MailboxError::BadGeometry);
    }
    bus_to_arm_physical(bus_addr, size_bytes)?;

    Ok(FirmwareFramebuffer {
        bus_addr,
        size_bytes,
        pitch_bytes,
        width_px: request.width_px,
        height_px: request.height_px,
        format: request.format,
    })
}

// --- Transport seam -------------------------------------------------------

/// Submits one property message to the firmware and returns the
/// firmware-mutated words.
///
/// [`MmioMailbox`] is the metal implementation. Emulation and host
/// tests implement this trait with a mock firmware: QEMU does not
/// model the `VideoCore`, so the protocol layer above this seam is what
/// emulation proves, and the doorbell below it is proven on metal
/// (`plans/PI.md` P7).
pub trait MailboxTransport {
    /// Exchange `message` with the firmware, mutating it in place with
    /// the response.
    ///
    /// # Errors
    ///
    /// Transport-level failures only ([`MailboxError::Window`],
    /// [`MailboxError::Timeout`]); protocol validation belongs to
    /// [`decode_framebuffer_response`].
    fn exchange(&mut self, message: &mut [u32; PROPERTY_WORDS]) -> Result<(), MailboxError>;
}

/// Request the firmware framebuffer over `transport`.
///
/// Encodes `request`, performs one exchange, and decodes/validates the
/// response.
///
/// # Errors
///
/// Any [`MailboxError`] from [`FramebufferRequest::encode`], the
/// transport, or [`decode_framebuffer_response`].
pub fn discover_framebuffer(
    transport: &mut dyn MailboxTransport,
    request: &FramebufferRequest,
) -> Result<FirmwareFramebuffer, MailboxError> {
    let mut message = request.encode()?;
    transport.exchange(&mut message)?;
    decode_framebuffer_response(request, &message)
}

// --- Display-size query ----------------------------------------------------

/// Tag: get the physical display width/height the firmware detected
/// (EDID-derived; both zero when no display is attached).
const TAG_GET_PHYSICAL_WH: u32 = 0x0004_0003;

/// Upper bound on a believable display dimension in pixels.
///
/// A validation bound on the firmware's answer, not
/// a capacity: no display the BCM2711 can drive approaches 16384 pixels
/// on a side, so a larger answer is a protocol breach, not a mode.
const MAX_DISPLAY_DIM: u32 = 16_384;

/// The display geometry the firmware detected on its attached output.
///
/// Both dimensions are zero when the firmware found no display (no
/// HDMI cable / no EDID); callers treat that as "no video output" and
/// fall back rather than allocating a zero-sized surface.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DisplaySize {
    /// Detected width in pixels (zero when no display is attached).
    pub width_px: u32,
    /// Detected height in pixels (zero when no display is attached).
    pub height_px: u32,
}

impl DisplaySize {
    /// Whether the firmware reported an attached, usable display.
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        self.width_px != 0 && self.height_px != 0
    }
}

/// Encode the display-size query property message (one get tag).
#[must_use]
pub fn encode_display_size_query() -> [u32; PROPERTY_WORDS] {
    let mut words = [0u32; PROPERTY_WORDS];
    let mut at = 2;
    at = push_tag(&mut words, at, TAG_GET_PHYSICAL_WH, &[0, 0]);
    // End tag (a zero word) is already in place; account for it.
    at += 1;
    words[0] = words_to_bytes(at);
    words[1] = CODE_REQUEST;
    words
}

/// Decode and validate the firmware's answer to
/// [`encode_display_size_query`].
///
/// # Errors
///
/// * [`MailboxError::FirmwareError`] — the firmware rejected the
///   request or returned an unknown header code.
/// * [`MailboxError::MalformedResponse`] — a protocol violation.
/// * [`MailboxError::BadGeometry`] — a dimension past the
///   `MAX_DISPLAY_DIM` validation bound, or exactly one dimension zero
///   (an attached display has two non-zero extents; "no display" is
///   both zero).
pub fn decode_display_size_response(
    words: &[u32; PROPERTY_WORDS],
) -> Result<DisplaySize, MailboxError> {
    match words[1] {
        CODE_RESPONSE_OK => {}
        CODE_RESPONSE_ERROR => return Err(MailboxError::FirmwareError),
        _ => return Err(MailboxError::MalformedResponse),
    }
    if words[0] % 4 != 0 || words[0] > words_to_bytes(PROPERTY_WORDS) {
        return Err(MailboxError::MalformedResponse);
    }
    let (width_px, height_px) = tag_pair(words, TAG_GET_PHYSICAL_WH)?;
    if width_px > MAX_DISPLAY_DIM || height_px > MAX_DISPLAY_DIM {
        return Err(MailboxError::BadGeometry);
    }
    if (width_px == 0) != (height_px == 0) {
        return Err(MailboxError::BadGeometry);
    }
    Ok(DisplaySize {
        width_px,
        height_px,
    })
}

/// Query the detected display size over `transport`.
///
/// # Errors
///
/// Any [`MailboxError`] from the transport or
/// [`decode_display_size_response`].
pub fn query_display_size(
    transport: &mut dyn MailboxTransport,
) -> Result<DisplaySize, MailboxError> {
    let mut message = encode_display_size_query();
    transport.exchange(&mut message)?;
    decode_display_size_response(&message)
}

// --- Mailbox liveness probe ------------------------------------------------

/// Tag: get the `VideoCore` firmware revision (request: none; response:
/// one revision word). The most benign property call there is — it
/// reads a constant and mutates no hardware state — which makes it the
/// right *liveness probe* for the runtime mailbox path.
const TAG_GET_FIRMWARE_REVISION: u32 = 0x0000_0001;

/// Encode the firmware-revision query property message (one get tag,
/// one response word).
#[must_use]
pub fn encode_firmware_revision_query() -> [u32; PROPERTY_WORDS] {
    let mut words = [0u32; PROPERTY_WORDS];
    let mut at = 2; // header written last, once the length is known.
    at = push_tag(&mut words, at, TAG_GET_FIRMWARE_REVISION, &[0]);
    // End tag (a zero word) is already in place; account for it.
    at += 1;
    words[0] = words_to_bytes(at);
    words[1] = CODE_REQUEST;
    words
}

/// Decode and validate the firmware's answer to
/// [`encode_firmware_revision_query`], returning the revision word.
///
/// # Errors
///
/// * [`MailboxError::FirmwareError`] — the firmware rejected the
///   request or returned an unknown header code.
/// * [`MailboxError::MalformedResponse`] — a protocol violation, or the
///   firmware did not set the per-tag response bit (an unhonoured tag).
pub fn decode_firmware_revision_response(
    words: &[u32; PROPERTY_WORDS],
) -> Result<u32, MailboxError> {
    match words[1] {
        CODE_RESPONSE_OK => {}
        CODE_RESPONSE_ERROR => return Err(MailboxError::FirmwareError),
        _ => return Err(MailboxError::MalformedResponse),
    }
    tag_word(words, TAG_GET_FIRMWARE_REVISION)
}

/// Probe the runtime mailbox by asking the `VideoCore` for its firmware
/// revision over `transport`.
///
/// This is a *liveness probe*, not a feature: the firmware-revision tag
/// reads a constant and touches no device state, so issuing it
/// immediately before a heavier property call (e.g.
/// [`notify_xhci_reset`]) localises a failure of that call. If the probe
/// itself fails or times out, the runtime mailbox path (doorbell
/// mapping, buffer bus address, cache coherency) is broken and the
/// heavier call never had a chance; if the probe succeeds but the
/// heavier call times out, the transport is sound and the firmware is
/// specifically dropping that tag (measure, don't
/// guess).
///
/// # Errors
///
/// Any [`MailboxError`] from the transport or
/// [`decode_firmware_revision_response`].
pub fn query_firmware_revision(transport: &mut dyn MailboxTransport) -> Result<u32, MailboxError> {
    let mut message = encode_firmware_revision_query();
    transport.exchange(&mut message)?;
    decode_firmware_revision_response(&message)
}

// --- VL805 xHCI firmware reload ------------------------------------------

/// Encode the `TAG_NOTIFY_XHCI_RESET` property message carrying the
/// VL805's `dev_addr`.
///
/// Public so the VL805 device driver (`drivers/bus/usb/vl805`) can build
/// the firmware-reload message and run it over the board-neutral
/// [`MailboxChannel`](tairix_abi::driver::mailbox::MailboxChannel) seam
/// without re-deriving the property layout (one
/// definition, shared by [`notify_xhci_reset`] and the driver).
#[must_use]
pub fn encode_xhci_reset(dev_addr: u32) -> [u32; PROPERTY_WORDS] {
    let mut words = [0u32; PROPERTY_WORDS];
    let mut at = 2; // header written last, once the length is known.
    at = push_tag(&mut words, at, TAG_NOTIFY_XHCI_RESET, &[dev_addr]);
    // End tag (a zero word) is already in place; account for it.
    at += 1;
    words[0] = words_to_bytes(at);
    words[1] = CODE_REQUEST;
    words
}

/// Validate the firmware's answer to an xHCI-reset notification and
/// return the **response value word** the firmware wrote back.
///
/// An OK top-level header code is necessary but **not** sufficient: a
/// firmware build that does not recognise (or cannot honour) the tag
/// still stamps the OK header while leaving the tag's own response code
/// untouched, so trusting the header alone would report a reset that
/// never happened. The firmware sets the per-tag response bit only when
/// it actually processed the tag, so this additionally requires that bit
/// (via `find_tag`, which enforces it). Fails closed on a firmware
/// error, a malformed header, or an unhonoured tag (the firmware is external input; an unverified ack is a defect, not a
/// success).
///
/// The returned word is the first value word the firmware wrote into the
/// tag's value buffer (a healthy firmware echoes the `dev_addr` it was
/// asked to act on), or `0` when the firmware returned no value words —
/// it is diagnostic only (logged at bring-up), never gating: an honoured
/// tag is a success regardless of the value.
pub fn decode_xhci_reset_response(words: &[u32; PROPERTY_WORDS]) -> Result<u32, MailboxError> {
    match words[1] {
        CODE_RESPONSE_OK => {}
        CODE_RESPONSE_ERROR => return Err(MailboxError::FirmwareError),
        _ => return Err(MailboxError::MalformedResponse),
    }
    let value = find_tag(words, TAG_NOTIFY_XHCI_RESET)?;
    Ok(value.words.first().copied().unwrap_or(0))
}

/// Ask the `VideoCore` to (re)load the VL805 xHCI controller's firmware
/// after a PCIe reset.
///
/// On the Raspberry Pi 4 the USB-A ports hang off a VL805 xHCI
/// controller behind the BCM2711 PCIe root complex. The previous boot
/// stage hands off with the root complex held in `PERST#` reset, which
/// dropped the VL805's firmware before the OS ran; on a board without
/// the SPI EEPROM (Pi 4 rev 1.4 and later) it has no firmware of its
/// own, so its registers read an uninitialised pattern and `Xhci::open`
/// fails.
/// The `VideoCore` co-processor holds the firmware blob and reloads it
/// on this property tag — but only once the PCIe bus is already
/// configured (the controller enumerated and its BAR based), so the
/// caller runs this *after* the bus is up and *before* xHCI bring-up
/// (`plans/PI.md` P10).
///
/// `dev_addr` is the VL805's PCI address encoded as the firmware expects
/// it: `bus << 20 | slot << 15 | func << 12` (`0x10_0000` for the
/// hardwired Pi 4 VL805 on bus 1).
///
/// Returns the firmware's response value word for the honoured tag — a
/// healthy firmware echoes the `dev_addr` it acted on, or `0` if it wrote
/// no value. The value is diagnostic only (logged at bring-up to confirm
/// the firmware processed the request), never gating: an honoured tag is
/// a success regardless.
///
/// # Errors
///
/// Any [`MailboxError`] from the transport, or
/// [`MailboxError::FirmwareError`] if the firmware rejects the request.
pub fn notify_xhci_reset(
    transport: &mut dyn MailboxTransport,
    dev_addr: u32,
) -> Result<u32, MailboxError> {
    let mut message = encode_xhci_reset(dev_addr);
    transport.exchange(&mut message)?;
    decode_xhci_reset_response(&message)
}

// --- MMIO doorbell transport ----------------------------------------------

/// Byte length of the mailbox doorbell register block.
pub const MAILBOX_REGS_LEN_BYTES: usize = 0x40;

/// Mailbox 0 (VC→ARM) read register.
const REG_MBOX0_READ: usize = 0x00;
/// Mailbox 0 status register.
const REG_MBOX0_STATUS: usize = 0x18;
/// Mailbox write register for ARM→VC posts.
const REG_MBOX1_WRITE: usize = 0x20;

/// Status bit: the mailbox is empty (nothing to read).
const STATUS_EMPTY: u32 = 1 << 30;
/// Status bit: the mailbox is full (no room to write).
const STATUS_FULL: u32 = 1 << 31;

/// The ARM→VC property-tags channel number.
const CHANNEL_PROPERTY: u32 = 8;
/// Mask selecting the channel nibble of a mailbox word.
const CHANNEL_MASK: u32 = 0xF;

/// Default doorbell poll budget. A bound on a *defence* against
/// unresponsive firmware, not a scalable capacity:
/// the firmware answers a property call in well under a millisecond,
/// so a million polls is orders of magnitude past any honest response
/// and the exchange fails closed with [`MailboxError::Timeout`]
/// rather than spinning forever.
pub const DEFAULT_POLL_BUDGET: u32 = 1_000_000;

/// Cache-maintenance hooks for the property buffer when the transport
/// runs over **cacheable** memory.
///
/// The boot framebuffer path runs with the data caches off, where the
/// buffer is already coherent with the firmware, and uses
/// [`BufferCoherency::none`]. A post-MMU caller whose property buffer is
/// Normal write-back memory supplies real hooks so the firmware sees the
/// staged request and the ARM re-reads the firmware's response rather
/// than a stale cached copy (SMP/coherency
/// correctness). Each hook receives the buffer's CPU base and byte
/// length.
#[derive(Copy, Clone)]
pub struct BufferCoherency {
    /// Clean `[base, base + len)` to the point of coherency after the
    /// request is staged, so the firmware's DMA reads the staged words.
    flush: Option<fn(u64, usize)>,
    /// Invalidate `[base, base + len)` before the response is read, so
    /// the ARM re-reads the firmware's writes from memory.
    invalidate: Option<fn(u64, usize)>,
}

impl BufferCoherency {
    /// No maintenance: the buffer is already coherent (caches off).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            flush: None,
            invalidate: None,
        }
    }

    /// Clean-before / invalidate-after hooks for a cacheable buffer.
    #[must_use]
    pub const fn new(flush: fn(u64, usize), invalidate: fn(u64, usize)) -> Self {
        Self {
            flush: Some(flush),
            invalidate: Some(invalidate),
        }
    }

    fn flush(self, base: u64, len: usize) {
        if let Some(flush) = self.flush {
            flush(base, len);
        }
    }

    fn invalidate(self, base: u64, len: usize) {
        if let Some(invalidate) = self.invalidate {
            invalidate(base, len);
        }
    }
}

/// Which doorbell wait a timed-out [`MmioMailbox::exchange`] gave up in,
/// recorded in [`ExchangeStats`] so a metal capture localises a
/// [`MailboxError::Timeout`] without re-running.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum TimeoutStage {
    /// The exchange did not time out.
    #[default]
    None,
    /// Timed out waiting for write room before the request could be
    /// posted (the firmware never drained the inbox).
    PostRoom,
    /// The request was posted, but no completion ever arrived on the
    /// property channel within the budget (the firmware never replied —
    /// e.g. it silently dropped the tag).
    Response,
}

/// Diagnostics from the most recent [`MmioMailbox::exchange`], retained
/// regardless of success or failure so the caller can log *why* a
/// property call behaved as it did.
///
/// The decisive datapoint for a [`MailboxError::Timeout`] is
/// [`Self::timeout_stage`]: `PostRoom` means the firmware never even
/// accepted the request, while `Response` means it accepted it but never
/// replied — two very different faults that the bare `Timeout` error
/// cannot tell apart (measure, don't guess). The
/// other fields pin the posted bus-address word, the last status
/// register value observed, and how many foreign-channel completions
/// were drained before ours (or would have been).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ExchangeStats {
    /// The exact word posted to the write register (the buffer bus
    /// address OR-ed with the property channel nibble).
    pub posted_word: u32,
    /// Polls spent waiting for write room before posting.
    pub post_room_polls: u32,
    /// Completion reads taken while waiting for our channel's reply
    /// (each either ours or a drained foreign-channel post).
    pub response_reads: u32,
    /// Of those, how many were completions for a *different* channel,
    /// drained and ignored.
    pub foreign_channel_reads: u32,
    /// The last value read from the mailbox status register.
    pub last_status: u32,
    /// Which wait the exchange timed out in, or [`TimeoutStage::None`].
    pub timeout_stage: TimeoutStage,
}

/// The metal mailbox transport: the doorbell register block plus a
/// DMA-visible property buffer, both reached through capability-gated
/// [`RegisterWindow`]s (no ambient authority).
pub struct MmioMailbox {
    regs: RegisterWindow,
    buffer: RegisterWindow,
    buffer_bus_addr: u32,
    poll_budget: u32,
    coherency: BufferCoherency,
    last_exchange: ExchangeStats,
}

impl MmioMailbox {
    /// Bring the transport up over the mapped doorbell `regs` and the
    /// mapped property `buffer` whose memory the firmware addresses
    /// via `buffer_bus_addr`.
    ///
    /// # Errors
    ///
    /// * [`MailboxError::Window`] — `regs` is shorter than the
    ///   doorbell block or `buffer` is shorter than one property
    ///   message.
    /// * [`MailboxError::BadAperture`] — `buffer_bus_addr` is not
    ///   16-byte aligned (the low nibble carries the channel) or the
    ///   buffer does not fit the `VideoCore` aperture.
    pub fn new(
        regs: RegisterWindow,
        buffer: RegisterWindow,
        buffer_bus_addr: u32,
        poll_budget: u32,
    ) -> Result<Self, MailboxError> {
        Self::with_coherency(
            regs,
            buffer,
            buffer_bus_addr,
            poll_budget,
            BufferCoherency::none(),
        )
    }

    /// As [`MmioMailbox::new`] but with explicit property-buffer cache
    /// maintenance for a transport running over cacheable memory
    /// (post-MMU); the boot path uses [`MmioMailbox::new`] (caches off,
    /// already coherent).
    ///
    /// # Errors
    ///
    /// As [`MmioMailbox::new`].
    pub fn with_coherency(
        regs: RegisterWindow,
        buffer: RegisterWindow,
        buffer_bus_addr: u32,
        poll_budget: u32,
        coherency: BufferCoherency,
    ) -> Result<Self, MailboxError> {
        if regs.len() < MAILBOX_REGS_LEN_BYTES || buffer.len() < PROPERTY_LEN_BYTES {
            return Err(MailboxError::Window);
        }
        if buffer_bus_addr & CHANNEL_MASK != 0 {
            return Err(MailboxError::BadAperture);
        }
        let base = u64::from(buffer_bus_addr & !BUS_ALIAS_MASK);
        let end = base
            .checked_add(u64::from(words_to_bytes(PROPERTY_WORDS)))
            .ok_or(MailboxError::BadAperture)?;
        if base == 0 || end > APERTURE_LIMIT {
            return Err(MailboxError::BadAperture);
        }
        Ok(Self {
            regs,
            buffer,
            buffer_bus_addr,
            poll_budget,
            coherency,
            last_exchange: ExchangeStats::default(),
        })
    }

    /// Diagnostics from the most recent [`MmioMailbox::exchange`] (a
    /// fresh default before the first call), retained on both success
    /// and failure so the caller can log a timed-out exchange's stage
    /// and counts without re-running it.
    #[must_use]
    pub fn last_exchange_stats(&self) -> ExchangeStats {
        self.last_exchange
    }

    /// Poll `status_reg` until `busy_bit` clears, within the budget,
    /// returning `(polls, last_status)` on success.
    ///
    /// A faulting register read fails closed with
    /// [`MailboxError::Window`]; exhausting the budget fails with
    /// [`MailboxError::Timeout`]. The caller records the returned counts
    /// (and the timeout stage) into [`ExchangeStats`].
    fn wait_clear(&self, status_reg: usize, busy_bit: u32) -> Result<(u32, u32), MailboxError> {
        for poll in 0..self.poll_budget {
            let status = self
                .regs
                .read_u32(status_reg)
                .map_err(|_| MailboxError::Window)?;
            if status & busy_bit == 0 {
                return Ok((poll, status));
            }
            core::hint::spin_loop();
        }
        Err(MailboxError::Timeout)
    }
}

impl MmioMailbox {
    /// The instrumented body of [`MmioMailbox::exchange`], threading
    /// diagnostics into `stats` so the public method records them on
    /// every return path.
    fn run_exchange(
        &mut self,
        message: &mut [u32; PROPERTY_WORDS],
        stats: &mut ExchangeStats,
    ) -> Result<(), MailboxError> {
        // Stage the request into the DMA-visible property buffer.
        for (i, &word) in message.iter().enumerate() {
            self.buffer
                .write_u32(i * 4, word)
                .map_err(|_| MailboxError::Window)?;
        }
        // Push the staged words to memory so the firmware's DMA reads
        // them rather than a copy stranded in the ARM data cache (a
        // no-op when the buffer is already coherent, e.g. caches off).
        self.coherency
            .flush(self.buffer.phys_base(), PROPERTY_LEN_BYTES);

        // Ring the doorbell: wait for write room, then post the
        // buffer's bus address tagged with the property channel.
        let (post_polls, post_status) =
            self.wait_clear(REG_MBOX0_STATUS, STATUS_FULL)
                .map_err(|e| {
                    if e == MailboxError::Timeout {
                        stats.timeout_stage = TimeoutStage::PostRoom;
                    }
                    e
                })?;
        stats.post_room_polls = post_polls;
        stats.last_status = post_status;
        let posted = self.buffer_bus_addr | CHANNEL_PROPERTY;
        stats.posted_word = posted;
        self.regs
            .write_u32(REG_MBOX1_WRITE, posted)
            .map_err(|_| MailboxError::Window)?;

        // Wait for the firmware's completion post on our channel,
        // discarding traffic for other channels within the budget.
        loop {
            if stats.response_reads >= self.poll_budget {
                stats.timeout_stage = TimeoutStage::Response;
                return Err(MailboxError::Timeout);
            }
            stats.response_reads += 1;
            let (_, empty_status) =
                self.wait_clear(REG_MBOX0_STATUS, STATUS_EMPTY)
                    .map_err(|e| {
                        if e == MailboxError::Timeout {
                            stats.timeout_stage = TimeoutStage::Response;
                        }
                        e
                    })?;
            stats.last_status = empty_status;
            let word = self
                .regs
                .read_u32(REG_MBOX0_READ)
                .map_err(|_| MailboxError::Window)?;
            if word & CHANNEL_MASK == CHANNEL_PROPERTY {
                if word & !CHANNEL_MASK != self.buffer_bus_addr {
                    // A property completion for a buffer we did not
                    // post: protocol breach, fail closed.
                    return Err(MailboxError::MalformedResponse);
                }
                break;
            }
            stats.foreign_channel_reads += 1;
            core::hint::spin_loop();
        }

        // Drop any stale cached copy so the read-back observes the
        // firmware's writes from memory, not the request we staged.
        self.coherency
            .invalidate(self.buffer.phys_base(), PROPERTY_LEN_BYTES);

        // Read the firmware-mutated message back.
        for (i, word) in message.iter_mut().enumerate() {
            *word = self
                .buffer
                .read_u32(i * 4)
                .map_err(|_| MailboxError::Window)?;
        }
        Ok(())
    }
}

impl MailboxTransport for MmioMailbox {
    fn exchange(&mut self, message: &mut [u32; PROPERTY_WORDS]) -> Result<(), MailboxError> {
        let mut stats = ExchangeStats::default();
        let result = self.run_exchange(message, &mut stats);
        self.last_exchange = stats;
        result
    }
}
