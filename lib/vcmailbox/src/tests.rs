//! Host unit tests for the mailbox property-channel client: framing,
//! response validation, bus↔physical translation, and the MMIO
//! doorbell transport — all against in-process mocks (the firmware is
//! not emulable, so its protocol semantics are modelled here and the
//! doorbell against a RAM-backed register window).

use super::*;
use crate::mock::MockFirmware;

/// Geometry every framebuffer test requests: 640×480 BGRA.
fn request() -> FramebufferRequest {
    FramebufferRequest {
        width_px: 640,
        height_px: 480,
        format: DisplayFormat::Bgra8888,
    }
}

/// A healthy mock response to [`request`].
fn ok_response() -> [u32; PROPERTY_WORDS] {
    let mut words = request().encode().expect("encode");
    MockFirmware::healthy().respond(&mut words);
    words
}

// --- Framing -----------------------------------------------------------

#[test]
fn encode_lays_out_header_tags_and_end_marker() {
    let words = request().encode().expect("encode");
    // Header: 30 used words (2 header + 27 tag + 1 end), request code.
    assert_eq!(words[0], 30 * 4, "message byte length");
    assert_eq!(words[1], CODE_REQUEST);
    // Tag order and request values.
    assert_eq!(words[2..7], [TAG_SET_PHYSICAL_WH, 8, 0, 640, 480]);
    assert_eq!(words[7..12], [TAG_SET_VIRTUAL_WH, 8, 0, 640, 480]);
    assert_eq!(words[12..16], [TAG_SET_DEPTH, 4, 0, 32]);
    assert_eq!(
        words[16..20],
        [TAG_SET_PIXEL_ORDER, 4, 0, PIXEL_ORDER_BGR],
        "BGRA requests BGR pixel order"
    );
    assert_eq!(
        words[20..25],
        [TAG_ALLOCATE, 8, 0, ALLOC_ALIGN_BYTES, 0],
        "allocate requests page alignment"
    );
    assert_eq!(words[25..29], [TAG_GET_PITCH, 4, 0, 0]);
    assert_eq!(words[29], 0, "end tag");
}

#[test]
fn encode_maps_rgba_to_rgb_pixel_order() {
    let mut req = request();
    req.format = DisplayFormat::Rgba8888;
    let words = req.encode().expect("encode");
    assert_eq!(words[19], PIXEL_ORDER_RGB);
}

#[test]
fn encode_rejects_degenerate_geometry() {
    let mut zero_w = request();
    zero_w.width_px = 0;
    assert_eq!(zero_w.encode(), Err(MailboxError::BadGeometry));
    let mut zero_h = request();
    zero_h.height_px = 0;
    assert_eq!(zero_h.encode(), Err(MailboxError::BadGeometry));
    let mut huge = request();
    huge.width_px = u32::MAX;
    huge.height_px = u32::MAX;
    assert_eq!(huge.encode(), Err(MailboxError::BadGeometry));
}

// --- Decoding (happy path) ----------------------------------------------

#[test]
fn discover_round_trips_through_a_healthy_firmware() {
    let mut firmware = MockFirmware::healthy();
    let fb = discover_framebuffer(&mut firmware, &request()).expect("discover");
    assert_eq!(fb.bus_addr, firmware.fb_bus);
    assert_eq!(fb.size_bytes, firmware.fb_size);
    assert_eq!(fb.pitch_bytes, firmware.fb_pitch);
    assert_eq!((fb.width_px, fb.height_px), (640, 480));
    assert_eq!(fb.format, DisplayFormat::Bgra8888);
    assert_eq!(fb.bus_alias(), 0xC000_0000);
    assert_eq!(fb.arm_physical_base().expect("translate"), 0x1000_0000);
}

// --- Decoding (fail closed) ---------------------------------------------

#[test]
fn decode_rejects_firmware_error_and_unknown_codes() {
    let mut err = ok_response();
    err[1] = CODE_RESPONSE_ERROR;
    assert_eq!(
        decode_framebuffer_response(&request(), &err),
        Err(MailboxError::FirmwareError)
    );
    let mut unknown = ok_response();
    unknown[1] = 0x1234_5678;
    assert_eq!(
        decode_framebuffer_response(&request(), &unknown),
        Err(MailboxError::MalformedResponse),
        "an unknown header code is a protocol breach, not a firmware verdict"
    );
}

