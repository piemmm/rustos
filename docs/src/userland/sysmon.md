# `sysmon` — live kernel-memory and load monitor

`sysmon` (`userland/apps/sysmon`, `plans/STRESSTEST.md` ST4) is the
system app-store command app that watches every aspect of the kernel's
memory and load through the System Information API, full screen and
live: physical memory, the kernel heap, the memory-pressure band with a
scrolling history strip, the reclaimable-cache ledger, the `ramzip`
compressed tier, the pinned-memory aggregate, mounted-volume storage
usage, per-CPU load, the kernel IRQ table, and a process census. Its
primary function is observing a machine under deliberate stress; at idle
it is quiescent between
refreshes (the input wait is bounded by the refresh interval — never a
poll loop).

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
`RAMZIP_STATS`, `CPU_LOAD`) and the `IRQ_LIST` interrupt-table fetch are
the shared `tairix_procinfo::kstats` walks — the same definitions the
`info:`/`stats:` resolver and the `sysinfo` CLI use, so the consumers
can never diverge. `KERNEL_MEMORY_STATS`, the ungated
`UPTIME`/`LOAD_AVERAGE`/`CPU_TIME_STATS`, and the process lists round
out the panels.

Each query degrades independently: a capability refusal
(`CAP_SYSINFO_KERNEL` for the kernel-wide statistics,
`CAP_SYSINFO_GLOBAL` for the all-process census, `CAP_SYSINFO_HW` for
the interrupt-lines panel) renders as that panel's spelled-out refusal,
a failed call as the figure's honest absence — the session continues
either way. The mounted-volume table is the ungated `MOUNT_LIST` walk
(`tairix_procinfo::for_each_mount`); a volume whose driver reports no
capacity shows `capacity unknown` rather than a fabricated size, and a
surprise-removed or recovery-conflicted volume is drawn in the warn
rendition with the condition named. The monitor's only fatal failure is
the terminal itself: a hiccuping or refusing service must never kill the
observer built to run under stress.

## Layout

A fixed seven-row summary block sits above a scrollable detail panel and
a key-hint footer. The seven summary rows, top to bottom, are:

1. a full-width **title bar** — the program name, `up <uptime>`, the
   1/5/15-minute load averages, and the pin state (`[pinned]`, or
   `[unpinned: <reason>]` when the self-pin was refused);
2. the **memory** gauge (`Mem`, described under *Bar key* below);
3. the **memory-pressure** gauge (`Pres`) — a five-segment bar, one
   segment per pressure band, each entered band filled in its band
   colour, followed by the current band name and the `free`/`reserve`
   byte figures and the total band-entry count. The pressure band is the
   kernel's coarse "how close to reclaim distress" signal: `normal`,
   `mild`, `moderate`, `severe`, `critical`;
4. the **aggregate CPU** gauge (`CPU`, described under *Bar key*);
5. the pressure-band **history strip** (`Hist`) — one glyph per refresh,
   oldest leftmost, each coloured by its band: `.` normal, `-` mild, `=`
   moderate, `#` severe, `!` critical, bounded at 120 samples, so a
   stretch of pressure reads as a coloured run;
6. the **task census** (`Tasks`) — the run-queue/total/sleeping counts,
   with an `(own)` marker when the system-wide census was refused and the
   figures are the caller's own processes only;
7. the **panel tab bar** — every detail panel named (`caches`, `ramzip`,
   `disks`, `cpu`, `irqs`, `procs`) with the focused one highlighted, and
   a `[first-last/total] <-/-> panel` scroll indicator when the focused
   panel overflows its viewport.

## Bar key (gauge glyphs and colours)

The three summary gauges are bracketed `[…]` bars. Two conventions are in
play, and the `?` overlay reproduces this key in-app:

- The **memory** bar (`Mem`) is a *stacked* bar: its cells name what
  physical RAM holds, a disjoint decomposition of used memory (`total −
  free`) so nothing is counted twice and the filled width is exactly the
  used fraction:
  - `#` — **user-resident** memory (green), resident in user address
    spaces;
  - `K` — the **kernel heap** (cyan), the kernel's own heaps and slabs;
  - `=` — **other** in-use memory (magenta): everything used but not
    separately attributed above (caches, buffers, kernel frames);
  - blank track — **free** memory.

  The trailing text states `used / total MiB`, the used percentage, the
  kernel-heap size, and — when non-zero — the `ramzip` compressed-store
  bytes and the `pinned` anonymous-memory total. Those last two overlap
  the bar's buckets (pinned pages are user-resident; the compressed store
  is kernel memory), so they are reported as figures rather than as
  separate, double-counting bar slices — honest accounting over a
  misleading picture.

