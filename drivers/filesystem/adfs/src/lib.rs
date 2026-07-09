//! RustOS Acorn ADFS filesystem driver (read/write).
//!
//! Attaches an Acorn ADFS / RISC OS `FileCore` volume sitting behind any
//! [`rustos_abi::driver::block::Block`] device and exposes it through the
//! versioned [`FilesystemRead`], [`FilesystemWrite`],
//! [`FilesystemTimestamps`], [`FilesystemAttrs`], and [`FilesystemStats`]
//! surfaces (new behaviour ships as a new trait, never by widening the
//! frozen mount/unmount
//! [`Filesystem`](rustos_abi::driver::filesystem::Filesystem)).
//!
//! # Supported formats
//!
//! Every ADFS on-disk format is supported, for reading and writing:
//!
//! * **Old map** volumes — the S (160 KiB), M (320 KiB), and L (640 KiB)
//!   floppies with 1280-byte `Hugo` directories, the D (800 KiB) floppy
//!   with 2048-byte directories, and old-map hard discs. The free-space
//!   map lives in sectors 0–1 and every object is one contiguous run of
//!   256-byte sectors.
//! * **New map** volumes — the E (800 KiB) and F (1600 KiB) floppies and
//!   new-map hard discs, where a multi-zone allocation map of
//!   variable-length fragments backs fragmented objects, and small
//!   objects may share a fragment through the low byte of their indirect
//!   disc address.
//! * **Big directories** — the E+/F+ variable-length directory format
//!   (`SBPr`/`oven` markers with a name heap), alongside the fixed
//!   2048-byte `Hugo`/`Nick` new directories and the 1280-byte old
//!   directories.
//!
//! ADFS has no per-inode owner, mode, ACL, or capability gate; those live
//! in the VFS metadata layer that mounts this driver. The driver therefore
//! makes **no** permission decisions (the VFS is the policy point, this is
//! raw structural I/O).
//!
//! # RISC OS metadata
//!
//! Load/exec addresses, the 12-bit filetype, the 40-bit centisecond
//! datestamp, and the `FileCore` attribute bits are surfaced through the
//! shared `rustos_fsmeta` Acorn preset as the canonical `acorn.*`
//! attribute keys, so a copy to `RustFS` and back is byte-exact.
//!
//! # Public surface
//!
//! The only public *function* is [`register`]. [`Adfs`] is a public *type*
//! the driver host instantiates with [`Adfs::open`] (or formats with
//! [`Adfs::format`]); the host reaches into it only through the
//! filesystem traits.
//!
//! # Capabilities
//!
//! Loading requires
//! [`CapabilityId::DRV_LOAD`](rustos_abi::CapabilityId::DRV_LOAD). The
//! driver runs in user space; it does not request `CAP_DRV_KERNEL`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemAttrs, FilesystemRead, FilesystemStats, FilesystemTimestamps,
    FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeTimes, VolumeStats,
};
use rustos_abi::time::Time64;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};
use rustos_fsmeta::preset::acorn;

mod bigdir;
mod dir;
mod disc;
mod mkfs;
mod newmap;
mod oldmap;
mod volume;

#[cfg(test)]
mod tests;

use bigdir::{BigDir, DirStore};
use dir::{FixedDir, FixedFormat, Object};
use disc::{
    boot_block_checksum, DiscRecord, BOOT_BLOCK_OFFSET, BOOT_BLOCK_SIZE, DISC_RECORD_IN_BOOT_BLOCK,
    DISC_RECORD_SIZE,
};
use newmap::NewMap;
use oldmap::{OldMap, OLD_SECTOR_SIZE, OLD_SECTOR_SIZE_U32};
use volume::Volume;

pub use mkfs::AdfsVariant;

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x4144_4653_0000_0001; // "ADFS" + index

/// Driver entry point.
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// Mask isolating the indirect disc address in a packed `NodeId` (a new
/// map indirect address is at most a 19-bit fragment id plus the 8-bit
/// share offset; an old-map start sector is 24 bits).
const NODE_ADDR_MASK: u64 = (1 << 28) - 1;
/// `NodeId` bit marking a directory.
const NODE_DIR_FLAG: u64 = 1 << 28;
/// `NodeId` validity bit, set on every live node so that no live node
/// ever equals `NodeId::NONE` (`0`).
const NODE_VALID_FLAG: u64 = 1 << 29;
/// Bit position at which the object's byte size is packed.
const NODE_SIZE_SHIFT: u64 = 32;

/// Pack an object's identity into a self-describing `NodeId`.
fn pack_node(indaddr: u32, is_dir: bool, size: u32) -> NodeId {
    let mut raw = u64::from(indaddr) & NODE_ADDR_MASK | NODE_VALID_FLAG;
    if is_dir {
        raw |= NODE_DIR_FLAG;
    }
    raw |= u64::from(size) << NODE_SIZE_SHIFT;
    NodeId::from_raw(raw)
}

/// Pack a directory `Object`'s identity (a file's node carries its
/// byte size; a directory's carries its on-disc directory size).
fn object_node(object: &Object) -> NodeId {
    pack_node(object.indaddr, object.is_dir(), object.size)
}

/// Indirect disc address (or old-map start sector) in a packed node.
fn node_addr(node: NodeId) -> u32 {
    // The masked value spans at most 28 bits, so it always fits `u32`.
    u32::try_from(node.raw() & NODE_ADDR_MASK).unwrap_or(0)
}

/// Whether a packed node denotes a directory.
fn node_is_dir(node: NodeId) -> bool {
    node.raw() & NODE_DIR_FLAG != 0
}

/// Whether the node was packed by this driver (fail-closed guard).
fn node_is_valid(node: NodeId) -> bool {
    node.raw() & NODE_VALID_FLAG != 0
}

/// Object byte size carried by a packed node (a directory's is its
/// on-disc directory size).
fn node_size(node: NodeId) -> u32 {
    // The high 32 bits of a `u64` always fit in `u32`.
    u32::try_from(node.raw() >> NODE_SIZE_SHIFT).unwrap_or(0)
}

/// A fixed directory's size, in the `u32` form node packing uses.
fn fixed_size_u32(format: FixedFormat) -> u32 {
    match format {
        FixedFormat::Old => 1280,
        FixedFormat::New => dir::NEW_DIR_SIZE_U32,
    }
}

/// The single stamp a typed object stores, widened to [`Time64`]; an
/// untyped object (raw load/exec addresses) stores no stamp and
/// honestly reports the epoch.
fn object_stamp(object: &Object) -> Time64 {
    match acorn::decode_load_exec(object.load, object.exec) {
        acorn::LoadExec::Typed { centiseconds, .. } => {
            acorn::centiseconds_to_time64(centiseconds).unwrap_or(Time64::UNIX_EPOCH)
        }
        acorn::LoadExec::Untyped { .. } => Time64::UNIX_EPOCH,
    }
}

/// The volume's map flavour and root geometry.
enum Backing {
    /// Old-map volume: contiguous objects addressed by start sector.
    Old {
        /// The fixed directory format the volume uses.
        format: FixedFormat,
        /// Start sector of the root directory.
        root_sector: u32,
    },
    /// New-map volume: fragmented objects addressed indirectly.
    New {
        /// The allocation-map engine (owns the validated disc record).
        map: NewMap,
        /// Whether the volume carries a boot block whose disc-record
        /// copy must be kept in step.
        boot_block: bool,
    },
}

