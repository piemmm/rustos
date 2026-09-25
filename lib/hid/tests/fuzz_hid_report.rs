//! Deterministic fuzz harness for the `lib/hid` decoders of device-written
//! bytes: the Report Descriptor parser, the report-protocol normaliser, and
//! the boot-protocol keyboard and mouse decoders every report passes through.
//!
//! Each is held to a naive model of what it must do:
//!
//! * no input panics, and a descriptor declaring `2^32 - 1` fields parses in
//!   the time its bytes take to walk;
//! * an accepted map locates only fields the boot layout can read — a
//!   one-bit-per-flag button or modifier bitmap, values 1..=32 bits wide,
//!   never an empty field — each past the Report ID byte when the device uses
//!   one, none overlapping another;
//! * a map normalises the report it describes, and every normalised report
//!   holds exactly what a naive bit reader takes from the located fields;
//! * a long item, or global items a Push and Pop enclose, change nothing a
//!   descriptor says;
//! * the boot decoders report exactly the held-state changes each report
//!   makes: never a key or button pressed twice or released unheld.
//!
//! A per-run-seeded `Prng` mutates real descriptors (the boot examples, the
//! Report-ID keyboard and mouse, a wireless receiver's two interfaces, and the
//! forged shapes the parser must refuse), assembles descriptors from random
//! items, and feeds pure noise. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz` exports `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock
//! budget.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use tairix_abi::driver::input::{Input, InputEvent, InputEventKind};
use tairix_abi::input::KeyInput;
use tairix_abi::DriverError;
use tairix_fuzzseed::Prng;
use tairix_hid::keyboard::MODIFIER_USAGE_BASE;
use tairix_hid::{
    parse_report_descriptor, BootKeyboard, BootMouse, HidReportMap, KeyboardConsole,
    ReportFieldSummary, ReportMapSummary, ReportSource, AXIS_X, AXIS_Y, BOOT_KEYBOARD_NORM_LEN,
    BOOT_MOUSE_NORM_LEN, MAX_REPORT_DESCRIPTOR, POINTER_BUTTON_CODE_BASE, REPORT_BUF_LEN,
};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// A boot mouse (USB HID 1.11 Appendix E.10).
const BOOT_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
    0xC0, 0xC0,
];

/// A boot mouse with a wheel.
const WHEEL_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xC0, 0xC0,
];

/// A boot keyboard (USB HID 1.11 Appendix E.6), with its LED output report.
const BOOT_KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

/// A keyboard whose reports carry Report ID 1.
const REPORT_ID_KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xC0,
];

/// A mouse whose reports carry Report ID 2.
const REPORT_ID_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
    0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02,
    0x81, 0x06, 0xC0, 0xC0,
];

/// A mouse with 12-bit axes packed across byte boundaries.
const TWELVE_BIT_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0xF8, 0x26, 0xFF, 0x07, 0x75, 0x0C, 0x95, 0x02,
    0x81, 0x06, 0xC0, 0xC0,
];

/// A wireless receiver's keyboard interface: the keyboard (Report ID 1), a
/// consumer-control collection (3), and system control (4).
const RECEIVER_KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x95, 0x08, 0x75, 0x01, 0x15, 0x00, 0x25, 0x01,
    0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x03, 0x95, 0x05,
    0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x03,
    0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x05, 0x07, 0x19, 0x00, 0x2A, 0xFF, 0x00,
    0x81, 0x00, 0xC0, 0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x03, 0x75, 0x10, 0x95, 0x02, 0x15,
    0x01, 0x26, 0xFF, 0x02, 0x19, 0x01, 0x2A, 0xFF, 0x02, 0x81, 0x00, 0xC0, 0x05, 0x01, 0x09, 0x80,
    0xA1, 0x01, 0x85, 0x04, 0x75, 0x02, 0x95, 0x01, 0x15, 0x01, 0x25, 0x03, 0x09, 0x82, 0x09, 0x81,
    0x09, 0x83, 0x81, 0x60, 0x75, 0x06, 0x81, 0x03, 0xC0,
];

