//! Host tests for the BCM2711 PCIe root-complex bring-up.
//!
//! QEMU models no Pi PCIe link timing (`AGENTS.md` §0.4 / §2.1), so
//! these tests drive the [`BrcmPcieRc`] state machine over a
//! register-level mock that models the root-port role bit and link-up
//! after a bounded number of status polls. The live link training is
//! the on-metal acceptance item.

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;

use rustos_abi::driver::mmio::MmioMapError;
use rustos_abi::{
    CapabilityId, DriverError, DriverHandle, DriverHost, DriverKind, MmioMapper, RegisterWindow,
};

use super::*;

/// The discovered Pi 4 windows: inbound viewport at PCIe 0 covering the
/// low 3 GiB of SDRAM, outbound MMIO at CPU `0x6_0000_0000` → PCIe
/// `0xc000_0000`, 1 GiB.
const PI_WINDOWS: PcieWindows = PcieWindows {
    inbound_pcie_base: 0,
    inbound_size: 0xc000_0000,
    outbound_cpu_base: 0x6_0000_0000,
    outbound_pcie_base: 0xc000_0000,
    outbound_size: 0x4000_0000,
};

/// A no-op delay: host tests assert register effects, not real time.
/// The link bring-up bounds its polls by iteration count and never reads
/// `now_us`, so a fixed clock is sufficient here.
struct NoDelay;
impl Delay for NoDelay {
    fn delay_us(&self, _us: u32) {}

    fn now_us(&self) -> u64 {
        0
    }
}

/// A monotonic clock that advances by a fixed `step` on every `now_us`
/// read, so the bring-up's four phase marks read four strictly-increasing
/// timestamps and each phase span is a positive, known value — the
/// property the `bring_up_timing` per-phase split (the metal-stall
/// localiser, `AGENTS.md` §15.7) relies on. `delay_us` is a no-op; the
/// split measures elapsed time between marks, not the (host-meaningless)
/// real delay.
struct SteppingDelay {
    now: core::cell::Cell<u64>,
    step: u64,
}
impl SteppingDelay {
    fn new(step: u64) -> Self {
        Self {
            now: core::cell::Cell::new(0),
            step,
        }
    }
}
impl Delay for SteppingDelay {
    fn delay_us(&self, _us: u32) {}

    fn now_us(&self) -> u64 {
        let v = self.now.get();
        self.now.set(v + self.step);
        v
    }
}

/// A register-file mock standing in for the controller window.
///
/// Records every write in order and models the read-only status
/// register: the root-port role bit reflects `root_port`, and the
/// link-up bits assert once the status register has been read more than
/// `link_after` times (so a bounded poll can succeed or, with a large
/// `link_after`, never).
struct MockRegs {
    mem: Vec<u32>,
    writes: Vec<(usize, u32)>,
    root_port: bool,
    link_after: u32,
    status_reads: u32,
}

impl MockRegs {
    fn new(root_port: bool, link_after: u32) -> Self {
        Self {
            mem: vec![0u32; regs::REGS_LEN_BYTES / 4 + 1],
            writes: Vec::new(),
            root_port,
            link_after,
            status_reads: 0,
        }
    }

    /// Index of the first recorded write to `offset`, if any.
    fn write_index(&self, offset: usize) -> Option<usize> {
        self.writes.iter().position(|(o, _)| *o == offset)
    }
}

impl PcieRegs for MockRegs {
    fn read32(&mut self, offset: usize) -> Result<u32, DriverError> {
        if offset + 4 > self.mem.len() * 4 {
            return Err(DriverError::DeviceFault);
        }
        if offset == regs::MISC_PCIE_STATUS {
            self.status_reads += 1;
            let mut v = self.mem[offset / 4];
            if self.root_port {
                v |= regs::PCIE_STATUS_PORT_MASK;
            }
            if self.status_reads > self.link_after {
                v |= regs::PCIE_STATUS_DL_ACTIVE_MASK | regs::PCIE_STATUS_PHYLINKUP_MASK;
            }
            return Ok(v);
        }
        Ok(self.mem[offset / 4])
    }

    fn write32(&mut self, offset: usize, value: u32) -> Result<(), DriverError> {
        if offset + 4 > self.mem.len() * 4 {
            return Err(DriverError::DeviceFault);
        }
        self.mem[offset / 4] = value;
        self.writes.push((offset, value));
        Ok(())
    }
}

#[test]
fn register_requires_drv_load() {
    struct H(bool);
    impl DriverHost for H {
        fn has_capability(&self, cap: CapabilityId) -> bool {
            cap == CapabilityId::DRV_LOAD && self.0
        }
        fn kind(&self) -> DriverKind {
            DriverKind::UserSpace
        }
    }
    assert_eq!(
        register(&H(false)).err(),
        Some(DriverError::PermissionDenied)
    );
    assert!(register(&H(true)).is_ok());
    // The marker is a valid (non-zero) handle.
    assert!(DriverHandle::from_raw(REGISTER_HANDLE_MARKER).is_ok());
}

#[test]
fn encode_ibar_size_covers_the_documented_ranges() {
    // 3 GiB of SDRAM rounds up to a 4 GiB viewport → log2 32 → 0x11.
    assert_eq!(encode_ibar_size(0xc000_0000), 0x11);
    // Exact power-of-two boundaries.
    assert_eq!(encode_ibar_size(0x1000), 0x1c); // 4 KiB
    assert_eq!(encode_ibar_size(0x8000), 0x1f); // 32 KiB
    assert_eq!(encode_ibar_size(0x1_0000), 1); // 64 KiB
    assert_eq!(encode_ibar_size(8u64 << 30), 0x12); // 8 GiB → log2 33
                                                    // Out of range / degenerate → disabled (0), failing closed.
    assert_eq!(encode_ibar_size(0), 0);
    assert_eq!(encode_ibar_size(0x800), 0); // 2 KiB, below 4 KiB
    assert_eq!(encode_ibar_size(64u64 << 30), 0); // 64 GiB, above 32 GiB
}

