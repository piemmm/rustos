//! The `pci.ids`/`usb.ids` snapshot parser, vetting filter, and compact
//! lookup-table encoder (host/generator side; needs `alloc`).
//!
//! Both public databases share one line grammar: `#` comments, blank lines,
//! and tab-indented entries of the form `id`, two spaces, `name`. Depth 0
//! holds vendors and tagged section headers (`C 03  Display controller`),
//! depth 1 their children, depth 2 the grandchildren (PCI subsystems,
//! class prog-ifs). [`parse`] validates the *whole* file under that grammar
//! and rejects it on the first deviation — never skip-and-continue, so a
//! smuggled section cannot hide (the raw download is untrusted input whose
//! strings end up on users' terminals).
//!
//! The vetting rules, in one place:
//!
//! - the source must be valid UTF-8 and at most [`MAX_SOURCE_BYTES`];
//! - no control characters other than the structural newline, and tab only
//!   as indentation (or inside comments): no ESC/CSI/C0/C1 bytes, so a
//!   hostile entry cannot inject terminal escape sequences through `lspci`
//!   output;
//! - ids are exact-width lowercase hex in the emitted scopes (vendors,
//!   devices, subsystems, the `C` class tables); the auxiliary `usb.ids`
//!   tables (`AT`, `HID`, `R`, `BIAS`, `PHY`, `HUT`, `L`, `HCC`, `VT`)
//!   accept 1–4 hex digits of either case, as upstream actually publishes
//!   them;
//! - names are non-empty after trimming spaces and at most
//!   [`MAX_NAME_BYTES`];
//! - duplicate ids within a scope reject the import, as does any entry
//!   without its parent, any unknown section tag, and any nesting deeper
//!   than the grammar defines;
//! - the total entry count is bounded by [`MAX_TABLE_ENTRIES`].
//!
//! [`ParsedDb::encode`] emits the compact table [`crate::DevIds`] decodes:
//! deterministic output (sorted maps, first-use string interning), so the
//! CI drift gate compares byte-for-byte. PCI subsystem entries and the
//! auxiliary USB tables are validated but not encoded: no consumer renders
//! them today (the hardware tree records no subsystem ids), and an unused
//! table would be speculative surface.

use alloc::collections::btree_map::Entry;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::{DbKind, MAX_NAME_BYTES, MAX_SOURCE_BYTES, MAX_TABLE_ENTRIES};

/// Why a snapshot was rejected, positioned at the offending line.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    /// 1-based line number of the rejection (0 for whole-file failures).
    pub line: usize,
    /// The vetting rule the line broke.
    pub kind: ParseErrorKind,
}

/// The vetting rule a rejected snapshot broke.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseErrorKind {
    /// The source exceeds [`MAX_SOURCE_BYTES`].
    SourceTooLarge,
    /// The source is not valid UTF-8.
    NotUtf8,
    /// A control character other than the structural tab/newline.
    ControlChar,
    /// A tab inside an entry body (tabs are indentation only).
    TabInEntry,
    /// A comment line indented by tabs.
    IndentedComment,
    /// More than two levels of tab indentation.
    TooDeep,
    /// An entry without the `id`, two-spaces, `name` shape.
    MissingSeparator,
    /// An id with the wrong width or character set for its scope.
    BadId,
    /// An empty name after trimming spaces.
    EmptyName,
    /// A name longer than [`MAX_NAME_BYTES`].
    NameTooLong,
    /// A depth-0 section tag outside the database's closed set.
    UnknownSection,
    /// A child entry with no parent entry above it.
    OrphanEntry,
    /// A nesting depth the current section does not define.
    UnexpectedDepth,
    /// An id repeated within its scope.
    DuplicateId,
    /// More than [`MAX_TABLE_ENTRIES`] entries in total.
    TooManyEntries,
    /// The file defines no vendors at all (a truncated or empty download).
    Empty,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ParseErrorKind::SourceTooLarge => "source exceeds the size bound",
            ParseErrorKind::NotUtf8 => "source is not valid UTF-8",
            ParseErrorKind::ControlChar => "control character in line",
            ParseErrorKind::TabInEntry => "tab inside an entry body",
            ParseErrorKind::IndentedComment => "comment indented by tabs",
            ParseErrorKind::TooDeep => "indented deeper than two tabs",
            ParseErrorKind::MissingSeparator => "entry is not `id`, two spaces, `name`",
            ParseErrorKind::BadId => "id has the wrong width or characters for its scope",
            ParseErrorKind::EmptyName => "entry name is empty",
            ParseErrorKind::NameTooLong => "entry name exceeds the length bound",
            ParseErrorKind::UnknownSection => "unknown section tag",
            ParseErrorKind::OrphanEntry => "child entry has no parent",
            ParseErrorKind::UnexpectedDepth => "nesting depth not defined for this section",
            ParseErrorKind::DuplicateId => "duplicate id within its scope",
            ParseErrorKind::TooManyEntries => "entry count exceeds the bound",
            ParseErrorKind::Empty => "database defines no vendors",
        };
        write!(f, "line {}: {what}", self.line)
    }
}