/// The receiver's mouse interface: sixteen buttons, 12-bit axes, a wheel and
/// AC Pan (Report ID 2), then two vendor reports (0x10, 0x11).
const RECEIVER_MOUSE: &[u8] = &[
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81, 0x02, 0x05, 0x01, 0x16, 0x01,
    0xF8, 0x26, 0xFF, 0x07, 0x75, 0x0C, 0x95, 0x02, 0x09, 0x30, 0x09, 0x31, 0x81, 0x06, 0x15, 0x81,
    0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, 0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95,
    0x01, 0x81, 0x06, 0xC0, 0xC0, 0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x10, 0x75, 0x08,
    0x95, 0x06, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x09, 0x01, 0x81, 0x00, 0x09, 0x01, 0x91, 0x00, 0xC0,
    0x06, 0x00, 0xFF, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x11, 0x75, 0x08, 0x95, 0x13, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x09, 0x02, 0x81, 0x00, 0x09, 0x02, 0x91, 0x00, 0xC0,
];

/// Forged shapes the parser once got wrong: pointer fields placed before
/// the first Report ID; buttons and X in report 1 with report 2's Y; report
/// 1 re-entered after report 2; a Report ID saved by Push; axes of
/// `2^32 - 1` fields; four modifier flags before padding.
const FORGED: [&[u8]; 6] = [
    &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25,
        0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x03, 0x05, 0x01,
        0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x95, 0x02, 0x75, 0x08, 0x81, 0x06, 0xC0,
        0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x02, 0x19, 0x00, 0x2A, 0xFF, 0x00, 0x15, 0x00,
        0x26, 0xFF, 0x00, 0x95, 0x01, 0x75, 0x08, 0x81, 0x00, 0xC0,
    ],
    &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x95,
        0x08, 0x75, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x30, 0x95, 0x01, 0x75, 0x08, 0x81, 0x06,
        0xC0, 0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x05, 0x01, 0x09, 0x31, 0x95, 0x01,
        0x75, 0x08, 0x81, 0x06, 0xC0,
    ],
    &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x95,
        0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x05, 0x81, 0x01, 0x85, 0x02, 0x95, 0x01, 0x75, 0x08,
        0x81, 0x01, 0x85, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x95, 0x02, 0x75, 0x08, 0x81,
        0x06, 0xC0,
    ],
    &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x95,
        0x08, 0x75, 0x01, 0x81, 0x02, 0xA4, 0x85, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0xB4,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x95, 0x02, 0x75, 0x08, 0x81, 0x06, 0xC0,
    ],
    &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x05, 0x01, 0x19, 0x30, 0x29, 0x38, 0x75, 0x08, 0x97,
        0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x06, 0xC0,
    ],
    &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x75, 0x01, 0x95,
        0x04, 0x81, 0x02, 0x95, 0x04, 0x81, 0x03, 0x95, 0x06, 0x75, 0x08, 0x19, 0x00, 0x29, 0x65,
        0x81, 0x00, 0xC0,
    ],
];

/// The descriptors the harness mutates.
fn descriptor_seeds() -> Vec<&'static [u8]> {
    let mut seeds = vec![
        BOOT_MOUSE,
        WHEEL_MOUSE,
        BOOT_KEYBOARD,
        REPORT_ID_KEYBOARD,
        REPORT_ID_MOUSE,
        TWELVE_BIT_MOUSE,
        RECEIVER_KEYBOARD,
        RECEIVER_MOUSE,
    ];
    seeds.extend(FORGED);
    seeds
}

/// Item types (`bType`).
const MAIN: u8 = 0;
const GLOBAL: u8 = 1;
const LOCAL: u8 = 2;

/// Data bytes a short item's two-bit size field selects.
const DATA_LEN: [usize; 4] = [0, 1, 2, 4];