#[test]
fn encode_scb_size_sizes_the_inbound_scb_window_to_the_region() {
    // The inbound SCB decode window is `ilog2(round_pow2(size)) - 15`;
    // for the same DMA region
    // it equals the `RC_BAR2` size encoding (no 4 KiB‥32 KiB special
    // case), so the inbound decoder covers exactly what the viewport
    // exposes and VideoCore's VL805 firmware-load DMA is not dropped.
    assert_eq!(encode_scb_size(0xc000_0000), 0x11); // 3 GiB → 4 GiB → log2 32
    assert_eq!(encode_scb_size(0x1_0000_0000), 0x11); // 4 GiB exact
    assert_eq!(encode_scb_size(8u64 << 30), 0x12); // 8 GiB → log2 33
    assert_eq!(encode_scb_size(0x1_0000), 1); // 64 KiB (smallest valid)
                                              // Out of range / degenerate → 0 (smallest encoding), failing closed.
    assert_eq!(encode_scb_size(0), 0);
    assert_eq!(encode_scb_size(0x8000), 0); // 32 KiB, below 64 KiB
    assert_eq!(encode_scb_size(128u64 << 30), 0); // 128 GiB, above 64 GiB
}

#[test]
fn bring_up_trains_the_link_and_programs_the_windows() {
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();

    // PERST# ends deasserted and the bridge reset is released.
    let final_swinit = m.mem[regs::RGR1_SW_INIT_1 / 4];
    assert_eq!(final_swinit & regs::RGR1_SW_INIT_1_PERST_MASK, 0);
    assert_eq!(final_swinit & regs::RGR1_SW_INIT_1_INIT_GENERIC_MASK, 0);

    // Misc control carries the bring-up bits; burst size is 0.
    let ctrl = m.mem[regs::MISC_MISC_CTRL / 4];
    assert_ne!(ctrl & regs::MISC_CTRL_SCB_ACCESS_EN_MASK, 0);
    assert_ne!(ctrl & regs::MISC_CTRL_CFG_READ_UR_MODE_MASK, 0);
    assert_ne!(ctrl & regs::MISC_CTRL_RCB_MPS_MODE_MASK, 0);
    assert_ne!(ctrl & regs::MISC_CTRL_RCB_64B_MODE_MASK, 0);
    assert_eq!(ctrl & regs::MISC_CTRL_MAX_BURST_SIZE_MASK, 0);
    // The inbound SCB decode window is sized to the inbound region
    // (`SCB0_SIZE`), matching the `RC_BAR2` size encoding (0x11 for the
    // Pi's 4 GiB viewport) so VideoCore's VL805 firmware-load DMA is not
    // silently dropped by an undersized inbound decoder.
    let scb0 =
        (ctrl & regs::MISC_CTRL_SCB0_SIZE_MASK) >> regs::MISC_CTRL_SCB0_SIZE_MASK.trailing_zeros();
    assert_eq!(scb0, 0x11);

    // Inbound viewport: offset 0, size field 0x11 (4 GiB).
    let bar2_lo = m.mem[regs::MISC_RC_BAR2_CONFIG_LO / 4];
    assert_eq!(bar2_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK, 0x11);
    assert_eq!(m.mem[regs::MISC_RC_BAR2_CONFIG_HI / 4], 0);

    // The unused inbound windows are disabled (size field cleared).
    assert_eq!(
        m.mem[regs::MISC_RC_BAR1_CONFIG_LO / 4] & regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        0
    );
    assert_eq!(
        m.mem[regs::MISC_RC_BAR3_CONFIG_LO / 4] & regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        0
    );

    // The RC advertises itself as a PCI-PCI bridge and ASPM L0s+L1.
    assert_eq!(
        m.mem[regs::RC_CFG_PRIV1_ID_VAL3 / 4] & regs::RC_CFG_PRIV1_ID_VAL3_CLASS_CODE_MASK,
        regs::PCI_CLASS_BRIDGE_PCI
    );
    let aspm = (m.mem[regs::RC_CFG_PRIV1_LINK_CAPABILITY / 4]
        & regs::RC_CFG_PRIV1_LINK_CAPABILITY_ASPM_SUPPORT_MASK)
        >> regs::RC_CFG_PRIV1_LINK_CAPABILITY_ASPM_SUPPORT_MASK.trailing_zeros();
    assert_eq!(aspm, regs::PCIE_LINK_STATE_L0S | regs::PCIE_LINK_STATE_L1);

    // Outbound window 0 maps CPU 0x6_0000_0000 → PCIe 0xc000_0000.
    assert_eq!(m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_LO / 4], 0xc000_0000);
    assert_eq!(m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_HI / 4], 0);
    // CPU base 0x6000 MiB: low field holds bits [11:0] (0), high field
    // holds the rest (6). The low field is zero for this Pi window; a
    // non-zero low-half read-back here would be the inverted-window bug.
    assert_eq!(
        m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT / 4] & regs::MEM_WIN0_BASE_LIMIT_BASE_MASK,
        0
    );
    assert_eq!(
        m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI / 4] & regs::MEM_WIN0_BASE_HI_BASE_MASK,
        6
    );
}

#[test]
fn entry_inbound_window_reports_the_state_before_bring_up_programs_it() {
    // An all-zero mock hands off an unconfigured inbound window, so the
    // entry capture reads size 0 while the post-program read-back carries
    // the discovered 0x11 size field — the two `4120`/`4119` captures a
    // metal run compares (raspberrypi/firmware #1495).
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let entry = rc.entry_inbound_window();
    assert_eq!(entry.rc_bar2_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK, 0);
    let m = rc.regs();
    assert_eq!(
        m.mem[regs::MISC_RC_BAR2_CONFIG_LO / 4] & regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        0x11
    );
}