/// Entry counts of a parsed snapshot, for generator reporting and tests.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    /// Depth-0 vendor entries.
    pub vendors: usize,
    /// Depth-1 device (USB: product) entries.
    pub devices: usize,
    /// Depth-2 PCI `subvendor subdevice` entries (validated, not encoded).
    pub subsystems: usize,
    /// `C` section class entries.
    pub classes: usize,
    /// `C` section subclass entries.
    pub subclasses: usize,
    /// `C` section prog-if (USB: protocol) entries.
    pub prog_ifs: usize,
    /// Auxiliary-section entries (validated, not encoded).
    pub aux: usize,
}

/// A device entry and its validated (unencoded) subsystem children.
#[derive(Debug)]
struct Device {
    name: String,
    subsystems: BTreeSet<u32>,
}

/// A vendor entry and its device children.
#[derive(Debug)]
struct Vendor {
    name: String,
    devices: BTreeMap<u16, Device>,
}

/// A subclass entry and its prog-if children.
#[derive(Debug)]
struct Subclass {
    name: String,
    prog_ifs: BTreeMap<u8, String>,
}

/// A class entry and its subclass children.
#[derive(Debug)]
struct Class {
    name: String,
    subclasses: BTreeMap<u8, Subclass>,
}

/// A fully vetted snapshot, ready to encode.
///
/// Construction ([`parse`]) is the vetting gate; a `ParsedDb` therefore
/// only ever holds data that passed every rule in the module docs.
#[derive(Debug)]
pub struct ParsedDb {
    kind: DbKind,
    vendors: BTreeMap<u16, Vendor>,
    classes: BTreeMap<u8, Class>,
    counts: Counts,
}

/// The auxiliary `usb.ids` section tags upstream publishes today. A new
/// upstream section is a deliberate grammar change: the import stops for
/// human review instead of silently accepting unknown structure.
const USB_AUX_TAGS: &[&str] = &["AT", "HID", "R", "BIAS", "PHY", "HUT", "L", "HCC", "VT"];

/// Where the parse cursor sits between lines: the current section and the
/// parent entries child lines attach to.
enum Cursor {
    /// The untagged vendor section (the file's opening section).
    Vendors {
        /// The open vendor a depth-1 device attaches to.
        vendor: Option<u16>,
        /// The open device a depth-2 PCI subsystem attaches to.
        device: Option<u16>,
    },
    /// A `C` class-table section header and its open children.
    Class {
        /// The class of the section header line itself.
        class: u8,
        /// The open subclass a depth-2 prog-if attaches to.
        subclass: Option<u8>,
    },
    /// An auxiliary section header (`USB_AUX_TAGS`), scoping its children.
    Aux {
        /// Index into [`USB_AUX_TAGS`] of the section's tag.
        tag: usize,
        /// The section header's own id.
        id: u32,
    },
}

/// The hex value of `id` when it is 1–8 digits of the permitted case.
fn hex_value(id: &str, lowercase_only: bool) -> Option<u32> {
    if id.is_empty() || id.len() > 8 {
        return None;
    }
    let mut value = 0u32;
    for c in id.chars() {
        let digit = match c {
            '0'..='9' => u32::from(c) - u32::from('0'),
            'a'..='f' => u32::from(c) - u32::from('a') + 10,
            'A'..='F' if !lowercase_only => u32::from(c) - u32::from('A') + 10,
            _ => return None,
        };
        value = value << 4 | digit;
    }
    Some(value)
}

/// The value of an exact-width lowercase-hex id, the strict form required
/// in every scope the encoder emits.
fn hex_exact(id: &str, width: usize) -> Option<u32> {
    (id.len() == width).then(|| hex_value(id, true)).flatten()
}

