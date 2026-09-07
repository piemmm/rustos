# `tairix-parallel` — bounded data-parallel work

`lib/parallel` is how a pass hands the machine's other cores work it has already
proved independent. It discovers nothing: independence is the caller's proof, and
the caller keeps it. The crate supplies the contract that proof is expressed
through, the one place an index becomes an element, the one split policy, and the
worker pool that runs the pieces.

## The contract, not the threads

A pass names `JobRunner` and never a pool:

```rust
pub unsafe trait JobRunner: Sync {
    fn width(&self) -> usize;
    fn run(&self, count: usize, job: &(dyn Fn(usize) + Sync));
}
```

`Serial` — run every job on the calling thread — is a complete implementation, so
a pass written against the trait works where there is no second core, no thread to
create (an in-kernel consumer), or no reason to hand work off. That is why
`lib/raster` and `userland/gui/wm` link this crate with `default-features = false`:
they express their passes' independent work through the trait and create no thread
themselves. Only the process that *owns* the pool — the desktop session — enables
the `pool` feature.

The trait is `unsafe` because `for_each` turns an index into an exclusive borrow.
An implementation must guarantee two things, and both are memory safety rather
than mere correctness:

1. Each index reaches `job` at most once, and never concurrently with itself.
2. `run` does not return until every invocation it made has returned.

An index a runner *skips* is different: that element is simply not visited, which
is a bug in the runner and not unsoundness. `for_each` re-checks the index against
the slice length, so a runner that hands out a bogus one leaves an element
unvisited rather than reaching outside the slice.

## Sizing

`bands(runner, units, grain)` is the one split policy: how many pieces `units`
units of work should become, given how few units are worth a hand-off. It answers
`1` whenever the runner is one thread wide or the work is smaller than one piece's
worth, and a caller then runs its plain loop with no atomics and no syscall — which
is why a pointer-motion repaint costs exactly what it did before a pool existed.

Above that the split is finer than the runner is wide. Pieces are claimed
dynamically, so the extra pieces cost one atomic increment each and buy back the
case that bites on a loaded machine: a core taken by another tenant leaves one
participant late, and with a piece each the whole pass waits for it. A pass whose
pieces carry a *per-piece* fixed cost asks for one piece per participant instead,
by passing its own share as the grain — the backdrop blur does, because each piece
primes its sliding window afresh.

## The pool

`Pool::for_cpus(online)` is the sizing policy: one participant per discovered
online CPU, of which the dispatching thread is one, so a single-CPU machine
creates no thread at all. The count is discovered through the System Information
API — never a constant — and a machine that reports one CPU, a caller that cannot
reach the service, and a process the kernel refuses a thread all end up composing
on the calling thread.

### The protocol

A dispatch publishes the work, opens its *claim* on `count` pieces, bumps an
epoch, and wakes the workers parked on it; every participant — the workers and
the dispatching thread — draws pieces off the claim until they run out. A
worker's draw and its hold on the dispatch are the **same** atomic: it becomes a
holder by taking a piece, and stops being one when it runs out of them. The
dispatcher returns once the pieces are exhausted and no holder is left.

That single word is the whole lifetime argument. The published work is a
reference to a value on the dispatcher's own stack, so the dispatcher must not
return while a worker could still read it. Because the pieces left and the
holders sit in one word, a worker reads the pointer only after a draw that took
a piece *and* incremented the holders in one compare-exchange, so the dispatcher
can never observe "no pieces left and no holders" while a worker is still
reading. Two words cannot express that: deciding "is there a piece for me"
separately from "I am now reading this dispatch" leaves a window in between, so
a worker would have to register its hold first and discover only afterwards
whether any work was left.

A draw yields `remaining - 1`, so pieces run from the top down. The count has to
live in the same word as the hold — a second atomic holding it would let a
worker pair one dispatch's count with a later dispatch's word and draw an
out-of-range index, which is unsound rather than merely wrong — and `JobRunner`
contracts that the order pieces run in is not observable.

### A dispatch costs what its work costs

A hold taken *before* the work is known makes a dispatch's latency the time for
the scheduler to run a woken worker to completion, whether or not that worker
got any work. Where runnable threads outnumber cores that is a run-queue wait
rather than a work wait, and it is unbounded. It was measured twice on a
four-core Pi 4B: 429 ms of compositing when the barrier was over every worker
that existed, and 992 ms after it had been narrowed to the workers that
registered — because a worker is woken by every dispatch, so it does reach a
CPU, take its hold, and then risk preemption before releasing it. Both appeared
in the desktop's frame-budget reports as `blocked_in=futex_wait` with four
syscalls in the span.

Taking the hold with the piece removes the case outright. A worker that finds
the pieces exhausted touches nothing, holds nothing, and parks again, so the
dispatcher never waits for it however long it is descheduled; what remains
waited for is a piece genuinely in flight, whose result the dispatch needs
before it can return. No parallelism is given up, because a worker is refused
only when there is no piece left to give it. It also removes the need for any
construction-time rendezvous, and for a separate "closed" flag: a pool between
dispatches has no pieces to give, which is the same state as a drained one, so a
worker still on its way to its loop — or waking spuriously — can only find a
draw refused.

The dispatcher also wakes only as many workers as there are pieces besides its
own, since a worker beyond that could do nothing but wake, find the pieces gone
and park again.

### Nothing spins

An idle worker is parked in `futex_wait` on the dispatch epoch; a dispatcher with
pieces still in flight is parked in `futex_wait` on the claim word. An idle pool
costs the address space its workers' kernel-owned stacks reserve and no CPU at
all.

### It cannot deadlock

A dispatch that finds one already in flight — nested inside a piece of it, or
issued from another thread — runs its work on the calling thread. There is no
arrangement of callers that waits on the pool.

## Capacity

Worker count is derived from the discovered online CPU count and the per-dispatch
split from the runner's width and the caller's grain. There is no fixed ceiling on
pieces, work units, or dispatches, and a thread the kernel refuses degrades the
pool rather than failing it — `worker_count` reports what it got, so a caller that
cares can say so.

## Testing

The `test-util` feature exports `Reversed`, the runner every consumer proves
bit-identity against: it reports a width it does not have and runs its pieces
**backwards on the calling thread**, so a comparison against `SERIAL` is a proof
about how a pass *divides* rather than about thread timing. It lives here because
the `unsafe impl` belongs beside the trait whose obligations it discharges, and
because the compositor, the frost, and the window-frame codec were otherwise
each spelling the same eight lines. `Reversed::widest` reports the most pieces
any dispatch asked for, so a test can assert the work really was split rather
than assume it.

Host tests cover the split policy at and around its boundaries, `for_each`
visiting each element exactly once, that the order pieces run in cannot change the
result, the unvisited-element case a skipping runner produces, the no-worker
degradation, and nested dispatch. The host has no syscall trap, so a pool there has
no workers by construction; the concurrent protocol is exercised by the `parallel`
role of the `threads_qemu_{aarch64,riscv64,x86_64}` verticals, which runs a divided
pass through a real multi-worker pool and compares every round against the same
pass run on one thread.
