//! Fixed-size ADFS directories: the 1280-byte old format (S/M/L) and
//! the 2048-byte new format (D/E/F).
//!
//! A fixed directory is a header (`sequence` byte plus a `Hugo`/`Nick`
//! marker), an array of 26-byte entries terminated by a zero name byte,
//! and a footer repeating the sequence byte and marker around the parent
//! pointer, directory name, and a check byte. Entries are kept in
//! case-insensitive sorted order. References: J.G. Harston, "Acorn 8-Bit
//! ADFS Filesystem Structure" (mdfs.net) and G. Holdsworth, "Guide To
//! Disc Formats".
//!
//! All logic here is pure in-memory buffer manipulation; the caller
//! reads and writes the directory bytes through the map engines.

use crate::volume::{get_u24, get_u32, put_u24, put_u32};
use tairix_abi::DriverError;

/// Byte size of an old-format directory.
pub const OLD_DIR_SIZE: usize = 1280;
/// Byte size of a new-format directory (`u32` form first so the byte
/// form derives from it losslessly).
pub const NEW_DIR_SIZE_U32: u32 = 2048;

/// Byte size of a new-format directory.
pub const NEW_DIR_SIZE: usize = NEW_DIR_SIZE_U32 as usize;
/// Byte size of one directory entry.
pub const ENTRY_SIZE: usize = 26;
/// Entries start after the 5-byte header.
pub const ENTRIES_OFFSET: usize = 5;
/// Maximum name length in a fixed directory.
pub const FIXED_NAME_LEN: usize = 10;

/// Longest object name any ADFS directory format carries (big
/// directories; fixed directories stop at 10).
pub const MAX_NAME_LEN: usize = 255;

/// Owner-read attribute bit (`R`).
pub const ATTR_OWNER_READ: u16 = 1 << 0;
/// Owner-write attribute bit (`W`).
pub const ATTR_OWNER_WRITE: u16 = 1 << 1;
/// Locked-against-deletion attribute bit (`L`).
pub const ATTR_LOCKED: u16 = 1 << 2;
/// The object is a directory (`D`).
pub const ATTR_DIRECTORY: u16 = 1 << 3;

/// One directory object, format-independent.
#[derive(Copy, Clone)]
pub struct Object {
    /// Object name bytes (case-preserved, high bits stripped).
    pub name: [u8; MAX_NAME_LEN],
    /// Live length of `name`.
    pub name_len: usize,
    /// RISC OS load address (or filetype + datestamp high bits).
    pub load: u32,
    /// RISC OS execution address (or datestamp low word).
    pub exec: u32,
    /// Object length in bytes.
    pub size: u32,
    /// Indirect disc address (new map) or start sector (old map).
    pub indaddr: u32,
    /// `FileCore` attribute bits (`ATTR_*`).
    pub attr: u16,
}

impl Object {
    /// An empty object with the given name.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the name is empty or longer
    /// than [`MAX_NAME_LEN`].
    pub fn named(name: &[u8]) -> Result<Self, DriverError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let mut object = Self {
            name: [0; MAX_NAME_LEN],
            name_len: name.len(),
            load: 0,
            exec: 0,
            size: 0,
            indaddr: 0,
            attr: 0,
        };
        object.name[..name.len()].copy_from_slice(name);
        Ok(object)
    }

    /// The object's live name bytes.
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }

    /// Whether the object is a directory.
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
}

/// Case-insensitive ASCII name ordering — the sort order `FileCore`
/// keeps directory entries in.
pub fn name_cmp(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    let a_iter = a.iter().map(u8::to_ascii_lowercase);
    let b_iter = b.iter().map(u8::to_ascii_lowercase);
    a_iter.cmp(b_iter)
}

/// Whether two names refer to the same directory entry.
pub fn name_eq(a: &[u8], b: &[u8]) -> bool {
    name_cmp(a, b) == core::cmp::Ordering::Equal
}

/// The two fixed directory sizes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FixedFormat {
    /// 1280-byte, 47-entry directories (`Hugo` only).
    Old,
    /// 2048-byte, 77-entry directories (`Hugo` or `Nick`).
    New,
}