/// An ADFS volume attached to a block device.
pub struct Adfs<B: Block> {
    volume: Volume<B>,
    backing: Backing,
}

impl<B: Block> Adfs<B> {
    /// Attach to an existing ADFS volume on `device`, identifying its
    /// format and validating every checksummed structure on the way.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if the device holds no structurally
    ///   valid ADFS volume of any variant.
    /// * [`DriverError::Unsupported`] if the device geometry cannot be
    ///   staged.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn open(device: B) -> Result<Self, DriverError> {
        let mut volume = Volume::new(device)?;
        // A boot block at 0xC00 (F-class floppies, hard discs) wins;
        // then a bare disc record at byte 4 (E-class); then the old map.
        let backing = match Self::probe_boot_block(&mut volume)? {
            Some(record) => Backing::New {
                map: NewMap::open(&mut volume, record)?,
                boot_block: true,
            },
            None => match Self::probe_bare_record(&mut volume)? {
                Some(record) => Backing::New {
                    map: NewMap::open(&mut volume, record)?,
                    boot_block: false,
                },
                None => Self::probe_old_map(&mut volume)?,
            },
        };
        let mut adfs = Self { volume, backing };
        // The root directory must itself validate.
        let root = adfs.root_node();
        adfs.load_dir(node_addr(root), node_size(root))?;
        Ok(adfs)
    }

    /// Format `device` as an empty ADFS volume of `variant` and attach
    /// to it.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if the device is too small for the
    ///   variant.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block access.
    pub fn format(device: B, variant: AdfsVariant) -> Result<Self, DriverError> {
        let mut volume = Volume::new(device)?;
        mkfs::format_volume(&mut volume, variant)?;
        let device = volume.into_device();
        Self::open(device)
    }

    /// Detach from the volume, returning the underlying device.
    pub fn into_device(self) -> B {
        self.volume.into_device()
    }

    /// Probe the checksummed boot block, returning its disc record.
    fn probe_boot_block(volume: &mut Volume<B>) -> Result<Option<DiscRecord>, DriverError> {
        if BOOT_BLOCK_OFFSET + BOOT_BLOCK_SIZE as u64 > volume.device_bytes() {
            return Ok(None);
        }
        let mut block = [0u8; BOOT_BLOCK_SIZE];
        volume.read_bytes(BOOT_BLOCK_OFFSET, &mut block)?;
        if boot_block_checksum(&block) != block[BOOT_BLOCK_SIZE - 1] {
            return Ok(None);
        }
        let mut raw = [0u8; DISC_RECORD_SIZE];
        raw.copy_from_slice(
            &block[DISC_RECORD_IN_BOOT_BLOCK..DISC_RECORD_IN_BOOT_BLOCK + DISC_RECORD_SIZE],
        );
        let record = DiscRecord::parse(&raw);
        // An all-zero block would checksum; a real record cannot be
        // all-zero (the disc size is non-zero) and its reserved tail
        // must be zero.
        if record.validate().is_err() || raw[0x34..0x3C].iter().any(|&b| b != 0) {
            return Ok(None);
        }
        Ok(Some(record))
    }

    /// Probe the bare zone-0 disc record of an E-class volume.
    fn probe_bare_record(volume: &mut Volume<B>) -> Result<Option<DiscRecord>, DriverError> {
        if (DISC_RECORD_SIZE as u64) + 4 > volume.device_bytes() {
            return Ok(None);
        }
        let mut raw = [0u8; DISC_RECORD_SIZE];
        volume.read_bytes(4, &mut raw)?;
        let record = DiscRecord::parse(&raw);
        // A bare record identifies a single-zone volume only; the zone
        // check performed by `NewMap::open` is the integrity gate.
        if record.validate().is_err()
            || record.nzones != 1
            || raw[0x34..0x3C].iter().any(|&b| b != 0)
        {
            return Ok(None);
        }
        Ok(Some(record))
    }

    /// Probe the old free-space map and root directory marker.
    fn probe_old_map(volume: &mut Volume<B>) -> Result<Backing, DriverError> {
        // Loading validates both map checksums and every free area.
        OldMap::load(volume)?;
        // Five zero bytes at sector 2 identify a large-sector (D)
        // volume whose root sits at sector 4; otherwise the root is the
        // 1280-byte directory at sector 2.
        let mut lead = [0u8; 5];
        volume.read_bytes(2 * OLD_SECTOR_SIZE, &mut lead)?;
        let (format, root_sector) = if lead == [0u8; 5] {
            (FixedFormat::New, 4)
        } else {
            (FixedFormat::Old, 2)
        };
        Ok(Backing::Old {
            format,
            root_sector,
        })
    }

    /// The packed root directory node.
    fn root_node(&self) -> NodeId {
        match &self.backing {
            Backing::Old {
                format,
                root_sector,
            } => pack_node(*root_sector, true, fixed_size_u32(*format)),
            Backing::New { map, .. } => {
                let size = if map.record.format_version != 0 {
                    map.record.root_size
                } else {
                    dir::NEW_DIR_SIZE_U32
                };
                pack_node(map.record.root, true, size)
            }
        }
    }

    /// Read exactly `buf.len()` bytes at `offset` within the object at
    /// `indaddr` (which must be allocated that far).
    fn object_read(
        &mut self,
        indaddr: u32,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), DriverError> {
        match &self.backing {
            Backing::Old { .. } => {
                let at = u64::from(indaddr) * OLD_SECTOR_SIZE + offset;
                self.volume.read_bytes(at, buf)
            }
            Backing::New { map, .. } => {
                let mut done = 0usize;
                while done < buf.len() {
                    let (at, run) = map.locate(&mut self.volume, indaddr, offset + done as u64)?;
                    let take = usize::try_from(run.min((buf.len() - done) as u64))
                        .map_err(|_| DriverError::LengthOutOfRange)?;
                    if take == 0 {
                        return Err(DriverError::BadMagic);
                    }
                    self.volume.read_bytes(at, &mut buf[done..done + take])?;
                    done += take;
                }
                Ok(())
            }
        }
    }

    /// Write `data` at `offset` within the object at `indaddr` (which
    /// must be allocated that far).
    fn object_write(&mut self, indaddr: u32, offset: u64, data: &[u8]) -> Result<(), DriverError> {
        match &self.backing {
            Backing::Old { .. } => {
                let at = u64::from(indaddr) * OLD_SECTOR_SIZE + offset;
                self.volume.write_bytes(at, data)
            }
            Backing::New { map, .. } => {
                let mut done = 0usize;
                while done < data.len() {
                    let (at, run) = map.locate(&mut self.volume, indaddr, offset + done as u64)?;
                    let take = usize::try_from(run.min((data.len() - done) as u64))
                        .map_err(|_| DriverError::LengthOutOfRange)?;
                    if take == 0 {
                        return Err(DriverError::BadMagic);
                    }
                    self.volume.write_bytes(at, &data[done..done + take])?;
                    done += take;
                }
                Ok(())
            }
        }
    }
}

