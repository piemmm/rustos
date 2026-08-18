//! The vesa-display scenario driven on the `BootCompleted` edge.
//!
//! Programs QEMU's `ramfb` (over the `fw_cfg` I/O-port DMA interface) to
//! scan out from a static guest-RAM surface, synthesises the
//! bootloader-captured VBE `ModeInfoBlock` describing that surface, then
//! loads the signed vesa display `.rxe` and drives it through `load ->
//! use -> unload -> reload`, reading the presented pixels back through
//! the capability-gated [`KernelMmioMapper`] to prove they reach the
//! scan-out surface QEMU consumes.

extern crate alloc;

use alloc::vec::Vec;
use core::ptr;

use tairix_abi::driver::display::Display;
use tairix_abi::{CapabilityId, DriverHost, DriverKind, Errno, MmioMapper};
use tairix_arch_x86_64::qemu_exit;
use tairix_caps::CapabilitySet;
use tairix_crypto::Ed25519PublicKey;
use tairix_drv_display_vesa::{
    register as vesa_register, VesaFramebuffer, VBE_MODE_INFO_BLOCK_LEN,
};
use tairix_drvhost::{
    DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
};
use tairix_fwcfg::{FwCfg, RamfbConfig, DRM_FORMAT_XRGB8888};
use tairix_kernel::SERIAL_SINK;
use tairix_kernel_mem::{AddressSpace, DirectPhysMap, HostPageTable, MmioMap, VirtAddr};
use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::KernelMmioMapper;
use tairix_log::{Event, EventId, Level, Sink};

use super::ioport::IoPortDma;
use crate::fixture::{SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY, VESA_IMAGE};

// --- Framebuffer geometry --------------------------------------------

/// Surface width in pixels.
const WIDTH: u32 = 64;
/// Surface height in pixels.
const HEIGHT: u32 = 64;
/// Bytes per pixel (32-bit colour).
const BPP: u32 = 4;
/// Scanline stride in bytes (no padding).
const STRIDE: u32 = WIDTH * BPP;
/// Total surface size in bytes.
const FB_BYTES: usize = (STRIDE * HEIGHT) as usize;

// --- VBE ModeInfoBlock field offsets (VBE 3.0 §4.3.2) ----------------

/// Byte offsets of the `ModeInfoBlock` fields the vesa driver reads.
mod vbe {
    /// `ModeAttributes` (u16).
    pub const MODE_ATTRIBUTES: usize = 0x00;
    /// `BytesPerScanLine` (u16).
    pub const BYTES_PER_SCAN_LINE: usize = 0x10;
    /// `XResolution` (u16).
    pub const X_RESOLUTION: usize = 0x12;
    /// `YResolution` (u16).
    pub const Y_RESOLUTION: usize = 0x14;
    /// `BitsPerPixel` (u8).
    pub const BITS_PER_PIXEL: usize = 0x19;
    /// `MemoryModel` (u8).
    pub const MEMORY_MODEL: usize = 0x1B;
    /// `RedMaskSize` (u8).
    pub const RED_MASK_SIZE: usize = 0x1F;
    /// `RedFieldPosition` (u8).
    pub const RED_FIELD_POSITION: usize = 0x20;
    /// `GreenMaskSize` (u8).
    pub const GREEN_MASK_SIZE: usize = 0x21;
    /// `GreenFieldPosition` (u8).
    pub const GREEN_FIELD_POSITION: usize = 0x22;
    /// `BlueMaskSize` (u8).
    pub const BLUE_MASK_SIZE: usize = 0x23;
    /// `BlueFieldPosition` (u8).
    pub const BLUE_FIELD_POSITION: usize = 0x24;
    /// `PhysBasePtr` (u32).
    pub const PHYS_BASE_PTR: usize = 0x28;

    /// `ModeAttributes`: supported (bit 0) + linear framebuffer (bit 7).
    pub const ATTR_SUPPORTED_LINEAR: u16 = (1 << 0) | (1 << 7);
    /// `MemoryModel`: direct colour.
    pub const MODEL_DIRECT_COLOUR: u8 = 6;
    /// 32 bits per pixel.
    pub const BPP_32: u8 = 32;
    /// 8-bit channel mask.
    pub const MASK_8: u8 = 8;
    /// Red field position for the `Bgra8888` byte order (blue at byte 0).
    pub const RED_POS_BGRA: u8 = 16;
    /// Green field position for `Bgra8888`.
    pub const GREEN_POS_BGRA: u8 = 8;
    /// Blue field position for `Bgra8888`.
    pub const BLUE_POS_BGRA: u8 = 0;
}

