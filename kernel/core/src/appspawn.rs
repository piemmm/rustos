//! Disk-backed program spawning: the `spawn` syscall's store-bundle path.
//!
//! A program the embedded boot-floor registry does not carry is resolved as
//! an on-disk `<Name>.app` bundle: the kernel reads the bundle through the
//! mounted, secured VFS under the **caller's** kernel-attested identity,
//! judges it through the one shared `tairix_appload` load gate (layout,
//! signed `AppInfo`, content hash, syscall-interface hash, `rxe` hardening
//! invariants), and spawns exactly the bytes the gate validated. App code is
//! never baked into the kernel; the bundle directory on the volume is the
//! app.
//!
//! This module supplies the pieces the syscall handler composes:
//!
//! * [`AppStore`] — the build's embedded app trust anchor plus a one-way
//!   readiness latch the boot path resolves once the `/System` mount
//!   reaches a terminal state, so an early spawn *parks* (event-woken,
//!   never a poll loop) instead of racing the mount. It also carries the
//!   per-boot **semantic launch cache** ([`LaunchCache`]): the read-only
//!   system stores are immutable for the life of the boot, so a bundle's
//!   whole-tree hash and signature are verified once and every later
//!   launch of the same bundle serves the cached, already-verified image —
//!   after re-authorising the *caller's* read of the entry point through
//!   the secured VFS, so the cache never widens authority. Launch latency
//!   is a designed hot path; re-verifying an immutable bundle on every
//!   keystroke-to-output cycle is work hoisted off it. The cache is a
//!   classified, budgeted, pressure-governed reclaimable-memory consumer
//!   (`plans/SMARTRAM.md` SMART4): the boot path that publishes the mount
//!   installs its budget and the system pressure gauge
//!   ([`AppStore::install_reclaim`]); until then — and whenever the
//!   classification gate refuses — the store serves every launch uncached
//!   through the full load gate (fail closed).
//! * [`FsBundleStore`] — the [`tairix_appload::BundleStore`] over the
//!   kernel [`FilesystemService`], with fail-closed size/depth bounds.
//! * [`AnchorVerifier`] — the [`tairix_appload::Verifier`] pinning the
//!   manifest's signer to the build's embedded app trust anchor and
//!   verifying the Ed25519 signature through `lib/crypto`.
//! * [`bundle_run_path`] — the strict `<root>/<Name>.app/Run` path shape a
//!   spawnable bundle entry point must have.
//! * [`app_error_errno`] — the fail-closed [`AppError`] → [`Errno`] map the
//!   syscall reports.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

use tairix_abi::{
    digest_bundle_contents, AppInfoHeader, BundleFileDigest, CapabilityQuery, Errno, FileKind,
    ProgramKind, APPINFO_MAX_CAPABILITIES, APPINFO_MAX_MIME, BUNDLE_SUFFIX, MIME_ENTRY_LEN,
};
use tairix_appload::{AppError, BundleContents, BundleStore, Clock, LoadedApp, Verifier};
use tairix_crypto::{Ed25519PublicKey, Ed25519Signature, Sha256Stream};
use tairix_kernel_mem::{MAX_ORDER, PAGE_SIZE};
use tairix_log::Sink;
use tairix_reclaim::{CacheBudget, MemoryPressure};
use tairix_sync::RwLock;

use crate::bootinfo::KernelArch;
use crate::fs::{FilesystemService, FinalLink};
use crate::launch_cache::LaunchCache;
use crate::sched::SchedulerArch;

/// The on-disk application store has not reached a terminal state yet: the
/// boot path that mounts `/System` is still running.
const STORE_PENDING: u8 = 0;
/// The `/System` mount was published; store-bundle reads can resolve.
const STORE_AVAILABLE: u8 = 1;
/// No readable store exists on this boot (no disk, no `/System` volume, or
/// a port with no storage floor): store-bundle spawns fail closed.
const STORE_UNAVAILABLE: u8 = 2;

/// The on-disk application store: the build's embedded app trust anchor
/// plus a one-way readiness latch the boot path resolves.
///
/// PID 1 issues its first `spawn` calls concurrently with the kthread that
/// publishes the `/System` mount, so the spawn handler must not race the
/// install: while the latch is *pending* a store-bundle spawn parks on
/// [`crate::waitq::APP_STORE_WAITQ`] and is woken the instant the boot path
/// resolves the latch — to *available* when the mount was published, or to
/// *unavailable* when this boot has no readable store (then the spawn fails
/// closed). Resolution is one-way and idempotent, mirroring
/// [`crate::users::LateUsersDb`]'s pending/resolved shape. A build with no
/// store at all simply never installs an `AppStore` and every store-bundle
/// spawn fails closed immediately, parking nothing.
pub struct AppStore {
    state: AtomicU8,
    anchor: [u8; 32],
    /// The semantic launch cache, absent until the boot path that
    /// publishes the `/System` mount installs its budget and pressure
    /// gauge; an uninstalled cache serves every launch uncached.
    cache: RwLock<Option<LaunchCache>>,
}

