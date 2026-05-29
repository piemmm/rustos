//! Minimal flat device-tree (FDT v17) parser.
//!
//! Used by two independent callers, per `AGENTS.md` §2.3 / §6:
//!
//! * `drivers/bus/mmio` — iterates `compatible = "virtio,mmio"` nodes
//!   to enumerate the `virt`-style MMIO virtio transport slots.
//! * later platform code — reads the boot DTB handed in by the
//!   kernel boot capability to extract CPU topology and reserved
//!   memory regions (Stage 4 follow-up; see `PLAN.md` Stage 4
//!   sub-bullet on the bus drivers landing).
//!
//! The parser is `no_std`, allocation-free, and never panics. Every
//! fallible operation returns [`DtbError`]; invalid blobs cannot
//! produce undefined behaviour or unbounded work — the parser refuses
//! the blob up front in [`Dtb::parse`] by validating the header and
//! cross-checking every span against `total_size`. The two structural
//! cursors ([`StructCursor`] and the property reader) only walk
//! pre-validated byte ranges thereafter.
//!
//! Only the subset of the Devicetree Specification v0.4 actually
//! required by RustOS is implemented: header validation, the struct
//! and strings blocks, node entry / property / end tokens, and
//! standard `reg` / `compatible` / `#address-cells` / `#size-cells`
//! readers. Adding new accessors is welcome; speculative coverage is
//! not (`AGENTS.md` §2.3).

use core::str;

/// FDT magic word (`0xd00dfeed`, big-endian on the wire).
pub const FDT_MAGIC: u32 = 0xd00d_feed;

/// Highest FDT version this parser accepts.
///
/// v17 is the long-standing baseline shipped by every modern firmware
/// and by QEMU; future versions must opt-in through a new ABI.
pub const FDT_SUPPORTED_VERSION: u32 = 17;

/// Minimum `last_comp_version` the parser will accept.
///
/// The spec requires that any v17 reader interoperate with blobs
/// declaring `last_comp_version <= 16`.
pub const FDT_MIN_LAST_COMP_VERSION: u32 = 16;

const FDT_BEGIN_NODE: u32 = 0x0000_0001;
const FDT_END_NODE: u32 = 0x0000_0002;
const FDT_PROP: u32 = 0x0000_0003;
const FDT_NOP: u32 = 0x0000_0004;
const FDT_END: u32 = 0x0000_0009;

const HEADER_LEN: usize = 40;

/// Errors produced while parsing an FDT blob.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DtbError {
    /// The blob is shorter than the v17 header.
    BufferTooSmall,
    /// `magic` is not [`FDT_MAGIC`].
    BadMagic,
    /// `version` or `last_comp_version` is outside the supported
    /// range.
    UnsupportedVersion,
    /// A header offset / size field exceeds `total_size` or is
    /// misaligned.
    HeaderOutOfRange,
    /// The struct block contains a token outside the FDT alphabet.
    BadToken,
    /// A property's `nameoff` is outside the strings block, or its
    /// length overflows the struct block.
    BadProperty,
    /// A string referenced by the strings block is not nul-terminated
    /// or is not valid UTF-8.
    BadString,
    /// `FDT_END_NODE` arrived without a matching `FDT_BEGIN_NODE`.
    UnbalancedNodes,
    /// The struct block ran out of bytes before `FDT_END`.
    UnexpectedEnd,
}