impl FixedFormat {
    /// Directory size in bytes.
    pub fn size(self) -> usize {
        match self {
            Self::Old => OLD_DIR_SIZE,
            Self::New => NEW_DIR_SIZE,
        }
    }

    /// Maximum number of entries.
    pub fn capacity(self) -> usize {
        match self {
            Self::Old => 47,
            Self::New => 77,
        }
    }

    /// Offset of the footer's end-of-entries marker byte.
    fn tail(self) -> usize {
        match self {
            Self::Old => OLD_DIR_SIZE - 0x35,
            Self::New => NEW_DIR_SIZE - 0x29,
        }
    }

    /// Offset of the 3-byte parent pointer.
    fn parent_at(self) -> usize {
        match self {
            Self::Old => 0x4D6,
            Self::New => 0x7DA,
        }
    }

    /// Offset of the 10-byte directory name.
    fn name_at(self) -> usize {
        match self {
            Self::Old => 0x4CC,
            Self::New => 0x7F0,
        }
    }

    /// Offset of the 19-byte directory title.
    fn title_at(self) -> usize {
        match self {
            Self::Old => 0x4D9,
            Self::New => 0x7DD,
        }
    }
}

/// A fixed-size directory staged in memory.
pub struct FixedDir {
    /// Raw directory bytes (only the first `format.size()` are live).
    pub data: [u8; NEW_DIR_SIZE],
    /// Which fixed format the buffer holds.
    pub format: FixedFormat,
}

impl FixedDir {
    /// Wrap and validate directory bytes.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] if the markers, sequence bytes, end
    /// marker, or check byte are inconsistent.
    pub fn parse(data: &[u8; NEW_DIR_SIZE], format: FixedFormat) -> Result<Self, DriverError> {
        let dir = Self {
            data: *data,
            format,
        };
        dir.validate()?;
        Ok(dir)
    }

    fn validate(&self) -> Result<(), DriverError> {
        let size = self.format.size();
        let marker = &self.data[1..5];
        let allowed = match self.format {
            FixedFormat::Old => marker == b"Hugo",
            FixedFormat::New => marker == b"Hugo" || marker == b"Nick",
        };
        if !allowed {
            return Err(DriverError::BadMagic);
        }
        // Head and tail must agree on the marker and sequence number.
        if self.data[0] != self.data[size - 6] || marker != &self.data[size - 5..size - 1] {
            return Err(DriverError::BadMagic);
        }
        if self.data[self.format.tail()] != 0 {
            return Err(DriverError::BadMagic);
        }
        // A directory must terminate its entry list.
        if self.count() > self.format.capacity() {
            return Err(DriverError::BadMagic);
        }
        // Entry names use printable bytes; a check byte of zero is
        // "unchecked" (always written so by 8-bit ADFS on old
        // directories).
        let check = self.data[size - 1];
        if check != 0 && check != self.check_byte() {
            return Err(DriverError::BadMagic);
        }
        Ok(())
    }

    /// Number of live entries.
    pub fn count(&self) -> usize {
        let mut count = 0;
        while count < self.format.capacity() {
            let at = ENTRIES_OFFSET + count * ENTRY_SIZE;
            if self.data[at] == 0 {
                break;
            }
            count += 1;
        }
        count
    }

    /// Decode the entry at `index`, if it is live.
    pub fn entry(&self, index: usize) -> Option<Object> {
        if index >= self.format.capacity() {
            return None;
        }
        let at = ENTRIES_OFFSET + index * ENTRY_SIZE;
        if self.data[at] == 0 {
            return None;
        }
        let raw = &self.data[at..at + ENTRY_SIZE];
        let mut object = Object {
            name: [0; MAX_NAME_LEN],
            name_len: 0,
            load: get_u32(raw, 10),
            exec: get_u32(raw, 14),
            size: get_u32(raw, 18),
            indaddr: get_u24(raw, 22),
            attr: 0,
        };
        for (i, &byte) in raw[..FIXED_NAME_LEN].iter().enumerate() {
            let ch = byte & 0x7F;
            if ch < 0x20 {
                break;
            }
            object.name[i] = ch;
            object.name_len = i + 1;
        }
        object.attr = match self.format {
            FixedFormat::Old => {
                let mut attr = 0u16;
                for (bit, &byte) in raw[..9].iter().enumerate() {
                    if byte & 0x80 != 0 {
                        attr |= 1 << bit;
                    }
                }
                attr
            }
            FixedFormat::New => u16::from(raw[25]) & 0x7F,
        };
        Some(object)
    }

