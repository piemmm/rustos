//! Architecture-neutral half of the shared virtio QEMU bring-up
//! scaffolding.
//!
//! Everything here builds for *every* freestanding virtio vertical —
//! the x86_64 PCI verticals and the riscv64 `virt`-board MMIO verticals
//! alike — because it names no architecture-specific facility. The two
//! arch-specific bring-up modules ([`crate::imp_pci`],
//! [`crate::imp_mmio`]) reach hardware (PCI vs. MMIO, MSI-X vs. PLIC,
//! `hlt` vs. `wfi`) and supply the concrete [`QemuEnv`]; this module
//! owns the parts that must not be duplicated across arches:
//!
//! * [`QemuEnv`] — the serial-breadcrumb + QEMU-exit seam each arch
//!   implements over its own serial sink and finisher device.
//! * [`ScenarioConfig`] + [`BakedSource`] — the signed-`.rxe` inputs.
//! * [`drive_driver_lifecycle`] — the `load → snapshot → reload →
//!   unload` cycle that drives the device round-trip *after* the reload
//!   and *before* the unload (the Stage 4.D Item 4 unload→reload→reuse
//!   deliverable, shared once here).
//! * [`virtio_blk_round_trip`] and the filesystem/users tails — the
//!   per-device round-trips, generic over the [`Transport`] so the PCI
//!   and MMIO verticals run *identical* device code.

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::driver::block::Block;
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::driver::input::{Input, InputEvent, InputEventKind};
use tairix_abi::{CapabilityId, DriverHandle, Errno};
use tairix_caps::CapabilitySet;
use tairix_crypto::Ed25519PublicKey;
use tairix_drv_fs_arxfs::ARXFS;
use tairix_drv_fs_fat32::Fat32;
use tairix_drv_storage_virtio_blk::VirtioBlk;
use tairix_drvhost::{
    DriverEntry, DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
};
use tairix_kernel_mem::bootinfo::{BootMemoryMap, MemoryRegion, RegionKind};
use tairix_kernel_mem::{PhysAddr, PAGE_SIZE};
use tairix_virtio::{Transport, VirtioHost, VirtioHostFactory};
use tairix_virtio_input::VirtioInput;

/// Upper bound of the boot identity map both arches build
/// (`DirectPhysMap::identity(IDENTITY_LIMIT)`): the bottom 4 GiB. Every
/// frame the per-device DMA allocator yields must fall below it so it is
/// reachable through that identity map. The x86_64 boot maps `0..4 GiB`;
/// the riscv64 `virt` board's RAM (`0x8000_0000..`) sits well inside it.
pub const IDENTITY_LIMIT: u64 = 0x1_0000_0000;

/// `EventId(4004)` — `AuditEvent::BootCompleted`. The arch boot harness
/// drives its scenario once on observing this event.
pub const BOOT_COMPLETED_EVENT_ID: tairix_log::EventId = tairix_log::EventId(4004);

/// Fixed driver path fed to `Host::load`. The image bytes come from the
/// in-memory [`BakedSource`] regardless of path, so the concrete string
/// only has to be well-formed.
const DRIVER_PATH: &str = "/System/Drivers/driver.rxe";

/// Serial-breadcrumb + QEMU-exit seam.
///
/// Each architecture implements this over its own `&'static` serial
/// sink and its own QEMU finisher device (x86_64 `isa-debug-exit`,
/// riscv64 `SiFive` Test), so the shared bring-up code logs progress and
/// flips the run result without naming either arch's facilities.
pub trait QemuEnv {
    /// Emit an info-level milestone breadcrumb on the serial sink.
    fn log(&self, msg: &str);

    /// Log `msg` and flip QEMU to failure. Never returns.
    fn fail(&self, msg: &str) -> !;

    /// Flip QEMU to success. Never returns.
    fn succeed(&self) -> !;

    /// The `&'static` serial sink the driver host audits through (the
    /// same sink [`log`](Self::log) writes breadcrumbs to).
    fn audit_sink(&self) -> &'static dyn tairix_log::Sink;
}

/// Image source returning the baked-in signed `.rxe` bytes regardless of
/// the requested path.
pub struct BakedSource<'a> {
    /// Signed `.rxe` image bytes.
    pub bytes: &'a [u8],
}

impl ImageSource for BakedSource<'_> {
    fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
        buf.extend_from_slice(self.bytes);
        Ok(())
    }
}

