## NAME

sysmon — watch the kernel's memory and load live

## SYNOPSIS

`sysmon [-d secs.tenths] [-h | -?]`

## DESCRIPTION

Shows a live, full-screen view of the kernel's memory and load through
the System Information API: physical memory, the kernel heap, the
memory-pressure band with its history, the reclaimable-cache ledger,
the `ramzip` compressed tier, the pinned-memory aggregate, per-CPU
load, the kernel IRQ table, and a process census. It is built to stay
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
- `p` — cycle the detail panel: reclaimable caches, the compressed
  tier, per-CPU load, interrupt lines, processes.
- `r` — refresh now.
- `+` / `-` — lengthen / shorten the refresh interval by one second,
  between 0.1 and 60 seconds.
- Up/Down, PageUp/PageDown, Home/End — scroll the detail panel.
- `h`, `?` — toggle the in-session key overlay.

Six summary lines precede the detail panel: the title (uptime, load
averages, and the pin state); the memory figures in MiB with the
pinned aggregate; the pressure band with its depth gauge, free and
reserve figures, and entry counters; the band-history strip (one glyph
per refresh: `.` normal, `-` mild, `=` moderate, `#` severe, `!`
critical); the aggregate CPU line; and the task census.

Every figure travels through the System Information API — there is no
`/proc` to scrape. The kernel-wide statistics queries need
`CAP_SYSINFO_KERNEL`, the interrupt-lines panel needs `CAP_SYSINFO_HW`
(it names which driver owns each line), and the all-process census
needs `CAP_SYSINFO_GLOBAL`: a caller without one sees that panel's
refusal spelled out while the rest of the session continues. The
interrupt panel shows one row per bound line — its id, the owning
driver task, the interrupt count since boot, and whether the line is
quarantined. The full interactive process list is `top`'s job; the
processes panel here shows the census and the top consumers by `%CPU`
and by memory only.

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
