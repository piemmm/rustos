# Interactive-surface latency diagnostics (frame budget + stall backtrace)

When the desktop pauses — a window drag that judders, a launcher that takes
half a second to open, a settings slider that sticks — the interesting
question is not *that* it paused but *what* paused it. This page describes
the facility that answers it: a thread declares the frame it owes the user,
and the kernel reports any span that overruns, naming the syscall that spent
the budget and the user call stack that led there.

It is the *responsiveness* sibling of
[user-fault kill diagnostics](./fault-diagnostics.md) and
[kernel panic diagnostics](./panic-diagnostics.md), and it is distinct from
the [CPU-lockup watchdog](./scheduler.md): that one watches a *core* for a
stalled scheduler, whereas this watches a *thread* against an obligation it
declared. Nothing here is fatal — the surface keeps running, and the report
is a developer aid.

It exists only in the non-shippable `debug` image. A shippable image
compiles the whole facility out.

## Why the kernel, and not the loop itself

A loop cannot diagnose its own stall. By the time it observes that an
iteration cost 300 ms, the stack that spent them has unwound: a backtrace
taken there names the detector. The capture has to happen while the stall is
still in progress, which means it has to be made by something that can see
the thread's user registers — and that is the kernel.

## Declaring a budget

An interactive surface calls `latency_watch` (syscall 119, no capability)
once before its event loop:

```rust
let _ = tairix_rt::latency_watch(tairix_abi::latency::DEFAULT_FRAME_BUDGET_NS);
```

It returns the budget actually armed: the value clamped up to
`MIN_FRAME_BUDGET_NS`, or `0` on an image that compiles the diagnostics out.
Zero is an answer, not a failure — a surface reads it back and carries on
rather than branching on the image it runs in. `BUDGET_DISARM` (`0`) disarms.

Declaring a budget grants nothing, changes no scheduling decision, and
reaches no other thread, which is why it needs no capability: a thread
describes only its own responsiveness obligation.

The surfaces that arm one are those that owe a user a frame *and* park on a
wait-set to wait for it: the desktop session, the switchboard, the greeter,
and the interactive apps (terminal, files, wallpaper, viewer, widgets,
datetime). Non-interactive services — `init`, `timed`, `fontd`, `netstack` —
deliberately do **not**, because they owe no frame and arming them would
manufacture false reports.

Two other classes are deliberately out of scope:

- **Tools whose blocking *is* their purpose.** A network client (`telnet`) or
  a load monitor waits on a remote peer or on the work it is watching, so
  every such wait would be reported as a stall it did not cause.
- **Text surfaces that idle inside a blocking read.** The span model closes
  on an *event wait*, so a surface that waits for the user by blocking in
  `stream_read` instead — the text login is the one in the tree — never
  closes its span, and the seconds it correctly spends waiting for someone
  to type read as an overrun. Arming the text login produced exactly that:
  `elapsed_ms=5010 blocked_in=stream_read`, for a prompt behaving perfectly.
  Widening the span rule to cover a blocking console read would fix that at
  the cost of the real stall class it exists to catch — a *graphical* app
  reading a console mid-frame — so the facility models the event-driven shape
  and says so.

The line is therefore "owes the user a frame and parks on a wait-set", not
"has an event loop".

## The span a budget applies to

A surface's obligation runs from one event to the next, so the kernel opens a
span when the thread returns from an event wait and closes it at the next
one. The surface pays no per-frame syscall and cannot misreport its own span.

`waitset_wait` is userland's only park primitive, so the rule lives in one
place:

| Wait | Closes the span? |
| --- | --- |
| An event set with members — the loop's own park | yes: the surface owes nothing until it returns |
| Memberless with no timeout — `park_forever`, a backgrounded session | yes: nothing can resume it |
| Memberless with a finite timeout — `park_ns` mid-frame | **no**: a sleep on the frame path is one of the stalls this exists to catch |

A blocking `ipc_call` parks internally through a *different* syscall, so it
never closes the span. That is what makes the dominant stall class visible: a
round trip to `fontd`, `confd`, or the display service is charged to the
frame it delayed.

## Where the overrun is noticed

At the two kernel boundaries the thread must cross for the stall to have
ended:

- **On syscall exit.** The span had not overrun at this call's entry and has
  now, so *this* call is what spent the budget. The frame taken at its entry
  names the blocking call site, and the thread's user stack has not moved
  since — it has been in the kernel throughout — so walking it now yields
  exactly the stall's stack.
- **On syscall entry.** The span overran while the thread was running user
  code. The frame just taken names the code that was executing.

Both captures happen while the thread is *inside* the kernel, which is what
makes the walk sound: a thread executing user code has a stack moving under
the reader, and a chain read from one is fiction. That is also why nothing
samples another thread — a stall is diagnosed by its own victim, at a point
where its stack is frozen.

**A thread that never returns from its syscall reports nothing, by design.**
That is a wedge rather than a pause, and it is already covered: the
CPU-lockup watchdog catches a stalled core, the service manager's liveness
watchdog catches a service that stops answering, and the desktop's own
not-responding detector catches an app that stops draining its events. A
fourth detector would report one event three ways.

## What a report contains

One `TaskLatencyOverrun` record (id `4150`) per span, emitted through the
**diagnostic** sink — the log/UART stream, never the hash-chained audit
trail, so no code address lands on the tamper-evident log:

