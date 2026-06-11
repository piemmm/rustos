//! BCM2711 `VideoCore` firmware mailbox property-channel client.
//!
//! The Raspberry Pi's GPU firmware owns the display pipeline until the
//! ARM side asks for a framebuffer over the **mailbox property
//! channel**: a 16-byte-aligned buffer of little-endian `u32` words the
//! ARM core fills with *tags*, hands to the firmware through the
//! mailbox doorbell registers, and reads back mutated in place. This
//! module is the `plans/PI.md` P7 protocol layer: it encodes the
//! framebuffer-allocation request, validates the firmware's response
//! fail-closed (`AGENTS.md` §5.4 — the firmware is an external input),
//! translates the returned `VideoCore` **bus** address into the ARM
//! **physical** address the kernel can map, and produces the
//! [`ScanoutConfig`] that the [`RpiHvs`](crate::RpiHvs) driver
//! consumes.
//!
//! Two layers, split so the protocol is host-testable without hardware:
//!
//! * The **pure framing layer** ([`FramebufferRequest::encode`],
//!   [`decode_framebuffer_response`]) operates on a `[u32;
//!   PROPERTY_WORDS]` message and never touches MMIO.
//! * The **transport seam** ([`MailboxTransport`]) submits a message
//!   and returns the firmware-mutated words. [`MmioMailbox`] is the
//!   metal implementation over two capability-gated
//!   [`RegisterWindow`]s (the doorbell register block and the DMA
//!   property buffer); emulation and host tests supply a mock
//!   transport instead — QEMU does not model the firmware, so the
//!   protocol semantics are proven here and on metal, never faked
//!   (`AGENTS.md` §2.1).
//!
//! The mailbox MMIO base is discovered (device tree → `hwtree`), never
//! a compiled-in constant (`plans/PI.md` §4); the caller maps it
//! through the capability-gated `MmioMapper` and hands the window in.

use rustos_abi::driver::display::DisplayFormat;
use rustos_abi::{DriverError, RegisterWindow};

use crate::dlist::ScanoutConfig;

#[cfg(test)]
#[path = "mailbox_tests.rs"]
pub(crate) mod tests;

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
pub const PROPERTY_WORDS: usize = 32;

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

    /// Produce the [`ScanoutConfig`] for [`RpiHvs::open`](crate::RpiHvs::open).
    ///
    /// # Errors
    ///
    /// [`MailboxError::BadAperture`] on a bad buffer address (see
    /// [`Self::arm_physical_base`]).
    pub fn scanout_config(&self) -> Result<ScanoutConfig, MailboxError> {
        Ok(ScanoutConfig {
            phys_base: self.arm_physical_base()?,
            width_px: self.width_px,
            height_px: self.height_px,
            stride_bytes: self.pitch_bytes,
            format: self.format,
        })
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
/// 2-bit `VideoCore` alias prefixes, e.g. [`crate::DEFAULT_BUS_ALIAS`]).
///
/// The exact inverse of [`bus_to_arm_physical`], used by the driver-host
/// wiring ([`crate::wiring`]) to post the carved property buffer's
/// address on the doorbell (`plans/PI.md` P7). Fails closed rather than
/// aliasing the wrong page.
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
/// (`AGENTS.md` §5.4 — validate every input; fail closed).
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
/// response. The caller turns the result into the driver's
/// [`ScanoutConfig`] via [`FirmwareFramebuffer::scanout_config`].
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

// --- MMIO doorbell transport ----------------------------------------------

/// Byte length of the mailbox doorbell register block.
pub const MAILBOX_REGS_LEN_BYTES: usize = 0x40;

/// Mailbox 0 (VC→ARM) read register.
const REG_MBOX0_READ: usize = 0x00;
/// Mailbox 0 status register.
const REG_MBOX0_STATUS: usize = 0x18;
/// Mailbox 1 (ARM→VC) write register.
const REG_MBOX1_WRITE: usize = 0x20;
/// Mailbox 1 status register.
const REG_MBOX1_STATUS: usize = 0x38;

/// Status bit: the mailbox is empty (nothing to read).
const STATUS_EMPTY: u32 = 1 << 30;
/// Status bit: the mailbox is full (no room to write).
const STATUS_FULL: u32 = 1 << 31;

/// The ARM→VC property-tags channel number.
const CHANNEL_PROPERTY: u32 = 8;
/// Mask selecting the channel nibble of a mailbox word.
const CHANNEL_MASK: u32 = 0xF;

/// Default doorbell poll budget. A bound on a *defence* against
/// unresponsive firmware, not a scalable capacity (`AGENTS.md` §24.4):
/// the firmware answers a property call in well under a millisecond,
/// so a million polls is orders of magnitude past any honest response
/// and the exchange fails closed with [`MailboxError::Timeout`]
/// rather than spinning forever (`AGENTS.md` §2.1).
pub const DEFAULT_POLL_BUDGET: u32 = 1_000_000;

/// The metal mailbox transport: the doorbell register block plus a
/// DMA-visible property buffer, both reached through capability-gated
/// [`RegisterWindow`]s (`AGENTS.md` §4 — no ambient authority).
pub struct MmioMailbox {
    regs: RegisterWindow,
    buffer: RegisterWindow,
    buffer_bus_addr: u32,
    poll_budget: u32,
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
        })
    }

    /// Poll `status_reg` until `busy_bit` clears, within the budget.
    fn wait_clear(&self, status_reg: usize, busy_bit: u32) -> Result<(), MailboxError> {
        for _ in 0..self.poll_budget {
            let status = self
                .regs
                .read_u32(status_reg)
                .map_err(|_| MailboxError::Window)?;
            if status & busy_bit == 0 {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(MailboxError::Timeout)
    }
}

impl MailboxTransport for MmioMailbox {
    fn exchange(&mut self, message: &mut [u32; PROPERTY_WORDS]) -> Result<(), MailboxError> {
        // Stage the request into the DMA-visible property buffer.
        for (i, &word) in message.iter().enumerate() {
            self.buffer
                .write_u32(i * 4, word)
                .map_err(|_| MailboxError::Window)?;
        }

        // Ring the doorbell: wait for write room, then post the
        // buffer's bus address tagged with the property channel.
        self.wait_clear(REG_MBOX1_STATUS, STATUS_FULL)?;
        self.regs
            .write_u32(REG_MBOX1_WRITE, self.buffer_bus_addr | CHANNEL_PROPERTY)
            .map_err(|_| MailboxError::Window)?;

        // Wait for the firmware's completion post on our channel,
        // discarding traffic for other channels within the budget.
        let mut polls = 0;
        loop {
            if polls >= self.poll_budget {
                return Err(MailboxError::Timeout);
            }
            polls += 1;
            self.wait_clear(REG_MBOX0_STATUS, STATUS_EMPTY)?;
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
            core::hint::spin_loop();
        }

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