#[test]
fn bring_up_preserves_a_firmware_configured_inbound_window() {
    // raspberrypi/firmware #1495: `VideoCore`'s `NOTIFY_XHCI_RESET`
    // firmware load assumes the `RC_BAR2` state the boot firmware set at
    // power-on. When the previous boot stage left the inbound window
    // configured (a non-zero size field), bring-up must leave it exactly
    // as the firmware set it rather than overwriting it.
    let mut regs0 = MockRegs::new(true, 1);
    // Seed a distinctive firmware-configured window: a recognisable base
    // plus the 8 GiB size encoding, high half at PCIe 0x4_0000_0000.
    let seeded_lo = 0xABCD_0000 | 0x12;
    let seeded_hi = 0x4;
    regs0.mem[regs::MISC_RC_BAR2_CONFIG_LO / 4] = seeded_lo;
    regs0.mem[regs::MISC_RC_BAR2_CONFIG_HI / 4] = seeded_hi;

    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");

    // The entry capture reflects the firmware's window verbatim.
    let entry = rc.entry_inbound_window();
    assert_eq!(entry.rc_bar2_lo, seeded_lo);
    assert_eq!(entry.rc_bar2_hi, seeded_hi);

    let m = rc.regs();
    // The window is untouched: still the firmware's value, and bring-up
    // recorded no write to either `RC_BAR2` register.
    assert_eq!(m.mem[regs::MISC_RC_BAR2_CONFIG_LO / 4], seeded_lo);
    assert_eq!(m.mem[regs::MISC_RC_BAR2_CONFIG_HI / 4], seeded_hi);
    assert!(m.write_index(regs::MISC_RC_BAR2_CONFIG_LO).is_none());
    assert!(m.write_index(regs::MISC_RC_BAR2_CONFIG_HI).is_none());
    // The unused inbound windows are still disabled (those are not the
    // system-memory viewport `VideoCore` assumes).
    assert_eq!(
        m.mem[regs::MISC_RC_BAR1_CONFIG_LO / 4] & regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        0
    );
    assert_eq!(
        m.mem[regs::MISC_RC_BAR3_CONFIG_LO / 4] & regs::RC_BAR_CONFIG_LO_SIZE_MASK,
        0
    );
}

#[test]
fn bring_up_names_the_downstream_bus_so_config_is_forwarded() {
    // Without this the root port forwards nothing and the VL805 on bus 1
    // is invisible to enumeration (the observed metal symptom: only the
    // bridge at BDF 0 answers configuration reads).
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    let bus_reg = m.mem[regs::RC_CFG_PRIMARY_BUS / 4];
    let primary = bus_reg & regs::PRIMARY_BUS_PRIMARY_MASK;
    let secondary = (bus_reg & regs::PRIMARY_BUS_SECONDARY_MASK)
        >> regs::PRIMARY_BUS_SECONDARY_MASK.trailing_zeros();
    let subordinate = (bus_reg & regs::PRIMARY_BUS_SUBORDINATE_MASK)
        >> regs::PRIMARY_BUS_SUBORDINATE_MASK.trailing_zeros();
    assert_eq!(primary, 0);
    assert_eq!(secondary, u32::from(regs::RC_SECONDARY_BUS));
    assert_eq!(subordinate, u32::from(regs::RC_SUBORDINATE_BUS));
    // The register is actually written (the bring-up touched it).
    assert!(m.write_index(regs::RC_CFG_PRIMARY_BUS).is_some());
}

#[test]
fn bring_up_opens_the_bridge_memory_window_so_bar_reads_are_forwarded() {
    // Without this the root port forwards no memory transaction downstream
    // and the VL805's BAR reads return the `0xdead_dead` abort poison even
    // though config reads succeed (the observed metal symptom).
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    let win = m.mem[regs::RC_CFG_MEMORY_BASE_LIMIT / 4];
    let base = (win & regs::MEMORY_BASE_LIMIT_BASE_MASK)
        >> regs::MEMORY_BASE_LIMIT_BASE_MASK.trailing_zeros();
    let limit = (win & regs::MEMORY_BASE_LIMIT_LIMIT_MASK)
        >> regs::MEMORY_BASE_LIMIT_LIMIT_MASK.trailing_zeros();
    // Base/limit hold address bits [31:20]; the window must cover the
    // outbound PCIe range [0xc000_0000, 0x1_0000_0000).
    let base_addr = u64::from(base) << regs::MEMORY_WINDOW_GRANULE_SHIFT;
    let limit_addr = (u64::from(limit) << regs::MEMORY_WINDOW_GRANULE_SHIFT)
        | ((1 << regs::MEMORY_WINDOW_GRANULE_SHIFT) - 1);
    assert_eq!(base_addr, PI_WINDOWS.outbound_pcie_base);
    assert_eq!(
        limit_addr,
        PI_WINDOWS.outbound_pcie_base + PI_WINDOWS.outbound_size - 1
    );
    // The BAR base the kernel assigns (the lowest address in the window)
    // is inside the forwarded window.
    assert!(
        base_addr <= PI_WINDOWS.outbound_pcie_base && PI_WINDOWS.outbound_pcie_base <= limit_addr
    );
    assert!(m.write_index(regs::RC_CFG_MEMORY_BASE_LIMIT).is_some());
}