impl<B: Block> Adfs<B> {
    /// Whether the volume uses big (E+/F+) directories.
    fn is_big_dir(&self) -> bool {
        matches!(&self.backing, Backing::New { map, .. } if map.record.format_version != 0)
    }

    /// The fixed directory format of a non-big-directory volume.
    fn fixed_format(&self) -> FixedFormat {
        match &self.backing {
            Backing::Old { format, .. } => *format,
            Backing::New { .. } => FixedFormat::New,
        }
    }

    /// Load and validate the directory object at `indaddr`.
    ///
    /// The node-carried size is deliberately not consulted: a fixed
    /// directory's size is dictated by the volume's format, a big
    /// directory's by its own validated header, and the allocation map
    /// bounds every access, so a stale or lying size still fails closed.
    fn load_dir(&mut self, indaddr: u32, _size_hint: u32) -> Result<DirHandle, DriverError> {
        if self.is_big_dir() {
            let mut store = ObjectStore {
                adfs: self,
                indaddr,
                size: u32::MAX,
            };
            Ok(DirHandle::Big(BigDir::load(&mut store, u32::MAX)?))
        } else {
            let format = self.fixed_format();
            let mut data = [0u8; dir::NEW_DIR_SIZE];
            self.object_read(indaddr, 0, &mut data[..format.size()])?;
            Ok(DirHandle::Fixed(FixedDir::parse(&data, format)?))
        }
    }

    /// Write a mutated fixed directory back to `indaddr`.
    fn store_fixed_dir(&mut self, indaddr: u32, dir: &FixedDir) -> Result<(), DriverError> {
        let size = dir.format.size();
        // Re-stage through the object map so a fragmented new-map
        // directory lands in the right places.
        let mut done = 0usize;
        while done < size {
            let take = (size - done).min(512);
            let data = &dir.data[done..done + take];
            self.object_write(indaddr, done as u64, data)?;
            done += take;
        }
        Ok(())
    }

    /// The `index`-th entry of the directory at `indaddr`, if any.
    fn dir_get(
        &mut self,
        indaddr: u32,
        size: u32,
        index: u32,
    ) -> Result<Option<Object>, DriverError> {
        match self.load_dir(indaddr, size)? {
            DirHandle::Fixed(dir) => Ok(usize::try_from(index).ok().and_then(|i| dir.entry(i))),
            DirHandle::Big(dir) => {
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr,
                    size: u32::MAX,
                };
                dir.entry(&mut store, index)
            }
        }
    }

    /// Find the entry named `name` in the directory at `indaddr`.
    fn dir_lookup(
        &mut self,
        indaddr: u32,
        size: u32,
        name: &[u8],
    ) -> Result<Option<(u32, Object)>, DriverError> {
        match self.load_dir(indaddr, size)? {
            DirHandle::Fixed(dir) => Ok(dir
                .find(name)
                // A fixed directory holds at most 77 entries.
                .map(|(index, object)| (u32::try_from(index).unwrap_or(0), object))),
            DirHandle::Big(dir) => {
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr,
                    size: u32::MAX,
                };
                dir.find(&mut store, name)
            }
        }
    }

    /// On-disc bytes allocated to the object at `indaddr` whose recorded
    /// size is `size`.
    fn object_allocated(&mut self, indaddr: u32, size: u32) -> Result<u64, DriverError> {
        match &self.backing {
            Backing::Old { .. } => {
                // An old-map object is exactly its contiguous sector run.
                Ok(u64::from(size).div_ceil(OLD_SECTOR_SIZE) * OLD_SECTOR_SIZE)
            }
            Backing::New { map, .. } => {
                if indaddr == 0 {
                    return Ok(0);
                }
                if indaddr & 0xFF != 0 {
                    // A shared-fragment object owns only its share
                    // granules; the fragment total belongs to several
                    // objects.
                    let granule = 1u64
                        << (u32::from(map.record.log2sharesize)
                            + u32::from(map.record.log2secsize));
                    return Ok(u64::from(size).div_ceil(granule) * granule);
                }
                map.object_allocated_bytes(&mut self.volume, indaddr >> 8)
            }
        }
    }
}

impl<B: Block> Adfs<B> {
    /// Allocate a fresh object of `bytes`, returning its indirect disc
    /// address (old map: start sector).
    fn allocate(&mut self, bytes: u64) -> Result<u32, DriverError> {
        match &self.backing {
            Backing::Old { .. } => {
                let sectors = u32::try_from(bytes.div_ceil(OLD_SECTOR_SIZE))
                    .map_err(|_| DriverError::NoSpace)?;
                let mut map = OldMap::load(&mut self.volume)?;
                let start = map.allocate(sectors)?;
                map.store(&mut self.volume)?;
                Ok(start)
            }
            Backing::New { map, .. } => map.allocate_object(&mut self.volume, bytes),
        }
    }

    /// Free the allocation of the object at `indaddr` whose recorded
    /// size is `size` (whole fragments only; the shared-fragment policy
    /// lives in the callers).
    fn free(&mut self, indaddr: u32, size: u32) -> Result<(), DriverError> {
        if indaddr == 0 {
            return Ok(());
        }
        match &self.backing {
            Backing::Old { .. } => {
                // A zero-length object occupies no sectors.
                let sectors = u64::from(size).div_ceil(OLD_SECTOR_SIZE);
                if sectors == 0 {
                    return Ok(());
                }
                // A `u32` size spans at most 2^24 sectors.
                let sectors = u32::try_from(sectors).map_err(|_| DriverError::BadMagic)?;
                let mut map = OldMap::load(&mut self.volume)?;
                map.free_span(indaddr, sectors)?;
                map.store(&mut self.volume)
            }
            Backing::New { map, .. } => map.free_object(&mut self.volume, indaddr >> 8),
        }
    }

    /// Copy `len` bytes from one object to another through the staging
    /// buffer.
    fn copy_object(&mut self, src: u32, dst: u32, len: u64) -> Result<(), DriverError> {
        let mut buf = [0u8; 512];
        let mut done = 0u64;
        while done < len {
            let take = usize::try_from((len - done).min(512)).unwrap_or(512);
            self.object_read(src, done, &mut buf[..take])?;
            self.object_write(dst, done, &buf[..take])?;
            done += take as u64;
        }
        Ok(())
    }

