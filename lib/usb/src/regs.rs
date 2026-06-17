//! xHCI register vocabulary (xHCI 1.2 §5).
//!
//! Byte offsets and bit masks for the capability, operational,
//! runtime (interrupter), and doorbell register blocks the bring-up
//! and enumeration paths touch. Only registers the driver actually
//! reads or writes are defined (`AGENTS.md` §2.3).
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

/// `HCSPARAMS2` — structural parameters 2 (§5.3.4). Carries the
/// Max Scratchpad Buffers field the controller requires software to
/// reserve before it can run (the VL805 reports 31).
pub const HCSPARAMS2: usize = 0x08;

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

/// `PAGESIZE` — operational base + `0x08` (§5.4.3). A bitmap: if bit
/// `n` is set the controller supports a page size of `2^(n+12)`; the
/// scratchpad buffers software reserves are each one such page and
/// page-aligned. The lowest set bit is the page size in use.
pub const PAGESIZE: usize = 0x08;

/// `CRCR` — command ring control, operational base + `0x18` (§5.4.5).
/// 64 bits: low dword first, high dword at `+4`.
pub const CRCR: usize = 0x18;

/// `DCBAAP` — device context base address array pointer, operational
/// base + `0x30` (§5.4.6). 64 bits: low dword first, high at `+4`.
pub const DCBAAP: usize = 0x30;

/// `CONFIG` — configure register, operational base + `0x38` (§5.4.7).
/// Bits 7:0 are `MaxSlotsEn`.
pub const CONFIG: usize = 0x38;

/// `CRCR` Ring Cycle State: the consumer cycle state the controller
/// starts the command ring with (§5.4.5).
pub const CRCR_RCS: u32 = 1 << 0;

/// `USBCMD` Run/Stop: `1` runs the controller, `0` halts it.
pub const USBCMD_RUN: u32 = 1 << 0;

/// `USBCMD` Host Controller Reset: self-clearing when reset completes.
pub const USBCMD_HCRST: u32 = 1 << 1;

/// `USBSTS` `HCHalted`: set while the controller is halted.
pub const USBSTS_HCH: u32 = 1 << 0;

/// `USBSTS` Host System Error: a write-1-to-clear latched controller
/// error (§5.4.2). The host-controller reset path may observe this when
/// firmware left a stale error before RustOS takes ownership.
pub const USBSTS_HSE: u32 = 1 << 2;

/// `USBSTS` Port Change Detect: a write-1-to-clear latched port-change
/// status bit (§5.4.2). Firmware handoff can leave it set before RustOS
/// resets the controller.
pub const USBSTS_PCD: u32 = 1 << 4;

/// `USBSTS` Controller Not Ready: the controller is not ready for normal
/// operational programming. The open path enforces it after the
/// host-controller reset so a stale pre-reset status can be cleared first.
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

/// `PORTSC` bits that are write-1-to-clear or reserved-preserve;
/// masked off before a control write so a read-modify-write never
/// clears a change bit by accident (§5.4.8).
pub const PORTSC_RW1C_MASK: u32 = 0x00FE_0002;

/// Byte offset of interrupter 0 within the runtime block (§5.5.2):
/// the interrupter array starts at `RTSOFF + 0x20`.
pub const IR0_BASE: usize = 0x20;

/// `ERSTSZ` — event ring segment table size, interrupter base + `0x08`
/// (§5.5.2.3.1).
pub const IR_ERSTSZ: usize = 0x08;

/// `ERSTBA` — event ring segment table base address, interrupter base
/// + `0x10` (§5.5.2.3.2). 64 bits: low dword first, high at `+4`.
pub const IR_ERSTBA: usize = 0x10;

/// `ERDP` — event ring dequeue pointer, interrupter base + `0x18`
/// (§5.5.2.3.3). 64 bits: low dword first, high at `+4`.
pub const IR_ERDP: usize = 0x18;

/// `ERDP` Event Handler Busy (write-1-to-clear, §5.5.2.3.3): set by
/// the controller when it posts an interrupt, cleared by the driver
/// when it updates the dequeue pointer.
pub const ERDP_EHB: u32 = 1 << 3;

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

/// `HCSPARAMS2` Max Scratchpad Buffers (§5.3.4): the count of
/// page-sized scratchpad buffers software must reserve for the
/// controller's private state, split across a high field (bits 25:21)
/// and a low field (bits 31:27). The VL805 reports 31; a controller
/// reporting `0` needs none.
#[must_use]
pub const fn hcsparams2_max_scratchpad(raw: u32) -> u32 {
    let hi = (raw >> 21) & 0x1F;
    let lo = (raw >> 27) & 0x1F;
    (hi << 5) | lo
}

/// The page size (in bytes) the controller's `PAGESIZE` register
/// reports (§5.4.3): `2^(n+12)` for the lowest set bit `n` of the low
/// 16 bits. Returns `0` when no bit is set (a malformed register), so
/// the caller fails closed rather than assuming a size.
#[must_use]
pub const fn pagesize_bytes(raw: u32) -> usize {
    let supported = raw & 0xFFFF;
    if supported == 0 {
        return 0;
    }
    1usize << (supported.trailing_zeros() as usize + 12)
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
