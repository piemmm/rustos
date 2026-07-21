//! HID **report-protocol** descriptor parsing and boot-layout normalisation
//! (USB HID 1.11 §6.2.2).
//!
//! Boot protocol (the fixed report shapes in [`crate::keyboard`] and
//! [`crate::mouse`]) is enough to bring a keyboard or mouse up, but some
//! devices only honour `SET_IDLE` — the request that makes an idle device
//! report only on change instead of streaming a duplicate every polling
//! interval — while running in **report protocol**. To use report protocol
//! the host must parse the device's HID Report Descriptor to learn where each
//! field sits in the report.
//!
//! This module does exactly that, and no more than the boot vocabulary needs:
//! it parses a Report Descriptor into a [`HidReportMap`] locating the boot
//! fields (mouse buttons/X/Y/wheel, keyboard modifiers/key-array) inside a
//! report-protocol report, then [`HidReportMap::normalize`] rewrites one such
//! report into the exact fixed boot layout the existing [`crate::BootMouse`] /
//! [`crate::BootKeyboard`] decoders already consume. The controller-side HID
//! enumeration engine (`tairix_usb`) uses it so the class drivers and the URB
//! ABI stay unchanged: they keep seeing boot-format reports, while the device
//! runs in the quiescent report protocol.
//!
//! It is pure, `no_std`, allocation-free, and fail-closed: an undecodable or
//! unsupported descriptor yields `None` (the caller falls back to boot
//! protocol), never a guess or a panic.

/// HID descriptor item type field (`bType`, bits 2:3 of the item prefix).
const ITEM_TYPE_MAIN: u8 = 0;
const ITEM_TYPE_GLOBAL: u8 = 1;
const ITEM_TYPE_LOCAL: u8 = 2;

/// Main-item tags (`bTag`, bits 4:7).
const MAIN_INPUT: u8 = 0x8;

/// Global-item tags.
const GLOBAL_USAGE_PAGE: u8 = 0x0;
const GLOBAL_REPORT_SIZE: u8 = 0x7;
const GLOBAL_REPORT_ID: u8 = 0x8;
const GLOBAL_REPORT_COUNT: u8 = 0x9;
const GLOBAL_PUSH: u8 = 0xA;
const GLOBAL_POP: u8 = 0xB;

/// Local-item tags.
const LOCAL_USAGE: u8 = 0x0;
const LOCAL_USAGE_MIN: u8 = 0x1;
const LOCAL_USAGE_MAX: u8 = 0x2;

/// HID usage pages we interpret (HID Usage Tables §3).
const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_KEYBOARD: u16 = 0x07;
const PAGE_BUTTON: u16 = 0x09;

/// Generic-desktop usages for pointer motion.
const USAGE_X: u32 = 0x0030;
const USAGE_Y: u32 = 0x0031;
const USAGE_WHEEL: u32 = 0x0038;

/// Keyboard modifier usage range (`LeftControl`..`RightGUI`).
const USAGE_KBD_LCTRL: u32 = 0xE0;
const USAGE_KBD_RGUI: u32 = 0xE7;

/// `Input` main-item data bit 0: `Constant` (padding, no usage) vs `Data`.
const INPUT_CONSTANT: u32 = 1 << 0;
/// `Input` main-item data bit 1: `Variable` (one field per usage) vs `Array`.
const INPUT_VARIABLE: u32 = 1 << 1;

/// Largest Report Descriptor this parser will walk, a validation bound on
/// untrusted device input (not a scalable capacity): far larger than any
/// boot-class keyboard or mouse descriptor, small enough to bound the walk.
pub const MAX_REPORT_DESCRIPTOR: usize = 512;

/// Location of a decoded field within the transmitted report: the bit offset
/// from the start of the report (**including** the leading Report ID byte
/// when the device uses report IDs), the field's bit width, and — for an
/// array field (the keyboard key-array) — its element count.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct FieldLoc {
    offset_bits: u16,
    size_bits: u8,
    count: u8,
}

/// Where a report-protocol **mouse** report keeps the boot fields.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MouseMap {
    /// Report ID prefixing this device's mouse report, or `0` when the
    /// device declares no report IDs (the report has no prefix byte).
    report_id: u8,
    /// The button bits (each 1 bit; `count` of them from bit 0 = left).
    buttons: FieldLoc,
    /// The signed X displacement field.
    x: FieldLoc,
    /// The signed Y displacement field.
    y: FieldLoc,
    /// The signed wheel field, when the device has one.
    wheel: Option<FieldLoc>,
}

