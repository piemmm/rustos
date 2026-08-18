//! Freestanding (`aarch64-unknown-none`) half of the
//! framebuffer-display QEMU vertical.
//!
//! The device-agnostic EL1 bring-up (FP enable + 2 GiB identity MMU +
//! vectors), the `AArch64QemuEnv` serial/semihosting seam, and the
//! one-shot boot harness all live in the shared
//! `tairix-test-virtio-qemu-support` crate; the
//! `fw_cfg`/`ramfb` DMA client lives in `tairix-fwcfg`. This module
//! supplies only what is unique to the aarch64 display vertical: programming
//! `ramfb` from the embedded `virt` DTB and driving the framebuffer
//! driver through `load -> use -> unload -> reload`, reading the presented
//! pixels back through the capability-gated [`KernelMmioMapper`] to prove
//! they reach the scan-out surface QEMU consumes, and then the
//! `plans/DISPLAY.md` D4 seat phase: the present right is derived from the
//! live lease on a real kernel [`SeatRegistry`], so a revoked client's
//! present is refused (its last frame stays on scan-out) while the new
//! foreground's fresh lease renders. The D6 multi-seat phase then proves
//! two independent seats on the same registry: a hot-attached display
//! node mints a second seat with its own owner, input routing, and
//! present gate, and hot-detaching it destroys the seat with no reboot
//! while the boot seat is untouched.
//!
//! The `fw_cfg`/`ramfb` bring-up is test-harness-specific (it synthesises
//! the device QEMU scans out), mirroring how the virtio verticals own
//! their GICv2 + EL1 bring-up in the test support crate rather than in
//! production kernel code.

extern crate alloc;

use alloc::vec::Vec;
use core::ptr;

use tairix_abi::driver::display::{Display, DisplayFormat, SeatGate};
use tairix_abi::input::{KeyInput, KeyValue, Modifiers};
use tairix_abi::seat::{SeatLease, SEAT_PRIMARY};
use tairix_abi::{CapabilityId, DriverError, DriverHost, DriverKind, Errno, MmioMapper};
use tairix_caps::CapabilitySet;
use tairix_crypto::Ed25519PublicKey;
use tairix_display::{Framebuffer, FramebufferConfig};
use tairix_drvhost::{
    DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
};
use tairix_fdt::Fdt;
use tairix_fwcfg::{FwCfg, MmioDma, RamfbConfig, DRM_FORMAT_XRGB8888};
use tairix_kernel_core::console::NULL_CONSOLE_INPUT;
use tairix_kernel_core::SeatRegistry;
use tairix_kernel_mem::{AddressSpace, DirectPhysMap, HostPageTable, MmioMap, VirtAddr};
use tairix_kernel_sec::captable::{ProcessId, TaskCapabilities};
use tairix_kernel_sec::identity::UserId;
use tairix_kernel_virtio::KernelMmioMapper;
use tairix_seat::SeatOwner;
use tairix_test_virtio_qemu_support::{
    bring_up_el1_identity_mmu, define_mmio_boot_harness_aarch64, AArch64QemuEnv, QemuEnv,
};

use crate::fixture::{DTB_BLOB, FB_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

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
/// `FB_BYTES` (4 pages) and the vertical mints fourteen windows (two
/// driver loads + two verifications, three seat-phase opens + three
/// verifications, then two multi-seat opens + two verifications), each
/// bracketed by two guard pages, so 128 pages leaves comfortable
/// headroom.
const MMIO_CAP_PAGES: usize = 128;

/// Upper bound of the boot identity map exposed to the MMIO mapper (the
/// bottom 4 GiB); the `virt` board's RAM and the framebuffer surface both
/// sit well inside it (and inside the 2 GiB the MMU actually maps).
const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// Synthetic owner process id for the driver context.
const TASK: ProcessId = ProcessId(0xFB1);

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

// --- Host plumbing ---------------------------------------------------

/// Image source returning the baked-in signed `.rxe` regardless of path.
struct BakedSource;

impl ImageSource for BakedSource {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(FB_IMAGE);
        Ok(())
    }
}

