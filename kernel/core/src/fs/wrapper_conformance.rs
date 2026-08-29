//! Forwarding conformance for the mount-path filesystem wrappers.
//!
//! A `Filesystem*` implementation that *wraps* an inner driver — the
//! cache ([`CachedFs`](super::CachedFs)), the storage-group identity map,
//! the boxed type-erased mount driver, a counting test shim — owes its
//! inner's whole surface. Four of the facet methods carry trait defaults
//! that answer [`DriverError::Unsupported`] so a *format* with no such
//! object can refuse honestly; in a wrapper that same default is a refusal
//! the wrapper invented, and it fails closed, so nothing crashes and no
//! ordinary test notices. That is how `read_link`/`create_link`/`link`
//! came to answer "this volume has no links" on a genuinely mounted
//! volume.
//!
//! This suite is the one place that property is stated. Each `assert_*`
//! function drives every method of one facet against [`fixture`] — an
//! inner driver that supports all of them — so any `Unsupported` reaching
//! the caller can only have come from the wrapper. Adding a facet method
//! means extending one function here, and every wrapper's test gains the
//! check at once.
//!
//! A wrapper that deliberately *replaces* a method rather than forwarding
//! it says so by asserting the replacement instead
//! ([`assert_security_replaced`]), so the divergence is declared and
//! reviewed rather than invisible.
//!
//! The premise is itself tested: `fscache_tests` runs the whole suite
//! against a bare [`RwMockFs`], so the fixture can never quietly stop
//! supporting an operation and let every wrapper's check pass vacuously.

use core::sync::atomic::{AtomicU64, Ordering};

use alloc::boxed::Box;

use tairix_abi::driver::filesystem::{
    FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite,
    NodeId, NodeKind, NodeSecurity, WritebackHost,
};
use tairix_abi::driver::{DriverError, DriverHandle};

use super::memfs::RwMockFs;

/// The regular file the fixture holds, with [`FILE_BODY`] as its contents.
pub const FILE_NAME: &[u8] = b"file";
/// Contents of [`FILE_NAME`].
pub const FILE_BODY: &[u8] = b"conformance";
/// The symbolic link the fixture holds; its stored target is [`FILE_NAME`].
pub const LINK_NAME: &[u8] = b"link";
/// The subdirectory the fixture holds.
pub const DIR_NAME: &[u8] = b"dir";

/// An inner driver supporting every method of every facet: a root holding
/// a regular file with contents, a symbolic link naming it, and a
/// subdirectory.
///
/// The caller wraps this and hands the wrapper to the `assert_*`
/// functions, which re-resolve everything by name through the wrapper's
/// own [`FilesystemRead::root`] and [`FilesystemRead::lookup`] — so the
/// suite uses only the trait surface and never needs the fixture's ids.
#[must_use]
pub fn fixture() -> RwMockFs {
    let mut fs = RwMockFs::new();
    let root = fs.root();
    let file = fs
        .create(root, FILE_NAME, NodeKind::RegularFile)
        .expect("the fixture creates a file");
    fs.write_at(root, FILE_NAME, 0, FILE_BODY)
        .expect("the fixture fills the file");
    let _ = file;
    fs.create_link(root, LINK_NAME, FILE_NAME)
        .expect("the fixture creates a symbolic link");
    fs.create(root, DIR_NAME, NodeKind::Directory)
        .expect("the fixture creates a directory");
    fs
}

/// Resolve `name` under the wrapper's own root, failing the test with the
/// forwarding diagnosis rather than a bare unwrap.
fn child<W: FilesystemRead + ?Sized>(wrapper: &mut W, name: &[u8]) -> NodeId {
    let root = wrapper.root();
    match wrapper.lookup(root, name) {
        Ok(node) => node,
        Err(error) => panic!(
            "the wrapper failed to resolve a name its inner holds ({error:?}); \
             lookup is not being forwarded"
        ),
    }
}