/// A short item of `tag` and `kind` carrying `data` in the fewest bytes
/// that hold it, or in all four when `wide`.
fn item(tag: u8, kind: u8, data: u32, wide: bool) -> Vec<u8> {
    let size: u8 = if wide || data > 0xFFFF {
        3
    } else if data > 0xFF {
        2
    } else {
        u8::from(data != 0)
    };
    let mut item = vec![(tag << 4) | (kind << 2) | size];
    item.extend_from_slice(&data.to_le_bytes()[..DATA_LEN[usize::from(size)]]);
    item
}

/// A long item (prefix `0xFE`): its data size, its tag, then the data.
fn long_item(rng: &mut Prng) -> Vec<u8> {
    let mut data = vec![0u8; rng.at_most(6)];
    rng.fill(&mut data);
    let mut item = vec![0xFE, u8::try_from(data.len()).unwrap_or(0), rng.next_u8()];
    item.extend_from_slice(&data);
    item
}

/// One of `values`, or now and then any value at all.
fn value(values: &[u32], rng: &mut Prng) -> u32 {
    if rng.below(6) == 0 {
        rng.next_u32()
    } else {
        *rng.pick(values)
    }
}

/// One whole item of a descriptor, drawn from what the parser interprets
/// and what it must pass over.
fn random_item(rng: &mut Prng) -> Vec<u8> {
    let wide = rng.below(8) == 0;
    match rng.below(16) {
        0 => item(
            0x0,
            GLOBAL,
            value(&[0x01, 0x07, 0x09, 0x0C, 0xFF00], rng),
            wide,
        ),
        1 => item(
            0x7,
            GLOBAL,
            value(&[0, 1, 3, 5, 8, 12, 16, 32, 33, 64], rng),
            wide,
        ),
        2 => item(
            0x9,
            GLOBAL,
            value(&[0, 1, 2, 3, 6, 8, 16, 40, 255, 256, 0xFFFF_FFFF], rng),
            wide,
        ),
        3 => item(0x8, GLOBAL, value(&[0, 1, 2, 3, 0x10], rng), wide),
        4 => item(
            0x0,
            LOCAL,
            value(&[0x01, 0x02, 0x06, 0x30, 0x31, 0x38, 0xE0, 0xE7], rng),
            wide,
        ),
        5 => item(
            0x1,
            LOCAL,
            value(&[0x00, 0x01, 0x2E, 0x30, 0xE0], rng),
            wide,
        ),
        6 => item(
            0x2,
            LOCAL,
            value(&[0x03, 0x08, 0x10, 0x31, 0x38, 0x65, 0xE7, 0xFF], rng),
            wide,
        ),
        7..=9 => item(0x8, MAIN, value(&[0x00, 0x01, 0x02, 0x03, 0x06], rng), wide),
        10 => item(0xA, MAIN, value(&[0x00, 0x01, 0x02], rng), wide),
        11 => item(0xC, MAIN, 0, false),
        12 => item(*rng.pick(&[0x9, 0xB]), MAIN, 0x02, wide),
        13 => item(*rng.pick(&[0xA, 0xB]), GLOBAL, 0, false),
        14 => long_item(rng),
        _ => {
            // Any short item at all, reserved tags and types included.
            let prefix = match rng.next_u8() {
                0xFE => 0xFF,
                prefix => prefix,
            };
            let mut item = vec![prefix; 1 + DATA_LEN[usize::from(prefix & 0x03)]];
            rng.fill(&mut item[1..]);
            item
        }
    }
}

/// A descriptor of random items, kept as its items so an item boundary is
/// known.
fn random_items(rng: &mut Prng) -> Vec<Vec<u8>> {
    (0..rng.at_most(48)).map(|_| random_item(rng)).collect()
}

