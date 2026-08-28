# OPEN-DEFECTS — Close the remaining open core-kernel defect classes

Status: **planned**

Binding under `AGENTS.md`. This plan is the single tracker for driving
the remaining open core-kernel defect classes to closure. It assumes
`plans/FIX-SYSCALL.md` is **substantially done** — syscalls run with
interrupts enabled on the bare-metal ports, the deferred drain runs on
the syscall return path, and the console-RX lock discipline (C1) is
closed — with only the residual per-arch validation verticals still
outstanding (D1 below). It supersedes no other plan; each defect keeps
its own detailed plan where one exists and this file is the umbrella.

Read first (§15.18): `plans/FIX-SYSCALL.md`, `plans/WATCHDOG.md`,
`plans/WIRING.md` (Arch HAL parity), `plans/ARCHSUPPORT.md`
(x86_64 product parity), `plans/CODEVERIFY.md` (the §27 sweep spirit),
`PLAN.md` immediate-work P-series (P-6 at ~line 2075).

## Scope

The open items, in priority order:

- **D1 — FIX-SYSCALL residual verticals** (x86_64/riscv64 syscall-body
  tests + metal re-confirmation). The design and code are done; the
  per-arch conformance verticals are not.
- **D2 — P-6: wait-queue §27 completeness rework — DONE.** The
  foundational primitive (`kernel/core/src/waitq.rs`) shipped as a thin
  slice; §27 required the complete primitive. Landed (three-index
  O(log n) `WaitSet` with a stated FIFO no-starvation discipline).
- **D3 — Hard-lockup watchdog parity** on x86_64 and riscv64 (aarch64
  is the only port with hard-lockup detection wired).
- **D4 — Latent §27 audit sweep — DONE.** The other foundational
  primitives (`lib/sync`, IPC/capability structures, allocators,
  `lib/collections`) were audited against §27. All are complete; `waitq`
  (D2) was the sole thin slice. One latent watch-item (the slab
  free-slot scan) is recorded and staged (not a live defect).
- **D51 — a byte-stream transfer staged the caller's whole declared length,
  not one ring — DONE.** `parked_stream_read` / `parked_stream_write` capped
  the staging buffer at `FS_IO_MAX` (1 MiB) while every backing they serve
  buffers exactly one `PIPE_CAPACITY` (64 KiB) ring, so a caller handing a
  whole payload to one call made the kernel allocate and zero a megabyte of
  heap, copy a megabyte across the user boundary, then discard all but 64 KiB
  of it. The parser-sandbox seam does exactly that (`send_frame` writes the
  entire payload in one `Channel::write`), so placing one 2.5 MB wallpaper
  master cost ~64 MiB of kernel-heap alloc/memset/copy in 1 MiB units, both
  directions — and `copy_in_user` restarts its copy from the buffer base
  after each demand-fault miss, so a large stage over first-touch user memory
  re-copied quadratically. Fixed by one shared `stream_stage_len` bound: both
  loops stage at most one ring and answer short, which the caller already
  loops on. Measured context: the decoder was never the cost — the 26 shipped
  8.3-megapixel masters decode in 404 ms total at thumbnail scale, ~90 ms
  each full-screen.
- **D61 — the stream write path registered for its wake *after* the poll that
  found the ring full — DONE.** `parked_stream_write` registered on the stream
  wait-queue inside the `Full` arm, so between the poll and the registration a
  peer that drained the ring woke only the tasks registered at that instant,
  and woke nobody. The writer then parked with `NO_DEADLINE` on space that had
  already freed, released only when unrelated pipe traffic happened to
  broadcast — a multi-second stall by construction, and an outright hang
  whenever the transfer was the machine's only pipe activity. The read path
  had the correct discipline and its own comment explaining it. Fixed by
  registering before the first poll and deregistering once the loop is left,
  matching the read path; the regression test observes the registration from
  inside the first step via `wake_waiter`'s registered/not answer.
- **D62 — the stream wait-queue was one global queue woken with `wake_all` —
  DONE.** Every 64 KiB chunk moved on *any* pipe or pty unparked every stream
  waiter on the machine, each of which re-polled its own unrelated backing and
  parked again, and each `wake_all` heap-allocated a `Vec` of the waiter ids.
  On a desktop with a sandbox worker per app plus shell ptys that is a
  double-figure thundering herd per chunk, so one app streaming a gallery
  taxed every other pipe user — a §2.16 / §27 defect, not a correctness one (a
  spurious wake is harmless). Closed by keying the waiters rather than
  splitting the queue: a `WaitQueue` registration is now `(WakeKey, TaskId)`
  key-major, so `wake_key` releases one condition's waiters as an O(log n +
  woken) range and `wake_all` stays what a genuine queue-wide broadcast uses.
  Each bounded ring mints its own `RingWaits` pair (bytes, space) — one pair
  per pipe, two per pty — and every park, transfer wake, last-end close, and
  `waitset_wait` `Stream` member names the one side it concerns. Keeping the
  single queue is what keeps the timed `sweep` / `earliest_deadline` /
  `nearest_timed_deadline` machinery unchanged: a timed `stream_read` needs no
  per-object queue for the sweep to enumerate. A `Stream` member resolves its
  ring once at wait entry, so the wait follows the object the descriptor held
  then; a sibling thread swapping that descriptor number mid-wait cannot be
  followed (the swap can always land between a re-resolution and the peer's
  write) and the readiness scan, which re-resolves the number, stops reporting
  the retired stream.
  Two further defects fell out of it. `PipeEnd`/pty-end `Drop` woke on *every*
  release, so a spawn's or a `stream_read` snapshot's clone/drop pair woke
  waiters for a condition that had not changed; only the last end of a side
  now wakes, and it wakes exactly the two conditions its departure retires.
  And `terminal_purge` on a pty flagged `console_wake` for the ring space its
  discard had just freed — the wrong queue entirely, so the parked writer was
  released only by unrelated pipe traffic and, once the broadcast was gone,
  never; `Pty::purge_session` now wakes both rings' space itself, where no
  caller can pick the wrong queue.
- **D66 — `DriverError::Busy` carried three unrelated meanings, and the generic
  mapping turned one of them into an I/O error — DONE.** Every distinguishable
  filesystem conflict now has its own driver value, so the VFS no longer
  disambiguates by which mapper the call site picked.
- **D54 — a desktop worker thread issues ~2500 file opens at session start,
  starving every concurrent reader — OPEN.** It is the measured whole of the
  read-throughput gap `plans/FIX-KHEAP.md` reported: bundle load rate tracks
  overlap with this burst, not bundle size. Desktop-side, not block-layer.
- **D50 — the flake hunt's concurrent replicas re-planted one guest's backing
  image underneath itself — DONE.** Up to four simultaneous runs of one
  enrolment shared a planted-image path, so a replica rewrote a live sibling's
  disk mid-run. Sidecar paths are now per-run, not per-binary.
- **D57 — the first tightening of memory stopped every cache in the system
  from admitting, and took the desktop's pictures with it — DONE.** Reported
  as "32 terminal windows and the icons all become the same white silhouette
  and the desktop stops responding". Reproduced on the aarch64 `virt` board at
  the default 256 MiB: windows 1–24 opened in a fifth of a second each, then
  25 took 9 s, 26 took 29 s, 27 took 65 s and 28 took 118 s, with ~1800
  `fs_open`/`fs_write` pairs and ~1500 font-endpoint calls per 6000 audit
  lines — every icon re-read and re-decoded and every glyph re-fetched, per
  repaint. Three defects, all in the shared reclaim model:
  - `GrowthAllowance::permits` refused **all** cache growth in any band above
    normal (below 20% free), contradicting `shrink_target`'s own per-class
    ceilings: a class the policy says to preserve could keep what it held but
    never admit again, so every cache in the system — kernel filesystem
    metadata, block, launch, transform — decayed to uselessness while its
    ledger read healthy. Admission now reads the same `shrink_target` a forced
    shrink evicts to, so growth and shrink are one policy.
  - The desktop's decoded icons and client-side glyphs were classed as
    drop-at-mild disposable UI, though rebuilding one needs a capability-gated
    read plus a parser-sandbox round trip (icons) or an IPC round trip
    (glyphs) — the resources a machine short of memory has least of. Both now
    declare a display-derived working-set floor that mild and moderate leave
    alone; severe and critical still take everything.
  - A retention refusal was re-attempted every round for ever.
    `ArtworkResolver::declined` reports it and the session's icon desk holds
    that key back until the band moves.
  The same reproduction now completes in 30 s with the bar drawing its real
  artwork throughout, and is enrolled as
  `tests/integration/desktop_pressure_qemu_aarch64` — the guest passing only
  when the published band really left normal, so it cannot pass without having
  tested the state it is named for. Adjacent and **not** fixed by this: D54,
  the session-start burst of file opens from the same worker.
- **D58 — three window counts stood in for the bytes a window actually costs
  — DONE.** Found while fixing D57. The session bounded one client to 32
  *windows* (`WINDOWS_PER_CLIENT_MAX`), a figure that says nothing about the
  address space it maps for them: 32 windows of a 4K frame is a gigabyte,
  while a hundred terminal windows are a few tens of megabytes — and a resize,
  which is the other way a client grows what the session holds, was not
  bounded at all. It is now a byte budget derived from the machine's RAM
  (`client_frame_budget_bytes`), charged by creates, popups, and resizes
  alike. The terminal (32) and the file manager (8) each carried a further
  hand-picked count in front of it; both are gone, because every resource they
  stood for — the frame region, the pty, the shell child — is already bounded
  by something derived and fail-closed that those apps already report.
- **D59 — the release that was meant to bound many-window memory freed
  nothing, and reached only one of a window's three copies — DONE.** Found by
  reading the release ladder while answering "can 32 windows of a 4K frame cost
  less than a gigabyte". Two halves:
  - The compositor released a *hidden* window's pixels and, in the same wake,
    asked its client to present again; the client did, the buffer was
    established afresh, and the bytes came straight back. At mild pressure —
    the band where only hidden windows are released — the ladder therefore
    freed nothing and cost one repaint per hidden window while the machine was
    short of memory. Every compositor test passed: they call the release
    directly and none models the session delivering the redraw it queued. The
    request is now made by `set_visible` when the window is next shown, which
    that path already did.
  - A window's pixels exist three times — the app's render target, the frame
    region, and the compositor's converted copy — and the pages behind the
    region go only when *both* sides unmap it. The session released only its
    own, so a hidden 4K window still cost ~64 MiB of the ~96 it had. It now
    unmaps its side (`WindowServer::release_frames`) and tells the client
    (`WindowEvent::ContentReleased`, `Errno::NotAttached` on a present against
    a released window), which lets go of its own two and re-attaches on the
    paint that follows the next redraw request.
  A third half was found later, by reading the ladder's trigger rather than its
  body: it ran **only** on the pressure band's wake, and that wake is
  edge-triggered. A user minimising a window on a machine whose pressure had
  already settled produced no edge, so the release never ran for the ordinary
  sequence — get tight, then put a window away — and the largest block the
  desktop can give back was freed only if the band happened to move again. The
  ladder's inputs are the band *and* each window's visibility, so it now runs
  on either edge: `Compositor::set_visible` applies the same per-window
  decision to the window it has just hidden, and the session drains the
  released notices on the input wake as well as the band's. A
  minimise-then-restore inside one wake withdraws its own undrained notice,
  since neither side has let go and telling the client would cost an unmap and
  a re-attach that change nothing.
  Two smaller defects fell out of that reading. The session recorded nothing
  when it handed a window's frames back, though every other reclaim decision on
  the machine is logged and this is the largest of them
  (`CONTENT_RELEASED`, naming the window and the bytes). And `WINDOW_SHOWN` —
  "a frame carrying this window's own pixels reached the display" — stayed
  latched across a release, so a window released and never re-presented still
  read as shown; `SessionWindows::content_released` puts the record back to
  awaited and the frame that brings the pixels back announces it afresh.
  Adjacent, deliberate, and **not** changed: the session keeps converting each
  present into its own copy rather than compositing from the client's buffer.
  That would halve a *visible* window's cost, but it moves the
  straight-alpha-to-premultiplied conversion from once per damaged pixel to
  once per composited pixel per frame and gives up the stable snapshot, which
  is a speed and integrity trade rather than a win; the zero-copy path for
  visible windows is the hardware layer scanout `plans/FIX-DISPLAY-ACCELERATION.md`
  stages.