```
[42.117] [WARN] id=4150 interactive surface overran its frame budget task=7
  name=desktop budget_ms=250 elapsed_ms=312 blocked_ms=304 calls=41
  sampled=blocking blocked_in=ipc_call blocked_in_ms=297
  pc=+0x0000000000004a1c bt=+0x0000000000001220,+0x00000000000008f0
```

| Field | Meaning |
| --- | --- |
| `task` | The stalling thread's scheduler id. |
| `name` | The program's kernel-attested basename. Never caller-supplied bytes. |
| `budget_ms` | The budget the surface declared. |
| `elapsed_ms` | How long the span had been open when the overrun was noticed. |
| `blocked_ms` | How much of `elapsed_ms` was spent inside syscalls. The remainder went to user-mode work, so the two together say whether this was a blocking stall or an unbounded-work one. |
| `calls` | Syscalls completed during the span, so many small round trips read differently from one long call. |
| `sampled` | What `pc`/`bt` name: `blocking` (the call that spent the budget), `running` (the user code that did), or `none` (the port publishes no frame). |
| `blocked_in` | The syscall that carried the span over, by name. Absent when the budget went to user-mode work. |
| `blocked_in_ms` | How long that call had been running when it crossed the budget. |
| `pc` | The stalling program counter. |
| `bt` | The user frame-pointer chain above it, newest first. |

Reading the example: 41 syscalls, 304 of 312 ms spent blocked, and the call
that crossed the line was a 297 ms `ipc_call` — a single slow round trip, not
death by a thousand cuts. Had `blocked_ms` been near zero with `calls=0`, the
same span would have been unbounded work on the loop instead.

### Addresses are load-relative, or absent

`pc` and every `bt` frame are offsets into the program's own PIE load base,
carrying the `+0x` marker that distinguishes them from absolute addresses.
When the load base is unknown the fields are **omitted entirely** rather than
emitted absolute, so the record is an offline `addr2line` input and never an
ASLR oracle. The record carries no register values, no secret, and no
capability token; a privileged debugger's full register dump remains the
[crash record](./fault-diagnostics.md)'s business.

Resolve a frame offline against the unstripped program binary:

```sh
addr2line -e Run -f -C -i 0x4a1c
```

### One report per pause

A span produces at most one record however many boundaries it crosses
afterwards, and a thread produces at most one per `MIN_REPORT_INTERVAL_NS`.
The two together mean a surface cycling through overrunning frames reports
the problem rather than flooding the log with it.

## Cost

Nothing is added to any hot path in a shippable image: the facility is behind
the `watchdog-diagnostics` feature that `tools/xtask` turns on for the
`debug` image alone, and each port checks whether an observer is installed
before reading its saved frame, so an unwired image pays one relaxed load per
kernel entry.

In a debug image, a syscall boundary reads the running thread's watch from
**this CPU's own slot**, never the authoritative map. That matters more than
it sounds: the map is behind an `RwLock` whose read acquires by
compare-exchange on one shared word, so consulting it per syscall would put a
contended write on a single cache line in front of every syscall on every CPU
the moment any surface armed a budget — a machine-wide serialisation point in
the one image a developer profiles in, added by the tool meant to measure
responsiveness.

The slot is published at each user switch-in and cleared on the way out, so
the map is read once per *switch* rather than once per syscall, and it carries
the thread id it belongs to: a slot naming a different thread is treated as no
watch, so a mis-sequenced publication cannot attribute one thread's span to
another. It holds an `Arc`, which is what keeps the watch alive if a sibling
termination forgets the thread while it is still running elsewhere.

What remains per boundary is a per-CPU slot read, and for a watched thread a
clock read and two uncontended locks on lines only that thread touches. The
user-stack walk happens only on the boundary that reports an overrun.

## Per-port support

The frame comes from the port's syscall entry, the only place the saved user
register state is in hand, and is reported through the Arch HAL's user-entry
observer:

| Target | Frame published | Notes |
| --- | --- | --- |
| `aarch64` | yes | The EL0 trampoline already saves `ELR_EL1` and `x29`. |
| `riscv64` | yes | The trap vector already saves `sepc` and `s0`. |
| `x86_64` | yes | The `syscall` stub saves no GPR block, so it hands `%rbp` and the saved user `%rip` to its trampoline in argument registers — no extra push, so the stub's frame and its alignment padding are unchanged. |
| `wasm32` | no | The host environment has no such trap. Reports carry `sampled=none`. |

A port that publishes no frame reports the span, the timings, and the
blocking call, and simply carries no backtrace — never a fabricated chain.

## Where the code is

- `lib/abi/src/latency.rs` — the budget and record bounds, and the
  `StallSample` provenance.
- `kernel/core/src/latency.rs` — the span state machine (host-tested; holds
  no identity and reads no address space).
- `kernel/core/src/syscalls.rs` — the two boundary hooks and the report
  renderer, which resolves the attested name and load base and walks the
  user stack through `crash::UserStackReader` and the one shared unwinder.
- `kernel/arch/api/src/userentry.rs` — the user-entry observer seam.
- `lib/rt/src/lib.rs` — the `latency_watch` wrapper.

The staged plan is `plans/FIX-STALLTRACE.md`; the interactive-surface rules
this enforces are `plans/FIX-DESKTOP.md` and
`plans/FIX-DESKTOP-SPEEDUP.md`.