/// Where a report-protocol **keyboard** report keeps the boot fields.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct KeyboardMap {
    /// Report ID prefixing this device's keyboard report, or `0` for none.
    report_id: u8,
    /// The eight 1-bit modifier flags (`LeftControl`..`RightGUI`).
    modifiers: FieldLoc,
    /// The key-array field: `count` usage-ID slots of `size_bits` each.
    keys: FieldLoc,
}

/// A parsed HID Report Descriptor reduced to the boot vocabulary: enough to
/// rewrite a report-protocol report into the fixed boot layout the
/// [`crate::BootMouse`] / [`crate::BootKeyboard`] decoders consume.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HidReportMap {
    /// A pointer whose reports carry buttons + X/Y (+ optional wheel).
    Mouse(MouseMap),
    /// A keyboard whose reports carry modifiers + a key-array.
    Keyboard(KeyboardMap),
}

/// A compact, public description of a parsed [`HidReportMap`] for diagnostics
/// (`plans/USB.md`): which boot device the map decodes, the Report ID its
/// reports carry (`0` = no report IDs), and the located bit offset/size/count
/// of its fields. It carries no interpretation logic — it only exposes what
/// the parser decided so the host-controller driver can log how a device's
/// reports are being read on metal (QEMU models no Pi USB).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ReportMapSummary {
    /// `true` for a keyboard map, `false` for a mouse map.
    pub is_keyboard: bool,
    /// Report ID prefixing this device's reports (`0` = no report IDs).
    pub report_id: u8,
    /// Bit offset of the primary field (mouse buttons / keyboard modifiers).
    pub primary_offset_bits: u16,
    /// Bit offset of the secondary field (mouse X / keyboard key array).
    pub secondary_offset_bits: u16,
    /// Per-element bit size of the secondary field.
    pub secondary_size_bits: u8,
    /// Element count of the secondary field (mouse X = 1; keyboard = key
    /// slots).
    pub secondary_count: u8,
}

/// Byte length of the normalised boot mouse report [`HidReportMap::normalize`]
/// writes for a [`HidReportMap::Mouse`] (button byte + X + Y + wheel).
pub const BOOT_MOUSE_NORM_LEN: usize = 4;

/// Byte length of the normalised boot keyboard report
/// [`HidReportMap::normalize`] writes for a [`HidReportMap::Keyboard`].
pub const BOOT_KEYBOARD_NORM_LEN: usize = 8;

/// Key-array slots a boot keyboard report carries (bytes 2..8).
const BOOT_KEY_SLOTS: usize = 6;

/// Read `size_bits` (1..=32) little-endian bits at `offset_bits` from `data`,
/// returning the unsigned value, or `None` if the field runs past `data`
/// (fail closed on a report shorter than the descriptor claims).
fn read_bits(data: &[u8], offset_bits: u32, size_bits: u32) -> Option<u32> {
    if size_bits == 0 || size_bits > 32 {
        return None;
    }
    let end = offset_bits.checked_add(size_bits)?;
    if end as usize > data.len().checked_mul(8)? {
        return None;
    }
    let mut value: u32 = 0;
    for i in 0..size_bits {
        let bit = offset_bits + i;
        let byte = data[(bit / 8) as usize];
        let set = (byte >> (bit % 8)) & 1;
        value |= u32::from(set) << i;
    }
    Some(value)
}

/// Sign-extend an unsigned `size_bits`-wide `value` to `i32`.
#[allow(clippy::cast_possible_wrap)] // Reinterpreting the top bit as the sign is the intent.
fn sign_extend(value: u32, size_bits: u8) -> i32 {
    if size_bits == 0 || size_bits >= 32 {
        return value as i32;
    }
    let shift = 32 - u32::from(size_bits);
    ((value << shift) as i32) >> shift
}

/// Clamp a signed displacement to the boot report's `i8` field, returning its
/// unsigned byte. A fast flick beyond `i8` range saturates rather than
/// wrapping — the same range a boot-protocol report could ever carry.
#[allow(clippy::cast_sign_loss)] // The clamp keeps the value in `i8` range.
fn clamp_i8(value: i32) -> u8 {
    i8::try_from(value.clamp(i32::from(i8::MIN), i32::from(i8::MAX))).unwrap_or(0) as u8
}

/// Max explicit `Usage` items collected per main item — the boot fields need
/// only X/Y/Wheel listed explicitly; a validation bound, not a capacity.
const MAX_USAGES: usize = 16;

