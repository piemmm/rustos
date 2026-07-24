## NAME

sysmon — watch the kernel's memory, caches, and load live

## SYNOPSIS

`sysmon [-d secs.tenths] [-h | -?]`

## DESCRIPTION

`sysmon` is a full-screen, live view of what the kernel is doing with
memory and CPU, read entirely through the System Information API — there
is no `/proc` to scrape. It shows physical memory and its composition,
the kernel heap, the memory-pressure band and its recent history, the
reclaimable-cache ledger with per-class **hit ratios**, the `ramzip`
compressed memory tier, the pinned-memory aggregate, mounted-volume
storage usage, per-CPU load, the kernel interrupt table, and a process
census. It stays usable while the system is under deliberate stress and
is quiescent between refreshes at idle (the read parks; it never spins).

At startup the monitor pins its own memory (`mem_pin`, requiring
`CAP_MEM_PIN`) so it never stalls on its own page fault-in under the very
pressure it observes. A refused pin is reported on the title line and the
session continues unpinned — the pin is incidental, never fatal.

The display refreshes every interval (3.0 seconds unless `-d` changes
it). The monitor takes no operands: it is driven by keys pressed inside
the session.

- `q` — quit.
- Left / Right (or `p`) — switch the detail panel (Left = previous,
  Right / `p` = next): caches, the compressed tier, mounted-volume
  storage (disks), per-CPU load, interrupt lines, processes.
- `r` — refresh now.
- `+` / `-` — lengthen / shorten the refresh interval by one second,
  between 0.1 and 60 seconds.
- Up/Down, PageUp/PageDown, Home/End — scroll the focused panel.
- `h`, `?` — toggle the in-session key overlay (which reproduces the bar
  key below).

### The summary block

A fixed summary block precedes the detail panel. Every row is labelled at
the left so it reads without colour; colour is only reinforcement.

- **Title bar** — the tool name, the system uptime (`up D days, H:MM`),
  the three load averages (1/5/15-minute), and the pin state
  (`[pinned]`, or `[unpinned: <reason>]` when the pin was refused).
- **`Mem`** — the memory bar (see the bar key), followed by the used /
  total figures (compact `K`/`M`/`G` units), the used percentage, the
  kernel-heap size, and — when non-zero — the `ramzip` compressed-store
  and `pinned` figures. The bar shrinks to keep every figure on an
  80-column line, so the figures are never clipped.
- **`Pres`** — the memory-pressure bar: a five-band gauge, each entered
  band filled in its own severity colour, followed by the current band
  name and the free / reserve figures and the total band-entry count.
- **`Hist`** — the pressure-band history strip: one glyph per refresh,
  oldest at the left, each coloured by its band — `.` normal, `-` mild,
  `=` moderate, `#` severe, `!` critical — so a stretch of pressure reads
  as a coloured run.
- **`CPU`** — the aggregate CPU bar (see the bar key), followed by the
  all-CPU busy percentage, the CPU count, and the summed context-switch
  and preemption counters.