impl AppStore {
    /// A store that starts *pending*, trusting exactly the Ed25519 signer
    /// `anchor`; the boot path that installs it promises to resolve it.
    #[must_use]
    pub const fn pending(anchor: [u8; 32]) -> Self {
        Self {
            state: AtomicU8::new(STORE_PENDING),
            anchor,
            cache: RwLock::new(None),
        }
    }

    /// The Ed25519 public key every store bundle's manifest must be signed
    /// by — the build's embedded application trust anchor.
    #[must_use]
    pub const fn anchor(&self) -> [u8; 32] {
        self.anchor
    }

    /// Resolve the latch: the `/System` mount was published. Wakes every
    /// parked spawn. One-way: a later call cannot un-resolve it.
    pub fn note_available(&self) {
        self.resolve(STORE_AVAILABLE);
    }

    /// Resolve the latch: this boot has no readable store. Wakes every
    /// parked spawn so it fails closed instead of waiting forever.
    pub fn note_unavailable(&self) {
        self.resolve(STORE_UNAVAILABLE);
    }

    fn resolve(&self, terminal: u8) {
        // Only the first resolution wins, so a late duplicate can never
        // flip an answered latch; every resolution wakes the waiters.
        let _ = self.state.compare_exchange(
            STORE_PENDING,
            terminal,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        crate::waitq::app_store_wake();
    }

    /// True while the boot path has not yet resolved the latch.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == STORE_PENDING
    }

