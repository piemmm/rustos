# `rustos-test-fs-soak` — in-RAM filesystem soak harness

A host-side test crate that soak-tests the three first-party
filesystems (`arxfs`, `ext4`, `fat32`) entirely in RAM, with no real
disk and no `mkfs` shell-out (`AGENTS.md` §12 / §2.12,
`.junie/filesystems.md`).

## What it does

- **`RamBlock`** — a zeroed, `Vec`-backed `Block` device addressing
  4096-byte logical sectors. The whole image lives in memory.
- **`SoakFs`** — a trait binding each driver's first-party formatter and
  remount path (`format`/`open`/`into_block`) behind the frozen
  `FilesystemRead` + `FilesystemWrite` ABI. One implementation per
  driver; the exerciser never names a concrete filesystem.
- **`exercise`** — a single, filesystem-agnostic body driven once per
  `SoakFs` (`AGENTS.md` §2.2). Each deterministic iteration:
  - formats a fresh volume, then runs an **integrity round-trip** —
    create files in the root and a nested directory, write deterministic
    content, read it back and check it, verify sizes and the directory
    listing, truncate one file and re-check its prefix, remove one file;
  - **remounts** the same device and re-verifies every survivor (and
    that the removed file stays gone);
  - asserts the **fail-closed extremes** the OS must report cleanly
    (§5.4 / §2.9): a full data region → `NoSpace` (then frees a file and
    confirms allocation resumes), a duplicate create and a non-empty
    `rmdir` → `Busy`, and an empty or oversize name → `LengthOutOfRange`.
- **`random_exercise`** — a second, *randomized*, model-checked body
  registered as the **`arxfs-random`** target (it runs over arxfs).
  Where `exercise` replays one fixed operation sequence, this one draws
  every step from the run's seed, so it exercises the filesystem **in a
  different manner on every launch**:
  - each step the RNG picks one of create-file, create-dir, write,
    append, extend (a write past the end with a zero-filled gap),
    truncate-grow, truncate-shrink, remove-file, remove-empty-dir,
    logical move (copy bytes to a fresh name + unlink the source — there
    is no native rename in `abi-v1`), or read-verify, across a tree of
    nested directories;
  - every mutation is mirrored into a byte-exact **oracle model** (path →
    expected bytes, plus the live directory set), and the filesystem's
    result is asserted against it. The random data written *is* the
    validatable content: the model stores exactly what each file must
    read back as;
  - every `REMOUNT_EVERY` operations (and once at the end) it flushes,
    remounts, and re-verifies the **whole** volume — every file's size
    and bytes, and every directory's listing — proving nothing is broken
    across a fresh `open()`;
  - fail-closed negative probes are sprinkled in (duplicate create →
    `Busy`, empty / 256-byte name → `LengthOutOfRange`, missing target →
    `NotFound`, non-empty `rmdir` → `Busy`) and must not mutate state.
  The `arxfs-random` target's **start** seed is drawn from platform
  entropy (time + pid), so each launch takes a new path; set
  `RUSTOS_FSSOAK_SEED` to pin it and replay a failure (the start seed is
  printed, and every error is tagged with the reproducing seed).
- **`run_target` / `TARGETS`** — the closed registry and per-target
  runner. `TARGETS` is the single source of truth the
  `cargo xtask fssoak --list` registry and the `tools/ci/soak.sh`
  fan-out enumerate; neither hard-codes the filesystem list.

The fixed-sequence exerciser is deterministic: a per-iteration seed
drives the content and a SplitMix64-style advance, so any failure
reproduces from its seed (printed in the error).

## Running it

A plain `cargo test -p rustos-test-fs-soak` runs **one** smoke
iteration per filesystem on a 320 MiB device (above FAT32's ~256 MiB
floor; ext4 gets two block groups), which takes a couple of seconds.

The nightly soak runs it under a wall-clock budget on a full-size
volume via the orchestrator, which sets two env seams the integration
tests read:

- `RUSTOS_FSSOAK_BUDGET_SECS` — loop each target until the budget
  elapses (unset / `0` runs a single iteration);
- `RUSTOS_FSSOAK_BYTES` — device size in bytes (≥ 1 GiB for the soak;
  `MIN_DEVICE_BYTES`).

```
cargo xtask fssoak --quick            # per-PR gate, ≥ 5 s per filesystem
cargo xtask fssoak --soak             # nightly, ≥ 24 h per filesystem
cargo xtask fssoak --target ext4 --secs 30
```

The three filesystems run **in parallel** under `tools/ci/soak.sh`
(one job and one log each), sharing the soak's wall-clock budget.

## Stability tier

`experimental` — a test crate, not part of the shipped OS. It must not
weaken any driver's `no_std` posture; it only consumes their public
APIs.