- **`Tasks`** — the process census: total, running, sleeping, stopped,
  and zombie counts (with `(own)` appended when the all-process census
  was refused and only the caller's own tasks are counted).
- **Panel tab bar** — every detail panel, the focused one highlighted,
  with a scroll indicator at the right when the focused panel overflows.

### The bar key

The `Mem` and `CPU` gauges are bracketed `[…]` bars. The `?` overlay
reproduces this key inside the running session.

The memory bar (`Mem`) is a **stacked** bar whose cells name what
physical memory holds — a *disjoint* split of used memory (`used` is
`total` minus `free`), so nothing is counted twice and the filled width
is exactly the used fraction:

- `#` — user-resident memory (green): pages resident in user address
  spaces.
- `K` — the kernel heap (cyan): the kernel's own heaps and slabs.
- `=` — other in-use memory (magenta): everything used but not attributed
  above (page caches, buffers, kernel frames).
- blank — free memory.

The `ramzip` compressed store and `pinned` anonymous memory overlap those
buckets (pinned pages are user-resident; the compressed store is kernel
memory), so they are reported as trailing figures beside the bar rather
than as separate, double-counting slices — honest accounting over a
misleading picture.

The pressure bar (`Pres`) colours each band by depth: normal/mild green,
moderate yellow, severe/critical red.

The CPU bar (`CPU`) fills with `#` busy cells over blank idle track,
coloured by the busy share (green below 60%, yellow below 85%, red at or
above 85%). TAIRiX accounts CPU time as busy versus idle only — there is
no user/system/iowait split in the API — so the bar shows a single honest
busy category, with per-core detail in the `cpu` panel.

### The detail panels

Left / Right (or `p`) steps through six panels. Each has an inverted
(reverse-video, bold) column header so the heading reads as a distinct
bar above the body.

### caches — the reclaimable-cache ledger

These are the caches the kernel may hand back to relieve memory pressure
**without data loss**: every entry is rebuildable from its canonical
source, so the kernel drops it rather than paging it out. The panel is
the direct answer to "are the caches doing their job?": each row is one
reclaim class, aggregated across every registered cache, and carries its
own **hit ratio**.

Columns:

- `class` — the reclaim class (see the class list below).
- `entries` — live entries currently held for the class.
- `cached` — the class's resident footprint: entry payload plus per-entry
  bookkeeping metadata, together.
- `hits` — lookups of the class served from cache since boot (the cache
  avoided the canonical source).
- `misses` — lookups of the class that fell through to the canonical
  source since boot.
- `hit%` — the cache-effectiveness ratio, `hits / (hits + misses)` as a
  whole percent. A high ratio means the cache is earning its memory; a
  low ratio means it is holding memory without avoiding work. It reads
  `-`, never a fabricated `0%`, for a class nothing has looked up this
  boot (an idle denominator).
- `ref` — admissions **ref**used since boot (an entry the cache declined
  to hold: over budget, unaccountable, or out of memory).
- `shr` — pressure-forced **shr**ink passes that reclaimed entries of the
  class since boot.
- `fail` — internal **fail**ures attributed to the class: a detected
  ledger defect that poisoned (fail-closed disabled) a cache.

Counts abbreviate above 99 999 as `k`/`M`/`G`/`T` (decimal thousands, not
KiB) so a column never widens.

The reclaim classes, in the order the kernel reclaims them under pressure
(first listed is dropped first, so a cache low in the list survives
longest):

- `disposable-ui` — disposable UI state (rasterised assets, glyph
  atlases, window snapshots): cheapest to lose, first to go.
- `predictive-prefetch` — speculatively prefetched data (listings,
  thumbnails, completion indexes): never needed for correctness.
- `background-validation` — idle-time validation work products (scan
  progress, candidate fingerprints): speculative work stops as pressure
  begins.
- `semantic-app-cache` — verified app-launch state (parsed manifests,
  validation summaries, command-resolution results). Reclaiming it can
  never make an app unlaunchable — the load gate simply re-runs.
- `runtime-cache` — runtime-owned derived state (loader preparation,
  resource maps): grouped with the semantic cache.
- `clean-file-data` — clean, rebuildable file *contents* re-readable from
  the volume: one bounded device read rebuilds a chunk. Reclaimed before
  anything is compressed into `ramzip`.
- `transform-cache` — expensive intermediate forms of authorised data
  (verified, decrypted, decompressed cluster data): costlier to rebuild
  than a clean read, so reclaimed after clean file data.
- `fs-metadata` — filesystem metadata: stat records, name-lookup results,
  directory entries, and security records. Small, hot, and rebuilt only
  by a multi-step tree walk, so it outlives file data under pressure.
- `reliability-assist` — rebuildable recovery-assist state (verification
  windows, health summaries): justified by recovery latency, so it is
  preserved the longest.

### ramzip — the compressed memory tier

`ramzip` compresses cold anonymous pages into a smaller in-RAM store
instead of paging them out. Its sections:

- `tier` — the live footprint: `entries` held, `logical` (uncompressed)
  bytes represented, `stored` ciphertext bytes actually held, and
  `metadata` bookkeeping bytes; then `saved` (logical minus stored) with
  its percentage of logical — the memory the tier is winning back.
- `capacity` — the derived caps the tier sizes itself to: `min` (always
  available), `soft` (target), `hard` (ceiling), and the current `pinned`
  bytes.
- `compress` — the store (write) path: `attempts` offered, `accepted` and
  stored, and the **accept-rate** (accepted / attempts) — this tier's
  own hit ratio for compression. Below it, the rejection breakdown:
  incompressible, policy, cap, ineligible, reserve, task-share, and
  thrash refusals.
- `restore` — the fetch (read) path: page `faults`, `warm` restores,
  `clustered` restores, and their total `restored`; then the `failures`
  (auth / decode) and the **success-rate** (restored / (restored +
  failures)). Each ratio is a percentage, or `-` for an idle denominator.
- `warm-up` — the background warm-restorer's `attempts`, `stopped`
  count, and `thrash-detected` count.

### disks — mounted-volume storage

One `df`-style row per mounted volume: mount point, filesystem type,
total size, used, available, use percentage, and an ASCII usage bar. A
volume whose driver reports no capacity shows `capacity unknown` rather
than a fabricated size; a surprise-removed or recovery-conflicted volume
is drawn in the warn rendition and marked (`[unavailable-dirty]`,
`[unavailable-lost]`, `[recovery-conflict]`). There are no per-device I/O
throughput counters in the API, so this is honest capacity and usage, not
fabricated transfer rates.

### cpu — per-CPU load

One row per CPU: its busy share over the interval (`busy%`), its
run-queue depth (`queue`), and its context-`switches` and `preemptions`
counts since boot.

### irqs — interrupt lines

One row per bound interrupt line, in ascending line order: the line id,
the owning driver task (`owner`), the interrupt `count` since boot, and
the line `state` — `active`, or `quarantined` (drawn in the warn
rendition) when the kernel's runaway-line safety net has disabled it.

### procs — the process census

The top consumers by `%cpu` and by memory (`size`), each with its pid,
command, and — for the memory table — its state. The full interactive
process list is `top`'s job; this is the census summary only.

### Capabilities

Every figure travels through the System Information API. The kernel-wide
statistics queries (memory, pressure, caches, `ramzip`, per-CPU load)
need `CAP_SYSINFO_KERNEL`; the interrupt-lines panel needs
`CAP_SYSINFO_HW`; the all-process census needs `CAP_SYSINFO_GLOBAL`. A
caller without one sees that panel's refusal spelled out — never a
fabricated figure — while the rest of the session continues (fail closed,
degrade gracefully). Mounted-volume storage is ungated.

## OPTIONS

- `-d, --delay <seconds>` — the interval between automatic refreshes, in
  seconds with an optional fraction (only the first fractional digit,
  tenths, is kept): `sysmon -d 1.5` refreshes every 1.5 seconds. Defaults
  to 3.0. GNU `top` accepts a zero delay and refreshes as fast as it can;
  TAIRiX never busy-loops, so a zero is clamped to the 0.1 s minimum.
- `-h, -?` — show this command's own short help and exit. Inside a
  running session the same keys toggle the key overlay instead.

## EXIT STATUS

- `0` — the session ended with `q`, or the short help was shown.
- `1` — the terminal failed; the reason is printed on standard error.
- `2` — the command line was not understood.

## ENVIRONMENT

- `LANG` — the preferred locale for the short help (a BCP-47 tag such as
  `fr-FR`).

## SEE ALSO

- `man`
- `sysinfo`
- `top`