/// Per-load `DriverHandle` marker the spawner reports. The bytes spell
/// `"FBUF"`, mirroring the marker the framebuffer driver's `register`
/// entry point used before it became a spawned `Run` process.
const FB_HANDLE_MARKER: u64 = 0x4642_5546_0000_0001;

/// Spawner clearing every verified manifest through the load-time
/// capability gate. The framebuffer driver is a spawned `Run` process in
/// production (its engine lives in `lib/display`), so the vertical's
/// in-process spawner carries only the gate a register entry point would
/// have enforced: no `CAP_DRV_LOAD`, no load.
struct ResolveFramebuffer;

impl DriverSpawner for ResolveFramebuffer {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<tairix_abi::DriverHandle, SpawnRegisterError> {
        if !ctx.host.has_capability(CapabilityId::DRV_LOAD) {
            return Err(SpawnRegisterError::Register(DriverError::PermissionDenied));
        }
        tairix_abi::DriverHandle::from_raw(FB_HANDLE_MARKER).map_err(SpawnRegisterError::Register)
    }
}

/// Driver-host view used for `Framebuffer::open`: grants `CAP_MMIO_MAP`
/// and exposes the real [`KernelMmioMapper`]. Distinct from the
/// [`Host`]-installed view, mirroring how the bus-driver verticals
/// separate the load gate from the map gate.
struct FramebufferHost<'a> {
    granted: CapabilitySet,
    mapper: &'a dyn MmioMapper,
    /// The live seat-lease gate for the client this host presents on
    /// behalf of; `None` for the seatless bring-up phases.
    seat: Option<&'a dyn SeatGate>,
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

    fn seat_gate(&self) -> Option<&dyn SeatGate> {
        self.seat
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
fn verify_surface(env: &dyn QemuEnv, mapper: &dyn MmioMapper, phys_base: u64, expected: &[u8]) {
    let Ok(window) = mapper.map_window(phys_base, FB_BYTES) else {
        env.fail("verify: map_window failed");
    };
    // Compare word-by-word over the whole surface.
    let mut off = 0;
    while off < FB_BYTES {
        let Ok(got) = window.read_u32(off) else {
            env.fail("verify: window read out of bounds");
        };
        let want = u32::from_le_bytes([
            expected[off],
            expected[off + 1],
            expected[off + 2],
            expected[off + 3],
        ]);
        if got != want {
            env.fail("verify: surface pixel mismatch");
        }
        off += 4;
    }
}

// --- Scenario entry --------------------------------------------------

/// Drive the whole vertical, then report success through the ARM
/// semihosting finisher. Never returns. Every failure flips QEMU failure
/// with a breadcrumb.
fn run_scenario() -> ! {
    let env = AArch64QemuEnv;
    bring_up_el1_identity_mmu(&env);
    env.log("framebuffer-qemu: scenario start");

    // 1. Parse the embedded device tree (QEMU's aarch64 `-kernel <ELF>`
    //    path passes no DTB pointer, so the blob is embedded at build
    //    time — see `build.rs`).
    let Ok(dtb) = Fdt::new(DTB_BLOB) else {
        env.fail("DTB parse");
    };

    // 2. Bring up fw_cfg and program ramfb (the framebuffer device).
    let Ok(dma) = MmioDma::from_dtb(&dtb) else {
        env.fail("no fw_cfg device in DTB");
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
        env.fail("fw_cfg: ramfb programming failed");
    }
    env.log("framebuffer-qemu: ramfb programmed");

    // 3. Assemble the geometry hand-off and drive the capability-gated
    //    driver lifecycle against it.
    let config = FramebufferConfig {
        phys_base: framebuffer_phys(),
        width_px: WIDTH,
        height_px: HEIGHT,
        stride_bytes: STRIDE,
        format: DisplayFormat::Bgra8888,
    };
    drive_lifecycle(&env, config);

    env.log("framebuffer-qemu: driver unloaded after device reuse");
    env.succeed()
}

/// Build the capability-checked [`KernelMmioMapper`], load the signed
/// `.rxe` through the [`Host`], and drive `load -> use -> unload ->
/// reload` against the surface `config` describes, verifying the
/// presented pixels reach the scan-out memory after each `present`.
/// Every failure flips QEMU failure with a breadcrumb.
fn drive_lifecycle(env: &dyn QemuEnv, config: FramebufferConfig) {
    let audit = env.audit_sink();

    // Capability-checked kernel MMIO mapper over the boot identity map.
    let mut grants = CapabilitySet::empty();
    grants.insert(CapabilityId::MMIO_MAP);
    grants.insert(CapabilityId::DRV_LOAD);
    let caller = TaskCapabilities::derive(TASK, UserId(0), grants, grants, audit);
    let phys = DirectPhysMap::identity(IDENTITY_LIMIT);
    let Ok(mut mmio) = MmioMap::new(
        AddressSpace::new(HostPageTable::new()),
        VirtAddr::new(MMIO_VBASE),
        MMIO_CAP_PAGES,
        &phys,
    ) else {
        env.fail("MMIO map construct");
    };
    let mapper = KernelMmioMapper::new(&mut mmio, &caller, audit);

    // Driver-host view for `Framebuffer::open` (CAP_MMIO_MAP granted).
    let mut open_grants = CapabilitySet::empty();
    open_grants.insert(CapabilityId::MMIO_MAP);
    let fb_host = FramebufferHost {
        granted: open_grants,
        mapper: &mapper,
        seat: None,
    };

    // Load the signed `.rxe` through the driver host (the gate).
    let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
        env.fail("trust anchor decode");
    };
    let trusted = [pubkey];
    let mut load_caps = CapabilitySet::empty();
    load_caps.insert(CapabilityId::DRV_LOAD);
    let source = BakedSource;
    let spawner = ResolveFramebuffer;
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        accepted_abi_version: tairix_abi::ABI_VERSION_CURRENT,
        source: &source,
        spawner: &spawner,
        sink: audit,
        virtio_host_factory: None,
        mmio_mapper: None,
    });
    let Ok(h1) = host.load("/System/Drivers/framebuffer.rxe", &load_caps) else {
        env.fail("signed .rxe load");
    };
    if host.loaded_count() != 1 || host.snapshot().first().map(|s| s.handle) != Some(h1) {
        env.fail("loaded state after load");
    }

    // use: open the surface and present a frame, then verify the pixels
    // reached the scan-out memory.
    present_and_verify(env, &fb_host, &mapper, config, &make_frame(0x00), "first");
    env.log("framebuffer-qemu: first frame presented and verified");

    // unload -> reload through the host.
    let Ok(h2) = host.reload(h1, &load_caps) else {
        env.fail("signed .rxe reload");
    };
    if h2 == h1 || host.loaded_count() != 1 {
        env.fail("loaded state after reload");
    }

    // use again after reload: present a distinct frame and verify.
    present_and_verify(
        env,
        &fb_host,
        &mapper,
        config,
        &make_frame(0xA5),
        "reloaded",
    );
    env.log("framebuffer-qemu: reloaded frame presented and verified");

    // unload: tear the driver down cleanly.
    if host.unload(h2).is_err() || host.loaded_count() != 0 {
        env.fail("driver unload");
    }

    // D4: the present right is derived from the live seat lease.
    drive_seat_lease(env, &mapper, config);

    // D6: multiple independent seats, created and destroyed by display
    // hotplug.
    drive_multi_seat(env, &mapper, config);
}

