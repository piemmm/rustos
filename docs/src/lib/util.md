# `rustos-util`

The strictly justified shared-utility crate: every item here is used by
**two or more** independent crates (`AGENTS.md` §2.3, §3), and promoting
code into it requires a `PLAN.md` note naming those callers. `no_std`
(the C-locale engines use `alloc`; `fmt` and `size` are allocation-free)
and panic-free throughout.

## Members

* `cfloat` — C-locale `printf(3)` floating-point rendering shared by the
  `seq` and `printf` command apps (`plans/APPS.md`): one `FloatDirective`
  (the five printf flags, width, precision, `efga`/`EFGA` conversions)
  rendering an IEEE 754 `f64` exactly as C's `printf` renders a `double`
  in the C locale — rounding, padding, alternate forms, special values —
  so the tools that promise printf semantics share one definition. A
  consumer whose GNU counterpart computes in `long double` documents
  that divergence at its own surface.
* `cnum` — C-locale `strtod(3)` scanning shared by the same two apps:
  `scan_double` reads the longest leading subject sequence (whitespace,
  sign, decimal or hexadecimal float, `inf`/`infinity`,
  `nan`/`nan(n-char-seq)`) with C's `endptr` measure, composing hex
  floats with exact one-step nearest-even rounding down to the
  subnormals — so `seq` demands full-token consumption and `printf`
  diagnoses a partial conversion over the one grammar.
* `fmt` — no-allocation numeric formatters that render task / port /
  capability identifiers into `lib/log`'s structured field values
  without touching an allocator on the hot path. Promoted from
  `kernel/sec` once `kernel/ipc` became the second caller; both consume
  it today.
* `size` — the GNU coreutils size vocabulary shared by the `du` and
  `df` command apps (`plans/APPS.md`): the `-B`/`--block-size` grammar
  (`512`, `1K`, `1MiB`, `1GB`, `c`/`w`/`b` byte suffixes, and the
  `human-readable`/`si` rendering words, parsed fail-closed into a
  `SizeScale`), ceiling block scaling (`blocks_ceil` — a partially used
  block is a used block, so usage is never under-reported), and the
  GNU `human_ceiling` renderings (`format_human`: one decimal below ten
  units, an integer otherwise, re-tiering a rounded-up amount, in
  powers of 1024 or 1000). Values are `u128` internally so a 100 TB+
  volume's byte totals can never overflow (`AGENTS.md` §26.6).

## How to grow the crate

Promoting code into `lib/util` requires a `PLAN.md` note documenting the
two-or-more concrete callers, an entry in the member list above, and
unit tests next to the new item per `AGENTS.md` §7.
