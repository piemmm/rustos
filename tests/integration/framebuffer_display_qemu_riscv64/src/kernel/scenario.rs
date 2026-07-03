//! The framebuffer-display scenario driven on the `BootCompleted` edge.
//!
//! Synthesises a `ramfb` framebuffer device, publishes its geometry as a
//! [`FramebufferConfig`] boot hand-off, then loads the signed
//! framebuffer display `.rxe` and drives it through `load -> use ->
//! unload -> reload`, reading the presented pixels back through the
//! capability-gated [`KernelMmioMapper`] to prove they reach the
//! scan-out surface QEMU consumes.

extern crate alloc;

use alloc::vec::Vec;
use core::ptr;

use rustos_abi::driver::display::{Display, DisplayFormat};
use rustos_abi::{CapabilityId, DriverHost, DriverKind, Errno, MmioMapper};
use rustos_arch_riscv64::{qemu_exit, SERIAL_SINK};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_display_framebuffer::{register as fb_register, Framebuffer, FramebufferConfig};
use rustos_drvhost::{
    DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
};
use rustos_fdt::Fdt;
use rustos_kernel_mem::{AddressSpace, DirectPhysMap, HostPageTable, MmioMap, VirtAddr};
use rustos_kernel_sec::captable::{TaskCapabilities, TaskId};
use rustos_kernel_sec::identity::UserId;
use rustos_kernel_virtio::KernelMmioMapper;
use rustos_log::{Event, EventId, Level, Sink};
use rustos_test_riscv64_boot::published_dtb;

use rustos_fwcfg::{FwCfg, MmioDma, RamfbConfig, DRM_FORMAT_XRGB8888};

use crate::fixture::{FB_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

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

// --- MMIO map sizing -------------------------------------------------

/// Bookkeeping virtual base of the register-window map (the windows are
/// reached through the identity map, so this only keys the slot bitmap).
const MMIO_VBASE: u64 = 0x6000_0000;
/// Capacity in pages of the register-window map: the surface is
/// `FB_BYTES` (4 pages) and the vertical mints three windows
/// (two driver loads + one verification), each bracketed by two guard
/// pages, so 32 pages leaves comfortable headroom.
const MMIO_CAP_PAGES: usize = 32;

/// Upper bound of the boot identity map (the bottom 4 GiB); the `virt`
/// board's RAM and the framebuffer surface both sit well inside it.
const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// Synthetic owner task id for the driver context.
const TASK: TaskId = TaskId(0xFB0);

/// Milestone breadcrumb event id.
const MILESTONE_ID: EventId = EventId(9200);

// --- Static scan-out surface -----------------------------------------

/// Page-aligned wrapper so the surface meets the mapper's word-access
/// alignment contract and starts on a frame boundary.
#[repr(C, align(4096))]
struct Surface([u8; FB_BYTES]);

/// The `ramfb` scan-out surface, in guest RAM. QEMU maps it read-only
/// and scans out from it for the life of the guest, so it must outlive
/// the scenario — hence a `static` rather than a stack/heap buffer.
static mut FRAMEBUFFER: Surface = Surface([0u8; FB_BYTES]);

/// Physical (identity) base address of [`FRAMEBUFFER`].
fn framebuffer_phys() -> u64 {
    ptr::addr_of!(FRAMEBUFFER) as u64
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
    qemu_exit::exit_failure(1)
}

// --- Host plumbing ---------------------------------------------------

/// Image source returning the baked-in signed `.rxe` regardless of path.
struct BakedSource;

impl ImageSource for BakedSource {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(FB_IMAGE);
        Ok(())
    }
}

/// Spawner registering every verified manifest in-process through the
/// framebuffer driver's `register` entry point.
struct ResolveFramebuffer;

impl DriverSpawner for ResolveFramebuffer {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<rustos_abi::DriverHandle, SpawnRegisterError> {
        fb_register(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// Driver-host view used for `Framebuffer::open`: grants `CAP_MMIO_MAP`
/// and exposes the real [`KernelMmioMapper`]. Distinct from the
/// [`Host`]-installed view, mirroring how the bus-driver verticals
/// separate the load gate from the map gate.
struct FramebufferHost<'a> {
    granted: CapabilitySet,
    mapper: &'a dyn MmioMapper,
}

impl DriverHost for FramebufferHost<'_> {
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
    // Compare word-by-word over the whole surface.
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
    log("framebuffer-qemu: scenario start");

