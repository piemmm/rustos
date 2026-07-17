//! Compressed cluster extents (`docs/src/filesystem/arxfs-spec.md` §10).
//!
//! A compression **cluster** is an aligned run of
//! [`COMPRESS_CLUSTER_BLOCKS`] logical blocks.
//! When a write covers a whole cluster with compressible data, the cluster's
//! plaintext is compressed as one `tairix_compress` frame and stored in
//! **fewer** contiguous physical blocks — the saved blocks are real free
//! space, unlike a per-block compression that can never free anything inside
//! a fixed 1:1 block. The mapping stays exact: offsets still divide into
//! logical blocks, a compressed extent covers exactly one whole cluster, and
//! reading any byte decompresses at most one bounded cluster, so random
//! access stays O(log n) in the extent tree regardless of file size.
//!
//! Every stored block is sealed exactly like a raw record (per-block AEAD,
//! stored-form descriptor, content-slot hash, physical checksum — the
//! `integrity` module); the first stored block's descriptor carries the whole
//! frame length and each continuation carries its position, so a misdirected
//! or reordered stored block fails closed on read.
//!
//! Sharing is at cluster granularity: a reflink refcounts the whole stored
//! run through the chunk/reverse-reference trees keyed by the extent's first
//! physical block. Partial overwrites and mid-cluster truncates first
//! **decompose** the cluster back into per-block raw records (bounded work),
//! then proceed through the ordinary per-block copy-on-write path.

use alloc::vec;
use alloc::vec::Vec;

use tairix_abi::DriverError;

use crate::dedupe::{ChunkRecord, REVERSE_REF_CAP};
use crate::integrity::{logical_hash, DataFault, StoredForm, LOGICAL_HASH_LEN};
use crate::xform;
use crate::{
    as_u32, as_usize, extent_spec, is_all_zero, Block, Extent, Inode, ARXFS,
    COMPRESS_CLUSTER_BLOCKS, MAX_BLOCK_SIZE, METADATA_RESERVE, RING_BLOCKS,
};

impl<B: Block> ARXFS<B> {
    /// Allocate `run` physically contiguous free data blocks, scanning upward
    /// from the low end exactly like the single-block data allocator and
    /// honouring the same metadata reserve. Returns the first block of the
    /// run; every block is claimed (txn-private, rollback-recorded).
    ///
    /// # Errors
    ///
    /// [`DriverError::NoSpace`] when no contiguous free run of `run` blocks
    /// exists above the reserve — the caller falls back to per-block storage,
    /// so fragmentation degrades compression, never correctness.
    pub(crate) fn alloc_data_run(&mut self, run: u64) -> Result<u64, DriverError> {
        if run == 0 || self.free_count < METADATA_RESERVE.saturating_add(run) {
            return Err(DriverError::NoSpace);
        }
        let start = RING_BLOCKS;
        let total = self.total_blocks;
        let span = total.saturating_sub(start);
        let mut scanned = 0u64;
        let mut block = self.alloc_cursor.max(start);
        while scanned < span {
            if block + run > total {
                scanned += total - block;
                block = start;
                continue;
            }
            if let Some(used) = (0..run).find(|&b| self.bit_used(block + b)) {
                scanned += used + 1;
                block += used + 1;
            } else {
                for b in 0..run {
                    self.claim_block(block + b);
                }
                self.alloc_cursor = block + run;
                return Ok(block);
            }
        }
        Err(DriverError::NoSpace)
    }

