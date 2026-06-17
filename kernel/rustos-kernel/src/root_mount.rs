//! Production root-volume unlock + users-database load composition
//! (`plans/PI.md` §3 P11 root-mount increment, Chunk A).
//!
//! This is the one place that turns the three artefacts a boot path
//! recovers off the storage device — the plaintext `root.unlock`
//! key-derivation descriptor (planted on the FAT boot partition by
//! `tools/mkimage` / the §11 installer), the passphrase the operator
//! typed at the console, and the encrypted root [`Block`] device — into
//! the validated `users-v1` database the `users_db_read` syscall serves
//! (`kernel/core::load_users_db_source`). It adds no policy of its own; it
//! threads the already-landed building blocks together in the one layer
//! permitted to name both the `rustfs` driver and `kernel/core`
//! (`rustos-kernel`, `Layer::Tooling`, `AGENTS.md` §17.4).
//!
//! The composition is, in order:
//!
//! 1. [`UnlockDescriptor::decode`] parses the on-FAT descriptor
//!    fail-closed (bad magic, unknown KDF, out-of-range cost, short
//!    buffer → refused, never trusted, `AGENTS.md` §5.4.3 / §2.9).
//! 2. [`UnlockDescriptor::derive_volume_key`] derives the volume key from
//!    the typed passphrase via the descriptor's PBKDF2-HMAC-SHA256
//!    parameters (`lib/crypto`, `docs/src/filesystem/rustfs-spec.md` §7).
//!    The derived key is held in a [`Zeroizing`] wrapper so it is wiped on
//!    drop and never lingers on the boot stack (`AGENTS.md` §4 — secret
//!    hygiene; the audited `zeroize` crate, not a hand-rolled primitive,
//!    §2.12).
//! 3. [`RustFs::open`] mounts the encrypted root under that key. A wrong
//!    passphrase never unwraps the master key and the mount is refused
//!    with [`DriverError::PermissionDenied`] — there is no separate
//!    "wrong passphrase" oracle and no fallback to a plaintext mount
//!    (fail closed, `AGENTS.md` §5.4 / §4 — encrypted-by-default).
//! 4. [`load_users_db_source`] reads and validates
//!    `/System/Security/Users` off the mounted root under the kernel's
//!    capability-less `uid 0` bootstrap identity (its own §5.3 permission
//!    check and fail-closed `users-v1` parse), retaining the canonical
//!    text in a [`HeldUsersDbSource`] the boot path installs through
//!    `BootInfo::with_users_db`.
//!
//! Every refusal is audited and yields **no** database, so a system whose
//! root cannot be unlocked or whose database cannot be read serves none
//! rather than inventing accounts (`AGENTS.md` §5.4.5). The board-specific
//! discovery and bring-up that *produce* the descriptor bytes, the
//! passphrase, and the [`Block`] device — the hardware-tree root-device
//! discovery, the in-kernel block `DriverHost`, the FAT read, and the
//! console passphrase prompt — sit above this seam in the boot path and
//! are wired in the following increment (`plans/PI.md` P11 Chunk B);
//! `virtio-blk` proves it on `-M virt`, EMMC2 on metal (§0.4 / P8).

use rustos_abi::driver::block::Block;
use rustos_abi::DriverError;
use rustos_drv_fs_rustfs::{RustFs, UnlockDescriptor, VolumeKey};
use rustos_kernel_core::{load_users_db_source, HeldUsersDbSource, UsersLoadError};
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use zeroize::Zeroizing;

/// Audit event: the encrypted root volume was unlocked under the
/// passphrase-derived key and mounted (`AGENTS.md` §5.4.4 / §19.4). The
/// subsequent users-database read is audited separately by
/// [`load_users_db_source`] (`UsersDbLoaded` / `UsersDbRejected`).
const ROOT_MOUNT_UNLOCKED: EventId = EventId(4133);

