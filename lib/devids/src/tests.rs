//! Unit tests: the vetting parser's accept/reject behaviour, the encoder's
//! determinism, and the table decoder's fail-closed validation.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::textdb::{self, ParseErrorKind};
use crate::{DbKind, DevIds, TableError, MAX_NAME_BYTES, MAX_SOURCE_BYTES};

/// A well-formed miniature `pci.ids`: vendors, devices, subsystems, and the
/// `C` class tables, with comments and blank lines interleaved.
const PCI_FIXTURE: &str = "\
# A comment line, which may contain a \t tab.

0001  Safenet (wrong ID)
8086  Intel Corporation
\t1237  440FX - 82441FX PMC [Natoma]
\t\t8086 1237  Board fixture
\t\t1028 04b6  Server fixture
\t7010  82371SB PIIX3 IDE [Natoma/Triton II]
C 01  Mass storage controller
\t01  IDE interface
\t\t00  ISA Compatibility mode-only controller
\t\t05  PCI native mode-only controller
\t06  SATA controller
\t\t01  AHCI 1.0
C 03  Display controller
\t00  VGA compatible controller
";

/// A well-formed miniature `usb.ids`: vendors, products, the `C` tables,
/// and auxiliary sections with and without children.
const USB_FIXTURE: &str = "\
# usb.ids fixture
1d6b  Linux Foundation
\t0002  2.0 root hub
\t0003  3.0 root hub
8087  Intel Corp.
C 03  Human Interface Device
\t01  Boot Interface Subclass
\t\t01  Keyboard
\t\t02  Mouse
AT 0100  USB Undefined
HID 21  HID
R 01  Input
BIAS 1  Right Hand
PHY 23  Pen
HUT 01  Generic Desktop Controls
\t002  Mouse
\t05A  Set Envelope Report
L 0409  English
\t01  US
HCC 21  Arabic
VT 0100  Vendor Specific
";

fn parse_ok(kind: DbKind, text: &str) -> textdb::ParsedDb {
    match textdb::parse(kind, text.as_bytes()) {
        Ok(db) => db,
        Err(e) => panic!("fixture must parse: {e}"),
    }
}

fn reject(kind: DbKind, text: &str) -> ParseErrorKind {
    match textdb::parse(kind, text.as_bytes()) {
        Ok(_) => panic!("input must be rejected: {text:?}"),
        Err(e) => e.kind,
    }
}

fn decoded(bytes: &[u8], kind: DbKind) -> DevIds<'_> {
    match DevIds::parse(kind, bytes) {
        Ok(ids) => ids,
        Err(e) => panic!("encoded table must decode: {e:?}"),
    }
}

#[test]
fn pci_fixture_parses_with_the_expected_counts() {
    let db = parse_ok(DbKind::Pci, PCI_FIXTURE);
    let counts = db.counts();
    assert_eq!(counts.vendors, 2);
    assert_eq!(counts.devices, 2);
    assert_eq!(counts.subsystems, 2);
    assert_eq!(counts.classes, 2);
    assert_eq!(counts.subclasses, 3);
    assert_eq!(counts.prog_ifs, 3);
    assert_eq!(counts.aux, 0);
}

#[test]
fn usb_fixture_parses_including_the_auxiliary_sections() {
    let db = parse_ok(DbKind::Usb, USB_FIXTURE);
    let counts = db.counts();
    assert_eq!(counts.vendors, 2);
    assert_eq!(counts.devices, 2);
    assert_eq!(counts.subsystems, 0);
    assert_eq!(counts.classes, 1);
    assert_eq!(counts.subclasses, 1);
    assert_eq!(counts.prog_ifs, 2);
    assert_eq!(counts.aux, 12);
}

