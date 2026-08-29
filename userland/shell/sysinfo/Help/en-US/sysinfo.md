## NAME

sysinfo — query system information

## SYNOPSIS

`sysinfo <query>`

## DESCRIPTION

Issues one typed query to the System Information API and renders the
reply. TAIRiX has no `/proc` and no `/sys`: this command is the
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
- `cpuinfo` — the per-CPU processor report (a `/proc/cpuinfo`-superset):
  each CPU's model/vendor, performance class, ISA-extension flags, raw
  identity register, the live measured core-clock speed (in MHz — or an
  honest "unknown" where no core-clock counter exists), and the fixed
  reference/timebase frequency. Public hardware facts, no capability
  required.
- `irq`, `irqs` — the kernel IRQ table: one row per bound interrupt
  line — its id, the owning driver task, the interrupt count since
  boot, and whether the line is quarantined (needs `CAP_SYSINFO_HW`).
- `frames` — what each desktop session's composited frames have cost:
  one row per publishing session — the pixels its frames recomposed and
  the mean per frame, the overdraw multiple (layer contributions per
  damaged pixel), the share resolved by copying an opaque run, the
  pixels a recomputed backdrop frost re-blurred, the pixels converted to
  scan-out, the worst single frame and its share of the screen, the
  driver calls that published them, and the window-furniture cache's
  hits and misses. These are the desktop's own figures — only a process
  holding a compositor can count pixels — so each row names the
  publisher the service attested it to, and a session that has published
  nothing prints no row (needs `CAP_SYSINFO_GLOBAL`).
- `storage`, `io` — per-volume storage I/O health: one row per
  fault-aware block-backed volume — a prefix of its durable id, the
  serving block-service endpoint, its current availability
  (available/degraded/recovering/lost), and the cumulative outcome
  counters (completions, resets, timeouts, medium errors, reissues) a
  failing or flapping disk becomes visible on (needs
  `CAP_SYSINFO_KERNEL`).
- `raid`, `arrays` — the composed RAID arrays and the devices the array
  composer holds: one row per array — a prefix of its identity, its
  level, its health (optimal/degraded/recovering/failed), its in-sync
  and defined member tallies, its stripe unit, its block count, and any
  rebuild or verification pass in progress — then one row per device —
  its hardware-tree node, the array it belongs to (a dash for an
  unaffiliated candidate), its slot, its disposition
  (candidate/held/in-sync/resyncing/faulted), its size, and the
  metadata generation it carries (needs `CAP_SYSINFO_HW`).
- `show <resource-ref>` — read one `info:`/`state:`/`stats:` resource
  reference and print its value. Those namespaces are typed values served
  through this API, never byte streams, so this is how one is read — `cat`
  cannot open it. A denial names the capability the resource needs.
- `describe <resource-ref>` — print the response envelope instead of the
  value: its producer, the authorization it was served under, and the
  payload's own metadata — for a metric its kind, unit, reset behaviour, and
  sampling window; for a fact its type and sensitivity.
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
- `sysinfo frames` — show what the desktop's frames are costing.

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
