//! Shared host-test fixtures for the on-disk application-bundle spawn path:
//! an in-memory [`FilesystemService`] and a signed-bundle composer, used by
//! both the `appspawn` unit tests and the `spawn` syscall-handler tests so
//! the fake volume is defined once.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
use tairix_abi::{
    BundleFileDigest, CapabilityId, CapabilityQuery, Errno, FileKind, FileStat, OpenFlags,
    UnlinkFlags, ABI_VERSION_CURRENT, LOAD_MAGIC,
};
use tairix_appload::{AppError, AppLoader, AppLoaderConfig, Clock, LoadedApp};
use tairix_caps::CapabilitySet;
use tairix_itest_harness::app_image::{compose_signed_appinfo, AppKind, AppManifestSource};
use tairix_kernel_syscall::SYSCALL_TABLE_HASH;

use crate::appspawn::{AnchorVerifier, FsBundleStore};
use crate::fs::{FilesystemService, ReaddirEntry};
use crate::test_sink::TestSink;

extern crate std;
use std::collections::BTreeMap;

/// The deterministic test signing seed; its derived public key is the trust
/// anchor the tests pin.
pub(crate) const SEED: [u8; 32] = [7u8; 32];

/// An in-memory [`FilesystemService`] over a fixed file map. Read-only:
/// every mutating operation fails closed, mirroring the read paths the
/// bundle store actually exercises.
///
/// `read_calls` counts, per path, how many times [`FilesystemService::read`]
/// was invoked, so a test can prove a file (e.g. `Run`) is read from disk
/// the expected number of times.
pub(crate) struct MemFs {
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    read_calls: std::sync::Mutex<BTreeMap<String, usize>>,
}

impl MemFs {
    pub(crate) fn new(files: &[(&str, &[u8])]) -> Self {
        Self {
            files: files
                .iter()
                .map(|(path, bytes)| ((*path).to_string(), bytes.to_vec()))
                .collect(),
            read_calls: std::sync::Mutex::new(BTreeMap::new()),
        }
    }

    /// How many times [`FilesystemService::read`] was called for `path`.
    pub(crate) fn read_calls(&self, path: &str) -> usize {
        self.read_calls
            .lock()
            .expect("read-call counter not poisoned")
            .get(path)
            .copied()
            .unwrap_or(0)
    }

    /// The immediate children of `dir`, derived from the file paths.
    fn children(&self, dir: &str) -> Vec<ReaddirEntry> {
        let prefix = if dir.ends_with('/') {
            dir.to_string()
        } else {
            format!("{dir}/")
        };
        let mut out: Vec<ReaddirEntry> = Vec::new();
        for (path, body) in &self.files {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            let (kind, name, size) = match rest.split_once('/') {
                Some((first, _)) => (FileKind::Directory, first, 0),
                None => (FileKind::Regular, rest, body.len() as u64),
            };
            if !out.iter().any(|e| e.name == name) {
                out.push(ReaddirEntry {
                    kind,
                    size,
                    allocated: size,
                    modified: tairix_abi::time::Time64::UNIX_EPOCH,
                    name: name.to_string(),
                });
            }
        }
        out
    }
}

impl FilesystemService for MemFs {
    fn open(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        path: &str,
        flags: OpenFlags,
    ) -> Result<(), Errno> {
        // Read-only fixture: a read open of an existing file resolves, any
        // mutating open fails closed like every other mutating operation.
        if flags.contains(OpenFlags::WRITE) || flags.contains(OpenFlags::CREATE) {
            return Err(Errno::NotImplemented);
        }
        if self.files.contains_key(path) {
            Ok(())
        } else {
            Err(Errno::NotFound)
        }
    }

    fn read(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        path: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Errno> {
        *self
            .read_calls
            .lock()
            .expect("read-call counter not poisoned")
            .entry(path.to_string())
            .or_insert(0) += 1;
        let bytes = self.files.get(path).ok_or(Errno::NotFound)?;
        let start = usize::try_from(offset).map_err(|_| Errno::OutOfRange)?;
        if start >= bytes.len() {
            return Ok(0);
        }
        let read = buf.len().min(bytes.len() - start);
        buf[..read].copy_from_slice(&bytes[start..start + read]);
        Ok(read)
    }

    fn write(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _offset: u64,
        _append: bool,
        _data: &[u8],
    ) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }

    fn readdir(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        path: &str,
    ) -> Result<Vec<ReaddirEntry>, Errno> {
        let children = self.children(path);
        if children.is_empty() {
            return Err(Errno::NotFound);
        }
        Ok(children)
    }

    fn stat(&self, _uid: u32, _caps: &dyn CapabilityQuery, _path: &str) -> Result<FileStat, Errno> {
        Err(Errno::NotImplemented)
    }

