## NAME

sysmon — watch the kernel's memory and load live

## SYNOPSIS

`sysmon [-d secs.tenths] [-h | -?]`

## DESCRIPTION

Shows a live, full-screen view of the kernel's memory and load through
the System Information API: physical memory, the kernel heap, the
memory-pressure band with its history, the reclaimable-cache ledger,
the `ramzip` compressed tier, the pinned-memory aggregate,
mounted-volume storage usage, per-CPU load, the kernel IRQ table, and a
process census. It is built to stay
usable while the system is under deliberate stress, and is quiescent
between refreshes at idle.

At startup the monitor pins its own memory (`mem_pin`, requiring
`CAP_MEM_PIN`) so it never stalls on its own page fault-in under the
very pressure it observes. A refused pin is reported on the title line
and the session continues unpinned — the pin is incidental, never
fatal.

The display refreshes itself every interval (3.0 seconds unless `-d`
changes it), and `r` refreshes it immediately. The monitor takes no
operands: it is controlled with keys pressed inside the session.

- `q` — quit.
- Left / Right (or `p`) — switch the detail panel: reclaimable caches,
  the compressed tier, mounted-volume storage (disks), per-CPU load,
  interrupt lines, processes.
- `r` — refresh now.
- `+` / `-` — lengthen / shorten the refresh interval by one second,
  between 0.1 and 60 seconds.
- Up/Down, PageUp/PageDown, Home/End — scroll the focused panel.
- `h`, `?` — toggle the in-session key overlay.

A fixed summary block precedes the detail panel: a title bar (uptime,
load averages, and the pin state); three bar gauges (`Mem`, `Pres`,
`CPU`, described under THE BAR KEY below); a colour-coded band-history
strip (one glyph per refresh: `.` normal, `-` mild, `=` moderate, `#`
severe, `!` critical); the task census; and a panel tab bar showing
every panel with the focused one highlighted. Colour is only
reinforcement: on a terminal without colour the gauges still fill, the
memory glyphs still name their category, the inverted table headers
still read via reverse video, and every line still reads.

**The bar key.** The three summary gauges are bracketed `[…]` bars, and
the `?` overlay reproduces this key inside the running session.

The memory bar (`Mem`) is a stacked bar whose cells name what physical
memory holds — a disjoint split of used memory, so nothing is counted
twice:

- `#` — user-resident memory (green).
- `K` — the kernel heap (cyan): the kernel's own heaps and slabs.
- `=` — other in-use memory (magenta): everything used but not
  attributed above (caches, buffers, kernel frames).
- blank — free memory.

The trailing text gives used / total MiB, the used percentage, the
kernel-heap size, and, when non-zero, the `ramzip` compressed-store and
`pinned` figures (reported beside the bar rather than as slices,
because they overlap the buckets above).

The pressure bar (`Pres`) and the CPU bar (`CPU`) are coloured by
severity (green below 60%, yellow below 85%, red above; the pressure
segments colour by band depth). The CPU bar fills with `#` busy cells
over blank idle track: TAIRiX accounts CPU time as busy versus idle
only — there is no user/system/iowait split — so it shows a single
honest busy category, with per-core detail in the `cpu` panel.

**The detail panels.** Left / Right (or `p`) steps through six panels.
Each has an inverted column header.

- `caches` — the reclaimable-cache ledger: memory the kernel could hand
  back under pressure without data loss, one row per reclaim class.
- `ramzip` — the compressed memory tier, which compresses cold
  anonymous pages into a smaller in-RAM store instead of paging them
  out. Its sections are the live tier footprint (entries, logical,
  stored, metadata, and bytes saved), the capacity caps, the compression
  (store) path with its accept rate, the restore (fetch) path with its
  success rate, and the warm-up restorer's counts. Each hit ratio is a
  percentage, or `-` when its denominator is idle rather than a ratio
  invented from zero.
- `disks` — one `df`-style row per mounted volume: mount point,
  filesystem type, total size, used, available, use percentage, and a
  usage bar. A volume whose driver reports no capacity shows `capacity
  unknown` rather than a fabricated size, and a surprise-removed or
  recovery-conflicted volume is marked. There are no per-device I/O
  throughput counters in the API, so this is honest capacity and usage,
  not fabricated transfer rates.
- `cpu` — each CPU's busy share over the interval, its run-queue depth,
  and its context-switch and preemption counts.
- `irqs` — one row per bound interrupt line: its id, the owning driver
  task, the interrupt count since boot, and whether the line is
  quarantined.
- `procs` — the process census and the top consumers by `%CPU` and by
  memory. The full interactive process list is `top`'s job.

Every figure travels through the System Information API — there is no
`/proc` to scrape. The kernel-wide statistics queries need
`CAP_SYSINFO_KERNEL`, the interrupt-lines panel needs `CAP_SYSINFO_HW`,
and the all-process census needs `CAP_SYSINFO_GLOBAL`: a caller without
one sees that panel's refusal spelled out while the rest of the session
continues.

## OPTIONS

- `-d, --delay <seconds>` — the interval between automatic refreshes,
  in seconds with an optional fraction (only the first fractional
  digit, tenths, is kept): `sysmon -d 1.5` refreshes every 1.5
  seconds. Defaults to 3.0. GNU `top` accepts a zero delay and
  refreshes as fast as it can; TAIRiX never busy-loops, so a zero is
  clamped to the 0.1 s minimum.
- `-h, -?` — show this command's own short help and exit. Inside a
  running session the same keys toggle the key overlay instead.

## EXIT STATUS

- `0` — the session ended with `q`, or the short help was shown.
- `1` — the terminal failed; the reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such
  as `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
