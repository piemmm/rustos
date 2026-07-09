//! The compiled ID-database table: format, fail-closed decoder, and
//! allocation-free binary-search lookups.
//!
//! `cargo xtask devids` compiles a vetted snapshot into one flat file per
//! database. The layout is little-endian throughout:
//!
//! ```text
//! header (36 bytes):
//!   magic        [u8; 8]   "RDEVIDS1"
//!   kind         u32       1 = PCI, 2 = USB
//!   vendors      u32       record count
//!   devices      u32       record count
//!   classes      u32       record count
//!   subclasses   u32       record count
//!   prog_ifs     u32       record count
//!   strings_len  u32       bytes of UTF-8 name data
//! five record tables, each `count × 12` bytes, sorted strictly
//! ascending by key:
//!   key          u32       see the per-table key layout below
//!   name_off     u32       byte offset into the strings blob
//!   name_len     u32       byte length of the name
//! strings blob (`strings_len` bytes): concatenated UTF-8 names.
//! ```
//!
//! Per-table keys: vendors `id`; devices `vendor << 16 | device`; classes
//! `class`; subclasses `class << 8 | subclass`; prog-ifs (USB: protocols)
//! `class << 16 | subclass << 8 | prog_if`.
//!
//! [`DevIds::parse`] validates the whole structure up front — magic, kind,
//! bounded counts, exact file length, strict key order, every name slice
//! in-bounds on UTF-8 character boundaries — so the lookups are infallible
//! slicing over a proven view. The file ships on the read-only system volume
//! but is still data, never trusted blindly.

use crate::{DbKind, MAX_NAME_BYTES, MAX_TABLE_ENTRIES};

/// Magic prefix of a compiled table file (format version 1).
pub const TABLE_MAGIC: [u8; 8] = *b"RDEVIDS1";

/// Header length in bytes.
const HEADER_LEN: usize = 36;

/// Record length in bytes (`key`, `name_off`, `name_len`, each `u32`).
const RECORD_LEN: usize = 12;

/// Why a compiled table was rejected.
///
/// Every variant is a whole-file rejection: a table that fails any check is
/// not partially usable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TableError {
    /// The file is shorter than the fixed header.
    TruncatedHeader,
    /// The magic prefix is not [`TABLE_MAGIC`].
    BadMagic,
    /// The header's kind discriminant is not the expected database.
    WrongKind,
    /// A record count exceeds [`MAX_TABLE_ENTRIES`].
    CountTooLarge,
    /// The file length does not exactly match the header's counts.
    LengthMismatch,
    /// The strings blob is not valid UTF-8.
    StringsNotUtf8,
    /// A record table's keys are not strictly ascending.
    UnsortedRecords,
    /// A record's name slice is out of bounds, empty, over
    /// [`MAX_NAME_BYTES`], or not on UTF-8 character boundaries.
    BadNameSlice,
}

/// A validated, allocation-free view over one compiled ID-database table.
///
/// Constructed by [`DevIds::parse`]; every lookup is an O(log n) binary
/// search over the sorted record table, returning `None` for an id the
/// database does not name (the caller renders the numeric form).
#[derive(Debug)]
pub struct DevIds<'a> {
    kind: DbKind,
    vendors: &'a [u8],
    devices: &'a [u8],
    classes: &'a [u8],
    subclasses: &'a [u8],
    prog_ifs: &'a [u8],
    strings: &'a str,
}

