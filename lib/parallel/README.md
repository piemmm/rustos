# tairix-parallel

Stability tier: **experimental**

How a pass hands the machine's other cores work it has already proved
independent. Nothing here discovers independence — that is the caller's proof,
and the caller keeps it.

## What it provides

- `JobRunner` — the contract a pass expresses its independent work through.
  `unsafe`, because `for_each` turns an index into an exclusive borrow and
  therefore depends on two memory-safety obligations: each index is run at most
  once and never concurrently with itself, and `run` does not return until every
  invocation it made has returned.
- `Serial` / `SERIAL` — the runner that runs every job on the calling thread. Not
  a stub: it is the correct runner wherever there is no second core, no thread to
  create (an in-kernel consumer), or no reason to hand work off. A pass written
  against `JobRunner` is complete with only this.
- `for_each` — the one place an index becomes `&mut items[i]`, and the crate's
  single `unsafe` block. A pass splits its output into disjoint pieces
  (`split_at_mut` / `chunks_mut`), hands the slice here, and gets each piece
  visited exactly once.
- `bands` — the one split policy: how many pieces `units` units of work should
  become, given how few units are worth a hand-off. Work below one piece's worth
  answers `1`, so a small repaint runs its plain loop with no atomics and no
  syscall.
- `Pool` (default feature `pool`) — the fork-join worker pool over `lib/rt`
  threads. `Pool::for_cpus(online)` is the sizing policy: one participant per
  discovered online CPU, of which the dispatching thread is one, so a single-CPU
  machine creates no thread at all.

## Design notes

- **Nothing spins.** An idle worker is parked in `futex_wait` on the dispatch
  epoch; a dispatcher whose workers are still running is parked in `futex_wait`
  on the outstanding count. An idle pool costs the address space its workers'
  stacks reserve and no CPU.
- **The dispatch lives on the dispatcher's stack.** It is published as an erased
  pointer, and the dispatcher returns only once *every* worker has acknowledged
  the dispatch — not merely once every piece has been claimed. A worker between
  "saw the epoch" and "claimed a piece" has not acknowledged yet, which is why
  the barrier is over workers rather than over pieces.
- **It cannot deadlock.** A dispatch that finds one already in flight — nested
  inside a piece of it, or issued from another thread — runs its work on the
  calling thread. There is no arrangement of callers that waits on the pool.
- **It degrades rather than fails.** A thread the kernel refuses (the `threads`
  rlimit, exhausted memory) is simply not created; the pool runs with what it
  got, and `worker_count` reports it so a caller can say so.
- **Oversubscription is deliberate.** Pieces are claimed dynamically and the
  split is finer than the runner is wide, because the case that bites on a loaded
  machine is a core taken by another tenant: with one piece each, the whole pass
  waits for the straggler; with several, the others absorb its share.

## Capacity

Worker count is derived from the discovered online CPU count, never a constant,
and the per-dispatch split is derived from the runner's width and the caller's
grain. There is no fixed ceiling on pieces, work units, or dispatches.

## Testing

`Reversed` (feature `test-util`) is the runner every consumer proves bit-identity
against: it reports a width it does not have and runs its pieces backwards on the
calling thread, so a comparison against `SERIAL` is a proof about how a pass
*divides* rather than about thread timing. It lives here because the `unsafe impl`
belongs beside the trait whose obligations it discharges, and because three
crates were otherwise spelling the same eight lines.

Host tests cover the pure half: the split policy at and around its boundaries,
`for_each` visiting each element exactly once, that the order pieces run in
cannot change the result, the unvisited-element case a skipping runner produces,
the no-worker degradation, and nested dispatch. The host has no syscall trap, so
a pool there has no workers by construction; the concurrent protocol is exercised
by the desktop's per-architecture QEMU verticals, which composite through a real
multi-worker pool.