/// Parsed view over a flat device-tree blob.
///
/// The parser borrows the underlying bytes; no copy is made.
#[derive(Copy, Clone, Debug)]
pub struct Dtb<'a> {
    structs: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Dtb<'a> {
    /// Validate the FDT header and return a parsed view.
    ///
    /// # Errors
    ///
    /// Returns [`DtbError`] for any header or span inconsistency; on
    /// success every subsequent traversal is guaranteed to remain
    /// inside `bytes`.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, DtbError> {
        if bytes.len() < HEADER_LEN {
            return Err(DtbError::BufferTooSmall);
        }
        let magic = read_be_u32(bytes, 0);
        if magic != FDT_MAGIC {
            return Err(DtbError::BadMagic);
        }
        let total_size = read_be_u32(bytes, 4) as usize;
        let off_dt_struct = read_be_u32(bytes, 8) as usize;
        let off_dt_strings = read_be_u32(bytes, 12) as usize;
        let _off_mem_rsvmap = read_be_u32(bytes, 16) as usize;
        let version = read_be_u32(bytes, 20);
        let last_comp_version = read_be_u32(bytes, 24);
        let size_dt_strings = read_be_u32(bytes, 32) as usize;
        let size_dt_struct = read_be_u32(bytes, 36) as usize;

        if version > FDT_SUPPORTED_VERSION || last_comp_version > FDT_MIN_LAST_COMP_VERSION {
            return Err(DtbError::UnsupportedVersion);
        }
        if total_size > bytes.len() {
            return Err(DtbError::HeaderOutOfRange);
        }
        // Struct block must be 4-byte aligned and inside the blob.
        if off_dt_struct % 4 != 0
            || size_dt_struct % 4 != 0
            || off_dt_struct
                .checked_add(size_dt_struct)
                .map_or(true, |e| e > total_size)
        {
            return Err(DtbError::HeaderOutOfRange);
        }
        if off_dt_strings
            .checked_add(size_dt_strings)
            .map_or(true, |e| e > total_size)
        {
            return Err(DtbError::HeaderOutOfRange);
        }
        Ok(Self {
            structs: &bytes[off_dt_struct..off_dt_struct + size_dt_struct],
            strings: &bytes[off_dt_strings..off_dt_strings + size_dt_strings],
        })
    }

    /// Iterate every node in document order.
    #[must_use]
    pub fn nodes(&self) -> NodeIter<'a> {
        NodeIter {
            cursor: StructCursor::new(self.structs),
            strings: self.strings,
            depth: 0,
        }
    }
}

/// A single device-tree node visited by [`NodeIter`].
#[derive(Copy, Clone, Debug)]
pub struct Node<'a> {
    /// Node name (e.g. `"virtio_mmio@a000000"`). Empty for the root.
    pub name: &'a str,
    /// Depth within the tree; the root is `0`.
    pub depth: u32,
    /// Borrowed property block starting at the first `FDT_PROP`
    /// token belonging to this node.
    props: &'a [u8],
    strings: &'a [u8],
}

impl<'a> Node<'a> {
    /// Iterate this node's properties (immediate children only).
    #[must_use]
    pub fn properties(&self) -> PropIter<'a> {
        PropIter {
            cursor: StructCursor::new(self.props),
            strings: self.strings,
        }
    }

    /// Return the property named `name`, if present.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<Property<'a>> {
        self.properties().find_map(|p| match p {
            Ok(p) if p.name == name => Some(p),
            _ => None,
        })
    }

    /// `true` iff this node's `compatible` property contains `target`
    /// as one of its nul-separated strings.
    #[must_use]
    pub fn is_compatible(&self, target: &str) -> bool {
        let Some(p) = self.property("compatible") else {
            return false;
        };
        p.iter_strings().any(|s| s == target)
    }
}

/// A single property of a [`Node`].
#[derive(Copy, Clone, Debug)]
pub struct Property<'a> {
    /// Property name (e.g. `"reg"`, `"compatible"`).
    pub name: &'a str,
    /// Raw property payload (big-endian on the wire).
    pub value: &'a [u8],
}

impl<'a> Property<'a> {
    /// Iterate the nul-separated strings inside a stringlist
    /// property such as `compatible`.
    #[must_use]
    pub fn iter_strings(&self) -> StringList<'a> {
        StringList { rem: self.value }
    }

    /// Read a single big-endian `u32` at `offset` inside the value.
    ///
    /// # Errors
    ///
    /// Returns [`DtbError::BadProperty`] if the offset / size is out
    /// of range.
    pub fn read_be_u32(&self, offset: usize) -> Result<u32, DtbError> {
        if offset.checked_add(4).map_or(true, |e| e > self.value.len()) {
            return Err(DtbError::BadProperty);
        }
        Ok(read_be_u32(self.value, offset))
    }

    /// Read a single big-endian `u64` at `offset` inside the value.
    ///
    /// # Errors
    ///
    /// Returns [`DtbError::BadProperty`] if the offset / size is out
    /// of range.
    pub fn read_be_u64(&self, offset: usize) -> Result<u64, DtbError> {
        if offset.checked_add(8).map_or(true, |e| e > self.value.len()) {
            return Err(DtbError::BadProperty);
        }
        let hi = read_be_u32(self.value, offset);
        let lo = read_be_u32(self.value, offset + 4);
        Ok((u64::from(hi) << 32) | u64::from(lo))
    }
}

