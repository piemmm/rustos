# ARXFS Sparse File Support

Status: **done.** Sparse support is implemented in
`drivers/filesystem/arxfs/`, specified in
`docs/src/filesystem/arxfs-spec.md` §19, and covered by the §17 test set
below. What remains is named in §19 of this file: two behaviours that need a
filesystem operation TAIRiX does not expose yet, added in place with its first
consumer as item S1 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`.

ARXFS represents a hole **implicitly**, as a gap between per-file extent-tree
mappings — the form §2 permits — rather than as an explicit `Zero` extent
record. §3 below describes both permitted representations; the chosen one is
the implicit gap, so no `ExtentKind` field, no on-disk format bump, and nothing
extra to checksum, encrypt, compress, dedupe, scrub, relocate, or trim. Read
§3's `Zero`-extent shape as the alternative it rules on, not as a required
field.

This appendix defines the sparse-file layer for ARXFS. It is appended to the ARXFS specification, which already implements copy-on-write transactions, checksums, encryption, compression, dedupe, scrub, check, rescue, TRIM, and device-health handling.

Sparse support is mandatory, always enabled, and not tunable. There is no mount option, feature flag, user knob, or profile that disables it.

RLE/FILL records are intentionally not part of this appendix. Repeated non-zero data is handled by the existing first-party zstd-compatible compressor or by the normal RAW fallback. All-zero logical ranges are handled by sparse ZERO/Hole extents instead of zstd, RLE, or physical data records.

## 1. Purpose

ARXFS must store logical all-zero ranges without allocating physical data records.

A file containing 10 MiB of zero bytes must be represented as metadata-only ZERO/Hole extents plus inode metadata. It must not create a zstd payload, a dedupe chunk, an encrypted data record, or any other physical data blob for the zero range.

Sparse support is required for:

- Efficient all-zero files.
- Efficient files with large unwritten gaps.
- Efficient VM images, database files, disk images, logs, and generated artefacts that contain zero-filled regions.
- Correct POSIX-style sparse-file behaviour where reads from holes return zero bytes.
- Better SSD/NVMe behaviour because unneeded physical allocation is avoided rather than allocated and later discarded.

## 2. Terminology

A **data extent** maps a logical file range to a physical ARXFS chunk or record.

A **ZERO extent** maps a logical file range to zero bytes and has no physical data payload.

A **hole** is an unmapped logical file range that reads as zero. ARXFS may represent holes explicitly as ZERO extents or implicitly as gaps between extent mappings. The implementation may choose either representation internally, but the observable behaviour is identical.

A **sparse file** is any file whose logical size is larger than the physical data allocated for its contents because one or more logical ranges are ZERO extents or holes.

## 3. On-Disk Representation

A hole is either an explicit `Zero` extent record or an unmapped logical range.
ARXFS chose the unmapped range (see the status note at the head of this file),
so the `Zero`-extent shape below is the alternative this section rules on and
bounds, not a field the format carries.

```text
ExtentKind:
    Data
    Zero
```

A `Data` extent references an existing ARXFS physical chunk or record.

A `Zero` extent, in the explicit representation, contains:

```text
inode_id
logical_offset
logical_length
extent_kind = Zero
generation
metadata checksum/authentication fields inherited from ARXFS metadata
```

A `Zero` extent must not contain:

```text
physical device id
physical offset
chunk id
compression metadata
encryption nonce for a data payload
dedupe hash
refcount owner
physical checksum
```

There is no physical data to checksum, encrypt, compress, dedupe, scrub, relocate, or trim.

Metadata protecting the ZERO extent itself remains covered by the existing ARXFS metadata integrity and encryption rules.

## 4. Storage Pipeline Order

The write pipeline must detect all-zero logical records before compression, dedupe, encryption, or physical allocation.

Required order:

```text
plaintext write buffer
    -> zero detection
    -> if all zero: create/update ZERO extent and stop
    -> otherwise: existing dedupe/compression/encryption/write path