/// Split an entry body into its id and name per the shared grammar: the id
/// runs to the first two-space separator, the name is the rest with
/// surrounding spaces trimmed (upstream carries a handful of stray spaces).
fn split_entry(body: &str, line: usize) -> Result<(&str, &str), ParseError> {
    let at = body.find("  ").ok_or(ParseError {
        line,
        kind: ParseErrorKind::MissingSeparator,
    })?;
    let id = &body[..at];
    if id.is_empty() {
        return Err(ParseError {
            line,
            kind: ParseErrorKind::MissingSeparator,
        });
    }
    let name = body[at + 2..].trim_matches(' ');
    if name.is_empty() {
        return Err(ParseError {
            line,
            kind: ParseErrorKind::EmptyName,
        });
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(ParseError {
            line,
            kind: ParseErrorKind::NameTooLong,
        });
    }
    Ok((id, name))
}

/// The uppercase section tag opening `body`, if any, and the rest of the
/// body after it. Vendor lines start with lowercase hex, so any leading
/// `A–Z` run followed by a space is a section header.
fn split_tag(body: &str) -> Option<(&str, &str)> {
    let end = body.find(|c: char| !c.is_ascii_uppercase())?;
    (end > 0 && body[end..].starts_with(' ')).then(|| (&body[..end], &body[end + 1..]))
}

/// Walking state threaded through [`parse`]: the accumulating model, the
/// cursor, and the bounded entry budget.
struct Parser {
    kind: DbKind,
    vendors: BTreeMap<u16, Vendor>,
    classes: BTreeMap<u8, Class>,
    /// Auxiliary sections: `aux[tag][section id]` is the set of child ids,
    /// kept only for duplicate detection (validated, never encoded).
    aux: BTreeMap<usize, BTreeMap<u32, BTreeSet<u32>>>,
    counts: Counts,
    total: u32,
    cursor: Cursor,
}

impl Parser {
    fn reject(line: usize, kind: ParseErrorKind) -> ParseError {
        ParseError { line, kind }
    }

    /// Account one accepted entry against the total bound.
    fn charge(&mut self, line: usize) -> Result<(), ParseError> {
        self.total += 1;
        if self.total > MAX_TABLE_ENTRIES {
            return Err(Self::reject(line, ParseErrorKind::TooManyEntries));
        }
        Ok(())
    }

    /// One depth-0 line: a vendor entry or a tagged section header.
    fn depth0(&mut self, body: &str, line: usize) -> Result<(), ParseError> {
        self.charge(line)?;
        if let Some((tag, rest)) = split_tag(body) {
            let (id, name) = split_entry(rest, line)?;
            if tag == "C" {
                let class =
                    hex_exact(id, 2).ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                let class =
                    u8::try_from(class).map_err(|_| Self::reject(line, ParseErrorKind::BadId))?;
                match self.classes.entry(class) {
                    Entry::Occupied(_) => {
                        return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(Class {
                            name: String::from(name),
                            subclasses: BTreeMap::new(),
                        });
                    }
                }
                self.counts.classes += 1;
                self.cursor = Cursor::Class {
                    class,
                    subclass: None,
                };
                return Ok(());
            }
            let aux_tag = (self.kind == DbKind::Usb)
                .then(|| USB_AUX_TAGS.iter().position(|&t| t == tag))
                .flatten()
                .ok_or_else(|| Self::reject(line, ParseErrorKind::UnknownSection))?;
            let id = (id.len() <= 4)
                .then(|| hex_value(id, false))
                .flatten()
                .ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
            let section = self.aux.entry(aux_tag).or_default();
            if section.insert(id, BTreeSet::new()).is_some() {
                return Err(Self::reject(line, ParseErrorKind::DuplicateId));
            }
            self.counts.aux += 1;
            self.cursor = Cursor::Aux { tag: aux_tag, id };
            return Ok(());
        }
        let (id, name) = split_entry(body, line)?;
        let vendor = hex_exact(id, 4).ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
        let vendor =
            u16::try_from(vendor).map_err(|_| Self::reject(line, ParseErrorKind::BadId))?;
        match self.vendors.entry(vendor) {
            Entry::Occupied(_) => return Err(Self::reject(line, ParseErrorKind::DuplicateId)),
            Entry::Vacant(slot) => {
                slot.insert(Vendor {
                    name: String::from(name),
                    devices: BTreeMap::new(),
                });
            }
        }
        self.counts.vendors += 1;
        self.cursor = Cursor::Vendors {
            vendor: Some(vendor),
            device: None,
        };
        Ok(())
    }