/// Per-vertical configuration shared by both arch scenarios.
///
/// The device id the bring-up walk matches is *not* here because its
/// type differs per transport (PCI `0x1040 + type` `u16` vs. the bare
/// virtio `u32` over MMIO); each arch scenario takes it as a separate
/// argument.
pub struct ScenarioConfig<'a> {
    /// Signed `.rxe` image bytes for the vertical's driver.
    pub rxe_image: &'a [u8],
    /// Trust-anchor public key the `HostConfig` accepts.
    pub trusted_pubkey: [u8; 32],
    /// SHA-256 fingerprint of the host's syscall table.
    pub syscall_table_hash: [u8; 32],
    /// Spawner completing the verified manifest's registration through
    /// the driver's `register`.
    pub spawner: &'a dyn DriverSpawner,
    /// Breadcrumb logged at scenario start.
    pub start_msg: &'a str,
}

/// Build the driver host over the signed `.rxe` and exercise the full
/// `load → snapshot → reload → unload` cycle against `factory`, running
/// `body` (the device round-trip) *after* the reload and *before* the
/// unload. Every transition that misbehaves flips QEMU failure with a
/// breadcrumb (no weakened tests). Never returns.
///
/// `body` is the per-device tail — typically [`virtio_blk_round_trip`]
/// or [`virtio_net_ping`], monomorphised over the arch's concrete
/// [`Transport`]. The whole cycle is shared so every vertical proves a
/// reloaded driver still brings its real device online and round-trips
/// I/O without duplicating the cycle per arch.
pub fn drive_driver_lifecycle<Tr, F>(
    env: &dyn QemuEnv,
    cfg: &ScenarioConfig<'_>,
    factory: &dyn VirtioHostFactory,
    transport: Tr,
    vhost: &dyn VirtioHost,
    body: F,
) -> !
where
    F: FnOnce(&dyn QemuEnv, Tr, &dyn VirtioHost) -> Result<(), &'static str>,
{
    let Ok(pubkey) = Ed25519PublicKey::from_bytes(&cfg.trusted_pubkey) else {
        env.fail("trust anchor decode");
    };
    let trusted = [pubkey];
    let mut load_caps = CapabilitySet::empty();
    load_caps.insert(CapabilityId::DRV_LOAD);
    load_caps.insert(CapabilityId::MEM_DMA);
    let source = BakedSource {
        bytes: cfg.rxe_image,
    };
    let mut host = Host::new(HostConfig {
        trusted_signers: &trusted,
        syscall_table_hash: cfg.syscall_table_hash,
        accepted_abi_version: tairix_abi::ABI_VERSION_CURRENT,
        source: &source,
        spawner: cfg.spawner,
        sink: env.audit_sink(),
        virtio_host_factory: Some(factory),
        mmio_mapper: None,
    });
    let Ok(first) = host.load(DRIVER_PATH, &load_caps) else {
        env.fail("signed .rxe load");
    };
    if host.loaded_count() != 1 {
        env.fail("loaded count after load");
    }
    if host.snapshot().first().map(|s| s.handle) != Some(first) {
        env.fail("snapshot handle mismatch");
    }
    let Ok(reloaded) = host.reload(first, &load_caps) else {
        env.fail("signed .rxe reload");
    };
    if reloaded == first {
        env.fail("reload returned stale handle");
    }
    if host.loaded_count() != 1 {
        env.fail("loaded count after reload");
    }
    env.log("virtio-qemu: signed .rxe loaded, reloaded");

    // Drive the device through the reloaded driver.
    if let Err(msg) = body(env, transport, vhost) {
        env.fail(msg);
    }

    // Unload and confirm the host returns to a clean state.
    if host.unload(reloaded).is_err() {
        env.fail("driver unload");
    }
    if host.loaded_count() != 0 {
        env.fail("loaded count after unload");
    }
    env.log("virtio-qemu: driver unloaded after device reuse");
    env.succeed()
}

/// Spawner registering every verified manifest in-process through a
/// fixed driver entry.
///
/// Shared by both verticals of a device class so the per-class spawner
/// is written once. The concrete `register` is
/// supplied at construction.
pub struct FixedSpawner {
    entry: DriverEntry,
}

impl FixedSpawner {
    /// Register every verified manifest through `entry`.
    #[must_use]
    pub const fn new(entry: DriverEntry) -> Self {
        Self { entry }
    }
}