    /// Grow the object at `indaddr` (recorded size `size`) so its
    /// allocation covers `new_size` bytes, relocating it when in-place
    /// growth is impossible. Returns the object's (possibly new)
    /// indirect disc address.
    ///
    /// A relocated shared-fragment object leaves its old fragment to
    /// [`Self::release_maybe_shared`], which the caller invokes with
    /// the old address.
    fn grow_object(&mut self, indaddr: u32, size: u32, new_size: u64) -> Result<u32, DriverError> {
        if new_size <= u64::from(size) && indaddr != 0 {
            return Ok(indaddr);
        }
        match &self.backing {
            Backing::Old { .. } => {
                let old_sectors =
                    u32::try_from(u64::from(size).div_ceil(OLD_SECTOR_SIZE)).unwrap_or(0);
                let new_sectors = u32::try_from(new_size.div_ceil(OLD_SECTOR_SIZE))
                    .map_err(|_| DriverError::NoSpace)?;
                if new_sectors <= old_sectors {
                    return Ok(indaddr);
                }
                let mut map = OldMap::load(&mut self.volume)?;
                if indaddr != 0 && old_sectors != 0 {
                    // Try to consume free space directly after the run.
                    if map.try_extend(indaddr + old_sectors, new_sectors - old_sectors) {
                        map.store(&mut self.volume)?;
                        return Ok(indaddr);
                    }
                }
                // Relocate: allocate the full run, copy, free the old.
                let start = map.allocate(new_sectors)?;
                if indaddr != 0 && old_sectors != 0 {
                    map.free_span(indaddr, old_sectors)?;
                }
                map.store(&mut self.volume)?;
                if indaddr != 0 && size != 0 {
                    self.copy_object(indaddr, start, u64::from(size))?;
                }
                Ok(start)
            }
            Backing::New { map, .. } => {
                if indaddr == 0 {
                    return map.allocate_object(&mut self.volume, new_size);
                }
                if indaddr.trailing_zeros() >= 8
                    && map.extend_object(&mut self.volume, indaddr >> 8, new_size)?
                {
                    return Ok(indaddr);
                }
                // Relocate to a fresh exclusive object (also the path a
                // shared-fragment object takes to grow).
                let new_indaddr = map.allocate_object(&mut self.volume, new_size)?;
                if size != 0 {
                    self.copy_object(indaddr, new_indaddr, u64::from(size))?;
                }
                self.release_maybe_shared(indaddr, size)?;
                Ok(new_indaddr)
            }
        }
    }

    /// Shrink the allocation of the object at `indaddr` from `size` to
    /// `new_size` bytes.
    fn shrink_object(&mut self, indaddr: u32, size: u32, new_size: u64) -> Result<(), DriverError> {
        if indaddr == 0 || new_size >= u64::from(size) {
            return Ok(());
        }
        match &self.backing {
            Backing::Old { .. } => {
                let old_sectors =
                    u32::try_from(u64::from(size).div_ceil(OLD_SECTOR_SIZE)).unwrap_or(0);
                let new_sectors =
                    u32::try_from(new_size.div_ceil(OLD_SECTOR_SIZE)).unwrap_or(old_sectors);
                if new_sectors >= old_sectors {
                    return Ok(());
                }
                let mut map = OldMap::load(&mut self.volume)?;
                map.free_span(indaddr + new_sectors, old_sectors - new_sectors)?;
                map.store(&mut self.volume)
            }
            Backing::New { map, .. } => {
                if indaddr & 0xFF != 0 {
                    // A shared-fragment object cannot release granules
                    // without moving its fragment neighbours; the slack
                    // is reclaimed when the object is removed or grows
                    // away.
                    return Ok(());
                }
                map.shrink_object(&mut self.volume, indaddr >> 8, new_size)
            }
        }
    }

    /// Release the allocation behind `indaddr`, honouring fragment
    /// sharing: a shared fragment is freed only when no other live
    /// directory entry references it.
    fn release_maybe_shared(&mut self, indaddr: u32, size: u32) -> Result<(), DriverError> {
        if indaddr == 0 {
            return Ok(());
        }
        let shared = matches!(&self.backing, Backing::New { .. }) && indaddr & 0xFF != 0;
        if shared && self.fragment_used_elsewhere(indaddr)? {
            return Ok(());
        }
        self.free(indaddr, size)
    }

    /// Whether any live directory entry other than `skip_indaddr`
    /// references the same new-map fragment id.
    ///
    /// The walk is depth-first over the directory tree with an explicit,
    /// bounded stack: a genuine `FileCore` tree never approaches the
    /// bound (RISC OS's path length limits nesting long before it), so
    /// exceeding it means a corrupt or cyclic tree and fails closed.
    fn fragment_used_elsewhere(&mut self, skip_indaddr: u32) -> Result<bool, DriverError> {
        const MAX_DEPTH: usize = 96;
        let frag_id = skip_indaddr >> 8;
        let root = self.root_node();
        let mut stack = [(0u32, 0u32, 0u32); MAX_DEPTH];
        stack[0] = (node_addr(root), node_size(root), 0);
        let mut depth = 0usize;
        loop {
            let (dir_addr, dir_size, index) = stack[depth];
            match self.dir_get(dir_addr, dir_size, index)? {
                None => {
                    if depth == 0 {
                        return Ok(false);
                    }
                    depth -= 1;
                }
                Some(object) => {
                    stack[depth].2 = index + 1;
                    if object.indaddr >> 8 == frag_id && object.indaddr != skip_indaddr {
                        return Ok(true);
                    }
                    if object.is_dir() {
                        depth += 1;
                        if depth == MAX_DEPTH {
                            return Err(DriverError::BadMagic);
                        }
                        stack[depth] = (object.indaddr, object.size, 0);
                    }
                }
            }
        }
    }
}

/// Bytes reserved in front of a shared-fragment object by its share
/// offset (the object sits `(N - 1)` granules into its fragment).
fn share_base_bytes(record: &DiscRecord, indaddr: u32) -> u64 {
    let share = u64::from(indaddr & 0xFF);
    if share == 0 {
        0
    } else {
        (share - 1) << (u32::from(record.log2sharesize) + u32::from(record.log2secsize))
    }
}

/// Largest big directory the format permits (4096 KiB).
const MAX_BIG_DIR: u32 = 4096 * 1024;

/// Characters `FileCore` forbids in an object name (the path and
/// wildcard vocabulary), on top of requiring printable ASCII.
const FORBIDDEN_NAME_BYTES: &[u8] = b"$&%@\\^:.#*\"|";

impl<B: Block> Adfs<B> {
    /// Validate a name being written to a directory.
    fn validate_new_name(&self, name: &[u8]) -> Result<(), DriverError> {
        let max = if self.is_big_dir() {
            dir::MAX_NAME_LEN
        } else {
            dir::FIXED_NAME_LEN
        };
        if name.is_empty() || name.len() > max {
            return Err(DriverError::LengthOutOfRange);
        }
        if name
            .iter()
            .any(|&b| !(0x21..=0x7E).contains(&b) || FORBIDDEN_NAME_BYTES.contains(&b))
        {
            return Err(DriverError::OutOfRange);
        }
        Ok(())
    }

    /// Grow the object at `indaddr` to hold `new_bytes` **without
    /// relocating it** (directories must keep their address). Returns
    /// whether the growth succeeded.
    fn grow_in_place(&mut self, indaddr: u32, new_bytes: u64) -> Result<bool, DriverError> {
        match &self.backing {
            // Old-map directories are fixed-size; nothing ever grows in
            // place.
            Backing::Old { .. } => Ok(false),
            Backing::New { map, .. } => {
                let total = share_base_bytes(&map.record, indaddr) + new_bytes;
                map.extend_object(&mut self.volume, indaddr >> 8, total)
            }
        }
    }