    fn truncate(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _size: u64,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn sync(&self, _uid: u32, _caps: &dyn CapabilityQuery) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn mkdir(&self, _uid: u32, _caps: &dyn CapabilityQuery, _path: &str) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn unlink(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _flags: UnlinkFlags,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn rename(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _src: &str,
        _dst: &str,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn set_mode(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _mode: u32,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    // The fixture's flat map stores no extended attributes; every attribute
    // operation answers the typed unsupported-backing refusal.
    fn attr_get(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
        _value_out: &mut [u8],
    ) -> Result<usize, Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_set(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
        _value: &[u8],
    ) -> Result<(), Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_list(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _index: u64,
        _key_out: &mut [u8],
    ) -> Result<Option<usize>, Errno> {
        Err(Errno::NotSupported)
    }

    fn attr_remove(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
    ) -> Result<(), Errno> {
        Err(Errno::NotSupported)
    }
}

/// A minimal valid single-segment PIE `rxe` whose CFI tag is the kernel's
/// compiled-in syscall-table hash — exactly what the load gate accepts
/// (mirrors `crate::spawn`'s `tiny_image` fixture, retagged).
pub(crate) fn tiny_run() -> Vec<u8> {
    let seg = Segment {
        vaddr: 0x1000,
        file_offset: (LoadHeader::WIRE_LEN + Segment::WIRE_LEN) as u64,
        file_size: 4,
        mem_size: 4096,
        permission: RxePermission::ReadExecute,
    };
    let header = LoadHeader {
        magic: LOAD_MAGIC,
        abi_version: ABI_VERSION_CURRENT,
        flags: LOAD_FLAG_PIE,
        segment_count: 1,
        needed_count: 0,
        entry: 0x1000,
        cfi_tag: SYSCALL_TABLE_HASH,
    };
    let mut rxe = Vec::new();
    rxe.extend_from_slice(&header.to_le_bytes());
    rxe.extend_from_slice(&seg.to_le_bytes());
    rxe.extend_from_slice(&[0x13, 0x00, 0x00, 0x00]);
    rxe
}

/// Compose a signed `ps` bundle (manifest + `Run` + one help document) in a
/// [`MemFs`] under `/System/Apps/ps.app`, returning the filesystem, the
/// signer's public key, and the `Run` bytes.
///
/// The `AppInfo` is composed and signed by the **same** host composer the
/// image build uses, so the kernel store/verifier and the composer can
/// never drift.
pub(crate) fn composed_bundle(caps: Vec<CapabilityId>) -> (MemFs, [u8; 32], Vec<u8>) {
    let run = tiny_run();
    let help = b"# ps\n";
    let manifest = AppManifestSource {
        id: "os.tairix.ps".to_string(),
        name: "ps".to_string(),
        version: "1.0".to_string(),
        kind: AppKind::Command,
        capabilities: caps,
    };
    let composed = compose_signed_appinfo(
        &SEED,
        &manifest,
        SYSCALL_TABLE_HASH,
        &[
            BundleFileDigest {
                path: "Help/en-US/ps.md",
                bytes: help,
            },
            BundleFileDigest {
                path: "Run",
                bytes: &run,
            },
        ],
    )
    .expect("composes");
    let fs = MemFs::new(&[
        ("/System/Apps/ps.app/AppInfo", composed.bytes.as_slice()),
        ("/System/Apps/ps.app/Run", run.as_slice()),
        ("/System/Apps/ps.app/Help/en-US/ps.md", help.as_slice()),
    ]);
    (fs, composed.signer_pubkey, run)
}

/// A `CapabilityQuery` granting nothing — the mock filesystem enforces
/// no permissions.
pub(crate) struct NoCaps;
impl CapabilityQuery for NoCaps {
    fn holds(&self, _cap: CapabilityId) -> bool {
        false
    }
}

/// A fixed clock: this fixture asserts load *outcomes*, not timing, so a
/// constant reading (zero-length phases) is sufficient.
struct NullClock;
impl Clock for NullClock {
    fn now_ns(&self) -> u64 {
        0
    }
}

/// Run the full `tairix_appload` gate over `fs`, exactly as the spawn
/// path does.
pub(crate) fn gate_load(fs: &MemFs, anchor: [u8; 32]) -> Result<LoadedApp, AppError> {
    let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
    let store = FsBundleStore::new(fs, 1000, &NoCaps);
    let verifier = AnchorVerifier::new(anchor);
    let clock = NullClock;
    let loader = AppLoader::new(AppLoaderConfig {
        accepted_abi_version: ABI_VERSION_CURRENT,
        syscall_table_hash: SYSCALL_TABLE_HASH,
        store: &store,
        verifier: &verifier,
        clock: &clock,
        sink,
    });
    loader.load(
        "/System/Apps/ps.app",
        &CapabilitySet::from_words([u64::MAX; 4]),
    )
}

/// A verified [`LoadedApp`] straight from the shared load gate, over the
/// composed in-memory test bundle.
pub(crate) fn verified_app() -> Arc<LoadedApp> {
    let (fs, anchor, _run) = composed_bundle(Vec::new());
    Arc::new(gate_load(&fs, anchor).expect("the composed bundle verifies"))
}
