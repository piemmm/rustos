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
- **D5 — `mem-pin-migration` intermittent multi-vCPU-TCG stall — DONE.**
  Root-caused to a lost-wakeup in the vertical's own secondary-CPU idle
  loop and fixed structurally (not a load artifact, not a budget bump).
- **D6 — `docs-check` cross-crate rustdoc failure documenting
  `tairix-kernel` — NON-REPRODUCING.** Formerly a `cargo xtask ci`
  `docs-check` failure (`error[E0432]: unresolved import
  tairix_abi::driver::virtio_pci::virtio_pci_window_resource`); it does
  not reproduce on the pinned toolchain from a full `cargo clean`. Kept on
  record with its reproduction procedure in case it recurs.
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
  culprit spinlock. The structural fix is OPEN, blocked on that evidence.
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
    silent overflow.
  - **Standing regression coverage (no new vertical — §2.2/§2.3).** Concurrent
    early-boot service bring-up during root-mount is exercised by
    `spawn_session_qemu_*` (4 services + session); EL0 multitasking under the
    live scheduler by `spawn_el0_timeshare_qemu_*` and `scheduler_stress_qemu`;
    guard-arena growth/fail-closed by the `stack_arena` host tests
    (`kernel/tairix-kernel/src/stack_arena_tests.rs`). A future rise of the
    fixed no-heap service caps (`startup::MAX_SERVICES` /
    `supervisor::MAX_SUPERVISED_SERVICES = 4`) belongs with the userland-heap
    PID 1 (`plans/SPAWN.md` SP5b) and lands with its own N-service guard then.

- **D19 — `autoload-input-qemu-aarch64` terminal stage: shell spawns
  `/System/Apps/0.app` instead of the typed command — CORE FIXED; a distinct
  downstream harness-contract drift remains as D20.** Root cause (established
  with kernel probes on the terminal→pty write and the seat `key_inject` path,
  since removed): the harness typed `sleep 3600\n` **before the terminal window
  was focused**. Evidence — every `sleep 3600` key edge (scalars 115,108,101,
  101,112,32,51,54,48,48) was correctly injected into the seat, but during the
  whole typing window every app-ward `MessageDelivered` went to the **files**
  window's port (`e117…0f`); the **terminal**'s port (`e117…10`) received its
  first event only ~0.8 s later, so `sleep 360` landed on the still-focused
  files window and only the trailing `0`+Enter reached the belatedly-focused
  terminal → `elsh` spawned `/System/Apps/0.app`. It was **never** a kernel
  deadlock/lost-wakeup (all IPC balanced; the guest sat idle at the shell
  prompt) and **not** a pty/`lib/tty` line-discipline bug — the pty carried
  exactly the two bytes it was handed (`0`, `\r`). The trigger was the
  FONT-SERVICE speedup changing the delivery/creation cadence: the typed-command
  step was gated on "7 generic window-event deliveries", which the files window
  alone satisfies long before the terminal window is even created, while the
  terminal-focus click is gated on a separate clock (3 frame-maps) — the two
  orderings were no longer guaranteed.
  - **Core fix landed (this change).** The typed command is now gated on a
    guest-emitted `TERMINAL_FOCUSED_MARKER` (the first app-ward delivery to the
    *second* distinct window port — the terminal receiving focus), emitted by
    the guest test kernel's `note_window_delivery`, mirroring the existing
    `CTRL_C_ARM_MARKER`/`FM9B_PICKER_OPEN_MARKER` readiness handshakes. With it,
    `sleep 3600` reaches `elsh` intact and `sleep.app` spawns (verified: serial
    shows `bundle=/System/Apps/sleep.app`, the AW4 round-trip witness, and
    `PTY ctrl-c armed`) — no `0.app`. Files touched:
    `tests/integration/autoload_input_qemu_aarch64/{lib.rs,src/main.rs}` and the
    `tools/xtask` runner gate.
  - **Remaining (tracked as D20).** The vertical still does not reach PASS: the
    *downstream* stages (pty Ctrl-C recovery, FM9-a/-b/-c, FM10, FM11) sequence
    on cumulative window-event delivery **counts** the same speedup shifted, so
    the FM9 pointer clicks now fire early and hijack focus before the recovery
    `true` spawns. `autoload-input-qemu-aarch64` therefore remains **RED**
    (user-approved) until D20 recalibrates that contract; the D19 input-drop
    itself is fixed and correct independent of it.

