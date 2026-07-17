//! Freestanding (`aarch64-unknown-none`) half of the `plans/PI.md` P11
//! Chunk B-2 root-mount->login integration test.
//!
//! The device-agnostic bring-up (boot harness, DTB MMIO walk, GICv2 + EL1
//! IRQ wiring, static DMA pool, signed-`.rxe` load) lives in the shared
//! `tairix-test-virtio-qemu-support` crate. This module
//! supplies the unlock-specific tail: once the signed virtio-blk driver is
//! loaded over the planted whole-disk encrypted-root image, it drives the
//! **production** interactive unlock policy
//! ([`unlock_root_disk_interactively`]) — typing the fixture passphrase at
//! the prompt over a scripted console — and proves the loaded database
//! installs into a [`LateUsersDb`] cell and authenticates the planted
//! account while refusing a wrong password.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tairix_abi::driver::virtio::VirtioHost;
use tairix_abi::Errno;
use tairix_drv_storage_virtio_blk::{register as virtio_blk_register, VirtioBlk};
use tairix_kernel::root_mount::{
    unlock_root_disk_interactively, NoWritableRootSink, UnlockInstall, UnlockOutcome,
};
use tairix_kernel::volume_policy::LateStorageGid;
use tairix_kernel_core::{ConsoleRead, LateIdentity, LateUsersDb, NullConsole, UsersDbSource};
use tairix_test_encrypted_root_image as disk_image;
use tairix_test_virtio_qemu_support::{
    define_mmio_boot_harness_aarch64, run_virtio_mmio_scenario, FixedSpawner, QemuEnv,
    ScenarioConfig, ScenarioTransport,
};
use tairix_users::UsersDb;

use crate::fixture::{DTB_BLOB, RXE_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

/// Bare virtio-blk MMIO device id (the `DeviceID` register value; over
/// MMIO this is the bare virtio device type, not the PCI `0x1040 + type`
/// encoding).
const VIRTIO_BLK_DEVICE_ID: u32 = 2;

/// Spawner registering every verified manifest through the virtio-blk driver's
/// `register` entry point.
static SPAWNER: FixedSpawner = FixedSpawner::new(virtio_blk_register);

/// A scripted console input source: yields the fixture
/// [`disk_image::PASSPHRASE`] bytes followed by a single line terminator,
/// then reports end of input — the exact bytes an operator types at the
/// `ARXFS passphrase:` prompt. `Sync` through an atomic cursor over the
/// immutable passphrase, as [`ConsoleRead`] requires (its `read` takes
/// `&self`).
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
            // The passphrase line is spent; report end of input rather than
            // looping, so a give-up path (a wrong unlock) terminates.
            return Ok(0);
        };
        buf[0] = byte;
        self.cursor.store(i + 1, Ordering::Relaxed);
        Ok(1)
    }
}

/// The unlock device tail: open the virtio-blk whole-disk device, drive the
/// production interactive unlock over a scripted console, and prove the
/// installed database authenticates the planted account.
fn root_unlock_login(
    env: &dyn QemuEnv,
    transport: ScenarioTransport,
    vhost: &dyn VirtioHost,
) -> Result<(), &'static str> {
    let blk = VirtioBlk::open(transport, vhost).map_err(|_| "virtio-blk open")?;
    env.log("root-unlock: virtio-blk root device open");

    // A fresh set-once cell stands in for the boot-wired
    // `tairix_kernel::root_mount::LATE_USERS_DB`: the policy under test is
    // the same, and a local cell keeps the one-shot scenario free of global
    // state.
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

    // `NullConsole` swallows the prompt bytes (the test asserts the unlock
    // outcome and the installed credentials, not the prompt rendering); the
    // scripted reader types the passphrase. The audit sink is the harness's,
    // so the unlock's decisions land on the same channel the boot log uses.
    // This vertical proves the unlock *policy* only; driver autoload is the
    // separate pre-unlock `/System`-volume path (design B), not exercised here.
    //
    // The `on_resolved` callback is how the production kthread releases
    // console 0 to `login` once the unlock resolves (`crate::aarch64::
    // root_unlock`); assert here that it fires on the success path, the
    // end-to-end witness of the fix that a *successful* unlock hands the
    // console back (it previously did not, wedging `login`).
    let released = AtomicBool::new(false);
    let outcome = unlock_root_disk_interactively(
        blk,
        &NullConsole,
        &input,
        &UnlockInstall {
            users: &late,
            identity: &late_identity,
            // This vertical proves the unlock policy + users/identity install,
            // not the writable-state mount (no driver-store device here to
            // open a second window from), so nothing is published and no
            // account-administration engine is wired.
            writable: &NoWritableRootSink,
            admin: None,
            // A fresh gid cell stands in for the boot-wired storage-group
            // policy cell, exactly like the users/identity cells above.
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

    // The cell now serves the loaded `users-v1` text; it must authenticate
    // the planted account and refuse a wrong password, proving the database
    // login reads through the dispatch hook is usable (`plans/PI.md` P11).
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

/// Drive the full virtio-blk-mmio bring-up, then the interactive root
/// unlock + login proof, reporting the result through the ARM semihosting
/// finisher. Never returns.
fn run_scenario() -> ! {
    let cfg = ScenarioConfig {
        rxe_image: RXE_IMAGE,
        trusted_pubkey: TRUSTED_SIGNER_PUBKEY,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        spawner: &SPAWNER,
        start_msg: "root-unlock: scenario start",
    };
    run_virtio_mmio_scenario(VIRTIO_BLK_DEVICE_ID, DTB_BLOB, &cfg, root_unlock_login)
}

define_mmio_boot_harness_aarch64!(run_scenario);