/// Every item of `desc` as its type and tag, a long item as the reserved
/// type, up to any truncated item.
fn item_kinds(desc: &[u8]) -> Vec<(u8, u8)> {
    let mut kinds = Vec::new();
    let mut at = 0;
    while let Some(&prefix) = desc.get(at) {
        if prefix == 0xFE {
            let Some(&len) = desc.get(at + 1) else {
                break;
            };
            kinds.push((3, 0xF));
            at += 3 + usize::from(len);
            continue;
        }
        kinds.push(((prefix >> 2) & 0x03, prefix >> 4));
        at += 1 + DATA_LEN[usize::from(prefix & 0x03)];
    }
    kinds
}

/// A `Push`, random global items — a Report ID among them — and the `Pop`
/// that restores what they changed.
fn shielded_globals(rng: &mut Prng) -> Vec<Vec<u8>> {
    let mut items = vec![item(0xA, GLOBAL, 0, false)];
    items.push(item(0x8, GLOBAL, u32::from(rng.next_u8()), false));
    for _ in 0..rng.at_most(3) {
        let tag = *rng.pick(&[0x0, 0x7, 0x9, 0x8]);
        items.push(item(tag, GLOBAL, u32::from(rng.next_u16()), false));
    }
    items.push(item(0xB, GLOBAL, 0, false));
    items
}

/// `template` with a handful of bytes flipped, then cut short or extended.
fn mutate(template: &[u8], rng: &mut Prng) -> Vec<u8> {
    let mut bytes = template.to_vec();
    for _ in 0..rng.at_most(6) {
        if bytes.is_empty() {
            break;
        }
        let at = rng.below(bytes.len());
        bytes[at] ^= rng.next_u8();
    }
    match rng.below(3) {
        0 => bytes.truncate(rng.at_most(bytes.len())),
        1 => bytes.extend((0..rng.at_most(12)).map(|_| rng.next_u8())),
        _ => {}
    }
    bytes
}

/// A field the parser located, as the bit range it covers.
struct Located {
    start: u32,
    end: u32,
}

impl Located {
    fn of(field: ReportFieldSummary) -> Self {
        let start = u32::from(field.offset_bits);
        Self {
            start,
            end: start + u32::from(field.size_bits) * u32::from(field.count),
        }
    }
}

/// The summary a mouse map gives when it located no button field.
const NO_BUTTONS: ReportFieldSummary = ReportFieldSummary {
    offset_bits: 0,
    size_bits: 0,
    count: 0,
};

fn is_bitmap(field: ReportFieldSummary) -> bool {
    field.size_bits == 1 && field.count >= 1
}

fn is_values(field: ReportFieldSummary) -> bool {
    (1..=32).contains(&field.size_bits) && field.count >= 1
}

/// Every field of an accepted map is readable, past its Report ID, and
/// clear of every other; the map normalises the report it describes.
fn check_map(map: &HidReportMap) {
    let (report_id, fields) = match map.summary() {
        ReportMapSummary::Mouse {
            report_id,
            buttons,
            x,
            y,
            wheel,
        } => {
            assert!(
                buttons == NO_BUTTONS || is_bitmap(buttons),
                "buttons {buttons:?}"
            );
            for axis in [Some(x), Some(y), wheel].into_iter().flatten() {
                assert!(is_values(axis) && axis.count == 1, "axis {axis:?}");
            }
            let buttons = (buttons != NO_BUTTONS).then_some(buttons);
            (
                report_id,
                [buttons, Some(x), Some(y), wheel]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>(),
            )
        }
        ReportMapSummary::Keyboard {
            report_id,
            modifiers,
            keys,
        } => {
            assert!(is_bitmap(modifiers), "modifiers {modifiers:?}");
            assert!(is_values(keys), "keys {keys:?}");
            (report_id, vec![modifiers, keys])
        }
    };
    let mut ranges: Vec<Located> = fields.into_iter().map(Located::of).collect();
    if report_id.is_some() {
        assert!(
            ranges.iter().all(|field| field.start >= 8),
            "a field over the Report ID byte in {map:?}"
        );
    }
    ranges.sort_by_key(|field| field.start);
    for pair in ranges.windows(2) {
        assert!(
            pair[0].end <= pair[1].start,
            "overlapping fields in {map:?}"
        );
    }
    let end = ranges.iter().map(|field| field.end).max().unwrap_or(0);
    let mut report = vec![0u8; usize::try_from(end.div_ceil(8)).unwrap_or(0)];
    if let (Some(id), Some(first)) = (report_id, report.first_mut()) {
        *first = id;
    }
    let mut out = [0u8; 8];
    let expected = match map {
        HidReportMap::Mouse(_) => BOOT_MOUSE_NORM_LEN,
        HidReportMap::Keyboard(_) => BOOT_KEYBOARD_NORM_LEN,
    };
    assert_eq!(
        map.normalize(&report, &mut out),
        Some(expected),
        "{map:?} does not normalise the report it describes"
    );
}