```

A zero range must not be passed to zstd merely to produce a tiny compressed block. Sparse ZERO extents are the canonical representation for all-zero data.

Repeated non-zero data, such as `0xFF` repeated for a whole record, is not special-cased by this appendix. It proceeds through the existing zstd-compatible compression path.

## 5. Read Semantics

Reads from ZERO extents and holes return zero-filled bytes.

The read path must be able to satisfy reads that cross mixed extent types:

```text
Data extent -> copy/decrypt/decompress stored data
Zero extent -> synthesize zero bytes
Hole        -> synthesize zero bytes
Data extent -> copy/decrypt/decompress stored data
```

Reads from sparse ranges must not perform physical disk reads for the zero portion.

Reads beyond end-of-file follow normal filesystem EOF behaviour and must not be confused with holes inside the file.

## 6. Write Semantics

### 6.1 Writing All-Zero Data

If a write buffer for a logical range is entirely zero, ARXFS must create or merge ZERO extents for that range.

If the range previously referenced physical data, the old data extents are removed from the current transaction view. Their physical chunks are released through the normal ARXFS copy-on-write refcount/free path.

If old physical chunks are still referenced by snapshots, reflinks, dedupe owners, or retained recovery roots, they remain live. They are not freed or trimmed until the existing ARXFS safety rules say they are unreachable.

### 6.2 Writing Non-Zero Data Into a Hole

A non-zero write into a ZERO extent or hole allocates normal physical data using the existing ARXFS data path.

The extent map must split as needed:

```text
before:
    [ Zero: 0..1 MiB ]

write non-zero 4 KiB at 512 KiB

after:
    [ Zero: 0..512 KiB ]
    [ Data: 512 KiB..516 KiB ]
    [ Zero: 516 KiB..1 MiB ]
