//! Big directories (ADFS E+/F+): the variable-length `SBPr`/`oven`
//! format with a name heap.
//!
//! Layout: a 28-byte header (sequence byte, version `0,0,0`, `SBPr`,
//! name length, directory size, entry count, name-heap size, parent,
//! then the directory name padded to a word), an array of 28-byte
//! entries, the name heap (each name `CR`-terminated and word-padded),
//! and an 8-byte tail (`oven`, sequence byte, two zero bytes, check
//! byte) at the very end of the directory. Reference: G. Holdsworth,
//! "Guide To Disc Formats" and the Linux `fs/adfs/dir_fplus` reference.
//!
//! The engine streams through a [`DirStore`] — byte-addressed access to
//! the directory object — so a directory of any size is handled without
//! a size-proportional buffer.

use crate::dir::{
    accumulate_bytes, accumulate_words, bcd_increment, fold_check, name_cmp, name_eq, Object,
    MAX_NAME_LEN,
};
use crate::volume::{get_u32, put_u32};
use rustos_abi::DriverError;

/// The `SBPr` header marker.
pub const BIG_START: [u8; 4] = *b"SBPr";
/// The `oven` tail marker.
pub const BIG_END: [u8; 4] = *b"oven";
/// Byte size of the fixed part of the header.
pub const BIG_HEADER_SIZE: u32 = 28;
/// Byte size of one big-directory entry.
pub const BIG_ENTRY_SIZE: u32 = 28;
/// Byte size of the tail.
pub const BIG_TAIL_SIZE: u32 = 8;
/// Big directories are sized in whole 2048-byte units.
pub const BIG_DIR_GRAIN: u32 = 2048;

/// Byte-addressed access to one directory object's bytes.
///
/// Implemented by the driver over the allocation map, so directory
/// logic never sees fragmentation.
pub trait DirStore {
    /// Read `buf.len()` bytes at `offset` within the directory object.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the range leaves the object.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), DriverError>;

    /// Write `data` at `offset` within the directory object.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the range leaves the object.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    fn write_at(&mut self, offset: u32, data: &[u8]) -> Result<(), DriverError>;
}

/// A parsed big-directory header.
#[derive(Copy, Clone)]
pub struct BigHeader {
    /// Sequence byte.
    pub masseq: u8,
    /// Directory-name length in bytes (excluding the `CR` terminator).
    pub name_len: u32,
    /// Directory size in bytes (a multiple of [`BIG_DIR_GRAIN`]).
    pub size: u32,
    /// Number of entries.
    pub entries: u32,
    /// Name-heap size in bytes.
    pub names_size: u32,
    /// Parent indirect disc address.
    pub parent: u32,
}

impl BigHeader {
    /// Offset of the entry array (header + word-padded name + `CR`).
    pub fn entries_offset(&self) -> u32 {
        BIG_HEADER_SIZE + pad_name(self.name_len)
    }

    /// Offset of the name heap.
    pub fn heap_offset(&self) -> u32 {
        self.entries_offset() + self.entries * BIG_ENTRY_SIZE
    }

    /// Offset one past the used extent (header, name, entries, heap).
    pub fn used_end(&self) -> u32 {
        self.heap_offset() + self.names_size
    }

    /// Offset of the tail.
    pub fn tail_offset(&self) -> u32 {
        self.size - BIG_TAIL_SIZE
    }

    fn encode(&self) -> [u8; BIG_HEADER_SIZE as usize] {
        let mut raw = [0u8; BIG_HEADER_SIZE as usize];
        raw[0] = self.masseq;
        raw[4..8].copy_from_slice(&BIG_START);
        put_u32(&mut raw, 8, self.name_len);
        put_u32(&mut raw, 12, self.size);
        put_u32(&mut raw, 16, self.entries);
        put_u32(&mut raw, 20, self.names_size);
        put_u32(&mut raw, 24, self.parent);
        raw
    }
}

/// Heap bytes a name of `len` occupies (`CR` terminator, word padded).
fn pad_name(len: u32) -> u32 {
    (len + 1 + 3) & !3
}

/// The big-directory engine: a validated header plus streaming
/// operations over a [`DirStore`].
pub struct BigDir {
    /// The validated header.
    pub header: BigHeader,
}

