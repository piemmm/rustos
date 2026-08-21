# `tairix-readlink` — print a symbolic link's target

A `plans/APPS.md` command app (`AGENTS.md` §3 `userland/apps/`), registered
as the system command store bundle `readlink.app` so the shell resolves the
bare word `readlink` to it. It prints the target each operand stores — the
stored spelling, read verbatim through `fs_readlink`, which never follows
the final component.

## What it guarantees

* **Verbatim, never resolved.** A link's target is data, not a path the
  kernel walked when the link was made (`plans/SYMLINKS.md` decision 1), so
  a relative target, a target carrying `..`, and a target naming nothing at
  all all print exactly as stored. `ls -l` is the tool that shows a link
  beside what it currently names.
* **A non-link is the kernel's refusal.** A file and a directory both have
  no target, and both are refused with the same `Errno::OutOfRange` domain
  reason; an absent name is `NotFound`. Quiet is the GNU default, `-v`
  diagnoses, and either way the remaining operands are still read and the
  run exits non-zero.
* **`-n` cannot corrupt a multi-operand listing.** The delimiters between
  targets are what separate them, so with more than one operand `-n` is
  ignored and that is reported — never two paths run together on one line.
* **`-f`/`-e`/`-m` call the kernel's canonicalisation, never a second copy
  of it.** Resolving every component of a path — following each link,
  handling `..` physically, enforcing the hop budget, checking search
  permission on every directory passed through, and the rule that a link
  cannot resolve outside what its mount projects — is the VFS's one
  implementation (`plans/SYMLINKS.md`), reached through `fs_realpath`. A
  userland copy that disagreed by one rule would print a path the kernel
  resolves differently, which is a security-relevant divergence rather
  than a mere duplicate. The three switches are alternatives choosing how
  much of the path must exist, so they are one parsed value and the last
  one given wins.

## Layering & safety

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` vocabulary and the shared
`tairix-help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). Its manifest requests `CAP_CONSOLE_WRITE` plus
`CAP_FS_ACCESS` — within the session baseline — and no `CAP_CONSOLE_READ`,
because it never prompts. The secured VFS authorises every path per inode
under the caller's attested identity.

The engine is pure: for one parsed `Command` it reads each operand's target
through the injected `Filesystem` seam and renders the lines through two
`Output` streams, so every behaviour is host-provable against in-memory
fixtures with no kernel — the seam discipline of the sibling tools (`ln`'s
`FileSystem`, `du`'s `Walk`). The production seam sizes each buffer from
the ABI's own bound for what it receives — `FS_SYMLINK_MAX` for a stored
target, `FS_PATH_MAX` for a canonical path — so one call always suffices
and no growth loop exists.

## Usage

```
readlink [-fem] [-nz] [-q | -s | -v] [--] file...
```

## Layout

* `src/lib.rs` — the option grammar, the seams, and the print engine.
* `src/tests.rs` — the parse and print tests over in-memory seams.
* `src/run.rs` — the freestanding `Run` binary (host stub elsewhere).
* `Help/<locale>/readlink.md` — the bundled help documents (thirteen
  locales), the single help source (`plans/APPS.md` §6.1).
* `Resources/readlink.svg` — the bundle's own icon master
  (`plans/ICONS.md`).