/// The `u32` at `off` in `bytes` (little-endian). `off` is a compile-time
/// header offset the caller has already bounds-checked.
fn le32(bytes: &[u8], off: usize) -> Option<u32> {
    let b = bytes.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The `(key, name_off, name_len)` of record `index` in a record table.
fn record(table: &[u8], index: usize) -> (u32, u32, u32) {
    let at = index * RECORD_LEN;
    // The slice length is a validated multiple of `RECORD_LEN`, so these
    // reads cannot fail; a defensive zero would mask a logic error, so the
    // impossible branch maps to an empty record instead of a panic.
    let field = |o: usize| le32(table, at + o).unwrap_or(0);
    (field(0), field(4), field(8))
}

impl<'a> DevIds<'a> {
    /// Validate `bytes` as a compiled table of `kind` and return the lookup
    /// view.
    ///
    /// # Errors
    ///
    /// Any structural defect rejects the whole file; see [`TableError`].
    pub fn parse(kind: DbKind, bytes: &'a [u8]) -> Result<Self, TableError> {
        if bytes.len() < HEADER_LEN {
            return Err(TableError::TruncatedHeader);
        }
        if bytes[..8] != TABLE_MAGIC {
            return Err(TableError::BadMagic);
        }
        // The header reads below are within `HEADER_LEN`, checked above.
        let header = |off: usize| le32(bytes, off).unwrap_or(0);
        if header(8) != kind.code() {
            return Err(TableError::WrongKind);
        }
        let counts = [header(12), header(16), header(20), header(24), header(28)];
        if counts.iter().any(|&c| c > MAX_TABLE_ENTRIES) {
            return Err(TableError::CountTooLarge);
        }
        let strings_len = header(32) as usize;
        let mut records_len = 0usize;
        for &c in &counts {
            records_len += c as usize * RECORD_LEN;
        }
        let expected = HEADER_LEN
            .checked_add(records_len)
            .and_then(|n| n.checked_add(strings_len))
            .ok_or(TableError::LengthMismatch)?;
        if bytes.len() != expected {
            return Err(TableError::LengthMismatch);
        }
        let strings = core::str::from_utf8(&bytes[HEADER_LEN + records_len..])
            .map_err(|_| TableError::StringsNotUtf8)?;
        let mut at = HEADER_LEN;
        let mut tables = [&bytes[0..0]; 5];
        for (slot, &count) in tables.iter_mut().zip(&counts) {
            let len = count as usize * RECORD_LEN;
            *slot = &bytes[at..at + len];
            at += len;
            validate_table(slot, strings)?;
        }
        Ok(Self {
            kind,
            vendors: tables[0],
            devices: tables[1],
            classes: tables[2],
            subclasses: tables[3],
            prog_ifs: tables[4],
            strings,
        })
    }

    /// Which database this table carries.
    #[must_use]
    pub fn kind(&self) -> DbKind {
        self.kind
    }

    /// The vendor name for `vendor`, if the database has one.
    #[must_use]
    pub fn vendor(&self, vendor: u16) -> Option<&'a str> {
        self.find(self.vendors, u32::from(vendor))
    }

    /// The device (USB: product) name for `device` under `vendor`, if the
    /// database has one.
    #[must_use]
    pub fn device(&self, vendor: u16, device: u16) -> Option<&'a str> {
        self.find(self.devices, u32::from(vendor) << 16 | u32::from(device))
    }

    /// The class name for `class`, if the database has one.
    #[must_use]
    pub fn class(&self, class: u8) -> Option<&'a str> {
        self.find(self.classes, u32::from(class))
    }

    /// The subclass name for `subclass` under `class`, if the database has
    /// one.
    #[must_use]
    pub fn subclass(&self, class: u8, subclass: u8) -> Option<&'a str> {
        self.find(self.subclasses, u32::from(class) << 8 | u32::from(subclass))
    }

    /// The programming-interface (USB: protocol) name for `prog_if` under
    /// `class:subclass`, if the database has one.
    #[must_use]
    pub fn prog_if(&self, class: u8, subclass: u8, prog_if: u8) -> Option<&'a str> {
        self.find(
            self.prog_ifs,
            u32::from(class) << 16 | u32::from(subclass) << 8 | u32::from(prog_if),
        )
    }

    /// Binary-search `table` for `key` and slice its name out of the proven
    /// strings blob.
    fn find(&self, table: &[u8], key: u32) -> Option<&'a str> {
        let count = table.len() / RECORD_LEN;
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (k, off, len) = record(table, mid);
            match k.cmp(&key) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    let (start, end) = (off as usize, off as usize + len as usize);
                    return self.strings.get(start..end);
                }
            }
        }
        None
    }
}

/// Check one record table: keys strictly ascending, every name slice
/// in-bounds, non-empty, length-bounded, and on character boundaries.
fn validate_table(table: &[u8], strings: &str) -> Result<(), TableError> {
    let count = table.len() / RECORD_LEN;
    let mut previous: Option<u32> = None;
    for i in 0..count {
        let (key, off, len) = record(table, i);
        if previous.is_some_and(|p| p >= key) {
            return Err(TableError::UnsortedRecords);
        }
        previous = Some(key);
        let (start, end) = (off as usize, (off as usize).wrapping_add(len as usize));
        if len == 0 || len as usize > MAX_NAME_BYTES || strings.get(start..end).is_none() {
            return Err(TableError::BadNameSlice);
        }
    }
    Ok(())
}