impl BigDir {
    /// Read and validate the directory rooted in `store`.
    ///
    /// `object_limit` is the directory object's allocation; the header
    /// size must fit inside it (the allocation may round up past the
    /// directory, and a held node id may carry a stale pre-growth size,
    /// so equality is deliberately not required).
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] on any structural violation (markers,
    /// version, sizes, sequence mismatch, or check byte).
    pub fn load<S: DirStore>(store: &mut S, object_limit: u32) -> Result<Self, DriverError> {
        let mut raw = [0u8; BIG_HEADER_SIZE as usize];
        store.read_at(0, &mut raw)?;
        if raw[1] != 0 || raw[2] != 0 || raw[3] != 0 || raw[4..8] != BIG_START {
            return Err(DriverError::BadMagic);
        }
        let header = BigHeader {
            masseq: raw[0],
            name_len: get_u32(&raw, 8),
            size: get_u32(&raw, 12),
            entries: get_u32(&raw, 16),
            names_size: get_u32(&raw, 20),
            parent: get_u32(&raw, 24),
        };
        if header.size > object_limit
            || header.size < BIG_DIR_GRAIN
            || header.size % BIG_DIR_GRAIN != 0
        {
            return Err(DriverError::BadMagic);
        }
        if header.name_len as usize > MAX_NAME_LEN
            || header.entries > header.size / BIG_ENTRY_SIZE
            || header.names_size > header.size
            || header.used_end() > header.tail_offset()
        {
            return Err(DriverError::BadMagic);
        }
        let mut tail = [0u8; BIG_TAIL_SIZE as usize];
        store.read_at(header.tail_offset(), &mut tail)?;
        if tail[..4] != BIG_END || tail[4] != header.masseq || tail[5] != 0 || tail[6] != 0 {
            return Err(DriverError::BadMagic);
        }
        let dir = Self { header };
        // A zero check byte is accepted as "unchecked" (some imaging
        // tools write it so); anything else must match.
        let stored = tail[7];
        if stored != 0 && stored != dir.check_byte(store)? {
            return Err(DriverError::BadMagic);
        }
        Ok(dir)
    }

    /// Compute the directory check byte: words then bytes over the used
    /// extent, then the tail's marker word and the three bytes before
    /// the check byte itself.
    fn check_byte<S: DirStore>(&self, store: &mut S) -> Result<u8, DriverError> {
        let mut check = 0u32;
        let mut offset = 0u32;
        let used = self.header.used_end();
        let mut buf = [0u8; 512];
        while offset < used {
            let take = (used - offset).min(512);
            store.read_at(offset, &mut buf[..take as usize])?;
            check = accumulate_words(&buf[..take as usize], check);
            offset += take;
        }
        let mut tail = [0u8; BIG_TAIL_SIZE as usize];
        store.read_at(self.header.tail_offset(), &mut tail)?;
        check = accumulate_words(&tail[..4], check);
        check = accumulate_bytes(&tail[4..7], check);
        Ok(fold_check(check))
    }

    /// Decode the entry at `index`.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] if the entry's name pointer or length
    /// leaves the heap.
    pub fn entry<S: DirStore>(
        &self,
        store: &mut S,
        index: u32,
    ) -> Result<Option<Object>, DriverError> {
        if index >= self.header.entries {
            return Ok(None);
        }
        let at = self.header.entries_offset() + index * BIG_ENTRY_SIZE;
        let mut raw = [0u8; BIG_ENTRY_SIZE as usize];
        store.read_at(at, &mut raw)?;
        let name_len = get_u32(&raw, 20);
        let name_ptr = get_u32(&raw, 24);
        if name_len == 0
            || name_len as usize > MAX_NAME_LEN
            || name_ptr
                .checked_add(name_len)
                .map_or(true, |end| end > self.header.names_size)
        {
            return Err(DriverError::BadMagic);
        }
        let mut object = Object {
            name: [0; MAX_NAME_LEN],
            name_len: name_len as usize,
            load: get_u32(&raw, 0),
            exec: get_u32(&raw, 4),
            size: get_u32(&raw, 8),
            indaddr: get_u32(&raw, 12),
            attr: (get_u32(&raw, 16) & 0xFF) as u16,
        };
        store.read_at(
            self.header.heap_offset() + name_ptr,
            &mut object.name[..name_len as usize],
        )?;
        Ok(Some(object))
    }

    /// Find the entry named `name`, returning its index and object.
    ///
    /// # Errors
    ///
    /// Propagates decode errors from [`Self::entry`].
    pub fn find<S: DirStore>(
        &self,
        store: &mut S,
        name: &[u8],
    ) -> Result<Option<(u32, Object)>, DriverError> {
        for index in 0..self.header.entries {
            let Some(object) = self.entry(store, index)? else {
                break;
            };
            if name_eq(object.name(), name) {
                return Ok(Some((index, object)));
            }
        }
        Ok(None)
    }

    /// Insert `object` in sorted position.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Busy`] if a same-named entry exists.
    /// * [`DriverError::NoSpace`] if the directory's current size
    ///   cannot hold the new entry and name (the caller grows the
    ///   directory object and retries).
    pub fn insert<S: DirStore>(
        &mut self,
        store: &mut S,
        object: &Object,
    ) -> Result<(), DriverError> {
        if self.find(store, object.name())?.is_some() {
            return Err(DriverError::Busy);
        }
        // The name was validated against `MAX_NAME_LEN`, so it fits.
        let name_len = u32::try_from(object.name_len).unwrap_or(u32::MAX);
        let grown = BIG_ENTRY_SIZE + pad_name(name_len);
        if self.header.used_end() + grown > self.header.tail_offset() {
            return Err(DriverError::NoSpace);
        }
        // Sorted position.
        let mut index = 0u32;
        while index < self.header.entries {
            let Some(existing) = self.entry(store, index)? else {
                break;
            };
            if name_cmp(object.name(), existing.name()) == core::cmp::Ordering::Less {
                break;
            }
            index += 1;
        }
        // Open a 28-byte gap at the insertion point (moves the later
        // entries and the whole heap up).
        let gap_at = self.header.entries_offset() + index * BIG_ENTRY_SIZE;
        let used = self.header.used_end();
        move_region(store, gap_at, gap_at + BIG_ENTRY_SIZE, used - gap_at)?;
        // Append the name to the (shifted) heap.
        let heap_offset = self.header.heap_offset() + BIG_ENTRY_SIZE;
        let name_ptr = self.header.names_size;
        let mut padded = [0u8; MAX_NAME_LEN + 4];
        let padded_len = pad_name(name_len) as usize;
        padded[..object.name_len].copy_from_slice(object.name());
        padded[object.name_len] = 0x0D;
        store.write_at(heap_offset + name_ptr, &padded[..padded_len])?;
        // Write the new entry.
        let mut raw = [0u8; BIG_ENTRY_SIZE as usize];
        put_u32(&mut raw, 0, object.load);
        put_u32(&mut raw, 4, object.exec);
        put_u32(&mut raw, 8, object.size);
        put_u32(&mut raw, 12, object.indaddr);
        put_u32(&mut raw, 16, u32::from(object.attr) & 0xFF);
        put_u32(&mut raw, 20, name_len);
        put_u32(&mut raw, 24, name_ptr);
        store.write_at(gap_at, &raw)?;
        self.header.entries += 1;
        self.header.names_size += pad_name(name_len);
        self.seal(store)
    }

    /// Remove the entry at `index`, compacting the name heap.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] if `index` is not a live entry.
    pub fn remove<S: DirStore>(&mut self, store: &mut S, index: u32) -> Result<(), DriverError> {
        let Some(object) = self.entry(store, index)? else {
            return Err(DriverError::BadMagic);
        };
        let mut raw = [0u8; BIG_ENTRY_SIZE as usize];
        let at = self.header.entries_offset() + index * BIG_ENTRY_SIZE;
        store.read_at(at, &mut raw)?;
        let name_ptr = get_u32(&raw, 24);
        let padded = pad_name(u32::try_from(object.name_len).unwrap_or(u32::MAX));
        // Close the entry gap (moves later entries and the heap down).
        let used = self.header.used_end();
        let after = at + BIG_ENTRY_SIZE;
        move_region(store, after, at, used - after)?;
        // Compact the heap over the removed name.
        let heap_offset = self.header.heap_offset() - BIG_ENTRY_SIZE;
        let name_at = heap_offset + name_ptr;
        let heap_end = heap_offset + self.header.names_size;
        move_region(
            store,
            name_at + padded,
            name_at,
            heap_end - (name_at + padded),
        )?;
        self.header.entries -= 1;
        self.header.names_size -= padded;
        // Rebase every name pointer past the removed name.
        for i in 0..self.header.entries {
            let entry_at = self.header.entries_offset() + i * BIG_ENTRY_SIZE;
            let mut ptr_raw = [0u8; 4];
            store.read_at(entry_at + 24, &mut ptr_raw)?;
            let ptr = get_u32(&ptr_raw, 0);
            if ptr > name_ptr {
                put_u32(&mut ptr_raw, 0, ptr - padded);
                store.write_at(entry_at + 24, &ptr_raw)?;
            }
        }
        self.seal(store)
    }

    /// Rewrite the metadata (load/exec/size/indaddr/attributes) of the
    /// entry at `index`, keeping its name.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] if `index` is not a live entry.
    pub fn update<S: DirStore>(
        &mut self,
        store: &mut S,
        index: u32,
        object: &Object,
    ) -> Result<(), DriverError> {
        if index >= self.header.entries {
            return Err(DriverError::BadMagic);
        }
        let at = self.header.entries_offset() + index * BIG_ENTRY_SIZE;
        let mut raw = [0u8; 20];
        put_u32(&mut raw, 0, object.load);
        put_u32(&mut raw, 4, object.exec);
        put_u32(&mut raw, 8, object.size);
        put_u32(&mut raw, 12, object.indaddr);
        put_u32(&mut raw, 16, u32::from(object.attr) & 0xFF);
        store.write_at(at, &raw)?;
        self.seal(store)
    }

    /// Repoint the directory's parent.
    ///
    /// # Errors
    ///
    /// Propagates store I/O errors.
    pub fn set_parent<S: DirStore>(
        &mut self,
        store: &mut S,
        parent: u32,
    ) -> Result<(), DriverError> {
        self.header.parent = parent;
        self.seal(store)
    }

    /// Grow the directory to `new_size` (the object has already been
    /// extended that far): the tail moves to the new end and the space
    /// between the used extent and the tail is zeroed.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if `new_size` is not a
    ///   larger multiple of [`BIG_DIR_GRAIN`].
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn grow<S: DirStore>(&mut self, store: &mut S, new_size: u32) -> Result<(), DriverError> {
        if new_size <= self.header.size || new_size % BIG_DIR_GRAIN != 0 {
            return Err(DriverError::LengthOutOfRange);
        }
        let old_tail = self.header.tail_offset();
        self.header.size = new_size;
        // Zero from the old tail up to the new tail position.
        let zeroes = [0u8; 512];
        let mut offset = old_tail;
        let new_tail = self.header.tail_offset();
        while offset < new_tail {
            let take = (new_tail - offset).min(512);
            store.write_at(offset, &zeroes[..take as usize])?;
            offset += take;
        }
        self.seal(store)
    }

    /// Bump the sequence number, rewrite header and tail, and refresh
    /// the check byte.
    fn seal<S: DirStore>(&mut self, store: &mut S) -> Result<(), DriverError> {
        self.header.masseq = bcd_increment(self.header.masseq);
        store.write_at(0, &self.header.encode())?;
        let mut tail = [0u8; BIG_TAIL_SIZE as usize];
        tail[..4].copy_from_slice(&BIG_END);
        tail[4] = self.header.masseq;
        store.write_at(self.header.tail_offset(), &tail)?;
        let check = self.check_byte(store)?;
        store.write_at(self.header.tail_offset() + 7, &[check])
    }

    /// Write a fresh, empty big directory of `size` bytes named `name`
    /// into `store`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if the name exceeds
    ///   [`MAX_NAME_LEN`] or `size` cannot hold it.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn initialise<S: DirStore>(
        store: &mut S,
        size: u32,
        name: &[u8],
        parent: u32,
    ) -> Result<(), DriverError> {
        if name.is_empty() || name.len() > MAX_NAME_LEN {
            return Err(DriverError::LengthOutOfRange);
        }
        let header = BigHeader {
            masseq: 0,
            name_len: u32::try_from(name.len()).unwrap_or(u32::MAX),
            size,
            entries: 0,
            names_size: 0,
            parent,
        };
        if size < BIG_DIR_GRAIN
            || size % BIG_DIR_GRAIN != 0
            || header.used_end() > header.tail_offset()
        {
            return Err(DriverError::LengthOutOfRange);
        }
        // Zero the whole directory, then lay down header, name, tail.
        let zeroes = [0u8; 512];
        let mut offset = 0u32;
        while offset < size {
            let take = (size - offset).min(512);
            store.write_at(offset, &zeroes[..take as usize])?;
            offset += take;
        }
        store.write_at(0, &header.encode())?;
        let mut padded = [0u8; MAX_NAME_LEN + 4];
        let padded_len = pad_name(header.name_len) as usize;
        padded[..name.len()].copy_from_slice(name);
        padded[name.len()] = 0x0D;
        store.write_at(BIG_HEADER_SIZE, &padded[..padded_len])?;
        let mut tail = [0u8; BIG_TAIL_SIZE as usize];
        tail[..4].copy_from_slice(&BIG_END);
        tail[4] = header.masseq;
        store.write_at(header.tail_offset(), &tail)?;
        let dir = Self { header };
        let check = dir.check_byte(store)?;
        store.write_at(header.tail_offset() + 7, &[check])
    }
}

/// Move `len` bytes from `src` to `dst` within the store, handling
/// overlapping ranges in either direction.
fn move_region<S: DirStore>(
    store: &mut S,
    src: u32,
    dst: u32,
    len: u32,
) -> Result<(), DriverError> {
    if len == 0 || src == dst {
        return Ok(());
    }
    let mut buf = [0u8; 512];
    if dst < src {
        let mut done = 0u32;
        while done < len {
            let take = (len - done).min(512);
            store.read_at(src + done, &mut buf[..take as usize])?;
            store.write_at(dst + done, &buf[..take as usize])?;
            done += take;
        }
    } else {
        let mut remaining = len;
        while remaining > 0 {
            let take = remaining.min(512);
            let at = remaining - take;
            store.read_at(src + at, &mut buf[..take as usize])?;
            store.write_at(dst + at, &buf[..take as usize])?;
            remaining -= take;
        }
    }
    Ok(())
}
