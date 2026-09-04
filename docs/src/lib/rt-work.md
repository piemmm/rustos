# `tairix-rt::work` — work an interactive loop must not do itself

An interactive surface owes the user a frame, so it performs no blocking I/O:
not in response to input, not while painting, and never because a control's
value changed. `tairix_rt::work` is the one arrangement that takes the work it
must not do — a store write, a service round trip, anything that waits.

It is the runtime half of `tairix_util::defer`: that crate supplies the
lock-free, I/O-free desk (`JobDesk`), and this module supplies the exclusion,
the parked worker thread, and the wake that reaches the loop's own wait-set.
There is exactly one of these; a surface never invents a second.

## The shape

```text
loop:   submit(job) ─────────────► desk ─────────────► worker: run(job)
loop:   … keeps drawing …                                       │
loop:   park on wait-set ◄──────── wake.nudge() ◄───────────────┘
loop:   collect() ──────────────► the answer
```

`Worker::new` takes the work as a plain `fn(&Req) -> Ans` rather than a closure
or a trait object, so the worker thread and the fall-back path below cannot be
given two that disagree, and nothing has to be boxed.

The desk is latest-wins and at most one job is ever in flight, so an
interaction that settles repeatedly costs one further job rather than one each,
and two answers can never race for what a store ends up saying.

## A machine that grants no worker is slower, never wrong

The kernel may refuse the wake pipe or the thread. `Worker::start` then reports
which — `NoWorker::Wake` or `NoWorker::Thread(errno)`, so the caller can state
the reason in its own words — and leaves the desk **stopped**. A stopped desk
makes every later `submit` carry the job out on the caller's own thread and
leave the answer where `collect` finds it, answering `true` so the caller knows
to collect at once.

That is the whole fall-back: there is one adopt path however the work was
carried out, so no caller needs a second one, and a machine with no threads is
exactly as correct as one with them.

`WorkerGuard` stops the worker when the scope holding it ends. The thread is
*detached* rather than joined: a worker mid-write of a slow store would
otherwise hold the teardown for as long as that store takes, and it leaves at
its next turn round its loop anyway.

## Reaching the loop: the wake is level-triggered

`Worker::wake()` hands out the `WorkerWake` whose `read_end()` the loop adds to
its wait-set. When that token fires the loop **must drain it** — a wait-set
stream member reports buffered bytes, not an edge, so an undrained wake reports
ready for ever and the park degrades into a spin.

For an app built on `tairix_window::WindowEvents`, that is what
`Parked::Interrupted` is for: the app's `EventSource::park` drains the wake and
answers `Interrupted`, the wait ends with `Ok(None)` instead of parking again,
and the loop collects. Answering `Parked::Served` for a worker wake would park
again on a source that is still ready.

## Consumers

* The terminal's settings publisher — a settled slider edit's store write.
* The wallpaper chooser's applier — the desktop session's *Apply* round trip,
  which the session answers only once it has written the store.
