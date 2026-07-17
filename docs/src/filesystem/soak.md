# Filesystem soak

The filesystem soak stress-tests the three first-party filesystems —
`arxfs`, `ext4`, and `fat32` — entirely in RAM, with no real disk and
no `mkfs` shell-out (`AGENTS.md` §12 / §2.12). It lives in the
`rustos-test-fs-soak` crate (`tests/integration/fs_soak`) and is driven
by `cargo xtask fssoak`.

## What it exercises

Each filesystem is formatted with its own first-party formatter
(`ARXFS::format`, `Ext4::format`, `Fat32::format`) onto a `RamBlock` —
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

## The randomized `arxfs-random` target

Alongside the fixed-sequence exerciser, a fourth target —
`arxfs-random` — drives arxfs through a **randomized, model-checked**
body (`random_exercise`). A filesystem is a critical system, so it must
be known to work for *any* operation order, not just one scripted path;
this target exercises the filesystem **in a different manner on every
launch**:

- Each step the RNG picks one of create-file, create-dir, write, append,
  extend (a write past the end, leaving a zero-filled gap),
  truncate-grow, truncate-shrink, remove-file, remove-empty-directory,
  logical move (copy the bytes to a fresh name and unlink the source —
  there is no native rename in `abi-v1`), or read-verify, across a tree
  of nested directories.
- Every mutation is mirrored into a byte-exact **oracle model** (each
  path's exact expected bytes, plus the live directory set). The random
  data written *is* the validatable content — the model records exactly
  what every file must read back as — and the filesystem's result is
  asserted against the model after each step.
- Periodically (and once at the end) the body flushes, remounts, and
  re-verifies the **whole** volume: every file's size and bytes and
  every directory's listing must match the model after a fresh `open()`.
- Fail-closed negative probes (`Busy`, `LengthOutOfRange`, `NotFound`)
  are interleaved and must not mutate state (§5.4 / §2.9).

The target's **start** seed is drawn from platform entropy (wall-clock
time mixed with the process id), so each launch takes a new path. Set
`RUSTOS_FSSOAK_SEED` to pin the start seed and replay a failure exactly;
the start seed is printed at launch and every error is tagged with the
reproducing seed.

## The `NoSpace` signal

A genuinely full volume is reported as `DriverError::NoSpace` (POSIX
`ENOSPC`), distinct from `DriverError::DeviceFault`'s unrecoverable
hardware error. Each driver's block/inode/cluster allocator returns
`NoSpace` on exhaustion; the soak asserts the OS reaches that state
cleanly rather than papering over a driver gap.

## Running it

```
cargo xtask fssoak --list             # the registry (arxfs/ext4/fat32/arxfs-random)
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

The nightly soak runs every target **in parallel**, one job and one log
each, through `tools/ci/soak.sh`'s `fssoak` kind (also part of `all`),
sharing the soak's wall-clock budget alongside the fuzz, proptest, and
repeated-test jobs. The registry is the single source of truth —
`soak.sh` enumerates `cargo xtask fssoak --list` and never hard-codes
the filesystem list, so `arxfs-random` runs concurrently with the
fixed-sequence `arxfs`/`ext4`/`fat32` jobs. Each ≥ 1 GiB volume in
flight needs its own GiB of runner RAM.

## Block-size note

The `RamBlock` uses 4096-byte sectors. On the soak's ≥ 256 MiB devices
ext4 therefore formats with 4096-byte blocks, so a single file (laid
down with ext4's classic block map) reaches ~4 MiB; the soak's per-file
fill size stays well under every driver's single-file limit so a fill
ends at a genuinely full volume, not a per-file map limit.
