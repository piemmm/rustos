# `stress` — comprehensive load generator

The system app-store command app that loads the machine deliberately,
in the spirit of the established `stress`/`stress-ng` surface
(`plans/STRESSTEST.md` ST5): an unswappable controlling process
spawning swappable workers that load the CPU, memory, the filesystem
write path, disk throughput, and the kernel caches — separately or
combined, with a configurable overcommit level, a run timeout, signal
teardown, quiet and background modes, and an option to run `sysmon`
alongside (`--monitor`).

## Shape

The crate is both the `rustos-stress` load library and the `Run`
entry-point binary of the `stress.app` bundle:

- `src/command.rs` — the closed §7.3 option grammar with GNU `stress`
  spellings (counts, byte sizes with `k`/`m`/`g`/`t`, `s`/`m`/`h`
  timeouts), fail-closed on every malformed or contradictory input.
- `src/worker.rs` — the worker role's argv codec: the controller
  re-enters this same binary through the kernel's attested `@self`
  spawn token; the decode is fail-closed and `REFUSED_EXIT` (3) is the
  typed-refusal exit status.
- `src/sizing.rs` — the byte-target policy over discovered RAM
  (`boot_facts`) and the scratch volume's free space (the unprivileged
  `MOUNT_LIST` walk), with documented conservative fallbacks — a
  policy over discovered hardware, never a frozen scalar.
- `src/load.rs` — the five bounded, restartable load units; the
  disk-touching ones run over the injected `Scratch` seam so host
  tests prove refusal handling and verify-mismatch detection.
- `src/ctrl.rs` — the controller's event-driven state machine
  (`Running → Draining → Killing`): child exits, observed signals,
  timeout, the grace escalation to `Kill`, the monitor policy, and
  the 0/1/130/143 exit decision — all host-provable.
- `src/report.rs` — the GNU-shaped dispatch/summary lines and the
  fd-3 `summary` record (`AGENTS.md` §20.1).
- `src/run.rs` + `src/worker_main.rs` + `src/controller_main.rs` —
  the freestanding program: role dispatch, the syscall-backed
  scratch, and the one wait-set (child exits + signal intake +
  deadlines) the controller parks on — never a poll loop.

Teardown is total on every exit path — completion, timeout, `^C`,
`Terminate`, a quit monitor, a failed dispatch: every live worker is
asked to `Terminate`, `Kill`ed after a bounded grace, reaped, and every
scratch file the run could have created is removed.

## Bundle

A full self-contained `.app` bundle: `AppInfo.toml` (requesting
`CAP_CONSOLE_WRITE`, `CAP_FS_ACCESS`, `CAP_PROC_SPAWN`,
`CAP_MEM_PIN`), the `Run` rxe, and the `Help/` tree (thirteen locales,
`en-US` canonical). In the OS image the bundle is discovered from the
store like any other app — nothing is compiled into the kernel or the
image builder.

## Layering & safety

`no_std` (with `alloc`). It links only `lib/*` crates — `lib/abi`,
`lib/procinfo`, and `lib/help` — never a kernel or driver crate
(`AGENTS.md` §17.4). No `unsafe`, no `unwrap`/`expect`/`panic!` in
production paths (`AGENTS.md` §2.9). Worker refusals (a full volume, a
resource limit, a refused allocation) are typed, counted, and reported
— never retried until they work. Stability tier: experimental.