    /// Encode `object` into the entry slot at `index`.
    fn set_entry(&mut self, index: usize, object: &Object) {
        let at = ENTRIES_OFFSET + index * ENTRY_SIZE;
        let seq = self.data[0];
        let format = self.format;
        let raw = &mut self.data[at..at + ENTRY_SIZE];
        raw.fill(0);
        let name_len = object.name_len.min(FIXED_NAME_LEN);
        raw[..name_len].copy_from_slice(&object.name[..name_len]);
        if name_len < FIXED_NAME_LEN {
            raw[name_len] = 0x0D;
        }
        put_u32(raw, 10, object.load);
        put_u32(raw, 14, object.exec);
        put_u32(raw, 18, object.size);
        put_u24(raw, 22, object.indaddr & 0x00FF_FFFF);
        match format {
            FixedFormat::Old => {
                for (bit, byte) in raw.iter_mut().enumerate().take(9) {
                    if object.attr & (1 << bit) != 0 {
                        *byte |= 0x80;
                    }
                }
                raw[25] = seq;
            }
            FixedFormat::New => {
                raw[25] = (object.attr & 0x7F) as u8;
            }
        }
    }

    /// Find the entry named `name`, returning its index and object.
    pub fn find(&self, name: &[u8]) -> Option<(usize, Object)> {
        for index in 0..self.format.capacity() {
            let object = self.entry(index)?;
            if name_eq(object.name(), name) {
                return Some((index, object));
            }
        }
        None
    }

    /// The parent directory's indirect disc address / start sector.
    pub fn parent(&self) -> u32 {
        get_u24(&self.data, self.format.parent_at())
    }

    /// Repoint the directory's parent and reseal.
    pub fn set_parent(&mut self, parent: u32) {
        put_u24(
            &mut self.data,
            self.format.parent_at(),
            parent & 0x00FF_FFFF,
        );
        self.seal();
    }

    /// Insert `object`, keeping entries sorted, and reseal the
    /// directory.
    ///
    /// # Errors
    ///
    /// * [`DriverError::AlreadyExists`] if a same-named entry exists.
    /// * [`DriverError::NoSpace`] if the directory is full.
    pub fn insert(&mut self, object: &Object) -> Result<(), DriverError> {
        if self.find(object.name()).is_some() {
            return Err(DriverError::AlreadyExists);
        }
        let count = self.count();
        if count == self.format.capacity() {
            return Err(DriverError::NoSpace);
        }
        let mut index = 0;
        while index < count {
            let existing = self.entry(index).ok_or(DriverError::BadMagic)?;
            if name_cmp(object.name(), existing.name()) == core::cmp::Ordering::Less {
                break;
            }
            index += 1;
        }
        // Shift the tail up one slot.
        let from = ENTRIES_OFFSET + index * ENTRY_SIZE;
        let to = ENTRIES_OFFSET + count * ENTRY_SIZE;
        self.data.copy_within(from..to, from + ENTRY_SIZE);
        self.set_entry(index, object);
        self.seal();
        Ok(())
    }

    /// Remove the entry at `index` and reseal the directory.
    pub fn remove(&mut self, index: usize) {
        let count = self.count();
        if index >= count {
            return;
        }
        let from = ENTRIES_OFFSET + (index + 1) * ENTRY_SIZE;
        let to = ENTRIES_OFFSET + count * ENTRY_SIZE;
        self.data.copy_within(from..to, from - ENTRY_SIZE);
        let last = ENTRIES_OFFSET + (count - 1) * ENTRY_SIZE;
        self.data[last..last + ENTRY_SIZE].fill(0);
        self.seal();
    }

