//! BCM2711 PCIe root-complex controller register map and bit fields.
//!
//! Byte offsets and bit positions are the BCM2711 root-complex register
//! definitions. Only the registers this bring-up driver drives are
//! named; an unused register is not declared (`AGENTS.md` §2.3).
//!
//! All offsets are relative to the controller's MMIO register block —
//! the `reg` window the `brcm,bcm2711-pcie` device-tree node advertises
//! (CPU-physical base `0xfd50_0000`, length `0x9310`), which the wiring
//! maps under [`CapabilityId::MMIO_MAP`](rustos_abi::CapabilityId).

/// Minimum controller register-window length the driver requires, in
/// bytes: through the [`RGR1_SW_INIT_1`] reset register (`0x9210`) plus
/// its dword. The device tree advertises `0x9310`, which covers it.
pub const REGS_LEN_BYTES: usize = 0x9214;

// --- Reset / PERST --------------------------------------------------------

/// `RGR1_SW_INIT_1`: bridge software-reset and fundamental-reset
/// (PERST#) control for the BCM2711.
pub const RGR1_SW_INIT_1: usize = 0x9210;
/// `PERST#` assertion bit within [`RGR1_SW_INIT_1`] (1 = asserted).
pub const RGR1_SW_INIT_1_PERST_MASK: u32 = 0x1;
/// Generic bridge software-init (reset) bit within [`RGR1_SW_INIT_1`]
/// (1 = bridge held in reset).
pub const RGR1_SW_INIT_1_INIT_GENERIC_MASK: u32 = 0x2;

// --- SerDes / hard debug --------------------------------------------------

/// `MISC_HARD_PCIE_HARD_DEBUG`: carries the SerDes power-down
/// (`IDDQ`) control among other hard-debug bits.
pub const MISC_HARD_PCIE_HARD_DEBUG: usize = 0x4204;
/// SerDes `IDDQ` (power-down) bit; cleared to power the SerDes up.
pub const HARD_DEBUG_SERDES_IDDQ_MASK: u32 = 0x0800_0000;

// --- Misc control ---------------------------------------------------------

/// `MISC_MISC_CTRL`: burst size, SCB access, RCB mode, and inbound
/// SCB-window size fields.
pub const MISC_MISC_CTRL: usize = 0x4008;
/// Enable the SCB (system-memory) inbound access path.
pub const MISC_CTRL_SCB_ACCESS_EN_MASK: u32 = 0x1000;
/// Return Unsupported-Request rather than hanging on a config read to
/// an absent function.
pub const MISC_CTRL_CFG_READ_UR_MODE_MASK: u32 = 0x2000;
/// Two-bit SCB maximum burst size field; for the BCM2711 the encoded
/// value is `0` (128 bytes).
pub const MISC_CTRL_MAX_BURST_SIZE_MASK: u32 = 0x30_0000;
/// Read-completion-boundary "max payload size" mode bit.
pub const MISC_CTRL_RCB_MPS_MODE_MASK: u32 = 0x400;
/// Read-completion-boundary 64-byte mode bit.
pub const MISC_CTRL_RCB_64B_MODE_MASK: u32 = 0x80;

// --- Inbound (RC) BAR windows ---------------------------------------------

/// `RC_BAR1_CONFIG_LO`: PCIe→GISB inbound window (disabled by this
/// driver — its size field is cleared).
pub const MISC_RC_BAR1_CONFIG_LO: usize = 0x402c;
/// `RC_BAR2_CONFIG_LO`: low half of the PCIe→system-memory inbound
/// viewport — offset bits plus the 5-bit size field.
pub const MISC_RC_BAR2_CONFIG_LO: usize = 0x4034;
/// `RC_BAR2_CONFIG_HI`: high 32 bits of the inbound viewport offset.
pub const MISC_RC_BAR2_CONFIG_HI: usize = 0x4038;
/// `RC_BAR3_CONFIG_LO`: PCIe→SCB inbound window (disabled by this
/// driver — its size field is cleared).
pub const MISC_RC_BAR3_CONFIG_LO: usize = 0x403c;
/// Five-bit inbound-BAR size field shared by `RC_BAR1`/`RC_BAR2`/`RC_BAR3`
/// (`[4:0]`).
pub const RC_BAR_CONFIG_LO_SIZE_MASK: u32 = 0x1f;

// --- Outbound (CPU→PCIe) memory window 0 -----------------------------------