/// The `plans/DISPLAY.md` D4 seat phase, on a real kernel
/// [`SeatRegistry`]: client A acquires the seat and presents; an
/// administrative revoke evicts it, and A's next present is refused with
/// the distinct `SeatRevoked` *before* the surface is touched (A's last
/// frame stays on scan-out even though A's mapping persists); client B
/// then acquires the freed seat under a fresh lease generation and its
/// present renders. Every failure flips QEMU failure with a breadcrumb.
fn drive_seat_lease(env: &dyn QemuEnv, mapper: &dyn MmioMapper, config: FramebufferConfig) {
    static SEAT: SeatRegistry = SeatRegistry::new(&NULL_CONSOLE_INPUT);
    const CLIENT_A: u64 = 0xA11;
    const CLIENT_B: u64 = 0xB22;

    let mut open_grants = CapabilitySet::empty();
    open_grants.insert(CapabilityId::MMIO_MAP);

    // Client A takes the seat and presents its frame.
    let Ok(lease_a) = SEAT.acquire(SEAT_PRIMARY, SeatOwner(CLIENT_A)) else {
        env.fail("seat: client A acquire");
    };
    let gate_a = SEAT.present_gate(SeatLease {
        seat_id: SEAT_PRIMARY,
        owner_task: CLIENT_A,
        generation: lease_a.generation,
    });
    let host_a = FramebufferHost {
        granted: open_grants,
        mapper,
        seat: Some(&gate_a),
    };
    let frame_a = make_frame(0x5A);
    {
        let Ok(mut fb) = Framebuffer::open(&host_a, config) else {
            env.fail("seat: open for client A");
        };
        if fb.present(&frame_a).is_err() {
            env.fail("seat: client A present");
        }
    }
    verify_surface(env, mapper, config.phys_base, &frame_a);
    env.log("framebuffer-qemu: seat owner presented");

    // The administrator revokes A's lease: the very next present is
    // refused with the distinct error and the scan-out keeps A's frame.
    if SEAT.revoke(SEAT_PRIMARY) != Ok(SeatOwner(CLIENT_A)) {
        env.fail("seat: revoke");
    }
    {
        let Ok(mut fb) = Framebuffer::open(&host_a, config) else {
            env.fail("seat: reopen for revoked client A");
        };
        if fb.present(&make_frame(0xEE)) != Err(DriverError::SeatRevoked) {
            env.fail("seat: revoked present not refused");
        }
    }
    verify_surface(env, mapper, config.phys_base, &frame_a);
    env.log("framebuffer-qemu: revoked present refused");

    // The new foreground acquires the freed seat and renders.
    let Ok(lease_b) = SEAT.acquire(SEAT_PRIMARY, SeatOwner(CLIENT_B)) else {
        env.fail("seat: client B acquire");
    };
    if lease_b.generation <= lease_a.generation {
        env.fail("seat: lease generation not monotonic");
    }
    let gate_b = SEAT.present_gate(SeatLease {
        seat_id: SEAT_PRIMARY,
        owner_task: CLIENT_B,
        generation: lease_b.generation,
    });
    let host_b = FramebufferHost {
        granted: open_grants,
        mapper,
        seat: Some(&gate_b),
    };
    let frame_b = make_frame(0x3C);
    {
        let Ok(mut fb) = Framebuffer::open(&host_b, config) else {
            env.fail("seat: open for client B");
        };
        if fb.present(&frame_b).is_err() {
            env.fail("seat: client B present");
        }
    }
    verify_surface(env, mapper, config.phys_base, &frame_b);
    env.log("framebuffer-qemu: new foreground rendered");
}