/// Every [`FilesystemRead`] method reaches the inner driver.
pub fn assert_read_forwards<W: FilesystemRead + ?Sized>(wrapper: &mut W) {
    let root = wrapper.root();
    assert_ne!(
        root.raw(),
        0,
        "the wrapper reports no root; `root` is not being forwarded"
    );

    let file = child(wrapper, FILE_NAME);
    let info = wrapper
        .node_info(file)
        .expect("`node_info` is not being forwarded");
    assert_eq!(info.kind, NodeKind::RegularFile);
    assert_eq!(info.size, FILE_BODY.len() as u64);

    let mut bytes = [0u8; 32];
    let read = wrapper
        .read_at(file, 0, &mut bytes[..FILE_BODY.len()])
        .expect("`read_at` is not being forwarded");
    assert_eq!(&bytes[..read], FILE_BODY);

    // The method whose default refusal reads as "this format has no
    // symbolic links" — a claim about the format, made by a wrapper.
    let link = child(wrapper, LINK_NAME);
    let mut target = [0u8; 64];
    let len = match wrapper.read_link(link, &mut target) {
        Ok(len) => len,
        Err(DriverError::Unsupported) => panic!(
            "`read_link` answered `Unsupported` for a link the inner driver \
             stores: the wrapper is refusing on its own behalf"
        ),
        Err(error) => panic!("`read_link` is not being forwarded ({error:?})"),
    };
    assert_eq!(&target[..len], FILE_NAME);

    let mut name = [0u8; 64];
    let entry = wrapper
        .read_dir(root, 0, &mut name)
        .expect("`read_dir` is not being forwarded");
    assert!(
        entry.is_some(),
        "the wrapper reports an empty listing for a populated directory"
    );
}

/// Every [`FilesystemWrite`] method reaches the inner driver.
///
/// Each mutation is made under its own fresh name and undone where it
/// would otherwise disturb a later assertion, so the facets may be driven
/// in any order against one wrapper.
pub fn assert_write_forwards<W: FilesystemRead + FilesystemWrite + ?Sized>(wrapper: &mut W) {
    let root = wrapper.root();
    let file = child(wrapper, FILE_NAME);

    let made = wrapper
        .create(root, b"created", NodeKind::RegularFile)
        .expect("`create` is not being forwarded");
    assert_ne!(made.raw(), 0);

    // Both defaults that make a *creating* call refuse: a wrapper that
    // leaves them silently reports the volume as link-incapable.
    match wrapper.create_link(root, b"created-link", FILE_NAME) {
        Ok(node) => assert_ne!(node.raw(), 0),
        Err(DriverError::Unsupported) => panic!(
            "`create_link` answered `Unsupported` on a driver that creates \
             symbolic links: the wrapper is refusing on its own behalf"
        ),
        Err(error) => panic!("`create_link` is not being forwarded ({error:?})"),
    }
    match wrapper.link(root, b"created-hardlink", file) {
        Ok(()) => {}
        Err(DriverError::Unsupported) => panic!(
            "`link` answered `Unsupported` on a driver that creates hard \
             links: the wrapper is refusing on its own behalf"
        ),
        Err(error) => panic!("`link` is not being forwarded ({error:?})"),
    }

    let written = wrapper
        .write_at(root, b"created", 0, b"bytes")
        .expect("`write_at` is not being forwarded");
    assert_eq!(written, b"bytes".len());
    wrapper
        .truncate(root, b"created", 1)
        .expect("`truncate` is not being forwarded");
    wrapper
        .rename(root, b"created", root, b"renamed")
        .expect("`rename` is not being forwarded");
    wrapper
        .remove(root, b"renamed")
        .expect("`remove` is not being forwarded");
    wrapper.flush().expect("`flush` is not being forwarded");
    assert_writeback_host_forwards(wrapper);
}

/// The mount handle the suite installs its write-back timer under.
const CONFORMANCE_VOLUME: u64 = 0x5742;

/// A [`WritebackHost`] recording the handle the innermost driver reported
/// against, so the forwarding is observed through the host rather than
/// through an inner driver the wrapper hides.
struct RecordingHost(AtomicU64);

impl WritebackHost for RecordingHost {
    fn now_ns(&self) -> Option<u64> {
        Some(0)
    }

    fn writeback_due(&self, volume: DriverHandle, _deadline_ns: Option<u64>) {
        self.0.store(volume.as_u64(), Ordering::Release);
    }
}