impl DriverSpawner for FixedSpawner {
    fn spawn_and_register(
        &self,
        ctx: &SpawnContext<'_>,
    ) -> Result<DriverHandle, SpawnRegisterError> {
        (self.entry)(ctx.host).map_err(SpawnRegisterError::Register)
    }
}

/// Carve the top `pages` of the highest identity-mapped Usable region of
/// `src` into a single-region [`BootMemoryMap`] for the per-device DMA
/// allocator.
///
/// The carved sub-region sits at the very top of RAM, away from the low
/// frames the boot pipeline and kernel heap consume, so the per-device
/// [`FrameAllocator`](tairix_kernel_mem::FrameAllocator) never hands out
/// a frame the live kernel is using. It is bounded below
/// [`IDENTITY_LIMIT`] so every frame it yields is reachable through the
/// identity map. Both arch scenarios carve identically.
#[must_use]
pub fn carve_dma_map(src: &BootMemoryMap, pages: usize) -> Option<BootMemoryMap> {
    let need = (pages as u64).checked_mul(PAGE_SIZE as u64)?;
    let mut best_end: Option<u64> = None;
    for r in src.regions() {
        if r.kind != RegionKind::Usable {
            continue;
        }
        let end = r.end()?.as_u64();
        let start = r.start.as_u64();
        if end > IDENTITY_LIMIT {
            continue;
        }
        if end.saturating_sub(start) < need {
            continue;
        }
        best_end = Some(best_end.map_or(end, |b| b.max(end)));
    }
    let end = best_end?;
    let carve_end = end & !(PAGE_SIZE as u64 - 1);
    let carve_start = carve_end.checked_sub(need)?;
    let mut m = BootMemoryMap::new();
    m.push(MemoryRegion {
        kind: RegionKind::Usable,
        start: PhysAddr::new(carve_start),
        length: need,
    });
    Some(m)
}

/// Read the flattened device tree's total size from its header
/// (`totalsize`, a big-endian `u32` at byte offset 4) so a `&[u8]` of the
/// exact blob length can be formed from the raw pointer. Shared by the
/// MMIO bring-up of every `virt`-board arch.
///
/// # Safety
///
/// `ptr` must address a valid flattened device-tree blob (the verbatim
/// firmware hand-off published by the boot trampoline); the first 8 bytes
/// must be readable.
#[must_use]
pub unsafe fn dtb_total_size(ptr: u64) -> usize {
    let header = ptr as *const u8;
    // SAFETY: the caller guarantees the 8-byte FDT header is readable.
    let bytes = unsafe { core::slice::from_raw_parts(header, 8) };
    u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize
}

// --- Device tails (generic over the transport) -----------------------

/// Logical sector size.
const SECTOR_LEN: usize = 512;

/// `true` if `sector` matches the pattern the host harness planted at
/// LBA 0 (`byte[i] == i mod 256`). Kept in sync with the `plant_raw_disk`
/// call in `tools/xtask/src/commands/qemu_tests.rs`.
fn sector0_matches(sector: &[u8; SECTOR_LEN]) -> bool {
    sector
        .iter()
        .enumerate()
        .all(|(i, b)| *b == u8::try_from(i & 0xFF).unwrap_or(0))
}

/// Fill `sector` with the pattern the test writes to LBA 1
/// (`byte[i] = (i mod 256) xor 0xA5`) — distinct from the LBA-0 pattern
/// so a stale-read regression cannot pass by accident.
fn fill_sector1(sector: &mut [u8; SECTOR_LEN]) {
    for (i, b) in sector.iter_mut().enumerate() {
        *b = u8::try_from(i & 0xFF).unwrap_or(0) ^ 0xA5;
    }
}