- **D20 — `autoload-input-qemu-aarch64` post-terminal contract is
  delivery-count-sequenced and drifted by FONT-SERVICE — OPEN.** Every stage
  after the terminal round trip (the pty Ctrl-C recovery, FM9-a New-Folder +
  rename, FM9-b Viewer/picker, FM9-c delete, FM10 move-to-Trash, FM11 empty
  Trash) is gated on **cumulative `MessageDelivered` counts**
  (`TERMINAL_ROUND_TRIP_DELIVERIES = 28`, `CTRL_C_RECOVERY_DELIVERIES = 40`,
  `FM9_TYPING_DONE_DELIVERIES = 41`, and the FM9 offsets from it). The
  FONT-SERVICE speedup changed the real counts (the `sleep` spawn now lands at
  count ~37, not 28; a later spawn reaches ~66), so the low FM9/FM9-b thresholds
  are already exceeded *during* the terminal stage: the FM9 clicks fire early,
  move focus off the terminal, and the Ctrl-C recovery `true` never spawns —
  the run stalls at the FM9 pointer stage. Worse, the shifted thresholds now
  **overlap across stages**, so the `≥`-style spawn witnesses (e.g. "a spawn at
  ≥40 is the recovery `true`") can false-latch on an unrelated later spawn (the
  Viewer). Bumping the numbers is **not** a robust fix. Proper fix: convert the
  post-terminal stage sequencing from fragile cumulative counts to
  **guest-emitted readiness markers** (the durable pattern the terminal-focus
  (D19), `CTRL_C_ARM_MARKER`, `FM9B_PICKER_OPEN_MARKER`, and
  `FM11_TRASH_FILLED_MARKER` handshakes already use) so each stage waits on a
  fact about the guest, not a timing-fragile count — and make each spawn/FS
  witness uniquely attributable rather than a shared `≥` threshold. Do **not**
  mask it by re-tuning counts or bumping the budget (§2.17, §7 no-flaky). Until
  it lands, `autoload-input-qemu-aarch64` is RED at the FM9 stage. (The D19
  terminal-focus fix is a prerequisite and is already in place.)

These are **distinct in kind**: D1 finishes an interrupt-model fix, D2
and D4 are §27 foundational-completeness defects, D3 is an Arch-HAL
parity gap, D5 was a test-harness idle-loop lost-wakeup (fixed), D6
is a rustdoc/docs-build failure, D10 was a fragile QEMU-harness
readiness gate (fixed), D18 was an early-boot concurrent-spawn scare that
proved non-reproducing once FONT-SERVICE removed the per-app font payload
(closed), D19 was the `0.app` input-drop — the harness typing before the
terminal window was focused — whose **core fix is landed** (terminal-focus
marker gate), and D20 is the remaining post-terminal delivery-count-contract
drift that keeps that same vertical RED. Do not
collapse them into one change; land each on its own whole-project-green gate
(§7).

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
| `lib/sync::Epoch` | epoch-based reclamation — `register`/`pin`/`defer_free`/`defer`/`advance`; complete lifecycle | §27-complete |
| `lib/collections::BitSet256` | 4×u64; full set algebra + subset + popcount + ascending fused iter, all O(1) | §27-complete |
| `lib/caps::CapabilitySet` | 256-bit; full algebra + subset-enforcing `delegate` + `revoke` + wire round-trip; delegation-never-widens property-tested (§19.7) | §27-complete |
| `lib/caps::CapToken` | unforgeable token vocabulary (`token.rs`) | §27-complete |
| `kernel/ipc::PortRegistry` | `BTreeMap` endpoint + name indexes — O(log n) `lookup`/`resolve`/`register`/`unregister`; bulk `teardown_owned_by` O(n) only on process exit (not a hot path) | §27-complete |
| `kernel/ipc` `call`/`port`/`notify` | reply/mailbox/notification queues over the shared `waitq` wake/drain discipline (D2) | §27-complete |
| `lib/kalloc::FreeListAllocator` | coalescing address-sorted first-fit free list; growable/shrinkable via `HeapSource`; deterministic OOM (null, never panic) | §27-complete (first-fit is O(free-blocks); coalescing bounds the count — the standard general-purpose design, not a thin slice) |
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

