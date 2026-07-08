# `rustos-df` — report filesystem space usage

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`),
registered as the system app store bundle `df.app` so the shell resolves
the bare word `df` to it. `df` reports, one row per mounted filesystem,
the volume's size, the space used, the space available, the percentage
used, and the mount point; with `file` operands it reports the
filesystem containing each operand. The option surface follows GNU
coreutils (`AGENTS.md` §16.7): `-a` shows the pseudo/duplicate mounts
the default hides, `-T`/`-t`/`-x` add and filter by filesystem type,
`-i` reports inodes, `-P` selects the POSIX portable wording, `--total`
appends a summary row, `-l` accepts the local-only filter (every RustOS
mount is local), and `-k`/`-h`/`-H`/`--si`/`-B <size>` select the scale
through the shared GNU size vocabulary in `lib/util`
(`rustos_util::size`), the same definition `du` renders with.
`-?`/`--help` render the tool's own short help from its bundled `Help/`
tree through the shared `lib/help` engine (`plans/APPS.md` §4).

Live system state is read exclusively through the System Information
API (`AGENTS.md` §16.6): the ungated `sysinfo-v1` `MOUNT_LIST` query
served by `sysinfod`, whose rows carry each backing volume's space
accounting (`VolumeStats`) as the mounted filesystem driver reports it —
the shared `rustos_procinfo::for_each_mount` walk the `mount` tool uses,
never a second query client and never a `/proc`-style scrape. A volume
with a dynamic inode table reports zero inode figures (the honest
"untracked" answer). When the default view hides capacity-less or
duplicate mounts, the omission is noted on fd 3
(`fs.mounts_omitted`, `AGENTS.md` §20.1), never in the table.

Documented divergences from GNU `df`: the `--output` field list and
`--sync`/`--no-sync` are not yet available; a relative `file` operand is
diagnosed rather than resolved (mount points are absolute and RustOS has
no path canonicalisation for tools yet); the `DF_BLOCK_SIZE`-family
environment variables are not read — the scale is selected by options
alone.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` vocabulary and the shared
`rustos-help`, `rustos-procinfo`, and `rustos-util` crates, so it never
links a kernel or driver crate (`AGENTS.md` §17.4). Its manifest
requests `CAP_CONSOLE_WRITE` plus `CAP_FS_ACCESS` — within the session
baseline; the mount listing itself is ungated and per-query authority is
enforced by `sysinfod` against this process's kernel-attested origin.

## Usage

```
df [-aikPTl] [-h | -H | --si | -B <size>] [-t <type>] [-x <type>]
   [--total] [--] [file...]
```

## Layout

* `src/lib.rs` — crate front matter and the module map.
* `src/command.rs` — the option grammar and its parser.
* `src/io.rs` — the `PathProbe`/`Output` seams.
* `src/client.rs` — selection, filtering, and the table renderer.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/df.md` — the bundled help documents (thirteen locales),
  the single help source (`plans/APPS.md` §6.1).