    /// One depth-1 line: a device, subclass, or auxiliary child.
    fn depth1(&mut self, body: &str, line: usize) -> Result<(), ParseError> {
        self.charge(line)?;
        let (id, name) = split_entry(body, line)?;
        match self.cursor {
            Cursor::Vendors { vendor, .. } => {
                let vendor =
                    vendor.ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                let device =
                    hex_exact(id, 4).ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                let device =
                    u16::try_from(device).map_err(|_| Self::reject(line, ParseErrorKind::BadId))?;
                // The vendor is always present: the cursor only holds ids
                // just inserted into the map.
                let entry = self
                    .vendors
                    .get_mut(&vendor)
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                match entry.devices.entry(device) {
                    Entry::Occupied(_) => {
                        return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(Device {
                            name: String::from(name),
                            subsystems: BTreeSet::new(),
                        });
                    }
                }
                self.counts.devices += 1;
                self.cursor = Cursor::Vendors {
                    vendor: Some(vendor),
                    device: Some(device),
                };
            }
            Cursor::Class { class, .. } => {
                let sub =
                    hex_exact(id, 2).ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                let sub =
                    u8::try_from(sub).map_err(|_| Self::reject(line, ParseErrorKind::BadId))?;
                let entry = self
                    .classes
                    .get_mut(&class)
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                match entry.subclasses.entry(sub) {
                    Entry::Occupied(_) => {
                        return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(Subclass {
                            name: String::from(name),
                            prog_ifs: BTreeMap::new(),
                        });
                    }
                }
                self.counts.subclasses += 1;
                self.cursor = Cursor::Class {
                    class,
                    subclass: Some(sub),
                };
            }
            Cursor::Aux { tag, id: section } => {
                let child = (id.len() <= 4)
                    .then(|| hex_value(id, false))
                    .flatten()
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                let children = self
                    .aux
                    .get_mut(&tag)
                    .and_then(|s| s.get_mut(&section))
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                if !children.insert(child) {
                    return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                }
                self.counts.aux += 1;
            }
        }
        Ok(())
    }

    /// One depth-2 line: a PCI subsystem or a class prog-if.
    fn depth2(&mut self, body: &str, line: usize) -> Result<(), ParseError> {
        self.charge(line)?;
        let (id, name) = split_entry(body, line)?;
        match self.cursor {
            Cursor::Vendors { vendor, device } => {
                if self.kind != DbKind::Pci {
                    return Err(Self::reject(line, ParseErrorKind::UnexpectedDepth));
                }
                let (vendor, device) = vendor
                    .zip(device)
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                let (sv, sd) = id
                    .split_once(' ')
                    .and_then(|(a, b)| hex_exact(a, 4).zip(hex_exact(b, 4)))
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                // The subsystem name passed the split_entry vetting above;
                // it is not encoded (no consumer renders subsystem names).
                let _ = name;
                let entry = self
                    .vendors
                    .get_mut(&vendor)
                    .and_then(|v| v.devices.get_mut(&device))
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                if !entry.subsystems.insert(sv << 16 | sd) {
                    return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                }
                self.counts.subsystems += 1;
            }
            Cursor::Class { class, subclass } => {
                let subclass =
                    subclass.ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                let prog =
                    hex_exact(id, 2).ok_or_else(|| Self::reject(line, ParseErrorKind::BadId))?;
                let prog =
                    u8::try_from(prog).map_err(|_| Self::reject(line, ParseErrorKind::BadId))?;
                let entry = self
                    .classes
                    .get_mut(&class)
                    .and_then(|c| c.subclasses.get_mut(&subclass))
                    .ok_or_else(|| Self::reject(line, ParseErrorKind::OrphanEntry))?;
                match entry.prog_ifs.entry(prog) {
                    Entry::Occupied(_) => {
                        return Err(Self::reject(line, ParseErrorKind::DuplicateId));
                    }
                    Entry::Vacant(slot) => {
                        slot.insert(String::from(name));
                    }
                }
                self.counts.prog_ifs += 1;
            }
            Cursor::Aux { .. } => {
                return Err(Self::reject(line, ParseErrorKind::UnexpectedDepth));
            }
        }
        Ok(())
    }
}

