# `sysmon` — live kernel-memory and load monitor

The system app-store command app that watches every aspect of the
kernel's memory and load through the System Information API
(`plans/STRESSTEST.md` ST4): physical memory, the kernel heap, the
memory-pressure band with its history strip, the reclaimable-cache
ledger, the `ramzip` compressed tier, the pinned-memory aggregate,
per-CPU load, and a process census. Its primary function is observing a
machine under deliberate stress (the `stress` tool's companion), and it
is quiescent between refreshes at idle.

## Shape

The crate is both the `tairix-sysmon` monitor library and the `Run`
entry-point binary of the `sysmon.app` bundle:

- `src/command.rs` — the GNU `-d`/`--delay` grammar (via the shared
  full-screen-viewer delay parser in `lib/curses`) and the reserved
  `-h`/`-?` short-help switches.
- `src/model.rs` — the I/O-free view state: the sampled snapshot with
  per-query degradation (`Gauge`), the panel focus/scroll, the bounded
  refresh interval, the pin state, and the key handling.
- `src/app.rs` — the renderer (summary block + focused detail panel +
  help overlay) and the event-driven loop: one bounded input wait per
  refresh interval, never a poll loop.
- `src/run.rs` — the freestanding program: parses options, pins its own
  memory (`mem_pin`; a refusal is reported on the title line and the
  session continues unpinned), enters the alternate screen, and runs
  the loop over the inherited fd 0/1 and the `sysinfod` IPC transport.

Every figure travels through `sysinfo-v1`; the four kernel-statistics
fetches are the shared `lib/procinfo` `kstats` walks, never a private
copy. Gated queries (`CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_GLOBAL`)
degrade to stated refusals per panel while the session continues; the
monitor's only fatal failure is the terminal itself.

## Bundle

A full self-contained `.app` bundle: `AppInfo.toml` (requesting
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, `CAP_FS_ACCESS`,
`CAP_SYSINFO_KERNEL`, `CAP_SYSINFO_GLOBAL`, `CAP_MEM_PIN`), the `Run`
rxe, and the `Help/` tree (thirteen locales, `en-US` canonical) that
`man`, the `-h` short help, and the help-locale test all read from
disk. In the OS image the bundle is discovered from the store like any
other app — nothing is compiled into the kernel or the image builder.

## Layering & safety

`no_std` (with `alloc`). It links only `lib/*` crates — `lib/abi`,
`lib/procinfo`, and the OS `lib/curses`/`lib/termcap`/`lib/vt` — never a
kernel or driver crate (`AGENTS.md` §17.4). No `unsafe`, no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9), and
nothing writes to fd 3 (`stdinfo`, §20).