    // 1. Parse the published device tree.
    let Some(dtb_ptr) = published_dtb() else {
        fail("no published DTB");
    };
    // SAFETY: `dtb_ptr` is the verbatim OpenSBI `a1` the boot pipeline
    // published; it addresses a valid flattened device tree that lives
    // for the life of the guest. The first 8 bytes carry the FDT header
    // whose `totalsize` bounds the blob.
    let dtb_len = unsafe {
        let header = dtb_ptr as *const u8;
        let bytes = core::slice::from_raw_parts(header, 8);
        u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize
    };
    // SAFETY: as above; `dtb_len` is the blob's self-described size.
    let dtb_bytes = unsafe { core::slice::from_raw_parts(dtb_ptr as *const u8, dtb_len) };
    let Ok(dtb) = Fdt::new(dtb_bytes) else {
        fail("DTB parse");
    };

    // 2. Bring up fw_cfg and program ramfb (the framebuffer device).
    let Ok(dma) = MmioDma::from_dtb(&dtb) else {
        fail("no fw_cfg device in DTB");
    };
    let fw = FwCfg::new(dma);
    let ramfb = RamfbConfig {
        phys_base: framebuffer_phys(),
        drm_format: DRM_FORMAT_XRGB8888,
        flags: 0,
        width: WIDTH,
        height: HEIGHT,
        stride: STRIDE,
    };
    if fw.program_ramfb(&ramfb).is_err() {
        fail("fw_cfg: ramfb programming failed");
    }
    log("framebuffer-qemu: ramfb programmed");

    // 3. Assemble the parsed-geometry boot hand-off and drive the
    //    capability-gated driver lifecycle against it.
    let phys_base = framebuffer_phys();
    let config = FramebufferConfig {
        phys_base,
        width_px: WIDTH,
        height_px: HEIGHT,
        stride_bytes: STRIDE,
        format: DisplayFormat::Bgra8888,
    };
    drive_lifecycle(config);
}

/// Build the capability-checked [`KernelMmioMapper`], load the signed
/// `.rxe` through the [`Host`], and drive `load -> use -> unload ->
/// reload` against the surface `config` describes, verifying the
/// presented pixels reach the scan-out memory after each `present`.
/// Every failure flips QEMU failure with a breadcrumb.
fn drive_lifecycle(config: FramebufferConfig) {
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

    // Driver-host view for `Framebuffer::open` (CAP_MMIO_MAP granted).
    let mut open_grants = CapabilitySet::empty();
    open_grants.insert(CapabilityId::MMIO_MAP);
    let fb_host = FramebufferHost {
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
    let spawner = ResolveFramebuffer;
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
        source: &source,
        spawner: &spawner,
        sink: &SERIAL_SINK,
        virtio_host_factory: None,
        mmio_mapper: None,
    });
    let Ok(h1) = host.load("/System/Drivers/framebuffer.rxe", &load_caps) else {
        fail("signed .rxe load");
    };
    if host.loaded_count() != 1 || host.snapshot().first().map(|s| s.handle) != Some(h1) {
        fail("loaded state after load");
    }

    // use: open the surface and present a frame, then verify the pixels
    // reached the scan-out memory.
    present_and_verify(&fb_host, &mapper, config, &make_frame(0x00), "first");
    log("framebuffer-qemu: first frame presented and verified");

    // unload -> reload through the host.
    let Ok(h2) = host.reload(h1, &load_caps) else {
        fail("signed .rxe reload");
    };
    if h2 == h1 || host.loaded_count() != 1 {
        fail("loaded state after reload");
    }

    // use again after reload: present a distinct frame and verify.
    present_and_verify(&fb_host, &mapper, config, &make_frame(0xA5), "reloaded");
    log("framebuffer-qemu: reloaded frame presented and verified");

    // unload: tear the driver down cleanly.
    if host.unload(h2).is_err() || host.loaded_count() != 0 {
        fail("driver unload");
    }
    log("framebuffer-qemu: driver unloaded after device reuse");
}

/// Open the framebuffer through `fb_host`, present `frame`, drop the
/// driver (the quiesce step), then confirm `frame` landed in the
/// scan-out surface via an independent window. `phase` names the step
/// in any failure breadcrumb.
fn present_and_verify(
    fb_host: &FramebufferHost<'_>,
    mapper: &dyn MmioMapper,
    config: FramebufferConfig,
    frame: &[u8],
    phase: &str,
) {
    {
        let Ok(mut fb) = Framebuffer::open(fb_host, config) else {
            fail(phase_msg(phase, "Framebuffer::open"));
        };
        if fb.present(frame).is_err() {
            fail(phase_msg(phase, "present"));
        }
        // `fb` drops here, releasing its window handle (the quiesce step).
    }
    verify_surface(mapper, config.phys_base, frame);
}

/// Pick a `&'static str` breadcrumb for `(phase, op)` without an
/// allocator (no `format!` on the fail path).
fn phase_msg(phase: &str, op: &str) -> &'static str {
    match (phase, op) {
        ("first", "Framebuffer::open") => "Framebuffer::open (first)",
        ("first", _) => "present (first)",
        (_, "Framebuffer::open") => "Framebuffer::open (reloaded)",
        _ => "present (reloaded)",
    }
}