    /// Compress and store the whole-cluster `plaintext` (a multiple of
    /// [`data_capacity`](Self::data_capacity) covering `len` logical blocks)
    /// as a compressed extent, returning its `(first physical block, stored
    /// blocks)` — or `None` when clustering cannot win, in which case the
    /// caller stores the blocks through the ordinary per-block path.
    ///
    /// Clustering wins only when the compressed frame fits in strictly fewer
    /// physical blocks than `len` **and** a contiguous free run of that size
    /// exists; an incompressible cluster or a fragmented volume degrades to
    /// raw storage, never to an error. Each stored block is sealed like any
    /// data record; the first carries the frame length, each continuation its
    /// position ([`StoredForm`]).
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on a seal or write failure.
    pub(crate) fn write_data_cluster(
        &mut self,
        plaintext: &[u8],
    ) -> Result<Option<(u64, u64)>, DriverError> {
        let capu = as_usize(self.data_capacity());
        let len_blocks = (plaintext.len() / capu) as u64;
        let mut frame = vec![0u8; plaintext.len()];
        let Ok(frame_len) = tairix_compress::compress(plaintext, &mut frame) else {
            // The frame would not even fit the plaintext-sized scratch:
            // incompressible, store per-block raw.
            return Ok(None);
        };
        let stored_blocks = (frame_len.div_ceil(capu)) as u64;
        if stored_blocks >= len_blocks {
            return Ok(None);
        }
        let phys = match self.alloc_data_run(stored_blocks) {
            Ok(phys) => phys,
            // No contiguous run: degrade to per-block storage (a genuinely
            // full volume fails closed there with the same error).
            Err(DriverError::NoSpace) => return Ok(None),
            Err(other) => return Err(other),
        };
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        for i in 0..as_usize(stored_blocks) {
            let offset = i * capu;
            let part = (frame_len - offset).min(capu);
            blk[..part].copy_from_slice(&frame[offset..offset + part]);
            blk[part..capu].fill(0);
            let form = if i == 0 {
                StoredForm::ClusterHead {
                    frame_len: as_u32(frame_len),
                }
            } else {
                StoredForm::ClusterPart { index: as_u32(i) }
            };
            self.seal_data_block(phys + i as u64, &mut blk, form)?;
        }
        Ok(Some((phys, stored_blocks)))
    }

    /// Read, verify, and decompress the compressed cluster extent `ext`,
    /// returning its plaintext (`ext.len` logical blocks of
    /// [`data_capacity`](Self::data_capacity) bytes).
    ///
    /// Every stored block is verified through the full per-block integrity
    /// pipeline, its stored form must match its position (a misdirected or
    /// reordered block fails closed), the frame length must agree with the
    /// extent's stored-block count, and the frame must decompress to exactly
    /// the cluster's logical size.
    ///
    /// # Errors
    ///
    /// [`DriverError::DeviceFault`] on any read, integrity, shape, or
    /// decompression failure (fail closed, never a panic).
    pub(crate) fn read_data_cluster(&mut self, ext: &Extent) -> Result<Vec<u8>, DriverError> {
        self.read_data_cluster_classified(ext)
            .map_err(|_| DriverError::DeviceFault)
    }

    /// As [`read_data_cluster`](Self::read_data_cluster), but reports *which*
    /// integrity layer rejected the cluster (the scrub seam). A shape
    /// mismatch — wrong stored form or position, a frame length that
    /// disagrees with the extent, or a wrong decompressed size — classifies
    /// as a logical fault.
    pub(crate) fn read_data_cluster_classified(
        &mut self,
        ext: &Extent,
    ) -> Result<Vec<u8>, DataFault> {
        let capu = as_usize(self.data_capacity());
        let mut frame = vec![0u8; as_usize(ext.stored) * capu];
        let frame_len = match self.read_cluster_frame(ext, capu, &mut frame) {
            Ok(frame_len) => frame_len,
            Err(fault) => {
                xform::scrub(frame);
                return Err(fault);
            }
        };
        let mut plain = vec![0u8; as_usize(ext.len) * capu];
        let produced = tairix_compress::decompress(&frame[..frame_len], &mut plain);
        // The frame holds the decrypted (compressed) content: wipe the
        // scratch before it returns to the heap, success or failure.
        xform::scrub(frame);
        match produced {
            Ok(produced) if produced == plain.len() => Ok(plain),
            _ => {
                xform::scrub(plain);
                Err(DataFault::Logical)
            }
        }
    }

    /// Read, verify, and decrypt the stored blocks of the compressed
    /// extent `ext` into `frame`, returning the whole frame's length as
    /// declared by the cluster head. Every stored form must match its
    /// position and the frame length must agree with the extent's
    /// stored-block count; a mismatch fails closed as a logical fault.
    fn read_cluster_frame(
        &mut self,
        ext: &Extent,
        capu: usize,
        frame: &mut [u8],
    ) -> Result<usize, DataFault> {
        let stored = as_usize(ext.stored);
        let mut frame_len = 0usize;
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        for i in 0..stored {
            let form = self.open_data_block(ext.phys + i as u64, &mut blk)?;
            match (i, form) {
                (0, StoredForm::ClusterHead { frame_len: len }) => {
                    frame_len = as_usize(u64::from(len));
                    if frame_len.div_ceil(capu) != stored {
                        return Err(DataFault::Logical);
                    }
                }
                (i, StoredForm::ClusterPart { index })
                    if i > 0 && as_usize(u64::from(index)) == i => {}
                _ => return Err(DataFault::Logical),
            }
            frame[i * capu..(i + 1) * capu].copy_from_slice(&blk[..capu]);
        }
        Ok(frame_len)
    }