/// `width` bits of `raw` from bit `offset`, least significant first: `None`
/// past its end.
fn bits(raw: &[u8], offset: u32, width: u32) -> Option<u32> {
    let mut value = 0;
    for bit in 0..width {
        let at = offset + bit;
        let byte = *raw.get(usize::try_from(at / 8).ok()?)?;
        value |= u32::from((byte >> (at % 8)) & 1) << bit;
    }
    Some(value)
}

/// `value`, `width` bits wide, as a two's-complement displacement clamped to
/// the boot report's signed byte.
fn displacement(value: u32, width: u32) -> u8 {
    let signed = if (value >> (width - 1)) & 1 == 1 {
        i64::from(value) - (1i64 << width)
    } else {
        i64::from(value)
    };
    i8::try_from(signed.clamp(-128, 127)).map_or(0, |byte| byte.to_le_bytes()[0])
}

/// The low byte of `value`.
fn low_byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// The boot report a naive reader takes from `raw` through `map`'s fields.
fn expected_normalization(map: &HidReportMap, raw: &[u8]) -> Option<Vec<u8>> {
    let field =
        |field: ReportFieldSummary, width: u32| bits(raw, u32::from(field.offset_bits), width);
    let prefixed = |report_id: Option<u8>| report_id.is_none_or(|id| raw.first() == Some(&id));
    match map.summary() {
        ReportMapSummary::Mouse {
            report_id,
            buttons,
            x,
            y,
            wheel,
        } => {
            if !prefixed(report_id) {
                return None;
            }
            let buttons = if buttons == NO_BUTTONS {
                0
            } else {
                field(buttons, u32::from(buttons.count).min(8))?
            };
            let axis = |axis: ReportFieldSummary| {
                let width = u32::from(axis.size_bits);
                field(axis, width).map(|value| displacement(value, width))
            };
            let wheel = match wheel {
                Some(wheel) => axis(wheel)?,
                None => 0,
            };
            Some(vec![low_byte(buttons), axis(x)?, axis(y)?, wheel])
        }
        ReportMapSummary::Keyboard {
            report_id,
            modifiers,
            keys,
        } => {
            if !prefixed(report_id) {
                return None;
            }
            let mut out = vec![0u8; BOOT_KEYBOARD_NORM_LEN];
            out[0] = low_byte(field(modifiers, u32::from(modifiers.count).min(8))?);
            for slot in 0..u32::from(keys.count).min(6) {
                let offset = u32::from(keys.offset_bits) + slot * u32::from(keys.size_bits);
                let Some(usage) = bits(raw, offset, u32::from(keys.size_bits)) else {
                    break;
                };
                out[2 + usize::try_from(slot).unwrap_or(0)] = low_byte(usage);
            }
            Some(out)
        }
    }
}

/// `map` normalises `raw` into exactly what the naive reader takes, and
/// refuses an output buffer too small for the boot report.
fn check_normalization(map: &HidReportMap, raw: &[u8]) {
    let mut out = [0u8; 8];
    let normalized = map.normalize(raw, &mut out).map(|len| out[..len].to_vec());
    assert_eq!(
        normalized,
        expected_normalization(map, raw),
        "{map:?} normalising {raw:02x?}"
    );
    assert_eq!(
        map.normalize(raw, &mut [0u8; BOOT_MOUSE_NORM_LEN - 1]),
        None,
        "an output buffer short of the boot report"
    );
}