    /// True once the latch resolved to a published, readable store.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.state.load(Ordering::Acquire) == STORE_AVAILABLE
    }

    /// True when `bundle` lies inside one of the immutable, read-only
    /// system stores, whose contents cannot change for the life of the
    /// boot — the only bundles whose verification result may be cached.
    /// Every program kind's store is such a store, so the set is derived
    /// from the one store definition rather than listed again here.
    ///
    /// A bundle on a writable volume (an installed bundle under `/Apps`, a
    /// bundle in a user's own store) is never cached: its bytes can change
    /// between launches, so every launch re-verifies it through the full
    /// load gate.
    #[must_use]
    pub fn cacheable_bundle(bundle: &str) -> bool {
        ProgramKind::ALL.iter().any(|kind| {
            bundle
                .strip_prefix(kind.store())
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// Install the semantic launch cache: bounded by `budget`, governed
    /// by the system `pressure` gauge, and classified through the
    /// `kernel/mem::reclaim` admission gate (`plans/SMARTRAM.md`
    /// SMART4). Called once by the boot path that publishes the
    /// `/System` mount, before it resolves the readiness latch; only the
    /// first installation wins. Until it runs, every launch is served
    /// uncached through the full load gate.
    pub fn install_reclaim(
        &self,
        budget: CacheBudget,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) {
        let mut cache = self.cache.write();
        if cache.is_none() {
            let launch = LaunchCache::new(budget, pressure, sink);
            // The installed launch cache registers its ledger with the
            // System Information memory-statistics registry
            // (observation-only); only the winning install registers. A
            // `None` means classification refused the cache at birth (it is
            // then poisoned and admits nothing), so there is nothing to
            // register — the refusal is already in the audit log.
            if let Some(ledger) = launch.ledger() {
                crate::memstats::MEM_STATS.register_ledger(ledger);
            }
            *cache = Some(launch);
        }
    }

    /// The cached verification result for `bundle`, refreshing its LRU
    /// stamp, or `None` when the bundle has not been verified this boot,
    /// its entry was reclaimed under pressure, or no cache is installed.
    ///
    /// A hit is proof the load gate accepted exactly these bytes earlier
    /// this boot from the immutable read-only store; it says nothing about
    /// the *caller*, whose read of the entry point the spawn path still
    /// authorises through the secured VFS before serving the hit.
    #[must_use]
    pub fn cached(&self, bundle: &str) -> Option<Arc<LoadedApp>> {
        self.cache.write().as_mut()?.lookup(bundle)
    }

    /// Whether a verified image for `bundle` is currently held — an
    /// advisory existence peek that does not disturb the cache (no LRU
    /// restamp, no hit/miss accounting, no serving), taking only a read
    /// lock ([`LaunchCache::contains`]).
    ///
    /// The synchronous spawn probe uses this to skip its filesystem
    /// existence lookup when the bundle is already cached: a hit is proof
    /// the load gate accepted these immutable read-only-store bytes earlier
    /// this boot, so the bundle certainly exists. It says nothing about the
    /// *caller* and grants no authority — the caller's read of the entry
    /// point is still authorised by the deferred load through the secured
    /// VFS, so the cache can never widen access.
    #[must_use]
    pub fn cached_present(&self, bundle: &str) -> bool {
        self.cache
            .read()
            .as_ref()
            .is_some_and(|cache| cache.contains(bundle))
    }

    /// Record `app` as the verified result for `bundle`. Admission and
    /// eviction follow the cache's classified budget and pressure policy
    /// ([`LaunchCache::insert`]); with no cache installed the result is
    /// served uncached.
    pub fn cache_verified(&self, bundle: &str, app: &Arc<LoadedApp>) {
        if let Some(cache) = self.cache.write().as_mut() {
            cache.insert(bundle, app);
        }
    }
}

/// Largest single bundle file the store will read (16 MiB). A validation
/// bound on untrusted volume contents, not a scalable capacity: a `Run`
/// rxe or `Resources/` asset beyond it is a hostile or corrupt bundle and
/// the whole load fails closed.
const BUNDLE_FILE_MAX: usize = 16 << 20;

/// A bundle file is read whole into one contiguous heap buffer
/// ([`FsBundleStore::read_file`]), so the kernel heap must be able to grow
/// a region large enough to hold the largest one. The heap's frame-backed
/// growth draws a single power-of-two frame block capped at
/// [`MAX_ORDER`]; a `BUNDLE_FILE_MAX` request rounds up to the next power
/// of two (allocator header + alignment push a full 16 MiB just over
/// 4096 pages), so the largest growable block must be at least twice
/// `BUNDLE_FILE_MAX`. This ties the two constants together at compile time
/// so raising `BUNDLE_FILE_MAX` without a matching `MAX_ORDER` — which
/// would silently reintroduce the load-fails-once-fragmented defect — does
/// not build.
const _: () = assert!(
    (1usize << MAX_ORDER) * PAGE_SIZE >= 2 * BUNDLE_FILE_MAX,
    "MAX_ORDER too small: the kernel heap cannot grow a region large enough \
     to read a BUNDLE_FILE_MAX bundle file",
);

/// Largest total byte count of one bundle's hashed contents (32 MiB, the
/// size of the whole `/System` store volume today). Bounds the kernel
/// memory one verification may hold; exceeding it fails the load closed.
const BUNDLE_TOTAL_MAX: usize = 32 << 20;

/// Largest wire `AppInfo` the fixed header grammar can describe: the
/// header plus a full capability body and a full MIME table. Anything
/// longer cannot decode and is refused before it is read.
const APPINFO_MAX: usize = AppInfoHeader::WIRE_LEN
    + (APPINFO_MAX_CAPABILITIES as usize) * 2
    + (APPINFO_MAX_MIME as usize) * MIME_ENTRY_LEN;

/// Deepest directory nesting the content-hash walk follows inside a
/// bundle. The fixed layout needs three levels (`Help/<locale>/<doc>`);
/// eight leaves headroom for `Resources/` trees while still refusing a
/// hostile unbounded recursion closed.
const WALK_DEPTH_MAX: usize = 8;

/// Most files one bundle may carry. Bounds the walk's bookkeeping; a tree
/// beyond it is refused closed.
const WALK_FILES_MAX: usize = 4096;

/// The [`BundleStore`] over the kernel's mounted, secured VFS.
///
/// Every read happens under the **caller's** kernel-attested identity
/// (`uid` + effective capability set), so the per-inode owner/mode/ACL and
/// mount-flag checks stay kernel-side and a caller can never launch a
/// bundle it could not read. The store adds no authority and trusts no
/// result: sizes, depths, and file counts are bounded fail-closed.
pub struct FsBundleStore<'a> {
    fs: &'a dyn FilesystemService,
    uid: u32,
    caps: &'a dyn CapabilityQuery,
}

impl<'a> FsBundleStore<'a> {
    /// A store reading through `fs` as the attested caller (`uid`, `caps`).
    #[must_use]
    pub fn new(fs: &'a dyn FilesystemService, uid: u32, caps: &'a dyn CapabilityQuery) -> Self {
        Self { fs, uid, caps }
    }

    /// Read the whole regular file at `path`, refusing one longer than
    /// `max_len` rather than allocating without bound.
    ///
    /// The exact size is learned from `stat` first, so the destination is
    /// reserved **once, to the exact length**, through the fallible
    /// [`Vec::try_reserve_exact`] — never grown by the infallible
    /// doubling `extend_from_slice` uses, which both wastes up to a whole
    /// second copy of a large image (a `Run` binary near `max_len` would
    /// double a heap request to the next power of two) and aborts the
    /// kernel on exhaustion instead of failing closed. Bytes are read
    /// straight into that buffer, so a `max_len`-sized bundle file is one
    /// allocation of its own length and no bounce copy. A read that is
    /// short of the stated size (a file truncated under us) is honoured by
    /// returning only what was read; one that runs past it (a file that
    /// grew) stops at the stated size and the extra is refused closed by
    /// the caller's own length checks.
    fn read_file(&self, path: &str, max_len: usize) -> Result<Vec<u8>, Errno> {
        // A bundle is self-contained, so this read keeps a final link rather
        // than following one: every part of the app is a real file inside its
        // own folder, and a link could name something outside the bundle that
        // the signature never covered. Enforced here, at the one read every
        // bundle file goes through, rather than depending on each caller
        // having checked a listing's kind first.
        let stat = self.fs.stat(self.uid, self.caps, path, FinalLink::Keep)?;
        if stat.kind == FileKind::Symlink {
            return Err(Errno::NotSupported);
        }
        let size = usize::try_from(stat.size).map_err(|_| Errno::LengthOutOfRange)?;
        if size > max_len {
            return Err(Errno::LengthOutOfRange);
        }
        let mut out = Vec::new();
        // Fail closed on exhaustion rather than aborting the kernel: the
        // one heap request for the whole file is the largest a bundle load
        // makes, and it must be a `Result`, never a panic.
        out.try_reserve_exact(size)
            .map_err(|_| Errno::OutOfMemory)?;
        // `resize` cannot reallocate: capacity is already `>= size`.
        out.resize(size, 0);
        let mut filled = 0usize;
        while filled < size {
            let read =
                self.fs
                    .read(self.uid, self.caps, path, filled as u64, &mut out[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        out.truncate(filled);
        Ok(out)
    }

    /// Walk the bundle tree under `root`, appending every regular file's
    /// bundle-relative path and bytes to `files` (excluding the top-level
    /// `AppInfo`, which the signature does not cover).
    fn collect_files(
        &self,
        root: &str,
        rel_dir: &str,
        depth: usize,
        total: &mut usize,
        files: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), Errno> {
        if depth > WALK_DEPTH_MAX {
            return Err(Errno::OutOfRange);
        }
        let abs_dir = if rel_dir.is_empty() {
            root.to_owned()
        } else {
            format!("{root}/{rel_dir}")
        };
        for entry in self
            .fs
            .readdir(self.uid, self.caps, &abs_dir, FinalLink::Follow)?
        {
            let name = entry.name;
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            match entry.kind {
                FileKind::Regular => {
                    if rel == "AppInfo" {
                        continue;
                    }
                    if files.len() >= WALK_FILES_MAX {
                        return Err(Errno::OutOfRange);
                    }
                    let bytes = self.read_file(&format!("{root}/{rel}"), BUNDLE_FILE_MAX)?;
                    *total = total.saturating_add(bytes.len());
                    if *total > BUNDLE_TOTAL_MAX {
                        return Err(Errno::LengthOutOfRange);
                    }
                    files.push((rel, bytes));
                }
                FileKind::Directory => {
                    self.collect_files(root, &rel, depth + 1, total, files)?;
                }
                // A bundle is self-contained: every part of the app is a
                // real file inside its own folder. A link could name
                // something outside the bundle the signature never covered,
                // so the whole bundle is refused rather than partly loaded.
                FileKind::Symlink => return Err(Errno::NotSupported),
            }
        }
        Ok(())
    }
}

impl BundleStore for FsBundleStore<'_> {
    fn entries(&self, bundle: &str) -> Result<Vec<String>, Errno> {
        Ok(self
            .fs
            .readdir(self.uid, self.caps, bundle, FinalLink::Follow)?
            .into_iter()
            .map(|entry| entry.name)
            .collect())
    }

    fn read_appinfo(&self, bundle: &str) -> Result<Vec<u8>, Errno> {
        self.read_file(&format!("{bundle}/AppInfo"), APPINFO_MAX)
    }

    fn contents(&self, bundle: &str) -> Result<BundleContents, Errno> {
        let mut files = Vec::new();
        let mut total = 0usize;
        self.collect_files(bundle, "", 0, &mut total, &mut files)?;
        // The canonical framing requires strictly ascending paths; one
        // directory tree cannot yield duplicates, so a byte sort suffices.
        files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let digests: Vec<BundleFileDigest<'_>> = files
            .iter()
            .map(|(path, bytes)| BundleFileDigest { path, bytes })
            .collect();
        let mut hasher = Sha256Stream::new();
        digest_bundle_contents(&digests, &mut |chunk| hasher.update(chunk))?;
        let content_hash = hasher.finalize();
        drop(digests);
        // The `Run` binary is one of the files the walk just read and
        // hashed, so hand its bytes straight back rather than reading the
        // whole file a second time. Its layout presence is guaranteed by the
        // loader's layout check; a bundle missing it fails closed here too.
        let run_image = files
            .into_iter()
            .find(|(path, _)| path == "Run")
            .map(|(_, bytes)| bytes)
            .ok_or(Errno::NotFound)?;
        Ok(BundleContents {
            content_hash,
            run_image,
        })
    }
}

/// The [`Verifier`] rooted at the build's embedded application trust
/// anchor.
///
/// The manifest names its own signer, so trust-rooting is this verifier's
/// whole job: a signer other than the anchor is refused before any
/// cryptography runs, and the Ed25519 signature is then verified through
/// the audited `lib/crypto` wrapper. Fails closed on every deviation.
pub struct AnchorVerifier {
    anchor: [u8; 32],
}

impl AnchorVerifier {
    /// A verifier trusting exactly the Ed25519 public key `anchor`.
    #[must_use]
    pub const fn new(anchor: [u8; 32]) -> Self {
        Self { anchor }
    }
}

impl Verifier for AnchorVerifier {
    fn verify(
        &self,
        signed: &[u8],
        signature: &[u8; 64],
        signer_pubkey: &[u8; 32],
    ) -> Result<(), Errno> {
        if *signer_pubkey != self.anchor {
            return Err(Errno::SignatureInvalid);
        }
        let key =
            Ed25519PublicKey::from_bytes(signer_pubkey).map_err(|_| Errno::SignatureInvalid)?;
        key.verify(signed, &Ed25519Signature(*signature))
            .map_err(|_| Errno::SignatureInvalid)
    }
}

/// The [`Clock`] the load gate reads to time its phases, backed by the
/// architecture monotonic clock.
///
/// It reports the current CPU's monotonic nanoseconds on every read (the
/// same source `clock_get` and the wait-queue deadlines use), so the
/// [`tairix_appload::events::APP_LOADED`] record can attribute a slow first
/// launch to disk reads versus verification. The reading is audit-only and
/// never affects a load decision.
pub struct ArchClock<'a, A: KernelArch> {
    arch: &'a A,
}

impl<'a, A: KernelArch> ArchClock<'a, A> {
    /// A clock reading the monotonic time source of `arch`.
    #[must_use]
    pub const fn new(arch: &'a A) -> Self {
        Self { arch }
    }
}

impl<A: KernelArch> Clock for ArchClock<'_, A> {
    fn now_ns(&self) -> u64 {
        self.arch
            .monotonic_ns(SchedulerArch::current_cpu(self.arch))
    }
}

/// A spawnable store-bundle entry-point path, split into the bundle root
/// the load gate judges and the command name the child is attested as.
#[derive(Debug, Eq, PartialEq)]
pub struct BundleRunPath<'a> {
    /// The bundle root directory (e.g. `/System/Commands/ps.app`).
    pub bundle: &'a str,
    /// The bundle directory's stem — the command/program name (e.g. `ps`).
    pub command: &'a str,
}

