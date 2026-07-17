# `sysmon` — live kernel-memory and load monitor

`sysmon` (`userland/apps/sysmon`, `plans/STRESSTEST.md` ST4) is the
system app-store command app that watches every aspect of the kernel's
memory and load through the System Information API, full screen and
live: physical memory, the kernel heap, the memory-pressure band with a
scrolling history strip, the reclaimable-cache ledger, the `ramzip`
compressed tier, the pinned-memory aggregate, per-CPU load, and a
process census. Its primary function is observing a machine under
deliberate stress; at idle it is quiescent between refreshes (the input
wait is bounded by the refresh interval — never a poll loop).

## Self-pinning

At startup the monitor pins its own memory (`mem_pin`, requiring
`CAP_MEM_PIN`) so it never stalls on its own page fault-in under the
very pressure it observes. The pin is incidental, never fatal: a
refusal — no capability in the launching intersection, or the
`pinned-memory-bytes` limit — is a stated reason on the title line
(`[unpinned: …]`) and the session continues unpinned. The kernel-side
action still fails closed; only the session survives.

## Data sources and degradation

Every figure travels through `sysinfo-v1` — there is no `/proc`. The
four kernel-statistics fetches (`MEMORY_PRESSURE`, `RECLAIM_STATS`,
`RAMZIP_STATS`, `CPU_LOAD`) are the shared `tairix_procinfo::kstats`
walks — the same definitions the `info:`/`stats:` resolver uses, so the
two consumers can never diverge. `KERNEL_MEMORY_STATS`, the ungated
`UPTIME`/`LOAD_AVERAGE`/`CPU_TIME_STATS`, and the process lists round
out the panels.

Each query degrades independently: a capability refusal
(`CAP_SYSINFO_KERNEL` for the kernel-wide statistics,
`CAP_SYSINFO_GLOBAL` for the all-process census) renders as that
panel's spelled-out refusal, a failed call as the figure's honest
absence — the session continues either way. The monitor's only fatal
failure is the terminal itself: a hiccuping or refusing service must
never kill the observer built to run under stress.

## Layout and keys

Six summary lines — title (uptime, load averages, pin state), memory
overview in MiB with the pinned aggregate, the pressure band with a
five-step depth gauge and free/reserve/entry figures, the band-history
strip (`.` normal, `-` mild, `=` moderate, `#` severe, `!` critical,
one glyph per refresh, bounded at 120 samples), the aggregate CPU line
(interval busy share plus the summed switch/preemption counters), and
the task census — sit above a scrollable detail panel the `p` key
cycles through four views: the reclaim ledger table, the `ramzip`
counter block, the per-CPU load table, and the process top consumers
(by `%CPU` over the interval and by resident bytes; the full
interactive list remains `top`'s job).

Keys follow the `top` conventions: `q` quit, `r` refresh now, `p`
cycle the panel, `+`/`-` lengthen/shorten the refresh interval by one
second within 0.1–60 s (re-armed on the very next wait), arrows and
PageUp/PageDown/Home/End scroll the panel, `?`/`h` toggle the key
overlay. The `-d, --delay` option is GNU `top`'s spelling, parsed by
the shared full-screen-viewer delay grammar in `lib/curses`.

## Shape and testing

The crate mirrors `top`'s host-testable seams: an I/O-free `Model`
(snapshot with per-query `Gauge` degradation, focus/scroll/interval/pin
state), a pure renderer over the curses screen model, and a `Run`
binary that binds only its inherited fd 0/1 and the `sysinfod` IPC
transport. Unit tests drive the model and renderer over in-memory
`sysinfo` and tty channels (keys, panel cycling, interval bounds,
refusal rendering, the refuse-everything survival case, the auto-refresh
tick); the `tairix-test-sysmon-qemu-aarch64` vertical boots the
production aarch64 image, logs in, runs `sysmon` on the console,
witnesses the pressure and reclaim figures on the transcript, quits
with `q`, and gets the shell prompt back on an intact screen.

The bundle is a full self-contained `.app`: signed `AppInfo` requesting
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS`,
`CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_GLOBAL`, and `CAP_MEM_PIN`, the
`Run` rxe, and a thirteen-locale `Help/` tree (`en-US` canonical) that
`man` and the `-h` short help read from disk.
