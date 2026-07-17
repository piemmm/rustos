## NAME

stress — load the machine's CPU, memory, disk, and caches on demand

## SYNOPSIS

`stress [--cpu N] [--io N] [--vm N] [--vm-bytes B] [--hdd N] [--hdd-bytes B] [--cache N] [--all N] [--overcommit P] [--timeout T] [--temp-path DIR] [--monitor] [--quiet] [--background]`

## DESCRIPTION

Dispatches worker processes that load the machine deliberately, in the
spirit of the established `stress`/`stress-ng` tools: CPU spinners
(`--cpu`), memory allocate-and-touch workers (`--vm`), small-buffer
write/sync workers (`--io`), large sequential disk writers (`--hdd`),
and cache-churning re-readers (`--cache`, a TAIRiX addition). Each
worker is its own swappable process; the controlling process pins its
own memory (`mem_pin`, requiring `CAP_MEM_PIN`) so it stays responsive
under the very pressure it creates, and observes `Ctrl-C`/`Terminate`
so every end of the run — completion, timeout, or a signal — tears the
workers down, reaps them, and removes every scratch file.

Memory and disk targets are sized from the machine itself: unless
`--vm-bytes`/`--hdd-bytes` name explicit figures, the vm workers share
half the discovered RAM and the hdd workers half the scratch volume's
free space. `--overcommit P` rescales those discovered targets to `P`
percent of the resource; over 100 the workers push into pressure, and
the typed refusals that produces (a full volume, a resource limit) are
counted and reported as expected outcomes, never retried and never a
crash. Loading the machine needs no privilege beyond the caller's own
resource limits — the limits are the defence, and `stress` respects
them.

Disk-touching workers write only beneath the scratch directory — the
app-scoped per-user cache directory (`$HOME/Library/stress`) unless
`--temp-path` names another — and every scratch file is removed on
teardown, including the signal paths.

A summary is printed when the run ends (suppressed by `--quiet`), and
a machine-readable `summary` record is emitted on the advisory
standard-information stream (fd 3).

## OPTIONS

- `--cpu N`, `--io N`, `--vm N`, `--hdd N` — dispatch `N` workers of
  the named kind, with the GNU `stress` meaning.
- `--cache N` — dispatch `N` cache-churn workers (TAIRiX-only:
  repeated cold directory walks and re-reads move the kernel's
  reclaimable-cache ledgers).
- `--all N` — `N` workers of every kind.
- `--vm-bytes B`, `--hdd-bytes B` — each worker's byte target, with
  the GNU suffixes (`k`, `m`, `g`, `t`; e.g. `256M`). Defaults are
  sized from discovered RAM / free space.
- `--overcommit P` — scale the discovered vm/hdd targets to `P`
  percent of the resource; may exceed 100 (refusals are then expected
  outcomes).
- `--timeout T` — stop after `T` (`s`/`m`/`h` suffixes; e.g. `5m`).
  No default: without it the run continues until a signal ends it.
- `--temp-path DIR` — the scratch directory for the disk-touching
  workers.
- `--monitor` — run `sysmon` in the foreground for the duration; the
  run is reported when the monitor exits. Contradicts `--background`.
- `-q, --quiet` — suppress the stdout summary and progress lines
  (errors still reach stderr).
- `--background` — print the detached controller's PID and return the
  prompt (implies `--quiet`). The shell's `&` job form works too;
  this flag is for scripts.
- `-h, -?, --help` — show this command's own short help and exit.
- `--version` — print the tool's name and version and exit.

## EXIT STATUS

- `0` — the run completed (typed worker refusals are expected
  outcomes and do not fail it).
- `1` — a worker failed outright, or the run could not be set up.
- `2` — the command line was not understood.
- `130` / `143` — `Ctrl-C` / `Terminate` ended the run, after the
  workers were torn down and the scratch files removed.

## ENVIRONMENT

- `HOME` — locates the default scratch directory
  (`$HOME/Library/stress`).
- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `sysmon`
- `top`