// --- MMIO map sizing -------------------------------------------------

/// Bookkeeping virtual base of the register-window map (the windows are
/// reached through the identity map, so this only keys the slot bitmap).
const MMIO_VBASE: u64 = 0x6000_0000;
/// Capacity in pages of the register-window map: the surface is
/// `FB_BYTES` (4 pages) and the vertical mints three windows (two driver
/// loads + one verification), each bracketed by two guard pages, so 32
/// pages leaves comfortable headroom.
const MMIO_CAP_PAGES: usize = 32;

/// Upper bound of the boot identity map (the bottom 4 GiB); the kernel
/// image and the framebuffer surface both sit well inside it.
const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// Synthetic owner process id for the driver context.
const TASK: ProcessId = ProcessId(0x5E5A);

/// Milestone breadcrumb event id.
const MILESTONE_ID: EventId = EventId(9210);

// --- Static scan-out surface -----------------------------------------

/// Page-aligned wrapper so the surface meets the mapper's word-access
/// alignment contract and starts on a frame boundary.
#[repr(C, align(4096))]
struct Surface([u8; FB_BYTES]);

/// The `ramfb` scan-out surface, in guest RAM. QEMU maps it read-only
/// and scans out from it for the life of the guest, so it must outlive
/// the scenario — hence a `static` rather than a stack/heap buffer.
static mut FRAMEBUFFER: Surface = Surface([0u8; FB_BYTES]);

/// Physical base address of [`FRAMEBUFFER`].
///
/// `FRAMEBUFFER` is a higher-half kernel static (the kernel is linked at
/// `KERNEL_VMA_BASE + phys`, see `kernel/arch/x86_64/linker.ld`), so its
/// physical address — what the VBE `PhysBasePtr` must hold and what the
/// MMIO mapper translates through the low-4-GiB identity map — is its
/// virtual address minus the higher-half base.
fn framebuffer_phys() -> u64 {
    (ptr::addr_of!(FRAMEBUFFER) as u64) - tairix_arch_x86_64::paging::KERNEL_VMA_BASE
}

// --- Logging / failure -----------------------------------------------

/// Emit an info-level breadcrumb on the serial sink.
fn log(msg: &str) {
    SERIAL_SINK.write_event(&Event {
        level: Level::Info,
        id: MILESTONE_ID,
        message: msg,
        fields: &[],
    });
}

/// Log `msg` and flip QEMU to failure. Never returns.
fn fail(msg: &str) -> ! {
    log(msg);
    qemu_exit::exit_failure()
}

// --- Host plumbing ---------------------------------------------------

/// Image source returning the baked-in signed `.rxe` regardless of path.
struct BakedSource;

impl ImageSource for BakedSource {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(VESA_IMAGE);
        Ok(())
    }
}

/// Spawner registering every verified manifest in-process through the
/// vesa driver's `register` entry point.
struct ResolveVesa;

impl DriverSpawner for ResolveVesa {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<tairix_abi::DriverHandle, SpawnRegisterError> {
        vesa_register(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// Driver-host view used for `VesaFramebuffer::open`: grants
/// `CAP_MMIO_MAP` and exposes the real [`KernelMmioMapper`]. Distinct
/// from the [`Host`]-installed view, mirroring how the bus-driver
/// verticals separate the load gate from the map gate.
struct VesaHost<'a> {
    granted: CapabilitySet,
    mapper: &'a dyn MmioMapper,
}

impl DriverHost for VesaHost<'_> {
    fn has_capability(&self, cap: CapabilityId) -> bool {
        self.granted.contains(cap)
    }

    fn kind(&self) -> DriverKind {
        DriverKind::UserSpace
    }