/// Parse `desc` and hold whatever it yields to the model.
fn exercise_descriptor(desc: &[u8], rng: &mut Prng) {
    let map = parse_report_descriptor(desc);
    if desc.is_empty() || desc.len() > MAX_REPORT_DESCRIPTOR {
        assert_eq!(map, None, "a descriptor of {} bytes parsed", desc.len());
    }
    let Some(map) = map else {
        return;
    };
    check_map(&map);
    let report_id = match map.summary() {
        ReportMapSummary::Mouse { report_id, .. }
        | ReportMapSummary::Keyboard { report_id, .. } => report_id,
    };
    for _ in 0..4 {
        let mut raw = vec![0u8; rng.at_most(24)];
        rng.fill(&mut raw);
        if let (Some(id), Some(first)) = (report_id, raw.first_mut()) {
            if rng.below(4) != 0 {
                *first = id;
            }
        }
        check_normalization(&map, &raw);
    }
}

#[test]
fn a_parsed_map_locates_only_readable_fields_and_normalizes_exactly() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "a_parsed_map_locates_only_readable_fields_and_normalizes_exactly",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let seeds = descriptor_seeds();
    for seed in &seeds[..8] {
        let map = parse_report_descriptor(seed).expect("a real descriptor maps");
        check_map(&map);
    }
    let mut iteration: u64 = 0;
    loop {
        let seed = *rng.pick(&seeds);
        exercise_descriptor(seed, &mut rng);
        exercise_descriptor(&mutate(seed, &mut rng), &mut rng);

        // Random items, and the same with a long item between two of them.
        let mut items = random_items(&mut rng);
        if rng.below(2) == 0 {
            let at = rng.at_most(items.len());
            items.insert(at, seed.to_vec());
        }
        let desc = items.concat();
        exercise_descriptor(&desc, &mut rng);
        let parsed = parse_report_descriptor(&desc);
        let mut with_long = items.clone();
        with_long.insert(rng.at_most(items.len()), long_item(&mut rng));
        let with_long = with_long.concat();
        if with_long.len() <= MAX_REPORT_DESCRIPTOR {
            assert_eq!(
                parse_report_descriptor(&with_long),
                parsed,
                "a long item changed what {desc:02x?} says"
            );
        }

        // Globals a Push and Pop enclose change nothing after them, once the
        // descriptor declares Report IDs anyway and keeps no stack of its own.
        let kinds = item_kinds(&desc);
        let declares_ids = kinds.contains(&(GLOBAL, 0x8));
        let stacks = kinds.contains(&(GLOBAL, 0xA)) || kinds.contains(&(GLOBAL, 0xB));
        if declares_ids && !stacks {
            let at = rng.at_most(items.len());
            let mut shielded = items.clone();
            shielded.splice(at..at, shielded_globals(&mut rng));
            let shielded = shielded.concat();
            if shielded.len() <= MAX_REPORT_DESCRIPTOR {
                assert_eq!(
                    parse_report_descriptor(&shielded),
                    parsed,
                    "a Push/Pop pair changed what {desc:02x?} says"
                );
            }
        }

        let mut noise = vec![0u8; rng.at_most(MAX_REPORT_DESCRIPTOR + 16)];
        rng.fill(&mut noise);
        exercise_descriptor(&noise, &mut rng);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

/// Reports in the order a device sends them, each with the length it claims
/// when that is not its own.
type Script = VecDeque<(Vec<u8>, Option<usize>)>;

/// A scripted interrupt-IN endpoint: its reports in order, each claiming its
/// own length unless a forged one is scripted.
#[derive(Clone, Default)]
struct Endpoint(Rc<RefCell<Script>>);

impl ReportSource for Endpoint {
    fn next_report(&mut self, buf: &mut [u8]) -> Result<Option<usize>, DriverError> {
        let Some((report, claimed)) = self.0.borrow_mut().pop_front() else {
            return Ok(None);
        };
        let len = report.len().min(buf.len());
        buf[..len].copy_from_slice(&report[..len]);
        Ok(Some(claimed.unwrap_or(len)))
    }
}

/// A report as a device may send it: keys, error usages, modifier usages,
/// and repeats drawn often, and a claimed length past the buffer now and then.
fn random_report(rng: &mut Prng) -> (Vec<u8>, Option<usize>) {
    let report: Vec<u8> = (0..rng.at_most(REPORT_BUF_LEN + 2))
        .map(|_| match rng.below(5) {
            0 => rng.next_u8(),
            1 => 0,
            2 => 0xE0 + rng.next_u8() % 8,
            _ => rng.next_u8() % 8,
        })
        .collect();
    let forged = (rng.below(32) == 0).then(|| REPORT_BUF_LEN + 1 + rng.below(8));
    (report, forged)
}

/// Drain `input` in random-sized polls until it has nothing more.
fn drain(input: &mut impl Input, rng: &mut Prng) -> Result<Vec<InputEvent>, DriverError> {
    let mut events = Vec::new();
    loop {
        let mut out = [InputEvent {
            kind: InputEventKind::Key,
            reserved0: 0,
            code: 0,
            value: 0,
        }; 8];
        let room = 1 + rng.below(out.len());
        let written = input.poll(&mut out[..room])?;
        assert!(written <= room);
        if written == 0 {
            return Ok(events);
        }
        events.extend_from_slice(&out[..written]);
    }
}

/// The boot keyboard's held state, kept the way the decoder documents it.
#[derive(Default)]
struct KeyboardModel {
    keys: [u8; 6],
    modifiers: u8,
}

impl KeyboardModel {
    /// The key edges `report` makes: releases, then presses, then modifier
    /// changes. The key array is kept when it carries an error usage, and a
    /// modifier usage in it is the bitmap's to report.
    fn edges(&mut self, report: &[u8]) -> Vec<(u16, i32)> {
        let mut keys = [0u8; 6];
        let present = (report.len() - 2).min(6);
        for (key, &usage) in keys.iter_mut().zip(&report[2..2 + present]) {
            if !(0xE0..=0xE7).contains(&usage) {
                *key = usage;
            }
        }
        let mut edges = Vec::new();
        if !keys.iter().any(|&key| (1..=3).contains(&key)) {
            for (slot, &old) in self.keys.iter().enumerate() {
                if old != 0 && !keys.contains(&old) && !self.keys[..slot].contains(&old) {
                    edges.push((u16::from(old), 0));
                }
            }
            for (slot, &new) in keys.iter().enumerate() {
                if new != 0 && !self.keys.contains(&new) && !keys[..slot].contains(&new) {
                    edges.push((u16::from(new), 1));
                }
            }
            self.keys = keys;
        }
        for bit in 0..8u8 {
            if ((self.modifiers ^ report[0]) >> bit) & 1 == 1 {
                edges.push((
                    MODIFIER_USAGE_BASE + u16::from(bit),
                    i32::from((report[0] >> bit) & 1),
                ));
            }
        }
        self.modifiers = report[0];
        edges
    }
}

#[test]
fn the_boot_keyboard_reports_exactly_the_changes_each_report_makes() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_boot_keyboard_reports_exactly_the_changes_each_report_makes",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let endpoint = Endpoint::default();
    let mut keyboard = BootKeyboard::new(endpoint.clone());
    let mut model = KeyboardModel::default();
    let mut console = KeyboardConsole::new();
    let mut held = [false; 256];
    let mut iteration: u64 = 0;
    loop {
        let (report, forged) = random_report(&mut rng);
        endpoint.0.borrow_mut().push_back((report.clone(), forged));
        let delivered = &report[..report.len().min(REPORT_BUF_LEN)];
        let expected = match (forged, delivered.len()) {
            (Some(_), _) => Err(DriverError::DeviceFault),
            (None, 0..2) => Err(DriverError::LengthOutOfRange),
            (None, _) => Ok(model.edges(delivered)),
        };
        let decoded = drain(&mut keyboard, &mut rng).map(|events| {
            events
                .iter()
                .map(|event| {
                    assert_eq!(event.kind, InputEventKind::Key);
                    let _ = console.feed(*event);
                    (event.code, event.value)
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(decoded, expected, "report {report:02x?}");
        for &(code, value) in decoded.iter().flatten() {
            assert!(
                code <= 0xFF && (0..=1).contains(&value),
                "{code:#06x} = {value}"
            );
            let slot = &mut held[usize::from(code)];
            assert_ne!(*slot, value == 1, "{code:#04x} edge to the state it was in");
            *slot = value == 1;
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

#[test]
fn the_boot_mouse_reports_exactly_the_changes_each_report_makes() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_boot_mouse_reports_exactly_the_changes_each_report_makes",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let endpoint = Endpoint::default();
    let mut mouse = BootMouse::new(endpoint.clone());
    let mut buttons = 0u8;
    let mut iteration: u64 = 0;
    loop {
        let (report, forged) = random_report(&mut rng);
        endpoint.0.borrow_mut().push_back((report.clone(), forged));
        let delivered = &report[..report.len().min(REPORT_BUF_LEN)];
        let expected = match (forged, delivered.len()) {
            (Some(_), _) => Err(DriverError::DeviceFault),
            (None, 0..3) => Err(DriverError::LengthOutOfRange),
            (None, _) => {
                let mut events = Vec::new();
                let now = delivered[0] & 0b111;
                for bit in 0..3u8 {
                    if ((buttons ^ now) >> bit) & 1 == 1 {
                        events.push((
                            InputEventKind::Key,
                            POINTER_BUTTON_CODE_BASE + u16::from(bit),
                            i32::from((now >> bit) & 1),
                        ));
                    }
                }
                buttons = now;
                let motion = [
                    (InputEventKind::Pointer, AXIS_X, delivered.get(1)),
                    (InputEventKind::Pointer, AXIS_Y, delivered.get(2)),
                    (InputEventKind::Scroll, AXIS_Y, delivered.get(3)),
                ];
                for (kind, axis, delta) in motion {
                    if let Some(&delta) = delta.filter(|&&delta| delta != 0) {
                        events.push((kind, axis, i32::from(i8::from_le_bytes([delta]))));
                    }
                }
                Ok(events)
            }
        };
        let decoded = drain(&mut mouse, &mut rng).map(|events| {
            events
                .iter()
                .map(|event| (event.kind, event.code, event.value))
                .collect::<Vec<_>>()
        });
        assert_eq!(decoded, expected, "report {report:02x?}");

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}

#[test]
fn the_console_producer_resolves_only_key_edges() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "the_console_producer_resolves_only_key_edges",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut console = KeyboardConsole::new();
    let mut iteration: u64 = 0;
    loop {
        let kind = *rng.pick(&[
            InputEventKind::Key,
            InputEventKind::Pointer,
            InputEventKind::Scroll,
        ]);
        let value = if rng.below(4) == 0 {
            i32::from_le_bytes(rng.next_u32().to_le_bytes())
        } else {
            i32::from(rng.below(2) == 0)
        };
        let code = if rng.below(2) == 0 {
            u16::from(rng.next_u8())
        } else {
            rng.next_u16()
        };
        let record = console.feed(InputEvent {
            kind,
            reserved0: 0,
            code,
            value,
        });
        if kind != InputEventKind::Key || !(0..=1).contains(&value) {
            assert_eq!(record, None, "{kind:?} {code:#06x} = {value}");
        }
        if let Some(record) = record {
            assert_eq!(KeyInput::from_bytes(&record.to_le_bytes()), Ok(record));
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
