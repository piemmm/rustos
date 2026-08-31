//! Protocol-faithful mock firmware for host tests.
//!
//! QEMU does not model the `VideoCore`, so every consumer of this crate
//! proves its protocol handling against this mock instead of faking the
//! semantics ad hoc: it walks the request
//! tags, echoes the set-tag values, fills the get-tag responses from
//! its configured answers, and stamps the response codes — exactly what
//! a healthy firmware does. The real doorbell is the on-metal
//! acceptance item (`plans/PI.md` P7/P7b).
//!
//! Compiled only for this crate's own tests and for consumers that
//! enable the `mock-firmware` feature as a dev-dependency; it never
//! ships in a production image.

use crate::{
    MailboxError, MailboxTransport, RtcRegister, CODE_RESPONSE_OK, PROPERTY_WORDS, TAG_ALLOCATE,
    TAG_GET_FIRMWARE_REVISION, TAG_GET_PHYSICAL_WH, TAG_GET_PITCH, TAG_GET_RTC_REG,
    TAG_RESPONSE_BIT, TAG_SET_RTC_REG,
};

/// A mock firmware answering property messages with configured values.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MockFirmware {
    /// Bus address the allocate tag answers with.
    pub fb_bus: u32,
    /// Buffer size in bytes the allocate tag answers with.
    pub fb_size: u32,
    /// Pitch in bytes the get-pitch tag answers with.
    pub fb_pitch: u32,
    /// Display width the display-size tag answers with.
    pub display_w: u32,
    /// Display height the display-size tag answers with.
    pub display_h: u32,
    /// Revision word the firmware-revision (liveness probe) tag answers
    /// with.
    pub firmware_revision: u32,
    /// Seconds the RTC counter holds. Writable through the set-register
    /// tag, so a consumer's write-then-read round trip is faithful.
    pub rtc_secs: u32,
    /// Millivolts the RTC's backup-cell voltage register answers with;
    /// zero models a board with no cell fitted.
    pub rtc_backup_mv: u32,
}

impl MockFirmware {
    /// A healthy firmware: a 640×480×32bpp surface at `0x1000_0000`
    /// physical under the `0xC000_0000` L2-cached alias (pitch 2560),
    /// with a 1920×1080 display attached, and an RTC holding
    /// 2026-01-01T00:00:00Z on a fitted backup cell.
    #[must_use]
    pub const fn healthy() -> Self {
        Self {
            fb_bus: 0xD000_0000,
            fb_size: 2560 * 480,
            fb_pitch: 2560,
            display_w: 1920,
            display_h: 1080,
            firmware_revision: 0x0123_4567,
            rtc_secs: 1_767_225_600,
            rtc_backup_mv: 3000,
        }
    }

    /// Answer one property message in place, as a healthy firmware
    /// would: every tag gains its response bit, the get tags carry the
    /// configured values, and a set-register tag mutates the modelled
    /// register so a write-then-read round trip is faithful.
    pub fn respond(&mut self, message: &mut [u32; PROPERTY_WORDS]) {
        let mut at = 2;
        while at + 3 <= PROPERTY_WORDS {
            let tag = message[at];
            if tag == 0 {
                break;
            }
            let buf_words = (message[at + 1] / 4) as usize;
            let resp_len = match tag {
                TAG_ALLOCATE => {
                    message[at + 3] = self.fb_bus;
                    message[at + 4] = self.fb_size;
                    8
                }
                TAG_GET_PITCH => {
                    message[at + 3] = self.fb_pitch;
                    4
                }
                TAG_GET_PHYSICAL_WH => {
                    message[at + 3] = self.display_w;
                    message[at + 4] = self.display_h;
                    8
                }
                TAG_GET_FIRMWARE_REVISION => {
                    message[at + 3] = self.firmware_revision;
                    4
                }
                // The RTC register tags echo the selector and carry the
                // value in the second word; an unmodelled selector is
                // answered zero, which is what a firmware that knows the
                // tag but not the register does.
                TAG_GET_RTC_REG => {
                    message[at + 4] = self.rtc_register(message[at + 3]);
                    8
                }
                TAG_SET_RTC_REG => {
                    self.set_rtc_register(message[at + 3], message[at + 4]);
                    8
                }
                // Set-tags echo their request values unchanged.
                _ => message[at + 1],
            };
            message[at + 2] = TAG_RESPONSE_BIT | resp_len;
            at += 3 + buf_words;
        }
        message[1] = CODE_RESPONSE_OK;
    }

    /// The modelled value of RTC register `selector`.
    fn rtc_register(&self, selector: u32) -> u32 {
        if selector == RtcRegister::Time.as_u32() {
            self.rtc_secs
        } else if selector == RtcRegister::BackupVolts.as_u32() {
            self.rtc_backup_mv
        } else {
            0
        }
    }

    /// Store `value` into RTC register `selector`, ignoring a selector the
    /// mock does not model (as a firmware ignores an unknown register).
    fn set_rtc_register(&mut self, selector: u32, value: u32) {
        if selector == RtcRegister::Time.as_u32() {
            self.rtc_secs = value;
        } else if selector == RtcRegister::BackupVolts.as_u32() {
            self.rtc_backup_mv = value;
        }
    }
}

impl MailboxTransport for MockFirmware {
    fn exchange(&mut self, message: &mut [u32; PROPERTY_WORDS]) -> Result<(), MailboxError> {
        self.respond(message);
        Ok(())
    }
}
