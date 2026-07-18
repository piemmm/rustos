# `tairix-sleep` — pause for a sum of time intervals

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/sleep.app/` so the shell resolves the bare word
`sleep` to it. `sleep` is the GNU coreutils tool: it pauses for the *sum* of
its `NUMBER[SUFFIX]` operands and then exits. `SUFFIX` is `s` (seconds, the
default), `m` (minutes), `h` (hours), or `d` (days); `NUMBER` is any
C-locale floating-point value — a decimal, a hexadecimal float, or
`inf`/`infinity` — parsed through the shared `lib/util` `scan_double`
scanner (the same one `seq` and `printf` use), so the number grammar lives
in one place. `sleep 1m 30s` pauses ninety seconds; `sleep inf` pauses
until the process is killed. A negative value, a `nan`, an unknown suffix,
or trailing junk is `invalid time interval`; no operand at all is
`missing operand`.

Beyond the operands, `sleep` knows no options but the reserved
`-h`/`-?`/`--help` short-help switches, which render the tool's own Help
document from its bundled `Help/` tree through the shared `lib/help`
engine, in the locale the inherited `LANG` variable names, falling back to
the usage banner when the tree is unavailable. TAIRiX exposes no OS-wide
version string, so — like every TAIRiX command app — `sleep` does not
implement GNU's `--version`; this is the one deliberate divergence from the
GNU surface.

The pause is genuinely off-CPU: the production sleeper parks the task on the
runtime's clock-backed timed wait (`ClockDelay`), so the CPU sleeps rather
than spinning (`AGENTS.md` §2.23). A finite interval is parked in bounded
chunks; `sleep inf` re-parks forever without ever busy-looping on a clock
read.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-abi`, `tairix-util`, and `tairix-help` crates, so it never
links a kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline — and
the secured VFS still authorises every path per-inode under the caller's
attested identity.

## Usage

```
sleep NUMBER[SUFFIX]...

  -h, -?         show this command's own short help
```

Exit codes: `0` when the interval elapsed (or a requested short help was
written); `1` when the short-help write failed; `2` on a usage error (an
unrecognised option, a missing operand, or an invalid time interval).

## Layout

- `src/lib.rs` — the pure, host-testable core: the GNU argument grammar,
  the interval scanner, the typed errors, the injected `Sleeper`/`Output`
  seams, and the engine, with its unit tests (including the per-locale
  switch-drift pin over the on-disk `Help/` tree).
- `src/run.rs` — the freestanding `Run` binary: wires the clock-backed
  off-CPU `RtSleeper`, `RtOutput`, and `BundleHelp` to the pure core; an
  inert stub on the host.
- `AppInfo.toml` — the signed-manifest source the app-bundle composer
  discovers.
- `Help/` — the internationalised structured-Markdown help tree
  (`en-US` canonical plus the standing required locales), authored here
  and planted onto `/System` by the image builder; never embedded in the
  binary.