```

### 6.3 Partial-Record Writes

Partial writes must preserve existing logical contents.

If a partial write into a data record makes the whole resulting logical record zero, ARXFS may replace that record with a ZERO extent.

If determining that would require expensive read-modify-check work, ARXFS may keep the result as a normal data record. The mandatory guarantee is that explicitly written all-zero buffers become sparse where the full written logical range is known to be zero.

### 6.4 File Extension

Extending a file with no written data creates a hole between the old EOF and the new EOF.

For example, setting file size to 10 MiB without writing data creates a 10 MiB logical file backed by no physical data records.

### 6.5 Truncation

Shrinking a file removes all extents beyond the new EOF. Removed physical data extents are released through the normal COW/refcount/free path. Removed ZERO extents only require metadata updates.

Expanding a file creates a hole from the old EOF to the new EOF.

## 7. Extent Normalisation

ARXFS must keep extent maps compact.

After any operation that creates, deletes, or splits extents, adjacent compatible ZERO extents should be merged within the same transaction.

Example:

```text
[ Zero: 0..128 KiB ] [ Zero: 128 KiB..256 KiB ]
```

normalises to:

```text
[ Zero: 0..256 KiB ]
```

A ZERO extent must never overlap a Data extent in the committed extent map for the same inode generation.

The committed extent map must be sorted by logical offset and must not contain overlapping extents.

## 8. Interaction With Dedupe

ZERO extents are not deduped chunks.

ARXFS must not create a global dedupe-index entry for an all-zero logical range represented as a ZERO extent.

Rationale: every zero range is already represented in the optimal shared form: no physical data at all.

If an existing physical all-zero data chunk is discovered, ARXFS may rewrite references to that chunk as ZERO extents. The physical chunk is then released when its refcount reaches zero and all snapshot/recovery constraints allow it. No background optimiser performs that rewrite today and none is planned: `plans/ARXFS-MAINTENANCE.md` drives verification and discard, and deliberately excludes data-rewriting optimisation from a health scheduler (§17 there). Scrub *reporting* the opportunity lands with that plan — it already decrypts every data block it verifies, so the all-zero test is free at that point — but acting on it is not scheduled work.

## 9. Interaction With Compression

ZERO extents bypass compression entirely.

The first-party zstd-compatible codec remains responsible for ordinary non-zero compressible data, including repeated non-zero data. Sparse handling is only for logical all-zero ranges.

This appendix deliberately avoids a separate RLE storage mode.

## 10. Interaction With Encryption

ZERO extents have no data payload and therefore no data encryption operation.

This must not create a plaintext data bypass because the logical bytes are defined by metadata as zero. The metadata that records the ZERO extent remains protected by the existing ARXFS metadata encryption, authentication, and checksum rules.

If ARXFS exposes encrypted-volume metadata leakage considerations elsewhere, sparse extents must be included in that model: an observer with raw-device access may infer allocation patterns unless the broader metadata-protection design hides them.

## 11. Interaction With Checksums and Scrub

ZERO extents have no physical data checksum.

Scrub must verify ZERO extents by validating their metadata only.

Deep scrub must treat ZERO extents as logically valid zero bytes and must not attempt to read a backing block.

If scrub sees a Data extent whose decrypted/decompressed logical contents are all zero, it reports it as a sparse-conversion opportunity (`plans/ARXFS-MAINTENANCE.md`). Conversion is not performed automatically: it is a data rewrite with refcount and snapshot interactions, so it belongs to an explicit operation that preserves COW, snapshots, refcounts, and transaction safety — never to the health scheduler.

## 12. Interaction With TRIM and Free Space

Creating a ZERO extent does not issue TRIM directly because no physical range belongs to the ZERO extent.

When a ZERO write replaces existing Data extents, the replaced physical ranges enter the existing ARXFS free/refcount/discard pipeline.

TRIM remains subject to the existing ARXFS safety rules:

```text
no live extent references
no snapshot references
no dedupe references
no retained recovery-root references
safe transaction generation reached
```

## 13. Filesystem Check and Recovery

`arxfs check` must validate sparse metadata.

Required checks:

```text
- ZERO extents have no physical address.
- ZERO extents have no chunk id.
- ZERO extents have no compression metadata.
- ZERO extents have no data checksum field requiring validation.
- ZERO extents have valid logical offset and length.
- ZERO extents do not overlap Data extents.
- ZERO extents are ordered correctly in each inode extent map.
- ZERO extents do not appear in refcount trees.
- ZERO extents do not appear in the dedupe index.
- ZERO extents do not appear in physical allocation maps.
```

Safe repair actions:

```text
- merge adjacent ZERO extents;
- remove impossible physical references attached to ZERO extents if the extent kind is unambiguously Zero and the physical reference is only stale secondary metadata;
- rebuild secondary indexes while ignoring ZERO extents;
- recreate implicit holes from gaps in valid extent maps.
```

Unsafe repair actions requiring explicit aggressive recovery mode:

```text
- converting damaged Data extents to ZERO extents;
- guessing that a missing physical extent was intended to be zero;
- dropping non-zero data because it resembles sparse metadata.
```

`arxfs rescue` must be able to reconstruct sparse files from inode extent maps even when physical allocation metadata is partially damaged, because ZERO extents do not require data-block recovery.

## 14. Space Accounting

ARXFS must account logical size and allocated physical size separately.

For a sparse file:

```text
logical_size: includes ZERO extents and holes
allocated_size: excludes ZERO extents and holes, except for metadata overhead
```

User-visible file size reports the logical size.

User-visible allocated-block reports must not count nonexistent data blocks for ZERO extents.

Metadata space consumed by the inode and extent records may be counted as filesystem metadata, not file data payload.

## 15. API Behaviour

ARXFS should expose sparse behaviour through normal file operations.

Required behaviours:

```text
read from hole -> zero bytes
write all-zero range -> sparse ZERO extent
truncate larger -> hole
truncate smaller -> remove extents beyond EOF
copy sparse file within ARXFS -> preserve sparseness where possible
clone/reflink sparse file -> preserve ZERO extents exactly
snapshot sparse file -> preserve ZERO extents exactly
```

Two behaviours are conditional on interfaces TAIRiX does not yet expose, and
are the whole of this appendix's remaining work (§19):

If TAIRiX exposes `SEEK_DATA` / `SEEK_HOLE`-style behaviour, ARXFS must report ZERO extents and implicit holes as holes, not as data.

If TAIRiX exposes an explicit punch-hole or zero-range API, ARXFS must implement it by creating ZERO extents and releasing replaced Data extents through the normal COW/refcount/free path.

## 16. Performance Requirements

Zero detection must be cheap and bounded.

The implementation should use a simple first-party all-zero scan over the write buffer. It must not allocate large temporary buffers, call the compressor to decide whether data is zero, or depend on external libraries.

For large writes, zero detection may operate per ARXFS record so that mixed data and zero regions are represented efficiently.

Sparse reads must synthesize zeroes without disk I/O for the sparse range.

Sparse writes must avoid unnecessary physical allocation.

## 17. Mandatory Tests

All of these pass, in `drivers/filesystem/arxfs/src/tests.rs` (the
`sparse_*` set plus `all_zero_cluster_write_becomes_holes_not_a_compressed_extent`).
Test 10 is inherent rather than separate: every ARXFS volume is encrypted, so
every other case already runs on an encrypted volume.

```text
1. create 10 MiB zero file
   - logical size is 10 MiB
   - allocated data payload is 0 bytes or metadata-only minimum
   - reading the whole file returns zeroes