/// Depth of the global-item Push/Pop stack (HID §6.2.2.8). A bound on
/// untrusted nesting, not a capacity.
const MAX_PUSH_DEPTH: usize = 8;

/// Global item state saved/restored by Push/Pop.
#[derive(Copy, Clone, Default)]
struct GlobalState {
    usage_page: u16,
    report_size: u32,
    report_count: u32,
}

/// The running parse of a Report Descriptor, accumulating the boot fields.
struct Parser {
    global: GlobalState,
    stack: [GlobalState; MAX_PUSH_DEPTH],
    stack_len: usize,
    /// `0` until a Report ID is declared; reports then carry a 1-byte prefix.
    report_id: u8,
    report_ids_used: bool,
    /// Next field's bit offset within the current report.
    bit_offset: u32,
    usages: [u32; MAX_USAGES],
    usages_len: usize,
    usage_min: Option<u32>,
    usage_max: Option<u32>,
    // Boot fields captured on first sight, each pinned to its report ID.
    mouse_report_id: Option<u8>,
    buttons: Option<FieldLoc>,
    x: Option<FieldLoc>,
    y: Option<FieldLoc>,
    wheel: Option<FieldLoc>,
    kbd_report_id: Option<u8>,
    modifiers: Option<FieldLoc>,
    keys: Option<FieldLoc>,
}

impl Parser {
    fn new() -> Self {
        Self {
            global: GlobalState::default(),
            stack: [GlobalState::default(); MAX_PUSH_DEPTH],
            stack_len: 0,
            report_id: 0,
            report_ids_used: false,
            bit_offset: 0,
            usages: [0; MAX_USAGES],
            usages_len: 0,
            usage_min: None,
            usage_max: None,
            mouse_report_id: None,
            buttons: None,
            x: None,
            y: None,
            wheel: None,
            kbd_report_id: None,
            modifiers: None,
            keys: None,
        }
    }

    /// Clear the local-item state after a main item (HID §6.2.2.8).
    fn clear_local(&mut self) {
        self.usages_len = 0;
        self.usage_min = None;
        self.usage_max = None;
    }

    /// Whether field index `i` of the current variable item carries `usage`,
    /// resolving both the explicit `Usage` list and a `Usage Minimum/Maximum`
    /// range.
    fn field_usage(&self, i: u32) -> Option<u32> {
        if (i as usize) < self.usages_len {
            return Some(self.usages[i as usize]);
        }
        if let (Some(min), Some(max)) = (self.usage_min, self.usage_max) {
            let candidate = min.checked_add(i)?;
            if candidate <= max {
                return Some(candidate);
            }
        }
        None
    }

    /// A field location at the current offset with the given per-field size and
    /// element count.
    fn loc(&self, size_bits: u32, count: u32, extra_offset: u32) -> Option<FieldLoc> {
        let offset = self.bit_offset.checked_add(extra_offset)?;
        Some(FieldLoc {
            offset_bits: u16::try_from(offset).ok()?,
            size_bits: u8::try_from(size_bits).ok()?,
            count: u8::try_from(count).ok()?,
        })
    }

    /// Record the boot fields carried by one `Input` main item, then advance
    /// the running bit offset by the whole item's width.
    fn on_input(&mut self, flags: u32) {
        let size = self.global.report_size;
        let count = self.global.report_count;
        let width = size.saturating_mul(count);
        // A constant (padding) item carries no usage; only advance past it.
        if flags & INPUT_CONSTANT != 0 {
            self.bit_offset = self.bit_offset.saturating_add(width);
            self.clear_local();
            return;
        }
        match self.global.usage_page {
            PAGE_BUTTON if flags & INPUT_VARIABLE != 0 => {
                if self.buttons.is_none() {
                    if let Some(loc) = self.loc(size, count, 0) {
                        self.buttons = Some(loc);
                        self.pin_mouse();
                    }
                }
            }
            PAGE_GENERIC_DESKTOP if flags & INPUT_VARIABLE != 0 => {
                for i in 0..count {
                    match self.field_usage(i) {
                        Some(USAGE_X) if self.x.is_none() => {
                            self.x = self.loc(size, 1, i.saturating_mul(size));
                            self.pin_mouse();
                        }
                        Some(USAGE_Y) if self.y.is_none() => {
                            self.y = self.loc(size, 1, i.saturating_mul(size));
                            self.pin_mouse();
                        }
                        Some(USAGE_WHEEL) if self.wheel.is_none() => {
                            self.wheel = self.loc(size, 1, i.saturating_mul(size));
                            self.pin_mouse();
                        }
                        _ => {}
                    }
                }
            }
            PAGE_KEYBOARD if flags & INPUT_VARIABLE != 0 => {
                // The eight modifier flags: a variable item over the
                // LeftControl..RightGUI usage range.
                let is_modifier_range = self.usage_min == Some(USAGE_KBD_LCTRL)
                    && self.usage_max == Some(USAGE_KBD_RGUI);
                let is_modifiers =
                    is_modifier_range || self.usages[..self.usages_len].contains(&USAGE_KBD_LCTRL);
                if is_modifiers && self.modifiers.is_none() {
                    if let Some(loc) = self.loc(size, count, 0) {
                        self.modifiers = Some(loc);
                        self.pin_keyboard();
                    }
                }
            }
            // The key-array: an array (non-variable) item of usage IDs.
            PAGE_KEYBOARD if self.keys.is_none() => {
                if let Some(loc) = self.loc(size, count, 0) {
                    self.keys = Some(loc);
                    self.pin_keyboard();
                }
            }
            _ => {}
        }
        self.bit_offset = self.bit_offset.saturating_add(width);
        self.clear_local();
    }

