//! The committed data files are load-bearing: the snapshots under `assets/`
//! must pass the vetting filter, the tables under `tables/` must be exactly
//! their compile (the same drift the `cargo xtask devids` CI gate rejects),
//! and well-known ids must resolve to their well-known names.

use std::path::PathBuf;

use rustos_devids::{textdb, DbKind, DevIds};

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn committed(kind: DbKind) -> (Vec<u8>, Vec<u8>) {
    let name = match kind {
        DbKind::Pci => "pci.ids",
        DbKind::Usb => "usb.ids",
    };
    let snapshot = std::fs::read(crate_root().join("assets").join(name)).expect("snapshot");
    let table =
        std::fs::read(crate_root().join("tables").join(format!("{name}.bin"))).expect("table");
    (snapshot, table)
}

#[test]
fn committed_snapshots_pass_vetting_and_match_the_committed_tables() {
    for kind in [DbKind::Pci, DbKind::Usb] {
        let (snapshot, table) = committed(kind);
        let parsed = match textdb::parse(kind, &snapshot) {
            Ok(db) => db,
            Err(e) => panic!("committed {kind:?} snapshot fails vetting: {e}"),
        };
        // A plausible real database, not a truncated stub.
        assert!(parsed.counts().vendors > 2000, "{kind:?} vendor count");
        assert!(parsed.counts().devices > 10_000, "{kind:?} device count");
        assert_eq!(
            parsed.encode(),
            table,
            "{kind:?} table drifted; run `cargo xtask devids --write`"
        );
    }
}

#[test]
fn well_known_pci_ids_resolve() {
    let (_, table) = committed(DbKind::Pci);
    let ids = DevIds::parse(DbKind::Pci, &table).expect("committed table decodes");
    assert_eq!(ids.vendor(0x8086), Some("Intel Corporation"));
    assert_eq!(ids.vendor(0x1af4), Some("Red Hat, Inc."));
    assert_eq!(
        ids.device(0x1af4, 0x1041),
        Some("Virtio 1.0 network device")
    );
    assert_eq!(ids.class(0x03), Some("Display controller"));
    assert_eq!(ids.subclass(0x01, 0x06), Some("SATA controller"));
    assert_eq!(ids.prog_if(0x01, 0x06, 0x01), Some("AHCI 1.0"));
    // An id the database does not carry renders numerically at the caller.
    assert_eq!(ids.device(0x8086, 0x0000), None);
}

#[test]
fn well_known_usb_ids_resolve() {
    let (_, table) = committed(DbKind::Usb);
    let ids = DevIds::parse(DbKind::Usb, &table).expect("committed table decodes");
    assert_eq!(ids.vendor(0x1d6b), Some("Linux Foundation"));
    assert_eq!(ids.device(0x1d6b, 0x0002), Some("2.0 root hub"));
    assert_eq!(ids.class(0x03), Some("Human Interface Device"));
    assert_eq!(ids.subclass(0x03, 0x01), Some("Boot Interface Subclass"));
    assert_eq!(ids.prog_if(0x03, 0x01, 0x01), Some("Keyboard"));
}