## D6 — `docs-check` cross-crate rustdoc failure documenting `tairix-kernel` — NON-REPRODUCING (monitoring)

**State:** does **not** reproduce on the pinned toolchain
(`nightly-2026-07-03`); `docs-check` is green. Kept on record — not
deleted — so that a recurrence has its prior context and its
reproduction procedure to hand.

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

**If it recurs.** Treat it as a real cross-crate-rustdoc / mergeable-info
defect (not a load flake, §7): capture whether it appears only under the
concurrent `cargo xtask ci` static-gate group (memory pressure) vs.
standalone, and the structural fix is to drop `-Z rustdoc-mergeable-info`
from `run_docs_check` (`tools/xtask/src/commands.rs`) — the mergeable-info
model is a doc-build *speed* optimisation, and correctness of the doc
build takes precedence (§2.16). Do not reinstate the flag until the
resolution failure is root-caused in rustdoc.

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
`tx_header`/`tx_data`/`tx_inflight` fields are replaced by a fixed,
allocation-free `TxStaging` pool of header+frame staging pairs, sized to the
transmit virtqueue (`TX_INFLIGHT_MAX = TX_QUEUE_SIZE / 2`). Each `service`
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
the decisive `DAIF.F` constraint in `.junie/fix-details.md`). The blocking
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
- **B4 (use the sampler to fix D13) — REMAINING.** The tool is proven (B3) and
  active on the QEMU debug image; reproducing the nondeterministic
  `stress --cpu N` multi-core wedge and fixing the SMP defect structurally
  (with a regression test, never a timeout/limit bump §2.17) is the open work.
  Not yet reproduced/fixed.

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

**Fix.** `tairix_kalloc::FreeListAllocator` now takes an installable
interrupt-control seam (`install_irq_control(disable, restore)`, two set-once
`fn`-pointer atomics read outside the lock); `with_inner` masks the current
CPU's interrupts *before* acquiring the lock and restores them *after*
releasing — foreclosing the reentrant self-deadlock. Each freestanding bin
installs its arch primitive at `boot()` entry, before interrupts are ever
enabled and before any secondary CPU/hart starts (one process-global install
covers every core; the hooks mask the *current* CPU): aarch64 via
`DaifIrqControl`, x86_64 via `RflagsIrqControl`, riscv64 via `sstatus.SIE`
(`csrrci`/`csrs`); the interrupt-free `wasm32` port and the host test build
install nothing (that window is single-CPU with interrupts already masked).
Regression test `the_lock_masks_interrupts_via_the_installed_control`
(`lib/kalloc`) asserts the lock masks then restores interrupts, balanced, once
a control is installed and not before. Host-tested and all four Tier-1 kernels
build clean; the metal confirmation (no boot wedge on the Pi 4 debug image) is
pending a user boot.

**Done when:** the near-every-boot Pi 4 boot wedge no longer reproduces on
metal with the interrupt-safe allocator lock, and `stress --cpu 20` no longer
wedges on metal + the QEMU stress vertical. (The FIQ and EDPCSR samplers remain
the standing masked-section observers for any *future* wedge.)

---

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