/// Parse and vet `bytes` as a `kind` snapshot.
///
/// # Errors
///
/// Any deviation from the vetting rules in the module docs rejects the
/// whole file; see [`ParseErrorKind`].
pub fn parse(kind: DbKind, bytes: &[u8]) -> Result<ParsedDb, ParseError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(ParseError {
            line: 0,
            kind: ParseErrorKind::SourceTooLarge,
        });
    }
    let text = core::str::from_utf8(bytes).map_err(|e| ParseError {
        line: bytes[..e.valid_up_to()].split(|&b| b == b'\n').count(),
        kind: ParseErrorKind::NotUtf8,
    })?;
    let mut parser = Parser {
        kind,
        vendors: BTreeMap::new(),
        classes: BTreeMap::new(),
        aux: BTreeMap::new(),
        counts: Counts::default(),
        total: 0,
        cursor: Cursor::Vendors {
            vendor: None,
            device: None,
        },
    };
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        if raw.is_empty() {
            continue;
        }
        let depth = raw.len() - raw.trim_start_matches('\t').len();
        let body = &raw[depth..];
        let comment = depth == 0 && body.starts_with('#');
        for c in body.chars() {
            if c == '\t' {
                if !comment {
                    return Err(ParseError {
                        line,
                        kind: ParseErrorKind::TabInEntry,
                    });
                }
            } else if c.is_control() {
                return Err(ParseError {
                    line,
                    kind: ParseErrorKind::ControlChar,
                });
            }
        }
        if comment {
            continue;
        }
        if body.starts_with('#') {
            return Err(ParseError {
                line,
                kind: ParseErrorKind::IndentedComment,
            });
        }
        match depth {
            0 => parser.depth0(body, line)?,
            1 => parser.depth1(body, line)?,
            2 => parser.depth2(body, line)?,
            _ => {
                return Err(ParseError {
                    line,
                    kind: ParseErrorKind::TooDeep,
                });
            }
        }
    }
    if parser.vendors.is_empty() {
        return Err(ParseError {
            line: 0,
            kind: ParseErrorKind::Empty,
        });
    }
    Ok(ParsedDb {
        kind,
        vendors: parser.vendors,
        classes: parser.classes,
        counts: parser.counts,
    })
}

impl ParsedDb {
    /// Which database this snapshot carries.
    #[must_use]
    pub fn kind(&self) -> DbKind {
        self.kind
    }

    /// Entry counts, for generator reporting and tests.
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Encode the compact lookup table [`crate::DevIds`] decodes.
    ///
    /// Deterministic: iteration follows the sorted maps and strings are
    /// interned at first use, so identical input bytes produce identical
    /// output bytes on every host (the CI drift gate compares
    /// byte-for-byte).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut strings = StringPool::default();
        let mut vendors = Vec::new();
        let mut devices = Vec::new();
        for (&id, vendor) in &self.vendors {
            vendors.push((u32::from(id), strings.intern(&vendor.name)));
            for (&dev, device) in &vendor.devices {
                devices.push((
                    u32::from(id) << 16 | u32::from(dev),
                    strings.intern(&device.name),
                ));
            }
        }
        let mut classes = Vec::new();
        let mut subclasses = Vec::new();
        let mut prog_ifs = Vec::new();
        for (&class, entry) in &self.classes {
            classes.push((u32::from(class), strings.intern(&entry.name)));
            for (&sub, subclass) in &entry.subclasses {
                subclasses.push((
                    u32::from(class) << 8 | u32::from(sub),
                    strings.intern(&subclass.name),
                ));
                for (&prog, name) in &subclass.prog_ifs {
                    prog_ifs.push((
                        u32::from(class) << 16 | u32::from(sub) << 8 | u32::from(prog),
                        strings.intern(name),
                    ));
                }
            }
        }
        let tables = [&vendors, &devices, &classes, &subclasses, &prog_ifs];
        let records: usize = tables.iter().map(|t| t.len()).sum();
        let mut out = Vec::with_capacity(36 + records * 12 + strings.blob.len());
        out.extend_from_slice(&crate::TABLE_MAGIC);
        out.extend_from_slice(&self.kind.code().to_le_bytes());
        for table in tables {
            let len = u32::try_from(table.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_le_bytes());
        }
        let blob_len = u32::try_from(strings.blob.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&blob_len.to_le_bytes());
        for table in tables {
            for &(key, (off, len)) in table {
                out.extend_from_slice(&key.to_le_bytes());
                out.extend_from_slice(&off.to_le_bytes());
                out.extend_from_slice(&len.to_le_bytes());
            }
        }
        out.extend_from_slice(&strings.blob);
        out
    }
}

/// First-use string interning for the encoder's strings blob.
#[derive(Default)]
struct StringPool {
    blob: Vec<u8>,
    seen: BTreeMap<String, (u32, u32)>,
}

impl StringPool {
    /// The `(offset, length)` of `name` in the blob, appending it on first
    /// use. Offsets fit `u32` by the parse-time source-size bound.
    fn intern(&mut self, name: &str) -> (u32, u32) {
        if let Some(&at) = self.seen.get(name) {
            return at;
        }
        let off = u32::try_from(self.blob.len()).unwrap_or(u32::MAX);
        let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        self.blob.extend_from_slice(name.as_bytes());
        self.seen.insert(String::from(name), (off, len));
        (off, len)
    }
}
