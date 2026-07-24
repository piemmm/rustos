//! Behavioural tests for the boot-time system-configuration loader
//! ([`crate::syscfg::load_and_apply_system_config`]): the store-present,
//! store-absent, and malformed-store paths, each applying the operator's
//! caching switches to an injected control and auditing its outcome.

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind, NodeSecurity};

use crate::cache_control::{CacheClass, CacheControl};
use crate::fs::memfs::RwMockFs;
use crate::syscfg::load_and_apply_system_config;
use crate::test_sink::TestSink;

/// Event ids of the two outcome records (mirrors `crate::audit`).
const APPLIED: u32 = 4110;
const REJECTED: u32 = 4111;

/// An in-memory root carrying `/System/Settings/Configuration/system.conf`
/// with `bytes`, owned by `uid 0` so the bootstrap reader can traverse and
/// read it (mirroring the mkimage-authored skeleton).
fn planted(bytes: &[u8]) -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(0, 0, 0o755);
    fs.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let root = fs.root();
    let system = fs
        .create(root, b"System", NodeKind::Directory)
        .expect("System");
    let settings = fs
        .create(system, b"Settings", NodeKind::Directory)
        .expect("Settings");
    let config = fs
        .create(settings, b"Configuration", NodeKind::Directory)
        .expect("Configuration");
    fs.create(config, b"system.conf", NodeKind::RegularFile)
        .expect("system.conf");
    fs.write_at(config, b"system.conf", 0, bytes)
        .expect("write system.conf");
    fs
}

/// A root with the `/System/Settings/Configuration` tree but **no**
/// `system.conf` file (a fresh install).
fn without_config() -> RwMockFs {
    let mut fs = RwMockFs::new().with_create_owner(0, 0, 0o755);
    fs.set_root_security(NodeSecurity::new(0o755, 0, 0));
    let root = fs.root();
    let system = fs
        .create(root, b"System", NodeKind::Directory)
        .expect("System");
    let settings = fs
        .create(system, b"Settings", NodeKind::Directory)
        .expect("Settings");
    fs.create(settings, b"Configuration", NodeKind::Directory)
        .expect("Configuration");
    fs
}

fn sink() -> &'static TestSink {
    alloc::boxed::Box::leak(alloc::boxed::Box::new(TestSink::new()))
}

fn control() -> CacheControl {
    CacheControl::new()
}

#[test]
fn an_absent_store_applies_the_all_enabled_defaults() {
    let mut fs = without_config();
    let control = control();
    let audit = sink();
    load_and_apply_system_config(&mut fs, &control, audit);

    for class in CacheClass::ALL {
        assert!(control.admits(*class), "{class:?} enabled by default");
    }
    // An absent store is the normal default case: applied, not rejected.
    let events = audit.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, APPLIED);
    assert_eq!(events[0].fields[0].1, "default");
}

#[test]
fn the_master_switch_off_disables_every_class() {
    let mut fs = planted(b"cache.all off\n");
    let control = control();
    let audit = sink();
    load_and_apply_system_config(&mut fs, &control, audit);

    for class in CacheClass::ALL {
        assert!(!control.admits(*class), "{class:?} obeys cache.all off");
    }
    let events = audit.snapshot();
    assert_eq!(events[0].id.0, APPLIED);
    assert_eq!(events[0].fields[0].1, "store");
}

#[test]
fn a_per_class_off_disables_only_that_class() {
    let mut fs = planted(b"cache.block off\n");
    let control = control();
    let audit = sink();
    load_and_apply_system_config(&mut fs, &control, audit);

    assert!(control.admits(CacheClass::Filesystem));
    assert!(!control.admits(CacheClass::Block));
    assert!(control.admits(CacheClass::Transform));
    assert!(control.admits(CacheClass::Semantic));
}

#[test]
fn a_malformed_store_is_rejected_and_the_defaults_are_applied() {
    // An unknown key fails the closed parser; the loader falls back to the
    // all-enabled defaults rather than guessing, and audits the rejection.
    let mut fs = planted(b"cache.bogus off\n");
    let control = control();
    let audit = sink();
    load_and_apply_system_config(&mut fs, &control, audit);

    for class in CacheClass::ALL {
        assert!(control.admits(*class), "malformed store keeps defaults");
    }
    let ids: alloc::vec::Vec<u32> = audit.snapshot().iter().map(|e| e.id.0).collect();
    assert!(ids.contains(&REJECTED), "the malformed store was audited");
    assert!(ids.contains(&APPLIED), "the defaults were still applied");
}

#[test]
fn applying_off_then_a_default_store_re_enables() {
    let control = control();
    let audit = sink();
    // First, an off store disables everything.
    let mut off = planted(b"cache.all off\n");
    load_and_apply_system_config(&mut off, &control, audit);
    assert!(!control.admits(CacheClass::Filesystem));
    // Then a default store re-enables (the loader always applies, never
    // merges): the control reflects the latest store exactly.
    let mut on = without_config();
    load_and_apply_system_config(&mut on, &control, audit);
    for class in CacheClass::ALL {
        assert!(control.admits(*class));
    }
}