/// virtio-blk device tail: open the device over `transport`, read
/// sector 0 and verify the harness-planted pattern, then write a known
/// pattern to sector 1 and read it back. Generic over the transport so
/// the PCI and MMIO verticals run identical code.
pub fn virtio_blk_round_trip<Tr: Transport>(
    env: &dyn QemuEnv,
    transport: Tr,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    let mut blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
    env.log("virtio-qemu: virtio-blk online");

    let mut s0 = [0u8; SECTOR_LEN];
    blk.read_blocks(0, &mut s0).map_err(|_| "read sector 0")?;
    if !sector0_matches(&s0) {
        return Err("sector 0 pattern mismatch");
    }
    env.log("virtio-qemu: sector 0 verified");

    let mut s1 = [0u8; SECTOR_LEN];
    fill_sector1(&mut s1);
    blk.write_blocks(1, &s1).map_err(|_| "write sector 1")?;
    let mut rb = [0u8; SECTOR_LEN];
    blk.read_blocks(1, &mut rb)
        .map_err(|_| "read-back sector 1")?;
    if rb != s1 {
        return Err("sector 1 round-trip mismatch");
    }
    env.log("virtio-qemu: sector 1 round-trip verified");
    Ok(())
}

/// FAT32-over-virtio-blk device tail: open the device over `transport`,
/// mount the planted FAT32 volume through the real
/// [`Fat32`](tairix_drv_fs_fat32::Fat32) driver, verify the planted
/// file reads back its known contents, then create and write a fresh
/// file and read it back. Generic over the transport so the PCI and
/// MMIO verticals run identical code.
///
/// The on-disk layout and the planted/written file names and contents
/// come from the shared [`tairix_test_fat32_image`] fixture — the same
/// source of truth the host harness plants the backing image from, so
/// the two sides cannot drift.
pub fn fat32_round_trip<Tr: Transport>(
    env: &dyn QemuEnv,
    transport: Tr,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    use tairix_test_fat32_image as image;

    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
    let mut fs = Fat32::open(blk).map_err(|_| "fat32 mount")?;
    env.log("virtio-qemu: fat32 volume mounted");

    let root = fs.root();
    let planted = fs
        .lookup(root, image::PLANTED_FILE_NAME)
        .map_err(|_| "lookup planted file")?;
    let mut buf = [0u8; 128];
    let n = fs
        .read_at(planted, 0, &mut buf)
        .map_err(|_| "read planted file")?;
    if &buf[..n] != image::PLANTED_FILE_CONTENT {
        return Err("planted file contents mismatch");
    }
    env.log("virtio-qemu: fat32 planted file verified");

    fs.create(root, image::NEW_FILE_NAME, NodeKind::RegularFile)
        .map_err(|_| "create new file")?;
    let written = fs
        .write_at(root, image::NEW_FILE_NAME, 0, image::NEW_FILE_CONTENT)
        .map_err(|_| "write new file")?;
    if written != image::NEW_FILE_CONTENT.len() {
        return Err("short write of new file");
    }

    let created = fs
        .lookup(root, image::NEW_FILE_NAME)
        .map_err(|_| "lookup new file")?;
    let mut rb = [0u8; 128];
    let m = fs
        .read_at(created, 0, &mut rb)
        .map_err(|_| "read-back new file")?;
    if &rb[..m] != image::NEW_FILE_CONTENT {
        return Err("new file round-trip mismatch");
    }
    env.log("virtio-qemu: fat32 write round-trip verified");
    Ok(())
}

/// arxfs-over-virtio-blk device tail: open the device over `transport`,
/// mount the planted arxfs volume through the real
/// [`ARXFS`](tairix_drv_fs_arxfs::ARXFS) driver, verify the planted
/// file reads back its known contents, then create and write a fresh
/// file and read it back. Generic over the transport so the PCI and
/// MMIO verticals run identical code.
///
/// The on-disk layout and the planted/written file names and contents
/// come from the shared [`tairix_test_arxfs_image`] fixture — the same
/// source of truth the host harness plants the backing image from (and
/// which the real driver itself authored), so the two sides cannot drift.
pub fn arxfs_round_trip<Tr: Transport>(
    env: &dyn QemuEnv,
    transport: Tr,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    use tairix_test_arxfs_image as image;

    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
    let geo = blk.geometry().map_err(|_| "arxfs geometry")?;
    if geo.block_count != image::TOTAL_SECTORS || geo.block_size as usize != image::SECTOR_BYTES {
        return Err("arxfs device geometry mismatch");
    }
    let mut fs = ARXFS::open(blk, &image::FIXTURE_VOLUME_KEY).map_err(|_| "arxfs mount")?;
    env.log("virtio-qemu: arxfs volume mounted");

    let root = fs.root();
    let planted = fs
        .lookup(root, image::PLANTED_FILE_NAME)
        .map_err(|_| "lookup planted file")?;
    let mut buf = [0u8; 128];
    let n = fs
        .read_at(planted, 0, &mut buf)
        .map_err(|_| "read planted file")?;
    if &buf[..n] != image::PLANTED_FILE_CONTENT {
        return Err("planted file contents mismatch");
    }
    env.log("virtio-qemu: arxfs planted file verified");

    fs.create(root, image::NEW_FILE_NAME, NodeKind::RegularFile)
        .map_err(|_| "create new file")?;
    let written = fs
        .write_at(root, image::NEW_FILE_NAME, 0, image::NEW_FILE_CONTENT)
        .map_err(|_| "write new file")?;
    if written != image::NEW_FILE_CONTENT.len() {
        return Err("short write of new file");
    }

    let created = fs
        .lookup(root, image::NEW_FILE_NAME)
        .map_err(|_| "lookup new file")?;
    let mut rb = [0u8; 128];
    let m = fs
        .read_at(created, 0, &mut rb)
        .map_err(|_| "read-back new file")?;
    if &rb[..m] != image::NEW_FILE_CONTENT {
        return Err("new file round-trip mismatch");
    }
    env.log("virtio-qemu: arxfs write round-trip verified");
    Ok(())
}

