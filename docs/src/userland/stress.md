# Load generator (`stress`)

`stress` (`userland/apps/stress`, `plans/STRESSTEST.md` ST5) is the
system app-store command app that loads the machine deliberately, in
the spirit of the established `stress`/`stress-ng` command surface: a
pinned, signal-observing **controller** process dispatching swappable
**worker** children that load the CPU, memory, the filesystem write
path, disk throughput, and the kernel caches — separately or combined
— with a configurable overcommit level, a run timeout, quiet and
background modes, and an option to run `sysmon` alongside. It is the
provocation half of the stress-testing tier; `sysmon` is the
observation half.

## Process model

One controller, N workers (`--cpu`/`--vm`/`--io`/`--hdd`/`--cache`,
`--all`). Each worker is the **same binary re-entered in worker mode**:
the controller spawns the kernel's reserved `@self` path token — the
kernel substitutes the caller's *attested* program path (never
`argv[0]`, which is data its spawner chose) and runs the full bundle
load gate — handing it a closed, fail-closed argv block naming the
worker's kind, byte target, index, and scratch directory. Workers are
deliberately unpinned (swappable), so memory workers exercise the
compressed swap tier; the controller pins itself (`mem_pin`) so it
stays responsive under the very pressure it creates.

The controller's loop is **one wait-set**: child exits
(`WAITSET_CHILD_ANY`), the signal intake (`signal_intake`, so `^C` and
`Terminate` are observed rather than fatal), and the timeout/grace
deadline as the bounded wait — never a poll loop. On any end of the
run — completion, timeout, an observed signal, a quit `--monitor`
session — it signals every live worker `Terminate`, escalates to
`Kill` after a bounded two-second grace, reaps them all, removes every
scratch file (the signal paths included), prints the summary (unless
`--quiet`), emits a `summary` record on fd 3 (`stdinfo`), and exits
with the familiar status: `0` on completion, `1` on a genuine worker
failure, `130`/`143` on `Interrupt`/`Terminate`.

## Load subsystems

Each worker runs one **bounded, restartable unit** in a loop — a typed
resource refusal (a full volume, a resource limit, a refused
allocation) is reported once and exits with the refusal status (3),
which the controller counts as an *expected outcome*, never a retry
(under `--overcommit` refusals are the point):

- **cpu** — syscall-free integer/float arithmetic: exercises
  preemption (a worker that never yields is involuntarily preempted).
- **vm** — allocate/touch/re-touch anonymous memory in a rotating
  pattern sized by `--vm-bytes`: allocation, fault, and — once the
  restartable-user-fault prerequisite enables the tier for running
  tasks — `ramzip` compress-out and fault-in.
- **io** — small-buffer stream writes with frequent `fs_sync` and a
  verified read-back: the write path and the block cache.
- **hdd** — large sequential patterned write/verify/delete cycles
  sized by `--hdd-bytes`: throughput and free-space accounting,
  self-cleaning per unit.
- **cache** — repeated cold walks and re-reads over a small scratch
  tree: churns the filesystem/block caches so their ledgers move.

Disk-touching workers write **only** beneath the scratch directory —
the app-scoped per-user cache directory (`$HOME/Library/stress`) by
default, `--temp-path` to override — under the caller's own attested
identity through the secured VFS; the run holds no authority its user
does not.

## Sizing

Byte targets are **policies over discovered hardware, never frozen
scalars**: the vm workers share half the boot-attested installed RAM
(`boot_facts`), the hdd workers half the scratch volume's free space
(the unprivileged `MOUNT_LIST` walk, longest-covering-mount match);
`--overcommit P` rescales the discovered targets to `P` percent of the
resource (over 100 pushes into pressure); explicit
`--vm-bytes`/`--hdd-bytes` win outright. When a discovery is
unavailable the policy falls back to documented conservative
per-worker figures (32 MiB vm / 16 MiB hdd) rather than failing a run
whose purpose is the load. Loading the machine needs no privilege
beyond the caller's own resource limits — the limits are the defence
(`stress` requests only `CAP_MEM_PIN` beyond `CAP_CONSOLE_WRITE` /
`CAP_FS_ACCESS` / `CAP_PROC_SPAWN`), and refusals are reported, never
retried.

## Host-testable shape

The crate is a host-testable library plus a thin freestanding program:
the option grammar (`command`), the worker argv codec (`worker`), the
sizing policy (`sizing`), the five load units over an injected
`Scratch` seam (`load`), the controller state machine with every
teardown path (`ctrl`), and the report shapes (`report`) are all
proven under plain `cargo test`; the program half
(`run`/`worker_main`/`controller_main`) only wires them to the real
syscalls. The end-to-end proof is the `stress_qemu_aarch64` vertical:
a full production boot, a short `--all` run under `--timeout` on the
console, the returned prompt, and the post-load `sysinfo
pressure`/`sysinfo reclaim` renders on the transcript.
