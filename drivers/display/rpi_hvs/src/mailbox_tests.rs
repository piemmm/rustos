//! Host unit tests for the mailbox property-channel client: framing,
//! response validation, bus↔physical translation, and the MMIO
//! doorbell transport — all against in-process mocks (the firmware is
//! not emulable, so its protocol semantics are modelled here and the
//! doorbell against a RAM-backed register window).

use super::*;
use rustos_abi::driver::display::DisplayMode;

/// Geometry every test requests: 640×480 BGRA.
fn request() -> FramebufferRequest {
    FramebufferRequest {
        width_px: 640,
        height_px: 480,
        format: DisplayFormat::Bgra8888,
    }
}

/// Bus address the mock firmware allocates the framebuffer at
/// (`0x1000_0000` physical under the `0xC000_0000` L2-cached alias).
const MOCK_FB_BUS: u32 = 0xD000_0000;
/// Size the mock firmware reports (pitch 2560 × 480).
const MOCK_FB_SIZE: u32 = 2560 * 480;
/// Pitch the mock firmware reports.
const MOCK_FB_PITCH: u32 = 2560;

/// A protocol-faithful mock firmware: walks the request tags, echoes
/// the geometry, fills the allocate/pitch responses, and stamps the
/// response codes — exactly what a healthy firmware does.
struct MockFirmware;

impl MockFirmware {
    fn respond(message: &mut [u32; PROPERTY_WORDS]) {
        let mut at = 2;
        while at + 3 <= PROPERTY_WORDS {
            let tag = message[at];
            if tag == 0 {
                break;
            }
            let buf_words = (message[at + 1] / 4) as usize;
            let resp_len = match tag {
                TAG_ALLOCATE => {
                    message[at + 3] = MOCK_FB_BUS;
                    message[at + 4] = MOCK_FB_SIZE;
                    8
                }
                TAG_GET_PITCH => {
                    message[at + 3] = MOCK_FB_PITCH;
                    4
                }
                // Set-tags echo their request values unchanged.
                _ => message[at + 1],
            };
            message[at + 2] = TAG_RESPONSE_BIT | resp_len;
            at += 3 + buf_words;
        }
        message[1] = CODE_RESPONSE_OK;
    }
}

impl MailboxTransport for MockFirmware {
    fn exchange(&mut self, message: &mut [u32; PROPERTY_WORDS]) -> Result<(), MailboxError> {
        Self::respond(message);
        Ok(())
    }
}

/// A healthy mock response to [`request`].
fn ok_response() -> [u32; PROPERTY_WORDS] {
    let mut words = request().encode().expect("encode");
    MockFirmware::respond(&mut words);
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
    let mut firmware = MockFirmware;
    let fb = discover_framebuffer(&mut firmware, &request()).expect("discover");
    assert_eq!(fb.bus_addr, MOCK_FB_BUS);
    assert_eq!(fb.size_bytes, MOCK_FB_SIZE);
    assert_eq!(fb.pitch_bytes, MOCK_FB_PITCH);
    assert_eq!((fb.width_px, fb.height_px), (640, 480));
    assert_eq!(fb.format, DisplayFormat::Bgra8888);
    assert_eq!(fb.bus_alias(), 0xC000_0000);
    assert_eq!(fb.arm_physical_base().expect("translate"), 0x1000_0000);

    let scanout = fb.scanout_config().expect("scanout");
    assert_eq!(scanout.phys_base, 0x1000_0000);
    assert_eq!(scanout.stride_bytes, MOCK_FB_PITCH);
    assert_eq!((scanout.width_px, scanout.height_px), (640, 480));
    assert_eq!(scanout.format, DisplayFormat::Bgra8888);
    assert_eq!(
        scanout.mode(),
        DisplayMode {
            width_px: 640,
            height_px: 480,
            stride_bytes: MOCK_FB_PITCH,
            format: DisplayFormat::Bgra8888,
        }
    );
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
    small[24] = MOCK_FB_PITCH * 480 - 1; // buffer smaller than the surface
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
fn mmio_exchange_times_out_when_the_firmware_never_answers() {
    // Write side jammed: MBOX1 reports FULL forever.
    let mut full = ready_regs();
    set_reg_word(&mut full, REG_MBOX1_STATUS, STATUS_FULL);
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

// --- Full chain: discovery feeds the driver -------------------------------

/// The P7 emulation artefact: a protocol-faithful mock firmware answers
/// the property exchange, the decoded response becomes the
/// [`ScanoutConfig`], and [`RpiHvs::open`] consumes it and presents a
/// frame into the discovered surface. The real scan-out (HVS hardware,
/// HDMI) is a metal acceptance item (`plans/PI.md` P7): QEMU's `virt`
/// RAM begins at `0x4000_0000`, outside the BCM2711 30-bit `VideoCore`
/// aperture, so no honest `virt` vertical can carry this chain.
#[test]
fn discovered_config_opens_the_hvs_driver() {
    extern crate alloc;
    use alloc::vec;

    use crate::tests::{MockHost, MockMapper};
    use crate::{HvsConfig, PlaneConfig, RpiHvs, MAX_PLANES};
    use rustos_abi::driver::display::Display;

    const DLIST_PHYS: u64 = 0x1100_0000;
    const CONTROL_PHYS: u64 = 0x1200_0000;
    const PLANE_PHYS: u64 = 0x2000_0000;

    let mut firmware = MockFirmware;
    let fb = discover_framebuffer(&mut firmware, &request()).expect("discover");
    let scanout = fb.scanout_config().expect("scanout");
    let surface_len = scanout.surface_len().expect("surface length");

    let mut mapper = MockMapper::new(true);
    mapper.add(scanout.phys_base, surface_len / 4);
    mapper.add(DLIST_PHYS, 64);
    mapper.add(CONTROL_PHYS, 2);
    mapper.add(PLANE_PHYS, 8);
    let host = MockHost {
        drv_load: true,
        mmio_map: true,
        mapper: Some(mapper),
    };

    let mut planes = [PlaneConfig {
        phys_base: 0,
        len_bytes: 0,
    }; MAX_PLANES];
    planes[0] = PlaneConfig {
        phys_base: PLANE_PHYS,
        len_bytes: 32,
    };
    let config = HvsConfig {
        scanout,
        dlist_phys_base: DLIST_PHYS,
        dlist_len_bytes: 256,
        control_phys_base: CONTROL_PHYS,
        planes,
        plane_count: 1,
        bus_alias: fb.bus_alias(),
    };

    let mut hvs = RpiHvs::open(&host, config).expect("open on discovered config");
    assert_eq!(hvs.mode_info().expect("mode"), scanout.mode());

    let frame = vec![0xA5u8; surface_len];
    hvs.present(&frame).expect("present");
    let mapper = host.mapper.as_ref().expect("mapper");
    for off in [0, 1, surface_len / 2, surface_len - 1] {
        assert_eq!(mapper.byte(scanout.phys_base, off), 0xA5, "byte {off}");
    }
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