/// Parse `path` as an absolute `…/<Name>.app/Run` bundle entry point.
///
/// The shape is strict and fails closed: UTF-8, absolute, ending in the
/// bundle's `Run` entry directly under a `<Name>.app` directory with a
/// non-empty stem, and with no empty, `.`, or `..` component anywhere (no
/// traversal). Anything else is `None` — the caller reports `NotFound`
/// without touching the filesystem.
#[must_use]
pub fn bundle_run_path(path: &[u8]) -> Option<BundleRunPath<'_>> {
    let path = core::str::from_utf8(path).ok()?;
    let rest = path.strip_prefix('/')?;
    if rest
        .split('/')
        .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return None;
    }
    let bundle = path.strip_suffix("/Run")?;
    let dir = bundle.rsplit('/').next()?;
    let stem = dir.strip_suffix(BUNDLE_SUFFIX)?;
    if stem.is_empty() {
        return None;
    }
    Some(BundleRunPath {
        bundle,
        command: stem,
    })
}

/// Map a load-gate refusal onto the stable [`Errno`] the `spawn` syscall
/// reports. Every arm is a refusal; the structural refusals (layout,
/// library policy) and any unforeseen (`#[non_exhaustive]`) variant fall
/// through to a closed `PermissionDenied`.
#[must_use]
pub fn app_error_errno(err: AppError) -> Errno {
    match err {
        AppError::Store(e) | AppError::Manifest(e) => e,
        AppError::InterfaceHashMismatch => Errno::AbiVersionUnsupported,
        AppError::Signature
        | AppError::PublisherCert
        | AppError::ContentHashMismatch
        | AppError::RunImage(_) => Errno::SignatureInvalid,
        _ => Errno::PermissionDenied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_bundle::{
        composed_bundle, composed_bundle_published_by, composed_bundle_signed_by, MemFs,
    };
    use crate::test_sink::TestSink;
    use tairix_abi::rxe::{LoadHeader, Segment};
    use tairix_abi::{BundleLayoutError, CapabilityId, RxeError, ABI_VERSION_CURRENT};
    use tairix_appload::{AppLoader, AppLoaderConfig};
    use tairix_caps::CapabilitySet;
    use tairix_itest_harness::app_image::PublisherSource;
    use tairix_kernel_syscall::SYSCALL_TABLE_HASH;

    extern crate std;
    use std::boxed::Box;
    use std::string::ToString;
    use std::vec;

    /// A `CapabilityQuery` granting nothing — the mock filesystem enforces
    /// no permissions, so the store tests need no authority.
    struct NoCaps;
    impl CapabilityQuery for NoCaps {
        fn holds(&self, _cap: CapabilityId) -> bool {
            false
        }
    }

    /// A fixed clock: these tests assert the load *outcome*, not the timing,
    /// so a constant reading (zero-length phases) is sufficient.
    struct TestClock;
    impl Clock for TestClock {
        fn now_ns(&self) -> u64 {
            0
        }
    }

    fn load(
        fs: &MemFs,
        anchor: [u8; 32],
    ) -> Result<tairix_appload::LoadedApp, tairix_appload::AppError> {
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let store = FsBundleStore::new(fs, 1000, &NoCaps);
        let verifier = AnchorVerifier::new(anchor);
        let clock = TestClock;
        let loader = AppLoader::new(AppLoaderConfig {
            accepted_abi_version: ABI_VERSION_CURRENT,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            store: &store,
            verifier: &verifier,
            clock: &clock,
            sink,
        });
        loader.load(
            "/System/Commands/ps.app",
            &CapabilitySet::from_words([u64::MAX; 4]),
        )
    }

    #[test]
    fn a_composed_bundle_loads_end_to_end_with_its_manifest_request() {
        let (fs, anchor, run) =
            composed_bundle(vec![CapabilityId::CONSOLE_WRITE, CapabilityId::FS_ACCESS]);
        let app = load(&fs, anchor).expect("loads");
        assert_eq!(app.id(), "os.tairix.ps");
        // The full-word grant set is the intersection identity, so the
        // granted ceiling is exactly the manifest request.
        assert!(app.granted().contains(CapabilityId::CONSOLE_WRITE));
        assert!(app.granted().contains(CapabilityId::FS_ACCESS));
        assert_eq!(app.granted().len(), 2);
        // The spawnable image is byte-for-byte the on-disk `Run`.
        assert_eq!(app.run_image(), run.as_slice());
        // Every bundle the build plants delegates, so the gate's certificate
        // check ran and attributed the bundle to a developer.
        assert!(!app.publisher().is_none());
    }

    /// The store keys per-app state on the publisher, not on the build key,
    /// so re-signing a release must leave the identity untouched — otherwise
    /// an update would silently orphan the user's settings, secrets and
    /// blobs. This is the property the whole publisher/signer split exists
    /// for, so it is proved end-to-end through the real composer and gate.
    #[test]
    fn re_signing_a_release_keeps_the_publisher_identity() {
        let (first_fs, first_anchor, _) = composed_bundle_signed_by(&[7u8; 32], vec![]);
        let (next_fs, next_anchor, _) = composed_bundle_signed_by(&[21u8; 32], vec![]);
        assert_ne!(first_anchor, next_anchor, "the build key really rotated");

        let first = load(&first_fs, first_anchor).expect("loads");
        let next = load(&next_fs, next_anchor).expect("loads");
        assert_eq!(first.publisher(), next.publisher());
        assert!(!first.publisher().is_none());
    }

    /// A certificate delegates one signing key, and only that one. An
    /// attacker holding a signing key the trust root admits must not be able
    /// to reuse a publisher's genuine certificate — a real signature, just
    /// over a different message — to claim that publisher's per-app store.
    ///
    /// The forged bundle is composed *properly signed*, so the manifest
    /// signature passes and the refusal can only come from the certificate no
    /// longer covering this signer.
    #[test]
    fn a_genuine_certificate_does_not_transfer_to_another_signing_key() {
        let (donor_fs, _, _) = composed_bundle_signed_by(&[7u8; 32], vec![]);
        let donor = AppInfoHeader::from_bytes(
            donor_fs
                .files
                .get("/System/Commands/ps.app/AppInfo")
                .expect("donor manifest"),
        )
        .expect("donor decodes");

        let (fs, anchor, _) = composed_bundle_published_by(
            &[21u8; 32],
            PublisherSource::Certificate {
                pubkey: donor.publisher_pubkey,
                cert: donor.publisher_cert,
            },
            vec![],
        );
        assert_eq!(load(&fs, anchor), Err(AppError::PublisherCert));
    }

    #[test]
    fn the_run_binary_is_read_from_disk_exactly_once() {
        // Regression: the load gate used to read `Run` twice — once in the
        // content-hash walk and again to fetch the entry-point image — which
        // doubled the disk I/O for the biggest file in a command bundle. The
        // hash walk now hands the `Run` bytes back, so `Run` is read exactly
        // as many times as any other single-pass bundle file (here the help
        // document, read only during the walk), and never more.
        let (fs, anchor, _) = composed_bundle(vec![]);
        load(&fs, anchor).expect("loads");
        let run_reads = fs.read_calls("/System/Commands/ps.app/Run");
        let help_reads = fs.read_calls("/System/Commands/ps.app/Help/en-US/ps.md");
        assert!(run_reads > 0, "the Run image must actually be read");
        assert_eq!(
            run_reads, help_reads,
            "Run is read once (the content-hash walk), not a second time for the entry image"
        );
    }

    #[test]
    fn a_symlinked_bundle_file_is_refused_rather_than_read_through() {
        // A bundle is self-contained, so a link inside one could name
        // something outside it that the signature never covered. The
        // manifest is the sharp case: it is read *before* the content walk
        // that refuses a link entry, so the refusal has to live at the read
        // itself rather than in a caller's kind check.
        for path in [
            "/System/Commands/ps.app/AppInfo",
            "/System/Commands/ps.app/Run",
        ] {
            let (fs, anchor, _) = composed_bundle(vec![]);
            let linked = fs.with_link(path);
            assert_eq!(
                load(&linked, anchor),
                Err(AppError::Store(Errno::NotSupported)),
                "a symlinked {path} must be refused, never read through"
            );
        }
    }

    #[test]
    fn a_tampered_run_fails_the_content_hash() {
        let (mut fs, anchor, _) = composed_bundle(vec![]);
        fs.files
            .get_mut("/System/Commands/ps.app/Run")
            .expect("run present")[LoadHeader::WIRE_LEN + Segment::WIRE_LEN] ^= 0xFF;
        assert_eq!(
            load(&fs, anchor),
            Err(AppError::ContentHashMismatch),
            "a flipped byte in Run must break the signed content hash"
        );
    }

    #[test]
    fn a_foreign_signer_is_refused_before_cryptography() {
        let (fs, _anchor, _) = composed_bundle(vec![]);
        // The store bundle is internally consistent, but the kernel's
        // anchor is a different key: trust-rooting must refuse it.
        assert_eq!(load(&fs, [0x42; 32]), Err(AppError::Signature));
    }

    #[test]
    fn a_corrupted_signature_is_refused() {
        let (mut fs, anchor, _) = composed_bundle(vec![]);
        let appinfo = fs
            .files
            .get_mut("/System/Commands/ps.app/AppInfo")
            .expect("manifest present");
        // Flip a bit inside the trailing signature field.
        let sig_start = AppInfoHeader::signed_range().end;
        appinfo[sig_start] ^= 0x01;
        assert_eq!(load(&fs, anchor), Err(AppError::Signature));
    }

    #[test]
    fn the_content_hash_walk_refuses_a_hostile_deep_tree() {
        let (mut fs, anchor, _) = composed_bundle(vec![]);
        // Nest a file beyond the walk's depth bound inside Resources/.
        let deep = "/System/Commands/ps.app/Resources/a/b/c/d/e/f/g/h/i/j/x";
        fs.files.insert(deep.to_string(), vec![1]);
        let err = load(&fs, anchor).expect_err("deep tree refused");
        assert!(matches!(err, AppError::Store(Errno::OutOfRange)));
    }

    #[test]
    fn an_oversized_appinfo_is_refused_before_decoding() {
        let (mut fs, anchor, _) = composed_bundle(vec![]);
        fs.files.insert(
            "/System/Commands/ps.app/AppInfo".to_string(),
            vec![0u8; APPINFO_MAX + 1],
        );
        let err = load(&fs, anchor).expect_err("oversized manifest refused");
        assert!(matches!(err, AppError::Store(Errno::LengthOutOfRange)));
    }

    #[test]
    fn bundle_run_path_accepts_store_and_user_bundles() {
        let parsed = bundle_run_path(b"/System/Commands/ps.app/Run").expect("store bundle");
        assert_eq!(parsed.bundle, "/System/Commands/ps.app");
        assert_eq!(parsed.command, "ps");
        let parsed = bundle_run_path(b"/Apps/Example.app/Run").expect("user bundle");
        assert_eq!(parsed.bundle, "/Apps/Example.app");
        assert_eq!(parsed.command, "Example");
        let parsed = bundle_run_path(b"/System/Services/login.app/Run").expect("service bundle");
        assert_eq!(parsed.command, "login");
    }

    #[test]
    fn bundle_run_path_rejects_every_malformed_shape() {
        for path in [
            &b"ps.app/Run"[..],                  // relative
            b"/System/Commands/ps.app",          // no /Run
            b"/System/Commands/ps/Run",          // bundle dir not .app
            b"/System/Commands/.app/Run",        // empty stem
            b"/System/Commands/ps.app/Code/Run", // Run not at bundle root
            b"/System/../Apps/ps.app/Run",       // traversal
            b"/System//Apps/ps.app/Run",         // empty component
            b"/System/./Apps/ps.app/Run",        // dot component
            b"/Run",                             // no bundle at all
            b"\xFF\xFEbad/ps.app/Run",           // not UTF-8
        ] {
            assert!(bundle_run_path(path).is_none(), "{path:?}");
        }
    }

    #[test]
    fn the_readiness_latch_resolves_one_way() {
        let store = AppStore::pending([9u8; 32]);
        assert!(store.is_pending());
        assert!(!store.is_available());
        store.note_available();
        assert!(!store.is_pending());
        assert!(store.is_available());
        // A late duplicate resolution cannot flip an answered latch.
        store.note_unavailable();
        assert!(store.is_available());
        assert_eq!(store.anchor(), [9u8; 32]);

        let dead = AppStore::pending([9u8; 32]);
        dead.note_unavailable();
        assert!(!dead.is_pending());
        assert!(!dead.is_available());
        dead.note_available();
        assert!(!dead.is_available());
    }

    #[test]
    fn only_readonly_system_store_bundles_are_cacheable() {
        assert!(AppStore::cacheable_bundle("/System/Commands/ps.app"));
        assert!(AppStore::cacheable_bundle("/System/Applications/files.app"));
        assert!(AppStore::cacheable_bundle("/System/Services/login.app"));
        // A writable-volume bundle can change between launches.
        assert!(!AppStore::cacheable_bundle("/Apps/Example.app"));
        assert!(!AppStore::cacheable_bundle("/Users/ada/Commands/own.app"));
        // A sibling directory sharing a store's prefix is not the store.
        assert!(!AppStore::cacheable_bundle("/System/CommandsEvil/ps.app"));
        assert!(!AppStore::cacheable_bundle(
            "/System/ApplicationsEvil/files.app"
        ));
        // A store root itself is a directory, not a bundle.
        assert!(!AppStore::cacheable_bundle("/System/Commands"));
        assert!(!AppStore::cacheable_bundle("/System/Applications"));
        assert!(!AppStore::cacheable_bundle(
            "/Users/mallory/System/Commands/ps.app"
        ));
    }

    #[test]
    fn app_errors_map_to_stable_fail_closed_errnos() {
        assert_eq!(
            app_error_errno(AppError::Store(Errno::NotFound)),
            Errno::NotFound
        );
        assert_eq!(
            app_error_errno(AppError::Manifest(Errno::BadMagic)),
            Errno::BadMagic
        );
        assert_eq!(
            app_error_errno(AppError::InterfaceHashMismatch),
            Errno::AbiVersionUnsupported
        );
        assert_eq!(
            app_error_errno(AppError::Signature),
            Errno::SignatureInvalid
        );
        assert_eq!(
            app_error_errno(AppError::PublisherCert),
            Errno::SignatureInvalid
        );
        assert_eq!(
            app_error_errno(AppError::ContentHashMismatch),
            Errno::SignatureInvalid
        );
        assert_eq!(
            app_error_errno(AppError::RunImage(RxeError::BadMagic)),
            Errno::SignatureInvalid
        );
        assert_eq!(
            app_error_errno(AppError::Layout(BundleLayoutError::UnknownEntry)),
            Errno::PermissionDenied
        );
    }
}