#[test]
fn encode_then_decode_round_trips_every_lookup() {
    let bytes = parse_ok(DbKind::Pci, PCI_FIXTURE).encode();
    let ids = decoded(&bytes, DbKind::Pci);
    assert_eq!(ids.kind(), DbKind::Pci);
    assert_eq!(ids.vendor(0x8086), Some("Intel Corporation"));
    assert_eq!(ids.vendor(0x0001), Some("Safenet (wrong ID)"));
    assert_eq!(
        ids.device(0x8086, 0x1237),
        Some("440FX - 82441FX PMC [Natoma]")
    );
    assert_eq!(ids.class(0x01), Some("Mass storage controller"));
    assert_eq!(ids.subclass(0x01, 0x06), Some("SATA controller"));
    assert_eq!(ids.prog_if(0x01, 0x06, 0x01), Some("AHCI 1.0"));
    // Misses fail closed to None: the caller renders the numeric form.
    assert_eq!(ids.vendor(0xdead), None);
    assert_eq!(ids.device(0x8086, 0xffff), None);
    assert_eq!(ids.class(0xff), None);
    assert_eq!(ids.subclass(0x01, 0xff), None);
    assert_eq!(ids.prog_if(0x01, 0x06, 0xff), None);
}

#[test]
fn usb_encode_then_decode_round_trips() {
    let bytes = parse_ok(DbKind::Usb, USB_FIXTURE).encode();
    let ids = decoded(&bytes, DbKind::Usb);
    assert_eq!(ids.vendor(0x1d6b), Some("Linux Foundation"));
    assert_eq!(ids.device(0x1d6b, 0x0003), Some("3.0 root hub"));
    assert_eq!(ids.prog_if(0x03, 0x01, 0x02), Some("Mouse"));
}

#[test]
fn encoding_is_deterministic() {
    let a = parse_ok(DbKind::Usb, USB_FIXTURE).encode();
    let b = parse_ok(DbKind::Usb, USB_FIXTURE).encode();
    assert_eq!(a, b);
}

#[test]
fn identical_names_are_interned_once() {
    let twice = "0001  Same Name\n0002  Same Name\n";
    let once = "0001  Same Name\n0002  Other Nam\n";
    let interned = parse_ok(DbKind::Pci, twice).encode();
    let distinct = parse_ok(DbKind::Pci, once).encode();
    assert!(interned.len() < distinct.len());
}

#[test]
fn stray_spaces_around_names_are_trimmed() {
    // Both shapes exist in today's usb.ids: a three-space separator and a
    // trailing space.
    let db = parse_ok(DbKind::Usb, "0001   Leading\n0002  Trailing \n");
    let bytes = db.encode();
    let ids = decoded(&bytes, DbKind::Usb);
    assert_eq!(ids.vendor(1), Some("Leading"));
    assert_eq!(ids.vendor(2), Some("Trailing"));
}

#[test]
fn vetting_rejects_structural_damage() {
    // Not the entry shape at all.
    assert_eq!(
        reject(DbKind::Pci, "not a database\n"),
        ParseErrorKind::MissingSeparator
    );
    // Escape-sequence injection through a name.
    assert_eq!(
        reject(DbKind::Pci, "0001  Evil\u{1b}[2Jname\n"),
        ParseErrorKind::ControlChar
    );
    // A C1 control character.
    assert_eq!(
        reject(DbKind::Pci, "0001  Evil\u{9b}name\n"),
        ParseErrorKind::ControlChar
    );
    // A carriage return (CRLF line endings are not the published format).
    assert_eq!(
        reject(DbKind::Pci, "0001  Name\r\n"),
        ParseErrorKind::ControlChar
    );
    // A tab inside an entry body.
    assert_eq!(
        reject(DbKind::Pci, "0001  Na\tme\n"),
        ParseErrorKind::TabInEntry
    );
    // An indented comment.
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n\t# hidden\n"),
        ParseErrorKind::IndentedComment
    );
    // Nesting deeper than the grammar defines.
    assert_eq!(
        reject(
            DbKind::Pci,
            "0001  V\n\t0001  D\n\t\t0001 0001  S\n\t\t\t00  X\n"
        ),
        ParseErrorKind::TooDeep
    );
    // Invalid UTF-8.
    assert_eq!(
        textdb::parse(DbKind::Pci, b"0001  Nam\xb4e\n")
            .expect_err("must reject")
            .kind,
        ParseErrorKind::NotUtf8
    );
}

