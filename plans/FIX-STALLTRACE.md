# FIX-STALLTRACE — Stack-traced interactive stalls

Status: **done.** The facility, its per-port frame publication, its arming
across every interactive surface, and its reference page
(`docs/src/architecture/latency-diagnostics.md`) have all landed. This file
records the design the implementation carries.

Binding under `AGENTS.md`. Read `plans/FIX-DESKTOP.md` and
`plans/FIX-DESKTOP-SPEEDUP.md` (the interactive-surface rules this gives a
runtime witness to), `plans/WATCHDOG.md` (the CPU-lockup watchdog whose
debug-gating and honest-provenance patterns this reuses), and
`plans/FIX-WILD.md` / `plans/FIX-PANICS.md` (the user-stack walk and
backtrace HAL it is built on).

## What it delivers

A thread declares the frame it owes the user; the kernel reports any span
that overruns, naming the syscall that spent the budget and the user call
stack that led there. Debug images only.

`plans/FIX-DESKTOP.md` had to find each of its ~18 freeze sites by reading
code; `plans/FIX-DESKTOP-SPEEDUP.md` measures what a frame *costs* but never
names the code that overran. This closes that gap: the charter's
interactive-surface rule now has a runtime witness rather than review alone.

## Binding decisions

1. **The kernel makes the capture, not the loop.** A loop cannot diagnose its
   own stall: by the time it observes that an iteration cost 300 ms, the
   stack that spent them has unwound, and a backtrace taken there names the
   detector. Only something that can read the thread's user registers
   mid-stall can answer, and that is the kernel.

2. **The span is a property of the wait primitive, not of the caller.** It
   opens when a thread returns from an event wait and closes at the next one,
   decided inside the `waitset_wait` handler from state it already holds — so
   a surface pays no per-frame syscall and cannot misreport its own span.
   `waitset_wait` is userland's only park primitive (`lib/rt`'s `park_ns` and
   `park_forever` both route through it), so the rule has one site. A
   memberless *finite* wait is a sleep rather than an event wait and
   deliberately leaves the span open, because a sleep on the frame path is one
   of the stalls this exists to catch. A blocking `ipc_call` parks through a
   different syscall and so is charged to the frame it delayed — which is what
   makes the dominant stall class visible.

3. **Detection is at the two syscall boundaries, and nowhere else.** On exit,
   a span that crosses its budget was carried over by the call just
   completed, and the frame taken at that call's entry names the blocking
   site. On entry, a span already over budget was spent running user code, and
   the frame just taken names it. Both captures are made while the thread is
   *inside* the kernel, which is what makes the walk sound: a thread executing
   user code has a stack moving under the reader, and a chain read from one is
   fiction.
   - **There is deliberately no timer sweep.** An earlier sketch had one, to
     report a stall promptly at the deadline. It bought nothing a boundary
     does not: the stack below a blocked thread's syscall entry is frozen for
     the whole block, so walking it at unblock time yields exactly the stall's
     stack. What a sweep *would* add is a user-stack walk in dispatcher
     context and a second report per pause.
   - **A thread that never returns from its syscall therefore reports
     nothing, by design.** That is a wedge rather than a pause, and three
     detectors already cover it: the CPU-lockup watchdog (a stalled core),
     the service manager's liveness watchdog (a service that stops
     answering, `plans/NEW-SERVICEMANAGER.md` SVC-8), and the desktop's own
     not-responding tracker (`userland/gui/session/src/vigil.rs`). A fourth
     would report one event three ways.

4. **The port publishes only what a frame-pointer walk consumes.** The Arch
   HAL user-entry observer (`kernel/arch/api/src/userentry.rs`, the inbound
   counterpart of `EnterUser`) carries `(cpu, pc, fp, fp_valid)` — three
   scalars, not a `UserRegisterFrame`: this fires on every kernel entry, so it
   must not build a register-file snapshot nobody reads. The walk's bounds
   come from the thread's own stack span, so the stack pointer is not among
   them. `fp_valid` is the port's honest verdict, so no consumer walks a chain
   from a register the port never saved.
   - The observer is consulted *before* the port reads its saved frame, so an
     image whose kernel installed none pays one relaxed load per entry.
   - `x86_64` is the only port whose `syscall` stub saves no GPR block. It
     hands `%rbp` (untouched by the stub) and the saved user `%rip` (already
     the fourth System V argument register) to its trampoline in argument
     registers, so no push is added and the stub's frame size — hence the
     documented "rsp ≡ 0 (mod 16) at `call`" padding — is unchanged. Reading
     `%rbp` inside the trampoline would be too late: System V makes it
     callee-saved, so the Rust prologue overwrites it.
   - `wasm32` has no such trap and publishes nothing; its reports carry
     `sampled=none`.