    /// Pin the mouse fields to the report ID active when the first was seen;
    /// a later report ID's pointer fields (a second collection) are ignored.
    fn pin_mouse(&mut self) {
        self.mouse_report_id.get_or_insert(self.report_id);
    }

    fn pin_keyboard(&mut self) {
        self.kbd_report_id.get_or_insert(self.report_id);
    }

    /// Assemble the parsed map, preferring a complete keyboard, then a
    /// complete mouse (X and Y are the minimum a pointer map needs).
    fn finish(self) -> Option<HidReportMap> {
        if let (Some(modifiers), Some(keys), Some(report_id)) =
            (self.modifiers, self.keys, self.kbd_report_id)
        {
            return Some(HidReportMap::Keyboard(KeyboardMap {
                report_id,
                modifiers,
                keys,
            }));
        }
        if let (Some(x), Some(y), Some(report_id)) = (self.x, self.y, self.mouse_report_id) {
            return Some(HidReportMap::Mouse(MouseMap {
                report_id,
                // A pointer with no button field reports a zero button byte.
                buttons: self.buttons.unwrap_or(FieldLoc {
                    offset_bits: 0,
                    size_bits: 0,
                    count: 0,
                }),
                x,
                y,
                wheel: self.wheel,
            }));
        }
        None
    }
}

/// Parse a HID Report Descriptor into the boot vocabulary, or `None` if it is
/// undecodable, oversize, or carries neither a mouse nor a keyboard the boot
/// layout can represent (the caller then falls back to boot protocol).
#[must_use]
pub fn parse(desc: &[u8]) -> Option<HidReportMap> {
    if desc.is_empty() || desc.len() > MAX_REPORT_DESCRIPTOR {
        return None;
    }
    let mut p = Parser::new();
    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        i += 1;
        // Long items (prefix 0xFE): skip bDataSize + 1 tag byte + data.
        if prefix == 0xFE {
            let data_size = *desc.get(i)? as usize;
            i = i.checked_add(1)?.checked_add(data_size)?;
            continue;
        }
        let b_size = (prefix & 0x03) as usize;
        let data_len = if b_size == 3 { 4 } else { b_size };
        let b_type = (prefix >> 2) & 0x03;
        let b_tag = (prefix >> 4) & 0x0F;
        if i + data_len > desc.len() {
            return None;
        }
        let mut data: u32 = 0;
        for (shift, &byte) in desc[i..i + data_len].iter().enumerate() {
            data |= u32::from(byte) << (8 * shift);
        }
        i += data_len;
        match b_type {
            ITEM_TYPE_MAIN => match b_tag {
                MAIN_INPUT => p.on_input(data),
                // A Collection/End-Collection/Output/Feature main item carries no
                // input field; it only ends the current local item state.
                _ => p.clear_local(),
            },
            ITEM_TYPE_GLOBAL => match b_tag {
                GLOBAL_USAGE_PAGE => {
                    p.global.usage_page = u16::try_from(data & 0xFFFF).unwrap_or(0);
                }
                GLOBAL_REPORT_SIZE => p.global.report_size = data,
                GLOBAL_REPORT_COUNT => p.global.report_count = data,
                GLOBAL_REPORT_ID => {
                    p.report_id = u8::try_from(data & 0xFF).unwrap_or(0);
                    p.report_ids_used = true;
                    // Each report ID's fields start after its 1-byte prefix.
                    p.bit_offset = 8;
                }
                GLOBAL_PUSH => {
                    if p.stack_len >= MAX_PUSH_DEPTH {
                        return None;
                    }
                    p.stack[p.stack_len] = p.global;
                    p.stack_len += 1;
                }
                GLOBAL_POP => {
                    p.stack_len = p.stack_len.checked_sub(1)?;
                    p.global = p.stack[p.stack_len];
                }
                _ => {}
            },
            ITEM_TYPE_LOCAL => match b_tag {
                LOCAL_USAGE => {
                    if p.usages_len < MAX_USAGES {
                        p.usages[p.usages_len] = data;
                        p.usages_len += 1;
                    }
                }
                LOCAL_USAGE_MIN => p.usage_min = Some(data),
                LOCAL_USAGE_MAX => p.usage_max = Some(data),
                _ => {}
            },
            _ => {}
        }
    }
    let _ = p.report_ids_used;
    p.finish()
}

