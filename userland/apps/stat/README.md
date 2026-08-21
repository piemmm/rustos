# `tairix-stat` — report a file's or a filesystem's status

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`), registered
as the system command store bundle `stat.app` so the shell resolves the bare
word `stat` to it. It renders the fields of one `fs_stat` per operand, either
as the default report or through a `--format` / `--printf` string of GNU's
own specifiers.

## What it guarantees

* **A link is described as itself.** Without `-L` the tool reports the
  *link*: `%N` shows it beside the target it stores, `%F` says
  `symbolic link`, and the sizes and stamps are the link's own. That is what
  `stat` is for beside `ls`, and it works because the production seam opens
  the path `NO_FOLLOW` — the descriptor's flags fix the posture, so the stat
  served for it cannot contradict the open (`plans/SYMLINKS.md` S2).
* **Two vocabularies, checked before any path is touched.** `-f` selects the
  filesystem reading, whose specifier set differs from the file one, and the
  format is parsed once — after the whole command line, so `-f` after `-c`
  still decides — with an unknown or unserviceable directive refused there.
  A format the platform cannot serve therefore never half-renders.
* **A fact that cannot be read is said so, never guessed.** A mount snapshot
  the caller may not read leaves `%m` and `%o` as `?`; a uid the user
  directory has no name for is GNU's `UNKNOWN`, never the number wearing a
  name field's label.
* **`%m` names the mount holding the *canonical* path.** The covering mount
  is the longest mount-point prefix of the path the kernel canonicalises
  (`fs_realpath`), matched by whole components — so a link into another
  volume reports the volume it lands on, and `/vol` never claims
  `/volume/x`.
* **Four specifiers are refused by name, not answered with a fabrication.**
  `%G` (the System Information API publishes a user directory and no group
  counterpart, so `%g` is the honest field), `%t`/`%T` of the file
  vocabulary (there are no device special files to have a major or minor
  type), and `%t` of the filesystem vocabulary (a volume carries no numeric
  type magic; `%T` names the type its mount records). This is the `du -x`
  posture: refused for a stated reason, never stubbed.
* **Two specifiers report the TAIRiX concept.** A volume is identified by a
  16-byte id rather than a device number, so `%d` is that id in decimal and
  `%D` in hexadecimal — the same value in two spellings. Comparing two
  files' `%d` still answers exactly "are these on one volume?".

## Layering & safety

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary, the one civil-date
breakdown (`tairix-fsmeta`'s `CivilTime`, which `ls`'s date column and the
login clock share), and the shared `tairix-help` engine, so it never links a
kernel or driver crate (`AGENTS.md` §17.4). Its manifest requests
`CAP_CONSOLE_WRITE` plus `CAP_FS_ACCESS` — within the session baseline — and
no `CAP_CONSOLE_READ`, because it never prompts. The mount snapshot and the
user directory are the ungated System Information queries `df` and `whoami`
read, through the one shared `tairix-procinfo` client, so neither adds a
capability.

The engine is pure: for one parsed `Command` it gathers each operand's facts
once through the injected `Filesystem` / `Mounts` / `Names` seams and renders
the format over them, so every specifier, both vocabularies, and each refusal
are host-provable against in-memory fixtures with no kernel — the seam
discipline of the sibling tools (`df`'s mount client, `du`'s `Walk`, `ln`'s
`FileSystem`). The five seams travel as one `Reporter` rather than as five
parameters, the shape `du`'s `Reporter` and `cp`'s `Copier` take.

## Usage

```
stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] file...
```

## Layout

* `src/command.rs` — the option grammar and the format grammar (directives,
  flags, width, precision, and the per-vocabulary specifier sets).
* `src/client.rs` — the `Reporter`, the fact gathering, and the renderer.
* `src/io.rs` — the seams and the data they carry.
* `src/error.rs` — the outcomes one run can have.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/stat.md` — the bundled help documents (thirteen locales),
  the single help source (`plans/APPS.md` §6.1).
* `Resources/stat.svg` — the bundle's own icon master (`plans/ICONS.md`).