    /// Insert `object` into the directory at `dir_addr`, growing a big
    /// directory in place when it is full.
    fn dir_insert(
        &mut self,
        dir_addr: u32,
        dir_size: u32,
        object: &Object,
    ) -> Result<(), DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(mut dir) => {
                dir.insert(object)?;
                self.store_fixed_dir(dir_addr, &dir)
            }
            DirHandle::Big(mut dir) => {
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr: dir_addr,
                    size: u32::MAX,
                };
                match dir.insert(&mut store, object) {
                    Err(DriverError::NoSpace) => {}
                    other => return other,
                }
                // Full: grow by one grain and retry.
                let new_size = dir
                    .header
                    .size
                    .checked_add(bigdir::BIG_DIR_GRAIN)
                    .filter(|&s| s <= MAX_BIG_DIR)
                    .ok_or(DriverError::NoSpace)?;
                if !self.grow_in_place(dir_addr, u64::from(new_size))? {
                    return Err(DriverError::NoSpace);
                }
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr: dir_addr,
                    size: u32::MAX,
                };
                dir.grow(&mut store, new_size)?;
                dir.insert(&mut store, object)?;
                self.record_dir_size(dir_addr, new_size)
            }
        }
    }

    /// Remove the entry at `index` from the directory at `dir_addr`.
    fn dir_remove_at(
        &mut self,
        dir_addr: u32,
        dir_size: u32,
        index: u32,
    ) -> Result<(), DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(mut dir) => {
                dir.remove(index as usize);
                self.store_fixed_dir(dir_addr, &dir)
            }
            DirHandle::Big(mut dir) => {
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr: dir_addr,
                    size: u32::MAX,
                };
                dir.remove(&mut store, index)
            }
        }
    }

    /// Rewrite the entry at `index` of the directory at `dir_addr` with
    /// `object`'s metadata (the name is unchanged).
    fn dir_update_at(
        &mut self,
        dir_addr: u32,
        dir_size: u32,
        index: u32,
        object: &Object,
    ) -> Result<(), DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(mut dir) => {
                dir.update(index as usize, object);
                self.store_fixed_dir(dir_addr, &dir)
            }
            DirHandle::Big(mut dir) => {
                let mut store = ObjectStore {
                    adfs: self,
                    indaddr: dir_addr,
                    size: u32::MAX,
                };
                dir.update(&mut store, index, object)
            }
        }
    }

    /// The parent address recorded inside the directory at `dir_addr`.
    fn dir_parent(&mut self, dir_addr: u32, dir_size: u32) -> Result<u32, DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(dir) => Ok(dir.parent()),
            DirHandle::Big(dir) => Ok(dir.header.parent),
        }
    }

    /// Record a grown big directory's new size where its size is
    /// stored: the parent's entry, or the disc record for the root.
    fn record_dir_size(&mut self, dir_addr: u32, new_size: u32) -> Result<(), DriverError> {
        let root = self.root_node();
        if dir_addr == node_addr(root) {
            // Only big directories grow, and they exist only on new-map
            // volumes.
            let Backing::New { map, boot_block } = &mut self.backing else {
                return Err(DriverError::BadMagic);
            };
            let boot = *boot_block;
            return map.set_root_size(&mut self.volume, boot, new_size);
        }
        let parent = self.dir_parent(dir_addr, 0)?;
        // Find the directory's entry in its parent by address.
        let mut index = 0u32;
        loop {
            let Some(entry) = self.dir_get(parent, 0, index)? else {
                // The tree disagrees with the child's parent pointer.
                return Err(DriverError::BadMagic);
            };
            if entry.indaddr == dir_addr {
                let mut updated = entry;
                updated.size = new_size;
                return self.dir_update_at(parent, 0, index, &updated);
            }
            index += 1;
        }
    }
}

/// A loaded, validated directory of either shape. A fixed directory is
/// held whole on the stack by design (the driver is allocation-free),
/// so the variant size difference is deliberate.
#[allow(clippy::large_enum_variant)]
enum DirHandle {
    /// A 1280- or 2048-byte fixed directory, held in memory.
    Fixed(FixedDir),
    /// A big directory (the engine streams through the object).
    Big(BigDir),
}

impl<B: Block> FilesystemRead for Adfs<B> {
    fn root(&self) -> NodeId {
        self.root_node()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        let size = node_size(node);
        let allocated = self.object_allocated(node_addr(node), size)?;
        if node_is_dir(node) {
            Ok(NodeInfo {
                kind: NodeKind::Directory,
                size: 0,
                allocated,
            })
        } else {
            Ok(NodeInfo {
                kind: NodeKind::RegularFile,
                size: u64::from(size),
                allocated,
            })
        }
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        if !node_is_valid(dir) || !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        if name.is_empty() || name.len() > dir::MAX_NAME_LEN {
            return Err(DriverError::NotFound);
        }
        match self.dir_lookup(node_addr(dir), node_size(dir), name)? {
            Some((_, object)) => Ok(object_node(&object)),
            None => Err(DriverError::NotFound),
        }
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        if !node_is_valid(file) {
            return Err(DriverError::NotFound);
        }
        if node_is_dir(file) {
            return Err(DriverError::Unsupported);
        }
        let size = u64::from(node_size(file));
        if buf.is_empty() || offset >= size {
            return Ok(0);
        }
        let want = usize::try_from((size - offset).min(buf.len() as u64))
            .map_err(|_| DriverError::LengthOutOfRange)?;
        self.object_read(node_addr(file), offset, &mut buf[..want])?;
        Ok(want)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        if !node_is_valid(dir) || !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        let Ok(index) = u32::try_from(cursor) else {
            // A cursor beyond any possible entry is a finished listing.
            return Ok(None);
        };
        let Some(object) = self.dir_get(node_addr(dir), node_size(dir), index)? else {
            return Ok(None);
        };
        if object.name_len > name_out.len() {
            return Err(DriverError::BufferTooSmall);
        }
        name_out[..object.name_len].copy_from_slice(object.name());
        let node = object_node(&object);
        let info = self.node_info(node)?;
        Ok(Some(DirEntry {
            node,
            info,
            modified: object_stamp(&object),
            name_len: object.name_len,
            next_cursor: u64::from(index) + 1,
        }))
    }
}

impl<B: Block> Adfs<B> {
    /// Resolve `name` within the directory node `dir`.
    fn resolve_child(&mut self, dir: NodeId, name: &[u8]) -> Result<(u32, Object), DriverError> {
        if !node_is_valid(dir) || !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        if name.is_empty() || name.len() > dir::MAX_NAME_LEN {
            return Err(DriverError::NotFound);
        }
        self.dir_lookup(node_addr(dir), node_size(dir), name)?
            .ok_or(DriverError::NotFound)
    }