/// Iterator over the strings inside a stringlist property.
#[derive(Clone, Debug)]
pub struct StringList<'a> {
    rem: &'a [u8],
}

impl<'a> Iterator for StringList<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rem.is_empty() {
            return None;
        }
        let nul = self.rem.iter().position(|&b| b == 0)?;
        let (head, tail) = self.rem.split_at(nul);
        self.rem = tail.get(1..).unwrap_or(&[]);
        str::from_utf8(head).ok()
    }
}

/// Iterator over the nodes of a [`Dtb`] in document order.
#[derive(Clone, Debug)]
pub struct NodeIter<'a> {
    cursor: StructCursor<'a>,
    strings: &'a [u8],
    depth: u32,
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Result<Node<'a>, DtbError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let token = match self.cursor.next_token() {
                Ok(Some(t)) => t,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            match token {
                Token::BeginNode(name) => {
                    let depth = self.depth;
                    self.depth += 1;
                    // Snapshot the cursor remainder so the caller
                    // iterates this node's properties starting after
                    // the name token; properties precede children in
                    // valid DTBs (Devicetree Spec v0.4 §5.4.2) so
                    // `PropIter` stops at the first nested node.
                    let props = self.cursor.remaining();
                    return Some(Ok(Node {
                        name,
                        depth,
                        props,
                        strings: self.strings,
                    }));
                }
                Token::EndNode => {
                    if self.depth == 0 {
                        return Some(Err(DtbError::UnbalancedNodes));
                    }
                    self.depth -= 1;
                }
                Token::Prop { .. } | Token::Nop => {}
                Token::End => return None,
            }
        }
    }
}

/// Iterator over the properties immediately under a [`Node`].
#[derive(Clone, Debug)]
pub struct PropIter<'a> {
    cursor: StructCursor<'a>,
    strings: &'a [u8],
}