    fn mmio_mapper(&self) -> Option<&dyn MmioMapper> {
        Some(self.mapper)
    }
}

// --- VBE ModeInfoBlock synthesis -------------------------------------

/// Build the 256-byte VBE `ModeInfoBlock` a bootloader's VBE mode query
/// (`0x4F01`) would have captured for the `Bgra8888` linear-framebuffer
/// surface at `phys_base`.
fn build_mode_info_block(phys_base: u32) -> [u8; VBE_MODE_INFO_BLOCK_LEN] {
    // VBE `XResolution` / `YResolution` / `BytesPerScanLine` are u16
    // fields; the vertical's geometry is fixed and fits, but convert
    // through `try_from` (never `as`) so an over-large value would land a
    // zero the driver rejects rather than a silently-truncated dimension.
    let width = u16::try_from(WIDTH).unwrap_or(0);
    let height = u16::try_from(HEIGHT).unwrap_or(0);
    let stride = u16::try_from(STRIDE).unwrap_or(0);

    let mut b = [0u8; VBE_MODE_INFO_BLOCK_LEN];
    b[vbe::MODE_ATTRIBUTES..vbe::MODE_ATTRIBUTES + 2]
        .copy_from_slice(&vbe::ATTR_SUPPORTED_LINEAR.to_le_bytes());
    b[vbe::BYTES_PER_SCAN_LINE..vbe::BYTES_PER_SCAN_LINE + 2]
        .copy_from_slice(&stride.to_le_bytes());
    b[vbe::X_RESOLUTION..vbe::X_RESOLUTION + 2].copy_from_slice(&width.to_le_bytes());
    b[vbe::Y_RESOLUTION..vbe::Y_RESOLUTION + 2].copy_from_slice(&height.to_le_bytes());
    b[vbe::BITS_PER_PIXEL] = vbe::BPP_32;
    b[vbe::MEMORY_MODEL] = vbe::MODEL_DIRECT_COLOUR;
    b[vbe::RED_MASK_SIZE] = vbe::MASK_8;
    b[vbe::RED_FIELD_POSITION] = vbe::RED_POS_BGRA;
    b[vbe::GREEN_MASK_SIZE] = vbe::MASK_8;
    b[vbe::GREEN_FIELD_POSITION] = vbe::GREEN_POS_BGRA;
    b[vbe::BLUE_MASK_SIZE] = vbe::MASK_8;
    b[vbe::BLUE_FIELD_POSITION] = vbe::BLUE_POS_BGRA;
    b[vbe::PHYS_BASE_PTR..vbe::PHYS_BASE_PTR + 4].copy_from_slice(&phys_base.to_le_bytes());
    b
}

// --- Frame patterns --------------------------------------------------

/// Deterministic test frame keyed by `salt` so the two phases write
/// distinguishable surfaces (a stale read cannot pass the second phase).
fn make_frame(salt: u8) -> Vec<u8> {
    (0..FB_BYTES)
        .map(|i| (u8::try_from(i & 0xFF).unwrap_or(0)).wrapping_mul(7) ^ salt)
        .collect()
}

/// Map a fresh window over the surface and confirm its first `FB_BYTES`
/// bytes equal `expected`. Never returns on mismatch.
fn verify_surface(mapper: &dyn MmioMapper, phys_base: u64, expected: &[u8]) {
    let Ok(window) = mapper.map_window(phys_base, FB_BYTES) else {
        fail("verify: map_window failed");
    };
    let mut off = 0;
    while off < FB_BYTES {
        let Ok(got) = window.read_u32(off) else {
            fail("verify: window read out of bounds");
        };
        let want = u32::from_le_bytes([
            expected[off],
            expected[off + 1],
            expected[off + 2],
            expected[off + 3],
        ]);
        if got != want {
            fail("verify: surface pixel mismatch");
        }
        off += 4;
    }
}

// --- Scenario entry --------------------------------------------------

/// Drive the whole vertical. Returns on success; the caller then signals
/// QEMU success. Every failure flips QEMU failure with a breadcrumb.
pub fn run() {
    log("vesa-qemu: scenario start");

    let phys64 = framebuffer_phys();
    let Ok(phys32) = u32::try_from(phys64) else {
        fail("vesa-qemu: surface above 4 GiB (VBE PhysBasePtr is 32-bit)");
    };

    // 1. Bring up fw_cfg and program ramfb (the framebuffer device).
    let fw = FwCfg::new(IoPortDma);
    let ramfb = RamfbConfig {
        phys_base: phys64,
        drm_format: DRM_FORMAT_XRGB8888,
        flags: 0,
        width: WIDTH,
        height: HEIGHT,
        stride: STRIDE,
    };
    if fw.program_ramfb(&ramfb).is_err() {
        fail("fw_cfg: ramfb programming failed");
    }
    log("vesa-qemu: ramfb programmed");

    // 2. Synthesise the bootloader-captured VBE ModeInfoBlock boot
    //    hand-off and drive the capability-gated driver lifecycle.
    let block = build_mode_info_block(phys32);
    drive_lifecycle(&block, phys64);
}

/// Build the capability-checked [`KernelMmioMapper`], load the signed
/// `.rxe` through the [`Host`], and drive `load -> use -> unload ->
/// reload` against the surface `block` describes, verifying the
/// presented pixels reach the scan-out memory after each `present`.
/// Every failure flips QEMU failure with a breadcrumb.
fn drive_lifecycle(block: &[u8], phys_base: u64) {
    // Capability-checked kernel MMIO mapper over the boot identity map.
    let mut grants = CapabilitySet::empty();
    grants.insert(CapabilityId::MMIO_MAP);
    grants.insert(CapabilityId::DRV_LOAD);
    let caller = TaskCapabilities::derive(TASK, UserId(0), grants, grants, &SERIAL_SINK);
    let phys = DirectPhysMap::identity(IDENTITY_LIMIT);
    let Ok(mut mmio) = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) else {
        fail("MMIO map construct");
    };
    let mapper = KernelMmioMapper::new(&mut mmio, &caller, &SERIAL_SINK);