/// The `plans/DISPLAY.md` D6 multi-seat phase, on a real kernel
/// [`SeatRegistry`]: hot-attaching a display node mints a second,
/// independent seat (own owner, own input routing, own present gate)
/// beside the boot seat, and hot-detaching the node destroys it with no
/// reboot — the dead seat's still-held lease loses its present authority
/// instantly while the boot seat's owner is untouched. Every failure
/// flips QEMU failure with a breadcrumb.
fn drive_multi_seat(env: &dyn QemuEnv, mapper: &dyn MmioMapper, config: FramebufferConfig) {
    static SEATS: SeatRegistry = SeatRegistry::new(&NULL_CONSOLE_INPUT);
    const CLIENT_A: u64 = 0xA11;
    const CLIENT_B: u64 = 0xB22;
    const DISPLAY_NODE: u32 = 42;

    fn press(c: char) -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char(c),
            modifiers: Modifiers::default(),
        }
    }

    // Hot-attach: the discovered display node mints a fresh seat beside
    // the boot seat.
    let second = SEATS.attach_display(DISPLAY_NODE);
    if second == SEAT_PRIMARY {
        env.fail("multi-seat: fresh seat id");
    }

    // Independent owners: A owns the boot seat, B the hotplug seat, and
    // the busy refusal stays per-seat.
    if SEATS.acquire(SEAT_PRIMARY, SeatOwner(CLIENT_A)).is_err() {
        env.fail("multi-seat: A acquires boot seat");
    }
    let Ok(lease_b) = SEATS.acquire(second, SeatOwner(CLIENT_B)) else {
        env.fail("multi-seat: B acquires hotplug seat");
    };
    if SEATS.acquire(second, SeatOwner(CLIENT_A)) != Err(Errno::SeatBusy) {
        env.fail("multi-seat: hotplug seat busy for A");
    }

    // Independent input routing: each seat's key edge drains only at that
    // seat's owner, never across.
    if SEATS.inject(SEAT_PRIMARY, press('a')).is_err() {
        env.fail("multi-seat: inject boot seat");
    }
    if SEATS.inject(second, press('b')).is_err() {
        env.fail("multi-seat: inject hotplug seat");
    }
    let mut buf = [0u8; KeyInput::WIRE_LEN];
    if SEATS.read_key(second, SeatOwner(CLIENT_A), &mut buf) != Err(Errno::SeatNotOwner) {
        env.fail("multi-seat: cross-seat drain not refused");
    }
    if SEATS.read_key(SEAT_PRIMARY, SeatOwner(CLIENT_A), &mut buf) != Ok(KeyInput::WIRE_LEN)
        || KeyInput::from_bytes(&buf) != Ok(press('a'))
    {
        env.fail("multi-seat: boot-seat drain");
    }
    if SEATS.read_key(second, SeatOwner(CLIENT_B), &mut buf) != Ok(KeyInput::WIRE_LEN)
        || KeyInput::from_bytes(&buf) != Ok(press('b'))
    {
        env.fail("multi-seat: hotplug-seat drain");
    }
    env.log("framebuffer-qemu: two seats routed input independently");

    // B presents to the display under its own seat's live lease.
    let mut open_grants = CapabilitySet::empty();
    open_grants.insert(CapabilityId::MMIO_MAP);
    let gate_b = SEATS.present_gate(SeatLease {
        seat_id: second,
        owner_task: CLIENT_B,
        generation: lease_b.generation,
    });
    let host_b = FramebufferHost {
        granted: open_grants,
        mapper,
        seat: Some(&gate_b),
    };
    let frame_b = make_frame(0x77);
    {
        let Ok(mut fb) = Framebuffer::open(&host_b, config) else {
            env.fail("multi-seat: open for B");
        };
        if fb.present(&frame_b).is_err() {
            env.fail("multi-seat: B present");
        }
    }
    verify_surface(env, mapper, config.phys_base, &frame_b);
    env.log("framebuffer-qemu: hotplug seat owner presented");

    // Hot-detach: the vanished display destroys its seat with no reboot.
    // B's still-held lease loses its present authority instantly (the
    // scan-out keeps B's last frame) while the boot seat's owner is
    // untouched.
    if SEATS.detach_display(DISPLAY_NODE) != Some(second) {
        env.fail("multi-seat: detach");
    }
    {
        let Ok(mut fb) = Framebuffer::open(&host_b, config) else {
            env.fail("multi-seat: reopen for B");
        };
        if fb.present(&make_frame(0xEE)) != Err(DriverError::PermissionDenied) {
            env.fail("multi-seat: dead-seat present not refused");
        }
    }
    verify_surface(env, mapper, config.phys_base, &frame_b);
    if SEATS.owner(SEAT_PRIMARY) != Some(SeatOwner(CLIENT_A)) {
        env.fail("multi-seat: boot seat disturbed");
    }
    env.log("framebuffer-qemu: hotplug seat destroyed, boot seat intact");
}

/// Open the framebuffer through `fb_host`, present `frame`, drop the
/// driver (the quiesce step), then confirm `frame` landed in the
/// scan-out surface via an independent window. `phase` names the step
/// in any failure breadcrumb.
fn present_and_verify(
    env: &dyn QemuEnv,
    fb_host: &FramebufferHost<'_>,
    mapper: &dyn MmioMapper,
    config: FramebufferConfig,
    frame: &[u8],
    phase: &str,
) {
    {
        let Ok(mut fb) = Framebuffer::open(fb_host, config) else {
            env.fail(phase_msg(phase, "Framebuffer::open"));
        };
        if fb.present(frame).is_err() {
            env.fail(phase_msg(phase, "present"));
        }
        // `fb` drops here, releasing its window handle (the quiesce step).
    }
    verify_surface(env, mapper, config.phys_base, frame);
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

define_mmio_boot_harness_aarch64!(run_scenario);