#[test]
fn bring_up_enables_memory_space_and_bus_master_on_the_bridge() {
    // Without this the root port forwards no memory transaction downstream
    // even with the bus numbers and Memory Base/Limit window named, so the
    // VL805's BAR reads return the `0xdead_dead` abort poison while config
    // reads succeed (the observed metal symptom: the bridge's `0x04` reads
    // back `0x0000`).
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    let command = m.mem[regs::RC_CFG_COMMAND / 4];
    assert_ne!(command & regs::COMMAND_MEMORY_SPACE_MASK, 0);
    assert_ne!(command & regs::COMMAND_BUS_MASTER_MASK, 0);
    // The write-1-to-clear Status word is left at 0 (no latched status bit
    // is disturbed by the read-modify-write).
    assert_eq!(command & regs::COMMAND_STATUS_MASK, 0);
    assert!(m.write_index(regs::RC_CFG_COMMAND).is_some());

    // The enable is issued *after* the link is trained — i.e. after the
    // final `PERST#`-deassert write to RGR1_SW_INIT_1 — as a PCI-PCI
    // bridge enable does (the device is enabled once the link is up). Writing
    // Memory Space Enable while `PERST#` was still asserted did not stick on
    // the integrated RC (the metal `4110` read-back showed `0x0000`).
    let command_at = m
        .write_index(regs::RC_CFG_COMMAND)
        .expect("command write recorded");
    let perst_deassert_at = m
        .writes
        .iter()
        .rposition(|(offset, _)| *offset == regs::RGR1_SW_INIT_1)
        .expect("PERST# deassert write recorded");
    assert!(
        command_at > perst_deassert_at,
        "bridge command enabled at write #{command_at} before the PERST# \
         deassert at write #{perst_deassert_at}"
    );
}

#[test]
fn inbound_window_readback_reports_the_programmed_viewport() {
    // The read-back is the metal diagnostic for the honoured-but-no-op
    // VideoCore VL805-firmware reload: that load runs over an inbound DMA
    // window, so a mismatch between our inbound translation and what
    // VideoCore expects makes the reload a no-op (raspberrypi/firmware
    // #1617). The read-back must surface exactly what bring-up wrote to
    // the active inbound viewport (`RC_BAR2`, encoded size in the low
    // field) and that the unused `RC_BAR1`/`RC_BAR3` windows are disabled.
    let regs0 = MockRegs::new(true, 1);
    let mut rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let rb = rc.inbound_window_readback();
    // Active inbound viewport: offset 0 (PI_WINDOWS.inbound_pcie_base),
    // size field 0x11 (4 GiB) in the low register.
    assert_eq!(rb.rc_bar2_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK, 0x11);
    assert_eq!(rb.rc_bar2_hi, high32(PI_WINDOWS.inbound_pcie_base));
    // The unused inbound windows read back disabled (size field cleared).
    assert_eq!(rb.rc_bar1_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK, 0);
    assert_eq!(rb.rc_bar3_lo & regs::RC_BAR_CONFIG_LO_SIZE_MASK, 0);
    // The read-back surfaces the programmed inbound SCB window size
    // (`SCB0_SIZE`), so a metal capture confirms the inbound decoder was
    // sized to match the viewport (0x11) rather than left undersized.
    let scb0 = (rb.misc_ctrl & regs::MISC_CTRL_SCB0_SIZE_MASK)
        >> regs::MISC_CTRL_SCB0_SIZE_MASK.trailing_zeros();
    assert_eq!(scb0, 0x11);
    // The link reads up for correlation.
    assert_ne!(rb.pcie_status & regs::PCIE_STATUS_DL_ACTIVE_MASK, 0);
}

#[test]
fn outbound_window_decodes_a_non_empty_range_covering_the_cpu_window() {
    // Regression for the metal `0xdead_dead` BAR master-abort: the BCM2711
    // *proprietary* outbound `MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT` register
    // holds the window **limit** in bits [31:20] and the **base** in bits
    // [15:4]. Defining the two field masks the wrong way round programs an
    // inverted (base-above-limit) window that decodes nothing, so every
    // CPU→PCIe memory access master-aborts (`0xdead_dead`) while config
    // reads — a different controller path — succeed. Decode the full CPU
    // base/limit with the *hardware* field positions (literal masks, not
    // the named constants, so a re-swap of the constants is still caught)
    // and assert the window is non-empty and covers the outbound CPU range.
    const HW_LIMIT_MASK: u32 = 0xfff0_0000; // limit low MiB bits: [31:20]
    const HW_BASE_MASK: u32 = 0x0000_fff0; // base low MiB bits: [15:4]

    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();

    let base_limit = m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_LIMIT / 4];
    let base_hi =
        m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI / 4] & regs::MEM_WIN0_BASE_HI_BASE_MASK;
    let limit_hi =
        m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_LIMIT_HI / 4] & regs::MEM_WIN0_LIMIT_HI_LIMIT_MASK;

    // Each low field is 12 bits wide; the rest of the MiB count is in the
    // companion *_HI register.
    let base_mb = (u64::from(base_hi) << 12)
        | u64::from((base_limit & HW_BASE_MASK) >> HW_BASE_MASK.trailing_zeros());
    let limit_mb = (u64::from(limit_hi) << 12)
        | u64::from((base_limit & HW_LIMIT_MASK) >> HW_LIMIT_MASK.trailing_zeros());

    let base_addr = base_mb << regs::MEMORY_WINDOW_GRANULE_SHIFT;
    let limit_addr = (limit_mb << regs::MEMORY_WINDOW_GRANULE_SHIFT)
        | ((1u64 << regs::MEMORY_WINDOW_GRANULE_SHIFT) - 1);

    // Non-empty: base must not sit above the limit (the inverted-window bug).
    assert!(
        base_addr <= limit_addr,
        "outbound window inverted: base {base_addr:#x} > limit {limit_addr:#x}"
    );
    // Covers exactly the CPU-side outbound range, so a CPU access at
    // `outbound_cpu_base` translates to the VL805's BAR rather than aborting.
    assert_eq!(base_addr, PI_WINDOWS.outbound_cpu_base);
    assert_eq!(
        limit_addr,
        PI_WINDOWS.outbound_cpu_base + PI_WINDOWS.outbound_size - 1
    );
}