- The **pressure** bar (`Pres`) and the **CPU** bar (`CPU`) are
  *severity*-coloured: green below 60%, yellow below 85%, red at or above
  85% (the pressure segments colour by band depth instead). The CPU bar
  fills with `#` **busy** cells over blank **idle** track. TAIRiX
  accounts CPU time as busy-vs-idle only — there is no user/system/iowait
  split in the kernel — so the bar honestly carries a single busy
  category rather than a fabricated breakdown; per-core detail lives in
  the `cpu` panel. The trailing text states the busy share, the CPU
  count, and the summed context-switch and preemption counters.

## Detail panels

The Left/Right arrow keys (or `p`) step the focused detail panel through
six views. Each table's column header is drawn as an inverted
(reverse-video) full-width bar, and stated refusals and degraded rows are
drawn in their own colour, so structure reads without hunting.

- **`caches`** — the **reclaimable-cache ledger**, one row per reclaim
  class (e.g. disposable UI surfaces, clean file-backed pages): the
  memory the kernel *could* hand back under pressure without data loss,
  broken down by class so an operator can see where reclaimable slack
  lives.
- **`ramzip`** — the **compressed memory tier**, laid out in aligned
  sections. `ramzip` transparently compresses cold anonymous pages into a
  smaller in-RAM store instead of paging them out, so more fits in
  physical memory before swap is touched. The sections are:
  - *tier* — live footprint: `entries` held, `logical` bytes represented,
    `stored` compressed bytes, `metadata` bookkeeping, and the `saved`
    bytes (logical − stored) as a percentage of logical;
  - *capacity* — the derived `min`/`soft`/`hard` byte caps the tier may
    grow within, and the `pinned` bytes it may never reclaim;
  - *compress* — the store (write) cache path: `attempts` offered,
    `accepted` and stored, and the **accept rate** (`accepted /
    attempts`); then the rejection reasons (incompressible, policy, cap,
    ineligible, reserve, task-share, thrash);
  - *restore* — the fetch (read) cache path: pages restored by demand
    `faults`, background `warm`-up, and post-fault `clustered` restores;
    then the failures (`auth`, `decode`) and the **success rate**
    (restores over restores-plus-failures);
  - *warm-up* — the background restorer's `attempts`, `stopped`, and
    `thrash-detected` counts.

  Each cache path's hit ratio is a percentage, or `-` when the
  denominator is idle — never a ratio invented from zero.
- **`disks`** — the **mounted-volume storage table**, one `df`-style row
  per mounted filesystem: mount point, filesystem type, total size, used,
  available, use percentage, and an ASCII usage bar. A volume whose
  driver reports no capacity shows `capacity unknown` rather than a
  fabricated size; a surprise-removed or recovery-conflicted volume is
  drawn in the warn rendition with the condition named. (There are no
  per-device I/O throughput counters in `sysinfo-v1`, so this panel is
  honest capacity/usage, not fabricated transfer rates.)
- **`cpu`** — the **per-CPU load table**: each CPU's busy share over the
  refresh interval, its run-queue depth, and its cumulative
  context-switch and involuntary-preemption counts since boot.
- **`irqs`** — the **kernel interrupt-line table**, one row per bound
  line: its id, the owning driver task, the interrupt count since boot,
  and its state; a *quarantined* line (one the runaway-interrupt safety
  net masked) is drawn in the warn colour.
- **`procs`** — the **process top consumers**, by `%CPU` over the
  interval and by resident bytes (the full interactive process list
  remains `top`'s job).

Colour is always reinforcement, never the sole channel: on a monochrome
terminal the gauges still fill, the glyphs (`#`/`K`/`=`) still name the
memory categories, and the inverted headers still read via reverse video,
so the layout never depends on colour to be legible.

## Keys

Keys follow the `top` conventions: `q` quit, `r` refresh now,
Left/Right (or `p`) switch the detail panel, Up/Down and
PageUp/PageDown/Home/End scroll the focused panel, `+`/`-`
lengthen/shorten the refresh interval by one second within 0.1–60 s
(re-armed on the very next wait), `?`/`h` toggle the key overlay (which
carries the bar key above). The `-d, --delay` option is GNU `top`'s
spelling, parsed by the shared full-screen-viewer delay grammar in
`lib/curses`.

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
`CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_GLOBAL`, `CAP_SYSINFO_HW`, and
`CAP_MEM_PIN`, the `Run` rxe, and a thirteen-locale `Help/` tree
(`en-US` canonical) that `man` and the `-h` short help read from disk.
