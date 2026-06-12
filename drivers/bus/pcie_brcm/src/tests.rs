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
struct NoDelay;
impl Delay for NoDelay {
    fn delay_us(&self, _us: u32) {}
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
    // holds the rest (6).
    assert_eq!(
        m.mem[regs::MISC_CPU_2_PCIE_MEM_WIN0_BASE_HI / 4] & regs::MEM_WIN0_BASE_HI_BASE_MASK,
        6
    );
}

#[test]
fn bring_up_resets_and_asserts_perst_before_powering_the_serdes() {
    let regs0 = MockRegs::new(true, 0);
    let rc = BrcmPcieRc::open(regs0, &NoDelay, &PI_WINDOWS).expect("link trains");
    let m = rc.regs();
    // The first writes touch the reset register (bridge held + PERST
    // asserted) and they precede the first SerDes (HARD_DEBUG) write.
    let first_swinit = m.write_index(regs::RGR1_SW_INIT_1).expect("reset write");
    let first_serdes = m
        .write_index(regs::MISC_HARD_PCIE_HARD_DEBUG)
        .expect("serdes write");
    assert!(first_swinit < first_serdes);
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