#[test]
fn bring_up_releases_sw_init_before_touching_misc_and_skips_the_serdes_toggle() {
    let regs0 = MockRegs::new(true, 1);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    // The always-accessible RGR1 bridge `sw_init` reset (0x9210) is
    // released FIRST, before the first MISC-block write (MISC_MISC_CTRL),
    // so the controller core is out of reset and the MISC access does not
    // stall on the SoC bus completion timeout (the ~10.8 s metal
    // master-abort). This follows the BCM2711 PCIe bring-up sequence.
    let first_rgr1 = m.write_index(regs::RGR1_SW_INIT_1).expect("reset write");
    let first_misc = m
        .write_index(regs::MISC_MISC_CTRL)
        .expect("misc ctrl write");
    assert!(first_rgr1 < first_misc);
    // The gentlest no-touch-probe bring-up never toggles the SerDes IDDQ
    // (MISC_HARD_PCIE_HARD_DEBUG): the SerDes is already powered by the
    // previous boot stage, and re-toggling it could drop the resident
    // VL805 firmware.
    assert_eq!(m.write_index(regs::MISC_HARD_PCIE_HARD_DEBUG), None);
}

#[test]
fn reset_releases_sw_init_without_re_asserting_a_fundamental_reset() {
    // Simulate the `start4.elf` handoff: the previous boot stage leaves the
    // bridge `sw_init` reset AND `PERST#` asserted (metal
    // `entry_rgr1_sw_init = 0x3`), with the VL805 firmware already loaded
    // over the power-on link. The gentlest no-touch-probe bring-up must NOT
    // re-assert a fundamental reset (which can drop that firmware): it only
    // *releases* the bridge `sw_init`, and `train_link` deasserts the
    // already-asserted `PERST#` — the single firmware-(re)load edge.
    let swinit = regs::RGR1_SW_INIT_1_INIT_GENERIC_MASK;
    let perst = regs::RGR1_SW_INIT_1_PERST_MASK;
    let mut regs0 = MockRegs::new(true, 1);
    regs0.mem[regs::RGR1_SW_INIT_1 / 4] = swinit | perst;
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    let rgr1_writes: Vec<u32> = m
        .writes
        .iter()
        .filter(|(offset, _)| *offset == regs::RGR1_SW_INIT_1)
        .map(|(_, value)| *value)
        .collect();
    // The bridge `sw_init` reset is only RELEASED, never re-asserted: no
    // RGR1 write sets the INIT_GENERIC bit (a re-assert would be a fresh
    // fundamental reset that can drop the resident VL805 firmware).
    assert!(
        rgr1_writes.iter().all(|v| v & swinit == 0),
        "bring-up re-asserted the bridge sw_init reset (a fundamental reset that can drop the VL805 firmware)"
    );
    // `train_link` deasserts the already-asserted `PERST#` as the final
    // RGR1 write, producing the single firmware-(re)load edge.
    assert_eq!(
        rgr1_writes.last().map(|v| v & perst),
        Some(0),
        "PERST# left asserted; the deassert edge was never produced"
    );
}