impl HidReportMap {
    /// Rewrite one report-protocol report `raw` into the fixed boot layout in
    /// `out`, returning the boot report length.
    ///
    /// * A [`Self::Mouse`] yields [`BOOT_MOUSE_NORM_LEN`] bytes
    ///   `[buttons, dx, dy, wheel]` (the [`crate::BootMouse`] layout).
    /// * A [`Self::Keyboard`] yields [`BOOT_KEYBOARD_NORM_LEN`] bytes
    ///   `[modifiers, 0, k0..k5]` (the [`crate::BootKeyboard`] layout).
    ///
    /// Returns `None` — never a partial or guessed report — when `raw` does
    /// not carry this map's report ID (a different report on the same
    /// endpoint), when a field runs past `raw` (a truncated report), or when
    /// `out` is too small. A `None` is treated by the caller exactly like an
    /// idle/no-op report: nothing is delivered.
    #[must_use]
    pub fn normalize(&self, raw: &[u8], out: &mut [u8]) -> Option<usize> {
        match self {
            Self::Mouse(map) => map.normalize(raw, out),
            Self::Keyboard(map) => map.normalize(raw, out),
        }
    }

    /// A compact [`ReportMapSummary`] of what this map decodes, for the
    /// host-controller driver's metal diagnostics (`plans/USB.md`).
    #[must_use]
    pub fn summary(&self) -> ReportMapSummary {
        match self {
            Self::Mouse(map) => ReportMapSummary {
                is_keyboard: false,
                report_id: map.report_id,
                primary_offset_bits: map.buttons.offset_bits,
                secondary_offset_bits: map.x.offset_bits,
                secondary_size_bits: map.x.size_bits,
                secondary_count: 1,
            },
            Self::Keyboard(map) => ReportMapSummary {
                is_keyboard: true,
                report_id: map.report_id,
                primary_offset_bits: map.modifiers.offset_bits,
                secondary_offset_bits: map.keys.offset_bits,
                secondary_size_bits: map.keys.size_bits,
                secondary_count: map.keys.count,
            },
        }
    }
}

/// Strip and check the report-ID prefix, returning the body offset in bits.
fn report_body_offset(report_id: u8, raw: &[u8]) -> Option<u32> {
    if report_id == 0 {
        return Some(0);
    }
    // A device with report IDs prefixes every report with its ID byte; a
    // report whose ID is not ours belongs to another collection — skip it.
    if raw.first().copied()? != report_id {
        return None;
    }
    Some(0)
}

impl MouseMap {
    fn normalize(&self, raw: &[u8], out: &mut [u8]) -> Option<usize> {
        if out.len() < BOOT_MOUSE_NORM_LEN {
            return None;
        }
        // The recorded offsets already include the report-ID prefix byte, so
        // only the ID demux (not an offset shift) depends on it.
        report_body_offset(self.report_id, raw)?;
        let buttons = if self.buttons.size_bits == 0 {
            0
        } else {
            let count = u32::from(self.buttons.count) * u32::from(self.buttons.size_bits);
            read_bits(raw, u32::from(self.buttons.offset_bits), count)? & 0xFF
        };
        let x = sign_extend(
            read_bits(
                raw,
                u32::from(self.x.offset_bits),
                u32::from(self.x.size_bits),
            )?,
            self.x.size_bits,
        );
        let y = sign_extend(
            read_bits(
                raw,
                u32::from(self.y.offset_bits),
                u32::from(self.y.size_bits),
            )?,
            self.y.size_bits,
        );
        let wheel = match self.wheel {
            Some(w) => sign_extend(
                read_bits(raw, u32::from(w.offset_bits), u32::from(w.size_bits))?,
                w.size_bits,
            ),
            None => 0,
        };
        out[0] = (buttons & 0xFF) as u8;
        out[1] = clamp_i8(x);
        out[2] = clamp_i8(y);
        out[3] = clamp_i8(wheel);
        Some(BOOT_MOUSE_NORM_LEN)
    }
}