    /// Replace the entry at `index` with `object` (same name slot) and
    /// reseal the directory.
    pub fn update(&mut self, index: usize, object: &Object) {
        self.set_entry(index, object);
        self.seal();
    }

    /// Bump the sequence number and refresh the check byte after a
    /// mutation.
    fn seal(&mut self) {
        let size = self.format.size();
        let seq = bcd_increment(self.data[0]);
        self.data[0] = seq;
        self.data[size - 6] = seq;
        self.data[size - 1] = self.check_byte();
    }

    /// Compute the directory check byte (the `ror13` accumulation).
    fn check_byte(&self) -> u8 {
        let size = self.format.size();
        let used = ENTRIES_OFFSET + self.count() * ENTRY_SIZE;
        let mut check = accumulate_words(&self.data[..used], 0);
        // Skip the end-of-entries marker, then take the footer words up
        // to (not including) the final word holding the check byte.
        let tail = self.format.tail() + 1;
        check = accumulate_words(&self.data[tail..size - 4], check);
        fold_check(check)
    }

    /// Build an empty directory named `name` with parent `parent`.
    ///
    /// `marker` selects `Hugo`/`Nick` for new-format directories; old
    /// directories are always `Hugo`.
    pub fn initialise(format: FixedFormat, marker: [u8; 4], name: &[u8], parent: u32) -> Self {
        let mut dir = Self {
            data: [0; NEW_DIR_SIZE],
            format,
        };
        let size = format.size();
        let marker = match format {
            FixedFormat::Old => *b"Hugo",
            FixedFormat::New => marker,
        };
        dir.data[1..5].copy_from_slice(&marker);
        dir.data[size - 5..size - 1].copy_from_slice(&marker);
        let name_at = format.name_at();
        let title_at = format.title_at();
        let name_len = name.len().min(FIXED_NAME_LEN);
        dir.data[name_at..name_at + name_len].copy_from_slice(&name[..name_len]);
        if name_len < FIXED_NAME_LEN {
            dir.data[name_at + name_len] = 0x0D;
        }
        let title_len = name.len().min(19);
        dir.data[title_at..title_at + title_len].copy_from_slice(&name[..title_len]);
        if title_len < 19 {
            dir.data[title_at + title_len] = 0x0D;
        }
        put_u24(&mut dir.data, format.parent_at(), parent & 0x00FF_FFFF);
        dir.data[size - 1] = dir.check_byte();
        dir
    }
}

/// Rotate `value` right by 13 bits — the directory check accumulator.
fn ror13(value: u32) -> u32 {
    value.rotate_right(13)
}

/// Accumulate a byte region: little-endian whole words first, then any
/// trailing bytes individually.
pub fn accumulate_words(region: &[u8], mut check: u32) -> u32 {
    let (chunks, remainder) = region.as_chunks::<4>();
    for word in chunks {
        check = get_u32(word, 0) ^ ror13(check);
    }
    for &byte in remainder {
        check = u32::from(byte) ^ ror13(check);
    }
    check
}

/// Accumulate individual bytes (used where a region must not be
/// word-grouped).
pub fn accumulate_bytes(region: &[u8], mut check: u32) -> u32 {
    for &byte in region {
        check = u32::from(byte) ^ ror13(check);
    }
    check
}

/// Fold the accumulated word into the final check byte.
pub fn fold_check(check: u32) -> u8 {
    ((check ^ (check >> 8) ^ (check >> 16) ^ (check >> 24)) & 0xFF) as u8
}

/// Increment a binary-coded-decimal sequence number, wrapping at 100.
pub fn bcd_increment(value: u8) -> u8 {
    let tens = (value >> 4) & 0xF;
    let ones = value & 0xF;
    let mut next = u32::from(tens) * 10 + u32::from(ones) + 1;
    if next >= 100 {
        next = 0;
    }
    // Both digits are below ten, so the byte can never truncate.
    u8::try_from((next / 10) << 4 | (next % 10)).unwrap_or(0)
}