- **D60 — the window-content release has no end-to-end vertical: the one
  claim only a live desktop can settle is untested — OPEN.** Stated when D59
  landed. Every seam of the release path is host-tested — the engine's
  release/re-attach and the byte budget dropping a released window to zero
  (`lib/window/src/tests.rs`), the compositor's deferral and released-notice
  queue (`userland/gui/wm/src/tests.rs`), the session's trim producing the
  notice (`userland/gui/session/src/windows.rs`), the `ContentReleased` wire
  round trip (`lib/abi/src/window_ipc.rs`) — but no QEMU run has ever taken a
  window through *release → client lets go → shown again → re-attach → its
  pixels back*.
  - **Why the existing vertical does not reach it.**
    `tests/integration/desktop_pressure_qemu_aarch64` gets the machine into a
    non-normal band with a screenful of terminal windows, but
    `release_content_under_pressure` only takes a window that is **not
    visible**, and visibility there is an explicit flag rather than occlusion:
    cascaded windows are all visible, however deeply stacked, so at mild and
    moderate pressure the ladder correctly releases nothing. Only critical
    pressure touches visible windows, and it spares the focused one.
  - **The groundwork is in, and it found the defect the coverage was missing.**
    Reading the release ladder's *trigger* while designing this vertical
    surfaced the third half of D59: the ladder ran only on the band's
    edge-triggered wake, so "minimise a window on an already-tight machine"
    released nothing. That is fixed and host-tested
    (`userland/gui/wm/src/tests.rs`, `userland/gui/session/src/windows.rs`), and
    with it the two markers a vertical needs now exist: the session's
    `CONTENT_RELEASED` record and a `WINDOW_SHOWN` that is re-earned after a
    release.
  - **Two things in the design above do not work; use these instead.**
    - `shm_unmap` is `audit: false` in the syscall table (an unprivileged
      release of the caller's own mapping, the same posture as `mem_unmap`), so
      the client's half of a release is *not* in the audit trail and cannot be
      the guest's witness. Turning auditing on for it to make a test observable
      would shape production for the test. Witness the **re-attach** instead —
      `sc=shm_create` + `sc=shm_grant` attributed by `comm` to the app under
      test, which `ProcName::from_path` sets to the bundle's stem — and take the
      release from the session's own `CONTENT_RELEASED` record on serial.
    - The window cannot be a *terminal* window restored from its icon-bar slot.
      The terminal declares a default action, so a primary click on its slot is
      relayed to the app and opens **another** window; only an application that
      declares none (`tairix_window::info_and_quit`) gets
      `TaskbarResponse::AppRaise`, which is what shows a minimised window
      again. Use such an app — `widgets` is the smallest: no filesystem, no
      arguments, a fixed 820×620 window, and it is listed in the program
      library.
  - **The shape that works.** Reach pressure the proven way (the existing
    screenful of terminal windows), launch `widgets`, then drive two full
    cycles: raise it, maximise it so its body covers screen the cascade cannot
    reach, minimise it, and restore it from its slot. Gate causally throughout —
    the reveal witness, then per-window `WINDOW_SHOWN` occurrences, then
    `CONTENT_RELEASED` — and photograph the frame after the *first* restore,
    holding the second cycle until the dump has been read back so the guest
    (whose PASS is the *third* frame region the app creates) outlives it. The
    assertion is that a strip of the work area only a maximised window can
    cover is not the wallpaper: a window that came back transparent would leave
    it wallpaper, and no cascaded terminal reaches it.
  - **Two prerequisites the script cannot reconstruct without.** The gallery's
    declared window extent (`WIN_WIDTH` / `WIN_HEIGHT` / its `WindowSizing`)
    is private to its `Run` binary, so a host reconstruction cannot read the
    one definition and would have to restate it; hoist those into the crate's
    `lib` as `lib/browse` already does for the file manager. And the clamp that
    turns a declared extent into a client size lives on
    `tairix_window::Desktop`, which is built from a `DesktopInfo` — the host
    needs one composed from the board geometry rather than a second copy of
    `Desktop::window_size`'s arithmetic.
  - **What remains genuinely unwitnessed even then.** That the *pages* were
    freed, rather than merely unmapped on both sides. The sysinfo memory
    reading could show free memory rising across the release, but it is noisy
    on a live desktop; the honest witness is the release record plus the
    re-attach, and the page accounting belongs to a kernel-side test of
    `shm_unmap` refcounting rather than to a desktop vertical.
- **D56 — the x86_64 page-table walk recovers a table by its raw physical
  address**, so every page table, and the direct map that shares the window
  with them, must live below the user virtual base. A machine with more than
  64 GiB of RAM fails closed on every frame above it. Surfaced by D55, which
  removed the smaller of the two bounds; not introduced by it.
- **D55 — the x86_64 direct physical map covered only the first gigabyte —
  DONE.** Every kernel path that reaches a frame by pointer — the spawn image
  write, the shared-memory scrub, the remap window's own record store, the
  kernel heap's slab page supply — failed closed for a frame above it, and
  the allocator hands out its highest frames first, so on a machine with more
  RAM the first frame drawn was already unreachable. The port now sizes one
  identity map from the boot memory map, as its siblings do. Two defects fell
  out of it (a huge leaf dereferenced as a page table, and a RAM self-test
  that silently skipped what the map did not cover).
- **D49 — a QEMU vertical's success status is also what a machine reset
  produces**, on aarch64 and riscv64 (both report success as plain `0`). The
  harness's verdict is therefore fail-open: a guest that took the machine down
  without reaching its assertions scores `Pass`. Latent today (no enrolment
  resets its guest) and confirmed by measurement, not inference.
- **D45 — the per-CPU live-space publication accepted a non-`Arc` pointer —
  DONE.** Its `Arc` refcount write therefore landed out of bounds, corrupting
  the host heap and failing the §7 gate's test phase. It was a real unsound
  `unsafe` write, not test isolation: the `Arc` provenance is now a type
  invariant. Two further defects fell out of it (a dispatch refusal that left
  stale per-CPU publications, and a futex bucket table swapped under live keys).
- **D5 — `mem-pin-migration` intermittent multi-vCPU-TCG stall — DONE.**
  Root-caused to a lost-wakeup in the vertical's own secondary-CPU idle
  loop and fixed structurally (not a load artifact, not a budget bump).
- **D6 — `docs-check` cross-crate resolution failure — RECURRED, CAUSE
  NAMED.** A `docs-check` build failing to resolve real, unconditional
  `pub` items in sibling crates. The recurrence was a **poisoned build
  cache** — truncated zero-byte rmeta left by a `cargo` build killed
  mid-flight, accepted as fresh by the next build — cleared by
  `cargo clean -p` of the named crates. The original instance survived a
  full `cargo clean`, so its cause is still unconfirmed and the entry stays
  on record with both procedures.
- **D10 — `autoload-input-qemu-aarch64` intermittent terminal-focus
  freeze — DONE.** The QEMU vertical intermittently timed out at the AW4
  terminal stage. Root cause was a fragile *test-harness* readiness gate
  (the terminal-window click keyed on a global window-endpoint
  `CallReplied` count that also counts window *presents*), not a kernel
  lost-wakeup: a timing-dependent files-window repaint inflated the count
  and fired the terminal-focus click before the terminal window existed.
  Fixed by gating on window *creation* (the once-per-window shared-frame
  `sc=shm_map`), which no repaint can inflate; a host regression test
  locks the creation-based gate in. (D7–D9 below are already-closed
  x86_64 defects.)
- **D11 — `netstack-listener-qemu-aarch64` RTO-cadence crawl — DONE.** The
  QEMU vertical intermittently (~1/3 of runs) timed out (300s): the single-CPU
  guest went **fully idle** (guest clock frozen) for a full TCP-RTO interval
  and only progressed when the host's retransmit raised a device IRQ. Root
  cause was **depth-1 transmit staging** in `lib/virtio_net` (candidate (b),
  not a scheduler lost-wakeup): each `service()` handed the device only one
  frame and stranded the ACK queued behind a data segment in the frame ring
  until an interrupt-driven re-service, so the transfer drained at the
  completion-interrupt/RTO cadence. Fixed by multi-in-flight TX pipelining (a
  fixed `TxStaging` pool sized to the transmit ring: reap-all + stage-all per
  `service`, head-keyed completion reclaim), with two host regression tests.
- **D12 — aarch64 GICv2 SGI end-of-interrupt dropped the source-CPU field
  — DONE.** Under IPI-heavy load (`stress --cpu 12`, and the earlier
  `--vm` reproductions) every CPU hard-locked with IRQ *unmasked* yet no
  interrupt delivered and a merely-*pending* stuck line. Root cause:
  `gic::acknowledge` masked the `GICC_IAR` value with `IAR_INTID_MASK`
  before `handle_irq` passed it to `GICC_EOIR`, discarding the SGI
  source-CPU field (bits [12:10]). The GICv2 spec requires an SGI's EOIR
  write to carry the same source-CPU bits read from IAR, so a reschedule
  IPI (SGI 0) sent from any CPU other than 0 was **never deactivated**:
  the CPU-interface running priority stayed raised and every further
  interrupt on that core (preemption timer, watchdog, devices) was
  blocked, wedging it. It presented as an undiagnosable "hard lockup" only
  because the never-EOI'd interrupt is a *banked* SGI, invisible to the
  observer's SPI-only `stuck_spi` scan. Fixed by carrying the full IAR
  value through acknowledge → dispatch (masking only for the INTID
  comparison) → EOI, so the source-CPU field survives to the completion
  write; host regression tests lock the full-IAR return and the
  source-CPU-preserving EOIR write in.
- **D13 — a distinct secondary-CPU hard lockup under `stress --cpu 20`;
  diagnostic enabler landed, root-cause fix OPEN.** On the (D12-fixed)
  debug image `stress --cpu 20` still wedges a secondary core (`id=4082
  cpu=3 context=kernel sampled=pre_silence k_site=task_body`, `k_bt` in
  the secondary idle/dispatch path). D12's interrupt path is confirmed
  correct, so this is a *separate* IRQ-masked spinlock deadlock/long-hold
  in the task-shim / address-space-activation path under heavy multi-core
  load — the maskable liveness sample cannot observe inside the section,
  so the report is a bare hard lockup. Because the exact lock cannot be
  pinned from static reading and guessing an SMP-deadlock fix is a hack
  (§2.1), the landed step is the **`k_lock` diagnostic enabler**: a
  debug-only per-CPU lock-site record (`lib/sync` `lock-diagnostics` →
  `kernel/core` observer → `CpuState::lock_sites`, rendered
  `k_lock=<file>:<line>` on `id=4085`) so the next reproduction names the
  culprit spinlock. That evidence has since arrived and points *away* from a
  deadlock: the reproduction detailed under D23 records `k_lock_state=held`
  (not `acquiring`), and on this configuration an `IrqSafeSpinLock` hold
  leaves `DAIF.F` clear and therefore stays sampleable — so a lock wedge
  could not have been silent. D23's exception-return corruption is a proven,
  reachable mechanism for a silent wedge on exactly this image, and is the
  leading candidate for this defect too. The interrupt-safe allocator lock has
  since landed, and its install — which reached only a heap a bin had
  published, so it silently no-op'd on every QEMU test bin — is now a
  crate-global seam covering every heap in a binary, so the QEMU matrix
  finally runs the fix it is meant to confirm. **Remaining:** re-run
  `stress --cpu 20` on metal; if it no longer wedges, close this, and if it
  does, the surviving report is fresh evidence for a genuinely separate
  defect.
- **D14 — `sysmon-qemu-aarch64` load-dependent 120 s timeout under
  concurrent `cargo xtask ci` — OPEN (to root-cause).** The single-CPU,
  full-boot `sysmon` acceptance vertical (unlock + PBKDF2 + interactive
  monitor session, 120 s budget) intermittently times out **only** when
  `cargo xtask ci` runs it alongside its ~dozen other QEMU guests: the
  run's concurrency budget (host logical CPUs) lets that many single-CPU
  TCG guests overlap, starving each of TCG throughput so this
  work-heavy guest misses 120 s. It **passes in isolation**
  (`cargo xtask test --qemu --only sysmon`) and passed a preceding full
  `ci` run, so it is not a code regression — it is the load-dependent
  QEMU-TCG timeout §7 names. Per §7/§2.17 the fix is **structural**
  (bound the QEMU concurrency so single-CPU TCG guests do not
  oversubscribe, or size the budget to the actual work), **not** a
  retry and **not** a bare timeout bump; and, exactly as D5/D11 turned
  out, a "load flake" here is to be *root-caused*, never waved through.
  Discovered during `plans/NEW-FILEMANAGER.md` FM11b; recorded here
  rather than fixed inline because it is a shared-tooling concurrency
  concern unrelated to that feature.

- **D44 — a console reader's re-park used the CPU id it remembered before its
  first park, suspending whichever task now ran there — DONE.** `elsh` was
  killed for a fault it never took. Root-caused to a stale per-CPU index in
  `BlockingConsoleRead::read_until`, which read the CPU once before its
  poll-and-park loop; fixed by reading it at each park, plus a fail-closed
  dispatcher check that a suspension point lies on the task's own kernel stack.
  See the full entry below.

- **D15 — `autoload-input-qemu-aarch64` freeze at the PTY Ctrl-C stage,
  timing-perturbable by an unrelated binary-size change — OPEN (for the
  PTY owner).** While landing the RFC 3168 TCP ECN engine (`plans/NETWORK.md`
  N13 — pure `lib/net`/`netstack` changes, nothing on the terminal/pty/
  shell/signal path, `enable_ecn` off by default so netstack behaviour is
  byte-identical), this vertical began freezing at the **AW4 PTY Ctrl-C
  job-control sub-stage** (`plans/PTY.md`): the guest emits `PTY ctrl-c
  armed`, spawns the recovery tasks, then the single CPU stops advancing
  (~60 s guest-time) with **no** kernel WARN/ERROR/panic/OOM — the desktop's
  own IPC loop stops too. A/B confirmed: with the ECN change the vertical
  freezes 4/4 runs (300 s **and** an 1800 s budget — a real freeze, not
  slowness); with the ECN change `git stash`ed it **passes**. Because ECN
  cannot reach the pty path, the correlation is a **timing perturbation**
  (the slightly larger `netstack`/driver binaries shift load/spawn timing),
  which points at a **D10-class fragile test-harness readiness gate** in the
  Ctrl-C stage — the same failure mode D10 fixed for the terminal-focus
  click (a gate keyed on a global window-endpoint `CallReplied`/occurrence
  count rather than a monotonic creation event). D10 fixed the terminal
  *focus* gate but the newer PTY Ctrl-C sub-stage appears to carry the same
  fragility. The ECN change is otherwise fully green (host tests,
  integration, `fuzz --secs 5`, clippy `-D warnings`, docs, fmt) and was
  accepted with this recorded for the PTY owner. Recommended fix
  (structural, per §7/§2.17): re-gate the Ctrl-C stage on a monotonic,
  count-independent readiness marker (as D10 did with `sc=shm_map` window
  creation), **not** a timeout bump or retry; reproduce with
  `cargo xtask test --qemu --only autoload-input-qemu-aarch64` on this
  branch. The subsequent RFC 8511 ABE change (`plans/NETWORK.md` N13,
  `lib/net::tcp::cc`) reproduces the identical freeze for the identical
  reason — a few added `lib/net` constants/helpers shift the same load/spawn
  timing; ABE cannot reach the pty path (`enable_ecn` off, `on_ecn`
  unreachable without a negotiated-ECN connection) — and was likewise
  accepted with this recorded for the PTY owner. The subsequent DHCPv4
  D2 change (`plans/DHCP.md` — the `lib/net::Stack`/`netstack` interface
  integration of the DHCP client) reproduces the identical 300 s timeout at
  the same PTY Ctrl-C stage (viewer.app loaded, desktop still pumping, no
  WARN/panic/OOM) for the identical reason — a few added `lib/net`/`netstack`
  bytes shift the same load/spawn timing; DHCP cannot reach the pty path — and
  was likewise accepted (User-confirmed) with this recorded for the PTY owner.
  The subsequent DHCPv6 D4a change (`plans/DHCP.md` — the pure
  `lib/net::dhcpv6` RFC 8415 client engine) reproduces the identical 300 s
  timeout at the same stage (viewer.app loaded ~87 s guest-time, desktop
  still pumping IPC, no WARN/panic/OOM) for the identical reason — the new
  `lib/net` module adds compiled bytes that shift the same load/spawn
  timing; DHCPv6 is inert at runtime here (D4a is engine-only, no netstack
  wiring) so it cannot reach the pty path — and was likewise recorded for
  the PTY owner. The subsequent DHCPv6 **D4c** change (`plans/DHCP.md` — the
  live two-process QEMU verticals; production `lib/net`/`netstack` untouched,
  all new code test-only in `netpeer`/`netstack_wire`/the three `dhcp6` test
  crates) reproduces the identical 300 s PTY-Ctrl-C-stage freeze for the
  identical reason and is likewise recorded for the PTY owner. That increment
  *did* land part of the §7/§2.17 structural recommendation — it bound the QEMU
  matrix concurrency harder (`qemu_host_budget_for`: one-quarter → one-**sixth**
  of logical CPUs) so co-scheduled single-CPU TCG guests get more host headroom;
  that rescued the new heavier `netstack-dhcp6-qemu-aarch64` full-boot vertical
  (which had briefly tripped a load-dependent 240 s timeout, now a 360 s budget
  sized to DHCPv6's larger work), but this desktop/PTY vertical still freezes on
  the D10-class readiness gate above, which only the marker re-gate will fix.
  Any small binary-size change is expected to keep tripping this gate until that
  structural fix lands. The subsequent DNS **DNS1** change (`plans/DNS.md` — the
  pure `lib/net::dns` RFC 1035/RFC 5452 stub-resolver engine, engine-only with no
  netstack wiring) reproduces the identical 300 s PTY-Ctrl-C-stage freeze for the
  identical reason — the new `lib/net` module adds compiled bytes that shift the
  same load/spawn timing, and DNS is inert at runtime here so it cannot reach the
  pty path — and was likewise accepted (User-confirmed) with this recorded for the
  PTY owner.

- **D16 — Raspberry Pi 4 near-every-boot hard lockup ~10 s after USB-HID
  bring-up — DONE.** On real BCM2711 (never QEMU, which uses virtio and a
  coherent I-cache) the boot wedged a core with interrupts masked shortly
  after the USB keyboard/mouse drivers loaded; the lockup watchdog reported
  a bare `context=kernel sampled=pre_silence k_site=user_switch
  k_detail=0x0e` (task 14, a `usb_kbd` EL0 driver) with no fault/syscall
  breadcrumb. Root-caused (on-metal beacons, then `objdump`) to **two**
  distinct metal-only defects, both fixed:
  1. **Missing I-cache maintenance after the loader writes a program's code
     pages.** `kernel/mem` `build_process_image` fills code through the
     cacheable direct map; the Cortex-A72 I-cache is not coherent with those
     writes, so a freshly-loaded driver fetched stale/garbage instructions
     and took an `EC=0` "unknown/unallocated instruction" abort on valid
     code (non-deterministic per physical frame — "always after USB" = the
     last-loaded drivers). Fix: a no-default `PhysMap::sync_instruction_cache`
     (aarch64 `dc cvau`+`dsb ish`+`ic ivau`+`dsb ish`+`isb`; coherent/host
     impls a documented no-op), called by the loader for `MapFlags::EXEC`
     segments only. The maintenance lives on `ConfiguredIdentityPhysMap`
     (the aarch64 physmap that carries the arch cache primitives), and **both
     aarch64 spawn producers — PID 1 `init_spawn.rs` and the runtime `spawn`
     `spawn_producer.rs` — load through it**; a `DirectPhysMap` (whose
     `sync_instruction_cache`/`clean_invalidate` are the I/O-coherent no-op)
     threaded into either loader silently defeats the guarantee and reproduces
     the fault as a `write=false fault_class=wild` data abort (the stale bytes
     now decode to a valid-but-wrong instruction that loads through a wild
     pointer) — the terminate path (fix 2) then kills the driver instead of
     halting, so the keyboard never comes up. The stored `BuiltImage.physmap`
     is the same map, so its `clean_invalidate` (the shared-memory zero-on-free
     scrub) is real on the Pi too. Regression coverage: the `kernel/mem` loader
     test proves `sync_instruction_cache` is called for EXEC segments only; the
     wiring is single-sourced (both loaders name `ConfiguredIdentityPhysMap`)
     and metal-only (QEMU `virt` is I-cache-coherent, so no host/QEMU vertical
     can exercise it — like fix 2 it is confirmed on metal).
  2. **The trap handler parked the whole CPU on that user exception.** An
     EL0 sync exception the specific handlers did not resolve (the `EC=0`
     here) fell through to `halt_current_cpu()` (`msr DAIFSet,#0xf` + `wfi`
     forever) — a one-task fault escalated to a system-wide hard lockup.
     Fix (§17.1/§2.9/§26.5): a shared `fatal_exception` that, for any
     lower-EL (`kind >= LOWER_SYNC`) exception, **terminates the offending
     task and keeps the CPU alive** via a new resolution-free
     `DispatchHook::terminate_user_fault` + aarch64 `UserFaultTerminateFn`;
     only a same-EL kernel fault or an unattributable one halts. Regression
     tests: the loader syncs code (and only code); the terminate path never
     returns "retry".

- **D17 — riscv64 loader has no instruction-cache maintenance for
  freshly-loaded code — OPEN (latent, real-hardware only).** Noticed while
  fixing D16's aarch64 wiring: the riscv64 spawn producers
  (`kernel/tairix-kernel/src/riscv64/{spawn_producer,init_spawn}.rs`) fill and
  map code through `DirectPhysMap`, whose `sync_instruction_cache` is the
  no-op, and RISC-V has **no** `ConfiguredIdentityPhysMap` equivalent — there
  is no `FENCE.I` maintenance anywhere on the load path. RISC-V instruction
  fetch is not required to be coherent with stores, so a real SiFive board can
  fetch stale code exactly as the Pi 4 did (D16). It does not reproduce on the
  QEMU `virt` target (TCG invalidates its translation cache on writes), so no
  vertical catches it. The proper fix is an Arch-HAL `sync_instruction_cache`
  slice for riscv64 (a `FENCE.I` broadcast to the executing harts, cross-hart
  via IPI) plus a riscv64 physmap that carries it, threaded into both riscv64
  spawn loaders — the same shape as the aarch64 fix, not a copy (§2.21). Left
  as a separate item because it is an Arch-HAL addition, not the reported
  aarch64 boot fault, and riscv64 metal is not the platform in play; it must
  not ship to real riscv hardware unfixed.

- **D18 — early-boot silent guest death when PID 1 spawns a 5th concurrent
  boot service — DONE (non-reproducing; superseded by FONT-SERVICE).** The
  original report was a silent aarch64 guest death ~2.5 s into boot when a 5th
  `service` was added to init's `DEFAULT_CONFIG`, attributed to a
  concurrency/capacity defect in the early-boot spawn path. It **no longer
  reproduces** on the current tree, and the feared silent-corruption path does
  not exist:
  - **Root cause was the per-app font payload, now removed.** Before
    FONT-SERVICE each GUI/service `Run` carried a ~10 MB `R` segment, so a 5th
    near-simultaneous address-space build during root-mount was genuinely heavy
    (page-table / RAM pressure at the tests' 256 MiB) — that weight, not the
    service *count*, was the trigger. FONT-SERVICE removed the payload
    (`fontd` rasterises on demand), so every early service is now slim.
  - **The spawn path is robust and fails closed.** Controlled aarch64
    `spawn-session` experiments (isolated): all 6 early processes
    (`sysinfod`→`netstack`→`devmgr`→`seatmgr`→`fontd`→`login`) spawn and the
    guest reaches login cleanly; a stress run of **10** concurrent boot
    services (crash-looping duplicates → heavy spawn churn) booted healthily to
    the 120 s harness timeout with **19** process-spawns, login serving IPC,
    and **no** panic/fault/guard-violation/corruption. The kthread-stack guard
    arena is ample (~60 stacks in the 4 MiB boot arena at 256 MiB) and its
    growth is implemented and fail-closed (chain-a-block via `FrameArenaGrow`,
    else the software-canary `BoxStack`); the startup-config parser fails
    closed at `> MAX_SERVICES` (`ConfigError::TooManyServices`). There is no
    silent overflow. `startup::MAX_SERVICES` is now *derived* from the
    boot floor (`DEFAULT_CONFIG`'s own `service`-directive count) rather than
    a magic `4`, so the floor can never overrun its own bound
    (`plans/NEW-SERVICEMANAGER.md` SVC-1).
  - **Standing regression coverage (no new vertical — §2.2/§2.3).** Concurrent
    early-boot service bring-up during root-mount is exercised by
    `spawn_session_qemu_*` (4 services + session); EL0 multitasking under the
    live scheduler by `spawn_el0_timeshare_qemu_*` and `scheduler_stress_qemu`;
    guard-arena growth/fail-closed by the `stack_arena` host tests
    (`kernel/tairix-kernel/src/stack_arena_tests.rs`). SVC-A has since moved
    PID 1 onto the heap-backed `Init` engine (`plans/NEW-SERVICEMANAGER.md`),
    so the services are no longer bounded by a no-heap `const`; only the
    per-console session table (`supervisor::MAX_SUPERVISED_CONSOLES`) remains a
    fixed stack bound, and the growable discovery-registered service tier lands
    with its own N-service guard on the `lib/rt` heap (`plans/SPAWN.md` SP5b).

- **D19 / D20 — `autoload-input-qemu-aarch64` terminal + post-terminal
  sequencing drift — CLOSED (green).** The vertical was RED because its
  post-terminal stages were sequenced on **cumulative `MessageDelivered`
  counts** that the FONT-SERVICE cadence change drifted, firing the
  file-manager clicks early, hijacking focus off the terminal, and stalling
  the run (surfacing as a 300 s timeout). Resolution:
  - The terminal → pty stage now sequences on **guest readiness markers and
    uniquely-attributable witnesses**, not counts: the AW4 round-trip and the
    pty `Ctrl-C` recovery are attributed to the *bundle loads* of
    `/System/Commands/sleep.app` and `/System/Commands/true.app` (the `appmgr`
    `APP_LOADED` `bundle` field), and the typed command is gated on
    `TERMINAL_FOCUSED_MARKER` (first delivery to the second window port).
  - The FM9-a/-b/-c, FM10 and FM11 **file-manager choreography was removed
    from this vertical** (user-approved scope-down): that application UI logic
    is proven by `lib/browse`'s host unit tests, and driving it via a long,
    blind pointer-injection script only added the count-drift fragility. The
    vertical now proves what only QEMU can — driver autoload, encrypted-root
    unlock, display bind, and the keyboard → session → terminal → pty → shell
    round trip + `Ctrl-C` job control — and passes six deterministic
    witnesses. The theme-toggle `light` screendump was also dropped: it is a
    WM feature orthogonal to this vertical, and it was never content-verified
    green (the toggle did not present a light frame; tracked below).
  - **Follow-up (not blocking):** in the QEMU desktop the appearance-toggle
    click did not produce a light-theme frame (the `window` and `light`
    screendumps were byte-identical). The theme-toggle *logic* is host-tested
    in `tairix_desktop_session`/`tairix_taskbar`; whether the QEMU gap is a
    click-choreography artefact or a real present path issue is unresolved and
    left for a compositor/display vertical to investigate.

- **D21 — a layered block device republishes an unreadable member class as
  `Virtual`, so the mount table can report a medium nobody declared — OPEN
  (structural fix staged).** The block-service publish site wraps the
  *served* class in `Some(...)` (`lib/abi/src/blkio.rs` `serve`:
  `let class = Some(device.device_class());`), and `Block::device_class()` is
  concrete by definition — its trait default is `BlkDeviceClass::Virtual` —
  so a layer over a device whose class word was unreadable publishes
  `Some(Virtual)`, a fabricated identity indistinguishable from a genuine
  paravirtual device. That now reaches userland: the mount medium threads
  from the completion through `MountBacking` to
  `MountRecord::medium()`, so the System Information API can assert a medium
  no driver reported. Noticed while landing that mount-medium path (which
  fixed the *decode* half: an undefined class word stays `None` end to end),
  recorded here rather than fixed inline because the remaining half widens
  the block trait across every implementor. Detail below.
- **D22 — `netstack-dhcp-qemu-riscv64` intermittent stall under the full
  pipeline (OPEN).** The vertical hits its 360 s deadline when the guest
  matrix shares the host with the rest of `cargo xtask ci`, having stopped at
  `driver-store catalogue unavailable`. The re-evaluation wakeup and the
  root-unlock independence are both cleared by code inspection, and its
  budget was already once enlarged for this same reason — so the fix is
  bounded guest concurrency or a real completion signal, never a third bump.
  Reproduced again under a whole-project `ci`; the same run measures the
  lone-run cost at ~30 s, bounding the gap at >12× (detail below).
- **D23 — the debug FIQ self-sample corrupted the aarch64 exception-return
  window — DONE.** A desktop session on the QEMU-`virt` **debug** image hard
  locked a secondary core with a `pre_silence` PC *inside* the trap
  trampoline's return epilogue. That epilogue programmed the single-copy
  `ELR_EL1`/`SPSR_EL1` pair ~40 instructions before its `eret`; the debug
  watchdog's Group-0/FIQ cadence — the one asynchronous exception that can
  land there — overwrote both, so the interrupted `eret` re-entered the
  epilogue at EL1 with its frame popped and climbed `sp` off the kernel
  stack into a recursive, `DAIF`-masked abort storm: silent, no panic, no
  recovery. Both `eret` sequences now mask asynchronous exceptions before
  they program the return state. Detail below.
- **D24 — in-kernel work had no yield boundary, so a burst of fast device
  operations monopolised a core — DONE.** A desktop session on the QEMU-`virt`
  debug image reported a 10 s soft lockup (`id=4080 cpu=0 stalled_ms=10000
  context=kernel`) while an app decoded wallpaper JPEGs and a second app wanted
  the CPU. The report was honest, not a false positive: the sample's
  `context=kernel` comes straight from `SPSR_EL1`, and its backtrace resolved
  through the storage stack (`SharedBlockHandle::read_blocks` →
  `BlockCache::cached_read` → `virtio_blk` → `notify_wait`). Root cause was a
  **missing boundary, not a spin**: both preemption latches are consumed only on
  the way back to user mode, so an in-kernel body that issues one bounded
  operation after another holds its CPU for the whole burst whenever the device
  is fast enough that no operation has to wait — `virtio_blk::submit_and_wait`
  polls the ring *before* waiting, and under QEMU the completion is already
  there, so the park that would have returned control to the dispatcher never
  happens. The dispatch loop's housekeeping and heartbeats stopped for the
  burst, which is exactly the condition `classify` reports. Fixed by giving
  in-kernel code the boundary it lacked (`preempt::yield_if_owed`, sharing one
  `honour_latched_tick` decision with the return-to-user point), called from the
  storage funnel every in-kernel device operation passes through
  (`SharedBlockHandle::with_device`, before the device lock is taken) and from
  the in-kernel `/System` store server's between-requests boundary. The
  diagnostic that misdirected the reading is fixed too: a kernel kthread body
  now stamps `k_site=kernel_body` instead of sharing `user_switch` with a real
  user task's EL0 run.
- **D33 — `waitset_wait` fixed-priority starvation — DONE.** A
  level-triggered member with work outstanding held the scan head, so a
  server handling one source per wake served nothing else. The registry now
  rotates the scan past the member each wait reported.
- **D34 — the tray monitor exited on session back-pressure — DONE.** A full
  session queue (`WouldBlock`) counted as a publish fault, so five busy
  sample periods killed the monitor; nothing restarts one. Back-pressure is
  now excluded from the give-up budget.
- **D35 — an app-ward window event is dropped when its mailbox is full
  (OPEN).** The session's delivery is one non-blocking send with no
  hold-back, so a state edge (`Resized`, `FilePicked`, …) can be lost.
  Needs a "destination has space" wait-set member kind first.
- **D36 — a panic *inside* the framebuffer console's renderer hangs its own
  report (OPEN).** Noticed while wiring the D8 surface handover
  (`plans/DISPLAY.md`), not caused by it. On a release build with a live
  framebuffer, `SerialSink::write_event` renders the record through
  `video::render_bytes`, which takes `RENDER_LOCK` **blocking**. A panic
  raised while that lock is already held by this CPU — an index or arithmetic
  fault inside `lib/fbcon`, or inside `render_bytes` itself — therefore
  deadlocks on the report it is trying to emit: no oops on the screen, no
  oops on serial, a silent hang. The re-entrancy guard does not help (it does
  not release the lock). D8's panic reclaim deliberately steps around this
  (`video::reclaim_surface` uses `try_lock` precisely so it cannot add a
  second hang site) but does **not** fix the underlying write path. The real
  fix is Linux's `bust_spinlocks` shape: on entry to the panic path, mark the
  console locks broken so every later console write proceeds unlocked — the
  machine is going down and a torn frame beats silence. Needs a `lib/sync`
  primitive for "abandon this lock", so it is a `lib/sync` + per-port change,
  not a one-liner. **Regression cover owed with the fix** (§7): a host test
  that panics with the render lock held and asserts the record still reaches
  the sink.
- **D25 — `boot_audit_ring`'s scripted test clock was process-wide, making its
  exact-instant assertions order-dependent — DONE.** Noticed while running the
  `kernel/core` suite for D24: `records_are_retained_and_read_non_destructively`
  read back `8 s` where it expected `1 s`. The module's `scripted_clock` counted
  on one `static AtomicU64` that several tests `reset_clock()` before asserting
  the exact instants their own writes recorded, and the harness runs those tests
  in parallel threads — so a sibling test's reads advanced the sequence between
  a test's reset and its own writes. Fixed structurally by making the counter
  per-thread (`std::thread_local!`), so a test's scripted sequence is its own;
  a regression test asserts the per-thread independence directly (it fails
  against a shared counter). Not a load artifact and not retried away: six
  consecutive whole-crate runs are green.
- **D37 — riscv64 appears to save no floating-point state (OPEN,
  unconfirmed).** Noticed by reading the port while scoping
  `plans/FIX-DESKTOP-SPEEDUP.md`: `riscv64gc-unknown-none-elf` is a
  hard-float ABI and `lib/raster`'s gradient path uses `f64`, yet neither
  `trap.s` nor `context.s` carries an `fsd`/`fld` and no `mstatus.FS`
  handling was found. Either FP faults or two tasks corrupt each other's
  float registers. Confirm first, then fix behind the Arch HAL
  context-switch slice — the same slice x86_64 needs for user-space SSE.

- **D38 — the nightly soak killed every filesystem soak, and a memtest
  sweep mid-progress — DONE.** Three wall-clock defects in the soak
  tooling: an `fssoak` child given an ordinary step's 45-minute deadline
  while being told to run for seven hours (so any budget above 45 minutes
  was unreachable by construction), a soak loop that always started one
  pass more than fitted its budget, and a memtest-takeover guest killed
  by a ceiling derived from a silence budget that describes no part of a
  whole-RAM sweep. All fixed structurally, none by a retry. Does **not**
  close D14, whose 120 s is an inactivity budget, not a ceiling.
- **D39 — a riscv64 guest stalled dead moments after a `spawn` — DONE.**
  `userentry::enter_user_mode` armed `sscratch` — which the trap vector reads
  as "this trap came from U-mode" — with `sstatus.SIE` still set, so an
  interrupt in the two instructions before its `sret` was misclassified,
  clobbered the caller's frame and returned down the S-mode path, which does
  not re-arm. The new process then ran with no kernel stack armed and every
  later trap built its frame on the task's own *user* stack until the program
  wild-jumped — and a U-mode instruction page fault halted the hart, silencing
  the guest. `SIE` now joins the mask cleared ahead of the arm, as aarch64
  has always done. The silence was a second defect: riscv64 offered only
  load/store U-mode page faults to the resolver and halted on everything
  else, so any user program's wild jump could park the machine. It now has
  aarch64's `UserFaultTerminateFn` and kills the task instead.
- **D40 — a mutating memory syscall re-froze the whole address space —
  DONE.** Every syscall or fault that changed a task's mappings rebuilt the
  registry's whole snapshot: a page-table walk plus a heap node per resident
  page, inside one non-preemptible call. The release half was fixed earlier
  (`mem_unmap`); the mapping half was staged and underrated, because the
  desktop session maps a frame region for **every window an app opens** — a
  `terminal.app` context menu paid four of them against the largest address
  space on the machine, the ~300 ms per menu reported on a Pi 4B. Each path
  now publishes only its own region's pages. Reading the same class found two
  more: a file-backed fault re-froze per page (O(N²) to read an N-page
  mapping) and stack growth re-froze a range it had just computed.
- **D42 — an x86_64 ring-3 wild jump halts the CPU instead of the task
  (OPEN).** Found by inspection while fixing D39's sibling half. That port
  has no `user_fault_terminator`, and its `#PF` dispatcher offers a ring-3
  fault to the resolver only for *data* accesses, so an instruction-fetch
  fault parks the CPU for one task's mistake. Mirror the aarch64/riscv64
  slot; also settle which exception vectors that port installs at all.
- **D43 — a riscv64 U-mode task could steer the kernel onto another hart's
  per-CPU state — DONE.** Found by inspection while designing per-thread
  thread-local storage. `tp` (x4) is both the psABI thread pointer U-mode
  writes freely and this port's per-hart kernel identity anchor
  (`SchedulerArch::current_cpu` → `smp::current_hartid` reads it), and the
  trap vector never touched it — so `li tp, <other hart>; ecall` had the
  kernel resolve *that* core's resume handle, dispatch slot, and live address
  space. `sscratch` now points at a per-task 16-byte **trap anchor** carrying
  the running hart's kernel `tp`; the from-U prologue spills the user's `tp`
  into the frame and reloads the kernel's before any other register is
  touched, and the U-return path publishes the current hart's value (so a
  migrated task re-enters U-mode under the right identity) and restores the
  user's. The frame slot lives on the task's own kernel stack, so the thread
  pointer is now genuinely per-task — the platform contract TLS rests on.
  Witness: `tests/integration/tp_isolation_qemu_riscv64` (a hostile-`tp`
  U-mode fixture on a two-CPU guest), plus the `trap_layout_tests.rs`
  ordering/layout pinning against `trap.s`.
- **D47 — every desktop launch lost its first argument, so the autostarted
  file manager ran as an ordinary window — DONE.** `appbar-qemu-aarch64` ran
  to its 600s ceiling. `spawn_app` passed the caller's arguments as the whole
  argv, so the program name a program's own arguments begin *after* was its
  first real argument: the file-manager autostart never saw `--desktop` and
  took `Role::Window` — an unasked-for home window at every login and a
  *Quit* row on a core component. The rule is now the one host-tested
  `launch_argv`. The harness half compounded it: the autostarted file manager
  holds strip slot 0, so measuring "the launched application" there compared
  it with itself; the script now drives `APPBAR_LAUNCHED_SLOT`.
- **D48 — a window `Create` an app could build but the session had to refuse,
  and nothing said so — DONE.** `datetime.app` asked for a fixed-size window
  *and* a minimum client size; the protocol refuses that pair, so the app
  exited before it ever drew. `WindowSizing` is now a sum type, so the
  combination cannot be spelled. Its second half was the silence: the elevated
  child's `stderr` is login's console, invisible behind the desktop, so login
  now audits an abnormal exit (`LAUNCH_ENDED_ABNORMALLY`) naming the reason.

These are **distinct in kind**: D1 finishes an interrupt-model fix, D2
and D4 are §27 foundational-completeness defects, D3 is an Arch-HAL
parity gap, D5 was a test-harness idle-loop lost-wakeup (fixed), D6
is a docs-build resolution failure (recurrence root-caused to a poisoned
build cache), D10 was a fragile QEMU-harness
readiness gate (fixed), D18 was an early-boot concurrent-spawn scare that
proved non-reproducing once FONT-SERVICE removed the per-app font payload
(closed), D19/D20 were the `autoload-input-qemu-aarch64` count-drift
(closed: marker-based sequencing + file-manager choreography moved to host
tests), D21 is an ABI-honesty gap — a layer asserting a hardware fact nobody
reported — D22 is a load-dependent QEMU-harness timeout whose mechanism is
not yet named, D23 was an observer-perturbs-the-observed defect the debug
watchdog's own non-maskable sample exposed (fixed), D42 is the x86_64 half
of the fatal-user-exception routing D39 closed for riscv64 (open), D24 was a
missing in-kernel preemption boundary — a fairness defect, not a wedge — that
let a burst of never-waiting device operations withhold a core (fixed), D25
was a process-wide test clock that made a host suite's exact-instant assertions
order-dependent (fixed), D36 is a panic-path self-deadlock in the console
write path (open, needs a lock-abandon primitive), D37 is a suspected
per-port context-switch gap found by reading, not by a failure (open, confirm
before fixing), and D43 was a privilege-boundary defect of the same
found-by-reading kind as D42 — a user-writable register the kernel trusted for
its own per-CPU identity (fixed), D47 was a dropped argv[0] in the desktop's
launch path that started a core component in the wrong role, behind a harness
that measured the wrong slot (fixed), and D48 was a window request an app could
build but the protocol had to refuse, dying in silence because a graphical
elevation's `stderr` reaches no one (fixed), and D52 is an x86_64 cross-CPU
shootdown protocol defect that only became reachable once a production caller
existed — the tree is safe by the current caller set, not by the protocol
(open), D57/D58/D59 were a *policy* group rather than coding slips — a
pressure model whose two halves disagreed, three counts standing in for the
resource they were meant to bound, and a release that undid itself one line
later and then only ran on an edge nobody crosses (all fixed) — and D60 is the
coverage those three left behind: a path tested at every seam and never once
end to end, which is exactly the shape that let D59's three halves hide behind
green unit tests (open; its design is corrected and its groundwork landed with
D59's third half). D61/D62 are the two stream-wake defects that had been filed
under numbers D52 and D53 already held by the shootdown and kernel-heap
entries; the citations in the tree now resolve to one defect each. Do not
collapse the open items into one change; land each on its own
whole-project-green gate (§7). D63 and D64 are the two defects here that are
*not* kernel defects, tracked here for their severity, and both are now fixed:
an ARXFS commit published its superblock slot with no durability barrier, so a
reordering device could lose an interior tree node beneath a durable root and
the volume would not mount (fixed in `plans/ARXFS-WRITEBACK.md` WB1, where the
batching that makes a per-commit barrier affordable landed with it, along with
three further ordering defects the work exposed); and ARXFS scrub's metadata
copy-repair wrote to the device with no read-only guard, so a mount held
read-only precisely because its medium must not be touched was written anyway
(fixed — the copy-repair is one read-only-aware rule, and reading that code
found two more read-only writes on the same path). D65 joined them and is now fixed: ARXFS's
B-tree insert recursed 8 KiB of stack per tree level, overflowing a release
kernel's 32 KiB stack — measured at 48 KiB for one write to a fragmented file,
and 34 KiB for one to a single-leaf tree, so it was reachable without any depth
at all (item A1 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`; the mutation path is
iterative and the measured cost no longer scales with depth).
D66 is the fourth and is also fixed: one `DriverError` value spoke
for a taken name, a populated directory and a retryable transient at once, so a
name taken between the VFS's pre-check and the driver call was reported as an
I/O error, and any consumer reaching a filesystem driver without the VFS's
per-operation mapping read `EWOULDBLOCK` where `EEXIST` was meant.

## Coupling to be aware of

D1 (FIX-SYSCALL) and D2 (P-6) ride the **same** `request_wake` /
`waitq::drain_pending_wakes` machinery. Whoever executes D2 must not
break the lock-free-ISR + deferred-drain shape the syscall return path
now depends on, and must re-audit exactly the park sites the syscall
path made interruptible (§2.2 — one discipline, not two). Sequence D1
before or alongside D2 where practical, and re-run the FIX-SYSCALL
verticals after D2 lands.

---

## D1 — Close the FIX-SYSCALL residual verticals

**State:** design + code done (T1–T5 of `plans/FIX-SYSCALL.md`); the
aarch64 syscall-body vertical passes. **Remaining:** the same vertical
on the other bare-metal targets, and metal re-confirmation.

- **D1.1 — x86_64 syscall-body vertical.** Port the aarch64
  `preempt`-style syscall-body test to x86_64 under QEMU: a task in a
  deliberately long syscall (a) has a device IRQ / preemption tick
  **taken during** the syscall (delivery), (b) is **not** rescheduled
  mid-syscall (non-preemptibility — IRQ in ring-0 serves-and-returns),
  and (c) is rescheduled at **return-to-user** when `need_resched` is
  latched. Include the wake-timeliness case (a parked blocking syscall
  woken via the lock-free drain).
- **D1.2 — riscv64 syscall-body vertical.** The same, under the riscv64
  QEMU target (`sstatus.SIE` enabled in-syscall, `sret` re-masks).
- **D1.3 — wasm32 (C2) confirmation.** Assert the no-op entry/exit still
  satisfies the deferred-drain + reschedule-at-return semantics via the
  host yield facility.
- **D1.4 — metal re-confirmation.** Re-confirm on Pi hardware that a
  long in-kernel syscall body no longer stalls the preemption tick /
  serial drain / input pump (the 2026-06-23 failure class). Record the
  metal checklist result; do not mark FIX-SYSCALL fully closed until it
  is confirmed.

**Done when:** the syscall-body vertical is green on every bare-metal
Tier-1 target under QEMU, wasm32 is confirmed, metal is re-confirmed,
and `plans/FIX-SYSCALL.md` is updated to done-state (§13) with its
`PLAN.md` sibling-of-P-5 entry finalised.

---

## D2 — P-6: wait-queue §27 completeness rework — DONE (host-proven)

**State:** landed. `kernel/core/src/waitq.rs`'s O(n) `Vec` wait set is
replaced by a three-index `WaitSet` (all `BTreeMap`, `const`-constructible
so the `static` queues keep `const fn new()`), meeting the §27 bar.

**Deliverables (§27 — the complete primitive, not new surface §27.4):**

- **D2.1 — real wait-set structure — DONE.** `by_task: BTreeMap<TaskId,
  Waiter>` gives O(log n) `register`/`deregister`/`wake_task` membership,
  and `order: BTreeMap<seq, TaskId>` (a monotonic arrival sequence) gives
  a *stated* FIFO first-come-first-served no-starvation discipline: the
  oldest `seq` is the head `wake_one`/`oldest_task` release, and a
  re-`register` keeps its `seq` so a looping waiter is never overtaken. No
  linear scan on the per-park path. (An `alloc`-only `BTreeMap` was chosen
  over an intrusive list because the latter needs per-task node storage in
  the scheduler — a far larger change for the same O(log n) removal and no
  `unsafe`.)
- **D2.2 — deadline-ordered structure — DONE.** `deadlines: BTreeMap<
  (deadline_ns, seq), TaskId>` holds only finite-deadline waiters, so
  `earliest_deadline` is O(log n) (the front key) and `sweep` visits only
  the already-expired prefix in deadline order — O(log n + woken), not a
  scan of every waiter per timer expiry. `nearest_timed_deadline` is a
  fixed-arity min over the five timed queues' O(log n) fronts.
- **D2.3 — `wake_one` path — DONE.** `wake_one` (FIFO head) and
  `wake_task` (addressed) are the single-target paths; `wake_all` is kept
  for genuine broadcast conditions only (cancellation, a shared latch
  resolving).
- **D2.4 — preserve P-5's discipline — DONE.** The lock-free ISR
  `request_wake` + deferred `drain_pending_wakes` shape is unchanged
  (§2.2); no second wake/drain path.
- **D2.5 — park sites re-audited — DONE.** Single-target events use
  `wake_task` (`CALL_WAITQ`/`SERVE_WAITQ`/`SIGNAL_INTAKE_WAITQ`); genuine
  broadcasts use `wake_all` (`CONSOLE`/`PROCWAIT`/`PIPE`/`HW_TREE`/
  `USERS_DB`/`APP_STORE`/`SEAT_INPUT`). The rework preserves each choice.

**Tests (§7/§23.4):** host tests cover FIFO wake order + re-register
position preservation, deadline ordering + expired-prefix sweep,
deregister across every index, the wake-one round-robin no-starvation
loop, and the unchanged lock-free `request_wake`/drain race. All 15
`waitq` tests green.

**Done:** `waitq.rs` meets the §27 bar with the above operations,
complexities, and stated fairness discipline; all park sites re-audited;
tests green; `PLAN.md` P-6 updated to done-state.

---

## D3 — Hard-lockup watchdog parity (x86_64, riscv64)

**State:** the soft-lockup detector is cross-arch; **hard-lockup**
detection (the non-maskable buddy cadence + `WatchdogArch`) is wired
only on aarch64 (virtual generic timer `CNTV`, PPI 27). x86_64 and
riscv64 keep only the soft detector and inherit hard detection once they
wire their own non-maskable cadence (`PLAN.md` ~2046, `plans/WATCHDOG.md`).

- **D3.1 — x86_64 hard-lockup cadence.** Wire a non-maskable liveness
  sample (NMI-driven cadence via the local APIC / HPET as the arch
  dictates) behind the existing `WatchdogArch` seam, feeding the
  arch-neutral buddy detector and `request_recovery` — no new surface,
  reuse the aarch64 shape (§2.21).
- **D3.2 — riscv64 hard-lockup cadence.** The same, using the riscv64
  non-maskable/high-priority timer facility.
- **D3.3 — `stuck_interrupt` parity.** Implement `WatchdogArch::
  stuck_interrupt` for each port (the aarch64 `gic::stuck_spi` analogue)
  so the `stuck_irq`/`stuck_state`/`stuck_owner` diagnostics are emitted
  on all bare-metal targets, not just aarch64.
- **D3.4 — conformance vertical.** Extend the `WatchdogArch` conformance
  suite (§17.2) with the hard-lockup case on x86_64 and riscv64 (a
  CPU wedged with IRQs masked is detected and a recovery attempted with
  its honest outcome logged, `CPU_LOCKUP_RECOVERY` 4084).

**Done when:** hard-lockup detection + recovery + stuck-line attribution
work and are conformance-tested on all three bare-metal targets;
`plans/WATCHDOG.md` and the README support matrix updated to match.

---

## D4 — Latent §27 audit sweep of foundational primitives — DONE

**State:** completed. Every foundational primitive `kernel/*`, `lib/*`,
and userland code builds on was read and judged against the §27 bar
(complete abstraction, right structure/complexity for §26 load,
fairness/ordering/wake-one where the abstraction implies it, no O(n) scan
on a load-bearing path). `waitq` (D2) was and remains the **only** thin
slice; every other primitive is §27-complete. One latent structural
watch-item (the slab free-slot scan) is recorded below — it is not a live
defect (its sole production caller uses one slot) and is staged, not
fixed in passing (D4.3).

**D4.1 — primitives enumerated and audited.** The full set below.

**D4.2/D4.3 — audit result (each primitive read, not assumed):**

| Primitive | Structure / complexity | Verdict |
|---|---|---|
| `lib/sync::SpinLock` / `IrqSafeSpinLock` | test-and-set acquire spin (charter's brief-hold carve-out); `new`/`try_lock`/`lock`/`is_locked`/`get_mut`/`into_inner`/guards | §27-complete |
| `lib/sync::McsLock` | canonical MCS queue lock — strict FIFO fairness, per-waiter local spin, O(1)/op | §27-complete (the fair lock the plain spinlock defers fairness to) |
| `lib/sync::RwLock` | writer-preference; stated fairness invariant (`pending_writers>0` blocks new readers) with a property test | §27-complete |
| `lib/sync::SeqLock` | read-mostly seqlock — `read`/`write`/`sequence`, retry-on-odd | §27-complete |
| `lib/sync::OnceCell` / `Once` | full once-init: `get`/`set`/`get_or_try_init`/`take`/`call_once`(+infallible), poison handling | §27-complete |
| `lib/collections::BitSet256` | 4×u64; full set algebra + subset + popcount + ascending fused iter, all O(1) | §27-complete |
| `lib/caps::CapabilitySet` | 256-bit; full algebra + subset-enforcing `delegate` + `revoke` + wire round-trip; delegation-never-widens property-tested (§19.7) | §27-complete |
| `lib/caps::CapToken` | unforgeable token vocabulary (`token.rs`) | §27-complete |
| `kernel/ipc::PortRegistry` | `BTreeMap` endpoint + name indexes — O(log n) `lookup`/`resolve`/`register`/`unregister`; bulk `teardown_owned_by` O(n) only on process exit (not a hot path) | §27-complete |
| `kernel/ipc` `call`/`port`/`notify` | reply/mailbox/notification queues over the shared `waitq` wake/drain discipline (D2) | §27-complete |
| `lib/kalloc::FreeListAllocator` | two tiers behind one `GlobalAlloc`: a header-free per-size-class slab up to the page granule, coalescing segregated fit over boundary tags above it; growable/shrinkable via `HeapSource`; deterministic OOM (null, never panic) | §27-complete (O(1) allocate, free, coalesce and region/page reclaim — no list walked on any path; pinned by the two per-operation node-reach tests) |
| `lib/rt` heap | first-fit over a coalesced, address-sorted free-**span** list; growable `SpanStore` (§24.1/§25); realloc grow/shrink in place | §27-complete (same standard first-fit design; §25-bound) |
| `kernel/mem::Slab` | guard-page + tag-rotation + zero-on-free + double-free/dirty-slot hardened fixed-size slab | §27-complete for its use — **watch-item** below |

**Slab free-slot scan — recorded, staged, not fixed in passing (D4.3).**
`Slab::alloc` finds a free slot with an `O(slot_count)` linear scan of the
`in_use` bitmap rather than an `O(1)` free-index. This is **not a live
§27 defect**: the sole production constructor (`kernel/core/src/kthread.rs`
kthread-stack slab) uses `slot_count == 1`, so the scan is O(1) in
practice, and the slab's purpose is guard/tag hardening of small, few-slot
object classes, not a high-fan-out hot-path allocator. It is recorded as a
latent structural watch-item: **should a large-`slot_count` consumer ever
be introduced, `Slab` must first gain an O(1) free-slot index (a free-slot
stack/head) so the allocation hot path does not become O(n) under §26
load.** Staged as a `PLAN.md` note rather than reworked here, per D4.3 (do
not fix in passing; the abstraction is complete and correct for every
present caller).

**Done:** every enumerated foundational primitive audited against §27 and
confirmed complete (table above); the one latent structural concern (slab
free-slot scan) recorded and staged with its specific trigger; no other
thin-slice core found; no in-scope code fix was required (all present
callers are served correctly), so the sweep lands as the recorded audit.

---

## D5 — `mem-pin-migration` intermittent multi-vCPU-TCG stall — DONE

**Root cause.** A lost-wakeup in the vertical's *own* secondary-CPU idle
loop (`tests/integration/mem_pin_qemu_aarch64/src/kernel.rs`
`migration_secondary`), not the scheduler or the CI runner. The re-rolled
secondary loop parked on a bare `wfi` with IRQ taking **enabled** and
without re-checking the run queue: when a placement/reschedule IPI landed
in the window between `step` returning `Idle` and the `wfi`, the handler
took and acknowledged the SGI, so the following `wfi` then slept with a
just-readied task already on this CPU's run queue (`wake_from_parked`
enqueues *before* `send_ipi`). During the parent phase no further IPI is
sent, so the CPU slept indefinitely and the guest made no progress until
the wall-clock budget fired. It reproduces only under QEMU-TCG timing
jitter, hence the isolation-passes / full-matrix-stalls signature.

**Fix.** `migration_secondary`'s idle and paused branches now use the
same race-free park the production dispatch loop uses
(`kernel/core/src/init.rs` `run_dispatch_loop`): mask IRQ taking, drain
flagged wakes and re-check `Scheduler::has_ready_work(cpu)` (and the pause
flag) under the mask, `wfi` only if still genuinely idle, then re-enable —
so an IPI arriving in the check→park window stays pending-but-masked and
wakes the `wfi`. No budget bump, no retry.

**Regression coverage.** The mirrored protocol is guarded host-
deterministically by `run_dispatch_loop`'s
`idle_commit_rechecks_work_published_after_the_idle_step` unit test
(work published inside the masked idle-commit window must not let the
dispatcher sleep). A per-window micro-reproducer is not feasible for a
hardware-timing race; the fix removes the harness's divergence from that
tested protocol, and the vertical now runs it.

---

## D6 — `docs-check` cross-crate resolution failure — recurred; cause was a stale build cache

**State:** `docs-check` is green. The failure recurred once since, with a
named and cleared cause (below), so the class is no longer a mystery — but
the entry stays on record because the *mergeable-info* suspicion for the
original instance was never confirmed.

**Prior symptom (historical).** `cargo xtask ci` → `docs-check`
(`cargo doc --workspace --no-deps --document-private-items
-Z rustdoc-mergeable-info`, `RUSTDOCFLAGS="-D warnings"`) was reported to
fail while documenting `tairix-kernel`:

```
error[E0432]: unresolved import
  tairix_abi::driver::virtio_pci::virtio_pci_window_resource
 --> kernel/tairix-kernel/src/hwdiscovery.rs:24:5
```

`virtio_pci_window_resource` is a real, unconditional `pub fn` in
`lib/abi/src/driver/virtio_pci.rs` (no `cfg`, no feature gate), so the
failure was a cross-crate rustdoc resolution issue under the unstable
`-Z rustdoc-mergeable-info` mergeable-info model, never a kernel-logic
defect.

**Reproduction attempted, could not reproduce.** The exact CI command was
run standalone — from a warm cache, from `cargo clean --doc`, and from a
full `cargo clean` (cold compile of every dependency's rmeta) — and each
run documented all 373 crates and merged cleanly (`cargo doc -p
tairix-kernel --no-deps` succeeds too). `cargo xtask docs-check`
(rustdoc + mdBook + link check) passes end to end. The host carries no
`sccache`/`RUSTC_WRAPPER` and no shared `CARGO_TARGET_DIR`, so this is not
a stale-cache artefact.

**Recurred again, same mechanism, and the surgical remedy is not enough.**
A third instance appeared as a *cascade* — `tairix_arch_api`,
`tairix_reclaim`, `tairix_crypto`, `tairix_cpuops`, `tairix_fsmeta`,
`tairix_devmatch`, `tairix_netconfig`, each a real unconditional `pub` item
in a feature-less workspace crate — after interleaved host and
`--target <triple> --keep-going` clippy runs. The signature was explicit:
2111 **zero-byte** `.rmeta` files under `target/`, each with link count 1
(never hardlinked into the cache) and timestamped to the minute of a build
that was cut short, sitting beside healthy rmeta for the same crate.
Two cheaper remedies **failed**: `cargo clean -p` of the named crates just
moved the failure to the next consumer, and deleting the zero-byte files
outright did not help either, because the *fingerprints* still recorded
those units as fresh. Only a full `cargo clean` cleared it. So the practical
rule is: on a "can't find crate for `<workspace crate>`" cascade, check for
zero-byte `.rmeta` (`find target -name '*.rmeta' -size 0`) and, if any are
present, go straight to a full `cargo clean` — do not spend runs on `-p`
cleans. Avoid interleaving concurrent host and cross-target cargo
invocations that may be interrupted.

A fourth instance confirmed both the signature and the remedy exactly: 1096
zero-byte `.rmeta` files after a session that interleaved host `cargo
test`/`clippy` runs with `--target aarch64-unknown-none` /
`x86_64-unknown-none` / `riscv64gc-unknown-none-elf` builds, again a
`can't find crate for tairix_arch_api` / `tairix_reclaim` cascade in
`docs-check`, and again cleared only by a full `cargo clean` (347 766 files,
123.8 GiB — which then costs a cold gate run). The practical discipline is
therefore to **batch** the cross-target builds and lints separately from the
host runs rather than alternating between them.

**Recurrence, root-caused: a corrupt/stale build cache.** The same shape
appeared again with three different symbols — `tairix_tty::read_bounded`,
`tairix_tty::is_line_delimiter` (from `kernel/core`) and
`tairix_qemu::ReservedSocket` (from `tools/xtask`) — each a real,
unconditional `pub` item in a feature-less crate, and each *recently added*
(`ReservedSocket` by the then-latest commit). It reproduced standalone under
`cargo xtask docs-check`, not only inside the concurrent gate group, and
vanished permanently after `cargo clean -p` of the four crates involved.
The cache held rmeta from a pre-commit revision alongside **zero-byte**
`.rmeta` files timestamped to the minute a nested `cargo` build was killed
mid-flight, so the mechanism is an interrupted build leaving truncated
metadata that a later build accepted as fresh — the errors came from *rustc*
checking a dependent crate, not from rustdoc.

**Consequences for the two theories.** This instance is **not** evidence for
the mergeable-info suspicion, so do **not** drop `-Z rustdoc-mergeable-info`
on its account; and the original entry's "not a stale-cache artefact" holds
only for the original instance, where a full `cargo clean` had already been
tried. Killing a `cargo` process mid-build is now a known way to poison the
cache: `cargo clean -p <crate>` for the crates named in the error is the
correct first response, and a green re-run *after* such a clean is a real
fix, not a retry.

**If it recurs without a killed build behind it.** Treat it as a real
cross-crate-rustdoc / mergeable-info defect (not a load flake): confirm
`cargo clean -p` does *not* clear it, capture whether it appears only under
the concurrent `cargo xtask ci` static-gate group (memory pressure) vs.
standalone, and the structural fix is then to drop
`-Z rustdoc-mergeable-info` from `run_docs_check`
(`tools/xtask/src/commands.rs`) — the mergeable-info model is a doc-build
*speed* optimisation, and correctness of the doc build takes precedence.

---

## D7 — x86_64 disk-completion interrupt triple-faulted the boot — DONE

**State:** fixed. The live x86_64 disk bring-up now delivers the
virtio-blk-PCI completion interrupt, wakes the scheduler-parked bring-up
repeatedly, and mounts the read-only `/System` volume — proven by
`tests/integration/root_unlock_admission_qemu_x86_64` (keys PASS on
`root_mount::SYSTEM_VOLUME_MOUNTED_MESSAGE`).

**Root cause (two x86_64 defects, both fixed).** The symptom looked like
"the parked kthread never wakes" (the serial stalls with no prompt), but
the guest was actually **triple-faulting** (`qemu -d int`: a ring-3 `#PF`
storm, then a kernel `#PF` in `syscall_entry_stub` with `RSP=0` /
`CR2=-8`, → `#DF`). Two independent bugs:

1. **External-IRQ ISR read the interrupted CPU frame at the wrong stack
   offset.** `external_irq.s` pushes a synthetic *vector qword* between the
   15-GPR `SavedRegs` block and the CPU-pushed `InterruptStackFrame`, but
   the shared `preempt::preempt_ring3_if_pending` located the frame at
   `regs + size_of::<SavedRegs>()` — correct only for the *timer* stub,
   which pushes no vector qword. On a device IRQ it read the vector qword
   as the interrupted `CS`, mis-decided ring-3, and ran an unbalanced
   `swapgs`; a later `syscall` then loaded `kernel_rsp0` from the wrong GS
   base (0) → push into a null stack → `#DF`. Fix:
   `preempt_ring3_if_pending` now takes the `InterruptStackFrame` pointer,
   and each ISR computes it at its own offset (the external path adds
   `EXTERNAL_VECTOR_QWORD_BYTES`). Timer-driven preemption (used by
   `spawn_session_qemu_x86_64`, which passed) was unaffected, which is why
   only the disk (external-IRQ) path crashed. Host guard:
   `irq::tests::external_irq_frame_sits_one_vector_qword_above_saved_regs`.
2. **The MSI-X source shared an IO-APIC pin's vector.** `virtio_blk_unlock`
   reused the PCI interrupt-line GSI's vector for the device's MSI and
   drove that pin's *level* `IoApicController` for an *edge* MSI. Fixed by
   `kernel/tairix-kernel/src/x86_64/msi.rs`: a dedicated MSI vector +
   virtual `MSI_LINE_BASE` line space with an edge no-op
   `CompositeIrqController` (the Linux / aarch64-`MSI_LINE_BASE` model), so
   an MSI-X source is never bound to a shared IO-APIC pin. Boot pre-installs
   the free external vectors as MSI lines; `root_unlock` allocates a
   dedicated `(vector, line)`.

## D8 — x86_64 encrypted-root / users-DB read loop stalls the interactive unlock — DONE

**State:** resolved. `root_unlock_admission_qemu_x86_64` now boots the full
two-kthread admission path through the interactive encrypted-root unlock and
keys PASS on `unlock_service::USERS_DB_INSTALLED_MESSAGE`, with the scripted
`ARXFS passphrase:` step restored — the kthread-admission install witness the
vertical was scoped to reach. The former deterministic stall does not
reproduce; the install completes deterministically (confirmed over repeated
untraced guest boots).

**Root cause.** D8 was a consequence of the pre-fix kernel-heap OOM/pressure
condition, not a logic loop in the read path. On the 256 MiB admission guest
the two concurrent disk kthreads (the interactive-unlock kthread and the
driver-store serve kthread) drove the pressure-governed
`BlockCache`/`SharedBlock` while the kernel heap could not grow past the old
8 MiB `MAX_ORDER` granule: allocations for the encrypted-root/users-DB read
path met sustained memory pressure that both starved the clean-block cache
(so the hot metadata blocks were re-read from the device instead of served)
and, at the OOM edge, prevented net forward progress within any budget —
hence "5000+ `notify_wait` returns, no log-visible progress, identical at
120 s and 300 s". The `kernel/mem` `frame::MAX_ORDER` 11 → 13 (8 MiB → 32 MiB)
growth plus the `appspawn::read_file` fallible-reserve read (landed for the
kernel-heap OOM defect after D8 was filed) removed that condition: the heap
now grows to back the read path, the pressure that drained the cache and
blocked progress no longer arises, and the admission install terminates.

**Evidence (traced boot).** A temporary per-read LBA trace on the boot
`BlockCache` device path confirmed the admission boot now makes monotonic
forward progress to `id=4139 root-unlock: users database installed` and on
to the login screen — two interleaved *advancing* read streams (the two
kthreads), not a single block re-read forever. Residual re-reading of hot
metadata blocks under the tight guest is the memory-pressure cache design
working as intended (drop clean, rebuildable blocks under pressure, re-read
on demand) — bounded, forward-progressing, and fail-closed, not the D8 loop.

**Regression.** `root_unlock_admission_qemu_x86_64` is extended to the
users-DB-install witness (was: the `/System` mount), so a re-introduction of
either the D7 triple fault or a D8-class admission stall fails the run loud;
the observer `root_unlock_login_qemu_x86_64` never drives this concurrent
two-kthread path, so this vertical is its only guard.

## D9 — x86_64 `spawn-session` login never exits on the (now live) console — DONE

**State:** fixed. `spawn_session_qemu_x86_64` reaches its seven-spawn
`wait`→reap→relaunch witness and passes a real guest boot.

**Root cause — two layers.** The vertical's PASS keys on **seven**
`ProcessSpawned` — `init`, the boot services `sysinfod` / `netstack` /
`devmgr` / `seatmgr`, the first `login`, and the **relaunched** `login`
after `init` reaps the first. Its documented model assumed the x86_64
console had *no read backing*, so `login`'s `stream_read` failed closed at
`NULL_CONSOLE_READ` and `login` exited. The A3 interrupt-driven COM1 receive
path made that assumption false: the diskless boot opens `CONSOLE0_GATE` at
the init seam (`root_unlock::spawn_if_present`, no binding →
`release_console0_to_login`), so `login` owns console 0 and its read is a
**live, poll-backed COM1 read**. With no scripted input `login` correctly
*waited* (a timed `stream_read` returning `TimedOut` → the view's idle
refresh re-queries `sysinfod`, the `ipc_call`s each replying cleanly). So
the test had to be brought in line with its aarch64 sibling and *drive*
login to exit.

Doing so exposed the **real production defect** underneath: the x86_64
COM1 log sink (`SerialSink::write_event`) and console-write backing
(`Com1Console::write`) called `Serial::init(COM1_BASE)` on **every** log
line / console write. `Serial::init` is *not* idempotent for an armed
interactive console — it writes `IER = 0` (disarming the receive interrupt
the login console enabled) and the FIFO-control clear bits (flushing the
receive FIFO). Under the debug-log flood a re-init raced `login`'s
interactive read and **silently dropped the typed input** while disabling
receive delivery — an intermittent hang. This is a genuine bug in the A3
console work: on real hardware, any log output or prompt write while a user
types at the x86_64 login would drop keystrokes.

**Fix (production + test).**
- **Production (`x86_64/serial_sink.rs`):** a `com1_writer()` helper brings
  the 16550 up **exactly once** (a `tairix_sync::Once` guard) and returns
  the non-reinitialising `Serial::at` on every later call. `SerialSink` and
  `Com1Console` route through it, so diagnostic output and prompt writes
  never clear `IER` or flush the receive FIFO. The `Serial::at` seam and its
  "init clears IER/FIFO" warning already existed; the write paths simply
  stopped re-initialising.
- **Test (`qemu_tests.rs`):** the x86_64 enrolment scripts a serial dialogue
  typing one character past `MAX_USERNAME_LEN` at the `Username:` field so
  the view refuses the over-long line (`LengthOutOfRange`), `login` fails
  closed and exits, and `init` reaps + relaunches it (seventh
  `ProcessSpawned`). The injected line is **newline-terminated**
  (`OVERLONG_USERNAME`, shared with aarch64) so it is a complete line the
  reader receives whether the console is in the view's raw discipline or a
  cooked line discipline. The stale test-crate module doc and enrolment
  comment are corrected to the live-console model.

Verified: `spawn_session_qemu_x86_64` is stable over repeated runs (was
~40 % flaky before the production fix); the aarch64 sibling still passes.

## Definition of done (whole plan, §7/§15/§23)

This umbrella is closed only when D1–D9 are each closed on their own
whole-project-green gate:

- D1: syscall-body verticals green on all bare-metal targets + wasm32
  confirmed + metal re-confirmed; FIX-SYSCALL marked done.
- D2: **DONE** — `waitq.rs` at the §27 bar (three-index O(log n)
  `WaitSet`, stated FIFO no-starvation) with tests; P-6 marked done.
- D3: hard-lockup watchdog + diagnostics conformance-tested on all three
  bare-metal targets.
- D4: **DONE** — every foundational primitive audited against §27
  (findings table recorded); all complete, the one latent structural
  concern (slab free-slot scan) staged; no in-scope code fix required.
- D5: **DONE** — the `mem-pin-migration` multi-vCPU-TCG stall root-caused
  to a lost-wakeup in the vertical's secondary idle loop and structurally
  fixed (production masked-park protocol); a full `cargo xtask ci` is
  whole-project-green.
- D6: **NON-REPRODUCING** — the cross-crate rustdoc `docs-check` failure
  documenting `tairix-kernel` does not reproduce on the pinned toolchain
  (verified from a full `cargo clean`); the `docs-check` step itself passes
  end to end. Recorded with its reproduction procedure in case it recurs.
- D7: **DONE** — the x86_64 disk-completion-interrupt triple fault
  root-caused (external-IRQ frame offset + shared IO-APIC-pin MSI vector)
  and fixed; `root_unlock_admission_qemu_x86_64` reaches the `/System`
  mount over the dedicated MSI-X vector.
- D8: **DONE** — the x86_64 encrypted-root / users-DB admission stall
  root-caused to the pre-fix kernel-heap OOM/pressure condition (removed by
  the `kernel/mem` `MAX_ORDER` growth + `appspawn` fallible-reserve read);
  `root_unlock_admission_qemu_x86_64` extended to key PASS on the users-DB
  install and confirmed deterministic over repeated guest boots.
- D9: **DONE** — root-caused to a stale dead-console test model *and* a real
  production bug it exposed: the x86_64 COM1 log sink / console-write backing
  re-ran `Serial::init` per write, clearing `IER` and flushing the receive
  FIFO and so dropping the interactive `login`'s typed input. Fixed by a
  one-time `com1_writer` init guard (`x86_64/serial_sink.rs`) plus an
  aarch64-aligned over-long-username serial script in the enrolment;
  `spawn_session_qemu_x86_64` reaches its seven-spawn witness and is stable.
- For each landing: `cargo fmt --all` (+ `--check`), `cargo xtask ci`
  (once), `cargo xtask fuzz --secs 5`, and `tools/ci/soak.sh both
  --secs 20` green and quoted; §23 self-review verdict stated.
- Housekeeping: `PLAN.md` immediate-work list reflects the closures, the
  README support matrix updated where a per-arch mark changes, and a row
  added to the `AGENTS.md` §15.18 jump-sheet:
  `Open core-kernel defect tracking → plans/OPEN-DEFECTS.md`.

## D10 — `autoload-input-qemu-aarch64` intermittent terminal-focus freeze — DONE

**State:** fixed. The `autoload-input-qemu-aarch64` vertical is stable over
repeated runs; the intermittent freeze (guest goes fully idle at the AW4
terminal stage, run times out) no longer occurs.

**Root cause — a fragile *test-harness* readiness gate, not a kernel
lost-wakeup.** The freeze was intermittent (timing-dependent), not the
deterministic deadlock first suspected. The harness gated the
terminal-window focus click on a **global count of window-endpoint
`CallReplied` records** (`TERMINAL_WINDOW_REPLIES = 4`). That count
includes window *presents*, not just window *creates*: it assumed the
files window presents exactly once (create + one startup present, 2
replies), so the 4th reply would be the terminal's create/present. But a
files-window click that lands so it repaints (a timing-sensitive outcome
under certain boot pacing) adds extra present replies; when the files
stage emitted ≥4 replies, the 4th `CallReplied` occurred *during the files
stage*, so the terminal-focus click fired onto the empty desktop before
the terminal window existed (→ files unfocus = the lone stray delivery),
the terminal was never focused, the typed-command delivery gate never
advanced, and the guest idled. The desktop session and `lib/window`
delivery path were correct throughout; the app-ward `ipc_send` is
non-blocking and the kernel wakes were not lost.

**Fix — gate on window *creation*, which no repaint can inflate.** A
window's shared frame region is mapped exactly once, when the window is
created (`WindowServer::create` → the session `ShmMapper`); a present
re-uses that mapping. So the harness now gates the terminal-window click on
the count of shared-frame **map** operations (`sc=shm_map`), of which
exactly three precede the terminal-window click — boot framebuffer
scan-out, files window create, terminal window create — independent of how
many times any window repaints. Contract constant
`TERMINAL_WINDOW_FRAME_MAPS = 3` (renamed from `TERMINAL_WINDOW_REPLIES`)
with the marker `AUTOLOAD_WINDOW_MAP_MARKER = "sc=shm_map"` in
`tools/xtask` `qemu_tests`. The files-window click keeps its own
create-keyed gate (the first window-endpoint reply), which is stable.

**Regression guard.** Host test
`qemu_tests::tests::terminal_window_click_gates_on_window_creation_not_repaint_count`
asserts the terminal-window click's steps key on the creation (`shm_map`)
marker and its `TERMINAL_WINDOW_FRAME_MAPS` occurrence count, and never on
the present-inclusive `CallReplied` count — so the fragile gate cannot
return. The QEMU vertical itself is the end-to-end guard.

**Note.** riscv64/x86_64 autoload siblings are input-only (no display,
desktop, or terminal stage), so this gate exists only in the aarch64
vertical's shared pointer-script contract; no sibling change was needed.

## D11 — `netstack-listener-qemu-aarch64` RTO-cadence crawl — DONE

**State:** fixed. The wedge (single-CPU guest going fully idle for a whole
TCP-RTO interval and only stepping forward on the host's retransmit) no
longer occurs; the transfer proceeds at line rate.

**Root cause — depth-1 transmit staging, candidate (b), not a scheduler
lost-wakeup.** `lib/virtio_net` held exactly **one** transmit staging pair,
so each `service()` could hand at most one frame to the device: `drain_tx`
sent the first queued frame, saw the single pair in flight, and left every
further queued frame (the TCP ACK sitting right behind a data segment) in
the shared frame ring as "back-pressure" with no re-service scheduled. The
trailing frame therefore egressed only on the *next* `service`, which the
stack issues from a device interrupt — so a run of frames drained at the
device's completion-interrupt cadence and, when the frame ring backed up,
the host stopped advancing until its RTO retransmit (an unrelated RX IRQ)
drove the next service. The CFQ park/unpark handshake and the
`serve_wake_task`/`waitset_wait` path were correct throughout (candidates
(a)/(c) ruled out).

**Fix — multi-in-flight transmit pipelining (`lib/virtio_net`).** The single
`tx_header`/`tx_data`/`tx_inflight` fields are replaced by an
allocation-free `TxStaging` pool of header+frame staging pairs whose depth is
derived from the discovered machine and the device's own advertised queue
maximum (`QueueDepths`, two descriptors per in-flight frame). Each `service`
reaps **every** completed transmission (returning its staging pair to the
pool, keyed by the descriptor head the used ring reports) and then stages
**every** queued frame until the frame ring is empty or the pool is
exhausted. A data segment and the ACK behind it now egress together in one
call; back-pressure applies only when the ring is genuinely full, and even
then never waits (safe across the cross-process `Service` boundary) and
never drops. `stage_and_post` returns `TxOutcome::Sent(head)` so a
completion maps back to exactly its staging pair; a malformed/device-
fabricated completion reclaims nothing (fail closed). No busy-poll, no ABI
change, no timeout bump.

**Regression guards (host, `lib/virtio_net`).**
`service_egresses_a_burst_in_one_call_without_a_completion` proves a
multi-frame burst all egresses in one `service` with the device undriven
(the depth-1 predecessor sent only the first);
`transmit_back_pressure_only_when_the_ring_is_full` proves back-pressure
fires only with the whole pool in flight and the held frames then egress in
order. The QEMU vertical itself is the end-to-end guard.

**Note.** riscv64/x86_64 share the same `lib/virtio_net` engine, so the fix
is arch-neutral (`§2.2`); no per-arch change was needed. Receive staging
stays single-buffered (re-posted each frame) — a separate concern the stack
rides out via TCP retransmit, out of scope for this transmit-egress fix.

**Definitive crawl cause — a rejected cumulative ACK during loss recovery
(`lib/net` `tcp_conn.rs`), the real reason the vertical timed out.** The
transmit-pipelining fix above was necessary but did not stop the crawl: with
the peer injecting guest→peer loss, the guest echo server enters
retransmission, and both go-back-N on RTO (`advance`, `snd_nxt = snd_una`)
and fast retransmit rewind the next-to-send cursor `snd_nxt` back below the
true transmit high-water `snd_max`. `process_ack` then bounded its
"ACK acknowledges something not yet sent" challenge (RFC 5961 §5) on the
*rewound* `snd_nxt` instead of `snd_max`, so a valid cumulative ACK covering
`(snd_nxt, snd_max]` — data the peer demonstrably held — was challenged and
dropped without advancing `snd_una`. `snd_una` froze, the sender
retransmitted already-acknowledged bytes every (doubling) RTO, and the
connection eventually hit the user timeout and RST. Fixed by gating that
challenge on `snd_max` (the highest sequence ever transmitted, which
`Plan::Retransmit` never advances) and, when a cumulative ACK advances
`snd_una` past the rewound cursor, carrying `snd_nxt` forward to preserve
`snd_una <= snd_nxt`. Regression guard (host, `lib/net`):
`cumulative_ack_advances_una_past_a_recovery_rewound_snd_nxt` establishes a
connection, bursts several segments, fires the RTO to rewind `snd_nxt`, then
delivers a cumulative ACK up to `snd_max` and asserts `snd_una` advances and
the RTO disarms (it froze before the fix). Arch-neutral — every port shares
`lib/net`.

## D13 — secondary-CPU hard lockup under `stress --cpu 20` (enabler landed, fix OPEN)

**State:** the `stress --cpu 20 --timeout 120s --background` wedge on the
debug image is a *distinct* defect from D12 (whose interrupt-completion path
is confirmed correct). The report is a bare hard lockup on a secondary core
(`cpu=3 context=kernel sampled=pre_silence k_site=task_body`, `k_bt` in
`_start_secondary → production_secondary_entry → smp::run_secondary →
init::run_dispatch_loop → exceptions::enable_irq`). Because the watchdog
liveness sample is a *maskable* virtual-timer IRQ (GICv2 non-secure has no
NMI), a hard lockup means the core entered an **IRQ-masked EL1 critical
section and never left it** — an `IrqSafeSpinLock` deadlock or long hold in
the task-shim / address-space-activation path under heavy multi-core
spawn/preempt load. The exact lock cannot be identified from static reading,
and fabricating an SMP-deadlock fix is a hack (§2.1).

**Landed (diagnostic enablers, all debug-only):**

- `k_lock` stuck-lock record — a per-CPU lock-site stack (`lib/sync`
  `lock-diagnostics` feature → `kernel/core` observer →
  `CpuState::{lock_sites,lock_depth,lock_top_acquiring}`, rendered
  `k_lock=<file>:<line>` + `k_lock_state` on the `id=4085` detail).
  `SpinLock::{lock,try_lock}` (which `IrqSafeSpinLock` wraps) are
  `#[track_caller]` under the feature and report through the `lockwatch`
  seam; a shippable image compiles it all out (bare CAS).
- **Trustworthy pre-silence backtrace** — the aarch64 `k_bt` frame-pointer
  walk (`kernel/arch/aarch64` `capture_sample_backtrace`) now validates each
  caller: a return address is accepted only if it lands in kernel
  executable text (new `__text_start`/`__text_end` linker bounds,
  `in_kernel_text`) and each frame pointer must sit strictly above the
  exception frame (stack floor) and strictly increase. This stops the walk
  emitting a stack **data** word as a caller — the cause of the earlier
  chains that interleaved unrelated `BTreeMap` instantiations and could not
  be trusted to justify a fix. The pure `walk_frames` core is host-tested
  (incl. the non-text-return-address rejection); the `AT S1E1R` map-probe
  fail-closed guarantee is unchanged.
- **Reclaim corruption tripwire** — `AddressSpaceRegistry::withdraw` now
  asserts (`debug_assertions`, i.e. debug image only) its post-condition via
  the pure `stale_task_entry`: after a task is removed from every per-task
  map, no map may still hold it. A violation faults **at the reclaim site**
  and means either a per-task map escaped withdraw (the "reused id inherits
  a dead task's state" precursor) or a `BTreeMap::remove` did not take —
  i.e. the map is corrupt, the leading D13 hypothesis. Host-tested; compiled
  out of shippable images.
- **Named kernel-internal stuck lines (always-on, not debug-gated)** — the
  hard-lockup summary's `stuck_owner` now names a stuck line the kernel
  services *itself* through a chained/bespoke handler (no `irq_wait`
  binding) instead of reporting a bare `unbound`. A new arch-neutral
  `watchdog::KernelInternalLines` seam (installed from
  `KernelArch::watchdog_line_names`, mirroring `watchdog_recovery`) is
  consulted after the task-owner lookup returns no binding; aarch64 maps its
  discovered `BRCM_MSI_SPI` → `stuck_owner=pcie-msi` and `UART_RX_INTID` →
  `stuck_owner=console-uart` (interrupt numbers from the device tree, never
  board constants). This turned the near-every-boot Pi 4 report
  `stuck_irq=153 stuck_owner=unbound` into `stuck_owner=console-uart` — the
  BCM2711 PL011 console UART receive SPI (GIC SPI 153) a wedged cpu 0 could
  not service — confirming 153 is a *bystander* of an IRQ-masked wedge, not
  its cause. (The resolver checks `BRCM_MSI_SPI` before `UART_RX_INTID` and
  still returns `console-uart`, so 153 is the UART line, not the MSI SPI as
  an earlier note guessed before the resolver existed.) The wedge itself is
  the D13-class masked-section freeze, which the FIQ self-sample is blind to
  on the real Pi 4 GIC-400 where Group 0 is secure. Host-tested
  (`resolve_stuck_owner_with` task-wins/named/unbound; the `console-uart` and
  `pcie-msi` renders).
- **Finer dispatch breadcrumb `switch_return` (debug-gated).** The
  boot-deterministic Pi 4 report (`k_site=user_switch`, `console-uart`
  bystander, ~10.27 s right after USB HID bring-up) wedges in the masked
  span the coarse `user_switch` crumb conflated — the arch context switch,
  or the dispatcher-side teardown after it. `KernelBreadcrumb::SwitchReturn`
  (`kernel/core`) now splits that span: stamped in `kthread::dispatch_step`
  immediately after `ContextSwitch::switch` returns, it attributes the
  post-switch teardown (resume-handle retire, live-space clear, the user-root
  translation-register **park**, guard check — all IRQ-masked) to
  `switch_return`, distinct from the switch-in / EL0 (`user_switch`). So the
  next metal boot tells a wedge coming *back* from a task (notably the
  user-root park) from one going *into* it. Arch-neutral, feature-gated (zero
  shippable cost), host-tested (round-trip, distinct tags, the
  `k_site=switch_return` render).

**Latest evidence (trustworthy tools):** a fresh `--cpu` repro
(`cpu=2 stalled_ms=10218 k_site=task_body k_lock=cfq/scheduler.rs:753 held
k_detail=0x15`) decodes with the now-trustworthy unwinder to a *clean,
consistent* chain — but it is the stale `pre_silence` **idle dispatch-loop
park** (`run_dispatch_loop → monotonic_ns → enable_irq`), not the wedge. The
breadcrumb stays `753`-`held` (not another lock `acquiring`), so cpu 2 is
wedged inside running task 21's body holding that task's body lock in an
**untracked `DAIF.I`-masked busy-spin/deadlock**. The reclaim tripwire did
**not** fire (not reclaim/id-reuse corruption) and aarch64 TLB shootdown is a
hardware inner-shareable broadcast (no IPI busy-wait) — both ruled out. So
`pre_silence` sampling is exhausted: the defect lives in exactly the
IRQ-masked section no maskable sample can see.

**Decision (build the masked-section sampler):** the non-maskable **FIQ
self-sample** is the tool this evidence calls for, staged B1–B4 (full design +
the decisive `DAIF.F` constraint in this plan). The blocking
constraint was that aarch64 already masks `DAIF.F` in *every* section the wedge
lives in (exception entry masks F in hardware; `enable_irq` clears I-only;
`IrqSafeSpinLock`'s `DaifIrqControl` masks I+F), so an effective FIQ sample
needs a cross-cutting, feature-gated `DAIF.F`-clear execution discipline plus
GICv2 Group-0 routing whose acknowledge semantics differ QEMU-`virt` vs the
Pi-4 GIC-400 — landed incrementally with an empirical, fail-closed delivery
probe, never guessed (§2.1/§2.16/§2.19).

- **B1 (DAIF.F-clear discipline) — DONE.** Feature-gated, QEMU-validated.
- **B2 (GIC Group-0 routing + `is_fiq` FIQ dispatcher arm + fail-closed
  boot deliverability probe reported as a `FeatureSupport` capability) —
  DONE.** Host-tested; debug + shippable aarch64 builds clean. The probe is
  `Supported` on a single-Security-state GIC and `Unsupported` on a
  two-Security-state GIC. Measured under QEMU (B3): the `virt` default
  (`secure=off`) is single-Security-state and the probe returns `Supported`,
  so the debug image self-samples via FIQ on QEMU (correcting an earlier
  untested assumption that it was `Unsupported`). Only a real Pi 4 GIC-400,
  or QEMU `virt,secure=on`, keeps Group 0 secure and returns `Unsupported`,
  falling back to the complete buddy detector (fail closed).
- **B3 (QEMU vertical proving `sampled=live`) — DONE.**
  `tests/integration/fiq_selfsample_qemu_aarch64` (enrolled in `cargo xtask
  test --qemu`) probes `Supported`, arms a short Group-0 (FIQ) cadence, masks
  `DAIF.I`, busy-spins in an `#[inline(never)]` marker, and asserts the FIQ
  self-sample captured a live in-kernel PC (interrupted `SPSR_EL1.I` masked)
  whose value and `capture_sample_backtrace` top land inside the marker
  (`sampled=live`).
- **B4 (use the sampler to fix D13) — one cause found and fixed; the metal
  FIQ-`Unsupported` path is separate and still open.** A **deterministic**
  `stress --cpu 20` wedge was reproduced on the **QEMU-`virt` debug image**
  (full RPi image + `ramfb`, i.e. the video framebuffer console active — it
  does **not** reproduce over a UART-only console): all cores end up in EL0
  workers with the preemption timer (`CNTP`) *fired but never delivered* by the
  GIC, so nothing preempts and the shell can no longer even spawn a command.
  Root cause: `Gicv2::enable_intid` gave **every** PPI the same mid-range GIC
  priority (`0x80`), so the debug watchdog's Group-0/FIQ self-sample
  (`WATCHDOG_PPI` 27) equalled the preemption-timer IRQ (`TIMER_PPI` 30). On a
  GICv2 CPU interface with `GICC_CTLR.FIQEn` set, a pending-but-masked
  (`DAIF.F`, e.g. while an EL0 task runs) Group-0 FIQ of priority ≥ the
  timer IRQ **holds off** that IRQ — permanently once the level-triggered
  watchdog `CNTV` has fired on every core — so preemption dies. **Fix:** the
  self-sample FIQ is dropped strictly below the timer
  (`watchdog::WATCHDOG_FIQ_PRIORITY = 0xC0`, applied via the new
  `gic::set_ppi_priority` in both the boot probe and per-CPU
  `route_watchdog_group0`); a masked Group-0 FIQ can no longer block the
  Group-1 timer, and a Group-0 FIQ is still signalled independently of a
  pending Group-1, so the masked-section self-sample still fires. Verified:
  the ramfb repro stays responsive under `stress --cpu 20` and
  `fiq_selfsample_qemu_aarch64` still passes; host regression guards
  `self_sample_fiq_is_strictly_lower_priority_than_the_preemption_timer`
  (`kernel/arch/aarch64` watchdog) and `set_priority_writes_the_priority_byte`
  (gic). This is **debug-image only** and QEMU-`virt`-only: on the real Pi 4
  GIC-400 the FIQ probe returns `Unsupported` (Group 0 secure), so
  `route_watchdog_group0` never runs and this priority path is inert — the
  real-Pi masked-section hard lockup is therefore a *separate* defect (below)
  that this fix does not claim to close.

**A second QEMU manifestation, cheaper than `stress --cpu 20` and not the B4
cause.** `stress_qemu_aarch64` (4 vCPUs, UART-only console, no `ramfb`) wedges
**during early boot**, before the login prompt, when the CI QEMU matrix runs it
concurrently with six other guests. The transcript's last line is
`id=4139 root-unlock: users database installed; login can authenticate`; in a
healthy run the next line is `comm=login sc=users_db_read` **1 ms of guest time
later**, and in the wedge no core emits anything again — the kernel's own
per-syscall DEBUG stream included — for the harness's whole 300 s inactivity
budget. Total silence across every core rules out a merely-starved guest and
places it in the same IRQ-masked section as the reports above. It is *not* the
B4 GIC priority inversion, which was `ramfb`-only and is fixed. It reproduces
only under host contention, so it is probabilistic rather than deterministic,
but it needs no 20-worker load and no display: the interleaving of early
multi-core spawn during root-mount is enough. Anyone driving the FIQ or EDPCSR
samplers at this defect should try this guest first — it reaches the wedge in
seconds of guest time.

The Pi-4B armstub FIQ-routing dependency remains a hardware-capability concern
for `plans/FIX-HARDWARE-FEATURES.md`.

**CoreSight external-debug (EDPCSR) cross-core sampler — the GIC-400 observer
the FIQ path cannot be.** On the real Pi 4 the FIQ self-sample is `Unsupported`
(Group 0 secure), so the near-every-boot masked-section wedge shows only a
stale `sampled=pre_silence` PC. The one live observation that survives there is
a read of the wedged core's PC by *another* core over the memory-mapped ARMv8
external-debug interface (`EDPCSR`, DDI 0487 H9): it does not halt the target
and rides no interrupt `DAIF` can mask. Landed:
- `WatchdogArch::remote_pc_sample(target) -> RemotePcSample`
  (`Sampled{pc,context}` / `Unavailable` / `Unsupported`), default `Unsupported`
  + conformance (`kernel/arch/api`). The hard-lockup `scan` reads it and renders
  a fresh `live_pc=+0x…` (image-relative) + `live_ctx` in the debug detail,
  alongside — never replacing — the stale `pc` (feature-gated).
- aarch64 `coresight` module: host-tested pure `sample_from` (EDLAR unlock →
  EDDEVID capability → EDPRSR validity → EDPCSR capture-first → assemble), a
  scale-sized set-once per-cpu debug-base registry, and the freestanding
  `VolatileDebugMmio`; `Watchdog::remote_pc_sample` delegates to it.
- Discovery: `fdt::debug_component_bases` parses the Linux
  `arm,coresight-cpu-debug` binding (translated `reg` + `cpu`-phandle→dense-id),
  host-tested; boot installs each base **only** when its gigapage is already
  Device-mapped (a read can never fault), else nothing (fail closed →
  `Unsupported`, buddy detector unchanged). QEMU `virt` and the stock Pi 4
  firmware DTB describe no debug nodes, so this is dormant there and validated
  by the fail-closed path; **enabling it on Pi 4 hardware requires the firmware
  DTB (or a supplied overlay) to carry the `arm,coresight-cpu-debug` nodes** —
  a provisioning step, not a code change. The live `EDPCSR` read itself is
  metal-confirmable only (QEMU models no EDPCSR), mirroring the FIQ-probe
  precedent.

**Fail-open `DAIF.F` discipline fixed — the leading suspect for the
near-every-boot Pi 4 masked-section wedge.** The debug (`watchdog-diagnostics`)
build left FIQ (`DAIF.F`) unmasked **gated on the compile-time feature alone**,
never on the runtime deliverability probe: the lock critical-section mask was a
`const` I-only immediate (`daif::critical_section_mask(cfg!(...))`) and
`enable_fiq_delivery()` fired on *every* `svc`/fault sync entry unconditionally.
On the real Pi 4 GIC-400 the probe returns `Unsupported` (Group 0 secure), yet
the kernel still ran with `DAIF.F` clear pervasively — a **fail-open** exposure
to secure-world Group-0 FIQs the non-secure kernel cannot service (and with no
self-sample benefit, since none is delivered there). This matches every trait
of the wedge: debug-build-only, real-Pi-4-only (QEMU `virt,secure=off` is
single-Security-state, so it never reproduces), masked-section, intermittent,
any CPU, ~10 s in after heavy `svc`/fault (USB HID) activity. **Fix:** both
`DAIF.F`-unmask sites now consult the runtime probe (`fiq_cadence_enabled()`)
and fail closed — the base lock mask is unconditionally I+F and F is re-cleared
only when the probe proved FIQ deliverable; the obsolete compile-time
`critical_section_mask` helper is deleted. Host-tested; the metal confirmation
(no boot wedge on the Pi 4 debug image) is pending a user boot. **This fix did
not resolve the wedge** — a later Pi 4 boot on a build carrying it wedged
identically, so it was a real robustness fix but not the root cause.

**Root cause found and fixed — the kernel heap allocator lock was not
interrupt-safe (§23.2).** Resolving a fresh near-every-boot Pi 4 report's stale
`pre_silence` `k_bt` against the debug ELF gave a fully coherent chain: cpu 0
was in `tairix_kalloc::FreeListAllocator::{carve,insert_hole}` via `alloc` ←
`BlockCache::populate` ← ARXFS `open_data_block`/`read_cluster_frame` (the
root-unlock eMMC read path). The allocator guarded its state with a **plain
`AtomicBool` spinlock that never masked interrupts**. TAIRiX takes interrupts
while in-kernel code runs, so an interrupt taken on a CPU already holding that
lock (e.g. the eMMC completion IRQ during the `BlockCache::populate`
allocation) whose handler allocates reenters `alloc`/`dealloc` and spins
forever on the lock its own interrupted mainline holds — a single-CPU
self-deadlock, IRQ-masked (exception entry masks `DAIF.I`), so the watchdog
cannot sample it → the observed hard lockup. This matches every trait: any CPU,
~10 s in under heavy concurrent boot allocation (ARXFS reads + USB bring-up),
`sampled=pre_silence` (the stale sample *is* the last watchdog tick before the
handler wedged), `k_site=user_switch` (the arch IRQ handler stamps no
breadcrumb), and real-Pi-4-only (the interrupt-vs-lock interleaving under heavy
allocation rarely arises in QEMU). The watchdog/detector machinery was verified
sound (physical-`CNTPCT` cross-CPU clock; idle CPUs marked `Idle`; a running
EL0 task's liveness refreshed by the maskable watchdog IRQ), so this is a
genuine wedge, not a false positive.

**Fix.** `tairix_kalloc` carries an installable interrupt-control seam
(`install_irq_control(disable, restore)`, two set-once `fn`-pointer atomics
read outside the lock); `with_inner` masks the current CPU's interrupts
*before* acquiring the lock and restores them *after* releasing — foreclosing
the reentrant self-deadlock. Each port installs its arch primitive at `boot()`
entry, before interrupts are ever enabled and before any secondary CPU/hart
starts (one install covers every core; the hooks mask the *current* CPU):
aarch64 via `DaifIrqControl`, x86_64 via `RflagsIrqControl`, riscv64 via
`sstatus.SIE` (`csrrci`/`csrs`); the interrupt-free `wasm32` port and the host
test build install nothing (that window is single-CPU with interrupts already
masked).

**The seam is crate-global, because the first shape of it was fail-open.**
The hooks were originally per-`FreeListAllocator`, and the boot path reached
the instance through `kheap::install_kheap_irq_control`, which forwarded to
whatever `register_global_heap` had published — *and silently no-op'd when
nothing had*. Only `kernel/tairix-kernel`'s production `main.rs` registers.
Every one of the ~155 freestanding QEMU integration-test bins declares its own
`#[global_allocator] FreeListAllocator` and registers it nowhere, so on all of
them the install was a no-op and the heap lock stayed interrupt-unsafe: the
D13 root cause was live in the entire QEMU matrix, including the
`stress_qemu_aarch64` vertical whose job is to confirm D13 fixed. The hooks
mask the *calling* CPU, so they describe the machine and not any one heap;
they now live at crate scope in `lib/kalloc`, the ports call
`tairix_kalloc::install_irq_control` directly, and the `kernel/core` forwarder
is deleted. Regression test
`the_lock_masks_interrupts_via_the_installed_control` (`lib/kalloc`) pins both
halves: the lock masks then restores around each hold once a control is
installed and not before, *and* an allocator built after the install that no
registry knows about is interrupt-safe too.

**Noticed while fixing it, not fixed here.**
- The same `register_global_heap` gate also withholds the frame-backed growth
  source (`install_frame_heap_source`) from every test bin, so a QEMU vertical
  boots a kernel whose heap is capped at its `.bss` bootstrap region and the
  growth path is never exercised under a guest. That is test fidelity and
  capacity, not the D13 safety property; closing it means registering the heap
  in every vertical and re-validating the matrix's memory behaviour, so it is
  staged rather than smuggled into this change.
- Heap growth runs *under* the heap lock and takes the frame allocator's and
  kernel-remap window's plain `SpinLock`s, so an ISR that allocated while
  interrupting an EL1 mainline holding one of those would still self-deadlock
  one layer down. No such path exists today: every ISR-reachable path is
  lock-free and allocation-free except the return-to-user preempt point, whose
  interrupted context is EL0 and therefore holds no kernel lock. The allocator
  masking is correct defence-in-depth; the layer below needs the same
  treatment only if an ISR ever allocates from an EL1-interrupting context.

**Done when:** the near-every-boot Pi 4 boot wedge no longer reproduces on
metal with the interrupt-safe allocator lock, and `stress --cpu 20` no longer
wedges on metal + the QEMU stress vertical. (The FIQ and EDPCSR samplers remain
the standing masked-section observers for any *future* wedge.)

---

## D21 — a layered block device republishes an unreadable member class as `Virtual` (OPEN)

**State:** the mount-medium path is honest end to end *except* across a
republishing layer. Discovered while landing the storage medium on
`MountRecord`; the decode half was fixed in that change, the trait half is
staged here because it touches every implementor of the block trait.

**Mechanism.** Three facts compose into a fabricated hardware claim:

- `blkio::decode_outcome` now yields `Option<BlkDeviceClass>`, so a class
  word the ABI does not define stays an explicit unknown rather than being
  rewritten to `Virtual`. `BlkDeviceClass::served_as(None)` is the single
  patience policy: an unknown is *served* `Virtual`'s bounded envelope
  without ever being *called* `Virtual`. That half is correct.
- `Block::device_class()` is concrete by construction — its trait default
  returns `BlkDeviceClass::Virtual`, and both clients document their result
  as the **served** class. There is no way for an implementor to say "the
  device told me something I cannot read".
- `blkio::serve` publishes that concrete value straight back onto the wire
  (`let class = Some(device.device_class());`). So a layer over a device
  whose class word was unreadable — the block-service seam re-serving a
  `RemoteBlock`, a partition window, the block cache, a RAID array folding
  members through `BlkDeviceClass::most_patient` — republishes
  `Some(Virtual)`: an identity indistinguishable from a genuine paravirtual
  device, asserted by a layer that was never told it.

That value is no longer confined to budget sizing. It threads from the
completion through `BlkClient::declared_class()` → `MountBacking` →
`MountPoint::medium()` → `MountRecord::medium()`, so the System Information
API can report a storage medium no driver ever declared. Sizing a cautious
I/O budget from an unknown is right; *naming* the unknown is not.

**Blast radius today (small, and only by luck).** The single user-visible
consumer of `MountRecord::medium()` is the drive icon, and
`tairix_icon::disk_icon` maps both `Some(Virtual)` and `None` to the same
generic `Disk` glyph — so nothing is currently misdrawn. Nothing else reads
the field yet. The gap is therefore latent, not cosmetic: the first consumer
that distinguishes "paravirtual" from "unknown" (a medium column in `df` or
`mount`, a volume-properties panel, a policy that treats virtual disks
differently) reads a fabricated fact with no way to tell.

**Structural fix.** Widen the accessor, keep the one patience policy:

- `Block::device_class()` returns `Option<BlkDeviceClass>`, defaulting to
  `None` (an implementor that knows nothing says nothing) rather than to a
  class it invented.
- Every implementor and forwarding layer carries the `Option` through: the
  partition window, the block cache, the retained journal, `SharedBlock`,
  the six RAID array kinds, USB mass storage, and both clients
  (`lib/blkclient`, `kernel/core/src/fs/blkclient.rs`) with their fixtures.
- `most_patient` folds `Option`s, so a composition with one unreadable
  member reports its medium as unknown — which it is — while still being
  *served* the widest envelope through `served_as`. Patience behaviour is
  unchanged at every call site; only the published identity becomes honest.
- `blkio::serve` then publishes what the device actually said, and the
  unknown reaches `MountRecord::medium() == None`, where the generic drive
  icon is the right answer *by design* instead of by coincidence.

**Done when:** no layer can publish a class its device did not declare; a
regression test composes an array over a member with an unreadable class
word and asserts both halves — the composition is served the cautious
envelope, and its mount reports `medium() == None`; and `served_as` remains
the only place an unknown is turned into a concrete envelope (§2.2).

---

## D22 — `netstack-dhcp-qemu-riscv64` stall: an unbounded device wait — DONE

**State:** fixed. The mechanism was a guest-side stall, not a budget of the
wrong shape: the in-kernel virtio completion wait could not expire, so a
single unobserved completion parked the boot task **inside** a disk request
while it held that disk's lock. `/System`'s mount and the driver-store service
sit behind the same lock, which is why the guest went silent for the rest of
the run at exactly the point `devmgr` reported the catalogue missing.

**How the two candidates were separated.** The measurement D22 asked for,
done on the same 22-thread host:

- A lone run's *guest* phase (excluding its build) completes the whole
  campaign in **under 6 s**, so the 360 s budget carried ~60× headroom, not
  the ~12× the earlier "~30 s" figure (which included the build) suggested.
- Under deliberate 2× host oversubscription (44 spinners on 22 threads) the
  guest phase stretched to ~54 s — about **7×** — and still **passed**, ten
  consecutive times. Starvation of the magnitude needed to blow a 60× margin
  is therefore not what the pipeline produces.
- The failing transcript's last line is `id=13005`, i.e. the guest fell silent
  ~355 s before the kill. A proportionally-starved guest would have kept
  narrating its boot; a stalled one is silent, which is what was observed.

**The defect.** `KernelVirtioHost::notify_wait` waited with `u64::MAX` — no
deadline at all — and `IrqParkWaiter` only registered a timed wake for callers
that used its own `park_wait`, so a virtio wait parked on the line *alone*.
Any completion the driver did not observe (a lost or coalesced interrupt) left
the task parked forever, holding `SharedBlock`'s lock. The sibling
bootstrap-floor driver already had this right: the SDHCI engine waits with
`EMMC2_SILENCE_BUDGET_NS` and fails the transfer closed, precisely so a dead
controller cannot become "a task parked forever holding the volume's lock".
The virtio path — which every Tier-1 target except the Pi actually boots
from — never got the same treatment.

**Fix (landed).**

- The wait loop hands its deadline to every park (`IrqWaiter::yield_now` takes
  `deadline_ns`), so a bounded wait is releasable by construction rather than
  when a caller remembers to arm one. `IrqParkWaiter`'s private
  deadline field is gone with the bookkeeping it existed for.
- `VirtioHost::notify_wait(queue_index, timeout_ns) -> CompletionSignal`: the
  caller states its budget and learns whether the device signalled or stayed
  silent. A driver with a request outstanding passes its device class's
  per-request deadline; an idle input driver waiting for an unsolicited event
  still passes `u64::MAX`, which is correct for a wait with nothing pending.
- `virtio_blk` fails a silent request closed with `DriverError::DeviceOffline`
  after one final ring re-scan, and never reissues in place (the device may
  still own the published chain). The wake-storm bound stays as it was.
- `CompletionSignal` is now one ABI vocabulary in `lib/abi`, shared by both
  floor storage drivers; eMMC2's private copy was deleted.
- The harness no longer conflates two failures: a gated run that reaches its
  ceiling reports `UNCONFIRMED … guest silent for Ns` (`GateNeverTripped`)
  instead of a bare `TIMEOUT`. The silence at the kill is the number that
  separates "alive but never confirmed" from "stalled at a fixed point", so a
  recurrence diagnoses itself instead of needing this investigation again. The
  comment claiming gated guests chatter (which is why silence was not read as
  the signal) was false and is corrected: they park silently on their
  wait-set, and the host peer retries every 500 ms indefinitely.

**Not** fixed by a budget bump: the 360 s budget is unchanged, and the earlier
240 → 360 s raise is exactly the mitigation that let this hide.

**Regression cover.** `a_silent_device_times_out_instead_of_waiting_forever`
and `the_callers_budget_reaches_the_park` (`kernel/virtio`),
`a_silent_device_fails_closed_with_device_offline` (`virtio_blk`),
`the_park_is_told_the_deadline_the_loop_is_bounded_by` (`kernel/irq`), and
`an_unconfirmed_gated_run_reports_the_silence_that_diagnoses_it`
(`tools/qemu`).

---

## D41 — root-unlock login vertical failed once under a loaded gate

Status: **open, unreproduced, not diagnosed**. Observed once during a
`cargo xtask ci` run whose QEMU verticals overlapped the pipeline's own image
builds; a second and third full run of the same pipeline on the same tree
passed, and the vertical is green now.

Symptom, from the guest log the harness dumped:

```
id=4138 root-unlock: gave up; no users database installed (reboot required)
        cause=console_unreadable
id=4139 root-unlock: gave up fail-closed; login refused until reboot
id=5004 syscall rejected ... comm=login sc=users_db_read err=12
id=5004 syscall rejected ... comm=login sc=fs_open err=12
id=10006 console error task=8 stage=username errno=7
```

The interesting part is `cause=console_unreadable` alongside `users_db_read`
and two `fs_open` calls all refused with the same errno: login concluded no
users database is installed *because it could not read the console*, and then
fell closed. Whether the console read failed first and the database reads are
its consequence, or all four share one cause, is exactly what is not yet
known.

**This is not to be closed as a load flake.** A wall-clock or readiness
window that is met on an idle host and missed when the pipeline is also
compiling is a load-dependent defect, not an environment blip, and a green
re-run is not evidence it is gone. The fix is structural — a budget sized to
the work, a completion signal, or bounded concurrency so guests do not
oversubscribe the host — never a retry.

**Next step.** Reproduce deliberately under host load (run the vertical while
the host is saturated) rather than waiting for it to recur, then follow the
refused `fs_open`/`users_db_read` errno back to which admission actually
denied it. It carries a regression test when the fix lands.

Suspected-unrelated to the font work that surfaced it (fonts changed the
image payload and `login` is what starts the font service, so the coupling is
worth ruling out first rather than assuming).

---

## D23 — the debug FIQ self-sample corrupted the exception-return window

Status: **done**. Reported as a hard lock while running the desktop on
`images/tairix-aarch64-rpi-debug.img` under `qemu-system-aarch64 -M virt`
with four vCPUs, ~18 s after launching a second `files.app` window:

```
id=4082 cpu hard lockup detected cpu=3 observer=1 stalled_ms=10044
        context=kernel sampled=pre_silence
id=4085 cpu lockup diagnostic detail cpu=3 observer=1 pc=+0x00000000001ee840
        pstate=0x0000000060000385 k_site=user_switch k_seq=38818
        k_lock=kernel/sched/cfq/src/scheduler.rs k_lock_line=753
        k_lock_state=held k_bt=+0x00000000001ee840
id=4084 cpu lockup recovery requested cpu=3 kind=hard outcome=attention
```

**Reading the record.** `pc=+0x1ee840` resolves (image base `0x80000`) to
`tairix_aarch64_trap_common+0xb0` — the instruction *two* past the
`msr SPSR_EL1` in the trampoline's return epilogue. `pstate` decodes to EL1h
with `I`/`A`/`D` masked and **`F` clear**, so the only asynchronous exception
that could be taken there was an FIQ: on this board the boot probe reports
Group 0 deliverable, and the sync handler clears `DAIF.F` so a wedged core
can be sampled. The record is therefore not a coincidence — it is the
sampler catching itself in the act, one instruction before the damage.

**Mechanism.** `ELR_EL1` and `SPSR_EL1` are single-copy: taking an exception
overwrites both. The epilogue programmed them and then ran ~40 further
instructions (`SP_EL0`, FPCR/FPSR, `q0`–`q31`, the GP restores, `add sp`)
before its `eret`. An FIQ in that window returns through its own handler,
which restores *its* saved pair, so the victim's `eret` resumes the epilogue
itself at EL1 with the frame already popped: each turn restores garbage from
the stack above and adds another 816 bytes to `sp`, walking off the kernel
stack until a load faults, and the fault then recurs with `DAIF` masked — a
silent, unrecoverable wedge that never reaches the panic printer, which is
why the log ends without diagnostics.

**Reachability is narrow and exact.** Only a `watchdog-diagnostics` build on a
single-Security-state GIC routes the cadence to FIQ, so the window is live on
the **debug image under QEMU** and nowhere else: a shippable image never
clears `DAIF.F` in the kernel, and a real Pi 4's GIC-400 probes `Unsupported`.
The QEMU integration verticals do not enable the feature either, so D15's
freeze is *not* this defect. D16's Pi-4 wedge shares the `k_site=user_switch`
breadcrumb but not the cause (it is I-cache/fault-handling, fixed there).

**The sibling ports are structurally unaffected**, so there is no common
logic to hoist: x86_64's `iretq` consumes its return state from the *stack*,
which a nested interrupt pushes below rather than overwriting, and riscv64's
epilogue restores `sstatus` (whose `SIE` is clear in every saved frame)
*before* `sepc`, with the syscall body re-masking on the way out and no
non-maskable channel wired. The hazard is specific to a return state held in
single-copy system registers that an asynchronous exception also writes.

**Fix.** Both of the port's `eret` sequences now close the window: the
trampoline epilogue (`vectors.s`) and the EL0 entry (`userentry::enter_el0`)
`msr DAIFSet, #0xf` before programming the return state. `eret` reloads PSTATE
from `SPSR_EL1`, so the mask never reaches the resumed context and EL0 still
runs preemptible. The masked span is straight-line, lock-free and MMIO-free,
so the sampler loses no coverage that could ever wedge (`plans/WATCHDOG.md`
B4).

**Regression cover.** `kernel/arch/aarch64::exceptions::eret_tests` pins the
ordering against both sources — mask before the `ELR_EL1`/`SPSR_EL1` write,
nothing re-enabling before the `eret`. Verified to fail on the pre-fix source
and pass after. The race itself cannot be entered deterministically from a
target test, and the assertion is the source-level invariant that makes the
sequence correct, so the source pin is the regression guard rather than a
QEMU vertical.

---

## D40 — a mutating memory syscall re-froze the whole address space

**State:** closed. Every syscall and fault path that knows which pages it
changed now publishes those, and only the two batch paths that genuinely
cannot name them re-freeze.

**Mechanism.** The registry holds a frozen `Send + Sync` snapshot of a task's
mappings for the user-copy path. `AddressSpace::freeze` rebuilds it by walking
the page table and allocating a fresh `BTreeMap` node for **every resident
page of the task**, and `tairix_kalloc` places a node by scanning its free
list (`carve` first-fit, `insert_hole` sorted insert). A wholesale re-freeze
therefore costs *resident pages × hole count* — the "O(N²), tens of seconds
under emulation" the `note_faulted_page` doc already named for the fault path
— and the kernel is non-preemptible, so the CPU's dispatch loop makes no
progress for the whole call.

**Report it explains.** A desktop under QEMU `virt` (4 vCPUs, aarch64 debug
image) froze with cpu 0 inside a single `mem_unmap`: `k_site=syscall
k_detail=0xf` (`MEM_UNMAP`), `k_seq` identical across two reports 0.73 s apart
(one call, not a loop), `stalled_ms=10000 context=kernel`. The soft record
carried no `observer`, so it came from `check_stall` on cpu 0's **own** timer
tick — the core was still taking maskable interrupts while making no dispatch
progress, which is a long in-kernel computation, not a lock or a wedge.

**Fix.** Every path that knows *which* pages changed publishes them as
in-place deltas (`AddressSpaceRegistry::note_faulted_page`) instead, through
one pair in `kernel/core/src/syscalls.rs`: `publish_region_mapping` (resolves
each page's `(frame, flags)` from the live space) and
`publish_region_teardown` (removes them). A snapshot that cannot absorb a
delta falls back to the wholesale re-freeze, so the delta is never a
correctness dependency.

Who publishes what: `mem_unmap`, `file_unmap`, `shm_unmap` and `dma_free`
drop the region they released; `shm_create`, `shm_map`, `mmio_map` and
`dma_alloc` publish the region they mapped; the anonymous, file-backed and
compressed-page faults publish the one page they backed, and stack growth the
range it committed. `mem_map` and `file_map` publish **nothing** — a
reservation commits no frame and writes no page-table entry, so the snapshot
is unchanged by construction — and `shm_grant` / `call_grant` publish nothing
because they mint a grant and map no page (the earlier *Remaining* list named
these three in error). `sharedreg::unmap`, `DmaPool::free_at`,
`LiveUserSpace::free_dma` and `DmaAllocFacility::free` gained a released-length
return, because those were the only two releases whose caller did not already
hold the extent.

Only the two genuinely unnameable batches still re-freeze: the ramzip
warm/cluster restore and the direct-reclaim sweep, each of which moves several
pages at once and reports no list.

Removing by delta also closes a **fail-open** hole: the wholesale re-freeze is
a documented no-op when no live space is published on the current CPU, so a
released region's pages stayed translating in the snapshot the copy path
walks — reachable memory the task no longer owns, whose frames the allocator
is free to hand to another task. The regression test
(`mem_unmap_drops_the_released_pages_from_the_snapshot`) drives exactly that
case and fails on the pre-fix source with "a released page must not stay
reachable through the snapshot".

**What the mapping half cost, and where it showed.** These were staged as
"one-shot window setups", which underrated them: the desktop session maps a
frame region for **every window an app opens**, so a `terminal.app` context
menu — a popup window — paid four of these (the app's `shm_create` and
`shm_grant`, the session's `shm_map`, then both unmaps) against the largest
address space on the machine. That is the ~300 ms per menu open and close
reported on a Pi 4B, and it is invisible under QEMU because the session's
resident set there is a fraction of a 1080p one. Reading the same class found
two more instances: `resolve_file_fault` re-froze per faulted page, making an
N-page file mapping O(N²) to read (the very hazard the anonymous path's delta
existed to avoid), and stack growth re-froze after committing a range it had
just computed.

**Regression cover (mapping half).**
`shm_map_and_unmap_publish_only_the_regions_own_pages`: a task with 64
resident pages maps and unmaps a one-page shared region and must end with a
snapshot of exactly three pages and **zero** whole-space freezes. Fails on the
pre-fix source with `(true, 67)` — the whole resident set the rebuild
imported.

Also unfixed, and **separate**: the same report's `id=4082 cpu hard lockup
detected cpu=0 observer=1 … sampled=pre_silence stuck_irq=77` is a
**misclassification**. cpu 0 was demonstrably still taking maskable
interrupts, so it was not silent to interrupts at all; only its Group-0/FIQ
liveness cadence had stopped. `DAIF.F` is masked by exception entry and is
re-cleared only on a *sync* entry, so an interrupt that preempts an EL0 task
carries the mask across the context switch into the dispatcher and every
in-kernel body it then runs — the debug sampler goes blind there, `last_seen_ns`
goes stale, and the buddy detector reports a hard lockup with a `stuck_irq`
story read live from the GIC that has nothing to do with the real stall. The
detector is honest about what it measured; the *channel* it measures is not
always deliverable. Fixing this means re-establishing the probed FIQ posture
after an interrupt-driven preemptive switch (aarch64 `preempt`/`kthread` switch
path), so a soft stall can never be dressed up as a hard lockup.

---

## D25 — a nested reader on the address-space registry wedged three CPUs — DONE

**State:** fixed. `terminal_size`'s pty-slave arm held an `aspaces` **reader**
across `with_caller_aspace`, which takes a second reader on that same lock.
`tairix_sync::RwLock` is writer-preference — `read()` blocks while
`pending_writers > 0`, and `write()` registers its intent *before* draining
readers — so the inner acquisition is refused the moment any other CPU calls
`aspaces.write()`, and the outer guard it is nested inside is exactly what that
writer waits for. Neither side can ever be granted, and every later
`aspaces.read()` (`stream_read`'s among them) queues behind the pending writer.
A `RwLockReadGuard` is a value with a `Drop` impl, so the outer borrow lives to
the end of its block, not to its last use.

**Report it explains.** A desktop under QEMU `virt` (4 vCPUs, aarch64 debug
image) froze with cpu 0 in `k_site=syscall k_detail=0xd` (`STREAM_READ`) and
cpu 3 in `k_site=user_switch`, both with `k_seq` identical across the soft and
hard records — one call each, no loop. `k_lock=scheduler.rs:753 k_lock_state=held`
is *not* diagnostic: that is `task.body.lock()`, legitimately held for the whole
off-CPU lifetime of any parked task. The accompanying `stuck_irq=77
stuck_state=pending` (the virtio mouse, mmio slot 29) is a consequence: every
device SPI is routed to cpu 0 alone (`CPU0_TARGET`), so a wedged cpu 0 leaves
its lines asserted and untaken. As under D40, the hard-lockup label and its
live-GIC `stuck_irq` story are the misclassification described there, not the
mechanism.

**Fix.** The arm takes the owned geometry bytes and releases the reader before
the copy-out, so no acquisition of that lock nests. An audit of all 25
held-guard `aspaces` sites found this to be the only nesting and no AB-BA cycle
(`record_fault_exit` already drops its `aspaces` reader before taking `caps`);
the ~186 immediate-drop `self.aspaces.read().method()` forms cannot nest by
construction.

**Why it hid.** `RwLock` reported nothing to the lockup watchdog, while
`SpinLock` publishes its whole acquire/hold/release lifecycle, so a CPU
spinning in `read()`/`write()` was invisible and the report named a stale
spinlock site instead. `RwLock` now mirrors `SpinLock` through the same
`lockwatch` seam, and its rustdoc states the recursive-read prohibition.

**Also fixed, same path.** `parked_stream_read` polled before registering on
the stream wait-queue, and a stream wake latches nothing — a peer producing
bytes between the poll and the registration woke nobody, so the reader parked
on data that had already arrived. It now registers before the first poll and stays
registered until the loop exits, matching `BlockingConsoleRead::read_until`.

**Regression cover.** Six `lib/sync` tests pin the grant/refuse semantics and
guard-drop release that make nesting fatal. Neither interleaving is reachable
from a host test — there is no controllable point between the two acquisitions,
nor between the poll and the registration — and a timing-based thread test
would be the load-dependent flake the charter forbids, so the source-level
invariant is the guard, as for D23.

---

## D26 — a mouse scroll produces no input event at all

**State:** open. Diagnosed while tracing D25; not a lockup, a functional gap.

**Mechanism.** QEMU's HID mouse reports wheel motion as `EV_KEY` with
`BTN_GEAR_UP`/`BTN_GEAR_DOWN` (`0x150`/`0x151`), not as `EV_REL`/`REL_WHEEL`.
`PointerInput::from_device_event` accepts only the contiguous pointer-button
range (`0x110..0x113`), and `VirtioKeyboardConsole::feed` has no mapping for
those codes either, so the `virtio_kbd` pump's `pointer_inject`-else-`key_inject`
pair rejects both ways and the event is discarded. `lib/virtio_input`'s
`decode_event` *does* map `REL_WHEEL` to `Scroll`, so the vocabulary is not the
gap — the device's actual encoding never reaches it. Horizontal wheel never
arrives at all: QEMU drops it host-side (`unmapped button: 7 [wheel-left]`).

**Fix direction.** Map the gear-button codes onto the existing `Scroll` event
in the one shared device-event decode, so a wheel reaches the seat and the
compositor by the same path a wheel over `REL_WHEEL` already would; no second
decode and no new vocabulary. Belongs with the display/input work
(`plans/DISPLAY.md`), with a decode unit test per encoding.

---

## D27 — ARXFS has no persistent deduplication index

**State:** open, correctness-safe.

**Mechanism.** The dedupe index (`drivers/filesystem/arxfs/src/dedupe.rs`) is
an in-RAM bounded LRU cache keyed by `(domain, length, logical hash)`, warmed
only by the writes of the current mount. The chunk tree it is checked against
is keyed by physical block, not by hash, so a hash lookup cannot fall back to
it. A duplicate written in an earlier mount session is therefore not found
until the cache warms again in the new session — reduced cross-mount
deduplication effectiveness, never a wrong merge (a missed duplicate is
correctness-safe; the data is simply stored twice, `arxfs-spec.md` §9).

**Fix direction.** A persistent, hash-keyed dedupe tree committed in the
transaction root, so a lookup survives a remount without walking the chunk
tree. Structural and larger than a single change: a new authoritative
on-disk structure, not an extension of the existing rebuildable cache.

---

## D28 — ARXFS per-transaction deferred-free and pending-mark sets are unbounded

**State:** open, pre-existing.

**Mechanism.** `txn_freed` (`drivers/filesystem/arxfs/src/allocator.rs`, a
`BTreeSet<u64>`) holds every block a transaction releases until it commits,
and the allocation map's `pending` map holds every bit change whose page or
summary block was not resident when the change was made. Both scale with the
size of a single transaction, so deleting a very large file allocates memory
proportional to the file's block count. This is pre-existing shape (the
previous free-space tracker's `Vec<u64>` had the same property) and conflicts
with the small-RAM/large-volume floor (`AGENTS.md` §26.7).

**Fix direction.** Extent-based deferred freeing (a run of contiguous blocks
recorded as one `(start, length)` entry) rather than per-block, bounding the
bookkeeping by the number of runs a transaction touches rather than the
number of blocks.

---

## D29 — a CPU-bound user task was never sampled, so a healthy core was reported hard-locked

**State:** done. Reported from the field: opening the Switchboard window on the
`virt` debug image "often" produced a lockup record in the debug log.

**Mechanism.** The debug image's liveness cadence is delivered as a Group-0
**FIQ** (the probe answers `Supported` on the single-Security-state `virt` GIC,
`plans/WATCHDOG.md` B2), but the port entered EL0 with `DAIF.F` **set**. A task
running in user mode therefore could not take the cadence at all: the FIQ could
only land during a kernel entry, so a core executing a CPU-bound user task went
unsampled for as long as the task ran. `last_seen_ns` kept the stamp of the last
kernel entry, aged past the 10 s hard threshold, and a buddy reported `id=4080`
→ `id=4082` → `id=4084` against a core that was demonstrably alive and taking
thousands of IRQs. Because the stale sample also froze `wd_ctx_in_kernel`,
`k_site`, `k_bt` and `k_lock` at that unrelated kernel entry, the record read
exactly like a real kernel wedge — the field report's `k_site=syscall`,
`k_lock=…/cfq/src/scheduler.rs:753 k_lock_state=held`,
`sampled=pre_silence` were all stale, not the cause. Two further consequences:
the soft detector mis-fired for the same reason (`classify` reports a stall only
for a CPU *last seen in the kernel*, which a rotting flag satisfies), and
`monopolises_cpu` — the guard against a task withholding the CPU, which fires
only on a *user*-context sample — was unreachable on the one configuration that
has the sampler.

Nothing about the Switchboard is special: any user task that stays in EL0 for
~10 s does it. The window's first paint (glyph rasterisation, chart and icon
drawing) is simply long enough under TCG.

**Why "often" and not "always" — the tickless interaction.** The trigger is a
core running a *lone* runnable user task. Being tickless, the scheduler disarms
the preemption one-shot when a task is the only runnable one on its CPU, so that
core takes **no** kernel entry at all and the pending cadence FIQ has no window
to land in. Put several runnable tasks on the same core and every preemption tick
is a kernel entry that lets the FIQ through, so the cadence still lands and
nothing is reported — measured: `stress --cpu 20` (20 spinners over 4 vCPUs) is
**clean even before the fix**, while `stress --cpu 1` reports 4/4. That is why
the defect looked intermittent and why it is the *idle-ish desktop* case — one
busy app, everything else parked — that shows it.

**Fix.** `kernel/arch/aarch64/src/userentry.rs` decides the EL0 entry `SPSR`
once, from the boot probe: `el0_spsr(fiq_cadence)` clears `DAIF.F` when
`watchdog::fiq_cadence_enabled()` is true, and is otherwise the unchanged
F-masked value — so a shippable image (no FIQ routed at all) and a board whose
probe answered `Unsupported` behave exactly as before (fail closed). Every later
return to EL0 restores the `SPSR` this entry established from the frame
`vectors.s` saved, so there is one definition of the EL0 mask state.
`plans/WATCHDOG.md` B1's "the EL0 `SPSR` stays F-masked (nested-FIQ-unsafe)" was
an over-generalisation from the two windows where nesting is genuinely unsafe
(`halt_current_cpu`, the FIQ arm itself) and is corrected there: EL0 is not
inside an FIQ handler, the FIQ vector runs on `SP_EL1` with F re-masked by the
PE, both `eret` sequences already mask asynchronous exceptions before
programming the return state (D23/B4), and interrupted user code holds no kernel
lock — an EL0 sample is strictly safer than a kernel-section one. The
diagnostic path needed no change: a non-kernel `pc` is omitted rather than
disclosed raw, and the frame walk rejects a user return address and a
below-floor frame pointer, so an EL0 sample yields one honest entry and cannot
fault.

**Evidence (A/B on the same tree, two kernels differing only in this
condition).** `stress --cpu 1 --timeout 30s` after a scripted unlock + login on
the 4-vCPU `virt` debug image: **before**, 4/4 runs produced the reported
`4080`/`4082`/`4084`/`4085` set, and a per-CPU FIQ-delivery census (QEMU `-d
int`) showed the spinner's core taking **1** sample while its idle siblings took
~46, with thousands of IRQs delivered to it throughout — the core was never
wedged; **after**, 10/10 runs clean. A live register dump during the reported
"lockup" showed the accused core in `EL0t` with `PSTATE.I` clear and its PC
advancing. `stress --cpu 20 --timeout 40s` is clean on **both** kernels, for the
tickless reason above.

**Regression cover.** `userentry`'s host tests pin both `SPSR` values, that only
the F bit differs between them, that EL0t/IRQ-unmasked/SError+Debug-masked hold
either way, and that an unprobed or shippable build keeps F masked. The
`fiq_selfsample_qemu_aarch64` vertical additionally asserts on the real board
that a `Supported` probe leaves the EL0 entry state F-clear, so the two cannot
drift apart again.

---

## D30 — the pinned-bar screendump was captured before the panel was painted — DONE

`tairix-test-taskbar-pin-qemu-aarch64` now passes in ~22 s.

**Not a geometry defect.** Both sides already agreed: the Switchboard asks only
for a *size* (`Desktop::window_size`), and the session alone places the window
through the one shared `cascade_origin_for` rule the assertion also reads. The
panel was destined for exactly the slot the checker sampled.

**The real cause — the same shared-rendezvous ordinal D31 names.** The guest
announced "panel created and painted" on the *second* reply served over
`WINDOW_ENDPOINT`, a count whose doc claimed it was "a sequence position, not
an open-ended tally of somebody else's traffic". That stopped being true when
the Switchboard gained a start-up `QueryDesktop`: the sequence became
query, create, present, so the marker fired on the **create** — one full round
trip before the panel had drawn anything — and the screendump caught an empty
cascade slot on an otherwise passing guest.

**Fix.** The witness is anchored, not counted. The guest recognises the panel's
own **create** reply by its distinctive wire length
(`WINDOW_CREATE_REPLY_LEN`), and the reply after it completes the present that
first drew the panel. No call added ahead of create — by this client or any
other sharing the rendezvous — can move the gate.

**Diagnosability defect fixed alongside.** The register's own "next step" asked
for a serial log the runner could not produce: `Outcome::Pass` discarded the
transcript, so a *screendump* assertion failing after a passing guest reported
a pixel ratio and nothing else. A pass now carries its transcript like every
other outcome, and the matrix persists it whenever a dump assertion or a link
peer's verdict fails.

---

## D31 — a QEMU vertical whose guest stays chatty ran unbounded — DONE

Two independent defects; both fixed. `tairix-test-autoload-input-qemu-aarch64`
now passes in ~26 s.

**1. The stalled choreography: a gate another component could satisfy.** The
in-window click waited for "a reply over `WINDOW_ENDPOINT`", but every client
of that shared rendezvous replies on it. The Switchboard's start-up
`QueryDesktop` (`userland/gui/switchboard` asks the session to describe the
desktop before it sizes anything) fires that gate ~0.5 s before the files
window is created, so the click landed on bare desktop, no window event was
ever delivered, and every later stage — which counted *system-wide*
`MessageDelivered` records — could never advance.

The AW3 stage is now on the same footing D19/D20 put the terminal stage on:
every gate names its own subject. The click waits on the files window's own
**frame map** (`FILES_WINDOW_FRAME_MAPS`; only a window *create* maps a frame,
so no query, present or reply can advance it), and the two former cumulative
counts are guest markers the test kernel emits from the destination **port** of
each delivery (`FILES_WINDOW_ACTIVATED_MARKER`, `FILES_HANDSHAKE_MARKER`), so
another app's or service's traffic cannot move them. No cumulative
`MessageDelivered` threshold remains in the vertical.

**2. An inactivity budget cannot bound a run.** `Spec::timeout` is the longest
a guest may fall *silent*; a guest that keeps printing resets it forever, so a
stalled choreography degraded into an unbounded pipeline hang instead of a
failure — here the desktop's own ~1 Hz refresh was enough. Every run now also
carries an absolute wall-clock ceiling (`Spec::runtime_ceiling`, twice the
declared budget, so each test still declares one number) and reports
`Outcome::RuntimeCeilingExceeded` with the silence at the kill, which
distinguishes a live-but-unfinished guest from one that stalled and went
quiet. The parallel runner also prints every job's completion and duration, so
an outstanding job is visible in the log rather than inferred from its absence.

---

## D32 — CPU 0 never returned to the dispatch loop, so every deferred wake stranded and the desktop froze (OPEN)

**State:** open. Observability and the recovery path have landed; *why the
non-maskable cadence stopped on CPU 0* is not yet determined. Reported from the
field on the aarch64 `virt` debug image, ~150 s into ordinary desktop use.

**Symptom.** The desktop stops responding — no keyboard, no pointer, nothing
repaints — while the log shows a soft stall and then a hard-lockup set against
CPU 0 alone:

```
[169.385] [ERROR] id=4080 cpu stall detected cpu=0 stalled_ms=10000 context=kernel
[169.386] [ERROR] id=4085 cpu lockup diagnostic detail cpu=0 pc=+0x1fae90 pstate=0x20000305 k_site=user_switch k_seq=1181763 k_lock=kernel/sched/cfq/src/scheduler.rs k_lock_line=753 k_lock_state=held
[169.475] [ERROR] id=4082 cpu hard lockup detected cpu=0 observer=2 stalled_ms=10089 context=kernel sampled=pre_silence stuck_irq=77 stuck_state=pending stuck_owner=0x9
[169.476] [WARN]  id=4084 cpu lockup recovery requested cpu=0 kind=hard outcome=attention
```

This is a **real** user-visible hang, not the D29 false positive: that class
reports a core that is demonstrably alive on a machine that stays responsive.
CPUs 1–3 are healthy throughout, but every device SPI is routed to CPU 0 alone
(`CPU0_TARGET`, `kernel/tairix-kernel/src/aarch64/gic_irq.rs`), so a CPU 0 that
stops servicing input freezes the whole session.

**Proven: CPU 0 was alive and taking interrupts.** `id=4080` and `id=4085`
carry **no `observer=` field**, and that identifies the emitter: the summary and
detail renderers emit `observer` only when it is `Some`, `scan` always passes
`Some(observer)`, and `check_stall` passes `None`. `check_stall` is reached only
from the per-arch tick dispatcher — on aarch64 `production_tick_dispatch` via
`handle_irq` → `preempt::on_timer_interrupt` → `TimerHal::dispatch_tick`. CPU 0
therefore took, dispatched and serviced timer PPI 30 at the moment it was
reported stalled.

That arithmetically **excludes an un-EOI'd interrupt**: every enabled line sits
at `MID_RANGE_PRIORITY`, so anything left active would have blocked PPI 30 too.
It also excludes the D13 ISR-shared-`SpinLock` theory — an exhaustive audit found
no plain `SpinLock` reachable from both an ISR and a syscall path (the one
genuinely shared structure, the console RX ring, is correctly gated by the
`IrqSafeSpinLock` `UART_RX_GATE`), and the record says `k_lock_state=held`, not
`acquiring`. An audit of the userland GUI event loops likewise found no
busy-poll.

**Mechanism.** CPU 0 dispatched a task (`k_site=user_switch`, the CFQ body lock
held by design across the whole user run at `kernel/sched/cfq/src/scheduler.rs`
:753) and **never returned to `run_dispatch_loop`**. Both liveness heartbeats
froze within 1 ms of each other at the last dispatch-loop iteration, because
`note_progress`/`note_alive` are stamped only by that loop. Every
interrupt-context wake is deferred by design — the ISR only flags
(`IrqTable::fire` sets `ready`, `WaitQueue::request_wake` sets `wake_pending`)
and the real `wake_all`/`unpark` happens in `drain_pending_wakes()`, **which runs
only from the dispatch loop**. So while CPU 0 stayed out of the loop no deferred
wake was ever delivered: the `virtio_kbd` owner parked on `IRQ_WAITQ` (task 9)
was never unparked, no input reached seatmgr/wm, and the desktop froze.
`stuck_irq=77 stuck_owner=0x9` is the *symptom* of that stranded owner, not the
cause. `sampled=pre_silence` on the `id=4082` record is honest; the `id=4080`
record's confident `context=kernel` was **not** (see Fix 2).

**Why nothing forced CPU 0 back — this is the defect.** Two mechanisms could
have, and both were disarmed:

1. The ordinary preempt point is competitor-gated: `reschedule_owed` returns
   `false` with no runnable competitor and no flagged deferred wake, so a lone
   CPU-bound task keeps the CPU by design.
2. The monopoly safety net rode a channel that had stopped. `request_forced_yield`
   had exactly **one** issuer, `on_watchdog_tick` → `monopolises_cpu`, which
   bailed immediately `if in_kernel` — reading `wd_ctx_in_kernel`, a field
   refreshed **only** by a cadence sample. CPU 0's last sample was taken inside
   `Scheduler::dispatch`, so the field **rotted at `true`** and the guard could
   never fire; and with the ~1 Hz cadence dead on that core, `on_watchdog_tick`
   never ran there at all.

The anti-monopoly guarantee was thus suppressed by exactly the condition it
exists to break, while the one path provably still running on the wedged core —
the maskable timer tick — computed the identical "no dispatch progress for 10 s"
condition in `check_stall` and **only logged it**.

**Fix 1 — the forced yield now rides the timer tick (landed).**
`monopolises_cpu` is split: `progress_overdue(state, now_ns)` is the
`Active` + armed + past-threshold half and takes **no** context argument, and
`monopolises_cpu` is `!in_kernel && progress_overdue(…)` for the cadence caller
that holds a fresh reading. `check_stall` calls `request_forced_yield` whenever
`progress_overdue` holds, read **unlatched** and evaluated independently of the
latched soft-lockup report, so an overdue core is pushed back at every tick
rather than once per episode. The forced-yield latch is consumed by the same
interrupt's return-to-user preempt point and is deliberately not
competitor-gated, so the CPU returns to `run_dispatch_loop`, which drains the
pending wakes, unparks the `IRQ_WAITQ` owner, and restores input. It arms no new
timer, so ticklessness is untouched. Recovery now takes ~1 s (the monopoly
window) instead of never.

**Fix 1b — the EL1 case (landed).** A task wedged in EL1 never reaches a
return-to-user preempt point, and `yield_if_owed_on` consumed only the *tick*
latch, so a forced yield could not be honoured there at all. `preempt_current`
and `yield_if_owed` now share one `honour_latches` decision that consumes both
latches, so a monopoly is broken at whichever boundary the CPU reaches first.

**Fix 2 — the diagnostic no longer lies (landed).** `context=kernel|user` was
rendered from `wd_ctx_in_kernel` unconditionally, even when that field was older
than the cadence interval — so `check_stall`'s report printed a confident
`context=kernel` from a field ten seconds out of date. That misreading cost two
wrong diagnoses of this very defect. A context older than the cadence interval
is now marked `sampled=pre_silence`, exactly as `scan` already did for the hard
path, from one shared `context_stale` predicate.

**Also landed (observability).** The `probe_fiq_deliverability` verdict was
discarded with `let _ =`, making an image whose non-maskable self-sample never
ran indistinguishable in the log from one where it worked; it is now reported
once on the boot CPU as `CpuWatchdogSelfSample` (id 4086, debug-only,
address-free). And because `GICD_ISACTIVER0` is banked per CPU — so an observer
reads its *own* SGI/PPI state, never the victim's, and `first_stuck_spi` scans
SPIs only — each CPU now publishes the interrupt it acknowledged into its own
per-CPU slot and clears it at the EOI, rendered as `in_flight` beside
`stuck_irq`. A core wedged inside a banked SGI or PPI will name it instead of
falling through to an innocent pending SPI.

**Open residual — why the ~1 Hz cadence stopped on CPU 0.** Undetermined. This
is a *detection* failure; Fix 1 makes the freeze recoverable regardless of which
candidate is right, but the candidate must still be found:

1. **`DAIF.F` masked for the window (strongest lead).** `el0_spsr` is applied
   only on **first** EL0 entry (`kernel/arch/aarch64/src/userentry.rs`:125), so a
   task first entered *before* the FIQ probe completed carries `F=1` for its
   whole life — D29's unmask is then inert for that task, exactly as if the probe
   had answered `Unsupported`. Worth its own investigation. The new `id=4086`
   record settles the probe half of the question on the next reproduction.
2. **Priority starvation.** The cadence PPI runs at the deliberately lowest
   `WATCHDOG_FIQ_PRIORITY` (0xC0); sustained 0x80 activity could hold it off.
3. **A missed first re-arm** of the one-shot cadence.

D29 is **not** the explanation for this report: that class is a false positive on
a machine that stays responsive, and this one hangs the desktop.

**Regression cover.** Host: `progress_overdue` fires for a CPU whose
`wd_ctx_in_kernel` has rotted at `true` (pinning that the guard no longer depends
on the rotting field); a context older than the cadence interval renders
`sampled=pre_silence`; an overdue CPU reaches the reschedule path from the
tick channel with no competitor and no latched tick; and a forced yield alone
reaches it through the in-kernel boundary too. All three fail before the fix and
pass after.

**Still needed — the QEMU vertical.** Extend
`tests/integration/preempt_el0_qemu_aarch64` with a **lone** CPU-bound EL0
spinner on one core, no other runnable task on that core, plus a second task
blocked in `irq_wait` on a device line; assert that the dispatch-loop progress
heartbeat advances within the monopoly window **and** that the `irq_wait` owner
is woken while the spinner still runs. It must be a *lone* runnable task, or
`reschedule_owed` short-circuits and the test passes vacuously. Do **not**
repurpose `preempt_inkernel_qemu_aarch64` (D24) or `fiq_selfsample_qemu_aarch64`
(D29).

---

## D33 — `waitset_wait` was a fixed priority, so a busy source starved every member behind it — DONE

**Symptom.** The desktop's Switchboard monitor became permanently
unresponsive after scrolling and clicking over its window, and sometimes
before its window was ever opened. It never recovered on its own.

**Root cause.** `waitset_wait` scanned members in registration order and
took the first ready one. Most member kinds are level-triggered peeks that
only the owner's own drain clears, so a source with work outstanding is
ready on *every* scan and held the head indefinitely. The desktop session
registers `SeatInput` first, then the window endpoint, the notification and
Switchboard mailboxes, and the child reaper — and it handles one source per
wake by design (`call_recv` blocks, so it must not touch an endpoint it was
not woken for). A hand on the mouse therefore served input and *nothing
else*, for as long as the input kept coming: applications blocked in a
window call hung, exited children went unreaped, and the mailboxes peers
post to filled until their sends began failing `WouldBlock`.

**Fix.** The wait-set registry keeps a resume cursor (`resume_after`) and
rotates the member snapshot to begin just after the member the previous
wait reported (`waitset::members` / `waitset::note_reported`), so every
ready member reaches the head within one lap. The cursor advances only once
the token has actually reached the caller, so a wait that failed to report
costs the member nothing; a member removed meanwhile falls back to
registration order. Registration order still decides within a lap.

**Regression cover.** `waitset_wait_reports_two_ready_members_in_turn`
(`kernel/core/src/syscalls.rs`) — two endpoints each holding an undrained
request; four consecutive waits must alternate. It reports the same token
four times without the cursor. Registry-level, in
`kernel/core/src/waitset.rs`:

- `a_fresh_set_scans_in_registration_order`
- `reporting_a_member_moves_the_scan_past_it`
- `the_rotation_is_per_kind_as_well_as_per_id`
- `removing_the_last_reported_member_falls_back_to_registration_order`
- `an_empty_set_rotates_to_nothing`
- `note_reported_is_owner_checked`

## D34 — the tray monitor treated a full session queue as a fault and exited — DONE

**Symptom.** The half of D33 that made the freeze *permanent*: the
Switchboard process was gone, and nothing restarts it. The session relaunches
it only from a fresh capsule press that finds no live instance, and a press
while the exit is still unreaped is held as a pending open aimed at a corpse.

**Root cause.** `Service::cycle` counted every non-`NotFound`,
non-`PermissionDenied` publish refusal towards
`MAX_CONSECUTIVE_PUBLISH_FAILURES` (5). A call endpoint at capacity refuses
the post with `WouldBlock` rather than blocking, so a session that had not
drained its queue for five sample periods (10 s — routine under D33) exhausted
the budget and the monitor exited with `PublishFailed`.

**Fix.** `WouldBlock` is excluded from the budget: it is the transient
back-pressure signal, not evidence of a fault or of an absent session. The
summary stays unacknowledged so the change gate re-offers it on the next
sample — one attempt per period, paced by the sampler, never a retry loop.
The two clean exits still catch the genuinely session-less cases, so orphan
detection is unweakened.

**Regression cover.**
`a_session_that_has_not_drained_its_queue_never_stops_the_service` (20
consecutive refused periods, then delivery on the first accepted one) and
`back_pressure_does_not_clear_the_give_up_budget` (a real fault after a
`WouldBlock` still trips it).

## D35 — an app-ward window event was silently dropped when its mailbox was full — DONE

**The defect.** `RtEventSink::deliver` (`userland/gui/session/src/run.rs`)
was one non-blocking `ipc_send`; on `WouldBlock` the session dropped the
event and never retried. That is right for a pure delta (a wheel tick, a
motion sample) and wrong for everything else: `Resized` left the client
hit-testing and rendering at a size the compositor no longer used, with no
second chance until the next resize; `CloseRequested`, `Focus`, `Minimized`
and `DesktopChanged` are state edges with no re-derivation path;
`FilePicked`/`PickCancelled` are one-shot conclusions whose loss left
`WindowServer::pick_pending` set for the life of the window, so that window
could never open another picker. An app did not have to be hung for this:
32 slots is a bounded resource and a slow drain is enough.

**Kernel — `WaitSourceKind::PortRoom` (wire value 10).** The send-side twin
of the `Port` member, so a sender can park on a full destination instead of
dropping or polling. Added by the *send*-authority check `ipc_send` applies
(the caller is the sender, not the binder); an unknown port and one the
caller may not post to give the same oracle-free `NotFound`.

- **Level-triggered, not the edge the original entry proposed.** The member
  is armed *after* a send was refused, so an edge seeded at that moment
  would already have passed if the receiver drained in between — the sender
  would then park forever on an empty mailbox, which is the freeze this
  defect is about. Ready means "a send would not be refused for want of
  room": below capacity, port gone, or send authority lost. The last two
  keep a sender from waiting on something waiting cannot fix, and answering
  ready unconditionally to an unauthorised caller leaks no occupancy.
- **The wake is targeted.** A `Port` records the tasks parked for its room
  (`watch_room`/`unwatch_room`, registered before the first readiness scan
  so a drain in the arming window is not lost), and a *committed*
  `ipc_recv` wakes exactly them. Port teardown broadcasts once, because the
  record dies with the port — the `call_wake`/`call_wake_task` pattern.
  Only a set holding a `PortRoom` member joins the queue.

**Session — the hold-back (`userland/gui/session/src/holdback.rs`).** One
ordered queue per `(destination mailbox, window)`; a destination already
owed something takes the next event unsent, so nothing overtakes what is
queued, and a flush serves an owner's windows round-robin so one window's
backlog starves no sibling. Folding is by what each quantity means: a state
edge replaces the held one in place (at most one of each per window), a
position is latest-wins, a wheel run sums until it reverses (the same
`shell::continues` predicate the live drain uses), and keys, buttons and the
pick conclusion are owed in full. `HOLD_BACK_CAPACITY` (64/window) is a
security bound, not a scalable capacity: overflow sheds the oldest *input*
event, which is total because folding leaves at most six edges and one
conclusion, and safe because a press is shed before its release. Not fixed
by enlarging `EVENT_MAILBOX_CAPACITY`.

`EventSink::deliver` now takes the typed `WindowEvent` rather than its wire
bytes, because only the sink knows whether an event goes out now, and a
held one must fold by kind and encode once when it finally goes.

**Regression cover.** `a_resize_and_a_pick_conclusion_survive_a_full_mailbox`
and `a_later_event_never_overtakes_one_already_owed`
(`userland/gui/session/src/holdback_tests.rs`) both fail before the fix, on
the dropped event and on the reordering respectively; 14 further tests cover
the folding, the bound, the shed order, and the flush outcomes.
`a_conclusion_the_sink_refuses_stays_pending_until_one_is_accepted`
(`lib/window/src/tests.rs`) pins the `pick_pending` protocol the drop
stranded. `waitset_wait_reports_port_room_once_a_drain_frees_a_slot`
(`kernel/core/src/syscalls.rs`) covers the authority gate, quiet-while-full,
ready-on-drain, no waiter record left behind, and the two always-ready
cases; `room_tracks_the_capacity_send_refuses_on` and
`room_waiters_are_recorded_once_and_forgotten_on_request`
(`kernel/ipc/src/port.rs`) cover the port itself.

## D36 — the shared stroke path never converged, so a graph reading wedged its own process — DONE

**Symptom.** The Switchboard monitor stopped updating, one core went to
100%, and the desktop's own hang detector flagged it as not responding. It
never recovered. Distinct from D33/D34: the *desktop* stayed healthy
throughout and the verdict was correct — the monitor really had stopped.
Not input-related, and no interaction is needed to reach it.

**Root cause.** `Surface::stroke_polyline` (`lib/raster/src/surface.rs`)
scaled each segment's perpendicular by the segment's length, and that
length came from a private Newton iteration that terminated on
`while x != prev`. For every `n = m² − 1` the iteration reaches a two-value
cycle (`m`, `m − 1`) and the successive estimates never agree, so the loop
runs forever — 315 such values below 100 000, and a squared segment length
lands on one for a whole family of ordinary slopes (a (2, 2) step is
already `8 = 3² − 1`). The loop issues no syscall, so the task is a lone
runnable CPU burner: nothing to park on, nothing to drain, no wake to miss.

The monitor draws a live `Chart` per resource on its Tasks, System and
Background sections. Its trace steps by whatever the last two readings
differ by, so every 2 s sample is a fresh chance to hit a bad length — the
observed "it just stops eventually, sometimes without touching it". The
same primitive draws window-furniture diagonals, so any window with a close
button was exposed at the sizes whose glyph geometry lands on one.

The justification comment for the hand-rolled helper ("the workspace
minimum Rust version predates `i32::isqrt`") had gone stale: the pinned
toolchain is 1.96.

**Fix.** The helper is deleted. The length is `u64::isqrt` over a widened
sum of squares — bounded by construction, and correct where the old `i32`
accumulation saturated (a segment longer than about 46 340 sub-units
measured short, so its perpendicular came out proportionally too large and
a hairline painted as a band tens of pixels wide). The divisor is a
`NonZeroU64`, so the zero-length case is discharged once at the top rather
than guarded inside the offset arithmetic.

**Regression cover.** In `lib/raster/src/tests.rs`:

- `a_stroke_of_any_slope_draws_and_terminates` — strokes every step in
  ±24 × ±24 and asserts a whole-pixel step always leaves a mark. Hangs
  before the fix (first bad step is (−18, −6), `18² + 6² = 19² − 1`).
- `a_stroke_longer_than_the_surface_keeps_its_weight` — a 4-million-unit
  diagonal must not reach the far corners. Fails before the fix.
- `a_stroke_needs_two_points_and_a_positive_weight`.

In `lib/controls/src/chart_tests.rs`,
`every_reading_plots_at_every_width` reproduces the field failure through
the instrument that hit it: every reading 0–1000 at every box width 9–20.
It hangs before the fix and takes 0.57 s after.

**Related.** This is exactly the task shape D32's forced-yield fix exists
to break — a lone runnable task that never returns to the dispatch loop —
and it confirms that a userland spin is reachable in practice. It is not
D32 itself: the desktop and the other cores stayed live here, because a
user-mode spinner is preemptible and only the wedged process is lost.

## D37 — riscv64 appears to save no floating-point state (OPEN, unconfirmed)

**Symptom.** None observed yet. This is a defect noticed by reading the
code (§2.18), recorded rather than left silent; it is not a field report.

**Suspected cause.** `riscv64gc-unknown-none-elf` mandates the `D`
extension and is a hard-float ABI, so both kernel and user code may emit
`f`/`d` instructions — and `lib/raster`'s gradient sampling (`paint.rs`)
uses `f64`, so a graphical riscv64 task does. But neither
`kernel/arch/riscv64/src/trap.s` nor `kernel/arch/riscv64/src/context.s`
contains a single `fsd`/`fld`, and no `mstatus.FS` handling was found
anywhere in the port. Exactly one of two things must therefore be true,
and both are defects:

- `mstatus.FS` is `Off`, so the first FP instruction faults with an
  illegal instruction — a user task drawing a gradient is killed; or
- `mstatus.FS` is on, and two tasks silently corrupt each other's
  `f0`–`f31`/`fcsr` across every context switch (§4 isolation).

**Confirmation procedure (do this first; do not guess).** Read the
riscv64 boot path for any `mstatus.FS` write; then a QEMU vertical that
(a) executes an `f64` computation in one user task and asserts the result
rather than a fault, and (b) runs two user tasks whose interleaved `f64`
state must not mix. The outcome names which of the two branches above is
real.

**Fix (once confirmed).** Per-port FP/vector context save/restore behind
the Arch HAL context-switch slice (§17.2), with `mstatus.FS` dirty
tracking so a task that never touches FP pays nothing, and an Arch HAL
conformance case proving two tasks cannot observe each other's FP state.
The same slice is what x86_64 needs before user space can have SSE
(`plans/FIX-DESKTOP-SPEEDUP.md` Stage G); do it once for both ports
rather than twice (§2.21).

**Regression cover (lands with the fix, §7).** The two QEMU cases above
plus the Arch HAL conformance case, on every port that reports FP
support — never closed on inspection alone.

## D38 — the nightly soak killed every filesystem soak, and a memtest sweep mid-progress — DONE

Two independent wall-clock defects in the soak tooling, both found in the
same seven-hour nightly run (`soak.yml`, run 31344475389); both fixed.

**1. A soak child was given an ordinary step's deadline.** All four
`fssoak` jobs died at 46 minutes with
`fssoak <fs> (25200 s) exceeded its 2700s timeout and was killed`, having
written a full `test result: ok` line seconds earlier — the soak was
*working*, and the orchestrator killed it for taking the time it was told
to take. `fssoak::run` handed each `cargo test` child `Context::run`,
whose budget is the 45-minute ordinary-step allowance, while exporting a
seven-hour budget to that same child: any budget above 45 minutes was
therefore unreachable by construction, so the nightly could never have
passed. The fuzz and proptest orchestrators already had the right rule in
`parallel::Job::with_soak_budget`, which is why only `fssoak` failed.
Fixed by lifting that rule into one shared `soak_deadline(budget)`
(`tools/xtask/src/main.rs`) — the budget plus an ordinary step's
allowance, saturating — and routing both `with_soak_budget` and
`fssoak::run` through it, so a fourth orchestrator cannot reintroduce the
divergence. The budget/device-size environment names now come from
`tairix-fuzzseed` too, where the fuzz/proptest names already live, so the
side that exports and the side that reads cannot drift.

**2. A soak loop overran its budget by a whole pass.** The harness
checked the deadline only *after* an iteration, so it always started one
more pass than fitted — on a 1 GiB volume that is 28 s (arxfs) of
overrun, and it is the orchestrator's kill deadline, not the budget, that
ends such a run. The loop now starts another pass only while a pass of
the last one's length still fits, and the two near-identical per-target
loops (`run`/`run_random`) collapsed into one that takes the exerciser as
a function pointer, since the budget arithmetic is what must not diverge.

**3. A progressing memtest sweep was killed by a ceiling that described
no part of it.** `supervisor-memtest-takeover-qemu-{aarch64,riscv64}`
both failed at 120.03 s while their guests were sweeping normally (49% of
RAM, zero errors), which failed the whole `test` soak job. `Spec`'s
absolute ceiling is derived as twice the inactivity budget (D31), a
derivation that assumes total runtime is a small multiple of one phase.
These verticals break that premise: their success *is* one full sweep of
guest RAM, so their runtime scales with the work and with host
contention. Measured: 40 s for boot, sweep, and reset on an idle host;
~4 minutes for the same sweep under the nightly's ~95 concurrent jobs —
so the 120 s ceiling was load-dependent by construction, exactly the
§7 flaky timeout, and would have kept failing every night. A run may now
declare its own ceiling (`Spec::with_runtime_ceiling`, floored at the
silence budget so the two faults stay distinguishable) and the three
takeover verticals declare 15 minutes, over three times the loaded
measurement. The derived default is unchanged for every other vertical,
and the silence budget stays 60 s, so a genuinely hung guest is still
caught as fast as before.

**Not a fix for D14.** The open `sysmon-qemu-aarch64` load-dependent
timeout is the same *class* but a different bound: that vertical's own
120 s is its **inactivity** budget, so it is a work-heavy guest starved
into real silence, not a progressing guest cut off by a ceiling. It still
needs its own root-cause (bounded QEMU concurrency, or a budget sized to
its work); nothing here closes it.

**Regression cover.** `soak_deadline` outlasts every budget it is given
and saturates instead of overflowing; every `fssoak` mode's deadline
outlasts its budget, with the ordinary-step budget asserted too short to
stand in for it; the pass-boundary predicate refuses a pass that would
not finish, admits one that would, and runs exactly one pass without a
deadline; a declared ceiling replaces the derived one while leaving the
silence budget untouched, and is floored at that budget; every takeover
vertical's declared ceiling outlasts its derived one. End-to-end:
`cargo xtask fssoak --target fat32 --secs 25` now runs its child under a
2725 s deadline (was 2700 s) and stops at 20.11 s, inside its budget; the
three takeover verticals pass in 34.4 s (aarch64), 37.2 s (riscv64), and
19.0 s (x86_64) against their 15-minute ceiling.

## D39 — a riscv64 guest stalled dead moments after a `spawn` — DONE

**Root cause: `userentry::enter_user_mode` armed `sscratch` with S-mode
interrupts still enabled.** The riscv64 trap vector's *only* discriminator
between a trap from U-mode and one from S-mode is the entry swap
`csrrw sp, sscratch, sp`: `sscratch` holds the running task's trap anchor
while U-mode runs and **0** while S-mode runs, and a non-zero swap-in
result therefore *means* "from U-mode". The entry sequence armed `sscratch`
two instructions before its `sret` but only cleared `SPP`/`SPIE`, never
`sstatus.SIE` — and the dispatch loop runs in-kernel bodies with S-mode
interrupts enabled, so the window was open on **every** `enter_user`.

A supervisor timer or external interrupt landing in that window was
misclassified as a U-mode trap. It built its frame at the freshly armed
stack top (above the live `sp`, clobbering the caller's own frame), then
returned down the *S-mode* epilogue, which does not re-arm — leaving
`sscratch` **0**. The new process was then `sret`-ed into U-mode with no
kernel stack armed, so its very next trap took the `bnez` fall-through and
built the kernel's trap frame **on the task's own user stack**, corrupting
the running program until it jumped into its data. That final wild jump
raised a U-mode *instruction* page fault, which the trap path answered with
`halt_current_hart()` — total guest silence, on a single-hart guest, moments
after a `spawn`.

That explains every observed property: it only ever appeared just after a
`spawn` (the only caller of `enter_user`), it was timing-dependent on a
~2-instruction window (hence rare, and sensitive to host load skewing
interrupt arrival against the guest instruction stream), and it silenced the
*whole* guest rather than one task.

**Fix (`kernel/arch/riscv64/src/userentry.rs`).** `SIE` joins `SPP`/`SPIE` in
the mask the entry `csrc` clears, ahead of the `csrw sscratch`, so no S-mode
interrupt can be taken while `sscratch` is armed. The mask never reaches the
task: `sret` restores U-mode's interrupt state from `SPIE`, and U-mode stays
preemptible because the hart runs below S-mode. This is what the aarch64
sibling's opening `msr DAIFSet, #0xf` has always done, and what `trap.s`'s
own stated invariant already assumed. Interrupts remain deliberately enabled
inside a syscall body (so a long `ecall` cannot monopolise the hart), which
is safe precisely because `sscratch` is 0 there.

**Fix (`kernel/arch/riscv64/src/{trap,fault}.rs`, that port's
`dispatch.rs`/`boot.rs`).** The reason the corruption ended in *silence* was
a second defect: riscv64 offered only U-mode **load/store** page faults to
the user-fault resolver and sent everything else — an instruction page fault,
an illegal instruction, a misaligned access — straight to
`halt_current_hart()`. Any user program with a wild jump could therefore park
the machine, an unprivileged denial of service. The port now has the
`UserFaultTerminateFn` slot aarch64 already had, and `trap::fatal_exception`
charges an unresumable exception to whoever caused it: from U-mode it kills
that task through the shared arch-neutral
`dispatch_core::terminate_user_fault_via_slot` and the hart carries on; from
S-mode (or when no task can be attributed) it takes the fault handler and
otherwise parks. With no terminator installed the old fatal path is still
what happens, so the install can only fail closed.

**Regression cover.** Host tests in `userentry.rs` pin the entry masks —
`SIE` cleared before the arm, `SPP`/`SPIE` cleared, `SUM` set, and the two
masks disjoint — and fail on the pre-fix constants; two compile-time
assertions pin the same invariants in every configuration. `fault.rs` pins
the terminator slot's set-once round-trip. End to end: **822 consecutive
boots** of `autoload-input-qemu-riscv64` reached the login prompt with no
stall (342 at six-way host concurrency, then 480 at eight-way), where the
same loop on the pre-fix binary silenced a guest at boot **117**.

**Reproducer worth keeping.** Booting `autoload-input-qemu-riscv64` to its
login prompt takes ~2–6 s per guest and drives several `enter_user`
transitions, so six-to-eight concurrent guests in a loop hit the window
roughly once per ~120 boots — a far cheaper probe for this class than the
network verticals the defect was first seen on. Watch for a guest that stops
emitting rather than one that exits: the stall is total silence, and the
guest process stays alive.

**Landed with the original entry (observability only).** The harness reported
"TIMEOUT after 240s **with no serial output**" for a guest that had emitted
12.8 KB — the same misdiagnosis D22 corrected for the sibling outcome, and it
cost time before the transcript was read. Both emitters
(`tools/qemu/src/bin/run.rs`, `tools/xtask/src/commands/qemu_tests.rs`) now
say the guest fell silent for its whole *inactivity* budget and point at the
transcript's last line as the stall point.

## D42 — an x86_64 ring-3 wild jump halts the CPU instead of the task (OPEN)

**Symptom (found by inspection while fixing D39, not yet observed).** The
x86_64 port has no `user_fault_terminator` equivalent, and its `#PF`
dispatcher offers a ring-3 fault to the resolver only when
`fault::is_user_data_fault(error_code)` holds — which is
`is_user(error_code) && error_code & PF_ERR_INSTR == 0`, i.e. **data**
accesses only. A ring-3 *instruction-fetch* page fault (a wild jump, exactly
the shape that made D39 fatal on riscv64) is therefore never offered to any
resolver and takes the fatal path, parking the CPU for what is one task's
fault. Unprivileged, trivially reachable, and the same defect class D39's
second half closed for riscv64 and aarch64 already closes.

**Fix shape.** Mirror the sibling ports: add the `UserFaultTerminateFn` slot
to `kernel/arch/x86_64/src/fault.rs`, install
`production_user_fault_terminate` beside the resolver in that port's boot,
and route the fatal tail of the `#PF` dispatcher (and of any other ring-3
exception the port dispatches) through it — reusing the arch-neutral
`dispatch_core::terminate_user_fault_via_slot`, never a second copy.

**Also to establish.** Which architectural exception vectors that port
actually installs beyond `#PF`/`#NMI`/`#DF`: a ring-3 `#UD` or `#GP` with no
IDT entry would be worse than a halt, and this entry should not be closed
without reading `interrupts.rs`'s base IDT to settle it.

**Regression cover (lands with the fix, §7).** A host test for the slot's
set-once round-trip, and a QEMU vertical in which a ring-3 task jumps to a
non-executable page and only that task dies.

## D43 — a riscv64 U-mode task could steer the kernel onto another hart's per-CPU state — DONE

**Root cause: the trap vector never re-established the kernel's `tp`.** On
riscv64 `tp` (x4) is an ordinary *unprivileged* register — the RISC-V psABI
thread pointer, which U-mode code may write with a single `mv` — and it is
also this port's per-hart kernel anchor: `SchedulerArch::current_cpu`
resolves the running CPU through `smp::current_hartid`, which reads `tp`, and
the Arch-HAL per-CPU slice reads and writes the same register. The vector
saved and restored the caller-saved and callee-saved GPR sets and the
return-state CSRs, but not `tp`, so every `ecall` handed the kernel whatever
value the trapping task had left there.

`li tp, <another hart id>; ecall` therefore made the kernel believe it was
running on that hart: `cpu_for_hartid` maps a *valid* sibling id, so the
dispatcher read and wrote another core's `CpuState` — its resume handle, its
dispatch slot, its published live address space. Driving
`reschedule_current` against a foreign core's saved dispatcher context
context-switches through another task's state; that is kernel memory
corruption reachable from any unprivileged program, not merely a wrong
reading. It was latent only because nothing in the tree used `tp` yet.

**Fix.** `sscratch`'s U-mode meaning is now a per-task **trap anchor**
instead of a bare kernel-stack top: a `TRAP_ANCHOR_BYTES` (16-byte)
kernel-only region at the top of the task's kernel-stack window whose first
word carries the kernel `tp` of the hart the task is running on, with the
trap frame built immediately below it. The from-U prologue spills the user's
`tp` straight into the frame's new `user_tp` slot and reloads the kernel's
from the anchor *before any other register is touched*; the U-return path
publishes the **current** hart's `tp` into the anchor (so a task resumed on a
different hart re-enters U-mode under that hart's true identity) and then
restores the user's value. `enter_user` carves and publishes the anchor
before it arms `sscratch`, and hands a freshly entered task a **zeroed** `tp`
rather than leaking the kernel's hart id into U-mode.

Because the frame lives on the trapping task's own kernel stack, the same
change makes the thread pointer genuinely **per task**: U-mode keeps a value
of its own across every trap and every context switch, which is the platform
contract thread-local storage rests on. No ABI or C-header impact
(`TrapFrame` is internal to the arch crate); `PerCpu`/`current_hartid` keep
their existing "`tp` holds the hart id" semantics, so no other port or
arch-neutral crate changed.

**Regression cover.** `tests/integration/tp_isolation_qemu_riscv64` is the
adversarial witness: its U-mode fixture
(`tests/integration/tp_probe_program`) writes a hostile sentinel into `tp`
before every `ecall` on a **two-CPU** guest (so the sentinel's low-bit `1`
names a real sibling rather than an unmapped id that would fall back safely),
and the dispatch callback fails the run unless `current_hartid()` is still
the true boot hart; the fixture's own exit code fails the run unless its
value came back intact. Confirmed to fail without the fix. Host-side,
`trap_layout_tests.rs` parses every `.equ` out of `trap.s` and pins it
against the `TrapFrame` field or Rust constant it addresses — removing the
hand-copied offsets that used to sit in `syscall_entry_tests.rs` — and pins
the entry ordering (nothing between the swap and the reload may read `tp`)
and the U-return publish-before-restore ordering. `sret_tests.rs` gained the
matching `enter_user` anchor/`tp`-clearing ordering test.

## D44 — a console reader's re-park used a remembered CPU id, so it suspended another core's task — DONE

**State:** fixed. Presented as `tairix-test-stress-qemu-aarch64` failing under
the concurrent `cargo xtask ci` matrix while passing in isolation: `elsh` was
killed moments after it reaped `sysmon` and reclaimed the console foreground,
with the kernel naming a wild fault the shell had not taken —

```
[  9.488] DEBUG id=5000 syscall dispatched task=9 comm=elsh sc=wait
[  9.488] DEBUG id=5000 syscall dispatched task=9 comm=elsh sc=console_foreground
root@tairix ~%
[  9.552] WARN  id=4034 task killed by unresolvable user fault task=9 name=elsh
                write=false fault_class=wild fault_offset=null_page region_offset=0
[  9.555] INFO  id=10004 session ended task=8 user=root exit_code=139
```

**Root cause.** Resume handles are per-CPU: `reschedule_current` suspended the
task published for the CPU id its *caller* passed. `BlockingConsoleRead::
read_until` read that id **once, before** its poll-and-park loop. A console
reader parks, is woken by the next keystroke, and re-parks — and between two
parks the scheduler may dispatch it on a different core. Every re-park after a
migration therefore named the core the reader had *left*, and suspended
whichever task the dispatcher had since published there: the caller's
continuation was written into that victim's `ThreadControl::task_ctx` and the
caller switched to the victim's dispatcher.

The victim was `elsh`, parked in its own console read. When the scheduler next
dispatched it, `dispatch_step` switched into a save area holding a **foreign**
kernel stack pointer, so the CPU unwound another task's syscall handler and
`eret`ed that task's user registers — under `elsh`'s page-table root. Every TAIRiX
program is a PIE at the same load bias, so the foreign registers addressed real
pages of `elsh`'s space: the wild stack pointer sat ~180 KiB below `elsh`'s own,
the growth resolver dutifully backed 17 pristine pages of `elsh`'s stack for a
fault that was not its, and the next epilogue read zeros and `ret`ed to address
`0`. The load dependence is a migration-frequency effect, nothing more.

**Evidence that pinned it.** The faulting EL0 frame was built on a kernel stack
(`0x44cffa00`) that was not `elsh`'s (`0x44c77a00`), on a CPU whose current-task
slot named `elsh` while `elsh`'s own syscalls had just run on another core; the
wild user stack pointer matched the `stress` workers' stack top (`0x1000872000`,
their `enter_user` value) rather than `elsh`'s (`0x10008a2000`); and a
dispatcher-side assertion caught `elsh`'s saved kernel stack pointer sitting
above its own stack top.

**Fix.** `BlockingConsoleRead::read_until` now reads the live CPU **at each
park**, inside its loop, exactly as every other wait loop in the kernel already
did (`procwait`, `sleeplock`, `blockwait`, `blkclient`, the pipe and stream
waits, `park_current_task` — whose own rustdoc had already named this hazard).
The caller's task id is still read once, because a task's id does not change
across a migration; only the CPU does.

Backstop (§2.17): `dispatch_step` now proves a suspension point belongs to the
task before switching into it — the saved kernel stack pointer must lie on that
task's own stack (`KernelStack::carries`, over the new
`KernelStack::usable_bytes`) — and fails the task closed exactly as a
stack-guard violation does. A future mispairing anywhere on the park path is
then a deterministic refusal, never silent cross-task corruption.

**Regression cover.** `kernel/core::console::tests::
a_parking_read_resolves_the_live_cpu_at_every_park` counts the reader's
`current_cpu` reads (`TestArch` now records them) and requires the park to have
resolved one for itself; it fails on the pre-fix source with `saw 1`.
`kernel/core::kthread::tests::
dispatch_step_refuses_a_suspension_point_on_a_foreign_stack` pokes a foreign
stack pointer into a save area and requires the step to refuse — verified to
fail on the pre-fix source, which switched into it — and
`a_kernel_stack_carries_only_its_own_usable_region` pins the predicate,
including that the guard region is not a legitimate frame. End to end, the
`stress-qemu-aarch64` vertical is the acceptance witness: the reproduction ran
in 1–6 rounds of five concurrent guests before the fix and stayed clean over
155 runs after it.

## D45 — the per-CPU live-space publication accepted a non-`Arc` pointer, so its refcount write landed out of bounds — DONE

**State:** fixed. Presented as `cargo test --workspace` intermittently dying
with `SIGABRT` inside the `tairix-kernel-core` lib-test binary — glibc's
`corrupted size vs. prev_size` from `free`, i.e. genuine heap corruption, not a
failed assertion — which failed the whole-project gate's test phase. Minimised
to two `syscalls::tests` run concurrently (either alone was clean at any thread
count).

**Root cause — a real unsound `unsafe` write, not test isolation.** The
per-CPU live-space slot holds a bare `*const ProcessSpace` so the
context-switch path pays no refcount traffic, and `kthread::
current_process_space` (what `thread_create` reaches) reconstructed an owning
`Arc` from it via `Arc::increment_strong_count` + `Arc::from_raw`. That is only
sound because the production publisher forms the pointer with `Arc::as_ptr`.
The invariant lived in a comment, not the type — so the *second* publisher,
`publish_live_space_for_test`, took a `&'static ProcessSpace` from a leaked
`Box` and published it. The increment then wrote eight bytes **16 bytes before**
the value (the neighbouring glibc chunk's `prev_size`), and the matching
decrement on drop could take a fabricated count to zero and free a pointer
before a live allocation. ASAN named it exactly: `WRITE of size 8 ... 16 bytes
before 56-byte region`, from `current_process_space` → `threads::exit`.

**Fix.** `LiveSpacePtr`'s field is private and its only constructor,
`LiveSpacePtr::borrowed(&Arc<ProcessSpace>)`, borrows from a live `Arc`, so a
publication that is not `Arc`-derived is now unrepresentable; the two `unsafe`
reads are its `reborrow` / `clone_owner` methods, whose remaining obligation is
liveness alone (the publication protocol's own property). The test publisher
takes an `Arc` **by value** and holds it in the publish guard, standing in for
the running thread's control-block clone, so the test path has production's
shape rather than a second one.

**A second defect found on the way (§2.18):** `dispatch_step`'s D44
foreign-stack refusal ran *after* `publish_resume` + `publish_live_space` and
returned `Exit` without clearing either. The scheduler then reaps the task and
drops its `ThreadControl` — and the `Arc<ProcessSpace>` clone with it — leaving
the CPU naming a freed control block: the next `reschedule_current` there would
switch into a reaped context, and a kernel kthread's dispatch never clears the
live-space slot, so a stale publication survives its whole run. The refusal now
runs before the switch-in hook and both publications, so a refused task leaves
the CPU naming nothing and never activates its user root either.

**A third, in the futex (§2.18):** `bucket_of` resolved a key by index into
whichever bucket table was live, and `init_buckets` could install the sized
table *after* keys had resolved against the single-bucket fallback — stranding
a registered waiter in a bucket no waker or deadline sweep looks in, which is a
lost wakeup that never resolves. The table is now published exactly once (an
empty `Vec` is the "never sized" answer, so latching it costs no allocation and
cannot fail) and a later sizing is refused; `init_buckets` also uses
`try_reserve_exact`, so a boot-time OOM degrades to the single bucket instead of
aborting. The `bucket_index` hash is factored out as a pure function, so the
spread properties are host-tested without any test installing a table.

**Regression cover.** `procspace::tests::
the_published_handle_is_what_a_reconstruction_shares` pins the strong count
moving on the *published* allocation across a reconstruction and back;
`kthread::tests::a_refused_dispatch_step_publishes_nothing_for_the_cpu` requires
a refused step to run no switch-in hook and leave neither publication (all three
of its assertions fail on the pre-fix source);
`futex::tests::a_resolved_table_is_never_swapped_out_from_under_a_live_key` plus
the two spread tests and `a_bucket_index_is_always_in_range` pin the table. End
to end the minimised pair is the witness: ~1/20 aborts before the fix and 3/3
ASAN heap-buffer-overflow reports, then 60/60 clean, the whole binary 100/100 at
the harness's default thread count, and 5/5 clean under ASAN.

## D46 — no discard reaches the hardware through a layer (partition half DONE, RAID and transport halves OPEN)

**State:** D21's defect class — a defaulted `Block` method answered by a
layer that was never told — for `discard`/`discard_capability` rather than
`device_class`. The partition half is closed; two halves remain, and neither
is a forwarding omission, which is why they are staged rather than swept in.

**The partition half — closed.** `PartitionBlock` translated and forwarded
every other operation but left both discard methods defaulted, so
`discard_capability()` answered "unsupported" for every filesystem mounted
on a partition — which is every real installation (MBR/GPT →
`PartitionBlock` → ARXFS). ARXFS's whole discard engine
(`drivers/filesystem/arxfs/src/discard.rs`) was therefore unreachable on
real hardware while testing clean against a raw device. It now forwards
through the same containment check a write gets (`inner_span`, one
definition, so a window can never name a neighbour's blocks), and reports
the device's granularity **only** when the window's start block is aligned
to it — a misaligned window withdraws support rather than promising an
alignment it cannot honour.

**The RAID half — open, and a semantics question.** `RaidArray` dispatches
both methods to the six level kinds, and *none* of them implements either,
so the dispatch can only ever reach the trait default: every array reports
no discard support. This is not a forward to add. Discard on a redundant
array invalidates the redundancy that covers the discarded range — a parity
strip computed over blocks the device may now return as anything — so each
level owes a decision: recompute parity over the discarded range, refuse the
range, or narrow it to whole stripes. Mirror and stripe are near-forwards;
parity, dual-parity, triple-parity, and RAID10 are not.

**The transport half — open, and an ABI change.** `BlkOp` has only
`Geometry`/`Read`/`Write`/`Flush`, so `RemoteBlock` and `BlkClient` cannot
express discard at all and their `Unsupported` is honest. Reaching a
user-space block driver's device needs a new opcode plus its server half in
`blkio::serve` and every block driver. The same wire gap drops
[`BufferClass`] on the `*_with_class` pair, so a sensitive buffer's
scrub-the-staging-copy request does not cross the seam — worth settling in
the same change as the opcode, since both are the same missing field.

**Done when:** each RAID level states and implements its discard posture
with a test per level over a member that records the ranges it is asked to
discard; the wire carries a discard opcode and the buffer class, with
`RemoteBlock`/`BlkClient` forwarding both; and no layer answers either
discard method from a trait default.

## D47 — every desktop launch lost its first argument, so the autostarted file manager ran as an ordinary window (DONE)

`tairix-test-appbar-qemu-aarch64` ran to its 600s ceiling. One defect in
the desktop and one in the harness, both closed.

**The desktop — the argument vector had no argv[0].** `spawn_app` passed
the caller's arguments as the *whole* argv, but a program's own arguments
begin after index 0 (the program name its spawner chose), so every desktop
launch silently lost its leading argument. The file manager's autostart
therefore never saw `--desktop` and took `Role::Window` instead of
`Role::Desktop`: a home-folder window nobody asked for at every login, the
ordinary Info/*Quit* slot convention on a core component the user must not
be able to quit, and a process that ends when that window closes. The same
loss dropped the folder a desktop icon opens and the document an icon
launch names.

The rule is now one host-testable function, `launch_argv` in
`userland/gui/session/src/launch.rs` — the program first, then the
caller's arguments — and `spawn_app` is the only launch path, so every
launch site is correct by construction. The freestanding `Run` loop cannot
be host-tested, which is why the rule lives beside the launch table rather
than in it. Every other spawner in the tree (`files`, `terminal`,
`stress`, `elsh`, `lib/sandbox`) already named its program.

**The harness — it measured the wrong slot.** The click script and both
pixel assertions were written when the strip's leading slot belonged to
the launched application. The autostarted file manager holds slot 0 for
the life of the session, so reading slot 0 for "the launched application"
compared the file manager against itself in the bare frame and could never
witness anything. The script now seats it ahead of the launched
application and drives `APPBAR_LAUNCHED_SLOT`; `assert_app_slot_drawn`
reads that slot and `assert_no_slot_beyond_the_launched_app` reads
`APPBAR_EMPTY_SLOT` beyond it.

The terminal's ~0.5s exit without a window was downstream of the
mis-launch and does not recur: with the component autostart correct the
launched terminal serves its launch window, its *New window* row, and its
slot's default action — the three creates the PASS needs — in 5/5
consecutive runs at ~20s each.

**Regression cover.** `tairix_desktop_session::launch::tests::
a_launch_names_the_program_before_its_arguments` fails on the pre-fix
shape; the vertical itself covers the wiring end to end.

## D48 — a window `Create` an app could build but the session had to refuse (DONE)

The symptom: choosing *Set Date & Time…* from the desktop clock's menu showed
the credential prompt, accepted the credentials, and then no window appeared.

**Cause.** `WindowRequest::Create` carried a `resizable` flag *beside* a
minimum client size, and its decoder refused `resizable = false` with a
non-zero minimum — a window that is never resized has nothing to measure a
floor against. `datetime.app` asked for exactly that: fixed size, minimum set
to its own extent. Every launch of it therefore ended at the window create,
having drawn nothing. A caller could build a request the server was obliged to
reject, which is a defect in the type, not in either party.

**Fix.** `WindowSizing` is a sum type — `Fixed`, or `Resizable { min_width_px,
min_height_px }` — carried whole in `Create`. The contradictory pair has no
spelling, so no app can construct it: `lib/abi`'s
`every_sizing_an_app_can_ask_for_survives_the_round_trip` enumerates every
sizing that exists and asserts each decodes back to itself. The wire decoder
still refuses the pair, because a foreign encoder can still put those bytes on
the wire. Each app now states one sizing: `datetime.app` `Fixed`; `lib/browse`
and Switchboard publish a `WIN_SIZING` their harness reads back; the terminal,
whose floor is a runtime font measurement, declares `win_sizing` and derives
`WIN_RESIZABLE` from it.

**Second half — the silence.** The app *did* state its reason on `stderr`, but
an elevated child inherits login's console, which under a graphical session is
the framebuffer text console behind the desktop: no consumer, no serial, no
user. Login, the only observer of that exit, now audits an abnormal one
(`LAUNCH_ENDED_ABNORMALLY`, `EventId(10_026)`) with the pid, the status, and
the reason `tairix_abi::load_failure_reason` gives a reserved load status. A
clean exit records nothing.

**A misreading to not repeat.** The first diagnosis called this an elevation
running under the wrong account, from `task capabilities derived … uid=1000
caps=3`. Both fields were misread: `caps` is the *count* of derived
capabilities (`kernel/sec/src/captable.rs`), so `caps=3` is all three the
manifest requested, `CAP_TIME_SET` included; and uid 1000 *is* the seeded debug
`root` account (`tools/mkimage/src/rootfs.rs`), i.e. the account the prompt
authenticated. There was never a capability or account defect here.

**Regression cover.** `tairix-test-datetime-elevate-qemu-aarch64` drives clock
→ *Set Date & Time…* → prompt → typed credentials and passes only on two
latches: an `APP_LOADED` naming the bundle, and a window create served on the
reserved endpoint *after* it. It ran to the 600 s ceiling before the fix and
now passes in ~20 s, five consecutive runs.

## D49 — on aarch64 and riscv64 a QEMU vertical's success status is also what a reset produces (OPEN)

Found while closing `plans/NETWORK.md` N16b, whose predecessor suspected the
runner was scoring short runs as passes. It was not — that vertical's runs are
genuine (see N16b) — but the audit it prompted found a real fail-open one level
down, in the harness's verdict itself.

**The defect.** `tairix_qemu::Arch::outcome_from_status` scores a run `Pass` on
the guest's success exit status alone. On x86_64 that status is
`(SUCCESS_EXIT_CODE << 1) | 1` = 33, a value only an `isa-debug-exit` write can
produce, so the status *is* evidence. On aarch64 (semihosting `SYS_EXIT`) and
riscv64 (`SiFive` Test `FINISHER_PASS`) the success status is plain `0` — and
so is the status QEMU exits with when the *machine* goes away under
`-no-reboot`: a guest reset, a PSCI `SYSTEM_OFF`, an SBI SRST shutdown, or a
monitor `quit`. A guest that never reached its assertions but took the machine
down scores exactly like one that passed, on two of the three Tier-1 arches.

Measured on QEMU 11.0.2, not inferred:

```text
$ printf 'system_reset\n' | qemu-system-aarch64 -M virt -display none \
      -no-reboot -monitor stdio -serial null; echo $?
0
```

The tree already knows this hazard: `outcome_from_done`'s `reset_success_marker`
arm exists precisely because "a crash that merely triple-faults into a reset
(also status `0`) still fails loud (it never reached the marker)". That defence
is opt-in, and only the three `memtest`-takeover verticals opt in. Every other
aarch64/riscv64 enrolment falls through to the bare status decode.

A second, narrower instance of the same shape: the host process status is 8
bits, so `qemu_exit::exit_failure(code)` with `code % 256 == 0` also lands on
`0` and scores `Pass`. No enrolment uses such a code today (the failure
constants run 1–19, plus `FAIL_EXIT_BASE = 100` and a small guest code), so
this half is latent, not live.

**Why it is latent rather than live today.** No enrolled vertical resets or
powers off its guest: the panic bridges park the CPU, an unhandled exception
reaches a panic, and no serial script types `reboot`/`shutdown`.
`KernelArch::reboot`/`poweroff` are reachable from userland, so the reachability
is one enrolment away; and the riscv64 path is the closest to live, because an
unhandled M-mode trap can end in a firmware reset rather than a park.

**Why the obvious host-side fixes do not work.** `-no-shutdown` keeps QEMU
alive across a guest reset, which would make the reset fail loud — but on
riscv64 it also blocks the success path, because QEMU 11 routes
`FINISHER_PASS` through the same shutdown machinery a reset uses (measured:
`tairix-test-kernel-arch-boot-riscv64` exits 0 in 0.11 s without it and has to
be killed with it). `-action shutdown=` accepts only `poweroff`/`pause`, so
there is no "exit with a distinguishable status" action either. The
distinguishing evidence has to come from the guest.

**The two candidate designs, and their costs.** Both make success provable
rather than inferred; neither is a per-vertical opt-in.

1. *A reserved success status.* `exit_success` reports a magic non-zero status
   on all three arches (aarch64: the `SYS_EXIT` subcode; riscv64: the
   `SiFive` coded-status word), so one shared decoder replaces the three
   per-arch ones and a reset can never produce it. Atomic by construction —
   one semihosting call or one MMIO store, nothing to interleave. The cost is
   that the reserved value then may never be a failure code, which the
   open-ended `FAIL_EXIT_BASE + code` space does not enforce; and it reads
   oddly on riscv64, where the device names that word "fail".
2. *A finisher witness on the console.* `exit_success` prints a fixed marker
   before terminating and the host requires it for a success status — the
   `reset_success_marker` mechanism, made universal instead of opt-in. No
   reserved value, one rule for every arch. The cost is that the marker must
   reach the transcript un-interleaved on an SMP guest, so it needs the
   console gate's whole-line framing plus a flush rather than the direct
   `beacon` path.

**Done when:** a success verdict on every Tier-1 arch rests on evidence a
reset cannot forge; the three `reset_success_marker` verticals keep working
(their success *is* a reset); a host unit test pins that a status-`0` exit
without the guest's evidence is `Fail` on aarch64 and riscv64; and the whole
`test --qemu` matrix is green on all three arches afterwards, since the change
alters the pass criterion for every enrolment and a vertical that was passing
on a machine-death status would surface here.

## D50 — the flake hunt's concurrent replicas re-planted one guest's disk underneath itself (DONE)

Found while making `test --qemu` persist a transcript for every run
(`plans/NETWORK.md` N16b), which added a second per-run output to a path set
that was already colliding.

**The defect.** `ci_long::flake_hunt` runs each unit `REPS` times
*concurrently*, so up to `budget / weight` runs of one enrolment are in flight
at once (four on a 24-CPU host; one on a small runner, where the weight
saturates the budget and the batch serialises — which is why this never
surfaced in CI). Each run re-plants its guest's backing image inside `run_one`,
and the path was a pure function of the *binary*, so a replica's
`plant_raw_disk` truncated and rewrote a 200 MiB image that a sibling replica's
QEMU had open — corrupting a live guest's disk mid-run. The failure it produces
is an arbitrary guest misbehaviour with no local cause, i.e. exactly what a
flake hunt is supposed to distinguish *from* a real flake.

The flake hunt already threaded a repetition index into its job factory; the
QEMU unit was the one factory that discarded it (`move |_|`).

**Fix.** The sidecar path is a function of the run, not the binary:
`sidecar_path(kernel, t, replica, ext)` separates both colliding axes — the
`TESTS` index for enrolments that share one built binary, and the replica index
for concurrent runs of one enrolment. Replica zero of a singly-enrolled binary
keeps the plain `<binary>.<ext>` name, so the pull-request matrix's paths are
unchanged. `Enrolment::run` takes the replica; `ci_long`'s QEMU unit passes the
hunt's own index.

**Regression cover.** `sidecar_paths_never_collide_across_enrolments_or_replicas`
enumerates every enrolment × 32 replicas × both sidecar kinds and asserts the
paths are distinct — it fails on the first shared-binary enrolment when the
replica index is dropped. The transcript is covered by the same test because it
is now a per-run output too.

## Non-goals / do not do

- Do NOT re-open the settled FIX-SYSCALL design decisions (no per-syscall
  interruptibility flag §2.3/§2.4; kernel stays non-preemptible §4;
  reuse P-5's single wake/drain discipline §2.2).
- Do NOT collapse D1–D4 into one mega-change — each is a distinct defect
  class with its own gate.
- Do NOT grow the compiled-in surface or ABI beyond what each seam
  already exposes (§2.3/§2.4).
- Do NOT mark any item done on a green compile alone — the tests and the
  §23 gate are the bar.

## D52 — an x86_64 shootdown initiator that cannot take the IPI can deadlock (OPEN)

**Mechanism.** `tairix_arch_x86_64::tlb_shootdown` reaches the other CPUs by
raising an IPI at each and spinning until every one has acknowledged from its
ISR. That protocol assumes each target *can* take the interrupt. A CPU that
initiates a shootdown while its own interrupts are masked cannot acknowledge
one, so two concurrent initiators can cycle: B holds the global mailbox and
waits for A's acknowledge, while A — masked — waits to acquire the mailbox.
Neither is a bug in isolation; the cycle needs both.

**Why it surfaced now.** Until the fragmentation-immune kernel-heap growth
landed (`plans/FIX-KHEAP.md`), nothing in production drove
`CrossCpuTlbShootdown` at all — only the `cross_cpu_tlb_shootdown_qemu_*`
verticals did. Kernel-heap teardown is the first production initiator, and it
runs under the global heap lock, which masks interrupts for the whole hold.

**Why the tree is safe today, and why that is not good enough.** That same
heap lock serialises every instance of the only production initiator, so two
initiators cannot coexist and the cycle cannot form. The safety therefore
rests on the current *caller set*, not on the protocol — the moment a second
production initiator lands (process address-space teardown is the obvious
one, and the HAL exists for it) the cycle becomes reachable. The precondition
is stated in `tairix_arch_api::CrossCpuTlbShootdown`'s contract and in the
x86_64 module docs so a second caller reads it before adding one, but a
stated precondition is a weaker guarantee than a protocol that cannot
deadlock.

**The fix.** Let a target acknowledge from a spin as well as from its ISR,
exactly once: publish a generation counter last (after the range and the
outstanding count), record per CPU the last generation it served, and have a
CPU that is itself spinning to acquire the mailbox serve the in-flight
request when its generation is new. The ISR does the same check, so a request
served from the spin is not double-acknowledged when the pending IPI is later
taken. x86_64 already carries `PerCpuStorage`, so the marker needs no new
ceiling. aarch64 needs nothing (`tlbi vaae1is` is a hardware broadcast with
no software acknowledge) and riscv64 needs nothing (the SBI RFENCE is served
by the firmware in M-mode, which S-mode masking does not gate) — this is an
x86_64-only protocol defect.

**Definition of done.** The generation-marker protocol lands, the
`cross_cpu_tlb_shootdown_qemu_x86_64` vertical is extended to drive a
shootdown from a CPU with interrupts masked while a second CPU initiates
concurrently, and the precondition wording added to the HAL contract for this
entry is deleted (§2.14) because the protocol no longer needs it.

## D53 — kernel-heap grow/shrink thrash now costs per-page work (OPEN, reachability unconfirmed)

**Mechanism.** `kalloc` hands a grown region back to its source the instant
the region drains, with no hysteresis. That was free when the source was one
`alloc_order` / `free_order` pair. Since the fragmentation-immune growth
landed (`plans/FIX-KHEAP.md`) a region costs work proportional to its page
count: at the 16-page growth granule an alloc/free cycle that spills out of
the bootstrap region pays 16 page-table installs, 16 teardowns, 16
system-wide invalidations and 16 frame frees — where it previously paid two
frame operations. `lib/kalloc`'s own
`grow_shrink_cycles_are_stable_and_reuse_space` test exercises exactly that
pattern and shows one grow *and* one shrink per allocation over 1000 rounds.

**Why it is recorded rather than fixed.** The regression bites only while the
heap is serving allocations out of *grown* regions — i.e. once the 64 MiB
bootstrap region is exhausted. Nothing observed so far establishes that the
system reaches that state: if it did, every small kernel allocation would
thrash and the whole machine would crawl, not one application. Building a
retention cache for a path that is not known to be hot is the speculative
optimisation the charter forbids, so the cost is stated here with its
arithmetic instead of guessed at.

**The fix, when it is confirmed.** Hysteresis where the cost lives: the
growth source keeps one granule-sized chunk mapped instead of tearing it
down, and hands it back on the next grow that fits. Retention is then bounded
by one growth granule rather than by the largest region ever grown, which is
what makes it acceptable against "an idle system does not hold memory it has
freed"; a larger region amortises its own mapping against the allocation that
needed it and is still returned promptly. Validating a retained run without
releasing its address space needs a non-mutating `SlotWindow` query
(`AnonWindowMap::validate`'s counterpart).

**Confirming it.** Instrument the source's grow/shrink counts over a desktop
session and check whether the heap is serving small allocations from grown
regions at all. Fix only if it is.

## D54 — a desktop worker thread issues ~2500 file opens at session start, starving every concurrent reader (OPEN)

**Mechanism.** Between 7.47 s and 12.65 s of the `autoload-input-qemu-aarch64`
desktop boot, one thread of the `desktop` process (task `0x10`, not its main
task) issues some 2500 audited `fs_open` + `fs_write` pairs — a rate near
1500 pairs per second sustained for five seconds. Every `fs_open` is a full
VFS path resolution against the writable root, so the burst monopolises the
one boot disk's serialised device windows for its whole duration.

**Impact, measured.** It is the whole of the "delivered throughput is
~0.9 MB/s where the driver measures 370 MB/s" gap `plans/FIX-KHEAP.md`
reported. Bundle load throughput (now self-describing: the `APP_LOADED`
record carries `read_bytes` beside `load`) is not a function of bundle size —
the largest bundle is the fastest (`desktop.app`, 2.15 MB at 6.2 MB/s) and
the smallest is among the slowest (`seatmgr.app`, 35 KB at 0.15 MB/s). It is
a function of *overlap with this burst*: the only two loads inside the window
are the only slow ones (`switchboard.app` 1.36 s and `files.app` 2.54 s, both
about 0.5 MB/s), while every load outside it takes 0.08–0.60 s. The burst
also emits ~5000 audit records on the serial console, which is itself a
per-syscall cost on the same path.

**What is not yet known.** Which loop it is. The burst begins ~150 ms after
the `desktop` process spawns and *before* its wallpaper sandbox worker is
spawned (7.861 s), so it is not the wallpaper transfer. `fs_read` is not
audited, so the reads between each open/write pair are invisible in the
serial transcript and the pattern "open a file, write a byte" fits both an
asset-per-item worker nudging the serve loop through `WorkerWake` and a
catalog walk. Its iteration count is also not deterministic — two runs of the
same vertical produced 4713 and 2484 pairs — so it is timing-dependent, which
is itself a signal about what drives it.

**Confirming it.** Add the opened path to the `fs_open` audit record, or run
the vertical with the session's own tracing, and identify the loop. Then fix
the loop — this is a desktop-side defect, not a block-layer one: no
per-operation saving in the block stack can compensate for 2500 path
resolutions that should not be issued.

---

## D55 — the x86_64 direct physical map covered only the first gigabyte (DONE)

**The defect.** The x86_64 port had two physical maps and neither was sized
from the machine. `direct_phys_map()` handed out a fixed `[0, 1 GiB)` window
at `KERNEL_VMA_BASE`, and the page-table/frame view was a fixed `[0, 4 GiB)`
identity window; both were build-time constants, where the aarch64 and
riscv64 ports size theirs from discovery. Every kernel path that reaches a
frame by pointer — the spawn image write, the shared-memory zero-on-free
scrub, the remap window's `kvslots` store, the kernel heap's slab page supply
(`plans/FIX-KHEAP.md`) — therefore failed closed for a frame above the
window. It was not latent: the buddy allocator hands out its highest frames
first, so on any machine with RAM above the window the *first* frame drawn was
already unreachable.

**Fix.** One map, sized from the boot memory map. The boot trampoline's
`[0, BOOT_IDENTITY_GIB GiB)` is now a floor rather than the window: once
`build_memory_map` has run, `mem_map::identity_window_gib` takes the top of
usable RAM (floored at the trampoline's window, capped at the user virtual
base, since the identity map shares each process root's low half with the
child image) and `paging::widen_boot_identity` installs it — as 1 GiB PDPT
leaves where the part has them, else one page directory per gigabyte carved
out of the map (`mem_map::carve_frames_from_map`, from the top of low usable
RAM so it cannot land on the legacy structures or the AP trampoline at
`0x8000`). Every root constructor and the direct map re-read the published
window, so none can carry a different one. It fails closed
(`BootError::IdentityWindowWiden`) rather than booting on RAM it cannot
address.

The two maps collapsed into `ConfiguredIdentityPhysMap`, which
`direct_phys_map()`, the page-table frame source, both spawn seams, and the
root-unlock DMA/MMIO bring-up all share, so the `PHYSMAP_SPAN` / `IDENTITY_GIB`
constants each of them carried are gone. PID 1's page tables moved to the
allocator-backed source the runtime spawn already used: on a part without
1 GiB pages the window costs a directory per gigabyte, which a fixed `.bss`
reserve would have capped.

**Two defects fell out of it.** `ensure_child` dereferenced a present *huge*
leaf as a page table, which 1 GiB identity leaves made reachable; it now
refuses. And the RAM self-test silently skipped what the direct map did not
cover while the console settled on the installed total: it now reports
`verified` and `unreachable` separately (`AuditEvent::RamSelfTest`, `Warn`
when non-zero) and starts each region past frame zero, whose identity
translation is the null pointer and so used to take the whole first chunk of
low RAM untested with it.

**Regression cover.** `tests/integration/physmap_qemu_x86_64` boots the
production pipeline on a 3584 MiB guest — the smallest `-m` for which QEMU's
`pc` machine places any RAM above 4 GiB — and requires both that the window
widened past the trampoline's own and that the self-test left no usable byte
unreachable, i.e. every byte above 4 GiB was written and read back through
`direct_phys_map()`. Host tests cover the window sizing, the top-down carve
and its reservation, and the engine's zero-page skip and unreachable
accounting.

**What is still bounded.** RAM above the user virtual base (64 GiB) stays
unreachable and fails closed, because the page-table walk still recovers
tables by raw physical address and so needs `virtual == physical`. Lifting
that means walking through a higher-half direct map instead — see D56.

## D56 — the x86_64 page-table walk recovers a table by its raw physical address (OPEN)

**Mechanism.** `paging::ensure_child` — and the read-only walks beside it —
dereference a page-table entry's physical address directly (`phys as *mut`),
so the port's direct map has to satisfy `virtual == physical`. An identity map
lives in the low half of every process root, which it shares with the child
image at `spawn_layout::CHILD_USER_BIAS` (64 GiB), so the window stops there.
D55 sized that window from the discovered RAM; this is the bound left under
it. The aarch64 and riscv64 ports have the same shape and the same bound
(riscv64's Sv39 root is smaller still), so the user bias cannot simply move —
it is one workspace-wide relocation bias every `rxe` is baked for.

**Consequence.** No corruption — a frame above the window fails its translate
and its consumer fails closed — but a machine with more than 64 GiB of RAM
degrades exactly as it did below 1 GiB before D55: the allocator hands out its
highest frames first, so the first frame drawn is unreachable and the kernel
runs on what is left. Not reachable on any target the matrix runs today.

**The fix.** Walk through a higher-half direct map instead of an identity one,
as Linux does: give the port a dedicated PML4 region, dereference a table at
`PHYSMAP_BASE + phys`, and drop the per-root low identity map to the MMIO and
firmware window it is genuinely needed for. That also removes the per-root
page-directory cost of identity-mapping RAM. The blast radius is the reason it
is staged separately: ~15 sites in `kernel/arch/x86_64/src/paging.rs` plus
every QEMU vertical that builds an `AddressSpace` from a static pool would
have to bring the map up first.

**Done when:** an x86_64 guest with RAM above the user virtual base reaches a
frame at the top of its pool through `direct_phys_map()`, and a vertical pins
it.

---

## D63 — an ARXFS commit published its superblock slot with no barrier (FIXED)

**Where.** `drivers/filesystem/arxfs/src/lib.rs` `ARXFS::commit`.

**Mechanism.** Commit wrote the transaction's copy-on-write blocks, then the
transaction root, then the superblock slot naming that root — and issued no
`Block::flush()` at any point. Only `map_persist`, reached from an explicit
`fs_sync`, ever forced the device cache.

Every device with a volatile write cache — every SD card, every consumer SSD,
every HDD — was therefore free to commit those writes to media in any order. The
damaging order is: the superblock slot and the transaction root reach media while
an interior B-tree node beneath that root does not. `open` re-validates the root
before accepting a slot, so a lost *root* falls back to the previous slot and is
survivable; a lost interior node beneath a **durable** root is not. Both mirror
copies of that node are absent, so the read fails closed and the volume does not
mount — a whole-volume loss recoverable only by `check` or `rescue`, from a
single power cut at the wrong microsecond.

**Severity.** Data loss on ordinary power failure, on the class of device the
Pi 4 boots from. It survived because every emulated device in the suite was
strictly ordered, so no existing test could observe it.

**Fix (item WB1 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`).** One barrier per
commit: the transaction's blocks are staged in the dirty set
(`src/wcache.rs`), drained to the device at the commit point, `flush()`ed, and
only then is the slot written. One is sufficient — the root is just another
block that must be durable before the slot naming it — and a second is issued
only for an explicit `fs_sync`. The batching the dirty set brings is what makes
the barrier affordable: a 64 KiB write on a 512-byte volume costs 158 device
writes against 746.

Three further ordering defects the work exposed were fixed with it, each with
its own regression test: a commit that failed after its first slot copy
published the transaction while the caller rolled it back and freed the
published root's blocks; `scrub`/`check`/`health` propagated a failed `commit()`
without rolling back, so a later commit published the failed transaction's
trees; and the allocation map's clean→dirty stamp was not barriered before the
first page write, so a reordering device could leave a mount adopting a map
stamped clean at a generation it no longer described. All three are recorded in
`plans/ARXFS-WRITEBACK.md` §8 WB1.

**Proved by.** A volatile-write-cache device model
(`MemBlock::with_volatile_cache`): after a commit the only blocks it still holds
are the slot's two copies, and a power loss committing any subset of them leaves
the prior committed state or the new one, both whole. The WB0 command ledger
asserts the shape — exactly one barrier per commit, with nothing but that slot
pair after it — and the crash-replay sweeps still leave prior-or-new at every
write budget.

## D64 — ARXFS scrub's copy-repair write bypassed the read-only guard (FIXED)

**Where.** `drivers/filesystem/arxfs/src/scrub.rs` `scrub_meta_into`, and the
missing gate on `ARXFS::scrub` / `ARXFS::health`.

**Mechanism.** One metadata read path serves every metadata class: read the
primary, fall back to the companion mirror, and repair the bad copy from the
good one. `read_meta` guarded that repair with `if !self.read_only`, and said
why — a read-only handle must never mutate the device. `scrub_meta_into`
performed the *same* repair with a bare `self.write_block(comp, …)` and no
guard, and neither `scrub` nor `health` called `deny_if_read_only` (only `trim`
did). The repair is a direct block write, not a transaction, so `commit`'s
read-only refusal did not catch it.

A read-only ARXFS handle therefore wrote to its device whenever a scrub — or any
scrub-path verification reached from `health` — found a repairable mirror. That
contradicted the guarantee the driver states for `/System`, and it was actively
dangerous in the state the flag exists for: a re-inserted volume whose
non-mutation could not be proven is mounted read-only *with its uncommitted
write set still held* (`plans/DEVICES.md` D4c) so that nothing touches a medium
whose contents are in doubt until an operator decides. A copy-repair there
mutates exactly that medium, and the retained-write replay decision is then
being made about a device the filesystem has already altered.

**Severity.** A read-only guarantee that did not hold, on the one path where
"read-only" is a data-preservation decision rather than a policy one. Reachable
only by an explicit `scrub`/`health` call on a read-only handle, which nothing
in production makes yet; it becomes systematically reachable the moment the
maintenance runner exists, which is why it was fixed ahead of it.

**Fix (item M1 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`).** The mirror
copy-repair is one method, `ARXFS::repair_meta_copy`, which a read-only handle
declines — so the rule the three repair-on-read sites each spelled for
themselves, and this one did not, is stated once and cannot be forgotten again.
A read-only scrub writes nothing at all: no copy-repair, no refcount correction,
no cursor, no cleared progress record, no transaction; `health` skips only its
durable baseline and returns the reading it took.

The finding survives the fix rather than being traded for it. A mirror the pass
may not rewrite is `ScrubReport::metadata_damaged`, never a repair that did not
happen, and it reaches the health classification, because a copy that went bad
is the same medium signal whether or not the handle could rewrite it — a
read-only volume with degraded mirrors reports `Degraded`, not a clean bill.

**Also fixed, found by the same reading, each with its own regression test.**
Two more read-only writes sat on the same path and failed the whole call rather
than reporting: a bounded pass died at the cursor it may not persist (the exact
call the maintenance runner drives), and a pass that finished one a read-write
mount had paused died at the progress record it may not clear — which would also
have dropped, in memory only, a reference the committed root still names.
`ScrubReport::complete` became `ScrubReport::pass`, the three states that
actually exist, because a bounded pass that kept no position is a different
audit fact (`PassVerdict::Stopped`, with its own event ID) from one that will be
resumed: repeating the first never reaches past its own budget. And
`CheckReport::structure` is a public field whose type could not be named by a
consumer; `StructureVerdict` is exported.

---

## D65 — ARXFS's B-tree insert recursed 8 KiB of stack per tree level (FIXED)

**Where.** `drivers/filesystem/arxfs/src/btree.rs` `btree_insert_rec` (with
`btree_insert_leaf`), and the depth-unbounded descent they shared.

**Mechanism.** Insert descended by recursion, and each level kept two
block-sized buffers live *across* the recursive call: the node it was editing,
and a second one it read the child back into afterwards, only to recover the
child's minimum key for the separator. A split added a third. One level of
`btree_insert_rec` reserved 8360 bytes on the release build for x86_64. The
kernel hosts this driver on 32 KiB per-thread stacks behind a 4 KiB guard page,
and one `write_at` performs several nested tree mutations, so the overrun did
not need a deep tree: measured with a stack probe on the release build, one
write to a fragmented file used **48 097** bytes over a three-level extent tree
and **34 633** over a single leaf. Nothing bounded the depth on that path
either, so a corrupt child pointer leading back to an ancestor recursed until
the guard page caught it.

**Fix (item A1 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`).** `btree_insert` and
`btree_remove` are iterative: one descent records the path, the leaf is edited
in place, and each ancestor is rewritten on the way back up, taking the child's
minimum key from the step that just wrote it instead of reading the node back.
The node buffers live in one `TreeEdit` scratch the mount lends per mutation, so
none reaches the stack; the per-record `Vec` decode the remove path performed at
every level is gone with `btree_load_entries`; and every level re-entered on the
way up is validated at `child_level + 1`, so the write path refuses a cyclic or
over-deep tree as the read descent does.

**Measured after.** The same write uses **11 633** bytes over the three-level
tree and **11 609** over the single leaf, the difference no longer scaling with
depth; what remains is the driver's own on-stack block staging down the write
path, not the tree edit, whose frames are 904 and 968 bytes. Removing one extent
from an 800-extent tree allocated 596 times and now allocates 118. Device reads
are unchanged per insert and one fewer per remove.

**Also fixed, found by the same reading.** The merge of two empty siblings
indexed into an empty entry list and **panicked** on the write path of a corrupt
volume; it is a fail-closed device fault, with a regression test. And
`btree_insert` copied the caller's value with `copy_from_slice`, so a record of
the wrong width would have panicked rather than been refused.

**Left to its owner, not deferred.** The whole `write_at` chain still spends
~11.6 KiB of stack in block-sized staging buffers (`write_file`,
`store_cluster`, `map_write`, `commit`). It is constant, inside the 32 KiB
budget, and now guarded by a test — but it is sized by `MAX_BLOCK_SIZE`, so item
**B1**, which widens the filesystem block size, must move that staging off the
stack in the same change; recorded in that item.

## D66 — one `DriverError` spoke for three filesystem conflicts at once (FIXED)

**Where.** `lib/abi/src/driver/mod.rs` `DriverError::Busy`, its use across
`arxfs`, `adfs`, `ext4`, `fat32` and `kernel/core/src/fs/memfs.rs`, and the
per-operation mappings in `kernel/core/src/fs/delegate.rs`.

**Mechanism.** `Busy` meant "a name is already taken", "this directory is not
empty", "this move would make a directory its own descendant", and its
documented "retryable transient" — with nothing in the value saying which. The
VFS recovered the meaning from *which mapper the call site picked*:
`map_link_error` read it as `AlreadyExists`, `map_rename_error` as `NotEmpty`,
and the generic `map_driver_error` as `Io`. So `VfsDelegate::create` and
`VfsDelegate::remove`, whose own pre-checks answer `AlreadyExists` and
`NotEmpty` correctly, each reported a conflict that arose between that check
and the driver call as an **I/O error**; a self-descending rename was reported
as "directory not empty", advice to empty a destination that emptying could
never make lawful; and because `Busy.as_errno()` is
`Errno::WouldBlock`, any consumer reaching a filesystem driver without the
VFS's per-operation mapping saw `EWOULDBLOCK` where a coreutils-faithful
`mkdir`/`ln`/`rmdir` needs `EEXIST`/`ENOTEMPTY`.

**Fix (item D66 of `plans/IMPLEMENT-OUTSTANDING-ARXFS.md`).** `DriverError`
gains `AlreadyExists` (19), `DirectoryNotEmpty` (20) and `DirectoryCycle` (21),
each mapping to the `Errno` its condition already had (`AlreadyExists`,
`NotEmpty`, and — `abi-v1` having no `EINVAL` — `OutOfRange`). Every driver
site now names the conflict it met, `Busy` keeps only the transient it
documents, and `map_rename_error` is deleted: `map_driver_error` is one total
mapping every call site shares, leaving `map_link_error` a single override for
the one code whose meaning really is surface-specific (`Unsupported` — "this
format stores no such object" on the link surface). `VfsError` gains
`DirectoryCycle` so the in-kernel record stays precise. In-place, with no shim.

**Also fixed, found by the same reading.** The generated C header's
driver-error table was hand-maintained with no completeness guard and had
already drifted three variants behind `lib/abi` (`MediumError`,
`DeviceOffline`, `TooManyLinks` were unnameable from C). It is now
`DRIVER_ERROR_NAMES` beside `ERRNO_NAMES`, with the same dense-`1..=N` table
test, so a variant cannot be dropped from the C view again — and both tables
additionally round-trip every emitted code through `from_i32`, so an entry
whose decode arm is missing fails too. And `TooManyLinks`, reachable only
through `map_link_error`, would have become `Io` on any other surface; it is
in the shared mapping now.

**Not changed, and checked rather than assumed.** `DriverError::Unsupported`
is also read differently per surface, but no misreport is reachable through
it: the VFS resolves the parent before delegating (so "not a directory" cannot
arrive at the link mapping), refuses a directory operand itself, and never
passes `NodeKind::Symlink` to `create`. `mount`/`unmount` keep `Busy` — an
already-mounted volume and one with open files are the resource-in-use `EBUSY`
the code is for.