impl KeyboardMap {
    fn normalize(&self, raw: &[u8], out: &mut [u8]) -> Option<usize> {
        if out.len() < BOOT_KEYBOARD_NORM_LEN {
            return None;
        }
        report_body_offset(self.report_id, raw)?;
        // The modifier byte is the front of the report; a report too short to
        // even carry it is not this keyboard's report.
        let modifiers = read_bits(raw, u32::from(self.modifiers.offset_bits), 8)?;
        out[0] = (modifiers & 0xFF) as u8;
        out[1] = 0;
        for slot in 0..BOOT_KEY_SLOTS {
            out[2 + slot] = 0;
        }
        // Fill each key slot that is present in `raw`, stopping at the first
        // one that runs past the report rather than dropping the whole report.
        // A report captured a byte short (e.g. a longer report-protocol report
        // clipped to the boot buffer) still delivers the keys that arrived —
        // silently dropping it instead is what lost every keypress.
        let slots = usize::from(self.keys.count).min(BOOT_KEY_SLOTS);
        for slot in 0..slots {
            let offset = u32::from(self.keys.offset_bits)
                + u32::try_from(slot).unwrap_or(0) * u32::from(self.keys.size_bits);
            let Some(usage) = read_bits(raw, offset, u32::from(self.keys.size_bits)) else {
                break;
            };
            out[2 + slot] = (usage & 0xFF) as u8;
        }
        Some(BOOT_KEYBOARD_NORM_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical boot mouse Report Descriptor (USB HID 1.11 Appendix E.10):
    /// 3 button bits + 5 padding bits, then 8-bit relative X and Y. Reports are
    /// 3 bytes: `[buttons, x, y]`.
    const BOOT_MOUSE_DESC: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95,
        0x02, 0x81, 0x06, 0xC0, 0xC0,
    ];

    /// A report-protocol mouse with a wheel (a fourth 8-bit relative axis):
    /// reports are 4 bytes `[buttons, x, y, wheel]`.
    const WHEEL_MOUSE_DESC: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
        0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05,
        0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75,
        0x08, 0x95, 0x03, 0x81, 0x06, 0xC0, 0xC0,
    ];