    // Driver-host view for `VesaFramebuffer::open` (CAP_MMIO_MAP granted).
    let mut open_grants = CapabilitySet::empty();
    open_grants.insert(CapabilityId::MMIO_MAP);
    let vesa_host = VesaHost {
        granted: open_grants,
        mapper: &mapper,
    };

    // Load the signed `.rxe` through the driver host (the gate).
    let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
        fail("trust anchor decode");
    };
    let trusted = [pubkey];
    let mut load_caps = CapabilitySet::empty();
    load_caps.insert(CapabilityId::DRV_LOAD);
    let source = BakedSource;
    let spawner = ResolveVesa;
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        accepted_abi_version: tairix_abi::ABI_VERSION_CURRENT,
        source: &source,
        spawner: &spawner,
        sink: &SERIAL_SINK,
        virtio_host_factory: None,
        mmio_mapper: None,
    });
    let Ok(h1) = host.load("/System/Drivers/vesa.rxe", &load_caps) else {
        fail("signed .rxe load");
    };
    if host.loaded_count() != 1 || host.snapshot().first().map(|s| s.handle) != Some(h1) {
        fail("loaded state after load");
    }

    // use: open the surface and present a frame, then verify the pixels
    // reached the scan-out memory.
    present_and_verify(
        &vesa_host,
        &mapper,
        block,
        phys_base,
        &make_frame(0x00),
        "first",
    );
    log("vesa-qemu: first frame presented and verified");

    // unload -> reload through the host.
    let Ok(h2) = host.reload(h1, &load_caps) else {
        fail("signed .rxe reload");
    };
    if h2 == h1 || host.loaded_count() != 1 {
        fail("loaded state after reload");
    }

    // use again after reload: present a distinct frame and verify.
    present_and_verify(
        &vesa_host,
        &mapper,
        block,
        phys_base,
        &make_frame(0xA5),
        "reloaded",
    );
    log("vesa-qemu: reloaded frame presented and verified");

    // unload: tear the driver down cleanly.
    if host.unload(h2).is_err() || host.loaded_count() != 0 {
        fail("driver unload");
    }
    log("vesa-qemu: driver unloaded after device reuse");
}

/// Open the framebuffer through `vesa_host`, present `frame`, drop the
/// driver (the quiesce step), then confirm `frame` landed in the scan-out
/// surface via an independent window. `phase` names the step in any
/// failure breadcrumb.
fn present_and_verify(
    vesa_host: &VesaHost<'_>,
    mapper: &dyn MmioMapper,
    block: &[u8],
    phys_base: u64,
    frame: &[u8],
    phase: &str,
) {
    {
        let Ok(mut fb) = VesaFramebuffer::open(vesa_host, block) else {
            fail(phase_msg(phase, "VesaFramebuffer::open"));
        };
        if fb.present(frame).is_err() {
            fail(phase_msg(phase, "present"));
        }
        // `fb` drops here, releasing its window handle (the quiesce step).
    }
    verify_surface(mapper, phys_base, frame);
}

/// Pick a `&'static str` breadcrumb for `(phase, op)` without an
/// allocator (no `format!` on the fail path).
fn phase_msg(phase: &str, op: &str) -> &'static str {
    match (phase, op) {
        ("first", "VesaFramebuffer::open") => "VesaFramebuffer::open (first)",
        ("first", _) => "present (first)",
        (_, "VesaFramebuffer::open") => "VesaFramebuffer::open (reloaded)",
        _ => "present (reloaded)",
    }
}
