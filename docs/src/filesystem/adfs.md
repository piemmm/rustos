# ADFS driver

`drivers/filesystem/adfs` (`tairix-drv-fs-adfs`) is the read/write
interoperability driver for Acorn ADFS / RISC OS `FileCore` volumes. It
attaches a volume behind any `tairix_abi::driver::block::Block` device
and exposes it through the versioned `FilesystemRead`,
`FilesystemWrite`, `FilesystemTimestamps`, `FilesystemAttrs`, and
`FilesystemStats` traits.

## Formats

Every ADFS on-disc format is supported, for reading and writing:

- **Old map** (S, M, L, D floppies and old-map hard discs): the
  checksummed free-space map in sectors 0–1, with every object a single
  contiguous run of 256-byte sectors. S/M/L use the 1280-byte `Hugo`
  directory (47 entries); D uses the 2048-byte directory (77 entries).
- **New map** (E, F floppies and new-map hard discs): a multi-zone
  allocation map of variable-length fragments — an `idlen`-bit fragment
  id, zero bits, and a stop bit per fragment, with a per-zone free-space
  list, per-zone check bytes, and a map-wide cross-check. F-class and
  hard-disc volumes carry the checksummed boot block at `0xC00` with the
  embedded disc record; E-class volumes carry the record at the start of
  zone 0. Small objects may share a fragment through the share offset in
  the low byte of an indirect disc address.
- **Big directories** (E+, F+): the variable-length `SBPr`/`oven`
  format with a name heap and 255-byte names, growing in 2048-byte
  grains to the format's 4 MiB ceiling. The root's size lives in the
  disc record and both copies (map zone 0 and the boot block) are kept
  in step when it grows.

The variant is identified by probing (boot block, then bare disc
record, then old map), and every checksum is validated before the
volume is accepted; corruption anywhere refuses the volume or directory
rather than serving suspect data.

## RISC OS metadata

The RISC OS load/exec words are surfaced through the canonical
`acorn.*` attribute keys of the shared metadata registry
([metadata registry](./metadata-registry.md)): `acorn.loadaddr`,
`acorn.execaddr`, `acorn.attr`, and — for filetyped objects —
`acorn.filetype` and `acorn.datestamp`. The conversions live in the
`tairix_fsmeta` Acorn preset, so a copy to `ARXFS` and back reproduces
the native fields byte-for-byte. A typed object's 40-bit centisecond
stamp is reported (widened to `Time64`) as all four node timestamps;
ADFS stores exactly one instant, and an untyped object honestly reports
the epoch rather than a fabricated time.

ADFS has no per-inode owner, mode, ACL, or capability gate: the §5.3
security metadata lives in the VFS layer, and the driver makes no
permission decisions. The `FileCore` attribute bits (including `L`,
locked) are surfaced as data through `acorn.attr` for the policy layer.

## Allocation

Old-map growth extends a contiguous run in place where free space
follows it and relocates the object otherwise; the 82-entry free-area
table is a format bound and fails closed (`NoSpace`) when fragmentation
exhausts it — the format's own "compaction required" condition. New-map
growth absorbs free map bits after an object's last fragment, appends
same-id fragments later in scan order (lookup concatenates fragments in
scan order, so appending earlier would reorder data), and relocates as
the last resort; shrinking returns fragments to the per-zone free
lists. A shared fragment is freed only when a bounded directory-tree
walk proves no other live entry references it.

Fixed directories never resize (their 47/77-entry capacity is a format
bound); big directories grow in place and never relocate, keeping
directory node ids stable.

## Test surface

Per-variant unit tests cover round-trips (with remount), listings,
lookups, sparse writes, truncation, removal with space-reclaim
verification, renames including replacement and cycle refusal, forced
relocation, disc-full behaviour, name validation, big-directory growth,
and the metadata round-trips. A corruption suite damages every
checksummed structure in turn (old-map sums, directory check bytes and
sequence pairs, zone checks, the cross-check, the boot block, big-dir
headers and heaps, entry pointers) and asserts the driver fails closed.
`tests/fuzz_adfs_mount.rs` is the fuzz harness registered with
`cargo xtask fuzz`: seed-logged byte sweeps over valid images of each
map flavour plus PRNG mutations, with the invariant that `open` and the
read surface never panic.