#[test]
fn vetting_rejects_bad_ids() {
    // Wrong width.
    assert_eq!(reject(DbKind::Pci, "001  V\n"), ParseErrorKind::BadId);
    assert_eq!(reject(DbKind::Pci, "00011  V\n"), ParseErrorKind::BadId);
    // Uppercase hex in an emitted scope is a bad id (the section-tag path
    // needs a leading uppercase run followed by a space).
    assert_eq!(reject(DbKind::Pci, "00AB  V\n"), ParseErrorKind::BadId);
    assert_eq!(reject(DbKind::Pci, "AB01  V\n"), ParseErrorKind::BadId);
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n\t00AB  D\n"),
        ParseErrorKind::BadId
    );
    // Non-hex.
    assert_eq!(reject(DbKind::Pci, "00g1  V\n"), ParseErrorKind::BadId);
    // Class ids are two hex digits.
    assert_eq!(reject(DbKind::Pci, "C 001  X\n"), ParseErrorKind::BadId);
    // A PCI subsystem id is two 4-hex words.
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n\t0001  D\n\t\t001 0001  S\n"),
        ParseErrorKind::BadId
    );
}

#[test]
fn vetting_rejects_name_defects() {
    assert_eq!(reject(DbKind::Pci, "0001   \n"), ParseErrorKind::EmptyName);
    let long = format!("0001  {}\n", "x".repeat(MAX_NAME_BYTES + 1));
    assert_eq!(reject(DbKind::Pci, &long), ParseErrorKind::NameTooLong);
    let max = format!("0001  {}\n", "x".repeat(MAX_NAME_BYTES));
    assert!(textdb::parse(DbKind::Pci, max.as_bytes()).is_ok());
}

#[test]
fn vetting_rejects_wrong_sections_and_orphans() {
    // The USB auxiliary tags are not part of the PCI grammar.
    assert_eq!(
        reject(DbKind::Pci, "HUT 01  X\n"),
        ParseErrorKind::UnknownSection
    );
    // An unknown tag in either database.
    assert_eq!(
        reject(DbKind::Usb, "XX 01  X\n"),
        ParseErrorKind::UnknownSection
    );
    // A child with no parent.
    assert_eq!(
        reject(DbKind::Pci, "\t0001  D\n"),
        ParseErrorKind::OrphanEntry
    );
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n\t\t0001 0001  S\n"),
        ParseErrorKind::OrphanEntry
    );
    // USB vendor sections have no depth-2 grammar.
    assert_eq!(
        reject(DbKind::Usb, "0001  V\n\t0001  D\n\t\t0001 0001  S\n"),
        ParseErrorKind::UnexpectedDepth
    );
    // Auxiliary sections have no depth-2 grammar.
    assert_eq!(
        reject(DbKind::Usb, "0001  V\nHUT 01  P\n\t002  U\n\t\t01  X\n"),
        ParseErrorKind::UnexpectedDepth
    );
}

#[test]
fn vetting_rejects_duplicates_in_every_scope() {
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n0001  W\n"),
        ParseErrorKind::DuplicateId
    );
    assert_eq!(
        reject(DbKind::Pci, "0001  V\n\t0002  D\n\t0002  E\n"),
        ParseErrorKind::DuplicateId
    );
    assert_eq!(
        reject(
            DbKind::Pci,
            "0001  V\n\t0002  D\n\t\t0003 0004  S\n\t\t0003 0004  T\n"
        ),
        ParseErrorKind::DuplicateId
    );
    assert_eq!(
        reject(DbKind::Pci, "0001  V\nC 01  X\nC 01  Y\n"),
        ParseErrorKind::DuplicateId
    );
    assert_eq!(
        reject(DbKind::Pci, "0001  V\nC 01  X\n\t02  A\n\t02  B\n"),
        ParseErrorKind::DuplicateId
    );
    assert_eq!(
        reject(DbKind::Usb, "0001  V\nHUT 01  P\n\t002  U\n\t002  W\n"),
        ParseErrorKind::DuplicateId
    );
    // The same id in different scopes is legitimate.
    let scoped = "0001  V\n\t0001  D\n0002  W\n\t0001  D2\nC 01  X\n\t01  A\n";
    assert!(textdb::parse(DbKind::Pci, scoped.as_bytes()).is_ok());
}