impl<'a> Iterator for PropIter<'a> {
    type Item = Result<Property<'a>, DtbError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let token = match self.cursor.next_token() {
                Ok(Some(t)) => t,
                Ok(None) => return None,
                Err(e) => return Some(Err(e)),
            };
            match token {
                Token::Prop { name_off, value } => {
                    let name = match read_cstring(self.strings, name_off as usize) {
                        Ok(s) => s,
                        Err(e) => return Some(Err(e)),
                    };
                    return Some(Ok(Property { name, value }));
                }
                Token::Nop => {}
                // Property iteration stops at the first non-prop
                // boundary: a child node, an end-of-node marker, or
                // the end of the struct block.
                Token::BeginNode(_) | Token::EndNode | Token::End => return None,
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Token<'a> {
    BeginNode(&'a str),
    EndNode,
    Prop { name_off: u32, value: &'a [u8] },
    Nop,
    End,
}

#[derive(Copy, Clone, Debug)]
struct StructCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> StructCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> &'a [u8] {
        self.bytes.get(self.pos..).unwrap_or(&[])
    }

    fn read_be_u32(&mut self) -> Result<u32, DtbError> {
        if self
            .pos
            .checked_add(4)
            .map_or(true, |e| e > self.bytes.len())
        {
            return Err(DtbError::UnexpectedEnd);
        }
        let v = read_be_u32(self.bytes, self.pos);
        self.pos += 4;
        Ok(v)
    }

    fn next_token(&mut self) -> Result<Option<Token<'a>>, DtbError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let token = self.read_be_u32()?;
        match token {
            FDT_BEGIN_NODE => {
                let name = self.read_node_name()?;
                Ok(Some(Token::BeginNode(name)))
            }
            FDT_END_NODE => Ok(Some(Token::EndNode)),
            FDT_PROP => {
                let len = self.read_be_u32()? as usize;
                let name_off = self.read_be_u32()?;
                if self
                    .pos
                    .checked_add(len)
                    .map_or(true, |e| e > self.bytes.len())
                {
                    return Err(DtbError::BadProperty);
                }
                let value = &self.bytes[self.pos..self.pos + len];
                self.pos += len;
                self.align_to_4();
                Ok(Some(Token::Prop { name_off, value }))
            }
            FDT_NOP => Ok(Some(Token::Nop)),
            FDT_END => Ok(Some(Token::End)),
            _ => Err(DtbError::BadToken),
        }
    }

    fn read_node_name(&mut self) -> Result<&'a str, DtbError> {
        let start = self.pos;
        let rel = self.bytes.get(start..).ok_or(DtbError::UnexpectedEnd)?;
        let nul = rel
            .iter()
            .position(|&b| b == 0)
            .ok_or(DtbError::BadString)?;
        let name = str::from_utf8(&rel[..nul]).map_err(|_| DtbError::BadString)?;
        self.pos = start + nul + 1;
        self.align_to_4();
        Ok(name)
    }

    fn align_to_4(&mut self) {
        let aligned = (self.pos + 3) & !3;
        // Clamp to `bytes.len()` so the next read sees EOF rather
        // than an out-of-bounds index.
        self.pos = aligned.min(self.bytes.len());
    }
}

