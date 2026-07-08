# `rustos-util`

Stability tier: **experimental**.

The strictly justified shared-utility crate (`AGENTS.md` §3): every item
is used by two or more independent crates, and promoting code into it
requires a `PLAN.md` note naming those callers. `no_std`,
allocation-free, and panic-free throughout; every member carries its
unit tests next to the code.

## Members

* `fmt` — no-allocation numeric formatters for structured-log field
  values. Consumers: `kernel/sec`, `kernel/ipc`.
* `size` — the GNU coreutils size vocabulary: `-B`/`--block-size`
  parsing (`SizeScale`), ceiling block scaling (`blocks_ceil`), and the
  `human_ceiling` renderings (`format_human`, powers of 1024 or 1000).
  Consumers: the `du` and `df` command apps (`plans/APPS.md`).

Long-form documentation: `docs/src/lib/util.md`.