#[test]
fn vetting_bounds_the_source_and_the_entry_count() {
    let oversized = vec![b'#'; MAX_SOURCE_BYTES + 1];
    assert_eq!(
        textdb::parse(DbKind::Pci, &oversized)
            .expect_err("must reject")
            .kind,
        ParseErrorKind::SourceTooLarge
    );
    // One entry over the count bound: a handful of vendors, each carrying
    // enough devices to cross it (u16 ids cap what one scope can hold).
    let mut text = String::new();
    let vendors = 5u32;
    let per_vendor = crate::MAX_TABLE_ENTRIES / vendors + 1;
    for vendor in 0..vendors {
        let _ = core::fmt::Write::write_fmt(&mut text, format_args!("{vendor:04x}  V\n"));
        for device in 0..per_vendor.min(0x1_0000) {
            let _ = core::fmt::Write::write_fmt(&mut text, format_args!("\t{device:04x}  D\n"));
        }
    }
    assert_eq!(
        textdb::parse(DbKind::Pci, text.as_bytes())
            .expect_err("must reject")
            .kind,
        ParseErrorKind::TooManyEntries
    );
}

#[test]
fn vetting_rejects_an_empty_database() {
    assert_eq!(reject(DbKind::Pci, ""), ParseErrorKind::Empty);
    assert_eq!(
        reject(DbKind::Pci, "# only comments\n\n"),
        ParseErrorKind::Empty
    );
}

#[test]
fn parse_error_display_names_the_line() {
    let err = textdb::parse(DbKind::Pci, b"0001  V\nbroken\n").expect_err("must reject");
    assert_eq!(err.line, 2);
    assert_eq!(
        format!("{err}"),
        "line 2: entry is not `id`, two spaces, `name`"
    );
}

/// Corrupt one byte range of an encoded table and expect the given error.
fn corrupt(mutate: impl FnOnce(&mut Vec<u8>), expected: TableError) {
    let mut bytes = parse_ok(DbKind::Pci, PCI_FIXTURE).encode();
    mutate(&mut bytes);
    assert_eq!(
        DevIds::parse(DbKind::Pci, &bytes).expect_err("must reject"),
        expected
    );
}

#[test]
fn decoder_rejects_structural_damage() {
    corrupt(|b| b.truncate(10), TableError::TruncatedHeader);
    corrupt(|b| b[0] = b'X', TableError::BadMagic);
    corrupt(|b| b[8] = 9, TableError::WrongKind);
    corrupt(
        |b| b[12..16].copy_from_slice(&u32::MAX.to_le_bytes()),
        TableError::CountTooLarge,
    );
    corrupt(|b| b.truncate(b.len() - 1), TableError::LengthMismatch);
    corrupt(
        |b| {
            let extended = b.len() + 1;
            b.resize(extended, 0);
        },
        TableError::LengthMismatch,
    );
    // Swap the first two vendor records out of order.
    corrupt(
        |b| {
            let (lo, hi) = (36, 36 + 12);
            let mut first = [0u8; 12];
            first.copy_from_slice(&b[lo..hi]);
            let mut second = [0u8; 12];
            second.copy_from_slice(&b[hi..hi + 12]);
            b[lo..hi].copy_from_slice(&second);
            b[hi..hi + 12].copy_from_slice(&first);
        },
        TableError::UnsortedRecords,
    );
    // Point the first vendor's name past the blob.
    corrupt(
        |b| b[40..44].copy_from_slice(&u32::MAX.to_le_bytes()),
        TableError::BadNameSlice,
    );
    // Zero-length name.
    corrupt(
        |b| b[44..48].copy_from_slice(&0u32.to_le_bytes()),
        TableError::BadNameSlice,
    );
    // Invalid UTF-8 in the strings blob.
    corrupt(
        |b| {
            let last = b.len() - 1;
            b[last] = 0xff;
        },
        TableError::StringsNotUtf8,
    );
}

#[test]
fn decoder_rejects_the_wrong_database_kind() {
    let bytes = parse_ok(DbKind::Usb, USB_FIXTURE).encode();
    assert_eq!(
        DevIds::parse(DbKind::Pci, &bytes).expect_err("must reject"),
        TableError::WrongKind
    );
}

#[test]
fn decoder_handles_the_empty_edge_of_each_table() {
    // A database with no `C` section decodes with empty class tables.
    let bytes = parse_ok(DbKind::Pci, "0001  Vendor\n").encode();
    let ids = decoded(&bytes, DbKind::Pci);
    assert_eq!(ids.vendor(1), Some("Vendor"));
    assert_eq!(ids.class(0), None);
    assert_eq!(ids.subclass(0, 0), None);
    assert_eq!(ids.prog_if(0, 0, 0), None);
}
