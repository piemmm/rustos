# tairix-partition

Shared, scheme-neutral partition-table model and a bounds-checked
partition-window `Block` adapter — the one definition the image author
(`tools/mkimage`) and the kernel boot reader (`kernel/tairix-kernel`)
share, so the bytes one writes are always the bytes the other reads back
(`AGENTS.md` §2.2).

A flashed TAIRiX disk is **not** one scheme on one board: a Raspberry Pi
image is MBR, a UEFI x86_64 disk is GPT, and TAIRiX reads either on any
architecture (`AGENTS.md` §17 — nothing here is board-specific). The
parser detects the scheme on the device and dispatches; every scheme is
validated fail-closed against an untrusted, possibly-hostile disk
(`AGENTS.md` §5.4 / §2.9 / §19.5).

## What it provides

- A scheme-neutral model in *device logical blocks* (64-bit LBAs, large
  enough for GPT): `Partition`, `PartitionType` (`FatBoot`,
  `ARXFSRoot`, `Other`), and an inline `PartitionTable`.
- `parse_partition_table(dev)` — detect MBR vs GPT and parse, returning
  the neutral table.
- `mbr` — the classic MBR scheme: `encode` (image author) + `parse`
  (boot reader), sharing one extent validator.
- `gpt` — the GUID Partition Table read path: a fail-closed,
  CRC32-validated header + entry-array parser (the write path lands with
  the UEFI image builder). Includes the first-party IEEE `crc32`
  (`AGENTS.md` §2.12).
- `PartitionBlock` — presents one partition's extent of an underlying
  `Block` device as a standalone, bounds-checked `Block`, so a
  filesystem driver mounts a partition without being able to address a
  block outside it (`AGENTS.md` §5.4 / §24.4). The window bounds that
  `Block` I/O, not its holder: whoever owns the adapter can still take
  the device back (`into_inner`) or borrow it (`device_mut`), which is
  how a RAID member's own metadata — reserved blocks below the data
  window — is read and written.

  A window is a range of the *same* hardware, so the device's own
  answers pass through rather than being replaced by the trait defaults:
  its I/O class, its health telemetry, its discard support, and what its
  backing can promise a background consumer. Nearly every filesystem is
  mounted on a partition, so a defaulted answer here would hide a failing
  drive's counters from the scrub scheduler, withhold every trim from the
  medium, and have a filesystem spend discretionary I/O on a composed
  device that is rebuilding. Discard is additionally reported
  only when the window's start block is **aligned** to the device's
  discard granularity: a caller that aligns to what it is told must
  produce a request the device accepts, so a misaligned window withdraws
  support rather than promising an alignment it cannot honour. Every
  translated operation — reads, writes, and discard alike — goes through
  the one containment check, so a window can never name a neighbouring
  partition's blocks.

## Trust and fail-closed behaviour

The on-disk table is untrusted input. A short or unsigned MBR sector, an
overlapping / out-of-range / sector-0 extent, a forged GPT signature, a
header or entry-array CRC mismatch, or an entries-LBA that escapes the
device are all rejected **whole** — no subset of a malformed table is
ever returned. A type byte / type GUID is a routing hint, never a trusted
identity: the filesystem a partition is handed to still validates its own
on-disk magic (`AGENTS.md` §18.6). The parsers have a fuzz target
(`tests/fuzz_partition.rs`, `AGENTS.md` §19.6).

## Stability

`experimental`. The on-disk MBR/GPT formats are external and fixed; the
Rust API may still change while TAIRiX is pre-release.