/// users-root device tail: open the device over `transport`, mount the
/// planted users-root volume through the real
/// [`ARXFS`](tairix_drv_fs_arxfs::ARXFS) driver, then drive the
/// kernel's boot-time users-database load
/// ([`tairix_kernel_core::load_users_db`]) against the mounted root —
/// the `plans/PI.md` P11 root-volume read path, end to end on a live
/// (emulated) board. The parsed database must authenticate the planted
/// account and refuse a wrong password, proving the loaded database is
/// usable by the login path.
///
/// The on-disk layout and the planted account come from the shared
/// [`tairix_test_arxfs_image`] users-root fixture — the same source of
/// truth the host harness plants the backing image from (authored by
/// the real driver), so the two sides cannot drift.
pub fn users_db_load<Tr: Transport>(
    env: &dyn QemuEnv,
    transport: Tr,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    use tairix_test_arxfs_image as image;

    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
    let mut fs = ARXFS::open(blk, &image::FIXTURE_VOLUME_KEY).map_err(|_| "users-root mount")?;
    env.log("virtio-qemu: users-root volume mounted");

    let db = tairix_kernel_core::load_users_db(&mut fs, env.audit_sink())
        .map_err(|_| "users database load")?;
    // Exactly the planted interactive fixture account: the on-disk
    // database holds human accounts only — the system/service identity is
    // compiled into the kernel (`tairix_users::system_accounts`), never
    // seeded to disk.
    if db.records().len() != 1 {
        return Err("users database record count mismatch");
    }
    env.log("virtio-qemu: users database loaded");

    let record = db
        .authenticate(
            image::USERS_FIXTURE_USERNAME,
            image::USERS_FIXTURE_PASSWORD.as_bytes(),
        )
        .map_err(|_| "planted account refused")?;
    if record.username() != image::USERS_FIXTURE_USERNAME {
        return Err("authenticated record names the wrong account");
    }
    if db
        .authenticate(image::USERS_FIXTURE_USERNAME, b"wrong password")
        .is_ok()
    {
        return Err("a wrong password must be refused");
    }
    env.log("virtio-qemu: users database authenticates");
    Ok(())
}

/// Readiness marker the QEMU runner waits to see on the serial console
/// before it injects a key. By the time the driver logs this, the
/// virtio-input device is fully online (`DRIVER_OK`) and its event queue
/// is set up; QEMU buffers the injected key until [`Input::poll`] posts
/// the first device-write descriptor, so logging the marker before the
/// first poll is race-free.
pub const INPUT_READY_MARKER: &str = "virtio-qemu: virtio-input eventq armed";

/// Bounded per-edge poll budget. The wait itself is interrupt-driven
/// inside [`Input::poll`] (the caller's IRQ waiter parks the CPU on the
/// eventq SPI), so this only bounds frame-marker / spurious-wake churn
/// between the real key edges; it never spins.
const MAX_INPUT_POLLS: usize = 64;