2. write non-zero data into middle of sparse file
   - surrounding regions remain holes/ZERO extents
   - middle region reads back correctly
   - extent map is ordered and non-overlapping

3. overwrite existing data with zeroes
   - logical reads return zeroes
   - old data chunks are released only when COW/refcount rules allow
   - snapshots still see old data

4. truncate up
   - new range is a hole
   - reads from new range return zeroes

5. truncate down
   - removed Data extents are freed through normal path
   - removed ZERO extents require no physical free

6. clone/reflink sparse file
   - ZERO extents remain metadata-only
   - no dedupe chunks are created for zero ranges

7. scrub sparse file
   - metadata validates
   - no physical reads are attempted for ZERO extents

8. check sparse file
   - valid sparse metadata passes
   - malformed ZERO extents with physical addresses are rejected or repaired safely

9. compression bypass
   - all-zero range does not create zstd payload
   - repeated non-zero range follows normal zstd/RAW path

10. encrypted volume sparse file
    - sparse reads return zeroes
    - no plaintext data payload exists for sparse ranges
    - metadata remains protected by ARXFS metadata rules
```

## 18. Acceptance Criteria

Every criterion below is met:

```text
- all-zero logical ranges use ZERO/Hole extents;
- 10 MiB of zeroes does not allocate a 10 MiB data payload;
- ZERO extents bypass dedupe, zstd, encryption, data checksums, physical allocation, and TRIM;
- reads from ZERO extents and holes return zeroes;
- writes into holes allocate normal data only for written non-zero ranges;
- snapshots, reflinks, retained roots, and COW recovery remain correct;
- arxfs check validates sparse metadata and can rebuild secondary indexes around it;
- no RLE/FILL storage mode is introduced by this appendix;
- the implementation uses first-party Rust only and adds no external dependency.
```
## 19. Remaining work

Both items need a filesystem operation TAIRiX does not expose yet. That
interface is TAIRiX's own and `abi-v1` is unfrozen, so "the interface does not
exist" is work to do, not a blocker: it is added in place, with its first
consumer, as item **S1** of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md` §4. Neither
item is optional once the operation lands (§15).

- **`SEEK_DATA` / `SEEK_HOLE`.** No TAIRiX seek ABI exposes a hole-aware seek,
  so ARXFS has nothing to answer. When one lands, the driver reports an
  unmapped range as a hole from the extent tree it already walks; the file
  offset arithmetic is the only new code.
- **Punch-hole / zero-range.** No explicit punch-hole or zero-range operation
  exists in the filesystem ABI. When one lands, ARXFS implements it by dropping
  the covering mappings and releasing the replaced data extents through the
  normal copy-on-write refcount/free path — the same code `store_block` already
  runs for an all-zero write, so the operation is a bound and a range walk, not
  a new pipeline.

An explicit `Zero` extent record is **not** remaining work: §2 permits the
implicit representation, ARXFS implements it, and adding the field would be an
on-disk change buying nothing.
