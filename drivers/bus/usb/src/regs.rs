//! xHCI register vocabulary (xHCI 1.2 §5).
//!
//! Byte offsets and bit masks for the capability, operational, and
//! doorbell register blocks the bring-up path touches. Only registers
//! the driver actually reads or writes are defined (`AGENTS.md` §2.3);
//! the runtime (interrupter) block joins when event-ring wiring lands.
//!
//! All offsets are relative to the start of the register window the
//! hardware tree reported for the controller — capability offsets from
//! the window base, operational offsets from `CAPLENGTH`, doorbell
//! offsets from `DBOFF` (`AGENTS.md` §18.1 — the base itself is always
//! discovered, never a constant).

/// `CAPLENGTH` (byte 0) and `HCIVERSION` (bytes 2..4) share the first
/// capability dword (xHCI 1.2 §5.3.1/§5.3.2).
pub const CAPLENGTH_HCIVERSION: usize = 0x00;

/// `HCSPARAMS1` — structural parameters 1 (§5.3.3).
pub const HCSPARAMS1: usize = 0x04;

/// `HCCPARAMS1` — capability parameters 1 (§5.3.6).
pub const HCCPARAMS1: usize = 0x10;

/// `DBOFF` — doorbell-array offset from the window base (§5.3.7).
pub const DBOFF: usize = 0x14;

/// `RTSOFF` — runtime-register-space offset from the window base
/// (§5.3.8).
pub const RTSOFF: usize = 0x18;

/// Low bits of `DBOFF` are reserved and masked off before use (§5.3.7).
pub const DBOFF_MASK: u32 = !0x3;

/// Low bits of `RTSOFF` are reserved and masked off before use (§5.3.8).
pub const RTSOFF_MASK: u32 = !0x1F;

/// Minimum legal `CAPLENGTH`: the capability block is at least the
/// eight defined dwords (§5.3). A smaller value means the operational
/// block would overlap the capability block — an absent or broken
/// controller.
pub const CAPLENGTH_MIN: u8 = 0x20;

/// Smallest `HCIVERSION` this driver accepts (xHCI 0.90, the first
/// published revision). An all-ones or zero read — the classic absent
/// MMIO device — fails this check.
pub const HCIVERSION_MIN: u16 = 0x0090;

/// `USBCMD` — operational base + `0x00` (§5.4.1).
pub const USBCMD: usize = 0x00;

/// `USBSTS` — operational base + `0x04` (§5.4.2).
pub const USBSTS: usize = 0x04;

/// `USBCMD` Run/Stop: `1` runs the controller, `0` halts it.
pub const USBCMD_RUN: u32 = 1 << 0;

/// `USBCMD` Host Controller Reset: self-clearing when reset completes.
pub const USBCMD_HCRST: u32 = 1 << 1;

/// `USBSTS` `HCHalted`: set while the controller is halted.
pub const USBSTS_HCH: u32 = 1 << 0;

/// `USBSTS` Controller Not Ready: registers must not be written while
/// set (§4.2 bring-up step 1).
pub const USBSTS_CNR: u32 = 1 << 11;

/// First `PORTSC` register — operational base + `0x400` (§5.4.8).
pub const PORTSC_BASE: usize = 0x400;

/// Byte stride between consecutive ports' register sets (§5.4.8).
pub const PORTSC_STRIDE: usize = 0x10;

/// `PORTSC` Current Connect Status: a device is attached.
pub const PORTSC_CCS: u32 = 1 << 0;

/// `PORTSC` Port Enabled/Disabled.
pub const PORTSC_PED: u32 = 1 << 1;

/// `PORTSC` Port Reset: set while a port reset is in progress.
pub const PORTSC_PR: u32 = 1 << 4;

/// `PORTSC` Port Power.
pub const PORTSC_PP: u32 = 1 << 9;

/// `PORTSC` Port Speed field shift (bits 13:10) — a protocol-defined
/// speed ID (`1` full, `2` low, `3` high, `4` super).
pub const PORTSC_SPEED_SHIFT: u32 = 10;

/// `PORTSC` Port Speed field mask (after shifting).
pub const PORTSC_SPEED_MASK: u32 = 0xF;

/// `PORTSC` Connect Status Change (write-1-to-clear).
pub const PORTSC_CSC: u32 = 1 << 17;

/// `HCSPARAMS1` `MaxSlots` field (bits 7:0).
#[must_use]
pub const fn hcsparams1_max_slots(raw: u32) -> u8 {
    raw.to_le_bytes()[0]
}

/// `HCSPARAMS1` `MaxPorts` field (bits 31:24).
#[must_use]
pub const fn hcsparams1_max_ports(raw: u32) -> u8 {
    raw.to_le_bytes()[3]
}

/// `HCCPARAMS1` AC64 (bit 0): the controller addresses 64-bit DMA.
#[must_use]
pub const fn hccparams1_ac64(raw: u32) -> bool {
    raw & 1 != 0
}

/// `HCCPARAMS1` CSZ (bit 2): device contexts are 64 bytes, not 32.
#[must_use]
pub const fn hccparams1_csz(raw: u32) -> bool {
    raw & (1 << 2) != 0
}

/// `CAPLENGTH` from the first capability dword.
#[must_use]
pub const fn caplength(raw: u32) -> u8 {
    raw.to_le_bytes()[0]
}

/// `HCIVERSION` from the first capability dword.
#[must_use]
pub const fn hciversion(raw: u32) -> u16 {
    let bytes = raw.to_le_bytes();
    u16::from_le_bytes([bytes[2], bytes[3]])
}