/// Drain the event queue until a `Key` event with the requested `value`
/// (`1` = press, `0` = release) is decoded, or the bounded budget is
/// exhausted. Frame markers (`EV_SYN`, surfaced as `Ok(0)`) and any
/// non-matching event are skipped.
fn wait_for_key<Tr: Transport>(
    input: &mut VirtioInput<'_, Tr>,
    value: i32,
) -> Result<bool, &'static str> {
    let mut events = [InputEvent {
        kind: InputEventKind::Key,
        reserved0: 0,
        code: 0,
        value: 0,
    }; 1];
    for _ in 0..MAX_INPUT_POLLS {
        let n = input.poll(&mut events).map_err(|_| "virtio-input poll")?;
        if n >= 1 && events[0].kind == InputEventKind::Key && events[0].value == value {
            return Ok(true);
        }
    }
    Ok(false)
}

/// virtio-input device tail: bring the device online over `transport`,
/// announce readiness, then decode a real injected key press followed by
/// its release. Generic over the transport so a PCI vertical could run
/// the identical code.
///
/// The key is injected by the QEMU runner through the monitor once it
/// observes [`INPUT_READY_MARKER`] on the serial console — a real
/// device→driver event, not a guest-side fabrication, which is the
/// virtio-input analogue of the PS/2 vertical's `0xD2` output-buffer
/// injection.
pub fn virtio_input_keypress<Tr: Transport>(
    env: &dyn QemuEnv,
    transport: Tr,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    let mut input = VirtioInput::open(transport, vhost).map_err(|_| "virtio-input open")?;
    env.log(INPUT_READY_MARKER);

    if !wait_for_key(&mut input, 1)? {
        return Err("virtio-input: no key press decoded");
    }
    env.log("virtio-qemu: virtio-input key press decoded");

    if !wait_for_key(&mut input, 0)? {
        return Err("virtio-input: no key release decoded");
    }
    env.log("virtio-qemu: virtio-input key release decoded");
    Ok(())
}

/// Shared root-mount → login scenario tail (`plans/PI.md` P11 Chunk B-2),
/// generic over the transport so the aarch64 virtio-MMIO and x86_64
/// virtio-PCI verticals drive *identical* unlock code from one definition
/// rather than two sibling copies.
///
/// Only built for the two disk-booting verticals that use it
/// (`itest_x86_64`, `itest_aarch64`); the riscv64 support target links no
/// `tairix-kernel` and never drives this tail, so gating it here keeps the
/// riscv64 dependency set free of the unlock policy crates.
#[cfg(any(itest_x86_64, itest_aarch64))]
pub use root_unlock::root_unlock_login;

#[cfg(any(itest_x86_64, itest_aarch64))]
mod root_unlock {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tairix_abi::Errno;
    use tairix_drv_storage_virtio_blk::VirtioBlk;
    use tairix_kernel::root_mount::{
        unlock_root_disk_interactively, NoWritableRootSink, UnlockInstall, UnlockOutcome,
    };
    use tairix_kernel::volume_policy::LateStorageGid;
    use tairix_kernel_core::{ConsoleRead, LateIdentity, LateUsersDb, NullConsole, UsersDbSource};
    use tairix_test_encrypted_root_image as disk_image;
    use tairix_users::UsersDb;

    use super::{QemuEnv, Transport, VirtioHost};

    /// A scripted console input source: yields the fixture
    /// [`disk_image::PASSPHRASE`] bytes followed by a single line
    /// terminator, then reports end of input — the exact bytes an operator
    /// types at the `ARXFS passphrase:` prompt. `Sync` through an atomic
    /// cursor over the immutable passphrase, as [`ConsoleRead`] requires
    /// (its `read` takes `&self`).
    struct ScriptedPassphrase {
        cursor: AtomicUsize,
    }

    impl ScriptedPassphrase {
        const fn new() -> Self {
            Self {
                cursor: AtomicUsize::new(0),
            }
        }
    }

