# `tairix-du` — estimate file space usage

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`),
registered as the system app store bundle `du.app` so the shell resolves
the bare word `du` to it. `du` walks each of its path operands and
reports, per directory (post-order), the on-disk storage its tree
occupies; with no operand it walks the current directory (`.`). The
option surface follows GNU coreutils (`AGENTS.md` §16.7): `-a` adds a
row per file, `-s` reports only the operands, `-c` appends a grand
total, `-d` bounds the reported depth, `-S` excludes subdirectories from
a directory's own row, `--apparent-size`/`-b` measure apparent byte
lengths, `-k`/`-m`/`-h`/`--si`/`-B <size>` select the reporting scale,
and `-0` NUL-terminates rows. `-?`/`--help` render the tool's own short
help from its bundled `Help/` tree through the shared `lib/help` engine
(`plans/APPS.md` §4).

The default measure is each node's **allocated** on-disk bytes (the
`fs_stat` `allocated` field the mounted format reports), so sparse or
compressed files report what they really occupy; block counts round up
through the shared GNU size vocabulary in `lib/util`
(`tairix_util::size`), the same definition `df` renders with. Documented
divergences from GNU `du`: TAIRiX has no hard links yet
(`plans/APPS.md` Stage E), so there is nothing for `-l`/hard-link
deduplication to do and the switches do not exist; there are no device
ids, so `-x`/`--one-file-system` is staged behind that kernel work
rather than stubbed; and the `DU_BLOCK_SIZE`-family environment
variables are not read — the scale is selected by options alone.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary, the shared
`tairix-help` engine, and the shared `tairix-util` size vocabulary, so
it never links a kernel or driver crate (`AGENTS.md` §17.4). Its
manifest requests `CAP_CONSOLE_WRITE` plus `CAP_FS_ACCESS` — within the
session baseline — and the secured VFS authorises every path per-inode
under the caller's attested identity. An unreachable path is diagnosed
on standard error and the walk continues (exit `1`), never a partial
guess; the walk uses an explicit frame stack, so a deep tree cannot
exhaust the call stack.

## Usage

```
du [-a | -s] [-cS0] [-h | -k | -m | -b | --si | -B <size>]
   [--apparent-size] [-d <n>] [--] [file...]
```

## Layout

* `src/lib.rs` — crate front matter and the module map.
* `src/command.rs` — the option grammar and its parser.
* `src/io.rs` — the `Walk`/`Output` seams.
* `src/client.rs` — the iterative post-order walk and row rendering.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/du.md` — the bundled help documents (thirteen locales),
  the single help source (`plans/APPS.md` §6.1).
