# `tairix-unlink` — remove one name

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`), registered
as the system command store bundle `unlink.app` so the shell resolves the
bare word `unlink` to it. It is the minimal tool the POSIX `unlink` function
deserves: exactly one operand, exactly one removal, and no other option than
the reserved `-?`/`--help` short help. `rm` is the tool with `-f`, `-i`, `-r`
and `-v`; `rmdir` is the tool for a directory. Keeping them separate is the
point — a script that must remove one name and nothing else gets a tool that
*cannot* do more, so a mistyped operand cannot become a recursive delete.

## What it guarantees

* **The name, never what it names.** The removal keeps the final component,
  so a symbolic link is removed itself and never followed. A link planted at
  the name cannot redirect the removal to its target.
* **A directory is the kernel's refusal, not this tool's guess.** The empty
  `UnlinkFlags` word asks for a non-directory removal, so the refusal is
  decided in the same locked walk that would have removed the entry: there
  is no check-then-remove window for a directory to be swapped into.
* **Two operands remove nothing.** A second operand is far likelier a
  mistake than an intention, so it is a usage error before any removal.
* **A missing name is reported.** There is no `-f`, so a tool that removes
  exactly one name says so when it removed none.

## Layering & safety

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE` plus
`CAP_FS_ACCESS` — within the session baseline — and no `CAP_CONSOLE_READ`,
because it never prompts. The secured VFS authorises the removal per inode
under the caller's attested identity.

The engine is pure: for one parsed `Command` it performs one removal through
the injected `Filesystem` seam and writes the short help through `Output`,
so every behaviour is host-provable against in-memory fixtures with no
kernel — the seam discipline of the sibling tools (`rmdir`'s `Filesystem`,
`ln`'s `FileSystem`).

## Usage

```
unlink [--] file
```

## Layout

* `src/lib.rs` — the option grammar, the seams, and the removal engine.
* `src/tests.rs` — the parse and removal tests over in-memory seams.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/unlink.md` — the bundled help documents (thirteen
  locales), the single help source (`plans/APPS.md` §6.1).
* `Resources/unlink.svg` — the bundle's own icon master (`plans/ICONS.md`).