/// Audit event: the root unlock was refused before a database could be
/// served — the on-FAT descriptor failed to decode, or the derived key
/// did not unlock the volume (a wrong passphrase, a non-rustfs volume, or
/// a device fault). The `stage` field names which check refused; no
/// secret (passphrase, key, or volume bytes) is ever logged (`AGENTS.md`
/// §4 / §19.4). The decision fails closed: no database is held (§5.4.5).
const ROOT_MOUNT_REJECTED: EventId = EventId(4134);

/// Why [`unlock_root_and_load_users`] produced no users database.
///
/// Each variant carries the underlying error from the first check that
/// refused; the composition stops at the first failure and returns it
/// (`AGENTS.md` §2.9 — fail closed, never partially applied).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RootMountError {
    /// The on-FAT `root.unlock` descriptor failed to decode: bad magic,
    /// an unknown KDF id, an out-of-range iteration count, or a short
    /// buffer. The descriptor is plaintext and untrusted, so it is fully
    /// validated before its parameters drive any key derivation
    /// (`AGENTS.md` §5.4.3).
    Descriptor(DriverError),
    /// The root volume could not be mounted under the derived key:
    /// [`DriverError::PermissionDenied`] for a wrong passphrase (the
    /// master key never unwraps), [`DriverError::BadMagic`] for a volume
    /// that is not rustfs, or a device fault. There is no plaintext-mount
    /// fallback (`AGENTS.md` §4 — encrypted by default; §5.4 — fail
    /// closed).
    Mount(DriverError),
    /// The volume mounted but `/System/Security/Users` could not be read
    /// or validated; [`load_users_db_source`] has already audited the
    /// precise cause.
    Users(UsersLoadError),
}

impl RootMountError {
    /// Short, stable, secret-free cause string for the audit record.
    #[must_use]
    pub fn cause(self) -> &'static str {
        match self {
            Self::Descriptor(_) => "descriptor_invalid",
            Self::Mount(DriverError::PermissionDenied) => "unlock_refused",
            Self::Mount(DriverError::BadMagic) => "not_a_rustfs_volume",
            Self::Mount(_) => "mount_failed",
            Self::Users(err) => err.cause(),
        }
    }
}

/// Unlock the encrypted root volume with the passphrase-derived key and
/// load its `/System/Security/Users` database.
///
/// * `descriptor_bytes` — the plaintext `root.unlock` key-derivation
///   descriptor read from the FAT boot partition.
/// * `passphrase` — the bytes the operator typed at the console prompt.
///   They are used only to derive the volume key and are never logged or
///   retained by this function.
/// * `block` — the encrypted root [`Block`] device the board brought up.
/// * `audit` — the sink the unlock/mount decision and (via
///   [`load_users_db_source`]) the database-read decision are logged
///   through (`AGENTS.md` §19.4).
///
/// On success the returned [`HeldUsersDbSource`] owns the validated
/// `users-v1` text (zeroed on drop, `AGENTS.md` §4); the boot path
/// `Box::leak`s it and installs it through `BootInfo::with_users_db`.
///
/// # Errors
///
/// A [`RootMountError`] naming the first check that refused. Every error
/// path yields no database and is audited; the derived key is wiped
/// regardless of outcome (`AGENTS.md` §4 / §5.4.5).
pub fn unlock_root_and_load_users<B: Block>(
    descriptor_bytes: &[u8],
    passphrase: &[u8],
    block: B,
    audit: &dyn Sink,
) -> Result<HeldUsersDbSource, RootMountError> {
    // 1. Decode the untrusted on-FAT descriptor fail-closed before its
    //    parameters drive any key derivation (`AGENTS.md` §5.4.3).
    let descriptor = match UnlockDescriptor::decode(descriptor_bytes) {
        Ok(descriptor) => descriptor,
        Err(err) => {
            let error = RootMountError::Descriptor(err);
            reject(audit, error);
            return Err(error);
        }
    };

    // 2. Derive the volume key from the typed passphrase. The key is the
    //    most sensitive transient on the boot stack: hold it in a
    //    zero-on-drop wrapper so it is wiped the instant it leaves scope,
    //    whether the mount succeeds or fails (`AGENTS.md` §4).
    let volume_key: Zeroizing<VolumeKey> =
        Zeroizing::new(descriptor.derive_volume_key(passphrase));

    // 3. Mount the encrypted root. A wrong passphrase fails to unwrap the
    //    master key and is refused fail-closed — no plaintext fallback,
    //    no separate oracle (`AGENTS.md` §4 / §5.4).
    let mut fs = match RustFs::open(block, &volume_key) {
        Ok(fs) => fs,
        Err(err) => {
            let error = RootMountError::Mount(err);
            reject(audit, error);
            return Err(error);
        }
    };
    log(
        audit,
        &Event {
            level: Level::Info,
            id: ROOT_MOUNT_UNLOCKED,
            message: "root-mount: encrypted root volume unlocked and mounted",
            fields: &[],
        },
    );

    // 4. Read and validate the users database off the mounted root. This
    //    audits its own outcome (`UsersDbLoaded` / `UsersDbRejected`) and
    //    retains the canonical text in the returned holder.
    load_users_db_source(&mut fs, audit).map_err(RootMountError::Users)
}

