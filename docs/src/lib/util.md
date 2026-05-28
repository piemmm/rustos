# `rustos-util`

Reserved destination for helpers used by **two or more** independent
crates (`AGENTS.md` §2.3).

## State at Stage 1

The crate is empty. Stage 1 deliberately resisted the temptation to
populate it with speculative helpers; nothing currently shared between
two callers needs a home outside the crate it already lives in:

* The 256-bit bitset that Stage 1 introduced has two future callers
  (`rustos-caps` already; `kernel/sched` planned in Stage 2) so it
  lives in `rustos-collections`, not here.
* The endianness helpers in `lib/abi` are used only by ABI decoders and
  so stay local to that crate.

## How to grow the crate

Promoting code into `lib/util` requires a `PLAN.md` note documenting the
two-or-more concrete callers, a one-paragraph rationale in this page,
and unit tests next to the new item per `AGENTS.md` §7.