    /// The canonical boot keyboard Report Descriptor (USB HID 1.11 Appendix
    /// E.6): 8 modifier bits, an 8-bit reserved byte, an LED **output**
    /// report, then a 6-byte key array. Input reports are 8 bytes.
    const BOOT_KEYBOARD_DESC: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05,
        0x75, 0x01, 0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91,
        0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65,
        0x81, 0x00, 0xC0,
    ];

    #[test]
    fn parses_boot_mouse_layout() {
        let map = parse(BOOT_MOUSE_DESC).expect("a boot mouse descriptor parses");
        match map {
            HidReportMap::Mouse(m) => {
                assert_eq!(m.report_id, 0);
                assert_eq!(m.buttons.offset_bits, 0);
                assert_eq!(m.buttons.count, 3);
                assert_eq!(m.buttons.size_bits, 1);
                assert_eq!(m.x.offset_bits, 8);
                assert_eq!(m.x.size_bits, 8);
                assert_eq!(m.y.offset_bits, 16);
                assert!(m.wheel.is_none());
            }
            HidReportMap::Keyboard(_) => panic!("expected a mouse map"),
        }
    }

    #[test]
    fn normalizes_a_boot_mouse_report_to_boot_bytes() {
        let map = parse(BOOT_MOUSE_DESC).unwrap();
        // Left button held, +5 X, -5 Y.
        let raw = [0x01u8, 0x05, 0xFB];
        let mut out = [0u8; 8];
        let n = map.normalize(&raw, &mut out).expect("normalizes");
        assert_eq!(n, BOOT_MOUSE_NORM_LEN);
        assert_eq!(out[0], 0x01);
        assert_eq!(out[1], 5);
        assert_eq!(i8::from_le_bytes([out[2]]), -5);
        assert_eq!(out[3], 0);
    }

    #[test]
    fn an_idle_mouse_report_normalizes_to_a_no_op() {
        // The idle streaming report a boot mouse repeats: all zero. It
        // normalizes to the zero boot report, which BootMouse decodes to no
        // events — the quiescence the report-protocol change relies on.
        let map = parse(BOOT_MOUSE_DESC).unwrap();
        let mut out = [0xFFu8; 8];
        let n = map.normalize(&[0, 0, 0], &mut out).unwrap();
        assert_eq!(&out[..n], &[0, 0, 0, 0]);
    }

    #[test]
    fn parses_and_normalizes_a_wheel_mouse() {
        let map = parse(WHEEL_MOUSE_DESC).unwrap();
        let HidReportMap::Mouse(m) = map else {
            panic!("expected a mouse map");
        };
        let wheel = m.wheel.expect("wheel field present");
        assert_eq!(wheel.offset_bits, 24);
        // buttons=right, x=+1, y=+2, wheel=-1.
        let raw = [0x02u8, 0x01, 0x02, 0xFF];
        let mut out = [0u8; 8];
        map.normalize(&raw, &mut out).unwrap();
        assert_eq!(out, [0x02, 1, 2, 0xFF, 0, 0, 0, 0]);
    }

    #[test]
    fn parses_boot_keyboard_layout() {
        let map = parse(BOOT_KEYBOARD_DESC).expect("a boot keyboard descriptor parses");
        let HidReportMap::Keyboard(k) = map else {
            panic!("expected a keyboard map");
        };
        assert_eq!(k.report_id, 0);
        assert_eq!(k.modifiers.offset_bits, 0);
        // The reserved byte (bits 8..16) is skipped; the key array starts at
        // byte 2, so the LED output report never shifts the input layout.
        assert_eq!(k.keys.offset_bits, 16);
        assert_eq!(k.keys.size_bits, 8);
        assert_eq!(k.keys.count, 6);
    }

    #[test]
    fn normalizes_a_keyboard_report_to_boot_bytes() {
        let map = parse(BOOT_KEYBOARD_DESC).unwrap();
        // LeftShift held (modifier bit 1), keys A (0x04) and B (0x05).
        let raw = [0x02u8, 0x00, 0x04, 0x05, 0, 0, 0, 0];
        let mut out = [0u8; 8];
        let n = map.normalize(&raw, &mut out).unwrap();
        assert_eq!(n, BOOT_KEYBOARD_NORM_LEN);
        assert_eq!(out, [0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0]);
    }

    #[test]
    fn report_id_prefixed_mouse_demuxes_by_id() {
        // A mouse whose reports carry Report ID 2: buttons + X + Y, each field
        // shifted one byte past the leading ID byte.
        let desc: &[u8] = &[
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09,
            0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
            0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81,
            0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xC0, 0xC0,
        ];
        let map = parse(desc).unwrap();
        let HidReportMap::Mouse(m) = map else {
            panic!("expected a mouse map");
        };
        assert_eq!(m.report_id, 2);
        assert_eq!(m.buttons.offset_bits, 8);
        assert_eq!(m.x.offset_bits, 16);
        let mut out = [0u8; 8];
        // The right report ID: decoded.
        assert!(map.normalize(&[0x02, 0x01, 0x03, 0xFE], &mut out).is_some());
        assert_eq!(out[0], 0x01);
        assert_eq!(out[1], 3);
        assert_eq!(i8::from_le_bytes([out[2]]), -2);
        // A different report ID on the same endpoint is not ours: skipped.
        assert!(map.normalize(&[0x03, 0x01, 0x03, 0xFE], &mut out).is_none());
    }

    #[test]
    fn a_truncated_report_fails_closed() {
        let map = parse(BOOT_MOUSE_DESC).unwrap();
        let mut out = [0u8; 8];
        // Only one byte where three are needed: no fabricated report.
        assert!(map.normalize(&[0x00], &mut out).is_none());
    }

    #[test]
    fn a_too_small_output_buffer_is_refused() {
        let map = parse(BOOT_MOUSE_DESC).unwrap();
        let mut out = [0u8; 2];
        assert!(map.normalize(&[0, 0, 0], &mut out).is_none());
    }

    #[test]
    fn junk_and_empty_descriptors_are_rejected() {
        assert!(parse(&[]).is_none());
        // A lone Usage Page item defines no input report.
        assert!(parse(&[0x05, 0x01]).is_none());
        // An oversize descriptor is refused rather than walked.
        let big = [0u8; MAX_REPORT_DESCRIPTOR + 1];
        assert!(parse(&big).is_none());
    }

    #[test]
    fn sign_extend_covers_widths() {
        assert_eq!(sign_extend(0x7F, 8), 127);
        assert_eq!(sign_extend(0x80, 8), -128);
        assert_eq!(sign_extend(0xFFF, 12), -1);
        assert_eq!(sign_extend(0x7FF, 12), 2047);
    }

    #[test]
    fn clamp_saturates_beyond_i8() {
        assert_eq!(clamp_i8(5), 5);
        assert_eq!(i8::from_le_bytes([clamp_i8(-5)]), -5);
        assert_eq!(i8::from_le_bytes([clamp_i8(1000)]), 127);
        assert_eq!(i8::from_le_bytes([clamp_i8(-1000)]), -128);
    }

    #[test]
    fn twelve_bit_axes_are_read_and_clamped() {
        // A high-resolution mouse: buttons (3 bits + 5 pad), then 12-bit X and
        // 12-bit Y packed into three bytes.
        let desc: &[u8] = &[
            0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01,
            0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01,
            0x75, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0xF8, 0x26,
            0xFF, 0x07, 0x75, 0x0C, 0x95, 0x02, 0x81, 0x06, 0xC0, 0xC0,
        ];
        let map = parse(desc).unwrap();
        let HidReportMap::Mouse(m) = map else {
            panic!("expected a mouse map");
        };
        assert_eq!(m.x.size_bits, 12);
        assert_eq!(m.x.offset_bits, 8);
        assert_eq!(m.y.offset_bits, 20);
        // X = 0x001 (+1), Y = 0xFFF (-1): packed little-endian across bytes
        // 1..4 as 0x01, 0xF0, 0xFF.
        let raw = [0x00u8, 0x01, 0xF0, 0xFF];
        let mut out = [0u8; 8];
        map.normalize(&raw, &mut out).unwrap();
        assert_eq!(i8::from_le_bytes([out[1]]), 1);
        assert_eq!(i8::from_le_bytes([out[2]]), -1);
    }

    /// A keyboard that declares a Report ID: its reports carry a leading ID
    /// byte, so modifiers sit at bit 8, the reserved byte at 16, and the
    /// six-key array at bit 24 — a **9-byte** report on the wire.
    const REPORT_ID_KEYBOARD_DESC: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15,
        0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01,
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81,
        0x00, 0xC0,
    ];

    #[test]
    fn report_id_keyboard_offsets_follow_the_id_byte() {
        let map = parse(REPORT_ID_KEYBOARD_DESC).expect("a report-ID keyboard parses");
        let HidReportMap::Keyboard(k) = map else {
            panic!("expected a keyboard map");
        };
        assert_eq!(k.report_id, 1);
        assert_eq!(k.modifiers.offset_bits, 8);
        assert_eq!(k.keys.offset_bits, 24);
        assert_eq!(k.keys.count, 6);
    }

    #[test]
    fn full_report_id_keyboard_report_normalizes() {
        let map = parse(REPORT_ID_KEYBOARD_DESC).unwrap();
        // The full 9-byte report: id=1, LeftShift, then keys A,B.
        let raw = [0x01u8, 0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0];
        let mut out = [0u8; 8];
        let n = map
            .normalize(&raw, &mut out)
            .expect("normalizes the full report");
        assert_eq!(n, BOOT_KEYBOARD_NORM_LEN);
        assert_eq!(out, [0x02, 0x00, 0x04, 0x05, 0, 0, 0, 0]);
    }

    #[test]
    fn truncated_report_id_keyboard_keeps_present_fields() {
        // The regression: a 9-byte report captured into only 8 bytes loses its
        // final key slot. A robust normalize must still deliver the modifiers
        // and the keys that DID arrive, never drop the whole report (which
        // silenced every keypress).
        let map = parse(REPORT_ID_KEYBOARD_DESC).unwrap();
        let raw = [0x01u8, 0x02, 0x00, 0x04, 0x05, 0, 0, 0]; // 8 bytes, last slot cut
        let mut out = [0xFFu8; 8];
        let n = map
            .normalize(&raw, &mut out)
            .expect("a truncated report still delivers its present fields");
        assert_eq!(n, BOOT_KEYBOARD_NORM_LEN);
        assert_eq!(out[0], 0x02);
        assert_eq!(out[2], 0x04);
        assert_eq!(out[3], 0x05);
    }
}