    /// Drop the reference `(ino, start_bi)` holds on the compressed cluster
    /// extent `ext`. An unshared cluster's stored run is freed outright; a
    /// shared one is decremented and the referrer struck from its
    /// reverse-reference list, mirroring the per-block release
    /// (`docs/src/filesystem/arxfs-spec.md` §9).
    pub(crate) fn release_cluster(
        &mut self,
        ext: &Extent,
        ino: u32,
        start_bi: u64,
    ) -> Result<(), DriverError> {
        let Some(record) = self.chunk_get(ext.phys)? else {
            for b in 0..ext.stored {
                self.free_block(ext.phys + b);
            }
            return Ok(());
        };
        let mut referrers = self.reverse_refs(ext.phys)?;
        referrers.retain(|&(r_ino, r_bi)| !(r_ino == ino && r_bi == start_bi));
        let remaining = record.refcount.saturating_sub(1);
        if remaining <= 1 {
            self.chunk_remove(ext.phys)?;
            self.reverse_refs_remove(ext.phys)?;
        } else {
            let updated = ChunkRecord {
                refcount: remaining,
                ..record
            };
            self.chunk_put(ext.phys, &updated)?;
            self.reverse_refs_put(ext.phys, &referrers)?;
        }
        Ok(())
    }

    /// Add a reference from the `dst` referrer (`(inode, logical start)`) to
    /// the compressed cluster extent `ext` already held by the `src`
    /// referrer, promoting it to a shared chunk on first share or bumping its
    /// count thereafter. The chunk record stores the cluster's plaintext
    /// length and hash, so it is self-describing and distinguishable from a
    /// single-block chunk.
    pub(crate) fn share_cluster(
        &mut self,
        ext: &Extent,
        src: (u32, u64),
        dst: (u32, u64),
        plain_hash: &[u8; LOGICAL_HASH_LEN],
        plain_len: u32,
    ) -> Result<(), DriverError> {
        if let Some(record) = self.chunk_get(ext.phys)? {
            let mut referrers = self.reverse_refs(ext.phys)?;
            referrers.push(dst);
            let updated = ChunkRecord {
                refcount: record.refcount + 1,
                ..record
            };
            self.chunk_put(ext.phys, &updated)?;
            self.reverse_refs_put(ext.phys, &referrers)?;
        } else {
            let record = ChunkRecord {
                refcount: 2,
                domain: self.dedupe_domain,
                length: plain_len,
                logical_hash: *plain_hash,
            };
            self.chunk_put(ext.phys, &record)?;
            self.reverse_refs_put(ext.phys, &[src, dst])?;
        }
        Ok(())
    }