#[inline]
fn read_be_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_cstring(bytes: &[u8], offset: usize) -> Result<&str, DtbError> {
    let rel = bytes.get(offset..).ok_or(DtbError::BadProperty)?;
    let nul = rel
        .iter()
        .position(|&b| b == 0)
        .ok_or(DtbError::BadString)?;
    str::from_utf8(&rel[..nul]).map_err(|_| DtbError::BadString)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid FDT v17 blob: a root node containing one
    /// child `virtio_mmio@a000000` with `compatible = "virtio,mmio"`
    /// and `reg = <0x0a000000 0x200>`.
    fn build_blob() -> alloc::vec::Vec<u8> {
        // Strings block.
        let mut strings: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let off_compatible = u32::try_from(strings.len()).expect("test strings offset fits u32");
        strings.extend_from_slice(b"compatible\0");
        let off_reg = u32::try_from(strings.len()).expect("test strings offset fits u32");
        strings.extend_from_slice(b"reg\0");

        // Struct block.
        let mut structs: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        // Root.
        structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        // Empty name + nul + padding to 4.
        structs.extend_from_slice(&[0, 0, 0, 0]);
        // Child node.
        structs.extend_from_slice(&FDT_BEGIN_NODE.to_be_bytes());
        let name = b"virtio_mmio@a000000\0";
        structs.extend_from_slice(name);
        while structs.len() % 4 != 0 {
            structs.push(0);
        }
        // compatible = "virtio,mmio\0"
        let compat = b"virtio,mmio\0";
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(
            &u32::try_from(compat.len())
                .expect("compat len fits u32")
                .to_be_bytes(),
        );
        structs.extend_from_slice(&off_compatible.to_be_bytes());
        structs.extend_from_slice(compat);
        while structs.len() % 4 != 0 {
            structs.push(0);
        }
        // reg = <0x0a000000 0x00000200> (eight bytes, big-endian)
        structs.extend_from_slice(&FDT_PROP.to_be_bytes());
        structs.extend_from_slice(&8u32.to_be_bytes());
        structs.extend_from_slice(&off_reg.to_be_bytes());
        structs.extend_from_slice(&0x0a00_0000u32.to_be_bytes());
        structs.extend_from_slice(&0x0000_0200u32.to_be_bytes());
        // Close child.
        structs.extend_from_slice(&FDT_END_NODE.to_be_bytes());
        // Close root.
        structs.extend_from_slice(&FDT_END_NODE.to_be_bytes());
        // End-of-struct.
        structs.extend_from_slice(&FDT_END.to_be_bytes());

        // Assemble blob: header (40) + struct + strings.
        let off_struct = u32::try_from(HEADER_LEN).expect("header len fits u32");
        let size_struct = u32::try_from(structs.len()).expect("test structs fit u32");
        let off_strings = off_struct + size_struct;
        let size_strings = u32::try_from(strings.len()).expect("test strings fit u32");
        let total = off_strings + size_strings;
        let mut blob = alloc::vec::Vec::new();
        blob.extend_from_slice(&FDT_MAGIC.to_be_bytes());
        blob.extend_from_slice(&total.to_be_bytes());
        blob.extend_from_slice(&off_struct.to_be_bytes());
        blob.extend_from_slice(&off_strings.to_be_bytes());
        blob.extend_from_slice(&0u32.to_be_bytes()); // off_mem_rsvmap
        blob.extend_from_slice(&FDT_SUPPORTED_VERSION.to_be_bytes());
        blob.extend_from_slice(&FDT_MIN_LAST_COMP_VERSION.to_be_bytes());
        blob.extend_from_slice(&0u32.to_be_bytes()); // boot_cpuid_phys
        blob.extend_from_slice(&size_strings.to_be_bytes());
        blob.extend_from_slice(&size_struct.to_be_bytes());
        blob.extend_from_slice(&structs);
        blob.extend_from_slice(&strings);
        blob
    }

    extern crate alloc;

    fn parse_err(bytes: &[u8]) -> DtbError {
        match Dtb::parse(bytes) {
            Ok(_) => panic!("expected DtbError"),
            Err(e) => e,
        }
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(parse_err(&[]), DtbError::BufferTooSmall);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut blob = build_blob();
        blob[0] ^= 0xFF;
        assert_eq!(parse_err(&blob), DtbError::BadMagic);
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut blob = build_blob();
        // Bump `version` to 99.
        blob[20..24].copy_from_slice(&99u32.to_be_bytes());
        assert_eq!(parse_err(&blob), DtbError::UnsupportedVersion);
    }

    #[test]
    fn rejects_struct_span_out_of_range() {
        let mut blob = build_blob();
        // Force `size_dt_struct` past EOF.
        blob[36..40].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        assert_eq!(parse_err(&blob), DtbError::HeaderOutOfRange);
    }

    #[test]
    fn iterates_root_and_child() {
        let blob = build_blob();
        let dtb = Dtb::parse(&blob).expect("blob parses");
        let mut names = alloc::vec::Vec::new();
        for n in dtb.nodes() {
            let n = n.expect("node decodes");
            names.push((n.depth, n.name));
        }
        assert_eq!(
            names,
            alloc::vec![(0u32, ""), (1u32, "virtio_mmio@a000000")]
        );
    }

    #[test]
    fn finds_compatible_and_reg() {
        let blob = build_blob();
        let dtb = Dtb::parse(&blob).expect("blob parses");
        let child = dtb
            .nodes()
            .find_map(|n| n.ok().filter(|n| n.depth == 1))
            .expect("child present");
        assert!(child.is_compatible("virtio,mmio"));
        let reg = child.property("reg").expect("reg property");
        assert_eq!(reg.read_be_u32(0).unwrap(), 0x0a00_0000);
        assert_eq!(reg.read_be_u32(4).unwrap(), 0x0000_0200);
    }

    #[test]
    fn read_be_u64_round_trip() {
        let v = Property {
            name: "reg",
            value: &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88],
        };
        assert_eq!(v.read_be_u64(0).unwrap(), 0x1122_3344_5566_7788);
        assert_eq!(v.read_be_u64(1), Err(DtbError::BadProperty));
    }

    #[test]
    fn string_list_iterates_nul_separated_entries() {
        let p = Property {
            name: "compatible",
            value: b"alpha\0beta\0",
        };
        let v: alloc::vec::Vec<&str> = p.iter_strings().collect();
        assert_eq!(v, alloc::vec!["alpha", "beta"]);
    }
}