/// `CPU_2_PCIE_MEM_WIN0_LO`: low 32 bits of the PCIe-space base the
/// outbound window 0 maps to.
pub const MISC_CPU_2_PCIE_MEM_WIN0_LO: usize = 0x400c;
/// `CPU_2_PCIE_MEM_WIN0_HI`: high 32 bits of that PCIe-space base.
pub const MISC_CPU_2_PCIE_MEM_WIN0_HI: usize = 0x4010;
/// `CPU_2_PCIE_MEM_WIN0_BASE_LIMIT`: CPU-side base and limit, in MiB,
/// packed into one register.
pub const MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT: usize = 0x4070;
/// CPU-base field (in MiB) within [`MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT`].
pub const MEM_WIN0_BASE_LIMIT_BASE_MASK: u32 = 0xfff0;
/// CPU-limit field (in MiB) within [`MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT`].
pub const MEM_WIN0_BASE_LIMIT_LIMIT_MASK: u32 = 0xfff0_0000;
/// `CPU_2_PCIE_MEM_WIN0_BASE_HI`: high bits of the CPU-side base.
pub const MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI: usize = 0x4080;
/// High-base field within [`MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI`].
pub const MEM_WIN0_BASE_HI_BASE_MASK: u32 = 0xff;
/// `CPU_2_PCIE_MEM_WIN0_LIMIT_HI`: high bits of the CPU-side limit.
pub const MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI: usize = 0x4084;
/// High-limit field within [`MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI`].
pub const MEM_WIN0_LIMIT_HI_LIMIT_MASK: u32 = 0xff;

// --- Status / role --------------------------------------------------------

/// `MISC_PCIE_STATUS`: link and role status.
pub const MISC_PCIE_STATUS: usize = 0x4068;
/// Controller is operating as a root port (not an endpoint).
pub const PCIE_STATUS_PORT_MASK: u32 = 0x80;
/// Data-link layer active.
pub const PCIE_STATUS_DL_ACTIVE_MASK: u32 = 0x20;
/// Physical link up.
pub const PCIE_STATUS_PHYLINKUP_MASK: u32 = 0x10;

// --- Root-complex private configuration -----------------------------------

/// `RC_CFG_PRIV1_LINK_CAPABILITY`: carries the advertised ASPM support.
pub const RC_CFG_PRIV1_LINK_CAPABILITY: usize = 0x04dc;
/// ASPM-support field within [`RC_CFG_PRIV1_LINK_CAPABILITY`] (`[11:10]`).
pub const RC_CFG_PRIV1_LINK_CAPABILITY_ASPM_SUPPORT_MASK: u32 = 0xc00;
/// ASPM L0s capability bit (advertised value, low bit of the field).
pub const PCIE_LINK_STATE_L0S: u32 = 0x1;
/// ASPM L1 capability bit (advertised value, high bit of the field).
pub const PCIE_LINK_STATE_L1: u32 = 0x2;

/// `RC_CFG_PRIV1_ID_VAL3`: carries the root-complex class code, which
/// must read as a PCI-PCI bridge for config accesses to behave.
pub const RC_CFG_PRIV1_ID_VAL3: usize = 0x043c;
/// Class-code field within [`RC_CFG_PRIV1_ID_VAL3`] (`[23:0]`).
pub const RC_CFG_PRIV1_ID_VAL3_CLASS_CODE_MASK: u32 = 0x00ff_ffff;
/// PCI-PCI bridge class code (base `0x06`, sub `0x04`, prog-if `0x00`).
pub const PCI_CLASS_BRIDGE_PCI: u32 = 0x0006_0400;

/// `RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1`: endian configuration for the
/// inbound BAR path.
pub const RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1: usize = 0x0188;
/// Endian-mode field for BAR2 within
/// [`RC_CFG_VENDOR_VENDOR_SPECIFIC_REG1`] (`[3:2]`).
pub const RC_CFG_VENDOR_REG1_ENDIAN_MODE_BAR2_MASK: u32 = 0xc;
/// Little-endian value for the BAR2 endian-mode field.
pub const RC_CFG_VENDOR_REG1_LITTLE_ENDIAN: u32 = 0x0;

/// Replace the bits selected by `mask` in `reg` with `value`, shifted
/// into the mask's least-significant set bit.
///
/// `value` is masked to the field width *before* shifting, so an
/// over-wide value (e.g. a megabyte count whose high bits belong in a
/// companion register) keeps only the bits the field can hold, avoiding
/// a shift overflow on a wide value.
#[must_use]
pub const fn replace_bits(reg: u32, value: u32, mask: u32) -> u32 {
    let shift = mask.trailing_zeros();
    // The value bits that fit in the field, low-justified.
    let field_bits = mask >> shift;
    let field = (value & field_bits) << shift;
    (reg & !mask) | field
}