    /// Store `cluster` (one whole aligned cluster's plaintext,
    /// [`COMPRESS_CLUSTER_BLOCKS`] blocks of
    /// [`data_capacity`](Self::data_capacity) bytes) at logical block `start`
    /// of `inode` (number `ino`), replacing whatever previously backed the
    /// range: an old compressed extent is released whole, old raw blocks are
    /// released per block, holes stay holes.
    ///
    /// The cluster is compressed when that frees at least one physical block;
    /// an all-zero cluster becomes holes and an incompressible one stores
    /// through the ordinary per-block pipeline (zero detection, dedupe, raw).
    pub(crate) fn store_cluster(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        start: u64,
        cluster: &[u8],
    ) -> Result<(), DriverError> {
        let capu = as_usize(self.data_capacity());
        // Release a superseded compressed extent whole — the whole range is
        // being replaced, so its plaintext is never needed. Alignment makes
        // any compressed extent covering this range start exactly here;
        // anything else is corruption.
        if let Some((estart, ext)) = self.extent_lookup(inode, start)? {
            if ext.compressed {
                if estart != start {
                    return Err(DriverError::DeviceFault);
                }
                self.release_cluster(&ext, ino, estart)?;
                inode.extent_root =
                    self.btree_remove(inode.extent_root, estart, extent_spec(ino))?;
            }
        }
        if !is_all_zero(cluster) {
            if let Some((phys, stored)) = self.write_data_cluster(cluster)? {
                for b in 0..COMPRESS_CLUSTER_BLOCKS {
                    let bi = start + b;
                    let old = self.block_ptr(inode, bi)?;
                    if old != 0 {
                        self.release_block_ref(old, ino, bi)?;
                    }
                    self.extent_remove(inode, ino, bi)?;
                }
                let value = Extent::cluster(phys, COMPRESS_CLUSTER_BLOCKS, stored).encode();
                inode.extent_root =
                    self.btree_insert(inode.extent_root, start, &value, extent_spec(ino))?;
                return Ok(());
            }
        }
        // Per-block fallback: all-zero blocks become holes, the rest store
        // through the ordinary zero-detect/dedupe/raw pipeline.
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        for b in 0..COMPRESS_CLUSTER_BLOCKS {
            let bi = start + b;
            let old_ptr = self.block_ptr(inode, bi)?;
            blk[..capu].copy_from_slice(&cluster[as_usize(b) * capu..][..capu]);
            self.store_block(inode, ino, bi, old_ptr, &mut blk)?;
        }
        Ok(())
    }

    /// Point `dst`'s cluster starting at logical block `start` at the
    /// compressed extent `ext` already held by `(src_ino, start)`, sharing
    /// the stored run — or, when the cluster's referrer set is full, storing
    /// the plaintext again uniquely for `dst` so the referrer set stays exact
    /// and bounded (`docs/src/filesystem/arxfs-spec.md` §9).
    pub(crate) fn clone_cluster_ref(
        &mut self,
        src_ino: u32,
        dst: &mut Inode,
        dst_ino: u32,
        start: u64,
        ext: &Extent,
    ) -> Result<(), DriverError> {
        let plain = self.read_data_cluster(ext)?;
        if usize::try_from(self.data_refcount(ext.phys)?).unwrap_or(usize::MAX) >= REVERSE_REF_CAP {
            let result = self.store_cluster(dst, dst_ino, start, &plain);
            xform::scrub(plain);
            return result;
        }
        let hash = logical_hash(&plain);
        let plain_len = as_u32(plain.len());
        xform::scrub(plain);
        self.share_cluster(ext, (src_ino, start), (dst_ino, start), &hash, plain_len)?;
        dst.extent_root =
            self.btree_insert(dst.extent_root, start, &ext.encode(), extent_spec(dst_ino))?;
        Ok(())
    }

    /// Decompose the compressed cluster extent `ext` (starting at logical
    /// block `start` of `inode`, number `ino`) back into ordinary per-block
    /// records: read its plaintext, release the cluster's reference, and
    /// re-store each block through the per-block pipeline (holes stay holes,
    /// dedupe applies). Bounded work — at most one cluster.
    ///
    /// Callers use it before any operation that cannot apply to a sealed
    /// cluster: a partial overwrite, a mid-cluster truncate.
    pub(crate) fn decompose_cluster(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        start: u64,
        ext: &Extent,
    ) -> Result<(), DriverError> {
        let plain = self.read_data_cluster(ext)?;
        let result = self.restore_per_block(inode, ino, start, ext, &plain);
        xform::scrub(plain);
        result
    }

    /// The decompose tail: release the cluster's reference and re-store
    /// each block of its `plain`text through the per-block pipeline.
    /// Split out so [`decompose_cluster`](Self::decompose_cluster) can
    /// wipe the plaintext on every exit path.
    fn restore_per_block(
        &mut self,
        inode: &mut Inode,
        ino: u32,
        start: u64,
        ext: &Extent,
        plain: &[u8],
    ) -> Result<(), DriverError> {
        self.release_cluster(ext, ino, start)?;
        inode.extent_root = self.btree_remove(inode.extent_root, start, extent_spec(ino))?;
        let capu = as_usize(self.data_capacity());
        let mut blk = [0u8; MAX_BLOCK_SIZE];
        for b in 0..ext.len {
            let slice = &plain[as_usize(b) * capu..][..capu];
            blk[..capu].copy_from_slice(slice);
            self.store_block(inode, ino, start + b, 0, &mut blk)?;
        }
        Ok(())
    }
}