#[test]
fn bring_up_fails_closed_when_the_link_never_trains() {
    // Root port, but the link never asserts within a 3-poll budget.
    let regs0 = MockRegs::new(true, u32::MAX);
    assert_eq!(
        BrcmPcieRc::open_with_polls(regs0, &NoDelay, &PI_WINDOWS, 3).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn bring_up_fails_closed_when_not_a_root_port() {
    // The controller never reports the root-port role: refuse before
    // advertising bridge config or training the link.
    let regs0 = MockRegs::new(false, 0);
    assert_eq!(
        BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn bring_up_timing_reports_each_phase_and_the_poll_count() {
    // The link is down at entry and trains after a couple of polls, so the
    // full reset → config → link-wait path runs. The phase marks read the
    // `SteppingDelay` clock in order, so each phase span is the positive
    // step and the poll loop records how many polls it took — the
    // per-phase split a metal capture needs to localise a multi-second
    // bring-up (`AGENTS.md` §15.7 / §23.4).
    let regs0 = MockRegs::new(true, 4);
    let delay = SteppingDelay::new(1_000);
    let rc = BrcmPcieRc::open_with_polls(regs0, &delay, &PI_WINDOWS, 20).expect("link trains");
    let t = rc.bring_up_timing();
    assert_eq!(t.reset_swinit_us, 1_000, "reset bridge sw_init sub-span");
    assert_eq!(
        t.reset_settle_us, 1_000,
        "reset post-de-reset MISC settle sub-span"
    );
    assert_eq!(t.config_us, 1_000, "config phase span");
    assert_eq!(t.linkwait_us, 1_000, "link-wait phase span");
    // The link was down at entry, so the bounded poll loop ran at least
    // once before the mock asserted link-up.
    assert!(t.link_polls >= 1, "polls = {}", t.link_polls);
    // The entry RGR1 sample is taken before the reset writes the register,
    // so it reflects the mock's reset state (0 — no PERST# pre-asserted),
    // proving the field is sampled at entry rather than a later write. On
    // metal a set `RGR1_SW_INIT_1_PERST_MASK` bit here would surface that
    // the previous boot stage already dropped the VL805 firmware
    // (`AGENTS.md` §15.7).
    assert_eq!(
        t.entry_rgr1_sw_init & regs::RGR1_SW_INIT_1_PERST_MASK,
        0,
        "entry RGR1 sampled before the reset writes it"
    );
}

#[test]
fn bring_up_timing_records_zero_polls_when_the_link_is_up_on_first_check() {
    // The link is reported up by the first link-wait poll (`link_after ==
    // 0`), so the bounded poll loop breaks immediately with no polls
    // recorded; the phase marks are still read, so the link-wait span
    // remains the clock step (the PERST-deassert settle).
    let regs0 = MockRegs::new(true, 0);
    let delay = SteppingDelay::new(1_000);
    let rc = BrcmPcieRc::open(regs0, &delay, &PI_WINDOWS).expect("link trains");
    let t = rc.bring_up_timing();
    assert_eq!(t.link_polls, 0);
    assert_eq!(t.linkwait_us, 1_000);
}

// --- Wiring (driver-host composition) -------------------------------------

fn leak_aligned(len: usize) -> NonNull<u8> {
    let words = len.div_ceil(4).max(1);
    let buf: Box<[u32]> = vec![0u32; words].into_boxed_slice();
    NonNull::new(Box::leak(buf).as_mut_ptr().cast::<u8>()).expect("non-null leaked buffer")
}

struct MockMapper {
    grant: bool,
}

impl MmioMapper for MockMapper {
    fn map_window(&self, phys_base: u64, len: usize) -> Result<RegisterWindow, MmioMapError> {
        if !self.grant {
            return Err(MmioMapError::CapabilityMissing);
        }
        let base = leak_aligned(len);
        // SAFETY: `base` covers `len` bytes, is 4-byte aligned, lives
        // for the whole test process (leaked), and is not aliased.
        Ok(unsafe { RegisterWindow::from_mapping(phys_base, base, len) })
    }
}

struct MockHost {
    mmio_map: bool,
    mapper: Option<MockMapper>,
}

impl DriverHost for MockHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        match cap {
            CapabilityId::DRV_LOAD => true,
            CapabilityId::MMIO_MAP => self.mmio_map,
            _ => false,
        }
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        self.mapper.as_ref().map(|m| m as &dyn MmioMapper)
    }
}

#[test]
fn open_discovered_requires_the_mmio_capability() {
    let host = MockHost {
        mmio_map: false,
        mapper: Some(MockMapper { grant: true }),
    };
    assert_eq!(
        wiring::open_discovered(&host, 0xfd50_0000, &PI_WINDOWS, &NoDelay).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn open_discovered_requires_a_mapper() {
    let host = MockHost {
        mmio_map: true,
        mapper: None,
    };
    assert_eq!(
        wiring::open_discovered(&host, 0xfd50_0000, &PI_WINDOWS, &NoDelay).err(),
        Some(DriverError::Unsupported)
    );
}

#[test]
fn open_discovered_reaches_the_root_port_check_over_an_inert_window() {
    // The mapper backs the window with zeroed heap, so the root-port
    // role bit reads 0 and the bring-up fails closed at that check —
    // exactly the on-metal boundary (a real controller reports the
    // role bit set). The composition reached the engine.
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
    };
    assert_eq!(
        wiring::open_discovered(&host, 0xfd50_0000, &PI_WINDOWS, &NoDelay).err(),
        Some(DriverError::DeviceFault)
    );
}

#[test]
fn bind_table_matches_the_pi4_pcie_node() {
    use rustos_abi::HwMatchKey;
    // Exactly one key: the BCM2711 PCIe root-complex `compatible`, at the
    // declared priority. It matches a discovered node carrying that key
    // and nothing else (e.g. the EMMC2 node).
    assert_eq!(BIND_KEYS.len(), 1);
    assert_eq!(BIND_KEYS[0].priority, BIND_PRIORITY);
    let pcie = HwMatchKey::compatible(b"brcm,bcm2711-pcie").expect("fits");
    assert!(BIND_KEYS[0].key.matches(&pcie));
    let emmc = HwMatchKey::compatible(b"brcm,bcm2711-emmc2").expect("fits");
    assert!(!BIND_KEYS[0].key.matches(&emmc));
}

// --- Discovered-node parsing & autonomous floor entry ---------------------

use rustos_abi::{HwDeviceClass, HwNode, HwResource};

/// The Pi 4 discovered values: controller `reg`, inbound `dma-ranges`
/// (PCIe base 0, 3 GiB), outbound `ranges` (CPU `0x6_0000_0000` → PCIe
/// `0xc000_0000`, 1 GiB).
const REGS_PHYS: u64 = 0xfd50_0000;
const APERTURE_TOP: u64 = 0xc000_0000;
const OUTBOUND_CPU: u64 = 0x6_0000_0000;
const OUTBOUND_PCIE: u64 = 0xc000_0000;
const OUTBOUND_SIZE: u64 = 0x4000_0000;

fn pcie_node() -> HwNode {
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
        .unwrap();
    node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
        .unwrap();
    node.push_resource(HwResource::bus_window(
        OUTBOUND_CPU,
        OUTBOUND_SIZE,
        OUTBOUND_PCIE,
    ))
    .unwrap();
    node
}

#[test]
fn bringup_inputs_are_assembled_from_the_node() {
    let bringup = wiring::pcie_bringup_from_node(&pcie_node()).expect("all resources present");
    assert_eq!(bringup.regs_phys, REGS_PHYS);
    assert_eq!(bringup.windows.inbound_pcie_base, 0);
    assert_eq!(bringup.windows.inbound_size, APERTURE_TOP);
    assert_eq!(bringup.windows.outbound_cpu_base, OUTBOUND_CPU);
    assert_eq!(bringup.windows.outbound_pcie_base, OUTBOUND_PCIE);
    assert_eq!(bringup.windows.outbound_size, OUTBOUND_SIZE);
}

#[test]
fn bringup_carries_a_nonzero_inbound_pcie_base() {
    // A viewport not anchored at PCIe address 0: the translation rides
    // the DMA resource's far-side base, distinct from the CPU top.
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
        .unwrap();
    node.push_resource(HwResource::dma_translated(
        APERTURE_TOP,
        APERTURE_TOP,
        0x4000_0000,
    ))
    .unwrap();
    node.push_resource(HwResource::bus_window(
        OUTBOUND_CPU,
        OUTBOUND_SIZE,
        OUTBOUND_PCIE,
    ))
    .unwrap();
    let bringup = wiring::pcie_bringup_from_node(&node).expect("resources present");
    assert_eq!(bringup.windows.inbound_pcie_base, 0x4000_0000);
    assert_eq!(bringup.windows.inbound_size, APERTURE_TOP);
}

#[test]
fn bringup_fails_closed_on_each_missing_resource() {
    // No controller register window.
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
        .unwrap();
    node.push_resource(HwResource::bus_window(
        OUTBOUND_CPU,
        OUTBOUND_SIZE,
        OUTBOUND_PCIE,
    ))
    .unwrap();
    assert_eq!(
        wiring::pcie_bringup_from_node(&node),
        Err(wiring::BringupError::NoControllerWindow)
    );

    // No inbound aperture.
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
        .unwrap();
    node.push_resource(HwResource::bus_window(
        OUTBOUND_CPU,
        OUTBOUND_SIZE,
        OUTBOUND_PCIE,
    ))
    .unwrap();
    assert_eq!(
        wiring::pcie_bringup_from_node(&node),
        Err(wiring::BringupError::NoInboundAperture)
    );

    // No outbound window.
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::mmio(REGS_PHYS, 0x9310))
        .unwrap();
    node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
        .unwrap();
    assert_eq!(
        wiring::pcie_bringup_from_node(&node),
        Err(wiring::BringupError::NoOutboundWindow)
    );
}

#[test]
fn missing_resource_maps_to_a_fail_closed_not_found() {
    // The autonomous entry surfaces an incomplete discovered node as a
    // bus-neutral fail-closed `NotFound`, never an invented window.
    assert_eq!(
        wiring::BringupError::NoControllerWindow.as_driver_error(),
        DriverError::NotFound
    );
}

#[test]
fn bring_up_from_node_requires_the_mmio_capability() {
    // A complete node still cannot be brought up without the capability:
    // the autonomous entry checks `CAP_MMIO_MAP` before mapping (§5.4).
    let host = MockHost {
        mmio_map: false,
        mapper: Some(MockMapper { grant: true }),
    };
    assert_eq!(
        wiring::bring_up_from_node(&host, &pcie_node(), &NoDelay).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn bring_up_from_node_fails_closed_on_an_incomplete_node() {
    // A node missing the controller window is refused before any mapping,
    // as `NotFound` — the capability is granted, so this proves the
    // node-parse gate, not the capability gate.
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
    };
    let mut node = HwNode::new(9, 1, HwDeviceClass::Bus);
    node.push_resource(HwResource::dma_translated(APERTURE_TOP, APERTURE_TOP, 0))
        .unwrap();
    node.push_resource(HwResource::bus_window(
        OUTBOUND_CPU,
        OUTBOUND_SIZE,
        OUTBOUND_PCIE,
    ))
    .unwrap();
    assert_eq!(
        wiring::bring_up_from_node(&host, &node, &NoDelay).err(),
        Some(DriverError::NotFound)
    );
}

#[test]
fn bring_up_from_node_reaches_the_root_port_check_over_a_mapped_window() {
    // A complete node with the capability granted maps the window and
    // reaches the engine's root-port check, which fails closed over the
    // zeroed mock window (the on-metal boundary; a real controller
    // reports the role bit set).
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
    };
    assert_eq!(
        wiring::bring_up_from_node(&host, &pcie_node(), &NoDelay).err(),
        Some(DriverError::DeviceFault)
    );
}

// --- VL805 enumerate-and-publish composition ------------------------------
//
// `publish_usb_function` is the post-link half of the user-space bus driver
// (`emit_vl805_node`): QEMU models no Pi PCIe link timing (`AGENTS.md`
// §0.4), so the link training itself is metal-only, but this half — find the
// USB function, assign/map its BAR, translate it to CPU-physical, and publish
// the node — is host-testable against a mock bus.

use core::cell::RefCell;
use rustos_abi::driver::bus::{Bus, BusDevice};
use rustos_abi::driver::pci::PciBus;
use rustos_abi::{HwMatchKey, HwResourceKind};
use rustos_pci::USB_CONTROLLER_CLASS;
use rustos_usb::XHCI_DMA_BYTES;

/// Bus-local address of the lone USB function the [`StubPciBus`] reports.
const USB_BDF: u64 = 0x0001_0000;
/// BAR window length the stub's `map_bar_window` reports.
const STUB_BAR_LEN: usize = 0x1000;

/// A mock [`PciBus`] for the publish composition: it reports a single
/// USB-class function, records the assign/enable/map call order, maps its
/// BAR at a configurable PCIe-bus base, and describes it as a VL805 node.
struct StubPciBus {
    /// Whether the bus carries a USB-class function at all.
    has_usb: bool,
    /// The PCIe-bus base `map_bar_window` reports for the BAR (so a test can
    /// place it inside or outside the outbound window).
    bar_bus_base: u64,
    /// The assign/enable/map call sequence, in order.
    calls: RefCell<Vec<&'static str>>,
}

impl StubPciBus {
    fn new(bar_bus_base: u64) -> Self {
        Self {
            has_usb: true,
            bar_bus_base,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl Bus for StubPciBus {
    fn enumerate(&self, out: &mut [BusDevice]) -> Result<usize, DriverError> {
        if !self.has_usb || out.is_empty() {
            return Ok(0);
        }
        out[0] = BusDevice {
            vendor: 0x1106,
            device: 0x3483,
            class: USB_CONTROLLER_CLASS,
            reserved0: 0,
            address: USB_BDF,
        };
        Ok(1)
    }
}

impl PciBus for StubPciBus {
    fn map_bar_window(
        &self,
        _bdf: u64,
        _bar_index: u8,
        mapper: &dyn MmioMapper,
    ) -> Result<RegisterWindow, DriverError> {
        self.calls.borrow_mut().push("map");
        mapper
            .map_window(self.bar_bus_base, STUB_BAR_LEN)
            .map_err(MmioMapError::as_driver_error)
    }

    fn enable_bus_master(&self, _bdf: u64) -> Result<(), DriverError> {
        self.calls.borrow_mut().push("enable");
        Ok(())
    }

    fn assign_bar(
        &self,
        _bdf: u64,
        _bar_index: u8,
        window_base: u64,
        _window_size: u64,
    ) -> Result<u64, DriverError> {
        self.calls.borrow_mut().push("assign");
        Ok(window_base)
    }

    fn read_config(&self, _bdf: u64, _offset: u16) -> Result<u32, DriverError> {
        Ok(0)
    }

    fn describe_function(&self, _bdf: u64) -> Result<HwNode, DriverError> {
        // Identity is unassigned (the kernel sets it on publish, D5b.2a); the
        // node carries the VL805's `vendor:device:class` match key.
        let mut node = HwNode::new(0, rustos_abi::hwtree::HW_NODE_ROOT, HwDeviceClass::Bus);
        node.push_match_key(HwMatchKey::pci(0x1106, 0x3483, 0x0C_03_30))
            .map_err(|_| DriverError::DeviceFault)?;
        Ok(node)
    }
}

/// A [`DriverHost`] double that maps BARs through a granting [`MockMapper`]
/// and captures the node published through [`DriverHost::emit_node`].
struct RecordingHost {
    emit_ok: bool,
    mapper: MockMapper,
    emitted: RefCell<Option<HwNode>>,
}

impl RecordingHost {
    fn new(emit_ok: bool) -> Self {
        Self {
            emit_ok,
            mapper: MockMapper { grant: true },
            emitted: RefCell::new(None),
        }
    }
}

impl DriverHost for RecordingHost {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        matches!(cap, CapabilityId::MMIO_MAP | CapabilityId::HW_EMIT)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(&self.mapper)
    }

    fn emit_node(&self, node: HwNode) -> Result<(), DriverError> {
        if !self.emit_ok {
            return Err(DriverError::PermissionDenied);
        }
        *self.emitted.borrow_mut() = Some(node);
        Ok(())
    }
}

#[test]
fn publish_usb_function_emits_the_translated_bar_and_dma_grants() {
    // The BAR is assigned the bottom of the Pi 4 outbound PCIe window, so its
    // CPU-physical address is the outbound CPU base.
    let bus = StubPciBus::new(PI_WINDOWS.outbound_pcie_base);
    let host = RecordingHost::new(true);
    let node = wiring::publish_usb_function(&host, &bus, &PI_WINDOWS).expect("publishes");

    // The shared `lib/pci` primitive drove assign → enable → map in order.
    assert_eq!(bus.calls.borrow().as_slice(), &["assign", "enable", "map"]);

    // The published node carries the function's match key plus exactly two
    // grant requests: the BAR as a CPU-physical `Mmio` window (inside the
    // outbound CPU window, so the kernel's BusWindow→Mmio coverage admits it)
    // and the inbound DMA aperture sized for the xHCI working set.
    let emitted = host.emitted.borrow();
    let emitted = emitted.as_ref().expect("a node was emitted");
    assert_eq!(emitted.match_keys().len(), 1);
    let mmio = emitted
        .resources()
        .iter()
        .find(|r| r.kind() == Some(HwResourceKind::Mmio))
        .expect("an Mmio BAR grant");
    assert_eq!(mmio.base(), PI_WINDOWS.outbound_cpu_base);
    assert_eq!(mmio.length(), STUB_BAR_LEN as u64);
    let dma = emitted
        .resources()
        .iter()
        .find(|r| r.kind() == Some(HwResourceKind::Dma))
        .expect("a Dma constraint grant");
    assert_eq!(
        dma.base(),
        PI_WINDOWS.inbound_pcie_base + PI_WINDOWS.inbound_size
    );
    assert_eq!(dma.length(), XHCI_DMA_BYTES as u64);
    // The returned node equals the published one (the kernel owns identity).
    assert_eq!(*emitted, node);
}

#[test]
fn publish_usb_function_without_a_usb_function_fails_closed_not_found() {
    let mut bus = StubPciBus::new(PI_WINDOWS.outbound_pcie_base);
    bus.has_usb = false;
    let host = RecordingHost::new(true);
    assert_eq!(
        wiring::publish_usb_function(&host, &bus, &PI_WINDOWS).err(),
        Some(DriverError::NotFound)
    );
    assert!(host.emitted.borrow().is_none());
}

#[test]
fn publish_usb_function_fails_closed_when_the_bar_is_outside_the_outbound_window() {
    // A BAR below the outbound PCIe base has no CPU-physical translation in
    // the bridge window, so the publish is refused rather than emitting a
    // grant the kernel could not cover (`AGENTS.md` §5.4).
    let bus = StubPciBus::new(PI_WINDOWS.outbound_pcie_base - 0x1000);
    let host = RecordingHost::new(true);
    assert_eq!(
        wiring::publish_usb_function(&host, &bus, &PI_WINDOWS).err(),
        Some(DriverError::OutOfRange)
    );
    assert!(host.emitted.borrow().is_none());
}

#[test]
fn publish_usb_function_propagates_a_refused_emit() {
    // A host that refuses the node publish (no `CAP_HW_EMIT`, or the node
    // requests an uncovered resource) surfaces the refusal — fail closed.
    let bus = StubPciBus::new(PI_WINDOWS.outbound_pcie_base);
    let host = RecordingHost::new(false);
    assert_eq!(
        wiring::publish_usb_function(&host, &bus, &PI_WINDOWS).err(),
        Some(DriverError::PermissionDenied)
    );
}

#[test]
fn emit_vl805_node_reaches_the_link_bringup_over_a_mapped_window() {
    // `emit_vl805_node` first trains the link via `open_discovered`; over the
    // inert zeroed mock window the root-port check fails closed with
    // DeviceFault — the on-metal boundary. That it reached this proves the
    // composition mapped the controller window and drove the engine.
    let host = MockHost {
        mmio_map: true,
        mapper: Some(MockMapper { grant: true }),
    };
    let bringup = wiring::pcie_bringup_from_node(&pcie_node()).expect("complete node");
    assert_eq!(
        wiring::emit_vl805_node(&host, &bringup, &NoDelay).err(),
        Some(DriverError::DeviceFault)
    );
}