    /// Whether the directory at `dir_addr` has no entries.
    fn dir_is_empty(&mut self, dir_addr: u32, dir_size: u32) -> Result<bool, DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(dir) => Ok(dir.count() == 0),
            DirHandle::Big(dir) => Ok(dir.header.entries == 0),
        }
    }

    /// The `Hugo`/`Nick` marker of the fixed directory at `dir_addr`,
    /// copied onto directories created inside it so a volume stays
    /// marker-consistent.
    fn fixed_marker(&mut self, dir_addr: u32, dir_size: u32) -> Result<[u8; 4], DriverError> {
        match self.load_dir(dir_addr, dir_size)? {
            DirHandle::Fixed(dir) => {
                let mut marker = [0u8; 4];
                marker.copy_from_slice(&dir.data[1..5]);
                Ok(marker)
            }
            DirHandle::Big(_) => Err(DriverError::BadMagic),
        }
    }

    /// Zero the byte range `[from, to)` of the object at `indaddr`.
    fn zero_object_range(&mut self, indaddr: u32, from: u64, to: u64) -> Result<(), DriverError> {
        let zeroes = [0u8; 512];
        let mut at = from;
        while at < to {
            let take = usize::try_from((to - at).min(512)).unwrap_or(512);
            self.object_write(indaddr, at, &zeroes[..take])?;
            at += take as u64;
        }
        Ok(())
    }
}