#[test]
fn decode_rejects_bad_header_length() {
    let mut words = ok_response();
    words[0] = 30 * 4 + 1; // not a word multiple
    assert_eq!(
        decode_framebuffer_response(&request(), &words),
        Err(MailboxError::MalformedResponse)
    );
    let mut oversized = ok_response();
    oversized[0] = words_to_bytes(PROPERTY_WORDS + 1);
    assert_eq!(
        decode_framebuffer_response(&request(), &oversized),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn decode_rejects_missing_response_bit() {
    let mut words = ok_response();
    words[22] &= !TAG_RESPONSE_BIT; // allocate tag's req/resp word
    assert_eq!(
        decode_framebuffer_response(&request(), &words),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn decode_rejects_short_and_oversized_tag_responses() {
    let mut short = ok_response();
    short[22] = TAG_RESPONSE_BIT | 4; // allocate must answer 8 bytes
    assert_eq!(
        decode_framebuffer_response(&request(), &short),
        Err(MailboxError::MalformedResponse)
    );
    let mut oversized = ok_response();
    oversized[22] = TAG_RESPONSE_BIT | 0xC; // larger than the value buffer
    assert_eq!(
        decode_framebuffer_response(&request(), &oversized),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn decode_rejects_substituted_geometry() {
    let mut width = ok_response();
    width[5] = 1024; // physical width echo
    assert_eq!(
        decode_framebuffer_response(&request(), &width),
        Err(MailboxError::MalformedResponse)
    );
    let mut depth = ok_response();
    depth[15] = 16; // depth echo
    assert_eq!(
        decode_framebuffer_response(&request(), &depth),
        Err(MailboxError::MalformedResponse)
    );
    let mut order = ok_response();
    order[19] = PIXEL_ORDER_RGB; // pixel-order echo
    assert_eq!(
        decode_framebuffer_response(&request(), &order),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn decode_rejects_missing_tag() {
    let mut words = ok_response();
    words[25] = 0x0004_FFFF; // replace get-pitch with an unknown tag
    assert_eq!(
        decode_framebuffer_response(&request(), &words),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn decode_rejects_inconsistent_pitch_and_size() {
    let mut narrow = ok_response();
    narrow[28] = 640 * 4 - 1; // pitch narrower than a scanline
    assert_eq!(
        decode_framebuffer_response(&request(), &narrow),
        Err(MailboxError::BadGeometry)
    );
    let mut small = ok_response();
    small[24] = MockFirmware::healthy().fb_pitch * 480 - 1; // buffer smaller than the surface
    assert_eq!(
        decode_framebuffer_response(&request(), &small),
        Err(MailboxError::BadGeometry)
    );
}

#[test]
fn decode_rejects_bad_buffer_aperture() {
    let mut zero = ok_response();
    zero[23] = 0xC000_0000; // zero after the alias strip
    assert_eq!(
        decode_framebuffer_response(&request(), &zero),
        Err(MailboxError::BadAperture)
    );
}

// --- Bus ↔ physical translation -----------------------------------------

#[test]
fn bus_translation_strips_each_alias() {
    for alias in [0x0000_0000u32, 0x4000_0000, 0x8000_0000, 0xC000_0000] {
        assert_eq!(
            bus_to_arm_physical(alias | 0x1000_0000, 0x1000).expect("translate"),
            0x1000_0000,
            "alias {alias:#010x}"
        );
    }
}

#[test]
fn bus_translation_fails_closed_on_bad_apertures() {
    // Zero base after the alias strip.
    assert_eq!(
        bus_to_arm_physical(0xC000_0000, 0x1000),
        Err(MailboxError::BadAperture)
    );
    // Not page-aligned.
    assert_eq!(
        bus_to_arm_physical(0xC000_0800, 0x1000),
        Err(MailboxError::BadAperture)
    );
    // Buffer end beyond the 30-bit aperture.
    assert_eq!(
        bus_to_arm_physical(0xFFFF_F000, 0x2000),
        Err(MailboxError::BadAperture)
    );
    // Exactly filling the aperture is fine.
    assert_eq!(
        bus_to_arm_physical(0xFFFF_F000, 0x1000).expect("translate"),
        0x3FFF_F000
    );
}

#[test]
fn physical_to_bus_round_trips_each_alias() {
    for alias in [0x0000_0000u32, 0x4000_0000, 0x8000_0000, 0xC000_0000] {
        let bus = arm_physical_to_bus(0x1000_0000, alias).expect("translate");
        assert_eq!(bus, alias | 0x1000_0000, "alias {alias:#010x}");
        assert_eq!(
            bus_to_arm_physical(bus & !0xF, 0x1000).expect("round trip"),
            0x1000_0000
        );
    }
}

#[test]
fn physical_to_bus_fails_closed() {
    // Zero physical base.
    assert_eq!(
        arm_physical_to_bus(0, 0xC000_0000),
        Err(MailboxError::BadAperture)
    );
    // At and beyond the 30-bit aperture limit.
    assert_eq!(
        arm_physical_to_bus(0x4000_0000, 0xC000_0000),
        Err(MailboxError::BadAperture)
    );
    assert_eq!(
        arm_physical_to_bus(u64::MAX, 0xC000_0000),
        Err(MailboxError::BadAperture)
    );
    // Bits outside the 2-bit alias prefix.
    assert_eq!(
        arm_physical_to_bus(0x1000_0000, 0x2000_0000),
        Err(MailboxError::BadAperture)
    );
    // The last in-aperture address still translates.
    assert_eq!(
        arm_physical_to_bus(0x3FFF_FFF0, 0xC000_0000).expect("translate"),
        0xFFFF_FFF0
    );
}

// --- MMIO doorbell transport ---------------------------------------------

/// RAM backing for a mock register window (4-byte aligned).
#[repr(align(8))]
struct Aligned<const N: usize>([u8; N]);

/// Build a [`RegisterWindow`] over an aligned RAM buffer.
fn window_over(buf: &mut [u8], phys: u64) -> RegisterWindow {
    let len = buf.len();
    let base = core::ptr::NonNull::new(buf.as_mut_ptr()).expect("buffer is non-null");
    // SAFETY: `base` covers exactly `len` bytes of the mutably borrowed
    // buffer, which outlives the window inside each test; the mutable
    // borrow guarantees no aliasing reference. `phys` is synthetic.
    unsafe { RegisterWindow::from_mapping(phys, base, len) }
}

/// Bus address the tests stage the property buffer at (16-byte
/// aligned, inside the aperture, no alias).
const TEST_BUFFER_BUS: u32 = 0x0001_0000;

/// Read the `u32` at byte `off` of a RAM register block.
fn reg_word(regs: &Aligned<MAILBOX_REGS_LEN_BYTES>, off: usize) -> u32 {
    u32::from_le_bytes([
        regs.0[off],
        regs.0[off + 1],
        regs.0[off + 2],
        regs.0[off + 3],
    ])
}

/// Write the `u32` at byte `off` of a RAM register block.
fn set_reg_word(regs: &mut Aligned<MAILBOX_REGS_LEN_BYTES>, off: usize, value: u32) {
    regs.0[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

/// A ready-to-exchange doorbell block: both statuses clear and the
/// read register pre-loaded with the property completion for
/// [`TEST_BUFFER_BUS`]. RAM cannot model read side effects, so the
/// success path sees the completion on its first poll.
fn ready_regs() -> Aligned<MAILBOX_REGS_LEN_BYTES> {
    let mut regs = Aligned([0u8; MAILBOX_REGS_LEN_BYTES]);
    set_reg_word(
        &mut regs,
        REG_MBOX0_READ,
        TEST_BUFFER_BUS | CHANNEL_PROPERTY,
    );
    regs
}

#[test]
fn mmio_mailbox_validates_its_windows_and_buffer_address() {
    let mut short_regs = Aligned([0u8; 8]);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    assert_eq!(
        MmioMailbox::new(
            window_over(&mut short_regs.0, 0),
            window_over(&mut buffer.0, 0),
            TEST_BUFFER_BUS,
            8,
        )
        .err(),
        Some(MailboxError::Window),
        "short register block"
    );

    let mut regs = ready_regs();
    let mut short_buffer = Aligned([0u8; 8]);
    assert_eq!(
        MmioMailbox::new(
            window_over(&mut regs.0, 0),
            window_over(&mut short_buffer.0, 0),
            TEST_BUFFER_BUS,
            8,
        )
        .err(),
        Some(MailboxError::Window),
        "short property buffer"
    );

    for (bus, why) in [
        (TEST_BUFFER_BUS | 0x4, "channel bits set"),
        (0xC000_0000, "zero base after alias strip"),
        (0x3FFF_FFF0, "buffer end past the aperture"),
    ] {
        let mut regs = ready_regs();
        let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
        assert_eq!(
            MmioMailbox::new(
                window_over(&mut regs.0, 0),
                window_over(&mut buffer.0, 0),
                bus,
                8,
            )
            .err(),
            Some(MailboxError::BadAperture),
            "{why}"
        );
    }
}

#[test]
fn mmio_exchange_stages_rings_and_reads_back() {
    let mut regs = ready_regs();
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    {
        let mut mailbox = MmioMailbox::new(
            window_over(&mut regs.0, 0),
            window_over(&mut buffer.0, 0),
            TEST_BUFFER_BUS,
            8,
        )
        .expect("construct");

        let request = request().encode().expect("encode");
        let mut message = request;
        mailbox.exchange(&mut message).expect("exchange");
        // RAM echoes the staged request back (no firmware to mutate it).
        assert_eq!(message, request);
    }
    // The doorbell write posted the buffer's bus address on channel 8.
    assert_eq!(
        reg_word(&regs, REG_MBOX1_WRITE),
        TEST_BUFFER_BUS | CHANNEL_PROPERTY
    );
    // The property buffer holds the staged message.
    assert_eq!(
        u32::from_le_bytes([buffer.0[0], buffer.0[1], buffer.0[2], buffer.0[3]]),
        30 * 4
    );
}

#[test]
fn mmio_exchange_waits_on_the_property_mailbox_status_before_writing() {
    let mut regs = ready_regs();
    set_reg_word(&mut regs, REG_MBOX0_STATUS, STATUS_FULL);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut regs.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");

    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));
    assert_eq!(reg_word(&regs, REG_MBOX1_WRITE), 0);
}

#[test]
fn mmio_exchange_times_out_when_the_firmware_never_answers() {
    // Write side jammed: the property mailbox reports FULL forever.
    let mut full = ready_regs();
    set_reg_word(&mut full, REG_MBOX0_STATUS, STATUS_FULL);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut full.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));

    // Read side silent: MBOX0 reports EMPTY forever.
    let mut empty = ready_regs();
    set_reg_word(&mut empty, REG_MBOX0_STATUS, STATUS_EMPTY);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut empty.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));

    // Chatter on another channel only: the budget runs out.
    let mut other_channel = ready_regs();
    set_reg_word(&mut other_channel, REG_MBOX0_READ, TEST_BUFFER_BUS | 0x3);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut other_channel.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));
}

#[test]
fn mmio_exchange_stats_localise_the_timeout_stage() {
    // Write side jammed (FULL forever): the exchange never gets to post,
    // so the recorded stage is `PostRoom` and no word was posted.
    let mut full = ready_regs();
    set_reg_word(&mut full, REG_MBOX0_STATUS, STATUS_FULL);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut full.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));
    let stats = mailbox.last_exchange_stats();
    assert_eq!(stats.timeout_stage, TimeoutStage::PostRoom);
    assert_eq!(stats.posted_word, 0);

    // Read side silent (EMPTY forever): the request posts, but no
    // completion ever arrives, so the recorded stage is `Response` and the
    // posted word is the buffer bus address on the property channel.
    let mut empty = ready_regs();
    set_reg_word(&mut empty, REG_MBOX0_STATUS, STATUS_EMPTY);
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut empty.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(mailbox.exchange(&mut message), Err(MailboxError::Timeout));
    let stats = mailbox.last_exchange_stats();
    assert_eq!(stats.timeout_stage, TimeoutStage::Response);
    assert_eq!(stats.posted_word, TEST_BUFFER_BUS | CHANNEL_PROPERTY);

    // Success path (RAM echoes our own completion): no timeout stage, and
    // the posted word is recorded for the diagnostic.
    let mut regs = ready_regs();
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut regs.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    mailbox.exchange(&mut message).expect("exchange");
    let stats = mailbox.last_exchange_stats();
    assert_eq!(stats.timeout_stage, TimeoutStage::None);
    assert_eq!(stats.posted_word, TEST_BUFFER_BUS | CHANNEL_PROPERTY);
    assert_eq!(stats.foreign_channel_reads, 0);
}

#[test]
fn mmio_exchange_rejects_a_foreign_property_completion() {
    let mut regs = ready_regs();
    set_reg_word(
        &mut regs,
        REG_MBOX0_READ,
        0x0002_0000 | CHANNEL_PROPERTY, // someone else's buffer
    );
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut regs.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    assert_eq!(
        mailbox.exchange(&mut message),
        Err(MailboxError::MalformedResponse)
    );
}

// --- Display-size query ----------------------------------------------------

/// A healthy mock response to [`encode_display_size_query`].
fn size_response() -> [u32; PROPERTY_WORDS] {
    let mut words = encode_display_size_query();
    MockFirmware::healthy().respond(&mut words);
    words
}

#[test]
fn size_query_lays_out_header_tag_and_end_marker() {
    let words = encode_display_size_query();
    // Header: 8 used words (2 header + 5 tag + 1 end), request code.
    assert_eq!(words[0], 8 * 4, "message byte length");
    assert_eq!(words[1], CODE_REQUEST);
    assert_eq!(words[2..7], [TAG_GET_PHYSICAL_WH, 8, 0, 0, 0]);
    assert_eq!(words[7], 0, "end tag");
}

#[test]
fn size_query_round_trips_through_a_healthy_firmware() {
    let mut firmware = MockFirmware::healthy();
    let size = query_display_size(&mut firmware).expect("query");
    assert_eq!((size.width_px, size.height_px), (1920, 1080));
    assert!(size.is_attached());
}

#[test]
fn size_decode_treats_zero_by_zero_as_detached() {
    let mut words = size_response();
    (words[5], words[6]) = (0, 0);
    let size = decode_display_size_response(&words).expect("decode");
    assert!(!size.is_attached());
}

#[test]
fn size_decode_rejects_protocol_breaches() {
    let mut err = size_response();
    err[1] = CODE_RESPONSE_ERROR;
    assert_eq!(
        decode_display_size_response(&err),
        Err(MailboxError::FirmwareError)
    );
    let mut unknown = size_response();
    unknown[1] = 0x1234_5678;
    assert_eq!(
        decode_display_size_response(&unknown),
        Err(MailboxError::MalformedResponse)
    );
    let mut no_bit = size_response();
    no_bit[4] &= !TAG_RESPONSE_BIT;
    assert_eq!(
        decode_display_size_response(&no_bit),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn size_decode_rejects_implausible_geometry() {
    // A dimension past the validation bound.
    let mut huge = size_response();
    huge[5] = MAX_DISPLAY_DIM + 1;
    assert_eq!(
        decode_display_size_response(&huge),
        Err(MailboxError::BadGeometry)
    );
    // Exactly one zero dimension: neither attached nor detached.
    let mut half = size_response();
    half[6] = 0;
    assert_eq!(
        decode_display_size_response(&half),
        Err(MailboxError::BadGeometry)
    );
    // The bound itself is accepted.
    let mut max = size_response();
    (max[5], max[6]) = (MAX_DISPLAY_DIM, MAX_DISPLAY_DIM);
    assert!(decode_display_size_response(&max)
        .expect("decode")
        .is_attached());
}

// --- VL805 xHCI firmware reload ------------------------------------------

/// The VL805's hardwired PCI device address on the Pi 4 (bus 1, slot 0,
/// func 0), as the firmware expects it.
const TEST_VL805_DEV_ADDR: u32 = 0x10_0000;

#[test]
fn xhci_reset_lays_out_the_dev_addr_tag() {
    let words = encode_xhci_reset(TEST_VL805_DEV_ADDR);
    // 7 used words: 2 header + a 4-word tag ([tag, value-len, request,
    // value]) + 1 end marker.
    assert_eq!(words[0], 7 * 4, "message byte length");
    assert_eq!(words[1], CODE_REQUEST);
    assert_eq!(
        words[2..6],
        [TAG_NOTIFY_XHCI_RESET, 4, 0, TEST_VL805_DEV_ADDR]
    );
    assert_eq!(words[6], 0, "end tag");
}

#[test]
fn xhci_reset_round_trips_through_a_healthy_firmware() {
    // A healthy firmware echoes the set-tag and stamps the OK header;
    // the notify call accepts it and surfaces the firmware's response
    // value word (the echoed `dev_addr`), which the metal bring-up logs
    // to confirm the firmware processed the request (`AGENTS.md` §15.7).
    let mut firmware = MockFirmware::healthy();
    assert_eq!(
        notify_xhci_reset(&mut firmware, TEST_VL805_DEV_ADDR).expect("reset accepted"),
        TEST_VL805_DEV_ADDR
    );
}

#[test]
fn xhci_reset_decode_fails_closed_on_a_bad_header() {
    // A genuine healthy response: OK header *and* the tag's response bit
    // stamped (the mock answers exactly as the firmware does).
    let mut ok = encode_xhci_reset(TEST_VL805_DEV_ADDR);
    MockFirmware::healthy().respond(&mut ok);
    // The honoured tag's response value word (the echoed `dev_addr`) is
    // surfaced for the bring-up diagnostic.
    assert_eq!(decode_xhci_reset_response(&ok), Ok(TEST_VL805_DEV_ADDR));

    let mut err = encode_xhci_reset(TEST_VL805_DEV_ADDR);
    err[1] = CODE_RESPONSE_ERROR;
    assert_eq!(
        decode_xhci_reset_response(&err),
        Err(MailboxError::FirmwareError)
    );

    let mut unknown = encode_xhci_reset(TEST_VL805_DEV_ADDR);
    unknown[1] = 0x1234_5678;
    assert_eq!(
        decode_xhci_reset_response(&unknown),
        Err(MailboxError::MalformedResponse)
    );
}

#[test]
fn xhci_reset_decode_rejects_an_unhonoured_tag() {
    // The wedge this guards: a firmware build that does not act on the
    // tag still stamps the OK *header* but leaves the tag's own response
    // code clear (no response bit). An OK header alone must NOT be read
    // as a successful reload — the decode requires the per-tag response
    // bit and fails closed otherwise (`AGENTS.md` §5.4), so the metal
    // bring-up reports `Failed` rather than a false `Reloaded`.
    let mut unhonoured = encode_xhci_reset(TEST_VL805_DEV_ADDR);
    unhonoured[1] = CODE_RESPONSE_OK; // header OK, tag code word still 0.
    assert_eq!(
        decode_xhci_reset_response(&unhonoured),
        Err(MailboxError::MalformedResponse)
    );
}

// --- Mailbox liveness probe ----------------------------------------------

#[test]
fn firmware_revision_query_lays_out_the_get_tag() {
    let words = encode_firmware_revision_query();
    // 7 used words: 2 header + a 4-word get tag ([tag, value-len,
    // request, response-word slot]) + 1 end marker.
    assert_eq!(words[0], 7 * 4, "message byte length");
    assert_eq!(words[1], CODE_REQUEST);
    // tag, response-buffer byte length (one word), request code, and the
    // zeroed slot the firmware writes the revision into.
    assert_eq!(words[2..6], [TAG_GET_FIRMWARE_REVISION, 4, 0, 0]);
    assert_eq!(words[6], 0, "end tag");
}

#[test]
fn firmware_revision_round_trips_through_a_healthy_firmware() {
    // The liveness probe reads the firmware's configured revision word
    // over the transport; a non-zero value proves the runtime mailbox
    // path is sound before the heavier xHCI-reset call (`AGENTS.md`
    // §15.7).
    let mut firmware = MockFirmware::healthy();
    assert_eq!(
        query_firmware_revision(&mut firmware).expect("revision read"),
        firmware.firmware_revision
    );
}

#[test]
fn firmware_revision_decode_fails_closed() {
    // A genuine healthy response decodes to the revision word.
    let mut ok = encode_firmware_revision_query();
    MockFirmware::healthy().respond(&mut ok);
    assert_eq!(
        decode_firmware_revision_response(&ok),
        Ok(MockFirmware::healthy().firmware_revision)
    );

    // Firmware top-level error.
    let mut err = encode_firmware_revision_query();
    err[1] = CODE_RESPONSE_ERROR;
    assert_eq!(
        decode_firmware_revision_response(&err),
        Err(MailboxError::FirmwareError)
    );

    // Unknown header code is a protocol breach, not a verdict.
    let mut unknown = encode_firmware_revision_query();
    unknown[1] = 0x1234_5678;
    assert_eq!(
        decode_firmware_revision_response(&unknown),
        Err(MailboxError::MalformedResponse)
    );

    // An OK header with the per-tag response bit clear (an unhonoured
    // tag) must not be read as a successful probe.
    let mut unhonoured = encode_firmware_revision_query();
    unhonoured[1] = CODE_RESPONSE_OK;
    assert_eq!(
        decode_firmware_revision_response(&unhonoured),
        Err(MailboxError::MalformedResponse)
    );
}

// --- Property-buffer cache-coherency seam --------------------------------

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `1` after `coh_flush` runs, `2` after `coh_invalidate` runs — but
/// only if flush ran first, so a final value of `2` proves the
/// clean-before / invalidate-after ordering `exchange` must keep.
static COH_ORDER: AtomicU32 = AtomicU32::new(0);
/// The CPU base each hook was handed (must be the buffer's `phys_base`).
static COH_FLUSH_BASE: AtomicU64 = AtomicU64::new(0);
static COH_INVALIDATE_BASE: AtomicU64 = AtomicU64::new(0);
/// The byte length each hook was handed (must be one property message).
static COH_FLUSH_LEN: AtomicU32 = AtomicU32::new(0);

fn coh_flush(base: u64, len: usize) {
    COH_FLUSH_BASE.store(base, Ordering::SeqCst);
    COH_FLUSH_LEN.store(
        u32::try_from(len).expect("property length fits u32"),
        Ordering::SeqCst,
    );
    let _ = COH_ORDER.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
}

fn coh_invalidate(base: u64, _len: usize) {
    COH_INVALIDATE_BASE.store(base, Ordering::SeqCst);
    // Reaches `2` only if `coh_flush` already moved it to `1`.
    let _ = COH_ORDER.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst);
}

#[test]
fn mmio_exchange_runs_the_coherency_hooks_clean_then_invalidate() {
    // A unique buffer phys so the hooks' base argument is checkable.
    const BUFFER_PHYS: u64 = 0x3_4B08_0000;
    let mut regs = ready_regs();
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    {
        let mut mailbox = MmioMailbox::with_coherency(
            window_over(&mut regs.0, 0),
            window_over(&mut buffer.0, BUFFER_PHYS),
            TEST_BUFFER_BUS,
            8,
            BufferCoherency::new(coh_flush, coh_invalidate),
        )
        .expect("construct");
        let mut message = request().encode().expect("encode");
        mailbox.exchange(&mut message).expect("exchange");
    }
    // Flush ran (after staging), then invalidate ran (before read-back).
    assert_eq!(COH_ORDER.load(Ordering::SeqCst), 2, "clean-then-invalidate");
    assert_eq!(COH_FLUSH_BASE.load(Ordering::SeqCst), BUFFER_PHYS);
    assert_eq!(COH_INVALIDATE_BASE.load(Ordering::SeqCst), BUFFER_PHYS);
    assert_eq!(
        COH_FLUSH_LEN.load(Ordering::SeqCst),
        u32::try_from(PROPERTY_LEN_BYTES).expect("property length fits u32")
    );
}

#[test]
fn mmio_new_defaults_to_no_coherency_maintenance() {
    // The default constructor installs no-op hooks: a round trip over an
    // already-coherent (caches-off) buffer still succeeds.
    let mut regs = ready_regs();
    let mut buffer = Aligned([0u8; PROPERTY_LEN_BYTES]);
    let mut mailbox = MmioMailbox::new(
        window_over(&mut regs.0, 0),
        window_over(&mut buffer.0, 0),
        TEST_BUFFER_BUS,
        8,
    )
    .expect("construct");
    let mut message = request().encode().expect("encode");
    mailbox.exchange(&mut message).expect("exchange");
}

#[test]
fn mailbox_errors_map_to_driver_errors() {
    assert_eq!(
        MailboxError::Window.as_driver_error(),
        DriverError::OutOfRange
    );
    assert_eq!(
        MailboxError::Timeout.as_driver_error(),
        DriverError::DeviceFault
    );
    assert_eq!(
        MailboxError::FirmwareError.as_driver_error(),
        DriverError::DeviceFault
    );
    assert_eq!(
        MailboxError::MalformedResponse.as_driver_error(),
        DriverError::BadMagic
    );
    assert_eq!(
        MailboxError::BadAperture.as_driver_error(),
        DriverError::LengthOutOfRange
    );
    assert_eq!(
        MailboxError::BadGeometry.as_driver_error(),
        DriverError::LengthOutOfRange
    );
}
