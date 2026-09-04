# `tairix-util`

The strictly justified shared-utility crate: every item here is used by
**two or more** independent crates (`AGENTS.md` §2.3, §3), and promoting
code into it requires a `PLAN.md` note naming those callers. `no_std`
(the C-locale engines use `alloc`; `fmt` and `size` are allocation-free)
and panic-free throughout.

## Members

* `conf` — the `#`-comment line grammar every line-oriented
  configuration store in the tree shares: `strip_comment` returns the
  portion of a line before its first `#`, leaving the surrounding
  whitespace for the caller to trim, so a blank result is a line
  carrying no setting. The boot-time system configuration
  (`lib/sysconfig`), the network configuration (`lib/netconfig`), and
  `userland/system/init`'s service registry and startup list all read
  that one definition, so a change to how a comment is recognised can
  never apply to some stores and not others. No store's keys or values
  may contain `#` — each store's own validators enforce that, which is
  what makes cutting at the first `#` unambiguous.
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
* `fallible` — reserving a buffer whose size comes from the data before
  filling it, so a request the machine refuses is a typed refusal rather
  than an allocation abort. Userland's heap answers exhaustion with a
  null pointer, which `alloc` turns into a process abort, and a buffer
  sized by image geometry or a decoded payload — a window's surface is
  close to a megabyte — is exactly what a machine short of memory
  refuses. `filled` and `collected` reserve exactly what a one-shot
  buffer needs; `grow_to` reserves amortised for a scratch grown across
  uses, so repeated growth is not quadratic; `reserve` serves a buffer
  filled by pushing. The rasteriser's surfaces and resample plans
  (`lib/raster`), the compositor's scan-out frame and layer buffers
  (`userland/gui/wm`), and the PNG decoder's row and output buffers
  (`lib/image`) all reserve through this one definition, so a desktop
  under memory pressure degrades rather than dying in it.
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
* `count` — the GNU count grammar for the `-c`/`-n` values shared by the
  `head` and `tail` command apps (`plans/APPS.md`): `parse_decimal`
  reads a plain digit run and `parse_suffixed` a digit run with the GNU
  multiplier alphabet (`b` = 512; `k`/`K`/`M`/`G`/… as powers of 1024, or
  of 1000 with a trailing `B`). A malformed spelling or unknown suffix is
  rejected as `None`; an in-grammar count beyond any possible input
  saturates at `u64::MAX` rather than wrapping, since a count larger than
  the input is served exactly by "all of it". The tool-specific sign
  handling (`head`'s leading `-`, `tail`'s `+`) stays in each tool.
* `secret` — the one definition of "the secret is gone": `wipe` overwrites a
  byte slice through volatile writes and fences afterwards, and `Wiped<N>`
  is a fixed-size buffer that wipes itself at the end of its scope. A plain
  `fill(0)` before the bytes are freed or reused is a dead store the
  optimiser may delete outright, so every credential buffer in the tree —
  the `lib/rt` elevation client, the shell's `elevate` builtin, the login
  supervisor's elevation broker, and the masked text field in
  `lib/controls` — erases through this one implementation rather than its
  own.
* `defer` — the one way an interactive surface hands a piece of slow work to
  a worker: `JobDesk<Req, Ans>` holds one request waiting, one in flight, and
  one answer landed, and nothing about it blocks, locks, or performs I/O (the
  embedder supplies the exclusion and the parking). Two properties are why it
  is not a queue. **Latest-wins**: a submission made while a job is in flight
  replaces any earlier waiting one, so an interaction that settles repeatedly
  costs at most one further job — a queue would make the surface's own
  responsiveness the thing that generated the backlog. **At most one in
  flight**: two concurrent writes to the same store would race for what it ends
  up saying, so a job is handed out only once the previous one has been
  answered. An answer a newer submission superseded is dropped rather than
  delivered, and what a submission *displaced* is handed back, so a caller
  waiting on the displaced request can be told it was superseded instead of
  left waiting for an answer that will never come. Consumed by the terminal's
  and the desktop session's settings publishers, the session's
  program-catalogue scan, and the file manager's bundle scan.
* `tailwindow` — the bounded rolling "keep the last N bytes/lines"
  windows shared by the same two apps: `ByteWindow` and `LineWindow`
  retain only the trailing N units of a stream, so `head`'s `-c -N` /
  `-n -N` elide modes and `tail`'s `-c N` / `-n N` last-N modes are two
  policies over one mechanism whose memory cost is N, never the input
  size — a 100 TB+ file is a constant-memory read.

## How to grow the crate

Promoting code into `lib/util` requires a `PLAN.md` note documenting the
two-or-more concrete callers, an entry in the member list above, and
unit tests next to the new item per `AGENTS.md` §7.