/// [`FilesystemWrite::set_writeback_host`] reaches the inner driver.
///
/// Its trait default is a silent no-op, so a wrapper that drops it costs no
/// error and no test failure anywhere else — the inner driver simply never
/// defers a commit again, and the batching win disappears without a trace.
/// The fixture reports to the host as it is installed, so an unforwarded
/// call leaves the host with nothing recorded.
fn assert_writeback_host_forwards<W: FilesystemWrite + ?Sized>(wrapper: &mut W) {
    let host: &'static RecordingHost = Box::leak(Box::new(RecordingHost(AtomicU64::new(0))));
    let volume = DriverHandle::from_raw(CONFORMANCE_VOLUME).expect("non-zero handle");
    wrapper.set_writeback_host(volume, host);
    assert_eq!(
        host.0.load(Ordering::Acquire),
        CONFORMANCE_VOLUME,
        "`set_writeback_host` is not being forwarded: the inner driver never          learned of the timer, so it will publish every operation instead of          batching"
    );
}

/// Both [`FilesystemSecurity`] methods reach the inner driver, and a
/// stored record survives the round trip.
pub fn assert_security_forwards<W: FilesystemRead + FilesystemSecurity + ?Sized>(wrapper: &mut W) {
    let file = child(wrapper, FILE_NAME);
    let stored = wrapper
        .security(file)
        .expect("`security` is not being forwarded");

    let changed = NodeSecurity::new(0o600, stored.uid + 7, stored.gid + 9);
    wrapper
        .set_security(file, changed)
        .expect("`set_security` is not being forwarded");
    let read_back = wrapper
        .security(file)
        .expect("`security` is not being forwarded");
    assert_eq!(
        (read_back.mode, read_back.uid, read_back.gid),
        (changed.mode, changed.uid, changed.gid),
        "the wrapper did not carry the stored record through"
    );
}

/// The wrapper *replaces* the security facet rather than forwarding it:
/// it still answers a real record (never an invented refusal), and its
/// refusal to store one is the declared mount policy.
pub fn assert_security_replaced<W: FilesystemRead + FilesystemSecurity + ?Sized>(wrapper: &mut W) {
    let file = child(wrapper, FILE_NAME);
    wrapper
        .security(file)
        .expect("a replacing wrapper still answers a record");
    assert_eq!(
        wrapper.set_security(file, NodeSecurity::new(0o600, 1, 1)),
        Err(DriverError::Unsupported),
        "a mapped record is mount policy, so storing one is refused whole"
    );
}

/// [`FilesystemStats`] reaches the inner driver.
pub fn assert_stats_forwards<W: FilesystemStats + ?Sized>(wrapper: &mut W) {
    wrapper.stats().expect("`stats` is not being forwarded");
}

/// The attribute facet is handed out, and all four of its methods reach
/// the inner driver through it.
///
/// This is the path the secured VFS takes, so the facet is driven exactly
/// as the VFS reaches it: through [`FilesystemAttrsProvider::attrs_fs`].
pub fn assert_attrs_forwards<W: FilesystemRead + FilesystemAttrsProvider + ?Sized>(
    wrapper: &mut W,
) {
    let file = child(wrapper, FILE_NAME);
    assert!(
        wrapper.attrs_fs().is_some(),
        "`attrs_fs` answered `None` on a driver that stores attributes: the \
         wrapper is withholding a facet its inner provides"
    );

    let key = b"user.conformance";
    let value = b"kept";
    {
        let attrs = wrapper.attrs_fs().expect("the facet is provided");
        attrs
            .set_attr(file, key, value)
            .expect("`set_attr` is not being forwarded");
    }
    {
        let attrs = wrapper.attrs_fs().expect("the facet is provided");
        let mut out = [0u8; 32];
        let len = attrs
            .get_attr(file, key, &mut out)
            .expect("`get_attr` is not being forwarded")
            .expect("the attribute just set is present");
        assert_eq!(&out[..len], value);
    }
    {
        let attrs = wrapper.attrs_fs().expect("the facet is provided");
        let mut out = [0u8; 32];
        let len = attrs
            .list_attr(file, 0, &mut out)
            .expect("`list_attr` is not being forwarded")
            .expect("the attribute just set is listed");
        assert_eq!(&out[..len], key);
    }
    {
        let attrs = wrapper.attrs_fs().expect("the facet is provided");
        attrs
            .remove_attr(file, key)
            .expect("`remove_attr` is not being forwarded");
    }
}