impl<B: Block> FilesystemWrite for Adfs<B> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        if !node_is_valid(dir) || !node_is_dir(dir) {
            return Err(DriverError::Unsupported);
        }
        self.validate_new_name(name)?;
        let dir_addr = node_addr(dir);
        let dir_size = node_size(dir);
        if self.dir_lookup(dir_addr, dir_size, name)?.is_some() {
            return Err(DriverError::Busy);
        }
        let mut object = Object::named(name)?;
        match kind {
            NodeKind::RegularFile => {
                // Created with owner read/write, no data allocation.
                object.attr = dir::ATTR_OWNER_READ | dir::ATTR_OWNER_WRITE;
                self.dir_insert(dir_addr, dir_size, &object)?;
            }
            NodeKind::Directory => {
                object.attr = dir::ATTR_DIRECTORY | dir::ATTR_OWNER_READ;
                // 8-bit ADFS (the 1280-byte directory format) creates
                // directories locked; 32-bit `FileCore` does not.
                if !self.is_big_dir() && self.fixed_format() == FixedFormat::Old {
                    object.attr |= dir::ATTR_LOCKED;
                }
                let (bytes, big) = if self.is_big_dir() {
                    (bigdir::BIG_DIR_GRAIN, true)
                } else {
                    (fixed_size_u32(self.fixed_format()), false)
                };
                let child = self.allocate(u64::from(bytes))?;
                let seeded = if big {
                    let mut store = ObjectStore {
                        adfs: self,
                        indaddr: child,
                        size: u32::MAX,
                    };
                    BigDir::initialise(&mut store, bytes, name, dir_addr)
                } else {
                    let format = self.fixed_format();
                    self.fixed_marker(dir_addr, dir_size).and_then(|marker| {
                        let fresh = FixedDir::initialise(format, marker, name, dir_addr);
                        self.store_fixed_dir(child, &fresh)
                    })
                };
                object.indaddr = child;
                object.size = bytes;
                let inserted = seeded.and_then(|()| self.dir_insert(dir_addr, dir_size, &object));
                if let Err(err) = inserted {
                    // Fail closed: reclaim the orphaned allocation.
                    self.free(child, bytes)?;
                    return Err(err);
                }
            }
        }
        Ok(object_node(&object))
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        let (index, object) = self.resolve_child(dir, name)?;
        if object.is_dir() {
            return Err(DriverError::Unsupported);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        // An ADFS length field is 32 bits; larger files cannot exist on
        // the format.
        if end > u64::from(u32::MAX) {
            return Err(DriverError::NoSpace);
        }
        let size = object.size;
        let indaddr = if end > u64::from(size) {
            self.grow_object(object.indaddr, size, end)?
        } else {
            object.indaddr
        };
        if offset > u64::from(size) {
            self.zero_object_range(indaddr, u64::from(size), offset)?;
        }
        self.object_write(indaddr, offset, data)?;
        if end > u64::from(size) || indaddr != object.indaddr {
            let mut updated = object;
            updated.indaddr = indaddr;
            // `end` was bounded by `u32::MAX` above.
            updated.size = size.max(u32::try_from(end).unwrap_or(u32::MAX));
            self.dir_update_at(node_addr(dir), node_size(dir), index, &updated)?;
        }
        Ok(data.len())
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        let (index, object) = self.resolve_child(dir, name)?;
        if object.is_dir() {
            return Err(DriverError::Unsupported);
        }
        if size > u64::from(u32::MAX) {
            return Err(DriverError::NoSpace);
        }
        let old = u64::from(object.size);
        let mut updated = object;
        match size.cmp(&old) {
            core::cmp::Ordering::Greater => {
                updated.indaddr = self.grow_object(object.indaddr, object.size, size)?;
                self.zero_object_range(updated.indaddr, old, size)?;
            }
            core::cmp::Ordering::Less if size == 0 => {
                self.release_maybe_shared(object.indaddr, object.size)?;
                updated.indaddr = 0;
            }
            core::cmp::Ordering::Less => {
                self.shrink_object(object.indaddr, object.size, size)?;
            }
            core::cmp::Ordering::Equal => return Ok(()),
        }
        // `size` was bounded by `u32::MAX` above.
        updated.size = u32::try_from(size).unwrap_or(u32::MAX);
        self.dir_update_at(node_addr(dir), node_size(dir), index, &updated)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        let (index, object) = self.resolve_child(dir, name)?;
        if object.is_dir() && !self.dir_is_empty(object.indaddr, object.size)? {
            return Err(DriverError::Busy);
        }
        self.dir_remove_at(node_addr(dir), node_size(dir), index)?;
        self.release_maybe_shared(object.indaddr, object.size)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        if !node_is_valid(dst_dir) || !node_is_dir(dst_dir) {
            return Err(DriverError::Unsupported);
        }
        let (src_index, object) = self.resolve_child(src_dir, src_name)?;
        let src_addr = node_addr(src_dir);
        let dst_addr = node_addr(dst_dir);
        if src_addr == dst_addr && dir::name_eq(src_name, dst_name) {
            return Ok(());
        }
        self.validate_new_name(dst_name)?;
        // Moving a directory into itself or its own subtree would
        // detach the cycle from the tree; walk the destination's
        // ancestry (bounded like the liveness walk).
        if object.is_dir() && src_addr != dst_addr {
            let root_addr = node_addr(self.root_node());
            let mut cursor = dst_addr;
            let mut reached_root = false;
            for _ in 0..96 {
                if cursor == object.indaddr {
                    return Err(DriverError::Busy);
                }
                if cursor == root_addr {
                    reached_root = true;
                    break;
                }
                cursor = self.dir_parent(cursor, 0)?;
            }
            if !reached_root {
                // Ancestry that never reaches the root is corrupt (or
                // cyclic); fail closed rather than risk detaching it.
                return Err(DriverError::BadMagic);
            }
        }
        let replaced = self.dir_lookup(dst_addr, node_size(dst_dir), dst_name)?;
        if let Some((_, existing)) = &replaced {
            // Kind-compatible replacement only; a directory may replace
            // only an empty directory.
            if existing.is_dir() != object.is_dir() {
                return Err(DriverError::Unsupported);
            }
            if existing.is_dir() && !self.dir_is_empty(existing.indaddr, existing.size)? {
                return Err(DriverError::Busy);
            }
        }
        self.dir_remove_at(src_addr, node_size(src_dir), src_index)?;
        if let Some((_, existing)) = &replaced {
            // The destination entry already carries the right name:
            // point it at the moved object and free the replaced
            // one. (Indices may have shifted when the source entry
            // left the same directory, so look the name up afresh.)
            let Some((dst_index, _)) = self.dir_lookup(dst_addr, node_size(dst_dir), dst_name)?
            else {
                return Err(DriverError::BadMagic);
            };
            let mut updated = object;
            updated.name = existing.name;
            updated.name_len = existing.name_len;
            self.dir_update_at(dst_addr, node_size(dst_dir), dst_index, &updated)?;
            self.release_maybe_shared(existing.indaddr, existing.size)?;
        } else {
            let mut moved = object;
            moved.name = [0; dir::MAX_NAME_LEN];
            moved.name[..dst_name.len()].copy_from_slice(dst_name);
            moved.name_len = dst_name.len();
            if let Err(err) = self.dir_insert(dst_addr, node_size(dst_dir), &moved) {
                // Roll the source entry back so no entry is lost.
                self.dir_insert(src_addr, node_size(src_dir), &object)?;
                return Err(err);
            }
        }
        // A moved directory's parent pointer follows it.
        if object.is_dir() && src_addr != dst_addr {
            match self.load_dir(object.indaddr, object.size)? {
                DirHandle::Fixed(mut child) => {
                    child.set_parent(dst_addr);
                    self.store_fixed_dir(object.indaddr, &child)?;
                }
                DirHandle::Big(mut child) => {
                    let mut store = ObjectStore {
                        adfs: self,
                        indaddr: object.indaddr,
                        size: u32::MAX,
                    };
                    child.set_parent(&mut store, dst_addr)?;
                }
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // Every mutation writes through to the device.
        Ok(())
    }
}

/// The canonical `acorn.*` attribute keys this format stores, in the
/// stable order [`FilesystemAttrs::list_attr`] enumerates them.
const ACORN_KEYS: [&[u8]; 5] = [
    b"acorn.loadaddr",
    b"acorn.execaddr",
    b"acorn.attr",
    b"acorn.filetype",
    b"acorn.datestamp",
];

impl<B: Block> Adfs<B> {
    /// Find the directory entry whose object sits at `indaddr`,
    /// returning `(parent_addr, parent_size, index, object)`.
    ///
    /// The walk mirrors [`Self::fragment_used_elsewhere`]: depth-first
    /// with a bounded stack, failing closed on a tree deeper than any
    /// genuine `FileCore` volume.
    fn find_entry_by_addr(
        &mut self,
        indaddr: u32,
    ) -> Result<Option<(u32, u32, u32, Object)>, DriverError> {
        const MAX_DEPTH: usize = 96;
        let root = self.root_node();
        let mut stack = [(0u32, 0u32, 0u32); MAX_DEPTH];
        stack[0] = (node_addr(root), node_size(root), 0);
        let mut depth = 0usize;
        loop {
            let (dir_addr, dir_size, index) = stack[depth];
            match self.dir_get(dir_addr, dir_size, index)? {
                None => {
                    if depth == 0 {
                        return Ok(None);
                    }
                    depth -= 1;
                }
                Some(object) => {
                    stack[depth].2 = index + 1;
                    if object.indaddr == indaddr {
                        return Ok(Some((dir_addr, dir_size, index, object)));
                    }
                    if object.is_dir() {
                        depth += 1;
                        if depth == MAX_DEPTH {
                            return Err(DriverError::BadMagic);
                        }
                        stack[depth] = (object.indaddr, object.size, 0);
                    }
                }
            }
        }
    }

    /// Resolve `node` to its directory entry, or the error the trait
    /// method reports for a dead node.
    fn entry_of(&mut self, node: NodeId) -> Result<(u32, u32, u32, Object), DriverError> {
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        self.find_entry_by_addr(node_addr(node))?
            .ok_or(DriverError::NotFound)
    }

    /// The attribute bits this volume's directory format can store.
    fn storable_attr_bits(&self) -> u16 {
        if self.is_big_dir() {
            0xFF
        } else {
            match self.fixed_format() {
                FixedFormat::Old => acorn::ATTR_BITS,
                FixedFormat::New => 0x7F,
            }
        }
    }

    /// The `acorn.*` keys present on `object`, in enumeration order.
    fn present_keys(object: &Object) -> ([bool; ACORN_KEYS.len()], usize) {
        let typed = matches!(
            acorn::decode_load_exec(object.load, object.exec),
            acorn::LoadExec::Typed { .. }
        );
        let present = [true, true, true, typed, typed];
        let count = present.iter().filter(|&&p| p).count();
        (present, count)
    }
}

impl<B: Block> FilesystemTimestamps for Adfs<B> {
    fn times(&mut self, node: NodeId) -> Result<NodeTimes, DriverError> {
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        if node == self.root_node() {
            // The root has no directory entry and so no stored stamp.
            return Ok(NodeTimes::default());
        }
        let (_, _, _, object) = self.entry_of(node)?;
        let stamp = object_stamp(&object);
        // ADFS stores exactly one instant (set at creation, rewritten on
        // update); it is honestly all four.
        Ok(NodeTimes {
            created: stamp,
            modified: stamp,
            accessed: stamp,
            changed: stamp,
        })
    }
}

impl<B: Block> FilesystemAttrs for Adfs<B> {
    fn get_attr(
        &mut self,
        node: NodeId,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        let key = rustos_fsmeta::AttrKey::parse(key).map_err(|_| DriverError::OutOfRange)?;
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        if node == self.root_node() {
            // The root has no directory entry to hold metadata.
            return Ok(None);
        }
        let (_, _, _, object) = self.entry_of(node)?;
        let decoded = acorn::decode_load_exec(object.load, object.exec);
        let mut staging = [0u8; acorn::ATTR_VALUE_MAX];
        let value: &[u8] = match key.as_bytes() {
            b"acorn.loadaddr" => {
                staging[..8].copy_from_slice(&acorn::addr_to_value(object.load));
                &staging[..8]
            }
            b"acorn.execaddr" => {
                staging[..8].copy_from_slice(&acorn::addr_to_value(object.exec));
                &staging[..8]
            }
            b"acorn.attr" => {
                let (encoded, len) = acorn::attr_to_value(object.attr & acorn::ATTR_BITS)
                    .map_err(|_| DriverError::OutOfRange)?;
                staging[..len].copy_from_slice(&encoded[..len]);
                &staging[..len]
            }
            b"acorn.filetype" => match decoded {
                acorn::LoadExec::Typed { filetype, .. } => {
                    let encoded =
                        acorn::filetype_to_value(filetype).map_err(|_| DriverError::OutOfRange)?;
                    staging[..3].copy_from_slice(&encoded);
                    &staging[..3]
                }
                acorn::LoadExec::Untyped { .. } => return Ok(None),
            },
            b"acorn.datestamp" => match decoded {
                acorn::LoadExec::Typed { centiseconds, .. } => {
                    let encoded = acorn::datestamp_to_value(centiseconds)
                        .map_err(|_| DriverError::OutOfRange)?;
                    staging[..10].copy_from_slice(&encoded);
                    &staging[..10]
                }
                acorn::LoadExec::Untyped { .. } => return Ok(None),
            },
            // Any other valid key is simply not present on this format.
            _ => return Ok(None),
        };
        if value.len() > value_out.len() {
            return Err(DriverError::BufferTooSmall);
        }
        value_out[..value.len()].copy_from_slice(value);
        Ok(Some(value.len()))
    }

    fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError> {
        let key = rustos_fsmeta::AttrKey::parse(key).map_err(|_| DriverError::OutOfRange)?;
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        if node == self.root_node() {
            // The root has no directory entry to hold metadata.
            return Err(DriverError::Unsupported);
        }
        let (parent, parent_size, index, object) = self.entry_of(node)?;
        let decoded = acorn::decode_load_exec(object.load, object.exec);
        let mut updated = object;
        match key.as_bytes() {
            b"acorn.loadaddr" => {
                updated.load =
                    acorn::addr_from_value(value).map_err(|_| DriverError::OutOfRange)?;
            }
            b"acorn.execaddr" => {
                updated.exec =
                    acorn::addr_from_value(value).map_err(|_| DriverError::OutOfRange)?;
            }
            b"acorn.attr" => {
                let attr = acorn::attr_from_value(value).map_err(|_| DriverError::OutOfRange)?;
                // The directory bit is the object's kind, not a free
                // attribute, and the volume's directory format bounds
                // the storable bits.
                if (attr & dir::ATTR_DIRECTORY != 0) != object.is_dir()
                    || attr & !self.storable_attr_bits() != 0
                {
                    return Err(DriverError::OutOfRange);
                }
                updated.attr = attr;
            }
            b"acorn.filetype" => {
                let filetype =
                    acorn::filetype_from_value(value).map_err(|_| DriverError::OutOfRange)?;
                let centiseconds = match decoded {
                    acorn::LoadExec::Typed { centiseconds, .. } => centiseconds,
                    acorn::LoadExec::Untyped { .. } => 0,
                };
                let (load, exec) = acorn::encode_typed(filetype, centiseconds)
                    .map_err(|_| DriverError::OutOfRange)?;
                updated.load = load;
                updated.exec = exec;
            }
            b"acorn.datestamp" => {
                let centiseconds =
                    acorn::datestamp_from_value(value).map_err(|_| DriverError::OutOfRange)?;
                let acorn::LoadExec::Typed { filetype, .. } = decoded else {
                    // The stamp lives inside the filetype encoding; an
                    // untyped object has nowhere to keep one.
                    return Err(DriverError::Unsupported);
                };
                let (load, exec) = acorn::encode_typed(filetype, centiseconds)
                    .map_err(|_| DriverError::OutOfRange)?;
                updated.load = load;
                updated.exec = exec;
            }
            // ADFS stores no general-purpose attributes; every other
            // namespace has nowhere to live.
            _ => return Err(DriverError::Unsupported),
        }
        self.dir_update_at(parent, parent_size, index, &updated)
    }

    fn list_attr(
        &mut self,
        node: NodeId,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        if node == self.root_node() {
            return Ok(None);
        }
        let (_, _, _, object) = self.entry_of(node)?;
        let (present, _) = Self::present_keys(&object);
        let mut seen = 0u64;
        for (key, &here) in ACORN_KEYS.iter().zip(present.iter()) {
            if !here {
                continue;
            }
            if seen == index {
                if key.len() > key_out.len() {
                    return Err(DriverError::BufferTooSmall);
                }
                key_out[..key.len()].copy_from_slice(key);
                return Ok(Some(key.len()));
            }
            seen += 1;
        }
        Ok(None)
    }

    fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
        let key = rustos_fsmeta::AttrKey::parse(key).map_err(|_| DriverError::OutOfRange)?;
        if !node_is_valid(node) {
            return Err(DriverError::NotFound);
        }
        if node == self.root_node() {
            return Err(DriverError::NotFound);
        }
        let (parent, parent_size, index, object) = self.entry_of(node)?;
        let decoded = acorn::decode_load_exec(object.load, object.exec);
        let mut updated = object;
        match key.as_bytes() {
            b"acorn.filetype" => match decoded {
                acorn::LoadExec::Typed { .. } => {
                    // Untyping clears the whole encoding — the stamp
                    // lives inside it.
                    updated.load = 0;
                    updated.exec = 0;
                }
                acorn::LoadExec::Untyped { .. } => return Err(DriverError::NotFound),
            },
            b"acorn.datestamp" => match decoded {
                acorn::LoadExec::Typed { filetype, .. } => {
                    let (load, exec) =
                        acorn::encode_typed(filetype, 0).map_err(|_| DriverError::OutOfRange)?;
                    updated.load = load;
                    updated.exec = exec;
                }
                acorn::LoadExec::Untyped { .. } => return Err(DriverError::NotFound),
            },
            // The addresses and attribute bits are structural fields of
            // every entry; they cannot be absent.
            b"acorn.loadaddr" | b"acorn.execaddr" | b"acorn.attr" => {
                return Err(DriverError::Unsupported)
            }
            _ => return Err(DriverError::NotFound),
        }
        self.dir_update_at(parent, parent_size, index, &updated)
    }
}

