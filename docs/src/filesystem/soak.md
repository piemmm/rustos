# Filesystem soak

The filesystem soak stress-tests the three first-party filesystems —
`rustfs`, `ext4`, and `fat32` — entirely in RAM, with no real disk and
no `mkfs` shell-out (`AGENTS.md` §12 / §2.12). It lives in the
`rustos-test-fs-soak` crate (`tests/integration/fs_soak`) and is driven
by `cargo xtask fssoak`.

## What it exercises

Each filesystem is formatted with its own first-party formatter
(`RustFs::format`, `Ext4::format`, `Fat32::format`) onto a `RamBlock` —
a `Vec`-backed `Block` device of at least 1 GiB — and then driven
through the frozen `FilesystemRead` + `FilesystemWrite` ABI by **one**
filesystem-agnostic exerciser (`AGENTS.md` §2.2). Each deterministic
iteration:

- **Integrity / consistency.** Creates files in the root and a nested
  directory, writes deterministic content, reads it back and verifies
  it, checks sizes and the directory listing, truncates a file and
  re-checks its prefix, and removes a file.
- **Remount.** Re-opens the same backing device and re-verifies every
  survivor (and that the removed file stays gone), so committed state
  must survive a fresh `open()`.
- **Extremes (fail closed, §5.4 / §2.9).** Fills the data region until
  allocation reports `NoSpace` (then frees a file and confirms
  allocation resumes — `NoSpace` is not terminal), and asserts a
  duplicate create and a non-empty `rmdir` report `Busy` while an empty
  or oversize name reports `LengthOutOfRange`. Never a panic, never
  silent corruption.

The exerciser is deterministic: a per-iteration seed drives the content
and a SplitMix64-style advance, so any failure reproduces from the seed
printed in the error.

## The `NoSpace` signal

A genuinely full volume is reported as `DriverError::NoSpace` (POSIX
`ENOSPC`), distinct from `DriverError::DeviceFault`'s unrecoverable
hardware error. Each driver's block/inode/cluster allocator returns
`NoSpace` on exhaustion; the soak asserts the OS reaches that state
cleanly rather than papering over a driver gap.

## Running it

```
cargo xtask fssoak --list             # the registry (rustfs/ext4/fat32)
cargo xtask fssoak --quick            # per-PR / smoke budget, ≥ 5 s each
cargo xtask fssoak --soak             # nightly budget, ≥ 24 h each
cargo xtask fssoak --target ext4 --secs 30
```

`cargo xtask fssoak` exports two env seams the integration tests read:
`RUSTOS_FSSOAK_BUDGET_SECS` (loop each filesystem until the budget
elapses) and `RUSTOS_FSSOAK_BYTES` (the device size, ≥ 1 GiB for the
soak). A plain `cargo test -p rustos-test-fs-soak` leaves both unset and
runs a single smoke iteration on a 320 MiB device (above FAT32's
~256 MiB floor; ext4 still gets two block groups).

## Parallelism

The nightly soak runs the three filesystems **in parallel**, one job
and one log each, through `tools/ci/soak.sh`'s `fssoak` kind (also part
of `all`), sharing the soak's wall-clock budget alongside the fuzz,
proptest, and repeated-test jobs. The registry is the single source of
truth — `soak.sh` enumerates `cargo xtask fssoak --list` and never
hard-codes the filesystem list. Three ≥ 1 GiB volumes in parallel need
≥ 3 GiB of runner RAM.

## Block-size note

The `RamBlock` uses 4096-byte sectors. On the soak's ≥ 256 MiB devices
ext4 therefore formats with 4096-byte blocks, so a single file (laid
down with ext4's classic block map) reaches ~4 MiB; the soak's per-file
fill size stays well under every driver's single-file limit so a fill
ends at a genuinely full volume, not a per-file map limit.