/// Emit the [`ROOT_MOUNT_REJECTED`] record for a refused unlock, naming
/// the failing `stage` with a secret-free cause string. The database load
/// stage audits itself, so this helper only ever reports a descriptor or
/// mount refusal.
fn reject(audit: &dyn Sink, error: RootMountError) {
    log(
        audit,
        &Event {
            level: Level::Error,
            id: ROOT_MOUNT_REJECTED,
            message: "root-mount: root volume unlock refused; no users database served",
            fields: &[Field {
                key: "cause",
                value: error.cause(),
            }],
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use core::cell::RefCell;

    use alloc::vec::Vec;

    use rustos_drv_fs_rustfs::{EntropySource, UNLOCK_DESCRIPTOR_LEN, UNLOCK_MIN_ITERATIONS};
    use rustos_kernel_core::UsersDbSource;
    use rustos_log::{Event as LogEvent, Sink as LogSink};
    use rustos_test_rustfs_image as image;
    use rustos_users::UsersDb;

    /// Deterministic entropy for provisioning a descriptor's salt in
    /// tests. A fixed sequence keeps the test reproducible (`AGENTS.md`
    /// §19.3); it is test scaffolding, never a production source.
    struct SeqEntropy {
        next: u8,
    }

    impl EntropySource for SeqEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
            for byte in out.iter_mut() {
                *byte = self.next;
                self.next = self.next.wrapping_add(1);
            }
            Ok(())
        }
    }

    /// Records every audited event id so a test can assert the audit trail.
    struct RecordingSink {
        ids: RefCell<Vec<u32>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                ids: RefCell::new(Vec::new()),
            }
        }
        fn ids(&self) -> Vec<u32> {
            self.ids.borrow().clone()
        }
    }

    impl LogSink for RecordingSink {
        fn write_event(&self, event: &LogEvent<'_>) {
            self.ids.borrow_mut().push(event.id.0);
        }
    }

    /// The passphrase the test "operator" types; the volume is provisioned
    /// under the key derived from it.
    const PASSPHRASE: &[u8] = b"correct horse battery staple";

    /// Provision a descriptor (low cost so the test stays fast under
    /// `cargo test`) and return its encoded bytes plus the volume key it
    /// derives from [`PASSPHRASE`].
    fn provision() -> ([u8; UNLOCK_DESCRIPTOR_LEN], VolumeKey) {
        // The policy floor (100k) is the cheapest a descriptor may carry,
        // keeping the per-test PBKDF2 derivations bounded while still
        // exercising the real key-derivation path (`AGENTS.md` §5.4).
        let descriptor =
            UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut SeqEntropy { next: 7 })
                .expect("descriptor provisions");
        let key = descriptor.derive_volume_key(PASSPHRASE);
        let mut bytes = [0u8; UNLOCK_DESCRIPTOR_LEN];
        descriptor.encode(&mut bytes).expect("descriptor encodes");
        (bytes, key)
    }

    #[test]
    fn the_correct_passphrase_unlocks_the_root_and_loads_a_usable_database() {
        // The end-to-end Chunk A path: a descriptor + the matching
        // passphrase derive the key the volume was provisioned under, the
        // volume mounts, and the served text is the exact, usable
        // `users-v1` database that authenticates the planted account.
        let (descriptor_bytes, key) = provision();
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let source = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect("the correct passphrase unlocks the root and loads the database");

        let text = source.text().expect("a loaded holder serves its text");
        let serialised = image::users_db_text().expect("fixture text serialises");
        assert_eq!(
            text,
            serialised.as_bytes(),
            "the served text is the exact canonical users-v1 database"
        );

        // The served database is usable: it parses and authenticates the
        // planted account but refuses a wrong password (`plans/PI.md` P11).
        let db = UsersDb::parse(core::str::from_utf8(text).expect("utf-8"))
            .expect("the served database parses");
        let record = db
            .authenticate(
                image::USERS_FIXTURE_USERNAME,
                image::USERS_FIXTURE_PASSWORD.as_bytes(),
            )
            .expect("the planted account authenticates");
        assert_eq!(record.username(), image::USERS_FIXTURE_USERNAME);
        assert!(
            db.authenticate(image::USERS_FIXTURE_USERNAME, b"wrong")
                .is_err(),
            "a wrong account password is refused"
        );

        // The unlock and the database load are both audited.
        assert!(sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_wrong_passphrase_is_refused_fail_closed_with_no_oracle() {
        // §4 / §5.4: the volume is provisioned under the key derived from
        // PASSPHRASE; a *different* passphrase derives a different key that
        // never unwraps the master key, so the mount is refused with
        // PermissionDenied and no database is served.
        let (descriptor_bytes, key) = provision();
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, b"wrong passphrase", block, &sink)
            .expect_err("a wrong passphrase must be refused");

        assert_eq!(err, RootMountError::Mount(DriverError::PermissionDenied));
        assert_eq!(err.cause(), "unlock_refused");
        // The refusal is audited and the volume never unlocked.
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_tampered_descriptor_is_refused_before_any_key_derivation() {
        // §5.4.3: a corrupt descriptor (bad magic) is rejected outright;
        // the passphrase is never even consulted and no mount is attempted.
        let (mut descriptor_bytes, key) = provision();
        descriptor_bytes[0] ^= 0xFF; // corrupt the magic
        let bytes = image::build_users_root_image_with_key(&key).expect("users-root volume builds");
        let block = image::VecBlock::from_bytes(bytes);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect_err("a tampered descriptor must be refused");

        assert!(matches!(err, RootMountError::Descriptor(_)), "{err:?}");
        assert_eq!(err.cause(), "descriptor_invalid");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }

    #[test]
    fn a_non_rustfs_volume_is_refused() {
        // §5.4 / §2.9: a device that is not a rustfs volume (here a zeroed
        // image of the right geometry) fails the mount closed rather than
        // being misread; no database is served.
        let (descriptor_bytes, _key) = provision();
        let sectors = usize::try_from(image::TOTAL_SECTORS).expect("sector count fits usize");
        let blank = alloc::vec![0u8; sectors * image::SECTOR_BYTES];
        let block = image::VecBlock::from_bytes(blank);
        let sink = RecordingSink::new();

        let err = unlock_root_and_load_users(&descriptor_bytes, PASSPHRASE, block, &sink)
            .expect_err("a non-rustfs volume must be refused");

        assert!(matches!(err, RootMountError::Mount(_)), "{err:?}");
        assert!(sink.ids().contains(&4134), "{:?}", sink.ids());
        assert!(!sink.ids().contains(&4133), "{:?}", sink.ids());
    }
}