5. **The syscall path reads a per-CPU publication, never the map.** The
   authoritative `BTreeMap` of watches sits behind an `RwLock` whose read
   acquires by compare-exchange on one shared word, so consulting it per
   syscall would put a contended write on a single cache line in front of
   every syscall on every CPU as soon as any surface armed a budget — a
   machine-wide serialisation point in the one image a developer profiles in,
   introduced by the facility meant to measure responsiveness. So the running
   thread's watch is published into `CpuState` at each user switch-in and
   cleared on the way out (beside the existing `live_space` publication), and
   the map is read once per *switch* instead.
   - The publication carries the **thread id** it belongs to and every access
     checks it: a slot naming a different thread is no watch at all, so a
     mis-sequenced publication can never attribute one thread's span to
     another (fail closed — no report rather than a wrong one).
   - It holds an `Arc`, not a borrowed pointer, because a sibling termination
     may `forget` a thread while it still runs on another CPU; the published
     clone keeps the watch alive until that CPU's next switch replaces it.
   - `arm` publishes to the calling CPU itself, since the arming thread is
     already running there and its switch-in published whatever it held
     before.

6. **Bookkeeping and reporting are separate.** `kernel/core/src/latency.rs`
   holds no identity, reads no address space, and emits no record — it answers
   "did this span overrun, and with what frame", so the whole state machine is
   host-testable without a kernel. The dispatcher, which already holds the
   capability table and the address-space registry, resolves the attested
   name and the PIE load base and walks the user stack from there.

7. **The report is one record on the diagnostic stream.** Code addresses are
   load-relative or **absent** — never absolute — so the record is an offline
   `addr2line` input and never an ASLR oracle, and it goes to the log/UART
   stream rather than the hash-chained audit trail, so no address lands on
   the tamper-evident log. It carries no register values (that stays the
   capability-gated crash record's business), no secret, and no capability
   token. One record per span (latched) plus a per-thread rate floor bounds
   what a surface cycling through overrunning frames can write.

8. **`latency_watch` needs no capability and answers rather than failing.**
   A thread describes only its own responsiveness obligation. The syscall is
   in the table unconditionally, so `SYSCALL_TABLE_HASH` is identical in both
   image profiles and `rxe` loading is unaffected; the *facility* is gated, and
   an image that compiles it out answers `0`. Zero is an answer, not an error,
   so no surface branches on the image it runs in.

9. **Which surfaces arm one: those that owe a frame *and* park on a
   wait-set.** The desktop session, the switchboard, the greeter, and the
   interactive apps (terminal, files, wallpaper, viewer, widgets, datetime).
   Non-interactive services (`init`, `timed`, `fontd`, `netstack`) do not:
   they owe no frame, so arming them would manufacture false reports. Nor do
   the tools whose blocking *is* their purpose — a network client, a load
   monitor — since every wait on a remote peer would be reported as a stall
   it did not cause.
   - **Nor a text surface that idles inside a blocking read.** The span
     closes on an *event wait*, so the text login — which waits for the user
     in `stream_read` — never closes its span and its correct behaviour reads
     as an overrun. Arming it produced exactly that, and the stall-trace
     vertical's own transcript caught it: `name=login elapsed_ms=5010
     blocked_in=stream_read` for a prompt doing its job. Widening the span
     rule to close on a blocking console read would trade that away against
     the real stall class it exists to catch — a *graphical* app reading a
     console mid-frame — so the facility models the event-driven shape and
     states the limit rather than blurring it.

## Bounds

`lib/abi/src/latency.rs`. All four are fixed validation or record bounds, not
capacities: `DEFAULT_FRAME_BUDGET_NS` (250 ms — a pause a user notices, far
past any real frame period), `MIN_FRAME_BUDGET_NS` (1 ms, so a userland-
supplied budget cannot turn every syscall into a log record),
`MIN_REPORT_INTERVAL_NS` (1 s), and `MAX_STALL_FRAMES` (16).

## What it also closed

- **The PIE load base was never recorded.** `AddressSpaceRegistry::set_load_base`
  existed but only tests ever called it, so every code address in a stall
  report — and in a `FIX-WILD` crash record — was omitted as unplaceable. The
  spawn path now records it: `tairix_kernel_mem::image_load_base` is the one
  definition of "the lowest relocated segment address", the three arch image
  builders carry it in `BuiltImage`, and admission records it per process.
  This was the prerequisite the facility could not be correct without, and
  the stall-trace vertical is what caught its absence — the first run
  reported a complete overrun with `pc`/`bt` missing.

- **The debug image was never linted.** `cargo xtask ci`'s clippy passes ran
  without `watchdog-diagnostics`, so the whole debug-image diagnostics path —
  the lockup detail, the kernel-activity breadcrumb, and now this — was code
  nothing ever linted. `tools/xtask`'s target-clippy stage gained a second
  kernel pass with that feature on, and the two pre-existing findings it
  surfaced were fixed.
- **The `+0x…` offset rendering existed twice.** Hoisted to
  `tairix_util::fmt::format_hex_offset` / `format_hex_offset_list`, so a
  kernel post-mortem and a user-space stall report cannot disagree about how
  a frame chain reads.
- **The audit catalogue's uniqueness test was not exhaustive.** Four variants
  had drifted out of it, so their ids were range-checked but never
  collision-checked; both cases now share one list.
