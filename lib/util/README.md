# `tairix-util`

Stability tier: **experimental**.

The strictly justified shared-utility crate (`AGENTS.md` §3): every item
is used by two or more independent crates, and promoting code into it
requires a `PLAN.md` note naming those callers. `no_std` (the C-locale
engines use `alloc`; `fmt` and `size` are allocation-free) and
panic-free throughout; every member carries its unit tests next to the
code.

## Members

* `conf` — the `#`-comment line grammar every line-oriented
  configuration store shares (`strip_comment`: a `#` opens a comment
  that runs to the end of the line). Consumers: `lib/sysconfig`,
  `lib/netconfig`, and `userland/system/init`'s service registry and
  startup list.
* `cfloat` — C-locale `printf(3)` floating-point rendering
  (`FloatDirective`: flags, width, precision, `efga`/`EFGA`
  conversions, rendered exactly as C prints a `double`). Consumers: the
  `seq` and `printf` command apps (`plans/APPS.md`).
* `cnum` — C-locale `strtod(3)` scanning (`scan_double`:
  longest-prefix `endptr` semantics, decimal and hexadecimal floats
  with exact one-step rounding, `inf`/`nan`). Consumers: the `seq` and
  `printf` command apps.
* `fmt` — no-allocation numeric formatters for structured-log field
  values. Consumers: `kernel/sec`, `kernel/ipc`.
* `size` — the GNU coreutils size vocabulary: `-B`/`--block-size`
  parsing (`SizeScale`), ceiling block scaling (`blocks_ceil`), and the
  `human_ceiling` renderings (`format_human`, powers of 1024 or 1000).
  Consumers: the `du` and `df` command apps (`plans/APPS.md`).
* `count` — the GNU count grammar for `-c`/`-n` values
  (`parse_decimal`, and `parse_suffixed` for the multiplier alphabet).
  Consumers: the `head` and `tail` command apps (`plans/APPS.md`).
* `tailwindow` — the bounded rolling "keep the last N bytes/lines"
  windows (`ByteWindow`, `LineWindow`), so a last-N view costs memory in
  N rather than in the input. Consumers: the `head` and `tail` command
  apps.

Long-form documentation: `docs/src/lib/util.md`.
