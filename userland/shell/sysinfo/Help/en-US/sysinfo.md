## NAME

sysinfo — query system information

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Issues one typed query to the System Information API and renders the
reply. RustOS has no `/proc` and no `/sys`: this command is the
terminal face of the same versioned, capability-checked API every
program uses, and no path bypasses the capability check.

The queries:

- `processes`, `ps` — list processes, one row per process.
- `memory`, `mem` — kernel memory statistics (needs
  `CAP_SYSINFO_KERNEL`).
- `hardware`, `hw` — the detected hardware tree (needs
  `CAP_SYSINFO_HW`).
- `identity`, `id` — machine identity and OS version.
- `uptime` — time since boot and the boot wall-clock time.
- `limits`, `rlimits` — your effective resource limits and live usage.
- `seats` — the seat inventory: each display's owner and foreground
  console (needs `CAP_SYSINFO_HW`).
- `pressure` — the live memory-pressure gauge: band, watermarks, and
  transition counters (needs `CAP_SYSINFO_KERNEL`).
- `reclaim` — the reclaimable-cache ledger, one row per class (needs
  `CAP_SYSINFO_KERNEL`).
- `ramzip` — the compressed memory tier's counters (needs
  `CAP_SYSINFO_KERNEL`).
- `cpu` — per-CPU run-queue depth, context switches, and preemptions
  (needs `CAP_SYSINFO_KERNEL`).
- `help` — this command's own short help.

With no query, the short help is shown.

## OPTIONS

- `--all, -a` — with `processes`: list every process on the system
  rather than only your own; the service grants this view only to a
  caller holding `CAP_SYSINFO_GLOBAL`.
- `-h, -?` — show this command's own short help.

## EXAMPLES

- `sysinfo identity` — print the machine identity and OS version.
- `sysinfo ps --all` — list every process on the system.

## EXIT STATUS

- `0` — the query was answered and rendered.
- `1` — the service refused or failed, or the result could not be
  delivered.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `ps`
- `top`