impl<B: Block> FilesystemStats for Adfs<B> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        match &self.backing {
            Backing::Old { .. } => {
                let map = OldMap::load(&mut self.volume)?;
                let free = map.free_sectors();
                Ok(VolumeStats {
                    block_size: OLD_SECTOR_SIZE_U32,
                    total_blocks: u64::from(map.disc_sectors()),
                    free_blocks: free,
                    avail_blocks: free,
                    // ADFS has no inode table; 0/0 is the honest
                    // "untracked" answer the ABI defines.
                    files: 0,
                    files_free: 0,
                })
            }
            Backing::New { map, .. } => {
                let bpmb = map.record.bytes_per_map_bit();
                let free = map.free_bytes(&mut self.volume)? / bpmb;
                Ok(VolumeStats {
                    block_size: u32::try_from(bpmb).unwrap_or(u32::MAX),
                    total_blocks: map.record.disc_size / bpmb,
                    free_blocks: free,
                    avail_blocks: free,
                    files: 0,
                    files_free: 0,
                })
            }
        }
    }
}

/// [`DirStore`] adapter exposing one object's bytes to the big-directory
/// engine.
struct ObjectStore<'a, B: Block> {
    adfs: &'a mut Adfs<B>,
    indaddr: u32,
    size: u32,
}

impl<B: Block> DirStore for ObjectStore<'_, B> {
    fn read_at(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), DriverError> {
        if u64::from(offset) + buf.len() as u64 > u64::from(self.size) {
            return Err(DriverError::BadMagic);
        }
        self.adfs.object_read(self.indaddr, u64::from(offset), buf)
    }

    fn write_at(&mut self, offset: u32, data: &[u8]) -> Result<(), DriverError> {
        if u64::from(offset) + data.len() as u64 > u64::from(self.size) {
            return Err(DriverError::BadMagic);
        }
        self.adfs
            .object_write(self.indaddr, u64::from(offset), data)
    }
}
