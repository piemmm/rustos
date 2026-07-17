# `tairix-true` — do nothing, successfully

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/true.app/` so the shell resolves the bare word
`true` to it. `true` is the GNU coreutils tool: it ignores every argument
and exits `0`, giving scripts a command that always succeeds. A **first**
argument of `-h`/`-?`/`--help` — the position GNU honours `--help` in —
renders the tool's own short help from its bundled `Help/` tree through
the shared `lib/help` engine, in the locale the inherited `LANG` variable
names, falling back to the usage banner when the tree is unavailable.

The crate is `no_std` (no `alloc` in the library), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its only dependency is the
shared `tairix-help` crate, so it never links a kernel or driver crate.
Its manifest (`AppInfo.toml`) requests `CAP_CONSOLE_WRITE` and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode under the caller's attested identity.

## Usage

```
true [ignored arguments]

  -h, -?         (first argument only) show this command's own short help
```

## Exit status

- `0` — always (the tool's whole purpose).
- `1` — a requested short help could not be written.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
