# tairix-fsprobe

Filesystem signature, label, and identity probe — the one `no_std`
definition of how the head of a block extent is recognised as a supported
filesystem (ARXFS / ext4 / FAT32), which stable 16-byte identity it
carries, and how that identity renders as a short display fingerprint
(`plans/ALIAS.md` §3.8). The volume-manager policy driver
(`drivers/storage/volmgr`) probes with it, and the filesystem drivers
import their shared constants and derivations from it (the ARXFS
metadata-header magic, the ext superblock magic, the FAT32
serial+label+tag identity), so the probe and the mounted driver can never
disagree about what identifies a volume.

## What it provides

- `probe(head)` — recognise a supported filesystem from an extent's
  leading `PROBE_HEAD_LEN` bytes, in a fixed, documented order (ARXFS,
  ext4, FAT32 — most-specific signature first). The bytes are untrusted
  removable-media content: every access is bounds-checked, every field
  is sanity-validated, and no match means `None`, never a guess.
- `ProbedVolume` — the matched filesystem type
  (`tairix_abi::volume::VolumeFsType`), the volume's stable identity
  exactly as the matching filesystem driver publishes it, and its
  recorded label (trimmed; empty where the format records none).
- `fat32_identity_from_boot` — the FAT32 content-derived identity
  (serial + label + tag; FAT32 has no UUID), shared with
  `drivers/filesystem/fat32`.
- `fingerprint(identity)` — the lowercase Crockford-base32 display
  fingerprint of an identity; callers take the prefix they need
  (catalog collision suffixes, alias identity guards).

## Stability

`experimental`. The probed on-disk formats are external and fixed; the
Rust API may still change while TAIRiX is pre-release.
