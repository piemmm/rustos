# `tairix-link` — give a file a second name

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`), registered
as the system command store bundle `link.app` so the shell resolves the bare
word `link` to it. It is the minimal tool the POSIX `link` function
deserves: exactly two operands, exactly one hard link, and no option other
than the reserved `-?`/`--help` short help. `ln` is the tool with `-f`,
`-i`, `-v`, `-s`, `-L`/`-P` and the `-t`/`-T` destination forms. Keeping
them separate is the point — a script that must create one hard link and
nothing else gets a tool that *cannot* replace a name, follow a link, or
make a symbolic link instead of a hard one.

## What it guarantees

* **Neither name is followed.** `fs_link` is called with an empty
  `LinkFlags` word — POSIX `link()` — so the node that gains a name is the
  one the caller spelled: a symbolic link planted at the existing name
  cannot redirect the new name at what it points to. The new name is never
  followed either, because it is a name being created.
* **An occupied new name is refused, not replaced.** A create never
  overwrites a name; `Errno::AlreadyExists` says so.
* **Each refusal keeps its own meaning.** `IsADirectory` (a directory has
  exactly one name everywhere), `CrossVolume` (a second name must live on
  the volume that stores the node), `TooManyLinks` (the format's per-node
  count would overflow), and `NotSupported` (the format stores one name per
  node — a permanent property, not a transient failure) reach the caller
  unchanged, because collapsing any of them onto another would give the
  wrong advice. A regression test pins all five.
* **A third operand links nothing.** It is a usage error before any
  filesystem call.

## Layering & safety

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE` plus
`CAP_FS_ACCESS` — within the session baseline — and no `CAP_CONSOLE_READ`,
because it never prompts. The secured VFS authorises both names per inode
under the caller's attested identity.

The engine is pure: for one parsed `Command` it makes one link through the
injected `Filesystem` seam and writes the short help through `Output`, so
every behaviour is host-provable against in-memory fixtures with no kernel
— the seam discipline of the sibling tools (`ln`'s `FileSystem`, `unlink`'s
`Filesystem`).

## Usage

```
link [--] existing new
```

## Layout

* `src/lib.rs` — the option grammar, the seams, and the linking engine.
* `src/tests.rs` — the parse and linking tests over in-memory seams.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/link.md` — the bundled help documents (thirteen locales),
  the single help source (`plans/APPS.md` §6.1).
* `Resources/link.svg` — the bundle's own icon master (`plans/ICONS.md`).