    impl ConsoleRead for ScriptedPassphrase {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            if buf.is_empty() {
                return Ok(0);
            }
            let i = self.cursor.load(Ordering::Relaxed);
            let byte = if i < disk_image::PASSPHRASE.len() {
                disk_image::PASSPHRASE[i]
            } else if i == disk_image::PASSPHRASE.len() {
                b'\n'
            } else {
                // The passphrase line is spent; report end of input rather
                // than looping, so a give-up path (a wrong unlock)
                // terminates.
                return Ok(0);
            };
            buf[0] = byte;
            self.cursor.store(i + 1, Ordering::Relaxed);
            Ok(1)
        }
    }

    /// The unlock device tail: open the virtio-blk whole-disk device over
    /// `transport`, drive the **production** interactive unlock policy
    /// ([`unlock_root_disk_interactively`]) over a scripted console typing
    /// the fixture passphrase, and prove the installed database
    /// authenticates the planted account while a wrong password is refused.
    ///
    /// Generic over the transport (`PciTransport` on x86_64,
    /// `MmioTransport` on the MMIO boards) so both verticals run this one
    /// definition.
    pub fn root_unlock_login<Tr: Transport>(
        env: &dyn QemuEnv,
        transport: Tr,
        vhost: &dyn VirtioHost,
    ) -> Result<(), &'static str> {
        let blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
        env.log("root-unlock: virtio-blk root device open");

        // A fresh set-once cell stands in for the boot-wired
        // `tairix_kernel::root_mount::LATE_USERS_DB`: the policy under test
        // is the same, and a local cell keeps the one-shot scenario free of
        // global state.
        let late = LateUsersDb::new();
        // A fresh identity-table cell stands in for the boot-wired
        // `tairix_kernel::root_mount::LATE_IDENTITY`, pre-loaded with the
        // compiled-in system identity exactly as the boot sec phase installs
        // it: the unlock then *replaces* the held table with the merged
        // system∪human table built from the planted root's
        // `/System/Security/{Users,Groups}` in the same step it installs the
        // users database.
        let late_identity = LateIdentity::new();
        late_identity
            .install(
                tairix_kernel_core::system_identity_table(env.audit_sink())
                    .map_err(|_| "compiled identity build")?,
            )
            .map_err(|_| "compiled identity install")?;
        let input = ScriptedPassphrase::new();

        // `NullConsole` swallows the prompt bytes (the test asserts the
        // unlock outcome and the installed credentials, not the prompt
        // rendering); the scripted reader types the passphrase. The audit
        // sink is the harness's, so the unlock's decisions land on the same
        // channel the boot log uses. This vertical proves the unlock
        // *policy* only; driver autoload is the separate pre-unlock
        // `/System`-volume path (design B), not exercised here.
        //
        // The `on_resolved` callback is how the production kthread releases
        // console 0 to `login` once the unlock resolves; assert here that it
        // fires on the success path, the end-to-end witness that a
        // *successful* unlock hands the console back.
        let released = AtomicBool::new(false);
        let outcome = unlock_root_disk_interactively(
            blk,
            &NullConsole,
            &input,
            &UnlockInstall {
                users: &late,
                identity: &late_identity,
                // This vertical proves the unlock policy + users/identity
                // install, not the writable-state mount (no driver-store
                // device here to open a second window from), so nothing is
                // published and no account-administration engine is wired.
                writable: &NoWritableRootSink,
                admin: None,
                // A fresh gid cell stands in for the boot-wired
                // storage-group policy cell, exactly like the users/identity
                // cells above.
                storage_gid: &LateStorageGid::new(),
            },
            env.audit_sink(),
            // The fixture passphrase is correct on the first try, so the
            // wrong-passphrase delay is never invoked; a no-op stands in.
            &|| {},
            &|| released.store(true, Ordering::Release),
        );
        if outcome != UnlockOutcome::Installed {
            return Err("interactive unlock did not install a database");
        }
        if !released.load(Ordering::Acquire) {
            return Err("successful unlock did not release console 0 to login");
        }
        env.log("root-unlock: passphrase accepted, users database installed");

        // The cell now serves the loaded `users-v1` text; it must
        // authenticate the planted account and refuse a wrong password,
        // proving the database login reads through the dispatch hook is
        // usable.
        let text = late
            .text()
            .map_err(|_| "late cell empty after a reported install")?;
        let db = UsersDb::parse(core::str::from_utf8(&text).map_err(|_| "served db is not utf-8")?)
            .map_err(|_| "served users database does not parse")?;
        let record = db
            .authenticate(disk_image::USERNAME, disk_image::PASSWORD.as_bytes())
            .map_err(|_| "planted account refused through the installed cell")?;
        if record.username() != disk_image::USERNAME {
            return Err("authenticated record names the wrong account");
        }
        if db
            .authenticate(disk_image::USERNAME, b"wrong password")
            .is_ok()
        {
            return Err("a wrong password must be refused");
        }
        env.log("root-unlock: planted account authenticates");
        Ok(())
    }
}
