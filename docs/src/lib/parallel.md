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

A dispatch publishes the work, bumps an epoch, and wakes the workers parked on it;
every participant claims pieces off one counter until they run out; each worker
then decrements an outstanding count, and the last one to reach zero wakes the
dispatcher if it parked.

The dispatcher returns only once that count is zero, and that is the whole
lifetime argument: the published work is a reference to a value on the
dispatcher's own stack, so the dispatcher must not return while a worker could
still read it. The barrier is over **workers**, not over pieces, because a worker
between "saw the epoch" and "claimed a piece" has not yet acknowledged the
dispatch.

Because the barrier is over workers, every worker must be able to reach it.
`Pool::with_workers` therefore does not return until every worker has read its
starting epoch and counted itself in — otherwise a worker that had not run yet
would read the epoch already bumped, decide the dispatch was one it had seen, and
park without acknowledging, and the dispatch would never complete.

### Nothing spins

An idle worker is parked in `futex_wait` on the dispatch epoch; a dispatcher whose
workers are still running is parked in `futex_wait` on the outstanding count. An
idle pool costs the address space its workers' kernel-owned stacks reserve and no
CPU at all.

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
